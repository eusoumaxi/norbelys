//! Members: a user's standing in a workspace, and the changes that keep a workspace governable.
//!
//! A membership has a role (`owner`, `admin`, `member`, `viewer`) and a status (`active`,
//! `suspended`, `removed`). A removed or suspended membership is a tombstone: the row stays, so
//! the person's history keeps its author and single sign-on never readmits them by itself (only
//! an accepted invitation revives a removed membership). Making a member `owner` is how ownership
//! is transferred; several owners may coexist, and the previous owner can then step down.
//!
//! # Rules
//!
//! The decisions are `domain::identity::membership_change`: only an owner promotes to owner or
//! changes an owner, and the last active owner can never be demoted, suspended or removed.
//! Suspending or removing a member revokes every API key they created, in the same transaction
//! (a key is delegated by its member and dies with the membership); a reactivated member's keys
//! stay revoked. The caller drops the revoked keys and the member's cached standing from the
//! authority (`Authority::forget`, `Authority::forget_member`).
//!
//! # Lock order
//!
//! The workspace row, then membership rows, then API key rows. Counting the active owners under
//! the workspace row's lock is what makes the last-owner rule exact: two owners demoting each
//! other at once are serialised, and the second sees one owner left. Every path that changes
//! roles or statuses (member updates and removals, invitation acceptance, single sign-on's
//! just-in-time membership) takes the workspace row first.

use serde::Serialize;
use uuid::Uuid;

use crate::db::Tx;
use crate::domain::identity::{
    self, ChangeFacts, ChangeRefusal, MembershipChange, MembershipStatus,
};
use crate::domain::ids::{Id, Membership, User, WorkspaceId};
use crate::domain::scope::MembershipRole;
use crate::domain::time::Timestamp;
use crate::http::versioning::{self, IfMatch};
use crate::problem::Problem;

/// A user as a member shows them.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct MemberUser {
    pub id: Id<User>,
    pub email: String,
    pub name: Option<String>,
}

/// A member of a workspace.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct MemberObject {
    pub id: Id<Membership>,
    /// The person.
    pub user: MemberUser,
    /// `owner`, `admin`, `member` or `viewer`.
    pub role: MembershipRole,
    /// `active`, `suspended` or `removed`.
    pub status: MembershipStatus,
    /// `creator`, `invitation` or `sso_jit`; new values may be added.
    #[schema(extensions(("x-open-enum" = json!(true))))]
    pub source: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The version `If-Match` names.
    pub version: i64,
}

/// Why a membership could not change.
#[derive(Debug, thiserror::Error)]
pub enum MemberError {
    /// No such member in the workspace.
    #[error("no such member")]
    NotFound,
    /// The rules refuse the change.
    #[error("the change is refused: {0:?}")]
    Refused(ChangeRefusal),
    /// `If-Match` names another version.
    #[error("the member changed since the version named")]
    Stale(Problem),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl From<MemberError> for Problem {
    fn from(error: MemberError) -> Self {
        match error {
            MemberError::NotFound => Problem::not_found("member"),
            MemberError::Refused(ChangeRefusal::OwnersOnly) => {
                Problem::forbidden("Only an owner may make someone an owner or change an owner.")
            }
            MemberError::Refused(ChangeRefusal::LastOwner) => Problem::invalid_state(
                "The workspace would be left without an active owner; make someone else an owner first.",
            ),
            MemberError::Refused(ChangeRefusal::Removed) => {
                Problem::invalid_state("A removed member is not edited; invite the person again.")
            }
            MemberError::Stale(problem) => problem,
            MemberError::Db(error) => error.into(),
        }
    }
}

struct Row {
    id: Id<Membership>,
    user_id: Id<User>,
    email: String,
    name: Option<String>,
    role: String,
    status: String,
    source: String,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl Row {
    fn object(self) -> Option<MemberObject> {
        Some(MemberObject {
            id: self.id,
            user: MemberUser {
                id: self.user_id,
                email: self.email,
                name: self.name,
            },
            role: self.role.parse().ok()?,
            status: self.status.parse().ok()?,
            source: self.source,
            created_at: self.created_at,
            version: versioning::of(self.updated_at),
            updated_at: self.updated_at,
        })
    }
}

/// One page of `workspace`'s members by id, optionally of one `status`.
///
/// # Errors
///
/// The database failed.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    status: Option<MembershipStatus>,
    after: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<MemberObject>, sqlx::Error> {
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT m.id AS "id: Id<Membership>", m.user_id AS "user_id: Id<User>", u.email, u.name,
                  m.role, m.status, m.source, m.created_at AS "created_at: Timestamp",
                  m.updated_at AS "updated_at: Timestamp"
             FROM memberships m JOIN users u ON u.id = m.user_id
            WHERE m.workspace_id = $1 AND ($2::text IS NULL OR m.status = $2)
              AND ($3::uuid IS NULL OR CASE WHEN $4 THEN m.id > $3 ELSE m.id < $3 END)
            ORDER BY CASE WHEN $4 THEN m.id END, m.id DESC LIMIT $5"#,
        workspace.uuid(),
        status.map(MembershipStatus::as_str),
        after,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows.into_iter().filter_map(Row::object).collect())
}

/// Member `id` of `workspace`.
///
/// # Errors
///
/// The database failed.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Membership>,
) -> Result<Option<MemberObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"SELECT m.id AS "id: Id<Membership>", m.user_id AS "user_id: Id<User>", u.email, u.name,
                  m.role, m.status, m.source, m.created_at AS "created_at: Timestamp",
                  m.updated_at AS "updated_at: Timestamp"
             FROM memberships m JOIN users u ON u.id = m.user_id
            WHERE m.workspace_id = $1 AND m.id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.and_then(Row::object))
}

