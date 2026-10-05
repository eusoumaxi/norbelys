//! Owner recovery and the operator's sessions: the recovery codes a person registers ahead of
//! need, the break-glass session an operator opens for an owner locked out of a workspace by its
//! single sign-on, and the short session an operator opens as a person for support
//! (impersonation).
//!
//! # Recovery codes
//!
//! `POST /v1/me/recovery_codes` (the dashboard, a signed-in browser) registers ten codes and
//! shows them once: sixteen digits each, in groups of four, so a person can read one out over the
//! phone. Only the keyed MAC of a code's digits is stored, and registering again replaces the
//! whole set. A code is worth something only to an operator's break-glass, and only when it was
//! registered before the workspace's single sign-on enforcement began: the person set it up while
//! the identity provider still vouched for them, so whoever later controls only their mailbox
//! (an email code still signs them in) cannot register codes of their own and use them.
//!
//! # Break-glass
//!
//! An owner whose workspace enforces single sign-on cannot reach it when the identity provider
//! breaks (a deleted application, an expired secret). They sign in the way that still works (an
//! email code, a passkey), which reaches no enforcing workspace, and call support. The operator
//! verifies them, takes one of their recovery codes, and runs `norbelys-server admin owners
//! break-glass`, which in one transaction ([`break_glass`]):
//!
//! 1. finds the workspace and its enforcing connection (refused when it enforces nothing: its
//!    owners sign in as usual) and the owner's active membership;
//! 2. consumes the code, refused unless it is an unused code of the owner registered before the
//!    enforcement began (before the earliest enforcement of the workspace's enforcing
//!    connections);
//! 3. turns the owner's newest live session (or the one the operator names) into a break-glass
//!    session: method `break_glass`, standing in for that connection, authenticated now, ending
//!    within 24 hours; its cookie is unchanged, so the owner's browser carries on;
//! 4. records `break_glass.started` in the workspace's audit log, with the reason;
//! 5. emails the workspace's other active owners.
//!
//! The session then mints workspace tokens for that workspace alone, carrying `workspace:read`
//! and `workspace:manage` only (`domain::identity::token_scopes`): the owner repairs the single
//! sign-on settings and the members, and reaches no product data; a key made with such a token
//! is bounded by the same scopes, and no application can be granted access from the session.
//! The api reads a session's row on every cookie request, so minting works at once; the checks of
//! a workspace token may still see the old row for up to a minute.
//!
//! # Impersonation
//!
//! `norbelys-server admin users impersonate` opens a session as a person for support
//! ([`impersonate`]): method `impersonation`, 10 minutes, recorded as `impersonation.started`
//! with the reason in every workspace where they are an active member, and listed among the
//! person's own sessions with the reason. The command prints its cookie and CSRF token once, for
//! the operator's browser. An impersonation reaches no workspace that enforces single sign-on.
//!
//! # Lock order
//!
//! A break-glass locks the recovery code, then the session; it writes the audit row and the
//! notices after them.

use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use serde_json::json;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::audit::{self, Action, AuditActor};
use super::memberships;
use super::sessions::{self, NewSession, SessionError, SignedIn};
use crate::crypto::{self, CryptoError, Keys};
use crate::db::Tx;
use crate::delivery::accept::{self, Transactional};
use crate::domain::email::EmailAddress;
use crate::domain::identity::{AuthMethod, Lasting, may_make, session_lifetime};
use crate::domain::ids::{Id, Session, User, Workspace, WorkspaceId};
use crate::domain::scope::MembershipRole;
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::Json;
use crate::problem::{ApiResult, Problem};

/// How many codes one registration makes.
pub const CODES: usize = 10;
/// The digits of one code.
const DIGITS: u32 = 16;
/// The longest reason an operator records.
const REASON_MAX: usize = 500;

/// Why recovery codes could not be registered, or an operator's session opened.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("give a reason: it goes into the audit log")]
    NoReason,
    #[error("no workspace has this slug or id")]
    NoWorkspace,
    #[error("the workspace does not enforce single sign-on: its owners sign in as usual")]
    NotEnforced,
    #[error("no active owner of the workspace has this address")]
    NotAnOwner,
    #[error(
        "the code is not one of the owner's unused recovery codes registered before the workspace began enforcing single sign-on"
    )]
    CodeRefused,
    #[error(
        "the person has no live session: they sign in first (an email code still signs them in), then this runs again"
    )]
    NoSession,
    #[error("no active user has this address")]
    NoUser,
    #[error("the other owners cannot be told: {0}")]
    Mail(accept::Error),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl From<RecoveryError> for Problem {
    fn from(error: RecoveryError) -> Self {
        match error {
            RecoveryError::Db(error) => error.into(),
            RecoveryError::Crypto(error) => error.into(),
            RecoveryError::Session(error) => error.into(),
            other => Problem::internal(&other),
        }
    }
}

