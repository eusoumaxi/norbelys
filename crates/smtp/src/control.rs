//! The control API: how the core provisions the managed MTA. The core's background jobs
//! (verifying a sending domain, creating a connection's login, checking it) call it over the
//! private network; nothing else may reach it.
//!
//! | Route | Does |
//! |---|---|
//! | `POST /v1/domains`, `GET /v1/domains`, `GET /v1/domains/{name}` | registers a domain with its ownership token; the DNS records to publish |
//! | `POST /v1/domains/{name}/verify` | checks the ownership TXT record and resumes any pending DKIM preparation |
//! | `POST /v1/accounts`, `GET /v1/accounts`, `GET /v1/accounts/{username}` | creates a login, its password answered once |
//! | `PATCH /v1/accounts/{username}` | `grant` (send as any address of its domain) and `catch_all` |
//! | `DELETE /v1/accounts/{username}` | disables the login |
//! | `POST /v1/accounts/{username}/password` | issues a new password, re-enabling a disabled login |
//! | `POST /v1/recipients/check` | checks up to eight recipients against their public MX hosts, without sending a message |
//! | `PUT /v1/routes/{provider_webhook_id}`, `GET /v1/routes`, `DELETE /v1/routes/{provider_webhook_id}` | where each login's evidence is posted, and with which secret |
//! | `GET /health/live`, `GET /health/ready` | unsigned, see [`crate::health`] |
//!
//! Authentication: every `/v1` request is verified per Standard Webhooks
//! (<https://www.standardwebhooks.com/>) with the per-installation secret before a handler sees
//! it: `webhook-id`, `webhook-timestamp` (within five minutes of now) and `webhook-signature`
//! (HMAC-SHA256 over `id.timestamp.body`), with a body of at most 64 KiB. Standard Webhooks
//! control protocol v2 frames the method and exact path/query with that body before signing,
//! and a `webhook-id` is accepted once within the tolerance window. The core
//! signs every request, retries included, with a fresh id. The memory of ids starts empty at
//! each start, which leaves a replay window of at most five minutes after a restart.
//!
//! The service is unprivileged: a change to Postfix, Dovecot or Rspamd is queued in
//! `pending_changes` in the same transaction as the state it follows from, and applied by
//! `provision-apply` ([`crate::provision`]), which a trigger file starts. Errors are RFC 9457
//! problem documents with a stable `code`; internal causes are logged, never answered.
//!
//! The listener speaks plain HTTP and binds to loopback by default. A password travels in the
//! answer to an account creation, so the path between the core and this listener must be
//! encrypted (a WireGuard tunnel or a TLS-terminating proxy) whenever it leaves the host.

pub mod accounts;
pub mod domains;
pub mod recipients;
pub mod routes;

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{MatchedPath, Request, State as Extract};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use hickory_resolver::TokioResolver;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::task::JoinError;

use crate::crypto::{self, CryptoError, Keys};
use crate::db::Db;
use crate::health;
use crate::serve::Shutdown;

/// The largest request body, in bytes.
const MAX_BODY: usize = 64 * 1024;
/// How many request ids the replay memory holds before it refuses new requests.
const MAX_SEEN: usize = 100_000;

/// Settings the handlers read.
#[derive(Debug)]
pub struct Settings {
    /// The MTA's public host name.
    pub mail_host: String,
    /// The MTA's sending address.
    pub public_ipv4: Ipv4Addr,
    /// Stable SPF include, or direct IPv4 publication for an unconfigured self-host.
    pub spf_include: Option<String>,
    /// The DKIM selector of new domains.
    pub dkim_selector: String,
    /// Host names a route may post evidence to.
    pub evidence_hosts: Vec<String>,
    /// The file whose change starts `provision-apply`.
    pub trigger: PathBuf,
}

/// What every handler can reach.
#[derive(Clone)]
pub struct State {
    /// The control API's connection.
    pub db: Db,
    /// The installation's keys.
    pub keys: Arc<Keys>,
    /// Read-only settings.
    pub settings: Arc<Settings>,
    /// Request ids seen within the tolerance window, with their timestamps.
    pub seen: Arc<Mutex<HashMap<String, i64>>>,
    /// The DNS resolver of ownership checks.
    pub resolver: TokioResolver,
    pub recipient_checks: recipients::Checks,
}

