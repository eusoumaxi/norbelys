//! The rollup, the nightly recount and the operator's rebuild: the only writers of
//! `campaign_daily_stats`.
//!
//! # The drain
//!
//! [`drain`] sums every increment with `processed_to <= id < cutoff` into the counters, with one
//! `INSERT … ON CONFLICT DO UPDATE SET sent = campaign_daily_stats.sent + excluded.sent` per
//! batch, and moves `rollup_watermarks.processed_to` to the cutoff in the same transaction. The
//! cutoff is the UUIDv7 boundary of `now() − 120 s` on the database's clock (the clock that
//! generates increment ids): every role that writes increments has a `transaction_timeout` of
//! at most 60 s, so a transaction holding an id below the cutoff has committed or aborted, and no
//! increment can appear below the watermark after it moved. A crash before the commit loses
//! nothing (the watermark did not move either); an aborted fact never wrote its increment.
//! Each run of the five-minute rollup ends with one `analytics.rollup` event (increments read,
//! counter rows upserted, the new watermark, its lag) and records the lag as
//! `norbelys_analytics_rollup_lag_seconds`.
//!
//! The increments are read without a row lock: the system role holds no `UPDATE` on them (it
//! never writes them), and the cutoff, not a lock, is what makes the sum final. Runs are
//! serialised instead by a transaction-scoped advisory lock and the watermark's row lock, so two
//! workers never count one range twice. Increments of a workspace whose row is gone (erased by
//! `workspace.delete`, which cannot delete increments) are skipped, so a deleted workspace's
//! counters never come back.
//!
//! # The recount
//!
//! `analytics.recount` reconciles each finished UTC day in one transaction under the same lock:
//! it captures a cutoff once, runs the drain through it (so the counters are caught up and the
//! watermark equals the cutoff), sums every online increment of the day below the cutoff,
//! overwrites the day's counters with the sums, marks them `verified`, records the day in
//! `rollup_watermarks` (`campaign_daily_stats.recount`, as the day's boundary) and adds the
//! difference to `norbelys_analytics_recount_drift_total`. Nothing is counted twice: every row
//! it summed is below the watermark the drain just set, and every row above belongs to the
//! ordinary rollup. A day whose first increments leaf is already gone cannot be reconciled: its
//! rows are marked `unverified` instead.
//!
//! # The rebuild
//!
//! `admin analytics rebuild <day>` recomputes a day's counters from the facts (accepted
//! messages, delivery events, human replies, human opens and clicks) under the same lock and
//! overwrites the day: the repair for a day whose increments are gone. It refuses yesterday and
//! today, which the recount and the rollup still own, and a day some of whose facts were already
//! archived, which the database alone would undercount.

use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::WATERMARK;
use crate::db::Tx;
use crate::domain::analytics::{self, ROLLUP_LAG};
use crate::domain::time::{Date, Timestamp};
use crate::jobs::{Class, Effect, Job, JobContext, JobError, Outcome, Queue};

/// The `rollup_watermarks` row where the recount records the last day it reconciled.
pub(crate) const RECOUNTED: &str = "campaign_daily_stats.recount";

static ROLLUP_LAG_SECONDS: LazyLock<Gauge<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_gauge("norbelys_analytics_rollup_lag_seconds")
        .with_unit("s")
        .with_description("How far behind now the counters are: the age of the rollup's watermark.")
        .build()
});

static RECOUNT_DRIFT: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_analytics_recount_drift_total")
        .with_description(
            "Counter units the nightly recount corrected (absolute difference), by metric.",
        )
        .build()
});

static RECOUNT_UNVERIFIED: LazyLock<Gauge<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_gauge("norbelys_analytics_recount_unverified_days")
        .with_description(
            "Days whose counters could not be reconciled: their increments were gone.",
        )
        .build()
});

