//! The principal a credential proves, and the authority caches.
//!
//! # Credentials
//!
//! `Authorization: Bearer <credential>` becomes a [`Principal`]: the workspace the request acts
//! in, who acts, the scopes they hold and their role. The credential's prefix selects the
//! verifier:
//!
//! - `nb_live_` and `nb_test_`: API keys, delegated by a member ([`super::api_keys`]);
//! - `nbs_`: workspace tokens the dashboard mints from its browser session
//!   ([`super::tokens`]);
//! - `nbc_`: the command-line client's access tokens, issued by the OAuth authorization server
//!   (`super::oauth`) from a grant whose resource is the API. The token's grant is read through
//!   the grants cache like a session, the person's standing through the membership cache, and the
//!   workspace's SSO enforcement is checked against the proof the grant recorded at consent;
//! - `nbo_`: MCP access tokens. The API refuses them for good (`401`): they are for the MCP
//!   resource, whose own guard verifies them the same way with [`Authority::verify_mcp`].
//!
//! A credential belongs to exactly one workspace, so there is no workspace header and no way to
//! point a credential at another tenant. A request without a bearer credential may carry the
//! session cookie instead; the middleware resolves it ([`super::sessions::SignedIn`]) for the
//! dashboard's session operations, which accept nothing else.
//!
//! # Caches and revocation
//!
//! Verification re-reads the primary database by default (`AUTHORITY_CACHE_SECONDS=0`),
//! including the credential and current membership. A new request after a committed revocation
//! therefore cannot reuse authority verified on another replica. Requests already executing
//! retain the principal they proved at admission; sensitive writes re-read it in their transaction.
//!
//! Optional `AUTHORITY_CACHE_SECONDS` (1 to 60) explicitly accepts bounded reuse. Its lifetime
//! starts when the database read starts and never passes the credential's own expiry. Keys,
//! sessions, grants and standings have separate bounded caches; local changes call `forget`,
//! `forget_session`, `forget_grant`, `forget_member` or `forget_workspace`. Other replicas with
//! this opt-in catch up within their configured TTL. A missing, revoked or expired row fails closed.
//!
//! A principal's scopes intersect its credential's grant with the member's current role.
//!
//! # Security events
//!
//! Every denial, and every sign-in, is the canonical `auth.decision` event ([`record_decision`])
//! with its reason, the kind of actor, the workspace when known, the sign-in method and the keyed
//! hash of the client address; denials are also counted (`norbelys_auth_denied_total` by reason),
//! and per client address over each minute, of which only the largest count is exported
//! (`norbelys_auth_denials_top_key_per_minute`, see `domain::telemetry::DenialWindow`): the
//! `auth-abuse` alert watches both, the flood of all denials and one address denied over and over.

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, header};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use moka::future::Cache;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use uuid::Uuid;

use super::oauth::grants::{self, GrantRow};
use super::sessions::{self, SessionRow, SignedIn};
use super::{api_keys, tokens};
use crate::crypto;
use crate::db::Database;
use crate::domain::identity::{AuthMethod, Enforcement, MembershipStatus, proof_holds};
use crate::domain::ids::{ApiKey, Grant, Id, Session, User, WorkspaceId};
use crate::domain::oauth::{Audience, Resources};
use crate::domain::scope::{MembershipRole, Scope, ScopeSet};
use crate::domain::telemetry::DenialWindow;
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::ratelimit::ClientAddress;
use crate::problem::Problem;

/// The largest authority reuse window an operator may explicitly select.
const MAX_AUTHORITY_TTL: Duration = Duration::from_secs(60);

static DENIALS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_auth_denied_total")
        .with_description("Authentication and authorization decisions that denied, by reason.")
        .build()
});

/// This process's denials per client address over each minute (see the module).
static DENIED_ADDRESSES: LazyLock<Mutex<DenialWindow>> = LazyLock::new(Mutex::default);

/// The current minute, in Unix minutes.
fn minute_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() / 60)
}

/// Registers `norbelys_auth_denials_top_key_per_minute`: the most authentications this process
/// denied to one client address over the last complete minute or the current one, read whenever
/// the metrics are collected (see the module). The api calls it once, at its start, so the gauge
/// reads 0 from the first collection rather than appearing with the first denial.
pub fn observe_denials() {
    let _ = opentelemetry::global::meter("norbelys")
        .u64_observable_gauge("norbelys_auth_denials_top_key_per_minute")
        .with_description(
            "The most authentications denied to one client address over the last complete minute \
             or the current one.",
        )
        .with_callback(|observer| {
            if let Ok(mut window) = DENIED_ADDRESSES.lock() {
                observer.observe(window.top(minute_now()), &[]);
            }
        })
        .build();
}

