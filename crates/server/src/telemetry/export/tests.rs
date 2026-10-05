//! Real SDK overflow and stdout failure fixtures; no collector or global provider is required.

use super::*;
use opentelemetry::logs::{Logger as _, LoggerProvider as _};
use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
use tracing_subscriber::layer::SubscriberExt as _;

#[derive(Debug)]
struct Blocked {
    started: std::sync::mpsc::Sender<()>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl Blocked {
    fn new() -> (
        Self,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (started, received) = std::sync::mpsc::channel();
        let (sent, release) = std::sync::mpsc::channel();
        (
            Self {
                started,
                release: std::sync::Mutex::new(Some(release)),
            },
            received,
            sent,
        )
    }

    fn block(&self) {
        if let Some(release) = self.release.lock().unwrap().take() {
            self.started.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    }
}

impl SpanExporter for Blocked {
    async fn export(&self, _: Vec<SpanData>) -> OTelSdkResult {
        self.block();
        Ok(())
    }
}

impl LogExporter for Blocked {
    async fn export(&self, _: LogBatch<'_>) -> OTelSdkResult {
        self.block();
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Observed(Arc<std::sync::Mutex<Vec<(&'static str, u64)>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Observed {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if let Some(loss) = queue_loss(event) {
            self.0.lock().unwrap().push(loss);
        }
    }
}

#[test]
fn sdk_queue_overflow_totals_are_observed_for_both_signals() {
    crate::telemetry::capture::retain_dispatcher();
    let observed = Observed::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(observed.clone()));
    let (exporter, started, release) = Blocked::new();
    let processor = opentelemetry_sdk::trace::BatchSpanProcessor::builder(exporter)
        .with_batch_config(
            opentelemetry_sdk::trace::BatchConfigBuilder::default()
                .with_max_queue_size(1)
                .with_max_export_batch_size(1)
                .build(),
        )
        .build();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_span_processor(processor)
        .build();
    let tracer = provider.tracer("fixture");
    tracer.start("first").end();
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    for _ in 0..16 {
        tracer.start("queued").end();
    }
    release.send(()).unwrap();
    provider.shutdown().unwrap();

    let (exporter, started, release) = Blocked::new();
    let processor = opentelemetry_sdk::logs::BatchLogProcessor::builder(exporter)
        .with_batch_config(
            opentelemetry_sdk::logs::BatchConfigBuilder::default()
                .with_max_queue_size(1)
                .with_max_export_batch_size(1)
                .build(),
        )
        .build();
    let provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
        .with_log_processor(processor)
        .build();
    let logger = provider.logger("fixture");
    logger.emit(logger.create_log_record());
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    for _ in 0..16 {
        logger.emit(logger.create_log_record());
    }
    release.send(()).unwrap();
    provider.shutdown().unwrap();
    assert_eq!(
        *observed.0.lock().unwrap(),
        vec![("traces_queue", 15), ("logs_queue", 15)]
    );
    // Similar text from another target and other SDK warnings never become loss counters.
    tracing::warn!(
        name = "BatchLogProcessor.LogsDropped",
        dropped_logs_count = 100_u64
    );
    tracing::warn!(target: "opentelemetry_sdk", name = "unrelated", dropped_logs_count = 100_u64);
    assert_eq!(observed.0.lock().unwrap().len(), 2);
}

#[test]
fn broken_stdout_and_full_or_disconnected_queues_never_stall_a_producer() {
    use std::io::Write as _;
    struct Broken(Arc<AtomicU64>);
    impl std::io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let errors = Arc::new(AtomicU64::new(0));
    let (stdout, pending) = Stdout::start_with(Broken(Arc::clone(&errors))).unwrap();
    Line(&stdout).write_all(b"fixture\n").unwrap();
    pending.drain(Duration::from_secs(1));
    assert_eq!(pending.0.load(Ordering::Acquire), 0);
    assert_eq!(errors.load(Ordering::Relaxed), 1);
    let (queue, receiver) = sync_channel(1);
    let stdout = Stdout {
        queue,
        pending: Arc::new(AtomicU64::new(0)),
    };
    assert_eq!(Line(&stdout).write(b"first").unwrap(), 5);
    assert_eq!(Line(&stdout).write(b"full").unwrap(), 4);
    assert_eq!(stdout.pending.load(Ordering::Acquire), 1);
    assert_eq!(receiver.recv().unwrap(), b"first");
    stdout.pending.fetch_sub(1, Ordering::AcqRel);
    drop(receiver);
    assert_eq!(Line(&stdout).write(b"closed").unwrap(), 6);
    assert_eq!(stdout.pending.load(Ordering::Acquire), 0);
    Line(&stdout).flush().unwrap();
}
