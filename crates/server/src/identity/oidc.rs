//! OpenID Connect: signing in through Google (and any issuer configured the same way), through a
//! workspace's SSO connection, and linking an external identity to a signed-in user.
//!
//! # The flow
//!
//! `POST /v1/auth/challenges` with `oidc` (a named provider), `sso` (an email whose domain routes
//! to a connection) or `oidc`/`sso` with `link: true` (from a signed-in session) starts a ceremony
//! of kind `oidc`, `sso` or `identity_link`. Its sealed state holds the upstream (the provider, or
//! the workspace, connection and policy version), the issuer, the nonce, the PKCE verifier (RFC
//! 7636, S256) and the relative path to return to; the provider's `state` is the ceremony's id.
//! The answer is the provider's authorization URL; the browser goes there and comes back to the
//! one `GET /v1/auth/callback`, which finds the ceremony by `state`, requires its browser cookie,
//! and calls [`finish`].
//!
//! The finish exchanges the code at the token endpoint (with the verifier, and the client secret
//! when the client has one) and verifies the ID token with `openidconnect`: the signature against
//! the issuer's key set, the issuer exactly as configured (discovery is matched strictly, which is
//! why Microsoft is reached through tenant-specific issuers rather than `common`), the audience,
//! the expiry and the nonce. A workspace's SSO additionally requests `max_age=86400` and requires a
//! verified `auth_time` within it: that instant, not our clock, becomes the session's
//! `authenticated_at`, so a new session cannot restart the clock on an old provider session.
//! Every request to a provider goes through the bounded fetcher of identity documents. An
//! issuer's discovery document and key set are cached in the process for 10 minutes and read
//! again once when a token's signature does not verify (the provider rotated its keys).
//!
//! # What the identity resolves to
//!
//! `domain::identity::resolve_external`: a linked `(issuer, subject)` signs its user in; an
//! unlinked one creates a user only from an address the provider marks verified and nobody
//! holds; a verified address someone holds is never merged (the person signs in their usual way
//! and links the identity). Through SSO, the connection's policy then decides the membership
//! (`domain::identity::admit_by_sso`): a new member with the default role when provisioning is on
//! and the address's domain is one the connection proved, never a revived tombstone. The re-check
//! at the finish refuses a connection that is gone, inactive or whose policy changed since the
//! start. An identity link requires the browser's current session to be the user who started it,
//! and refuses an identity linked to someone else.
//!
//! Every outcome returns the browser to the ceremony's path: with the session cookie on a
//! sign-in, `linked=true` on a link, or `error` and `error_description` (`access_denied`,
//! `unverified_email`, `needs_link`, `already_linked`, `policy_changed`, `session_required`,
//! `sign_in_failed`).

use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, header};
use moka::future::Cache;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClaimsVerificationError, ClientId, ClientSecret, CsrfToken, IssuerUrl,
    LoginHint, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse as _,
};
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use super::authority::{Decision, record_decision};
use super::ceremonies::{self, CallbackQuery, CeremonyError, Consumed, Kind, Started};
use super::fetch::{FetchError, Fetcher};
use super::memberships;
use super::sessions::{self, NewSession, SsoProof};
use super::sso::{self, SignInConnection};
use super::users;
use crate::crypto::{self, CryptoError, Keys};
use crate::db::{self, Tx};
use crate::delivery::accept;
use crate::domain::email::EmailAddress;
use crate::domain::identity::{self, Admission, AuthMethod, ExternalFacts, Resolution};
use crate::domain::ids::{Id, User, WorkspaceId};
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::ratelimit::ClientAddress;
use crate::problem::Problem;

/// Google's issuer.
const GOOGLE_ISSUER: &str = "https://accounts.google.com";
/// How long an issuer's discovery document and key set are reused.
const METADATA_TTL: Duration = Duration::from_secs(10 * 60);
/// The oldest provider authentication a workspace's SSO accepts.
const SSO_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