/// The router: health unsigned, everything under `/v1` verified.
pub fn router(state: State) -> Router {
    let v1 = Router::new()
        .route("/v1/recipients/check", post(recipients::check))
        .route("/v1/domains", post(domains::create).get(domains::list))
        .route("/v1/domains/{name}", get(domains::retrieve))
        .route("/v1/domains/{name}/verify", post(domains::verify))
        .route("/v1/accounts", post(accounts::create).get(accounts::list))
        .route(
            "/v1/accounts/{username}",
            get(accounts::retrieve)
                .patch(accounts::update)
                .delete(accounts::disable),
        )
        .route(
            "/v1/accounts/{username}/password",
            post(accounts::reset_password),
        )
        .route("/v1/routes", get(routes::list))
        .route(
            "/v1/routes/{id}",
            put(routes::upsert).delete(routes::remove),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), verify));
    Router::new()
        .route("/health/live", get(health::live))
        .route("/health/ready", get(health::ready))
        .route("/metrics", get(health::metrics))
        .merge(v1)
        .with_state(state)
}

/// Serves the control API on `addr` until shutdown.
///
/// # Errors
///
/// The listener cannot bind or the server fails.
pub async fn serve(
    addr: SocketAddr,
    private_transport: bool,
    state: State,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    let private = match addr.ip() {
        std::net::IpAddr::V4(ip) => ip.is_private(),
        std::net::IpAddr::V6(ip) => ip.is_unique_local(),
    };
    anyhow::ensure!(
        addr.ip().is_loopback() || (private_transport && private),
        "control HTTP must bind loopback or an explicitly authorized private tunnel address"
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "control API listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move { shutdown.wait().await })
        .await?;
    Ok(())
}

/// Verifies a request per Standard Webhooks, then emits its canonical event (`mta.request`).
async fn verify(Extract(state): Extract<State>, request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(String::new, |path| path.as_str().to_owned());
    let request_id = uuid::Uuid::now_v7().to_string();
    let span = tracing::info_span!("smtp.control", otel.kind = "server", request_id = %request_id,
        http.request.method = %method, http.route = %route,
        http.response.status_code = tracing::field::Empty, otel.status_code = tracing::field::Empty);
    use tracing::Instrument as _;
    let mut response = async {
        match authenticate(&state, request).await {
            Ok(request) => next.run(request).await,
            Err(problem) => problem.into_response(),
        }
    }
    .instrument(span.clone())
    .await;
    let status = response.status().as_u16();
    span.record("http.response.status_code", status);
    if status >= 500 {
        span.record("otel.status_code", "ERROR");
    }
    if let Ok(value) = request_id.parse() {
        response.headers_mut().insert("x-request-id", value);
    }
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let _entered = span.enter();
    crate::telemetry::unit(crate::telemetry::Event::Request);
    if status < 400 {
        tracing::info!(event = "mta.request", method = %method, route = %route, status, duration_ms, "mta.request");
    } else {
        tracing::warn!(event = "mta.request", method = %method, route = %route, status, duration_ms, "mta.request");
    }
    response
}

/// Checks the signature headers, reads the bounded body, and hands the request on with it.
async fn authenticate(state: &State, request: Request) -> Result<Request, ApiError> {
    let (parts, body) = request.into_parts();
    if header_str(&parts.headers, "norbelys-control-version") != Some("2") {
        return Err(ApiError::Unauthorized(
            "control protocol version 2 is required",
        ));
    }
    let id = header_str(&parts.headers, "webhook-id")
        .filter(|id| (1..=256).contains(&id.len()))
        .ok_or(ApiError::Unauthorized("a webhook-id header is required"))?
        .to_owned();
    let timestamp = header_str(&parts.headers, "webhook-timestamp")
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or(ApiError::Unauthorized(
            "a webhook-timestamp header in Unix seconds is required",
        ))?;
    let signature = header_str(&parts.headers, "webhook-signature")
        .ok_or(ApiError::Unauthorized(
            "a webhook-signature header is required",
        ))?
        .to_owned();
    let now = jiff::Timestamp::now().as_second();
    if now.abs_diff(timestamp) > crypto::TOLERANCE_SECONDS.unsigned_abs() {
        return Err(ApiError::Unauthorized(
            "the webhook-timestamp is outside the five-minute tolerance",
        ));
    }
    let body = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?;
    if !state.keys.control().verify(
        &id,
        timestamp,
        &norbelys_mail::webhooks::control_payload(
            parts.method.as_str(),
            parts.uri.path_and_query().map_or("/", |path| path.as_str()),
            &body,
        ),
        &signature,
    ) {
        return Err(ApiError::Unauthorized(
            "the webhook-signature does not match",
        ));
    }
    remember(&state.seen, id, timestamp, now)?;
    Ok(Request::from_parts(parts, Body::from(body)))
}

/// Records a verified request id; a second use within the tolerance window is refused.
fn remember(
    seen: &Mutex<HashMap<String, i64>>,
    id: String,
    timestamp: i64,
    now: i64,
) -> Result<(), ApiError> {
    let mut seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
    seen.retain(|_, at| now.abs_diff(*at) <= crypto::TOLERANCE_SECONDS.unsigned_abs());
    if seen.contains_key(&id) {
        return Err(ApiError::Conflict(
            "this webhook-id was already used; sign every request with a fresh one".to_owned(),
        ));
    }
    if seen.len() >= MAX_SEEN {
        return Err(ApiError::Unavailable(
            "too many requests within the tolerance window".to_owned(),
        ));
    }
    seen.insert(id, timestamp);
    Ok(())
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Parses a JSON body into `T`; unknown fields are refused by `T`'s own attributes.
///
/// # Errors
///
/// The body is not the JSON `T` expects.
pub fn parse<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|error| ApiError::Invalid(format!("invalid JSON body: {error}")))
}

