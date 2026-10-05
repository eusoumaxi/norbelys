//! Telemetry: logs, traces and metrics over OpenTelemetry, the same signals and export as the
//! product's server processes. This binary is a separate deployable that never links the
//! server, so the setup lives here too, with the service name `norbelys-smtp`.
//!
//! [`init`] runs once, before any runtime starts, and [`Telemetry::shutdown`] after it ends: the
//! OTLP exporters use a blocking HTTP client on their own threads. Logs are JSON lines on
//! stdout; with an endpoint, traces, logs and metrics are also exported over OTLP/HTTP, with
//! the subcommand (`norbelys.role`) and the environment as resource attributes. Nothing in
//! telemetry blocks the service: exporters are batched and drop when their queues are full.
//!
//! # Canonical events
//!
//! A unit of work emits one canonical event when it ends: a `tracing` event whose `event` field
//! names it (`mta.<unit>`, the closed list [`Event`]), with typed, bounded fields and never a
//! secret, a password, a body or a credential; its message repeats the name, so a plain log line
//! reads the same. Diagnostic lines (an error's cause, a recovery) carry no `event` field.
//!
//! Each emitter also counts its unit beside its metrics ([`unit()`],
//! `norbelys_telemetry_metric_events_total{event}`), and a layer of the subscriber counts every
//! canonical event that reaches the log pipeline (`norbelys_telemetry_events_total{event}`), as
//! the product's roles do. These compare emission through the subscriber, not delivery by an
//! exporter. Independent scraping remains available when OTLP is unavailable.
//!
//! # Metrics
//!
//! Metrics are named `norbelys_mta_*` and carry only labels from closed sets, beside the coverage
//! counters above and `norbelys_heartbeat`, 1 while `serve` runs ([`heartbeat`]): its absence is
//! how the `telemetry-absent` alert sees a dead service or exporter, as for every role.

use std::sync::{LazyLock, OnceLock};

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::config;

/// The exporters to flush when the process stops.
pub struct Telemetry {
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
    meter: SdkMeterProvider,
}

impl Telemetry {
    /// Flushes and stops every exporter; errors are reported, never fatal.
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
        if let Err(error) = self.meter.shutdown() {
            tracing::warn!(error = %error, "metric exporter shutdown");
        }
    }
}

/// Installs the global subscriber and independent Prometheus reader for `role`.
/// An optional OTLP endpoint adds exports of logs, spans and metrics.
///
/// # Errors
///
/// An OTLP exporter cannot be built from the configured endpoint.
pub fn init(role: &'static str, settings: Option<&config::Telemetry>) -> anyhow::Result<Telemetry> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stdout = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(false)
        .flatten_event(true);

    let resource = resource(role, settings);
    let (scrape, registry) = scrape_reader()?;
    let _ = REGISTRY.set(registry);
    let meters = SdkMeterProvider::builder()
        .with_reader(scrape)
        .with_resource(resource.clone());
    let Some(endpoint) = settings
        .and_then(|s| s.otlp_endpoint.as_deref())
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
        tracing_subscriber::registry()
            .with(filter)
            .with(Coverage)
            .with(stdout)
            .init();
        return Ok(Telemetry {
            tracer: None,
            logger: None,
            meter,
        });
    };

    let base = endpoint.as_str().trim_end_matches('/');

    let spans = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{base}/v1/traces"))
        .build()?;
    let tracer = SdkTracerProvider::builder()
        .with_sampler(opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(
            f64::from(settings.map_or(100, |s| s.trace_sample_percent)) / 100.0,
        ))
        .with_batch_exporter(spans)
        .with_resource(resource.clone())
        .build();

    let logs = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_endpoint(format!("{base}/v1/logs"))
        .build()?;
    let logger = SdkLoggerProvider::builder()
        .with_batch_exporter(logs)
        .with_resource(resource.clone())
        .build();

    let metrics = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(format!("{base}/v1/metrics"))
        .build()?;
    let meter = meters
        .with_periodic_exporter(metrics)
        .with_resource(resource)
        .build();
    opentelemetry::global::set_meter_provider(meter.clone());

    tracing_subscriber::registry()
        .with(filter)
        .with(Coverage)
        .with(stdout)
        .with(tracing_opentelemetry::layer().with_tracer(tracer.tracer("norbelys-smtp")))
        .with(OpenTelemetryTracingBridge::new(&logger))
        .init();

    Ok(Telemetry {
        tracer: Some(tracer),
        logger: Some(logger),
        meter,
    })
}

/// The same identity accompanies independently scraped and OTLP-exported metrics.
fn resource(role: &'static str, settings: Option<&config::Telemetry>) -> Resource {
    Resource::builder()
        .with_service_name("norbelys-smtp")
        .with_attributes([
            KeyValue::new("service.namespace", "norbelys"),
            KeyValue::new("norbelys.role", role),
            KeyValue::new(
                "deployment.environment.name",
                settings
                    .map_or("development", |settings| settings.environment.as_str())
                    .to_owned(),
            ),
        ])
        .build()
}

static REGISTRY: OnceLock<prometheus::Registry> = OnceLock::new();

/// Independent metric scraping, available with or without an OTLP backend.
pub fn exposition() -> Option<String> {
    let registry = REGISTRY.get()?;
    prometheus::TextEncoder::new()
        .encode_to_string(&registry.gather())
        .ok()
}

fn scrape_reader() -> anyhow::Result<(
    opentelemetry_prometheus::PrometheusExporter,
    prometheus::Registry,
)> {
    let registry = prometheus::Registry::new();
    let reader = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .without_counter_suffixes()
        .without_units()
        .build()?;
    Ok((reader, registry))
}

