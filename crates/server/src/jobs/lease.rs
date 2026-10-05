//! The lease of a claimed job: the context a job runs in, its fenced writes (checkpoint,
//! heartbeat, effect marker) and its one conclusion.
//!
//! A lease is time-bounded ownership of one job row by one worker: `lease_owner` names the
//! worker process, `lease_expires_at` bounds it (60 seconds, renewed by every checkpoint and
//! heartbeat), and `claims` is its generation, incremented by every claim. Every write here is
//! fenced by `(lease_owner, claims)`: a worker whose lease was recovered, or claimed again in a
//! newer generation, records nothing, and its chunk rolls back whole. A zombie that wakes up
//! late therefore cannot overwrite the work of the worker that replaced it.
//!
//! A chunk is the unit of progress: its effects and its checkpoint commit in one transaction,
//! so after a crash the job resumes from the last committed progress and replays at most one
//! chunk. A quantum (200 chunks or 30 seconds) bounds how long a run holds its lane slot before
//! it yields; cancellation and shutdown are observed at the same chunk boundaries.
//!
//! Lock order: a conclusion locks the lane row, then the job row, as the claim and the
//! recovery do (`jobs/lanes.rs`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use uuid::Uuid;

use super::{Class, Effect, JobError, JobId, Outcome, Queue, RecoveryHook};
use crate::db::{self, Database, Tx};
use crate::domain::ids::WorkspaceId;
use crate::domain::retry;
use crate::process::Shutdown;

/// How long a lease lasts after a claim, a checkpoint or a heartbeat.
pub(crate) const LEASE: Duration = Duration::from_secs(60);
/// A quantum: the chunks, or the time, a run gets before it must yield its lane slot.
const QUANTUM_CHUNKS: u32 = 200;
const QUANTUM_TIME: Duration = Duration::from_secs(30);

/// What fences a claimed job's writes.
#[derive(Debug, Clone)]
pub(crate) struct Lease {
    pub id: Uuid,
    pub workspace: WorkspaceId,
    pub queue: Queue,
    pub owner: Arc<str>,
    pub claims: i32,
}

/// One chunk's transaction. Its effects are written through [`Chunk::tx`]; only
/// [`JobContext::checkpoint`] commits it, under the fence. Dropped, it rolls back.
pub struct Chunk {
    tx: Tx,
}

impl Chunk {
    /// The chunk's transaction, for the chunk's effects.
    pub fn tx(&mut self) -> &mut Tx {
        &mut self.tx
    }
}

/// The context of one run of a claimed job.
pub struct JobContext {
    db: Database,
    system: Database,
    env: Arc<http::Extensions>,
    lease: Lease,
    class: Class,
    effect: Effect,
    progress: Option<Value>,
    shutdown: Shutdown,
    started: Instant,
    chunks: u32,
    cancel: bool,
    effect_started: bool,
}

/// What [`JobContext::new`] needs.
pub(super) struct Parts {
    pub db: Database,
    pub system: Database,
    pub env: Arc<http::Extensions>,
    pub lease: Lease,
    pub class: Class,
    pub effect: Effect,
    pub progress: Option<Value>,
    pub cancel: bool,
    pub shutdown: Shutdown,
}

impl JobContext {
    pub(super) fn new(parts: Parts) -> Self {
        Self {
            db: parts.db,
            system: parts.system,
            env: parts.env,
            lease: parts.lease,
            class: parts.class,
            effect: parts.effect,
            progress: parts.progress,
            shutdown: parts.shutdown,
            started: Instant::now(),
            chunks: 0,
            cancel: parts.cancel,
            effect_started: false,
        }
    }

    /// The job's workspace.
    #[must_use]
    pub fn workspace(&self) -> WorkspaceId {
        self.lease.workspace
    }

    /// The job's id.
    #[must_use]
    pub fn id(&self) -> JobId {
        JobId::from_uuid(self.lease.id)
    }

    /// The progress of the last checkpoint, from this run or an earlier claim: where a resumed
    /// job continues.
    #[must_use]
    pub fn progress(&self) -> Option<&Value> {
        self.progress.as_ref()
    }

