//! The runner: the loops a worker process runs to claim, execute, recover and schedule jobs.
//!
//! - **Claim loops**, one per queue that has registered kinds. Each holds a fixed number of
//!   permits (the jobs of that queue this process runs at once) and claims only when a permit
//!   is free, as many jobs as it has free permits, so claimed work never waits in memory. A
//!   claimed job runs in its own task and gives its permit back when it concludes.
//! - **The listener**: one dedicated connection, outside the pool, holding `LISTEN` on the
//!   wake-up channel. A notification names a queue and wakes its claim loop. `LISTEN` is
//!   committed before the first claim, so a wake-up sent after the first sweep cannot be
//!   missed; after a reconnection every queue sweeps, because notifications sent while the
//!   connection was down are lost.
//! - **Sweeps**: a claim loop that found nothing (no due job, or every lane with due jobs
//!   full) waits for a wake-up: a notification, one of its own jobs concluding (which frees a
//!   lane slot), a timer it set itself (a job it rescheduled a few seconds ahead), or 30
//!   seconds, so a lost wake-up costs at most one sweep.
//! - **The recovery loop**, every 30 seconds: expired leases are recovered by effect class, with
//!   their kind's recovery hook, and the backlog gauges are recorded.
//! - **The schedule loop**, about once a second: periodic kinds are enqueued when due.
//!
//! Shutdown: once the process is asked to stop, the loops stop claiming, every running job sees
//! `should_yield()` at its next chunk boundary, checkpoints and yields, and the claim loops
//! wait up to 35 seconds for their permits to come back (the supervisor's grace is 40). A job
//! still running after that keeps its lease until it expires, and the recovery of another
//! worker takes over.
//!
//! Telemetry: one `job.run` event per claim-to-conclusion (job, kind, queue, workspace,
//! generation, chunks, outcome, duration, error code), one `job.recover` event per recovered
//! lease, and the metrics `norbelys_jobs_runs_total{queue, outcome}`,
//! `norbelys_jobs_recoveries_total{effect, result}` and
//! `norbelys_jobs_backlog_age_seconds{queue}`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge};
use secrecy::{ExposeSecret as _, SecretString};
use sqlx::postgres::PgListener;
use strum::IntoEnumIterator as _;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tracing::Instrument as _;
use uuid::Uuid;

use super::lanes::{self, Claimed};
use super::lease::{self, Conclusion, Lease, Parts};
use super::{
    CHANNEL, Class, JobContext, JobId, Queue, Registry, SYSTEM_WORKSPACE, draw, schedules,
};
use crate::db::Database;
use crate::process::Shutdown;

/// How often an idle claim loop and the recovery loop look again without a wake-up.
const SWEEP: Duration = Duration::from_secs(30);
/// How often the schedule loop looks for due schedules.
const TICK: Duration = Duration::from_secs(1);
/// How long a stopping claim loop waits for its running jobs to yield.
const GRACE: Duration = Duration::from_secs(35);
/// How much later than a rescheduled job's delay its wake-up timer fires: `run_at` is set by
/// the database's clock and the timer runs on this process's, so a timer that fired exactly on
/// time could claim a few milliseconds before the job is due, find nothing, and leave the job
/// to the next sweep.
const WAKE_MARGIN: Duration = Duration::from_millis(500);

/// The job runner of one worker process.
pub struct Runner {
    /// The worker's pool (`norbelys_worker`): claims, chunks of tenant and fan-out kinds,
    /// conclusions, recovery.
    pub db: Database,
    /// The worker's `norbelys_system` pool, for the chunks of system kinds only.
    pub system: Database,
    /// The worker's own login, for the dedicated `LISTEN` connection.
    pub listen_url: SecretString,
    /// Every kind this worker runs.
    pub registry: Registry,
    /// Resources the kinds read through `JobContext::env` (the keys, the webhook client).
    pub env: http::Extensions,
    /// Permits per queue that differ from the queue's default.
    pub permits: Vec<(Queue, u32)>,
    /// The process's stop signal.
    pub shutdown: Shutdown,
}

