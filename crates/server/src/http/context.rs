//! The request context: the request id and the path, available to every problem and log
//! line of the request without being threaded through every function.
//!
//! `X-Request-Id` is set by the reverse proxy in front of the API in production, which
//! overwrites any value a client sent; a request that arrives without one (local runs, a
//! direct call) gets a fresh UUIDv7 here, so every request has an id.
//!
//! The same middleware measures every request: `norbelys_http_requests_total{route, method,
//! status_class}` and `norbelys_http_request_duration_seconds{route, method}` (by route
//! template, never by path, so the labels stay bounded), a span per request (`http.request`,
//! kept or not by the trace sampler), and the request's canonical event. That event is emitted
//! for every error, refusal and slow answer, for 5 % of the fast successes of the hot routes and
//! never for a health probe's success (`domain::telemetry::keep_request`); the metrics count
//! every request.
//!
//! A client's W3C `traceparent` makes the request's span continue the caller's trace; without one,
//! a UUID request id is the trace's id (`telemetry::adopt`). The span's own context is part of
//! the request's context ([`trace_parent`]): the rows the request creates for later work
//! (`messages`, `jobs`, `outbox_events`) keep it, so that work links back to this request, and a
//! request id leads to those rows.

use std::sync::LazyLock;

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use tracing::Instrument as _;
use uuid::Uuid;

use crate::domain::telemetry::keep_request;
use crate::telemetry::{self, Event};

/// The header that carries the request id.
pub static REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

static REQUESTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_http_requests_total")
        .with_description("HTTP requests answered, by route template, method and status class.")
        .build()
});

static DURATION: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_histogram("norbelys_http_request_duration_seconds")
        .with_unit("s")
        .with_description("How long the api took to answer, by route template and method.")
        .with_boundaries(vec![
            0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 10.0,
        ])
        .build()
});

/// What the current request is.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub request_id: String,
    pub path: String,
    /// The W3C `traceparent` of the request's span, when this process records traces.
    pub trace_parent: Option<String>,
}

tokio::task_local! {
    static CURRENT: RequestContext;
}

/// The current request's context, outside a request `None`.
#[must_use]
pub fn current() -> Option<RequestContext> {
    CURRENT.try_with(Clone::clone).ok()
}

/// The W3C `traceparent` of the current request's span, which every row a request creates for
/// later work keeps (`messages`, `jobs`, `outbox_events`; see the module): `None` outside a
/// request (work a job or a sender creates), or when this process records no traces.
#[must_use]
pub fn trace_parent() -> Option<String> {
    CURRENT
        .try_with(|context| context.trace_parent.clone())
        .ok()
        .flatten()
}

/// Middleware: fixes the request id, measures the request (see the module), records its
/// canonical event (method, matched route, status, duration, request id: one line answers "what
/// happened to this request") when the sampling keeps it, and echoes the id on the response.
pub async fn layer(mut request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get(&REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map_or_else(|| Uuid::now_v7().to_string(), str::to_owned);
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        request.headers_mut().insert(REQUEST_ID.clone(), value);
    }
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or_else(
            || "unmatched".to_owned(),
            |matched| matched.as_str().to_owned(),
        );
    let span = tracing::info_span!(
        "http.request",
        http.request.method = %method,
        http.route = route.as_str(),
        request_id = %request_id,
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        http.response.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty
    );
    // The caller's trace first: asking the span for its context starts it.
    telemetry::adopt(&span, request.headers(), &request_id);
    let context = RequestContext {
        request_id: request_id.clone(),
        path,
        trace_parent: telemetry::trace_parent(&span),
    };
    let started = std::time::Instant::now();
    let mut response = CURRENT
        .scope(context, next.run(request))
        .instrument(span.clone())
        .await;
    let _entered = span.enter();
    let elapsed = started.elapsed();
    let status = response.status().as_u16();
    span.record("http.response.status_code", status);
    if status >= 500 {
        span.record("otel.status_code", "ERROR");
        span.record("error.type", "server_error");
    }
    let method = method.as_str();
    REQUESTS.add(
        1,
        &[
            KeyValue::new("route", route.clone()),
            KeyValue::new("method", method.to_owned()),
            KeyValue::new("status_class", status_class(status)),
        ],
    );
    DURATION.record(
        elapsed.as_secs_f64(),
        &[
            KeyValue::new("route", route.clone()),
            KeyValue::new("method", method.to_owned()),
        ],
    );
    if keep_request(method, &route, status, elapsed, &request_id) {
        telemetry::unit(Event::HttpRequest);
        tracing::info!(
            event = "http.request",
            request_id = %request_id,
            method = %method,
            route = route.as_str(),
            status,
            duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "request"
        );
    }
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(REQUEST_ID.clone(), value);
    }
    response
}

/// The class of `status` as a label: `2xx`, `4xx`.
fn status_class(status: u16) -> &'static str {
    match status / 100 {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        _ => "5xx",
    }
}
