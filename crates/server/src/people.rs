//! The audience: people, their custom fields, groups, segments, suppressions, imports and
//! exports. This file holds the operations on people themselves; each other resource has its
//! own file, and `http.rs` serves them all under `/v1`.
//!
//! # People
//!
//! A person is one address in a workspace (`UNIQUE (workspace_id, email_key)`, the key being the
//! address in ASCII lowercase), with a given name, a family name, a company and custom fields.
//! Custom values are typed by the workspace's field definitions ([`fields`]): an API write is
//! checked against them before it reaches the database, and the database's
//! `people_field_definition` trigger checks again, so no writer stores a value of the wrong
//! type. A person belongs to at most 100 groups; the list is written whole on the person
//! (`group_ids`) or grown by an import, and read back on every person.
//!
//! A person with history (an enrollment, a message) cannot be deleted: the history keeps
//! pointing at the person, and the database refuses the delete (`409 invalid_state`). A person
//! without history is removed with its memberships.
//!
//! # Segments in lists
//!
//! `GET /people?segment_id=` lists a segment's people: the segment's filter is compiled
//! (`domain::segments`) into a SQL/JSON path predicate and its variables, and the list query
//! applies it to each person's document, which every query here builds the same way:
//!
//! ```sql
//! jsonb_build_object('email', p.email_key, 'email_domain', split_part(p.email_key, '@', 2),
//!     'given_name', p.given_name, 'family_name', p.family_name, 'company', p.company,
//!     'created_at', date_part('epoch', p.created_at),
//!     'fields', p.custom_fields)
//! ```
//!
//! # Lock order
//!
//! Every write that touches custom values takes the workspace's field lock (a transaction-level
//! advisory lock on `people-fields:<workspace>`) first: shared for person writes and import
//! chunks, exclusive for a change of a definition. Then the people rows (an import upserts its
//! chunk in address order, so two imports never wait on each other in opposite orders), then
//! their memberships. A group's deletion locks the group row before its memberships. An update
//! of one person ([`lock_version`]) takes the field lock first even when it changes no custom
//! value: its `UPDATE` fires the field trigger, which would otherwise take the lock after the
//! row's, the opposite of a field deletion's order (the lock, then the rows holding its values),
//! and the two could deadlock.
//!
//! # Versions
//!
//! A person carries `version` (see `http::versioning`), and its object shows `group_ids`, rows
//! of `group_people`. Every write of a person's memberships therefore updates the person's row in
//! the same transaction, so the version moves with the list: an update runs its `UPDATE` whatever
//! it changes, and an import's upsert updates every person who existed already. Deleting a group
//! is the exception the versioning module explains: it cannot cause a lost update.

pub(crate) mod cleanup;
pub mod exports;
pub mod fields;
pub mod groups;
pub mod http;
pub mod imports;
pub mod segments;
pub mod suppressions;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::db::Tx;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Group, Id, Person, Segment, WorkspaceId};
use crate::domain::segments::Compiled;
use crate::domain::time::Timestamp;
use crate::http::versioning;
use crate::storage::StorageError;

/// Groups a person may belong to.
pub const GROUPS_MAX: usize = 100;

