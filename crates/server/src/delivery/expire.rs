//! Ending queued messages without a submission: `delivery.expire`, which fails the messages whose
//! deadline passed while they waited, and [`cancel`], a person's cancellation of one queued
//! message.
//!
//! # Expiry
//!
//! A message stops being tried at its deadline: the earlier of its own usefulness (a sign-in
//! code's ten minutes, an invitation's expiry) and its first actual submission plus the retry
//! window. Every Start checks the deadline first and fails an expired message there, but a
//! message whose connection is paused, waiting for a person or held by its breaker is never
//! claimed, so nothing would reach its Start. `delivery.expire` runs every 5 minutes on the grid
//! and fails each `queued` message past its deadline: its queue row is deleted, the message is
//! `failed` as expired, the holds it caused are resolved (nothing will try it again) and
//! `message.failed` is told. It is a fan-out kind: it finds the workspaces with such rows as the
//! scheduler role (the queue's lease and deadline columns), then works in each workspace's own
//! transaction, a chunk at a time; once a chunk has committed, each message it failed is
//! reported as the error-level `delivery.failure` event.
//!
//! # Cancelling
//!
//! Only a `queued` message may be cancelled: one a sender claimed or started is on its way and
//! cannot be recalled (`409 invalid_state`), and a final one has nothing left to cancel. The
//! cancellation deletes the queue row and makes the message `cancelled` and tells
//! `message.cancelled`.
//!
//! Lock order, as every delivery path: queue rows, then message rows, in message order. Neither
//! path needs the connection's row (they release no reservation: a queued message holds none),
//! so neither takes it.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::evidence;
use crate::db::Tx;
use crate::domain::ids::{Id, Message, WorkspaceId};
use crate::domain::messages::State as MessageState;
use crate::domain::policy::delivery::Category;
use crate::jobs::{Class, Effect, Job, JobContext, JobError, Outcome, Queue};
use crate::webhooks::EventType;

/// Workspaces the expiry visits per run; the rest wait for the next run, 5 minutes later.
const WORKSPACES_PER_RUN: i64 = 1_000;
/// Messages failed per chunk.
const CHUNK: i64 = 500;

/// `delivery.expire`: every 5 minutes, fails the queued messages past their deadline (see the
/// module).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeliveryExpire {}

impl Job for DeliveryExpire {
    const KIND: &'static str = "delivery.expire";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::Idempotent;
    const CLASS: Class = Class::FanOut;

    fn unique_key(&self) -> Option<String> {
        Some("singleton".to_owned())
    }

    fn schedule() -> Option<(&'static str, Self)> {
        Some(("*/5 * * * *", Self {}))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let mut directory = cx.directory().await?;
        let workspaces = sqlx::query_scalar!(
            "SELECT DISTINCT workspace_id FROM delivery_queue
              WHERE state = 'queued' AND deadline_at < now() LIMIT $1",
            WORKSPACES_PER_RUN,
        )
        .fetch_all(&mut *directory)
        .await?;
        directory.commit().await?;
        let mut expired = cx
            .progress()
            .and_then(|progress| progress.get("expired"))
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
                let failed = expire(chunk.tx(), workspace).await?;
                expired = expired.saturating_add(u64::try_from(failed.len()).unwrap_or(0));
                cx.checkpoint(chunk, json!({ "expired": expired })).await?;
                for message in &failed {
                    evidence::report_failure(
                        workspace,
                        Id::from_uuid(*message),
                        MessageState::Failed,
                        Some(Category::Expired),
                    );
                }
                if failed.len() < usize::try_from(CHUNK).unwrap_or(usize::MAX) {
                    break;
                }
            }
        }
        Ok(Outcome::Done)
    }
}

/// Fails one chunk of `workspace`'s queued messages past their deadline, in the caller's
/// transaction; rows another path holds are skipped (a claim that already took one fails it at
/// its Start). Returns the messages that failed, for their `delivery.failure` events once the
/// transaction has committed.
///
/// # Errors
///
/// The database refused.
pub(crate) async fn expire(tx: &mut Tx, workspace: WorkspaceId) -> Result<Vec<Uuid>, sqlx::Error> {
    let messages = sqlx::query_scalar!(
        "WITH due AS (
             SELECT q.message_id FROM delivery_queue q JOIN messages m ON m.workspace_id = q.workspace_id AND m.id = q.message_id
              WHERE q.workspace_id = $1 AND q.state = 'queued' AND q.deadline_at < now()
              ORDER BY q.message_id LIMIT $2
                FOR UPDATE OF q, m SKIP LOCKED)
         DELETE FROM delivery_queue q USING due
          WHERE q.workspace_id = $1 AND q.message_id = due.message_id
         RETURNING q.message_id",
        workspace.uuid(),
        CHUNK,
    )
    .fetch_all(&mut **tx)
    .await?;
    if messages.is_empty() {
        return Ok(messages);
    }
    sqlx::query!(
        "UPDATE messages SET state = 'failed', status_detail = 'It expired before it could be sent.'
          WHERE workspace_id = $1 AND id = ANY($2)",
        workspace.uuid(),
        &messages,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE recipient_holds SET resolved_at = now(), resolution = 'expired'
          WHERE workspace_id = $1 AND message_id = ANY($2) AND resolved_at IS NULL",
        workspace.uuid(),
        &messages,
    )
    .execute(&mut **tx)
    .await?;
    let now = crate::process::now();
    for message in &messages {
        evidence::tell(
            tx,
            workspace,
            EventType::MessageFailed,
            Id::from_uuid(*message),
            None,
            Some(Category::Expired),
            now,
        )
        .await?;
    }
    Ok(messages)
}

/// Why a message could not be cancelled.
#[derive(Debug, thiserror::Error)]
pub enum CancelError {
    /// No such message in the workspace (`404 not_found`).
    #[error("no such message")]
    NotFound,
    /// The message is not queued (`409 invalid_state`).
    #[error("only a queued message can be cancelled; this one is {0}")]
    NotQueued(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Cancels `message` of `workspace` while it is `queued`, in the caller's transaction (see the
/// module).
///
/// # Errors
///
/// [`CancelError::NotFound`], [`CancelError::NotQueued`] naming its state, or the database
/// refused.
pub async fn cancel(
    tx: &mut Tx,
    workspace: WorkspaceId,
    message: Id<Message>,
) -> Result<(), CancelError> {
    let queued = sqlx::query_scalar!(
        "SELECT q.state FROM delivery_queue q JOIN messages m ON m.workspace_id = q.workspace_id AND m.id = q.message_id
          WHERE q.workspace_id = $1 AND q.message_id = $2
            FOR UPDATE OF q, m",
        workspace.uuid(),
        message.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    match queued.as_deref() {
        Some("queued") => {}
        Some(state) => return Err(CancelError::NotQueued(state.to_owned())),
        None => {
            let state = sqlx::query_scalar!(
                "SELECT state FROM messages WHERE workspace_id = $1 AND id = $2",
                workspace.uuid(),
                message.uuid(),
            )
            .fetch_optional(&mut **tx)
            .await?;
            return Err(state.map_or(CancelError::NotFound, CancelError::NotQueued));
        }
    }
    sqlx::query!(
        "DELETE FROM delivery_queue WHERE workspace_id = $1 AND message_id = $2",
        workspace.uuid(),
        message.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE messages SET state = 'cancelled', status_detail = 'Cancelled while it was queued.'
          WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        message.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    evidence::tell(
        tx,
        workspace,
        EventType::MessageCancelled,
        message,
        None,
        None,
        crate::process::now(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
