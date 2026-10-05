//! The tests of W3C trace correlation: a caller's `traceparent` continued by the request's span,
//! kept by the rows a request creates for later work, and linked to (never joined) by the work
//! that takes those rows up. Traces are recorded on the test's own thread by an SDK tracer that
//! keeps every span, and finished spans are collected in memory, so a test can read the links a
//! span ended with.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider, SpanData, SpanProcessor};
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt as _;
use uuid::Uuid;

use super::{adopt, link, trace_parent};
use crate::testing::TestDb;

/// The caller's trace and span, as a W3C `traceparent` names them.
const CALLER_TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
const CALLER_SPAN: &str = "b7ad6b7169203331";

/// The spans that ended, in the order they ended.
#[derive(Debug, Clone, Default)]
struct Finished(Arc<Mutex<Vec<SpanData>>>);

impl SpanProcessor for Finished {
    fn on_start(&self, _span: &mut opentelemetry_sdk::trace::Span, _cx: &opentelemetry::Context) {}

    fn on_end(&self, span: SpanData) {
        self.0.lock().unwrap().push(span);
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

/// Makes this thread's subscriber one that records traces, every span kept, until the guard is
/// dropped; the spans that end are collected in the returned [`Finished`].
fn traced() -> (Finished, tracing::subscriber::DefaultGuard) {
    super::capture::retain_dispatcher();
    let finished = Finished::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_span_processor(finished.clone())
        .build();
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("correlation"))),
    );
    (finished, guard)
}

/// The four parts of a W3C `traceparent`: version, trace id, span id, flags.
fn parts(trace_parent: &str) -> Vec<&str> {
    trace_parent.split('-').collect()
}

/// A valid `traceparent` makes a span continue the caller's trace with a span id of its own,
/// while a missing, malformed or all-zero one leaves the span to start its own trace, unless the
/// request id is a UUID: then that id is the trace id, so an operator holding a request id finds
/// its trace and the rows it created; a link points a span at another trace without joining it,
/// and a value that is not a `traceparent` adds no link. These are the moves the request path
/// (adopt and keep) and the sender, the runner and the webhook deliveries (link) rely on.
#[test]
fn spans_adopt_a_callers_trace_and_link_without_joining_it() {
    let (finished, _guard) = traced();
    let mut headers = HeaderMap::new();
    headers.insert(
        "traceparent",
        HeaderValue::from_str(&format!("00-{CALLER_TRACE}-{CALLER_SPAN}-01")).unwrap(),
    );
    let request = tracing::info_span!("request");
    // A caller's trace wins over the one its request id would seed.
    adopt(&request, &headers, &Uuid::now_v7().to_string());
    let kept = trace_parent(&request).unwrap();
    let kept_parts = parts(&kept);
    assert_eq!(kept_parts.len(), 4, "{kept}");
    assert_eq!((kept_parts[0], kept_parts[1]), ("00", CALLER_TRACE));
    assert_ne!(
        kept_parts[2], CALLER_SPAN,
        "a span of ours, not the caller's"
    );

    for header in [
        None,
        Some("garbage"),
        Some("00-00000000000000000000000000000000-b7ad6b7169203331-01"),
    ] {
        let mut headers = HeaderMap::new();
        if let Some(value) = header {
            headers.insert("traceparent", HeaderValue::from_static(value));
        }
        let alone = tracing::info_span!("alone");
        adopt(&alone, &headers, "not-a-uuid");
        let own = trace_parent(&alone).unwrap();
        assert_ne!(parts(&own)[1], CALLER_TRACE, "{header:?}");
    }

    let request_id = Uuid::now_v7();
    let seeded = tracing::info_span!("seeded");
    adopt(&seeded, &HeaderMap::new(), &request_id.to_string());
    let own = trace_parent(&seeded).unwrap();
    assert_eq!(
        parts(&own)[1],
        request_id.simple().to_string(),
        "without a traceparent, the request id is the trace id"
    );

    let wave = tracing::info_span!("wave");
    link(&wave, &kept);
    link(&wave, "not a traceparent");
    let own = trace_parent(&wave).unwrap();
    assert_ne!(parts(&own)[1], CALLER_TRACE, "a link never joins the trace");
    drop(wave);
    let ended = finished.0.lock().unwrap();
    let wave = ended.iter().find(|span| span.name == "wave").unwrap();
    let links: Vec<String> = wave
        .links
        .links
        .iter()
        .map(|link| {
            format!(
                "{}-{}",
                link.span_context.trace_id(),
                link.span_context.span_id()
            )
        })
        .collect();
    assert_eq!(links, vec![format!("{CALLER_TRACE}-{}", kept_parts[2])]);
}

/// A caller's `traceparent` is accepted on the API, and the rows its requests create for later
/// work keep the request span's own context, in the caller's trace with a span id of ours: the
/// check a new connection enqueues as a job, and a sent message with its `message.queued` event.
/// That is what the runner, the sender's wave and the webhook deliveries link back to.
#[tokio::test]
async fn the_rows_a_request_creates_keep_its_traceparent() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    // An endpoint that subscribes to `message.queued`, so a sent message records that event.
    let endpoint = app
        .post("/v1/webhook_endpoints")
        .bearer(&acme.key)
        .idempotency(&Uuid::now_v7().to_string())
        .json(json!({ "url": "https://hooks.example.com/n", "event_types": ["message.queued"] }))
        .send()
        .await;
    assert_eq!(endpoint.status, StatusCode::CREATED, "{}", endpoint.json);
    let (_finished, _guard) = traced();
    let caller = format!("00-{CALLER_TRACE}-{CALLER_SPAN}-01");
    let connection = app
        .post("/v1/connections")
        .bearer(&acme.key)
        .idempotency(&Uuid::now_v7().to_string())
        .header("traceparent", &caller)
        .json(json!({
            "provider": "smtp",
            "account_email": "max@acme.example",
            "smtp": { "host": "smtp.acme.example", "port": 587, "security": "starttls", "password": "secret" },
        }))
        .send()
        .await;
    assert_eq!(
        connection.status,
        StatusCode::CREATED,
        "{}",
        connection.json
    );
    let message = app
        .post("/v1/messages")
        .bearer(&acme.key)
        .idempotency(&Uuid::now_v7().to_string())
        .header("traceparent", &caller)
        .json(json!({
            "from": "max@acme.example",
            "to": ["ada@example.com"],
            "subject": "Hello",
            "html": "<p>Hi</p>",
        }))
        .send()
        .await;
    assert_eq!(message.status, StatusCode::ACCEPTED, "{}", message.json);

    let kept: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT 'job ' || kind, trace_parent FROM jobs WHERE workspace_id = $1 AND kind = 'connection.check'
         UNION ALL SELECT 'message', trace_parent FROM messages WHERE workspace_id = $1
         UNION ALL SELECT 'event ' || type, trace_parent FROM outbox_events WHERE workspace_id = $1 AND type = 'message.queued'",
    )
    .bind(acme.id.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert_eq!(kept.len(), 3, "{kept:?}");
    for (row, trace_parent) in &kept {
        let trace_parent = trace_parent
            .as_deref()
            .unwrap_or_else(|| panic!("{row} keeps no traceparent"));
        let kept_parts = parts(trace_parent);
        assert_eq!(kept_parts.len(), 4, "{row}: {trace_parent}");
        assert_eq!(
            (kept_parts[0], kept_parts[1]),
            ("00", CALLER_TRACE),
            "{row}"
        );
        assert_ne!(kept_parts[2], CALLER_SPAN, "{row}: a span of ours");
    }
}
