//! A test layer that records the canonical events a test causes, with their fields as text, so a
//! test can assert which events an operation emitted, how many, and with which fields.
//!
//! [`Capture::install`] makes it, with the coverage layer, the current thread's subscriber: on a
//! test's current-thread runtime everything the test drives (a request through the in-process
//! router, a job through the runner's harness) emits on that thread, so parallel tests never see
//! each other's events. It also clears the thread's coverage counts (`telemetry::mirror`), so a
//! test compares the two sides of the reconciliation over exactly what it drove.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex};

use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt as _};

/// Keeps a second dispatcher registered while a test installs a thread-local subscriber.
/// With only one registered dispatcher, tracing derives a new callsite's interest from the
/// thread that first reaches it. A parallel test without a subscriber can therefore disable
/// that callsite for the recording test too. This dispatcher is never installed: it records
/// nothing and makes registration consult all subscribers instead of the calling thread.
pub(crate) fn retain_dispatcher() {
    static OTHER: LazyLock<tracing::Dispatch> =
        LazyLock::new(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
    LazyLock::force(&OTHER);
}

/// One canonical event: its name, and every field as text (a `Display` field as displayed).
#[derive(Debug, Clone)]
pub(crate) struct Captured {
    /// The `event` field.
    pub name: String,
    /// Every field, by name.
    pub fields: BTreeMap<String, String>,
}

impl Captured {
    /// The field `name` as text, or `""` when the event has none.
    pub(crate) fn field(&self, name: &str) -> &str {
        self.fields.get(name).map_or("", String::as_str)
    }
}

/// The recorder; its clones share what was recorded.
#[derive(Debug, Clone, Default)]
pub(crate) struct Capture(Arc<Mutex<Vec<Captured>>>);

impl Capture {
    /// Makes the coverage layer and this recorder the current thread's subscriber until the guard
    /// is dropped, and clears the thread's coverage counts.
    pub(crate) fn install(&self) -> tracing::subscriber::DefaultGuard {
        retain_dispatcher();
        super::mirror::reset();
        tracing::subscriber::set_default(
            tracing_subscriber::registry()
                .with(super::Coverage)
                .with(self.clone()),
        )
    }

    /// Every canonical event recorded so far, in order.
    pub(crate) fn events(&self) -> Vec<Captured> {
        self.0.lock().unwrap().clone()
    }

    /// The events named `name` recorded after the first `from` events.
    pub(crate) fn named_since(&self, from: usize, name: &str) -> Vec<Captured> {
        self.events()
            .into_iter()
            .skip(from)
            .filter(|event| event.name == name)
            .collect()
    }
}

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _cx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let Some(name) = fields.0.get("event").cloned() else {
            return;
        };
        self.0.lock().unwrap().push(Captured {
            name,
            fields: fields.0,
        });
    }
}

/// An event's fields as text.
#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

/// The first emission on an unobserved thread must not disable the same callsite for the
/// thread collecting events; otherwise unrelated parallel tests silently lose telemetry.
#[test]
fn an_unobserved_thread_cannot_disable_a_captured_callsite() {
    fn emit() {
        crate::telemetry::unit(crate::telemetry::Event::JobRun);
        tracing::info!(event = "job.run", "capture registration probe");
    }
    let capture = Capture::default();
    let _guard = capture.install();
    std::thread::spawn(emit).join().unwrap();
    emit();
    assert_eq!(capture.named_since(0, "job.run").len(), 1);
}
