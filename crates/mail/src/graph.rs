//! Microsoft Graph, for mailboxes connected through Microsoft OAuth: submission with
//! `POST /me/sendMail` (MIME), reading a folder through delta queries, a message's MIME
//! (`$value`), the Sent Items search that settles an `uncertain` submission, and the identity
//! read of a connection check (`GET /me`). Reference:
//! <https://learn.microsoft.com/en-us/graph/api/resources/mail-api-overview>.
//!
//! Submission:
//! - One request per submission, ending by the caller's deadline (at most
//!   [`crate::http::REQUEST_TIMEOUT`]). `202 Accepted` is `accepted`: Graph returns no id and no
//!   proof of delivery.
//! - A request that may have reached Microsoft without its reply (a timeout or a reset after the
//!   connection was made, a `5xx`) is `uncertain`; a failure to connect is `transient`.
//! - `429` is a throttle of the mailbox (Outlook allows 10,000 requests per 10 minutes and 4
//!   concurrent requests per app and mailbox), with its `Retry-After`. Throttled requests still
//!   count against the limits and Microsoft's SDKs retry on their own, which is why this module
//!   never retries. See <https://learn.microsoft.com/en-us/graph/throttling-limits>.
//! - `401` is an unauthorized credential; `ErrorSendAsDenied` refuses the message's From
//!   address; other `403`s and `404`s refuse the account (missing permission, mailbox not
//!   enabled for the API); other `4xx` refuse the message.
//!
//! Reading:
//! - The cursor is the delta link (`{delta_link}`), or the next link while a round of pages is
//!   still being read. Ids are requested as immutable ids (`Prefer: IdType="ImmutableId"`), so a
//!   message keeps its id when it moves between folders and a resync deduplicates on it.
//! - A delta token Graph no longer knows (`410 Gone`, `syncStateNotFound`) restarts at the
//!   caller's `since` with an unfiltered delta round; dates are filtered locally because
//!   Graph caps a server-side date filter at 5,000 messages, even when links are paginated.
//! - Every link Graph returns is checked to stay on `https://graph.microsoft.com` under the
//!   signed-in user's mail folders, so the bearer token is never sent anywhere else.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use reqwest::StatusCode;
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use url::Url;

use crate::http::{self, ApiError, HttpClient};
use crate::receive::{Page, RawMessage, Reset, ResetReason, TransportIdentity};
use crate::submission::{Cause, Failure, Phase, Rejection, Scope, Submission};

const ORIGIN: &str = "https://graph.microsoft.com";
const BASE: &str = "https://graph.microsoft.com/v1.0/me/";
const IMMUTABLE_IDS: &str = "IdType=\"ImmutableId\"";

/// The receive cursor of a Graph binding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphCursor {
    /// The link to call next: a delta link, or a next link while a round is being read.
    pub delta_link: String,
    /// Client-side date filter for an unfinished full delta round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resync_since: Option<Timestamp>,
    /// Listed ids waiting for their bounded handoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<crate::receive::Pending>,
}

/// The signed-in user, as `GET /me` names it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Me {
    /// The user's object id in the tenant.
    pub id: String,
    /// The sign-in name: the login SMTP AUTH uses for the same mailbox, and the address a
    /// Microsoft connection proves. Needs the `User.Read` permission.
    pub user_principal_name: String,
    /// The primary SMTP address, when the user has a mailbox.
    pub mail: Option<String>,
    /// The display name.
    pub display_name: Option<String>,
}

/// Submits `mime` with `POST /me/sendMail` (the MIME, base64-encoded, as `text/plain`). The
/// message's `Bcc` header names the blind recipients; Graph removes it. Graph saves the message
/// to Sent Items.
///
/// # Errors
///
/// A [`Rejection`] in the `api` phase, by the mapping of this module's documentation.
pub async fn send_mail(
    http: &HttpClient,
    token: &SecretString,
    mime: &[u8],
    deadline: Instant,
) -> Result<Submission, Rejection> {
    let url = parse(&format!("{BASE}sendMail"))
        .map_err(|error| local(Failure::Transient, Cause::NoReply, &error.to_string()))?;
    let request = http
        .post(url)
        .bearer_auth(token.expose_secret())
        .header(reqwest::header::CONTENT_TYPE, "text/plain")
        .body(STANDARD.encode(mime));
    let response = http::submit(request, deadline).await?;
    let status = response.status();
    if status == StatusCode::ACCEPTED {
        return Ok(Submission::default());
    }
    let retry_after = http::retry_after(response.headers(), Timestamp::now());
    let body = http::read_body(response, http::ERROR_LIMIT)
        .await
        .unwrap_or_default();
    let reason = http::error_reason(&body);
    if status.is_success() {
        return Ok(Submission::default());
    }
    let exception = (status == StatusCode::FORBIDDEN && reason == "ErrorSendAsDenied").then_some((
        Failure::Permanent,
        Scope::Message,
        Cause::Refused,
    ));
    Err(http::rejection(
        status,
        &reason,
        &http::error_message(&body),
        retry_after,
        exception,
    ))
}

