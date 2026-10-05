//! Lanes: the claim of due jobs and the recovery of expired leases, both inside
//! `SET LOCAL ROLE norbelys_scheduler`, the global view of lanes and lease columns across
//! workspaces that never sees a payload. The worker login is a member of that role without
//! inheriting it, so the global view exists only inside these transactions.
//!
//! A lane is the capacity of one workspace in one queue: a claim locks it, leases at most its
//! free slots (bounded by the worker's free permits), and adds what it leased to `running` in
//! the same transaction; the oldest turn goes first, so workspaces share a queue fairly. Lanes
//! are the runner's one deliberate small contention point.
//!
//! Lock order: the lane row, then the job rows (the same in `jobs/lease.rs`).

use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use uuid::Uuid;

use super::{JobId, Queue, RecoveryHook, Registry};
use crate::db::{self, Database};
use crate::domain::ids::WorkspaceId;
use crate::domain::retry;
use crate::domain::time::Timestamp;

static LANE_WAIT: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_histogram("norbelys_jobs_lane_wait_seconds")
        .with_unit("s")
        .with_description(
            "How long a job waited between being due and being claimed, by queue: the time its \
             lane and the workers' permits held it back.",
        )
        .with_boundaries(vec![
            0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0,
        ])
        .build()
});

/// A job leased by a claim.
#[derive(Debug, Clone)]
pub(super) struct Claimed {
    pub id: Uuid,
    pub workspace: WorkspaceId,
    pub kind: String,
    pub claims: i32,
    pub attempts: i16,
}