/// An identity provider offered to everyone.
#[derive(Clone)]
pub struct Provider {
    /// The issuer, matched exactly.
    pub issuer: String,
    /// Our client id there.
    pub client_id: String,
    /// Our client secret there.
    pub client_secret: Option<SecretString>,
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Provider")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

/// The providers offered to everyone, the one redirect URL, and the issuers' cached metadata.
#[derive(Clone)]
pub struct Providers {
    named: Arc<Vec<(String, Provider)>>,
    redirect: Option<Url>,
    metadata: Cache<String, CoreProviderMetadata>,
}

impl std::fmt::Debug for Providers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Providers")
            .field("named", &self.named)
            .field("redirect", &self.redirect)
            .finish_non_exhaustive()
    }
}

impl Providers {
    /// Google with Norbelys's app when `google` (its client id and secret) is configured, and the
    /// one callback URL every provider returns to (`OAUTH_REDIRECT_URL`).
    #[must_use]
    pub fn new(google: Option<(String, SecretString)>, redirect: Option<Url>) -> Self {
        let named = google
            .map(|(client_id, secret)| {
                (
                    "google".to_owned(),
                    Provider {
                        issuer: GOOGLE_ISSUER.to_owned(),
                        client_id,
                        client_secret: Some(secret),
                    },
                )
            })
            .into_iter()
            .collect();
        Self {
            named: Arc::new(named),
            redirect,
            metadata: Cache::builder()
                .max_capacity(1_000)
                .time_to_live(METADATA_TTL)
                .build(),
        }
    }

    /// The providers of the tests: `named` at its issuer, returning to `redirect`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn for_tests(named: Vec<(String, Provider)>, redirect: Url) -> Self {
        Self {
            named: Arc::new(named),
            redirect: Some(redirect),
            metadata: Cache::builder().max_capacity(16).build(),
        }
    }

    /// The names of the providers offered to everyone.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.named.iter().map(|(name, _)| name.clone()).collect()
    }

    /// The provider named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Provider> {
        self.named
            .iter()
            .find(|(named, _)| named == name)
            .map(|(_, provider)| provider)
    }

    /// The one callback URL, when configured.
    #[must_use]
    pub fn redirect(&self) -> Option<&Url> {
        self.redirect.as_ref()
    }

    /// `issuer`'s discovery document with its key set, cached unless `refresh`.
    async fn metadata(
        &self,
        fetcher: &Fetcher,
        issuer: &str,
        refresh: bool,
    ) -> Result<CoreProviderMetadata, OidcError> {
        if !refresh && let Some(metadata) = self.metadata.get(issuer).await {
            return Ok(metadata);
        }
        let issuer_url = IssuerUrl::new(issuer.to_owned())
            .map_err(|_| OidcError::Discovery("the issuer is not a URL".to_owned()))?;
        let http = http_client(fetcher);
        let metadata = CoreProviderMetadata::discover_async(issuer_url, &http)
            .await
            .map_err(|error| OidcError::Discovery(error.to_string()))?;
        self.metadata
            .insert(issuer.to_owned(), metadata.clone())
            .await;
        Ok(metadata)
    }
}

/// The HTTP client the OpenID Connect library calls: the bounded fetcher.
fn http_client(
    fetcher: &Fetcher,
) -> impl Fn(
    openidconnect::HttpRequest,
) -> std::pin::Pin<
    Box<dyn Future<Output = Result<openidconnect::HttpResponse, FetchError>> + Send>,
> {
    let fetcher = fetcher.clone();
    move |request| {
        let fetcher = fetcher.clone();
        Box::pin(async move { fetcher.execute(request).await })
    }
}

/// Why a provider ceremony failed.
#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    /// No provider by that name, or no callback URL configured.
    #[error("this sign-in method is not configured")]
    Unconfigured,
    /// The issuer's discovery document or keys could not be read.
    #[error("the identity provider could not be reached: {0}")]
    Discovery(String),
    #[error(transparent)]
    Ceremony(#[from] CeremonyError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

/// Where a ceremony's sign-in goes, as its sealed state records it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "via", rename_all = "snake_case")]
enum Upstream {
    /// A provider offered to everyone.
    Named { provider: String },
    /// A workspace's SSO connection, under the policy version at the start.
    Sso {
        workspace: Uuid,
        connection: Uuid,
        policy_version: i32,
    },
}

/// The sealed state of an `oidc`, `sso` or `identity_link` ceremony.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    upstream: Upstream,
    issuer: String,
    nonce: String,
    verifier: String,
    return_to: String,
}

