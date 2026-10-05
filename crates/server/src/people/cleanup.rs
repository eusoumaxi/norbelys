//! Fenced, bounded cleanup after logical group or custom-field deletion.
//!
//! Small resources finish in the request's transaction. Larger ones retain a tombstone and
//! a durable job; reads hide them immediately, new writes cannot restore memberships or old
//! field values, and a field key stays reserved until cleanup completes. Each job chunk and
//! its checkpoint commit together, so retries and recovered leases safely resume work.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::db::Tx;
use crate::domain::ids::WorkspaceId;
use crate::jobs::{Effect, Job, JobContext, JobError, Outcome, Queue};

/// Maximum rows touched in one transaction, including the initial API chunk.
pub(super) const BATCH: i64 = 500;

/// The immutable identity of the resource being removed; ids are never reused.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Resource {
    Group(Uuid),
    Field(Uuid),
}

/// Durable cleanup of one tombstoned resource in its own workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PeopleCleanup {
    pub resource: Resource,
}

impl Job for PeopleCleanup {
    const KIND: &'static str = "people.cleanup";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::Idempotent;

    fn unique_key(&self) -> Option<String> {
        Some(match self.resource {
            Resource::Group(id) => format!("group:{id}"),
            Resource::Field(id) => format!("field:{id}"),
        })
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        loop {
            let mut chunk = cx.begin().await?;
            let done = clean(chunk.tx(), cx.workspace(), &self.resource).await?;
            cx.checkpoint(chunk, json!({ "done": done })).await?;
            if done {
                return Ok(Outcome::Done);
            }
            if cx.should_yield() {
                return Ok(Outcome::Yield {
                    after: Duration::ZERO,
                });
            }
        }
    }
}

