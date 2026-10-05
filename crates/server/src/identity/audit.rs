//! The audit log: who changed access to a workspace, when, and from where.
//!
//! Every change to a workspace's access writes one row of `audit_log` in the same transaction
//! as the change, so a change without its entry, or an entry without its change, cannot exist:
//! memberships (joined, role changed, suspended, reactivated, removed, ownership transferred),
//! invitations (sent, revoked, accepted), API keys (created, changed, revoked, and revoked with
//! their creator's membership), SSO connections (created, changed, deleted, verified), OAuth
//! grants (revoked), the workspace itself (settings or mode changed, deletion requested), and the
//! operator's sessions in it (a break-glass for an owner, an impersonation of a member) and an
//! operator's resync of a connection's mailbox.
//! Decisions about mail are kept here too, because they override evidence on a person's word:
//! the release of a message's recipient holds, with the evidence the person gave, and the
//! lifting of a manual suppression, with its address and reason.
//! A row names its actor (`user`, `api_key`, `oauth` or `system`, and the id), the action, the
//! target's id, details (the request id always, and the values that changed), and the keyed
//! hash of the client address. Rows are listed newest first for 180 days; older rows are not
//! shown and are removed by retention.
//!
//! Actions are `<object>.<verb>` in the past tense, a closed vocabulary ([`Action`]), so a reader
//! can filter on them.
//!
//! A person's own account belongs to no workspace, so it has a log of its own (`user_audit_log`,
//! [`record_user`]): their sign-ins, sign-outs and revoked sessions, and an operator's suspension
//! or reactivation of them, with the same actors, details and address hash. The person reads only
//! their own rows (row security on the signed-in user); retention removes them after the same
//! 180 days.

use serde::Serialize;
use uuid::Uuid;

use super::authority::Actor;
use crate::db::Tx;
use crate::domain::ids::{ApiKey, AuditEntry, Grant, Id, User, WorkspaceId};
use crate::domain::time::Timestamp;
use crate::http::context;

/// How long entries are shown.
pub const RETENTION_DAYS: i32 = 180;

/// Who acted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditActor {
    /// A person, through a session or a workspace token.
    User(Id<User>),
    /// An API key.
    ApiKey(Id<ApiKey>),
    /// An application acting through an OAuth grant.
    OAuth(Id<Grant>),
    /// The platform itself (an operator command, a background check).
    System,
    /// An operator creating an API key, recorded under the established `system`/`admin` actor.
    Admin,
}

impl AuditActor {
    fn kind(self) -> &'static str {
        match self {
            Self::User(_) => "user",
            Self::ApiKey(_) => "api_key",
            Self::OAuth(_) => "oauth",
            Self::System | Self::Admin => "system",
        }
    }

    fn id(self) -> String {
        match self {
            Self::User(user) => user.to_string(),
            Self::ApiKey(key) => key.to_string(),
            Self::OAuth(grant) => grant.to_string(),
            Self::System => "system".to_owned(),
            Self::Admin => "admin".to_owned(),
        }
    }
}

impl From<Actor> for AuditActor {
    fn from(actor: Actor) -> Self {
        match actor {
            Actor::User { user, .. } => Self::User(user),
            Actor::ApiKey { key, .. } => Self::ApiKey(key),
            Actor::OAuth { grant, .. } => Self::OAuth(grant),
        }
    }
}