/// Through what a sign-in goes.
#[derive(Debug, Clone, Copy)]
pub enum Via<'a> {
    /// A provider offered to everyone, by name.
    Named(&'a str),
    /// A workspace's SSO connection.
    Sso(&'a SignInConnection),
}

/// A provider ceremony to start.
#[derive(Debug, Clone, Copy)]
pub struct StartRequest<'a> {
    /// Through what.
    pub via: Via<'a>,
    /// For a link: the signed-in user the identity will be linked to.
    pub link_to: Option<Id<User>>,
    /// The dashboard path to return to (relative, checked by the caller).
    pub return_to: &'a str,
    /// The address the person typed, offered to the provider as a hint.
    pub login_hint: Option<&'a str>,
}

/// Starts a provider ceremony (see the module): the ceremony for the browser, and the provider's
/// authorization URL.
///
/// # Errors
///
/// The provider is not configured or cannot be reached, or the database failed.
pub async fn start(
    tx: &mut Tx,
    keys: &Keys,
    providers: &Providers,
    fetcher: &Fetcher,
    request: &StartRequest<'_>,
) -> Result<(Started, String), OidcError> {
    let redirect = providers.redirect().ok_or(OidcError::Unconfigured)?;
    let (upstream, issuer, client_id) = match request.via {
        Via::Named(name) => {
            let provider = providers.get(name).ok_or(OidcError::Unconfigured)?;
            (
                Upstream::Named {
                    provider: name.to_owned(),
                },
                provider.issuer.clone(),
                provider.client_id.clone(),
            )
        }
        Via::Sso(connection) => (
            Upstream::Sso {
                workspace: connection.workspace.uuid(),
                connection: connection.id,
                policy_version: connection.policy_version,
            },
            connection.issuer.clone(),
            connection.client_id.clone(),
        ),
    };
    let metadata = providers.metadata(fetcher, &issuer, false).await?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let nonce = crypto::random_token(24)?;
    let kind = match (request.link_to, request.via) {
        (Some(_), _) => Kind::IdentityLink,
        (None, Via::Named(_)) => Kind::Oidc,
        (None, Via::Sso(_)) => Kind::Sso,
    };
    let state = State {
        upstream,
        issuer,
        nonce: nonce.clone(),
        verifier: verifier.secret().clone(),
        return_to: request.return_to.to_owned(),
    };
    let state = serde_json::to_value(&state).map_err(|_| CeremonyError::Shape)?;
    let started = ceremonies::start_for(tx, keys, kind, request.link_to, &state).await?;
    let client = CoreClient::from_provider_metadata(metadata, ClientId::new(client_id), None)
        .set_redirect_uri(RedirectUrl::from_url(redirect.clone()));
    let id = started.id.to_string();
    let mut authorization = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            move || CsrfToken::new(id.clone()),
            move || Nonce::new(nonce.clone()),
        )
        .add_scope(Scope::new("email".to_owned()))
        .add_scope(Scope::new("profile".to_owned()))
        .set_pkce_challenge(challenge);
    if let Via::Sso(_) = request.via {
        authorization = authorization.set_max_age(SSO_MAX_AGE);
    }
    if let Some(hint) = request.login_hint {
        authorization = authorization.set_login_hint(LoginHint::new(hint.to_owned()));
    }
    let (url, _, _) = authorization.url();
    Ok((started, url.to_string()))
}

/// The external identity a verified ID token proves.
#[derive(Debug, Clone)]
struct External {
    issuer: String,
    subject: String,
    email: Option<EmailAddress>,
    email_verified: bool,
    auth_time: Option<Timestamp>,
}

