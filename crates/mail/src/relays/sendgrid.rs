//! SendGrid's v3 API (<https://www.twilio.com/docs/sendgrid/api-reference>, read 2026-10-02), on
//! `https://api.sendgrid.com`, or `https://api.eu.sendgrid.com` for connections using
//! `smtp.eu.sendgrid.net`, with an API key as a bearer token. The SMTP host selects the same
//! region for permission checks and reconciliation, so an EU key is never sent to the global API.
//!
//! - `GET /v3/scopes` ([`scopes`]): the permissions of a key. A connection's daily check reads
//!   them with the key its SMTP login uses (SendGrid's SMTP user is `apikey` and the password an
//!   API key): `401` means the key was deleted or revoked; a list without `mail.send` means it
//!   can no longer send.
//! - The Email Activity API ([`activity`]), a paid add-on of the account: the messages whose
//!   unique argument names one of ours (`GET /v3/messages` with the query
//!   `(unique_args['norbelys_message_id']="…")`), then each one's events
//!   (`GET /v3/messages/{msg_id}`). The reconciliation reads it for messages whose submission
//!   answer was lost.
//!
//! Each Email Activity event becomes a receipt in the shape of the Event Webhook's events, which
//! [`crate::webhooks::sendgrid::events`] parses, so a reconciled event takes the path of every
//! other callback: `processed`, `delivered`, `deferred` keep their names; `bounced` becomes
//! `bounce`, of type `bounce` when SendGrid calls it hard and `blocked` when soft; `dropped` stays;
//! `spam_report` becomes `spamreport`; `unsubscribe` stays. Opens, clicks and group
//! subscriptions are left out, as the webhook parser leaves them. The Activity API names no event
//! id, so the receipt's id is built from what identifies the event, the same on every read:
//! `activity:<msg_id>:<event>:<unix seconds>`.

use jiff::Timestamp;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::time::Instant;
use url::Url;
use uuid::Uuid;

use crate::compose::MESSAGE_TAG;
use crate::http::{self, ApiError, HttpClient};
use crate::webhooks::Receipt;

/// The scope a key needs to send mail.
pub const MAIL_SEND: &str = "mail.send";
/// SendGrid's messages a reconciled message may have (one per personalization).
const MESSAGES_MAX: u32 = 10;

/// The API origin serving the connection's SMTP region. A custom or global SMTP host uses the
/// global API; the EU host is matched case-insensitively, including a DNS root suffix.
#[must_use]
pub fn api_base(smtp_host: &str) -> &'static str {
    if smtp_host
        .trim()
        .trim_end_matches('.')
        .eq_ignore_ascii_case("smtp.eu.sendgrid.net")
    {
        "https://api.eu.sendgrid.com"
    } else {
        "https://api.sendgrid.com"
    }
}

/// The scopes the API key `key` holds.
///
/// # Errors
///
/// The call failed ([`ApiError`]): [`ApiError::Unauthorized`] for a deleted or revoked key.
pub async fn scopes(
    http: &HttpClient,
    key: &SecretString,
    smtp_host: &str,
    deadline: Instant,
) -> Result<Vec<String>, ApiError> {
    #[derive(Deserialize)]
    struct Body {
        #[serde(default)]
        scopes: Vec<String>,
    }
    let body: Body = get(http, key, endpoint(smtp_host, &["v3", "scopes"])?, deadline).await?;
    Ok(body.scopes)
}

