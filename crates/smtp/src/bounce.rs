//! Bounces through VERP return paths: delivery status notifications (RFC 3464) that remote
//! servers send back to a message's own return path become `bounced` (or `deferred`) events in
//! the outbox ([`crate::events`]). Without them the MTA would know only what Postfix learns while
//! delivering; a receiver that accepts a message and refuses it later reports only to the
//! envelope sender.
//!
//! **The return path.** `bounce+<token>@<mail host>`, the token 16 to 56 lowercase letters and
//! digits, so the local part stays within SMTP's 64 octets. Whoever submits through a relay
//! login (the core's sender) sets it as the envelope sender (`MAIL FROM`), one token per message
//! (our message id as 32 hex digits, for example); the helper allows relay logins that sender
//! ([`crate::provision`]). The token is not a secret and carries no signature: recipients see
//! it in `Return-Path`. What proves that a notification concerns our message, and selects its
//! tenant, is the MTA's own record: the tail ([`crate::tail`]) stores `returns(token → login,
//! Message-ID, queue id)` when it reads the `qmgr` line of an authenticated submission with that
//! envelope sender. A notification for a token that was never recorded is refused for good
//! (`550`): the tail records a submission within seconds of Postfix logging it, long before any
//! remote server reports on it.
//!
//! **Delivery to the service.** Postfix routes `bounce+…@<mail host>` to this module's LMTP
//! listener (RFC 2033, <https://www.rfc-editor.org/rfc/rfc2033>) through `transport_maps`, so a
//! notification is never stored in a mailbox. The same listener takes the feedback address
//! `fbl@<mail host>`, whose abuse reports [`crate::feedback`] turns into complaints. `RCPT` is
//! answered `550` for an address that is neither the feedback address nor a recorded return
//! path; after `DATA` (at most 4 MiB of retained evidence, else `552`) each accepted recipient
//! gets its own reply, in order: `250` once its events are recorded or the message is ignored,
//! `451` when the database fails (Postfix retries). Bare LF line ends are accepted; lines are
//! dot-unstuffed.
//!
//! **What is recorded.** The message is parsed with `norbelys_mail::dsn::parse`. Each recipient
//! group with `Action: failed` becomes a `bounced` event and each with `Action: delayed` a
//! `deferred` one, for the group's `Final-Recipient` (else `Original-Recipient`), with its
//! `Status` and `Diagnostic-Code`; other actions report no failure and are skipped. The event's
//! Message-ID, queue id and login come from the MTA's record, never from the notification; its
//! route is the login's route now, and a login without one gets nothing (counted). Event ids
//! are SHA-256 of the node, the token, the notification's digest and the group's position, so a
//! notification delivered twice records nothing new. Not recorded, but accepted: a message that
//! is not a delivery status notification (an auto-reply sent to the return path, a free-text
//! notice), and a notification whose returned `Message-ID` is another message's (forged or
//! misrouted).
//!
//! **Confidence.** `provenance` is `verp_dsn`: the record proves the notification concerns our
//! message, but nothing authenticates whoever wrote it, so the core treats these events as
//! corroborated, never as authenticated, and does not suppress an address on their strength
//! alone.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::db::{Connection, OptionalExtension as _};
use jiff::Timestamp;
use norbelys_mail::dsn::{self, Action};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;

use crate::crypto;
use crate::db::{self, Db};
use crate::events::{self, Evidence, Journaled};
use crate::feedback;
use crate::serve::Shutdown;
use crate::telemetry;

/// The local part every return path starts with.
pub const PREFIX: &str = "bounce+";
/// The shortest token.
pub const TOKEN_MIN: usize = 16;
/// The longest token: `bounce+` and the token stay within SMTP's 64-octet local part.
pub const TOKEN_MAX: usize = 56;
/// Maximum retained evidence. DSNs stream and discard returned bodies; feedback reports
/// retain complete signed bytes and are refused when they exceed this bound.
const MAX_MESSAGE: usize = 4 * 1024 * 1024;
/// The longest LMTP command line kept.
const MAX_COMMAND: usize = 4096;
/// The longest message line kept; longer lines are cut, which no DSN field needs.
const MAX_LINE: usize = 64 * 1024;