/// Exchanges `code` and verifies the ID token it brings (see the module).
async fn exchange(
    providers: &Providers,
    fetcher: &Fetcher,
    issuer: &str,
    client_id: &str,
    client_secret: Option<&SecretString>,
    code: &str,
    state: &State,
) -> Result<External, &'static str> {
    let redirect = providers.redirect().ok_or("sign_in_failed")?;
    let metadata = providers
        .metadata(fetcher, issuer, false)
        .await
        .map_err(|_| "sign_in_failed")?;
    let build = |metadata: CoreProviderMetadata| {
        CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(client_id.to_owned()),
            client_secret.map(|secret| ClientSecret::new(secret.expose_secret().to_owned())),
        )
        .set_redirect_uri(RedirectUrl::from_url(redirect.clone()))
    };
    let client = build(metadata);
    let http = http_client(fetcher);
    let response = client
        .exchange_code(AuthorizationCode::new(code.to_owned()))
        .map_err(|_| "sign_in_failed")?
        .set_pkce_verifier(PkceCodeVerifier::new(state.verifier.clone()))
        .request_async(&http)
        .await
        .map_err(|error| {
            tracing::info!(error = %error, "an authorization code was not exchanged");
            "sign_in_failed"
        })?;
    let token = response.id_token().ok_or("sign_in_failed")?;
    let nonce = Nonce::new(state.nonce.clone());
    let claims = match token.claims(&client.id_token_verifier(), &nonce) {
        Ok(claims) => claims.clone(),
        Err(ClaimsVerificationError::SignatureVerification(_)) => {
            // The provider may have rotated its keys since they were cached: read them again once.
            let fresh = providers
                .metadata(fetcher, issuer, true)
                .await
                .map_err(|_| "sign_in_failed")?;
            token
                .claims(&build(fresh).id_token_verifier(), &nonce)
                .map_err(|_| "sign_in_failed")?
                .clone()
        }
        Err(error) => {
            tracing::info!(error = %error, "an ID token was refused");
            return Err("sign_in_failed");
        }
    };
    Ok(External {
        issuer: claims.issuer().to_string(),
        subject: claims.subject().to_string(),
        email: claims
            .email()
            .and_then(|email| EmailAddress::parse(email.as_str()).ok()),
        email_verified: claims.email_verified() == Some(true),
        auth_time: claims
            .auth_time()
            .and_then(|at| jiff::Timestamp::from_second(at.timestamp()).ok())
            .map(Timestamp),
    })
}

/// Where the browser goes when a provider ceremony ends, and the cookies it receives.
#[derive(Debug, Clone)]
pub struct Finished {
    /// The relative location.
    pub location: String,
    /// `Set-Cookie` values (the session's, on a sign-in).
    pub cookies: Vec<String>,
}

/// `path` with `pairs` added to its query.
fn location(path: &str, pairs: &[(&str, &str)]) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    let separator = if path.contains('?') { '&' } else { '?' };
    format!("{path}{separator}{query}")
}

fn failed(state: &State, code: &str, description: &str) -> Finished {
    Finished {
        location: location(
            &state.return_to,
            &[("error", code), ("error_description", description)],
        ),
        cookies: Vec::new(),
    }
}

