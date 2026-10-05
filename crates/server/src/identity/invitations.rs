//! Invitations: pending offers of a membership, sent to an email address.
//!
//! An administrator invites one or more addresses, each with a role (`admin`, `member` or
//! `viewer`; ownership is given to a member afterwards). An invitation expires 10 days after it
//! is sent. Inviting an address that already has a pending invitation sends it again: a new
//! token, the new role, ten more days, in one statement whose arbiter is the
//! one-pending-invitation-per-address index, so two administrators inviting the same address at
//! once leave one invitation. An address that is already an active member is not invited.
//!
//! The mail carries a token (32 random bytes, stored only as its keyed hash) in a link to the
//! dashboard. Accepting it (`POST /v1/me/memberships { invitation_token }`) resolves the token
//! with `invitation_by_token()`, the lookup that runs before any workspace is known, and requires
//! a signed-in user whose verified email address is the invited one. Acceptance then creates the
//! membership, or revives a removed or suspended one with the invited role, and marks the
//! invitation accepted, in one transaction; a second acceptance of the same invitation, however
//! close in time, finds it accepted and changes nothing.
//!
//! Lock order: the workspace row (through `memberships`), then the invitation row, then the
//! membership row.

use std::time::Duration;

use serde::Serialize;
use uuid::Uuid;

use super::memberships::{self, Admitted};
use crate::crypto::{self, CryptoError, Keys};
use crate::db::Tx;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, Invitation, Membership, User, WorkspaceId};
use crate::domain::scope::MembershipRole;
use crate::domain::time::Timestamp;

/// How long an invitation stays valid after it is sent.
pub const LIFETIME: Duration = Duration::from_secs(10 * 24 * 3600);
/// The dashboard page an invitation link opens.
const LINK_PATH: &str = "/invitations/accept";

/// An invitation's state, derived from its row.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum InvitationStatus {
    /// Waiting for its person.
    Pending,
    /// Accepted.
    Accepted,
    /// Revoked by an administrator.
    Revoked,
    /// Not accepted in time; sending it again extends it.
    Expired,
}

/// An invitation as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct InvitationObject {
    pub id: Id<Invitation>,
    /// The invited address, as written.
    pub email: String,
    /// The role it offers.
    pub role: MembershipRole,
    /// `pending`, `accepted`, `revoked` or `expired`.
    pub status: InvitationStatus,
    /// Who sent it last.
    pub invited_by: Id<User>,
    pub expires_at: Timestamp,
    pub accepted_at: Option<Timestamp>,
    pub accepted_by: Option<Id<User>>,
    pub revoked_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

struct Row {
    id: Id<Invitation>,
    email: String,
    role: String,
    invited_by: Id<User>,
    expires_at: Timestamp,
    accepted_at: Option<Timestamp>,
    accepted_by: Option<Id<User>>,
    revoked_at: Option<Timestamp>,
    created_at: Timestamp,
}

impl Row {
    fn object(self, now: Timestamp) -> Option<InvitationObject> {
        let status = if self.accepted_at.is_some() {
            InvitationStatus::Accepted
        } else if self.revoked_at.is_some() {
            InvitationStatus::Revoked
        } else if self.expires_at <= now {
            InvitationStatus::Expired
        } else {
            InvitationStatus::Pending
        };
        Some(InvitationObject {
            id: self.id,
            email: self.email,
            role: self.role.parse().ok()?,
            status,
            invited_by: self.invited_by,
            expires_at: self.expires_at,
            accepted_at: self.accepted_at,
            accepted_by: self.accepted_by,
            revoked_at: self.revoked_at,
            created_at: self.created_at,
        })
    }
}

