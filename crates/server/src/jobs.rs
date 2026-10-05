//! The generic job runner: durable background work as rows of PostgreSQL.
//!
//! Every unit of background work that is not a delivery attempt or an inbox poll (those keep
//! their own protocols) is one row of `jobs`, with a kind, a queue, a lane, a lease and an
//! effect class. PostgreSQL is the queue: there is no broker, enqueuing is transactional with
//! the business change that asks for the work, and every hand-off is visible in SQL.
//!
//! # How a job moves
//!
//! 1. **Enqueue.** [`enqueue`] inserts the row (`available`, due at `run_at`) inside the
//!    caller's transaction and creates the workspace's lane on first use. After the commit the
//!    caller sends a wake-up with [`wake`] (`NOTIFY` on a separate statement, so a slow
//!    listener never holds business locks); a lost wake-up costs at most one 30-second sweep.
//!    A job enqueued by a request keeps the request's W3C `traceparent` (`trace_parent`), which
//!    its run's span links to; one enqueued by other work keeps none.
//! 2. **Claim.** A worker with a free permit locks the lane of the workspace whose turn is
//!    oldest, leases at most the lane's free slots of its due jobs with
//!    `FOR UPDATE SKIP LOCKED`, and counts them in the lane, in one transaction run as the
//!    `norbelys_scheduler` role, which sees lanes and lease columns but never a payload.
//! 3. **Run.** The worker reads the payload inside the job's workspace and runs the kind in
//!    chunks. Each chunk's effects and its checkpoint (progress plus a renewed lease) commit in
//!    one transaction through [`JobContext::checkpoint`]. After a quantum (200 chunks or 30
//!    seconds) the job yields its slot by rescheduling itself, so long jobs share the lane.
//! 4. **Conclude.** The run's [`Outcome`] (or error) decides the row's next state:
//!    `completed`, `available` again (a retry or a yield), `failed`, `cancelled` or
//!    `needs_review`; the lane slot is released in the same transaction.
//! 5. **Recover.** A lease that expires (the worker died, or stopped renewing) is recovered by
//!    the sweep according to the kind's [`Effect`]: run again later, or wait for an operator
//!    when an ambiguous external effect may have happened.
//!
//! # How a module adds a kind
//!
//! 1. Implement [`Job`] for the payload struct (its fields are the payload; the runner stores
//!    the kind's `VERSION` with it). Register it once in the worker role (`roles/worker.rs`).
//! 2. [`enqueue`] it in the transaction that decided the work, then [`wake`] its queue after
//!    the commit.
//! 3. Work in chunks: [`JobContext::begin`] opens a [`lease::Chunk`], the chunk's writes go through
//!    [`lease::Chunk::tx`], and [`JobContext::checkpoint`] commits them together with the progress,
//!    or answers [`JobError::ClaimLost`] and rolls the chunk back. Never hold a transaction
//!    across a call to the outside: load, commit, call, then record in a chunk.
//! 4. Return [`Outcome::Yield`] when [`JobContext::should_yield`] says so.
//! 5. A kind that reserves something outside its own rows before an external call (an AI call
//!    reserves budget) declares a [`Job::RECOVERY_HOOK`]: the runner runs it in the transaction
//!    that ends every claim of the job, concluded or recovered, so nothing stays reserved.
//!
//! # Invariants
//!
//! - No simultaneous valid ownership: the fence of every write after a claim is
//!   `(lease_owner, claims)`, and `claims` grows with every claim, so a worker that lost its
//!   lease (recovered, perhaps claimed again elsewhere) can record nothing.
//! - A lane's `running` equals its leased jobs: a claim adds what it leased; each claim's one
//!   conclusion, or its recovery, takes one away, under the lane's row lock. The database
//!   refuses `running > max_running`.
//! - A yield is not a failure: `attempts` grows only when a run fails, and `MAX_ATTEMPTS`
//!   failed runs end the job `failed`.
//! - At most one live job per `(workspace, kind, unique_key)`: enqueuing a twin coalesces.
//! - Payloads carry their kind's version; a kind or version this worker does not know ends
//!   `failed` with `unknown_kind` or `unknown_version`, never retried silently.
//!
//! Lock order: the lane row (`job_lanes`), then the job rows. Every path that takes both (the
//! claim, a conclusion, the recovery) takes the lane first, so two paths never wait on each
//! other in opposite orders.