/// Takes the rollup's lock for the rest of the transaction: the rollup, the recount and the
/// rebuild never run at once.
async fn lock(tx: &mut Tx) -> Result<(), sqlx::Error> {
    sqlx::query!("SELECT pg_advisory_xact_lock(hashtextextended('analytics.rollup', 0))")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// The rollup's cutoff now: the UUIDv7 boundary of the database's `now() − 120 s`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn cutoff(tx: &mut Tx) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT uuidv7_boundary(now() - make_interval(secs => $1)) AS "cutoff!""#,
        ROLLUP_LAG.as_secs_f64(),
    )
    .fetch_one(&mut **tx)
    .await
}

/// What one drain did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drained {
    /// The watermark after the drain.
    pub processed_to: Uuid,
    /// The counter rows inserted or added to.
    pub rows: u64,
    /// The increments in the drained range (those of erased workspaces included, which add to
    /// no counter).
    pub increments: u64,
}

/// Counts every increment from the watermark up to `cutoff` into `campaign_daily_stats` and moves
/// the watermark to `cutoff`, inside `tx` (see the module). A cutoff not past the watermark
/// counts nothing and leaves it. The caller commits.
///
/// # Errors
///
/// The database refused.
pub async fn drain(tx: &mut Tx, cutoff: Uuid) -> Result<Drained, sqlx::Error> {
    lock(tx).await?;
    sqlx::query!(
        "INSERT INTO rollup_watermarks (name, processed_to) VALUES ($1, '00000000-0000-0000-0000-000000000000')
         ON CONFLICT (name) DO NOTHING",
        WATERMARK,
    )
    .execute(&mut **tx)
    .await?;
    let processed_to = sqlx::query_scalar!(
        "SELECT processed_to FROM rollup_watermarks WHERE name = $1 FOR UPDATE",
        WATERMARK,
    )
    .fetch_one(&mut **tx)
    .await?;
    let Some((from, to)) = analytics::drain_range(processed_to, cutoff) else {
        return Ok(Drained {
            processed_to,
            rows: 0,
            increments: 0,
        });
    };
    let increments = sqlx::query_scalar!(
        r#"SELECT count(*) AS "increments!" FROM stats_increments WHERE id >= $1 AND id < $2"#,
        from,
        to,
    )
    .fetch_one(&mut **tx)
    .await?;
    let rows = sqlx::query!(
        r#"INSERT INTO campaign_daily_stats AS s
                (workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version, day,
                 sent, delivered, bounced, opened, clicked, replied, unsubscribed, complained)
           SELECT i.workspace_id, i.campaign_id, i.step_id, i.step_revision, i.variant_id, i.variant_version, i.day,
                  coalesce(sum(i.delta) FILTER (WHERE i.metric = 'sent'), 0),
                  coalesce(sum(i.delta) FILTER (WHERE i.metric = 'delivered'), 0),
                  coalesce(sum(i.delta) FILTER (WHERE i.metric = 'bounced'), 0),
                  coalesce(sum(i.delta) FILTER (WHERE i.metric = 'opened'), 0),
                  coalesce(sum(i.delta) FILTER (WHERE i.metric = 'clicked'), 0),
                  coalesce(sum(i.delta) FILTER (WHERE i.metric = 'replied'), 0),
                  coalesce(sum(i.delta) FILTER (WHERE i.metric = 'unsubscribed'), 0),
                  coalesce(sum(i.delta) FILTER (WHERE i.metric = 'complained'), 0)
             FROM stats_increments i
             JOIN workspaces w ON w.id = i.workspace_id
            WHERE i.id >= $1 AND i.id < $2
              AND i.campaign_id IS NOT NULL AND i.step_id IS NOT NULL AND i.step_revision IS NOT NULL
              AND i.variant_id IS NOT NULL AND i.variant_version IS NOT NULL
            GROUP BY i.workspace_id, i.campaign_id, i.step_id, i.step_revision, i.variant_id, i.variant_version, i.day
           ON CONFLICT (workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version, day) DO UPDATE
              SET sent = s.sent + excluded.sent, delivered = s.delivered + excluded.delivered,
                  bounced = s.bounced + excluded.bounced, opened = s.opened + excluded.opened,
                  clicked = s.clicked + excluded.clicked, replied = s.replied + excluded.replied,
                  unsubscribed = s.unsubscribed + excluded.unsubscribed, complained = s.complained + excluded.complained"#,
        from,
        to,
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();
    drain_message_stats(tx, from, to).await?;
    sqlx::query!(
        "UPDATE rollup_watermarks SET processed_to = $2, updated_at = now() WHERE name = $1",
        WATERMARK,
        to,
    )
    .execute(&mut **tx)
    .await?;
    Ok(Drained {
        processed_to: to,
        rows,
        increments: u64::try_from(increments).unwrap_or(0),
    })
}

/// What one day's recount found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recounted {
    /// The day was reconciled; `drift` is the sum of the absolute corrections per metric, in the
    /// order sent, delivered, bounced, opened, clicked, replied, unsubscribed, complained.
    Verified { drift: [i64; 8] },
    /// The day's increments were already gone: its rows are marked `unverified`.
    Unverified,
}

