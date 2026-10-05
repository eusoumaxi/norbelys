//! What an SMTP reply means for the message: the outcome, the scope and the cause of a negative
//! reply, and the id an acceptance names. Pure functions over a reply's code, enhanced status
//! and text, so the mapping is the same for every session and easy to read in one place.
//!
//! The rules, in order:
//! 1. Documented quota refusals and throttles are `transient` whatever their class, and pause
//!    what they name:
//!    - Exchange Online's `554 5.2.0 STOREDRV.Submission.Exception:SubmissionQuotaExceededException`
//!      (matched by its exact text, since other `5.2.0` replies keep their meaning) pauses the
//!      connection until the mailbox's rolling limit frees;
//!    - any `5.4.5` pauses the connection: RFC 3463 defines X.4.5 as congestion, "useful only as
//!      a persistent transient error", and Gmail answers its daily sending limit with
//!      `550 5.4.5`;
//!    - Amazon SES's `454 Throttling failure` (its send rate or daily quota) pauses the SES
//!      account, the quota scope every connection of that account shares;
//!    - `421 4.7.x`, Exchange's `432 4.3.2` (too many connections) and every other `4.7.x`
//!      outside `AUTH` pause the connection; any other `421` (the server is closing the
//!      channel) is a transient refusal of the connection.
//! 2. Amazon SES's refusals that mean the connection lost access to its account are `transient`
//!    for the message, which waits for a person, and concern the connection, whatever the phase
//!    they answer (<https://docs.aws.amazon.com/ses/latest/dg/troubleshoot-smtp.html>, read
//!    2026-10-02):
//!    - `554 Access denied: User … is not authorized to perform ses:SendRawEmail on resource …`:
//!      the IAM user behind the SMTP credential lacks the sending permission, so the credential
//!      is replaced or granted it ([`Cause::Unauthorized`]);
//!    - `554 Message rejected: Email address is not verified. The following identities failed
//!      the check in region …`: an identity of the message (its From address, or a recipient
//!      while the account is in the sandbox) is not verified in the account's Region, which a
//!      person repairs at AWS ([`Cause::Forbidden`]).
//!
//!    Failing each message instead would empty a campaign's whole queue over a setting a person
//!    can repair in minutes; stopping the connection keeps the queue for when it is repaired.
//! 3. Before `MAIL FROM` nothing about the message was submitted: a refusal while connecting or
//!    authenticating is `transient` for the message and scoped to the connection, and any `5xx`
//!    to `AUTH` is [`Cause::Unauthorized`], so the connection asks its owner for a new
//!    credential while its messages wait.
//! 4. From `MAIL FROM` on, `4xx` is `transient` and `5xx` is `permanent`, scoped by the enhanced
//!    status's subject (RFC 3463 §3, <https://www.rfc-editor.org/rfc/rfc3463#section-3>):
//!    security and policy (X.7) concern the connection; addressing (X.1) concerns the recipient
//!    at `RCPT TO` and the message (its sender) at `MAIL FROM`; mailbox status (X.2) concerns
//!    the recipient at `RCPT TO` and for a full mailbox (X.2.2) anywhere, and the message
//!    otherwise (X.2.3 too large for the mailbox, Exchange's `5.2.0` refusal to send as an
//!    address the mailbox does not own); the mail system (X.3)
//!    and routing (X.4) concern the connection, except a message too big (X.3.4) and routing
//!    refused at `RCPT TO` (the recipient's domain); the protocol (X.5) and the content (X.6)
//!    concern the message. Without a status the phase decides: `RCPT TO` the recipient,
//!    otherwise the message.

use crate::status::EnhancedStatus;
use crate::submission::{Cause, Failure, Phase, Scope};

/// Where the final `250` names the accepted message. A pool setting, because each pool serves
/// one provider and a `250` text is never parsed blindly: Gmail's `250 2.0.0 OK  <time> <id>`
/// would otherwise look like an SES token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyId {
    /// The reply carries no id the caller relies on (mailboxes, Mailgun).
    None,
    /// Postfix's form, `250 2.0.0 Ok: queued as <id>`: the managed MTA, SendGrid.
    QueuedAs,
    /// Amazon SES's `250 Ok <token>`. SES replaces the `Message-ID` header with its own value, and
    /// its events carry this token as `mail.messageId`; the caller keeps it to correlate replies
    /// to the header the recipient actually saw.
    SesToken,
}

