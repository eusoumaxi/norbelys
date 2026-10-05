//! Segments: saved filters over people, evaluated whenever they are read.
//!
//! A segment stores its filter (`domain::segments`), never its members: who is in it is
//! decided at each read from the people as they are then. `GET /people?segment_id=` lists its
//! people; retrieving the segment counts them, up to 10,000 (`people_count_capped` above), and
//! says when (`computed_at`). A list of segments leaves the count out, because counting reads
//! every person of the workspace once per segment.
//!
//! A filter is checked against the workspace's field definitions under the shared field lock
//! (`fields::lock_shared`), so a definition cannot change between the check and the commit; a
//! later change of a definition that would break the filter is refused by the field's own
//! operations, so a stored filter always compiles.
//!
//! Lock order: an update locks the segment's row ([`lock_version`], to check its `If-Match`)
//! before the shared field lock its filter's check takes; the field operations read segments
//! without locking them, so the two orders never meet.

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use super::{Error, PeopleFilters, Selection};
use crate::db::Tx;
use crate::domain::ids::{Id, Segment, WorkspaceId};
use crate::domain::segments::{self, Compiled, Filter};
use crate::domain::time::Timestamp;
use crate::http::versioning;
use crate::pagination::COUNT_CAP;

/// A segment as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SegmentObject {
    pub id: Id<Segment>,
    pub name: String,
    /// Who the segment means.
    pub filter: Filter,
    /// The people the filter matches, counted when the segment is retrieved, up to 10,000;
    /// null in a list.
    pub people_count: Option<i64>,
    /// True when more than 10,000 people match; null in a list.
    pub people_count_capped: Option<bool>,
    /// When `people_count` was counted; null in a list.
    pub computed_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The segment's version, also the response's `ETag`: `updated_at` in microseconds since
    /// the Unix epoch. An update sent with it in `If-Match` applies only to this version, so a
    /// replaced filter never undoes a change made since it was read.
    pub version: i64,
}

