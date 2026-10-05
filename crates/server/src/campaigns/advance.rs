//! `enrollment.advance`: every 5 minutes, on the grid, moves every workspace's campaigns on.
//!
//! It is a fan-out singleton of the `system` workspace: it reads its directory of workspaces
//! (`dispatch_workspaces`, which every enrollment's insert registers) as the scheduler role,
//! then works in each workspace's own transactions, one chunk at a time, each chunk committed
//! with its checkpoint. In each workspace, in this order:
//!
//! 1. **Settle** ([`settle`]): every enrollment whose current message ended moves on, as
//!    `domain::campaigns::settle` decides: to its next step, due the step's delay after the
//!    actual send; to `completed` after its last step; to a new try of the step after a failure
//!    that never reached a submission; or to `failed` or `stopped`. An enrollment that ended
//!    while its message was claimed gets that message cancelled once its Start has returned it
//!    to the queue. Settling clears `message_id`, so the index of enrollments in flight holds
//!    only the conversations still waiting on a message.
//! 2. **Archived campaigns**: the live enrollments of an archived campaign are stopped, their
//!    queued messages cancelled.
//! 3. **Create**: the creation pass ([`super::creator::pass`]) over the enrollments due before
//!    the mark after next: the current slot and one ahead, so a message is queued before its
//!    sender's phase in the next slot; a message created ahead waits for its due time. A run
//!    that was missed (the worker was down) leaves overdue enrollments, which the next run takes
//!    with the coming slot's: catch-up needs nothing more.
//! 4. **Complete**: an `active` campaign whose every enrollment ended is `completed`.
//!
//! The run's clock is read once, at its start, so every workspace is cut at the same mark.
//! Its progress is the last workspace it finished, and only that: every checkpoint inside a
//! workspace keeps naming the one before it (with the workspace in progress and its step as
//! information). A run that yields between workspaces continues after the last one it finished;
//! a run recovered in the middle of a workspace (its worker died, or its lease was lost) starts
//! that workspace again from its first step, and redoing part of a workspace is harmless:
//! settling clears the message it settled, and creating skips an enrollment that has its
//! message, so every enrollment moves on once.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::creator::{self, CHUNK};
use super::enrollments;
use crate::crypto::Keys;
use crate::db::Tx;
use crate::domain::campaigns::{
    self as policy, CampaignStatus, Ended, EnrollmentStatus, Settlement,
};
use crate::domain::ids::{Campaign, Id, WorkspaceId};
use crate::domain::messages::State as MessageState;
use crate::domain::retry;
use crate::domain::time::Timestamp;
use crate::jobs::{self, Class, Effect, Job, JobContext, JobError, Outcome, Queue};

/// Enrollments settled per chunk.
const SETTLE_CHUNK: i64 = 500;
/// Workspaces read from the directory per batch.
const WORKSPACES: i64 = 1_000;

/// `enrollment.advance`: see the module.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnrollmentAdvance {}

impl Job for EnrollmentAdvance {
    const KIND: &'static str = "enrollment.advance";
    const QUEUE: Queue = Queue::Enrollment;
    const EFFECT: Effect = Effect::Idempotent;
    const CLASS: Class = Class::FanOut;

    fn unique_key(&self) -> Option<String> {
        Some("singleton".to_owned())
    }

    fn schedule() -> Option<(&'static str, Self)> {
        Some(("*/5 * * * *", Self {}))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let keys = cx.env::<Keys>()?.clone();
        let now = jiff::Timestamp::now();
        let mut after: Option<Uuid> = cx
            .progress()
            .and_then(|progress| progress.get("workspace"))
            .and_then(serde_json::Value::as_str)
            .and_then(|text| text.parse().ok());
        loop {
            let mut directory = cx.directory().await?;
            let workspaces = sqlx::query_scalar!(
                "SELECT workspace_id FROM dispatch_workspaces
                  WHERE ($1::uuid IS NULL OR workspace_id > $1) ORDER BY workspace_id LIMIT $2",
                after,
                WORKSPACES,
            )
            .fetch_all(&mut *directory)
            .await?;
            directory.commit().await?;
            let last_batch = i64::try_from(workspaces.len()).unwrap_or(0) < WORKSPACES;
            for workspace in workspaces {
                if cx.should_yield() {
                    return Ok(Outcome::Yield {
                        after: Duration::ZERO,
                    });
                }
                advance(cx, &keys, WorkspaceId::trusted(workspace), after, now).await?;
                after = Some(workspace);
            }
            if last_batch {
                return Ok(Outcome::Done);
            }
        }
    }
}

