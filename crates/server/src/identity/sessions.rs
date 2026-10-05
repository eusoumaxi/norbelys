//! Browser sessions: the cookie a signed-in browser holds, what it proves, and the request
//! checks that keep a third-party page from using it.
//!
//! # The cookie
//!
//! `__Host-nb_session` carries 32 random bytes (base64url); only their keyed hash is stored
//! (`sessions.token_hash`), so a copy of the table signs nobody in. The cookie is `HttpOnly`
//! (no script reads it), `Secure`, `SameSite=Lax`, `Path=/` and has no `Domain`: the `__Host-`
//! prefix makes the browser refuse it otherwise, so no sibling subdomain can set or overwrite
//! it (<https://httpwg.org/http-extensions/draft-ietf-httpbis-rfc6265bis.html>). The dashboard
//! proxies the API under its own origin, which keeps the cookie first-party. A sign-in always
//! issues a fresh cookie and never reuses one the browser brought, so a cookie planted before
//! sign-in proves nothing after it.
//!
//! # Lifetimes
//!
//! 30 days without use and 90 days at most; 24 hours at most for a session proven through a
//! workspace's single sign-on or opened by an operator's break-glass, 10 minutes for an
//! operator's impersonation (see `domain::identity` and `identity::recovery`). The idle expiry
//! moves forward at most once every 5 minutes, when the cookie is used. A user keeps at most 50
//! live sessions: a sign-in beyond them ends the oldest. A session records how it authenticated
//! (`auth_method`), through which SSO connection under which policy version, and when
//! (`authenticated_at`, the identity provider's `auth_time` for SSO): the workspace authentication
//! proof an enforcing workspace checks.
//!
//! # Which requests the cookie authorises, and how
//!
//! The cookie is accepted only by the dashboard surface's session operations: `/v1/me` and its
//! sub-resources, `POST /v1/auth/tokens`, listing and creating workspaces, starting a passkey
//! registration or an identity link, and the OAuth server's consent (`GET` and `POST
//! /oauth/consent`, with the same CSRF token on the decision). Product calls carry `nbs_` bearer
//! tokens, which no browser attaches by itself. A request to a session operation that carries an
//! `Authorization` header is refused with `403 session_required`: a program's credential never
//! reaches these operations.
//! Every cookie-authorised mutation (any method but `GET`, `HEAD` and `OPTIONS`) must also carry:
//!
//! - an `Origin` on the dashboard's allow-list (`DASHBOARD_ORIGINS`), which a cross-site page
//!   cannot forge;
//! - the session's CSRF token in `X-CSRF-Token`: an HMAC of the session's id, handed to the
//!   dashboard with the session and by `GET /v1/me`, which a cross-site page cannot read;
//! - a JSON body where it has one (checked by the body extractor).
//!
//! `SameSite=Lax` is the second net. The anonymous sign-in calls (`POST /v1/auth/challenges` and
//! `POST /v1/auth/sessions`) are protected against login CSRF the same way without a session:
//! an allowed `Origin` and a custom header ([`SignInGuard`]), as the OWASP cheat sheet describes
//! (<https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html>).
//!
//! # Resolution
//!
//! The authentication middleware resolves the cookie of a request without a bearer token once,
//! by its hash, and leaves a [`SignedIn`] in the request's extensions; handlers take it as an
//! extractor. Revocation is immediate on this path: every request reads the row.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, header};
use axum_extra::extract::CookieJar;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::{self, CryptoError, Keys};
use crate::db::{Database, Tx};
use crate::domain::identity::{self, AuthMethod, Proof, SessionClock, Standing};
use crate::domain::ids::{Id, Session, User};
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::identity::audit::{self, Action, AuditActor};
use crate::jobs::{Effect, Job, JobContext, JobError, Outcome, Queue};
use crate::problem::{Code, Problem};

/// The session cookie.
pub const COOKIE: &str = "__Host-nb_session";
/// The header carrying the CSRF token (and, on the anonymous sign-in calls, any value).
pub const CSRF_HEADER: &str = "x-csrf-token";
/// The most live sessions a user keeps.
pub const MAX_SESSIONS: i64 = 50;

/// Why a session could not be written.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

