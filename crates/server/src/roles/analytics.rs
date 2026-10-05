//! Analytics process composition. Snapshot behavior lives with the analytics feature.

use crate::config::AnalyticsArgs;
#[cfg(feature = "analytics")]
use crate::process::{self, Shutdown};
#[cfg(feature = "analytics")]
use crate::storage::Storage;
#[cfg(feature = "analytics")]
use std::time::Duration;

/// Runs the role until the process is asked to stop.
///
/// # Errors
///
/// This build has no analytics engine, or the database or the object store cannot be reached.
pub async fn run(args: AnalyticsArgs) -> anyhow::Result<()> {
    #[cfg(not(feature = "analytics"))]
    {
        let _ = args;
        anyhow::bail!(
            "this build has no analytics engine: the analytics role needs the `analytics` feature (DuckDB)"
        )
    }
    #[cfg(feature = "analytics")]
    {
        let db = super::connect(
            &args.common,
            super::background_pool(
                "norbelys-analytics",
                args.pool_size,
                Duration::from_secs(300),
            ),
        )
        .await?;
        let storage = Storage::from_args(&args.storage, &args.common.environment)?;
        let shutdown = Shutdown::watch_signal();
        let passes = crate::analytics::snapshots::run(&db, &storage, shutdown.clone());
        process::supervise(
            "analytics.pass",
            passes,
            "health",
            process::serve_health(args.common.health_addr, db.clone(), shutdown.clone()),
            shutdown,
        )
        .await
    }
}
