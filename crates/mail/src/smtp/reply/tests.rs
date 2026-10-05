use strum::IntoEnumIterator as _;

use super::{Meaning, ReplyId, classify};
use crate::status::EnhancedStatus;
use crate::submission::{Cause, Failure, Phase, Scope};

fn meaning(phase: Phase, code: u16, text: &str) -> Meaning {
    classify(phase, code, EnhancedStatus::find(text), text)
}

fn expect(failure: Failure, scope: Scope, cause: Cause) -> Meaning {
    Meaning {
        failure,
        scope,
        cause,
    }
}

/// A reply without an enhanced status is judged by its class and the phase it answered: before
/// `MAIL FROM` nothing about the message was submitted, so the message waits (transient) and the
/// connection is concerned, with a `5xx` to `AUTH` meaning a refused credential; from `MAIL FROM`
/// on, `4xx` is transient and `5xx` permanent, about the recipient at `RCPT TO` and the message
/// elsewhere. Generated over every phase, so a new phase fails until it has an answer.
#[test]
fn replies_without_a_status_follow_their_class_and_phase() {
    use Cause::{Refused, Unauthorized};
    use Failure::{Permanent, Transient};
    use Scope::{Connection, Message, Recipient};
    for phase in Phase::iter() {
        let (temporary, permanent) = match phase {
            Phase::Connect => (
                expect(Transient, Connection, Refused),
                expect(Transient, Connection, Refused),
            ),
            Phase::Auth => (
                expect(Transient, Connection, Refused),
                expect(Transient, Connection, Unauthorized),
            ),
            Phase::MailFrom | Phase::Data | Phase::Api => (
                expect(Transient, Message, Refused),
                expect(Permanent, Message, Refused),
            ),
            Phase::RcptTo => (
                expect(Transient, Recipient, Refused),
                expect(Permanent, Recipient, Refused),
            ),
        };
        assert_eq!(
            meaning(phase, 451, "Requested action aborted"),
            temporary,
            "{phase:?} 451"
        );
        assert_eq!(
            meaning(phase, 550, "Requested action not taken"),
            permanent,
            "{phase:?} 550"
        );
    }
}

/// With an enhanced status the subject decides who is concerned (RFC 3463 §3): policy (X.7)
/// concerns the connection even at `RCPT TO`, addressing (X.1) the recipient or, at `MAIL FROM`,
/// the message's sender; a full mailbox the recipient wherever it comes; a message too large,
/// content and protocol errors the message; mail-system and routing failures the connection
/// unless routing failed for one recipient.
#[test]
fn enhanced_statuses_choose_the_scope() {
    use Cause::Refused;
    use Failure::{Permanent, Transient};
    use Scope::{Connection, Message, Recipient};
    let cases = [
        (
            Phase::RcptTo,
            550,
            "5.1.1 <ghost@example.com>: user unknown",
            expect(Permanent, Recipient, Refused),
        ),
        (
            Phase::MailFrom,
            553,
            "5.1.8 sender address rejected",
            expect(Permanent, Message, Refused),
        ),
        (
            Phase::Data,
            552,
            "5.2.2 mailbox full",
            expect(Permanent, Recipient, Refused),
        ),
        (
            Phase::RcptTo,
            452,
            "4.2.2 mailbox over quota",
            expect(Transient, Recipient, Refused),
        ),
        (
            Phase::Data,
            552,
            "5.2.3 message length exceeds administrative limit",
            expect(Permanent, Message, Refused),
        ),
        (
            Phase::Data,
            554,
            "5.2.0 STOREDRV.Submission.Exception:SendAsDeniedException",
            expect(Permanent, Message, Refused),
        ),
        (
            Phase::Data,
            552,
            "5.3.4 message too big for system",
            expect(Permanent, Message, Refused),
        ),
        (
            Phase::MailFrom,
            452,
            "4.3.1 insufficient system storage",
            expect(Transient, Connection, Refused),
        ),
        (
            Phase::RcptTo,
            550,
            "5.4.4 unable to route",
            expect(Permanent, Recipient, Refused),
        ),
        (
            Phase::Data,
            451,
            "4.4.1 no answer from host",
            expect(Transient, Connection, Refused),
        ),
        (
            Phase::MailFrom,
            501,
            "5.5.2 syntax error",
            expect(Permanent, Message, Refused),
        ),
        (
            Phase::Data,
            554,
            "5.6.0 message content rejected",
            expect(Permanent, Message, Refused),
        ),
        (
            Phase::RcptTo,
            550,
            "5.7.1 relaying denied",
            expect(Permanent, Connection, Refused),
        ),
        (
            Phase::Data,
            550,
            "5.7.26 unauthenticated mail is prohibited",
            expect(Permanent, Connection, Refused),
        ),
    ];
    for (phase, code, text, expected) in cases {
        assert_eq!(meaning(phase, code, text), expected, "{code} {text}");
    }
}