impl From<SessionError> for Problem {
    fn from(error: SessionError) -> Self {
        match error {
            SessionError::Db(error) => error.into(),
            SessionError::Crypto(error) => error.into(),
        }
    }
}

/// The single sign-on behind a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SsoProof {
    /// The connection.
    pub connection: Uuid,
    /// Its policy version at the sign-in.
    pub policy_version: i32,
}

/// What a sign-in creates a session from.
#[derive(Debug, Clone)]
pub struct NewSession {
    /// The user.
    pub user: Id<User>,
    /// How they authenticated.
    pub method: AuthMethod,
    /// The SSO connection, for `sso`.
    pub sso: Option<SsoProof>,
    /// When they authenticated: the identity provider's `auth_time` for `sso`, now otherwise.
    pub authenticated_at: Option<Timestamp>,
    /// The keyed hash of the client address.
    pub ip_hash: Option<Vec<u8>>,
    /// The browser's `User-Agent`, for the user's list of sessions (at most 512 characters).
    pub user_agent: Option<String>,
}

/// A session just created.
#[derive(Debug, Clone)]
pub struct Issued {
    /// Its id.
    pub id: Id<Session>,
    /// The `Set-Cookie` value that hands it to the browser.
    pub cookie: String,
    /// Its CSRF token.
    pub csrf_token: String,
    /// Its absolute expiry.
    pub expires_at: Timestamp,
    /// Older sessions the cap of 50 ended, for the authority to forget.
    pub ended: Vec<Id<Session>>,
}

/// The `Set-Cookie` value of a session cookie holding `token` for `max_age_seconds`.
#[must_use]
pub fn cookie(token: &str, max_age_seconds: u64) -> String {
    format!("{COOKIE}={token}; Path=/; Max-Age={max_age_seconds}; Secure; HttpOnly; SameSite=Lax")
}

/// The `Set-Cookie` value that removes the session cookie.
pub const CLEAR_COOKIE: &str =
    "__Host-nb_session=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax";

/// The CSRF token of `session`.
#[must_use]
pub fn csrf_token(keys: &Keys, session: Id<Session>) -> String {
    keys.csrf_token(session.uuid().as_bytes())
}

/// Creates a session for `new` and returns its cookie; ends the user's oldest live sessions
/// beyond 50.
///
/// # Errors
///
/// The random source or the database failed.
pub async fn create(tx: &mut Tx, keys: &Keys, new: &NewSession) -> Result<Issued, SessionError> {
    let token = crypto::random_token(32)?;
    let lifetime = identity::session_lifetime(new.method);
    let id = Id::<Session>::new();
    let user_agent = new
        .user_agent
        .as_deref()
        .map(|agent| agent.chars().take(512).collect::<String>());
    let expires_at = sqlx::query_scalar!(
        r#"INSERT INTO sessions (id, user_id, token_hash, auth_method, sso_connection_id, sso_policy_version,
                                 authenticated_at, expires_at, idle_expires_at, ip_hash, user_agent)
           VALUES ($1, $2, $3, $4, $5, $6, coalesce($7, now()), now() + make_interval(secs => $8),
                   now() + make_interval(secs => $9), $10, $11)
           RETURNING expires_at AS "expires_at: Timestamp""#,
        id.uuid(),
        new.user.uuid(),
        keys.hash_token(&token),
        new.method.as_str(),
        new.sso.map(|sso| sso.connection),
        new.sso.map(|sso| sso.policy_version),
        new.authenticated_at as _,
        lifetime.absolute.as_secs_f64(),
        lifetime.idle.as_secs_f64(),
        new.ip_hash,
        user_agent,
    )
    .fetch_one(&mut **tx)
    .await?;
    let ended = sqlx::query_scalar!(
        r#"UPDATE sessions SET revoked_at = now(), revoked_reason = 'replaced'
            WHERE user_id = $1 AND revoked_at IS NULL
              AND id NOT IN (SELECT id FROM sessions WHERE user_id = $1 AND revoked_at IS NULL
                              ORDER BY id DESC LIMIT $2)
           RETURNING id AS "id: Id<Session>""#,
        new.user.uuid(),
        MAX_SESSIONS,
    )
    .fetch_all(&mut **tx)
    .await?;
    // The person's own log: who signed in how, and which sessions the cap ended. An operator's
    // impersonation is the platform acting, not the person.
    let actor = if new.method == AuthMethod::Impersonation {
        AuditActor::System
    } else {
        AuditActor::User(new.user)
    };
    audit::record_user(
        tx,
        new.user,
        actor,
        Action::SessionCreated,
        Some(id.to_string()),
        serde_json::json!({ "method": new.method.as_str(), "ended": ended }),
        new.ip_hash.as_deref(),
    )
    .await?;
    Ok(Issued {
        id,
        cookie: cookie(&token, lifetime.absolute.as_secs()),
        csrf_token: csrf_token(keys, id),
        expires_at,
        ended,
    })
}