/// Runs the four steps of the module in one workspace, a chunk at a time. Every checkpoint before
/// the last keeps `finished` (the last workspace this run finished, if any) as the run's
/// progress, so a run recovered in the middle of `workspace` starts it again instead of skipping
/// its rest; the last checkpoint records `workspace` itself as finished.
async fn advance(
    cx: &mut JobContext,
    keys: &Keys,
    workspace: WorkspaceId,
    finished: Option<Uuid>,
    now: jiff::Timestamp,
) -> Result<(), JobError> {
    let progress =
        |step: &str| json!({ "workspace": finished, "current": workspace.uuid(), "step": step });
    loop {
        let mut chunk = cx.begin_in(workspace).await?;
        let settled = settle(chunk.tx(), workspace, now, SETTLE_CHUNK).await?;
        cx.checkpoint(chunk, progress("settle")).await?;
        if settled < usize::try_from(SETTLE_CHUNK).unwrap_or(usize::MAX) {
            break;
        }
    }
    loop {
        let mut chunk = cx.begin_in(workspace).await?;
        let stopped = stop_archived(chunk.tx(), workspace, SETTLE_CHUNK).await?;
        cx.checkpoint(chunk, progress("archived")).await?;
        if stopped < usize::try_from(SETTLE_CHUNK).unwrap_or(usize::MAX) {
            break;
        }
    }
    let mut cursor = None;
    loop {
        let mut chunk = cx.begin_in(workspace).await?;
        let pass = creator::pass(chunk.tx(), keys, workspace, None, now, cursor, CHUNK).await?;
        cx.checkpoint(chunk, progress("create")).await?;
        if pass.created > 0 {
            crate::delivery::accept::wake(cx.db()).await;
        }
        if pass.generating > 0 {
            jobs::wake(cx.db(), Queue::Ai).await;
        }
        cursor = pass.cursor;
        if cursor.is_none() {
            break;
        }
    }
    let mut chunk = cx.begin_in(workspace).await?;
    complete(chunk.tx(), workspace).await?;
    cx.checkpoint(chunk, json!({ "workspace": workspace.uuid() }))
        .await?;
    Ok(())
}