/// Why the runner could not start.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("cannot listen for wake-ups")]
    Listen(#[source] sqlx::Error),
    #[error("a critical runner loop stopped: {0}")]
    Loop(&'static str),
}

/// What every loop and job task of the runner shares.
struct Shared {
    db: Database,
    system: Database,
    registry: Registry,
    env: Arc<http::Extensions>,
    owner: Arc<str>,
    wakes: HashMap<Queue, Arc<Notify>>,
    shutdown: Shutdown,
    runs: Counter<u64>,
    recoveries: Counter<u64>,
    backlog: Gauge<f64>,
}

impl Shared {
    /// What the loops and job tasks of one runner share; `owner` names this process in every
    /// lease it takes.
    fn new(
        db: Database,
        system: Database,
        registry: Registry,
        env: http::Extensions,
        owner: Arc<str>,
        shutdown: Shutdown,
    ) -> Self {
        let meter = opentelemetry::global::meter("norbelys");
        Self {
            db,
            system,
            registry,
            env: Arc::new(env),
            owner,
            wakes: Queue::iter()
                .map(|queue| (queue, Arc::new(Notify::new())))
                .collect(),
            shutdown,
            runs: meter
                .u64_counter("norbelys_jobs_runs_total")
                .with_description("Job runs, from claim to conclusion, by queue and outcome.")
                .build(),
            recoveries: meter
                .u64_counter("norbelys_jobs_recoveries_total")
                .with_description("Expired leases recovered, by effect class and resulting state.")
                .build(),
            backlog: meter
                .f64_gauge("norbelys_jobs_backlog_age_seconds")
                .with_unit("s")
                .with_description("Age of the oldest due job that is not running, by queue.")
                .build(),
        }
    }

    fn wake(&self, queue: Queue) {
        if let Some(notify) = self.wakes.get(&queue) {
            notify.notify_one();
        }
    }

    fn wake_all(&self) {
        for notify in self.wakes.values() {
            notify.notify_one();
        }
    }
}

impl Runner {
    /// Runs until the process is asked to stop and every claim loop has drained.
    ///
    /// # Errors
    ///
    /// The `LISTEN` connection cannot be opened.
    pub async fn run(self) -> Result<(), RunnerError> {
        let owner: Arc<str> = Arc::from(format!(
            "worker-{}-{}",
            std::process::id(),
            Uuid::now_v7().simple()
        ));
        let queues = self.registry.queues();
        let permits: Vec<(Queue, u32)> = queues
            .iter()
            .map(|queue| {
                let configured = self
                    .permits
                    .iter()
                    .find(|(named, _)| named == queue)
                    .map(|(_, permits)| *permits);
                (
                    *queue,
                    configured.unwrap_or_else(|| queue.default_permits()).max(1),
                )
            })
            .collect();
        let shared = Arc::new(Shared::new(
            self.db,
            self.system,
            self.registry,
            self.env,
            Arc::clone(&owner),
            self.shutdown,
        ));

        // `LISTEN` first, then the first sweep (each claim loop starts by claiming), then wait.
        let mut listener = PgListener::connect(self.listen_url.expose_secret())
            .await
            .map_err(RunnerError::Listen)?;
        listener
            .listen(CHANNEL)
            .await
            .map_err(RunnerError::Listen)?;
        tracing::info!(owner = %owner, queues = ?permits, "job runner started");

        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(listen_loop(Arc::clone(&shared), listener));
        tasks.spawn(recovery_loop(Arc::clone(&shared)));
        tasks.spawn(schedule_loop(Arc::clone(&shared)));
        for (queue, permits) in permits {
            tasks.spawn(claim_loop(Arc::clone(&shared), queue, permits));
        }
        let mut failure = None;
        while let Some(joined) = tasks.join_next().await {
            if joined.is_err() || !shared.shutdown.requested() {
                let category = if joined.as_ref().is_err_and(tokio::task::JoinError::is_panic) {
                    "task_panic"
                } else if joined.is_ok() {
                    "unexpected_return"
                } else {
                    "task_cancelled"
                };
                failure.get_or_insert(category);
                tracing::error!(error_code = category, "a runner loop stopped abnormally");
                shared.shutdown.request();
            }
        }
        tracing::info!(owner = %owner, "job runner stopped");
        failure.map_or(Ok(()), |category| Err(RunnerError::Loop(category)))
    }
}

/// Receives wake-ups and wakes the named queue's claim loop.
async fn listen_loop(shared: Arc<Shared>, mut listener: PgListener) {
    let mut shutdown = shared.shutdown.clone();
    loop {
        let received = tokio::select! {
            received = listener.try_recv() => received,
            () = shutdown.wait() => return,
        };
        match received {
            Ok(Some(notification)) => {
                if let Ok(queue) = notification.payload().parse::<Queue>() {
                    shared.wake(queue);
                }
            }
            // The connection was lost and opened again: wake-ups may have been missed.
            Ok(None) => shared.wake_all(),
            Err(error) => {
                tracing::warn!(error = %error, "the wake-up listener failed; reconnecting");
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(1)) => {}
                    () = shutdown.wait() => return,
                }
                shared.wake_all();
            }
        }
    }
}