/// The token of `address` when it is a return path of this MTA (`mail_host`).
#[must_use]
pub fn token<'a>(address: &'a str, mail_host: &str) -> Option<&'a str> {
    let (local, domain) = address.rsplit_once('@')?;
    let token = local.strip_prefix(PREFIX)?;
    let valid = domain.eq_ignore_ascii_case(mail_host)
        && (TOKEN_MIN..=TOKEN_MAX).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    valid.then_some(token)
}

/// What the MTA recorded about a return path from the authenticated submission that used it.
pub(crate) struct Return {
    /// The login that submitted the message.
    pub(crate) username: String,
    /// The message's `Message-ID`, angle brackets included, when it had one.
    pub(crate) internet_message_id: Option<String>,
    /// Postfix's queue id of the submission.
    pub(crate) queue_id: String,
}

/// The record of the return path `token`, when the tail made one (within seven days).
///
/// # Errors
///
/// The query fails.
pub(crate) fn lookup(conn: &Connection, token: &str) -> db::Result<Option<Return>> {
    conn.query_row(
        "SELECT username, internet_message_id, queue_id FROM returns WHERE token = ?1",
        [token],
        |row| {
            Ok(Return {
                username: row.get(0)?,
                internet_message_id: row.get(1)?,
                queue_id: row.get(2)?,
            })
        },
    )
    .optional()
}

/// The event kind of a recipient group's action, or `None` when it reports no failure.
fn kind(action: Option<Action>) -> Option<&'static str> {
    match action? {
        Action::Failed => Some("bounced"),
        Action::Delayed => Some("deferred"),
        Action::Delivered | Action::Relayed | Action::Expanded => None,
    }
}

/// What one notification produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// Events recorded, and events whose login reports to no route.
    Events {
        /// New events.
        inserted: usize,
        /// Events not recorded because the login has no route.
        unrouted: usize,
    },
    /// Accepted, but not evidence: why.
    Ignored(&'static str),
}