/// Why an audience operation did not happen. The HTTP mapping is `http.rs`'s.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The named resource does not exist in the workspace.
    #[error("no such {0}")]
    NotFound(&'static str),
    /// A unique value is taken.
    #[error("{0}")]
    Conflict(String),
    /// The resource's state or its dependants forbid the change.
    #[error("{0}")]
    InvalidState(String),
    /// The input is unusable as a whole: the RFC 6901 pointer of the part at fault (empty for
    /// the whole body) and what is wrong.
    #[error("{1}")]
    Invalid(String, String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// A search's `LIKE` pattern: the trimmed text in lowercase, its `\`, `%` and `_` escaped
/// (backslash is `LIKE`'s default escape), followed by `%`; `None` for an empty search.
pub(crate) fn like_prefix(q: &str) -> Option<String> {
    let q = q.trim();
    if q.is_empty() {
        return None;
    }
    let escaped: String = q
        .to_lowercase()
        .chars()
        .flat_map(|c| match c {
            '\\' | '%' | '_' => vec!['\\', c],
            other => vec![other],
        })
        .collect();
    Some(format!("{escaped}%"))
}

/// The SQLSTATE of a database error, if it is one.
pub(crate) fn sqlstate(error: &sqlx::Error) -> Option<String> {
    match error {
        sqlx::Error::Database(database) => database.code().map(|code| code.into_owned()),
        _ => None,
    }
}

/// A person as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PersonObject {
    pub id: Id<Person>,
    /// The address as it was written; it is unique in the workspace ignoring ASCII case.
    pub email: String,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub company: Option<String>,
    /// The person's custom values, by field key.
    #[schema(value_type = Object)]
    pub fields: Value,
    /// The groups the person belongs to, at most 100.
    pub group_ids: Vec<Id<Group>>,
    /// When mail was last sent to the person.
    pub last_sent_at: Option<Timestamp>,
    /// When the person last replied.
    pub replied_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The person's version, also the response's `ETag`: `updated_at` in microseconds since
    /// the Unix epoch. An update sent with it in `If-Match` applies only to this version, so a
    /// replacement of `group_ids` never erases a membership added since it was read.
    pub version: i64,
}

/// A person's row, before its memberships are attached.
pub(crate) struct PersonRow {
    pub id: Id<Person>,
    pub email: String,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub company: Option<String>,
    pub custom_fields: Value,
    pub last_sent_at: Option<Timestamp>,
    pub replied_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl PersonRow {
    fn object(self, group_ids: Vec<Id<Group>>) -> PersonObject {
        PersonObject {
            id: self.id,
            email: self.email,
            given_name: self.given_name,
            family_name: self.family_name,
            company: self.company,
            fields: self.custom_fields,
            group_ids,
            last_sent_at: self.last_sent_at,
            replied_at: self.replied_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
            version: versioning::of(self.updated_at),
        }
    }
}

/// The filters of the people list, and of an export of people. Each narrows the list; together
/// they all apply.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PeopleFilters {
    /// The person with this address (ignoring ASCII case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Members of this group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub group_id: Option<Id<Group>>,
    /// People matching this segment's filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub segment_id: Option<Id<Segment>>,
    /// Text anywhere in the address, full name, company or custom field values (ignoring case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Bounds on creation time, flattened into the list's filter keys.
    #[serde(flatten)]
    pub created: crate::http::extract::CreatedRange,
}

/// [`PeopleFilters`] ready for the queries: the substring pattern escaped for `LIKE`, the segment
/// compiled.
pub(crate) struct Selection {
    email: Option<String>,
    group: Option<Uuid>,
    pattern: Option<String>,
    created_gte: Option<Timestamp>,
    created_gt: Option<Timestamp>,
    created_lte: Option<Timestamp>,
    created_lt: Option<Timestamp>,
    path: Option<String>,
    vars: Value,
}

impl Selection {
    /// The selection of `filters`, with the segment's compiled filter when one is named.
    pub(crate) fn new(filters: &PeopleFilters, segment: Option<Compiled>) -> Self {
        let pattern = filters
            .q
            .as_deref()
            .and_then(like_prefix)
            .map(|prefix| format!("%{prefix}"));
        let (path, vars) = match segment {
            Some(compiled) => (Some(compiled.path), compiled.vars),
            None => (None, Value::Object(Map::new())),
        };
        Self {
            email: filters.email.clone(),
            group: filters.group_id.map(|group| group.uuid()),
            pattern,
            created_gte: filters.created.gte,
            created_gt: filters.created.gt,
            created_lte: filters.created.lte,
            created_lt: filters.created.lt,
            path,
            vars,
        }
    }
}

/// One page of `workspace`'s people matching `selection`, in id order after `cursor`, with
/// their memberships. Fetches `limit` rows; the caller asks for one more than it shows.
///
/// # Errors
///
/// The database is unavailable.
pub(crate) async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    selection: &Selection,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<PersonObject>, sqlx::Error> {
    let s = selection;
    // Two statements, one per direction, so each walks the primary key in its own order.
    let rows = if ascending {
        sqlx::query_as!(
            PersonRow,
            r#"SELECT p.id AS "id: Id<Person>", p.email, p.given_name, p.family_name, p.company, p.custom_fields,
                      p.last_sent_at AS "last_sent_at: Timestamp", p.replied_at AS "replied_at: Timestamp",
                      p.created_at AS "created_at: Timestamp", p.updated_at AS "updated_at: Timestamp"
                 FROM people p
                WHERE p.workspace_id = $1 AND ($2::uuid IS NULL OR p.id > $2)
                  AND ($3::text IS NULL OR p.email_key = ascii_lower($3))
                  AND ($4::uuid IS NULL OR EXISTS (SELECT 1 FROM group_people g
                        WHERE g.workspace_id = p.workspace_id AND g.group_id = $4 AND g.person_id = p.id))
                  AND ($5::text IS NULL OR lower(p.email) LIKE $5
                       OR lower(concat_ws(' ', p.given_name, p.family_name)) LIKE $5
                       OR lower(p.company) LIKE $5
                       OR EXISTS (SELECT 1 FROM jsonb_each_text(p.custom_fields) field
                            JOIN person_field_definitions definition
                              ON definition.workspace_id = p.workspace_id AND definition.key = field.key
                             AND definition.deleted_at IS NULL
                            WHERE lower(field.value) LIKE $5
                               OR (definition.field_type = 'boolean'
                                   AND (CASE field.value WHEN 'true' THEN 'yes' WHEN 'false' THEN 'no' END) LIKE $5)
                               OR ($5 ~ '^%[+0-9() .-]+%$'
                                   AND length(regexp_replace($5, '[^0-9]', '', 'g')) >= 3
                                   AND field.value ~ '^[+0-9() .-]+$'
                                   AND regexp_replace(field.value, '[^0-9]', '', 'g')
                                       LIKE '%' || regexp_replace($5, '[^0-9]', '', 'g') || '%')))
                  AND ($6::timestamptz IS NULL OR p.created_at >= $6) AND ($7::timestamptz IS NULL OR p.created_at > $7)
                  AND ($8::timestamptz IS NULL OR p.created_at <= $8) AND ($9::timestamptz IS NULL OR p.created_at < $9)
                  AND ($10::text IS NULL OR coalesce(jsonb_path_match(
                        jsonb_build_object('email', p.email_key, 'email_domain', split_part(p.email_key, '@', 2),
                            'given_name', p.given_name, 'family_name', p.family_name, 'company', p.company,
                            'created_at', date_part('epoch', p.created_at), 'fields', p.custom_fields),
                        $10::text::jsonpath, $11::jsonb, true), false))
                ORDER BY p.id LIMIT $12"#,
            workspace.uuid(),
            cursor,
            s.email,
            s.group,
            s.pattern,
            s.created_gte as _,
            s.created_gt as _,
            s.created_lte as _,
            s.created_lt as _,
            s.path,
            s.vars,
            limit,
        )
        .fetch_all(&mut **tx)
        .await?
    } else {
        sqlx::query_as!(
            PersonRow,
            r#"SELECT p.id AS "id: Id<Person>", p.email, p.given_name, p.family_name, p.company, p.custom_fields,
                      p.last_sent_at AS "last_sent_at: Timestamp", p.replied_at AS "replied_at: Timestamp",
                      p.created_at AS "created_at: Timestamp", p.updated_at AS "updated_at: Timestamp"
                 FROM people p
                WHERE p.workspace_id = $1 AND ($2::uuid IS NULL OR p.id < $2)
                  AND ($3::text IS NULL OR p.email_key = ascii_lower($3))
                  AND ($4::uuid IS NULL OR EXISTS (SELECT 1 FROM group_people g
                        WHERE g.workspace_id = p.workspace_id AND g.group_id = $4 AND g.person_id = p.id))
                  AND ($5::text IS NULL OR lower(p.email) LIKE $5
                       OR lower(concat_ws(' ', p.given_name, p.family_name)) LIKE $5
                       OR lower(p.company) LIKE $5
                       OR EXISTS (SELECT 1 FROM jsonb_each_text(p.custom_fields) field
                            JOIN person_field_definitions definition
                              ON definition.workspace_id = p.workspace_id AND definition.key = field.key
                             AND definition.deleted_at IS NULL
                            WHERE lower(field.value) LIKE $5
                               OR (definition.field_type = 'boolean'
                                   AND (CASE field.value WHEN 'true' THEN 'yes' WHEN 'false' THEN 'no' END) LIKE $5)
                               OR ($5 ~ '^%[+0-9() .-]+%$'
                                   AND length(regexp_replace($5, '[^0-9]', '', 'g')) >= 3
                                   AND field.value ~ '^[+0-9() .-]+$'
                                   AND regexp_replace(field.value, '[^0-9]', '', 'g')
                                       LIKE '%' || regexp_replace($5, '[^0-9]', '', 'g') || '%')))
                  AND ($6::timestamptz IS NULL OR p.created_at >= $6) AND ($7::timestamptz IS NULL OR p.created_at > $7)
                  AND ($8::timestamptz IS NULL OR p.created_at <= $8) AND ($9::timestamptz IS NULL OR p.created_at < $9)
                  AND ($10::text IS NULL OR coalesce(jsonb_path_match(
                        jsonb_build_object('email', p.email_key, 'email_domain', split_part(p.email_key, '@', 2),
                            'given_name', p.given_name, 'family_name', p.family_name, 'company', p.company,
                            'created_at', date_part('epoch', p.created_at), 'fields', p.custom_fields),
                        $10::text::jsonpath, $11::jsonb, true), false))
                ORDER BY p.id DESC LIMIT $12"#,
            workspace.uuid(),
            cursor,
            s.email,
            s.group,
            s.pattern,
            s.created_gte as _,
            s.created_gt as _,
            s.created_lte as _,
            s.created_lt as _,
            s.path,
            s.vars,
            limit,
        )
        .fetch_all(&mut **tx)
        .await?
    };
    with_groups(tx, workspace, rows).await
}

/// One page of `workspace`'s people matching `selection`, by when each last changed
/// (`updated_at`, then id), after `after` (the instant and the id of the last row seen), with
/// their memberships: the `updated_at` sort of `GET /people`, which `people_by_updated` serves.
/// Fetches `limit` rows; the caller asks for one more than it shows.
///
/// # Errors
///
/// The database is unavailable.
pub(crate) async fn list_by_update(
    tx: &mut Tx,
    workspace: WorkspaceId,
    selection: &Selection,
    after: Option<(Timestamp, Uuid)>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<PersonObject>, sqlx::Error> {
    let s = selection;
    let (after_at, cursor) = after.unzip();
    // Two statements, one per direction, so each walks `people_by_updated` in its own order; the
    // column is never NULL and the id breaks its ties.
    let rows = if ascending {
        sqlx::query_as!(
            PersonRow,
            r#"SELECT p.id AS "id: Id<Person>", p.email, p.given_name, p.family_name, p.company, p.custom_fields,
                      p.last_sent_at AS "last_sent_at: Timestamp", p.replied_at AS "replied_at: Timestamp",
                      p.created_at AS "created_at: Timestamp", p.updated_at AS "updated_at: Timestamp"
                 FROM people p
                WHERE p.workspace_id = $1
                  AND ($13::timestamptz IS NULL OR (p.updated_at, p.id) > ($13::timestamptz, $2::uuid))
                  AND ($3::text IS NULL OR p.email_key = ascii_lower($3))
                  AND ($4::uuid IS NULL OR EXISTS (SELECT 1 FROM group_people g
                        WHERE g.workspace_id = p.workspace_id AND g.group_id = $4 AND g.person_id = p.id))
                  AND ($5::text IS NULL OR lower(p.email) LIKE $5
                       OR lower(concat_ws(' ', p.given_name, p.family_name)) LIKE $5
                       OR lower(p.company) LIKE $5
                       OR EXISTS (SELECT 1 FROM jsonb_each_text(p.custom_fields) field
                            JOIN person_field_definitions definition
                              ON definition.workspace_id = p.workspace_id AND definition.key = field.key
                             AND definition.deleted_at IS NULL
                            WHERE lower(field.value) LIKE $5
                               OR (definition.field_type = 'boolean'
                                   AND (CASE field.value WHEN 'true' THEN 'yes' WHEN 'false' THEN 'no' END) LIKE $5)
                               OR ($5 ~ '^%[+0-9() .-]+%$'
                                   AND length(regexp_replace($5, '[^0-9]', '', 'g')) >= 3
                                   AND field.value ~ '^[+0-9() .-]+$'
                                   AND regexp_replace(field.value, '[^0-9]', '', 'g')
                                       LIKE '%' || regexp_replace($5, '[^0-9]', '', 'g') || '%')))
                  AND ($6::timestamptz IS NULL OR p.created_at >= $6) AND ($7::timestamptz IS NULL OR p.created_at > $7)
                  AND ($8::timestamptz IS NULL OR p.created_at <= $8) AND ($9::timestamptz IS NULL OR p.created_at < $9)
                  AND ($10::text IS NULL OR coalesce(jsonb_path_match(
                        jsonb_build_object('email', p.email_key, 'email_domain', split_part(p.email_key, '@', 2),
                            'given_name', p.given_name, 'family_name', p.family_name, 'company', p.company,
                            'created_at', date_part('epoch', p.created_at), 'fields', p.custom_fields),
                        $10::text::jsonpath, $11::jsonb, true), false))
                ORDER BY p.updated_at, p.id LIMIT $12"#,
            workspace.uuid(),
            cursor,
            s.email,
            s.group,
            s.pattern,
            s.created_gte as _,
            s.created_gt as _,
            s.created_lte as _,
            s.created_lt as _,
            s.path,
            s.vars,
            limit,
            after_at as _,
        )
        .fetch_all(&mut **tx)
        .await?
    } else {
        sqlx::query_as!(
            PersonRow,
            r#"SELECT p.id AS "id: Id<Person>", p.email, p.given_name, p.family_name, p.company, p.custom_fields,
                      p.last_sent_at AS "last_sent_at: Timestamp", p.replied_at AS "replied_at: Timestamp",
                      p.created_at AS "created_at: Timestamp", p.updated_at AS "updated_at: Timestamp"
                 FROM people p
                WHERE p.workspace_id = $1
                  AND ($13::timestamptz IS NULL OR (p.updated_at, p.id) < ($13::timestamptz, $2::uuid))
                  AND ($3::text IS NULL OR p.email_key = ascii_lower($3))
                  AND ($4::uuid IS NULL OR EXISTS (SELECT 1 FROM group_people g
                        WHERE g.workspace_id = p.workspace_id AND g.group_id = $4 AND g.person_id = p.id))
                  AND ($5::text IS NULL OR lower(p.email) LIKE $5
                       OR lower(concat_ws(' ', p.given_name, p.family_name)) LIKE $5
                       OR lower(p.company) LIKE $5
                       OR EXISTS (SELECT 1 FROM jsonb_each_text(p.custom_fields) field
                            JOIN person_field_definitions definition
                              ON definition.workspace_id = p.workspace_id AND definition.key = field.key
                             AND definition.deleted_at IS NULL
                            WHERE lower(field.value) LIKE $5
                               OR (definition.field_type = 'boolean'
                                   AND (CASE field.value WHEN 'true' THEN 'yes' WHEN 'false' THEN 'no' END) LIKE $5)
                               OR ($5 ~ '^%[+0-9() .-]+%$'
                                   AND length(regexp_replace($5, '[^0-9]', '', 'g')) >= 3
                                   AND field.value ~ '^[+0-9() .-]+$'
                                   AND regexp_replace(field.value, '[^0-9]', '', 'g')
                                       LIKE '%' || regexp_replace($5, '[^0-9]', '', 'g') || '%')))
                  AND ($6::timestamptz IS NULL OR p.created_at >= $6) AND ($7::timestamptz IS NULL OR p.created_at > $7)
                  AND ($8::timestamptz IS NULL OR p.created_at <= $8) AND ($9::timestamptz IS NULL OR p.created_at < $9)
                  AND ($10::text IS NULL OR coalesce(jsonb_path_match(
                        jsonb_build_object('email', p.email_key, 'email_domain', split_part(p.email_key, '@', 2),
                            'given_name', p.given_name, 'family_name', p.family_name, 'company', p.company,
                            'created_at', date_part('epoch', p.created_at), 'fields', p.custom_fields),
                        $10::text::jsonpath, $11::jsonb, true), false))
                ORDER BY p.updated_at DESC, p.id DESC LIMIT $12"#,
            workspace.uuid(),
            cursor,
            s.email,
            s.group,
            s.pattern,
            s.created_gte as _,
            s.created_gt as _,
            s.created_lte as _,
            s.created_lt as _,
            s.path,
            s.vars,
            limit,
            after_at as _,
        )
        .fetch_all(&mut **tx)
        .await?
    };
    with_groups(tx, workspace, rows).await
}

/// Counts `workspace`'s people matching `selection`, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub(crate) async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    selection: &Selection,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    let s = selection;
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM people p
                WHERE p.workspace_id = $1
                  AND ($2::text IS NULL OR p.email_key = ascii_lower($2))
                  AND ($3::uuid IS NULL OR EXISTS (SELECT 1 FROM group_people g
                        WHERE g.workspace_id = p.workspace_id AND g.group_id = $3 AND g.person_id = p.id))
                  AND ($4::text IS NULL OR lower(p.email) LIKE $4
                       OR lower(concat_ws(' ', p.given_name, p.family_name)) LIKE $4
                       OR lower(p.company) LIKE $4
                       OR EXISTS (SELECT 1 FROM jsonb_each_text(p.custom_fields) field
                            JOIN person_field_definitions definition
                              ON definition.workspace_id = p.workspace_id AND definition.key = field.key
                             AND definition.deleted_at IS NULL
                            WHERE lower(field.value) LIKE $4
                               OR (definition.field_type = 'boolean'
                                   AND (CASE field.value WHEN 'true' THEN 'yes' WHEN 'false' THEN 'no' END) LIKE $4)
                               OR ($4 ~ '^%[+0-9() .-]+%$'
                                   AND length(regexp_replace($4, '[^0-9]', '', 'g')) >= 3
                                   AND field.value ~ '^[+0-9() .-]+$'
                                   AND regexp_replace(field.value, '[^0-9]', '', 'g')
                                       LIKE '%' || regexp_replace($4, '[^0-9]', '', 'g') || '%')))
                  AND ($5::timestamptz IS NULL OR p.created_at >= $5) AND ($6::timestamptz IS NULL OR p.created_at > $6)
                  AND ($7::timestamptz IS NULL OR p.created_at <= $7) AND ($8::timestamptz IS NULL OR p.created_at < $8)
                  AND ($9::text IS NULL OR coalesce(jsonb_path_match(
                        jsonb_build_object('email', p.email_key, 'email_domain', split_part(p.email_key, '@', 2),
                            'given_name', p.given_name, 'family_name', p.family_name, 'company', p.company,
                            'created_at', date_part('epoch', p.created_at), 'fields', p.custom_fields),
                        $9::text::jsonpath, $10::jsonb, true), false))
                LIMIT $11) counted"#,
        workspace.uuid(),
        s.email,
        s.group,
        s.pattern,
        s.created_gte as _,
        s.created_gt as _,
        s.created_lte as _,
        s.created_lt as _,
        s.path,
        s.vars,
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// Attaches each person's group ids, read for the whole page in one query.
async fn with_groups(
    tx: &mut Tx,
    workspace: WorkspaceId,
    rows: Vec<PersonRow>,
) -> Result<Vec<PersonObject>, sqlx::Error> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id.uuid()).collect();
    let mut groups: HashMap<Uuid, Vec<Id<Group>>> = HashMap::new();
    for membership in sqlx::query!(
        r#"SELECT person_id, group_id AS "group_id: Id<Group>" FROM group_people m
            WHERE workspace_id = $1 AND person_id = ANY($2) AND EXISTS (SELECT 1 FROM groups g WHERE g.workspace_id = m.workspace_id AND g.id = m.group_id AND g.deleted_at IS NULL) ORDER BY person_id, group_id"#,
        workspace.uuid(),
        &ids,
    )
    .fetch_all(&mut **tx)
    .await?
    {
        groups
            .entry(membership.person_id)
            .or_default()
            .push(membership.group_id);
    }
    let deleted_keys: Vec<String> = sqlx::query_scalar("SELECT key FROM person_field_definitions WHERE workspace_id = $1 AND deleted_at IS NOT NULL")
        .bind(workspace.uuid()).fetch_all(&mut **tx).await?;
    Ok(rows
        .into_iter()
        .map(|mut row| {
            if let Some(fields) = row.custom_fields.as_object_mut() {
                for key in &deleted_keys {
                    fields.remove(key);
                }
            }
            let group_ids = groups.remove(&row.id.uuid()).unwrap_or_default();
            row.object(group_ids)
        })
        .collect())
}

