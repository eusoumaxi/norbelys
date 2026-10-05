//! Warm-up: a new sending account earns its daily limit day by day.
//!
//! A connection that is warming (`warmup_stage` set) may use only its stage's share of its daily
//! limit (`domain::schedule::warmed_limit`: a tenth, then a fifth, and so on to the whole limit
//! over eight stages), because mailbox providers judge a sender partly by how its volume grows: a
//! new account that sends its full volume at once looks like a compromised one. The ramp is
//! practitioner guidance, not a published provider rule.
//!
//! `connections.warmup` runs once a day at 00:50 UTC and evaluates the UTC day that just ended
//! for every active warming connection (`domain::schedule::next_warmup_stage`): a clean day (mail
//! accepted, no complaint) advances one stage, and past the last stage the connection is warm
//! (`warmup_stage` null); a day with a complaint, or with nothing sent, holds the stage. Each
//! evaluation records the day it covered (`warmup_evaluated_on`) in the same statement that moves
//! the stage, and a connection is evaluated only for a day after the one it records, so a run
//! that is retried, or two runs, never advance a connection twice for one day.
//!
//! It is a fan-out kind: the workspaces with warming connections are found as the scheduler
//! (routing columns only), then each workspace is evaluated in its own tenant transaction, a
//! chunk of connections at a time, the connection rows locked `FOR UPDATE SKIP LOCKED` (a row a
//! claim holds is evaluated at the next run, which still finds its day unevaluated).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::Tx;
use crate::domain::ids::WorkspaceId;
use crate::domain::schedule::{WarmupDay, next_warmup_stage};
use crate::jobs::{Class, Effect, Job, JobContext, JobError, Outcome, Queue};

/// Workspaces one run visits; the rest wait for the next day's run.
const WORKSPACES_PER_RUN: i64 = 10_000;
/// Connections evaluated per chunk.
const CHUNK: i64 = 500;

/// `connections.warmup`: once a day, moves every active warming connection along its ramp (see
/// the module).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnectionsWarmup {}

impl Job for ConnectionsWarmup {
    const KIND: &'static str = "connections.warmup";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::Idempotent;
    const CLASS: Class = Class::FanOut;

    fn unique_key(&self) -> Option<String> {
        Some("singleton".to_owned())
    }

