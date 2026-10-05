//! Workspaces: the tenant every other resource belongs to.
//!
//! # Operations
//!
//! - **Retrieve** (public): a credential reads only its own workspace; any other id is not found.
//! - **List** and **create** (the dashboard, with the session cookie): a signed-in user lists the
//!   workspaces they are an active member of, and creates one, becoming its owner. These run
//!   before any workspace token exists, so they read memberships under the user policy.
//! - **Update** (the dashboard, with a workspace token and `workspace:manage`): the name, the
//!   IANA time zone, the settings and the mode (`live` or `test`). Switching the mode retires
//!   every API key of the other mode at once, since a key's prefix names its mode. Updates take
//!   `If-Match` against `version`.
//! - **Delete** (the dashboard, owners only): requests deletion. The workspace is marked deleted
//!   at once, so every credential of it stops working within a minute and it leaves its members'
//!   lists; the request is queued in `workspace_deletions`, and the data is erased after a
//!   30-day tombstone during which an operator can still restore it.
//!
//! Creating a workspace with its first owner and API key is also an operator command
//! (`norbelys-server admin create-workspace`), for deployments that start without the dashboard.
//!
//! # Creating under row security
//!
//! A new workspace's membership row is under the workspace policy, so the creating transaction
//! picks the workspace's id first, sets it as the transaction's workspace, then inserts the
//! workspace and the owner's membership: the policy admits exactly that workspace's rows.

use axum::extract::State;
use serde::Serialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use super::api_keys::{self, KeyMode};
use super::audit::{self, Action, AuditActor};
use super::authority::Principal;
use crate::crypto;
use crate::db::{self, Database, Tx};
use crate::domain::email::EmailAddress;
use crate::domain::identity::slug_from_name;
use crate::domain::ids::{ApiKey, Id, User, Workspace, WorkspaceId};
use crate::domain::scope::{MembershipRole, Scope, ScopeSet};
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::Path;
use crate::http::versioning::{self, IfMatch, Tagged};
use crate::problem::{ApiResult, Problem};

/// A workspace as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct WorkspaceObject {
    pub id: Id<Workspace>,
    pub slug: String,
    pub name: String,
    /// `live` or `test`; a test workspace never reaches a provider, and its keys are `nb_test_`
    /// keys. New values may be added.
    #[schema(value_type = KeyMode)]
    pub mode: String,
    /// The IANA time zone of the workspace's dates.
    pub timezone: String,
    /// The workspace's settings (`settings.ai`: the AI switches and budget).
    pub settings: serde_json::Value,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The version `If-Match` names.
    pub version: i64,
    /// When deletion was requested; the workspace is erased 30 days later.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletion_requested_at: Option<Timestamp>,
    /// The current UTC month's usage: sends, people, connections and AI spend with their limits
    /// and `computed_at`. Present when the workspace is retrieved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::analytics::usage::WorkspaceUsage>,
}

struct Row {
    id: Id<Workspace>,
    slug: String,
    name: String,
    mode: String,
    timezone: String,
    settings: serde_json::Value,
    created_at: Timestamp,
    updated_at: Timestamp,
    deleted_at: Option<Timestamp>,
}

impl From<Row> for WorkspaceObject {
    fn from(row: Row) -> Self {
        Self {
            id: row.id,
            slug: row.slug,
            name: row.name,
            mode: row.mode,
            timezone: row.timezone,
            settings: row.settings,
            created_at: row.created_at,
            version: versioning::of(row.updated_at),
            updated_at: row.updated_at,
            deletion_requested_at: row.deleted_at,
            usage: None,
        }
    }
}

/// The public routes of this module.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(retrieve))
}

/// Retrieve a workspace.
///
/// Only the credential's own workspace exists for it; any other id is not found.
#[utoipa::path(
    get,
    path = "/workspaces/{id}",
    tag = "Workspace",
    operation_id = "workspaces.retrieve",
    params(("id" = Id<Workspace>, Path, description = "The workspace id (`ws_…`).")),
    responses(
        (status = 200, description = "The workspace.", body = WorkspaceObject,
         headers(("ETag" = String, description = "The workspace's version, for `If-Match`."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `workspace:read`."),
        (status = 404, description = "No such workspace for this credential."),
    ),
    security(("bearer" = []))
)]
async fn retrieve(
    principal: Principal,
    State(db): State<Database>,
    Path(id): Path<Id<Workspace>>,
) -> ApiResult<Tagged<WorkspaceObject>> {
    principal.require(Scope::WorkspaceRead)?;
    if id != principal.workspace.id() {
        return Err(Problem::not_found("workspace"));
    }
    let mut tx = db.begin_in(principal.workspace).await?;
    let workspace = read(&mut tx, principal.workspace)
        .await?
        .ok_or_else(|| Problem::not_found("workspace"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: workspace.version,
        body: workspace,
    })
}

/// Reads a live workspace (not being deleted), with its usage.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
) -> Result<Option<WorkspaceObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"SELECT id AS "id: Id<Workspace>", slug, name, mode, timezone, settings,
                  created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp",
                  deleted_at AS "deleted_at: Timestamp"
             FROM workspaces WHERE id = $1 AND deleted_at IS NULL"#,
        workspace.uuid()
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let mut object = WorkspaceObject::from(row);
    object.usage = Some(crate::analytics::usage::read(tx, workspace, crate::process::now()).await?);
    Ok(Some(object))
}

