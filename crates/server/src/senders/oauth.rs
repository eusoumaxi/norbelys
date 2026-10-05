//! Connecting a mailbox through OAuth: the consent ceremony a connection starts, and what the one
//! `GET /v1/auth/callback` does with its answer.
//!
//! # Start
//!
//! `POST /v1/connections { provider: "google" | "microsoft" }` (or `verify` on a connection whose
//! grant was lost) starts a ceremony of kind `mailbox_oauth`: its sealed state carries the
//! workspace, the provider, the OpenID Connect `nonce`, the PKCE verifier (RFC 7636), the
//! relative path to return to, and either the settings of the connection to create or the
//! connection to reconnect. The answer carries the provider's consent URL, whose `state` is the
//! ceremony's id, and sets the ceremony cookie on the browser that asked; only that browser can
//! finish it.
//!
//! # Finish
//!
//! The callback re-checks what the start assumed: the user still holds `connections:manage` in
//! the workspace (and, for a member reconnecting, owns the connection). It exchanges the code,
//! checks the scopes granted (a granular consent can drop one), and reads the account from the ID
//! token: the issuer and the immutable subject (Google's `sub`, Microsoft's `oid` with the
//! tenant's issuer), with the audience, the expiry and the nonce checked. The token's signature
//! is not checked: it came straight from the provider's token endpoint over TLS, authenticated
//! with our client secret, which OpenID Connect Core 1.0 §3.1.3.7 allows in place of the
//! signature check
//! (<https://openid.net/specs/openid-connect-core-1_0.html#IDTokenValidation>). The mailbox's
//! address comes from the provider itself, never from the token's mutable claims: Gmail's
//! `users.getProfile` (`emailAddress`) or Graph's `GET /me` (`userPrincipalName`, the login SMTP
//! AUTH uses for the same mailbox).
//!
//! A new connection then lands by the restoration order; a reconnect accepts only the **same**
//! subject, so a mailbox is never swapped silently: another account is refused, naming both. The
//! browser is sent back to its return path with `connection_id`, or with `error` and
//! `error_description`.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use norbelys_mail::oauth::{self as provider_oauth, App, AuthorizationRequest};
use norbelys_mail::{gmail, graph};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use uuid::Uuid;

use super::connections::{self, Authorization, NewConnection, Subject};
use super::credentials::{self, Credential, Grant};
use super::identities::IdentityInput;
use super::{Error, Settings, SettingsError, health};
use crate::config::MailArgs;
use crate::crypto::{self, Keys};
use crate::db::Tx;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Connection, Id, QuotaScope, User, WorkspaceId};
use crate::domain::scope::Scope;
use crate::domain::senders::{HealthEvent, Provider, SendWindow, Status};
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::identity::api_keys;
use crate::identity::ceremonies::{self, CallbackQuery, Kind};
use crate::jobs::{self, Queue};
use crate::problem::Problem;

/// How long the provider calls of a callback may take in all.
const BUDGET: std::time::Duration = std::time::Duration::from_secs(25);
/// How much an ID token's expiry may lag our clock.
const SKEW_SECONDS: i64 = 300;

/// Norbelys's OAuth apps at Google and Microsoft.
#[derive(Clone, Debug, Default)]
pub struct Apps {
    google: Option<App>,
    microsoft: Option<(App, String)>,
}

impl Apps {
    /// The apps `args` configure: an app is its client id and secret, and both share the one
    /// redirect URL, which must be `https`.
    ///
    /// # Errors
    ///
    /// An app is half configured, or configured without an `https` redirect URL.
    pub fn from_args(args: &MailArgs) -> Result<Self, SettingsError> {
        let app =
            |name: &str, id: &Option<String>, secret: &Option<SecretString>| match (id, secret) {
                (None, None) => Ok(None),
                (Some(id), Some(secret)) => {
                    let redirect = args
                        .oauth_redirect_url
                        .clone()
                        .filter(|url| url.scheme() == "https")
                        .ok_or_else(|| {
                            SettingsError::Invalid(format!(
                                "the {name} OAuth app needs OAUTH_REDIRECT_URL, an https URL"
                            ))
                        })?;
                    Ok(Some(App {
                        client_id: id.clone(),
                        client_secret: secret.clone(),
                        redirect_uri: redirect,
                    }))
                }
                _ => Err(SettingsError::Invalid(format!(
                    "the {name} OAuth client id and secret are set together"
                ))),
            };
        Ok(Self {
            google: app(
                "Google",
                &args.google_oauth_client_id,
                &args.google_oauth_client_secret,
            )?,
            microsoft: app(
                "Microsoft",
                &args.microsoft_oauth_client_id,
                &args.microsoft_oauth_client_secret,
            )?
            .map(|app| (app, args.microsoft_oauth_tenant.clone())),
        })
    }

