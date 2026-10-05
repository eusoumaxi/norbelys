//! Mailgun webhooks (the JSON form of its event webhooks).
//!
//! Verification (<https://documentation.mailgun.com/docs/mailgun/user-manual/webhooks/securing-webhooks>):
//! the body carries `signature.timestamp`, `signature.token` and `signature.signature`, the hex
//! HMAC-SHA256 of `timestamp` followed by `token` under the account's HTTP webhook signing key.
//! The HMAC covers the timestamp and token, not the JSON body, so a body is trusted only together
//! with the replay key stored by the caller (the event's `id`) and TLS.
//!
//! Freshness: Mailgun retries a failed webhook for about 8 hours (10 and 15 minutes, then 30
//! minutes, 1, 2 and 4 hours), so a signed timestamp up to [`MAX_AGE`] old is accepted; the
//! event key stops a replay inside that window.
//!
//! Events (`event-data.event`): `accepted` → accepted; `delivered` → delivered; `failed` with
//! severity `temporary` → deferred; `failed` permanent with reason `bounce` → bounced, any other
//! permanent reason (Mailgun's suppression lists, `old` for an expired retry window) → rejected;
//! `rejected` → rejected; `complained` → complaint; `unsubscribed` → unsubscribed. Opens, clicks
//! and storage events produce nothing (Norbelys tracks engagement itself). Our message id comes
//! from the user variable named [`crate::compose::MESSAGE_TAG`].

use aws_lc_rs::hmac;
use jiff::{SignedDuration, Timestamp};
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::Value;

use super::{Event, EventKind, KeyError, ParseError, Receipt, VerifyError};
use crate::compose::MESSAGE_TAG;
use crate::status::EnhancedStatus;

/// How old a signed timestamp may be: Mailgun's retry schedule plus margin.
pub const MAX_AGE: SignedDuration = SignedDuration::from_hours(12);

/// The account's HTTP webhook signing key (not the sending API key).
#[derive(Debug, Clone)]
pub struct MailgunKey(hmac::Key);

impl MailgunKey {
    /// The key from Mailgun's "HTTP webhook signing key".
    ///
    /// # Errors
    ///
    /// The key is empty.
    pub fn new(signing_key: &SecretString) -> Result<Self, KeyError> {
        let key = signing_key.expose_secret().trim();
        if key.is_empty() {
            return Err(KeyError("the Mailgun signing key is empty"));
        }
        Ok(Self(hmac::Key::new(hmac::HMAC_SHA256, key.as_bytes())))
    }
}

/// Verifies one Mailgun webhook body and returns its one receipt: the event's `id`, and the
/// event alone (`{"event-data": …}`) without the `signature` member. Mailgun signs every retry
/// anew (a fresh timestamp and token), so the event, not the body, is what stays the same across
/// retries: a replay is recognised by its unchanged bytes, and only a changed event is a
/// different body.
///
/// # Errors
///
/// [`VerifyError`]: too large, a missing or wrong signature, a stale timestamp, or no event id.
pub fn verify(key: &MailgunKey, body: &[u8], now: Timestamp) -> Result<Receipt, VerifyError> {
    if body.len() > super::MAX_BODY {
        return Err(VerifyError::TooLarge);
    }
    let payload: Value = serde_json::from_slice(body)
        .map_err(|error| VerifyError::InvalidPayload(error.to_string()))?;
    let field = |name: &str| {
        payload
            .pointer(&format!("/signature/{name}"))
            .and_then(Value::as_str)
    };
    let (Some(timestamp), Some(token), Some(signature)) =
        (field("timestamp"), field("token"), field("signature"))
    else {
        return Err(VerifyError::Unauthorized(
            "the signature fields are missing",
        ));
    };
    if token.is_empty() || token.len() > 256 || timestamp.len() > 20 {
        return Err(VerifyError::Unauthorized(
            "the signature fields are malformed",
        ));
    }
    let signature = hex(signature).ok_or(VerifyError::Unauthorized("the signature is not hex"))?;
    hmac::verify(&key.0, format!("{timestamp}{token}").as_bytes(), &signature)
        .map_err(|_| VerifyError::Unauthorized("the signature does not verify"))?;
    let seconds = timestamp
        .parse()
        .map_err(|_| VerifyError::Unauthorized("the timestamp is not a number"))?;
    super::fresh(seconds, now, MAX_AGE)?;
    let event_id = super::event_id(payload.pointer("/event-data/id").and_then(Value::as_str))?;
    let event = payload.get("event-data").cloned().unwrap_or(Value::Null);
    let raw = serde_json::to_vec(&serde_json::json!({ "event-data": event }))
        .map_err(|error| VerifyError::InvalidPayload(error.to_string()))?;
    Ok(Receipt { event_id, raw })
}

/// The events of a stored receipt: none for event types Norbelys does not record, else one.
///
/// # Errors
///
/// The receipt is not a Mailgun event (no `event-data`, no timestamp).
pub fn events(raw: &[u8]) -> Result<Vec<Event>, ParseError> {
    let payload: Value =
        serde_json::from_slice(raw).map_err(|error| ParseError(error.to_string()))?;
    let data = payload
        .get("event-data")
        .ok_or_else(|| ParseError("no event-data".to_owned()))?;
    let text = |pointer: &str| data.pointer(pointer).and_then(Value::as_str);
    let kind = match (text("/event"), text("/severity"), text("/reason")) {
        (Some("accepted"), _, _) => EventKind::Accepted,
        (Some("delivered"), _, _) => EventKind::Delivered,
        (Some("failed"), Some("temporary"), _) => EventKind::Deferred,
        (Some("failed"), _, Some("bounce")) => EventKind::Bounced,
        (Some("failed" | "rejected"), _, _) => EventKind::Rejected,
        (Some("complained"), _, _) => EventKind::Complaint,
        (Some("unsubscribed"), _, _) => EventKind::Unsubscribed,
        _ => return Ok(Vec::new()),
    };
    let event_id = super::event_id(text("/id")).map_err(|error| ParseError(error.to_string()))?;
    let observed_at = super::unix_seconds(data.get("timestamp"))
        .ok_or_else(|| ParseError("no timestamp".to_owned()))?;
    let message = text("/delivery-status/message").filter(|message| !message.is_empty());
    let description =
        text("/delivery-status/description").filter(|description| !description.is_empty());
    let detail = message.or(description);
    let status = text("/delivery-status/enhanced-code")
        .and_then(|code| code.parse::<EnhancedStatus>().ok())
        .or_else(|| detail.and_then(EnhancedStatus::find));
    let smtp_code = data
        .pointer("/delivery-status/code")
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok())
        .filter(|code| (200..600).contains(code));
    Ok(vec![Event {
        event_id,
        kind,
        message_id: super::message_tag(
            data.pointer(&format!("/user-variables/{MESSAGE_TAG}"))
                .and_then(Value::as_str),
        ),
        internet_message_id: text("/message/headers/message-id")
            .map(|id| crate::text::bounded(id, 998)),
        provider_message_id: None,
        recipient: super::recipient(text("/recipient")),
        status,
        smtp_code,
        diagnostic: super::diagnostic(detail),
        observed_at,
        provenance: None,
    }])
}

/// Lowercase or uppercase hex to bytes; `None` for anything else.
fn hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || text.len() > 256 {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect()
}

#[cfg(test)]
mod tests;
