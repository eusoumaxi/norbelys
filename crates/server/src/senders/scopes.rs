//! Quota scopes: the provider-side limits of an account the customer owns, shared by several of
//! its connections (a Microsoft tenant, an Amazon SES account and Region, a relay account, a
//! Google Cloud project of the customer's own). Norbelys's own Google project and Microsoft app
//! are platform configuration, never a scope.
//!
//! A scope holds daily limits (messages and recipients, counted by the sender in its own ledger
//! over today's and yesterday's UTC buckets), a short-window limit (`window_limit` units of
//! `window_unit` per `window_seconds`), and a durable pause that a provider's answer naming the
//! shared limit sets, so every replica sees it.
//!
//! Every SES connection names its account's scope, archived ones included: the schema refuses an
//! SES connection without one, so deleting a scope that SES connections name is refused, naming
//! them. Deleting any other scope removes its ledger; its connections keep running without one.
//!
//! An SES scope shows the webhook its account's configuration set posts to: its earliest-created
//! SES connection's, archived or not, so what it shows never changes. One subscription per account
//! delivers each event once, under one key; the event names our message by its tag, whichever
//! connection sent it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::connections::WebhookObject;
use super::{Error, Settings};
use crate::db::Tx;
use crate::domain::ids::{Connection, Id, ProviderWebhook, QuotaScope, WorkspaceId};
use crate::domain::time::Timestamp;
use crate::http::versioning;

/// What one submission is charged in a short window (`quota_scopes.window_unit`): one request,
/// its recipients, or the provider's own units.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumIter,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum WindowUnit {
    Requests,
    Recipients,
    Units,
}

impl WindowUnit {
    /// The unit as stored and shown.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// A quota scope as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct QuotaScopeObject {
    pub id: Id<QuotaScope>,
    /// The provider of the account; new values may be added.
    #[schema(value_type = crate::domain::senders::Provider)]
    pub provider: String,
    /// The account: a project id, a tenant id, `account:region`, a relay host.
    pub scope_key: String,
    /// Messages a rolling day across its connections.
    pub messages_per_day: Option<i32>,
    /// Recipients a rolling day across its connections.
    pub recipients_per_day: Option<i32>,
    /// Units a short window allows (SES: its maximum send rate).
    pub window_limit: Option<i32>,
    /// What one submission is charged in the short window.
    #[schema(value_type = Option<WindowUnit>)]
    pub window_unit: Option<String>,
    /// The short window's length, in seconds.
    pub window_seconds: Option<i32>,
    /// The provider paused the account until then.
    pub paused_until: Option<Timestamp>,
    /// What the provider said.
    pub paused_detail: Option<String>,
    /// For SES: the webhook the account's configuration set posts to.
    pub webhook: Option<WebhookObject>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The scope's version, also the response's `ETag`: `updated_at` in microseconds since the
    /// Unix epoch. An update sent with it in `If-Match` applies only to this version. A pause
    /// the provider imposes moves it too.
    pub version: i64,
}

