//! Amazon SES events, published by a configuration set's event destination to an Amazon SNS
//! topic that posts to our endpoint.
//!
//! Verification of an SNS message
//! (<https://docs.aws.amazon.com/sns/latest/dg/sns-verify-signature-of-message.html>):
//! - Anyone can create an SNS topic and subscribe our URL to it, and SNS signs every message it
//!   sends, so a valid signature alone proves nothing: the message's `TopicArn` must be the
//!   topic configured for this webhook ([`SnsTopic`]).
//! - The signing certificate is fetched from `SigningCertURL` only when that URL is `https` on
//!   the SNS host of the topic's own Region (`sns.<region>.amazonaws.com`) with a
//!   `SimpleNotificationService-<id>.pem` path; the HTTPS connection to that host is what makes
//!   the certificate trustworthy, as in AWS's own validators. Fetches are bounded in time, size
//!   and concurrency ([`SnsCertificates`]), and certificates are cached by URL for an hour.
//! - `SignatureVersion` 1 is RSA PKCS #1 v1.5 with SHA-1, version 2 the same with SHA-256, over
//!   the message's fields in AWS's fixed order, each as `name\nvalue\n`.
//! - The message's `Timestamp` must be within [`MAX_AGE`]: SNS retries an HTTP delivery for at
//!   most an hour, and the event key stops a replay inside the window.
//!
//! A `SubscriptionConfirmation` is returned to the caller, which confirms it with
//! [`confirm_subscription`]; the confirmation URL is rebuilt from the topic and the token, never
//! taken from the message. An `UnsubscribeConfirmation` is acknowledged without effect (an
//! operator may have unsubscribed on purpose; it is never re-subscribed automatically).
//!
//! Events (`eventType`, or `notificationType` in SES's older notification format,
//! <https://docs.aws.amazon.com/ses/latest/dg/event-publishing-retrieving-sns-contents.html>):
//! - `Send` → accepted (message-level); `Delivery` → delivered, per recipient;
//!   `DeliveryDelay` → deferred, per recipient;
//! - `Bounce`: `Permanent` with subtype `General` or `NoEmail` → bounced; every other bounce
//!   (SES's own suppression lists, `Transient` bounces SES stopped retrying, `Undetermined`) →
//!   rejected, because none of them proves the address invalid;
//! - `Complaint` → complaint for its one recipient; when SES lists several recipients that *may*
//!   have complained, one complaint without a recipient (nobody is named); subtype
//!   `OnAccountSuppressionList` → rejected; feedback type `not-spam` → nothing;
//! - `Reject` and `Rendering Failure` → rejected (message-level); opens, clicks and subscription
//!   events → nothing.
//!
//! SES replaces the `Message-ID` header with its own value: an event names the message by our
//! message tag (`mail.tags`, [`crate::compose::MESSAGE_TAG`]), by SES's token (`mail.messageId`)
//! and, unless SES truncated the headers, by the original `Message-ID` (`mail.headers`).

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use aws_lc_rs::signature::{
    RSA_PKCS1_2048_8192_SHA1_FOR_LEGACY_USE_ONLY, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey,
    VerificationAlgorithm,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use jiff::{SignedDuration, Timestamp};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use url::Url;

use super::{Event, EventKind, KeyError, ParseError, Receipt, VerifyError};
use crate::compose::MESSAGE_TAG;
use crate::http::{self, ApiError, HttpClient};
use crate::status::EnhancedStatus;

/// How old an SNS message's `Timestamp` may be.
pub const MAX_AGE: SignedDuration = SignedDuration::from_hours(2);
/// How long a fetched signing certificate is reused.
const CERTIFICATE_TTL: Duration = Duration::from_secs(3_600);
/// The largest certificate document read.
const CERTIFICATE_LIMIT: usize = 64 * 1024;
/// How many certificates are cached.
const CACHE_ENTRIES: usize = 32;
/// How long fetching a certificate, permit included, may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The SNS topic a webhook accepts messages from, with the SNS host of its Region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnsTopic {
    arn: String,
    host: String,
}