/// The metrics in the order of [`Recounted::Verified`]'s drift.
const METRICS: [&str; 8] = [
    "sent",
    "delivered",
    "bounced",
    "opened",
    "clicked",
    "replied",
    "unsubscribed",
    "complained",
];

/// Reconciles `day` inside `tx` (see the module): drains through `cutoff` (the job passes
/// [`cutoff`], captured once), then overwrites the day's counters from its online increments
/// below it and records the day. The caller commits.
///
/// # Errors
///
/// The database refused.
pub async fn recount(tx: &mut Tx, day: Date, cutoff: Uuid) -> Result<Recounted, sqlx::Error> {
    drain(tx, cutoff).await?;
    // Increments of a day are written on or after it, so they start in the leaf covering its
    // first instant; once that leaf is dropped the day cannot be summed whole.
    let gone = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM partition_leaves
                           WHERE parent = 'stats_increments' AND lower <= $1::date::timestamp AT TIME ZONE 'UTC'
                             AND $1::date::timestamp AT TIME ZONE 'UTC' < upper AND dropped_at IS NOT NULL) AS "gone!""#,
        day as _,
    )
    .fetch_one(&mut **tx)
    .await?;
    let outcome = if gone {
        sqlx::query!(
            "UPDATE campaign_daily_stats SET verification = 'unverified', verified_at = NULL
              WHERE day = $1 AND verification <> 'verified'",
            day as _,
        )
        .execute(&mut **tx)
        .await?;
        Recounted::Unverified
    } else {
        recount_message_stats(tx, day, cutoff).await?;
        let drift = sqlx::query!(
            r#"WITH recounted AS (
                   SELECT i.workspace_id, i.campaign_id, i.step_id, i.step_revision, i.variant_id, i.variant_version, i.day,
                          coalesce(sum(i.delta) FILTER (WHERE i.metric = 'sent'), 0)::int AS sent,
                          coalesce(sum(i.delta) FILTER (WHERE i.metric = 'delivered'), 0)::int AS delivered,
                          coalesce(sum(i.delta) FILTER (WHERE i.metric = 'bounced'), 0)::int AS bounced,
                          coalesce(sum(i.delta) FILTER (WHERE i.metric = 'opened'), 0)::int AS opened,
                          coalesce(sum(i.delta) FILTER (WHERE i.metric = 'clicked'), 0)::int AS clicked,
                          coalesce(sum(i.delta) FILTER (WHERE i.metric = 'replied'), 0)::int AS replied,
                          coalesce(sum(i.delta) FILTER (WHERE i.metric = 'unsubscribed'), 0)::int AS unsubscribed,
                          coalesce(sum(i.delta) FILTER (WHERE i.metric = 'complained'), 0)::int AS complained
                     FROM stats_increments i
                     JOIN workspaces w ON w.id = i.workspace_id
                    WHERE i.day = $1 AND i.id < $2
                      AND i.campaign_id IS NOT NULL AND i.step_id IS NOT NULL AND i.step_revision IS NOT NULL
                      AND i.variant_id IS NOT NULL AND i.variant_version IS NOT NULL
                    GROUP BY i.workspace_id, i.campaign_id, i.step_id, i.step_revision, i.variant_id, i.variant_version, i.day),
               stored AS (SELECT * FROM campaign_daily_stats WHERE day = $1),
               compared AS (
                   SELECT coalesce(r.workspace_id, s.workspace_id) AS workspace_id, coalesce(r.campaign_id, s.campaign_id) AS campaign_id,
                          coalesce(r.step_id, s.step_id) AS step_id, coalesce(r.step_revision, s.step_revision) AS step_revision,
                          coalesce(r.variant_id, s.variant_id) AS variant_id, coalesce(r.variant_version, s.variant_version) AS variant_version,
                          coalesce(r.sent, 0) AS sent, coalesce(r.delivered, 0) AS delivered, coalesce(r.bounced, 0) AS bounced,
                          coalesce(r.opened, 0) AS opened, coalesce(r.clicked, 0) AS clicked, coalesce(r.replied, 0) AS replied,
                          coalesce(r.unsubscribed, 0) AS unsubscribed, coalesce(r.complained, 0) AS complained,
                          abs(coalesce(r.sent, 0) - coalesce(s.sent, 0)) AS d_sent,
                          abs(coalesce(r.delivered, 0) - coalesce(s.delivered, 0)) AS d_delivered,
                          abs(coalesce(r.bounced, 0) - coalesce(s.bounced, 0)) AS d_bounced,
                          abs(coalesce(r.opened, 0) - coalesce(s.opened, 0)) AS d_opened,
                          abs(coalesce(r.clicked, 0) - coalesce(s.clicked, 0)) AS d_clicked,
                          abs(coalesce(r.replied, 0) - coalesce(s.replied, 0)) AS d_replied,
                          abs(coalesce(r.unsubscribed, 0) - coalesce(s.unsubscribed, 0)) AS d_unsubscribed,
                          abs(coalesce(r.complained, 0) - coalesce(s.complained, 0)) AS d_complained
                     FROM recounted r
                     FULL JOIN stored s USING (workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version)),
               written AS (
                   INSERT INTO campaign_daily_stats AS s
                          (workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version, day,
                           sent, delivered, bounced, opened, clicked, replied, unsubscribed, complained, verification, verified_at)
                   SELECT workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version, $1,
                          sent, delivered, bounced, opened, clicked, replied, unsubscribed, complained, 'verified', now()
                     FROM compared
                   ON CONFLICT (workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version, day) DO UPDATE
                      SET sent = excluded.sent, delivered = excluded.delivered, bounced = excluded.bounced,
                          opened = excluded.opened, clicked = excluded.clicked, replied = excluded.replied,
                          unsubscribed = excluded.unsubscribed, complained = excluded.complained,
                          verification = 'verified', verified_at = now()
                   RETURNING 1)
               SELECT (SELECT count(*) FROM written) AS "written!",
                      coalesce(sum(d_sent), 0)::bigint AS "sent!", coalesce(sum(d_delivered), 0)::bigint AS "delivered!",
                      coalesce(sum(d_bounced), 0)::bigint AS "bounced!", coalesce(sum(d_opened), 0)::bigint AS "opened!",
                      coalesce(sum(d_clicked), 0)::bigint AS "clicked!", coalesce(sum(d_replied), 0)::bigint AS "replied!",
                      coalesce(sum(d_unsubscribed), 0)::bigint AS "unsubscribed!", coalesce(sum(d_complained), 0)::bigint AS "complained!"
                 FROM compared"#,
            day as _,
            cutoff,
        )
        .fetch_one(&mut **tx)
        .await?;
        Recounted::Verified {
            drift: [
                drift.sent,
                drift.delivered,
                drift.bounced,
                drift.opened,
                drift.clicked,
                drift.replied,
                drift.unsubscribed,
                drift.complained,
            ],
        }
    };
    sqlx::query!(
        "INSERT INTO rollup_watermarks (name, processed_to) VALUES ($1, uuidv7_boundary($2::date::timestamp AT TIME ZONE 'UTC'))
         ON CONFLICT (name) DO UPDATE SET processed_to = greatest(rollup_watermarks.processed_to, excluded.processed_to), updated_at = now()",
        RECOUNTED,
        day as _,
    )
    .execute(&mut **tx)
    .await?;
    Ok(outcome)
}

