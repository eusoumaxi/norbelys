//! Telemetry: logs, traces and metrics for every role.
//!
//! Every role calls [`init`] once, before doing anything else, and [`Telemetry::shutdown`]
//! last, so buffered spans, log records and log lines are flushed when the process stops.
//!
//! # Signals
//!
//! - **Logs** are JSON lines on stdout, one object per event with its fields flattened, so a
//!   collector or `jq` can read them without a parser of our own. They go through a lossy,
//!   non-blocking writer (`export::Stdout`): a standard output that cannot keep up loses lines,
//!   counted, and never stalls the process. The verbosity comes from `RUST_LOG` (default `info`).
//! - **Canonical events** ([`Event`]) are the log events that close a unit of work (a request, a
//!   claim, a wave, a poll, a job run), one per unit, with its outcome. Batch events are always
//!   emitted; a request's event is decided by `domain::telemetry::keep_request`, so fast
//!   successes on hot routes are recorded for 5 % of requests, a health probe's success never,
//!   and everything else always.
//! - **Traces** are spans per request, job run and wave, with a per-message child under a wave.
//!   OpenTelemetry's trace-id ratio sampler retains complete traces at the configured
//!   `NORBELYS_TRACE_SAMPLE_PERCENT` (100 by default); children are never sampled separately.
//! - **Metrics** are never sampled. They are always readable in Prometheus's text format at
//!   `/metrics` on the role's health port ([`exposition`]), and pushed over OTLP too when an
//!   endpoint is configured. Their names already carry their unit and `_total`, so the
//!   exposition keeps them exactly as written. `norbelys_heartbeat` is 1 while the role runs:
//!   its absence is how a dead role, or a dead exporter, shows.
//! - **OTLP**: with `OTEL_EXPORTER_OTLP_ENDPOINT`, traces, logs and metrics go to `/v1/traces`,
//!   `/v1/logs` and `/v1/metrics` under it (<https://opentelemetry.io/docs/specs/otlp/>), through
//!   the SDK's bounded batch exporters, which drop rather than block; failed exports are counted
//!   in `norbelys_telemetry_dropped_total`. The SDK exports from its own threads, which is why
//!   the OTLP client is the blocking `reqwest` one: it never runs on the async runtime.
//!
//! Every signal carries the role as `service.name` within the `service.namespace` `norbelys`, a
//! `service.instance.id` of its own per process, the role again as `norbelys.role`, and the
//! deployment (`deployment.environment.name`), so one backend tells roles and replicas apart.
//!
//! # Correlation
//!
//! A client's W3C `traceparent` (<https://www.w3.org/TR/trace-context/>) is accepted: the request's
//! span continues the caller's trace ([`adopt`]), while whether it is kept stays the sampler's
//! decision. Without one, a request id that is a UUID (as the proxy makes it) is the trace's id,
//! so an operator holding a request id holds its trace id too. The rows a request creates for
//! later work (`messages`, `jobs`, `outbox_events`) keep the request span's own `traceparent`
//! ([`trace_parent`]); the sender's wave span, a job's run
//! span and a webhook delivery's span **link** to it ([`link`]) rather than continue it, because
//! the request ended long before (a message waits for its window, a job for its turn), and a span
//! must not stay open across that wait. Without an OTLP endpoint no trace is recorded, so nothing
//! is kept and nothing is linked.
//!
//! # Coverage
//!
//! Every unit of work that emits its canonical event also counts it on the metric side
//! ([`unit()`], `norbelys_telemetry_metric_events_total{event}`), and a layer of the subscriber
//! ([`Coverage`]) counts every canonical event that reaches the log pipeline
//! (`norbelys_telemetry_events_total{event}`). Their difference identifies losses before the
//! subscriber (a filter, a missing layer or a misspelt event). Equality does not prove delivery
//! to stdout or OTLP: exporter/queue losses have separate dropped counters. Error logs are
//! unsampled at emission; their traces remain subject to the configured head sampler.
//!
//! Events are `tracing` events with bounded, named fields (an id, a status, a duration), never
//! free text assembled from values: that keeps label sets small in the metrics backend and makes
//! logs queryable. The workspace lints forbid `println!` for the same reason.