/// Asks `provision-apply` to run, after a change was committed; a failure is logged and the
/// change waits for the next run (the helper also runs on a timer).
pub fn trigger(state: &State) {
    if let Err(error) = crate::provision::trigger(&state.settings.trigger) {
        tracing::error!(error = %error, path = %state.settings.trigger.display(), "cannot touch the provisioning trigger; the change waits for the next run");
    }
}

/// A list response.
#[derive(Debug, Serialize)]
pub struct List<T> {
    /// The items.
    pub data: Vec<T>,
}

/// True for a lowercase, fully qualified domain name: labels of letters, digits and inner
/// hyphens (at most 63), a final label of 2 to 63 letters, at most 253 characters.
#[must_use]
pub fn is_domain(name: &str) -> bool {
    let labels: Vec<&str> = name.split('.').collect();
    let Some((last, rest)) = labels.split_last() else {
        return false;
    };
    name.len() <= 253
        && !rest.is_empty()
        && (2..=63).contains(&last.len())
        && last.bytes().all(|b| b.is_ascii_lowercase())
        && rest.iter().all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// Splits a lowercase login address into its local part and domain: one `@`, a local part of
/// letters, digits and `._+-` that starts and ends with a letter or digit (at most 64, no
/// `..`), a valid domain, at most 254 characters in all.
#[must_use]
pub fn split_address(address: &str) -> Option<(&str, &str)> {
    let (local, domain) = address.split_once('@')?;
    let edge = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let valid_local = (1..=64).contains(&local.len())
        && local.bytes().next().is_some_and(edge)
        && local.bytes().last().is_some_and(edge)
        && local
            .bytes()
            .all(|b| edge(b) || matches!(b, b'.' | b'_' | b'+' | b'-'))
        && !local.contains("..");
    (address.len() <= 254 && valid_local && is_domain(domain)).then_some((local, domain))
}

/// An error answered to the core.
#[derive(Debug)]
pub enum ApiError {
    /// `401 unauthorized`: the request is not signed by the installation secret.
    Unauthorized(&'static str),
    /// `404 not_found`.
    NotFound(String),
    /// `409 conflict`: the request contradicts the current state.
    Conflict(String),
    /// `422 validation_failed`: the body or a path value is invalid.
    Invalid(String),
    /// `413 payload_too_large`.
    PayloadTooLarge,
    /// `429 too_many_requests`: the bounded recipient checker is at capacity.
    Busy(String),
    /// `503 service_unavailable`: a dependency (DNS, the replay memory) cannot answer now.
    Unavailable(String),
    /// `500 internal_error`: logged, never detailed on the wire.
    Internal(String),
}

impl ApiError {
    fn parts(&self) -> (StatusCode, &'static str, &'static str, &str) {
        match self {
            Self::Unauthorized(detail) => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Unauthorized",
                detail,
            ),
            Self::NotFound(detail) => (StatusCode::NOT_FOUND, "not_found", "Not found", detail),
            Self::Conflict(detail) => (StatusCode::CONFLICT, "conflict", "Conflict", detail),
            Self::Invalid(detail) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_failed",
                "Validation failed",
                detail,
            ),
            Self::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "Payload too large",
                "the body exceeds 64 KiB",
            ),
            Self::Busy(detail) => (
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_requests",
                "Too many requests",
                detail,
            ),
            Self::Unavailable(detail) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "Service unavailable",
                detail,
            ),
            Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Internal error",
                "the request failed; the cause is in the service's log",
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Self::Internal(cause) = &self {
            tracing::error!(error = %cause, "a control request failed");
        }
        let (status, code, title, detail) = self.parts();
        let body = serde_json::json!({
            "type": "about:blank",
            "title": title,
            "status": status.as_u16(),
            "code": code,
            "detail": detail,
        });
        let mut response = (status, axum::Json(body)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        response
    }
}

impl From<crate::db::Error> for ApiError {
    fn from(error: crate::db::Error) -> Self {
        Self::Internal(format!("database: {error}"))
    }
}

impl From<JoinError> for ApiError {
    fn from(error: JoinError) -> Self {
        Self::Internal(format!("blocking task: {error}"))
    }
}

