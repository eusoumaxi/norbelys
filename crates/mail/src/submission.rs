//! The vocabulary every transport shares: what is submitted, what an acceptance returns, and
//! what every other ending means.
//!
//! A submission hands one message to a provider and reads its answer. Each transport (SMTP, the
//! Gmail API, Microsoft Graph) answers `Result<Submission, Rejection>`:
//!
//! - `Ok(Submission)` means the provider took the message: a reply that accepted it was read
//!   (SMTP `250` after the content, HTTP `200`/`202`). Acceptance is not delivery; later
//!   evidence (bounces, webhooks) is a separate concern.
//! - `Err(Rejection)` carries a [`Failure`] that the caller's retry policy acts on:
//!   - `transient`: nothing was taken, or the provider refused for now; the message may be
//!     submitted again later;
//!   - `permanent`: the provider refused this message for good;
//!   - `uncertain`: the message may have been taken but the answer was lost (SMTP after the
//!     server answered `354` to `DATA` and the content was being sent; HTTP after the request
//!     left without a reply that says it was not processed). An uncertain message must never be
//!     resubmitted automatically, because that could deliver it twice; only a read-only search
//!     of the mailbox's Sent folder or a later provider event may settle it.
//!
//! Every rejection also says where it happened ([`Phase`]), who it concerns ([`Scope`]: one
//! recipient, the message, the connection's credential, a shared account limit, or Norbelys's
//! own app) and what kind of answer it was ([`Cause`]), so the caller can pause the right thing:
//! a recipient's full mailbox must never stop a credential, while a throttle must pause the
//! credential (or the shared account) it names.
//!
//! Invariant: before `MAIL FROM` nothing about the message has been submitted, so failures while
//! connecting or authenticating are `transient` and scoped to the connection; the message waits
//! for the connection to be usable again instead of failing.

use std::collections::HashSet;
use std::str::FromStr;

use jiff::Timestamp;
use lettre::Address;

use crate::status::EnhancedStatus;

/// The most envelope recipients one message may have, `To`, `Cc` and `Bcc` together. Providers
/// allow fewer per message (Amazon SES 50, Gmail 100 over SMTP); those tighter limits are
/// checked before submission by the caller.
pub const MAX_RECIPIENTS: usize = 150;

/// The SMTP envelope of one message: the reverse path (`MAIL FROM`) and 1 to
/// [`MAX_RECIPIENTS`] distinct recipients (`RCPT TO`), each a syntactically valid address.
///
/// The HTTP transports (Gmail, Graph) do not take an envelope: they read the recipients from
/// the MIME headers, which is why a message for them is composed with its `Bcc` header kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    from: Address,
    recipients: Vec<Address>,
}

/// Why an envelope cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    /// An address is not a valid `local@domain`.
    #[error("`{0}` is not a valid envelope address")]
    Address(String),
    /// The envelope names no recipient.
    #[error("an envelope needs at least one recipient")]
    NoRecipients,
    /// The envelope names more recipients than one message may have.
    #[error("an envelope has at most {MAX_RECIPIENTS} recipients, not {0}")]
    TooManyRecipients(usize),
    /// The same address, compared in ASCII lowercase, appears twice.
    #[error("`{0}` appears twice in the envelope")]
    Duplicate(String),
}

impl Envelope {
    /// Builds an envelope from the From address and the recipients (`To`, `Cc` and `Bcc`
    /// together), in the order `RCPT TO` will name them.
    ///
    /// Addresses are compared in ASCII lowercase of the whole address, without folding dots or
    /// `+` tags, which is how Norbelys compares addresses everywhere.
    ///
    /// # Errors
    ///
    /// An address is invalid, there is no recipient or more than [`MAX_RECIPIENTS`], or an
    /// address repeats.
    pub fn new<'a>(
        from: &str,
        recipients: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, EnvelopeError> {
        let from = parse(from)?;
        let mut seen = HashSet::new();
        let mut parsed = Vec::new();
        for recipient in recipients {
            let address = parse(recipient)?;
            if !seen.insert(address.to_string().to_ascii_lowercase()) {
                return Err(EnvelopeError::Duplicate(recipient.to_owned()));
            }
            parsed.push(address);
        }
        match parsed.len() {
            0 => Err(EnvelopeError::NoRecipients),
            count if count > MAX_RECIPIENTS => Err(EnvelopeError::TooManyRecipients(count)),
            _ => Ok(Self {
                from,
                recipients: parsed,
            }),
        }
    }

    /// The reverse path (`MAIL FROM`).
    #[must_use]
    pub fn from(&self) -> &Address {
        &self.from
    }

    /// The recipients, in the order `RCPT TO` names them.
    #[must_use]
    pub fn recipients(&self) -> &[Address] {
        &self.recipients
    }

