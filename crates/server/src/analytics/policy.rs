//! Policy refusals are delivery evidence, independent of submission state. Count each message
//! once and keep later delivery separate from unresolved or terminal failures. Account-level
//! restrictions cannot be used as evidence that a variant's content caused the refusal.

use serde::Serialize;
use sqlx::FromRow;
use uuid::Uuid;

use crate::db::Tx;
use crate::domain::analytics::GroupBy;
use crate::domain::ids::{Campaign, Id, Step, Variant, WorkspaceId};
use crate::domain::time::{Date, Timestamp};

/// Distinct messages, never the number of repeated provider notifications.
#[derive(Debug, Clone, Default, Serialize, utoipa::ToSchema, FromRow)]
pub struct PolicyCounts {
    /// Messages with at least one policy refusal; includes recovered deliveries.
    pub affected: i64,
    /// A policy refusal with neither confirmed delivery nor a terminal refusal.
    pub pending: i64,
    /// A policy refusal followed or accompanied by confirmed delivery.
    pub recovered: i64,
    /// A terminal bounce or rejection without confirmed delivery.
    pub failed: i64,
    /// Messages with a recognised account restriction (currently JFE050005).
    /// Other policy refusals have unknown scope, not necessarily content problems.
    pub account_restricted: i64,
}

/// Policy counts for the same grouping as the campaign report.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema, FromRow)]
pub struct PolicyGroup {
    pub day: Option<Date>,
    #[schema(value_type = Option<String>)]
    pub campaign_id: Option<Id<Campaign>>,
    #[schema(value_type = Option<String>)]
    pub step_id: Option<Id<Step>>,
    #[schema(value_type = Option<String>)]
    pub variant_id: Option<Id<Variant>>,
    pub variant_version: Option<i32>,
    #[sqlx(flatten)]
    pub counts: PolicyCounts,
}

/// Current retained evidence, not the delayed daily rollup. Archived messages are excluded.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PolicyReport {
    pub totals: PolicyCounts,
    pub data: Vec<PolicyGroup>,
    pub has_more: bool,
    pub computed_at: Timestamp,
}

/// One aggregate row; the grand total precedes the bounded list of groups.
#[derive(FromRow)]
struct Row {
    grouped: bool,
    #[sqlx(flatten)]
    group: PolicyGroup,
}

/// Read one campaign's retained message evidence using the campaign and message indexes.
/// The range selects message submission days; later evidence still settles those messages.
pub(super) async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    campaign: Option<Uuid>,
    step: Option<Uuid>,
    from: Date,
    to: Date,
    group_by: Option<GroupBy>,
) -> Result<PolicyReport, sqlx::Error> {
    let rows = sqlx::query_as::<_, Row>(
        r#"WITH facts AS (
            SELECT m.id, m.campaign_id, m.step_id, m.variant_id, m.variant_version,
                   (coalesce(m.sent_at, m.created_at) AT TIME ZONE 'UTC')::date AS day,
                   bool_or(e.category = 'policy' AND e.kind IN ('deferred', 'rejected', 'bounced')) AS affected,
                   bool_or(e.kind = 'delivered') AS delivered,
                   bool_or(e.kind IN ('bounced', 'rejected')) AS failed,
                   bool_or(e.category = 'policy' AND e.diagnostic LIKE '%JFE050005%') AS account_restricted
              FROM messages m
              JOIN delivery_events e ON e.workspace_id = m.workspace_id AND e.message_id = m.id
             WHERE m.workspace_id = $1 AND m.campaign_id = $2
               AND ($3::uuid IS NULL OR m.step_id = $3)
               AND (coalesce(m.sent_at, m.created_at) AT TIME ZONE 'UTC')::date BETWEEN $4 AND $5
             GROUP BY m.id, m.campaign_id, m.step_id, m.variant_id, m.variant_version, m.sent_at, m.created_at
        ), dimensions AS (
            SELECT CASE WHEN $6 = 'day' THEN day END AS day,
                   CASE WHEN $6 IN ('campaign', 'step', 'variant') THEN campaign_id END AS campaign_id,
                   CASE WHEN $6 IN ('step', 'variant') THEN step_id END AS step_id,
                   CASE WHEN $6 = 'variant' THEN variant_id END AS variant_id,
                   CASE WHEN $6 = 'variant' THEN variant_version END AS variant_version,
                   delivered, failed, account_restricted
              FROM facts WHERE affected
        )
        SELECT grouping(day) = 0 AS grouped, day, campaign_id, step_id, variant_id, variant_version,
               count(*)::bigint AS affected,
               count(*) FILTER (WHERE NOT delivered AND NOT failed)::bigint AS pending,
               count(*) FILTER (WHERE delivered)::bigint AS recovered,
               count(*) FILTER (WHERE NOT delivered AND failed)::bigint AS failed,
               count(*) FILTER (WHERE account_restricted)::bigint AS account_restricted
          FROM dimensions
         GROUP BY GROUPING SETS ((day, campaign_id, step_id, variant_id, variant_version), ())
         ORDER BY grouped, day, campaign_id, step_id, variant_id, variant_version
         LIMIT $7"#,
    )
    .bind(workspace.uuid())
    .bind(campaign)
    .bind(step)
    .bind(from)
    .bind(to)
    .bind(group_by.map(<&'static str>::from))
    .bind(super::http::GROUPS + 2)
    .fetch_all(&mut **tx)
    .await?;
    let mut totals = PolicyCounts::default();
    let mut data = Vec::new();
    for row in rows {
        if row.grouped {
            if group_by.is_some() {
                data.push(row.group);
            }
        } else {
            totals = row.group.counts;
        }
    }
    let has_more = data.len() > usize::try_from(super::http::GROUPS).unwrap_or(usize::MAX);
    data.truncate(usize::try_from(super::http::GROUPS).unwrap_or(usize::MAX));
    Ok(PolicyReport {
        totals,
        data,
        has_more,
        computed_at: crate::process::now(),
    })
}