/// Who acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    /// A person, through a workspace token minted from their browser session.
    User {
        /// The person.
        user: Id<User>,
        /// The session the token was minted from.
        session: Id<Session>,
    },
    /// A delegated API key.
    ApiKey {
        /// The key.
        key: Id<ApiKey>,
        /// The member who created it.
        created_by: Id<User>,
    },
    /// An application acting for a person through an OAuth grant: the command-line client or an
    /// MCP client.
    OAuth {
        /// The person who consented.
        user: Id<User>,
        /// The grant.
        grant: Id<Grant>,
    },
}

impl Actor {
    /// The audit log's `actor_id`.
    #[must_use]
    pub fn id(self) -> String {
        match self {
            Self::User { user, .. } => user.to_string(),
            Self::ApiKey { key, .. } => key.to_string(),
            Self::OAuth { grant, .. } => grant.to_string(),
        }
    }

    /// The user behind the actor.
    #[must_use]
    pub fn user(self) -> Id<User> {
        match self {
            Self::User { user, .. }
            | Self::OAuth { user, .. }
            | Self::ApiKey {
                created_by: user, ..
            } => user,
        }
    }
}

/// The kind of credential that proved a principal: what decides the surfaces it may call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Credential {
    /// A dashboard workspace token (`nbs_`), minted from a browser session: the only bearer the
    /// dashboard surface accepts.
    WorkspaceToken,
    /// An API key (`nb_live_`, `nb_test_`).
    ApiKey,
    /// An OAuth access token: the command-line client's (`nbc_`) or an MCP client's (`nbo_`).
    OAuth,
}

/// What a verified credential may do, in one workspace.
#[derive(Debug, Clone, Copy)]
pub struct Principal {
    pub workspace: WorkspaceId,
    pub actor: Actor,
    pub scopes: ScopeSet,
    pub role: MembershipRole,
    /// True for a workspace in test mode: the sender uses a fake transport.
    pub test_mode: bool,
    /// The kind of credential.
    pub credential: Credential,
}

impl Principal {
    /// The only authorization call: the principal must hold `scope`.
    ///
    /// # Errors
    ///
    /// `403 forbidden` naming the missing scope.
    pub fn require(&self, scope: Scope) -> Result<(), Problem> {
        if self.scopes.contains(scope) {
            Ok(())
        } else {
            Err(Problem::forbidden(format!(
                "The credential lacks the `{scope}` scope."
            )))
        }
    }

    /// The authorization call for a member-owned resource (a connection, owned by the member who
    /// created it): the principal must hold `scope`, and a principal acting for a `member` acts
    /// only on the resources that member created. Owners and admins act on every one.
    ///
    /// # Errors
    ///
    /// `403 forbidden` naming the missing scope, or the ownership.
    pub fn require_owned(&self, scope: Scope, owner: Option<Id<User>>) -> Result<(), Problem> {
        self.require(scope)?;
        if self.role == MembershipRole::Member && owner != Some(self.actor.user()) {
            return Err(Problem::forbidden(
                "A member manages only the connections they created.",
            ));
        }
        Ok(())
    }

    /// The surface check of the dashboard's workspace operations (members, invitations, keys,
    /// SSO, the audit log, workspace changes): only a workspace token minted from a browser
    /// session reaches them, so no credential a program holds can create another credential or a
    /// member.
    ///
    /// # Errors
    ///
    /// `403 session_required` for any other credential.
    pub fn require_session(&self) -> Result<(), Problem> {
        match self.credential {
            Credential::WorkspaceToken => Ok(()),
            Credential::ApiKey | Credential::OAuth => Err(sessions::session_required()),
        }
    }

    /// The check of the owner-only actions: deleting the workspace, making someone an owner,
    /// switching single sign-on enforcement.
    ///
    /// # Errors
    ///
    /// `403 forbidden` for anyone but an owner.
    pub fn require_owner(&self) -> Result<(), Problem> {
        if self.role == MembershipRole::Owner {
            Ok(())
        } else {
            Err(Problem::forbidden(
                "Only an owner of the workspace may do this.",
            ))
        }
    }

    /// The session behind the principal, for a workspace token.
    #[must_use]
    pub fn session(&self) -> Option<Id<Session>> {
        match self.actor {
            Actor::User { session, .. } => Some(session),
            Actor::ApiKey { .. } | Actor::OAuth { .. } => None,
        }
    }
}

