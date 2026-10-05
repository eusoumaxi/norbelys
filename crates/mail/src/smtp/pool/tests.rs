use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use secrecy::SecretString;
use tokio::time::Instant;

use super::{SessionCap, SmtpPool, check_credential};
use crate::net::AddressPolicy;
use crate::smtp::{PoolConfig, ReplyId, SmtpAuth, SmtpSecurity, SmtpServer};
use crate::status::EnhancedStatus;
use crate::submission::{Cause, Envelope, Failure, Phase, Rejection, Scope};
use crate::testing::{Smtp, connector, reply, smtp_default, smtp_server};

const MIME: &[u8] = b"From: ada@example.com\r\nSubject: hi\r\n\r\nhello\r\n";

/// A fake server's answers, as a table entry.
type Script = fn(&str) -> Smtp;

fn pool(config: PoolConfig) -> SmtpPool {
    SmtpPool::new(connector(AddressPolicy::Any), config)
}

fn plain(port: u16) -> SmtpServer<'static> {
    SmtpServer {
        host: "127.0.0.1",
        port,
        security: SmtpSecurity::Plain,
    }
}

fn secret() -> SecretString {
    SecretString::from("app-password".to_owned())
}

/// The fixture account authenticates every password-based SMTP session.
fn password_auth(password: &SecretString) -> SmtpAuth<'_> {
    SmtpAuth::Password {
        username: "ada@example.com",
        password,
    }
}

/// A server that advertises authentication but no MIME or UTF-8 extensions.
fn without_extensions(line: &str) -> Smtp {
    if line.starts_with("EHLO") {
        reply("250-fake.test\r\n250 AUTH PLAIN")
    } else {
        smtp_default(line)
    }
}

fn envelope(recipients: &[&str]) -> Envelope {
    Envelope::new("ada@example.com", recipients.iter().copied()).expect("a valid envelope")
}

fn in_five_minutes() -> Instant {
    Instant::now() + Duration::from_secs(300)
}

fn assert_rejection(
    rejection: &Rejection,
    failure: Failure,
    phase: Phase,
    scope: Scope,
    cause: Cause,
) {
    assert_eq!(
        (
            rejection.failure,
            rejection.phase,
            rejection.scope,
            rejection.cause
        ),
        (failure, phase, scope, cause),
        "{rejection:?}"
    );
}

/// The happy path end to end over a socket: AUTH, MAIL FROM, RCPT TO, DATA and the content, the
/// queue id read from the `250` in the pool's provider form; then the clean session is parked
/// and the next submission reuses it after a `NOOP` instead of opening a second connection.
#[tokio::test]
async fn an_accepted_submission_returns_its_id_and_parks_the_session() {
    let (port, transcript) = smtp_server(smtp_default).await;
    let pool = pool(PoolConfig {
        reply_id: ReplyId::QueuedAs,
        ..PoolConfig::default()
    });
    let password = secret();
    let auth = password_auth(&password);
    let session = pool
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a session");
    let submission = session
        .submit(&envelope(&["grace@example.com"]), MIME, in_five_minutes())
        .await
        .expect("an accepted submission");
    assert_eq!(submission.provider_message_id.as_deref(), Some("4ABC123"));
    assert_eq!(
        submission.reply.as_deref(),
        Some("250 2.0.0 Ok: queued as 4ABC123")
    );
    assert!(submission.refused.is_empty());
    assert!(transcript.has("AUTH PLAIN "));
    assert!(transcript.has("MAIL FROM:<ada@example.com>"));
    assert!(transcript.has("RCPT TO:<grace@example.com>"));
    assert!(transcript.has("content:From: ada@example.com"));

    let again = pool
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a parked session");
    again
        .submit(&envelope(&["grace@example.com"]), MIME, in_five_minutes())
        .await
        .expect("accepted again");
    assert_eq!(transcript.count("connect"), 1);
    assert_eq!(transcript.count("NOOP"), 1);
}

