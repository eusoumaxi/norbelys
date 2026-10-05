//! The HTTP client every provider call goes through (Gmail, Microsoft Graph, the OAuth token
//! endpoints, Amazon SNS certificates), and the errors of the calls that are not submissions.
//!
//! Invariants:
//! - Automatic retries are off. Retries happen in exactly one place, the caller's delivery
//!   queue; a client that also retried would multiply the calls a provider sees (three layers
//!   of three attempts are 27 calls) and could resend a submission whose first attempt was
//!   accepted. Microsoft's Graph SDKs retry by default, which is why this client is built here.
//! - Redirects are off, so a bearer token never follows a `Location` to another host; proxies
//!   are off; only `https` is spoken.
//! - A connect timeout is set apart from each request's deadline, so a failure to connect (the
//!   request was never sent) is told apart from a lost reply (the request may have been
//!   processed).
//! - Every body read is bounded.

use std::time::Duration;

use jiff::Timestamp;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use tokio::time::Instant;
use url::Url;

use crate::submission::{Cause, Failure, Phase, Rejection, Scope};

/// How long establishing a connection may take; past it nothing was sent.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest one request to a provider's API may take; a submission is one request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Common HTTP submission outcomes. Providers pass only their documented exceptions; the
/// uncertain 5xx rule and refusal record stay identical across Gmail and Graph.
pub(crate) fn rejection(
    status: StatusCode,
    reason: &str,
    message: &str,
    retry_after: Option<Timestamp>,
    exception: Option<(Failure, Scope, Cause)>,
) -> Rejection {
    let (failure, scope, cause) = exception.unwrap_or_else(|| match status.as_u16() {
        401 => (Failure::Transient, Scope::Connection, Cause::Unauthorized),
        429 => (Failure::Transient, Scope::Connection, Cause::Throttled),
        403 | 404 => (Failure::Transient, Scope::Connection, Cause::Forbidden),
        500..=599 => (Failure::Uncertain, Scope::Connection, Cause::Refused),
        _ => (Failure::Permanent, Scope::Message, Cause::Refused),
    });
    Rejection {
        failure,
        phase: Phase::Api,
        scope,
        cause,
        code: Some(status.as_u16()),
        status: None,
        retry_after: retry_after.filter(|_| cause == Cause::Throttled),
        diagnostic: crate::text::bounded(
            &format!("{} {reason}: {message}", status.as_u16()),
            crate::text::DIAGNOSTIC_CHARS,
        ),
        refused: Vec::new(),
    }
}
/// The largest JSON body read from a provider (a page, a profile, a token response).
pub(crate) const JSON_LIMIT: usize = 4 * 1024 * 1024;
/// The largest error body read to learn the provider's reason.
pub(crate) const ERROR_LIMIT: usize = 64 * 1024;

/// Sends one submission within its remaining budget, without retrying. Only a connection error
/// proves that nothing was sent; any other transport failure leaves the submission uncertain.
pub(crate) async fn submit(
    request: RequestBuilder,
    deadline: Instant,
) -> Result<Response, Rejection> {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .min(REQUEST_TIMEOUT);
    let rejected = |failure, cause, detail: &str| {
        Rejection::local(failure, Phase::Api, Scope::Connection, cause, detail)
    };
    if remaining.is_zero() {
        return Err(rejected(
            Failure::Transient,
            Cause::Deadline,
            "the submission deadline passed before the request",
        ));
    }
    request.timeout(remaining).send().await.map_err(|error| {
        let failure = if error.is_connect() {
            Failure::Transient
        } else {
            Failure::Uncertain
        };
        rejected(failure, Cause::NoReply, &error.without_url().to_string())
    })
}

/// The process's client for provider APIs: no automatic retries, no redirects, no proxy,
/// `https` only. Cheap to clone; typically one shared client per process.
#[derive(Clone, Debug)]
pub struct HttpClient {
    client: reqwest::Client,
    /// Where every request is sent instead of the provider's origin: `None` in production; a
    /// fake provider on the loopback interface in this crate's tests.
    rebase: Option<Url>,
}

/// Why the client cannot be built.
#[derive(Debug, thiserror::Error)]
#[error("the HTTP client could not be built: {0}")]
pub struct ClientError(#[from] reqwest::Error);

impl HttpClient {
    /// A client with this module's policy.
    ///
    /// # Errors
    ///
    /// The TLS backend could not be initialised.
    pub fn new() -> Result<Self, ClientError> {
        let client = reqwest::Client::builder()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .https_only(true)
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(60))
            .user_agent("norbelys")
            .build()?;
        Ok(Self {
            client,
            rebase: None,
        })
    }

    /// A client whose every request goes to `origin` (a fake provider speaking plain HTTP on the
    /// loopback interface) with the same policy otherwise: no retries, no redirects. Compiled for
    /// this crate's tests and, through the `test-support` feature, for the tests of its callers.
    ///
    /// # Errors
    ///
    /// The TLS backend could not be initialised.
    #[cfg(any(test, feature = "test-support"))]
    pub fn rebased(origin: &str) -> Result<Self, ClientError> {
        let client = reqwest::Client::builder()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?;
        Ok(Self {
            client,
            rebase: Url::parse(origin).ok(),
        })
    }

    /// A `GET` of `url`.
    pub(crate) fn get(&self, url: Url) -> RequestBuilder {
        self.client.get(self.rebase(url))
    }

    /// A `POST` to `url`.
    pub(crate) fn post(&self, url: Url) -> RequestBuilder {
        self.client.post(self.rebase(url))
    }

    /// `url` as it is actually requested: unchanged in production, moved to the test origin in
    /// this crate's tests (scheme, host and port; the path and query stay).
    pub(crate) fn rebase(&self, mut url: Url) -> Url {
        if let Some(origin) = &self.rebase {
            let _scheme = url.set_scheme(origin.scheme());
            let _host = url.set_host(origin.host_str());
            let _port = url.set_port(origin.port());
        }
        url
    }
}