/// Locks the workspace row: the head of this module's lock order.
///
/// # Errors
///
/// The database failed.
pub async fn lock_workspace(tx: &mut Tx, workspace: WorkspaceId) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "SELECT 1 AS one FROM workspaces WHERE id = $1 FOR UPDATE",
        workspace.uuid()
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(())
}

/// What a change did.
#[derive(Debug, Clone)]
pub struct Changed {
    /// The member after the change.
    pub member: MemberObject,
    /// The hashes of the API keys the change revoked, for the authority to forget.
    pub revoked_keys: Vec<Vec<u8>>,
}

/// A change to apply.
#[derive(Debug, Clone, Copy, Default)]
pub struct Change {
    /// A new role.
    pub role: Option<MembershipRole>,
    /// A new status: `active` or `suspended` (removal is [`remove`]).
    pub status: Option<MembershipStatus>,
}

/// Applies `change` to member `id`, made by a member with `actor_role`, under `if_match` (see the
/// module for the rules, the key revocations and the lock order).
///
/// # Errors
///
/// [`MemberError`].
pub async fn change(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Membership>,
    actor_role: MembershipRole,
    change: Change,
    if_match: &IfMatch,
) -> Result<Changed, MemberError> {
    lock_workspace(tx, workspace).await?;
    let current = sqlx::query!(
        r#"SELECT role, status, user_id AS "user_id: Id<User>", updated_at AS "updated_at: Timestamp"
             FROM memberships WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(MemberError::NotFound)?;
    if_match
        .check(versioning::of(current.updated_at))
        .map_err(MemberError::Stale)?;
    let mut role: MembershipRole = current.role.parse().map_err(|_| MemberError::NotFound)?;
    let mut status: MembershipStatus = current.status.parse().map_err(|_| MemberError::NotFound)?;
    let mut owners = active_owners(tx, workspace).await?;
    let mut steps = Vec::new();
    if let Some(new_role) = change.role.filter(|new_role| *new_role != role) {
        steps.push(MembershipChange::Role(new_role));
    }
    match change.status {
        Some(MembershipStatus::Suspended) if status == MembershipStatus::Active => {
            steps.push(MembershipChange::Suspend);
        }
        Some(MembershipStatus::Active) if status == MembershipStatus::Suspended => {
            steps.push(MembershipChange::Reactivate);
        }
        Some(MembershipStatus::Removed) => steps.push(MembershipChange::Remove),
        _ => {}
    }
    for step in steps {
        identity::membership_change(
            step,
            &ChangeFacts {
                actor_role,
                target_role: role,
                target_status: status,
                active_owners: owners,
            },
        )
        .map_err(MemberError::Refused)?;
        let was_active_owner = role == MembershipRole::Owner && status == MembershipStatus::Active;
        match step {
            MembershipChange::Role(new_role) => role = new_role,
            MembershipChange::Suspend => status = MembershipStatus::Suspended,
            MembershipChange::Reactivate => status = MembershipStatus::Active,
            MembershipChange::Remove => status = MembershipStatus::Removed,
        }
        let is_active_owner = role == MembershipRole::Owner && status == MembershipStatus::Active;
        owners += i64::from(is_active_owner) - i64::from(was_active_owner);
    }
    write(tx, workspace, id, role, status).await?;
    let revoked_keys = if status == MembershipStatus::Active {
        Vec::new()
    } else {
        revoke_keys(tx, workspace, current.user_id).await?
    };
    let member = read(tx, workspace, id)
        .await?
        .ok_or(MemberError::NotFound)?;
    Ok(Changed {
        member,
        revoked_keys,
    })
}

/// Removes member `id` (a tombstone), made by a member with `actor_role` (see the module).
///
/// # Errors
///
/// [`MemberError`].
pub async fn remove(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Membership>,
    actor_role: MembershipRole,
) -> Result<Changed, MemberError> {
    change(
        tx,
        workspace,
        id,
        actor_role,
        Change {
            role: None,
            status: Some(MembershipStatus::Removed),
        },
        &IfMatch::Absent,
    )
    .await
}

async fn active_owners(tx: &mut Tx, workspace: WorkspaceId) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM memberships
            WHERE workspace_id = $1 AND role = 'owner' AND status = 'active'"#,
        workspace.uuid()
    )
    .fetch_one(&mut **tx)
    .await
}