/// Why a notification was not taken.
#[derive(Debug, thiserror::Error)]
pub enum BounceError {
    /// No authenticated submission used this return path (or its record expired).
    #[error("the return path was never recorded")]
    Unknown,
    /// The database failed; nothing was recorded.
    #[error("database: {0}")]
    Database(#[from] db::Error),
    /// The blocking task failed.
    #[error("blocking task: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Records the events of the notification `raw`, delivered to the return path `token`, as
/// observed at `now`, in one transaction.
///
/// # Errors
///
/// The token was never recorded, or the database fails.
pub fn record(
    conn: &mut Connection,
    node: &str,
    token: &str,
    raw: &[u8],
    now: Timestamp,
) -> Result<Recorded, BounceError> {
    let tx = conn.transaction()?;
    let Some(origin) = lookup(&tx, token)? else {
        return Err(BounceError::Unknown);
    };
    let Some(report) = dsn::parse(raw) else {
        return Ok(Recorded::Ignored("not a delivery status notification"));
    };
    if let (Some(recorded), Some(returned)) = (
        origin.internet_message_id.as_deref(),
        report.original_message_id.as_deref(),
    ) && recorded.trim_start_matches('<').trim_end_matches('>') != returned
    {
        return Ok(Recorded::Ignored(
            "the returned Message-ID is another message's",
        ));
    }
    let digest = crypto::sha256_hex(raw);
    let observed_at = now.to_string();
    let (mut inserted, mut unrouted) = (0, 0);
    for (index, group) in report.recipients.iter().enumerate() {
        let Some(kind) = kind(group.action) else {
            continue;
        };
        let Some(recipient) = group
            .final_recipient
            .as_deref()
            .or(group.original_recipient.as_deref())
        else {
            continue;
        };
        let event_id =
            crypto::sha256_hex(format!("{node}:verp:{token}:{digest}:{index}").as_bytes());
        let status = group.status.map(|status| status.to_string());
        let journaled = events::journal(
            &tx,
            &Evidence {
                event_id: &event_id,
                internet_message_id: origin.internet_message_id.as_deref(),
                username: &origin.username,
                recipient: Some(recipient),
                queue_id: Some(&origin.queue_id),
                kind,
                enhanced_status: status.as_deref(),
                detail: group
                    .diagnostic
                    .as_deref()
                    .map(|detail| detail.chars().take(2000).collect()),
                provenance: "verp_dsn",
                observed_at: observed_at.clone(),
            },
        )?;
        match journaled {
            Journaled::Inserted => inserted += 1,
            Journaled::Unrouted => unrouted += 1,
            Journaled::Duplicate => {}
        }
    }
    tx.commit()?;
    Ok(Recorded::Events { inserted, unrouted })
}

/// What the listener's sessions share.
struct Context {
    db: Db,
    node: String,
    mail_host: String,
    outcomes: Counter<u64>,
    /// What the feedback address needs: reports are taken by [`crate::feedback`].
    intake: feedback::Intake,
}

/// A recipient accepted in one LMTP transaction.
enum Recipient {
    /// A VERP return path, by its token.
    Return(String),
    /// The feedback address.
    Feedback,
}

/// Serves LMTP on `addr` until shutdown: notifications to VERP return paths, and feedback
/// reports to the feedback address through `intake`.
///
/// # Errors
///
/// The listener cannot bind.
pub async fn serve(
    addr: SocketAddr,
    db: Db,
    node: String,
    mail_host: String,
    intake: feedback::Intake,
    shutdown: Shutdown,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "bounce LMTP listening");
    let context = Context {
        db,
        node,
        mail_host,
        intake,
        outcomes: telemetry::meter()
            .u64_counter("norbelys_mta_notifications")
            .with_description(
                "Notifications returned to VERP paths by outcome: recorded, ignored, refused, failed",
            )
            .build(),
    };
    listen(listener, Arc::new(context), shutdown).await
}

/// Accepts LMTP sessions on `listener` until shutdown, each in its own task.
async fn listen(
    listener: TcpListener,
    context: Arc<Context>,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let context = Arc::clone(&context);
                    let span = tracing::info_span!("smtp.lmtp", otel.kind = "server", otel.status_code = tracing::field::Empty);
                    use tracing::Instrument as _;
                    tokio::spawn(async move {
                        if let Err(error) = session(stream, &context).await {
                            tracing::Span::current().record("otel.status_code", "ERROR");
                            tracing::debug!(error_kind = ?error.kind(), "LMTP session ended");
                        }
                    }.instrument(span));
                }
                Err(error) => {
                    tracing::warn!(error = %error, "LMTP accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            () = shutdown.wait() => return Ok(()),
        }
    }
}