    /// The worker's pool, for reads outside a chunk (load, commit, then call outside).
    #[must_use]
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// A resource the worker role provides to its kinds (the deployment's keys, the outbound
    /// webhook client).
    ///
    /// # Errors
    ///
    /// The worker role did not provide `T`.
    pub fn env<T: Send + Sync + 'static>(&self) -> Result<&T, JobError> {
        self.env.get::<T>().ok_or_else(|| {
            JobError::Failed(format!(
                "the worker provides no {}",
                std::any::type_name::<T>()
            ))
        })
    }

    /// Opens a chunk in the job's workspace: on the worker's pool, or on its `norbelys_system`
    /// pool for a system kind.
    ///
    /// # Errors
    ///
    /// The database is unavailable.
    pub async fn begin(&self) -> Result<Chunk, JobError> {
        let db = if self.class == Class::System {
            &self.system
        } else {
            &self.db
        };
        Ok(Chunk {
            tx: db.begin_in(self.lease.workspace).await?,
        })
    }

    /// Opens a chunk in another workspace; fan-out kinds only. Its checkpoint returns to the
    /// job's workspace for the fence, in the same transaction.
    ///
    /// # Errors
    ///
    /// The kind is not a fan-out kind, or the database is unavailable.
    pub async fn begin_in(&self, workspace: WorkspaceId) -> Result<Chunk, JobError> {
        if self.class != Class::FanOut {
            return Err(JobError::Failed(
                "only a fan-out kind opens another workspace".to_owned(),
            ));
        }
        Ok(Chunk {
            tx: self.db.begin_in(workspace).await?,
        })
    }

    /// The worker's `norbelys_system` pool; system kinds only. For the work a system kind cannot
    /// do inside a chunk: statements PostgreSQL refuses in a transaction block (`DETACH PARTITION
    /// … CONCURRENTLY`), and session-scoped advisory locks held across several transactions. Its
    /// writes are not fenced; the kind records what it did in a later chunk.
    ///
    /// # Errors
    ///
    /// The kind is not a system kind.
    pub fn system(&self) -> Result<&Database, JobError> {
        if self.class != Class::System {
            return Err(JobError::Failed(
                "only a system kind uses the system pool".to_owned(),
            ));
        }
        Ok(&self.system)
    }

    /// A read-only transaction as `norbelys_scheduler`, the role that sees routing columns
    /// across workspaces but no payload, body or secret: a fan-out kind reads its directory
    /// of work (which workspaces have something to do) through it.
    ///
    /// # Errors
    ///
    /// The kind is not a fan-out kind, or the database is unavailable.
    pub async fn directory(&self) -> Result<Tx, JobError> {
        if self.class != Class::FanOut {
            return Err(JobError::Failed(
                "only a fan-out kind reads a directory".to_owned(),
            ));
        }
        let mut tx = self.db.begin().await?;
        db::as_scheduler(&mut tx).await?;
        Ok(tx)
    }

    /// Writes `progress` and renews the lease under the fence, as the chunk's last statement,
    /// then commits the chunk: its effects and its checkpoint together, or neither. Clears the
    /// effect marker.
    ///
    /// # Errors
    ///
    /// [`JobError::ClaimLost`] when the fence matched no row (the chunk is rolled back), or the
    /// database failed.
    pub async fn checkpoint(&mut self, mut chunk: Chunk, progress: Value) -> Result<(), JobError> {
        db::set_workspace(&mut chunk.tx, self.lease.workspace).await?;
        let cancel = sqlx::query_scalar!(
            r#"UPDATE jobs SET progress = $5, lease_expires_at = now() + make_interval(secs => $6), effect_started_at = NULL
                WHERE workspace_id = $1 AND id = $2 AND lease_owner = $3 AND claims = $4
               RETURNING cancel_requested_at IS NOT NULL AS "cancel!""#,
            self.lease.workspace.uuid(),
            self.lease.id,
            &*self.lease.owner,
            self.lease.claims,
            progress,
            LEASE.as_secs_f64(),
        )
        .fetch_optional(&mut *chunk.tx)
        .await?;
        let Some(cancel) = cancel else {
            return Err(JobError::ClaimLost);
        };
        chunk.tx.commit().await?;
        self.chunks = self.chunks.saturating_add(1);
        self.cancel |= cancel;
        self.effect_started = false;
        self.progress = Some(progress);
        Ok(())
    }

    /// True when the quantum is spent, cancellation was asked for, or the process is stopping:
    /// the job returns `Outcome::Yield { after: Duration::ZERO }` at this chunk boundary.
    #[must_use]
    pub fn should_yield(&self) -> bool {
        self.interrupted()
            || self.chunks >= QUANTUM_CHUNKS
            || self.started.elapsed() >= QUANTUM_TIME
    }

    /// Cancellation or shutdown, even while an external listing has no resumable offset.
    #[must_use]
    pub fn interrupted(&self) -> bool {
        self.cancel || self.shutdown.requested()
    }

    /// Renews the lease under the fence without a checkpoint, for a long external call (an AI
    /// call up to 90 s). A renewal cannot extend a lost lease.
    ///
    /// # Errors
    ///
    /// [`JobError::ClaimLost`], or the database failed.
    pub async fn heartbeat(&mut self) -> Result<(), JobError> {
        let mut tx = self.db.begin_in(self.lease.workspace).await?;
        let cancel = sqlx::query_scalar!(
            r#"UPDATE jobs SET lease_expires_at = now() + make_interval(secs => $5)
                WHERE workspace_id = $1 AND id = $2 AND lease_owner = $3 AND claims = $4
               RETURNING cancel_requested_at IS NOT NULL AS "cancel!""#,
            self.lease.workspace.uuid(),
            self.lease.id,
            &*self.lease.owner,
            self.lease.claims,
            LEASE.as_secs_f64(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(cancel) = cancel else {
            return Err(JobError::ClaimLost);
        };
        tx.commit().await?;
        self.cancel |= cancel;
        Ok(())
    }

    /// Records, under the fence, that this chunk's external effect is about to start; external
    /// ambiguous kinds only. The checkpoint clears it; a run that ends or is recovered with it
    /// set goes to `needs_review`, because the effect may or may not have happened.
    ///
    /// # Errors
    ///
    /// The kind is not external ambiguous, [`JobError::ClaimLost`], or the database failed.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the contract of the kinds later modules add")
    )]
    pub async fn mark_effect_started(&mut self) -> Result<(), JobError> {
        if self.effect != Effect::ExternalAmbiguous {
            return Err(JobError::Failed(
                "only an external ambiguous kind marks its effect".to_owned(),
            ));
        }
        let mut tx = self.db.begin_in(self.lease.workspace).await?;
        let marked = sqlx::query_scalar!(
            r#"UPDATE jobs SET effect_started_at = now(), lease_expires_at = now() + make_interval(secs => $5)
                WHERE workspace_id = $1 AND id = $2 AND lease_owner = $3 AND claims = $4
               RETURNING id"#,
            self.lease.workspace.uuid(),
            self.lease.id,
            &*self.lease.owner,
            self.lease.claims,
            LEASE.as_secs_f64(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        if marked.is_none() {
            return Err(JobError::ClaimLost);
        }
        tx.commit().await?;
        self.effect_started = true;
        Ok(())
    }

    /// The checkpoints this run made.
    pub(super) fn chunks(&self) -> u32 {
        self.chunks
    }

    /// The facts of this run its conclusion depends on, with the job's failed attempts so far
    /// and its limit.
    pub(super) fn facts(&self, attempts: i16, max_attempts: i16) -> RunFacts {
        RunFacts {
            cancel: self.cancel,
            effect_started: self.effect_started,
            attempts,
            max_attempts,
        }
    }
}

/// How a claimed job ends this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Conclusion {
    Completed,
    /// Back to `available` after `after`; `failure` is set when the run counts as failed.
    Available {
        after: Duration,
        failure: Option<String>,
    },
    Failed {
        error: String,
    },
    Cancelled,
    NeedsReview {
        error: String,
    },
}

