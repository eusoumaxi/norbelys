//! Counters and the data lifecycle: the rollup that turns increments into campaign counters, the
//! reads that serve them (`GET /analytics`, a campaign's `stats`, a workspace's `usage`), and the
//! system jobs that keep the database to its online window (the archive of old partitions to
//! Parquet, the pruning of short-lived rows, the erasure of deleted workspaces).
//!
//! # Facts and counters
//!
//! Facts are rows (attempts, delivery events, inbound messages, tracking events); counters are
//! derived. Each counted fact writes one `stats_increments` row in its own transaction, and
//! `campaign_daily_stats` has one writer, the rollup ([`rollup`]): every five minutes it sums
//! the increments below a lagged UUIDv7 cutoff into the counters and advances its watermark in
//! the same transaction, and every night the recount reconciles the previous day from the
//! increments and marks it verified. Counters are therefore at most about seven minutes behind,
//! and every read of them carries `computed_at`, the instant the rollup last advanced.
//!
//! # Lifecycle
//!
//! The time-partitioned tables hold a period per leaf. Once a leaf's period is older than its
//! table's retention, `archive.export` ([`archive`]) detaches it, seals it, exports it to Parquet
//! in object storage when the table is archived, verifies the object and drops the leaf;
//! `retention.prune` ([`retention`]) deletes the expired rows of the unpartitioned short-lived
//! tables; `workspace.delete` ([`deletion`]) erases a workspace once its 30-day tombstone has
//! passed. They are system kinds: they run on the maintenance lane through the worker's
//! `norbelys_system` pool, the one background path that reads every workspace's rows.

pub mod archive;
pub mod deletion;
pub mod http;
pub mod metrics;
pub mod parquet;
mod policy;
pub mod retention;
pub mod rollup;
#[cfg(feature = "analytics")]
pub(crate) mod snapshots;
#[cfg(test)]
mod tests;
pub mod usage;

use std::collections::HashMap;

use serde::Serialize;
use uuid::Uuid;

use crate::db::Tx;
use crate::domain::ids::WorkspaceId;
use crate::domain::time::Timestamp;

/// The name of the rollup's watermark row in `rollup_watermarks`.
pub(crate) const WATERMARK: &str = "campaign_daily_stats";

/// The counters of a campaign, a step, a variant or a day, summed from `campaign_daily_stats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct Counters {
    /// Messages a provider accepted.
    pub sent: i64,
    /// Messages the recipient's server took.
    pub delivered: i64,
    /// Messages that could not reach their recipient for good.
    pub bounced: i64,
    /// Messages with at least one human open.
    pub opened: i64,
    /// Messages with at least one human click.
    pub clicked: i64,
    /// Messages a person replied to.
    pub replied: i64,
    /// Recipients who unsubscribed.
    pub unsubscribed: i64,
    /// Recipients who complained.
    pub complained: i64,
}

/// When the counters were last advanced by the rollup: the `computed_at` every counter read
/// carries. `None` before the rollup's first run.
///
/// # Errors
///
/// The database is unavailable.
pub async fn computed_at(tx: &mut Tx) -> Result<Option<Timestamp>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM rollup_watermarks WHERE name = $1"#,
        WATERMARK,
    )
    .fetch_optional(&mut **tx)
    .await
}

/// The all-time counters of each of `campaigns` in `workspace` (a campaign without counters is
/// absent from the map), with their `computed_at`: what a campaign's `stats` shows.
///
/// # Errors
///
/// The database is unavailable.
pub async fn campaign_stats(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaigns: &[Uuid],
) -> Result<(HashMap<Uuid, Counters>, Option<Timestamp>), sqlx::Error> {
    let computed_at = computed_at(tx).await?;
    let rows = sqlx::query!(
        r#"SELECT campaign_id, sum(sent)::bigint AS "sent!", sum(delivered)::bigint AS "delivered!",
                  sum(bounced)::bigint AS "bounced!", sum(opened)::bigint AS "opened!",
                  sum(clicked)::bigint AS "clicked!", sum(replied)::bigint AS "replied!",
                  sum(unsubscribed)::bigint AS "unsubscribed!", sum(complained)::bigint AS "complained!"
             FROM campaign_daily_stats WHERE workspace_id = $1 AND campaign_id = ANY($2)
            GROUP BY campaign_id"#,
        workspace.uuid(),
        campaigns,
    )
    .fetch_all(&mut **tx)
    .await?;
    let stats = rows
        .into_iter()
        .map(|row| {
            (
                row.campaign_id,
                Counters {
                    sent: row.sent,
                    delivered: row.delivered,
                    bounced: row.bounced,
                    opened: row.opened,
                    clicked: row.clicked,
                    replied: row.replied,
                    unsubscribed: row.unsubscribed,
                    complained: row.complained,
                },
            )
        })
        .collect();
    Ok((stats, computed_at))
}