struct Row {
    id: Id<QuotaScope>,
    provider: String,
    scope_key: String,
    messages_per_day: Option<i32>,
    recipients_per_day: Option<i32>,
    window_limit: Option<i32>,
    window_unit: Option<String>,
    window_seconds: Option<i32>,
    paused_until: Option<Timestamp>,
    paused_detail: Option<String>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

/// The limits of a scope, as written.
#[derive(Debug, Clone, Default)]
pub struct Limits {
    /// Messages a rolling day.
    pub messages_per_day: Option<i32>,
    /// Recipients a rolling day.
    pub recipients_per_day: Option<i32>,
    /// Units a short window allows; set with the two below, or none of them.
    pub window_limit: Option<i32>,
    /// `requests`, `recipients` or `units`.
    pub window_unit: Option<String>,
    /// The short window, in seconds.
    pub window_seconds: Option<i32>,
}

/// The rows with their SES webhooks: one query for a page.
async fn assemble(
    tx: &mut Tx,
    settings: &Settings,
    workspace: WorkspaceId,
    rows: Vec<Row>,
) -> Result<Vec<QuotaScopeObject>, sqlx::Error> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id.uuid()).collect();
    let webhooks: HashMap<Uuid, WebhookObject> = sqlx::query!(
        r#"SELECT DISTINCT ON (c.quota_scope_id) c.quota_scope_id AS "scope!", w.id AS "id: Id<ProviderWebhook>",
                  w.signing_secret IS NOT NULL AS "key_set!"
             FROM connections c
             JOIN provider_webhooks w ON w.workspace_id = c.workspace_id AND w.connection_id = c.id
            WHERE c.workspace_id = $1 AND c.provider = 'ses' AND c.quota_scope_id = ANY($2)
            ORDER BY c.quota_scope_id, c.created_at, c.id"#,
        workspace.uuid(),
        &ids,
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| {
        (
            row.scope,
            WebhookObject {
                id: row.id,
                url: settings.webhook_url(row.id),
                key_set: row.key_set,
            },
        )
    })
    .collect();
    Ok(rows
        .into_iter()
        .map(|row| QuotaScopeObject {
            webhook: webhooks.get(&row.id.uuid()).cloned(),
            id: row.id,
            provider: row.provider,
            scope_key: row.scope_key,
            messages_per_day: row.messages_per_day,
            recipients_per_day: row.recipients_per_day,
            window_limit: row.window_limit,
            window_unit: row.window_unit,
            window_seconds: row.window_seconds,
            paused_until: row.paused_until,
            paused_detail: row.paused_detail,
            created_at: row.created_at,
            updated_at: row.updated_at,
            version: versioning::of(row.updated_at),
        })
        .collect())
}

/// Locks a quota scope's row for an update and returns its version, for the update's `If-Match`
/// to be checked against before anything is written; `None` when the workspace has no such
/// scope. A scope's row comes before its connections' rows in every path's lock order.
///
/// # Errors
///
/// The database is unavailable.
pub async fn lock_version(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<QuotaScope>,
) -> Result<Option<i64>, sqlx::Error> {
    let updated_at = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM quota_scopes WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated_at.map(versioning::of))
}

/// Reads one quota scope of `workspace`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    settings: &Settings,
    workspace: WorkspaceId,
    id: Id<QuotaScope>,
) -> Result<Option<QuotaScopeObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"SELECT id AS "id: Id<QuotaScope>", provider, scope_key, messages_per_day, recipients_per_day, window_limit,
                  window_unit, window_seconds, paused_until AS "paused_until: Timestamp", paused_detail,
                  created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM quota_scopes WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    match row {
        Some(row) => Ok(assemble(tx, settings, workspace, vec![row]).await?.pop()),
        None => Ok(None),
    }
}

/// One page of `workspace`'s quota scopes of `provider` (all when `None`) in id order, after
/// `cursor` when given; `limit` rows.
///
/// # Errors
///
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    settings: &Settings,
    workspace: WorkspaceId,
    provider: Option<&str>,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<QuotaScopeObject>, sqlx::Error> {
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id AS "id: Id<QuotaScope>", provider, scope_key, messages_per_day, recipients_per_day, window_limit,
                  window_unit, window_seconds, paused_until AS "paused_until: Timestamp", paused_detail,
                  created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM quota_scopes
            WHERE workspace_id = $1 AND ($2::text IS NULL OR provider = $2)
              AND ($3::uuid IS NULL OR CASE WHEN $4 THEN id > $3 ELSE id < $3 END)
            ORDER BY CASE WHEN $4 THEN id END ASC, id DESC
            LIMIT $5"#,
        workspace.uuid(),
        provider,
        cursor,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    assemble(tx, settings, workspace, rows).await
}