/// A session as authentication reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// The session.
    pub id: Id<Session>,
    /// Its user.
    pub user: Id<User>,
    /// How it authenticated.
    pub method: AuthMethod,
    /// The SSO connection, for `sso`.
    pub sso_connection: Option<Uuid>,
    /// The connection's policy version at the sign-in, for `sso`.
    pub sso_policy_version: Option<i32>,
    /// When the person authenticated.
    pub authenticated_at: Timestamp,
    /// When it was revoked, if it was.
    pub revoked_at: Option<Timestamp>,
    /// Its absolute expiry.
    pub expires_at: Timestamp,
    /// Its idle expiry.
    pub idle_expires_at: Timestamp,
    /// When its cookie was last used.
    pub last_seen_at: Timestamp,
    /// False when its user is suspended.
    pub user_active: bool,
}

impl SessionRow {
    /// Its workspace authentication proof.
    #[must_use]
    pub fn proof(&self) -> Proof {
        Proof {
            method: self.method,
            connection: self.sso_connection,
            policy_version: self.sso_policy_version,
            authenticated_at: self.authenticated_at.0,
        }
    }

    /// Whether it still proves its user at `now` (a suspended user's session does not).
    #[must_use]
    pub fn active(&self, now: Timestamp) -> bool {
        self.user_active
            && identity::session_standing(
                &SessionClock {
                    revoked_at: self.revoked_at.map(|at| at.0),
                    expires_at: self.expires_at.0,
                    idle_expires_at: self.idle_expires_at.0,
                },
                now.0,
            ) == Standing::Active
    }
}

/// The stored session before its authentication method has been validated.
struct StoredSession {
    id: Id<Session>,
    user_id: Id<User>,
    auth_method: String,
    sso_connection_id: Option<Uuid>,
    sso_policy_version: Option<i32>,
    authenticated_at: Timestamp,
    revoked_at: Option<Timestamp>,
    expires_at: Timestamp,
    idle_expires_at: Timestamp,
    last_seen_at: Timestamp,
    user_active: bool,
}

impl StoredSession {
    /// Refuses an unknown stored method rather than granting an unrecognised proof.
    fn validated(self) -> Option<SessionRow> {
        let row = self;
        Some(SessionRow {
            id: row.id,
            user: row.user_id,
            method: row.auth_method.parse().ok()?,
            sso_connection: row.sso_connection_id,
            sso_policy_version: row.sso_policy_version,
            authenticated_at: row.authenticated_at,
            revoked_at: row.revoked_at,
            expires_at: row.expires_at,
            idle_expires_at: row.idle_expires_at,
            last_seen_at: row.last_seen_at,
            user_active: row.user_active,
        })
    }
}

/// Reads the session whose cookie token hashes to `hash`.
///
/// # Errors
///
/// The database failed.
pub async fn by_hash(tx: &mut Tx, hash: &[u8]) -> Result<Option<SessionRow>, sqlx::Error> {
    let row = sqlx::query_as!(
        StoredSession,
        r#"SELECT s.id AS "id: Id<Session>", s.user_id AS "user_id: Id<User>", s.auth_method,
                  s.sso_connection_id, s.sso_policy_version, s.authenticated_at AS "authenticated_at: Timestamp",
                  s.revoked_at AS "revoked_at: Timestamp", s.expires_at AS "expires_at: Timestamp",
                  s.idle_expires_at AS "idle_expires_at: Timestamp", s.last_seen_at AS "last_seen_at: Timestamp",
                  u.status = 'active' AS "user_active!"
             FROM sessions s JOIN users u ON u.id = s.user_id
            WHERE s.token_hash = $1"#,
        hash
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.and_then(StoredSession::validated))
}

