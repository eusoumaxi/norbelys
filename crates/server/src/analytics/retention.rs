//! `retention.prune`: deletes the rows of the unpartitioned short-lived tables once nothing
//! needs them, in batches, and starts the erasure of workspaces whose tombstone has passed.
//!
//! | Rows | Deleted when |
//! |---|---|
//! | `connection_usage`, `quota_scope_usage` (daily sending ledgers) | older than 35 days and no attempt still references the day |
//! | `idempotency_keys` | expired (24 hours after the request) |
//! | `recipient_validations` (the preflight cache) | expired (a day after DNS answered): an expired verdict is never read again |
//! | `provider_event_keys` | received more than 3 days ago, longer than any provider retries an event |
//! | `login_codes`, `auth_ceremonies`, `oauth_codes`, `oauth_device_codes` | expired for a day (no rate window counts them any longer; a device code's grant outlives it) |
//! | `jobs` | finished (completed, failed or cancelled) more than 7 days ago, kept that long for inspection |
//! | `sessions` | revoked, or expired, more than 30 days ago |
//! | `audit_log`, `user_audit_log` | older than 180 days, past what the logs show |
//! | `exports` | expired (their file was kept 7 days), right after their file |
//!
//! Each statement deletes at most [`BATCH`] rows and commits with the job's checkpoint, so a
//! large backlog never holds locks or a transaction for long, and a crash repeats at most one
//! batch (deleting what is due is idempotent). The time-partitioned tables are not pruned here:
//! their periods leave whole through `archive.export`.
//!
//! What object storage keeps is pruned here too, never by a bucket's lifecycle rule, so moving
//! to another provider moves the behaviour with it: an expired export's file (where its job
//! writes it, ready or not), then the export's row; and an import's uploaded file whose import
//! row never committed (its request failed, or its process died, between the upload and the
//! commit) once it is [`ABANDONED_AFTER_DAYS`] old, longer than any request takes. Every file is
//! a request of its own, so files go [`FILES`] at a time, each batch followed by a checkpoint or
//! a renewal of the lease; an object already gone counts as deleted, so a run repeated after a
//! crash deletes nothing twice. Calling object storage makes the kind `ExternalRetryable`.
//!
//! A workspace whose deletion was requested 30 days ago and is still marked deleted gets its
//! `workspace.delete` job, enqueued in the `system` workspace and recorded on the request.

use std::collections::BTreeMap;

use futures_util::TryStreamExt as _;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::deletion::WorkspaceDelete;
use crate::db::Tx;
use crate::domain::ids::{Id, WorkspaceId};
use crate::jobs::{
    self, Class, Effect, Job, JobContext, JobError, Outcome, Queue, SYSTEM_WORKSPACE,
};
use crate::people::exports::{Format, object_key};
use crate::storage::{Storage, StorageError};

/// Rows one statement deletes at most.
pub const BATCH: i64 = 10_000;
/// Files deleted from object storage between two checkpoints or renewals of the job's lease:
/// each file is a request of its own, and the lease lasts a minute.
const FILES: i64 = 100;
/// How old an uploaded import file is, in days, before it counts as abandoned when no import row
/// names it: the request that uploads the file commits the row a moment later.
const ABANDONED_AFTER_DAYS: i32 = 1;

/// The kinds of rows pruned, in the order they are visited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum Pruned {
    MessageContents,
    Attachments,
    ConnectionUsage,
    QuotaScopeUsage,
    IdempotencyKeys,
    RecipientValidations,
    ProviderEventKeys,
    LoginCodes,
    AuthCeremonies,
    OauthCodes,
    OauthDeviceCodes,
    Jobs,
    Sessions,
    AuditLog,
    UserAuditLog,
}