/// An `auth.decision` event (see the module).
#[derive(Debug, Clone, Copy)]
pub struct Decision<'a> {
    /// `denied` or `signed_in`.
    pub outcome: &'static str,
    /// Why (`unknown_credential`, `session_revoked`, `code_mismatch`, …).
    pub reason: &'static str,
    /// `user`, `api_key`, `workspace_token` or `anonymous`.
    pub actor_kind: &'static str,
    /// The workspace, when known.
    pub workspace: Option<WorkspaceId>,
    /// The sign-in method, for sign-ins.
    pub method: Option<AuthMethod>,
    /// The keyed hash of the client address.
    pub ip_hash: Option<&'a [u8]>,
}

/// Emits the `auth.decision` canonical event, and counts a denial, by reason and against its
/// client address (see the module).
pub fn record_decision(decision: &Decision<'_>) {
    if decision.outcome == "denied" {
        DENIALS.add(1, &[KeyValue::new("reason", decision.reason)]);
        if let Some(address) = decision.ip_hash
            && let Ok(mut window) = DENIED_ADDRESSES.lock()
        {
            window.deny(address, minute_now());
        }
    }
    crate::telemetry::unit(crate::telemetry::Event::AuthDecision);
    tracing::info!(
        event = "auth.decision",
        outcome = decision.outcome,
        reason = decision.reason,
        actor_kind = decision.actor_kind,
        workspace_id = decision.workspace.map(|workspace| workspace.to_string()),
        auth_method = decision.method.map(AuthMethod::as_str),
        ip_hash = decision.ip_hash.map(crypto::hex),
        "auth decision"
    );
}

#[derive(Clone)]
struct Cached {
    principal: Principal,
    valid_until: Instant,
}

#[derive(Clone)]
struct CachedSession {
    row: SessionRow,
    valid_until: Instant,
}

/// A user's standing in a workspace, as a workspace token's verification (and minting) reads it.
#[derive(Clone, Debug)]
pub struct Standing {
    /// The user.
    pub user: Id<User>,
    /// Their role.
    pub role: MembershipRole,
    /// Their membership's status.
    pub status: MembershipStatus,
    /// Whether the workspace is in test mode.
    pub test_mode: bool,
    /// Whether the workspace is being deleted.
    pub deleted: bool,
    /// The workspace's enforcing SSO connections.
    pub enforcing: Vec<Enforcement>,
}

#[derive(Clone)]
struct CachedGrant {
    row: GrantRow,
    valid_until: Instant,
}

#[derive(Clone)]
struct CachedStanding {
    standing: Standing,
    valid_until: Instant,
}

/// The authority caches shared by the api process (see the module).
#[derive(Clone)]
pub struct Authority {
    ttl: Duration,
    keys: Cache<Vec<u8>, Cached>,
    sessions: Cache<Uuid, CachedSession>,
    grants: Cache<Uuid, CachedGrant>,
    memberships: Cache<(Uuid, Uuid), CachedStanding>,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Authority(..)")
    }
}

impl Default for Authority {
    fn default() -> Self {
        Self::new()
    }
}

/// The instant `at` falls on, for a cache entry's validity; now when it is past.
fn instant_of(at: Timestamp, now: Timestamp) -> Instant {
    let ahead = at.0.duration_since(now.0);
    let ahead = if ahead.is_negative() {
        Duration::ZERO
    } else {
        ahead.unsigned_abs()
    };
    Instant::now() + ahead
}

impl Authority {
    /// Empty caches.
    #[must_use]
    pub fn new() -> Self {
        Self::with_ttl(Duration::ZERO)
    }

    /// Optional authority reuse, bounded to 60 seconds. Positive TTLs explicitly accept a
    /// cross-replica revocation window; the default zero never reuses a verified principal.
    #[must_use]
    pub fn with_ttl(ttl: Duration) -> Self {
        let ttl = ttl.min(MAX_AUTHORITY_TTL);
        Self {
            ttl,
            keys: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(ttl)
                .support_invalidation_closures()
                .build(),
            sessions: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(ttl)
                .build(),
            grants: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(ttl)
                .build(),
            memberships: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(ttl)
                .support_invalidation_closures()
                .build(),
        }
    }

    /// Drops what an API key's revocation invalidated in this process; other replicas catch up
    /// within the TTL.
    pub async fn forget(&self, credential_hash: &[u8]) {
        self.keys.invalidate(credential_hash).await;
    }

