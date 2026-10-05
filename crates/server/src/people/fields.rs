//! Custom field definitions: the typed attributes a workspace adds to its people.
//!
//! A definition has a key (how values are stored in a person's `fields`, and how CSV columns
//! and segment conditions name it), a label for people to read, a type, and for an enum its
//! options. The key and the type never change: a stored value's meaning depends on both. The
//! label and the options may change, with two guards that keep every stored value and every
//! segment valid: an option still held by a person cannot be removed, and a change that would
//! break a segment's filter is refused. Deleting a definition removes its values from every
//! person in bounded transactions; reads hide deleted values immediately, and the key stays
//! reserved until the durable cleanup finishes. A definition a segment uses cannot be deleted.
//!
//! # The field lock
//!
//! A person's values are checked against the definitions twice: by the API before the write,
//! and by the database's `people_field_definition` trigger during it. Both read the
//! definitions under one transaction-level advisory lock per workspace
//! (`hashtextextended('people-fields:' || workspace, 0)`, the key the trigger uses): person
//! writes and import chunks hold it shared ([`lock_shared`]), a change of a definition holds it
//! exclusive. A definition therefore cannot change between a write's check and its commit, and
//! writes never wait on each other.
//!
//! Lock order: the field lock, then the definition's row (an update takes both in
//! [`lock_version`] to check its `If-Match`), then the people holding the definition's values.

use serde::Serialize;
use serde_json::{Value, json};

use super::Error;
use super::cleanup::{self, PeopleCleanup, Resource};
use crate::db::Tx;
use crate::domain::ids::{Field, Id, Segment, WorkspaceId};
use crate::domain::people::{Definition, FIELDS_MAX, FieldType};
use crate::domain::segments::{self, Filter};
use crate::domain::time::Timestamp;
use crate::http::versioning;
use crate::jobs;

/// A custom field definition as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct FieldObject {
    pub id: Id<Field>,
    /// How values are stored in a person's `fields` and named in imports and segments.
    pub key: String,
    /// The name people read.
    pub label: String,
    /// What its values are. New values may be added.
    #[serde(rename = "type")]
    #[schema(value_type = FieldType)]
    pub field_type: String,
    /// An enum field's allowed values; empty for every other type.
    pub options: Vec<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The field's version, also the response's `ETag`: `updated_at` in microseconds since the
    /// Unix epoch. An update sent with it in `If-Match` applies only to this version, so a
    /// replacement of `options` never undoes a change made since it was read.
    pub version: i64,
}

/// A definition's row, as every query here reads it.
struct FieldRow {
    id: Id<Field>,
    key: String,
    label: String,
    field_type: String,
    options: Vec<String>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl From<FieldRow> for FieldObject {
    fn from(row: FieldRow) -> Self {
        Self {
            id: row.id,
            key: row.key,
            label: row.label,
            field_type: row.field_type,
            options: row.options,
            created_at: row.created_at,
            updated_at: row.updated_at,
            version: versioning::of(row.updated_at),
        }
    }
}

/// Takes the workspace's field lock shared, for a transaction that writes custom values.
///
/// # Errors
///
/// The database is unavailable.
pub async fn lock_shared(tx: &mut Tx, workspace: WorkspaceId) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "SELECT pg_advisory_xact_lock_shared(hashtextextended('people-fields:' || $1::uuid::text, 0))",
        workspace.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Takes the workspace's field lock exclusively, for a change of a definition.
pub(super) async fn lock_exclusive(tx: &mut Tx, workspace: WorkspaceId) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended('people-fields:' || $1::uuid::text, 0))",
        workspace.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The workspace's definitions, as the checks use them. Read them under the field lock when
/// the transaction writes values.
///
/// # Errors
///
/// The database is unavailable.
pub async fn definitions(
    tx: &mut Tx,
    workspace: WorkspaceId,
) -> Result<Vec<Definition>, sqlx::Error> {
    let rows = sqlx::query!(
        "SELECT key, field_type, options FROM person_field_definitions WHERE deleted_at IS NULL AND workspace_id = $1 ORDER BY key",
        workspace.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Definition {
                field_type: row.field_type.parse::<FieldType>().ok()?,
                key: row.key,
                options: row.options,
            })
        })
        .collect())
}