pub mod http;
pub mod kinds;
mod lanes;
pub mod lease;
pub mod runner;
pub mod schedules;
#[cfg(test)]
mod tests;

use std::pin::Pin;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

pub use kinds::Registry;
pub use lease::JobContext;
pub use runner::Runner;

use crate::db::{Database, Tx};
use crate::domain::ids::{self, Id, WorkspaceId};
use crate::domain::time::Timestamp;

/// The id of a job (`job_…` on the wire).
pub type JobId = Id<ids::Job>;

/// The `system` workspace, created with the schema: it owns maintenance work, the fan-out and
/// system kinds, and system connections. System work never belongs to "no workspace".
pub const SYSTEM_WORKSPACE: WorkspaceId =
    WorkspaceId::trusted(Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_0000));

/// The largest payload a job carries, serialized; a larger input is stored elsewhere (an
/// object in the object store) and the payload carries its key.
const PAYLOAD_LIMIT: usize = 64 * 1024;

/// The `NOTIFY` channel of wake-ups; the payload is the queue's name.
pub(crate) const CHANNEL: &str = "norbelys_work";

/// A job queue. Each workspace has one lane per queue it uses, so a busy workspace cannot take
/// every slot of a queue, and the queues isolate kinds of work from each other (a flood of
/// imports never delays webhooks).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub enum Queue {
    /// People imports.
    Imports,
    /// Campaign materialisation, enrollments and their follow-ups.
    Enrollment,
    /// Customer webhook deliveries.
    Webhooks,
    /// Exports and archives.
    Exports,
    /// Checks and housekeeping: the outbox relay, partitions, connection checks.
    Maintenance,
    /// Calls to an AI provider.
    Ai,
    /// Transactional mail of the platform (sign-in codes, invitations).
    Transactional,
    /// Normalisation of provider callbacks.
    Receipts,
}

impl Queue {
    /// The queue as stored in `jobs.queue` and `job_lanes.queue`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The `max_running` of a workspace's new lane: how many of its jobs in this queue run at
    /// once, across every worker.
    #[must_use]
    pub fn lane_size(self) -> i32 {
        match self {
            Self::Imports | Self::Enrollment | Self::Transactional => 4,
            Self::Webhooks | Self::Receipts => 8,
            Self::Ai | Self::Exports => 2,
            Self::Maintenance => 1,
        }
    }

    /// How many jobs of this queue one worker process runs at once unless its configuration
    /// (`JOB_PERMITS`) says otherwise: what the worker's memory and connections allow.
    #[must_use]
    pub fn default_permits(self) -> u32 {
        match self {
            Self::Webhooks => 16,
            Self::Receipts => 8,
            Self::Imports | Self::Enrollment | Self::Transactional => 4,
            Self::Ai | Self::Exports | Self::Maintenance => 2,
        }
    }
}

/// What a job's chunk does to the outside world. A lease cannot fence a remote server, so the
/// effect class selects how an expired lease is recovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Effect {
    /// Chunks only write our database: replaying a chunk is harmless, so recovery runs the job
    /// again.
    Idempotent,
    /// Chunks call outside, and the call is safe to repeat (an idempotency key, a PUT, a DNS
    /// lookup, a Standard Webhooks POST with a stable `webhook-id`): recovery runs it again.
    ExternalRetryable,
    /// Chunks call outside, and a repeat could double an effect. The job calls
    /// [`JobContext::mark_effect_started`] before the call; the checkpoint that records the
    /// outcome clears the mark. A run that ends, or a lease that expires, with the mark set goes
    /// to `needs_review` for an operator instead of running again.
    ExternalAmbiguous,
}

