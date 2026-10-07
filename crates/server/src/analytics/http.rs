//! `GET /analytics`: the workspace's campaign counters over a range of UTC days, optionally
//! filtered to one campaign or step and grouped by day, campaign, step or variant.
//!
//! The counters are read from `campaign_daily_stats`, which the rollup keeps at most about seven
//! minutes behind the facts; the answer carries the rollup's `computed_at`. Nothing here scans
//! raw facts: a request reads at most one row per campaign, step, variant and day of the range,
//! the range is at most 366 days, and a grouped answer holds at most [`GROUPS`] groups (with
//! `has_more` when the grouping has more). The optional policy report reads retained delivery
//! evidence for one explicitly selected campaign, independently of the rollup watermark.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use super::Counters;
use crate::db::Tx;
use crate::domain::analytics::{self, GroupBy, RangeError};
use crate::domain::ids::{Campaign, Id, Step, Variant, WorkspaceId};
use crate::domain::scope::Scope;
use crate::domain::time::{Date, Timestamp};
use crate::http::AppState;
use crate::http::extract::Query;
use crate::identity::authority::Principal;
use crate::problem::{ApiResult, Problem};

/// The most groups one answer holds: 2,500, so the variants of the largest campaign (50 steps of
/// 50 variants), one version each, are one answer. A group is at most about 440 bytes (three
/// ids, a version and eight counters), so the answer stays under 1.1 MiB, within the MCP's 2 MiB
/// bound on one answer.
pub const GROUPS: i64 = 2_500;

/// The public routes of this module.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(retrieve))
}

/// The query of `GET /analytics`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnalyticsQuery {
    campaign_id: Option<Id<Campaign>>,
    step_id: Option<Id<Step>>,
    from: Option<Date>,
    to: Option<Date>,
    group_by: Option<GroupBy>,
    #[serde(default)]
    include_policy: bool,
}

/// One group of counters.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct AnalyticsGroup {
    /// The day, when grouped by day.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day: Option<Date>,
    /// The campaign, when grouped by campaign, step or variant.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub campaign_id: Option<Id<Campaign>>,
    /// The step, when grouped by step or variant.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub step_id: Option<Id<Step>>,
    /// The variant, when grouped by variant.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub variant_id: Option<Id<Variant>>,
    /// The variant's version, when grouped by variant: each published version counts apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_version: Option<i32>,
    pub counters: Counters,
}

/// The campaign counters of a range of days.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct AnalyticsObject {
    /// The first day counted (UTC).
    pub from: Date,
    /// The last day counted (UTC), included.
    pub to: Date,
    /// How `data` is grouped; `null` when it is not.
    pub group_by: Option<GroupBy>,
    /// The counters of the whole range and filters.
    pub totals: Counters,
    /// One entry per group, ordered by the grouping; empty when not grouped.
    #[schema(max_items = 2500)]
    pub data: Vec<AnalyticsGroup>,
    /// The grouping has more groups than `data` holds (at most 2,500).
    pub has_more: bool,
    /// How far the rollup had counted when these were read; `null` before its first run.
    pub computed_at: Option<Timestamp>,
    /// Policy refusals from retained delivery evidence; present only with `include_policy=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<super::policy::PolicyReport>,
}

