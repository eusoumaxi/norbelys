//! Store and gate tests of the job runner, against real PostgreSQL.
//!
//! Each test gets its own database (see `crate::testing`) and drives the runner's steps itself
//! through [`Harness`] (one claim, each claimed job run to its conclusion) or the lane functions
//! directly, so it can inspect the rows between a claim, a run and a recovery while every step
//! runs the production code. The kinds below exist only for these tests; each exercises one
//! path of the contract, and writes audit rows as its observable effects.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::kinds::PartitionsCreate;
use super::runner::Harness;
use super::{
    Effect, Job, JobContext, JobError, JobId, NewJob, Outcome, Queue, Registry, SYSTEM_WORKSPACE,
    enqueue, enqueue_value, lanes, payload_of, schedules,
};
use crate::db::{Database, Tx};
use crate::domain::ids::WorkspaceId;
use crate::testing::TestDb;

/// The action of the audit rows the test kinds write as their effects.
const NOTE: &str = "test.note";

/// Writes the effect tagged `tag` in a chunk's transaction.
async fn note(tx: &mut Tx, workspace: WorkspaceId, tag: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO audit_log (workspace_id, actor_kind, actor_id, action, target) VALUES ($1, 'system', 'test', $2, $3)",
        workspace.uuid(),
        NOTE,
        tag,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// How many effects tagged `tag` committed, in every workspace.
async fn notes(test: &TestDb, tag: &str) -> i64 {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM audit_log WHERE action = $1 AND target = $2"#,
        NOTE,
        tag
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap()
}

/// Another worker takes the job over, as a recovery followed by a new claim would: the claim
/// generation moves on, through the system login, which no fence stops.
async fn take_over(cx: &JobContext) {
    let system = cx.env::<Database>().unwrap();
    sqlx::query!(
        "UPDATE jobs SET claims = claims + 1 WHERE id = $1",
        cx.id().uuid()
    )
    .execute(system.pool())
    .await
    .unwrap();
}

/// Writes its effect in one chunk, then completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Note {
    tag: String,
}

impl Job for Note {
    const KIND: &'static str = "test.note";
    const QUEUE: Queue = Queue::Imports;
    const EFFECT: Effect = Effect::Idempotent;

    fn unique_key(&self) -> Option<String> {
        Some(self.tag.clone())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let mut chunk = cx.begin().await?;
        note(chunk.tx(), cx.workspace(), &self.tag).await?;
        cx.checkpoint(chunk, json!({ "noted": self.tag })).await?;
        Ok(Outcome::Done)
    }
}

/// Checkpoints a first chunk and yields; the next run finds that progress, writes its effect
/// and completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TwoRuns {
    tag: String,
}

impl Job for TwoRuns {
    const KIND: &'static str = "test.two_runs";
    const QUEUE: Queue = Queue::Imports;
    const EFFECT: Effect = Effect::Idempotent;

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        if cx.progress().is_none() {
            let chunk = cx.begin().await?;
            cx.checkpoint(chunk, json!({ "step": 1 })).await?;
            return Ok(Outcome::Yield {
                after: Duration::ZERO,
            });
        }
        let mut chunk = cx.begin().await?;
        note(chunk.tx(), cx.workspace(), &self.tag).await?;
        cx.checkpoint(chunk, json!({ "step": 2 })).await?;
        Ok(Outcome::Done)
    }
}

/// Always fails; its second failure ends it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Fails {}

impl Job for Fails {
    const KIND: &'static str = "test.fails";
    const QUEUE: Queue = Queue::Imports;
    const EFFECT: Effect = Effect::Idempotent;
    const MAX_ATTEMPTS: u16 = 2;

    async fn run(self, _cx: &mut JobContext) -> Result<Outcome, JobError> {
        Err(JobError::Failed("planned failure".to_owned()))
    }
}

/// Marks its ambiguous external effect as started, then fails without recording the outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ambiguous {}

impl Job for Ambiguous {
    const KIND: &'static str = "test.ambiguous";
    const QUEUE: Queue = Queue::Imports;
    const EFFECT: Effect = Effect::ExternalAmbiguous;

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        cx.mark_effect_started().await?;
        Err(JobError::Failed("the call's outcome is unknown".to_owned()))
    }
}