impl From<CryptoError> for ApiError {
    fn from(error: CryptoError) -> Self {
        Self::Internal(format!("crypto: {error}"))
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(format!("json: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;
    use crate::testing::{TempDir, call, router, signed, signed_as};

    /// Domain names are lowercase, fully qualified, with valid labels and a letters-only final
    /// label: a name reaches DNS lookups, file names and Postfix maps.
    #[test]
    fn accepts_only_lowercase_fully_qualified_domains() {
        for valid in ["example.com", "mail-1.example.co", "a.bc"] {
            assert!(is_domain(valid), "{valid}");
        }
        for invalid in [
            "example",
            "Example.com",
            "-a.com",
            "a-.com",
            "a..com",
            "a.c",
            "a.c0m",
            "a b.com",
            "../etc",
            &format!("{}.com", "a".repeat(64)),
        ] {
            assert!(!is_domain(invalid), "{invalid}");
        }
    }

    /// Logins are lowercase addresses whose local part starts and ends with a letter or digit:
    /// they become lines of Postfix and Dovecot files, so separators and control characters must
    /// never get through.
    #[test]
    fn accepts_only_lowercase_login_addresses() {
        assert_eq!(
            split_address("a.b+c@example.com"),
            Some(("a.b+c", "example.com"))
        );
        for invalid in [
            "example.com",
            "a@b@example.com",
            ".a@example.com",
            "a.@example.com",
            "a..b@example.com",
            "A@example.com",
            "a|b@example.com",
            "a\n@example.com",
            "a@example",
        ] {
            assert_eq!(split_address(invalid), None, "{invalid:?}");
        }
    }

    /// Health answers without a signature, so the supervisor can probe it; readiness checks
    /// the database.
    #[tokio::test]
    async fn health_needs_no_signature() {
        let dir = TempDir::new();
        let (app, _) = router(&dir);
        for path in ["/health/live", "/health/ready"] {
            let request = axum::http::Request::get(path).body(Body::empty()).unwrap();
            assert_eq!(call(&app, request).await.0, StatusCode::NO_CONTENT);
        }
    }

    /// Every `/v1` request must be signed with the installation secret, recently: a missing,
    /// forged or stale signature is `401` before any handler runs, so nothing else can
    /// provision the MTA.
    #[tokio::test]
    async fn refuses_unsigned_forged_and_stale_requests() {
        let dir = TempDir::new();
        let (app, _) = router(&dir);
        let unsigned = axum::http::Request::get("/v1/domains")
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(&app, unsigned).await.0, StatusCode::UNAUTHORIZED);

        let mut forged = signed("GET", "/v1/domains", "");
        forged
            .headers_mut()
            .insert("webhook-signature", HeaderValue::from_static("v1,AAAA"));
        assert_eq!(call(&app, forged).await.0, StatusCode::UNAUTHORIZED);

        let stale = signed_as(
            "GET",
            "/v1/domains",
            "",
            "msg_stale",
            jiff::Timestamp::now().as_second() - 301,
        );
        let (status, problem) = call(&app, stale).await;
        assert_eq!(
            (status, problem["code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("unauthorized"))
        );
    }

    /// A signed request is accepted once: replaying it within the tolerance window is refused,
    /// even against another route, because Standard Webhooks signs only the body.
    #[tokio::test]
    async fn accepts_a_request_id_once() {
        let dir = TempDir::new();
        let (app, _) = router(&dir);
        let now = jiff::Timestamp::now().as_second();
        let first = signed_as("GET", "/v1/domains", "", "msg_once", now);
        assert_eq!(call(&app, first).await.0, StatusCode::OK);
        let replay = signed_as("GET", "/v1/accounts", "", "msg_once", now);
        assert_eq!(call(&app, replay).await.0, StatusCode::CONFLICT);
    }

    /// Bodies are capped at 64 KiB before they are verified or parsed, so a large body cannot
    /// cost memory or time.
    #[tokio::test]
    async fn refuses_bodies_over_64_kib() {
        let dir = TempDir::new();
        let (app, _) = router(&dir);
        let body = format!(r#"{{"name":"{}"}}"#, "a".repeat(MAX_BODY));
        let (status, _) = call(&app, signed("POST", "/v1/domains", &body)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }
}

#[cfg(test)]
mod signature_binding_tests {
    use super::*;
    use crate::testing::{TempDir, call, signed, state};

    #[tokio::test]
    async fn a_control_signature_cannot_change_method_or_route() {
        let dir = TempDir::new();
        let app = router(state(&dir));
        let mut method = signed("GET", "/v1/accounts/account@example.com", "");
        *method.method_mut() = axum::http::Method::DELETE;
        assert_eq!(call(&app, method).await.0, StatusCode::UNAUTHORIZED);
        let mut path = signed("GET", "/v1/domains", "");
        *path.uri_mut() = "/v1/accounts".parse().unwrap();
        assert_eq!(call(&app, path).await.0, StatusCode::UNAUTHORIZED);
    }
}
