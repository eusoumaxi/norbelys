//! The Gmail API, for mailboxes connected through Google OAuth: submission with
//! `users.messages.send`, reading the mailbox through `users.history.list`, a message's raw
//! MIME, the Sent search that settles an `uncertain` submission, and the identity reads of a
//! connection check (`users.getProfile`, `users.settings.sendAs.list`). Reference:
//! <https://developers.google.com/workspace/gmail/api/reference/rest>.
//!
//! Submission:
//! - One request per submission, ending by the caller's deadline (at most
//!   [`crate::http::REQUEST_TIMEOUT`]); `200` with a message id is `accepted`.
//! - A request that may have reached Google without its reply (a timeout or a reset after the
//!   connection was made, a `5xx`) is `uncertain`: resending could deliver it twice. A failure
//!   to connect is `transient`, because nothing was sent.
//! - `429` and the rate-limit `403`s are throttles. A limit of Norbelys's own Google Cloud
//!   project, shared by every workspace, is scoped to the platform: `dailyLimitExceeded`, and
//!   any message Google words as "Quota exceeded for quota metric … for consumer
//!   'project_number:…'". Every other throttle (`rateLimitExceeded`, `userRateLimitExceeded`,
//!   "User-rate limit exceeded", the per-user sending limits) pauses the connection. Gmail's
//!   own "Retry after" time in the error message is used when no `Retry-After` header is sent.
//!   See <https://developers.google.com/workspace/gmail/api/guides/handle-errors>.
//! - `401` is an unauthorized credential; `domainPolicy` and other `403`s refuse the account
//!   (a Workspace admin disabled Gmail API access); other `4xx` refuse the message.
//!
//! Reading:
//! - The cursor keeps the history id and Google's opaque page token while a history round
//!   continues. Pending IDs preserve every message of a record larger than the caller's page.
//! - First reads and expired history cursors retain the message-list continuation until the
//!   complete overlapping snapshot is read. Each request and retained page remains bounded.
//! - Gmail keeps history for at least about a week; an older start answers `404`, which
//!   restarts at the caller's `since` through `messages.list` (`after:` its Unix time). The
//!   history id is read *before* that listing, so a message arriving during it is still found by
//!   the next history page. See <https://developers.google.com/workspace/gmail/api/guides/sync>.

use base64::Engine as _;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, URL_SAFE};
use jiff::Timestamp;
use reqwest::StatusCode;
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use url::Url;

use crate::http::{self, ApiError, HttpClient};
use crate::receive::{Page, RawMessage, Reset, ResetReason, TransportIdentity};
use crate::submission::{Cause, Failure, Phase, Rejection, Scope, Submission};

const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me/";

/// Gmail's `raw` is base64url, padded or not.
const URL_SAFE_LENIENT: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// The receive cursor the caller persists for this Gmail binding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailCursor {
    /// The history id after which changes are read.
    pub history_id: String,
    /// Opaque continuation of a history round; its starting history id stays fixed until done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_page_token: Option<String>,
    /// Overlapping message-list snapshot still in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resync: Option<GmailResync>,
    /// Listed ids waiting for their bounded handoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<crate::receive::Pending>,
}

/// A resync continues the same date-bounded list after the caller commits its previous page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailResync {
    pub since: Timestamp,
    pub page_token: String,
}

/// The mailbox `users.getProfile` names.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    /// The mailbox's address. The read scope already allows this call, and the address it names is
    /// the one a Google connection proves.
    pub email_address: String,
    /// The mailbox's current history id.
    pub history_id: String,
}

/// One address the mailbox may send as (`users.settings.sendAs`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendAs {
    /// The address.
    pub send_as_email: String,
    /// The mailbox's own address.
    #[serde(default)]
    pub is_primary: bool,
    /// `accepted` or `pending` for an alias; absent for the primary address.
    #[serde(default)]
    pub verification_status: Option<String>,
}