    /// Drops a revoked session and every standing read through it.
    pub async fn forget_session(&self, session: Id<Session>) {
        self.sessions.invalidate(&session.uuid()).await;
        let id = session.uuid();
        if let Err(error) = self
            .memberships
            .invalidate_entries_if(move |(credential, _), _| *credential == id)
        {
            tracing::warn!(error = %error, "a session's standings were not dropped");
        }
    }

    /// Drops a revoked OAuth grant and every standing read through it.
    pub async fn forget_grant(&self, grant: Id<Grant>) {
        self.grants.invalidate(&grant.uuid()).await;
        let id = grant.uuid();
        if let Err(error) = self
            .memberships
            .invalidate_entries_if(move |(credential, _), _| *credential == id)
        {
            tracing::warn!(error = %error, "a grant's standings were not dropped");
        }
    }

    /// Drops `user`'s standings and keys in `workspace` after their membership changed.
    pub fn forget_member(&self, workspace: WorkspaceId, user: Id<User>) {
        let ws = workspace.uuid();
        let invalidated = self
            .memberships
            .invalidate_entries_if(move |(_, cached_ws), cached| {
                *cached_ws == ws && cached.standing.user == user
            })
            .and_then(|_| {
                self.keys.invalidate_entries_if(move |_, cached| {
                    cached.principal.workspace.uuid() == ws && cached.principal.actor.user() == user
                })
            });
        if let Err(error) = invalidated {
            tracing::warn!(error = %error, "a member's standings were not dropped");
        }
    }

    /// Drops every standing and key of `workspace` (its mode, deletion or SSO policy changed).
    pub fn forget_workspace(&self, workspace: WorkspaceId) {
        let ws = workspace.uuid();
        let invalidated = self
            .memberships
            .invalidate_entries_if(move |(_, cached_ws), _| *cached_ws == ws)
            .and_then(|_| {
                self.keys
                    .invalidate_entries_if(move |_, cached| cached.principal.workspace.uuid() == ws)
            });
        if let Err(error) = invalidated {
            tracing::warn!(error = %error, "a workspace's standings were not dropped");
        }
    }

    /// Verifies a bearer credential for a request from `client`; a denial is recorded as a
    /// security event.
    ///
    /// # Errors
    ///
    /// `401 unauthorized` for anything that does not prove a principal; `503` when the database
    /// cannot be read.
    pub async fn verify(
        &self,
        state: &AppState,
        credential: &str,
        client: ClientAddress,
    ) -> Result<Principal, Problem> {
        let verified = if credential.starts_with(tokens::PREFIX) {
            self.verify_token(state, credential).await
        } else if credential.starts_with(Audience::Cli.prefix()) {
            self.verify_access(state, credential, Audience::Cli).await
        } else if credential.starts_with(Audience::Mcp.prefix()) {
            // MCP tokens are for the MCP resource, never the API.
            Err(Denied::Reason("token_not_accepted", "oauth"))
        } else {
            self.verify_key(&state.db, credential).await
        };
        Self::answer(state, client, verified)
    }

    /// Verifies a bearer credential for the MCP server: an `nbo_` access token issued for this
    /// deployment's MCP resource proves a principal there; while the authorization server is off
    /// (`OAUTH_SERVER_DISABLED`), when no such token can be issued, an API key does too, carried
    /// by any client that lets a person paste a bearer token. A denial is recorded as a security
    /// event.
    ///
    /// # Errors
    ///
    /// `401 unauthorized` for anything else; `503` when the database cannot be read.
    pub async fn verify_mcp(
        &self,
        state: &AppState,
        credential: &str,
        client: ClientAddress,
    ) -> Result<Principal, Problem> {
        let verified = if credential.starts_with(Audience::Mcp.prefix()) {
            self.verify_access(state, credential, Audience::Mcp).await
        } else if !state.identity.oauth_server {
            self.verify_key(&state.db, credential).await
        } else {
            Err(Denied::Reason("token_not_accepted", "anonymous"))
        };
        Self::answer(state, client, verified)
    }

    /// SMTP authentication never reuses cached key authority, even when HTTP caching is enabled.
    async fn verify_smtp(
        &self,
        state: &AppState,
        credential: &str,
        client: ClientAddress,
    ) -> Result<Principal, Problem> {
        let verified = match api_keys::parse(credential) {
            Some(mode) => read_key(&state.db, &api_keys::hash(credential), mode)
                .await
                .map(|(principal, _)| principal),
            None => Err(Denied::Reason("token_not_accepted", "anonymous")),
        };
        Self::answer(state, client, verified)
    }