/// A recipient refused at `RCPT TO` while another is accepted does not stop the message: the
/// accepted recipient still receives it, the refusal is returned as evidence about that recipient
/// alone, and the session, having seen a negative reply, is closed rather than reused.
#[tokio::test]
async fn refused_recipients_are_reported_while_the_others_receive_the_message() {
    let (port, transcript) = smtp_server(|line| {
        if line == "RCPT TO:<ghost@example.com>" {
            reply("550 5.1.1 <ghost@example.com>: Recipient address rejected")
        } else {
            smtp_default(line)
        }
    })
    .await;
    let pool = pool(PoolConfig::default());
    let password = secret();
    let auth = password_auth(&password);
    let session = pool
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a session");
    let submission = session
        .submit(
            &envelope(&["grace@example.com", "ghost@example.com"]),
            MIME,
            in_five_minutes(),
        )
        .await
        .expect("accepted for the other recipient");
    let [refusal] = submission.refused.as_slice() else {
        panic!("one refusal expected: {:?}", submission.refused);
    };
    assert_eq!(refusal.recipient, "ghost@example.com");
    assert_eq!(refusal.code, 550);
    assert_eq!(refusal.status, "5.1.1".parse::<EnhancedStatus>().ok());
    assert!(transcript.has("content:"));

    pool.session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a new session");
    assert_eq!(transcript.count("connect"), 2);
}

/// When every recipient is refused the message itself is rejected, with the dominant refusal's
/// meaning and every refusal listed, and `DATA` is never sent.
#[tokio::test]
async fn a_message_whose_recipients_are_all_refused_is_rejected_without_data() {
    let (port, transcript) = smtp_server(|line| {
        if line.starts_with("RCPT TO:") {
            reply("550 5.1.1 user unknown")
        } else {
            smtp_default(line)
        }
    })
    .await;
    let pool = pool(PoolConfig::default());
    let session = authenticated(&pool, port).await;
    let rejection = session
        .submit(
            &envelope(&["ghost@example.com", "phantom@example.com"]),
            MIME,
            in_five_minutes(),
        )
        .await
        .expect_err("every recipient refused");
    assert_rejection(
        &rejection,
        Failure::Permanent,
        Phase::RcptTo,
        Scope::Recipient,
        Cause::Refused,
    );
    assert_eq!(rejection.code, Some(550));
    assert_eq!(rejection.refused.len(), 2);
    assert!(!transcript.has("DATA"));
}

/// A reply to one `RCPT TO` that speaks for the session (a throttle, or a `421` closing the
/// channel) stops the submission at once, before the next recipient and before `DATA`, so nothing
/// is sent and the whole message can be retried after the pause.
#[tokio::test]
async fn a_session_level_refusal_at_rcpt_stops_before_data() {
    let cases = [
        ("451 4.7.1 Rate limited, try again later", Cause::Throttled),
        (
            "421 Service not available, closing transmission channel",
            Cause::Refused,
        ),
    ];
    for (answer, cause) in cases {
        let (port, transcript) = smtp_server(move |line| {
            if line == "RCPT TO:<grace@example.com>" {
                reply(answer)
            } else {
                smtp_default(line)
            }
        })
        .await;
        let pool = pool(PoolConfig::default());
        let session = authenticated(&pool, port).await;
        let rejection = session
            .submit(
                &envelope(&["grace@example.com", "linus@example.com"]),
                MIME,
                in_five_minutes(),
            )
            .await
            .expect_err("stopped");
        assert_rejection(
            &rejection,
            Failure::Transient,
            Phase::RcptTo,
            Scope::Connection,
            cause,
        );
        assert!(!transcript.has("RCPT TO:<linus@example.com>"), "{answer}");
        assert!(!transcript.has("DATA"), "{answer}");
    }
}