/// Finishes an `oidc`, `sso` or `identity_link` ceremony the callback consumed (see the module).
///
/// # Errors
///
/// The ceremony's state cannot be read (no return path is known), or the database failed.
pub async fn finish(
    app: &AppState,
    consumed: Consumed,
    query: &CallbackQuery,
    headers: &HeaderMap,
    client: ClientAddress,
) -> Result<Finished, Problem> {
    let state: State = serde_json::from_value(consumed.state)
        .map_err(|_| Problem::bad_request("The ceremony's state is not understood."))?;
    if let Some(error) = &query.error {
        tracing::info!(error = %error, "the identity provider refused the sign-in");
        return Ok(failed(
            &state,
            "access_denied",
            "The identity provider did not complete the sign-in.",
        ));
    }
    let Some(code) = query.code.as_deref() else {
        return Ok(failed(
            &state,
            "sign_in_failed",
            "The provider sent no code.",
        ));
    };
    let identity = &app.identity;
    let (connection, client_id, client_secret) = match &state.upstream {
        Upstream::Named { provider } => {
            let Some(provider) = identity.providers.get(provider) else {
                return Ok(failed(
                    &state,
                    "sign_in_failed",
                    "The provider is no longer offered.",
                ));
            };
            (
                None,
                provider.client_id.clone(),
                provider.client_secret.clone(),
            )
        }
        Upstream::Sso {
            workspace,
            connection,
            policy_version,
        } => {
            let workspace = WorkspaceId::trusted(*workspace);
            let mut tx = app.db.begin_in(workspace).await?;
            let found = sso::for_sign_in(&mut tx, &app.keys, workspace, *connection)
                .await
                .map_err(|error| Problem::internal(&error))?;
            tx.commit().await?;
            let Some(found) = found.filter(|found| found.active && found.issuer == state.issuer)
            else {
                return Ok(failed(
                    &state,
                    "sign_in_failed",
                    "The SSO connection is no longer active.",
                ));
            };
            if found.policy_version != *policy_version {
                return Ok(failed(
                    &state,
                    "policy_changed",
                    "The workspace's SSO policy changed during the sign-in; start again.",
                ));
            }
            (
                Some(found.clone()),
                found.client_id.clone(),
                found.client_secret.clone(),
            )
        }
    };
    let external = match exchange(
        &identity.providers,
        &identity.fetcher,
        &state.issuer,
        &client_id,
        client_secret.as_ref(),
        code,
        &state,
    )
    .await
    {
        Ok(external) => external,
        Err(code) => return Ok(failed(&state, code, "The sign-in could not be verified.")),
    };
    if connection.is_some() {
        let fresh = external.auth_time.is_some_and(|at| {
            let age = crate::process::now().0.duration_since(at.0);
            !age.is_negative() && age.unsigned_abs() < SSO_MAX_AGE
        });
        if !fresh {
            return Ok(failed(
                &state,
                "sign_in_failed",
                "The identity provider's authentication is too old; sign in again.",
            ));
        }
    }
    match consumed.kind {
        Kind::IdentityLink => link(app, &state, consumed.user, &external, headers).await,
        _ => sign_in(app, &state, connection.as_ref(), &external, headers, client).await,
    }
}

/// The most identities one person links (`GET /v1/me` lists them all).
pub const MAX_IDENTITIES: i64 = 10;

/// How many identities `user` has linked, besides `except` (an issuer and subject being linked
/// again, which takes no new place).
///
/// # Errors
///
/// The database failed.
pub async fn linked_identities(
    tx: &mut Tx,
    user: Id<User>,
    except: Option<(&str, &str)>,
) -> Result<i64, sqlx::Error> {
    let (issuer, subject) = except.unzip();
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM identity_links
            WHERE user_id = $1 AND NOT (issuer IS NOT DISTINCT FROM $2 AND subject IS NOT DISTINCT FROM $3)"#,
        user.uuid(),
        issuer,
        subject,
    )
    .fetch_one(&mut **tx)
    .await
}

/// Links `external` to the user who started the ceremony, from their current session.
async fn link(
    app: &AppState,
    state: &State,
    user: Option<Id<User>>,
    external: &External,
    headers: &HeaderMap,
) -> Result<Finished, Problem> {
    let current = sessions::resolve(&app.db, &app.keys, headers).await?;
    let Some(user) = user.filter(|user| {
        current
            .as_ref()
            .is_some_and(|current| current.user == *user)
    }) else {
        return Ok(failed(
            state,
            "session_required",
            "Sign in as the person who started linking, then link again.",
        ));
    };
    // Checked again at the finish: a linked identity signs in long after the session, and an
    // operator's impersonation links none (`domain::identity::may_make`).
    if current.as_ref().is_some_and(|current| {
        !identity::may_make(current.row.method, identity::Lasting::IdentityLink)
    }) {
        return Ok(failed(
            state,
            "impersonation",
            "An impersonation session cannot link an identity to the person's account.",
        ));
    }
    let mut tx = app.db.begin().await?;
    // At most MAX_IDENTITIES per person, counted under the person's row so two links finishing at
    // once cannot both take the last place; relinking an identity already theirs adds none.
    sqlx::query!("SELECT id FROM users WHERE id = $1 FOR UPDATE", user.uuid())
        .fetch_optional(&mut *tx)
        .await?;
    let except = Some((external.issuer.as_str(), external.subject.as_str()));
    if linked_identities(&mut tx, user, except).await? >= MAX_IDENTITIES {
        tx.rollback().await?;
        return Ok(failed(
            state,
            "too_many_identities",
            "An account links at most 10 identities: unlink one, then link again.",
        ));
    }
    let linked = sqlx::query_scalar!(
        r#"INSERT INTO identity_links (issuer, subject, user_id, email) VALUES ($1, $2, $3, $4)
           ON CONFLICT (issuer, subject) DO UPDATE SET last_used_at = now()
           RETURNING user_id AS "user_id: Id<User>""#,
        external.issuer,
        external.subject,
        user.uuid(),
        external.email.as_ref().map(EmailAddress::as_str),
    )
    .fetch_one(&mut *tx)
    .await?;
    if linked != user {
        tx.rollback().await?;
        return Ok(failed(
            state,
            "already_linked",
            "This identity is linked to another account.",
        ));
    }
    tx.commit().await?;
    Ok(Finished {
        location: location(&state.return_to, &[("linked", "true")]),
        cookies: Vec::new(),
    })
}