impl Effect {
    /// The effect as a metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Where a kind runs: which database role and which workspaces it may touch. Checked when the
/// kind is registered and every time one of its jobs is claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Class {
    /// Runs as `norbelys_worker` inside the job's own workspace; row security admits only
    /// that workspace's rows.
    Tenant,
    /// A singleton of the `system` workspace that enumerates its own directory of work across
    /// workspaces as the scheduler role ([`JobContext::directory`]) and then opens one tenant
    /// transaction per workspace ([`JobContext::begin_in`]).
    FanOut,
    /// A singleton of the `system` workspace on the maintenance queue that needs every
    /// workspace's rows by period rather than by tenant (partitions, retention, archives). It
    /// runs through the worker's `norbelys_system` pool, the one place a background process
    /// bypasses row security.
    System,
}

/// How a run ends, as the job reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The job is complete.
    Done,
    /// The run failed in a way the job understands (the peer asked to wait, for example); run
    /// again after `after`. Counts as a failed attempt.
    Retry { after: Duration },
    /// The quantum is spent, cancellation was asked for, the process is stopping, or the job
    /// waits for work it does not own; run again after `after` (zero to continue at once).
    /// Never counts as a failure.
    Yield { after: Duration },
    /// Stop for good: the job ends `failed` with the reason, or `cancelled` when cancellation
    /// was asked for.
    Discard { reason: String },
}

/// Why a run stopped early.
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// The lease was lost (recovered, perhaps claimed again): nothing more may be recorded.
    #[error("the job's lease was lost")]
    ClaimLost,
    /// The payload does not decode as the kind's payload; it never will, so the job fails.
    #[error("the payload is not valid for this kind: {0}")]
    InvalidPayload(String),
    /// The database failed; the run is retried with the runner's backoff.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    /// A failure the job describes; the run is retried with the runner's backoff.
    #[error("{0}")]
    Failed(String),
}

impl JobError {
    /// A short, bounded code for the `job.run` event.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ClaimLost => "claim_lost",
            Self::InvalidPayload(_) => "invalid_payload",
            Self::Db(_) => "database",
            Self::Failed(_) => "failed",
        }
    }
}

/// A recovery hook (see [`Job::RECOVERY_HOOK`]): given the transaction that ends a claim, in the
/// job's workspace as the worker, the workspace and the job, it settles what the claim left
/// open. It must be idempotent: a claim ends once, but every claim of the job runs it.
pub type RecoveryHook =
    for<'a> fn(
        &'a mut Tx,
        WorkspaceId,
        JobId,
    ) -> Pin<Box<dyn Future<Output = Result<(), sqlx::Error>> + Send + 'a>>;

/// One kind of durable background work. The implementing struct is the payload: it is
/// serialized into `jobs.payload` when enqueued and deserialized when the job is claimed.
pub trait Job: DeserializeOwned + Serialize + Send + Sync + 'static {
    /// The kind's name, stored in `jobs.kind`; never renamed once published, because queued
    /// rows of an older release must still find their kind.
    const KIND: &'static str;
    /// The queue, and therefore the lane, the kind runs in.
    const QUEUE: Queue;
    /// What a chunk does to the outside world; selects the recovery rule.
    const EFFECT: Effect;
    /// Where the kind runs.
    const CLASS: Class = Class::Tenant;
    /// The payload's version, stored with it. Bump it when the payload changes shape: a
    /// worker that meets a version it does not know fails the job instead of misreading it.
    const VERSION: u16 = 1;
    /// Failed runs before the job ends `failed`; yields never count.
    const MAX_ATTEMPTS: u16 = 10;
    /// The kind's recovery hook, for a kind that reserves something outside its own rows before
    /// an external call (an AI call reserves budget): the runner runs it inside the transaction
    /// that ends every claim of the job, whether the run concluded or its expired lease was
    /// recovered, so whatever the claim left reserved is settled once, before the job can run
    /// again. `None` for kinds that reserve nothing.
    const RECOVERY_HOOK: Option<RecoveryHook> = None;

    /// At most one live (`available` or `running`) job per `(workspace, KIND, unique_key)`:
    /// enqueuing a twin coalesces with the live one and brings a waiting one's run forward.
    fn unique_key(&self) -> Option<String> {
        None
    }

    /// For a periodic kind: its cron expression, read in UTC, and the job each run enqueues
    /// in the `system` workspace. Periodic kinds are fan-out or system singletons with a
    /// unique key, so a slow run coalesces with the next tick instead of piling up.
    fn schedule() -> Option<(&'static str, Self)> {
        None
    }

    /// Runs chunks until done or until it must yield. Each chunk commits its effects and its
    /// checkpoint together through [`JobContext::checkpoint`]; the runner never commits for the
    /// job.
    fn run(self, cx: &mut JobContext) -> impl Future<Output = Result<Outcome, JobError>> + Send;
}