/// `DATA` is sent only while 180 s of the budget remain (30 s for the command, 30 s for the
/// content, 120 s for the final reply): just under that, the submission stops before `DATA` as
/// transient, because a budget that ran out after the content would leave the outcome unknown;
/// just over it, `DATA` and the content are sent.
#[tokio::test]
async fn data_is_sent_only_with_180_seconds_of_budget() {
    for (seconds, sent) in [(179, false), (181, true)] {
        let (port, transcript) = smtp_server(smtp_default).await;
        let password = secret();
        let auth = password_auth(&password);
        let session = pool(PoolConfig::default())
            .session(&plain(port), &auth, in_five_minutes())
            .await
            .expect("a session");
        let result = session
            .submit(
                &envelope(&["grace@example.com"]),
                MIME,
                Instant::now() + Duration::from_secs(seconds),
            )
            .await;
        assert_eq!(transcript.has("DATA"), sent, "{seconds} s");
        match result {
            Ok(_) => assert!(sent, "{seconds} s"),
            Err(rejection) => {
                assert!(!sent, "{seconds} s: {rejection:?}");
                assert_rejection(
                    &rejection,
                    Failure::Transient,
                    Phase::Data,
                    Scope::Connection,
                    Cause::Deadline,
                );
                assert!(transcript.has("RCPT TO:<grace@example.com>"));
            }
        }
    }
}

/// A deadline that passed before `MAIL FROM` sends nothing at all: the message is untouched and
/// may be submitted again.
#[tokio::test]
async fn a_deadline_passed_before_mail_from_sends_nothing() {
    let (port, transcript) = smtp_server(smtp_default).await;
    let pool = pool(PoolConfig::default());
    let password = secret();
    let auth = password_auth(&password);
    let session = pool
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a session");
    let rejection = session
        .submit(&envelope(&["grace@example.com"]), MIME, Instant::now())
        .await
        .expect_err("too late");
    assert_rejection(
        &rejection,
        Failure::Transient,
        Phase::MailFrom,
        Scope::Connection,
        Cause::Deadline,
    );
    assert!(!transcript.has("MAIL FROM"));
}

/// Once the server answered `354` and the content was sent, a connection lost before the final
/// reply may hide an acceptance: the outcome is uncertain, so the message is never resent
/// automatically (that could deliver it twice).
#[tokio::test]
async fn a_connection_lost_after_the_content_is_uncertain() {
    let (port, transcript) = smtp_server(|line| {
        if line == "." {
            Smtp::Close
        } else {
            smtp_default(line)
        }
    })
    .await;
    let pool = pool(PoolConfig::default());
    let password = secret();
    let auth = password_auth(&password);
    let session = pool
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a session");
    let rejection = session
        .submit(&envelope(&["grace@example.com"]), MIME, in_five_minutes())
        .await
        .expect_err("no reply");
    assert_rejection(
        &rejection,
        Failure::Uncertain,
        Phase::Data,
        Scope::Connection,
        Cause::NoReply,
    );
    assert!(transcript.has("content:"));
}

/// A negative final reply after the content keeps its meaning through the session: here
/// Exchange's quota exception, a `554` that must pause the connection rather than fail the
/// message.
#[tokio::test]
async fn a_negative_final_reply_is_classified() {
    let (port, _transcript) = smtp_server(|line| {
        if line == "." {
            reply("554 5.2.0 STOREDRV.Submission.Exception:SubmissionQuotaExceededException; Failed to process message")
        } else {
            smtp_default(line)
        }
    })
    .await;
    let pool = pool(PoolConfig::default());
    let session = authenticated(&pool, port).await;
    let rejection = session
        .submit(&envelope(&["grace@example.com"]), MIME, in_five_minutes())
        .await
        .expect_err("refused");
    assert_rejection(
        &rejection,
        Failure::Transient,
        Phase::Data,
        Scope::Connection,
        Cause::Throttled,
    );
    assert_eq!(rejection.code, Some(554));
}