    /// Answers a verification: the principal, or the denial, recorded as a security event.
    fn answer(
        state: &AppState,
        client: ClientAddress,
        verified: Result<Principal, Denied>,
    ) -> Result<Principal, Problem> {
        verified.map_err(|denied| match denied {
            Denied::Reason(reason, actor_kind) => {
                let ip_hash = state.keys.hash_address(&client.as_key());
                record_decision(&Decision {
                    outcome: "denied",
                    reason,
                    actor_kind,
                    workspace: None,
                    method: None,
                    ip_hash: Some(&ip_hash),
                });
                Problem::unauthorized()
            }
            Denied::Problem(problem) => problem,
        })
    }

    async fn verify_key(&self, db: &Database, credential: &str) -> Result<Principal, Denied> {
        let mode =
            api_keys::parse(credential).ok_or(Denied::Reason("unknown_credential", "anonymous"))?;
        let hash = api_keys::hash(credential);
        if !self.ttl.is_zero()
            && let Some(cached) = self.keys.get(&hash).await
            && cached.valid_until > Instant::now()
        {
            return Ok(cached.principal);
        }
        let read_started = Instant::now();
        let (principal, expires_at) = read_key(db, &hash, mode).await?;
        if self.ttl.is_zero() {
            return Ok(principal);
        }
        self.keys
            .insert(
                hash,
                Cached {
                    principal,
                    valid_until: expires_at.map_or(read_started + self.ttl, |at| {
                        (read_started + self.ttl).min(instant_of(at, crate::process::now()))
                    }),
                },
            )
            .await;
        Ok(principal)
    }

    async fn verify_token(&self, state: &AppState, token: &str) -> Result<Principal, Denied> {
        let verified = state
            .identity
            .tokens
            .verify(&state.db, &state.keys, token)
            .await
            .map_err(|error| match error {
                tokens::TokenError::Invalid | tokens::TokenError::NoKey => {
                    Denied::Reason("invalid_token", "workspace_token")
                }
                tokens::TokenError::Db(error) => Denied::Problem(error.into()),
                other => Denied::Problem(Problem::internal(&other)),
            })?;
        let now = crate::process::now();
        let session = self
            .session(&state.db, verified.session, now)
            .await?
            .filter(|row| row.user == verified.user && row.active(now))
            .ok_or(Denied::Reason("session_ended", "workspace_token"))?;
        let workspace = WorkspaceId::trusted(verified.workspace.uuid());
        let standing = self
            .standing(
                &state.db,
                verified.session,
                workspace,
                verified.user,
                &session,
                now,
            )
            .await?
            .ok_or(Denied::Reason("not_a_member", "workspace_token"))?;
        if standing.status != MembershipStatus::Active {
            return Err(Denied::Reason("membership_inactive", "workspace_token"));
        }
        if standing.deleted {
            return Err(Denied::Reason("workspace_deleted", "workspace_token"));
        }
        proof_holds(&standing.enforcing, &session.proof(), now.0)
            .map_err(|refusal| Denied::Reason(refusal.into(), "workspace_token"))?;
        Ok(Principal {
            workspace,
            actor: Actor::User {
                user: verified.user,
                session: verified.session,
            },
            scopes: verified.scopes.intersect(standing.role.scopes()),
            role: standing.role,
            test_mode: standing.test_mode,
            credential: Credential::WorkspaceToken,
        })
    }