/// Signs the person in: resolves the identity, admits them through SSO, creates the session.
async fn sign_in(
    app: &AppState,
    state: &State,
    connection: Option<&SignInConnection>,
    external: &External,
    headers: &HeaderMap,
    client: ClientAddress,
) -> Result<Finished, Problem> {
    let ip_hash = app.keys.hash_address(&client.as_key());
    let method = if connection.is_some() {
        AuthMethod::Sso
    } else {
        AuthMethod::Oidc
    };
    let refuse =
        |reason: &'static str, code: &str, description: &str| -> Result<Finished, Problem> {
            record_decision(&Decision {
                outcome: "denied",
                reason,
                actor_kind: "anonymous",
                workspace: connection.map(|connection| connection.workspace),
                method: Some(method),
                ip_hash: Some(&ip_hash),
            });
            Ok(failed(state, code, description))
        };
    let mut tx = app.db.begin().await?;
    let linked = sqlx::query_scalar!(
        r#"SELECT user_id AS "user_id: Id<User>" FROM identity_links WHERE issuer = $1 AND subject = $2 FOR UPDATE"#,
        external.issuer,
        external.subject,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let verified_email = external.email.as_ref().filter(|_| external.email_verified);
    let holder = match verified_email {
        Some(email) => users::by_email(&mut tx, email).await?,
        None => None,
    };
    let resolution = identity::resolve_external(ExternalFacts {
        linked_user: linked.map(|user| user.uuid()),
        email_verified: verified_email.is_some(),
        email_holder: holder.as_ref().map(|holder| holder.id.uuid()),
    });
    let (user, welcomed) = match (resolution, verified_email) {
        (Resolution::SignIn(user), _) => {
            sqlx::query!(
                "UPDATE identity_links SET last_used_at = now(), email = coalesce($3, email)
                  WHERE issuer = $1 AND subject = $2",
                external.issuer,
                external.subject,
                external.email.as_ref().map(EmailAddress::as_str),
            )
            .execute(&mut *tx)
            .await?;
            (Id::<User>::from_uuid(user), false)
        }
        (Resolution::CreateUser, Some(email)) => {
            let (created, new) = users::find_or_create(&mut tx, email).await?;
            if !new {
                return refuse(
                    "needs_link",
                    "needs_link",
                    "An account already uses this address: sign in your usual way, then link this identity.",
                );
            }
            sqlx::query!(
                "INSERT INTO identity_links (issuer, subject, user_id, email) VALUES ($1, $2, $3, $4)",
                external.issuer,
                external.subject,
                created.id.uuid(),
                email.as_str(),
            )
            .execute(&mut *tx)
            .await?;
            // This sign-in created the account (signing up is the first sign-in): welcome its
            // person.
            let welcomed = super::http::welcome(app, &mut tx, created.id, email).await?;
            (created.id, welcomed)
        }
        (Resolution::NeedsLink, _) => {
            return refuse(
                "needs_link",
                "needs_link",
                "An account already uses this address: sign in your usual way, then link this identity.",
            );
        }
        (Resolution::Unverified | Resolution::CreateUser, _) => {
            return refuse(
                "unverified_email",
                "unverified_email",
                "The identity provider did not vouch for an email address.",
            );
        }
    };
    let standing = users::standing(&mut tx, user).await?;
    if !standing.is_some_and(|standing| standing.active) {
        return refuse(
            "user_suspended",
            "sign_in_failed",
            "The sign-in could not be completed.",
        );
    }
    if let Some(connection) = connection {
        admit(&mut tx, connection, user, verified_email).await?;
    }
    let issued = sessions::create(
        &mut tx,
        &app.keys,
        &NewSession {
            user,
            method,
            sso: connection.map(|connection| SsoProof {
                connection: connection.id,
                policy_version: connection.policy_version,
            }),
            authenticated_at: if connection.is_some() {
                external.auth_time
            } else {
                None
            },
            ip_hash: Some(ip_hash.clone()),
            user_agent: headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        },
    )
    .await?;
    users::seen(&mut tx, user).await?;
    tx.commit().await?;
    if welcomed {
        accept::wake(&app.db).await;
    }
    for ended in &issued.ended {
        app.authority.forget_session(*ended).await;
    }
    record_decision(&Decision {
        outcome: "signed_in",
        reason: "verified",
        actor_kind: "user",
        workspace: connection.map(|connection| connection.workspace),
        method: Some(method),
        ip_hash: Some(&ip_hash),
    });
    Ok(Finished {
        location: location(&state.return_to, &[("signed_in", "true")]),
        cookies: vec![issued.cookie],
    })
}