/// Leases up to `permits` due jobs of the next lane of `queue` with free slots.
///
/// # Errors
///
/// The database failed; nothing was leased.
pub(super) async fn claim(
    db: &Database,
    queue: Queue,
    owner: &str,
    permits: usize,
) -> Result<Vec<Claimed>, sqlx::Error> {
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    // 1. The lane, locked: capacity is a row, not a count; the oldest turn first.
    let lane = sqlx::query!(
        "SELECT workspace_id, running, max_running FROM job_lanes
          WHERE queue = $1 AND running < max_running
            AND EXISTS (SELECT 1 FROM jobs j WHERE j.workspace_id = job_lanes.workspace_id AND j.queue = $1
                                                AND j.state = 'available' AND j.run_at <= now())
          ORDER BY turn_at LIMIT 1 FOR UPDATE SKIP LOCKED",
        queue.as_str(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(lane) = lane else {
        tx.commit().await?;
        return Ok(Vec::new());
    };
    // 2. At most the lane's free slots, bounded by this worker's free permits.
    let free = i64::from(lane.max_running.saturating_sub(lane.running))
        .min(i64::try_from(permits).unwrap_or(i64::MAX));
    let leased = sqlx::query!(
        "WITH due AS (
             SELECT id FROM jobs WHERE workspace_id = $1 AND queue = $2 AND state = 'available' AND run_at <= now()
              ORDER BY run_at, id LIMIT $3 FOR UPDATE SKIP LOCKED)
         UPDATE jobs j SET state = 'running', lease_owner = $4, lease_expires_at = now() + make_interval(secs => $5),
                claims = j.claims + 1, updated_at = now()
           FROM due WHERE j.id = due.id
         RETURNING j.id, j.workspace_id, j.kind, j.claims, j.attempts,
                   extract(epoch FROM now() - j.run_at)::float8 AS \"waited!\"",
        lane.workspace_id,
        queue.as_str(),
        free,
        owner,
        super::lease::LEASE.as_secs_f64(),
    )
    .fetch_all(&mut *tx)
    .await?;
    // 3. The lane counts what was actually leased, and takes its next turn.
    if !leased.is_empty() {
        sqlx::query!(
            "UPDATE job_lanes SET running = running + $3, turn_at = now() WHERE workspace_id = $1 AND queue = $2",
            lane.workspace_id,
            queue.as_str(),
            i32::try_from(leased.len()).unwrap_or(i32::MAX),
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    for row in &leased {
        LANE_WAIT.record(
            row.waited.max(0.0),
            &[KeyValue::new("queue", queue.as_str())],
        );
    }
    Ok(leased
        .into_iter()
        .map(|row| Claimed {
            id: row.id,
            workspace: WorkspaceId::trusted(row.workspace_id),
            kind: row.kind,
            claims: row.claims,
            attempts: row.attempts,
        })
        .collect())
}

/// One expired lease, recovered.
#[derive(Debug, Clone)]
pub(super) struct Recovered {
    pub id: Uuid,
    pub workspace: WorkspaceId,
    pub kind: String,
    pub queue: String,
    /// `available`, `failed` or `needs_review`.
    pub state: String,
}

/// Recovers jobs whose lease expired (their worker died or stopped renewing): back to `available` after the `JOBS` backoff
/// with one more failed attempt, or `failed` once the attempts are spent; `needs_review` when an
/// external ambiguous effect was marked as started. One transaction per job, each lane first; a
/// kind with a recovery hook in `registry` has it run in that transaction, in the job's
/// workspace, so what the lost claim reserved is settled before the job can run again.
///
/// # Errors
///
/// The database failed while reading the expired leases.
pub(super) async fn recover(
    db: &Database,
    registry: &Registry,
    draw: impl Fn() -> u64,
) -> Result<Vec<Recovered>, sqlx::Error> {
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    let expired = sqlx::query!(
        "SELECT id, workspace_id, queue, kind, claims, attempts FROM jobs
          WHERE state = 'running' AND lease_expires_at < now()
          ORDER BY lease_expires_at LIMIT 100"
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let mut recovered = Vec::with_capacity(expired.len());
    for job in expired {
        let failures = u32::try_from(job.attempts).unwrap_or_default();
        let after = retry::backoff(failures, &retry::JOBS, draw());
        let hook = registry.get(&job.kind).and_then(|kind| kind.recovery_hook);
        match recover_one(
            db,
            job.id,
            job.workspace_id,
            &job.queue,
            job.claims,
            after,
            hook,
        )
        .await
        {
            Ok(Some(state)) => recovered.push(Recovered {
                id: job.id,
                workspace: WorkspaceId::trusted(job.workspace_id),
                kind: job.kind,
                queue: job.queue,
                state,
            }),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %error, job = %job.id, "lease recovery failed; the next sweep retries")
            }
        }
    }
    Ok(recovered)
}

async fn recover_one(
    db: &Database,
    id: Uuid,
    workspace: Uuid,
    queue: &str,
    claims: i32,
    after: Duration,
    hook: Option<RecoveryHook>,
) -> Result<Option<String>, sqlx::Error> {
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    sqlx::query_scalar!(
        "SELECT 1 FROM job_lanes WHERE workspace_id = $1 AND queue = $2 FOR UPDATE",
        workspace,
        queue
    )
    .fetch_optional(&mut *tx)
    .await?;
    // A stale marker cannot apply to a later chunk: every checkpoint and conclusion clears it.
    let state = sqlx::query_scalar!(
        "UPDATE jobs SET
                state = CASE WHEN effect_started_at IS NOT NULL THEN 'needs_review'
                             WHEN attempts + 1 >= max_attempts THEN 'failed' ELSE 'available' END,
                run_at = now() + make_interval(secs => $4), attempts = attempts + 1,
                lease_owner = NULL, lease_expires_at = NULL, updated_at = now(),
                last_error = CASE WHEN effect_started_at IS NOT NULL
                                  THEN 'needs_review: the lease expired after the external effect started; an operator resolves it'
                                  ELSE 'lease_expired: the worker stopped renewing the lease' END
          WHERE id = $1 AND workspace_id = $2 AND claims = $3 AND state = 'running' AND lease_expires_at < now()
         RETURNING state",
        id,
        workspace,
        claims,
        after.as_secs_f64(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(state) = state else {
        return Ok(None);
    };
    sqlx::query!(
        "UPDATE job_lanes SET running = running - 1 WHERE workspace_id = $1 AND queue = $2",
        workspace,
        queue
    )
    .execute(&mut *tx)
    .await?;
    if state == "failed" || hook.is_some() {
        // The scheduler cannot write `finished_at` or a kind's rows; the worker can, inside the
        // job's workspace.
        db::reset_role(&mut tx).await?;
        db::set_workspace(&mut tx, WorkspaceId::trusted(workspace)).await?;
    }
    if state == "failed" {
        sqlx::query!(
            "UPDATE jobs SET finished_at = now() WHERE workspace_id = $1 AND id = $2",
            workspace,
            id
        )
        .execute(&mut *tx)
        .await?;
    }
    if let Some(hook) = hook {
        hook(
            &mut tx,
            WorkspaceId::trusted(workspace),
            JobId::from_uuid(id),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Some(state))
}

/// The oldest due job's run time per queue, for the `jobs_backlog_age_seconds` gauge: one
/// index probe per lane rather than a scan of every waiting job.
///
/// # Errors
///
/// The database failed.
pub(super) async fn backlog(db: &Database) -> Result<Vec<(String, Timestamp)>, sqlx::Error> {
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    let rows = sqlx::query!(
        r#"SELECT l.queue, min(j.run_at) AS "oldest!: Timestamp" FROM job_lanes l
           CROSS JOIN LATERAL (SELECT run_at FROM jobs
                                WHERE workspace_id = l.workspace_id AND queue = l.queue AND state = 'available' AND run_at <= now()
                                ORDER BY run_at LIMIT 1) j
           GROUP BY l.queue"#
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.queue, row.oldest))
        .collect())
}