/// One LMTP session (RFC 2033): one reply per command, and after `DATA` one reply per accepted
/// recipient, in order.
async fn session(stream: TcpStream, context: &Context) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let greeting = format!("220 {} LMTP norbelys-smtp", context.mail_host);
    reply(&mut write, &greeting).await?;
    let mut sender = false;
    let mut recipients: Vec<Recipient> = Vec::new();
    let mut line = Vec::new();
    loop {
        if !read_line(&mut reader, &mut line, MAX_COMMAND).await? {
            return Ok(());
        }
        let command = String::from_utf8_lossy(&line).into_owned();
        let verb = command
            .split([' ', ':'])
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        match verb.as_str() {
            "LHLO" => {
                let capabilities = format!(
                    "250-{}\r\n250-PIPELINING\r\n250-ENHANCEDSTATUSCODES\r\n250 8BITMIME",
                    context.mail_host
                );
                reply(&mut write, &capabilities).await?;
            }
            "MAIL" => {
                sender = true;
                recipients.clear();
                reply(&mut write, "250 2.1.0 OK").await?;
            }
            "RCPT" if !sender => reply(&mut write, "503 5.5.1 MAIL first").await?,
            "RCPT" => {
                let address = path(&command);
                let answer = if address
                    .is_some_and(|address| feedback::is_address(address, &context.mail_host))
                {
                    recipients.push(Recipient::Feedback);
                    "250 2.1.5 OK"
                } else {
                    match address.and_then(|address| token(address, &context.mail_host)) {
                        None => "550 5.1.1 Not a return path of this host",
                        Some(found) => match known(context, found).await {
                            Ok(true) => {
                                recipients.push(Recipient::Return(found.to_owned()));
                                "250 2.1.5 OK"
                            }
                            Ok(false) => "550 5.1.1 Unknown return path",
                            Err(error) => {
                                tracing::error!(error = %error, "return path lookup failed");
                                "451 4.3.0 Temporary failure, try again later"
                            }
                        },
                    }
                };
                reply(&mut write, answer).await?;
            }
            "DATA" if recipients.is_empty() => {
                reply(&mut write, "503 5.5.1 No valid recipients").await?
            }
            "DATA" => {
                reply(&mut write, "354 End data with <CR><LF>.<CR><LF>").await?;
                let compact = recipients
                    .iter()
                    .all(|recipient| matches!(recipient, Recipient::Return(_)));
                let message = read_data(&mut reader, compact).await?;
                for recipient in std::mem::take(&mut recipients) {
                    let answer = match (&message, recipient) {
                        (None, _) => "552 5.3.4 Message too big",
                        (Some(message), Recipient::Return(token)) => {
                            receive(context, &token, message).await
                        }
                        (Some(message), Recipient::Feedback) => {
                            feedback::receive(
                                &context.db,
                                &context.node,
                                &context.mail_host,
                                &context.intake,
                                message,
                            )
                            .await
                        }
                    };
                    reply(&mut write, answer).await?;
                }
                sender = false;
            }
            "RSET" => {
                sender = false;
                recipients.clear();
                reply(&mut write, "250 2.0.0 OK").await?;
            }
            "NOOP" => reply(&mut write, "250 2.0.0 OK").await?,
            "QUIT" => {
                reply(&mut write, "221 2.0.0 Bye").await?;
                return Ok(());
            }
            _ => reply(&mut write, "502 5.5.2 Command not recognised").await?,
        }
    }
}

/// Whether a return path was recorded.
async fn known(context: &Context, token: &str) -> Result<bool, BounceError> {
    let token = token.to_owned();
    context
        .db
        .call(move |conn| Ok::<_, BounceError>(lookup(conn, &token)?.is_some()))
        .await
}

/// Records one notification for one return path; the LMTP reply that says how it went.
async fn receive(context: &Context, token: &str, message: &Arc<Vec<u8>>) -> &'static str {
    let (node, owned, raw) = (context.node.clone(), token.to_owned(), Arc::clone(message));
    let result = context
        .db
        .call(move |conn| record(conn, &node, &owned, &raw, Timestamp::now()))
        .await;
    telemetry::unit(telemetry::Event::Bounce);
    let (outcome, answer) = match &result {
        Ok(Recorded::Events { inserted, unrouted }) => {
            tracing::info!(
                event = "mta.bounce",
                token,
                events = inserted,
                unrouted,
                outcome = "recorded",
                "mta.bounce"
            );
            ("recorded", "250 2.0.0 Recorded")
        }
        Ok(Recorded::Ignored(reason)) => {
            tracing::info!(
                event = "mta.bounce",
                token,
                reason,
                outcome = "ignored",
                "mta.bounce"
            );
            ("ignored", "250 2.0.0 Accepted, not a delivery report")
        }
        Err(BounceError::Unknown) => {
            tracing::warn!(
                event = "mta.bounce",
                token,
                outcome = "refused",
                "mta.bounce"
            );
            ("refused", "550 5.1.1 Unknown return path")
        }
        Err(error) => {
            tracing::error!(event = "mta.bounce", token, error = %error, outcome = "failed", "mta.bounce");
            ("failed", "451 4.3.0 Temporary failure, try again later")
        }
    };
    context
        .outcomes
        .add(1, &[KeyValue::new("outcome", outcome)]);
    answer
}

