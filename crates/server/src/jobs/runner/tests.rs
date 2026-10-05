//! Gate tests of the runner against real PostgreSQL: a job far longer than one claim, and the
//! wake-up listener losing its connection.
//!
//! A long job is driven through [`Harness`], the runner's own claim and execution without its
//! loops, one claim at a time, so the test sees every generation's outcome. The listener test runs
//! the production [`listen_loop`] on a connection of its own and kills that connection's backend
//! with `pg_terminate_backend`, as a network cut or a database restart would, then watches each
//! queue's wake-up directly: what a claim loop waits for between its sweeps.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::postgres::PgListener;
use strum::IntoEnumIterator as _;

use super::{Harness, Shared, listen_loop};
use crate::db::Tx;
use crate::domain::ids::WorkspaceId;
use crate::jobs::{self, CHANNEL, Effect, Job, JobContext, JobError, Outcome, Queue, Registry};
use crate::process::Shutdown;
use crate::testing::TestDb;

/// The action of the audit rows [`Long`] writes as its effects.
const EFFECT: &str = "test.long";

/// Writes one effect tagged `tag` in a chunk's transaction: an audit row, counted afterwards.
async fn effect(tx: &mut Tx, workspace: WorkspaceId, tag: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_log (workspace_id, actor_kind, actor_id, action, target) VALUES ($1, 'system', 'test', $2, $3)",
    )
    .bind(workspace.uuid())
    .bind(EFFECT)
    .bind(tag)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// How many effects tagged `tag` committed.
async fn effects(test: &TestDb, tag: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = $1 AND target = $2")
        .bind(EFFECT)
        .bind(tag)
        .fetch_one(test.system.pool())
        .await
        .unwrap()
}

/// Works through `chunks` chunks, one effect each, and yields its lane slot after `per_run` chunks
/// of a claim (or earlier, when the runner's quantum says so). The last chunk also writes the
/// `<tag>:finished` effect, in the same transaction as its progress, so a run after the last
/// checkpoint finds nothing left and the job finishes once whatever happens.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Long {
    tag: String,
    chunks: u64,
    per_run: u64,
}