    /// An OAuth access token of `audience`: its signature and type, then its grant (live, of the
    /// token's person and workspace), the person's standing, and the workspace's SSO enforcement
    /// against the proof the grant recorded at consent. Its scopes are the token's, narrowed to
    /// the grant's and to the person's current role.
    async fn verify_access(
        &self,
        state: &AppState,
        token: &str,
        audience: Audience,
    ) -> Result<Principal, Denied> {
        let resources = Resources::of(&state.settings.public_api_url);
        let verified = state
            .identity
            .tokens
            .verify_access(&state.db, &state.keys, token, audience, &resources)
            .await
            .map_err(|error| match error {
                tokens::TokenError::Invalid | tokens::TokenError::NoKey => {
                    Denied::Reason("invalid_token", "oauth")
                }
                tokens::TokenError::Db(error) => Denied::Problem(error.into()),
                other => Denied::Problem(Problem::internal(&other)),
            })?;
        let now = crate::process::now();
        let grant = self
            .grant(&state.db, verified.grant, now)
            .await?
            .filter(|row| {
                row.user == verified.user && row.workspace == verified.workspace && row.active(now)
            })
            .ok_or(Denied::Reason("grant_ended", "oauth"))?;
        let workspace = WorkspaceId::trusted(verified.workspace.uuid());
        let standing = self
            .standing_of(
                &state.db,
                (verified.grant.uuid(), workspace.uuid()),
                workspace,
                verified.user,
                instant_of(grant.expires_at, now),
            )
            .await?
            .ok_or(Denied::Reason("not_a_member", "oauth"))?;
        if standing.status != MembershipStatus::Active {
            return Err(Denied::Reason("membership_inactive", "oauth"));
        }
        if standing.deleted {
            return Err(Denied::Reason("workspace_deleted", "oauth"));
        }
        proof_holds(&standing.enforcing, &grant.proof, now.0)
            .map_err(|refusal| Denied::Reason(refusal.into(), "oauth"))?;
        Ok(Principal {
            workspace,
            actor: Actor::OAuth {
                user: verified.user,
                grant: verified.grant,
            },
            scopes: verified
                .scopes
                .intersect(grant.scopes)
                .intersect(standing.role.scopes()),
            role: standing.role,
            test_mode: standing.test_mode,
            credential: Credential::OAuth,
        })
    }

    /// The OAuth grant's row, cached (see the module) and never past the grant's expiry.
    async fn grant(
        &self,
        db: &Database,
        grant: Id<Grant>,
        now: Timestamp,
    ) -> Result<Option<GrantRow>, Denied> {
        if !self.ttl.is_zero()
            && let Some(cached) = self.grants.get(&grant.uuid()).await
            && cached.valid_until > Instant::now()
        {
            return Ok(Some(cached.row));
        }
        let read_started = Instant::now();
        let mut tx = db.begin().await?;
        let row = grants::by_id(&mut tx, grant).await?;
        tx.commit().await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if self.ttl.is_zero() {
            return Ok(Some(row));
        }
        let valid_until = (read_started + self.ttl).min(instant_of(row.expires_at, now));
        self.grants
            .insert(
                grant.uuid(),
                CachedGrant {
                    row: row.clone(),
                    valid_until,
                },
            )
            .await;
        Ok(Some(row))
    }

    /// The session row, cached (see the module).
    async fn session(
        &self,
        db: &Database,
        session: Id<Session>,
        now: Timestamp,
    ) -> Result<Option<SessionRow>, Denied> {
        if !self.ttl.is_zero()
            && let Some(cached) = self.sessions.get(&session.uuid()).await
            && cached.valid_until > Instant::now()
        {
            return Ok(Some(cached.row));
        }
        let read_started = Instant::now();
        let Some(row) = sessions::by_id(db, session)
            .await
            .map_err(|error| Denied::Problem(error.into()))?
        else {
            return Ok(None);
        };
        if self.ttl.is_zero() {
            return Ok(Some(row));
        }
        let valid_until = (read_started + self.ttl)
            .min(instant_of(row.expires_at, now))
            .min(instant_of(row.idle_expires_at, now));
        self.sessions
            .insert(
                session.uuid(),
                CachedSession {
                    row: row.clone(),
                    valid_until,
                },
            )
            .await;
        Ok(Some(row))
    }

    /// The user's standing in the workspace, read through the session, cached (see the module).
    async fn standing(
        &self,
        db: &Database,
        session: Id<Session>,
        workspace: WorkspaceId,
        user: Id<User>,
        row: &SessionRow,
        now: Timestamp,
    ) -> Result<Option<Standing>, Denied> {
        let ceiling = instant_of(row.expires_at, now).min(instant_of(row.idle_expires_at, now));
        self.standing_of(
            db,
            (session.uuid(), workspace.uuid()),
            workspace,
            user,
            ceiling,
        )
        .await
    }

    /// The user's standing in the workspace, cached under `key` (the session or grant it was read
    /// through, and the workspace) for at most the TTL and never past `ceiling` (see the module).
    async fn standing_of(
        &self,
        db: &Database,
        key: (Uuid, Uuid),
        workspace: WorkspaceId,
        user: Id<User>,
        ceiling: Instant,
    ) -> Result<Option<Standing>, Denied> {
        if !self.ttl.is_zero()
            && let Some(cached) = self.memberships.get(&key).await
            && cached.valid_until > Instant::now()
        {
            return Ok(Some(cached.standing));
        }
        let read_started = Instant::now();
        let Some(standing) = read_standing(db, workspace, user)
            .await
            .map_err(|error| Denied::Problem(error.into()))?
        else {
            return Ok(None);
        };
        if self.ttl.is_zero() {
            return Ok(Some(standing));
        }
        let valid_until = (read_started + self.ttl).min(ceiling);
        self.memberships
            .insert(
                key,
                CachedStanding {
                    standing: standing.clone(),
                    valid_until,
                },
            )
            .await;
        Ok(Some(standing))
    }
}