/// One page of the workspaces `user` is an active member of, by id, with their role in each;
/// read under the user policy (the transaction runs as the user).
///
/// # Errors
///
/// The database is unavailable.
pub async fn of_user(
    tx: &mut Tx,
    user: Id<User>,
    after: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<(WorkspaceObject, MembershipRole)>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT w.id AS "id: Id<Workspace>", w.slug, w.name, w.mode, w.timezone, w.settings,
                  w.created_at AS "created_at: Timestamp", w.updated_at AS "updated_at: Timestamp", m.role
             FROM memberships m JOIN workspaces w ON w.id = m.workspace_id
            WHERE m.user_id = $1 AND m.status = 'active' AND w.deleted_at IS NULL
              AND ($2::uuid IS NULL OR CASE WHEN $3 THEN w.id > $2 ELSE w.id < $2 END)
            ORDER BY CASE WHEN $3 THEN w.id END, w.id DESC LIMIT $4"#,
        user.uuid(),
        after,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let role = row.role.parse().ok()?;
            Some((
                WorkspaceObject::from(Row {
                    id: row.id,
                    slug: row.slug,
                    name: row.name,
                    mode: row.mode,
                    timezone: row.timezone,
                    settings: row.settings,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                    deleted_at: None,
                }),
                role,
            ))
        })
        .collect())
}

/// A workspace a signed-in user creates.
#[derive(Debug, Clone, Copy)]
pub struct NewWorkspace<'a> {
    /// Its display name.
    pub name: &'a str,
    /// Its slug; derived from the name (with a random suffix) when absent.
    pub slug: Option<&'a str>,
    /// `live` or `test`.
    pub mode: KeyMode,
    /// Its IANA time zone.
    pub timezone: &'a str,
}

/// A random suffix of `n` lowercase letters and digits for a derived slug.
fn suffix(n: usize) -> Result<String, crypto::CryptoError> {
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    Ok(crypto::random_bytes(n)?
        .into_iter()
        .map(|byte| {
            char::from(
                ALPHABET
                    .get(usize::from(byte) % ALPHABET.len())
                    .copied()
                    .unwrap_or(b'x'),
            )
        })
        .collect())
}

/// Why a workspace could not be created.
#[derive(Debug, thiserror::Error)]
pub enum CreateWorkspaceError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Crypto(#[from] crypto::CryptoError),
}

/// Creates `new` with `user` as its owner, in a transaction begun as the user (see the module);
/// the transaction is left set to the new workspace.
///
/// # Errors
///
/// The slug is taken (a unique violation, `409`) or malformed (a check violation), or the
/// database or the random source failed.
pub async fn create_for(
    tx: &mut Tx,
    user: Id<User>,
    new: &NewWorkspace<'_>,
) -> Result<WorkspaceObject, CreateWorkspaceError> {
    let id = Id::<Workspace>::new();
    let tenant = WorkspaceId::trusted(id.uuid());
    db::set_workspace(tx, tenant).await?;
    let slug = match new.slug {
        Some(slug) => slug.to_owned(),
        None => format!("{}-{}", slug_from_name(new.name), suffix(6)?),
    };
    let row = sqlx::query_as!(
        Row,
        r#"INSERT INTO workspaces (id, slug, name, mode, timezone) VALUES ($1, $2, $3, $4, $5)
           RETURNING id AS "id: Id<Workspace>", slug, name, mode, timezone, settings,
                     created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp",
                     deleted_at AS "deleted_at: Timestamp""#,
        id.uuid(),
        slug,
        new.name,
        new.mode.workspace_mode(),
        new.timezone,
    )
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query!(
        "INSERT INTO memberships (workspace_id, user_id, role, source) VALUES ($1, $2, 'owner', 'creator')",
        id.uuid(),
        user.uuid()
    )
    .execute(&mut **tx)
    .await?;
    Ok(row.into())
}

/// What an update may change.
#[derive(Debug, Clone, Default)]
pub struct Changes<'a> {
    /// A new display name.
    pub name: Option<&'a str>,
    /// A new IANA time zone.
    pub timezone: Option<&'a str>,
    /// Settings to set: each top-level key given replaces the stored one.
    pub settings: Option<&'a serde_json::Map<String, serde_json::Value>>,
    /// A new mode.
    pub mode: Option<KeyMode>,
}

/// Locks `workspace`'s row and answers its current version, for an update's `If-Match`; `None`
/// for a workspace being deleted.
///
/// # Errors
///
/// The database failed.
pub async fn lock_version(tx: &mut Tx, workspace: WorkspaceId) -> Result<Option<i64>, sqlx::Error> {
    let updated_at = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM workspaces
            WHERE id = $1 AND deleted_at IS NULL FOR UPDATE"#,
        workspace.uuid()
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated_at.map(versioning::of))
}