/// What happened (see the module).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter, strum::IntoStaticStr, strum::EnumString,
)]
pub enum Action {
    /// A person became a member (an accepted invitation, single sign-on).
    #[strum(serialize = "member.joined")]
    MemberJoined,
    /// A member's role changed.
    #[strum(serialize = "member.role_changed")]
    MemberRoleChanged,
    /// A member was suspended; their keys were revoked.
    #[strum(serialize = "member.suspended")]
    MemberSuspended,
    /// A suspended member may act again.
    #[strum(serialize = "member.reactivated")]
    MemberReactivated,
    /// A member was removed; their keys were revoked.
    #[strum(serialize = "member.removed")]
    MemberRemoved,
    /// A member was made an owner.
    #[strum(serialize = "ownership.transferred")]
    OwnershipTransferred,
    /// An invitation was sent, or sent again and extended.
    #[strum(serialize = "invitation.sent")]
    InvitationSent,
    /// A pending invitation was revoked.
    #[strum(serialize = "invitation.revoked")]
    InvitationRevoked,
    /// An invitation was accepted.
    #[strum(serialize = "invitation.accepted")]
    InvitationAccepted,
    /// An API key was created.
    #[strum(serialize = "api_key.created")]
    ApiKeyCreated,
    /// An API key's name changed.
    #[strum(serialize = "api_key.updated")]
    ApiKeyUpdated,
    /// An API key was revoked.
    #[strum(serialize = "api_key.revoked")]
    ApiKeyRevoked,
    /// An SSO connection was created.
    #[strum(serialize = "sso_connection.created")]
    SsoConnectionCreated,
    /// An SSO connection changed; its policy version moved.
    #[strum(serialize = "sso_connection.updated")]
    SsoConnectionUpdated,
    /// An SSO connection was deleted.
    #[strum(serialize = "sso_connection.deleted")]
    SsoConnectionDeleted,
    /// An SSO connection's discovery and domains were checked on request.
    #[strum(serialize = "sso_connection.verified")]
    SsoConnectionVerified,
    /// An OAuth grant was revoked.
    #[strum(serialize = "grant.revoked")]
    GrantRevoked,
    /// The workspace was created.
    #[strum(serialize = "workspace.created")]
    WorkspaceCreated,
    /// The workspace's name, time zone, settings or mode changed.
    #[strum(serialize = "workspace.updated")]
    WorkspaceUpdated,
    /// Deletion of the workspace was requested.
    #[strum(serialize = "workspace.deletion_requested")]
    WorkspaceDeletionRequested,
    /// A person released the holds of a message's recipients, with their evidence.
    #[strum(serialize = "message.holds_released")]
    MessageHoldsReleased,
    /// A person lifted a manual suppression; its address and reason are kept as details.
    #[strum(serialize = "suppression.deleted")]
    SuppressionDeleted,
    /// An operator opened a break-glass session for an owner locked out by single sign-on (the
    /// reason, the session, its end).
    #[strum(serialize = "break_glass.started")]
    BreakGlassStarted,
    /// An operator opened a session as a member, for support (the reason, the session, its end).
    #[strum(serialize = "impersonation.started")]
    ImpersonationStarted,
    /// An operator read a connection's mailbox again from shortly before its last poll (its
    /// receive bindings' cursors forgotten).
    #[strum(serialize = "connection.resynced")]
    ConnectionResynced,
    /// A person signed in: a session was created (its method, and the sessions the cap ended). In
    /// the person's own log.
    #[strum(serialize = "session.created")]
    SessionCreated,
    /// A person signed a browser out. In the person's own log.
    #[strum(serialize = "session.ended")]
    SessionEnded,
    /// A session was revoked: by the person from another device, or with their suspension. In the
    /// person's own log.
    #[strum(serialize = "session.revoked")]
    SessionRevoked,
    /// An operator suspended a person: no sign-in, every session revoked. In the person's own log.
    #[strum(serialize = "user.suspended")]
    UserSuspended,
    /// An operator let a suspended person sign in again. In the person's own log.
    #[strum(serialize = "user.reactivated")]
    UserReactivated,
}