/// Writes its effect, loses its job to another worker, then tries to checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Zombie {
    tag: String,
}

impl Job for Zombie {
    const KIND: &'static str = "test.zombie";
    const QUEUE: Queue = Queue::Imports;
    const EFFECT: Effect = Effect::Idempotent;

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let mut chunk = cx.begin().await?;
        note(chunk.tx(), cx.workspace(), &self.tag).await?;
        take_over(cx).await;
        cx.checkpoint(chunk, json!({})).await?;
        Ok(Outcome::Done)
    }
}

/// Renews its lease, proves it (an effect written outside its chunks), loses the job to
/// another worker, then tries to renew again.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Beats {
    tag: String,
}

impl Job for Beats {
    const KIND: &'static str = "test.beats";
    const QUEUE: Queue = Queue::Imports;
    const EFFECT: Effect = Effect::Idempotent;

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        cx.heartbeat().await?;
        let mut proof = cx.env::<Database>()?.begin().await?;
        note(&mut proof, cx.workspace(), &self.tag).await?;
        proof.commit().await?;
        take_over(cx).await;
        cx.heartbeat().await?;
        Ok(Outcome::Done)
    }
}

/// Works chunk by chunk until it must yield. With `cancel_midway`, a person asks for its
/// cancellation while it runs (through the system login, as the API's cancel would).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Patient {
    cancel_midway: bool,
}

impl Job for Patient {
    const KIND: &'static str = "test.patient";
    const QUEUE: Queue = Queue::Imports;
    const EFFECT: Effect = Effect::Idempotent;

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        loop {
            if cx.should_yield() {
                return Ok(Outcome::Yield {
                    after: Duration::ZERO,
                });
            }
            if self.cancel_midway {
                sqlx::query!(
                    "UPDATE jobs SET cancel_requested_at = now() WHERE id = $1",
                    cx.id().uuid()
                )
                .execute(cx.env::<Database>()?.pool())
                .await?;
            }
            let chunk = cx.begin().await?;
            cx.checkpoint(chunk, json!({})).await?;
        }
    }
}

/// The kinds of these tests, plus the runner's own maintenance kind.
fn registry() -> Registry {
    let mut registry = Registry::default();
    registry
        .register::<Note>()
        .unwrap()
        .register::<TwoRuns>()
        .unwrap()
        .register::<Fails>()
        .unwrap()
        .register::<Ambiguous>()
        .unwrap()
        .register::<Zombie>()
        .unwrap()
        .register::<Beats>()
        .unwrap()
        .register::<Patient>()
        .unwrap()
        .register::<PartitionsCreate>()
        .unwrap();
    registry
}

/// A runner named `owner` on the test database; its kinds reach the system login through their
/// environment, to play the other worker or the person.
fn harness(test: &TestDb, owner: &str) -> Harness {
    let mut env = http::Extensions::new();
    env.insert(test.system.clone());
    Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry(),
        env,
        owner,
    )
}

/// Enqueues `job` in `workspace` in a transaction of the api's login, as a request would.
async fn enqueue_in<J: Job>(test: &TestDb, workspace: WorkspaceId, job: &J) -> JobId {
    let mut tx = test.app.begin_in(workspace).await.unwrap();
    let id = enqueue(&mut tx, workspace, job, None).await.unwrap();
    tx.commit().await.unwrap();
    id
}

/// A job's row, as the tests inspect it.
#[derive(Debug)]
struct Row {
    state: String,
    attempts: i16,
    claims: i32,
    lease_owner: Option<String>,
    last_error: Option<String>,
    progress: Option<Value>,
    finished: bool,
    marked: bool,
    /// Seconds from now until the job is due (negative: overdue).
    due_in_seconds: f64,
    /// Seconds between its last change and its next run: the wait its last conclusion or
    /// recovery chose, whatever time the test reads it at.
    wait_seconds: f64,
}