    /// Apps for tests, with `app` as both providers' app.
    #[cfg(test)]
    pub(crate) fn for_tests(app: App) -> Self {
        Self {
            google: Some(app.clone()),
            microsoft: Some((app, "common".to_owned())),
        }
    }

    /// The identity provider, the app and the scopes of an OAuth `provider`, when its app is
    /// configured.
    #[must_use]
    pub fn app(
        &self,
        provider: Provider,
    ) -> Option<(provider_oauth::Provider, &App, &'static [&'static str])> {
        match provider {
            Provider::Google => self.google.as_ref().map(|app| {
                (
                    provider_oauth::Provider::Google,
                    app,
                    provider_oauth::GOOGLE_API_SCOPES,
                )
            }),
            Provider::Microsoft => self.microsoft.as_ref().map(|(app, tenant)| {
                (
                    provider_oauth::Provider::Microsoft {
                        tenant: tenant.clone(),
                    },
                    app,
                    provider_oauth::MICROSOFT_GRAPH_SCOPES,
                )
            }),
            Provider::Smtp
            | Provider::Ses
            | Provider::Sendgrid
            | Provider::Mailgun
            | Provider::Norbelys => None,
        }
    }
}

/// What an administrator does when the provider blocked Norbelys's app for an account.
#[must_use]
pub fn admin_steps(provider: Provider, client_id: &str) -> String {
    match provider {
        Provider::Microsoft => format!(
            "A Microsoft 365 administrator blocked the app: they consent to Norbelys (client id {client_id}) for the tenant, then verify the connection."
        ),
        _ => format!(
            "A Google Workspace administrator restricted the app: in the Admin console, Security, Access and data control, API controls, Manage App Access, Configure new app, client id {client_id}, then Trusted (or Specific Google data with the Gmail scopes). Then verify the connection; where Google revoked the tokens, reconnect it once."
        ),
    }
}

/// The settings of a connection a consent will create.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    /// The From addresses; empty for the proven address alone.
    pub identities: Vec<IdentityInput>,
    /// The folders to read.
    pub folders: Vec<String>,
    /// Submissions a UTC day.
    pub daily_limit: i32,
    /// The interval between cold sends, checked and rounded.
    pub send_interval_minutes: Option<i32>,
    /// When campaign mail may be submitted, checked.
    pub send_window: Option<SendWindow>,
    /// The send window's IANA time zone, checked.
    pub timezone: String,
    /// The warm-up stage to start at.
    pub warmup_stage: Option<i16>,
    /// The quota scope of the account, checked at the start and again when connecting.
    pub quota_scope: Option<Id<QuotaScope>>,
}

/// What a consent is for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "intent", rename_all = "snake_case")]
pub enum Intent {
    /// A new connection with these settings.
    Connect { pending: Box<Pending> },
    /// The grant of this connection, lost, comes back.
    Reconnect { connection: Id<Connection> },
}

/// The sealed state of a `mailbox_oauth` ceremony.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CeremonyState {
    workspace: Uuid,
    provider: Provider,
    nonce: String,
    code_verifier: String,
    return_to: String,
    intent: Intent,
}

/// A started consent: the URL for the browser and the cookie that binds the ceremony to it.
#[derive(Debug, Clone)]
pub struct Started {
    pub authorization: Authorization,
    /// The `Set-Cookie` value of the ceremony cookie.
    pub cookie: String,
}

