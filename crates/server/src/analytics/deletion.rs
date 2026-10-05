//! `workspace.delete`: erases a workspace once the 30-day tombstone of its deletion request has
//! passed: every stored file and every row it owns, then the workspace itself.
//!
//! # What is erased, in which order
//!
//! 1. **Files**: every object under the workspace's key prefixes (`imports/`, `exports/`,
//!    `images/`, `inbound/`, each followed by the workspace's uuid), and the full bodies its
//!    inbound messages name.
//! 2. **Rows**: every table with a `workspace_id` column, read from the catalogue rather than
//!    listed by hand, so a table added later is erased too. Tables are emptied in the order of
//!    their foreign keys (a table before the tables it references), computed from
//!    `pg_constraint`; the few references that form cycles (an enrollment and its current
//!    message, a step and its current revision, a revision and its winning variant) are cleared
//!    first, which breaks each cycle. Rows go in batches of [`BATCH`], each committed with the
//!    job's checkpoint, so a large workspace never holds one long transaction.
//! 3. **The workspace** row, and the request marked completed, in one transaction, with the
//!    tombstone object recorded on the request.
//!
//! Increments (`stats_increments`) are the one exception: the system role can never write them
//! (the rollup's guarantee depends on it), so they stay until their daily partitions are
//! dropped (a day's partition goes once the whole day is older than the two-day retention), and
//! the rollup skips increments whose workspace is gone. Archived Parquet files hold
//! rows of many workspaces and are immutable; the tombstone (`tombstones/<workspace>.json`) names
//! the erased workspace so that everything reading the archive skips it until those files expire.
//!
//! Every step deletes only what is still there, so a crash, a retry or a second job repeats
//! harmlessly. A request withdrawn in time (the workspace no longer marked deleted) erases
//! nothing.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::AssertSqlSafe;
use uuid::Uuid;

use super::parquet::quoted;
use crate::db::Tx;
use crate::domain::ids::WorkspaceId;
use crate::jobs::{Class, Effect, Job, JobContext, JobError, Outcome, Queue, SYSTEM_WORKSPACE};
use crate::storage::Storage;

/// Rows one statement deletes at most.
pub const BATCH: i64 = 10_000;

/// The key prefixes of a workspace's stored files, each followed by `/<workspace uuid>`.
const PREFIXES: [&str; 5] = ["imports", "exports", "images", "inbound", "attachments"];

/// Tables never erased by workspace: the request itself is the record of the erasure, and
/// increments cannot be written by the system role (they expire with their partitions).
const KEPT: [&str; 2] = ["workspace_deletions", "stats_increments"];

/// The references that form foreign-key cycles among a workspace's tables, as
/// `(referencing table, referenced table)`: cleared before the tables are emptied.
const CYCLES: [(&str, &str); 3] = [
    ("enrollments", "messages"),
    ("steps", "step_revisions"),
    ("step_revisions", "step_revision_variants"),
];

/// `workspace.delete` (see the module).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDelete {
    /// The workspace to erase.
    pub workspace: Uuid,
}

impl Job for WorkspaceDelete {
    const KIND: &'static str = "workspace.delete";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::ExternalRetryable;
    const CLASS: Class = Class::System;