/// Reads session `id` (the session a workspace token names).
///
/// # Errors
///
/// The database failed.
pub async fn by_id(db: &Database, id: Id<Session>) -> Result<Option<SessionRow>, sqlx::Error> {
    let row = sqlx::query_as!(
        StoredSession,
        r#"SELECT s.id AS "id: Id<Session>", s.user_id AS "user_id: Id<User>", s.auth_method,
                  s.sso_connection_id, s.sso_policy_version, s.authenticated_at AS "authenticated_at: Timestamp",
                  s.revoked_at AS "revoked_at: Timestamp", s.expires_at AS "expires_at: Timestamp",
                  s.idle_expires_at AS "idle_expires_at: Timestamp", s.last_seen_at AS "last_seen_at: Timestamp",
                  u.status = 'active' AS "user_active!"
             FROM sessions s JOIN users u ON u.id = s.user_id
            WHERE s.id = $1"#,
        id.uuid()
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.and_then(StoredSession::validated))
}

/// Moves `session`'s idle expiry forward (see the module); never past its absolute expiry.
///
/// # Errors
///
/// The database failed.
pub async fn touch(tx: &mut Tx, session: &SessionRow) -> Result<(), sqlx::Error> {
    let now = crate::process::now();
    let idle = Timestamp(identity::idle_expiry(
        session.method,
        now.0,
        session.expires_at.0,
    ));
    sqlx::query!(
        "UPDATE sessions SET last_seen_at = now(), idle_expires_at = greatest(idle_expires_at, $2)
          WHERE id = $1 AND revoked_at IS NULL",
        session.id.uuid(),
        idle as _,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Revokes `user`'s session `session` with `reason` (`signed_out` for this browser, `revoked`
/// for another), by the person themselves from the client whose address hashes to `ip_hash`;
/// true when a live session was revoked, which the person's own log records.
///
/// # Errors
///
/// The database failed.
pub async fn revoke(
    tx: &mut Tx,
    user: Id<User>,
    session: Id<Session>,
    reason: &str,
    ip_hash: Option<&[u8]>,
) -> Result<bool, sqlx::Error> {
    let revoked = sqlx::query!(
        "UPDATE sessions SET revoked_at = now(), revoked_reason = $3
          WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
        session.uuid(),
        user.uuid(),
        reason,
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if revoked == 0 {
        return Ok(false);
    }
    let action = if reason == "signed_out" {
        Action::SessionEnded
    } else {
        Action::SessionRevoked
    };
    audit::record_user(
        tx,
        user,
        AuditActor::User(user),
        action,
        Some(session.to_string()),
        serde_json::json!({ "reason": reason }),
        ip_hash,
    )
    .await?;
    Ok(true)
}

/// A session as `GET /v1/me` lists it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SessionObject {
    pub id: Id<Session>,
    /// True for the session of this request.
    pub current: bool,
    /// How it authenticated.
    pub auth_method: AuthMethod,
    /// When the person authenticated.
    pub authenticated_at: Timestamp,
    pub created_at: Timestamp,
    /// When its cookie was last used (to within 5 minutes).
    pub last_seen_at: Timestamp,
    /// Its absolute expiry.
    pub expires_at: Timestamp,
    /// The browser that signed in, as it named itself.
    pub user_agent: Option<String>,
}

/// `user`'s live sessions, newest first (at most 50 exist).
///
/// # Errors
///
/// The database failed.
pub async fn list(
    tx: &mut Tx,
    user: Id<User>,
    current: Id<Session>,
) -> Result<Vec<SessionObject>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id AS "id: Id<Session>", auth_method, authenticated_at AS "authenticated_at: Timestamp",
                  created_at AS "created_at: Timestamp", last_seen_at AS "last_seen_at: Timestamp",
                  expires_at AS "expires_at: Timestamp", user_agent
             FROM sessions
            WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now() AND idle_expires_at > now()
            ORDER BY id DESC LIMIT $2"#,
        user.uuid(),
        MAX_SESSIONS,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(SessionObject {
                current: row.id == current,
                id: row.id,
                auth_method: row.auth_method.parse().ok()?,
                authenticated_at: row.authenticated_at,
                created_at: row.created_at,
                last_seen_at: row.last_seen_at,
                expires_at: row.expires_at,
                user_agent: row.user_agent,
            })
        })
        .collect())
}