/// A row about to be enqueued, built from a registered kind (the schedule loop uses it).
pub(crate) struct NewJob<'a> {
    pub kind: &'a str,
    pub queue: Queue,
    pub max_attempts: u16,
    pub payload: serde_json::Value,
    pub unique_key: Option<String>,
}

/// Enqueues `job` in `workspace` inside the caller's transaction, so the job exists exactly
/// when the business change that asked for it commits; creates the workspace's lane on first
/// use. `run_at` defaults to now. A job with a live twin (same kind and unique key) coalesces
/// with it: a waiting twin's run is brought forward to `run_at` when that is earlier, and the
/// twin's id is returned.
///
/// # Errors
///
/// The payload does not serialize as a JSON object of at most 64 KiB (`sqlx::Error::Encode`,
/// a programming error), or the database refused the row.
pub async fn enqueue<J: Job>(
    tx: &mut Tx,
    workspace: WorkspaceId,
    job: &J,
    run_at: Option<Timestamp>,
) -> Result<JobId, sqlx::Error> {
    let new = NewJob {
        kind: J::KIND,
        queue: J::QUEUE,
        max_attempts: J::MAX_ATTEMPTS,
        payload: payload_of(job, J::VERSION)?,
        unique_key: job.unique_key(),
    };
    enqueue_value(tx, workspace, &new, run_at).await
}