impl Action {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Records one entry in `workspace`, inside the change's transaction. The request id of the
/// current request is added to `details` (an object).
///
/// # Errors
///
/// The database refused the row.
pub async fn record(
    tx: &mut Tx,
    workspace: WorkspaceId,
    actor: AuditActor,
    action: Action,
    target: Option<String>,
    details: serde_json::Value,
    ip_hash: Option<&[u8]>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO audit_log (workspace_id, actor_kind, actor_id, action, target, details, ip_hash)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        workspace.uuid(),
        actor.kind(),
        actor.id(),
        action.as_str(),
        target,
        with_request(details),
        ip_hash,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Records one entry in `user`'s own log (`user_audit_log`), inside the change's transaction:
/// what happened to their account outside any workspace (a sign-in, a sign-out, a revoked
/// session, an operator's suspension). The request id of the current request is added to
/// `details` (an object), as for a workspace's entries.
///
/// # Errors
///
/// The database refused the row.
pub async fn record_user(
    tx: &mut Tx,
    user: Id<User>,
    actor: AuditActor,
    action: Action,
    target: Option<String>,
    details: serde_json::Value,
    ip_hash: Option<&[u8]>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO user_audit_log (user_id, actor_kind, actor_id, action, target, details, ip_hash)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        user.uuid(),
        actor.kind(),
        actor.id(),
        action.as_str(),
        target,
        with_request(details),
        ip_hash,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// `details` as an object, with the current request's id when there is one.
fn with_request(details: serde_json::Value) -> serde_json::Value {
    let mut details = match details {
        serde_json::Value::Object(map) => map,
        serde_json::Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".to_owned(), other);
            map
        }
    };
    if let Some(request) = context::current() {
        details.insert(
            "request_id".to_owned(),
            serde_json::Value::String(request.request_id),
        );
    }
    serde_json::Value::Object(details)
}

/// Who acted, as an entry shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ActorObject {
    /// `user`, `api_key`, `oauth` or `system`; new values may be added.
    #[schema(extensions(("x-open-enum" = json!(true))))]
    pub kind: String,
    /// The user's, key's or grant's id, or `system`.
    pub id: String,
}

/// An entry of the audit log.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct AuditObject {
    pub id: Id<AuditEntry>,
    /// Who acted.
    pub actor: ActorObject,
    /// What happened, `<object>.<verb>` (`member.role_changed`); new values may be added.
    #[schema(extensions(("x-open-enum" = json!(true))))]
    pub action: String,
    /// The id of what it happened to.
    pub target: Option<String>,
    /// The request id and the values that changed.
    pub details: serde_json::Value,
    pub created_at: Timestamp,
}

/// One page of entries, newest first (or oldest first), after `after`, optionally of one
/// `action`; only the last 180 days.
///
/// # Errors
///
/// The database failed.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    action: Option<&str>,
    after: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<AuditObject>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id AS "id: Id<AuditEntry>", actor_kind, actor_id, action, target, details,
                  created_at AS "created_at: Timestamp"
             FROM audit_log
            WHERE workspace_id = $1
              AND created_at > now() - make_interval(days => $2)
              AND ($3::text IS NULL OR action = $3)
              AND ($4::uuid IS NULL OR CASE WHEN $5 THEN id > $4 ELSE id < $4 END)
            ORDER BY CASE WHEN $5 THEN id END ASC, CASE WHEN NOT $5 THEN id END DESC
            LIMIT $6"#,
        workspace.uuid(),
        RETENTION_DAYS,
        action,
        after,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| AuditObject {
            id: row.id,
            actor: ActorObject {
                kind: row.actor_kind,
                id: row.actor_id,
            },
            action: row.action,
            target: row.target,
            details: row.details,
            created_at: row.created_at,
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
    action: Option<&str>,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM (SELECT 1 FROM audit_log WHERE workspace_id = $1 AND created_at > now() - make_interval(days => $2) AND ($3::text IS NULL OR action = $3) LIMIT $4) counted").bind(workspace.uuid()).bind(RETENTION_DAYS).bind(action).bind(cap.saturating_add(1)).fetch_one(&mut **tx).await
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use strum::IntoEnumIterator as _;

    use super::*;

    /// Every action is an `<object>.<verb>` spelling that parses back to itself, so the
    /// vocabulary readers filter on stays closed and unambiguous.
    #[test]
    fn actions_are_a_closed_dotted_vocabulary() {
        for action in Action::iter() {
            let text = action.as_str();
            assert_eq!(text.matches('.').count(), 1, "{text}");
            assert_eq!(Action::from_str(text), Ok(action));
        }
    }

    /// Operator-created keys keep their established `system`/`admin` actor, while background
    /// work keeps `system`/`system`, so routing both through the audit writer changes no attribution.
    #[test]
    fn operator_and_background_actors_keep_their_attribution() {
        assert_eq!(AuditActor::Admin.kind(), "system");
        assert_eq!(AuditActor::Admin.id(), "admin");
        assert_eq!(AuditActor::System.kind(), "system");
        assert_eq!(AuditActor::System.id(), "system");
    }
}