/// A signed-in browser: the live session a request's cookie proves (see the module).
#[derive(Debug, Clone)]
pub struct SignedIn {
    /// The user.
    pub user: Id<User>,
    /// The session.
    pub session: Id<Session>,
    /// The session as read.
    pub row: SessionRow,
}

/// The session cookie of `headers`, if any.
#[must_use]
pub fn cookie_token(headers: &HeaderMap) -> Option<String> {
    CookieJar::from_headers(headers)
        .get(COOKIE)
        .map(|cookie| cookie.value().to_owned())
        .filter(|value| !value.is_empty())
}

/// Resolves the session cookie in `headers`: the live session it proves, its idle expiry moved
/// forward when due; `None` without a cookie or for a cookie that proves nothing.
///
/// # Errors
///
/// The database failed.
pub async fn resolve(
    db: &Database,
    keys: &Keys,
    headers: &HeaderMap,
) -> Result<Option<SignedIn>, sqlx::Error> {
    let Some(token) = cookie_token(headers) else {
        return Ok(None);
    };
    let mut tx = db.begin().await?;
    let row = by_hash(&mut tx, &keys.hash_token(&token)).await?;
    let now = crate::process::now();
    let Some(row) = row.filter(|row| row.active(now)) else {
        tx.commit().await?;
        return Ok(None);
    };
    if identity::touch_due(row.last_seen_at.0, now.0) {
        touch(&mut tx, &row).await?;
    }
    tx.commit().await?;
    Ok(Some(SignedIn {
        user: row.user,
        session: row.id,
        row,
    }))
}

/// `403 session_required`: a program's credential on a session operation.
#[must_use]
pub fn session_required() -> Problem {
    Problem::new(
        Code::SessionRequired,
        "This operation accepts only a signed-in browser session; API keys and other bearer tokens are refused.",
    )
}

/// Checks that a mutation comes from one of the dashboard's origins.
///
/// # Errors
///
/// `403 forbidden` for a missing or foreign `Origin`.
pub fn check_origin(headers: &HeaderMap, allowed: &[url::Url]) -> Result<(), Problem> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            Problem::forbidden("The request must come from the dashboard (`Origin`).")
        })?;
    if allowed
        .iter()
        .any(|url| url.origin().ascii_serialization() == origin)
    {
        Ok(())
    } else {
        Err(Problem::forbidden(
            "The request's `Origin` is not one of the dashboard's.",
        ))
    }
}

impl FromRequestParts<AppState> for SignedIn {
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if parts.headers.contains_key(header::AUTHORIZATION) {
            return Err(session_required());
        }
        let signed_in = match parts.extensions.get::<Self>() {
            Some(signed_in) => signed_in.clone(),
            None => resolve(&state.db, &state.keys, &parts.headers)
                .await?
                .ok_or_else(Problem::unauthorized)?,
        };
        if !parts.method.is_safe() {
            check_origin(&parts.headers, &state.identity.settings.dashboard_origins)?;
            let presented = parts
                .headers
                .get(CSRF_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if !state
                .keys
                .verify_csrf(signed_in.session.uuid().as_bytes(), presented)
            {
                return Err(Problem::forbidden(
                    "The request lacks the session's CSRF token (`X-CSRF-Token`).",
                ));
            }
        }
        Ok(signed_in)
    }
}

/// The login-CSRF guard of the anonymous sign-in calls: an allowed `Origin` and the custom
/// `X-CSRF-Token` header (any value), which a cross-site form cannot send (see the module). A
/// program's bearer credential is refused here too.
#[derive(Debug, Clone, Copy)]
pub struct SignInGuard;