/// The events SendGrid's Email Activity holds for the caller's message `message`, as receipts
/// the Event Webhook parser reads (see the module); none when SendGrid has no message with that
/// unique argument.
///
/// # Errors
///
/// A call failed ([`ApiError`]): a `400` or `403` when the account lacks the Email Activity
/// add-on or the key its permission, a `429` when SendGrid throttles the reads.
pub async fn activity(
    http: &HttpClient,
    key: &SecretString,
    smtp_host: &str,
    message: Uuid,
    deadline: Instant,
) -> Result<Vec<Receipt>, ApiError> {
    #[derive(Deserialize)]
    struct List {
        #[serde(default)]
        messages: Vec<Summary>,
    }
    #[derive(Deserialize)]
    struct Summary {
        msg_id: String,
    }
    let mut url = endpoint(smtp_host, &["v3", "messages"])?;
    url.query_pairs_mut()
        .append_pair(
            "query",
            &format!("(unique_args['{MESSAGE_TAG}']=\"{message}\")"),
        )
        .append_pair("limit", &MESSAGES_MAX.to_string());
    let list: List = get(http, key, url, deadline).await?;
    let mut receipts = Vec::new();
    for summary in list.messages {
        if !valid_msg_id(&summary.msg_id) {
            continue;
        }
        let detail: Detail = get(
            http,
            key,
            endpoint(smtp_host, &["v3", "messages", summary.msg_id.as_str()])?,
            deadline,
        )
        .await?;
        receipts.extend(receipts_of(&summary.msg_id, message, &detail));
    }
    Ok(receipts)
}

/// One message of the Email Activity, as `GET /v3/messages/{msg_id}` returns it.
#[derive(Debug, Deserialize)]
struct Detail {
    to_email: Option<String>,
    #[serde(default)]
    events: Vec<ActivityEvent>,
}

/// One event of an Email Activity message.
#[derive(Debug, Deserialize)]
struct ActivityEvent {
    event_name: String,
    processed: Option<String>,
    reason: Option<String>,
    bounce_type: Option<String>,
}

/// The receipts of `detail`'s events, in the Event Webhook's shape (see the module); an event
/// of a kind the webhook parser does not record, or without a readable instant, is left out.
fn receipts_of(msg_id: &str, message: Uuid, detail: &Detail) -> Vec<Receipt> {
    detail
        .events
        .iter()
        .filter_map(|event| {
            let (name, kind) = match event.event_name.as_str() {
                "processed" => ("processed", None),
                "delivered" => ("delivered", None),
                "deferred" => ("deferred", None),
                "bounced" if event.bounce_type.as_deref() == Some("soft") => {
                    ("bounce", Some("blocked"))
                }
                "bounced" => ("bounce", Some("bounce")),
                "dropped" => ("dropped", None),
                "spam_report" => ("spamreport", None),
                "unsubscribe" => ("unsubscribe", None),
                _ => return None,
            };
            let at = event.processed.as_deref()?.parse::<Timestamp>().ok()?;
            let event_id = format!("activity:{msg_id}:{}:{}", event.event_name, at.as_second());
            if event_id.len() > 256 || !event_id.bytes().all(|byte| byte.is_ascii_graphic()) {
                return None;
            }
            let mut raw = json!({
                "sg_event_id": event_id,
                "sg_message_id": msg_id,
                "event": name,
                "type": kind,
                "email": detail.to_email,
                "timestamp": at.as_second(),
                "reason": event.reason,
            });
            if let Some(object) = raw.as_object_mut() {
                object.insert(MESSAGE_TAG.to_owned(), Value::from(message.to_string()));
            }
            Some(Receipt {
                event_id,
                raw: serde_json::to_vec(&raw).ok()?,
            })
        })
        .collect()
}

/// A SendGrid message id as the Activity API writes them (letters, digits, `.`, `-`, `_`): it
/// becomes a path segment.
fn valid_msg_id(id: &str) -> bool {
    (1..=200).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// The URL of `segments` on SendGrid's API.
fn endpoint(smtp_host: &str, segments: &[&str]) -> Result<Url, ApiError> {
    let mut url = Url::parse(api_base(smtp_host))
        .map_err(|error| ApiError::InvalidResponse(error.to_string()))?;
    url.path_segments_mut()
        .map_err(|()| ApiError::InvalidResponse("the SendGrid base is not a path".to_owned()))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

/// A `GET` of `url` with the key as the bearer token, read as JSON.
async fn get<T: DeserializeOwned>(
    http: &HttpClient,
    key: &SecretString,
    url: Url,
    deadline: Instant,
) -> Result<T, ApiError> {
    http::json(http::send(http.get(url).bearer_auth(key.expose_secret()), deadline).await?).await
}