/// Claims and starts the jobs of one queue while permits are free.
async fn claim_loop(shared: Arc<Shared>, queue: Queue, permits: u32) {
    let semaphore = Arc::new(Semaphore::new(usize::try_from(permits).unwrap_or(1)));
    let wake = shared
        .wakes
        .get(&queue)
        .map_or_else(|| Arc::new(Notify::new()), Arc::clone);
    let mut shutdown = shared.shutdown.clone();
    loop {
        let first = tokio::select! {
            permit = Arc::clone(&semaphore).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
            () = shutdown.wait() => break,
        };
        if shutdown.requested() {
            break;
        }
        let mut held = vec![first];
        while let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() {
            held.push(permit);
        }
        let claimed = match lanes::claim(&shared.db, queue, &shared.owner, held.len()).await {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::warn!(error = %error, queue = queue.as_str(), "claim failed");
                Vec::new()
            }
        };
        let idle = claimed.is_empty();
        for (job, permit) in claimed.into_iter().zip(held.drain(..)) {
            tokio::spawn(execute(Arc::clone(&shared), queue, job, permit));
        }
        drop(held);
        if idle {
            tokio::select! {
                () = wake.notified() => {}
                () = tokio::time::sleep(SWEEP) => {}
                () = shutdown.wait() => break,
            }
        }
    }
    // Running jobs see the stop at their next chunk boundary and yield; wait for them.
    if tokio::time::timeout(GRACE, semaphore.acquire_many(permits))
        .await
        .is_err()
    {
        tracing::warn!(
            queue = queue.as_str(),
            "jobs still running at the end of the grace; their leases will expire"
        );
    }
}

/// How one claimed job's run ended, for its event and its metric.
struct Report {
    outcome: &'static str,
    chunks: u32,
    error_code: Option<&'static str>,
    /// The job is `available` again after this long.
    again_after: Option<Duration>,
}

impl Report {
    fn ended(outcome: &'static str, error_code: Option<&'static str>) -> Self {
        Self {
            outcome,
            chunks: 0,
            error_code,
            again_after: None,
        }
    }
}