/// Why a credential proves nothing: a reason for the security event and the kind of actor it
/// claimed to be, or an infrastructure failure answered as it is.
enum Denied {
    Reason(&'static str, &'static str),
    Problem(Problem),
}

impl From<sqlx::Error> for Denied {
    fn from(error: sqlx::Error) -> Self {
        Self::Problem(error.into())
    }
}

async fn read_key(
    db: &Database,
    hash: &[u8],
    mode: api_keys::KeyMode,
) -> Result<(Principal, Option<Timestamp>), Denied> {
    let mut tx = db.begin().await?;
    let key = api_keys::find(&mut tx, hash)
        .await?
        .ok_or(Denied::Reason("unknown_credential", "api_key"))?;
    tx.commit().await?;
    let now = crate::process::now();
    if key.revoked_at.is_some() || key.expires_at.is_some_and(|expires| expires <= now) {
        return Err(Denied::Reason("api_key_revoked", "api_key"));
    }
    let workspace = WorkspaceId::trusted(key.workspace);
    let mut tx = db.begin_in(workspace).await?;
    let standing = api_keys::standing(&mut tx, workspace, key.created_by)
        .await?
        .ok_or(Denied::Reason("not_a_member", "api_key"))?;
    tx.commit().await?;
    if !standing.active {
        return Err(Denied::Reason("membership_inactive", "api_key"));
    }
    if standing.workspace_deleted {
        return Err(Denied::Reason("workspace_deleted", "api_key"));
    }
    if standing.workspace_mode != mode.workspace_mode() {
        return Err(Denied::Reason("mode_mismatch", "api_key"));
    }
    let granted = ScopeSet::parse(key.scopes.iter().map(String::as_str)).unwrap_or_default();
    Ok((
        Principal {
            workspace,
            actor: Actor::ApiKey {
                key: key.id,
                created_by: key.created_by,
            },
            scopes: granted.intersect(standing.role.scopes()),
            role: standing.role,
            test_mode: mode == api_keys::KeyMode::Test,
            credential: Credential::ApiKey,
        },
        key.expires_at,
    ))
}

/// Reads `user`'s membership in `workspace`, the workspace's mode and deletion, and its enforcing
/// SSO connections, inside the workspace; `None` without a membership row.
///
/// # Errors
///
/// The database failed.
pub async fn read_standing(
    db: &Database,
    workspace: WorkspaceId,
    user: Id<User>,
) -> Result<Option<Standing>, sqlx::Error> {
    let mut tx = db.begin_in(workspace).await?;
    let row = sqlx::query!(
        r#"SELECT m.role, m.status, w.mode, w.deleted_at IS NOT NULL AS "deleted!",
                  coalesce(array_agg(c.id) FILTER (WHERE c.id IS NOT NULL), '{}') AS "connections!",
                  coalesce(array_agg(c.policy_version) FILTER (WHERE c.id IS NOT NULL), '{}') AS "versions!"
             FROM memberships m
             JOIN workspaces w ON w.id = m.workspace_id
             LEFT JOIN sso_connections c ON c.workspace_id = m.workspace_id AND c.enforced AND c.status = 'active'
            WHERE m.workspace_id = $1 AND m.user_id = $2
            GROUP BY m.role, m.status, w.mode, w.deleted_at"#,
        workspace.uuid(),
        user.uuid()
    )
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row.and_then(|row| {
        Some(Standing {
            user,
            role: row.role.parse().ok()?,
            status: row.status.parse().ok()?,
            test_mode: row.mode == "test",
            deleted: row.deleted,
            enforcing: row
                .connections
                .into_iter()
                .zip(row.versions)
                .map(|(connection, policy_version)| Enforcement {
                    connection,
                    policy_version,
                })
                .collect(),
        })
    }))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Middleware: verifies the bearer credential when one is present and leaves the principal in