async fn row(test: &TestDb, id: JobId) -> Row {
    sqlx::query_as!(
        Row,
        r#"SELECT state, attempts, claims, lease_owner, last_error, progress, finished_at IS NOT NULL AS "finished!",
                  effect_started_at IS NOT NULL AS "marked!", extract(epoch FROM run_at - now())::float8 AS "due_in_seconds!",
                  extract(epoch FROM run_at - updated_at)::float8 AS "wait_seconds!"
             FROM jobs WHERE id = $1"#,
        id.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap()
}

/// A lane's `running` and `max_running`.
async fn lane(test: &TestDb, workspace: WorkspaceId, queue: Queue) -> (i32, i32) {
    let lane = sqlx::query!(
        "SELECT running, max_running FROM job_lanes WHERE workspace_id = $1 AND queue = $2",
        workspace.uuid(),
        queue.as_str()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    (lane.running, lane.max_running)
}

/// Lets a claimed job's lease expire, as when its worker died.
async fn expire(test: &TestDb, id: JobId) {
    sqlx::query!(
        "UPDATE jobs SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
        id.uuid()
    )
    .execute(test.system.pool())
    .await
    .unwrap();
}

/// A payload larger than 64 KiB is refused before it reaches the queue: large inputs belong in
/// object storage, and bloated rows would slow every claim and recovery.
#[test]
fn an_oversized_payload_is_refused() {
    assert!(
        payload_of(
            &Note {
                tag: "x".repeat(70 * 1024)
            },
            1
        )
        .is_err()
    );
    assert_eq!(
        payload_of(
            &Note {
                tag: "small".to_owned()
            },
            3
        )
        .unwrap(),
        json!({ "tag": "small", "version": 3 })
    );
}

/// Enqueuing in a business transaction creates the workspace's lane with its queue's size and a
/// waiting job, due now, whose payload carries its kind's version.
#[tokio::test]
async fn enqueue_creates_the_lane_and_a_waiting_job() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let id = enqueue_in(
        &test,
        acme.id,
        &Note {
            tag: "first".to_owned(),
        },
    )
    .await;
    let job = row(&test, id).await;
    assert_eq!(
        (job.state.as_str(), job.claims, job.attempts),
        ("available", 0, 0)
    );
    assert!(job.due_in_seconds <= 0.0);
    let payload = sqlx::query_scalar!("SELECT payload FROM jobs WHERE id = $1", id.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(payload, json!({ "tag": "first", "version": 1 }));
    assert_eq!(
        lane(&test, acme.id, Queue::Imports).await,
        (0, Queue::Imports.lane_size())
    );
}

/// Enqueuing a job whose unique key has a waiting twin returns the twin and brings its run
/// forward, so repeated requests (a second verify, a retry) never pile up duplicate work.
#[tokio::test]
async fn a_twin_coalesces_and_brings_the_waiting_run_forward() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let mut tx = test.app.begin_in(acme.id).await.unwrap();
    let in_an_hour = crate::process::now().plus(Duration::from_secs(3_600));
    let first = enqueue(
        &mut tx,
        acme.id,
        &Note {
            tag: "twin".to_owned(),
        },
        Some(in_an_hour),
    )
    .await
    .unwrap();
    let second = enqueue(
        &mut tx,
        acme.id,
        &Note {
            tag: "twin".to_owned(),
        },
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(first, second);
    assert!(row(&test, first).await.due_in_seconds <= 0.0);
    let live =
        sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM jobs WHERE kind = 'test.note'"#)
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(live, 1);
}

/// A claim leases at most its lane's free slots, however many permits the worker has and jobs
/// are due, and counts them in the lane; a full lane gives nothing more.
#[tokio::test]
async fn a_claim_never_exceeds_the_lane() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    for index in 0..6 {
        enqueue_in(
            &test,
            acme.id,
            &Note {
                tag: format!("job-{index}"),
            },
        )
        .await;
    }
    let first = lanes::claim(&test.worker, Queue::Imports, "worker-a", 10)
        .await
        .unwrap();
    assert_eq!(first.len(), 4);
    assert_eq!(lane(&test, acme.id, Queue::Imports).await, (4, 4));
    assert!(
        lanes::claim(&test.worker, Queue::Imports, "worker-a", 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Two workers racing for a lane's last free slot: exactly one takes a job, and the lane ends
/// exactly full with exactly as many jobs leased.
#[tokio::test]
async fn two_workers_racing_for_the_last_slot_take_it_once() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    for index in 0..5 {
        enqueue_in(
            &test,
            acme.id,
            &Note {
                tag: format!("job-{index}"),
            },
        )
        .await;
    }
    assert_eq!(
        lanes::claim(&test.worker, Queue::Imports, "worker-a", 3)
            .await
            .unwrap()
            .len(),
        3
    );
    let (a, b) = tokio::join!(
        lanes::claim(&test.worker, Queue::Imports, "worker-a", 2),
        lanes::claim(&test.worker, Queue::Imports, "worker-b", 2),
    );
    assert_eq!(a.unwrap().len() + b.unwrap().len(), 1);
    assert_eq!(lane(&test, acme.id, Queue::Imports).await, (4, 4));
    let leased =
        sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM jobs WHERE state = 'running'"#)
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(leased, 4);
}

/// A run's chunk commits its effect together with the job's progress, and the conclusion
/// completes the job, clears its lease and frees its lane slot.
#[tokio::test]
async fn a_completed_run_commits_its_effect_once() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let id = enqueue_in(
        &test,
        acme.id,
        &Note {
            tag: "done".to_owned(),
        },
    )
    .await;
    assert_eq!(
        harness(&test, "worker-a").run_once(Queue::Imports, 1).await,
        vec![(id, "done")]
    );
    let job = row(&test, id).await;
    assert_eq!(
        (job.state.as_str(), job.attempts, job.claims),
        ("completed", 0, 1)
    );
    assert!(job.finished && job.lease_owner.is_none() && job.last_error.is_none());
    assert_eq!(job.progress, Some(json!({ "noted": "done" })));
    assert_eq!(notes(&test, "done").await, 1);
    assert_eq!(lane(&test, acme.id, Queue::Imports).await.0, 0);
}

/// A worker whose job was taken over records nothing: its checkpoint is refused, its chunk's
/// effect rolls back, and the job and its lane slot stay with the worker that took over.
#[tokio::test]
async fn a_lost_lease_records_nothing() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let id = enqueue_in(
        &test,
        acme.id,
        &Zombie {
            tag: "zombie".to_owned(),
        },
    )
    .await;
    assert_eq!(
        harness(&test, "worker-a").run_once(Queue::Imports, 1).await,
        vec![(id, "lost")]
    );
    assert_eq!(notes(&test, "zombie").await, 0);
    let job = row(&test, id).await;
    assert_eq!((job.state.as_str(), job.claims), ("running", 2));
    assert_eq!(lane(&test, acme.id, Queue::Imports).await.0, 1);
}

/// A heartbeat renews a lease its worker still holds, and cannot renew one another worker took
/// over: a long call cannot keep a lost job alive.
#[tokio::test]
async fn a_heartbeat_cannot_extend_a_lost_lease() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let id = enqueue_in(
        &test,
        acme.id,
        &Beats {
            tag: "beats".to_owned(),
        },
    )
    .await;
    assert_eq!(
        harness(&test, "worker-a").run_once(Queue::Imports, 1).await,
        vec![(id, "lost")]
    );
    assert_eq!(
        notes(&test, "beats").await,
        1,
        "the first heartbeat renewed the held lease"
    );
}