#[cfg(test)]
pub(crate) mod capture;
#[cfg(test)]
mod correlation;
#[cfg(test)]
mod coverage;
mod export;
pub(crate) mod fleet;
mod host;
pub(crate) mod postgres;

use std::collections::HashMap;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use opentelemetry::propagation::{Extractor, TextMapPropagator as _};
use opentelemetry::trace::{
    SpanContext, SpanId, TraceContextExt as _, TraceFlags, TraceId, TraceState, TracerProvider as _,
};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use uuid::Uuid;

use crate::config::Common;
pub use crate::domain::telemetry::Event;

/// How long a stopping process waits for the log lines still queued for standard output.
const LINES_GRACE: Duration = Duration::from_secs(2);

static EVENTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_telemetry_events_total")
        .with_description("Canonical events that reached the log pipeline, by event.")
        .build()
});

static METRIC_EVENTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_telemetry_metric_events_total")
        .with_description(
            "Units of work that emitted their canonical event, counted beside their metrics, by \
             event.",
        )
        .build()
});

/// The registry `/metrics` reads, filled by the Prometheus reader [`init`] installs.
static REGISTRY: OnceLock<prometheus::Registry> = OnceLock::new();

/// The exporters to flush when the process stops.
pub struct Telemetry {
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
    meter: SdkMeterProvider,
    pending: export::Pending,
}

impl Telemetry {
    /// Flushes and stops every exporter, then waits briefly for the log lines still queued;
    /// errors are reported, never fatal.
    pub fn shutdown(self) {
        if let Some(tracer) = self.tracer
            && let Err(error) = tracer.shutdown()
        {
            tracing::warn!(error = %error, "trace exporter shutdown");
        }
        if let Some(logger) = self.logger
            && let Err(error) = logger.shutdown()
        {
            tracing::warn!(error = %error, "log exporter shutdown");
        }
        // SDK queue-drop totals are emitted during tracer/logger shutdown. Drain stdout before
        // the final metric collection so refused writes are included in that last export too.
        self.pending.drain(LINES_GRACE);
        if let Err(error) = self.meter.shutdown() {
            tracing::warn!(error = %error, "metric exporter shutdown");
        }
        self.pending.drain(LINES_GRACE);
    }
}

/// Installs the global subscriber and the meter provider for `role`.
///
/// # Errors
///
/// The standard-output writer cannot start, or an exporter cannot be built from the configured
/// endpoint.
pub fn init(role: &'static str, common: &Common) -> anyhow::Result<Telemetry> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        .add_directive("opentelemetry_sdk=warn".parse()?);
    let (writer, pending) = export::Stdout::start()?;
    let stdout = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(false)
        .flatten_event(true)
        .with_writer(writer);
    let resource = resource(role, common);

    // Metrics: always readable at `/metrics`, and pushed over OTLP when it is configured. The
    // names already end in their unit and `_total`, so the exposition must not add them again.
    let registry = prometheus::Registry::new();
    let scrape = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .without_counter_suffixes()
        .without_units()
        .build()?;
    // A process initialises once; a second call would keep the first registry.
    let _ = REGISTRY.set(registry);
    let meters = SdkMeterProvider::builder()
        .with_reader(scrape)
        .with_resource(resource.clone());

    let Some(endpoint) = common
        .otlp_endpoint
        .as_deref()
        .and_then(|value| url::Url::parse(value).ok())
        .filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
    else {
        let meter = meters.build();
        opentelemetry::global::set_meter_provider(meter.clone());
        register_instruments();
        tracing_subscriber::registry()
            .with(filter)
            .with(export::ProcessorLosses)
            .with(Coverage)
            .with(stdout)
            .init();
        return Ok(Telemetry {
            tracer: None,
            logger: None,
            meter,
            pending,
        });
    };

    let base = endpoint.as_str().trim_end_matches('/');
    let spans = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{base}/v1/traces"))
        .build()?;
    let tracer = SdkTracerProvider::builder()
        .with_sampler(Sampler::TraceIdRatioBased(
            f64::from(common.trace_sample_percent) / 100.0,
        ))
        .with_batch_exporter(export::Counted::new(spans, "traces"))
        .with_resource(resource.clone())
        .build();

    let logs = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_endpoint(format!("{base}/v1/logs"))
        .build()?;
    let logger = SdkLoggerProvider::builder()
        .with_batch_exporter(export::Counted::new(logs, "logs"))
        .with_resource(resource)
        .build();

    let metrics = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(format!("{base}/v1/metrics"))
        .build()?;
    let meter = meters
        .with_periodic_exporter(export::Counted::new(metrics, "metrics_batches"))
        .build();
    opentelemetry::global::set_meter_provider(meter.clone());
    register_instruments();

    tracing_subscriber::registry()
        .with(filter)
        .with(export::ProcessorLosses)
        .with(Coverage)
        .with(stdout)
        .with(tracing_opentelemetry::layer().with_tracer(tracer.tracer("norbelys")))
        .with(OpenTelemetryTracingBridge::new(&logger))
        .init();

    Ok(Telemetry {
        tracer: Some(tracer),
        logger: Some(logger),
        meter,
        pending,
    })
}

