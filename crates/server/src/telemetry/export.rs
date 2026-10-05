//! What leaves the process, and what it loses on the way.
//!
//! - [`Stdout`] is the writer of the JSON log lines: each line goes to a writer thread through a
//!   bounded queue of [`LINES`] lines. When standard output cannot keep up (a slow pipe, a full
//!   disk behind the container runtime) the queue fills and new lines are dropped and counted,
//!   so logging never blocks a request, a claim or a submission. [`Pending`] lets the process
//!   wait, at its very end, for the lines already queued.
//! - [`Counted`] wraps an OTLP exporter: the spans or log records of a batch whose export failed
//!   (the backend unreachable, a timeout) are lost, and counted. The SDK's batch processors drop
//!   further items when their queue is full; [`ProcessorLosses`] observes their exact lifetime
//!   totals at shutdown. Queue loss starts are diagnostic events while the process runs.
//!
//! `norbelys_telemetry_dropped_total{signal}` distinguishes queued stdout loss (`stdout`),
//! refused writes (`stdout_write`), failed span/log exports (`traces`, `logs`), SDK queue loss
//! (`traces_queue`, `logs_queue`) and failed metric batches (`metrics_batches`). The last counts
//! batches rather than records. These counters expose observed loss; they do not acknowledge
//! end-to-end delivery, and a collector failure may also prevent exporting the loss counter.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::logs::Severity;
use opentelemetry::metrics::Counter;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogBatch, LogExporter};
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};

/// Log lines queued for standard output before new ones are dropped: a few megabytes at most.
const LINES: usize = 4_096;

static DROPPED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_telemetry_dropped_total")
        .with_description(
            "Observed stdout, exporter and processor queue losses by finite signal; metric \
             losses count failed batches. Independent scraping remains available.",
        )
        .build()
});

/// Counts `count` items of `signal` lost.
fn dropped(signal: &'static str, count: u64) {
    if count > 0 {
        DROPPED.add(count, &[KeyValue::new("signal", signal)]);
    }
}

/// SDK 0.33 reports queue-overflow totals at shutdown. Exact names and count fields are
/// version-specific and covered by a fixture test; arbitrary SDK text is never a metric label.
pub(super) struct ProcessorLosses;

#[derive(Default)]
struct Loss {
    name: Option<&'static str>,
    count: u64,
}

impl tracing::field::Visit for Loss {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "name" {
            self.name = match value {
                "BatchSpanProcessor.SpansDropped" => Some("traces_queue"),
                "BatchLogProcessor.LogsDropped" => Some("logs_queue"),
                _ => None,
            };
        }
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if matches!(field.name(), "dropped_span_count" | "dropped_logs_count") {
            self.count = value;
        }
    }
    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ProcessorLosses {
    fn on_event(&self, event: &tracing::Event<'_>, _cx: tracing_subscriber::layer::Context<'_, S>) {
        if let Some((signal, count)) = queue_loss(event) {
            dropped(signal, count);
        }
    }
}

fn queue_loss(event: &tracing::Event<'_>) -> Option<(&'static str, u64)> {
    if event.metadata().target() != "opentelemetry_sdk" {
        return None;
    }
    let mut loss = Loss::default();
    event.record(&mut loss);
    loss.name.map(|signal| (signal, loss.count))
}

/// The lossy, non-blocking writer of log lines on standard output (see the module).
#[derive(Debug, Clone)]
pub(super) struct Stdout {
    queue: SyncSender<Vec<u8>>,
    pending: Arc<AtomicU64>,
}

/// The lines queued and not yet written.
#[derive(Debug)]
pub(super) struct Pending(Arc<AtomicU64>);

impl Stdout {
    /// Starts the writer thread.
    ///
    /// # Errors
    ///
    /// The thread could not be started.
    pub(super) fn start() -> std::io::Result<(Self, Pending)> {
        Self::start_with(std::io::stdout())
    }

    fn start_with(
        mut out: impl std::io::Write + Send + 'static,
    ) -> std::io::Result<(Self, Pending)> {
        let (queue, lines) = sync_channel::<Vec<u8>>(LINES);
        let pending = Arc::new(AtomicU64::new(0));
        let written = Arc::clone(&pending);
        std::thread::Builder::new()
            .name("norbelys-stdout".to_owned())
            .spawn(move || {
                for line in lines {
                    // A line standard output refuses is lost like a dropped one: there is
                    // nowhere else to report it.
                    if out.write_all(&line).is_err() {
                        dropped("stdout_write", 1);
                    }
                    written.fetch_sub(1, Ordering::AcqRel);
                }
            })?;
        Ok((
            Self {
                queue,
                pending: Arc::clone(&pending),
            },
            Pending(pending),
        ))
    }
}

#[cfg(test)]
mod tests;

/// One event's line on its way to the writer thread.
#[derive(Debug)]
pub(super) struct Line<'a>(&'a Stdout);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Stdout {
    type Writer = Line<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        Line(self)
    }
}

impl std::io::Write for Line<'_> {
    /// Queues `buf`, one formatted event, or drops it when the queue is full; it never blocks
    /// and never fails, so the subscriber never stalls or reports an error on a lost line.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.pending.fetch_add(1, Ordering::AcqRel);
        // Full (standard output is behind) or disconnected (the writer thread is gone): lost.
        if self.0.queue.try_send(buf.to_vec()).is_err() {
            self.0.pending.fetch_sub(1, Ordering::AcqRel);
            dropped("stdout", 1);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Pending {
    /// Waits, at most `within`, until every queued line is written: the last lines of a
    /// stopping process (why it stopped) are the ones most worth keeping.
    pub(super) fn drain(&self, within: Duration) {
        let until = Instant::now() + within;
        while self.0.load(Ordering::Acquire) > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// An OTLP exporter whose failed batches are counted as lost (see the module).
#[derive(Debug)]
pub(super) struct Counted<E> {
    inner: E,
    signal: &'static str,
}

impl<E> Counted<E> {
    /// Wraps `inner`, counting its losses as `signal`.
    pub(super) fn new(inner: E, signal: &'static str) -> Self {
        Self { inner, signal }
    }
}

impl<E: SpanExporter> SpanExporter for Counted<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let count = u64::try_from(batch.len()).unwrap_or(u64::MAX);
        let result = self.inner.export(batch).await;
        if result.is_err() {
            dropped(self.signal, count);
        }
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: LogExporter> LogExporter for Counted<E> {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        let count = u64::try_from(batch.iter().count()).unwrap_or(u64::MAX);
        let result = self.inner.export(batch).await;
        if result.is_err() {
            dropped(self.signal, count);
        }
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn event_enabled(&self, level: Severity, target: &str, name: Option<&str>) -> bool {
        self.inner.event_enabled(level, target, name)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: PushMetricExporter> PushMetricExporter for Counted<E> {
    async fn export(
        &self,
        metrics: &opentelemetry_sdk::metrics::data::ResourceMetrics,
    ) -> OTelSdkResult {
        let result = self.inner.export(metrics).await;
        if result.is_err() {
            dropped("metrics_batches", 1);
            tracing::warn!(
                error_code = "metric_export_failed",
                "metric batch could not be exported; independent scraping remains available"
            );
        }
        result
    }
    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }
    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }
    fn temporality(&self) -> opentelemetry_sdk::metrics::Temporality {
        self.inner.temporality()
    }
}