/// A job that yields runs again from its progress, and a yield never counts as a failed attempt.
#[tokio::test]
async fn a_yield_resumes_from_progress_without_counting_a_failure() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let runner = harness(&test, "worker-a");
    let id = enqueue_in(
        &test,
        acme.id,
        &TwoRuns {
            tag: "two".to_owned(),
        },
    )
    .await;
    assert_eq!(
        runner.run_once(Queue::Imports, 1).await,
        vec![(id, "yield")]
    );
    let job = row(&test, id).await;
    assert_eq!(
        (job.state.as_str(), job.attempts, job.claims),
        ("available", 0, 1)
    );
    assert_eq!(job.progress, Some(json!({ "step": 1 })));
    assert_eq!(runner.run_once(Queue::Imports, 1).await, vec![(id, "done")]);
    let job = row(&test, id).await;
    assert_eq!(
        (job.state.as_str(), job.attempts, job.claims),
        ("completed", 0, 2)
    );
    assert_eq!(notes(&test, "two").await, 1);
}

/// A failing job runs again after the job backoff, counting each failure, and ends `failed`
/// with its error once its attempts are spent.
#[tokio::test]
async fn a_failing_job_is_retried_then_failed() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let runner = harness(&test, "worker-a");
    let id = enqueue_in(&test, acme.id, &Fails {}).await;
    assert_eq!(
        runner.run_once(Queue::Imports, 1).await,
        vec![(id, "retry")]
    );
    let job = row(&test, id).await;
    assert_eq!((job.state.as_str(), job.attempts), ("available", 1));
    assert!(
        (1.0..=60.0).contains(&job.wait_seconds),
        "{}",
        job.wait_seconds
    );
    assert_eq!(job.last_error.as_deref(), Some("error: planned failure"));
    sqlx::query!("UPDATE jobs SET run_at = now() WHERE id = $1", id.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    assert_eq!(
        runner.run_once(Queue::Imports, 1).await,
        vec![(id, "failed")]
    );
    let job = row(&test, id).await;
    assert_eq!((job.state.as_str(), job.attempts), ("failed", 2));
    assert!(job.finished);
}