async fn write(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Membership>,
    role: MembershipRole,
    status: MembershipStatus,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE memberships
            SET role = $3, status = $4,
                status_changed_at = CASE WHEN status <> $4 THEN now() ELSE status_changed_at END
          WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
        role.as_str(),
        status.as_str(),
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Revokes every live API key `user` created in `workspace`; returns their hashes.
///
/// # Errors
///
/// The database failed.
pub async fn revoke_keys(
    tx: &mut Tx,
    workspace: WorkspaceId,
    user: Id<User>,
) -> Result<Vec<Vec<u8>>, sqlx::Error> {
    sqlx::query_scalar!(
        "UPDATE api_keys SET revoked_at = now()
          WHERE workspace_id = $1 AND created_by = $2 AND revoked_at IS NULL
         RETURNING secret_hash",
        workspace.uuid(),
        user.uuid(),
    )
    .fetch_all(&mut **tx)
    .await
}

/// What [`admit`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admitted {
    /// A new membership.
    Joined(Id<Membership>),
    /// A removed or suspended membership was made active again (an invitation only).
    Revived(Id<Membership>),
    /// The person was an active member already; nothing changed.
    AlreadyMember(Id<Membership>),
}

/// Admits `user` into `workspace` with `role`: a new membership, or, through an invitation
/// (`revive`), a removed or suspended one made active again with the invitation's role. An
/// active member keeps their role. Takes the workspace row's lock first (see the module).
///
/// # Errors
///
/// The database failed.
pub async fn admit(
    tx: &mut Tx,
    workspace: WorkspaceId,
    user: Id<User>,
    role: MembershipRole,
    source: &str,
    revive: bool,
) -> Result<Option<Admitted>, sqlx::Error> {
    lock_workspace(tx, workspace).await?;
    let existing = sqlx::query!(
        r#"SELECT id AS "id: Id<Membership>", status FROM memberships
            WHERE workspace_id = $1 AND user_id = $2 FOR UPDATE"#,
        workspace.uuid(),
        user.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    match existing {
        Some(row) if row.status == "active" => Ok(Some(Admitted::AlreadyMember(row.id))),
        Some(row) if revive => {
            sqlx::query!(
                "UPDATE memberships SET status = 'active', role = $3, source = $4, status_changed_at = now()
                  WHERE workspace_id = $1 AND id = $2",
                workspace.uuid(),
                row.id.uuid(),
                role.as_str(),
                source,
            )
            .execute(&mut **tx)
            .await?;
            Ok(Some(Admitted::Revived(row.id)))
        }
        Some(_) => Ok(None),
        None => {
            let id = sqlx::query_scalar!(
                r#"INSERT INTO memberships (workspace_id, user_id, role, source) VALUES ($1, $2, $3, $4)
                   RETURNING id AS "id: Id<Membership>""#,
                workspace.uuid(),
                user.uuid(),
                role.as_str(),
                source,
            )
            .fetch_one(&mut **tx)
            .await?;
            Ok(Some(Admitted::Joined(id)))
        }
    }
}

/// A membership as `GET /v1/me` lists it: the workspace and the user's standing in it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct MembershipObject {
    pub id: Id<Membership>,
    /// The workspace.
    pub workspace: WorkspaceSummary,
    pub role: MembershipRole,
    pub status: MembershipStatus,
    pub created_at: Timestamp,
}