struct SegmentRow {
    id: Id<Segment>,
    name: String,
    filter: Value,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl SegmentRow {
    /// The row as the API shows it, with a count when one was taken.
    fn object(self, counted: Option<i64>) -> Result<SegmentObject, sqlx::Error> {
        let filter = serde_json::from_value(self.filter).map_err(|_| {
            tracing::error!(segment_id = %self.id, error_code = "stored_filter_invalid", "stored segment filter is invalid");
            sqlx::Error::Decode("stored segment filter is invalid".into())
        })?;
        Ok(SegmentObject {
            id: self.id,
            name: self.name,
            filter,
            people_count: counted.map(|count| count.min(COUNT_CAP)),
            people_count_capped: counted.map(|count| count > COUNT_CAP),
            computed_at: counted.map(|_| crate::process::now()),
            created_at: self.created_at,
            updated_at: self.updated_at,
            version: versioning::of(self.updated_at),
        })
    }
}

/// One page of `workspace`'s segments in id order after `cursor`, without counts. Fetches
/// `limit` rows.
///
/// # Errors
///
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<SegmentObject>, sqlx::Error> {
    // A workspace's segments are few enough to sort: one statement serves both orders.
    let rows = sqlx::query_as!(
        SegmentRow,
        r#"SELECT id AS "id: Id<Segment>", name, filter, created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM segments
            WHERE workspace_id = $1 AND ($2::uuid IS NULL OR CASE WHEN $3 THEN id > $2 ELSE id < $2 END)
            ORDER BY CASE WHEN $3 THEN id END, id DESC LIMIT $4"#,
        workspace.uuid(),
        cursor,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter().map(|row| row.object(None)).collect()
}

/// Counts `workspace`'s segments, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(tx: &mut Tx, workspace: WorkspaceId, cap: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (SELECT 1 FROM segments WHERE workspace_id = $1 LIMIT $2) counted"#,
        workspace.uuid(),
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

async fn row(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Segment>,
) -> Result<Option<SegmentRow>, sqlx::Error> {
    sqlx::query_as!(
        SegmentRow,
        r#"SELECT id AS "id: Id<Segment>", name, filter, created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM segments WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await
}

/// Locks a segment's row for an update and returns its version, for the update's `If-Match` to
/// be checked against before anything is written; `None` when the workspace has no such
/// segment.
///
/// # Errors
///
/// The database is unavailable.
pub async fn lock_version(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Segment>,
) -> Result<Option<i64>, sqlx::Error> {
    let updated_at = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM segments WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated_at.map(versioning::of))
}

/// The compiled filter of a segment, against the workspace's current definitions.
///
/// # Errors
///
/// [`Error::NotFound`]; [`Error::InvalidState`] if the stored filter no longer compiles (the
/// field operations prevent it); or the database is unavailable.
pub async fn compiled(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Segment>,
) -> Result<Compiled, Error> {
    let row = row(tx, workspace, id)
        .await?
        .ok_or(Error::NotFound("segment"))?;
    compile(tx, workspace, &row.filter).await
}

async fn compile(tx: &mut Tx, workspace: WorkspaceId, filter: &Value) -> Result<Compiled, Error> {
    let definitions = super::fields::definitions(tx, workspace).await?;
    serde_json::from_value::<Filter>(filter.clone())
        .ok()
        .and_then(|filter| segments::compile(&filter, &definitions).ok())
        .ok_or_else(|| {
            Error::InvalidState(
                "The segment's filter no longer fits the workspace's fields.".to_owned(),
            )
        })
}

/// Reads a segment with its people counted now.
///
/// # Errors
///
/// [`Error::InvalidState`] if its filter no longer compiles, or the database is unavailable.
pub async fn retrieve(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Segment>,
) -> Result<Option<SegmentObject>, Error> {
    let Some(row) = row(tx, workspace, id).await? else {
        return Ok(None);
    };
    let compiled = compile(tx, workspace, &row.filter).await?;
    let counted = super::count(
        tx,
        workspace,
        &Selection::new(&PeopleFilters::default(), Some(compiled)),
        COUNT_CAP,
    )
    .await?;
    Ok(Some(row.object(Some(counted))?))
}

/// Creates a segment; `filter` was checked against the definitions read under the shared field
/// lock in this transaction.
///
/// # Errors
///
/// The database refused.
pub async fn create(
    tx: &mut Tx,
    workspace: WorkspaceId,
    name: &str,
    filter: &Filter,
) -> Result<SegmentObject, Error> {
    let id = sqlx::query_scalar!(
        r#"INSERT INTO segments (workspace_id, name, filter) VALUES ($1, $2, $3) RETURNING id AS "id: Id<Segment>""#,
        workspace.uuid(),
        name,
        serde_json::to_value(filter).map_err(|error| sqlx::Error::Encode(Box::new(error)))?,
    )
    .fetch_one(&mut **tx)
    .await?;
    retrieve(tx, workspace, id)
        .await?
        .ok_or(Error::NotFound("segment"))
}

/// Renames a segment or replaces its filter (checked as for [`create`]).
///
/// # Errors
///
/// [`Error::NotFound`], or the database refused.
pub async fn update(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Segment>,
    name: Option<&str>,
    filter: Option<&Filter>,
) -> Result<SegmentObject, Error> {
    let filter = filter
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    sqlx::query_scalar!(
        "UPDATE segments SET name = coalesce($3, name), filter = coalesce($4, filter) WHERE workspace_id = $1 AND id = $2 RETURNING id",
        workspace.uuid(),
        id.uuid(),
        name,
        filter,
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound("segment"))?;
    retrieve(tx, workspace, id)
        .await?
        .ok_or(Error::NotFound("segment"))
}

/// Deletes a segment. False when it does not exist.
///
/// # Errors
///
/// The database refused.
pub async fn delete(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Segment>,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar!(
        "DELETE FROM segments WHERE workspace_id = $1 AND id = $2 RETURNING id",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .is_some())
}