#[cfg(test)]
mod metrics_tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;

    #[test]
    fn metrics_are_readable_without_any_otlp_configuration() {
        let (reader, registry) = scrape_reader().unwrap();
        let provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource("serve", None))
            .build();
        provider
            .meter("fixture")
            .u64_counter("norbelys_mta_fixture_total")
            .build()
            .add(3, &[]);
        let text = prometheus::TextEncoder::new()
            .encode_to_string(&registry.gather())
            .unwrap();
        assert!(
            text.contains("norbelys_mta_fixture_total{otel_scope_name=\"fixture\"} 3"),
            "{text}"
        );
        assert!(text.contains("service_name=\"norbelys-smtp\""), "{text}");
        assert!(text.contains("norbelys_role=\"serve\""), "{text}");
        provider.shutdown().unwrap();
    }
}

/// The meter every module records its `norbelys_mta_*` instruments on; a no-op until
/// [`init`] installed an exporter.
#[must_use]
pub fn meter() -> opentelemetry::metrics::Meter {
    opentelemetry::global::meter("norbelys-smtp")
}

/// The canonical events of the service, each the end of one unit of work (see the module). The
/// `event` field of each carries its name, and the coverage counters are labelled by it; a name
/// outside this list is never counted, so those labels stay bounded.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
)]
pub enum Event {
    /// One request to the control API, answered (a refused signature included).
    #[strum(serialize = "mta.request")]
    Request,
    /// One read of `mail.log` that found lines, or that failed with an error other than the
    /// previous read's: a read that finds nothing is idle polling, and the same failure every
    /// ten seconds would say nothing new (the tail's error counter counts each).
    #[strum(serialize = "mta.tail")]
    Tail,
    /// One batch of delivery events posted to the core's route, whatever came of it.
    #[strum(serialize = "mta.dispatch")]
    Dispatch,
    /// One change of admission control between open and closed: the measures behind it are
    /// gauges, read every few seconds, and only a change is news.
    #[strum(serialize = "mta.admission")]
    Admission,
    /// One batch of queued changes the privileged provisioning helper applied (a batch that
    /// cannot be applied ends the run, and the process reports the error as it stops).
    #[strum(serialize = "mta.provision")]
    Provision,
    /// One bounded local batch confirmed in Turso, or preserved for retry.
    #[strum(serialize = "mta.archive")]
    Archive,
    /// One retention pass that removed rows.
    #[strum(serialize = "mta.retention")]
    Retention,
    /// One check, and patch when needed, of docker-mailserver's account helper.
    #[strum(serialize = "mta.dms_patch")]
    DmsPatch,
    /// One feedback-loop report delivered to the feedback address.
    #[strum(serialize = "mta.feedback")]
    Feedback,
    /// One delivery status notification delivered to the bounce address.
    #[strum(serialize = "mta.bounce")]
    Bounce,
}

impl Event {
    /// The event's name, the value of its `event` field (`mta.request`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

static EVENTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    meter()
        .u64_counter("norbelys_telemetry_events_total")
        .with_description("Canonical events that reached the log pipeline, by event.")
        .build()
});

static METRIC_EVENTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    meter()
        .u64_counter("norbelys_telemetry_metric_events_total")
        .with_description(
            "Units of work that emitted their canonical event, counted beside their metrics, by \
             event.",
        )
        .build()
});

/// Counts one unit of work whose canonical `event` is emitted, on the metric side of the
/// coverage reconciliation (see the module): call it beside the event, exactly when the event is
/// emitted.
pub fn unit(event: Event) {
    METRIC_EVENTS.add(1, &[KeyValue::new("event", event.as_str())]);
}

/// Registers `norbelys_heartbeat`, 1 whenever the metrics are collected, for `serve`: the
/// short-lived subcommands would make it come and go.
pub fn heartbeat() {
    let _ = meter()
        .u64_observable_gauge("norbelys_heartbeat")
        .with_description(
            "1 while the service runs: its absence for minutes means the service, or its \
             exporter, is gone.",
        )
        .with_callback(|observer| observer.observe(1, &[]))
        .build();
}

/// The layer that counts the canonical events reaching the log pipeline (see the module). It
/// sits after the log filter, so an event the filter hides is not counted, which is the drift
/// the reconciliation exists to show.
struct Coverage;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Coverage {
    fn on_event(&self, event: &tracing::Event<'_>, _cx: tracing_subscriber::layer::Context<'_, S>) {
        event.record(&mut Coverage);
    }
}

// The visitor counts only recognised names; arbitrary fields never become metric labels.
impl tracing::field::Visit for Coverage {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "event"
            && let Ok(event) = value.parse::<Event>()
        {
            EVENTS.add(1, &[KeyValue::new("event", event.as_str())]);
        }
    }

    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::str::FromStr as _;

    use strum::IntoEnumIterator as _;

    use super::*;

    /// Every canonical event's name is unique, is `mta.<unit>`, and is found back by its name,
    /// while a name outside the list is not: the coverage counters can only ever carry these
    /// labels, and an emitter whose name is misspelt is counted on no side, so the reconciliation
    /// shows it.
    #[test]
    fn every_event_name_is_unique_and_found_back() {
        let mut names = BTreeSet::new();
        for event in Event::iter() {
            assert_eq!(Event::from_str(event.as_str()), Ok(event), "{event:?}");
            assert!(names.insert(event.as_str()), "{} twice", event.as_str());
            assert!(
                event
                    .as_str()
                    .strip_prefix("mta.")
                    .is_some_and(|unit| !unit.is_empty() && !unit.contains('.')),
                "{} is mta.<unit>",
                event.as_str()
            );
        }
        for unknown in ["", "mta", "mta.requests", "http.request", "MTA.REQUEST"] {
            assert!(Event::from_str(unknown).is_err(), "{unknown}");
        }
    }
}