/// A set of recovery codes just registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    /// The codes, as a person reads them (`1234-5678-9012-3456`).
    pub codes: Vec<String>,
    /// When they were registered.
    pub created_at: Timestamp,
}

/// Registers a new set of [`CODES`] recovery codes for `user` inside `tx`, replacing the old set
/// (see the module); answers the codes, which are never readable again.
///
/// # Errors
///
/// The random source or the database failed.
pub async fn register(
    tx: &mut Tx,
    keys: &Keys,
    user: Id<User>,
) -> Result<Registered, RecoveryError> {
    sqlx::query!("DELETE FROM recovery_codes WHERE user_id = $1", user.uuid())
        .execute(&mut **tx)
        .await?;
    let created_at = sqlx::query_scalar!(r#"SELECT now() AS "now!: Timestamp""#)
        .fetch_one(&mut **tx)
        .await?;
    let mut codes = Vec::with_capacity(CODES);
    for _ in 0..CODES {
        let digits = crypto::random_digits(DIGITS)?;
        sqlx::query!(
            "INSERT INTO recovery_codes (user_id, code_hash, created_at) VALUES ($1, $2, $3)",
            user.uuid(),
            keys.hash_token(&digits),
            created_at as _,
        )
        .execute(&mut **tx)
        .await?;
        codes.push(grouped(&digits));
    }
    Ok(Registered { codes, created_at })
}

/// `digits` in groups of four joined by hyphens, as a person reads a code.
fn grouped(digits: &str) -> String {
    digits
        .as_bytes()
        .chunks(4)
        .filter_map(|chunk| std::str::from_utf8(chunk).ok())
        .collect::<Vec<_>>()
        .join("-")
}

/// Consumes one of `user`'s unused codes matching `code` (its digits: hyphens and spaces do not
/// count) registered before `before`; answers whether one was.
async fn consume(
    tx: &mut Tx,
    keys: &Keys,
    user: Id<User>,
    code: &str,
    before: Timestamp,
) -> Result<bool, sqlx::Error> {
    let digits: String = code.chars().filter(char::is_ascii_digit).collect();
    let consumed = sqlx::query_scalar!(
        "UPDATE recovery_codes SET used_at = now()
          WHERE id = (SELECT id FROM recovery_codes
                       WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL AND created_at < $3
                       ORDER BY id LIMIT 1 FOR UPDATE)
         RETURNING id",
        user.uuid(),
        keys.hash_token(&digits),
        before as _,
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(consumed.is_some())
}

/// What an operator asks a break-glass for.
#[derive(Debug, Clone, Copy)]
pub struct BreakGlassRequest<'a> {
    /// The workspace: its slug, or its id (`ws_…`).
    pub workspace: &'a str,
    /// The owner's address.
    pub owner: &'a EmailAddress,
    /// One of the owner's recovery codes, as they read it out.
    pub code: &'a str,
    /// The session to open it on; the owner's newest live session when absent.
    pub session: Option<Id<Session>>,
    /// Why, for the audit log and the other owners.
    pub reason: &'a str,
}

/// A break-glass session as opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BreakGlass {
    /// The workspace it repairs.
    pub workspace: Id<Workspace>,
    /// The owner.
    pub user: Id<User>,
    /// The session, now a break-glass session.
    pub session: Id<Session>,
    /// When it ends at the latest.
    pub expires_at: Timestamp,
    /// How many other owners were told by email.
    pub owners_told: usize,
}