/// Submits `mime` with `users.messages.send` (the message's `Bcc` header names the blind
/// recipients; Gmail removes it).
///
/// # Errors
///
/// A [`Rejection`] in the `api` phase, by the mapping of this module's documentation.
pub async fn send(
    http: &HttpClient,
    token: &SecretString,
    mime: &[u8],
    deadline: Instant,
) -> Result<Submission, Rejection> {
    let url = endpoint("messages/send")
        .map_err(|error| local(Failure::Transient, Cause::NoReply, &error.to_string()))?;
    let body = serde_json::json!({ "raw": URL_SAFE.encode(mime) });
    let request = http
        .post(url)
        .bearer_auth(token.expose_secret())
        .json(&body);
    let response = http::submit(request, deadline).await?;
    let status = response.status();
    if status.is_success() {
        let body = http::read_body(response, http::ERROR_LIMIT)
            .await
            .unwrap_or_default();
        let id = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            });
        return Ok(Submission {
            provider_message_id: id,
            ..Submission::default()
        });
    }
    let retry_after = http::retry_after(response.headers(), Timestamp::now());
    let body = http::read_body(response, http::ERROR_LIMIT)
        .await
        .unwrap_or_default();
    Err(rejection(
        status,
        &http::error_reason(&body),
        &http::error_message(&body),
        retry_after,
    ))
}

/// The meaning of an error status to `messages.send`.
fn rejection(
    status: StatusCode,
    reason: &str,
    message: &str,
    retry_after: Option<Timestamp>,
) -> Rejection {
    let rate = status.as_u16() == 429 || (status.as_u16() == 403 && http::is_rate_reason(reason));
    let project = reason == "dailyLimitExceeded" || http::names_project(message);
    let exception = if rate {
        Some((
            Failure::Transient,
            if project {
                Scope::Platform
            } else {
                Scope::Connection
            },
            Cause::Throttled,
        ))
    } else if status.as_u16() == 400 && reason == "failedPrecondition" {
        Some((Failure::Transient, Scope::Connection, Cause::Forbidden))
    } else {
        None
    };
    http::rejection(
        status,
        reason,
        message,
        retry_after.or_else(|| retry_time(message)),
        exception,
    )
}

/// Gmail's own wait in an error message: `… Retry after 2026-10-01T12:00:00.000Z`.
fn retry_time(message: &str) -> Option<Timestamp> {
    let at = message.find("Retry after ")?;
    let stamp = message
        .get(at + "Retry after ".len()..)?
        .split_whitespace()
        .next()?;
    stamp
        .trim_end_matches('.')
        .parse::<Timestamp>()
        .ok()
        .filter(|at| *at > Timestamp::now())
}

fn local(failure: Failure, cause: Cause, detail: &str) -> Rejection {
    Rejection::local(failure, Phase::Api, Scope::Connection, cause, detail)
}

/// The mailbox's address and current history id (`users.getProfile`, 1 unit).
///
/// # Errors
///
/// The call failed ([`ApiError`]).
pub async fn profile(
    http: &HttpClient,
    token: &SecretString,
    deadline: Instant,
) -> Result<Profile, ApiError> {
    get(http, token, endpoint("profile")?, deadline).await
}

/// The addresses the mailbox may send as, with their verification (`users.settings.sendAs.list`,
/// 1 quota unit): how a check confirms that a sender identity may use its From address.
///
/// # Errors
///
/// The call failed ([`ApiError`]).
pub async fn send_as(
    http: &HttpClient,
    token: &SecretString,
    deadline: Instant,
) -> Result<Vec<SendAs>, ApiError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct List {
        #[serde(default)]
        send_as: Vec<SendAs>,
    }
    let list: List = get(http, token, endpoint("settings/sendAs")?, deadline).await?;
    Ok(list.send_as)
}