impl Conclusion {
    fn state(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Available { .. } => "available",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
            Self::NeedsReview { .. } => "needs_review",
        }
    }
}

/// What a run's conclusion depends on besides its outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RunFacts {
    /// Cancellation was asked for and observed by this run (at its claim or a checkpoint).
    pub cancel: bool,
    /// An external ambiguous effect was marked as started and no checkpoint recorded it since.
    pub effect_started: bool,
    /// Failed runs before this one.
    pub attempts: i16,
    /// Failed runs that end the job `failed`.
    pub max_attempts: i16,
}

/// What a run's outcome and facts decide, with the `job.run` outcome label; `None` when the
/// lease was lost, so nothing may be written. Pure: `draw` is the backoff's random draw.
pub(super) fn decide(
    result: &Result<Outcome, JobError>,
    facts: RunFacts,
    draw: u64,
) -> Option<(Conclusion, &'static str)> {
    let RunFacts {
        cancel,
        effect_started,
        attempts,
        max_attempts,
    } = facts;
    let retry = |after: Duration, error: String| {
        if attempts.saturating_add(1) >= max_attempts {
            (Conclusion::Failed { error }, "failed")
        } else {
            (
                Conclusion::Available {
                    after,
                    failure: Some(error),
                },
                "retry",
            )
        }
    };
    if matches!(result, Err(JobError::ClaimLost)) {
        return None;
    }
    if effect_started {
        let detail = match result {
            Err(error) => {
                format!("needs_review: the run failed after its external effect started: {error}")
            }
            Ok(_) => {
                "needs_review: the run ended after its external effect started without recording it"
                    .to_owned()
            }
        };
        return Some((Conclusion::NeedsReview { error: detail }, "needs_review"));
    }
    Some(match result {
        Ok(Outcome::Done) => (Conclusion::Completed, "done"),
        _ if cancel => (Conclusion::Cancelled, "discard"),
        Ok(Outcome::Yield { after }) => (
            Conclusion::Available {
                after: *after,
                failure: None,
            },
            "yield",
        ),
        Ok(Outcome::Retry { after }) => {
            retry(*after, "retry: the job asked to run again".to_owned())
        }
        Ok(Outcome::Discard { reason }) => (
            Conclusion::Failed {
                error: format!("discarded: {reason}"),
            },
            "discard",
        ),
        Err(JobError::InvalidPayload(detail)) => (
            Conclusion::Failed {
                error: format!("invalid_payload: {detail}"),
            },
            "failed",
        ),
        Err(error) => {
            let failures = u32::try_from(attempts).unwrap_or_default();
            retry(
                retry::backoff(failures, &retry::JOBS, draw),
                format!("error: {error}"),
            )
        }
    })
}