fn local(failure: Failure, cause: Cause, detail: &str) -> Rejection {
    Rejection::local(failure, Phase::Api, Scope::Connection, cause, detail)
}

/// The signed-in user (`GET /me`): the proof of a Microsoft connection's address.
///
/// # Errors
///
/// The call failed ([`ApiError`]).
pub async fn me(
    http: &HttpClient,
    token: &SecretString,
    deadline: Instant,
) -> Result<Me, ApiError> {
    let mut url = parse(&format!("{ORIGIN}/v1.0/me"))?;
    url.query_pairs_mut()
        .append_pair("$select", "id,userPrincipalName,mail,displayName");
    let response = http::send(http.get(url).bearer_auth(token.expose_secret()), deadline).await?;
    http::json(response).await
}

/// The next page of messages created in `folder` (a folder id or a well-known name; `INBOX` is
/// read as `inbox`), at most `limit` ids. Without a cursor, or when Graph no longer knows the
/// cursor's delta token, a new delta round starts with the messages received at or after
/// `since`; after a lost cursor the page carries a [`Reset`].
///
/// # Errors
///
/// A call failed ([`crate::receive::Error`]), or Graph returned a link outside the user's mail folders.
pub async fn changes(
    http: &HttpClient,
    token: &SecretString,
    folder: &str,
    cursor: Option<&GraphCursor>,
    since: Timestamp,
    limit: u32,
    deadline: Instant,
) -> Result<Page<String, GraphCursor>, crate::receive::Error> {
    let result: Result<Page<String, GraphCursor>, ApiError> = async {
        let limit = limit.clamp(1, 1_000);
        if let Some((cursor, pending)) =
            cursor.and_then(|cursor| cursor.pending.as_ref().map(|pending| (cursor, pending)))
        {
            checked_link(http, &cursor.delta_link)?;
            if pending.ids.len() > crate::receive::Pending::MAX {
                return Err(ApiError::InvalidResponse(
                    "pending Graph page exceeds its bound".into(),
                ));
            }
            for id in &pending.ids {
                check_id(id)?;
            }
            let (ids, more, pending) = pending.page(limit);
            return Ok(Page {
                ids,
                more,
                cursor: GraphCursor {
                    pending,
                    ..cursor.clone()
                },
                reset: None,
            });
        }
        let reason = match cursor {
            None => None,
            Some(cursor) => {
                let link = checked_link(http, &cursor.delta_link)?;
                match delta(http, token, link, limit, deadline, cursor.resync_since).await {
                    Err(ApiError::Status { status, reason }) if expired(status, &reason) => {
                        Some(ResetReason::Delta)
                    }
                    result => return result,
                }
            }
        };
        let mut url = parse(BASE)?;
        let folder = if folder.eq_ignore_ascii_case("INBOX") {
            "inbox"
        } else {
            folder
        };
        url.path_segments_mut()
            .map_err(|()| ApiError::InvalidResponse("the Graph base is not a path".to_owned()))?
            .pop_if_empty()
            .extend(["mailFolders", folder, "messages", "delta"]);
        // Graph caps date-filtered delta rounds at 5,000 messages, across all pages.
        // An unfiltered round with a local date filter retains complete next/delta links.
        url.query_pairs_mut()
            .append_pair("$select", "id,receivedDateTime");
        let mut page = delta(http, token, url, limit, deadline, Some(since)).await?;
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

/// Whether a delta error means the token is gone: `410`, or Graph's sync-state codes.
fn expired(status: u16, reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    status == 410
        || reason.contains("syncstatenotfound")
        || reason.contains("resyncrequired")
        || reason.contains("invaliddeltatoken")
}

async fn delta(
    http: &HttpClient,
    token: &SecretString,
    url: Url,
    limit: u32,
    deadline: Instant,
    resync_since: Option<Timestamp>,
) -> Result<Page<String, GraphCursor>, ApiError> {
    #[derive(Deserialize)]
    struct Delta {
        #[serde(default)]
        value: Vec<Item>,
        #[serde(rename = "@odata.nextLink")]
        next_link: Option<String>,
        #[serde(rename = "@odata.deltaLink")]
        delta_link: Option<String>,
    }
    #[derive(Deserialize)]
    struct Item {
        id: String,
        #[serde(default, rename = "receivedDateTime")]
        received: Option<Timestamp>,
        #[serde(rename = "@removed")]
        removed: Option<serde_json::Value>,
    }
    let request = http.get(url).bearer_auth(token.expose_secret()).header(
        "Prefer",
        format!("odata.maxpagesize={limit}, {IMMUTABLE_IDS}"),
    );
    let page: Delta = http::json(http::send(request, deadline).await?).await?;
    let mut ids: Vec<String> = Vec::new();
    for item in page.value.into_iter().filter(|item| {
        item.removed.is_none()
            && resync_since
                .zip(item.received)
                .is_none_or(|(since, received)| received >= since)
    }) {
        check_id(&item.id)?;
        if !ids.contains(&item.id) {
            ids.push(item.id);
        }
    }
    let (link, more) = match (page.next_link, page.delta_link) {
        (Some(next), _) => (next, true),
        (None, Some(delta)) => (delta, false),
        (None, None) => {
            return Err(ApiError::InvalidResponse(
                "a delta page without a next or delta link".to_owned(),
            ));
        }
    };
    checked_link(http, &link)?;
    if ids.len() > crate::receive::Pending::MAX {
        return Err(ApiError::InvalidResponse(
            "Graph page exceeds its retained bound".into(),
        ));
    }
    let (ids, page_more, pending) = crate::receive::Pending::split(ids, limit, more);
    Ok(Page {
        ids,
        cursor: GraphCursor {
            delta_link: link,
            resync_since: if more { resync_since } else { None },
            pending,
        },
        more: page_more,
        reset: None,
    })
}

/// A link Graph returned, accepted only on Graph's origin (`https://graph.microsoft.com`) under the
/// signed-in user's mail folders, so the bearer token never follows a link elsewhere.
fn checked_link(http: &HttpClient, link: &str) -> Result<Url, ApiError> {
    let invalid =
        || ApiError::InvalidResponse("a delta link outside the user's mail folders".to_owned());
    if link.len() > 16 * 1024 {
        return Err(invalid());
    }
    let url = Url::parse(link).map_err(|_| invalid())?;
    let graph = http.rebase(parse(ORIGIN)?);
    let allowed = url.origin() == graph.origin()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.path().starts_with("/v1.0/me/mailFolders")
        && url.path().ends_with("/messages/delta");
    if allowed { Ok(url) } else { Err(invalid()) }
}

/// One message's MIME (`GET /me/messages/{id}/$value`), at most `max_bytes`, with its
/// `receivedDateTime` read first.
///
/// # Errors
///
/// A call failed ([`crate::receive::Error`]), or the id is not a Graph id.
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
        struct Metadata {
            received_date_time: Option<String>,
        }
        check_id(id)?;
        let mut metadata_url = message_url(id)?;
        metadata_url
            .query_pairs_mut()
            .append_pair("$select", "receivedDateTime");
        let request = http
            .get(metadata_url)
            .bearer_auth(token.expose_secret())
            .header("Prefer", IMMUTABLE_IDS);
        let metadata: Metadata = http::json(http::send(request, deadline).await?).await?;

        let mut value_url = message_url(id)?;
        value_url
            .path_segments_mut()
            .map_err(|()| ApiError::InvalidResponse("the Graph base is not a path".to_owned()))?
            .push("$value");
        let request = http
            .get(value_url)
            .bearer_auth(token.expose_secret())
            .header("Prefer", IMMUTABLE_IDS);
        let response = http::send(request, deadline).await?;
        if !response.status().is_success() {
            return Err(http::status_error(response).await);
        }
        let (raw, truncated, declared) = http::read_prefix(response, max_bytes).await?;
        Ok(RawMessage {
            identity: TransportIdentity::Provider {
                provider_message_id: id.to_owned(),
            },
            raw,
            size: declared,
            truncated,
            received_at: metadata.received_date_time.and_then(|at| at.parse().ok()),
        })
    }
    .await;
    result.map_err(Into::into)
}