/// The last day the recount reconciled, if any.
///
/// # Errors
///
/// The database is unavailable.
pub async fn last_recounted(tx: &mut Tx) -> Result<Option<Date>, sqlx::Error> {
    let recorded = sqlx::query_scalar!(
        "SELECT processed_to FROM rollup_watermarks WHERE name = $1",
        RECOUNTED,
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(recorded
        .and_then(analytics::uuidv7_instant)
        .map(Date::utc_day))
}

/// Why a rebuild was refused.
#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    /// The day is yesterday or later: the recount and the rollup still own it.
    #[error("{0} is not finished long enough: rebuild days before yesterday")]
    TooRecent(Date),
    /// Some of the day's facts already left the database for the archive: rebuilt from what is
    /// still online, the day would lose counts.
    #[error("{0}'s facts are partly in the archive: the database alone cannot rebuild it")]
    Archived(Date),
    /// The database refused.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Recomputes `day`'s counters from the facts still online and overwrites the day, inside `tx`
/// (see the module); returns how many counter rows the day now has. The caller commits.
///
/// The facts and the day each is counted on mirror the producers of increments: a message is
/// `sent` on the UTC day it was accepted; a delivery event moves its metric on the day it was
/// recorded (a delivery; a recipient refused or bounced for good, for an unknown address, no
/// route or another permanent refusal; an unsubscribe; a complaint); a human reply to a campaign
/// message counts on the day it was received; a message is `opened` (`clicked`) on the day of its
/// first human open (click). Only campaign messages are counted.
///
/// # Errors
///
/// [`RebuildError::TooRecent`] for yesterday or later, [`RebuildError::Archived`] when a period
/// holding some of the day's facts was already archived, or the database refused.
pub async fn rebuild(tx: &mut Tx, day: Date, now: Timestamp) -> Result<u64, RebuildError> {
    if day >= Date::utc_day(now.minus(Duration::from_secs(86_400))) {
        return Err(RebuildError::TooRecent(day));
    }
    // The facts of `day`: messages accepted that day (created up to 7 days before, the furthest a
    // send may be scheduled), events recorded and tracking events observed that day.
    let archived = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM partition_leaves
                           WHERE dropped_at IS NOT NULL
                             AND ((parent = 'messages' AND upper > ($1::date - 7)::timestamp AT TIME ZONE 'UTC'
                                   AND lower < ($1::date + 1)::timestamp AT TIME ZONE 'UTC')
                               OR (parent IN ('delivery_events', 'tracking_events')
                                   AND upper > $1::date::timestamp AT TIME ZONE 'UTC'
                                   AND lower < ($1::date + 1)::timestamp AT TIME ZONE 'UTC'))) AS "archived!""#,
        day as _,
    )
    .fetch_one(&mut **tx)
    .await?;
    if archived {
        return Err(RebuildError::Archived(day));
    }
    let cutoff = cutoff(tx).await?;
    drain(tx, cutoff).await?;
    sqlx::query!("DELETE FROM campaign_daily_stats WHERE day = $1", day as _)
        .execute(&mut **tx)
        .await?;
    let rows = sqlx::query!(
        r#"WITH bounds AS (SELECT $1::date::timestamp AT TIME ZONE 'UTC' AS lo, ($1::date + 1)::timestamp AT TIME ZONE 'UTC' AS hi),
           facts AS (
               SELECT m.workspace_id, m.id AS message_id, 'sent' AS metric
                 FROM messages m, bounds b
                WHERE m.kind = 'campaign' AND m.sent_at >= b.lo AND m.sent_at < b.hi
               UNION ALL
               SELECT e.workspace_id, e.message_id,
                      CASE e.kind WHEN 'delivered' THEN 'delivered' WHEN 'unsubscribed' THEN 'unsubscribed'
                                  WHEN 'complaint' THEN 'complained' ELSE 'bounced' END
                 FROM delivery_events e, bounds b
                WHERE e.message_id IS NOT NULL AND e.created_at >= b.lo AND e.created_at < b.hi
                  AND (e.kind IN ('delivered', 'unsubscribed', 'complaint')
                       OR (e.kind IN ('bounced', 'rejected') AND e.category IN ('invalid_recipient', 'no_route', 'rejected')))
               UNION ALL
               SELECT r.workspace_id, r.message_id, 'replied'
                 FROM inbound_messages r, bounds b
                WHERE r.message_id IS NOT NULL AND r.classification = 'human_reply'
                  AND r.received_at >= b.lo AND r.received_at < b.hi
               UNION ALL
               SELECT t.workspace_id, t.message_id, CASE t.kind WHEN 'open' THEN 'opened' ELSE 'clicked' END
                 FROM (SELECT workspace_id, message_id, kind, min(occurred_at) AS first_at
                         FROM tracking_events WHERE actor_class = 'human' AND kind IN ('open', 'click')
                        GROUP BY workspace_id, message_id, kind) t, bounds b
                WHERE t.first_at >= b.lo AND t.first_at < b.hi)
           INSERT INTO campaign_daily_stats
                  (workspace_id, campaign_id, step_id, step_revision, variant_id, variant_version, day,
                   sent, delivered, bounced, opened, clicked, replied, unsubscribed, complained, verification, verified_at)
           SELECT m.workspace_id, m.campaign_id, m.step_id, m.step_revision, m.variant_id, m.variant_version, $1,
                  count(*) FILTER (WHERE f.metric = 'sent'), count(*) FILTER (WHERE f.metric = 'delivered'),
                  count(*) FILTER (WHERE f.metric = 'bounced'), count(*) FILTER (WHERE f.metric = 'opened'),
                  count(*) FILTER (WHERE f.metric = 'clicked'), count(*) FILTER (WHERE f.metric = 'replied'),
                  count(*) FILTER (WHERE f.metric = 'unsubscribed'), count(*) FILTER (WHERE f.metric = 'complained'),
                  'verified', now()
             FROM facts f
             JOIN messages m ON m.workspace_id = f.workspace_id AND m.id = f.message_id AND m.kind = 'campaign'
             JOIN workspaces w ON w.id = m.workspace_id
            GROUP BY m.workspace_id, m.campaign_id, m.step_id, m.step_revision, m.variant_id, m.variant_version"#,
        day as _,
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(rows)
}

