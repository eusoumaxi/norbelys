//! A workspace's `usage` for the current UTC month: messages sent, people, connections and AI
//! spend, each with its limit, and `computed_at`. It is part of the workspace object
//! (`GET /workspaces/{id}`), not a resource of its own.
//!
//! Sends are read from the per-connection daily ledger (`connection_usage.used`, settled when a
//! provider accepts a message), which counts every message the workspace sent (campaign, direct,
//! reply and transactional alike) at the moment it settles, so the figure is never behind.
//! People and connections are counted as they stand; AI spend is the month's settled cost and
//! the bounds of the calls still running, against the month's budget. The deployment has no
//! billing plan today, so sends, people and connections have no limit (`null`); the AI budget
//! is the one limit enforced.

use serde::Serialize;

use crate::db::Tx;
use crate::domain::ids::WorkspaceId;
use crate::domain::time::{Date, Timestamp};

/// A count and the limit it is held to; `limit` is `null` when nothing limits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct Metered {
    pub used: i64,
    pub limit: Option<i64>,
}

/// The month's AI spend, in micro-dollars (millionths of a US dollar).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct AiSpend {
    /// What settled calls cost.
    pub spent_micros: i64,
    /// The bounds of the calls still running.
    pub reserved_micros: i64,
    /// The month's budget: no call starts that could take spend and reservations past it.
    pub budget_micros: i64,
}

/// A workspace's usage for the current UTC month.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct WorkspaceUsage {
    /// The month counted, as its first day (UTC).
    pub month: Date,
    /// Messages providers accepted this month.
    pub sends: Metered,
    /// People in the workspace.
    pub people: Metered,
    /// Connections that are not archived.
    pub connections: Metered,
    /// AI spend this month.
    pub ai: AiSpend,
    /// When these figures were read.
    pub computed_at: Timestamp,
}

/// Reads `workspace`'s usage at `now`, inside `tx` (the workspace's own transaction).
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    now: Timestamp,
) -> Result<WorkspaceUsage, sqlx::Error> {
    let ai = crate::ai::store::month_usage(tx, workspace, now).await?;
    let counts = sqlx::query!(
        r#"SELECT (SELECT coalesce(sum(used), 0) FROM connection_usage
                    WHERE workspace_id = $1 AND day >= $2 AND day < ($2::date + interval '1 month')::date)::bigint AS "sends!",
                  (SELECT count(*) FROM people WHERE workspace_id = $1) AS "people!",
                  (SELECT count(*) FROM connections WHERE workspace_id = $1 AND status <> 'archived') AS "connections!""#,
        workspace.uuid(),
        ai.month as _,
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(WorkspaceUsage {
        month: ai.month,
        sends: Metered {
            used: counts.sends,
            limit: None,
        },
        people: Metered {
            used: counts.people,
            limit: None,
        },
        connections: Metered {
            used: counts.connections,
            limit: None,
        },
        ai: AiSpend {
            spent_micros: ai.spent_micros,
            reserved_micros: ai.reserved_micros,
            budget_micros: ai.budget_micros,
        },
        computed_at: now,
    })
}