/// Why a provider call that is not a submission failed: a read, an identity check, a
/// reconciliation search.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// `401`: the access token was refused.
    #[error("the provider refused the access token")]
    Unauthorized,
    /// `403` naming the account's permission or policy (Gmail `domainPolicy`,
    /// `insufficientPermissions`; Graph `ErrorAccessDenied`).
    #[error("the provider refused the account: {reason}")]
    Forbidden {
        /// The provider's reason or error code.
        reason: String,
    },
    /// `429`, or a `403` whose reason is a rate limit: wait, then call again.
    #[error("the provider throttled the call ({reason})")]
    Throttled {
        /// The provider's wait as an absolute instant, when it gave a usable one.
        retry_after: Option<Timestamp>,
        /// Whose limit it is: the connection's or Norbelys's own project or app.
        platform: bool,
        /// The provider's reason or error code.
        reason: String,
    },
    /// Any other error status.
    #[error("the provider answered {status} ({reason})")]
    Status {
        /// The HTTP status.
        status: u16,
        /// The provider's reason or error code.
        reason: String,
    },
    /// No response: the connection failed or was reset.
    #[error("no response from the provider: {0}")]
    Network(String),
    /// No response before the deadline.
    #[error("no response from the provider before the deadline")]
    Timeout,
    /// The response was larger than this call reads.
    #[error("the provider's response exceeds {0} bytes")]
    TooLarge(usize),
    /// The response could not be understood.
    #[error("the provider's response is not understood: {0}")]
    InvalidResponse(String),
}

/// The provider's reason in an error body: Google's `error.errors[0].reason` (else
/// `error.status`), Graph's `error.code`, an OAuth `error`.
pub(crate) fn error_reason(body: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::new();
    };
    let error = value.get("error");
    let reason = error
        .and_then(|error| error.pointer("/errors/0/reason"))
        .or_else(|| {
            error
                .and_then(|error| error.get("code"))
                .filter(|code| code.is_string())
        })
        .or_else(|| error.and_then(|error| error.get("status")))
        .or_else(|| error.filter(|error| error.is_string()));
    reason
        .and_then(serde_json::Value::as_str)
        .map(|reason| crate::text::bounded(reason, 200))
        .unwrap_or_default()
}

/// The provider's error message (Google's and Graph's `error.message`), bounded.
pub(crate) fn error_message(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .map(|message| crate::text::bounded(&message, crate::text::DIAGNOSTIC_CHARS))
        .unwrap_or_default()
}

/// `Retry-After` (RFC 9110 §10.2.3: delay-seconds or an HTTP date) as an absolute instant; a
/// zero, past or malformed value is `None`, so the caller falls back to its own backoff.
pub(crate) fn retry_after(headers: &HeaderMap, now: Timestamp) -> Option<Timestamp> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let at = if value.bytes().all(|byte| byte.is_ascii_digit()) {
        let seconds: i64 = value.parse().ok()?;
        now.checked_add(jiff::SignedDuration::from_secs(seconds))
            .ok()?
    } else {
        jiff::fmt::rfc2822::DateTimeParser::new()
            .parse_timestamp(value)
            .ok()?
    };
    (at > now).then_some(at)
}

/// Sends `request` within `deadline` and the request deadline.
pub(crate) async fn send(request: RequestBuilder, deadline: Instant) -> Result<Response, ApiError> {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .min(REQUEST_TIMEOUT);
    if remaining.is_zero() {
        return Err(ApiError::Timeout);
    }
    request.timeout(remaining).send().await.map_err(|error| {
        if error.is_timeout() {
            ApiError::Timeout
        } else {
            ApiError::Network(error.without_url().to_string())
        }
    })
}

