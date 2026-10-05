//! `senders.removed`: what happens to the conversations of a sender that left a campaign's pool.
//!
//! # When it runs
//!
//! Every removal writes the row a Start share-locks and enqueues this job in the same
//! transaction ([`enqueue`]):
//!
//! | Removal | The row it writes | The job's scope |
//! |---|---|---|
//! | identities taken out of a campaign's `senders.identity_ids`, or a tag out of its `senders.tags` | the campaign | [`Scope::Campaign`] |
//! | a tag taken off an identity, or the identity disabled | the identity | [`Scope::Identities`] |
//! | a connection archived (its identities are archived with it) | the connection | [`Scope::Connection`] |
//!
//! A removal committed before a Start's locks makes that Start return its message unstarted; a
//! Start that committed first submits, because a started submission is never recalled.
//!
//! # What it does
//!
//! It reads the pool as it is when it runs, never the removal's own idea of it: a sender that
//! was put back before the job ran keeps its conversations. In batches, under each enrollment's
//! lock (then the queue rows and messages, in message order), for every live conversation whose
//! current message is from a sender no longer in its campaign's pool:
//!
//! - a message still `queued` is cancelled (`status_detail` "sender removed") and its queue row
//!   deleted; then, under the campaign's `on_sender_removed`, `reassign` re-arms the step (no
//!   message, no thread root, its affinity dropped, due now), so the
//!   one creation contract makes exactly one new message from another sender of the pool, in a
//!   new thread, generating AI content anew where the step uses it; `stop` stops the enrollment
//!   ("sender removed");
//! - a message a sender has `claimed` is left to its Start, which sees the removal and returns
//!   it to the queue, releasing its reservation once: the job yields
//!   (`Yield { after: 60 s }`) while any unstarted message (queued or claimed) of a removed
//!   sender is left, so a later run cancels it;
//! - a message `in_flight` finishes as it is.
//!
//! Conversations waiting between steps (no message) need nothing here: the creation pass reads
//! the pool when their next step is due and applies the same rule then.
//!
//! An archived connection also ends its mail that is not campaign mail: direct messages and
//! replies still queued on it fail at once ("sender archived"), since the API caller chose that
//! sender and no other is substituted.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::enrollments;
use crate::db::Tx;
use crate::domain::campaigns::{
    self as policy, EnrollmentStatus, OnSenderRemoved, Removal, Unsent,
};
use crate::domain::ids::{Campaign, Connection, Id, Message, SenderIdentity, WorkspaceId};
use crate::jobs::{self, Effect, Job, JobContext, JobError, Outcome, Queue};
use crate::webhooks::EventType;

/// Conversations handled per chunk.
const CHUNK: i64 = 500;
/// How long the job waits for claimed messages to come back from their Starts.
const WAIT: Duration = Duration::from_secs(60);
/// The `status_detail` of what a removal cancels or stops.
pub const SENDER_REMOVED: &str = "sender removed";
/// The `status_detail` of mail that fails because its connection was archived.
pub const SENDER_ARCHIVED: &str = "sender archived";

/// Whose conversations a removal concerns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// A campaign's pool lost identities or tags: its conversations whose sender left it.
    Campaign(Id<Campaign>),
    /// Identities lost a tag or were disabled: their conversations in every campaign they left.
    Identities(Vec<Id<SenderIdentity>>),
    /// A connection was archived: its identities' conversations in every campaign, and its
    /// queued mail that is not campaign mail.
    Connection(Id<Connection>),
}

/// `senders.removed`: see the module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendersRemoved {
    /// The removal: one job per removal.
    pub removal: Uuid,
    /// Whose conversations it concerns.
    pub scope: Scope,
}

/// Enqueues the job of a removal, in the transaction that writes it; the caller wakes the
/// `enrollment` queue after the commit (or leaves it to the next sweep).
///
/// # Errors
///
/// The database refused.
pub async fn enqueue(tx: &mut Tx, workspace: WorkspaceId, scope: Scope) -> Result<(), sqlx::Error> {
    jobs::enqueue(
        tx,
        workspace,
        &SendersRemoved {
            removal: Uuid::now_v7(),
            scope,
        },
        None,
    )
    .await?;
    Ok(())
}

impl Job for SendersRemoved {
    const KIND: &'static str = "senders.removed";
    const QUEUE: Queue = Queue::Enrollment;
    const EFFECT: Effect = Effect::Idempotent;

