//! The process lifecycle every role shares.
//!
//! - [`shutdown_signal`] resolves on Ctrl-C or `SIGTERM`, the signal a container runtime
//!   sends before stopping a process; servers pass it to their graceful shutdown so
//!   in-flight requests finish.
//! - [`Shutdown`] is the same signal as a cloneable flag for long-running loops (claims,
//!   polls, job lanes) that must stop taking new work and let the current unit finish.
//! - [`serve_health`] gives the background roles, which serve no HTTP of their own, the two
//!   probes an orchestrator needs: `/health/live` (the process runs) and `/health/ready`
//!   (it can reach the database, so it can do useful work), and `/metrics`, the process's
//!   metrics in Prometheus's text format (`telemetry::exposition`).
//! - [`serve_metrics`] serves `/metrics` alone on the health address of the roles that answer
//!   their probes on their own listener (the api, tracking). The address is the process's own
//!   port, never the public one: metrics carry no identifiers, but they describe the deployment.

use std::net::SocketAddr;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::sync::watch;

use crate::db::Database;

/// Read the wall clock at the application boundary. Domain decisions receive this instant
/// as an argument, which keeps their calculations deterministic and independent of I/O.
#[must_use]
pub fn now() -> crate::domain::time::Timestamp {
    crate::domain::time::Timestamp(jiff::Timestamp::now())
}

/// Resolves when the process is asked to stop: Ctrl-C, or `SIGTERM` from the supervisor.
pub async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %error, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(error = %error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("shutdown requested");
}

/// A shutdown flag that long-running loops watch: `true` once the process must stop.
#[derive(Clone)]
pub struct Shutdown {
    receiver: watch::Receiver<bool>,
    sender: watch::Sender<bool>,
    failure: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Shutdown {
    /// Starts watching the shutdown signal; the returned flag flips once.
    #[must_use]
    pub fn watch_signal() -> Self {
        let (sender, receiver) = watch::channel(false);
        let signal = sender.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            let _ = signal.send(true);
        });
        Self {
            receiver,
            sender,
            failure: std::sync::Arc::default(),
        }
    }

    /// A stop flag flipped by the returned sender instead of a signal, so a test can stop the
    /// loops it starts, or let running jobs observe a stop.
    #[cfg(test)]
    #[must_use]
    pub fn manual() -> (Self, watch::Sender<bool>) {
        let (sender, receiver) = watch::channel(false);
        (
            Self {
                receiver,
                sender: sender.clone(),
                failure: std::sync::Arc::default(),
            },
            sender,
        )
    }

    /// Stops every loop and listener sharing this flag after capacity is lost.
    pub fn request(&self) {
        let _ = self.sender.send(true);
    }

    /// Stops the role after a supervised dependency loses capacity, preserving failure status.
    pub fn fail(&self) {
        self.failure
            .store(true, std::sync::atomic::Ordering::Release);
        self.request();
    }

    #[must_use]
    pub fn failed(&self) -> bool {
        self.failure.load(std::sync::atomic::Ordering::Acquire)
    }

    /// True once the process must stop.
    #[must_use]
    pub fn requested(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Resolves when the process must stop.
    pub async fn wait(&mut self) {
        let _ = self.receiver.wait_for(|stopping| *stopping).await;
    }
}

/// Serves `/health/live`, `/health/ready` and `/metrics` for a background role until shutdown.
/// Ready means the database answers.
///
/// # Errors
///
/// The listener cannot bind.
pub async fn serve_health(
    addr: SocketAddr,
    db: Database,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    let router = Router::new()
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .route("/metrics", get(metrics))
        .with_state(db);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "health listener ready");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { shutdown.wait().await })
        .await?;
    Ok(())
}

async fn ready(State(db): State<Database>) -> StatusCode {
    if db.ping().await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Serves `/metrics` alone on `addr` until shutdown, for a role whose probes answer on its own
/// listener. Binding or serving failure returns early; the role's exporter supervisor observes
/// that completion and stops the role so unavailable metrics cannot remain silently healthy.
pub async fn serve_metrics(addr: SocketAddr, mut shutdown: Shutdown) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!(%addr, error = %error, "the metrics listener cannot bind; /metrics is not served");
            return;
        }
    };
    tracing::info!(%addr, "metrics listener ready");
    let router = Router::new().route("/metrics", get(metrics));
    if let Err(error) = axum::serve(listener, router)
        .with_graceful_shutdown(async move { shutdown.wait().await })
        .await
    {
        tracing::warn!(error = %error, "the metrics listener stopped");
    }
}

