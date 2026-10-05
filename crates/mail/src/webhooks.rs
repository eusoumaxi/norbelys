//! Provider webhooks: verifying a provider's signed callback, splitting it into one
//! [`Receipt`] per provider event, and later parsing a receipt into typed [`Event`]s.
//!
//! The flow these functions serve:
//! 1. The ingress endpoint receives a POST for one provider webhook (bound to one connection,
//!    so the workspace is known from the URL, never from the payload) and calls the provider's
//!    `verify`. Verification checks the signature and its freshness before the body is trusted,
//!    and returns the receipts: each provider event's id and its exact bytes.
//! 2. The caller stores each receipt keyed by `(provider webhook, event id)` before
//!    understanding it, so a replayed event is recognised by its key and answered with success
//!    without effects, and an event survives a crash between receipt and processing.
//! 3. A background job calls the provider's `events` on each stored receipt and records the
//!    resulting events as evidence about the caller's messages.
//!
//! Invariants:
//! - Nothing in a payload chooses a tenant: events name the caller's message only through the
//!   id the relay carried ([`crate::compose::MESSAGE_TAG`]) or the `Message-ID` this library
//!   composed.
//! - Bodies are at most [`MAX_BODY`]; a batch holds at most [`MAX_EVENTS`] events. Provider
//!   modules may impose a tighter cap (for example [`norbelys::MAX_BATCH`] for the managed MTA).
//! - Freshness windows are per provider, wide enough for each provider's retry schedule and
//!   narrower than the time the caller keeps event keys, so a replay outside the window is
//!   refused by the signature check and one inside it by the key.
//! - Provider text kept on an event is bounded and on one line.
//!
//! Providers: [`mailgun`] (HMAC-SHA256 over timestamp and token), [`sendgrid`] (ECDSA P-256 over
//! timestamp and body), [`ses`] (Amazon SNS message signatures, versions 1 and 2) and
//! [`norbelys`] (the managed MTA, Standard Webhooks).

pub mod mailgun;
pub mod norbelys;
pub mod sendgrid;
pub mod ses;

use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

use crate::status::EnhancedStatus;

/// The largest webhook body accepted.
pub const MAX_BODY: usize = 1024 * 1024;
/// The most events one body may carry.
pub const MAX_EVENTS: usize = 10_000;
/// How far in the future a signed timestamp may be, for clock skew.
const MAX_SKEW: SignedDuration = SignedDuration::from_mins(5);

/// One provider event as received: what the caller stores before understanding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The provider's id of the event, unique per provider webhook: the replay key.
    pub event_id: String,
    /// The event's exact bytes (one element of a batch, or the whole notification), to be
    /// parsed later by the provider's `events`.
    pub raw: Vec<u8>,
}

/// One observation a provider event makes about one message (and one recipient, when named).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The provider's event id; every event parsed from one receipt shares it.
    pub event_id: String,
    /// What the provider observed.
    pub kind: EventKind,
    /// The caller's message id, from the relay's metadata (an SES tag, a SendGrid unique
    /// argument, a Mailgun variable); `None` when absent or not a UUID, and the event is then
    /// matched by `internet_message_id` or kept unmatched for review.
    pub message_id: Option<Uuid>,
    /// The `Message-ID` header the event names: the one this library composed, as the relay or
    /// the managed MTA saw it.
    pub internet_message_id: Option<String>,
    /// The provider's own id of the message: SES's token, SendGrid's `sg_message_id`, the
    /// managed MTA's queue id.
    pub provider_message_id: Option<String>,
    /// The recipient the event is about; `None` for a message-level event or when the provider
    /// cannot say which recipient it was (an SES complaint listing several candidates).
    pub recipient: Option<String>,
    /// The enhanced status the provider reported.
    pub status: Option<EnhancedStatus>,
    /// The SMTP reply code the provider reported.
    pub smtp_code: Option<u16>,
    /// The provider's diagnostic, bounded to one line.
    pub diagnostic: Option<String>,
    /// When the provider observed it.
    pub observed_at: Timestamp,
    /// How the managed MTA learned it; `None` for the relays, whose events are all their own
    /// signed observations.
    pub provenance: Option<Provenance>,
}

/// What a provider observed about a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// The provider took the message for delivery.
    Accepted,
    /// A delivery attempt failed temporarily; the provider keeps trying.
    Deferred,
    /// The next hop accepted the message. Not inbox placement.
    Delivered,
    /// The recipient's server refused the recipient for good (a hard bounce).
    Bounced,
    /// The provider gave up or refused without proving the recipient invalid: its own
    /// suppression list, a policy block, an expired retry window, a soft bounce it stopped
    /// retrying.
    Rejected,
    /// The recipient reported the message as spam.
    Complaint,
    /// The recipient unsubscribed through the provider.
    Unsubscribed,
}

impl EventKind {
    /// The kind spelling the caller records for the delivery event.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Deferred => "deferred",
            Self::Delivered => "delivered",
            Self::Bounced => "bounced",
            Self::Rejected => "rejected",
            Self::Complaint => "complaint",
            Self::Unsubscribed => "unsubscribed",
        }
    }
}

