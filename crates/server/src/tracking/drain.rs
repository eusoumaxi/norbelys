//! The drain: the tracking role's task that moves spooled opens and clicks into PostgreSQL, in
//! batches, through its own small pool and the `norbelys_tracking` login.
//!
//! # One batch, one transaction
//!
//! [`store`] writes a batch in one statement and one transaction:
//!
//! 1. the raw events into `tracking_events`, `ON CONFLICT DO NOTHING` on their key (workspace,
//!    occurrence, the id minted when the request was answered), `RETURNING` only the rows this
//!    transaction inserted;
//! 2. for those rows alone, the per-message rollup `message_engagement`: opens and clicks of
//!    every class, human ones apart, and the first of each, aggregated per message before the
//!    upsert (an upsert may touch a row once per statement);
//! 3. for those rows alone, the campaign counters' increments: `opened` when a message's human
//!    opens go from none to some, `clicked` when its human clicks do (the counters count messages,
//!    not events), on the UTC day the first human event happened. The upsert's `RETURNING` gives
//!    each message's human counts before and after, so this needs no second read. The campaign,
//!    step and variant come from the message's row, which the login may read for exactly that;
//!    mail outside a campaign carries none and is never counted, nor is a message whose row the
//!    archive already moved out.
//!
//! The spool acknowledges the batch only after the commit. A crash between the commit and the
//! acknowledgement drains the batch again: its events conflict on their keys, nothing is
//! inserted, so nothing is rolled up or counted twice. Two transactions storing the same events
//! at once (a drain that timed out after its commit raced by its retry) meet on the same keys:
//! the second waits for the first and inserts nothing; two transactions with different events of
//! one message meet on its rollup row, whose lock orders them, so the second sees the first's
//! counts as its "before" and only one of them counts the message's first human open.
//!
//! A message's rollup lives in a partition of its creation day, archived with the period, so
//! events of a message older than the record window (they can reach the database later than
//! they happened, after an outage) are stored raw and not rolled up: the partition they would
//! update may be gone, and one failing row must not block the batch.
//!
//! # The loop
//!
//! [`run`] takes the oldest events (at most [`BATCH`]), stores them, acknowledges them, and goes
//! on at once while there is more; an empty spool is looked at again every second. A refused
//! batch stays in the spool and is tried again after a backoff (`domain::retry::DRAIN`), so a
//! database outage or a missing month partition shows as a growing spool and lag, never as lost
//! events. When the process is asked to stop, the loop drains what it can until the spool is
//! empty or the database refuses, and returns.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use opentelemetry::metrics::Gauge;
use uuid::Uuid;

use super::spool::{Batch, Event, Spool, SpoolError};
use crate::db::Database;
use crate::domain::retry;
use crate::domain::time::Timestamp;
use crate::domain::tracking::RECORD_WINDOW;
use crate::process::Shutdown;

/// Events stored per transaction at most.
pub const BATCH: usize = 500;
/// How long an empty spool waits before it is looked at again.
const IDLE: Duration = Duration::from_secs(1);

static SPOOL_BYTES: LazyLock<Gauge<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_gauge("norbelys_tracking_spool_bytes")
        .with_unit("By")
        .with_description("Live bytes of the tracking spool: events not yet drained.")
        .build()
});

static DRAIN_LAG: LazyLock<Gauge<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_gauge("norbelys_tracking_drain_lag_seconds")
        .with_unit("s")
        .with_description(
            "Age of the oldest event the drain last took; zero when the spool is empty.",
        )
        .build()
});

/// What storing one batch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stored {
    /// Events this transaction inserted; the rest were stored before.
    pub inserted: u64,
    /// Campaign counter increments written (`opened`, `clicked`).
    pub increments: u64,
}