/// The next page of messages added to `label` (a label id: `INBOX` for the inbox), at most
/// `limit` ids. Without a cursor, or when Gmail no longer has the cursor's history, the page
/// lists the label's messages received after `since`, following all message-list pages before
/// reading history again. The history anchor is read before listing, preserving concurrent
/// arrivals; after a lost cursor the first page carries a [`Reset`].
///
/// # Errors
///
/// A call failed ([`crate::receive::Error`]); `404` on the history is not an error but a reset.
pub async fn changes(
    http: &HttpClient,
    token: &SecretString,
    label: &str,
    cursor: Option<&GmailCursor>,
    since: Timestamp,
    limit: u32,
    deadline: Instant,
) -> Result<Page<String, GmailCursor>, crate::receive::Error> {
    let result: Result<Page<String, GmailCursor>, ApiError> = async {
        let limit = limit.clamp(1, 500);
        if let Some((cursor, pending)) =
            cursor.and_then(|cursor| cursor.pending.as_ref().map(|pending| (cursor, pending)))
        {
            if pending.ids.len() > crate::receive::Pending::MAX {
                return Err(ApiError::InvalidResponse(
                    "pending Gmail page exceeds its bound".into(),
                ));
            }
            for id in &pending.ids {
                check_id(id)?;
            }
            let (ids, more, pending) = pending.page(limit);
            return Ok(Page {
                ids,
                more,
                cursor: GmailCursor {
                    pending,
                    ..cursor.clone()
                },
                reset: None,
            });
        }
        if let Some(cursor) = cursor.filter(|cursor| cursor.resync.is_some()) {
            return resync_page(http, token, label, cursor, since, limit, deadline).await;
        }
        let reason = match cursor {
            None => None,
            Some(cursor) => match history(http, token, label, cursor, limit, deadline).await {
                Err(ApiError::Status { status: 404, .. }) => Some(ResetReason::History),
                result => return result,
            },
        };
        let profile = profile(http, token, deadline).await?;
        let cursor = GmailCursor {
            history_id: profile.history_id,
            ..Default::default()
        };
        let mut page = resync_page(http, token, label, &cursor, since, limit, deadline).await?;
        page.reset = reason.map(|reason| Reset {
            reason,
            since,
            truncated: false,
        });
        Ok(page)
    }
    .await;
    result.map_err(Into::into)
}

/// A bounded list page, retaining its opaque continuation and the pre-list history anchor.
async fn resync_page(
    http: &HttpClient,
    token: &SecretString,
    label: &str,
    cursor: &GmailCursor,
    since: Timestamp,
    limit: u32,
    deadline: Instant,
) -> Result<Page<String, GmailCursor>, ApiError> {
    let since = cursor.resync.as_ref().map_or(since, |resync| resync.since);
    let page_token = cursor
        .resync
        .as_ref()
        .map(|resync| resync.page_token.as_str());
    check_page_token(page_token)?;
    let mut url = endpoint("messages")?;
    url.query_pairs_mut()
        .append_pair("labelIds", label)
        .append_pair("q", &format!("after:{}", since.as_second()))
        .append_pair("maxResults", &limit.to_string());
    if let Some(page_token) = page_token {
        url.query_pairs_mut().append_pair("pageToken", page_token);
    }
    let list: MessageList = get(http, token, url, deadline).await?;
    check_page_token(list.next_page_token.as_deref())?;
    let mut ids = unique_ids(list.messages.iter().map(|message| message.id.as_str()))?;
    ids.reverse();
    if ids.len() > crate::receive::Pending::MAX {
        return Err(ApiError::InvalidResponse(
            "Gmail page exceeds its retained bound".into(),
        ));
    }
    let (ids, more, pending) =
        crate::receive::Pending::split(ids, limit, list.next_page_token.is_some());
    Ok(Page {
        ids,
        cursor: GmailCursor {
            history_id: cursor.history_id.clone(),
            history_page_token: None,
            resync: list
                .next_page_token
                .map(|page_token| GmailResync { since, page_token }),
            pending,
        },
        more,
        reset: None,
    })
}

/// Tokens are opaque but bounded; percent-encoding prevents them from changing URL structure.
fn check_page_token(token: Option<&str>) -> Result<(), ApiError> {
    if token.is_some_and(|token| {
        token.is_empty() || token.len() > 16 * 1024 || token.chars().any(char::is_control)
    }) {
        return Err(ApiError::InvalidResponse("invalid Gmail page token".into()));
    }
    Ok(())
}