/// Retrieve the workspace's campaign counters.
///
/// Counters per UTC day from the rollup, at most about seven minutes behind; `computed_at` says
/// how far it had counted. The range defaults to the 30 days ending today and covers at most
/// 366 days.
#[utoipa::path(
    get,
    path = "/analytics",
    tag = "Reports",
    operation_id = "analytics.retrieve",
    params(
        ("campaign_id" = Option<Id<Campaign>>, Query, description = "Only this campaign's counters."),
        ("step_id" = Option<Id<Step>>, Query, description = "Only this step's counters."),
        ("from" = Option<String>, Query, description = "The first UTC day (`YYYY-MM-DD`); default 29 days before `to`."),
        ("to" = Option<String>, Query, description = "The last UTC day, included (`YYYY-MM-DD`); default today."),
        ("group_by" = Option<GroupBy>, Query, description = "`day`, `campaign`, `step` or `variant`."),
        ("include_policy" = Option<bool>, Query, description = "Include retained policy evidence for one campaign; requires campaign_id."),
    ),
    responses(
        (status = 200, description = "The counters.", body = AnalyticsObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `analytics:read`."),
        (status = 422, description = "A parameter is invalid, the range is reversed or longer than 366 days."),
    ),
    security(("bearer" = []))
)]
async fn retrieve(
    principal: Principal,
    State(state): State<AppState>,
    Query(query): Query<AnalyticsQuery>,
) -> ApiResult<Json<AnalyticsObject>> {
    principal.require(Scope::AnalyticsRead)?;
    if query.include_policy && query.campaign_id.is_none() {
        return Err(Problem::invalid_field(
            "?campaign_id",
            "required",
            "A campaign is required to include policy evidence.",
        ));
    }
    let (from, to) =
        analytics::range(query.from, query.to, crate::process::now()).map_err(|error| {
            let field = match error {
                RangeError::Reversed => "?from",
                RangeError::TooLong => "?to",
            };
            Problem::invalid_field(field, "invalid", error.to_string())
        })?;
    let ws = principal.workspace;
    let filters = Filters {
        from,
        to,
        campaign: query.campaign_id.map(|id| id.uuid()),
        step: query.step_id.map(|id| id.uuid()),
    };
    let mut tx = state.db.begin_in(ws).await?;
    let computed_at = super::computed_at(&mut tx).await?;
    let totals = groups(&mut tx, ws, &filters, None, 1)
        .await?
        .into_iter()
        .next()
        .map(|group| group.counters)
        .unwrap_or_default();
    let mut data = match query.group_by {
        Some(group_by) => groups(&mut tx, ws, &filters, Some(group_by), GROUPS + 1).await?,
        None => Vec::new(),
    };
    let policy = if query.include_policy {
        Some(
            super::policy::read(
                &mut tx,
                ws,
                filters.campaign,
                filters.step,
                from,
                to,
                query.group_by,
            )
            .await?,
        )
    } else {
        None
    };
    tx.commit().await?;
    let has_more = i64::try_from(data.len()).unwrap_or(i64::MAX) > GROUPS;
    data.truncate(usize::try_from(GROUPS).unwrap_or(usize::MAX));
    Ok(Json(AnalyticsObject {
        from,
        to,
        group_by: query.group_by,
        totals,
        data,
        has_more,
        computed_at,
        policy,
    }))
}

/// The range and filters of one read.
struct Filters {
    from: Date,
    to: Date,
    campaign: Option<Uuid>,
    step: Option<Uuid>,
}

/// The counters of `filters` in groups of `group_by` (one group of everything when `None`), in
/// the grouping's order, at most `limit` of them.
async fn groups(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &Filters,
    group_by: Option<GroupBy>,
    limit: i64,
) -> Result<Vec<AnalyticsGroup>, sqlx::Error> {
    let group = group_by.map(<&'static str>::from);
    let rows = sqlx::query!(
        r#"SELECT CASE WHEN $6 = 'day' THEN day END AS "day: Date",
                  CASE WHEN $6 IN ('campaign', 'step', 'variant') THEN campaign_id END AS "campaign_id: Id<Campaign>",
                  CASE WHEN $6 IN ('step', 'variant') THEN step_id END AS "step_id: Id<Step>",
                  CASE WHEN $6 = 'variant' THEN variant_id END AS "variant_id: Id<Variant>",
                  CASE WHEN $6 = 'variant' THEN variant_version END AS variant_version,
                  coalesce(sum(sent), 0)::bigint AS "sent!", coalesce(sum(delivered), 0)::bigint AS "delivered!",
                  coalesce(sum(bounced), 0)::bigint AS "bounced!", coalesce(sum(opened), 0)::bigint AS "opened!",
                  coalesce(sum(clicked), 0)::bigint AS "clicked!", coalesce(sum(replied), 0)::bigint AS "replied!",
                  coalesce(sum(unsubscribed), 0)::bigint AS "unsubscribed!", coalesce(sum(complained), 0)::bigint AS "complained!"
             FROM campaign_daily_stats
            WHERE workspace_id = $1 AND day BETWEEN $2 AND $3
              AND ($4::uuid IS NULL OR campaign_id = $4) AND ($5::uuid IS NULL OR step_id = $5)
            GROUP BY 1, 2, 3, 4, 5
            ORDER BY 1, 2, 3, 4, 5
            LIMIT $7"#,
        workspace.uuid(),
        filters.from as _,
        filters.to as _,
        filters.campaign,
        filters.step,
        group,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| AnalyticsGroup {
            day: row.day,
            campaign_id: row.campaign_id,
            step_id: row.step_id,
            variant_id: row.variant_id,
            variant_version: row.variant_version,
            counters: Counters {
                sent: row.sent,
                delivered: row.delivered,
                bounced: row.bounced,
                opened: row.opened,
                clicked: row.clicked,
                replied: row.replied,
                unsubscribed: row.unsubscribed,
                complained: row.complained,
            },
        })
        .collect())
}