/// Opens a break-glass session as `request` asks, inside `tx` as the operator's login (see the
/// module). After the commit the caller wakes the sender for the notices.
///
/// # Errors
///
/// A [`RecoveryError`] naming what is missing or refused: no reason, no such workspace, a
/// workspace that enforces nothing, not an active owner, a code refused, no live session; the
/// platform cannot send the notices (no transactional sender); or the database.
pub async fn break_glass(
    tx: &mut Tx,
    keys: &Keys,
    request: &BreakGlassRequest<'_>,
) -> Result<BreakGlass, RecoveryError> {
    let reason = reason(request.reason)?;
    let by_id = request
        .workspace
        .parse::<Id<Workspace>>()
        .ok()
        .map(|id| id.uuid());
    let place = sqlx::query!(
        r#"SELECT id AS "id: Id<Workspace>", name FROM workspaces
            WHERE (id = $1 OR slug = $2) AND deleted_at IS NULL"#,
        by_id,
        request.workspace,
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(RecoveryError::NoWorkspace)?;
    let workspace = WorkspaceId::trusted(place.id.uuid());
    // The connection the session stands in for, and the earliest enforcement in force (a
    // connection without its instant counts as enforcing forever: no code predates it).
    let enforcing = sqlx::query!(
        r#"SELECT id, policy_version, enforced_at AS "enforced_at: Timestamp"
             FROM sso_connections
            WHERE workspace_id = $1 AND enforced AND status = 'active'
            ORDER BY enforced_at NULLS FIRST, id"#,
        workspace.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    let Some(connection) = enforcing.first() else {
        return Err(RecoveryError::NotEnforced);
    };
    let owner = sqlx::query!(
        r#"SELECT u.id AS "id: Id<User>", coalesce(u.name, u.email) AS "shown!"
             FROM users u
             JOIN memberships m ON m.user_id = u.id
            WHERE u.email_key = $1 AND m.workspace_id = $2 AND m.role = 'owner'
              AND m.status = 'active' AND u.status = 'active'"#,
        request.owner.key(),
        workspace.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(RecoveryError::NotAnOwner)?;
    let Some(since) = connection.enforced_at else {
        return Err(RecoveryError::CodeRefused);
    };
    if !consume(tx, keys, owner.id, request.code, since).await? {
        return Err(RecoveryError::CodeRefused);
    }
    let lifetime = session_lifetime(AuthMethod::BreakGlass).absolute;
    let opened = sqlx::query!(
        r#"UPDATE sessions
              SET auth_method = $3, sso_connection_id = $4, sso_policy_version = $5, authenticated_at = now(),
                  active_workspace_id = $6,
                  expires_at = least(expires_at, now() + make_interval(secs => $7)),
                  idle_expires_at = least(idle_expires_at, now() + make_interval(secs => $7))
            WHERE id = (SELECT id FROM sessions
                         WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now() AND idle_expires_at > now()
                           AND ($2::uuid IS NULL OR id = $2)
                         ORDER BY id DESC LIMIT 1 FOR UPDATE)
           RETURNING id AS "id: Id<Session>", expires_at AS "expires_at: Timestamp""#,
        owner.id.uuid(),
        request.session.map(|session| session.uuid()),
        AuthMethod::BreakGlass.as_str(),
        connection.id,
        connection.policy_version,
        workspace.uuid(),
        lifetime.as_secs_f64(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(RecoveryError::NoSession)?;
    audit::record(
        tx,
        workspace,
        AuditActor::System,
        Action::BreakGlassStarted,
        Some(owner.id.to_string()),
        json!({
            "reason": reason,
            "session_id": opened.id,
            "expires_at": opened.expires_at,
        }),
        None,
    )
    .await?;
    let mut owners_told = 0;
    for person in memberships::told(tx, workspace, &[]).await? {
        if person.role != MembershipRole::Owner || person.user == owner.id {
            continue;
        }
        let Ok(to) = EmailAddress::parse(&person.email) else {
            continue;
        };
        accept::transactional(
            tx,
            keys,
            &Transactional::BreakGlass {
                to: &to,
                workspace_name: &place.name,
                owner: &owner.shown,
                reason,
                expires_at: opened.expires_at,
            },
        )
        .await
        .map_err(RecoveryError::Mail)?;
        owners_told += 1;
    }
    Ok(BreakGlass {
        workspace: place.id,
        user: owner.id,
        session: opened.id,
        expires_at: opened.expires_at,
        owners_told,
    })
}

/// An impersonation as opened: printed once for the operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Impersonation {
    /// The person.
    pub user: Id<User>,
    /// The session.
    pub session: Id<Session>,
    /// When it ends, used or not.
    pub expires_at: Timestamp,
    /// The `Cookie` header value that carries it (`__Host-nb_session=…`).
    pub cookie: String,
    /// Its CSRF token, for `X-CSRF-Token` on every change.
    pub csrf_token: String,
    /// The workspaces whose audit log records it.
    pub workspaces: Vec<Id<Workspace>>,
}

/// Opens a 10-minute session as `person` for support, inside `tx` as the operator's login, and
/// records it with `reason` in every workspace where they are an active member (see the module).
///
/// # Errors
///
/// No reason, no active user with the address, the random source, or the database.
pub async fn impersonate(
    tx: &mut Tx,
    keys: &Keys,
    person: &EmailAddress,
    reason: &str,
) -> Result<Impersonation, RecoveryError> {
    let reason = self::reason(reason)?;
    let user = sqlx::query_scalar!(
        r#"SELECT id AS "id: Id<User>" FROM users WHERE email_key = $1 AND status = 'active'"#,
        person.key(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(RecoveryError::NoUser)?;
    let issued = sessions::create(
        tx,
        keys,
        &NewSession {
            user,
            method: AuthMethod::Impersonation,
            sso: None,
            authenticated_at: None,
            ip_hash: None,
            user_agent: Some(format!("Norbelys support: {reason}")),
        },
    )
    .await?;
    let workspaces = sqlx::query_scalar!(
        r#"SELECT workspace_id AS "workspace_id: Id<Workspace>" FROM memberships
            WHERE user_id = $1 AND status = 'active' ORDER BY workspace_id"#,
        user.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    for workspace in &workspaces {
        audit::record(
            tx,
            WorkspaceId::trusted(workspace.uuid()),
            AuditActor::System,
            Action::ImpersonationStarted,
            Some(user.to_string()),
            json!({
                "reason": reason,
                "session_id": issued.id,
                "expires_at": issued.expires_at,
            }),
            None,
        )
        .await?;
    }
    let cookie = issued
        .cookie
        .split(';')
        .next()
        .unwrap_or_default()
        .to_owned();
    Ok(Impersonation {
        user,
        session: issued.id,
        expires_at: issued.expires_at,
        cookie,
        csrf_token: issued.csrf_token,
        workspaces,
    })
}

/// `reason` trimmed, when it says something and fits the audit log.
fn reason(reason: &str) -> Result<&str, RecoveryError> {
    let reason = reason.trim();
    if reason.is_empty() || reason.chars().count() > REASON_MAX {
        return Err(RecoveryError::NoReason);
    }
    Ok(reason)
}

/// The recovery codes routes: registering a set.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(create_codes))
}

/// Recovery codes as registered: shown this once.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct RecoveryCodesObject {
    /// Ten codes of sixteen digits, each usable once.
    pub codes: Vec<String>,
    /// When they were registered: they prove an owner to a break-glass of a workspace that began
    /// enforcing single sign-on after this instant.
    pub created_at: Timestamp,
}

/// Register recovery codes.
///
/// Ten single-use codes, shown this once, replacing any registered before. With one of them,
/// Norbelys support can open a break-glass session for an owner locked out of a workspace by its
/// single sign-on, when the codes were registered before the workspace began enforcing it.
#[utoipa::path(
    post,
    path = "/me/recovery_codes",
    tag = "Dashboard",
    operation_id = "recovery_codes.create",
    responses(
        (status = 201, description = "The codes, shown this once.", body = RecoveryCodesObject),
        (status = 400, description = "No `Idempotency-Key`."),
        (status = 401, description = "No signed-in session."),
        (status = 403, description = "No CSRF token or dashboard origin, a bearer credential (`session_required`), or an operator's impersonation session."),
    ),
    security(("session" = []))
)]
async fn create_codes(
    State(app): State<AppState>,
    signed_in: SignedIn,
) -> ApiResult<(StatusCode, Json<RecoveryCodesObject>)> {
    // Codes outlive the session: an operator's impersonation registers none in the person's name.
    if !may_make(signed_in.row.method, Lasting::RecoveryCodes) {
        return Err(Problem::forbidden(
            "An impersonation session cannot register recovery codes in the person's name.",
        ));
    }
    let mut tx = app.db.begin_as_user(signed_in.user).await?;
    let registered = register(&mut tx, &app.keys, signed_in.user).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(RecoveryCodesObject {
            codes: registered.codes,
            created_at: registered.created_at,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::grouped;

    /// A code reads as four groups of four digits, so a person can read it out over the phone
    /// and an operator type it back with or without the hyphens.
    #[test]
    fn a_code_reads_in_groups_of_four() {
        assert_eq!(grouped("1234567890123456"), "1234-5678-9012-3456");
        assert_eq!(grouped("12345"), "1234-5");
    }
}
