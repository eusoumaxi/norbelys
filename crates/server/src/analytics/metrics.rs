//! Public metrics for all message kinds. Event counts and rates read only the daily rollup;
//! usage delegates to the existing current-month ledger reader. No request scans raw delivery
//! or tracking facts. Filters share the rollup's frozen connection, kind and campaign dimensions.
//!
//! Rates are event counts per 10,000 accepted messages in the same UTC date range, not cohort
//! conversion rates: a late event can belong to a message sent outside that range. A zero
//! denominator returns null. The response includes counts and computed_at so callers can
//! interpret both denominator and freshness rather than treating missing data as zero success.

use super::{Counters, usage::WorkspaceUsage};
use crate::domain::scope::Scope;
use crate::domain::time::{Date, Timestamp};
use crate::domain::{
    analytics,
    ids::{Campaign, Connection, Id},
    messages::Kind,
};
use crate::http::{
    AppState,
    extract::{Json, Query},
};
use crate::identity::authority::Principal;
use crate::problem::{ApiResult, Problem};
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

/// Which metric family to retrieve.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    /// Daily delivery and engagement counters for every message kind.
    #[default]
    Events,
    /// Ratios of events to accepted messages in the selected range.
    Rates,
    /// Current-month sending and AI usage, and current people and connections.
    Usage,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Filters {
    family: Option<Family>,
    from: Option<Date>,
    to: Option<Date>,
    connection_id: Option<Id<Connection>>,
    campaign_id: Option<Id<Campaign>>,
    kind: Option<Kind>,
}

/// Ratios per 10,000 accepted messages; null means no denominator, never zero success.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Rates {
    /// Recipient-server acceptance per 10,000 sends.
    pub delivered: Option<i64>,
    /// Permanent failures per 10,000 sends.
    pub bounced: Option<i64>,
    /// Human unique opens per 10,000 sends.
    pub opened: Option<i64>,
    /// Human unique clicks per 10,000 sends.
    pub clicked: Option<i64>,
    /// Human replies per 10,000 sends.
    pub replied: Option<i64>,
    /// Unsubscribes per 10,000 sends.
    pub unsubscribed: Option<i64>,
    /// Complaints per 10,000 sends.
    pub complained: Option<i64>,
}
impl Rates {
    fn from_counts(counts: Counters) -> Self {
        let rate = |value: i64| analytics::rate_per_10000(value, counts.sent);
        Self {
            delivered: rate(counts.delivered),
            bounced: rate(counts.bounced),
            opened: rate(counts.opened),
            clicked: rate(counts.clicked),
            replied: rate(counts.replied),
            unsubscribed: rate(counts.unsubscribed),
            complained: rate(counts.complained),
        }
    }
}

/// Daily event history, retained independently of the message rows.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct MetricDay {
    /// UTC day when the events were recorded.
    pub day: Date,
    /// Counters for the requested filters.
    pub counters: Counters,
}

/// Counts and optional rates for a range, or current workspace usage.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum Metrics {
    /// Daily delivery and engagement, across all message kinds by default.
    Events {
        /// The requested family: events or rates.
        family: Family,
        /// Inclusive first UTC day.
        from: Date,
        /// Inclusive last UTC day.
        to: Date,
        /// Counts over the whole range.
        totals: Counters,
        /// Daily counts, at most 366 entries.
        #[schema(max_items = 366)]
        data: Vec<MetricDay>,
        /// Present only for the rates family; values are per 10,000 sends.
        #[serde(skip_serializing_if = "Option::is_none")]
        rates: Option<Rates>,
        /// How far the rollup has processed; null before its first run.
        computed_at: Option<Timestamp>,
    },
    /// Current-month usage without historical filters.
    Usage {
        /// Always usage.
        family: Family,
        /// The same usage visible on the workspace resource.
        usage: WorkspaceUsage,
    },
}

/// Public customer metrics; distinct from the deployment's Prometheus scrape endpoint.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(retrieve))
}