/// A server that will not take the credential makes the connection need a new one, never fails a
/// message: a `5xx` to `AUTH`, and a server offering no mechanism for it (Exchange Online with
/// SMTP AUTH turned off), are both transient, in the `auth` phase, about the connection, with the
/// cause that asks the connection's owner to act.
#[tokio::test]
async fn a_refused_credential_is_transient_and_unauthorized() {
    let cases: [(Script, Option<u16>); 2] = [
        (
            |line| {
                if line.starts_with("AUTH") {
                    reply("535 5.7.8 Username and Password not accepted")
                } else {
                    smtp_default(line)
                }
            },
            Some(535),
        ),
        (
            |line| {
                if line.starts_with("EHLO") {
                    reply("250-fake.test\r\n250 8BITMIME")
                } else {
                    smtp_default(line)
                }
            },
            None,
        ),
    ];
    for (script, code) in cases {
        let (port, _transcript) = smtp_server(script).await;
        let password = secret();
        let auth = password_auth(&password);
        let rejection = pool(PoolConfig::default())
            .session(&plain(port), &auth, in_five_minutes())
            .await
            .expect_err("refused");
        assert_rejection(
            &rejection,
            Failure::Transient,
            Phase::Auth,
            Scope::Connection,
            Cause::Unauthorized,
        );
        assert_eq!(rejection.code, code);
    }
}

/// SASL `XOAUTH2` sends `user=<login>^Aauth=Bearer <token>^A^A` with `AUTH`; an accepted token
/// opens the session, and a refused one comes back as a `334` challenge carrying the error, which
/// must be answered with an empty line to read the real `535`, so the refusal is reported as
/// unauthorized rather than as a protocol error.
#[tokio::test]
async fn xoauth2_authenticates_and_reads_a_refusal_through_its_challenge() {
    let (port, transcript) = smtp_server(|line| {
        let response = line
            .strip_prefix("AUTH XOAUTH2 ")
            .and_then(|encoded| STANDARD.decode(encoded).ok());
        if let Some(response) = response {
            if String::from_utf8_lossy(&response).contains("auth=Bearer ya29.token\x01") {
                smtp_default(line)
            } else {
                reply("334 eyJzdGF0dXMiOiI0MDAiLCJzY2hlbWVzIjoiQmVhcmVyIn0=")
            }
        } else if line.is_empty() {
            reply("535 5.7.8 Username and Password not accepted")
        } else {
            smtp_default(line)
        }
    })
    .await;
    let pool = pool(PoolConfig::default());
    let good = SecretString::from("ya29.token".to_owned());
    let auth = SmtpAuth::Xoauth2 {
        username: "ada@example.com",
        token: &good,
    };
    pool.session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("an accepted token");
    let initial = transcript
        .lines()
        .into_iter()
        .find_map(|line| line.strip_prefix("AUTH XOAUTH2 ").map(str::to_owned))
        .expect("an initial response");
    assert_eq!(
        STANDARD.decode(initial).expect("base64"),
        b"user=ada@example.com\x01auth=Bearer ya29.token\x01\x01"
    );

    let refused = SecretString::from("ya29.expired".to_owned());
    let auth = SmtpAuth::Xoauth2 {
        username: "ada@example.com",
        token: &refused,
    };
    let rejection = pool
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect_err("refused");
    assert_rejection(
        &rejection,
        Failure::Transient,
        Phase::Auth,
        Scope::Connection,
        Cause::Unauthorized,
    );
}