/// Counts `workspace`'s quota scopes of `provider`, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    provider: Option<&str>,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM quota_scopes WHERE workspace_id = $1 AND ($2::text IS NULL OR provider = $2) LIMIT $3
           ) counted"#,
        workspace.uuid(),
        provider,
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// Creates a quota scope.
///
/// # Errors
///
/// The workspace has a scope of this provider and key (`409 conflict`), or the database refused.
pub async fn create(
    tx: &mut Tx,
    workspace: WorkspaceId,
    provider: &str,
    scope_key: &str,
    limits: &Limits,
) -> Result<Id<QuotaScope>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"INSERT INTO quota_scopes (workspace_id, provider, scope_key, messages_per_day, recipients_per_day,
                                     window_limit, window_unit, window_seconds)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           RETURNING id AS "id: Id<QuotaScope>""#,
        workspace.uuid(),
        provider,
        scope_key,
        limits.messages_per_day,
        limits.recipients_per_day,
        limits.window_limit,
        limits.window_unit,
        limits.window_seconds,
    )
    .fetch_one(&mut **tx)
    .await
}

/// A change of a scope's limits: `None` keeps a limit, `Some(None)` clears it.
#[derive(Debug, Clone, Default)]
pub struct LimitChanges {
    /// Messages a rolling day.
    pub messages_per_day: Option<Option<i32>>,
    /// Recipients a rolling day.
    pub recipients_per_day: Option<Option<i32>>,
    /// The short window, set or cleared whole: units, what one submission is charged, seconds.
    pub window: Option<Option<(i32, String, i32)>>,
}

/// Applies `changes` to a scope's limits; returns false when the workspace has no such scope.
///
/// # Errors
///
/// The database refused.
pub async fn update(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<QuotaScope>,
    changes: &LimitChanges,
) -> Result<bool, sqlx::Error> {
    let window = changes.window.clone().flatten();
    let updated = sqlx::query_scalar!(
        "UPDATE quota_scopes
            SET messages_per_day = CASE WHEN $3 THEN $4 ELSE messages_per_day END,
                recipients_per_day = CASE WHEN $5 THEN $6 ELSE recipients_per_day END,
                window_limit = CASE WHEN $7 THEN $8 ELSE window_limit END,
                window_unit = CASE WHEN $7 THEN $9 ELSE window_unit END,
                window_seconds = CASE WHEN $7 THEN $10 ELSE window_seconds END
          WHERE workspace_id = $1 AND id = $2 RETURNING id",
        workspace.uuid(),
        id.uuid(),
        changes.messages_per_day.is_some(),
        changes.messages_per_day.flatten(),
        changes.recipients_per_day.is_some(),
        changes.recipients_per_day.flatten(),
        changes.window.is_some(),
        window.as_ref().map(|window| window.0),
        window.as_ref().map(|window| window.1.clone()),
        window.as_ref().map(|window| window.2),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated.is_some())
}

/// Deletes a scope and its ledger. Refused while SES connections, archived ones included, name
/// it: their account's limits live on it.
///
/// # Errors
///
/// No such scope, SES connections name it (`InvalidState`, naming them), or the database
/// refused.
pub async fn delete(tx: &mut Tx, workspace: WorkspaceId, id: Id<QuotaScope>) -> Result<(), Error> {
    let exists = sqlx::query_scalar!(
        "SELECT 1 AS one FROM quota_scopes WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    if exists.is_none() {
        return Err(Error::NotFound("quota scope"));
    }
    let named: Vec<Id<Connection>> = sqlx::query_scalar!(
        r#"SELECT id AS "id: Id<Connection>" FROM connections
            WHERE workspace_id = $1 AND quota_scope_id = $2 AND provider = 'ses' ORDER BY id LIMIT 10"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    if !named.is_empty() {
        let names: Vec<String> = named.iter().map(ToString::to_string).collect();
        return Err(Error::InvalidState(format!(
            "SES connections name this scope ({}), archived ones included; an SES account's limits live on its scope.",
            names.join(", ")
        )));
    }
    sqlx::query!(
        "DELETE FROM quota_scopes WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}