    /// Whether any address has non-ASCII characters, which needs the server's `SMTPUTF8`
    /// extension (RFC 6531, <https://www.rfc-editor.org/rfc/rfc6531>).
    #[must_use]
    pub fn has_non_ascii(&self) -> bool {
        std::iter::once(&self.from)
            .chain(&self.recipients)
            .any(|address| !address.to_string().is_ascii())
    }
}

fn parse(address: &str) -> Result<Address, EnvelopeError> {
    Address::from_str(address.trim()).map_err(|_| EnvelopeError::Address(address.to_owned()))
}

/// A provider took the message: the attempt outcome `accepted`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Submission {
    /// The provider's id of the accepted message, when it returns one: Gmail's message id, the
    /// token in Amazon SES's `250 Ok <token>` (SES replaces the `Message-ID` header with its own
    /// value, and its events carry that token), the managed MTA's queue id. Graph returns none.
    pub provider_message_id: Option<String>,
    /// The final SMTP reply as read (`250 …`), bounded; `None` over HTTP.
    pub reply: Option<String>,
    /// Recipients the server refused at `RCPT TO` while others were accepted. They do not
    /// receive this message; each refusal is evidence about that recipient alone.
    pub refused: Vec<RecipientRefusal>,
}

/// One recipient refused at `RCPT TO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientRefusal {
    /// The envelope address the server refused.
    pub recipient: String,
    /// The SMTP reply code (`550`, `452`).
    pub code: u16,
    /// The reply's enhanced status, when it carried one.
    pub status: Option<EnhancedStatus>,
    /// The reply, code included, bounded to one line.
    pub diagnostic: String,
}

/// A submission that did not end in `accepted`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// What it means for the message.
    pub failure: Failure,
    /// The protocol step it happened in.
    pub phase: Phase,
    /// Who it concerns.
    pub scope: Scope,
    /// What kind of answer it was.
    pub cause: Cause,
    /// The SMTP reply code, or the HTTP status in the `api` phase; `None` when nothing was read.
    pub code: Option<u16>,
    /// The enhanced status the reply carried.
    pub status: Option<EnhancedStatus>,
    /// The provider's own wait (`Retry-After`, Gmail's "Retry after" time) as an absolute
    /// instant. A zero, past or malformed value is `None`: the caller then uses its own backoff.
    pub retry_after: Option<Timestamp>,
    /// The provider's text or a local reason, bounded and on one line. Never a credential.
    pub diagnostic: String,
    /// Per-recipient `RCPT TO` refusals read before the submission stopped.
    pub refused: Vec<RecipientRefusal>,
}

impl Rejection {
    /// A rejection decided without a provider reply: `code`, `status` and `retry_after` empty.
    pub(crate) fn local(
        failure: Failure,
        phase: Phase,
        scope: Scope,
        cause: Cause,
        diagnostic: &str,
    ) -> Self {
        Self {
            failure,
            phase,
            scope,
            cause,
            code: None,
            status: None,
            retry_after: None,
            diagnostic: crate::text::bounded(diagnostic, crate::text::DIAGNOSTIC_CHARS),
            refused: Vec::new(),
        }
    }
}

/// What a rejection means for the message. Together with `accepted` these are the submission
/// outcomes the caller records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(test, derive(strum::EnumIter))]
pub enum Failure {
    /// Not submitted, or refused for now: the message may be submitted again.
    Transient,
    /// Refused for good: submitting the same message again would fail the same way.
    Permanent,
    /// It may have been accepted: never resubmitted automatically; a read-only search or a later
    /// provider event may settle it.
    Uncertain,
}

impl Failure {
    /// The stored spelling: `transient`, `permanent`, `uncertain`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Permanent => "permanent",
            Self::Uncertain => "uncertain",
        }
    }
}

/// The protocol step a submission ended in — the submission phase the caller records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(test, derive(strum::EnumIter))]
pub enum Phase {
    /// Resolving the host, TCP, TLS, the greeting, `EHLO` and `STARTTLS`.
    Connect,
    /// `AUTH`.
    Auth,
    /// `MAIL FROM`, and the checks made just before it (the deadline, the extensions a message
    /// needs).
    MailFrom,
    /// `RCPT TO`.
    RcptTo,
    /// `DATA`, the content and the final reply.
    Data,
    /// One HTTP request to a provider's API.
    Api,
}

impl Phase {
    /// The stored spelling: `connect`, `auth`, `mail_from`, `rcpt_to`, `data`, `api`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Auth => "auth",
            Self::MailFrom => "mail_from",
            Self::RcptTo => "rcpt_to",
            Self::Data => "data",
            Self::Api => "api",
        }
    }
}