/// Before the server answered `354` to `DATA`, a lost or refused reply cannot hide an
/// acceptance: a connection closed at `RCPT TO` or at `DATA`, a refused `DATA`, and a server that
/// stops answering until the deadline are all transient for their phase, never uncertain, so the
/// message can be submitted again.
#[tokio::test]
async fn failures_before_the_content_are_transient() {
    let cases: [(Script, u64, Phase, Cause); 4] = [
        (
            |line| {
                if line.starts_with("RCPT") {
                    Smtp::Close
                } else {
                    smtp_default(line)
                }
            },
            300_000,
            Phase::RcptTo,
            Cause::NoReply,
        ),
        (
            |line| {
                if line == "DATA" {
                    Smtp::Close
                } else {
                    smtp_default(line)
                }
            },
            300_000,
            Phase::Data,
            Cause::NoReply,
        ),
        (
            |line| {
                if line == "DATA" {
                    reply("451 4.3.0 Try again later")
                } else {
                    smtp_default(line)
                }
            },
            300_000,
            Phase::Data,
            Cause::Refused,
        ),
        (
            |line| {
                if line.starts_with("MAIL") {
                    Smtp::Silent
                } else {
                    smtp_default(line)
                }
            },
            300,
            Phase::MailFrom,
            Cause::NoReply,
        ),
    ];
    for (script, millis, phase, cause) in cases {
        let (port, transcript) = smtp_server(script).await;
        let pool = pool(PoolConfig::default());
        let password = secret();
        let auth = password_auth(&password);
        let session = pool
            .session(&plain(port), &auth, in_five_minutes())
            .await
            .expect("a session");
        let rejection = session
            .submit(
                &envelope(&["grace@example.com"]),
                MIME,
                Instant::now() + Duration::from_millis(millis),
            )
            .await
            .expect_err("failed");
        assert_eq!(
            (rejection.failure, rejection.phase, rejection.cause),
            (Failure::Transient, phase, cause),
            "{rejection:?}"
        );
        assert!(!transcript.has("content:"));
    }
}

/// `STARTTLS` is required, never opportunistic: a server that does not offer it is refused
/// before any credential is sent in plaintext.
#[tokio::test]
async fn a_server_without_starttls_is_refused_before_auth() {
    let (port, transcript) = smtp_server(without_extensions).await;
    let pool = pool(PoolConfig::default());
    let password = secret();
    let auth = password_auth(&password);
    let server = SmtpServer {
        host: "127.0.0.1",
        port,
        security: SmtpSecurity::StartTls,
    };
    let rejection = pool
        .session(&server, &auth, in_five_minutes())
        .await
        .expect_err("no STARTTLS");
    assert_rejection(
        &rejection,
        Failure::Transient,
        Phase::Connect,
        Scope::Connection,
        Cause::Refused,
    );
    assert!(!transcript.has("AUTH"));
}

/// For tenant-typed hosts plaintext is refused, and so is any host that resolves to a private
/// address, before a socket opens.
#[tokio::test]
async fn tenant_hosts_cannot_be_plaintext_or_private() {
    let pool = SmtpPool::new(connector(AddressPolicy::PublicOnly), PoolConfig::default());
    let password = secret();
    let auth = password_auth(&password);
    let plaintext = pool
        .session(&plain(25), &auth, in_five_minutes())
        .await
        .expect_err("plaintext");
    assert_rejection(
        &plaintext,
        Failure::Transient,
        Phase::Connect,
        Scope::Connection,
        Cause::Refused,
    );
    let private = SmtpServer {
        host: "127.0.0.1",
        port: 465,
        security: SmtpSecurity::Tls,
    };
    let rejection = pool
        .session(&private, &auth, in_five_minutes())
        .await
        .expect_err("private");
    assert_rejection(
        &rejection,
        Failure::Transient,
        Phase::Connect,
        Scope::Connection,
        Cause::NoReply,
    );
}

/// Content with bytes beyond ASCII travels as `BODY=8BITMIME` when the server offers the
/// extension, and is refused as unsupported before `MAIL FROM` when it does not, since sending it
/// anyway would corrupt the message.
#[tokio::test]
async fn eight_bit_content_needs_8bitmime() {
    let content = "Subject: café\r\n\r\nolé\r\n".as_bytes();
    let password = secret();
    let auth = password_auth(&password);

    let (port, transcript) = smtp_server(smtp_default).await;
    let session = pool(PoolConfig::default())
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a session");
    session
        .submit(
            &envelope(&["grace@example.com"]),
            content,
            in_five_minutes(),
        )
        .await
        .expect("accepted");
    assert!(transcript.has("MAIL FROM:<ada@example.com> BODY=8BITMIME"));

    let (port, transcript) = smtp_server(without_extensions).await;
    let session = pool(PoolConfig::default())
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a session");
    let rejection = session
        .submit(
            &envelope(&["grace@example.com"]),
            content,
            in_five_minutes(),
        )
        .await
        .expect_err("unsupported");
    assert_rejection(
        &rejection,
        Failure::Permanent,
        Phase::MailFrom,
        Scope::Message,
        Cause::Unsupported,
    );
    assert!(!transcript.has("MAIL FROM"));
}