async fn history(
    http: &HttpClient,
    token: &SecretString,
    label: &str,
    cursor: &GmailCursor,
    limit: u32,
    deadline: Instant,
) -> Result<Page<String, GmailCursor>, ApiError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct History {
        #[serde(default)]
        history: Vec<Record>,
        next_page_token: Option<String>,
        history_id: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Record {
        #[serde(default)]
        messages_added: Vec<Added>,
    }
    #[derive(Deserialize)]
    struct Added {
        message: MessageRef,
    }

    if cursor.history_id.is_empty() || !cursor.history_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ApiError::InvalidResponse(
            "the history cursor is not a history id".to_owned(),
        ));
    }
    check_page_token(cursor.history_page_token.as_deref())?;
    let mut url = endpoint("history")?;
    url.query_pairs_mut()
        .append_pair("startHistoryId", &cursor.history_id)
        .append_pair("historyTypes", "messageAdded")
        .append_pair("labelId", label)
        .append_pair("maxResults", &limit.to_string());
    if let Some(page_token) = &cursor.history_page_token {
        url.query_pairs_mut().append_pair("pageToken", page_token);
    }
    let page: History = get(http, token, url, deadline).await?;
    check_page_token(page.next_page_token.as_deref())?;
    let ids = unique_ids(page.history.iter().flat_map(|record| {
        record
            .messages_added
            .iter()
            .map(|added| added.message.id.as_str())
    }))?;
    let more = page.next_page_token.is_some();
    let history_id = if more {
        cursor.history_id.clone()
    } else {
        page.history_id
    };
    if ids.len() > crate::receive::Pending::MAX {
        return Err(ApiError::InvalidResponse(
            "Gmail page exceeds its retained bound".into(),
        ));
    }
    let (ids, more, pending) = crate::receive::Pending::split(ids, limit, more);
    Ok(Page {
        ids,
        cursor: GmailCursor {
            history_id,
            history_page_token: page.next_page_token,
            resync: None,
            pending,
        },
        more,
        reset: None,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageList {
    #[serde(default)]
    messages: Vec<MessageRef>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct MessageRef {
    id: String,
}

/// One message's raw MIME (`users.messages.get`, `format=raw`), at most `max_bytes`. A message
/// too large to read whole comes back as its headers alone (`format=metadata`), marked
/// truncated.
///
/// # Errors
///
/// A call failed ([`crate::receive::Error`]), or Gmail's `raw` is not base64url.
pub async fn message(
    http: &HttpClient,
    token: &SecretString,
    id: &str,
    max_bytes: usize,
    deadline: Instant,
) -> Result<RawMessage, crate::receive::Error> {
    let result: Result<RawMessage, ApiError> = async {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            raw: String,
            size_estimate: Option<u64>,
            internal_date: Option<String>,
        }
        check_id(id)?;
        let mut url = endpoint(&format!("messages/{id}"))?;
        url.query_pairs_mut().append_pair("format", "raw");
        let limit = max_bytes.saturating_mul(4) / 3 + 64 * 1024;
        let response =
            http::send(http.get(url).bearer_auth(token.expose_secret()), deadline).await?;
        if !response.status().is_success() {
            return Err(http::status_error(response).await);
        }
        let body = match http::read_body(response, limit).await {
            Ok(body) => body,
            Err(ApiError::TooLarge(_)) => return headers_only(http, token, id, deadline).await,
            Err(error) => return Err(error),
        };
        let message: Raw = serde_json::from_slice(&body)
            .map_err(|error| ApiError::InvalidResponse(error.to_string()))?;
        let mut raw = URL_SAFE_LENIENT
            .decode(message.raw.as_bytes())
            .map_err(|error| ApiError::InvalidResponse(format!("raw is not base64url: {error}")))?;
        let truncated = raw.len() > max_bytes;
        raw.truncate(max_bytes);
        Ok(RawMessage {
            identity: TransportIdentity::Provider {
                provider_message_id: id.to_owned(),
            },
            raw,
            size: message.size_estimate,
            truncated,
            received_at: internal_date(message.internal_date.as_deref()),
        })
    }
    .await;
    result.map_err(Into::into)
}

async fn headers_only(
    http: &HttpClient,
    token: &SecretString,
    id: &str,
    deadline: Instant,
) -> Result<RawMessage, ApiError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Metadata {
        payload: Payload,
        size_estimate: Option<u64>,
        internal_date: Option<String>,
    }
    #[derive(Deserialize)]
    struct Payload {
        #[serde(default)]
        headers: Vec<Header>,
    }
    #[derive(Deserialize)]
    struct Header {
        name: String,
        value: String,
    }
    let mut url = endpoint(&format!("messages/{id}"))?;
    url.query_pairs_mut().append_pair("format", "metadata");
    let metadata: Metadata = get(http, token, url, deadline).await?;
    let mut raw = String::new();
    for header in &metadata.payload.headers {
        let clean = |text: &str| !text.contains(['\r', '\n']);
        if clean(&header.name)
            && clean(&header.value)
            && raw.len() + header.name.len() + header.value.len() < 64 * 1024
        {
            raw.push_str(&format!("{}: {}\r\n", header.name, header.value));
        }
    }
    raw.push_str("\r\n");
    Ok(RawMessage {
        identity: TransportIdentity::Provider {
            provider_message_id: id.to_owned(),
        },
        raw: raw.into_bytes(),
        size: metadata.size_estimate,
        truncated: true,
        received_at: internal_date(metadata.internal_date.as_deref()),
    })
}

/// Whether the mailbox's Sent label holds a message with this `Message-ID` (`messages.list`
/// with `rfc822msgid:`, 5 quota units): the read-only way to settle an `uncertain` submission.
/// A message sent through the API lands in the Sent label, so finding it proves acceptance;
/// finding nothing proves nothing (the search index may lag), and the message stays uncertain.
///
/// # Errors
///
/// The call failed ([`crate::receive::Error`]).
pub async fn find_sent(
    http: &HttpClient,
    token: &SecretString,
    internet_message_id: &str,
    deadline: Instant,
) -> Result<bool, crate::receive::Error> {
    let result: Result<bool, ApiError> = async {
        let id = internet_message_id
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>');
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(ApiError::InvalidResponse(
                "a Message-ID is printable ASCII".to_owned(),
            ));
        }
        let mut url = endpoint("messages")?;
        url.query_pairs_mut()
            .append_pair("labelIds", "SENT")
            .append_pair("q", &format!("rfc822msgid:{id}"))
            .append_pair("maxResults", "1");
        let list: MessageList = get(http, token, url, deadline).await?;
        Ok(!list.messages.is_empty())
    }
    .await;
    result.map_err(Into::into)
}

async fn get<T: serde::de::DeserializeOwned>(
    http: &HttpClient,
    token: &SecretString,
    url: Url,
    deadline: Instant,
) -> Result<T, ApiError> {
    let response = http::send(http.get(url).bearer_auth(token.expose_secret()), deadline).await?;
    http::json(response).await
}

fn endpoint(path: &str) -> Result<Url, ApiError> {
    Url::parse(&format!("{BASE}{path}"))
        .map_err(|error| ApiError::InvalidResponse(error.to_string()))
}

fn check_id(id: &str) -> Result<(), ApiError> {
    let valid = (1..=256).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(ApiError::InvalidResponse(format!(
            "`{id}` is not a Gmail message id"
        )))
    }
}

fn unique_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Result<Vec<String>, ApiError> {
    let mut unique: Vec<String> = Vec::new();
    for id in ids {
        check_id(id)?;
        if !unique.iter().any(|seen| seen == id) {
            unique.push(id.to_owned());
        }
    }
    Ok(unique)
}

fn internal_date(milliseconds: Option<&str>) -> Option<Timestamp> {
    Timestamp::from_millisecond(milliseconds?.parse().ok()?).ok()
}

#[cfg(test)]
mod tests;