/// A run that fails after marking its ambiguous external effect ends in `needs_review`, marker
/// kept: running it again could double the effect, so an operator decides.
#[tokio::test]
async fn a_failed_ambiguous_effect_needs_review() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let id = enqueue_in(&test, acme.id, &Ambiguous {}).await;
    assert_eq!(
        harness(&test, "worker-a").run_once(Queue::Imports, 1).await,
        vec![(id, "needs_review")]
    );
    let job = row(&test, id).await;
    assert_eq!(job.state, "needs_review");
    assert!(job.marked && job.lease_owner.is_none());
    assert_eq!(lane(&test, acme.id, Queue::Imports).await.0, 0);
}

/// A job the worker must not run fails without running: an unknown kind (enqueued by another
/// release), a payload version the kind does not know, and a system kind outside the `system`
/// workspace.
#[tokio::test]
async fn a_job_the_worker_must_not_run_fails_without_running() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let mut tx = test.worker.begin_in(acme.id).await.unwrap();
    let unknown = NewJob {
        kind: "test.unknown",
        queue: Queue::Maintenance,
        max_attempts: 10,
        payload: json!({ "version": 1 }),
        unique_key: None,
    };
    let unknown = enqueue_value(&mut tx, acme.id, &unknown, None)
        .await
        .unwrap();
    let newer = NewJob {
        kind: Note::KIND,
        queue: Queue::Maintenance,
        max_attempts: 10,
        payload: json!({ "tag": "newer", "version": 9 }),
        unique_key: None,
    };
    let newer = enqueue_value(&mut tx, acme.id, &newer, None).await.unwrap();
    let misplaced = enqueue(&mut tx, acme.id, &PartitionsCreate {}, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // The maintenance lane admits one job at a time.
    let runner = harness(&test, "worker-a");
    for _ in 0..3 {
        assert_eq!(
            runner
                .run_once(Queue::Maintenance, 1)
                .await
                .first()
                .map(|(_, outcome)| *outcome),
            Some("failed")
        );
    }
    for (id, code) in [
        (unknown, "unknown_kind:"),
        (newer, "unknown_version:"),
        (misplaced, "wrong_class:"),
    ] {
        let job = row(&test, id).await;
        assert_eq!(job.state, "failed");
        assert!(
            job.last_error
                .as_deref()
                .is_some_and(|error| error.starts_with(code)),
            "{:?}",
            job.last_error
        );
    }
    assert_eq!(notes(&test, "newer").await, 0);
}

/// Cancellation reaches a running job at its next checkpoint: the job stops and ends
/// `cancelled`, not available again.
#[tokio::test]
async fn a_running_job_observes_cancellation_at_its_next_checkpoint() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let id = enqueue_in(
        &test,
        acme.id,
        &Patient {
            cancel_midway: true,
        },
    )
    .await;
    assert_eq!(
        harness(&test, "worker-a").run_once(Queue::Imports, 1).await,
        vec![(id, "discard")]
    );
    let job = row(&test, id).await;
    assert_eq!(job.state, "cancelled");
    assert!(job.finished);
    assert_eq!(lane(&test, acme.id, Queue::Imports).await.0, 0);
}