/// A credential never holds more than `max_open` sessions, in use or parked: a second request
/// waits for one to free and gives up at its deadline.
#[tokio::test]
async fn a_credential_holds_at_most_max_open_sessions() {
    let (port, _transcript) = smtp_server(smtp_default).await;
    let pool = pool(PoolConfig {
        max_open: 1,
        ..PoolConfig::default()
    });
    let password = secret();
    let auth = password_auth(&password);
    let held = pool
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a session");
    let rejection = pool
        .session(
            &plain(port),
            &auth,
            Instant::now() + Duration::from_millis(200),
        )
        .await
        .expect_err("no free session");
    assert_rejection(
        &rejection,
        Failure::Transient,
        Phase::Connect,
        Scope::Connection,
        Cause::Deadline,
    );
    drop(held);
}

/// The authentication probe proves the current credential, so it never borrows a parked session
/// (that proves an earlier login): it opens its own, authenticates and quits.
#[tokio::test]
async fn the_probe_authenticates_on_a_fresh_session_and_quits() {
    let (port, transcript) = smtp_server(smtp_default).await;
    let pool = pool(PoolConfig::default());
    let password = secret();
    let auth = password_auth(&password);
    drop(
        pool.session(&plain(port), &auth, in_five_minutes())
            .await
            .expect("a parked session"),
    );
    pool.probe(&plain(port), &auth, in_five_minutes())
        .await
        .expect("the credential works");
    assert_eq!(transcript.count("connect"), 2);
    assert!(transcript.has("QUIT"));
}

/// SASL separates its fields with control characters, so a login or token carrying one (or a
/// password with the `PLAIN` separator) could forge fields: such credentials are refused before
/// any connection.
#[test]
fn credentials_that_could_forge_sasl_fields_are_refused() {
    let forged = SecretString::from("token\x01auth=Bearer other".to_owned());
    let nul = SecretString::from("pass\0word".to_owned());
    let fine = secret();
    for auth in [
        SmtpAuth::Xoauth2 {
            username: "ada@example.com",
            token: &forged,
        },
        SmtpAuth::Password {
            username: "ada@example.com",
            password: &nul,
        },
        SmtpAuth::Password {
            username: "ada\r\n@example.com",
            password: &fine,
        },
    ] {
        let rejection = check_credential(&auth).expect_err("refused");
        assert_eq!(rejection.cause, Cause::Unauthorized);
    }
    assert!(
        check_credential(&SmtpAuth::Password {
            username: "ada@example.com",
            password: &fine
        })
        .is_ok()
    );
}

/// A parked session is reused only if it still answers: one the server closed while it was
/// parked fails its `NOOP` and is replaced by a new session, so a stale socket never costs a
/// submission.
#[tokio::test]
async fn a_parked_session_the_server_closed_is_replaced() {
    let (port, transcript) = smtp_server(|line| {
        if line == "NOOP" {
            Smtp::Close
        } else {
            smtp_default(line)
        }
    })
    .await;
    let pool = pool(PoolConfig::default());
    let password = secret();
    let auth = password_auth(&password);
    let session = pool
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a session");
    session
        .submit(&envelope(&["grace@example.com"]), MIME, in_five_minutes())
        .await
        .expect("accepted");
    let replacement = pool
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a new session");
    replacement
        .submit(&envelope(&["grace@example.com"]), MIME, in_five_minutes())
        .await
        .expect("accepted");
    assert_eq!(transcript.count("connect"), 2);
}