/// Retrieve daily counts, rates or current workspace usage.
#[utoipa::path(get,path = "/metrics",operation_id = "metrics.retrieve",tag = "Reports",
    params(("family"=Option<Family>,Query,description="events (default), rates, or usage."),
        ("from"=Option<Date>,Query,description="Inclusive first UTC date; default 29 days before to."),
        ("to"=Option<Date>,Query,description="Inclusive last UTC date; default today. Maximum range: 366 days."),
        ("connection_id"=Option<Id<Connection>>,Query,description="Filter events or rates by connection."),
        ("campaign_id"=Option<Id<Campaign>>,Query,description="Filter events or rates by campaign."),
        ("kind"=Option<Kind>,Query,description="campaign, direct, reply or transactional.")),
    responses((status=200,description="Metrics and their freshness.",body=Metrics),(status=401,description="No valid credential."),(status=403,description="Requires analytics:read."),(status=422,description="Invalid dates, range, or filters for usage.")),security(("bearer"=[])))]
async fn retrieve(
    principal: Principal,
    State(state): State<AppState>,
    Query(query): Query<Filters>,
) -> ApiResult<Json<Metrics>> {
    principal.require(Scope::AnalyticsRead)?;
    let family = query.family.unwrap_or_default();
    let mut tx = state.db.begin_in(principal.workspace).await?;
    if matches!(family, Family::Usage) {
        if query.from.is_some()
            || query.to.is_some()
            || query.connection_id.is_some()
            || query.campaign_id.is_some()
            || query.kind.is_some()
        {
            return Err(Problem::invalid_field(
                "?family",
                "filters",
                "Usage describes the current workspace and month; date and message filters apply only to events and rates.",
            ));
        }
        let usage = super::usage::read(&mut tx, principal.workspace, crate::process::now()).await?;
        tx.commit().await?;
        return Ok(Json(Metrics::Usage { family, usage }));
    }
    let (from, to) = analytics::range(query.from, query.to, crate::process::now())
        .map_err(|error| Problem::invalid_field("?from", "range", error.to_string()))?;
    let rows:Vec<(Date,String,i64)>=sqlx::query_as("SELECT day,metric,sum(value)::bigint FROM message_daily_stats WHERE workspace_id=$1 AND day BETWEEN $2 AND $3 AND ($4::uuid IS NULL OR connection_id=$4) AND ($5::uuid IS NULL OR campaign_id=$5) AND ($6::text IS NULL OR message_kind=$6) GROUP BY day,metric ORDER BY day,metric")
        .bind(principal.workspace.uuid()).bind(from).bind(to).bind(query.connection_id.map(|id|id.uuid())).bind(query.campaign_id.map(|id|id.uuid())).bind(query.kind.map(Kind::as_str)).fetch_all(&mut *tx).await?;
    let computed_at = super::computed_at(&mut tx).await?;
    tx.commit().await?;
    let mut totals = Counters::default();
    let mut days = std::collections::BTreeMap::<Date, Counters>::new();
    for (day, metric, value) in rows {
        add(&mut totals, &metric, value);
        add(days.entry(day).or_default(), &metric, value);
    }
    let data = days
        .into_iter()
        .map(|(day, counters)| MetricDay { day, counters })
        .collect();
    let rates = matches!(family, Family::Rates).then(|| Rates::from_counts(totals));
    Ok(Json(Metrics::Events {
        family,
        from,
        to,
        totals,
        data,
        rates,
        computed_at,
    }))
}

fn add(counts: &mut Counters, metric: &str, value: i64) {
    let target = match metric {
        "sent" => &mut counts.sent,
        "delivered" => &mut counts.delivered,
        "bounced" => &mut counts.bounced,
        "opened" => &mut counts.opened,
        "clicked" => &mut counts.clicked,
        "replied" => &mut counts.replied,
        "unsubscribed" => &mut counts.unsubscribed,
        "complained" => &mut counts.complained,
        _ => return,
    };
    *target = target.saturating_add(value);
}