impl SnsTopic {
    /// The topic from its ARN, `arn:<partition>:sns:<region>:<account>:<name>`.
    ///
    /// # Errors
    ///
    /// The ARN is not an SNS topic ARN in the `aws`, `aws-cn` or `aws-us-gov` partition.
    pub fn new(arn: &str) -> Result<Self, KeyError> {
        let invalid = KeyError("not an SNS topic ARN");
        let parts: Vec<&str> = arn.trim().split(':').collect();
        let ["arn", partition, "sns", region, account, name] = parts.as_slice() else {
            return Err(invalid);
        };
        let suffix = match *partition {
            "aws" | "aws-us-gov" => "amazonaws.com",
            "aws-cn" => "amazonaws.com.cn",
            _ => return Err(invalid),
        };
        let region_ok = (5..=32).contains(&region.len())
            && region
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        let account_ok = account.len() == 12 && account.bytes().all(|byte| byte.is_ascii_digit());
        let name_ok = (1..=256).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
        if !(region_ok && account_ok && name_ok) {
            return Err(invalid);
        }
        Ok(Self {
            arn: arn.trim().to_owned(),
            host: format!("sns.{region}.{suffix}"),
        })
    }

    /// The topic's ARN.
    #[must_use]
    pub fn arn(&self) -> &str {
        &self.arn
    }
}

/// The signing certificates of SNS, fetched on demand and cached by URL. One per process: its
/// semaphore bounds how many fetches run at once, whatever the number of webhooks.
pub struct SnsCertificates {
    cache: Mutex<HashMap<String, Cached>>,
    fetches: Semaphore,
}

struct Cached {
    spki: Vec<u8>,
    until: Instant,
}

impl std::fmt::Debug for SnsCertificates {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnsCertificates")
            .finish_non_exhaustive()
    }
}

impl SnsCertificates {
    /// An empty cache allowing `max_concurrent_fetches` fetches at once (at least one, at most
    /// tokio's semaphore maximum).
    #[must_use]
    pub fn new(max_concurrent_fetches: usize) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            fetches: Semaphore::new(max_concurrent_fetches.clamp(1, Semaphore::MAX_PERMITS)),
        }
    }

    /// Caches `spki` (a DER `SubjectPublicKeyInfo`) as the key of the certificate at `url` for
    /// the cache's lifetime, so a caller's tests verify messages they signed themselves without
    /// reaching SNS. Compiled for this crate's tests and, through the `test-support` feature, for
    /// the tests of its callers.
    #[cfg(any(test, feature = "test-support"))]
    pub fn preload(&self, url: &str, spki: Vec<u8>) {
        let until = Instant::now()
            .checked_add(CERTIFICATE_TTL)
            .unwrap_or_else(Instant::now);
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(url.to_owned(), Cached { spki, until });
    }

    fn cached(&self, url: &Url) -> Option<Vec<u8>> {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .get(url.as_str())
            .filter(|cached| cached.until > Instant::now())
            .map(|cached| cached.spki.clone())
    }

    /// The `SubjectPublicKeyInfo` of the certificate at `url`, from the cache or fetched.
    async fn get(&self, http: &HttpClient, url: &Url) -> Result<Vec<u8>, VerifyError> {
        if let Some(spki) = self.cached(url) {
            return Ok(spki);
        }
        let deadline = Instant::now()
            .checked_add(FETCH_TIMEOUT)
            .unwrap_or_else(Instant::now);
        let _permit = tokio::time::timeout_at(deadline, self.fetches.acquire())
            .await
            .map_err(|_| {
                VerifyError::Unavailable("too many certificate fetches at once".to_owned())
            })?
            .map_err(|_| {
                VerifyError::Unavailable("the certificate fetcher is closed".to_owned())
            })?;
        if let Some(spki) = self.cached(url) {
            return Ok(spki);
        }
        let unavailable = |error: ApiError| VerifyError::Unavailable(error.to_string());
        let response = http::send(http.get(url.clone()), deadline)
            .await
            .map_err(unavailable)?;
        if !response.status().is_success() {
            return Err(VerifyError::Unavailable(format!(
                "the certificate URL answered {}",
                response.status().as_u16()
            )));
        }
        let pem = http::read_body(response, CERTIFICATE_LIMIT)
            .await
            .map_err(unavailable)?;
        let spki = spki_from_pem(&pem).ok_or(VerifyError::Unauthorized(
            "the signing certificate cannot be parsed",
        ))?;
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.len() >= CACHE_ENTRIES {
            cache.clear();
        }
        let until = Instant::now()
            .checked_add(CERTIFICATE_TTL)
            .unwrap_or_else(Instant::now);
        cache.insert(
            url.as_str().to_owned(),
            Cached {
                spki: spki.clone(),
                until,
            },
        );
        Ok(spki)
    }
}