/// Why an invitation could not be sent or accepted.
#[derive(Debug, thiserror::Error)]
pub enum InvitationError {
    /// The address is already an active member.
    #[error("already a member")]
    AlreadyMember,
    /// No invitation has this token.
    #[error("no such invitation")]
    NotFound,
    /// The invitation was accepted, revoked or has expired.
    #[error("the invitation is no longer pending")]
    NotPending,
    /// The signed-in user's verified address is not the invited one.
    #[error("the invitation is for another address")]
    OtherAddress,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

/// A sent invitation and the token its mail carries.
#[derive(Debug, Clone)]
pub struct Sent {
    /// The invitation.
    pub invitation: InvitationObject,
    /// The token, for the link; never stored.
    pub token: String,
}

/// The link an invitation mail carries: the dashboard's acceptance page with the token in the
/// fragment, which browsers never send to a server or a referrer.
#[must_use]
pub fn link(dashboard: &url::Url, token: &str) -> String {
    let mut url = dashboard.clone();
    url.set_path(LINK_PATH);
    url.set_fragment(Some(&format!("token={token}")));
    url.to_string()
}

/// Sends (or sends again, extending it) an invitation of `email` to `workspace` with `role`, by
/// `invited_by` (see the module).
///
/// # Errors
///
/// [`InvitationError::AlreadyMember`], or the random source or the database failed.
pub async fn send(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    email: &EmailAddress,
    role: MembershipRole,
    invited_by: Id<User>,
) -> Result<Sent, InvitationError> {
    let member = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM memberships m JOIN users u ON u.id = m.user_id
                           WHERE m.workspace_id = $1 AND u.email_key = ascii_lower($2) AND m.status = 'active') AS "member!""#,
        workspace.uuid(),
        email.as_str(),
    )
    .fetch_one(&mut **tx)
    .await?;
    if member {
        return Err(InvitationError::AlreadyMember);
    }
    let token = crypto::random_token(32)?;
    let row = sqlx::query_as!(
        Row,
        r#"INSERT INTO invitations (workspace_id, email, role, token_hash, invited_by, expires_at)
           VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6))
           ON CONFLICT (workspace_id, email_key) WHERE accepted_at IS NULL AND revoked_at IS NULL
           DO UPDATE SET token_hash = EXCLUDED.token_hash, role = EXCLUDED.role,
                         invited_by = EXCLUDED.invited_by, expires_at = EXCLUDED.expires_at
           RETURNING id AS "id: Id<Invitation>", email, role, invited_by AS "invited_by: Id<User>",
                     expires_at AS "expires_at: Timestamp", accepted_at AS "accepted_at: Timestamp",
                     accepted_by AS "accepted_by: Id<User>", revoked_at AS "revoked_at: Timestamp",
                     created_at AS "created_at: Timestamp""#,
        workspace.uuid(),
        email.as_str(),
        role.as_str(),
        keys.hash_token(&token),
        invited_by.uuid(),
        LIFETIME.as_secs_f64(),
    )
    .fetch_one(&mut **tx)
    .await?;
    let invitation = row
        .object(crate::process::now())
        .ok_or(InvitationError::NotFound)?;
    Ok(Sent { invitation, token })
}

/// One page of `workspace`'s invitations by id, optionally of one `status`.
///
/// # Errors
///
/// The database failed.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    status: Option<InvitationStatus>,
    after: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<InvitationObject>, sqlx::Error> {
    let status: Option<&'static str> = status.map(Into::into);
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id AS "id: Id<Invitation>", email, role, invited_by AS "invited_by: Id<User>",
                  expires_at AS "expires_at: Timestamp", accepted_at AS "accepted_at: Timestamp",
                  accepted_by AS "accepted_by: Id<User>", revoked_at AS "revoked_at: Timestamp",
                  created_at AS "created_at: Timestamp"
             FROM invitations
            WHERE workspace_id = $1
              AND ($2::text IS NULL OR $2 = CASE WHEN accepted_at IS NOT NULL THEN 'accepted'
                                                 WHEN revoked_at IS NOT NULL THEN 'revoked'
                                                 WHEN expires_at <= now() THEN 'expired'
                                                 ELSE 'pending' END)
              AND ($3::uuid IS NULL OR CASE WHEN $4 THEN id > $3 ELSE id < $3 END)
            ORDER BY CASE WHEN $4 THEN id END, id DESC LIMIT $5"#,
        workspace.uuid(),
        status,
        after,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    let now = crate::process::now();
    Ok(rows.into_iter().filter_map(|row| row.object(now)).collect())
}