/// Deletes one batch of `pruned`'s due rows inside `tx`; returns how many were deleted.
async fn prune(tx: &mut Tx, pruned: Pruned) -> Result<u64, sqlx::Error> {
    let done = match pruned {
        Pruned::MessageContents => sqlx::query("DELETE FROM message_contents WHERE (workspace_id,id) IN (SELECT c.workspace_id,c.id FROM message_contents c WHERE (direction='outbound' AND NOT EXISTS(SELECT 1 FROM messages m WHERE m.workspace_id=c.workspace_id AND m.id=c.id)) OR (direction='inbound' AND NOT EXISTS(SELECT 1 FROM inbound_messages m WHERE m.workspace_id=c.workspace_id AND m.id=c.id)) LIMIT $1)").bind(BATCH).execute(&mut **tx).await?,
        Pruned::Attachments => sqlx::query("DELETE FROM attachments WHERE (workspace_id,id) IN (SELECT a.workspace_id,a.id FROM attachments a WHERE created_at < now()-interval '1 day' AND NOT EXISTS(SELECT 1 FROM message_attachments m WHERE m.workspace_id=a.workspace_id AND m.attachment_id=a.id) LIMIT $1)").bind(BATCH).execute(&mut **tx).await?,
        Pruned::ConnectionUsage => sqlx::query!(
            "DELETE FROM connection_usage WHERE ctid IN (
                 SELECT u.ctid FROM connection_usage u
                  WHERE u.day < (now() AT TIME ZONE 'UTC')::date - 35
                    AND NOT EXISTS (SELECT 1 FROM attempts a WHERE a.workspace_id = u.workspace_id
                                       AND a.connection_id = u.connection_id AND a.reserved_day = u.day)
                  LIMIT $1)",
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        Pruned::QuotaScopeUsage => sqlx::query!(
            "DELETE FROM quota_scope_usage WHERE ctid IN (
                 SELECT u.ctid FROM quota_scope_usage u
                  WHERE u.day < (now() AT TIME ZONE 'UTC')::date - 35
                    AND NOT EXISTS (SELECT 1 FROM attempts a WHERE a.workspace_id = u.workspace_id
                                       AND a.quota_scope_id = u.scope_id AND a.reserved_day = u.day)
                  LIMIT $1)",
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        Pruned::IdempotencyKeys => sqlx::query!(
            "DELETE FROM idempotency_keys WHERE id IN (
                 SELECT id FROM idempotency_keys WHERE expires_at < now() LIMIT $1)",
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        Pruned::RecipientValidations => sqlx::query!(
            "DELETE FROM recipient_validations WHERE (workspace_id, email_key) IN (
                 SELECT workspace_id, email_key FROM recipient_validations WHERE expires_at < now() LIMIT $1)",
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        Pruned::ProviderEventKeys => sqlx::query!(
            "DELETE FROM provider_event_keys WHERE (tableoid, ctid) IN (
                 SELECT tableoid, ctid FROM provider_event_keys WHERE received_at < now() - interval '3 days' LIMIT $1)",
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        Pruned::LoginCodes => sqlx::query!(
            "DELETE FROM login_codes WHERE id IN (
                 SELECT id FROM login_codes WHERE expires_at < now() - interval '1 day' LIMIT $1)",
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        Pruned::AuthCeremonies => sqlx::query!(
            "DELETE FROM auth_ceremonies WHERE id IN (
                 SELECT id FROM auth_ceremonies WHERE expires_at < now() - interval '1 day' LIMIT $1)",
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        Pruned::OauthCodes => sqlx::query!(
            "DELETE FROM oauth_codes WHERE code_hash IN (
                 SELECT code_hash FROM oauth_codes WHERE expires_at < now() - interval '1 day' LIMIT $1)",
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        Pruned::OauthDeviceCodes => sqlx::query!(
            "DELETE FROM oauth_device_codes WHERE device_code_hash IN (
                 SELECT device_code_hash FROM oauth_device_codes WHERE expires_at < now() - interval '1 day' LIMIT $1)",
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        Pruned::Jobs => sqlx::query!(
            "DELETE FROM jobs WHERE id IN (
                 SELECT id FROM jobs WHERE state IN ('completed', 'failed', 'cancelled')
                    AND finished_at < now() - interval '7 days' LIMIT $1)",
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        Pruned::Sessions => sqlx::query!(
            "DELETE FROM sessions WHERE id IN (
                 SELECT id FROM sessions
                  WHERE revoked_at < now() - interval '30 days'
                     OR least(expires_at, idle_expires_at) < now() - interval '30 days'
                  LIMIT $1)",
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        Pruned::AuditLog => sqlx::query!(
            "DELETE FROM audit_log WHERE (workspace_id, id) IN (
                 SELECT workspace_id, id FROM audit_log
                  WHERE created_at < now() - make_interval(days => $2) LIMIT $1)",
            BATCH,
            crate::identity::audit::RETENTION_DAYS,
        )
        .execute(&mut **tx)
        .await?,
        Pruned::UserAuditLog => sqlx::query!(
            "DELETE FROM user_audit_log WHERE (user_id, id) IN (
                 SELECT user_id, id FROM user_audit_log
                  WHERE created_at < now() - make_interval(days => $2) LIMIT $1)",
            BATCH,
            crate::identity::audit::RETENTION_DAYS,
        )
        .execute(&mut **tx)
        .await?,
    };
    Ok(done.rows_affected())
}