/// Starts a consent at `provider` for `intent`, by `user` of `workspace`, returning to
/// `return_to` (a relative path) and preselecting `login_hint`.
///
/// # Errors
///
/// The provider's app is not configured, or the database refused the ceremony.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is one fact of the ceremony, named at the call"
)]
pub async fn start(
    tx: &mut Tx,
    keys: &Keys,
    settings: &Settings,
    workspace: WorkspaceId,
    user: Id<User>,
    provider: Provider,
    intent: Intent,
    return_to: String,
    login_hint: Option<&str>,
) -> Result<Started, Error> {
    let (identity_provider, app, scopes) = settings.apps.app(provider).ok_or_else(|| {
        Error::invalid(
            "/provider",
            format!(
                "Connecting {} mailboxes is not configured on this deployment.",
                provider.as_str()
            ),
        )
    })?;
    let nonce = crypto::random_token(32)?;
    let code_verifier = crypto::random_token(48)?;
    let state = CeremonyState {
        workspace: workspace.uuid(),
        provider,
        nonce: nonce.clone(),
        code_verifier: code_verifier.clone(),
        return_to,
        intent,
    };
    let state = serde_json::to_value(&state)
        .map_err(|_| Error::invalid("", "The consent's settings are not valid."))?;
    let ceremony = ceremonies::start(tx, keys, Kind::MailboxOauth, user, &state).await?;
    let url = provider_oauth::authorization_url(
        &identity_provider,
        app,
        &AuthorizationRequest {
            state: &ceremony.id.to_string(),
            nonce: &nonce,
            code_verifier: &SecretString::from(code_verifier),
            login_hint,
            scopes,
        },
    )
    .map_err(|error| Error::InvalidState(format!("The consent URL cannot be built: {error}")))?;
    Ok(Started {
        authorization: Authorization {
            url: url.to_string(),
            expires_at: ceremony.expires_at,
        },
        cookie: ceremony.cookie,
    })
}

/// Where the callback sends the browser: its return path with `pairs` added to the query.
fn location(return_to: &str, pairs: &[(&str, &str)]) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    let separator = if return_to.contains('?') { '&' } else { '?' };
    format!("{return_to}{separator}{query}")
}