/// Revokes pending invitation `id`; true when one was revoked.
///
/// # Errors
///
/// The database failed.
pub async fn revoke(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Invitation>,
) -> Result<bool, sqlx::Error> {
    let revoked = sqlx::query!(
        "UPDATE invitations SET revoked_at = now()
          WHERE workspace_id = $1 AND id = $2 AND accepted_at IS NULL AND revoked_at IS NULL",
        workspace.uuid(),
        id.uuid(),
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(revoked > 0)
}

/// Whether invitation `id` exists in `workspace`, whatever its state.
///
/// # Errors
///
/// The database failed.
pub async fn exists(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Invitation>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM invitations WHERE workspace_id = $1 AND id = $2) AS "exists!""#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_one(&mut **tx)
    .await
}

/// The invitation a token names, found before any workspace is known.
#[derive(Debug, Clone, Copy)]
pub struct Found {
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// The invitation.
    pub id: Id<Invitation>,
}

/// Resolves `token` with the pre-workspace lookup.
///
/// # Errors
///
/// The database failed.
pub async fn find(tx: &mut Tx, keys: &Keys, token: &str) -> Result<Option<Found>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT workspace_id AS "workspace_id!", id AS "id!: Id<Invitation>"
             FROM invitation_by_token($1)"#,
        keys.hash_token(token)
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| Found {
        workspace: WorkspaceId::trusted(row.workspace_id),
        id: row.id,
    }))
}

/// What an acceptance did.
#[derive(Debug, Clone, Copy)]
pub struct Accepted {
    /// The membership.
    pub membership: Id<Membership>,
    /// Whether it is new or revived (false when the person was a member already).
    pub joined: bool,
}

/// Accepts invitation `found` for `user`, whose verified address key is `email_key`, inside a
/// transaction set to the invitation's workspace (see the module).
///
/// # Errors
///
/// [`InvitationError`]: not pending, another address; or the database.
pub async fn accept(
    tx: &mut Tx,
    found: Found,
    user: Id<User>,
    email_key: Option<&str>,
) -> Result<Accepted, InvitationError> {
    memberships::lock_workspace(tx, found.workspace).await?;
    let row = sqlx::query!(
        r#"SELECT email_key, role, accepted_at IS NULL AND revoked_at IS NULL AND expires_at > now() AS "pending!"
             FROM invitations WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        found.workspace.uuid(),
        found.id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(InvitationError::NotFound)?;
    if !row.pending {
        return Err(InvitationError::NotPending);
    }
    if email_key != Some(row.email_key.as_str()) {
        return Err(InvitationError::OtherAddress);
    }
    let role: MembershipRole = row.role.parse().map_err(|_| InvitationError::NotFound)?;
    let admitted = memberships::admit(tx, found.workspace, user, role, "invitation", true)
        .await?
        .ok_or(InvitationError::NotPending)?;
    sqlx::query!(
        "UPDATE invitations SET accepted_at = now(), accepted_by = $3 WHERE workspace_id = $1 AND id = $2",
        found.workspace.uuid(),
        found.id.uuid(),
        user.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    Ok(match admitted {
        Admitted::Joined(membership) | Admitted::Revived(membership) => Accepted {
            membership,
            joined: true,
        },
        Admitted::AlreadyMember(membership) => Accepted {
            membership,
            joined: false,
        },
    })
}

/// Counts matching rows without materializing their data, bounded to `cap + 1`.
///
/// # Errors
///
/// The database failed.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    status: Option<InvitationStatus>,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM (SELECT 1 FROM invitations WHERE workspace_id = $1 AND ($2::text IS NULL OR $2 = CASE WHEN accepted_at IS NOT NULL THEN 'accepted' WHEN revoked_at IS NOT NULL THEN 'revoked' WHEN expires_at <= now() THEN 'expired' ELSE 'pending' END) LIMIT $3) counted").bind(workspace.uuid()).bind(status.map(<&'static str>::from)).bind(cap.saturating_add(1)).fetch_one(&mut **tx).await
}