/// Reads one person of `workspace`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Person>,
) -> Result<Option<PersonObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        PersonRow,
        r#"SELECT id AS "id: Id<Person>", email, given_name, family_name, company, custom_fields,
                  last_sent_at AS "last_sent_at: Timestamp", replied_at AS "replied_at: Timestamp",
                  created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM people WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else { return Ok(None) };
    Ok(with_groups(tx, workspace, vec![row])
        .await?
        .into_iter()
        .next())
}

/// A person to create; its custom values are already checked against the definitions read
/// under the field lock (see [`fields::lock_shared`]).
pub struct NewPerson {
    pub email: EmailAddress,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub company: Option<String>,
    /// Custom values; a null value is not stored.
    pub fields: Map<String, Value>,
    pub group_ids: Vec<Id<Group>>,
}

/// Creates a person with its memberships.
///
/// # Errors
///
/// [`Error::Conflict`] when the address is taken, [`Error::NotFound`] for a group that does not
/// exist, or the database refused.
pub async fn create(
    tx: &mut Tx,
    workspace: WorkspaceId,
    person: &NewPerson,
) -> Result<PersonObject, Error> {
    let fields: Map<String, Value> = person
        .fields
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let id = sqlx::query_scalar!(
        r#"INSERT INTO people (workspace_id, email, given_name, family_name, company, custom_fields)
           VALUES ($1, $2, $3, $4, $5, $6) RETURNING id AS "id: Id<Person>""#,
        workspace.uuid(),
        person.email.as_str(),
        person.given_name,
        person.family_name,
        person.company,
        Value::Object(fields),
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(taken)?;
    set_groups(tx, workspace, id, &person.group_ids).await?;
    read(tx, workspace, id)
        .await?
        .ok_or(Error::NotFound("person"))
}

/// Locks a person's row for an update and returns its version, for the update's `If-Match` to
/// be checked against before anything is written; `None` when the workspace has no such person.
/// The workspace's field lock is taken shared first, the order every writer of people follows
/// (see the module): the row's update fires the field trigger, which takes that lock anyway.
///
/// # Errors
///
/// The database is unavailable.
pub async fn lock_version(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Person>,
) -> Result<Option<i64>, sqlx::Error> {
    fields::lock_shared(tx, workspace).await?;
    let updated_at = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM people WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated_at.map(versioning::of))
}