impl ReplyId {
    /// The id in a `250` reply's text (the code excluded), when the form matches.
    #[must_use]
    pub fn extract(self, text: &str) -> Option<String> {
        let token = match self {
            Self::None => return None,
            Self::QueuedAs => {
                let at = text.to_ascii_lowercase().find("queued as ")?;
                text.get(at + "queued as ".len()..)?
                    .split_whitespace()
                    .next()?
            }
            Self::SesToken => {
                let mut words = text.split_whitespace();
                if !words.next()?.eq_ignore_ascii_case("ok") {
                    return None;
                }
                words.next()?
            }
        };
        let token = token.trim_end_matches(['.', ',', ';', ')']);
        let valid = (1..=128).contains(&token.len())
            && token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'));
        valid.then(|| token.to_owned())
    }
}

/// What a negative reply means: its outcome, who it concerns and what kind of answer it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meaning {
    /// The outcome for the message.
    pub failure: Failure,
    /// Who it concerns.
    pub scope: Scope,
    /// What kind of answer it was.
    pub cause: Cause,
}

/// The meaning of a negative reply (`code` 400–599) read in `phase`, by the rules of this
/// module's documentation.
#[must_use]
pub fn classify(phase: Phase, code: u16, status: Option<EnhancedStatus>, text: &str) -> Meaning {
    let meaning = |failure, scope, cause| Meaning {
        failure,
        scope,
        cause,
    };
    let subject = status.map(EnhancedStatus::subject);
    let throttle = |scope| meaning(Failure::Transient, scope, Cause::Throttled);

    if text.contains("STOREDRV.Submission.Exception:SubmissionQuotaExceededException") {
        return throttle(Scope::Connection);
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("throttling failure") {
        return throttle(Scope::QuotaScope);
    }
    if ses_access_denied(&lower) {
        return meaning(Failure::Transient, Scope::Connection, Cause::Unauthorized);
    }
    if ses_unverified(&lower) {
        return meaning(Failure::Transient, Scope::Connection, Cause::Forbidden);
    }
    if status.is_some_and(|status| status.subject() == 4 && status.detail() == 5)
        || (code == 432 && subject == Some(3))
        || (code < 500 && subject == Some(7) && phase != Phase::Auth)
    {
        return throttle(Scope::Connection);
    }
    // 421: the server is closing the channel, whatever the command was.
    if code == 421 {
        return meaning(Failure::Transient, Scope::Connection, Cause::Refused);
    }

    match phase {
        Phase::Connect => return meaning(Failure::Transient, Scope::Connection, Cause::Refused),
        Phase::Auth if code >= 500 => {
            return meaning(Failure::Transient, Scope::Connection, Cause::Unauthorized);
        }
        Phase::Auth => return meaning(Failure::Transient, Scope::Connection, Cause::Refused),
        Phase::MailFrom | Phase::RcptTo | Phase::Data | Phase::Api => {}
    }

    let failure = if code >= 500 {
        Failure::Permanent
    } else {
        Failure::Transient
    };
    meaning(failure, scope(phase, status), Cause::Refused)
}

/// Amazon SES's refusal of the SMTP credential's IAM user (`554 Access denied: User … is not
/// authorized to perform ses:SendRawEmail on resource …`), from its lowercased text.
fn ses_access_denied(lower: &str) -> bool {
    lower.contains("access denied")
        && lower.contains("not authorized to perform")
        && lower.contains("ses:")
}

/// Amazon SES's refusal of an identity not verified in the account's Region (`554 Message
/// rejected: Email address is not verified. …`), from its lowercased text.
fn ses_unverified(lower: &str) -> bool {
    lower.contains("message rejected") && lower.contains("not verified")
}

/// The scope of a refusal from `MAIL FROM` on: by the status's subject, else by the phase.
fn scope(phase: Phase, status: Option<EnhancedStatus>) -> Scope {
    let at_recipient = phase == Phase::RcptTo;
    match status.map(|status| (status.subject(), status.detail())) {
        Some((7, _)) => Scope::Connection,
        Some((1, _)) | Some((4, _)) if at_recipient => Scope::Recipient,
        Some((1, _)) => Scope::Message,
        Some((2, 2)) => Scope::Recipient,
        Some((2, _)) if at_recipient => Scope::Recipient,
        Some((2, _)) | Some((3, 4)) | Some((5 | 6, _)) => Scope::Message,
        Some((3 | 4, _)) => Scope::Connection,
        Some(_) | None => match phase {
            Phase::RcptTo => Scope::Recipient,
            Phase::MailFrom | Phase::Data | Phase::Api => Scope::Message,
            Phase::Connect | Phase::Auth => Scope::Connection,
        },
    }
}

#[cfg(test)]
mod tests;