/// the request's extensions, for idempotency, rate limits and handlers; without one, resolves the
/// session cookie when present and leaves the [`SignedIn`] browser instead. A request without
/// either continues; a handler that needs one answers `401`.
pub async fn authenticate(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    if let Some(credential) = bearer(request.headers()) {
        let client = ClientAddress::of(
            request.headers(),
            request.extensions(),
            state.limits.trust_forwarded_for(),
            state.limits.trusted_proxy_ips(),
        );
        let verified = if request.uri().path() == "/v1/smtp_authorization" {
            state
                .authority
                .verify_smtp(&state, credential, client)
                .await
        } else {
            state.authority.verify(&state, credential, client).await
        };
        match verified {
            Ok(principal) => {
                request.extensions_mut().insert(principal);
            }
            Err(problem) => return problem.into_response(),
        }
    } else {
        match sessions::resolve(&state.db, &state.keys, request.headers()).await {
            Ok(Some(signed_in)) => {
                request.extensions_mut().insert::<SignedIn>(signed_in);
            }
            Ok(None) => {}
            Err(error) => return Problem::from(error).into_response(),
        }
    }
    next.run(request).await
}

impl FromRequestParts<AppState> for Principal {
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(principal) = parts.extensions.get::<Self>() {
            return Ok(*principal);
        }
        let credential = bearer(&parts.headers).ok_or_else(Problem::unauthorized)?;
        let client = ClientAddress::of(
            &parts.headers,
            &parts.extensions,
            state.limits.trust_forwarded_for(),
            state.limits.trusted_proxy_ips(),
        );
        let principal = if parts.uri.path() == "/v1/smtp_authorization" {
            state
                .authority
                .verify_smtp(state, credential, client)
                .await?
        } else {
            state.authority.verify(state, credential, client).await?
        };
        parts.extensions.insert(principal);
        Ok(principal)
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::testing::TestDb;

    #[tokio::test]
    async fn default_authority_observes_revocation_on_every_replica() {
        let test = TestDb::new().await;
        let workspace = test.workspace("revocation").await;
        let key = test.api_key(&workspace, ScopeSet::all()).await;
        let replicas = [Authority::new(), Authority::new()];
        let opted_in = Authority::with_ttl(Duration::from_secs(60));
        for authority in replicas.iter().chain(std::iter::once(&opted_in)) {
            assert!(authority.verify_key(&test.app, &key).await.is_ok());
        }
        let hash = api_keys::hash(&key);
        let mut tx = test.system.begin_in(workspace.id).await.unwrap();
        sqlx::query(
            "UPDATE api_keys SET revoked_at = now() WHERE workspace_id = $1 AND secret_hash = $2",
        )
        .bind(workspace.id.uuid())
        .bind(&hash)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        for authority in &replicas {
            assert!(authority.verify_key(&test.app, &key).await.is_err());
            assert!(authority.keys.get(&hash).await.is_none());
        }
        assert!(
            opted_in.verify_key(&test.app, &key).await.is_ok(),
            "positive TTL explicitly accepts bounded reuse"
        );
        opted_in.forget(&hash).await;
        assert!(opted_in.verify_key(&test.app, &key).await.is_err());
        assert_eq!(
            Authority::with_ttl(Duration::from_secs(999)).ttl,
            MAX_AUTHORITY_TTL
        );
    }

    #[tokio::test]
    async fn optional_key_cache_never_extends_a_keys_own_expiry() {
        let test = TestDb::new().await;
        let workspace = test.workspace("expiry").await;
        let key = test.api_key(&workspace, ScopeSet::all()).await;
        let hash = api_keys::hash(&key);
        let mut tx = test.system.begin_in(workspace.id).await.unwrap();
        sqlx::query("UPDATE api_keys SET expires_at = now() + interval '2 seconds' WHERE workspace_id = $1 AND secret_hash = $2")
            .bind(workspace.id.uuid()).bind(&hash).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let authority = Authority::with_ttl(Duration::from_secs(60));
        assert!(authority.verify_key(&test.app, &key).await.is_ok());
        let mut cached = authority.keys.get(&hash).await.unwrap();
        assert!(cached.valid_until <= Instant::now() + Duration::from_secs(2));
        // Expired cached authority must fall through to the current row even while Moka's
        // longer storage TTL retains the entry.
        cached.valid_until = Instant::now();
        authority.keys.insert(hash.clone(), cached).await;
        let mut tx = test.system.begin_in(workspace.id).await.unwrap();
        sqlx::query("UPDATE api_keys SET expires_at = now() - interval '1 second' WHERE workspace_id = $1 AND secret_hash = $2")
            .bind(workspace.id.uuid()).bind(&hash).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert!(authority.verify_key(&test.app, &key).await.is_err());
    }
}