/// A change to a person: absent members stay as they are.
#[derive(Default)]
pub struct PersonChanges {
    pub email: Option<EmailAddress>,
    /// `Some(None)` clears the name.
    pub given_name: Option<Option<String>>,
    pub family_name: Option<Option<String>>,
    pub company: Option<Option<String>>,
    /// Merged into the person's values; a null value removes its key.
    pub fields: Option<Map<String, Value>>,
    /// Replaces the person's memberships whole.
    pub group_ids: Option<Vec<Id<Group>>>,
}

/// Changes a person; its custom values are already checked under the field lock.
///
/// # Errors
///
/// [`Error::NotFound`] for an absent person or group, [`Error::Conflict`] when the new address
/// is taken, or the database refused.
pub async fn update(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Person>,
    changes: &PersonChanges,
) -> Result<PersonObject, Error> {
    let updated = sqlx::query_scalar!(
        r#"UPDATE people SET email = coalesce($3, email),
                  given_name = CASE WHEN $4 THEN $5 ELSE given_name END,
                  family_name = CASE WHEN $6 THEN $7 ELSE family_name END,
                  company = CASE WHEN $8 THEN $9 ELSE company END,
                  custom_fields = CASE WHEN $10::jsonb IS NULL THEN custom_fields
                                       ELSE jsonb_strip_nulls(custom_fields || $10) END
            WHERE workspace_id = $1 AND id = $2
           RETURNING id AS "id: Id<Person>""#,
        workspace.uuid(),
        id.uuid(),
        changes.email.as_ref().map(EmailAddress::as_str),
        changes.given_name.is_some(),
        changes.given_name.clone().flatten(),
        changes.family_name.is_some(),
        changes.family_name.clone().flatten(),
        changes.company.is_some(),
        changes.company.clone().flatten(),
        changes.fields.clone().map(Value::Object),
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(taken)?
    .ok_or(Error::NotFound("person"))?;
    if let Some(group_ids) = &changes.group_ids {
        sqlx::query!(
            "DELETE FROM group_people WHERE workspace_id = $1 AND person_id = $2 AND group_id <> ALL($3)",
            workspace.uuid(),
            updated.uuid(),
            &group_ids.iter().map(|group| group.uuid()).collect::<Vec<_>>(),
        )
        .execute(&mut **tx)
        .await?;
        set_groups(tx, workspace, updated, group_ids).await?;
    }
    read(tx, workspace, updated)
        .await?
        .ok_or(Error::NotFound("person"))
}

/// Adds `person` to each of `groups` it is not in yet.
async fn set_groups(
    tx: &mut Tx,
    workspace: WorkspaceId,
    person: Id<Person>,
    groups: &[Id<Group>],
) -> Result<(), Error> {
    if groups.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "INSERT INTO group_people (workspace_id, group_id, person_id)
         SELECT $1, g, $2 FROM unnest($3::uuid[]) AS g ON CONFLICT DO NOTHING",
        workspace.uuid(),
        person.uuid(),
        &groups.iter().map(|group| group.uuid()).collect::<Vec<_>>(),
    )
    .execute(&mut **tx)
    .await
    .map_err(|error| match sqlstate(&error).as_deref() {
        Some("23503") => Error::NotFound("group"),
        _ => Error::Db(error),
    })?;
    Ok(())
}