/// Finishes a `mailbox_oauth` ceremony consumed by the callback for `user`: returns where to
/// send the browser, with `connection_id` or with the error.
///
/// # Errors
///
/// The ceremony's state cannot be read (a problem, since no return path is known), or the
/// database failed.
pub async fn finish(
    app_state: &AppState,
    user: Option<Id<User>>,
    state: serde_json::Value,
    query: &CallbackQuery,
) -> Result<String, Problem> {
    let state: CeremonyState = serde_json::from_value(state)
        .map_err(|_| Problem::bad_request("The ceremony's state is not understood."))?;
    let fail = |code: &str, description: &str| {
        location(
            &state.return_to,
            &[("error", code), ("error_description", description)],
        )
    };
    if let Some(error) = &query.error {
        let description = query
            .error_description
            .as_deref()
            .unwrap_or("The provider did not grant the consent.");
        return Ok(fail(error, description));
    }
    let (Some(code), Some(user)) = (&query.code, user) else {
        return Ok(fail(
            "invalid_request",
            "The provider sent no authorization code.",
        ));
    };
    let workspace = WorkspaceId::trusted(state.workspace);
    let settings = &app_state.settings.senders;
    let Some((identity_provider, app, scopes)) = settings.apps.app(state.provider) else {
        return Ok(fail(
            "unavailable",
            "Connecting this provider is not configured on this deployment.",
        ));
    };

    // The start assumed the user may manage connections here; a membership removed or demoted
    // since then finishes nothing.
    let mut tx = app_state.db.begin_in(workspace).await?;
    let standing = api_keys::standing(&mut tx, workspace, user).await?;
    let owner = match &state.intent {
        Intent::Reconnect { connection } => {
            connections::owner(&mut tx, workspace, *connection).await?
        }
        Intent::Connect { .. } => None,
    };
    tx.commit().await?;
    let allowed = standing.is_some_and(|standing| {
        standing.active
            && !standing.workspace_deleted
            && standing.role.scopes().contains(Scope::ConnectionsManage)
            && (standing.role != crate::domain::scope::MembershipRole::Member
                || owner.is_none()
                || owner == Some(Some(user)))
    });
    if !allowed {
        return Ok(fail(
            "forbidden",
            "You can no longer manage this workspace's connections.",
        ));
    }

    let http = &settings.http;
    let deadline = Instant::now() + BUDGET;
    let tokens = match provider_oauth::exchange(
        http,
        &identity_provider,
        app,
        &SecretString::from(code.clone()),
        &SecretString::from(state.code_verifier.clone()),
        deadline,
    )
    .await
    {
        Ok(tokens) => tokens,
        Err(error) => return Ok(fail("provider_refused", &error.to_string())),
    };
    if let Err(error) = provider_oauth::check_scopes(&tokens.scope, scopes) {
        return Ok(fail(
            "scope_missing",
            &format!("{error}; allow every permission Norbelys asks for."),
        ));
    }
    let subject = match verified_subject(
        state.provider,
        &app.client_id,
        &state.nonce,
        tokens.id_token.as_deref(),
        jiff::Timestamp::now(),
    ) {
        Ok(subject) => subject,
        Err(reason) => return Ok(fail("invalid_id_token", reason)),
    };
    let address = match state.provider {
        Provider::Google => gmail::profile(http, &tokens.access_token, deadline)
            .await
            .map(|profile| profile.email_address),
        _ => graph::me(http, &tokens.access_token, deadline)
            .await
            .map(|me| me.user_principal_name),
    };
    let address = match address.map(|address| EmailAddress::parse(&address)) {
        Ok(Ok(address)) => address,
        Ok(Err(_)) => {
            return Ok(fail(
                "invalid_account",
                "The provider names no usable address for this mailbox.",
            ));
        }
        Err(error) => return Ok(fail("provider_unavailable", &error.to_string())),
    };
    let Some(refresh_token) = tokens.refresh_token else {
        return Ok(fail(
            "provider_refused",
            "The provider gave no refresh token; consent again.",
        ));
    };
    let grant = Credential::OAuth(Grant {
        refresh_token,
        access_token: tokens.access_token,
        expires_at: Timestamp(tokens.expires_at),
        scope: tokens.scope,
    });

    let mut tx = app_state.db.begin_in(workspace).await?;
    let outcome = match &state.intent {
        Intent::Reconnect { connection } => {
            reconnect(
                &mut tx,
                &app_state.keys,
                workspace,
                *connection,
                state.provider,
                &subject,
                &address,
                &grant,
            )
            .await
        }
        Intent::Connect { pending } => {
            let new = NewConnection {
                provider: state.provider,
                account_email: address.as_str().to_owned(),
                subject: Some(subject),
                smtp: None,
                imap: None,
                credential: Some(grant),
                identities: if pending.identities.is_empty() {
                    vec![IdentityInput::address(address.clone(), true)]
                } else {
                    pending.identities.clone()
                },
                folders: pending.folders.clone(),
                webhook_key: None,
                daily_limit: pending.daily_limit,
                send_interval_minutes: pending.send_interval_minutes,
                send_window: pending.send_window.clone(),
                timezone: pending.timezone.clone(),
                warmup_stage: pending.warmup_stage,
                quota_scope: pending.quota_scope,
                created_by: user,
            };
            connections::connect(&mut tx, &app_state.keys, workspace, &new)
                .await
                .map(|landed| landed.id())
        }
    };
    let connection = match outcome {
        Ok(connection) => connection,
        Err(Error::Conflict(detail) | Error::InvalidState(detail)) => {
            return Ok(fail("conflict", &detail));
        }
        Err(Error::Invalid { detail, .. }) => return Ok(fail("invalid_request", &detail)),
        Err(Error::NotFound(what)) => return Ok(fail("not_found", &format!("No such {what}."))),
        Err(
            error @ (Error::Db(_) | Error::Crypto(_) | Error::Credential(_) | Error::Ceremony(_)),
        ) => {
            return Err(Problem::internal(&error));
        }
    };
    tx.commit().await?;
    jobs::wake(&app_state.db, Queue::Maintenance).await;
    Ok(location(
        &state.return_to,
        &[("connection_id", &connection.to_string())],
    ))
}

