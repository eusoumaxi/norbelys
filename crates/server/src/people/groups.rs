//! Groups: named lists of people, written by hand or filled by imports.
//!
//! A group holds no rule (that is a segment): membership is a row of `group_people`, written
//! on the person (`group_ids`, replaced whole) or added by an import. A group shows its exact
//! `people_count`, an index-only count of its memberships. Deleting a group removes its
//! memberships and leaves the people; an import that named the group keeps running without it.
//!
//! Lock order: a deletion locks the group row first, so a membership written meanwhile (whose
//! foreign key shares that row) either commits before the deletion sees the memberships, or
//! fails because the group is gone. An update locks it too ([`lock_version`]), to check its
//! `If-Match`.
//!
//! A deletion leaves the members' rows (and so their versions) as they are, although their
//! `group_ids` lose the group: touching every member would rewrite one row per member and lock
//! them after the group's row, the reverse of an import's order. No update of a person can be
//! lost to it, as `http::versioning` explains: a `PATCH` naming the deleted group is `404`.

use serde::Serialize;
use uuid::Uuid;

use super::cleanup::{self, PeopleCleanup, Resource};
use crate::db::Tx;
use crate::domain::ids::{Group, Id, WorkspaceId};
use crate::domain::time::Timestamp;
use crate::http::versioning;
use crate::jobs;

/// A group as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct GroupObject {
    pub id: Id<Group>,
    pub name: String,
    pub description: Option<String>,
    /// The people in the group, counted now; not part of `version`, since no update writes it.
    pub people_count: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The group's version, also the response's `ETag`: `updated_at` in microseconds since the
    /// Unix epoch. An update sent with it in `If-Match` applies only to this version.
    pub version: i64,
}

/// A group's row with its count, as every query here reads it.
struct GroupRow {
    id: Id<Group>,
    name: String,
    description: Option<String>,
    people_count: i64,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl From<GroupRow> for GroupObject {
    fn from(row: GroupRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            description: row.description,
            people_count: row.people_count,
            created_at: row.created_at,
            updated_at: row.updated_at,
            version: versioning::of(row.updated_at),
        }
    }
}

/// One page of `workspace`'s groups whose name starts with `prefix` (a `LIKE` pattern of
/// lowercase text), in id order after `cursor`. Fetches `limit` rows.
///
/// # Errors
///
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    prefix: Option<&str>,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<GroupObject>, sqlx::Error> {
    // A workspace's groups are few enough to sort: one statement serves both orders.
    let rows = sqlx::query_as!(
        GroupRow,
        r#"SELECT g.id AS "id: Id<Group>", g.name, g.description,
                  (SELECT count(*) FROM group_people m WHERE m.workspace_id = g.workspace_id AND m.group_id = g.id) AS "people_count!",
                  g.created_at AS "created_at: Timestamp", g.updated_at AS "updated_at: Timestamp"
             FROM groups g
            WHERE g.workspace_id = $1 AND g.deleted_at IS NULL AND ($2::text IS NULL OR lower(g.name) LIKE $2)
              AND ($3::uuid IS NULL OR CASE WHEN $4 THEN g.id > $3 ELSE g.id < $3 END)
            ORDER BY CASE WHEN $4 THEN g.id END, g.id DESC LIMIT $5"#,
        workspace.uuid(),
        prefix,
        cursor,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows.into_iter().map(GroupObject::from).collect())
}

/// Counts `workspace`'s groups whose name starts with `prefix`, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    prefix: Option<&str>,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM groups WHERE workspace_id = $1 AND deleted_at IS NULL AND ($2::text IS NULL OR lower(name) LIKE $2) LIMIT $3) counted"#,
        workspace.uuid(),
        prefix,
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// Reads one group of `workspace`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Group>,
) -> Result<Option<GroupObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        GroupRow,
        r#"SELECT g.id AS "id: Id<Group>", g.name, g.description,
                  (SELECT count(*) FROM group_people m WHERE m.workspace_id = g.workspace_id AND m.group_id = g.id) AS "people_count!",
                  g.created_at AS "created_at: Timestamp", g.updated_at AS "updated_at: Timestamp"
             FROM groups g WHERE g.workspace_id = $1 AND g.deleted_at IS NULL AND g.id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(GroupObject::from))
}

/// Locks a group's row for an update and returns its version, for the update's `If-Match` to be
/// checked against before anything is written; `None` when the workspace has no such group.
///
/// # Errors
///
/// The database is unavailable.
pub async fn lock_version(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Group>,
) -> Result<Option<i64>, sqlx::Error> {
    let updated_at = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM groups WHERE workspace_id = $1 AND deleted_at IS NULL AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated_at.map(versioning::of))
}

/// Creates an empty group.
///
/// # Errors
///
/// The database refused.
pub async fn create(
    tx: &mut Tx,
    workspace: WorkspaceId,
    name: &str,
    description: Option<&str>,
) -> Result<GroupObject, sqlx::Error> {
    sqlx::query_as!(
        GroupRow,
        r#"INSERT INTO groups (workspace_id, name, description) VALUES ($1, $2, $3)
           RETURNING id AS "id: Id<Group>", name, description, 0::bigint AS "people_count!",
                     created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp""#,
        workspace.uuid(),
        name,
        description,
    )
    .fetch_one(&mut **tx)
    .await
    .map(GroupObject::from)
}

/// Renames a group or changes its description (`Some(None)` clears it). `None` when the group
/// does not exist.
///
/// # Errors
///
/// The database refused.
pub async fn update(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Group>,
    name: Option<&str>,
    description: Option<Option<&str>>,
) -> Result<Option<GroupObject>, sqlx::Error> {
    let updated = sqlx::query_scalar!(
        "UPDATE groups SET name = coalesce($3, name), description = CASE WHEN $4 THEN $5 ELSE description END
          WHERE workspace_id = $1 AND deleted_at IS NULL AND id = $2 RETURNING id",
        workspace.uuid(),
        id.uuid(),
        name,
        description.is_some(),
        description.flatten(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    match updated {
        Some(_) => read(tx, workspace, id).await,
        None => Ok(None),
    }
}

/// Hides a group immediately and removes memberships in bounded chunks; the people stay.
/// A durable job finishes large groups. False when the group does not exist.
///
/// # Errors
///
/// The database refused.
pub async fn delete(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Group>,
) -> Result<bool, sqlx::Error> {
    let locked = sqlx::query_scalar!(
        "SELECT id FROM groups WHERE workspace_id = $1 AND deleted_at IS NULL AND id = $2 FOR UPDATE",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    if locked.is_none() {
        return Ok(false);
    }
    sqlx::query("UPDATE groups SET deleted_at = now() WHERE workspace_id = $1 AND id = $2")
        .bind(workspace.uuid())
        .bind(id.uuid())
        .execute(&mut **tx)
        .await?;
    let resource = Resource::Group(id.uuid());
    if !cleanup::clean(tx, workspace, &resource).await? {
        jobs::enqueue(tx, workspace, &PeopleCleanup { resource }, None).await?;
    }
    Ok(true)
}