/// `analytics.rollup`: the five-minute drain of increments into the counters (see the module).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnalyticsRollup {}

impl Job for AnalyticsRollup {
    const KIND: &'static str = "analytics.rollup";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::Idempotent;
    const CLASS: Class = Class::System;

    fn unique_key(&self) -> Option<String> {
        Some("singleton".to_owned())
    }

    fn schedule() -> Option<(&'static str, Self)> {
        Some(("*/5 * * * *", Self {}))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let started = std::time::Instant::now();
        let mut chunk = cx.begin().await?;
        let cutoff = cutoff(chunk.tx()).await?;
        let drained = drain(chunk.tx(), cutoff).await?;
        cx.checkpoint(chunk, json!({ "rows": drained.rows }))
            .await?;
        let lag = analytics::uuidv7_instant(drained.processed_to)
            .map(|at| crate::process::now().0.duration_since(at.0));
        if let Some(lag) = lag {
            ROLLUP_LAG_SECONDS.record(lag.as_secs_f64().max(0.0), &[]);
        }
        crate::telemetry::unit(crate::telemetry::Event::AnalyticsRollup);
        tracing::info!(
            event = "analytics.rollup",
            increments = drained.increments,
            rows_upserted = drained.rows,
            processed_to = %drained.processed_to,
            lag_ms = lag.and_then(|lag| u64::try_from(lag.as_millis()).ok()),
            duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "analytics.rollup"
        );
        Ok(Outcome::Done)
    }
}