/// Why a drain pass stopped.
#[derive(Debug, thiserror::Error)]
pub enum DrainError {
    #[error(transparent)]
    Spool(#[from] SpoolError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Stores `events` in one transaction (see the module) through the tracking login, whose
/// policies admit every workspace: the events carry the workspaces their verified tokens named.
///
/// # Errors
///
/// The database refused; nothing of the batch is stored.
pub async fn store(db: &Database, events: &[Event]) -> Result<Stored, sqlx::Error> {
    if events.is_empty() {
        return Ok(Stored {
            inserted: 0,
            increments: 0,
        });
    }
    let workspaces: Vec<Uuid> = events.iter().map(|e| e.workspace.uuid()).collect();
    let ids: Vec<Uuid> = events.iter().map(|e| e.id).collect();
    let messages: Vec<Uuid> = events.iter().map(|e| e.message.uuid()).collect();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let links: Vec<Option<i16>> = events.iter().map(|e| e.link).collect();
    let url_hashes: Vec<Option<Vec<u8>>> = events.iter().map(|e| e.url_hash.clone()).collect();
    let actors: Vec<&str> = events.iter().map(|e| e.actor.as_str()).collect();
    let ip_hashes: Vec<Option<Vec<u8>>> = events.iter().map(|e| e.ip_hash.clone()).collect();
    let user_agents: Vec<Option<String>> = events.iter().map(|e| e.user_agent.clone()).collect();
    let occurred: Vec<Timestamp> = events.iter().map(|e| e.occurred_at).collect();
    let window = i64::try_from(RECORD_WINDOW.as_secs()).unwrap_or(i64::MAX);
    let mut tx = db.begin().await?;
    let row: (i64,i64) = sqlx::query_as(
        r#"WITH incoming AS (
               SELECT * FROM unnest($1::uuid[], $2::uuid[], $3::uuid[], $4::text[], $5::int2[], $6::bytea[],
                                    $7::text[], $8::bytea[], $9::text[], $10::timestamptz[])
                      AS e(workspace_id, id, message_id, kind, link_index, url_hash, actor_class, ip_hash,
                           user_agent, occurred_at)),
           inserted AS (
               INSERT INTO tracking_events (workspace_id, id, message_id, kind, link_index, url_hash, actor_class,
                                            ip_hash, user_agent, occurred_at)
               SELECT workspace_id, id, message_id, kind, link_index, url_hash, actor_class, ip_hash, user_agent,
                      occurred_at
                 FROM incoming
               ON CONFLICT (workspace_id, occurred_at, id) DO NOTHING
               RETURNING workspace_id, message_id, kind, actor_class, occurred_at),
           per_message AS (
               SELECT workspace_id, message_id,
                      min(occurred_at) FILTER (WHERE kind = 'open') AS first_open_at,
                      count(*) FILTER (WHERE kind = 'open') AS opens,
                      min(occurred_at) FILTER (WHERE kind = 'click') AS first_click_at,
                      count(*) FILTER (WHERE kind = 'click') AS clicks,
                      count(*) FILTER (WHERE kind = 'open' AND actor_class = 'human') AS human_opens,
                      count(*) FILTER (WHERE kind = 'click' AND actor_class = 'human') AS human_clicks,
                      min(occurred_at) FILTER (WHERE kind = 'open' AND actor_class = 'human') AS first_human_open_at,
                      min(occurred_at) FILTER (WHERE kind = 'click' AND actor_class = 'human') AS first_human_click_at
                 FROM inserted
                WHERE kind IN ('open', 'click')
                  AND message_id >= uuidv7_boundary(now() - $11::bigint * interval '1 second')
                GROUP BY workspace_id, message_id),
           engaged AS (
               INSERT INTO message_engagement AS g (workspace_id, message_id, first_open_at, opens, first_click_at,
                                                    clicks, human_opens, human_clicks)
               SELECT workspace_id, message_id, first_open_at, opens, first_click_at, clicks, human_opens,
                      human_clicks
                 FROM per_message
               ON CONFLICT (workspace_id, message_id) DO UPDATE
                  SET first_open_at = least(g.first_open_at, excluded.first_open_at),
                      opens = g.opens + excluded.opens,
                      first_click_at = least(g.first_click_at, excluded.first_click_at),
                      clicks = g.clicks + excluded.clicks,
                      human_opens = g.human_opens + excluded.human_opens,
                      human_clicks = g.human_clicks + excluded.human_clicks
               RETURNING new.workspace_id, new.message_id,
                         coalesce(old.human_opens, 0) AS opens_before, new.human_opens AS opens_after,
                         coalesce(old.human_clicks, 0) AS clicks_before, new.human_clicks AS clicks_after),
           firsts AS (
               SELECT e.workspace_id, e.message_id, 'opened' AS metric, p.first_human_open_at AS at
                 FROM engaged e JOIN per_message p USING (workspace_id, message_id)
                WHERE e.opens_before = 0 AND e.opens_after > 0
               UNION ALL
               SELECT e.workspace_id, e.message_id, 'clicked', p.first_human_click_at
                 FROM engaged e JOIN per_message p USING (workspace_id, message_id)
                WHERE e.clicks_before = 0 AND e.clicks_after > 0),
           counted AS (
               INSERT INTO stats_increments (workspace_id, connection_id, message_kind, campaign_id, step_id, step_revision, variant_id,
                                             variant_version, day, metric, delta)
               SELECT m.workspace_id, m.connection_id, m.kind, m.campaign_id, m.step_id, m.step_revision, m.variant_id, m.variant_version,
                      (f.at AT TIME ZONE 'UTC')::date, f.metric, 1
                 FROM firsts f
                 JOIN messages m ON m.workspace_id = f.workspace_id AND m.id = f.message_id
               RETURNING 1)
           SELECT (SELECT count(*) FROM inserted) AS "inserted!", (SELECT count(*) FROM counted) AS "increments!""#,
    ).bind(&workspaces).bind(&ids).bind(&messages).bind(&kinds).bind(&links).bind(&url_hashes).bind(&actors).bind(&ip_hashes).bind(&user_agents).bind(&occurred).bind(window)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Stored {
        inserted: u64::try_from(row.0).unwrap_or(0),
        increments: u64::try_from(row.1).unwrap_or(0),
    })
}