/// A unique violation on the address is a taken address; anything else stays as it is.
fn taken(error: sqlx::Error) -> Error {
    match sqlstate(&error).as_deref() {
        Some("23505") => Error::Conflict("A person with this email exists already.".to_owned()),
        _ => Error::Db(error),
    }
}

/// Deletes a person and its memberships, unless history refers to it.
///
/// # Errors
///
/// [`Error::NotFound`], [`Error::InvalidState`] when an enrollment or a message refers to the
/// person, or the database refused.
pub async fn delete(tx: &mut Tx, workspace: WorkspaceId, id: Id<Person>) -> Result<(), Error> {
    sqlx::query!(
        "DELETE FROM group_people WHERE workspace_id = $1 AND person_id = $2",
        workspace.uuid(),
        id.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    let deleted = sqlx::query_scalar!(
        "DELETE FROM people WHERE workspace_id = $1 AND id = $2 RETURNING id",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| match sqlstate(&error).as_deref() {
        // restrict_violation: an `ON DELETE RESTRICT` reference (enrollments, messages).
        Some("23001" | "23503") => Error::InvalidState(
            "The person has enrollments or messages, which keep their history; it cannot be deleted."
                .to_owned(),
        ),
        _ => Error::Db(error),
    })?;
    deleted.map(|_| ()).ok_or(Error::NotFound("person"))
}