impl Job for Long {
    const KIND: &'static str = "test.long";
    const QUEUE: Queue = Queue::Imports;
    const EFFECT: Effect = Effect::Idempotent;

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let mut done = cx
            .progress()
            .and_then(|progress| progress.get("done"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut this_run = 0;
        while done < self.chunks {
            if this_run == self.per_run || cx.should_yield() {
                return Ok(Outcome::Yield {
                    after: Duration::ZERO,
                });
            }
            let mut chunk = cx.begin().await?;
            done += 1;
            effect(chunk.tx(), cx.workspace(), &self.tag).await?;
            if done == self.chunks {
                effect(
                    chunk.tx(),
                    cx.workspace(),
                    &format!("{}:finished", self.tag),
                )
                .await?;
            }
            cx.checkpoint(chunk, json!({ "done": done })).await?;
            this_run += 1;
        }
        Ok(Outcome::Done)
    }
}

/// A job of 1,000 chunks that yields every 90 (more often than the runner's quantum of 200, as a
/// kind with a tighter budget of its own does) runs over at least twelve claims, beyond the ten
/// failed attempts that would end a job, and completes: yields never count as failures, every
/// chunk's effect commits exactly once (1,000 effects, none lost or replayed, through eleven
/// resumptions from the checkpointed progress), the job finishes once, and its lane slot is free.
#[tokio::test]
async fn a_long_job_yields_beyond_ten_generations_and_finishes_once() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let mut registry = Registry::default();
    registry.register::<Long>().unwrap();
    let runner = Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        http::Extensions::new(),
        "worker-long",
    );
    let mut tx = test.app.begin_in(acme.id).await.unwrap();
    let id = jobs::enqueue(
        &mut tx,
        acme.id,
        &Long {
            tag: "long".to_owned(),
            chunks: 1_000,
            per_run: 90,
        },
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut outcomes = Vec::new();
    loop {
        let ran = runner.run_once(Queue::Imports, 1).await;
        assert_eq!(
            ran.iter().map(|(job, _)| *job).collect::<Vec<_>>(),
            [id],
            "the job is claimed again at once after each yield: {outcomes:?}"
        );
        let outcome = ran[0].1;
        outcomes.push(outcome);
        if outcome != "yield" {
            break;
        }
        assert!(outcomes.len() < 40, "the job never ends: {outcomes:?}");
    }
    assert_eq!(outcomes.last(), Some(&"done"), "{outcomes:?}");
    assert!(outcomes.len() >= 12, "{outcomes:?}");

    let (state, attempts, claims, finished): (String, i16, i32, bool) = sqlx::query_as(
        "SELECT state, attempts, claims, finished_at IS NOT NULL FROM jobs WHERE id = $1",
    )
    .bind(id.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!((state.as_str(), attempts, finished), ("completed", 0, true));
    assert_eq!(usize::try_from(claims).unwrap(), outcomes.len());
    assert_eq!(effects(&test, "long").await, 1_000);
    assert_eq!(effects(&test, "long:finished").await, 1);
    let running: i32 = sqlx::query_scalar(
        "SELECT running FROM job_lanes WHERE workspace_id = $1 AND queue = 'imports'",
    )
    .bind(acme.id.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(running, 0);
}

/// True when `queue`'s wake-up comes within `within`, consuming it. A wake-up sent while no claim
/// loop waits is kept (one at most) until the loop next waits, as the claim loop relies on.
async fn woken(shared: &Shared, queue: Queue, within: Duration) -> bool {
    let notify = Arc::clone(
        shared
            .wakes
            .get(&queue)
            .expect("every queue has its wake-up"),
    );
    tokio::time::timeout(within, notify.notified())
        .await
        .is_ok()
}

/// A wake-up wakes the queue it names and no other. When the listener's connection dies, the
/// listener connects again, takes up `LISTEN` before anything else, and wakes every queue, once,
/// so each claim loop sweeps for the work whose notification was lost while the connection was
/// down; and a notification sent right after that wake-up is received, so no work can fall between
/// the reconnection and the sweeps. Without the wake-up, work enqueued during the cut would wait
/// for the 30-second sweep; with `LISTEN` taken up after it, a notification could be lost for good.
#[tokio::test]
async fn a_lost_listener_wakes_every_queue_once_listening_again() {
    let test = TestDb::new().await;
    let (shutdown, stop) = Shutdown::manual();
    let shared = Arc::new(Shared::new(
        test.worker.clone(),
        test.system.clone(),
        Registry::default(),
        http::Extensions::new(),
        Arc::from("worker-listener"),
        shutdown,
    ));
    let own = test.worker_pool(2).await;
    let mut listener = PgListener::connect_with(own.pool()).await.unwrap();
    listener.listen(CHANNEL).await.unwrap();
    let backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut listener)
        .await
        .unwrap();
    let task = tokio::spawn(listen_loop(Arc::clone(&shared), listener));

    jobs::wake(&test.worker, Queue::Imports).await;
    assert!(woken(&shared, Queue::Imports, Duration::from_secs(10)).await);
    for queue in Queue::iter().filter(|queue| *queue != Queue::Imports) {
        assert!(
            !woken(&shared, queue, Duration::from_millis(50)).await,
            "{queue:?} was woken by another queue's notification"
        );
    }

    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(backend)
        .fetch_one(test.worker.pool())
        .await
        .unwrap();
    assert!(terminated);
    for queue in Queue::iter() {
        assert!(
            woken(&shared, queue, Duration::from_secs(10)).await,
            "{queue:?} was not woken after the listener lost its connection"
        );
    }
    // A listener that kept failing would wake every queue again at each of its retries.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    for queue in Queue::iter() {
        assert!(
            !woken(&shared, queue, Duration::from_millis(10)).await,
            "{queue:?} was woken again: the listener did not recover"
        );
    }

    jobs::wake(&test.worker, Queue::Webhooks).await;
    assert!(
        woken(&shared, Queue::Webhooks, Duration::from_secs(10)).await,
        "a notification sent after the reconnection's wake-up was lost"
    );

    let _ = stop.send(true);
    task.await.unwrap();
}