/// Runs one claimed job to its conclusion, then gives its permit back.
async fn execute(
    shared: Arc<Shared>,
    queue: Queue,
    claimed: Claimed,
    permit: OwnedSemaphorePermit,
) {
    let started = Instant::now();
    let lease = Lease {
        id: claimed.id,
        workspace: claimed.workspace,
        queue,
        owner: Arc::clone(&shared.owner),
        claims: claimed.claims,
    };
    let span = tracing::info_span!("job.run", job_id = %claimed.id, workspace_id = %claimed.workspace, kind = %claimed.kind, queue = queue.as_str(), otel.status_code = tracing::field::Empty);
    let report = crate::process::guarded(
        "job.run",
        run_claimed(&shared, &lease, &claimed).instrument(span.clone()),
    )
    .await
    .unwrap_or_else(|()| Report::ended("failed", Some("task_panic")));
    {
        let _entered = span.enter();
        if report.error_code.is_some() {
            span.record("otel.status_code", "ERROR");
        }
        record(&shared, queue, &claimed, &report, started.elapsed());
    }
    drop(permit);
    // The conclusion freed a slot in the job's lane: due jobs of a lane that was full wait for
    // exactly this, so the claim loop looks again now rather than at its next sweep.
    shared.wake(queue);
    // A job rescheduled a few seconds ahead runs then, not at the next sweep.
    if let Some(after) = report.again_after
        && after < SWEEP
        && let Some(notify) = shared.wakes.get(&queue).map(Arc::clone)
    {
        tokio::spawn(async move {
            tokio::time::sleep(after.saturating_add(WAKE_MARGIN)).await;
            notify.notify_one();
        });
    }
}

/// Emits one run's `job.run` event and counts it (`norbelys_jobs_runs_total` and the coverage
/// unit): exactly once per claimed job, whatever its outcome.
fn record(shared: &Shared, queue: Queue, claimed: &Claimed, report: &Report, elapsed: Duration) {
    crate::telemetry::unit(crate::telemetry::Event::JobRun);
    tracing::info!(
        event = "job.run",
        job_id = %JobId::from_uuid(claimed.id),
        kind = %claimed.kind,
        queue = queue.as_str(),
        workspace_id = %claimed.workspace,
        generation = claimed.claims,
        chunks = report.chunks,
        outcome = report.outcome,
        duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        error_code = report.error_code,
        "job run"
    );
    shared.runs.add(
        1,
        &[
            KeyValue::new("queue", queue.as_str()),
            KeyValue::new("outcome", report.outcome),
        ],
    );
}

/// What the claim's second step reads: the payload, as the worker inside the job's workspace,
/// under the fence (the scheduler role that claimed the job cannot read a payload).
struct Loaded {
    payload: serde_json::Value,
    progress: Option<serde_json::Value>,
    max_attempts: i16,
    cancel: bool,
    /// The W3C `traceparent` of the request that enqueued the job, when one did.
    trace_parent: Option<String>,
}

async fn load(db: &Database, lease: &Lease) -> Result<Option<Loaded>, sqlx::Error> {
    let mut tx = db.begin_in(lease.workspace).await?;
    let row = sqlx::query!(
        r#"SELECT payload, progress, max_attempts, cancel_requested_at IS NOT NULL AS "cancel!", trace_parent
             FROM jobs WHERE workspace_id = $1 AND id = $2 AND lease_owner = $3 AND claims = $4"#,
        lease.workspace.uuid(),
        lease.id,
        &*lease.owner,
        lease.claims,
    )
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row.map(|row| Loaded {
        payload: row.payload,
        progress: row.progress,
        max_attempts: row.max_attempts,
        cancel: row.cancel,
        trace_parent: row.trace_parent,
    }))
}