/// The address between the angle brackets of `RCPT TO:<address> …`.
fn path(command: &str) -> Option<&str> {
    let (_, rest) = command.split_once('<')?;
    let (address, _) = rest.split_once('>')?;
    Some(address.trim())
}

async fn reply(write: &mut OwnedWriteHalf, text: &str) -> std::io::Result<()> {
    write.write_all(text.as_bytes()).await?;
    write.write_all(b"\r\n").await
}

/// Reads one line into `line` without its CRLF or LF, keeping at most `limit` bytes of it;
/// `false` at the end of the stream.
async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<bool> {
    line.clear();
    let mut chunk = Vec::new();
    loop {
        chunk.clear();
        let read = (&mut *reader)
            .take(8192)
            .read_until(b'\n', &mut chunk)
            .await?;
        if read == 0 {
            return Ok(!line.is_empty());
        }
        let room = limit.saturating_sub(line.len());
        line.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
        if chunk.last() == Some(&b'\n') {
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            return Ok(true);
        }
    }
}

/// The message after `DATA`, dot-unstuffed, with CRLF line ends; `None` when it exceeds
/// [`MAX_MESSAGE`] of retained evidence (the full input is drained to preserve session framing).
/// DSN-only delivery discards returned bodies; feedback keeps the signed original bytes.
async fn read_data<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    compact: bool,
) -> std::io::Result<Option<Arc<Vec<u8>>>> {
    let mut reduced = compact.then(|| dsn::StreamReducer::new(MAX_MESSAGE));
    let mut message = Vec::new();
    let mut fits = true;
    let mut line = Vec::new();
    loop {
        if !read_line(reader, &mut line, MAX_LINE).await? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the client closed the session inside DATA",
            ));
        }
        if line == b"." {
            return Ok(match reduced {
                Some(reduced) => reduced.finish().map(Arc::new),
                None => fits.then(|| Arc::new(message)),
            });
        }
        let unstuffed = if line.starts_with(b"..") {
            line.get(1..).unwrap_or_default()
        } else {
            line.as_slice()
        };
        if let Some(reduced) = &mut reduced {
            reduced.line(unstuffed);
            continue;
        }
        fits = fits && message.len() + unstuffed.len() + 2 <= MAX_MESSAGE;
        if fits {
            message.extend_from_slice(unstuffed);
            message.extend_from_slice(b"\r\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::db::params;

    use super::*;
    use crate::serve::Shutdown;
    use crate::testing::{TempDir, memory};

    const GMAIL: &str = include_str!("../fixtures/dsn-gmail-address-not-found.eml");
    const EXCHANGE: &str = include_str!("../fixtures/dsn-exchange-two-recipients.eml");
    const GMAIL_TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const EXCHANGE_TOKEN: &str = "fedcba9876543210fedcba9876543210";

    /// A database where `relay@example.com` reports to route `pwh_1` and the tail recorded the
    /// return path `token` for the submission of `message_id`.
    fn recorded(conn: &Connection, token: &str, login: &str, message_id: &str) {
        conn.execute_batch(
            "INSERT OR IGNORE INTO domains (name, ownership_token, dkim_selector, created_at) VALUES ('example.com', 't', 's', 'now');
             INSERT OR IGNORE INTO accounts (username, domain, kind, rate_class, created_at) VALUES
               ('relay@example.com', 'example.com', 'relay', 'relay', 'now'),
               ('owner@example.com', 'example.com', 'mailbox', 'customer', 'now');
             INSERT OR IGNORE INTO routes VALUES ('pwh_1', 'https://localhost/webhooks/pwh_1', x'00', 'now');
             INSERT OR IGNORE INTO account_routes VALUES ('relay@example.com', 'pwh_1');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO returns VALUES (?1, ?2, ?3, '4ZQB1xYz', 0)",
            params![token, login, message_id],
        )
        .unwrap();
    }

    /// The listener's shared state over the database at `database`: mail host
    /// `mail.example.com`, no enrolled feedback loop.
    fn context(database: &std::path::Path) -> Context {
        let resolver = hickory_resolver::TokioResolver::builder_tokio()
            .unwrap()
            .build()
            .unwrap();
        Context {
            db: Db::open(database).unwrap(),
            node: "node".to_owned(),
            mail_host: "mail.example.com".to_owned(),
            outcomes: telemetry::meter().u64_counter("test").build(),
            intake: feedback::Intake::new(resolver, Vec::new()),
        }
    }

    fn payloads(conn: &Connection) -> Vec<serde_json::Value> {
        conn.prepare("SELECT payload FROM events ORDER BY rowid")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
            .collect()
    }

    /// A return path is `bounce+` and 16 to 56 lowercase letters or digits on this MTA's own
    /// host; anything else is not one of ours and is refused at `RCPT`.
    #[test]
    fn recognises_return_paths_of_this_host() {
        let host = "mail.example.com";
        assert_eq!(
            token(&format!("bounce+{GMAIL_TOKEN}@MAIL.example.com"), host),
            Some(GMAIL_TOKEN)
        );
        for other in [
            format!("bounce+{GMAIL_TOKEN}@other.example.com"),
            format!("bounce-{GMAIL_TOKEN}@mail.example.com"),
            "bounce+0123456789abcde@mail.example.com".to_owned(),
            format!("bounce+{}@mail.example.com", "a".repeat(57)),
            "bounce+0123456789ABCDEF@mail.example.com".to_owned(),
            "bounce+0123456789abcdef".to_owned(),
        ] {
            assert_eq!(token(&other, host), None, "{other}");
        }
    }

    /// A failed recipient is a bounce and a delayed one a deferral; delivered, relayed and
    /// expanded report no failure and record nothing.
    #[test]
    fn maps_actions_to_event_kinds() {
        assert_eq!(kind(Some(Action::Failed)), Some("bounced"));
        assert_eq!(kind(Some(Action::Delayed)), Some("deferred"));
        for none in [
            Some(Action::Delivered),
            Some(Action::Relayed),
            Some(Action::Expanded),
            None,
        ] {
            assert_eq!(kind(none), None);
        }
    }

    /// A notification records one event per failed recipient group, with the recipient, status
    /// and diagnostic from the report but the Message-ID, queue id and login from the MTA's own
    /// record, `verp_dsn` provenance, and the login's route; delivered again, it records nothing.
    #[test]
    fn records_each_failed_recipient_once() {
        let mut conn = memory();
        recorded(
            &conn,
            EXCHANGE_TOKEN,
            "relay@example.com",
            "<m2.t1.tag@mail.example.com>",
        );
        let now = Timestamp::from_second(1_790_910_000).unwrap();

        let first = record(&mut conn, "node", EXCHANGE_TOKEN, EXCHANGE.as_bytes(), now).unwrap();
        assert_eq!(
            first,
            Recorded::Events {
                inserted: 2,
                unrouted: 0
            }
        );
        let again = record(&mut conn, "node", EXCHANGE_TOKEN, EXCHANGE.as_bytes(), now).unwrap();
        assert_eq!(
            again,
            Recorded::Events {
                inserted: 0,
                unrouted: 0
            }
        );

        let events = payloads(&conn);
        let full = events[0].as_object().unwrap();
        assert_eq!(full["kind"], "bounced");
        assert_eq!(full["recipient"], "full@contoso.com");
        assert_eq!(full["enhanced_status"], "5.2.2");
        assert!(
            full["detail"]
                .as_str()
                .unwrap()
                .starts_with("554 5.2.2 mailbox full")
        );
        assert_eq!(full["internet_message_id"], "<m2.t1.tag@mail.example.com>");
        assert_eq!(full["queue_id"], "4ZQB1xYz");
        assert_eq!(full["username"], "relay@example.com");
        assert_eq!(full["provenance"], "verp_dsn");
        assert_eq!(full["observed_at"], "2026-10-02T03:00:00Z");
        assert_eq!(
            (
                events[1]["recipient"].as_str(),
                events[1]["enhanced_status"].as_str()
            ),
            (Some("gone@contoso.com"), Some("5.1.10"))
        );
        let routed: i64 = conn
            .query_row(
                "SELECT count(*) FROM events WHERE provider_webhook_id = 'pwh_1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(routed, 2);
    }

    /// Nothing is recorded without proof and a tenant: an unrecorded return path is refused; a
    /// report returning another message's Message-ID, or a message that is not a delivery report
    /// (an auto-reply), is ignored; a login without a route records nothing and is counted.
    #[test]
    fn records_nothing_without_proof_or_tenant() {
        let mut conn = memory();
        let now = Timestamp::now();
        assert!(matches!(
            record(&mut conn, "node", EXCHANGE_TOKEN, EXCHANGE.as_bytes(), now),
            Err(BounceError::Unknown)
        ));

        recorded(
            &conn,
            EXCHANGE_TOKEN,
            "relay@example.com",
            "<another@mail.example.com>",
        );
        assert_eq!(
            record(&mut conn, "node", EXCHANGE_TOKEN, EXCHANGE.as_bytes(), now).unwrap(),
            Recorded::Ignored("the returned Message-ID is another message's")
        );
        let auto_reply =
            b"From: grace@example.org\r\nSubject: Out of office\r\n\r\nBack on Monday.\r\n";
        assert_eq!(
            record(&mut conn, "node", EXCHANGE_TOKEN, auto_reply, now).unwrap(),
            Recorded::Ignored("not a delivery status notification")
        );

        recorded(
            &conn,
            GMAIL_TOKEN,
            "owner@example.com",
            "<m1.t1.tag@mail.example.com>",
        );
        assert_eq!(
            record(&mut conn, "node", GMAIL_TOKEN, GMAIL.as_bytes(), now).unwrap(),
            Recorded::Events {
                inserted: 0,
                unrouted: 1
            }
        );
        assert!(payloads(&conn).is_empty());
    }

    /// Postfix delivers a notification over LMTP: an unrecorded return path is refused at
    /// `RCPT`, the dot-stuffed message is accepted, and the one accepted recipient gets one reply
    /// after `DATA`, by which time its bounce is recorded.
    #[tokio::test]
    async fn takes_notifications_over_lmtp() {
        let dir = TempDir::new();
        let database = dir.join("smtp.sqlite");
        recorded(
            &db::open(&database).unwrap(),
            GMAIL_TOKEN,
            "relay@example.com",
            "<m1.t1.tag@mail.example.com>",
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, receiver) = tokio::sync::watch::channel(false);
        let context = context(&database);
        let server = tokio::spawn(listen(
            listener,
            Arc::new(context),
            Shutdown::from(receiver),
        ));
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let (read, mut write) = stream.split();
        let mut reader = BufReader::new(read);
        let mut next = async || {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            line
        };
        assert!(next().await.starts_with("220 "));

        let mut data = String::new();
        for line in GMAIL.split("\r\n") {
            if line.starts_with('.') {
                data.push('.');
            }
            data.push_str(line);
            data.push_str("\r\n");
        }
        let unknown = "bounce+ffffffffffffffffffffffffffffffff@mail.example.com";
        write.write_all(b"LHLO mail.example.com\r\n").await.unwrap();
        let mut capabilities = vec![next().await];
        while capabilities.last().unwrap().starts_with("250-") {
            capabilities.push(next().await);
        }
        write.write_all(b"MAIL FROM:<>\r\n").await.unwrap();
        assert!(next().await.starts_with("250 "));
        write
            .write_all(format!("RCPT TO:<bounce+{GMAIL_TOKEN}@mail.example.com>\r\n").as_bytes())
            .await
            .unwrap();
        assert!(next().await.starts_with("250 "));
        write
            .write_all(format!("RCPT TO:<{unknown}>\r\n").as_bytes())
            .await
            .unwrap();
        assert!(next().await.starts_with("550 "));
        write.write_all(b"DATA\r\n").await.unwrap();
        assert!(next().await.starts_with("354 "));
        write.write_all(data.as_bytes()).await.unwrap();
        write.write_all(b".\r\n").await.unwrap();
        assert_eq!(next().await, "250 2.0.0 Recorded\r\n");
        write.write_all(b"QUIT\r\n").await.unwrap();
        assert!(next().await.starts_with("221 "));

        let conn = db::open(&database).unwrap();
        let events = payloads(&conn);
        assert_eq!(events.len(), 1);
        assert_eq!(
            (events[0]["kind"].as_str(), events[0]["recipient"].as_str()),
            (Some("bounced"), Some("ghost@example.org"))
        );
        assert_eq!(events[0]["provenance"], "verp_dsn");
        stop.send(true).unwrap();
        server.await.unwrap().unwrap();
    }

    /// The feedback address shares the listener: `fbl@<mail host>` is accepted at `RCPT`, and a
    /// report that returns headers our own key signed becomes one `complaint` before the reply
    /// to `DATA` (its reporter is not verified here, so its provenance is `feedback_id_only`).
    #[tokio::test]
    async fn takes_feedback_reports_over_lmtp() {
        let dir = TempDir::new();
        let database = dir.join("smtp.sqlite");
        let (key, public) = feedback::tests::our_key();
        feedback::tests::recorded(&db::open(&database).unwrap(), Some(public.as_str()));
        let report = feedback::tests::report(
            "abuse",
            None,
            Some("grace@yahoo.example"),
            &feedback::tests::signed_headers(&key),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, receiver) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(listen(
            listener,
            Arc::new(context(&database)),
            Shutdown::from(receiver),
        ));
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let (read, mut write) = stream.split();
        let mut reader = BufReader::new(read);
        let mut next = async || {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            line
        };
        assert!(next().await.starts_with("220 "));
        write.write_all(b"LHLO mail.example.com\r\n").await.unwrap();
        while next().await.starts_with("250-") {}
        write
            .write_all(b"MAIL FROM:<>\r\nRCPT TO:<fbl@mail.example.com>\r\nDATA\r\n")
            .await
            .unwrap();
        assert!(next().await.starts_with("250 "));
        assert!(next().await.starts_with("250 "));
        assert!(next().await.starts_with("354 "));
        write.write_all(&report).await.unwrap();
        write.write_all(b".\r\n").await.unwrap();
        assert_eq!(next().await, "250 2.0.0 Recorded\r\n");

        let events = payloads(&db::open(&database).unwrap());
        assert_eq!(events.len(), 1);
        assert_eq!(
            (
                events[0]["kind"].as_str(),
                events[0]["provenance"].as_str(),
                events[0]["recipient"].as_str()
            ),
            (
                Some("complaint"),
                Some("feedback_id_only"),
                Some("grace@yahoo.example")
            )
        );
        stop.send(true).unwrap();
        server.await.unwrap().unwrap();
    }
}