/// The public key of a PEM X.509 certificate, parsed by the X.509 parser rustls itself uses.
fn spki_from_pem(pem: &[u8]) -> Option<Vec<u8>> {
    let der = CertificateDer::from_pem_slice(pem).ok()?;
    let certificate = webpki::EndEntityCert::try_from(&der).ok()?;
    Some(certificate.subject_public_key_info().as_ref().to_vec())
}

/// A verified SNS message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnsMessage {
    /// An SES event: its receipt (SNS's `MessageId`, and the SES event JSON SNS carried).
    Notification(Receipt),
    /// SNS asks to confirm the subscription of our URL to the topic.
    SubscriptionConfirmation(SubscriptionConfirmation),
    /// SNS confirms that the subscription was removed: nothing to do.
    UnsubscribeConfirmation,
}

/// A verified request to confirm a subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionConfirmation {
    /// SNS's id of the confirmation message.
    pub message_id: String,
    token: String,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "Type")]
    kind: String,
    #[serde(rename = "MessageId")]
    message_id: String,
    #[serde(rename = "TopicArn")]
    topic_arn: String,
    #[serde(rename = "Subject")]
    subject: Option<String>,
    #[serde(rename = "Message")]
    message: String,
    #[serde(rename = "Timestamp")]
    timestamp: String,
    #[serde(rename = "SignatureVersion")]
    signature_version: String,
    #[serde(rename = "Signature")]
    signature: String,
    #[serde(rename = "SigningCertURL")]
    signing_cert_url: String,
    #[serde(rename = "Token")]
    token: Option<String>,
    #[serde(rename = "SubscribeURL")]
    subscribe_url: Option<String>,
}