/// One page of `workspace`'s definitions in id order after `cursor`. Fetches `limit` rows.
///
/// # Errors
///
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    cursor: Option<uuid::Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<FieldObject>, sqlx::Error> {
    // At most 100 definitions: one statement serves both orders.
    let rows = sqlx::query_as!(
        FieldRow,
        r#"SELECT id AS "id: Id<Field>", key, label, field_type, options,
                  created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM person_field_definitions
            WHERE deleted_at IS NULL AND workspace_id = $1 AND ($2::uuid IS NULL OR CASE WHEN $3 THEN id > $2 ELSE id < $2 END)
            ORDER BY CASE WHEN $3 THEN id END, id DESC LIMIT $4"#,
        workspace.uuid(),
        cursor,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows.into_iter().map(FieldObject::from).collect())
}

/// Counts `workspace`'s definitions (there are at most 100).
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(tx: &mut Tx, workspace: WorkspaceId) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM person_field_definitions WHERE deleted_at IS NULL AND workspace_id = $1"#,
        workspace.uuid(),
    )
    .fetch_one(&mut **tx)
    .await
}

async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Field>,
) -> Result<Option<FieldObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        FieldRow,
        r#"SELECT id AS "id: Id<Field>", key, label, field_type, options,
                  created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM person_field_definitions WHERE deleted_at IS NULL AND workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(FieldObject::from))
}

/// Locks a definition for an update and returns its version, for the update's `If-Match` to be
/// checked against before anything is written; `None` when the workspace has no such field. The
/// workspace's field lock is taken exclusively first, as every change of a definition takes it
/// before the definition's row (see the module).
///
/// # Errors
///
/// The database is unavailable.
pub async fn lock_version(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Field>,
) -> Result<Option<i64>, sqlx::Error> {
    lock_exclusive(tx, workspace).await?;
    let updated_at = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM person_field_definitions
            WHERE deleted_at IS NULL AND workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated_at.map(versioning::of))
}

/// A definition to create; its key, label and options are already checked.
pub struct NewField {
    pub key: String,
    pub label: String,
    pub field_type: FieldType,
    pub options: Vec<String>,
}

/// Creates a definition under the exclusive field lock.
///
/// # Errors
///
/// [`Error::Conflict`] when the key is taken, [`Error::InvalidState`] when the workspace holds
/// 100 definitions already, or the database refused.
pub async fn create(
    tx: &mut Tx,
    workspace: WorkspaceId,
    field: &NewField,
) -> Result<FieldObject, Error> {
    lock_exclusive(tx, workspace).await?;
    if count(tx, workspace).await? >= FIELDS_MAX {
        return Err(Error::InvalidState(
            "A workspace holds at most 100 custom fields.".to_owned(),
        ));
    }
    sqlx::query_as!(
        FieldRow,
        r#"INSERT INTO person_field_definitions (workspace_id, key, label, field_type, options)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id AS "id: Id<Field>", key, label, field_type, options,
                     created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp""#,
        workspace.uuid(),
        field.key,
        field.label,
        field.field_type.as_str(),
        &field.options,
    )
    .fetch_one(&mut **tx)
    .await
    .map(FieldObject::from)
    .map_err(|error| match super::sqlstate(&error).as_deref() {
        Some("23505") => Error::Conflict(format!(
            "A field with the key `{}` exists already.",
            field.key
        )),
        _ => Error::Db(error),
    })
}

/// A change to a definition: its label, or an enum's options (replaced whole).
pub struct FieldChanges {
    pub label: Option<String>,
    pub options: Option<Vec<String>>,
}