/// Drains `spool` into `db` until the process is asked to stop and the spool is empty (or the
/// database refuses once stopping); see the module.
pub async fn run(db: Database, spool: Spool, mut shutdown: Shutdown) {
    let mut failures: u32 = 0;
    loop {
        let stopping = shutdown.requested();
        let wait = match pass(&db, &spool).await {
            Ok(true) => {
                failures = 0;
                continue;
            }
            Ok(false) if stopping => return,
            Ok(false) => IDLE,
            Err(error) => {
                tracing::error!(error = %error, failures, "the tracking drain could not store a batch");
                if stopping {
                    return;
                }
                let wait = retry::backoff(failures, &retry::DRAIN, crate::jobs::draw());
                failures = failures.saturating_add(1);
                wait
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = shutdown.wait() => {}
        }
    }
}

/// One batch: taken, stored, acknowledged, reported. True when it moved anything (more may wait).
async fn pass(db: &Database, spool: &Spool) -> Result<bool, DrainError> {
    let started = Instant::now();
    let Batch {
        records: events,
        rejected,
    } = spool.take(BATCH).await?;
    let lag = events
        .first()
        .map_or(Duration::ZERO, |(_, oldest)| elapsed(oldest.occurred_at));
    DRAIN_LAG.record(lag.as_secs_f64(), &[]);
    if events.is_empty() {
        SPOOL_BYTES.record(spool.bytes(), &[]);
        return Ok(rejected > 0);
    }
    let (seqs, events): (Vec<i64>, Vec<Event>) = events.into_iter().unzip();
    let stored = store(db, &events).await?;
    spool.ack(seqs).await?;
    let spool_bytes = spool.bytes();
    SPOOL_BYTES.record(spool_bytes, &[]);
    let count = u64::try_from(events.len()).unwrap_or(u64::MAX);
    crate::telemetry::unit(crate::telemetry::Event::TrackingDrain);
    tracing::info!(
        event = "tracking.drain",
        events = count,
        inserted = stored.inserted,
        duplicates = count.saturating_sub(stored.inserted),
        increments = stored.increments,
        rejected,
        spool_bytes,
        lag_ms = u64::try_from(lag.as_millis()).unwrap_or(u64::MAX),
        duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "tracking events drained"
    );
    Ok(true)
}

/// How long ago `at` was; zero for an instant not yet past (another host's clock).
fn elapsed(at: Timestamp) -> Duration {
    crate::process::now()
        .0
        .duration_since(at.0)
        .try_into()
        .unwrap_or(Duration::ZERO)
}