/// `analytics.recount`: the nightly reconciliation of the finished days (see the module), one
/// transaction per day, catching up missed nights.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnalyticsRecount {}

impl Job for AnalyticsRecount {
    const KIND: &'static str = "analytics.recount";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::Idempotent;
    const CLASS: Class = Class::System;

    fn unique_key(&self) -> Option<String> {
        Some("singleton".to_owned())
    }

    fn schedule() -> Option<(&'static str, Self)> {
        Some(("45 0 * * *", Self {}))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let last = {
            let mut chunk = cx.begin().await?;
            last_recounted(chunk.tx()).await?
        };
        for day in analytics::recount_days(last, crate::process::now()) {
            let mut chunk = cx.begin().await?;
            let cutoff = cutoff(chunk.tx()).await?;
            let outcome = recount(chunk.tx(), day, cutoff).await?;
            let unverified = sqlx::query_scalar!(
                r#"SELECT count(DISTINCT day) AS "days!" FROM campaign_daily_stats WHERE verification = 'unverified'"#
            )
            .fetch_one(&mut **chunk.tx())
            .await?;
            cx.checkpoint(chunk, json!({ "recounted": day })).await?;
            RECOUNT_UNVERIFIED.record(u64::try_from(unverified).unwrap_or(0), &[]);
            match outcome {
                Recounted::Verified { drift } => {
                    for (metric, units) in METRICS.into_iter().zip(drift) {
                        RECOUNT_DRIFT.add(
                            u64::try_from(units).unwrap_or(0),
                            &[KeyValue::new("metric", metric)],
                        );
                    }
                    tracing::info!(day = %day, drift = drift.iter().sum::<i64>(), "analytics.recount");
                }
                Recounted::Unverified => {
                    tracing::warn!(day = %day, "analytics.recount: the day's increments are gone; its counters are unverified");
                }
            }
        }
        Ok(Outcome::Done)
    }
}