/// The process's metrics in Prometheus's text format; `404` before telemetry is initialised.
async fn metrics() -> Response {
    match crate::telemetry::exposition() {
        Some(text) => (
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Catch a task panic without exporting the panic payload (which may contain user data).
/// The caller records its unit's terminal outcome or stops its critical role.
pub async fn guarded<T>(name: &'static str, future: impl Future<Output = T>) -> Result<T, ()> {
    use futures_util::FutureExt as _;
    std::panic::AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .map_err(|_| {
            opentelemetry::global::meter("norbelys")
                .u64_counter("norbelys_task_failures_total")
                .build()
                .add(1, &[opentelemetry::KeyValue::new("task", name)]);
            tracing::error!(
                task = name,
                error_code = "task_panic",
                "task panicked; its lease remains fenced"
            );
        })
}

/// Read-only metric loops share the role's supervisor: a panic or early completion makes
/// the role stop instead of keeping a healthy heartbeat with permanently stale gauges.
pub fn spawn_exporter(
    name: &'static str,
    future: impl Future<Output = ()> + Send + 'static,
    shutdown: Shutdown,
) {
    tokio::spawn(async move {
        let result = guarded(name, future).await;
        if result.is_err() || !shutdown.requested() {
            tracing::error!(
                task = name,
                error_code = "exporter_stopped",
                "metric exporter lost capacity"
            );
            shutdown.fail();
        }
    });
}

/// Both futures are critical: an error, panic or unexpected return stops their shared role.
/// Shutdown waits for the surviving future's cleanup, bounded by the role grace period.
pub async fn supervise<A, B>(
    first_name: &'static str,
    first: A,
    second_name: &'static str,
    second: B,
    shutdown: Shutdown,
) -> anyhow::Result<()>
where
    A: Future<Output = anyhow::Result<()>>,
    B: Future<Output = anyhow::Result<()>>,
{
    let first = guarded(first_name, first);
    let second = guarded(second_name, second);
    tokio::pin!(first, second);
    let (name, outcome, cleanup) = tokio::select! {
        outcome = &mut first => {
            let expected = shutdown.requested();
            shutdown.request();
            let cleanup = tokio::time::timeout(std::time::Duration::from_secs(60), &mut second).await;
            (first_name, (expected, outcome), cleanup)
        },
        outcome = &mut second => {
            let expected = shutdown.requested();
            shutdown.request();
            let cleanup = tokio::time::timeout(std::time::Duration::from_secs(60), &mut first).await;
            (second_name, (expected, outcome), cleanup)
        },
    };
    let (expected, result) = outcome;
    result.map_err(|()| anyhow::anyhow!("critical task {name} panicked"))??;
    if !expected {
        anyhow::bail!("critical task {name} stopped unexpectedly");
    }
    if shutdown.failed() {
        anyhow::bail!("a supervised exporter lost capacity");
    }
    cleanup
        .map_err(|_| anyhow::anyhow!("critical task cleanup exceeded its grace"))?
        .map_err(|()| anyhow::anyhow!("critical task cleanup panicked"))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_early_critical_return_requests_shutdown_and_fails_the_role() {
        let (shutdown, _) = Shutdown::manual();
        let mut sibling = shutdown.clone();
        let result = supervise(
            "failed",
            async { Ok(()) },
            "sibling",
            async move {
                sibling.wait().await;
                Ok(())
            },
            shutdown.clone(),
        )
        .await;
        assert!(result.is_err());
        assert!(shutdown.requested());
    }

    #[tokio::test]
    async fn a_critical_panic_is_observed_and_stops_its_sibling() {
        let (shutdown, _) = Shutdown::manual();
        let mut sibling = shutdown.clone();
        let result = supervise(
            "panic",
            async { panic!("synthetic failure") },
            "sibling",
            async move {
                sibling.wait().await;
                Ok(())
            },
            shutdown.clone(),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("panicked"));
        assert!(shutdown.requested());
    }

    #[tokio::test]
    async fn expected_shutdown_waits_for_both_futures_without_a_false_failure() {
        let (shutdown, _) = Shutdown::manual();
        shutdown.request();
        assert!(
            supervise("one", async { Ok(()) }, "two", async { Ok(()) }, shutdown)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn an_exporter_panic_stops_the_role_with_failure_status() {
        let (shutdown, _) = Shutdown::manual();
        let mut stopped = shutdown.clone();
        spawn_exporter(
            "fixture",
            async { panic!("private payload") },
            shutdown.clone(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), stopped.wait())
            .await
            .unwrap();
        assert!(shutdown.failed());
        assert!(
            supervise("one", async { Ok(()) }, "two", async { Ok(()) }, shutdown)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_exporter_return_is_failure_only_before_requested_shutdown() {
        let (unexpected, _) = Shutdown::manual();
        let mut stopped = unexpected.clone();
        spawn_exporter("fixture", async {}, unexpected.clone());
        tokio::time::timeout(std::time::Duration::from_secs(1), stopped.wait())
            .await
            .unwrap();
        assert!(unexpected.failed());

        let (expected, _) = Shutdown::manual();
        expected.request();
        let (sent, received) = tokio::sync::oneshot::channel();
        spawn_exporter(
            "fixture",
            async {
                sent.send(()).unwrap();
            },
            expected.clone(),
        );
        received.await.unwrap();
        tokio::task::yield_now().await;
        assert!(!expected.failed());
    }
}