/// Ends this claim of the job: its state, its next run, its error and its lane slot, under the
/// lane lock and the fence, and the kind's recovery `hook` in the same transaction, so nothing
/// the claim reserved stays open once it ends. Returns false when the fence matched nothing
/// (the lease was lost and its recovery already released the slot).
///
/// # Errors
///
/// The database failed; the lease then expires and the recovery concludes the job.
pub(super) async fn conclude(
    db: &Database,
    lease: &Lease,
    conclusion: &Conclusion,
    hook: Option<RecoveryHook>,
) -> Result<bool, sqlx::Error> {
    let (after, error, failed) = match conclusion {
        Conclusion::Available { after, failure } => {
            (*after, failure.clone(), i16::from(failure.is_some()))
        }
        Conclusion::Failed { error } => (Duration::ZERO, Some(error.clone()), 1),
        Conclusion::NeedsReview { error } => (Duration::ZERO, Some(error.clone()), 1),
        Conclusion::Completed => (Duration::ZERO, None, 0),
        Conclusion::Cancelled => (
            Duration::ZERO,
            Some("cancelled: cancellation was requested".to_owned()),
            0,
        ),
    };
    let mut tx = db.begin_in(lease.workspace).await?;
    sqlx::query_scalar!(
        "SELECT 1 FROM job_lanes WHERE workspace_id = $1 AND queue = $2 FOR UPDATE",
        lease.workspace.uuid(),
        lease.queue.as_str(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    let concluded = sqlx::query_scalar!(
        "UPDATE jobs SET state = $5,
                run_at = CASE WHEN $5 = 'available' THEN now() + make_interval(secs => $6) ELSE run_at END,
                attempts = attempts + $7, lease_owner = NULL, lease_expires_at = NULL,
                effect_started_at = CASE WHEN $5 = 'needs_review' THEN effect_started_at END,
                last_error = CASE WHEN $5 = 'completed' THEN NULL ELSE coalesce($8, last_error) END,
                finished_at = CASE WHEN $5 IN ('completed', 'failed', 'cancelled') THEN now() END
          WHERE workspace_id = $1 AND id = $2 AND lease_owner = $3 AND claims = $4
         RETURNING id",
        lease.workspace.uuid(),
        lease.id,
        &*lease.owner,
        lease.claims,
        conclusion.state(),
        after.as_secs_f64(),
        failed,
        error,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if concluded.is_none() {
        return Ok(false);
    }
    if let Some(hook) = hook {
        hook(&mut tx, lease.workspace, JobId::from_uuid(lease.id)).await?;
    }
    sqlx::query!(
        "UPDATE job_lanes SET running = running - 1 WHERE workspace_id = $1 AND queue = $2",
        lease.workspace.uuid(),
        lease.queue.as_str(),
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Conclusion, RunFacts, decide};
    use crate::domain::retry;
    use crate::jobs::{JobError, Outcome};

    /// A first run of a job that ends `failed` after three failed runs, neither cancelled nor
    /// holding an effect marker.
    const FRESH: RunFacts = RunFacts {
        cancel: false,
        effect_started: false,
        attempts: 0,
        max_attempts: 3,
    };
    /// A run after two failures: one more ends the job.
    const LAST: RunFacts = RunFacts {
        attempts: 2,
        ..FRESH
    };

    fn failure() -> Result<Outcome, JobError> {
        Err(JobError::Failed("planned failure".to_owned()))
    }

    /// Every outcome of an ordinary run concludes as documented: done completes; a yield waits
    /// without counting a failure; a retry or an error counts one (an error waits for the job
    /// backoff) and ends the job `failed` once the attempts are spent; a discard or a payload
    /// that cannot decode fails at once; a lost lease writes nothing at all.
    #[test]
    fn outcomes_conclude_as_documented() {
        let seven = Duration::from_secs(7);
        assert_eq!(
            decide(&Ok(Outcome::Done), FRESH, 0),
            Some((Conclusion::Completed, "done"))
        );
        assert_eq!(
            decide(&Ok(Outcome::Yield { after: seven }), FRESH, 0),
            Some((
                Conclusion::Available {
                    after: seven,
                    failure: None
                },
                "yield"
            ))
        );
        assert!(matches!(
            decide(&Ok(Outcome::Retry { after: seven }), FRESH, 0),
            Some((Conclusion::Available { after, failure: Some(_) }, "retry")) if after == seven
        ));
        assert!(matches!(
            decide(&Ok(Outcome::Retry { after: seven }), LAST, 0),
            Some((Conclusion::Failed { .. }, "failed"))
        ));
        assert!(matches!(
            decide(
                &Ok(Outcome::Discard {
                    reason: "obsolete".to_owned()
                }),
                FRESH,
                0
            ),
            Some((Conclusion::Failed { .. }, "discard"))
        ));
        assert!(matches!(
            decide(&failure(), FRESH, 0),
            Some((Conclusion::Available { after, failure: Some(_) }, "retry")) if after == retry::JOBS.floor
        ));
        assert!(matches!(
            decide(&failure(), LAST, 0),
            Some((Conclusion::Failed { .. }, "failed"))
        ));
        assert!(matches!(
            decide(
                &Err(JobError::InvalidPayload("missing field".to_owned())),
                FRESH,
                0
            ),
            Some((Conclusion::Failed { .. }, "failed"))
        ));
        assert_eq!(decide(&Err(JobError::ClaimLost), FRESH, 0), None);
    }

    /// A cancellation the run observed ends the job `cancelled` whatever stopped the run (a
    /// yield, a discard, an error), while a run that finished its work still completes.
    #[test]
    fn an_observed_cancellation_ends_the_job_cancelled() {
        let cancelled = RunFacts {
            cancel: true,
            ..FRESH
        };
        for result in [
            Ok(Outcome::Yield {
                after: Duration::ZERO,
            }),
            Ok(Outcome::Discard {
                reason: "cancelled".to_owned(),
            }),
            failure(),
        ] {
            assert_eq!(
                decide(&result, cancelled, 0),
                Some((Conclusion::Cancelled, "discard"))
            );
        }
        assert_eq!(
            decide(&Ok(Outcome::Done), cancelled, 0),
            Some((Conclusion::Completed, "done"))
        );
    }

    /// A run that ends while its ambiguous external effect is marked and unrecorded goes to
    /// `needs_review` whatever it returned, because the effect may or may not have happened;
    /// only a lost lease still writes nothing.
    #[test]
    fn an_unrecorded_ambiguous_effect_needs_review() {
        let marked = RunFacts {
            effect_started: true,
            ..FRESH
        };
        for result in [
            Ok(Outcome::Done),
            Ok(Outcome::Yield {
                after: Duration::ZERO,
            }),
            failure(),
        ] {
            assert!(matches!(
                decide(&result, marked, 0),
                Some((Conclusion::NeedsReview { .. }, "needs_review"))
            ));
        }
        assert_eq!(decide(&Err(JobError::ClaimLost), marked, 0), None);
    }
}