/// Changes a definition under the exclusive field lock.
///
/// # Errors
///
/// [`Error::NotFound`]; [`Error::InvalidState`] for options on a field that is not an enum, an
/// option a person still holds, or a change that breaks a segment's filter; or the database
/// refused.
pub async fn update(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Field>,
    changes: &FieldChanges,
) -> Result<FieldObject, Error> {
    lock_exclusive(tx, workspace).await?;
    let current = read(tx, workspace, id)
        .await?
        .ok_or(Error::NotFound("field"))?;
    if let Some(options) = &changes.options {
        if current.field_type != FieldType::Enum.as_str() {
            return Err(Error::InvalidState(
                "Only an enum field has options.".to_owned(),
            ));
        }
        let removed: Vec<String> = current
            .options
            .iter()
            .filter(|option| !options.contains(option))
            .cloned()
            .collect();
        let holders = sqlx::query_scalar!(
            r#"SELECT count(*) AS "count!" FROM people WHERE workspace_id = $1 AND custom_fields ->> $2 = ANY($3)"#,
            workspace.uuid(),
            current.key,
            &removed,
        )
        .fetch_one(&mut **tx)
        .await?;
        if holders > 0 {
            return Err(Error::InvalidState(format!(
                "{holders} people hold an option this change removes; change their values first."
            )));
        }
        let mut changed = definitions(tx, workspace).await?;
        if let Some(definition) = changed
            .iter_mut()
            .find(|definition| definition.key == current.key)
        {
            definition.options.clone_from(options);
        }
        if let Some(segment) = broken_segment(tx, workspace, &current.key, &changed).await? {
            return Err(Error::InvalidState(format!(
                "The segment {segment} compares this field with an option the change removes."
            )));
        }
    }
    sqlx::query_as!(
        FieldRow,
        r#"UPDATE person_field_definitions SET label = coalesce($3, label), options = coalesce($4, options)
            WHERE deleted_at IS NULL AND workspace_id = $1 AND id = $2
           RETURNING id AS "id: Id<Field>", key, label, field_type, options,
                     created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp""#,
        workspace.uuid(),
        id.uuid(),
        changes.label,
        changes.options.as_deref(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .map(FieldObject::from)
    .ok_or(Error::NotFound("field"))
}

/// Hides a definition and its values immediately under the exclusive field lock.
/// Large physical cleanups continue in a durable job; the key stays reserved until completion.
///
/// # Errors
///
/// [`Error::NotFound`], [`Error::InvalidState`] when a segment uses the field, or the database
/// refused.
pub async fn delete(tx: &mut Tx, workspace: WorkspaceId, id: Id<Field>) -> Result<(), Error> {
    lock_exclusive(tx, workspace).await?;
    let key = read(tx, workspace, id)
        .await?
        .ok_or(Error::NotFound("field"))?
        .key;
    if let Some(segment) = users(tx, workspace, &key).await?.first() {
        return Err(Error::InvalidState(format!(
            "The segment {} uses this field; change its filter first.",
            segment.0
        )));
    }
    sqlx::query("UPDATE person_field_definitions SET deleted_at = now() WHERE workspace_id = $1 AND id = $2")
        .bind(workspace.uuid()).bind(id.uuid()).execute(&mut **tx).await?;
    let resource = Resource::Field(id.uuid());
    if !cleanup::clean(tx, workspace, &resource).await? {
        jobs::enqueue(tx, workspace, &PeopleCleanup { resource }, None).await?;
    }
    Ok(())
}

/// The segments whose filter has a condition on the field `key`, with their filters.
async fn users(
    tx: &mut Tx,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Vec<(Id<Segment>, Value)>, sqlx::Error> {
    let condition = json!({ "conditions": [{ "field": format!("fields.{key}") }] });
    Ok(sqlx::query!(
        r#"SELECT id AS "id: Id<Segment>", filter FROM segments WHERE workspace_id = $1 AND filter @> $2 ORDER BY id"#,
        workspace.uuid(),
        condition,
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| (row.id, row.filter))
    .collect())
}

/// The first segment using the field `key` whose filter no longer compiles under `changed`.
async fn broken_segment(
    tx: &mut Tx,
    workspace: WorkspaceId,
    key: &str,
    changed: &[Definition],
) -> Result<Option<Id<Segment>>, sqlx::Error> {
    Ok(users(tx, workspace, key)
        .await?
        .into_iter()
        .find(|(_, filter)| {
            serde_json::from_value::<Filter>(filter.clone())
                .ok()
                .is_none_or(|filter| segments::compile(&filter, changed).is_err())
        })
        .map(|(id, _)| id))
}