impl FromRequestParts<AppState> for SignInGuard {
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if parts.headers.contains_key(header::AUTHORIZATION) {
            return Err(session_required());
        }
        check_origin(&parts.headers, &state.identity.settings.dashboard_origins)?;
        if parts
            .headers
            .get(CSRF_HEADER)
            .is_none_or(|value| value.is_empty())
        {
            return Err(Problem::forbidden(
                "Sign-in requests carry the `X-CSRF-Token` header.",
            ));
        }
        Ok(Self)
    }
}

/// Sessions one chunk of `sessions.revoke_user` revokes at most.
const REVOKE_BATCH: i64 = 1_000;

/// `sessions.revoke_user`: revokes every live session of a person, a batch per chunk, when an
/// operator suspends them. The suspension already stops their sessions at once (resolving a
/// cookie reads the user's standing with the session); the revocation makes it final, so a later
/// reactivation revives none of them and each row says why it ended. A repeat finds nothing left
/// to revoke.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeUser {
    /// The person.
    pub user: Uuid,
}

impl Job for RevokeUser {
    const KIND: &'static str = "sessions.revoke_user";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::Idempotent;

    fn unique_key(&self) -> Option<String> {
        Some(self.user.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        loop {
            let mut chunk = cx.begin().await?;
            let revoked = sqlx::query!(
                "UPDATE sessions SET revoked_at = now(), revoked_reason = 'user_suspended'
                  WHERE id IN (SELECT id FROM sessions WHERE user_id = $1 AND revoked_at IS NULL LIMIT $2)",
                self.user,
                REVOKE_BATCH,
            )
            .execute(&mut **chunk.tx())
            .await?
            .rows_affected();
            cx.checkpoint(chunk, serde_json::json!({ "revoked": revoked }))
                .await?;
            if revoked < u64::try_from(REVOKE_BATCH).unwrap_or(u64::MAX) {
                return Ok(Outcome::Done);
            }
            if cx.should_yield() {
                return Ok(Outcome::Yield {
                    after: std::time::Duration::ZERO,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    /// The cookie carries every attribute the `__Host-` prefix and the threat model need, and
    /// clearing it uses the same attributes so the browser matches it.
    #[test]
    fn the_cookie_is_host_only_secure_and_unreadable_by_scripts() {
        let value = cookie("t0k3n", 60);
        assert_eq!(
            value,
            "__Host-nb_session=t0k3n; Path=/; Max-Age=60; Secure; HttpOnly; SameSite=Lax"
        );
        assert!(!value.contains("Domain"));
        assert!(CLEAR_COOKIE.starts_with("__Host-nb_session=;"));
        assert!(CLEAR_COOKIE.ends_with("Secure; HttpOnly; SameSite=Lax"));
    }

    /// Only an exact dashboard origin passes: another scheme, host or port, a missing header or
    /// `null` (a sandboxed or privacy-redirected request) is refused.
    #[test]
    fn mutations_come_only_from_the_dashboard_origins() {
        let allowed = vec![url::Url::parse("https://app.norbelys.test").unwrap()];
        let with = |origin: Option<&str>| {
            let mut headers = HeaderMap::new();
            if let Some(origin) = origin {
                headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
            }
            check_origin(&headers, &allowed).map_err(|problem| problem.code)
        };
        assert_eq!(with(Some("https://app.norbelys.test")), Ok(()));
        for refused in [
            None,
            Some("null"),
            Some("http://app.norbelys.test"),
            Some("https://app.norbelys.test:8443"),
            Some("https://evil.test"),
        ] {
            assert_eq!(with(refused), Err(Code::Forbidden), "{refused:?}");
        }
    }

    /// A CSRF token is bound to its session: it verifies for that session only, and a
    /// truncated or empty one never does.
    #[test]
    fn csrf_tokens_are_bound_to_their_session() {
        let keys = crate::testing::keys();
        let (one, two) = (Id::<Session>::new(), Id::<Session>::new());
        let token = csrf_token(&keys, one);
        assert!(keys.verify_csrf(one.uuid().as_bytes(), &token));
        assert!(!keys.verify_csrf(two.uuid().as_bytes(), &token));
        assert!(!keys.verify_csrf(one.uuid().as_bytes(), &token[..token.len() - 2]));
        assert!(!keys.verify_csrf(one.uuid().as_bytes(), ""));
    }
}