/// When the process is asked to stop, a running job yields at its next chunk boundary and stays
/// available for another worker, without counting a failure.
#[tokio::test]
async fn a_stop_makes_a_running_job_yield() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let runner = harness(&test, "worker-a");
    runner.stop();
    let id = enqueue_in(
        &test,
        acme.id,
        &Patient {
            cancel_midway: false,
        },
    )
    .await;
    assert_eq!(
        runner.run_once(Queue::Imports, 1).await,
        vec![(id, "yield")]
    );
    let job = row(&test, id).await;
    assert_eq!((job.state.as_str(), job.attempts), ("available", 0));
}

/// An expired lease (its worker died) is recovered to run again after the job backoff, counting
/// a failure and freeing its lane slot: a crashed worker's job is never lost.
#[tokio::test]
async fn an_expired_lease_is_recovered_to_run_again() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let id = enqueue_in(
        &test,
        acme.id,
        &Note {
            tag: "crashed".to_owned(),
        },
    )
    .await;
    assert_eq!(
        lanes::claim(&test.worker, Queue::Imports, "crashed-worker", 1)
            .await
            .unwrap()
            .len(),
        1
    );
    expire(&test, id).await;
    let recovered = lanes::recover(&test.worker, &Registry::default(), || 0)
        .await
        .unwrap();
    assert_eq!(
        recovered
            .iter()
            .map(|job| job.state.as_str())
            .collect::<Vec<_>>(),
        ["available"]
    );
    let job = row(&test, id).await;
    assert_eq!((job.state.as_str(), job.attempts), ("available", 1));
    assert!(job.lease_owner.is_none());
    assert!(
        job.last_error
            .as_deref()
            .is_some_and(|error| error.starts_with("lease_expired:"))
    );
    assert!(
        (job.wait_seconds - 1.0).abs() < 0.001,
        "{}",
        job.wait_seconds
    );
    assert_eq!(lane(&test, acme.id, Queue::Imports).await.0, 0);
}

/// An expired lease whose ambiguous effect was marked goes to `needs_review` instead of running
/// again: the effect may have happened.
#[tokio::test]
async fn an_expired_lease_after_a_marked_effect_needs_review() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let id = enqueue_in(&test, acme.id, &Ambiguous {}).await;
    lanes::claim(&test.worker, Queue::Imports, "crashed-worker", 1)
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE jobs SET effect_started_at = now() WHERE id = $1",
        id.uuid()
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    expire(&test, id).await;
    lanes::recover(&test.worker, &Registry::default(), || 0)
        .await
        .unwrap();
    assert_eq!(row(&test, id).await.state, "needs_review");
    assert_eq!(lane(&test, acme.id, Queue::Imports).await.0, 0);
}

/// An expired lease on a job's last attempt fails the job, its finish time recorded although the
/// recovery runs as the scheduler role (which cannot write it), so finished-job pruning finds it.
#[tokio::test]
async fn an_expired_lease_on_the_last_attempt_fails_the_job() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let id = enqueue_in(
        &test,
        acme.id,
        &Note {
            tag: "last".to_owned(),
        },
    )
    .await;
    lanes::claim(&test.worker, Queue::Imports, "crashed-worker", 1)
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE jobs SET attempts = max_attempts - 1 WHERE id = $1",
        id.uuid()
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    expire(&test, id).await;
    lanes::recover(&test.worker, &Registry::default(), || 0)
        .await
        .unwrap();
    let job = row(&test, id).await;
    assert_eq!(job.state, "failed");
    assert!(job.finished);
}

