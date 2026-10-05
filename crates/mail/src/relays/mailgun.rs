//! Mailgun's Events API (<https://documentation.mailgun.com/docs/mailgun/user-manual/events/>,
//! read 2026-10-02), with the account's private API key over HTTP Basic (`api:<key>`), on
//! `https://api.mailgun.net`, or `https://api.eu.mailgun.net` for a domain in Mailgun's EU
//! region, whose SMTP host is `smtp.eu.mailgun.org` ([`api_base`]).
//!
//! `GET /v3/<domain>/events` ([`events`]) pages through a domain's events in a time range, oldest
//! first (`ascending=yes`, at most 300 a page), following each page's `paging.next`, which must
//! stay on the same API and domain, until a page comes back empty. Mailgun stores events by
//! several routes, so the newest ones can still be missing from a page that was already read; its
//! guidance is to trust only events older than about half an hour, which the caller's range does.
//! Mailgun keeps events at least a day and up to 30 days, by plan.
//! The daily credential check reads at most one event ([`check_key`]); it proves access to the
//! same domain and API permission that reconciliation needs, without requesting broader access.
//!
//! Each event becomes a receipt in the shape the webhook route stores (`{"event-data": …}`,
//! keyed by the event's `id`), so a reconciled event and its webhook delivery share one replay
//! key and [`crate::webhooks::mailgun::events`] reads both. Only the kinds that parser records are
//! kept (accepted, delivered, failed, rejected, complained, unsubscribed): opens, clicks and
//! storage events would only take room.

use jiff::Timestamp;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;
use url::Url;

use crate::http::{self, ApiError, HttpClient};
use crate::webhooks::Receipt;

/// Events a page holds at most.
const PAGE: u32 = 300;
/// The event kinds the webhook parser records.
const RECORDED: [&str; 6] = [
    "accepted",
    "delivered",
    "failed",
    "rejected",
    "complained",
    "unsubscribed",
];

/// What one read of a range found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Events {
    /// The recorded kinds' events, as receipts, oldest first.
    pub receipts: Vec<Receipt>,
    /// The range was read to its end; `false` when the page bound stopped the read first.
    pub complete: bool,
}

/// The API origin of a Mailgun domain, from the SMTP host its connection submits to: the EU
/// region's for `smtp.eu.mailgun.org`, the US one's otherwise.
#[must_use]
pub fn api_base(smtp_host: &str) -> &'static str {
    if smtp_host
        .trim()
        .trim_end_matches('.')
        .eq_ignore_ascii_case("smtp.eu.mailgun.org")
    {
        "https://api.eu.mailgun.net"
    } else {
        "https://api.mailgun.net"
    }
}

/// The sending domain of a Mailgun SMTP login (`postmaster@mg.example.com` gives
/// `mg.example.com`): Mailgun's SMTP credentials belong to one domain, the one its events are
/// read for.
#[must_use]
pub fn domain_of(login: &str) -> Option<String> {
    let (_, domain) = login.trim().rsplit_once('@')?;
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    valid_domain(&domain).then_some(domain)
}

/// Checks that `key` can read events for the SMTP login's domain in its region. Reads at most
/// one event, discards it, and follows no pagination or redirects. The API key is independent
/// of the SMTP password, so a refusal must not revoke a working SMTP credential.
///
/// # Errors
///
/// The login names no domain, the API key lacks access, or the bounded request fails.
pub async fn check_key(
    http: &HttpClient,
    key: &SecretString,
    smtp_host: &str,
    login: &str,
    deadline: Instant,
) -> Result<(), ApiError> {
    let domain = domain_of(login).ok_or_else(|| {
        ApiError::InvalidResponse("the Mailgun SMTP login names no domain".to_owned())
    })?;
    let url = Url::parse(&format!(
        "{}/v3/{domain}/events?limit=1",
        api_base(smtp_host)
    ))
    .map_err(|error| ApiError::InvalidResponse(error.to_string()))?;
    let response = http::send(
        http.get(url).basic_auth("api", Some(key.expose_secret())),
        deadline,
    )
    .await?;
    let _: Value = http::json(response).await?;
    Ok(())
}

/// The recorded events of `domain` from `begin` to `end`, read from `base` ([`api_base`]) with
/// the private API key `key`, at most `pages` pages (see the module).
///
/// # Errors
///
/// A call failed ([`ApiError`]: [`ApiError::Unauthorized`] for a refused key), a page is not an
/// events page, or a next page points elsewhere than this domain's events.
pub async fn events(
    http: &HttpClient,
    key: &SecretString,
    base: &str,
    domain: &str,
    (begin, end): (Timestamp, Timestamp),
    pages: usize,
    deadline: Instant,
) -> Result<Events, ApiError> {
    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        items: Vec<Value>,
        paging: Option<Paging>,
    }
    #[derive(Deserialize)]
    struct Paging {
        next: Option<String>,
    }
    if !valid_domain(domain) {
        return Err(ApiError::InvalidResponse(format!(
            "`{domain}` is not a Mailgun domain"
        )));
    }
    let prefix = format!("/v3/{domain}/events");
    let mut url = Url::parse(&format!("{base}{prefix}"))
        .map_err(|error| ApiError::InvalidResponse(error.to_string()))?;
    url.query_pairs_mut()
        .append_pair("begin", &begin.as_second().to_string())
        .append_pair("end", &end.as_second().to_string())
        .append_pair("ascending", "yes")
        .append_pair("limit", &PAGE.to_string());
    let origin = url.origin();
    let mut receipts = Vec::new();
    for _ in 0..pages {
        let response = http::send(
            http.get(url.clone())
                .basic_auth("api", Some(key.expose_secret())),
            deadline,
        )
        .await?;
        let page: Page = http::json(response).await?;
        if page.items.is_empty() {
            return Ok(Events {
                receipts,
                complete: true,
            });
        }
        receipts.extend(page.items.into_iter().filter_map(receipt));
        let Some(next) = page.paging.and_then(|paging| paging.next) else {
            return Ok(Events {
                receipts,
                complete: true,
            });
        };
        url = Url::parse(&next).map_err(|error| ApiError::InvalidResponse(error.to_string()))?;
        if url.origin() != origin || !url.path().starts_with(&prefix) {
            return Err(ApiError::InvalidResponse(
                "Mailgun's next page is not on this domain's events".to_owned(),
            ));
        }
    }
    Ok(Events {
        receipts,
        complete: false,
    })
}

/// The receipt of one event, as the webhook route stores it; `None` for a kind the parser does
/// not record or an event without a usable id.
fn receipt(event: Value) -> Option<Receipt> {
    let kind = event.get("event").and_then(Value::as_str)?;
    if !RECORDED.contains(&kind) {
        return None;
    }
    let event_id = event
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| {
            (1..=256).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_graphic())
        })?
        .to_owned();
    let raw = serde_json::to_vec(&json!({ "event-data": event })).ok()?;
    Some(Receipt { event_id, raw })
}

/// A domain as a host name: it becomes part of the path.
fn valid_domain(domain: &str) -> bool {
    (1..=253).contains(&domain.len())
        && domain.contains('.')
        && domain
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}