/// Adds every message kind to the shared rollup transaction and watermark. Dimensions are
/// frozen on each increment; an archived message is never needed to interpret its counters.
async fn drain_message_stats(tx: &mut Tx, from: Uuid, to: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO message_daily_stats AS s (workspace_id,day,connection_id,message_kind,campaign_id,metric,value) SELECT i.workspace_id,i.day,i.connection_id,i.message_kind,i.campaign_id,i.metric,sum(i.delta) FROM stats_increments i JOIN workspaces w ON w.id=i.workspace_id WHERE i.id >= $1 AND i.id < $2 GROUP BY i.workspace_id,i.day,i.connection_id,i.message_kind,i.campaign_id,i.metric ON CONFLICT (workspace_id,day,connection_id,message_kind,campaign_id,metric) DO UPDATE SET value=s.value+excluded.value")
        .bind(from).bind(to).execute(&mut **tx).await?;
    Ok(())
}

/// Replaces a finished day's general metrics from the same complete increment stream used
/// by the campaign recount, after draining to the locked cutoff.
async fn recount_message_stats(tx: &mut Tx, day: Date, cutoff: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM message_daily_stats WHERE day=$1")
        .bind(day)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO message_daily_stats (workspace_id,day,connection_id,message_kind,campaign_id,metric,value) SELECT i.workspace_id,i.day,i.connection_id,i.message_kind,i.campaign_id,i.metric,sum(i.delta) FROM stats_increments i JOIN workspaces w ON w.id=i.workspace_id WHERE i.day=$1 AND i.id < $2 GROUP BY i.workspace_id,i.day,i.connection_id,i.message_kind,i.campaign_id,i.metric")
        .bind(day).bind(cutoff).execute(&mut **tx).await?;
    Ok(())
}