/// A workspace as a membership names it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct WorkspaceSummary {
    pub id: Id<crate::domain::ids::Workspace>,
    pub slug: String,
    pub name: String,
    /// `live` or `test`.
    pub mode: String,
}

/// The signed-in user's active memberships in workspaces not being deleted, by workspace id,
/// read under the user policy (the transaction runs as the user, before any workspace).
///
/// # Errors
///
/// The database failed.
pub async fn of_user(
    tx: &mut Tx,
    user: Id<User>,
    after: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<MembershipObject>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT m.id AS "id: Id<Membership>", w.id AS "workspace_id: Id<crate::domain::ids::Workspace>",
                  w.slug, w.name, w.mode, m.role, m.status, m.created_at AS "created_at: Timestamp"
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
            Some(MembershipObject {
                id: row.id,
                workspace: WorkspaceSummary {
                    id: row.workspace_id,
                    slug: row.slug,
                    name: row.name,
                    mode: row.mode,
                },
                role: row.role.parse().ok()?,
                status: row.status.parse().ok()?,
                created_at: row.created_at,
            })
        })
        .collect())
}

/// Membership `id` of `workspace` as `GET /v1/me` shows it, read inside the workspace.
///
/// # Errors
///
/// The database failed.
pub async fn membership(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Membership>,
) -> Result<Option<MembershipObject>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT m.id AS "id: Id<Membership>", w.id AS "workspace_id: Id<crate::domain::ids::Workspace>",
                  w.slug, w.name, w.mode, m.role, m.status, m.created_at AS "created_at: Timestamp"
             FROM memberships m JOIN workspaces w ON w.id = m.workspace_id
            WHERE m.workspace_id = $1 AND m.id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.and_then(|row| {
        Some(MembershipObject {
            id: row.id,
            workspace: WorkspaceSummary {
                id: row.workspace_id,
                slug: row.slug,
                name: row.name,
                mode: row.mode,
            },
            role: row.role.parse().ok()?,
            status: row.status.parse().ok()?,
            created_at: row.created_at,
        })
    }))
}

/// A person the platform tells about something in a workspace by email (a connection's health,
/// a failing webhook endpoint, a break-glass session): an active member who is an active user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Told {
    /// The user.
    pub user: Id<User>,
    /// Their address, as stored.
    pub email: String,
    /// Their role in the workspace.
    pub role: MembershipRole,
}

impl Told {
    /// Whether they administer the workspace: an owner or an admin.
    #[must_use]
    pub fn admin(&self) -> bool {
        matches!(self.role, MembershipRole::Owner | MembershipRole::Admin)
    }
}

/// The workspace's active owners and admins, and the active members among `also` (the creators
/// of the connections a notice is about, say), each once, in address order: who is told about
/// the workspace by email. Suspended and removed members, and suspended users, are never told.
/// Runs in the caller's transaction, in `workspace` (or as the operator's login).
///
/// # Errors
///
/// The database failed.
pub async fn told(
    tx: &mut Tx,
    workspace: WorkspaceId,
    also: &[Uuid],
) -> Result<Vec<Told>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT u.id AS "user: Id<User>", u.email, m.role
             FROM memberships m
             JOIN users u ON u.id = m.user_id
            WHERE m.workspace_id = $1 AND m.status = 'active' AND u.status = 'active'
              AND (m.role IN ('owner', 'admin') OR m.user_id = ANY($2))
            ORDER BY u.email, u.id"#,
        workspace.uuid(),
        also,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Told {
                user: row.user,
                email: row.email,
                role: row.role.parse().ok()?,
            })
        })
        .collect())
}

/// Counts matching rows without materializing their data, bounded to `cap + 1`.
///
/// # Errors
///
/// The database failed.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    status: Option<MembershipStatus>,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM (SELECT 1 FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.workspace_id = $1 AND ($2::text IS NULL OR m.status = $2) LIMIT $3) counted").bind(workspace.uuid()).bind(status.map(MembershipStatus::as_str)).bind(cap.saturating_add(1)).fetch_one(&mut **tx).await
}