    fn unique_key(&self) -> Option<String> {
        Some(self.removal.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        let mut handled = cx
            .progress()
            .and_then(|progress| progress.get("handled"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        loop {
            if cx.should_yield() {
                return Ok(Outcome::Yield {
                    after: Duration::ZERO,
                });
            }
            let mut chunk = cx.begin().await?;
            let applied = apply(chunk.tx(), workspace, &self.scope, CHUNK).await?;
            let failed = if let Scope::Connection(connection) = &self.scope {
                fail_unpooled(chunk.tx(), workspace, *connection, CHUNK).await?
            } else {
                Vec::new()
            };
            let done = applied.saturating_add(failed.len());
            handled = handled.saturating_add(u64::try_from(done).unwrap_or(0));
            cx.checkpoint(chunk, json!({ "handled": handled })).await?;
            for message in failed {
                crate::delivery::evidence::report_failure(
                    workspace,
                    Id::<Message>::from_uuid(message),
                    crate::domain::messages::State::Failed,
                    Some(crate::domain::policy::delivery::Category::SenderArchived),
                );
            }
            if done == 0 {
                break;
            }
        }
        jobs::wake(cx.db(), Queue::Enrollment).await;
        let mut tx = cx.db().begin_in(workspace).await?;
        let left = unstarted(&mut tx, workspace, &self.scope).await?;
        tx.commit().await?;
        if left > 0 {
            Ok(Outcome::Yield { after: WAIT })
        } else {
            Ok(Outcome::Done)
        }
    }
}

/// The scope as the queries' filters: a campaign, identities, a connection.
fn filters(scope: &Scope) -> (Option<Uuid>, Option<Vec<Uuid>>, Option<Uuid>) {
    match scope {
        Scope::Campaign(campaign) => (Some(campaign.uuid()), None, None),
        Scope::Identities(identities) => (
            None,
            Some(identities.iter().map(|id| id.uuid()).collect()),
            None,
        ),
        Scope::Connection(connection) => (None, None, Some(connection.uuid())),
    }
}

/// Handles up to `limit` conversations of `scope` whose queued message is from a sender no
/// longer in its campaign's pool (see the module), in the caller's transaction; enrollments,
/// queue rows and messages another path holds are left for a later run. Returns how many it
/// handled.
///
/// # Errors
///
/// The database refused.
pub async fn apply(
    tx: &mut Tx,
    workspace: WorkspaceId,
    scope: &Scope,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    let (campaign, identities, connection) = filters(scope);
    let rows = sqlx::query!(
        r#"SELECT e.id, e.campaign_id, e.person_id, e.message_id AS "message_id!", c.on_sender_removed
             FROM enrollments e
             JOIN messages m ON m.workspace_id = e.workspace_id AND m.id = e.message_id
             JOIN delivery_queue q ON q.workspace_id = m.workspace_id AND q.message_id = m.id
             JOIN campaigns c ON c.workspace_id = e.workspace_id AND c.id = e.campaign_id
             JOIN sender_identities i ON i.workspace_id = m.workspace_id AND i.id = m.sender_identity_id
            WHERE e.workspace_id = $1 AND e.status = 'active' AND e.message_id IS NOT NULL AND q.state = 'queued'
              AND ($2::uuid IS NULL OR e.campaign_id = $2)
              AND ($3::uuid[] IS NULL OR m.sender_identity_id = ANY($3))
              AND ($4::uuid IS NULL OR m.connection_id = $4)
              AND NOT (i.enabled AND i.archived_at IS NULL
                       AND (i.tags && c.sender_tags
                            OR EXISTS (SELECT 1 FROM campaign_senders s
                                        WHERE s.workspace_id = c.workspace_id AND s.campaign_id = c.id
                                          AND s.sender_identity_id = i.id)))
            ORDER BY e.id LIMIT $5
              FOR UPDATE OF e SKIP LOCKED"#,
        workspace.uuid(),
        campaign,
        identities.as_deref(),
        connection,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    let messages: Vec<Uuid> = rows.iter().map(|row| row.message_id).collect();
    let cancelled = enrollments::cancel_queued(tx, workspace, &messages, SENDER_REMOVED).await?;
    let mut handled = 0_usize;
    for row in rows
        .iter()
        .filter(|row| cancelled.contains(&row.message_id))
    {
        let rule: OnSenderRemoved = row
            .on_sender_removed
            .parse()
            .unwrap_or(OnSenderRemoved::Reassign);
        match policy::on_removed(rule, Unsent::Queued) {
            Removal::CancelAndReassign => {
                sqlx::query!(
                    "UPDATE enrollments
                        SET message_id = NULL, thread_root_message_id = NULL, next_run_at = now()
                      WHERE workspace_id = $1 AND id = $2",
                    workspace.uuid(),
                    row.id,
                )
                .execute(&mut **tx)
                .await?;
                sqlx::query!(
                    "DELETE FROM campaign_sender_affinity WHERE workspace_id = $1 AND campaign_id = $2 AND person_id = $3",
                    workspace.uuid(),
                    row.campaign_id,
                    row.person_id,
                )
                .execute(&mut **tx)
                .await?;
            }
            Removal::CancelAndStop => {
                sqlx::query!(
                    "UPDATE enrollments SET message_id = NULL WHERE workspace_id = $1 AND id = $2",
                    workspace.uuid(),
                    row.id,
                )
                .execute(&mut **tx)
                .await?;
                enrollments::end(
                    tx,
                    workspace,
                    &[row.id],
                    EnrollmentStatus::Stopped,
                    Some(SENDER_REMOVED),
                )
                .await?;
            }
            Removal::Wait | Removal::Leave => {}
        }
        handled = handled.saturating_add(1);
    }
    Ok(handled)
}

/// Fails up to `limit` messages that are not campaign mail still queued on the archived
/// `connection` ("sender archived"), resolving the holds they caused: nothing will try them
/// again. Returns the messages that failed, for their `delivery.failure` events once the
/// transaction has committed.
async fn fail_unpooled(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    limit: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let failed = sqlx::query_scalar!(
        "WITH queued AS (
             SELECT q.message_id FROM delivery_queue q JOIN messages m ON m.workspace_id = q.workspace_id AND m.id = q.message_id
              WHERE q.workspace_id = $1 AND q.connection_id = $2 AND q.state = 'queued' AND m.kind <> 'campaign'
              ORDER BY q.message_id LIMIT $3
                FOR UPDATE OF q, m SKIP LOCKED)
         DELETE FROM delivery_queue q USING queued
          WHERE q.workspace_id = $1 AND q.message_id = queued.message_id
         RETURNING q.message_id",
        workspace.uuid(),
        connection.uuid(),
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    if failed.is_empty() {
        return Ok(failed);
    }
    sqlx::query!(
        "UPDATE messages SET state = 'failed', status_detail = $3 WHERE workspace_id = $1 AND id = ANY($2)",
        workspace.uuid(),
        &failed,
        SENDER_ARCHIVED,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE recipient_holds SET resolved_at = now(), resolution = 'expired'
          WHERE workspace_id = $1 AND message_id = ANY($2) AND resolved_at IS NULL",
        workspace.uuid(),
        &failed,
    )
    .execute(&mut **tx)
    .await?;
    let now = crate::process::now();
    for message in &failed {
        crate::delivery::evidence::tell(
            tx,
            workspace,
            EventType::MessageFailed,
            Id::<Message>::from_uuid(*message),
            None,
            None,
            now,
        )
        .await?;
    }
    Ok(failed)
}

/// How many unstarted messages (queued or claimed) of senders removed by `scope` are left:
/// campaign messages of live conversations whose sender is out of the pool, and, for an
/// archived connection, its mail that is not campaign mail. The job is complete at zero.
///
/// # Errors
///
/// The database refused.
pub async fn unstarted(
    tx: &mut Tx,
    workspace: WorkspaceId,
    scope: &Scope,
) -> Result<i64, sqlx::Error> {
    let (campaign, identities, connection) = filters(scope);
    sqlx::query_scalar!(
        r#"SELECT (SELECT count(*) FROM enrollments e
                     JOIN messages m ON m.workspace_id = e.workspace_id AND m.id = e.message_id
                     JOIN delivery_queue q ON q.workspace_id = m.workspace_id AND q.message_id = m.id
                     JOIN campaigns c ON c.workspace_id = e.workspace_id AND c.id = e.campaign_id
                     JOIN sender_identities i ON i.workspace_id = m.workspace_id AND i.id = m.sender_identity_id
                    WHERE e.workspace_id = $1 AND e.status = 'active' AND e.message_id IS NOT NULL
                      AND q.state IN ('queued', 'claimed')
                      AND ($2::uuid IS NULL OR e.campaign_id = $2)
                      AND ($3::uuid[] IS NULL OR m.sender_identity_id = ANY($3))
                      AND ($4::uuid IS NULL OR m.connection_id = $4)
                      AND NOT (i.enabled AND i.archived_at IS NULL
                               AND (i.tags && c.sender_tags
                                    OR EXISTS (SELECT 1 FROM campaign_senders s
                                                WHERE s.workspace_id = c.workspace_id AND s.campaign_id = c.id
                                                  AND s.sender_identity_id = i.id))))
                + (SELECT count(*) FROM delivery_queue q JOIN messages m ON m.workspace_id = q.workspace_id AND m.id = q.message_id
                    WHERE q.workspace_id = $1 AND $4::uuid IS NOT NULL AND q.connection_id = $4
                      AND q.state IN ('queued', 'claimed') AND m.kind <> 'campaign') AS "left!""#,
        workspace.uuid(),
        campaign,
        identities.as_deref(),
        connection,
    )
    .fetch_one(&mut **tx)
    .await
}