    fn schedule() -> Option<(&'static str, Self)> {
        Some(("50 0 * * *", Self {}))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let mut directory = cx.directory().await?;
        let workspaces = sqlx::query_scalar!(
            "SELECT DISTINCT workspace_id FROM connections
              WHERE status = 'active' AND warmup_stage IS NOT NULL LIMIT $1",
            WORKSPACES_PER_RUN,
        )
        .fetch_all(&mut *directory)
        .await?;
        directory.commit().await?;
        let mut evaluated = cx
            .progress()
            .and_then(|progress| progress.get("evaluated"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        for workspace in workspaces {
            let workspace = WorkspaceId::trusted(workspace);
            loop {
                if cx.should_yield() {
                    return Ok(Outcome::Yield {
                        after: Duration::ZERO,
                    });
                }
                let mut chunk = cx.begin_in(workspace).await?;
                let count = evaluate(chunk.tx(), workspace).await?;
                evaluated = evaluated.saturating_add(u64::try_from(count).unwrap_or(0));
                cx.checkpoint(chunk, json!({ "evaluated": evaluated }))
                    .await?;
                if count < usize::try_from(CHUNK).unwrap_or(usize::MAX) {
                    break;
                }
            }
        }
        tracing::info!(
            event = "connections.warmup",
            evaluated,
            "warming connections evaluated"
        );
        Ok(Outcome::Done)
    }
}

/// Evaluates yesterday (UTC) for one chunk of `workspace`'s active warming connections not yet
/// evaluated for it, in the caller's transaction; returns how many were evaluated.
///
/// # Errors
///
/// The database refused.
pub(crate) async fn evaluate(tx: &mut Tx, workspace: WorkspaceId) -> Result<usize, sqlx::Error> {
    let days = sqlx::query!(
        r#"WITH due AS (
               SELECT c.id, c.warmup_stage
                 FROM connections c
                WHERE c.workspace_id = $1 AND c.status = 'active' AND c.warmup_stage IS NOT NULL
                  AND (c.warmup_evaluated_on IS NULL
                       OR c.warmup_evaluated_on < (now() AT TIME ZONE 'UTC')::date - 1)
                ORDER BY c.id
                LIMIT $2
                  FOR UPDATE OF c SKIP LOCKED)
           SELECT due.id, due.warmup_stage AS "stage!",
                  coalesce((SELECT u.used FROM connection_usage u
                             WHERE u.workspace_id = $1 AND u.connection_id = due.id
                               AND u.day = (now() AT TIME ZONE 'UTC')::date - 1), 0) AS "sent!",
                  (SELECT count(*) FROM delivery_events e
                     JOIN messages m ON m.workspace_id = e.workspace_id AND m.id = e.message_id
                    WHERE e.workspace_id = $1 AND m.connection_id = due.id AND e.kind = 'complaint'
                      AND e.observed_at >= ((now() AT TIME ZONE 'UTC')::date - 1)::timestamp AT TIME ZONE 'UTC'
                      AND e.observed_at < ((now() AT TIME ZONE 'UTC')::date)::timestamp AT TIME ZONE 'UTC') AS "complaints!"
             FROM due"#,
        workspace.uuid(),
        CHUNK,
    )
    .fetch_all(&mut **tx)
    .await?;
    for day in &days {
        let next = next_warmup_stage(
            Some(day.stage),
            WarmupDay {
                sent: u32::try_from(day.sent).unwrap_or(0),
                complaints: u32::try_from(day.complaints).unwrap_or(u32::MAX),
            },
        );
        sqlx::query!(
            "UPDATE connections
                SET warmup_stage = $3, warmup_evaluated_on = (now() AT TIME ZONE 'UTC')::date - 1
              WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            day.id,
            next,
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(days.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{SenderSpec, TestDb};

    /// Sets a connection's warm-up and yesterday's accepted count, as the claim and the Finish
    /// would have left them.
    async fn warming(test: &TestDb, connection: uuid::Uuid, stage: i16, sent_yesterday: i32) {
        sqlx::query("UPDATE connections SET warmup_stage = $2 WHERE id = $1")
            .bind(connection)
            .bind(stage)
            .execute(test.system.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO connection_usage (workspace_id, connection_id, day, used)
             SELECT workspace_id, id, (now() AT TIME ZONE 'UTC')::date - 1, $2 FROM connections WHERE id = $1",
        )
        .bind(connection)
        .bind(sent_yesterday)
        .execute(test.system.pool())
        .await
        .unwrap();
    }

    async fn stage(test: &TestDb, connection: uuid::Uuid) -> Option<i16> {
        sqlx::query_scalar("SELECT warmup_stage FROM connections WHERE id = $1")
            .bind(connection)
            .fetch_one(test.system.pool())
            .await
            .unwrap()
    }

    /// A clean day advances a warming connection one stage, a day with nothing sent holds it,
    /// and evaluating the same day again changes nothing: a retried or doubled run never moves a
    /// connection twice for one day, so the ramp is exactly one stage per clean day.
    #[tokio::test]
    async fn a_clean_day_advances_one_stage_once() {
        let test = TestDb::new().await;
        let ws = test.workspace("acme").await.id;
        let clean = test
            .sender(ws, &SenderSpec::mailbox("ada@acme.test"))
            .await
            .connection
            .uuid();
        let idle = test
            .sender(ws, &SenderSpec::mailbox("grace@acme.test"))
            .await
            .connection
            .uuid();
        warming(&test, clean, 2, 40).await;
        warming(&test, idle, 2, 0).await;

        let mut tx = test.worker.begin_in(ws).await.unwrap();
        assert_eq!(evaluate(&mut tx, ws).await.unwrap(), 2);
        tx.commit().await.unwrap();
        assert_eq!(stage(&test, clean).await, Some(3));
        assert_eq!(stage(&test, idle).await, Some(2));

        let mut tx = test.worker.begin_in(ws).await.unwrap();
        assert_eq!(
            evaluate(&mut tx, ws).await.unwrap(),
            0,
            "the day is evaluated once"
        );
        tx.commit().await.unwrap();
        assert_eq!(stage(&test, clean).await, Some(3));
    }
}