/// Settles up to `limit` enrollments of `workspace` whose current message ended (see the
/// module), in the caller's transaction; enrollments another path holds are left for the next
/// run. Returns how many it looked at.
///
/// # Errors
///
/// The database refused.
pub async fn settle(
    tx: &mut Tx,
    workspace: WorkspaceId,
    now: jiff::Timestamp,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT e.id, e.status, e.attempts, e.message_id AS "message_id!", m.state, m.attempt_number,
                  m.sent_at AS "sent_at: Timestamp", m.status_detail,
                  (SELECT r.delay_seconds FROM steps s
                     JOIN step_revisions r ON r.workspace_id = s.workspace_id AND r.step_id = s.id AND r.revision = s.current_revision
                    WHERE s.workspace_id = e.workspace_id AND s.campaign_id = e.campaign_id
                      AND s.position = e.current_position + 1) AS next_delay
             FROM enrollments e
             JOIN messages m ON m.workspace_id = e.workspace_id AND m.id = e.message_id
            WHERE e.workspace_id = $1 AND e.message_id IS NOT NULL
              AND CASE WHEN e.status IN ('active', 'paused')
                       THEN m.state IN ('sent', 'failed', 'cancelled', 'suppressed')
                       ELSE m.state NOT IN ('claimed', 'in_flight', 'uncertain') END
            ORDER BY e.id LIMIT $2
              FOR UPDATE OF e SKIP LOCKED"#,
        workspace.uuid(),
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    let count = rows.len();
    for row in rows {
        let live = row
            .status
            .parse::<EnrollmentStatus>()
            .is_ok_and(EnrollmentStatus::is_live);
        if !live {
            // It ended while its message was claimed: cancel the message the Start returned.
            enrollments::cancel_queued(
                tx,
                workspace,
                &[row.message_id],
                "The enrollment ended before its message was sent.",
            )
            .await?;
            clear(tx, workspace, row.id).await?;
            continue;
        }
        let state = row.state.parse().unwrap_or(MessageState::Failed);
        let settlement = policy::settle(&Ended {
            state,
            submitted: row.attempt_number > 0,
            detail: row.status_detail.as_deref(),
            failures: row.attempts,
            next_delay_seconds: row.next_delay,
        });
        match settlement {
            Settlement::Wait => {}
            Settlement::Advance { delay } => {
                let sent_at = row.sent_at.map_or(now, |at| at.0);
                sqlx::query!(
                    "UPDATE enrollments
                        SET current_position = current_position + 1, message_id = NULL, attempts = 0, next_run_at = $3
                      WHERE workspace_id = $1 AND id = $2",
                    workspace.uuid(),
                    row.id,
                    Timestamp(policy::after_send(sent_at, delay)) as _,
                )
                .execute(&mut **tx)
                .await?;
            }
            Settlement::Retry { failures } => {
                let wait = retry::backoff(
                    u32::try_from(failures).unwrap_or(0),
                    &retry::DELIVERY,
                    jobs::draw(),
                );
                sqlx::query!(
                    "UPDATE enrollments SET message_id = NULL, attempts = $3, next_run_at = $4
                      WHERE workspace_id = $1 AND id = $2",
                    workspace.uuid(),
                    row.id,
                    failures,
                    Timestamp(now).plus(wait) as _,
                )
                .execute(&mut **tx)
                .await?;
            }
            Settlement::Complete => {
                clear(tx, workspace, row.id).await?;
                enrollments::end(tx, workspace, &[row.id], EnrollmentStatus::Completed, None)
                    .await?;
            }
            Settlement::Fail { detail } => {
                clear(tx, workspace, row.id).await?;
                enrollments::end(
                    tx,
                    workspace,
                    &[row.id],
                    EnrollmentStatus::Failed,
                    Some(&detail),
                )
                .await?;
            }
            Settlement::Stop { detail } => {
                clear(tx, workspace, row.id).await?;
                enrollments::end(
                    tx,
                    workspace,
                    &[row.id],
                    EnrollmentStatus::Stopped,
                    Some(&detail),
                )
                .await?;
            }
        }
    }
    Ok(count)
}

/// Clears an enrollment's pointer to its settled message.
async fn clear(tx: &mut Tx, workspace: WorkspaceId, enrollment: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE enrollments SET message_id = NULL WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        enrollment,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Stops up to `limit` live enrollments of `workspace`'s archived campaigns; returns how many.
async fn stop_archived(
    tx: &mut Tx,
    workspace: WorkspaceId,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    let ids = sqlx::query_scalar!(
        "SELECT e.id FROM campaigns c
           JOIN enrollments e ON e.workspace_id = c.workspace_id AND e.campaign_id = c.id AND e.status IN ('active', 'paused')
          WHERE c.workspace_id = $1 AND c.status = 'archived'
          ORDER BY e.id LIMIT $2",
        workspace.uuid(),
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(enrollments::end(
        tx,
        workspace,
        &ids,
        EnrollmentStatus::Stopped,
        Some("The campaign was archived."),
    )
    .await?
    .len())
}

/// Marks `completed` every `active` campaign of `workspace` whose every enrollment ended (one
/// at least); a campaign being enrolled into is locked and left for the next run.
async fn complete(tx: &mut Tx, workspace: WorkspaceId) -> Result<(), sqlx::Error> {
    let done = sqlx::query_scalar!(
        r#"SELECT c.id AS "id: Id<Campaign>" FROM campaigns c
            WHERE c.workspace_id = $1 AND c.status = 'active'
              AND EXISTS (SELECT 1 FROM enrollments e WHERE e.workspace_id = c.workspace_id AND e.campaign_id = c.id)
              AND NOT EXISTS (SELECT 1 FROM enrollments e
                               WHERE e.workspace_id = c.workspace_id AND e.campaign_id = c.id AND e.status IN ('active', 'paused'))
            ORDER BY c.id
              FOR UPDATE SKIP LOCKED"#,
        workspace.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    for campaign in done {
        super::set_status(tx, workspace, campaign, CampaignStatus::Completed).await?;
    }
    Ok(())
}