/// A due schedule enqueues its job once and moves to its next instant; when it falls due again
/// while that job still waits, the tick coalesces with it instead of piling up runs.
#[tokio::test]
async fn a_due_schedule_enqueues_its_job_once() {
    let test = TestDb::new().await;
    let registry = registry();
    assert_eq!(schedules::seed(&test.system, &registry).await.unwrap(), 1);
    assert_eq!(
        schedules::tick(&test.worker, &registry).await.unwrap(),
        vec![Queue::Maintenance]
    );
    assert!(
        schedules::tick(&test.worker, &registry)
            .await
            .unwrap()
            .is_empty()
    );
    let next = sqlx::query_scalar!(
        r#"SELECT next_run_at > now() AS "later!" FROM job_schedules WHERE name = 'partitions.create'"#
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert!(next);
    sqlx::query!("UPDATE job_schedules SET next_run_at = now()")
        .execute(test.system.pool())
        .await
        .unwrap();
    assert_eq!(
        schedules::tick(&test.worker, &registry).await.unwrap(),
        vec![Queue::Maintenance]
    );
    let waiting = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM jobs WHERE kind = 'partitions.create' AND workspace_id = $1"#,
        SYSTEM_WORKSPACE.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(waiting, 1);
}

/// The system kind `partitions.create` runs through the system login on the maintenance lane and
/// leaves every partitioned table covered at least two days ahead.
#[tokio::test]
async fn partitions_create_keeps_the_leaves_two_days_ahead() {
    let test = TestDb::new().await;
    let mut tx = test.worker.begin_in(SYSTEM_WORKSPACE).await.unwrap();
    let id = enqueue(&mut tx, SYSTEM_WORKSPACE, &PartitionsCreate {}, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        harness(&test, "worker-a")
            .run_once(Queue::Maintenance, 1)
            .await,
        vec![(id, "done")]
    );
    let uncovered = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM partition_policies p
            WHERE NOT EXISTS (SELECT 1 FROM partition_leaves l
                               WHERE l.parent = p.table_name AND l.lower <= now() + interval '2 days'
                                 AND l.upper > now() + interval '2 days')"#
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(uncovered, 0);
}

/// `partitions.create` also refreshes the statistics of the partitioned parents: a parent holding
/// a row has statistics over its whole tree (`inherited`) only once the job ran. Autovacuum
/// analyzes the leaves but never a partitioned parent, so without the job the planner would
/// estimate every query through a parent (a join, a range over several periods) from nothing.
#[tokio::test]
async fn partitions_create_analyzes_the_parents() {
    let test = TestDb::new().await;
    sqlx::query!(
        "INSERT INTO outbox_events (workspace_id, type, subject_type, subject_id, payload)
         VALUES ($1, 'message.sent', 'message', uuidv7(), '{}')",
        SYSTEM_WORKSPACE.uuid(),
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    assert_eq!(statistics_of_the_outbox_tree(&test).await, 0);
    let mut tx = test.worker.begin_in(SYSTEM_WORKSPACE).await.unwrap();
    let id = enqueue(&mut tx, SYSTEM_WORKSPACE, &PartitionsCreate {}, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        harness(&test, "worker-a")
            .run_once(Queue::Maintenance, 1)
            .await,
        vec![(id, "done")]
    );
    assert!(statistics_of_the_outbox_tree(&test).await > 0);
}

/// The statistics rows of `outbox_events` over its whole tree, which only an `ANALYZE` of the
/// parent writes.
async fn statistics_of_the_outbox_tree(test: &TestDb) -> i64 {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM pg_stats
            WHERE schemaname = 'public' AND tablename = 'outbox_events' AND inherited"#
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap()
}

/// Two workers draining the same queue at once run every job exactly once: each effect commits
/// once, each job is claimed once and completes, and every lane ends empty.
#[tokio::test]
async fn two_workers_run_each_job_exactly_once() {
    let test = TestDb::new().await;
    let mut tags = Vec::new();
    for workspace in ["acme", "globex", "initech"] {
        let workspace = test.workspace(workspace).await;
        for index in 0..10 {
            let tag = format!("{}-{index}", workspace.id);
            enqueue_in(&test, workspace.id, &Note { tag: tag.clone() }).await;
            tags.push(tag);
        }
    }
    let drain = |runner: Harness| async move {
        let mut ran = 0;
        loop {
            let outcomes = runner.run_once(Queue::Imports, 3).await;
            if outcomes.is_empty() {
                return ran;
            }
            assert!(
                outcomes.iter().all(|(_, outcome)| *outcome == "done"),
                "{outcomes:?}"
            );
            ran += outcomes.len();
        }
    };
    let (a, b) = tokio::join!(
        drain(harness(&test, "worker-a")),
        drain(harness(&test, "worker-b"))
    );
    assert_eq!(a + b, 30);
    for tag in &tags {
        assert_eq!(notes(&test, tag).await, 1, "{tag}");
    }
    let once = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM jobs WHERE kind = 'test.note' AND state = 'completed' AND claims = 1"#
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(once, 30);
    let busy = sqlx::query_scalar!(r#"SELECT coalesce(sum(running), 0) AS "busy!" FROM job_lanes"#)
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(busy, 0);
}