/// Cleans one bounded batch. Returns true only when the tombstone can be removed safely.
pub(super) async fn clean(
    tx: &mut Tx,
    workspace: WorkspaceId,
    resource: &Resource,
) -> Result<bool, sqlx::Error> {
    let (deleted, id) = match *resource {
        Resource::Group(id) => {
            let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM groups WHERE workspace_id = $1 AND id = $2 AND deleted_at IS NOT NULL FOR UPDATE)")
                .bind(workspace.uuid()).bind(id).fetch_one(&mut **tx).await?;
            if !live {
                return Ok(true);
            }
            let deleted = sqlx::query("DELETE FROM group_people WHERE workspace_id = $1 AND group_id = $2 AND person_id IN (SELECT person_id FROM group_people WHERE workspace_id = $1 AND group_id = $2 ORDER BY person_id LIMIT $3)")
                .bind(workspace.uuid()).bind(id).bind(BATCH).execute(&mut **tx).await?.rows_affected();
            (deleted, id)
        }
        Resource::Field(id) => {
            super::fields::lock_exclusive(tx, workspace).await?;
            let key: Option<String> = sqlx::query_scalar("SELECT key FROM person_field_definitions WHERE workspace_id = $1 AND id = $2 AND deleted_at IS NOT NULL FOR UPDATE")
                .bind(workspace.uuid()).bind(id).fetch_optional(&mut **tx).await?;
            let Some(key) = key else {
                return Ok(true);
            };
            let deleted = sqlx::query("UPDATE people SET custom_fields = custom_fields - $2 WHERE workspace_id = $1 AND id IN (SELECT id FROM people WHERE workspace_id = $1 AND custom_fields ? $2 ORDER BY id LIMIT $3 FOR UPDATE)")
                .bind(workspace.uuid()).bind(key).bind(BATCH).execute(&mut **tx).await?.rows_affected();
            (deleted, id)
        }
    };
    if deleted >= u64::try_from(BATCH).unwrap_or(u64::MAX) {
        return Ok(false);
    }
    let query = match resource {
        Resource::Group(_) => {
            "DELETE FROM groups WHERE workspace_id = $1 AND id = $2 AND deleted_at IS NOT NULL"
        }
        Resource::Field(_) => {
            "DELETE FROM person_field_definitions WHERE workspace_id = $1 AND id = $2 AND deleted_at IS NOT NULL"
        }
    };
    sqlx::query(query)
        .bind(workspace.uuid())
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{Id, Person};
    use crate::domain::people::FieldType;
    use crate::jobs::{Registry, runner::Harness};
    use crate::people::{self, fields, groups};
    use crate::testing::TestDb;

    #[tokio::test]
    async fn large_deletions_hide_immediately_reserve_keys_and_finish_in_fenced_chunks() {
        let test = TestDb::new().await;
        let workspace = test.workspace("cleanup").await.id;
        let other = test.workspace("other").await.id;
        let mut tx = test.app.begin_in(workspace).await.unwrap();
        let field = fields::create(
            &mut tx,
            workspace,
            &fields::NewField {
                key: "tier".into(),
                label: "Tier".into(),
                field_type: FieldType::Text,
                options: vec![],
            },
        )
        .await
        .unwrap();
        let group = groups::create(&mut tx, workspace, "Audience", None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        sqlx::query("INSERT INTO people (workspace_id, email, custom_fields) SELECT $1, 'person-' || n || '@example.test', '{\"tier\":\"old\"}'::jsonb FROM generate_series(1, 1507) n")
            .bind(workspace.uuid()).execute(test.system.pool()).await.unwrap();
        sqlx::query("INSERT INTO group_people (workspace_id, group_id, person_id) SELECT workspace_id, $2, id FROM people WHERE workspace_id = $1")
            .bind(workspace.uuid()).bind(group.id.uuid()).execute(test.system.pool()).await.unwrap();
        sqlx::query("INSERT INTO people (workspace_id, email, custom_fields) VALUES ($1, 'other@example.test', '{\"tier\":\"keep\"}')")
            .bind(other.uuid()).execute(test.system.pool()).await.unwrap();
        let person: Uuid = sqlx::query_scalar(
            "SELECT id FROM people WHERE workspace_id = $1 ORDER BY id DESC LIMIT 1",
        )
        .bind(workspace.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        let mut tx = test.app.begin_in(workspace).await.unwrap();
        assert!(groups::delete(&mut tx, workspace, group.id).await.unwrap());
        fields::delete(&mut tx, workspace, field.id).await.unwrap();
        tx.commit().await.unwrap();
        let mut tx = test.app.begin_in(workspace).await.unwrap();
        assert!(
            groups::read(&mut tx, workspace, group.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            fields::definitions(&mut tx, workspace)
                .await
                .unwrap()
                .is_empty()
        );
        let visible = people::read(&mut tx, workspace, Id::<Person>::from_uuid(person))
            .await
            .unwrap()
            .unwrap();
        assert!(visible.group_ids.is_empty());
        assert!(visible.fields.get("tier").is_none());
        tx.commit().await.unwrap();
        let mut tx = test.app.begin_in(workspace).await.unwrap();
        assert!(
            fields::create(
                &mut tx,
                workspace,
                &fields::NewField {
                    key: "tier".into(),
                    label: "Reused".into(),
                    field_type: FieldType::Number,
                    options: vec![],
                }
            )
            .await
            .is_err()
        );
        tx.rollback().await.unwrap();
        let remaining: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM people WHERE workspace_id = $1 AND custom_fields ? 'tier'",
        )
        .bind(workspace.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        assert_eq!(remaining, 1507 - BATCH);
        let mut registry = Registry::default();
        registry.register::<PeopleCleanup>().unwrap();
        let runner = Harness::new(
            test.worker.clone(),
            test.system.clone(),
            registry,
            http::Extensions::new(),
            "cleanup-worker",
        );
        for _ in 0..2 {
            assert_eq!(runner.run_once(Queue::Maintenance, 1).await[0].1, "done");
        }
        let mut tx = test.app.begin_in(workspace).await.unwrap();
        let fresh = fields::create(
            &mut tx,
            workspace,
            &fields::NewField {
                key: "tier".into(),
                label: "Reused".into(),
                field_type: FieldType::Number,
                options: vec![],
            },
        )
        .await
        .unwrap();
        assert_ne!(fresh.id, field.id);
        tx.commit().await.unwrap();
        let remaining: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM people WHERE workspace_id = $1 AND custom_fields ? 'tier'",
        )
        .bind(workspace.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        assert_eq!(remaining, 0);
        let untouched: String =
            sqlx::query_scalar("SELECT custom_fields->>'tier' FROM people WHERE workspace_id = $1")
                .bind(other.uuid())
                .fetch_one(test.system.pool())
                .await
                .unwrap();
        assert_eq!(untouched, "keep");
        // A replay after completion cannot touch a new field with the old key or another tenant.
        let mut tx = test.worker.begin_in(workspace).await.unwrap();
        assert!(
            clean(&mut tx, workspace, &Resource::Field(field.id.uuid()))
                .await
                .unwrap()
        );
        assert!(
            clean(&mut tx, workspace, &Resource::Group(group.id.uuid()))
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
    }
}