/// Who a rejection concerns, which decides what may be paused. Only `Connection`,
/// `QuotaScope` and `Platform` concern a credential or an account; the others never stop one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// One recipient's mailbox: an unknown user, a full mailbox, a domain without a route.
    Recipient,
    /// This message: its size, its content, its From address.
    Message,
    /// The connection's credential: throttled, refused, unauthorized or unreachable.
    Connection,
    /// A provider-side limit of an account the customer owns and several connections share: an
    /// Amazon SES account and Region, a Microsoft tenant, a relay account.
    QuotaScope,
    /// A limit of Norbelys's own Google Cloud project or Microsoft app, shared by every
    /// workspace: no single tenant owns it, so the caller backs off in process.
    Platform,
}

/// What kind of answer ended the submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cause {
    /// The provider refused: an SMTP `4xx` or `5xx` reply, or an HTTP error status.
    Refused,
    /// The provider asked to slow down: HTTP `429`, SMTP `421 4.7.x`, `432 4.3.2`, any other
    /// `4.7.x` outside `AUTH`, Amazon SES's `454 Throttling failure`, or a documented quota
    /// refusal. The caller pauses what the scope names at once.
    Throttled,
    /// The provider refused the credential: any `5xx` to `AUTH` (`535 5.7.8`), HTTP `401`,
    /// Amazon SES's `554 Access denied` (the IAM user behind the SMTP credential may not send).
    /// The connection needs a new credential, a new consent or a granted permission.
    Unauthorized,
    /// The provider refused the account by policy or permission: an HTTP `403` that names the
    /// account (Gmail's `domainPolicy`, Graph's `ErrorAccessDenied`), Amazon SES's `554 Message
    /// rejected: Email address is not verified` (an identity not verified in the Region).
    Forbidden,
    /// No usable answer: the host did not resolve or is not allowed, the connection failed or
    /// was reset, TLS failed, or a reply was malformed or did not come in time.
    NoReply,
    /// The caller's clock ended it: the submission deadline passed before `MAIL FROM`, too
    /// little of the budget remained to send `DATA`, or no session of the credential freed in
    /// time.
    Deadline,
    /// The message cannot be sent as composed through this server: it needs `SMTPUTF8` or
    /// `8BITMIME` and the server offers neither.
    Unsupported,
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::{Envelope, EnvelopeError, Failure, MAX_RECIPIENTS, Phase};

    /// An envelope is the exact list `RCPT TO` names, so it is checked once when built: valid
    /// addresses, 1 to 150 recipients, and no address twice (ASCII case-insensitive, the way
    /// addresses are compared everywhere), since a duplicate would deliver the message twice.
    #[test]
    fn envelopes_accept_only_distinct_valid_recipients() {
        let envelope = Envelope::new(
            "ada@example.com",
            ["grace@example.com", "linus@example.org"],
        )
        .expect("a valid envelope");
        assert_eq!(envelope.from().to_string(), "ada@example.com");
        assert_eq!(envelope.recipients().len(), 2);
        assert!(!envelope.has_non_ascii());
        assert_eq!(
            Envelope::new(
                "ada@example.com",
                ["Grace@Example.com", "grace@example.com"]
            ),
            Err(EnvelopeError::Duplicate("grace@example.com".to_owned()))
        );
        assert_eq!(
            Envelope::new("ada@example.com", []),
            Err(EnvelopeError::NoRecipients)
        );
        assert_eq!(
            Envelope::new("ada@example.com", ["not an address"]),
            Err(EnvelopeError::Address("not an address".to_owned()))
        );
        assert!(matches!(
            Envelope::new("nobody", ["grace@example.com"]),
            Err(EnvelopeError::Address(_))
        ));
        let many: Vec<String> = (0..=MAX_RECIPIENTS)
            .map(|n| format!("p{n}@example.com"))
            .collect();
        assert_eq!(
            Envelope::new("ada@example.com", many.iter().map(String::as_str)),
            Err(EnvelopeError::TooManyRecipients(MAX_RECIPIENTS + 1))
        );
        let utf8 = Envelope::new("ada@example.com", ["josé@example.com"])
            .expect("an internationalised address");
        assert!(utf8.has_non_ascii());
    }

    /// Phases and failures are stored as text under database constraints, so their spellings
    /// are a contract: a new variant fails here until its stored spelling is decided.
    #[test]
    fn phases_and_failures_keep_their_stored_spellings() {
        let phases: Vec<&str> = Phase::iter().map(Phase::as_str).collect();
        assert_eq!(
            phases,
            ["connect", "auth", "mail_from", "rcpt_to", "data", "api"]
        );
        let failures: Vec<&str> = Failure::iter().map(Failure::as_str).collect();
        assert_eq!(failures, ["transient", "permanent", "uncertain"]);
    }
}