/// Every signal's resource: the role, its instance, its deployment (see the module).
fn resource(role: &'static str, common: &Common) -> Resource {
    Resource::builder()
        .with_service_name(role)
        .with_attributes([
            KeyValue::new("service.namespace", "norbelys"),
            KeyValue::new("service.instance.id", Uuid::now_v7().to_string()),
            KeyValue::new("norbelys.role", role),
            KeyValue::new("deployment.environment.name", common.environment.clone()),
        ])
        .build()
}

/// The instruments of the process itself: the heartbeat and its host's readings, read when the
/// metrics are collected.
fn register_instruments() {
    let meter = opentelemetry::global::meter("norbelys");
    let _ = meter
        .u64_observable_gauge("norbelys_heartbeat")
        .with_description(
            "1 while the role runs: its absence for minutes means the role, or its exporter, is \
             gone.",
        )
        .with_callback(|observer| observer.observe(1, &[]))
        .build();
    host::register(&meter);
}

/// Every metric in Prometheus's text format, for `/metrics`; `None` before [`init`] (in tests)
/// or when the registry cannot be encoded.
#[must_use]
pub fn exposition() -> Option<String> {
    use prometheus::Encoder as _;
    let registry = REGISTRY.get()?;
    let mut text = Vec::new();
    if let Err(error) = prometheus::TextEncoder::new().encode(&registry.gather(), &mut text) {
        tracing::warn!(error = %error, "the metrics could not be encoded");
        return None;
    }
    String::from_utf8(text).ok()
}

/// The W3C Trace Context header carrying a trace's id and its parent span's
/// (<https://www.w3.org/TR/trace-context/#traceparent-header>).
const TRACEPARENT: &str = "traceparent";

/// Makes `span` continue the trace a client's `traceparent` header (with its `tracestate`)
/// names (see the module), or, without a valid one, the trace its request id seeds
/// ([`seeded`]). Neither changes anything when the request id is not a UUID: the span starts a
/// trace of its own. Call it before the span is first entered or asked for its context; without
/// a trace exporter it does nothing.
pub fn adopt(span: &tracing::Span, headers: &http::HeaderMap, request_id: &str) {
    let remote = TraceContextPropagator::new()
        .extract_with_context(&opentelemetry::Context::new(), &Headers(headers));
    let parent = if remote.span().span_context().is_valid() {
        remote
    } else if let Some(seed) = seeded(request_id) {
        opentelemetry::Context::new().with_remote_span_context(seed)
    } else {
        return;
    };
    // Refused only without the tracing layer (no exporter) or once the span has started.
    let _ = span.set_parent(parent);
}

/// The remote parent a request id seeds when the client sent no `traceparent`: the UUID's 16
/// bytes as the trace id and its last 8 as the parent's span id (the proxy that named the
/// request stands for that parent). So the request id an operator holds is the trace's id, and
/// every row the request created for later work keeps it: a `trace_parent` that starts with
/// `00-<the request id's 32 hex digits>-`. The sampled flag is left unset, since the sampler
/// decides by the trace id and ignores a remote decision. `None` for an id that is not a UUID,
/// or whose trace or span part would be all zeros (invalid in W3C Trace Context).
fn seeded(request_id: &str) -> Option<SpanContext> {
    let bytes = Uuid::try_parse(request_id).ok()?.into_bytes();
    let span_bytes: [u8; 8] = bytes.get(8..)?.try_into().ok()?;
    let trace_id = TraceId::from_bytes(bytes);
    let span_id = SpanId::from_bytes(span_bytes);
    (trace_id != TraceId::INVALID && span_id != SpanId::INVALID).then(|| {
        SpanContext::new(
            trace_id,
            span_id,
            TraceFlags::default(),
            true,
            TraceState::default(),
        )
    })
}