/// Verifies one SNS message for `topic` (the module's documentation lists every check).
///
/// # Errors
///
/// [`VerifyError::Unavailable`] when the signing certificate could not be fetched (SNS retries);
/// the other variants refuse the message: too large, another topic, an unknown signature
/// version, a certificate URL outside the topic's SNS host, a stale timestamp, a signature that
/// does not verify, an unknown message type.
pub async fn verify(
    topic: &SnsTopic,
    certificates: &SnsCertificates,
    http: &HttpClient,
    body: &[u8],
    now: Timestamp,
) -> Result<SnsMessage, VerifyError> {
    if body.len() > super::MAX_BODY {
        return Err(VerifyError::TooLarge);
    }
    let envelope: Envelope = serde_json::from_slice(body)
        .map_err(|error| VerifyError::InvalidPayload(error.to_string()))?;
    if envelope.topic_arn != topic.arn {
        return Err(VerifyError::Unauthorized(
            "the message is from another topic",
        ));
    }
    let algorithm: &'static dyn VerificationAlgorithm = match envelope.signature_version.as_str() {
        "1" => &RSA_PKCS1_2048_8192_SHA1_FOR_LEGACY_USE_ONLY,
        "2" => &RSA_PKCS1_2048_8192_SHA256,
        _ => return Err(VerifyError::Unauthorized("an unknown signature version")),
    };
    let url = certificate_url(topic, &envelope.signing_cert_url)?;
    let canonical = string_to_sign(&envelope)?;
    let signature = STANDARD
        .decode(envelope.signature.as_bytes())
        .ok()
        .filter(|signature| (128..=1024).contains(&signature.len()))
        .ok_or(VerifyError::Unauthorized(
            "the signature is not base64 of an RSA signature",
        ))?;
    let spki = certificates.get(http, &url).await?;
    UnparsedPublicKey::new(algorithm, &spki)
        .verify(&canonical, &signature)
        .map_err(|_| VerifyError::Unauthorized("the signature does not verify"))?;
    let signed_at: Timestamp = envelope
        .timestamp
        .parse()
        .map_err(|_| VerifyError::Unauthorized("the timestamp is not RFC 3339"))?;
    super::fresh(signed_at.as_second(), now, MAX_AGE)?;
    let message_id = super::event_id(Some(&envelope.message_id))?;
    match envelope.kind.as_str() {
        "Notification" => Ok(SnsMessage::Notification(Receipt {
            event_id: message_id,
            raw: envelope.message.into_bytes(),
        })),
        "SubscriptionConfirmation" => {
            let token = envelope
                .token
                .filter(|token| {
                    (1..=4096).contains(&token.len())
                        && token.bytes().all(|byte| byte.is_ascii_graphic())
                })
                .ok_or_else(|| {
                    VerifyError::InvalidPayload("a confirmation without a usable token".to_owned())
                })?;
            Ok(SnsMessage::SubscriptionConfirmation(
                SubscriptionConfirmation { message_id, token },
            ))
        }
        "UnsubscribeConfirmation" => Ok(SnsMessage::UnsubscribeConfirmation),
        _ => Err(VerifyError::InvalidPayload(
            "an unknown SNS message type".to_owned(),
        )),
    }
}

/// The certificate URL, accepted only on the topic's SNS host.
fn certificate_url(topic: &SnsTopic, raw: &str) -> Result<Url, VerifyError> {
    let refused = VerifyError::Unauthorized("the certificate URL is not the topic's SNS host");
    let url = Url::parse(raw).map_err(|_| refused.clone())?;
    let file = url
        .path()
        .strip_prefix("/SimpleNotificationService-")
        .and_then(|path| path.strip_suffix(".pem"));
    let valid = url.scheme() == "https"
        && url.host_str() == Some(topic.host.as_str())
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && file.is_some_and(|file| {
            (1..=128).contains(&file.len()) && file.bytes().all(|byte| byte.is_ascii_alphanumeric())
        });
    if valid { Ok(url) } else { Err(refused) }
}

/// The string SNS signed: the type's fields in AWS's order, each `name\nvalue\n`.
fn string_to_sign(envelope: &Envelope) -> Result<Vec<u8>, VerifyError> {
    fn push(out: &mut String, name: &str, value: &str) {
        out.push_str(name);
        out.push('\n');
        out.push_str(value);
        out.push('\n');
    }
    let mut out = String::with_capacity(envelope.message.len() + 512);
    match envelope.kind.as_str() {
        "Notification" => {
            push(&mut out, "Message", &envelope.message);
            push(&mut out, "MessageId", &envelope.message_id);
            if let Some(subject) = &envelope.subject {
                push(&mut out, "Subject", subject);
            }
            push(&mut out, "Timestamp", &envelope.timestamp);
            push(&mut out, "TopicArn", &envelope.topic_arn);
            push(&mut out, "Type", &envelope.kind);
        }
        "SubscriptionConfirmation" | "UnsubscribeConfirmation" => {
            let missing = || {
                VerifyError::InvalidPayload("a confirmation without its URL or token".to_owned())
            };
            push(&mut out, "Message", &envelope.message);
            push(&mut out, "MessageId", &envelope.message_id);
            push(
                &mut out,
                "SubscribeURL",
                envelope.subscribe_url.as_deref().ok_or_else(missing)?,
            );
            push(&mut out, "Timestamp", &envelope.timestamp);
            push(
                &mut out,
                "Token",
                envelope.token.as_deref().ok_or_else(missing)?,
            );
            push(&mut out, "TopicArn", &envelope.topic_arn);
            push(&mut out, "Type", &envelope.kind);
        }
        _ => {
            return Err(VerifyError::InvalidPayload(
                "an unknown SNS message type".to_owned(),
            ));
        }
    }
    Ok(out.into_bytes())
}