/// Sessions are not kept beyond what the provider tolerates: one idle past the idle timeout is
/// not reused, and one that reached its message count is closed instead of parked (relays such as
/// Amazon SES ask for rotation).
#[tokio::test]
async fn sessions_retire_after_their_idle_timeout_or_message_count() {
    let password = secret();
    let auth = password_auth(&password);

    let (port, transcript) = smtp_server(smtp_default).await;
    let idle = pool(PoolConfig {
        idle_timeout: Duration::from_millis(50),
        ..PoolConfig::default()
    });
    drop(
        idle.session(&plain(port), &auth, in_five_minutes())
            .await
            .expect("a session"),
    );
    tokio::time::sleep(Duration::from_millis(120)).await;
    drop(
        idle.session(&plain(port), &auth, in_five_minutes())
            .await
            .expect("a new session"),
    );
    assert_eq!(
        (transcript.count("connect"), transcript.count("NOOP")),
        (2, 0)
    );

    let (port, transcript) = smtp_server(smtp_default).await;
    let rotating = pool(PoolConfig {
        max_messages: 1,
        ..PoolConfig::default()
    });
    let session = rotating
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a session");
    session
        .submit(&envelope(&["grace@example.com"]), MIME, in_five_minutes())
        .await
        .expect("accepted");
    drop(
        rotating
            .session(&plain(port), &auth, in_five_minutes())
            .await
            .expect("a new session"),
    );
    assert_eq!(transcript.count("connect"), 2);
}

/// An address beyond ASCII travels with `SMTPUTF8` when the server offers it (RFC 6531) and is
/// refused as unsupported before `MAIL FROM` when it does not.
#[tokio::test]
async fn internationalised_addresses_need_smtputf8() {
    let password = secret();
    let auth = password_auth(&password);
    let envelope = envelope(&["josé@example.com"]);

    let (port, transcript) = smtp_server(smtp_default).await;
    let session = pool(PoolConfig::default())
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a session");
    session
        .submit(&envelope, MIME, in_five_minutes())
        .await
        .expect("accepted");
    assert!(transcript.has("MAIL FROM:<ada@example.com> SMTPUTF8"));

    let (port, transcript) = smtp_server(without_extensions).await;
    let session = pool(PoolConfig::default())
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a session");
    let rejection = session
        .submit(&envelope, MIME, in_five_minutes())
        .await
        .expect_err("unsupported");
    assert_rejection(
        &rejection,
        Failure::Permanent,
        Phase::MailFrom,
        Scope::Message,
        Cause::Unsupported,
    );
    assert!(!transcript.has("MAIL FROM"));
}

/// Pools sharing a cap hold at most its sessions together: with one place, a session busy in one
/// pool makes another pool's request wait until its deadline (the process's own bound, scoped to
/// the platform so no connection's circuit breaker counts it), and once parked it is closed to
/// give its place to the other pool. An SMTP worker process so never holds more sockets than its
/// budget, and an idle pool gives way to a busy one.
#[tokio::test]
async fn pools_sharing_a_cap_hold_at_most_its_sessions() {
    let (port, _transcript) = smtp_server(smtp_default).await;
    let cap = SessionCap::new(1);
    let first = SmtpPool::capped(connector(AddressPolicy::Any), PoolConfig::default(), &cap);
    let second = SmtpPool::capped(connector(AddressPolicy::Any), PoolConfig::default(), &cap);
    let password = secret();
    let auth = password_auth(&password);
    let held = first
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("a session");
    let waited = second
        .session(
            &plain(port),
            &auth,
            Instant::now() + Duration::from_millis(200),
        )
        .await
        .expect_err("no place is free");
    assert_rejection(
        &waited,
        Failure::Transient,
        Phase::Connect,
        Scope::Platform,
        Cause::Deadline,
    );
    // Dropped without a transaction, the session is parked, still holding the place.
    drop(held);
    let taken = second
        .session(&plain(port), &auth, in_five_minutes())
        .await
        .expect("the parked session's place");
    drop(taken);
}

/// Authenticates one session on the fake SMTP server with the common test password.
async fn authenticated(pool: &SmtpPool, port: u16) -> crate::smtp::SmtpSession {
    let password = secret();
    pool.session(&plain(port), &password_auth(&password), in_five_minutes())
        .await
        .expect("a session")
}