async fn run_claimed(shared: &Shared, lease: &Lease, claimed: &Claimed) -> Report {
    let loaded = match load(&shared.db, lease).await {
        Ok(Some(loaded)) => loaded,
        Ok(None) => return Report::ended("lost", Some("claim_lost")),
        Err(error) => {
            tracing::warn!(error = %error, job = %claimed.id, "a claimed job could not be read; its lease will expire");
            return Report::ended("lost", Some("database"));
        }
    };
    // The run's span links to the request that enqueued the job, never continuing its trace.
    if let Some(trace_parent) = &loaded.trace_parent {
        crate::telemetry::link(&tracing::Span::current(), trace_parent);
    }
    let version = loaded
        .payload
        .get("version")
        .and_then(serde_json::Value::as_u64);
    let kind = shared.registry.get(&claimed.kind);
    let refusal = match kind {
        None => Some(format!(
            "unknown_kind: no worker of this release runs `{}`",
            claimed.kind
        )),
        Some(kind) if version != Some(u64::from(kind.version)) => Some(format!(
            "unknown_version: `{}` runs payload version {}, not {version:?}",
            claimed.kind, kind.version
        )),
        Some(kind) if kind.class != Class::Tenant && lease.workspace != SYSTEM_WORKSPACE => {
            Some(format!(
                "wrong_class: `{}` runs in the system workspace only",
                claimed.kind
            ))
        }
        Some(_) => None,
    };
    let (Some(kind), None, false) = (kind, &refusal, loaded.cancel) else {
        let conclusion = match refusal {
            Some(error) => Conclusion::Failed { error },
            None => Conclusion::Cancelled,
        };
        let outcome = if conclusion == Conclusion::Cancelled {
            "discard"
        } else {
            "failed"
        };
        let hook = kind.and_then(|kind| kind.recovery_hook);
        return match lease::conclude(&shared.db, lease, &conclusion, hook).await {
            Ok(true) => Report::ended(outcome, None),
            Ok(false) => Report::ended("lost", Some("claim_lost")),
            Err(error) => {
                tracing::warn!(error = %error, job = %claimed.id, "a refused job's conclusion was not recorded");
                Report::ended("lost", Some("database"))
            }
        };
    };

    let cx = JobContext::new(Parts {
        db: shared.db.clone(),
        system: shared.system.clone(),
        env: Arc::clone(&shared.env),
        lease: lease.clone(),
        class: kind.class,
        effect: kind.effect,
        progress: loaded.progress,
        cancel: false,
        shutdown: shared.shutdown.clone(),
    });
    let (result, cx) = (kind.run)(loaded.payload, cx).await;
    let chunks = cx.chunks();
    let error_code = result.as_ref().err().map(super::JobError::code);
    if let Err(error) = &result {
        tracing::warn!(error = %error, job = %claimed.id, kind = %claimed.kind, "job run failed");
    }
    let Some((conclusion, outcome)) = lease::decide(
        &result,
        cx.facts(claimed.attempts, loaded.max_attempts),
        draw(),
    ) else {
        return Report {
            outcome: "lost",
            chunks,
            error_code,
            again_after: None,
        };
    };
    let again_after = match &conclusion {
        Conclusion::Available { after, .. } => Some(*after),
        _ => None,
    };
    match lease::conclude(&shared.db, lease, &conclusion, kind.recovery_hook).await {
        Ok(true) => Report {
            outcome,
            chunks,
            error_code,
            again_after,
        },
        Ok(false) => Report {
            outcome: "lost",
            chunks,
            error_code: Some("claim_lost"),
            again_after: None,
        },
        Err(error) => {
            tracing::warn!(error = %error, job = %claimed.id, "a job's conclusion was not recorded; its lease will expire");
            Report {
                outcome: "lost",
                chunks,
                error_code: Some("database"),
                again_after: None,
            }
        }
    }
}

/// Recovers expired leases and records the backlog, every sweep, until the stop.
async fn recovery_loop(shared: Arc<Shared>) {
    let mut shutdown = shared.shutdown.clone();
    loop {
        match lanes::recover(&shared.db, &shared.registry, draw).await {
            Ok(recovered) => {
                for job in recovered {
                    let effect = shared
                        .registry
                        .get(&job.kind)
                        .map_or("unknown", |kind| kind.effect.as_str());
                    tracing::warn!(
                        event = "job.recover",
                        job_id = %JobId::from_uuid(job.id),
                        kind = %job.kind,
                        queue = %job.queue,
                        workspace_id = %job.workspace,
                        effect,
                        result = %job.state,
                        "an expired lease was recovered"
                    );
                    shared.recoveries.add(
                        1,
                        &[
                            KeyValue::new("effect", effect),
                            KeyValue::new("result", job.state),
                        ],
                    );
                }
            }
            Err(error) => tracing::warn!(error = %error, "lease recovery failed"),
        }
        match lanes::backlog(&shared.db).await {
            Ok(oldest) => {
                let now = crate::process::now();
                for queue in shared.registry.queues() {
                    let age = oldest
                        .iter()
                        .find(|(name, _)| name == queue.as_str())
                        .map_or(0.0, |(_, at)| {
                            now.0.duration_since(at.0).as_secs_f64().max(0.0)
                        });
                    shared
                        .backlog
                        .record(age, &[KeyValue::new("queue", queue.as_str())]);
                }
            }
            Err(error) => tracing::warn!(error = %error, "the job backlog could not be read"),
        }
        tokio::select! {
            () = tokio::time::sleep(SWEEP) => {}
            () = shutdown.wait() => return,
        }
    }
}