/// Confirms the subscription of our URL to `topic` (`ConfirmSubscription` on the topic's SNS
/// host, with the confirmation's token). Idempotent: confirming twice is harmless.
///
/// # Errors
///
/// SNS refused or did not answer by `deadline` ([`ApiError`]).
pub async fn confirm_subscription(
    http: &HttpClient,
    topic: &SnsTopic,
    confirmation: &SubscriptionConfirmation,
    deadline: Instant,
) -> Result<(), ApiError> {
    let mut url = Url::parse(&format!("https://{}/", topic.host))
        .map_err(|error| ApiError::InvalidResponse(error.to_string()))?;
    url.query_pairs_mut()
        .append_pair("Action", "ConfirmSubscription")
        .append_pair("TopicArn", &topic.arn)
        .append_pair("Token", &confirmation.token);
    let response = http::send(http.get(url), deadline).await?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(http::status_error(response).await)
    }
}

/// The events of a stored receipt (one SES event; `event_id` is the receipt's), by the mapping
/// of this module's documentation.
///
/// # Errors
///
/// The receipt is not an SES event (no event type, no `mail` object, no time).
pub fn events(raw: &[u8], event_id: &str) -> Result<Vec<Event>, ParseError> {
    let event: Value =
        serde_json::from_slice(raw).map_err(|error| ParseError(error.to_string()))?;
    let name = event
        .get("eventType")
        .or_else(|| event.get("notificationType"))
        .and_then(Value::as_str)
        .ok_or_else(|| ParseError("no event type".to_owned()))?;
    let mail = event
        .get("mail")
        .ok_or_else(|| ParseError("no mail object".to_owned()))?;
    let base = Base::new(event_id, mail)?;
    let detail = |section: &str| event.get(section);
    let time = |section: &str| {
        detail(section)
            .and_then(|section| section.get("timestamp"))
            .and_then(Value::as_str)
            .and_then(|at| at.parse::<Timestamp>().ok())
            .unwrap_or(base.sent_at)
    };
    let text = |section: &str, field: &str| {
        detail(section)
            .and_then(|section| section.get(field))
            .and_then(Value::as_str)
    };
    let list = |section: &str, field: &str| {
        detail(section)
            .and_then(|section| section.get(field))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let events = match name {
        "Send" => vec![base.event(EventKind::Accepted, None, None, base.sent_at)],
        "Delivery" => {
            let response = text("delivery", "smtpResponse");
            list("delivery", "recipients")
                .iter()
                .map(|recipient| {
                    base.event(
                        EventKind::Delivered,
                        recipient.as_str(),
                        response,
                        time("delivery"),
                    )
                })
                .collect()
        }
        "DeliveryDelay" => list("deliveryDelay", "delayedRecipients")
            .iter()
            .map(|recipient| {
                base.recipient_event(EventKind::Deferred, recipient, time("deliveryDelay"))
            })
            .collect(),
        "Bounce" => {
            let kind = match (
                text("bounce", "bounceType"),
                text("bounce", "bounceSubType"),
            ) {
                (Some("Permanent"), Some("General" | "NoEmail")) => EventKind::Bounced,
                _ => EventKind::Rejected,
            };
            list("bounce", "bouncedRecipients")
                .iter()
                .map(|recipient| base.recipient_event(kind, recipient, time("bounce")))
                .collect()
        }
        "Complaint" => {
            let at = time("complaint");
            let recipients = list("complaint", "complainedRecipients");
            if text("complaint", "complaintSubType") == Some("OnAccountSuppressionList") {
                recipients
                    .iter()
                    .map(|recipient| base.recipient_event(EventKind::Rejected, recipient, at))
                    .collect()
            } else if text("complaint", "complaintFeedbackType") == Some("not-spam") {
                Vec::new()
            } else {
                match recipients.as_slice() {
                    [only] => vec![base.recipient_event(EventKind::Complaint, only, at)],
                    _ => vec![base.event(
                        EventKind::Complaint,
                        None,
                        text("complaint", "complaintFeedbackType"),
                        at,
                    )],
                }
            }
        }
        "Reject" => vec![base.event(
            EventKind::Rejected,
            None,
            text("reject", "reason"),
            base.sent_at,
        )],
        "Rendering Failure" => {
            vec![base.event(
                EventKind::Rejected,
                None,
                text("failure", "errorMessage"),
                base.sent_at,
            )]
        }
        _ => Vec::new(),
    };
    Ok(events)
}

/// What every event of one SES message shares.
struct Base {
    event_id: String,
    message_id: Option<uuid::Uuid>,
    internet_message_id: Option<String>,
    token: Option<String>,
    sent_at: Timestamp,
}

impl Base {
    fn new(event_id: &str, mail: &Value) -> Result<Self, ParseError> {
        let sent_at = mail
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|at| at.parse::<Timestamp>().ok())
            .ok_or_else(|| ParseError("no mail timestamp".to_owned()))?;
        let tag = mail
            .pointer(&format!("/tags/{MESSAGE_TAG}/0"))
            .and_then(Value::as_str);
        let original = mail
            .get("headers")
            .and_then(Value::as_array)
            .and_then(|headers| {
                headers.iter().find_map(|header| {
                    let named = header
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| name.eq_ignore_ascii_case("message-id"));
                    named
                        .then(|| header.get("value").and_then(Value::as_str))
                        .flatten()
                })
            });
        Ok(Self {
            event_id: super::event_id(Some(event_id))
                .map_err(|error| ParseError(error.to_string()))?,
            message_id: super::message_tag(tag),
            internet_message_id: original.map(|id| crate::text::bounded(id, 998)),
            token: mail
                .get("messageId")
                .and_then(Value::as_str)
                .map(|id| crate::text::bounded(id, 256)),
            sent_at,
        })
    }

    fn event(
        &self,
        kind: EventKind,
        recipient: Option<&str>,
        detail: Option<&str>,
        observed_at: Timestamp,
    ) -> Event {
        Event {
            event_id: self.event_id.clone(),
            kind,
            message_id: self.message_id,
            internet_message_id: self.internet_message_id.clone(),
            provider_message_id: self.token.clone(),
            recipient: super::recipient(recipient),
            status: detail.and_then(EnhancedStatus::find),
            smtp_code: detail.and_then(super::smtp_code),
            diagnostic: super::diagnostic(detail),
            observed_at,
            provenance: None,
        }
    }

    /// An event for one entry of a recipients list (`emailAddress`, `status`, `diagnosticCode`).
    fn recipient_event(&self, kind: EventKind, entry: &Value, observed_at: Timestamp) -> Event {
        let text = |field: &str| entry.get(field).and_then(Value::as_str);
        let diagnostic = text("diagnosticCode");
        let mut event = self.event(kind, text("emailAddress"), diagnostic, observed_at);
        if let Some(status) =
            text("status").and_then(|status| status.parse::<EnhancedStatus>().ok())
        {
            event.status = Some(status);
        }
        event
    }
}

#[cfg(test)]
mod tests;