    fn unique_key(&self) -> Option<String> {
        Some(self.workspace.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        run(cx, WorkspaceId::trusted(self.workspace)).await
    }
}

fn failed(error: impl std::fmt::Display) -> JobError {
    JobError::Failed(error.to_string())
}

async fn run(cx: &mut JobContext, workspace: WorkspaceId) -> Result<Outcome, JobError> {
    if workspace == SYSTEM_WORKSPACE {
        return Ok(Outcome::Discard {
            reason: "the system workspace is never erased".to_owned(),
        });
    }
    let storage = cx.env::<Storage>()?.clone();
    let (order, bodies) = {
        let mut tx = cx.system()?.begin().await?;
        let state = sqlx::query!(
            r#"SELECT d.completed_at IS NOT NULL AS "completed!", w.id IS NOT NULL AS "exists!",
                      coalesce(w.deleted_at IS NOT NULL, false) AS "deleted!"
                 FROM workspace_deletions d LEFT JOIN workspaces w ON w.id = d.workspace_id
                WHERE d.workspace_id = $1"#,
            workspace.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        match state {
            Some(state) if !state.completed && state.exists && state.deleted => {}
            Some(state) if state.exists && !state.deleted => {
                tracing::info!(workspace = %workspace.uuid(), "workspace.delete: the deletion was withdrawn; nothing erased");
                return Ok(Outcome::Done);
            }
            Some(_) => return Ok(Outcome::Done),
            None => {
                return Ok(Outcome::Discard {
                    reason: "no deletion was requested for this workspace".to_owned(),
                });
            }
        }
        let order = order(&mut tx).await?;
        let bodies = sqlx::query_scalar!(
            r#"SELECT body_object_key AS "key!" FROM inbound_messages WHERE workspace_id = $1 AND body_object_key IS NOT NULL"#,
            workspace.uuid(),
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        (order, bodies)
    };

    // 1. Files.
    for key in bodies {
        storage.delete(&key).await.map_err(failed)?;
    }
    for prefix in PREFIXES {
        for key in storage
            .list(&format!("{prefix}/{}", workspace.uuid()))
            .await
            .map_err(failed)?
        {
            storage.delete(&key).await.map_err(failed)?;
        }
    }

    // 2. Rows: the cycles first, then each table in batches.
    for (referencing, referenced) in CYCLES {
        loop {
            let mut chunk = cx.begin().await?;
            let cleared = clear_cycle(chunk.tx(), workspace, referencing).await?;
            cx.checkpoint(
                chunk,
                json!({ "cleared": format!("{referencing}->{referenced}") }),
            )
            .await?;
            if cleared < u64::try_from(BATCH).unwrap_or(u64::MAX) {
                break;
            }
        }
    }
    for table in &order {
        loop {
            let mut chunk = cx.begin().await?;
            let deleted = sqlx::query(AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE (tableoid, ctid) IN (
                     SELECT tableoid, ctid FROM {table} WHERE workspace_id = $1 LIMIT $2)",
                table = quoted(table)
            )))
            .bind(workspace.uuid())
            .bind(BATCH)
            .execute(&mut **chunk.tx())
            .await?
            .rows_affected();
            cx.checkpoint(chunk, json!({ "table": table, "deleted": deleted }))
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

    // 3. The workspace, with its tombstone.
    let tombstone = format!("tombstones/{}.json", workspace.uuid());
    storage
        .put(
            &tombstone,
            bytes::Bytes::from(
                serde_json::to_vec(&json!({
                    "workspace_id": workspace.uuid(),
                    "erased_at": crate::process::now(),
                    "archive": "rows of this workspace remain in archived Parquet objects until they expire; readers of the archive skip it",
                }))
                .map_err(failed)?,
            ),
        )
        .await
        .map_err(failed)?;
    let mut chunk = cx.begin().await?;
    sqlx::query!("DELETE FROM workspaces WHERE id = $1", workspace.uuid())
        .execute(&mut **chunk.tx())
        .await?;
    sqlx::query!(
        "UPDATE workspace_deletions SET completed_at = now(), tombstone_object_key = $2 WHERE workspace_id = $1",
        workspace.uuid(),
        tombstone,
    )
    .execute(&mut **chunk.tx())
    .await?;
    cx.checkpoint(chunk, json!({ "erased": workspace.uuid() }))
        .await?;
    tracing::info!(workspace = %workspace.uuid(), "workspace.delete: the workspace was erased");
    Ok(Outcome::Done)
}

/// Clears one batch of a cycle's references in `workspace`; returns the rows changed.
async fn clear_cycle(
    tx: &mut Tx,
    workspace: WorkspaceId,
    referencing: &str,
) -> Result<u64, JobError> {
    let done = match referencing {
        "enrollments" => sqlx::query!(
            "UPDATE enrollments SET message_id = NULL WHERE ctid IN (
                 SELECT ctid FROM enrollments WHERE workspace_id = $1 AND message_id IS NOT NULL LIMIT $2)",
            workspace.uuid(),
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        "steps" => sqlx::query!(
            "UPDATE steps SET current_revision = NULL WHERE ctid IN (
                 SELECT ctid FROM steps WHERE workspace_id = $1 AND current_revision IS NOT NULL LIMIT $2)",
            workspace.uuid(),
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        "step_revisions" => sqlx::query!(
            "UPDATE step_revisions SET winner_variant_id = NULL, winner_variant_version = NULL, winner_selected_at = NULL
              WHERE ctid IN (SELECT ctid FROM step_revisions WHERE workspace_id = $1 AND winner_variant_id IS NOT NULL LIMIT $2)",
            workspace.uuid(),
            BATCH,
        )
        .execute(&mut **tx)
        .await?,
        other => return Err(failed(format!("no rule clears the references of {other}"))),
    };
    Ok(done.rows_affected())
}

/// The tables holding a workspace's rows, in an order that empties each before the tables it
/// references (see the module).
///
/// # Errors
///
/// The foreign keys form a cycle that [`CYCLES`] does not break (a schema change needs a rule),
/// or the database failed.
async fn order(tx: &mut Tx) -> Result<Vec<String>, JobError> {
    let tables: BTreeSet<String> = sqlx::query_scalar!(
        r#"SELECT c.relname::text AS "name!" FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public'
            WHERE c.relkind IN ('r', 'p') AND NOT c.relispartition
              AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attname = 'workspace_id' AND NOT a.attisdropped)"#
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .filter(|table| !KEPT.contains(&table.as_str()))
    .collect();
    let edges = sqlx::query!(
        r#"SELECT DISTINCT a.relname::text AS "referencing!", b.relname::text AS "referenced!"
             FROM pg_constraint con JOIN pg_class a ON a.oid = con.conrelid JOIN pg_class b ON b.oid = con.confrelid
            WHERE con.contype = 'f' AND con.conparentid = 0 AND a.oid <> b.oid"#
    )
    .fetch_all(&mut **tx)
    .await?;
    let edges: Vec<(String, String)> = edges
        .into_iter()
        .map(|edge| (edge.referencing, edge.referenced))
        .filter(|(from, to)| {
            tables.contains(from)
                && tables.contains(to)
                && !CYCLES.contains(&(from.as_str(), to.as_str()))
        })
        .collect();
    deletion_order(&tables, &edges).map_err(|stuck| {
        failed(format!(
            "the foreign keys among {} form a cycle no rule breaks",
            stuck.join(", ")
        ))
    })
}

/// Orders `tables` so that every table comes before each table it references (`edges` are
/// `(referencing, referenced)`); ties in name order. On a cycle, the tables left unordered.
fn deletion_order(
    tables: &BTreeSet<String>,
    edges: &[(String, String)],
) -> Result<Vec<String>, Vec<String>> {
    // How many tables still to empty reference each table.
    let mut waiting: HashMap<&str, usize> =
        tables.iter().map(|table| (table.as_str(), 0)).collect();
    for (_, referenced) in edges {
        if let Some(count) = waiting.get_mut(referenced.as_str()) {
            *count += 1;
        }
    }
    let mut order = Vec::with_capacity(tables.len());
    let mut left: BTreeSet<&str> = tables.iter().map(String::as_str).collect();
    while let Some(next) = left
        .iter()
        .copied()
        .find(|table| waiting.get(table).copied().unwrap_or(0) == 0)
    {
        left.remove(next);
        order.push(next.to_owned());
        for (referencing, referenced) in edges {
            if referencing == next
                && let Some(count) = waiting.get_mut(referenced.as_str())
            {
                *count = count.saturating_sub(1);
            }
        }
    }
    if left.is_empty() {
        Ok(order)
    } else {
        Err(left.into_iter().map(str::to_owned).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::deletion_order;

    /// A table is emptied before every table it references, and a cycle no rule breaks is named
    /// instead of guessed at: deleting in the wrong order fails on a foreign key half-way through
    /// an erasure.
    #[test]
    fn tables_are_emptied_before_what_they_reference() {
        let tables: BTreeSet<String> = ["campaigns", "messages", "attempts", "people"]
            .map(str::to_owned)
            .into();
        let edge = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        let edges = vec![
            edge("attempts", "messages"),
            edge("messages", "campaigns"),
            edge("messages", "people"),
        ];
        let order = deletion_order(&tables, &edges).unwrap();
        let at = |table: &str| order.iter().position(|t| t == table).unwrap();
        assert!(at("attempts") < at("messages"));
        assert!(at("messages") < at("campaigns"));
        assert!(at("messages") < at("people"));
        let cyclic = vec![edge("attempts", "messages"), edge("messages", "attempts")];
        assert_eq!(
            deletion_order(&tables, &cyclic),
            Err(vec!["attempts".to_owned(), "messages".to_owned()])
        );
    }
}