/// The W3C `traceparent` of `span` (`00-<trace id>-<span id>-<flags>`), the context a row
/// created under it keeps so the work that takes the row up later can link back (see the
/// module); `None` when this process records no traces, and the span therefore has no context.
#[must_use]
pub fn trace_parent(span: &tracing::Span) -> Option<String> {
    let mut carrier: HashMap<String, String> = HashMap::new();
    TraceContextPropagator::new().inject_context(&span.context(), &mut carrier);
    carrier.remove(TRACEPARENT)
}

/// Links `span` to the span a stored W3C `traceparent` names, without continuing its trace (see
/// the module). A value that does not parse adds nothing. The SDK keeps the first 128 links of a
/// span and drops the rest, which bounds a wave of many messages.
pub fn link(span: &tracing::Span, trace_parent: &str) {
    let carrier = HashMap::from([(TRACEPARENT.to_owned(), trace_parent.to_owned())]);
    let remote = TraceContextPropagator::new()
        .extract_with_context(&opentelemetry::Context::new(), &carrier);
    span.add_link(remote.span().span_context().clone());
}

/// A request's headers, as the trace context propagator reads them.
struct Headers<'a>(&'a http::HeaderMap);

impl Extractor for Headers<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}

/// Counts one unit of work whose canonical `event` is emitted, on the metric side of the
/// coverage reconciliation (see the module). Call it beside the event, exactly when the event
/// is emitted: a request whose event the sampling leaves out is not counted here either.
pub fn unit(event: Event) {
    METRIC_EVENTS.add(1, &[KeyValue::new("event", event.as_str())]);
    #[cfg(test)]
    mirror::count(&mirror::UNITS, event);
}

/// The layer that counts the canonical events reaching the log pipeline (see the module). It
/// sits after the log filter, so an event the filter hides is not counted, which is the drift
/// the reconciliation exists to show.
pub(crate) struct Coverage;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Coverage {
    fn on_event(&self, event: &tracing::Event<'_>, _cx: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().fields().field("event").is_none() {
            return;
        }
        let mut name = Name(None);
        event.record(&mut name);
        if let Some(event) = name.0 {
            EVENTS.add(1, &[KeyValue::new("event", event.as_str())]);
            #[cfg(test)]
            mirror::count(&mirror::SEEN, event);
        }
    }
}

/// The canonical event an event's `event` field names, if it names one.
struct Name(Option<Event>);

impl tracing::field::Visit for Name {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "event" {
            self.0 = value.parse().ok();
        }
    }

    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
}

/// The coverage counters mirrored per thread, for the tests that drive operations and compare
/// both sides of the reconciliation: on a test's current-thread runtime, every request and job
/// it drives runs on its own thread, so parallel tests never see each other's counts.
#[cfg(test)]
pub(crate) mod mirror {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::thread::LocalKey;

    use super::Event;

    /// One side's counts.
    pub(crate) type Counts = BTreeMap<Event, u64>;

    thread_local! {
        /// The events the pipeline saw.
        pub(super) static SEEN: RefCell<Counts> = const { RefCell::new(BTreeMap::new()) };
        /// The units counted beside their metrics.
        pub(super) static UNITS: RefCell<Counts> = const { RefCell::new(BTreeMap::new()) };
    }

    /// Counts one `event` on `side`.
    pub(super) fn count(side: &'static LocalKey<RefCell<Counts>>, event: Event) {
        side.with(|counts| *counts.borrow_mut().entry(event).or_default() += 1);
    }

    /// Forgets this thread's counts.
    pub(crate) fn reset() {
        SEEN.with(|counts| counts.borrow_mut().clear());
        UNITS.with(|counts| counts.borrow_mut().clear());
    }

    /// This thread's counts: the events the pipeline saw, then the units counted beside metrics.
    pub(crate) fn counts() -> (Counts, Counts) {
        (
            SEEN.with(|counts| counts.borrow().clone()),
            UNITS.with(|counts| counts.borrow().clone()),
        )
    }
}