/// Whether Sent Items holds a message with this `Message-ID` (`internetMessageId eq '…'`): the
/// read-only way to settle an `uncertain` submission. Graph saves what `sendMail` accepted to
/// Sent Items with the `Message-ID` the message carried; finding nothing proves nothing.
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
        #[derive(Deserialize)]
        struct List {
            #[serde(default)]
            value: Vec<serde_json::Value>,
        }
        let bare = internet_message_id
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>');
        if bare.is_empty() || !bare.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(ApiError::InvalidResponse(
                "a Message-ID is printable ASCII".to_owned(),
            ));
        }
        let literal = format!("<{bare}>").replace('\'', "''");
        let mut url = parse(&format!("{BASE}mailFolders/sentitems/messages"))?;
        url.query_pairs_mut()
            .append_pair("$filter", &format!("internetMessageId eq '{literal}'"))
            .append_pair("$select", "id")
            .append_pair("$top", "1");
        let response =
            http::send(http.get(url).bearer_auth(token.expose_secret()), deadline).await?;
        let list: List = http::json(response).await?;
        Ok(!list.value.is_empty())
    }
    .await;
    result.map_err(Into::into)
}

fn message_url(id: &str) -> Result<Url, ApiError> {
    let mut url = parse(BASE)?;
    url.path_segments_mut()
        .map_err(|()| ApiError::InvalidResponse("the Graph base is not a path".to_owned()))?
        .pop_if_empty()
        .extend(["messages", id]);
    Ok(url)
}

fn parse(url: &str) -> Result<Url, ApiError> {
    Url::parse(url).map_err(|error| ApiError::InvalidResponse(error.to_string()))
}

fn check_id(id: &str) -> Result<(), ApiError> {
    let valid = (1..=1024).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'/');
    if valid {
        Ok(())
    } else {
        Err(ApiError::InvalidResponse(
            "not a Graph message id".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests;