/// Throttles and documented quota refusals are transient whatever their class and pause what
/// they name, so a credential over its limit stops sending instead of failing its whole queue:
/// Exchange's quota exception (by its exact text, other `5.2.0` replies keeping their meaning),
/// Gmail's `550 5.4.5` daily limit, `421 4.7.x`, Exchange's `432 4.3.2`, any `4.7.x` outside
/// `AUTH`, and Amazon SES's `454 Throttling failure`, which concerns the SES account. A bare
/// `421` closes the channel: transient, about the connection, but not a throttle; a `4.7.0` to
/// `AUTH` is a temporary authentication failure, not a throttle either.
#[test]
fn throttles_and_quota_refusals_are_transient_and_scoped() {
    use Cause::{Refused, Throttled};
    use Failure::Transient;
    use Scope::{Connection, QuotaScope};
    let cases = [
        (
            Phase::Data,
            554,
            "5.2.0 STOREDRV.Submission.Exception:SubmissionQuotaExceededException; Failed to process message",
            expect(Transient, Connection, Throttled),
        ),
        (
            Phase::MailFrom,
            550,
            "5.4.5 Daily user sending limit exceeded.",
            expect(Transient, Connection, Throttled),
        ),
        (
            Phase::Connect,
            421,
            "4.7.0 Try again later, closing connection. (EHLO)",
            expect(Transient, Connection, Throttled),
        ),
        (
            Phase::MailFrom,
            432,
            "4.3.2 STOREDRV.ClientSubmit; sender thread limit exceeded",
            expect(Transient, Connection, Throttled),
        ),
        (
            Phase::RcptTo,
            451,
            "4.7.1 Rate limited, try again later",
            expect(Transient, Connection, Throttled),
        ),
        (
            Phase::MailFrom,
            454,
            "Throttling failure: Maximum sending rate exceeded.",
            expect(Transient, QuotaScope, Throttled),
        ),
        (
            Phase::MailFrom,
            454,
            "Throttling failure: Daily message quota exceeded.",
            expect(Transient, QuotaScope, Throttled),
        ),
        (
            Phase::RcptTo,
            421,
            "Service not available, closing transmission channel",
            expect(Transient, Connection, Refused),
        ),
        (
            Phase::Auth,
            454,
            "4.7.0 Temporary authentication failure",
            expect(Transient, Connection, Refused),
        ),
        (
            Phase::Auth,
            535,
            "5.7.8 Username and Password not accepted",
            expect(Transient, Connection, Cause::Unauthorized),
        ),
    ];
    for (phase, code, text, expected) in cases {
        assert_eq!(meaning(phase, code, text), expected, "{code} {text}");
    }
}

/// Amazon SES's two refusals that mean a connection lost access to its account stop the
/// connection and keep the message, in every phase SES may answer them: a credential whose IAM
/// user lacks `ses:SendRawEmail` needs a new or granted credential (unauthorized), an identity
/// not verified in the Region needs a person at AWS (forbidden). Were they read as the message's
/// permanent failures, a lapsed verification would fail a campaign's whole queue. A `Message
/// rejected` for another reason keeps its ordinary meaning. The texts are SES's own, as its SMTP
/// troubleshooting page lists them.
#[test]
fn ses_access_refusals_stop_the_connection_and_keep_the_message() {
    use Cause::{Forbidden, Refused, Unauthorized};
    use Failure::{Permanent, Transient};
    use Scope::{Connection, Message};
    let denied = "Access denied: User arn:aws:iam::123456789012:user/ses-smtp-user.20260101-120000 \
                  is not authorized to perform ses:SendRawEmail on resource \
                  arn:aws:ses:us-east-1:123456789012:identity/example.com";
    let unverified = "Message rejected: Email address is not verified. The following identities \
                      failed the check in region US-EAST-1: ada@example.com";
    for phase in [Phase::MailFrom, Phase::RcptTo, Phase::Data] {
        assert_eq!(
            meaning(phase, 554, denied),
            expect(Transient, Connection, Unauthorized),
            "{phase:?} access denied"
        );
        assert_eq!(
            meaning(phase, 554, unverified),
            expect(Transient, Connection, Forbidden),
            "{phase:?} unverified identity"
        );
    }
    assert_eq!(
        meaning(
            Phase::Data,
            554,
            "Message rejected: the message contains a virus"
        ),
        expect(Permanent, Message, Refused)
    );
}

/// An acceptance's id is read only in the form the pool's provider writes: Postfix's `queued
/// as <id>` and SES's `Ok <token>`. A pool that expects neither, or a reply in another form
/// (Gmail's `OK <time> <id>` looks like an SES token to a blind parser), yields no id.
#[test]
fn acceptance_ids_are_read_in_their_provider_form_only() {
    let ses = "0100018a1b2c3d4e-12345678-1234-1234-1234-123456789012-000000";
    assert_eq!(
        ReplyId::QueuedAs.extract("2.0.0 Ok: queued as 4F1B23C04D"),
        Some("4F1B23C04D".to_owned())
    );
    assert_eq!(
        ReplyId::SesToken.extract(&format!("Ok {ses}")),
        Some(ses.to_owned())
    );
    assert_eq!(
        ReplyId::SesToken.extract("2.0.0 OK  1696169811 ffacd0b85a97d-32gsmtp"),
        None
    );
    assert_eq!(
        ReplyId::QueuedAs.extract("2.0.0 OK <id@host> [Hostname=AM0P]"),
        None
    );
    assert_eq!(ReplyId::QueuedAs.extract("Ok: queued as <bad>"), None);
    assert_eq!(
        ReplyId::None.extract("2.0.0 Ok: queued as 4F1B23C04D"),
        None
    );
}
