//! SendGrid's Event Webhook (<https://www.twilio.com/docs/sendgrid/for-developers/tracking-events/getting-started-event-webhook-security-features>).
//!
//! Verification: SendGrid signs `timestamp` followed by the exact body with ECDSA P-256 over
//! SHA-256, sending the base64 DER signature in `X-Twilio-Email-Event-Webhook-Signature` and the
//! timestamp in `X-Twilio-Email-Event-Webhook-Timestamp`. The verification key is the public key
//! SendGrid shows for the webhook (base64 DER `SubjectPublicKeyInfo`); only SendGrid holds the
//! private key, so a stolen public key cannot forge an event.
//!
//! Freshness: SendGrid retries a failed batch for 24 hours, so a signed timestamp up to
//! [`MAX_AGE`] old is accepted; the event key stops a replay inside that window.
//!
//! The body is a JSON array of events; each becomes one receipt keyed by `sg_event_id`, holding
//! that element's exact bytes. Events: `processed` → accepted; `deferred` → deferred;
//! `delivered` → delivered; `bounce` of type `bounce` → bounced, of type `blocked` → rejected;
//! `dropped` → rejected (SendGrid's own suppression or an invalid message); `spamreport` →
//! complaint; `unsubscribe` → unsubscribed. Group unsubscribes, opens and clicks produce nothing:
//! a group opt-out is not a global unsubscribe, and Norbelys tracks engagement itself. Our
//! message id comes back as the unique argument [`crate::compose::MESSAGE_TAG`], a top-level
//! field of each event.

use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1, ParsedPublicKey};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use jiff::{SignedDuration, Timestamp};
use serde_json::Value;
use serde_json::value::RawValue;

use super::{Event, EventKind, KeyError, ParseError, Receipt, VerifyError};
use crate::compose::MESSAGE_TAG;
use crate::status::EnhancedStatus;

/// How old a signed timestamp may be: SendGrid's 24-hour retry window plus margin.
pub const MAX_AGE: SignedDuration = SignedDuration::from_hours(25);

/// The webhook's ECDSA P-256 verification key.
#[derive(Debug)]
pub struct SendgridKey(ParsedPublicKey);

impl SendgridKey {
    /// The key from SendGrid's webhook settings: base64 of a DER `SubjectPublicKeyInfo`.
    ///
    /// # Errors
    ///
    /// The text is not base64 of a P-256 public key.
    pub fn new(public_key: &str) -> Result<Self, KeyError> {
        let der = STANDARD
            .decode(public_key.trim())
            .map_err(|_| KeyError("the SendGrid key is not base64"))?;
        let key = ParsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, der)
            .map_err(|_| KeyError("the SendGrid key is not a P-256 public key"))?;
        Ok(Self(key))
    }
}

/// Verifies one SendGrid batch and returns one receipt per event.
///
/// # Errors
///
/// [`VerifyError`]: too large, a missing or wrong signature, a stale timestamp, a body that is
/// not an array of events, or an event without `sg_event_id`.
pub fn verify(
    key: &SendgridKey,
    headers: &http::HeaderMap,
    body: &[u8],
    now: Timestamp,
) -> Result<Vec<Receipt>, VerifyError> {
    if body.len() > super::MAX_BODY {
        return Err(VerifyError::TooLarge);
    }
    let timestamp = super::header(headers, "x-twilio-email-event-webhook-timestamp")
        .ok_or(VerifyError::Unauthorized("the timestamp header is missing"))?;
    let signature = super::header(headers, "x-twilio-email-event-webhook-signature")
        .filter(|signature| signature.len() <= 512)
        .ok_or(VerifyError::Unauthorized("the signature header is missing"))?;
    let signature = STANDARD
        .decode(signature)
        .map_err(|_| VerifyError::Unauthorized("the signature is not base64"))?;
    let mut signed = Vec::with_capacity(timestamp.len() + body.len());
    signed.extend_from_slice(timestamp.as_bytes());
    signed.extend_from_slice(body);
    key.0
        .verify_sig(&signed, &signature)
        .map_err(|_| VerifyError::Unauthorized("the signature does not verify"))?;
    let seconds = timestamp
        .parse()
        .map_err(|_| VerifyError::Unauthorized("the timestamp is not a number"))?;
    super::fresh(seconds, now, MAX_AGE)?;

    let batch: Vec<&RawValue> = serde_json::from_slice(body)
        .map_err(|error| VerifyError::InvalidPayload(error.to_string()))?;
    if batch.len() > super::MAX_EVENTS {
        return Err(VerifyError::TooLarge);
    }
    batch
        .into_iter()
        .map(|raw| {
            let event: Value = serde_json::from_str(raw.get())
                .map_err(|error| VerifyError::InvalidPayload(error.to_string()))?;
            let event_id = super::event_id(event.get("sg_event_id").and_then(Value::as_str))?;
            Ok(Receipt {
                event_id,
                raw: raw.get().as_bytes().to_vec(),
            })
        })
        .collect()
}

/// The events of a stored receipt (one SendGrid event): none for event types Norbelys does not
/// record, else one.
///
/// # Errors
///
/// The receipt is not a SendGrid event (no `sg_event_id` or `timestamp`).
pub fn events(raw: &[u8]) -> Result<Vec<Event>, ParseError> {
    let event: Value =
        serde_json::from_slice(raw).map_err(|error| ParseError(error.to_string()))?;
    let text = |name: &str| event.get(name).and_then(Value::as_str);
    let kind = match (text("event"), text("type")) {
        (Some("processed"), _) => EventKind::Accepted,
        (Some("deferred"), _) => EventKind::Deferred,
        (Some("delivered"), _) => EventKind::Delivered,
        (Some("bounce"), Some("blocked")) => EventKind::Rejected,
        (Some("bounce"), _) => EventKind::Bounced,
        (Some("dropped"), _) => EventKind::Rejected,
        (Some("spamreport"), _) => EventKind::Complaint,
        (Some("unsubscribe"), _) => EventKind::Unsubscribed,
        _ => return Ok(Vec::new()),
    };
    let event_id =
        super::event_id(text("sg_event_id")).map_err(|error| ParseError(error.to_string()))?;
    let observed_at = super::unix_seconds(event.get("timestamp"))
        .ok_or_else(|| ParseError("no timestamp".to_owned()))?;
    let detail = text("reason")
        .filter(|reason| !reason.is_empty())
        .or_else(|| text("response"));
    let status = text("status")
        .and_then(|status| status.parse::<EnhancedStatus>().ok())
        .or_else(|| detail.and_then(EnhancedStatus::find));
    Ok(vec![Event {
        event_id,
        kind,
        message_id: super::message_tag(text(MESSAGE_TAG)),
        internet_message_id: text("smtp-id").map(|id| crate::text::bounded(id, 998)),
        provider_message_id: text("sg_message_id").map(|id| crate::text::bounded(id, 256)),
        recipient: super::recipient(text("email")),
        status,
        smtp_code: detail.and_then(super::smtp_code),
        diagnostic: super::diagnostic(detail),
        observed_at,
        provenance: None,
    }])
}

#[cfg(test)]
mod tests;
