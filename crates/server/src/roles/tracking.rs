//! The tracking role: answers the open pixels and click links of campaign mail from their signed
//! tokens, with no database on the request path, and drains the events into PostgreSQL in the
//! same process.
//!
//! - **The spool** (`TRACKING_SPOOL_DIR`) is where every event is written before its request is
//!   answered; it lives on a disk that survives restarts, and a `development` deployment without
//!   one keeps it under the system's temporary directory.
//! - **The pool** (`DATABASE_URL`, the `norbelys_tracking` login, two connections) serves the drain
//!   alone. It connects lazily: the role starts and answers while the database is unavailable,
//!   and the drain catches up once it is back.
//! - **The listener** (`HTTP_ADDR`) serves `/t/o/{token}`, `/t/c/{token}`, the brand mark of the
//!   platform's own mail (`/brand/v1/email-mark.png`, from the binary), `/health/live` and
//!   `/health/ready` (`tracking::http`). Unsubscribes and uploaded images are the api's: they
//!   write to the database or read the object store, which this role never does.
//! - **Stopping** (`SIGTERM`): the listener stops taking requests and finishes those in flight
//!   (their events are already on disk), the drain stores what it can within [`DRAIN_GRACE`], and
//!   the spool is checkpointed and closed. Whatever is left stays on disk for the next start.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context as _;

use crate::config::TrackingArgs;
use crate::db::{Database, PoolSettings};
use crate::process::Shutdown;
use crate::tracking::drain;
use crate::tracking::http::{Tracker, tracker};
use crate::tracking::spool::{Limits, Spool};

/// How long the drain may go on storing events once the role is asked to stop.
const DRAIN_GRACE: Duration = Duration::from_secs(15);

pub async fn run(args: TrackingArgs) -> anyhow::Result<()> {
    let keys = super::role_keys(&args.common, crate::config::KeyRole::Tracking)?;
    let dir = super::spool_dir(
        args.spool_dir.as_deref(),
        &args.common,
        "TRACKING_SPOOL_DIR",
        "norbelys-tracking-spool",
    )?;
    let spool = Spool::open(&dir, Limits::default())
        .with_context(|| format!("cannot open the tracking spool in {}", dir.display()))?;
    let url = args
        .common
        .database_url
        .as_ref()
        .context("DATABASE_URL is required by this role")?;
    let db = Database::connect_lazy(
        url,
        PoolSettings {
            min_connections: 0,
            ..super::background_pool("norbelys-tracking", args.pool_size, Duration::from_secs(10))
        },
    )
    .context("the tracking role's DATABASE_URL is not a database URL")?;
    let shutdown = Shutdown::watch_signal();
    // `/metrics` on the health address: the role's probes answer on its own listener.
    crate::process::spawn_exporter(
        "metrics.listener",
        crate::process::serve_metrics(args.common.health_addr, shutdown.clone()),
        shutdown.clone(),
    );
    let mut draining = tokio::spawn(drain::run(db, spool.clone(), shutdown.clone()));
    let app = tracker(Tracker {
        keys,
        spool: spool.clone(),
        trust_forwarded_for: args.trust_forwarded_for,
        trusted_proxy_ips: args.trusted_proxy_ips,
    });
    let listener = tokio::net::TcpListener::bind(args.http_addr).await?;
    tracing::info!(addr = %args.http_addr, spool = %dir.display(), "tracking listening");
    let mut stop = shutdown.clone();
    let serving = async {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.wait().await })
        .await?;
        Ok(())
    };
    let mut drain_stop = shutdown.clone();
    let drain_task = async {
        tokio::select! {
            joined = &mut draining => joined.map_err(|_| anyhow::anyhow!("spool drain panicked or was cancelled")),
            () = drain_stop.wait() => {
                match tokio::time::timeout(DRAIN_GRACE, &mut draining).await {
                    Ok(joined) => joined.map_err(|_| anyhow::anyhow!("spool drain panicked during shutdown")),
                    Err(_) => Err(anyhow::anyhow!("spool drain exceeded its shutdown grace")),
                }
            }
        }
    };
    let result = crate::process::supervise(
        "http.listener",
        serving,
        "spool.drain",
        drain_task,
        shutdown,
    )
    .await;
    if !draining.is_finished() {
        draining.abort();
        let _ = draining.await;
    }
    spool.close().await;
    result
}