/// Just-in-time membership through `connection` (see the module); the transaction switches to
/// the connection's workspace for it.
async fn admit(
    tx: &mut Tx,
    connection: &SignInConnection,
    user: Id<User>,
    verified_email: Option<&EmailAddress>,
) -> Result<(), Problem> {
    db::set_workspace(tx, connection.workspace).await?;
    let existing = sqlx::query_scalar!(
        "SELECT status FROM memberships WHERE workspace_id = $1 AND user_id = $2",
        connection.workspace.uuid(),
        user.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .and_then(|status| status.parse().ok());
    let domain_proved = verified_email.is_some_and(|email| {
        connection
            .verified_domains
            .iter()
            .any(|domain| *domain == email.domain())
    });
    if identity::admit_by_sso(existing, connection.jit_provisioning, domain_proved)
        == Admission::Create
        && let Some(memberships::Admitted::Joined(membership)) = memberships::admit(
            tx,
            connection.workspace,
            user,
            connection.default_role,
            "sso_jit",
            false,
        )
        .await?
    {
        super::audit::record(
            tx,
            connection.workspace,
            super::audit::AuditActor::System,
            super::audit::Action::MemberJoined,
            Some(membership.to_string()),
            serde_json::json!({ "source": "sso_jit", "role": connection.default_role.as_str() }),
            None,
        )
        .await?;
    }
    Ok(())
}

/// The check of a return path: relative to the dashboard (one leading `/`), never another
/// origin, at most 512 characters.
///
/// # Errors
///
/// `422 validation_failed` at `/return_to`.
pub fn return_path(path: Option<&str>) -> Result<String, Problem> {
    let path = path.unwrap_or("/");
    let relative = path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && path.len() <= 512
        && !path.chars().any(char::is_control);
    if relative {
        Ok(path.to_owned())
    } else {
        Err(Problem::invalid_field(
            "/return_to",
            "format",
            "The return path is relative to the dashboard: it starts with one `/`.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A return path stays on the dashboard: an absolute URL, a protocol-relative one or a
    /// backslash trick that browsers read as another host is refused, so a sign-in can never
    /// send the browser to another site.
    #[test]
    fn return_paths_stay_on_the_dashboard() {
        assert_eq!(return_path(None).unwrap(), "/");
        assert_eq!(
            return_path(Some("/settings?tab=sso")).unwrap(),
            "/settings?tab=sso"
        );
        for refused in [
            "https://evil.test/",
            "//evil.test",
            "/\\evil.test",
            "settings",
            "/a\nb",
        ] {
            assert!(return_path(Some(refused)).is_err(), "{refused}");
        }
    }

    /// Outcomes are added to the return path's own query, whatever it already has.
    #[test]
    fn outcomes_join_the_return_paths_query() {
        assert_eq!(location("/", &[("signed_in", "true")]), "/?signed_in=true");
        assert_eq!(
            location("/x?a=1", &[("error", "needs_link")]),
            "/x?a=1&error=needs_link"
        );
    }
}