/// Brings a lost grant back onto `connection`, only for the same account.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is one fact of the reconnect, named at the call"
)]
async fn reconnect(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    provider: Provider,
    subject: &Subject,
    address: &EmailAddress,
    grant: &Credential,
) -> Result<Id<Connection>, Error> {
    let row = sqlx::query!(
        "SELECT provider, status, paused, account_email, account_issuer, account_subject
           FROM connections WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound("connection"))?;
    let status: Status = row.status.parse().map_err(|_| {
        Error::InvalidState("The connection's status is not understood.".to_owned())
    })?;
    if status == Status::Archived || row.provider != provider.as_str() {
        return Err(Error::InvalidState(
            "The connection was archived or changed meanwhile; start again.".to_owned(),
        ));
    }
    let same = row.account_issuer.as_deref() == Some(subject.issuer.as_str())
        && row.account_subject.as_deref() == Some(subject.subject.as_str());
    if !same {
        return Err(Error::Conflict(format!(
            "You signed in as {address}; this connection is {}. Sign in with that account.",
            row.account_email
        )));
    }
    let sealed = credentials::seal(keys, workspace, connection, grant)?;
    sqlx::query_scalar!(
        "SELECT set_connection_credential($1, $2, $3)",
        workspace.uuid(),
        connection.uuid(),
        sealed,
    )
    .fetch_one(&mut **tx)
    .await?;
    health::apply(
        tx,
        workspace,
        connection,
        status,
        row.paused,
        HealthEvent::VerifyRequested,
        None,
    )
    .await?;
    jobs::enqueue(
        tx,
        workspace,
        &super::check::ConnectionCheck { connection },
        None,
    )
    .await?;
    Ok(connection)
}

/// The claims of an ID token that name and bind the account.
#[derive(Deserialize)]
struct Claims {
    iss: String,
    aud: Audience,
    exp: i64,
    nonce: Option<String>,
    sub: String,
    oid: Option<String>,
    tid: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

/// The account an ID token received from `provider`'s token endpoint names, after checking its
/// audience (our `client_id`), expiry (against `now`, with five minutes of skew), `nonce` and
/// issuer: Google's issuer and `sub`; Microsoft's tenant issuer and `oid`, which Microsoft sends
/// only with the `profile` scope and which a token without is refused for.
///
/// # Errors
///
/// Why the token does not prove an account.
pub fn verified_subject(
    provider: Provider,
    client_id: &str,
    nonce: &str,
    id_token: Option<&str>,
    now: jiff::Timestamp,
) -> Result<Subject, &'static str> {
    let token = id_token.ok_or("The provider sent no ID token.")?;
    let payload = token
        .split('.')
        .nth(1)
        .and_then(|part| URL_SAFE_NO_PAD.decode(part.trim_end_matches('=')).ok())
        .ok_or("The ID token is not a JWT.")?;
    let claims: Claims = serde_json::from_slice(&payload)
        .map_err(|_| "The ID token's claims are not understood.")?;
    let audience = match &claims.aud {
        Audience::One(one) => one == client_id,
        Audience::Many(many) => many.iter().any(|one| one == client_id),
    };
    if !audience {
        return Err("The ID token was issued for another client.");
    }
    if claims.exp.saturating_add(SKEW_SECONDS) < now.as_second() {
        return Err("The ID token has expired.");
    }
    if claims.nonce.as_deref() != Some(nonce) {
        return Err("The ID token answers another request.");
    }
    match provider {
        Provider::Microsoft => {
            let tenant = claims.tid.ok_or("The ID token names no tenant.")?;
            let issuer = format!("https://login.microsoftonline.com/{tenant}/v2.0");
            if claims.iss != issuer {
                return Err("The ID token's issuer is not its tenant's.");
            }
            let oid = claims
                .oid
                .ok_or("The ID token carries no `oid`; Norbelys asks for the `profile` scope.")?;
            Ok(Subject {
                issuer,
                subject: oid,
            })
        }
        _ => {
            if claims.iss != "https://accounts.google.com" && claims.iss != "accounts.google.com" {
                return Err("The ID token's issuer is not Google.");
            }
            Ok(Subject {
                issuer: "https://accounts.google.com".to_owned(),
                subject: claims.sub,
            })
        }
    }
}