/// Applies `changes` to `workspace` under `if_match`: the caller made sure the workspace exists.
///
/// # Errors
///
/// `412` for a stale `If-Match`, `404` for a workspace being deleted, or the database.
pub async fn update(
    tx: &mut Tx,
    workspace: WorkspaceId,
    changes: &Changes<'_>,
    if_match: &IfMatch,
) -> Result<WorkspaceObject, Problem> {
    let current = lock_version(tx, workspace)
        .await?
        .ok_or_else(|| Problem::not_found("workspace"))?;
    if_match.check(current)?;
    let settings = changes
        .settings
        .map(|settings| serde_json::Value::Object(settings.clone()));
    let row = sqlx::query_as!(
        Row,
        r#"UPDATE workspaces
              SET name = coalesce($2, name), timezone = coalesce($3, timezone),
                  settings = CASE WHEN $4::jsonb IS NULL THEN settings ELSE settings || $4 END,
                  mode = coalesce($5, mode)
            WHERE id = $1
           RETURNING id AS "id: Id<Workspace>", slug, name, mode, timezone, settings,
                     created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp",
                     deleted_at AS "deleted_at: Timestamp""#,
        workspace.uuid(),
        changes.name,
        changes.timezone,
        settings,
        changes.mode.map(KeyMode::workspace_mode),
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(row.into())
}

/// Requests `workspace`'s deletion by `requested_by` (a user's id): marks it deleted and queues
/// the erasure after the tombstone. `None` when it is being deleted already.
///
/// # Errors
///
/// The database failed.
pub async fn request_deletion(
    tx: &mut Tx,
    workspace: WorkspaceId,
    requested_by: &str,
) -> Result<Option<WorkspaceObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"UPDATE workspaces SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL
           RETURNING id AS "id: Id<Workspace>", slug, name, mode, timezone, settings,
                     created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp",
                     deleted_at AS "deleted_at: Timestamp""#,
        workspace.uuid()
    )
    .fetch_optional(&mut **tx)
    .await?;
    if row.is_some() {
        sqlx::query!(
            "INSERT INTO workspace_deletions (workspace_id, requested_by) VALUES ($1, $2)",
            workspace.uuid(),
            requested_by
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(row.map(WorkspaceObject::from))
}

/// What [`create_with_owner`] made.
#[derive(Debug, Serialize)]
pub struct Created {
    pub workspace: Id<Workspace>,
    pub owner: Id<User>,
    pub api_key: Id<ApiKey>,
    /// Shown once.
    pub api_key_secret: String,
}

/// Creates a workspace, its owner (an existing user with that address, or a new one) and a
/// first API key with every scope. Runs as `norbelys_system` in one transaction.
///
/// # Errors
///
/// The slug is taken or malformed, or the database refused a row.
pub async fn create_with_owner(
    tx: &mut Tx,
    slug: &str,
    name: &str,
    owner: &EmailAddress,
    mode: KeyMode,
) -> Result<Created, api_keys::CreateError> {
    let workspace = sqlx::query_scalar!(
        r#"INSERT INTO workspaces (slug, name, mode) VALUES ($1, $2, $3) RETURNING id AS "id: Id<Workspace>""#,
        slug,
        name,
        mode.workspace_mode()
    )
    .fetch_one(&mut **tx)
    .await?;
    let user = sqlx::query_scalar!(
        r#"INSERT INTO users (email, email_verified_at) VALUES ($1, now())
           ON CONFLICT (email_key) DO UPDATE SET updated_at = now()
           RETURNING id AS "id: Id<User>""#,
        owner.as_str()
    )
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query!(
        "INSERT INTO memberships (workspace_id, user_id, role, source) VALUES ($1, $2, 'owner', 'creator')",
        workspace.uuid(),
        user.uuid()
    )
    .execute(&mut **tx)
    .await?;
    let tenant = WorkspaceId::trusted(workspace.uuid());
    let (key, secret) = api_keys::create(
        tx,
        tenant,
        user,
        "Created by the operator",
        ScopeSet::all(),
        mode,
        None,
    )
    .await?;
    audit::record(
        tx,
        tenant,
        AuditActor::Admin,
        Action::ApiKeyCreated,
        Some(key.to_string()),
        serde_json::json!({}),
        None,
    )
    .await?;
    Ok(Created {
        workspace,
        owner: user,
        api_key: key,
        api_key_secret: secret.secret,
    })
}

/// Counts matching rows without materializing their data, bounded to `cap + 1`.
///
/// # Errors
///
/// The database failed.
pub async fn count_of_user(tx: &mut Tx, user: Id<User>, cap: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM (SELECT 1 FROM memberships m JOIN workspaces w ON w.id = m.workspace_id WHERE m.user_id = $1 AND m.status = 'active' AND w.deleted_at IS NULL LIMIT $2) counted").bind(user.uuid()).bind(cap.saturating_add(1)).fetch_one(&mut **tx).await
}