/// Enqueues `workspace.delete` for every workspace whose deletion was requested at least 30 days
/// ago, is still marked deleted and has no job yet, inside `tx`; returns how many.
async fn due_deletions(tx: &mut Tx) -> Result<u64, sqlx::Error> {
    let due = sqlx::query_scalar!(
        "SELECT d.workspace_id FROM workspace_deletions d JOIN workspaces w ON w.id = d.workspace_id
          WHERE d.completed_at IS NULL AND d.job_id IS NULL AND w.deleted_at IS NOT NULL
            AND d.requested_at <= now() - interval '30 days'
          ORDER BY d.requested_at LIMIT 100"
    )
    .fetch_all(&mut **tx)
    .await?;
    for workspace in &due {
        let job = jobs::enqueue(
            tx,
            SYSTEM_WORKSPACE,
            &WorkspaceDelete {
                workspace: *workspace,
            },
            None,
        )
        .await?;
        sqlx::query!(
            "UPDATE workspace_deletions SET job_id = $2 WHERE workspace_id = $1",
            workspace,
            job.uuid(),
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(u64::try_from(due.len()).unwrap_or(0))
}

/// An object storage failure, to be retried with the runner's backoff.
fn stored(error: StorageError) -> JobError {
    JobError::Failed(error.to_string())
}

/// Deletes the exports whose file is no longer kept (their `expires_at` passed), [`FILES`] at a
/// time: each one's file first (the key it records, else where its job writes it, so a file
/// written by a run that never made the export ready goes too), then their rows, committed with
/// the checkpoint. Returns false when the job must yield before it is done.
async fn expired_exports(cx: &mut JobContext, storage: &Storage) -> Result<bool, JobError> {
    loop {
        let mut tx = cx.system()?.begin().await?;
        let expired = sqlx::query!(
            "SELECT workspace_id, id, format, object_key FROM exports
              WHERE expires_at < now() ORDER BY expires_at LIMIT $1",
            FILES,
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        if expired.is_empty() {
            return Ok(true);
        }
        for export in &expired {
            let key = export.object_key.clone().or_else(|| {
                let format = export.format.parse::<Format>().ok()?;
                Some(object_key(
                    WorkspaceId::trusted(export.workspace_id),
                    Id::from_uuid(export.id),
                    format,
                ))
            });
            if let Some(key) = key {
                storage.delete(&key).await.map_err(stored)?;
            }
        }
        let (workspaces, ids): (Vec<Uuid>, Vec<Uuid>) = expired
            .iter()
            .map(|export| (export.workspace_id, export.id))
            .unzip();
        let mut chunk = cx.begin().await?;
        let deleted = sqlx::query!(
            "DELETE FROM exports WHERE (workspace_id, id) IN (SELECT * FROM unnest($1::uuid[], $2::uuid[]))",
            &workspaces,
            &ids,
        )
        .execute(&mut **chunk.tx())
        .await?
        .rows_affected();
        cx.checkpoint(chunk, json!({ "table": "exports", "deleted": deleted }))
            .await?;
        if cx.should_yield() {
            return Ok(false);
        }
        if i64::try_from(expired.len()).unwrap_or(i64::MAX) < FILES {
            return Ok(true);
        }
    }
}

/// Deletes the files of abandoned import uploads: every object under
/// `imports/<workspace>/<import>/` whose import has no row and whose id (a UUIDv7, minted just
/// before the upload) is more than [`ABANDONED_AFTER_DAYS`] old. The lease is renewed every
/// [`FILES`] deletions. Returns false when the job must yield before it is done.
async fn abandoned_uploads(cx: &mut JobContext, storage: &Storage) -> Result<bool, JobError> {
    // Check even pages containing only live objects: scanning a large bucket must renew
    // the lease and yield, not only pages which happen to contain deletable objects.
    let mut pages = storage
        .stream_keys("imports")
        .map_err(stored)?
        .try_chunks(usize::try_from(FILES).unwrap_or(1));
    while let Some(keys) = pages.try_next().await.map_err(|error| stored(error.1))? {
        let mut files: BTreeMap<(Uuid, Uuid), Vec<String>> = BTreeMap::new();
        for key in keys {
            let mut parts = key.split('/');
            let (Some("imports"), Some(Ok(workspace)), Some(Ok(import))) = (
                parts.next(),
                parts.next().map(str::parse::<Uuid>),
                parts.next().map(str::parse::<Uuid>),
            ) else {
                continue;
            };
            files.entry((workspace, import)).or_default().push(key);
        }
        let (workspaces, imports): (Vec<Uuid>, Vec<Uuid>) = files.keys().copied().unzip();
        let mut tx = cx.system()?.begin().await?;
        let abandoned = sqlx::query!(
            r#"SELECT k.workspace_id AS "workspace_id!", k.import_id AS "import_id!"
             FROM unnest($1::uuid[], $2::uuid[]) AS k(workspace_id, import_id)
            WHERE k.import_id < uuidv7_boundary(now() - make_interval(days => $3))
              AND NOT EXISTS (SELECT 1 FROM imports i
                               WHERE i.workspace_id = k.workspace_id AND i.id = k.import_id)"#,
            &workspaces,
            &imports,
            ABANDONED_AFTER_DAYS,
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut deleted = 0;
        for upload in abandoned {
            for key in files
                .get(&(upload.workspace_id, upload.import_id))
                .into_iter()
                .flatten()
            {
                storage.delete(key).await.map_err(stored)?;
                deleted += 1;
            }
        }
        cx.heartbeat().await?;
        // With no deletions, restarting an unordered listing would scan the same live
        // prefix forever. Renew while scanning it; yield after measurable deletion progress
        // or an explicit interruption. A later run resumes by absence of deleted objects.
        if cx.interrupted() || (deleted > 0 && cx.should_yield()) {
            return Ok(false);
        }
    }

    Ok(true)
}

/// `retention.prune` (see the module).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetentionPrune {}

impl Job for RetentionPrune {
    const KIND: &'static str = "retention.prune";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::ExternalRetryable;
    const CLASS: Class = Class::System;

    fn unique_key(&self) -> Option<String> {
        Some("singleton".to_owned())
    }

    fn schedule() -> Option<(&'static str, Self)> {
        Some(("15 0 * * *", Self {}))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let storage = cx.env::<Storage>()?.clone();
        let mut chunk = cx.begin().await?;
        let enqueued = due_deletions(chunk.tx()).await?;
        cx.checkpoint(chunk, json!({ "deletions": enqueued }))
            .await?;
        if enqueued > 0 {
            jobs::wake(cx.db(), Queue::Maintenance).await;
        }
        for pruned in <Pruned as strum::IntoEnumIterator>::iter() {
            loop {
                let mut chunk = cx.begin().await?;
                let deleted = prune(chunk.tx(), pruned).await?;
                cx.checkpoint(
                    chunk,
                    json!({ "table": <&'static str>::from(pruned), "deleted": deleted }),
                )
                .await?;
                if cx.should_yield() {
                    return Ok(Outcome::Yield {
                        after: Duration::ZERO,
                    });
                }
                if deleted < u64::try_from(BATCH).unwrap_or(u64::MAX) {
                    break;
                }
            }
        }
        if !expired_exports(cx, &storage).await?
            || !abandoned_uploads(cx, &storage).await?
            || !abandoned_attachments(cx, &storage).await?
        {
            return Ok(Outcome::Yield {
                after: Duration::ZERO,
            });
        }
        Ok(Outcome::Done)
    }
}

/// Removes objects with no metadata after a full day, including uploads whose commit failed
/// and files from fenced-out inbox polls. A live metadata row always protects its object.
async fn abandoned_attachments(cx: &mut JobContext, storage: &Storage) -> Result<bool, JobError> {
    let mut examined: i64 = 0;
    for key in storage.list("attachments").await.map_err(stored)? {
        let mut parts = key.split('/');
        let (Some("attachments"), Some(Ok(workspace)), Some(Ok(id))) = (
            parts.next(),
            parts.next().map(str::parse::<Uuid>),
            parts.next().map(str::parse::<Uuid>),
        ) else {
            continue;
        };
        let mut tx = cx.system()?.begin().await?;
        let abandoned:bool=sqlx::query_scalar("SELECT $2::uuid < uuidv7_boundary(now()-interval '1 day') AND NOT EXISTS(SELECT 1 FROM attachments WHERE workspace_id=$1 AND id=$2)").bind(workspace).bind(id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        if abandoned {
            storage.delete(&key).await.map_err(stored)?;
        }
        examined += 1;
        if examined % FILES == 0 {
            cx.heartbeat().await?;
            if cx.should_yield() {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