/// Enqueues due periodic jobs, about once a second, until the stop.
async fn schedule_loop(shared: Arc<Shared>) {
    let mut shutdown = shared.shutdown.clone();
    loop {
        match schedules::tick(&shared.db, &shared.registry).await {
            Ok(queues) => {
                for queue in queues {
                    super::wake(&shared.db, queue).await;
                }
            }
            Err(error) => tracing::warn!(error = %error, "the schedule tick failed"),
        }
        tokio::select! {
            () = tokio::time::sleep(TICK) => {}
            () = shutdown.wait() => return,
        }
    }
}

/// The runner's claim and execution without its loops, for tests: a test drives each step
/// (claim, run, recover) itself and inspects the rows in between, while every step runs the
/// production code.
#[cfg(test)]
pub(crate) struct Harness {
    shared: Arc<Shared>,
    stop: tokio::sync::watch::Sender<bool>,
}

#[cfg(test)]
impl Harness {
    /// A runner of the kinds in `registry` named `owner` in the leases it takes, on `db` (the
    /// worker login) and `system` (the system login), with `env` for its kinds.
    pub(crate) fn new(
        db: Database,
        system: Database,
        registry: Registry,
        env: http::Extensions,
        owner: &str,
    ) -> Self {
        let (shutdown, stop) = Shutdown::manual();
        Self {
            shared: Arc::new(Shared::new(
                db,
                system,
                registry,
                env,
                Arc::from(owner),
                shutdown,
            )),
            stop,
        }
    }

    /// One claim of `queue`, taking at most `permits` due jobs of one lane, then each claimed
    /// job run to its conclusion in turn, as the claim loop and its job tasks do. Returns each
    /// job's id and its `job.run` outcome label.
    pub(crate) async fn run_once(
        &self,
        queue: Queue,
        permits: usize,
    ) -> Vec<(JobId, &'static str)> {
        let claimed = lanes::claim(&self.shared.db, queue, &self.shared.owner, permits)
            .await
            .expect("the claim runs");
        let mut outcomes = Vec::with_capacity(claimed.len());
        for job in claimed {
            let lease = Lease {
                id: job.id,
                workspace: job.workspace,
                queue,
                owner: Arc::clone(&self.shared.owner),
                claims: job.claims,
            };
            let started = Instant::now();
            let report = run_claimed(&self.shared, &lease, &job).await;
            record(&self.shared, queue, &job, &report, started.elapsed());
            outcomes.push((JobId::from_uuid(job.id), report.outcome));
        }
        outcomes
    }

    /// One pass of the recovery loop: every expired lease recovered by its effect class, with its
    /// kind's recovery hook. Returns each recovered job's id and its new state.
    pub(crate) async fn recover_once(&self) -> Vec<(JobId, String)> {
        lanes::recover(&self.shared.db, &self.shared.registry, || 0)
            .await
            .expect("the recovery runs")
            .into_iter()
            .map(|job| (JobId::from_uuid(job.id), job.state))
            .collect()
    }

    /// Asks the jobs this runner runs to stop at their next chunk boundary, as a `SIGTERM` does.
    pub(crate) fn stop(&self) {
        let _ = self.stop.send(true);
    }
}

#[cfg(test)]
mod tests;