/// Enqueues many jobs of one kind in one statement, with the coalescing of [`enqueue`]. The
/// unique keys must be distinct within one call (PostgreSQL refuses to update one row twice in
/// a statement). Returns how many rows were inserted or brought forward.
///
/// # Errors
///
/// As [`enqueue`].
pub async fn enqueue_many<J: Job>(
    tx: &mut Tx,
    workspace: WorkspaceId,
    jobs: &[J],
    run_at: Option<Timestamp>,
) -> Result<u64, sqlx::Error> {
    if jobs.is_empty() {
        return Ok(0);
    }
    let payloads = jobs
        .iter()
        .map(|job| payload_of(job, J::VERSION))
        .collect::<Result<Vec<_>, _>>()?;
    let keys: Vec<Option<String>> = jobs.iter().map(Job::unique_key).collect();
    create_lane(tx, workspace, J::QUEUE).await?;
    let affected = sqlx::query!(
        "INSERT INTO jobs (workspace_id, queue, kind, payload, unique_key, max_attempts, run_at, trace_parent)
         SELECT $1, $2, $3, job.payload, job.unique_key, $4, coalesce($5, now()), $8::text
           FROM unnest($6::jsonb[], $7::text[]) AS job(payload, unique_key)
         ON CONFLICT (workspace_id, kind, unique_key) WHERE unique_key IS NOT NULL AND state IN ('available', 'running')
         DO UPDATE SET run_at = least(jobs.run_at, EXCLUDED.run_at) WHERE jobs.state = 'available'",
        workspace.uuid(),
        J::QUEUE.as_str(),
        J::KIND,
        max_attempts(J::MAX_ATTEMPTS),
        run_at as _,
        &payloads,
        &keys as _,
        crate::http::context::trace_parent(),
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(affected)
}

/// [`enqueue`] for a row built from a registered kind rather than from a typed payload.
///
/// # Errors
///
/// As [`enqueue`].
pub(crate) async fn enqueue_value(
    tx: &mut Tx,
    workspace: WorkspaceId,
    new: &NewJob<'_>,
    run_at: Option<Timestamp>,
) -> Result<JobId, sqlx::Error> {
    create_lane(tx, workspace, new.queue).await?;
    let trace_parent = crate::http::context::trace_parent();
    // A running twin is locked but not updated, so the insert returns nothing and the twin is
    // read instead; if it finished between the two statements its key is free and the insert
    // is tried again.
    for _ in 0..3 {
        let inserted = sqlx::query_scalar!(
            r#"INSERT INTO jobs (workspace_id, queue, kind, payload, unique_key, max_attempts, run_at, trace_parent)
               VALUES ($1, $2, $3, $4, $5, $6, coalesce($7, now()), $8)
               ON CONFLICT (workspace_id, kind, unique_key) WHERE unique_key IS NOT NULL AND state IN ('available', 'running')
               DO UPDATE SET run_at = least(jobs.run_at, EXCLUDED.run_at) WHERE jobs.state = 'available'
               RETURNING id AS "id: JobId""#,
            workspace.uuid(),
            new.queue.as_str(),
            new.kind,
            new.payload,
            new.unique_key,
            max_attempts(new.max_attempts),
            run_at as _,
            trace_parent.as_deref(),
        )
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(id) = inserted {
            return Ok(id);
        }
        let running = sqlx::query_scalar!(
            r#"SELECT id AS "id: JobId" FROM jobs
                WHERE workspace_id = $1 AND kind = $2 AND unique_key = $3 AND state IN ('available', 'running')"#,
            workspace.uuid(),
            new.kind,
            new.unique_key,
        )
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(id) = running {
            return Ok(id);
        }
    }
    Err(sqlx::Error::Protocol(format!(
        "the live `{}` job kept changing while it was enqueued",
        new.kind
    )))
}

/// Sends the wake-up for `queue` after the transaction that enqueued work has committed, as a
/// statement of its own: the `NOTIFY` lock is then taken outside the business transaction, and
/// a failure never changes the caller's answer (it is logged; the sweep finds the work anyway).
pub async fn wake(db: &Database, queue: Queue) {
    if let Err(error) = sqlx::query!("SELECT pg_notify($1, $2)", CHANNEL, queue.as_str())
        .execute(db.pool())
        .await
    {
        tracing::warn!(error = %error, queue = queue.as_str(), "wake-up not sent");
    }
}

/// Creates the workspace's lane of `queue` with the queue's size, unless it exists. The
/// insert takes no lock on an existing lane, so enqueuing never waits for a claim.
async fn create_lane(tx: &mut Tx, workspace: WorkspaceId, queue: Queue) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO job_lanes (workspace_id, queue, max_running) VALUES ($1, $2, $3)
         ON CONFLICT (workspace_id, queue) DO NOTHING",
        workspace.uuid(),
        queue.as_str(),
        queue.lane_size(),
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The stored payload: the job's JSON object plus its kind's `version`, at most 64 KiB.
pub(crate) fn payload_of<J: Serialize>(
    job: &J,
    version: u16,
) -> Result<serde_json::Value, sqlx::Error> {
    let mut payload =
        match serde_json::to_value(job).map_err(|error| sqlx::Error::Encode(Box::new(error)))? {
            serde_json::Value::Object(object) => object,
            serde_json::Value::Null => serde_json::Map::new(),
            _ => return Err(sqlx::Error::Encode("a job payload is a JSON object".into())),
        };
    payload.insert("version".to_owned(), serde_json::Value::from(version));
    let payload = serde_json::Value::Object(payload);
    if payload.to_string().len() > PAYLOAD_LIMIT {
        return Err(sqlx::Error::Encode(
            "a job payload is at most 64 KiB; store the input elsewhere and pass its key".into(),
        ));
    }
    Ok(payload)
}

fn max_attempts(value: u16) -> i16 {
    i16::try_from(value).unwrap_or(i16::MAX)
}

/// A uniformly random 64-bit draw for the jitter of a backoff (0 if the system random source
/// fails, which only removes the jitter).
pub(crate) fn draw() -> u64 {
    crate::crypto::random_bytes(8)
        .ok()
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map_or(0, u64::from_le_bytes)
}