/// The body, at most `limit` bytes.
pub(crate) async fn read_body(mut response: Response, limit: usize) -> Result<Vec<u8>, ApiError> {
    let declared_too_long = response
        .content_length()
        .is_some_and(|length| u64::try_from(limit).is_ok_and(|limit| length > limit));
    if declared_too_long {
        return Err(ApiError::TooLarge(limit));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ApiError::Network(error.without_url().to_string()))?
    {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(ApiError::TooLarge(limit));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The first `limit` bytes of the body, whether it was cut, and its declared length.
pub(crate) async fn read_prefix(
    mut response: Response,
    limit: usize,
) -> Result<(Vec<u8>, bool, Option<u64>), ApiError> {
    let declared = response.content_length();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ApiError::Network(error.without_url().to_string()))?
    {
        let room = limit.saturating_sub(body.len());
        if chunk.len() > room {
            body.extend_from_slice(chunk.get(..room).unwrap_or_default());
            return Ok((body, true, declared));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false, declared))
}

/// A successful JSON response, or the error its status means.
pub(crate) async fn json<T: DeserializeOwned>(response: Response) -> Result<T, ApiError> {
    if !response.status().is_success() {
        return Err(status_error(response).await);
    }
    let body = read_body(response, JSON_LIMIT).await?;
    serde_json::from_slice(&body).map_err(|error| ApiError::InvalidResponse(error.to_string()))
}

/// The [`ApiError`] of a non-success response.
pub(crate) async fn status_error(response: Response) -> ApiError {
    let status = response.status();
    let retry_after = retry_after(response.headers(), Timestamp::now());
    let body = read_body(response, ERROR_LIMIT).await.unwrap_or_default();
    let reason = error_reason(&body);
    let project = names_project(&error_message(&body));
    match status {
        StatusCode::UNAUTHORIZED => ApiError::Unauthorized,
        StatusCode::TOO_MANY_REQUESTS => ApiError::Throttled {
            retry_after,
            platform: project,
            reason,
        },
        StatusCode::FORBIDDEN if is_rate_reason(&reason) => ApiError::Throttled {
            retry_after,
            platform: project || reason == "dailyLimitExceeded",
            reason,
        },
        StatusCode::FORBIDDEN => ApiError::Forbidden { reason },
        status => ApiError::Status {
            status: status.as_u16(),
            reason,
        },
    }
}

/// Whether a Google error message names the project's quota rather than one user's limit:
/// Google words a project limit as "Quota exceeded for quota metric … for consumer
/// 'project_number:…'", while per-user limits read "User-rate limit exceeded".
pub(crate) fn names_project(message: &str) -> bool {
    message.contains("for consumer 'project_number:")
        || message.contains("Quota exceeded for quota metric")
}

/// Google's rate-limit reasons (Gmail API, handling errors): a throttle, not a refusal.
pub(crate) fn is_rate_reason(reason: &str) -> bool {
    matches!(
        reason,
        "rateLimitExceeded" | "userRateLimitExceeded" | "dailyLimitExceeded"
    )
}

#[cfg(test)]
mod tests {
    use jiff::{SignedDuration, Timestamp};
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    use super::{error_reason, names_project, retry_after};

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_str(value).expect("a header value"),
        );
        headers
    }

    /// `Retry-After` becomes an absolute instant from either of its RFC 9110 forms, and a zero,
    /// past or malformed value counts as absent, so the caller falls back to its own backoff
    /// instead of retrying at once.
    #[test]
    fn retry_after_is_an_absolute_future_instant_or_nothing() {
        let now: Timestamp = "2026-10-01T12:00:00Z".parse().expect("a timestamp");
        assert_eq!(
            retry_after(&headers("120"), now),
            now.checked_add(SignedDuration::from_secs(120)).ok()
        );
        assert_eq!(
            retry_after(&headers("Thu, 01 Oct 2026 12:05:00 GMT"), now),
            "2026-10-01T12:05:00Z".parse().ok()
        );
        assert_eq!(retry_after(&headers("0"), now), None);
        assert_eq!(
            retry_after(&headers("Thu, 01 Oct 2026 11:00:00 GMT"), now),
            None
        );
        assert_eq!(retry_after(&headers("soon"), now), None);
        assert_eq!(retry_after(&HeaderMap::new(), now), None);
    }

    /// The provider's reason is read from each error shape the crate meets: Google's
    /// `error.errors[0].reason`, Graph's `error.code`, an OAuth `error` string; Google's
    /// project-quota wording is told apart from a per-user limit.
    #[test]
    fn error_reasons_come_from_each_provider_shape() {
        let google = br#"{"error":{"code":403,"errors":[{"reason":"domainPolicy"}],"status":"PERMISSION_DENIED"}}"#;
        let graph = br#"{"error":{"code":"ErrorAccessDenied","message":"Access is denied."}}"#;
        let oauth = br#"{"error":"invalid_grant","error_description":"Bad Request"}"#;
        assert_eq!(error_reason(google), "domainPolicy");
        assert_eq!(error_reason(graph), "ErrorAccessDenied");
        assert_eq!(error_reason(oauth), "invalid_grant");
        assert_eq!(error_reason(b"not json"), "");
        assert!(names_project(
            "Quota exceeded for quota metric 'Queries' and limit 'Queries per minute' of service 'gmail.googleapis.com' for consumer 'project_number:123'."
        ));
        assert!(!names_project(
            "User-rate limit exceeded.  Retry after 2026-10-01T12:00:00.000Z"
        ));
    }
}