/// How the managed MTA learned what an event says. The webhook signature authenticates the MTA,
/// not the party that reported to it, so the caller weighs an event by this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provenance {
    /// The next hop's SMTP reply, read by the MTA itself.
    SmtpReply,
    /// A delivery status notification that reached the MTA's per-message VERP return path: it
    /// concerns the caller's message, but its reporter is not authenticated.
    VerpDsn,
    /// An abuse report from an enrolled feedback loop whose DKIM signature the MTA verified.
    FblArfDkim,
    /// An abuse report matched only by a signed `Feedback-ID` header.
    FeedbackIdOnly,
}

impl Provenance {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SmtpReply => "smtp_reply",
            Self::VerpDsn => "verp_dsn",
            Self::FblArfDkim => "fbl_arf_dkim",
            Self::FeedbackIdOnly => "feedback_id_only",
        }
    }
}

/// Why a webhook request is refused. The ingress answers every variant with a refusal except
/// [`VerifyError::Unavailable`], which asks the provider to retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// The body is larger than [`MAX_BODY`], or a batch has more than [`MAX_EVENTS`] events.
    #[error("the webhook body exceeds its bound")]
    TooLarge,
    /// The signature is missing, malformed, from another key or topic, or does not verify.
    #[error("the webhook signature does not verify: {0}")]
    Unauthorized(&'static str),
    /// The signed timestamp is outside the provider's freshness window.
    #[error("the webhook's signed timestamp is outside the freshness window")]
    Stale,
    /// The verified body is not the provider's format.
    #[error("the webhook body is not valid: {0}")]
    InvalidPayload(String),
    /// A dependency of verification is unavailable (the SNS signing certificate could not be
    /// fetched): the provider should retry.
    #[error("the webhook cannot be verified now: {0}")]
    Unavailable(String),
}

/// Why the key material of a provider webhook is not usable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid webhook key: {0}")]
pub struct KeyError(pub &'static str);

/// Why a stored receipt cannot be parsed into events.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the receipt is not a valid event: {0}")]
pub struct ParseError(pub String);

/// The first value of header `name`, when it is visible ASCII.
pub(crate) fn header<'a>(headers: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
}

/// Accepts a signed Unix `seconds` within `max_age` before `now` and [`MAX_SKEW`] after it.
pub(crate) fn fresh(
    seconds: i64,
    now: Timestamp,
    max_age: SignedDuration,
) -> Result<(), VerifyError> {
    let signed = Timestamp::from_second(seconds).map_err(|_| VerifyError::Stale)?;
    let age = now.duration_since(signed);
    if age > max_age || age < -MAX_SKEW {
        Err(VerifyError::Stale)
    } else {
        Ok(())
    }
}

/// An event id as a replay key: 1 to 256 visible ASCII characters.
pub(crate) fn event_id(value: Option<&str>) -> Result<String, VerifyError> {
    value
        .map(str::trim)
        .filter(|id| {
            (1..=256).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .map(str::to_owned)
        .ok_or_else(|| VerifyError::InvalidPayload("an event without a usable id".to_owned()))
}

/// Our message id from a relay's metadata value.
pub(crate) fn message_tag(value: Option<&str>) -> Option<Uuid> {
    value.and_then(|value| Uuid::parse_str(value.trim()).ok())
}

/// A Unix time given as an integer or a fractional number of seconds.
pub(crate) fn unix_seconds(value: Option<&serde_json::Value>) -> Option<Timestamp> {
    let value = value?;
    if let Some(seconds) = value.as_i64() {
        return Timestamp::from_second(seconds).ok();
    }
    let duration = SignedDuration::try_from_secs_f64(value.as_f64()?).ok()?;
    Timestamp::UNIX_EPOCH.checked_add(duration).ok()
}

/// An SMTP reply code at the start of a diagnostic (`550 …`, `smtp; 550 …`).
pub(crate) fn smtp_code(text: &str) -> Option<u16> {
    let text = text.trim();
    let text = text.split_once(';').map_or(
        text,
        |(kind, rest)| if kind.len() <= 16 { rest.trim() } else { text },
    );
    text.get(..3)
        .filter(|digits| digits.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|digits| digits.parse().ok())
        .filter(|code| (200..600).contains(code))
}

/// A bounded diagnostic, `None` when empty.
pub(crate) fn diagnostic(text: Option<&str>) -> Option<String> {
    text.and_then(crate::text::diagnostic)
}

/// A recipient address as the provider wrote it, bounded; `None` when absent or empty.
pub(crate) fn recipient(text: Option<&str>) -> Option<String> {
    text.map(|text| crate::text::bounded(text, 320))
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests;

/// Versioned control-RPC bytes covered by a Standard Webhooks HMAC. Method and the exact
/// path/query are authenticated along with the body; TLS or a private tunnel protects replies.
#[must_use]
pub fn control_payload(method: &str, path_query: &str, body: &[u8]) -> Vec<u8> {
    let mut payload = format!("Norbelys-Control/2\n{method}\n{path_query}\n").into_bytes();
    payload.extend_from_slice(body);
    payload
}