/// Builds an unsigned ID token with `claims`, as the fake providers of the tests send.
#[cfg(test)]
pub(crate) fn unsigned_id_token(claims: &serde_json::Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap_or_default());
    format!("{header}.{payload}.signature")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{unsigned_id_token, verified_subject};
    use crate::domain::senders::Provider;

    /// An ID token proves an account only when it was issued for our client, has not expired,
    /// answers our nonce and comes from the provider's issuer: Google's `sub`, or a Microsoft
    /// tenant's `oid` under that tenant's issuer. Every other token is refused with its reason,
    /// so a token replayed from another client or another sign-in never binds an account.
    #[test]
    fn id_tokens_bind_only_their_own_account() {
        let now: jiff::Timestamp = "2026-10-01T12:00:00Z".parse().unwrap();
        let exp = now.as_second() + 600;
        let google = json!({"iss": "https://accounts.google.com", "aud": "client", "exp": exp, "nonce": "n", "sub": "108"});
        let subject = verified_subject(
            Provider::Google,
            "client",
            "n",
            Some(&unsigned_id_token(&google)),
            now,
        )
        .unwrap();
        assert_eq!(
            (subject.issuer.as_str(), subject.subject.as_str()),
            ("https://accounts.google.com", "108")
        );
        let microsoft = json!({"iss": "https://login.microsoftonline.com/t1/v2.0", "aud": ["client"], "exp": exp,
                               "nonce": "n", "sub": "pairwise", "oid": "o1", "tid": "t1"});
        let subject = verified_subject(
            Provider::Microsoft,
            "client",
            "n",
            Some(&unsigned_id_token(&microsoft)),
            now,
        )
        .unwrap();
        assert_eq!(
            (subject.issuer.as_str(), subject.subject.as_str()),
            ("https://login.microsoftonline.com/t1/v2.0", "o1")
        );

        let refused = [
            (
                Provider::Google,
                json!({"iss": "https://accounts.google.com", "aud": "other", "exp": exp, "nonce": "n", "sub": "1"}),
            ),
            (
                Provider::Google,
                json!({"iss": "https://accounts.google.com", "aud": "client", "exp": now.as_second() - 400, "nonce": "n", "sub": "1"}),
            ),
            (
                Provider::Google,
                json!({"iss": "https://accounts.google.com", "aud": "client", "exp": exp, "nonce": "other", "sub": "1"}),
            ),
            (
                Provider::Google,
                json!({"iss": "https://evil.example", "aud": "client", "exp": exp, "nonce": "n", "sub": "1"}),
            ),
            (
                Provider::Microsoft,
                json!({"iss": "https://login.microsoftonline.com/t2/v2.0", "aud": "client", "exp": exp, "nonce": "n", "sub": "s", "oid": "o", "tid": "t1"}),
            ),
            (
                Provider::Microsoft,
                json!({"iss": "https://login.microsoftonline.com/t1/v2.0", "aud": "client", "exp": exp, "nonce": "n", "sub": "s", "tid": "t1"}),
            ),
        ];
        for (provider, claims) in refused {
            assert!(
                verified_subject(
                    provider,
                    "client",
                    "n",
                    Some(&unsigned_id_token(&claims)),
                    now
                )
                .is_err(),
                "{claims}"
            );
        }
        assert!(verified_subject(Provider::Google, "client", "n", None, now).is_err());
        assert!(
            verified_subject(Provider::Google, "client", "n", Some("not a token"), now).is_err()
        );
    }
}
