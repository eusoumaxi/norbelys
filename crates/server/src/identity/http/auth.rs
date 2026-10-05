//! `auth`: the sign-in operations. `GET /v1/auth/config` tells the dashboard which methods are
//! offered; `POST /v1/auth/challenges` starts any method; `POST /v1/auth/sessions` finishes an
//! email code, a sign-in link or a passkey and sets the session cookie; `POST /v1/auth/tokens`
//! mints a workspace token from the cookie. The redirect methods finish at the one
//! `GET /v1/auth/callback` (`identity::ceremonies`, `identity::oidc`).

use axum::extract::{FromRequestParts as _, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use webauthn_rs::prelude::PublicKeyCredential;

use super::{
    CLEAR_CEREMONY, MembershipList, ceremony_secret, first_memberships, set_cookies,
    sign_in_refused, welcome,
};
use crate::delivery::accept;
use crate::domain::email::EmailAddress;
use crate::domain::identity::{AuthMethod, Lasting, may_make, proof_holds, token_scopes};
use crate::domain::ids::{Challenge, Id, Session, Workspace, WorkspaceId};
use crate::domain::scope::MembershipRole;
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::Json;
use crate::http::ratelimit::{Allowance, ClientAddress, Policy};
use crate::identity::authority::{Decision, read_standing, record_decision};
use crate::identity::captcha::{self, Verdict};
use crate::identity::sessions::{self, NewSession, SignInGuard, SignedIn};
use crate::identity::tokens::{Grant, TokenError};
use crate::identity::users::{self, UserObject};
use crate::identity::{codes, oidc, passkeys, sso};
use crate::problem::{ApiResult, Code, Problem};

/// The routes of `auth` (the callback is `identity::ceremonies`').
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(config))
        .routes(routes!(create_challenge))
        .routes(routes!(create_session))
        .routes(routes!(create_token))
}

/// A sign-in method, or the start of a passkey registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChallengeMethod {
    /// A one-time code and link sent by email: the default, and how a new user signs up.
    EmailCode,
    /// A discoverable passkey: no email needed.
    Passkey,
    /// Registering a passkey for the signed-in user.
    PasskeyRegistration,
    /// An OpenID Connect provider offered to everyone (Google).
    Oidc,
    /// The SSO connection of the workspace that proved the address's domain.
    Sso,
}

/// The captcha the dashboard shows on the email-code challenge.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct CaptchaObject {
    /// `turnstile`, `hcaptcha` or `recaptcha`.
    pub provider: captcha::Provider,
    /// The public site key of its widget.
    pub site_key: String,
    /// The action the widget declares.
    pub action: &'static str,
}

/// What the dashboard needs to offer sign-in.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ConfigObject {
    /// The sign-in methods offered.
    pub methods: Vec<ChallengeMethod>,
    /// The OpenID Connect providers offered to everyone, by name (`google`).
    pub oidc_providers: Vec<String>,
    /// The captcha of the email-code challenge, when one is configured.
    pub captcha: Option<CaptchaObject>,
}

/// Read the sign-in configuration.
///
/// The methods offered, the OpenID Connect providers and the captcha's provider and site key,
/// so the dashboard's widget and the server's check always agree.
#[utoipa::path(
    get,
    path = "/auth/config",
    tag = "Dashboard",
    operation_id = "auth.config",
    responses((status = 200, description = "The sign-in configuration.", body = ConfigObject)),
    security(())
)]
async fn config(State(app): State<AppState>) -> Json<ConfigObject> {
    let identity = &app.identity;
    let redirect = identity.providers.redirect().is_some();
    let oidc_providers = if redirect {
        identity.providers.names()
    } else {
        Vec::new()
    };
    let mut methods = vec![ChallengeMethod::EmailCode, ChallengeMethod::Passkey];
    if !oidc_providers.is_empty() {
        methods.push(ChallengeMethod::Oidc);
    }
    if redirect {
        methods.push(ChallengeMethod::Sso);
    }
    Json(ConfigObject {
        methods,
        oidc_providers,
        captcha: identity.captcha.as_ref().map(|captcha| CaptchaObject {
            provider: captcha.provider(),
            site_key: captcha.site_key().to_owned(),
            action: captcha::ACTION,
        }),
    })
}

/// The body of `POST /auth/challenges`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateChallenge {
    /// The method.
    #[garde(skip)]
    method: ChallengeMethod,
    /// `email_code` and `sso`: the address.
    #[garde(length(chars, min = 3, max = 254))]
    email: Option<String>,
    /// `email_code`, when a captcha is configured: the widget's token.
    #[garde(length(min = 1, max = 8192))]
    captcha_token: Option<String>,
    /// `oidc`: the provider's name (`google`).
    #[garde(length(min = 1, max = 64))]
    provider: Option<String>,
    /// `oidc` and `sso`: link the identity to the signed-in user instead of signing in (needs the
    /// session cookie and its CSRF token).
    #[garde(skip)]
    link: Option<bool>,
    /// `oidc` and `sso`: the dashboard path to come back to (relative; `/` by default).
    #[garde(length(min = 1, max = 512))]
    return_to: Option<String>,
}

/// A started challenge.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ChallengeObject {
    pub id: Id<Challenge>,
    pub method: ChallengeMethod,
    /// When it can no longer be finished.
    pub expires_at: Timestamp,
    /// `oidc` and `sso`: where to send the browser.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_url: Option<String>,
    /// The passkey methods: the options for `navigator.credentials` (`create` for a
    /// registration, `get` for a sign-in).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub options: Option<serde_json::Value>,
}

fn captcha_failed(detail: &str) -> Problem {
    Problem::new(Code::CaptchaFailed, detail)
}

/// The answer to a started challenge: `201`, the ceremony cookie, and the rate-limit headers.
fn started(object: ChallengeObject, cookie: &str, allowances: &[Allowance]) -> ApiResult<Response> {
    let mut response = (StatusCode::CREATED, axum::Json(object)).into_response();
    set_cookies(&mut response, &[cookie.to_owned()])?;
    for allowance in allowances {
        allowance.write(response.headers_mut());
    }
    Ok(response)
}

/// Start a sign-in, a passkey registration or an identity link.
///
/// An email code answers the same for every syntactically valid address and sends the code and a
/// link to it; a passkey answers the browser's request options; `oidc` and `sso` answer the
/// provider's authorization URL (`sso` routes by the address's domain, and answers `404` when no
/// workspace proved it). Every challenge sets the ceremony cookie that binds it to this browser.
#[utoipa::path(
    post,
    path = "/auth/challenges",
    tag = "Dashboard",
    operation_id = "challenges.create",
    request_body(content = CreateChallenge, example = json!({"method": "email_code", "email": "ada@example.com", "captcha_token": "0.xY…"})),
    responses(
        (status = 201, description = "The challenge, with the ceremony cookie.", body = ChallengeObject),
        (status = 401, description = "A registration or link without a signed-in session."),
        (status = 403, description = "The request does not come from the dashboard, carries a bearer credential (`session_required`), or is a registration or link from an operator's impersonation session."),
        (status = 404, description = "`sso`: no workspace proved the address's domain."),
        (status = 409, description = "A link beyond the account's 10 identities (`invalid_state`)."),
        (status = 422, description = "The body is invalid, or the captcha failed (`captcha_failed`)."),
        (status = 429, description = "Too many challenges for this address or client (`rate_limited`)."),
        (status = 503, description = "Mail cannot be sent, or the identity provider cannot be reached."),
    ),
    security(())
)]
async fn create_challenge(
    State(app): State<AppState>,
    _guard: SignInGuard,
    client: ClientAddress,
    mut parts: Parts,
    Json(body): Json<CreateChallenge>,
) -> ApiResult<Response> {
    let email = body
        .email
        .as_deref()
        .map(|email| {
            EmailAddress::parse(email)
                .map_err(|error| Problem::invalid_field("/email", "format", error.to_string()))
        })
        .transpose()?;
    let link = body.link.unwrap_or(false);
    match body.method {
        ChallengeMethod::EmailCode => {
            let email = email.ok_or_else(|| {
                Problem::invalid_field("/email", "required", "An email code needs `email`.")
            })?;
            email_code(&app, client, &email, body.captcha_token.as_deref()).await
        }
        ChallengeMethod::Passkey => {
            let mut tx = app.db.begin().await?;
            let (ceremony, options) =
                passkeys::start_authentication(&mut tx, &app.keys, &app.identity.webauthn).await?;
            tx.commit().await?;
            started(
                ChallengeObject {
                    id: ceremony.id,
                    method: body.method,
                    expires_at: ceremony.expires_at,
                    authorization_url: None,
                    options: serde_json::to_value(options).ok(),
                },
                &ceremony.cookie,
                &[],
            )
        }
        ChallengeMethod::PasskeyRegistration => {
            let signed_in = SignedIn::from_request_parts(&mut parts, &app).await?;
            // A passkey signs in long after the session: never registered by an impersonation.
            if !may_make(signed_in.row.method, Lasting::Passkey) {
                return Err(Problem::forbidden(
                    "An impersonation session cannot register a passkey in the person's name.",
                ));
            }
            let mut tx = app.db.begin().await?;
            let user = users::read(&mut tx, signed_in.user)
                .await?
                .ok_or_else(Problem::unauthorized)?;
            let display = user.name.clone().unwrap_or_else(|| user.email.clone());
            let (ceremony, options) = passkeys::start_registration(
                &mut tx,
                &app.keys,
                &app.identity.webauthn,
                user.id,
                &user.email,
                &display,
            )
            .await?;
            tx.commit().await?;
            started(
                ChallengeObject {
                    id: ceremony.id,
                    method: body.method,
                    expires_at: ceremony.expires_at,
                    authorization_url: None,
                    options: serde_json::to_value(options).ok(),
                },
                &ceremony.cookie,
                &[],
            )
        }
        ChallengeMethod::Oidc | ChallengeMethod::Sso => {
            let link_to = if link {
                let signed_in = SignedIn::from_request_parts(&mut parts, &app).await?;
                // A linked identity signs in long after the session: never linked by an
                // impersonation.
                if !may_make(signed_in.row.method, Lasting::IdentityLink) {
                    return Err(Problem::forbidden(
                        "An impersonation session cannot link an identity to the person's account.",
                    ));
                }
                let mut tx = app.db.begin().await?;
                let linked = oidc::linked_identities(&mut tx, signed_in.user, None).await?;
                tx.commit().await?;
                if linked >= oidc::MAX_IDENTITIES {
                    return Err(Problem::invalid_state(
                        "An account links at most 10 identities: unlink one first.",
                    ));
                }
                Some(signed_in.user)
            } else {
                None
            };
            let return_to = oidc::return_path(body.return_to.as_deref())?;
            let connection = match body.method {
                ChallengeMethod::Sso => {
                    let email = email.as_ref().ok_or_else(|| {
                        Problem::invalid_field("/email", "required", "`sso` needs `email`.")
                    })?;
                    Some(route(&app, email).await?)
                }
                _ => None,
            };
            let via = match &connection {
                Some(connection) => oidc::Via::Sso(connection),
                None => oidc::Via::Named(body.provider.as_deref().ok_or_else(|| {
                    Problem::invalid_field("/provider", "required", "`oidc` needs `provider`.")
                })?),
            };
            let mut tx = app.db.begin().await?;
            let (ceremony, url) = oidc::start(
                &mut tx,
                &app.keys,
                &app.identity.providers,
                &app.identity.fetcher,
                &oidc::StartRequest {
                    via,
                    link_to,
                    return_to: &return_to,
                    login_hint: email.as_ref().map(EmailAddress::as_str),
                },
            )
            .await?;
            tx.commit().await?;
            started(
                ChallengeObject {
                    id: ceremony.id,
                    method: body.method,
                    expires_at: ceremony.expires_at,
                    authorization_url: Some(url),
                    options: None,
                },
                &ceremony.cookie,
                &[],
            )
        }
    }
}

/// The SSO connection `email`'s domain routes to (see `identity::sso`).
async fn route(app: &AppState, email: &EmailAddress) -> ApiResult<sso::SignInConnection> {
    let unrouted = || Problem::not_found("SSO connection for this address's domain");
    let mut tx = app.db.begin().await?;
    let routed = sso::route(&mut tx, &email.domain()).await?;
    tx.commit().await?;
    let (workspace, connection) = routed.ok_or_else(unrouted)?;
    let mut tx = app.db.begin_in(workspace).await?;
    let found = sso::for_sign_in(&mut tx, &app.keys, workspace, connection).await?;
    tx.commit().await?;
    found.filter(|found| found.active).ok_or_else(unrouted)
}

/// The email-code challenge: the captcha, the rate limits, then the code and its mail.
async fn email_code(
    app: &AppState,
    client: ClientAddress,
    email: &EmailAddress,
    captcha_token: Option<&str>,
) -> ApiResult<Response> {
    let degraded = match &app.identity.captcha {
        None => false,
        Some(captcha) => {
            let token = captcha_token
                .ok_or_else(|| captcha_failed("Solve the captcha: `captcha_token` is required."))?;
            match captcha.verify(token, client.0).await {
                Verdict::Valid => false,
                Verdict::Invalid => {
                    return Err(captcha_failed(
                        "The captcha token is invalid, expired or already used; solve it again.",
                    ));
                }
                Verdict::Unavailable(_) => true,
            }
        }
    };
    let (by_email, by_address) = if degraded {
        (Policy::CodeEmailDegraded, Policy::CodeAddressDegraded)
    } else {
        (Policy::CodeEmail, Policy::CodeAddress)
    };
    let allowances = [
        app.limits.check(by_email, email.key().as_bytes())?,
        app.limits.check(by_address, client.as_key().as_bytes())?,
    ];
    let dashboard = app
        .identity
        .settings
        .dashboard()
        .ok_or_else(|| Problem::internal(&"no dashboard origin is configured"))?;
    let ip_hash = app.keys.hash_address(&client.as_key());
    let mut tx = app.db.begin().await?;
    let challenge = codes::start(&mut tx, &app.keys, email, Some(&ip_hash), dashboard).await?;
    tx.commit().await?;
    accept::wake(&app.db).await;
    started(
        ChallengeObject {
            id: challenge.challenge,
            method: ChallengeMethod::EmailCode,
            expires_at: challenge.expires_at,
            authorization_url: None,
            options: None,
        },
        &challenge.cookie,
        &allowances,
    )
}

/// The body of `POST /auth/sessions`: one of three forms.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateSession {
    /// The challenge: with `code` (an email code) or `credential` (a passkey), from the browser
    /// that started it.
    #[garde(skip)]
    #[schema(value_type = Option<String>, example = "chl_0190f8a2b4c87a10b6d2e4f6a8c0e2f4")]
    challenge_id: Option<Id<Challenge>>,
    /// The 6-digit code from the mail.
    #[garde(length(min = 1, max = 16))]
    code: Option<String>,
    /// The sign-in link's token, from any browser: with `email`.
    #[garde(length(min = 1, max = 128))]
    token: Option<String>,
    /// With `token`: the address the link page named.
    #[garde(length(chars, min = 3, max = 254))]
    email: Option<String>,
    /// The authenticator's assertion (`PublicKeyCredential` as JSON).
    #[garde(skip)]
    #[schema(value_type = Option<Object>)]
    credential: Option<serde_json::Value>,
}

/// A new session, as a sign-in answers it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SessionSummary {
    pub id: Id<Session>,
    /// How it authenticated.
    pub auth_method: AuthMethod,
    /// Its absolute expiry.
    pub expires_at: Timestamp,
}

/// A completed sign-in.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SignedInObject {
    /// The user (created on a first sign-in).
    pub user: UserObject,
    /// The session the cookie carries.
    pub session: SessionSummary,
    /// The session's CSRF token, for the `X-CSRF-Token` header of every change.
    pub csrf_token: String,
    /// The user's workspaces.
    pub memberships: MembershipList,
}

/// What a finish proved, before the session.
enum Proven {
    /// An address, by a code or a link.
    Email(EmailAddress),
    /// A user, by a passkey.
    User(Id<crate::domain::ids::User>),
}

/// Finish a sign-in.
///
/// With `challenge_id` and `code` (the email code, from the browser that asked), `token` and
/// `email` (the sign-in link, from any browser), or `challenge_id` and `credential` (a passkey).
/// The first sign-in creates the user. A fresh session cookie is set; every refusal is the same
/// `401`.
#[utoipa::path(
    post,
    path = "/auth/sessions",
    tag = "Dashboard",
    operation_id = "sessions.create",
    request_body(content = CreateSession, example = json!({"challenge_id": "chl_0190f8a2b4c87a10b6d2e4f6a8c0e2f4", "code": "042317"})),
    responses(
        (status = 201, description = "Signed in: the user, the session (its cookie is set) and the memberships.", body = SignedInObject),
        (status = 401, description = "The sign-in could not be completed (one answer for every reason)."),
        (status = 403, description = "The request does not come from the dashboard, or carries a bearer credential (`session_required`)."),
        (status = 422, description = "The body is none of the three forms."),
        (status = 429, description = "Too many sign-in attempts from this client (`rate_limited`)."),
    ),
    security(())
)]
async fn create_session(
    State(app): State<AppState>,
    _guard: SignInGuard,
    client: ClientAddress,
    headers: HeaderMap,
    Json(body): Json<CreateSession>,
) -> ApiResult<Response> {
    let allowance = app
        .limits
        .check(Policy::SignIn, client.as_key().as_bytes())?;
    let ip_hash = app.keys.hash_address(&client.as_key());
    let refuse = |reason: &'static str, method: AuthMethod| {
        record_decision(&Decision {
            outcome: "denied",
            reason,
            actor_kind: "anonymous",
            workspace: None,
            method: Some(method),
            ip_hash: Some(&ip_hash),
        });
        sign_in_refused()
    };
    let mut tx = app.db.begin().await?;
    let (proven, method) = match (
        &body.challenge_id,
        &body.code,
        &body.token,
        &body.email,
        &body.credential,
    ) {
        (Some(challenge), Some(code), None, None, None) => {
            let Some(secret) = ceremony_secret(&headers) else {
                return Err(refuse("no_ceremony", AuthMethod::EmailCode));
            };
            let email = codes::finish_code(&mut tx, &app.keys, *challenge, &secret, code).await?;
            (email.map(Proven::Email), AuthMethod::EmailCode)
        }
        (None, None, Some(token), Some(email), None) => {
            let email = EmailAddress::parse(email).ok();
            let proven = match email {
                Some(email) => codes::finish_link(&mut tx, &app.keys, token, &email).await?,
                None => None,
            };
            (proven.map(Proven::Email), AuthMethod::EmailCode)
        }
        (Some(challenge), None, None, None, Some(credential)) => {
            let credential: PublicKeyCredential = serde_json::from_value(credential.clone())
                .map_err(|_| {
                    Problem::invalid_field(
                        "/credential",
                        "format",
                        "The credential is not a WebAuthn assertion (`PublicKeyCredential`).",
                    )
                })?;
            let Some(secret) = ceremony_secret(&headers) else {
                return Err(refuse("no_ceremony", AuthMethod::Passkey));
            };
            let user = passkeys::finish_authentication(
                &mut tx,
                &app.keys,
                &app.identity.webauthn,
                *challenge,
                &secret,
                &credential,
            )
            .await?;
            (user.map(Proven::User), AuthMethod::Passkey)
        }
        _ => {
            return Err(Problem::invalid_field(
                "",
                "invalid",
                "Give `challenge_id` with `code`, `token` with `email`, or `challenge_id` with `credential`.",
            ));
        }
    };
    let Some(proven) = proven else {
        // A wrong code counts as an attempt and a passkey ceremony is spent: both are committed.
        tx.commit().await?;
        return Err(refuse("not_proven", method));
    };
    let (user, created) = match proven {
        Proven::Email(email) => {
            let (user, created) = users::find_or_create(&mut tx, &email).await?;
            (user, created.then_some(email))
        }
        Proven::User(user) => (
            users::standing(&mut tx, user)
                .await?
                .ok_or_else(|| refuse("no_user", method))?,
            None,
        ),
    };
    if !user.active {
        tx.commit().await?;
        return Err(refuse("user_suspended", method));
    }
    // This sign-in created the account (signing up is the first sign-in): welcome its person.
    let welcomed = match &created {
        Some(email) => welcome(&app, &mut tx, user.id, email).await?,
        None => false,
    };
    let issued = sessions::create(
        &mut tx,
        &app.keys,
        &NewSession {
            user: user.id,
            method,
            sso: None,
            authenticated_at: None,
            ip_hash: Some(ip_hash.clone()),
            user_agent: headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        },
    )
    .await?;
    users::seen(&mut tx, user.id).await?;
    crate::db::set_user(&mut tx, user.id.uuid()).await?;
    let memberships = first_memberships(&mut tx, user.id).await?;
    let object = users::read(&mut tx, user.id)
        .await?
        .ok_or_else(Problem::unauthorized)?;
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
        workspace: None,
        method: Some(method),
        ip_hash: Some(&ip_hash),
    });
    let mut response = (
        StatusCode::CREATED,
        axum::Json(SignedInObject {
            user: object,
            session: SessionSummary {
                id: issued.id,
                auth_method: method,
                expires_at: issued.expires_at,
            },
            csrf_token: issued.csrf_token,
            memberships,
        }),
    )
        .into_response();
    set_cookies(&mut response, &[issued.cookie, CLEAR_CEREMONY.to_owned()])?;
    allowance.write(response.headers_mut());
    Ok(response)
}

/// The body of `POST /auth/tokens`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateToken {
    /// The workspace the token acts in; the user must be an active member.
    #[garde(skip)]
    #[schema(value_type = String, example = "ws_0190f8a2b4c87a10b6d2e4f6a8c0e2f4")]
    workspace_id: Id<Workspace>,
}

/// A minted workspace token.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct TokenObject {
    /// `nbs_…`, for `Authorization: Bearer`.
    pub token: String,
    /// Always `Bearer`.
    pub token_type: &'static str,
    /// When it expires (five minutes after minting).
    pub expires_at: Timestamp,
    pub workspace_id: Id<Workspace>,
    /// The user's role there.
    pub role: MembershipRole,
    /// The scopes it carries.
    pub scopes: Vec<String>,
}

/// Mint a workspace token.
///
/// The dashboard's bearer for one workspace, from the session cookie: five minutes, the user's
/// role's scopes. In a workspace that enforces single sign-on, the session must be proven through
/// its connection under its current policy within the last 24 hours.
#[utoipa::path(
    post,
    path = "/auth/tokens",
    tag = "Dashboard",
    operation_id = "tokens.create",
    request_body(content = CreateToken, example = json!({"workspace_id": "ws_0190f8a2b4c87a10b6d2e4f6a8c0e2f4"})),
    responses(
        (status = 201, description = "The token.", body = TokenObject),
        (status = 401, description = "No signed-in session."),
        (status = 403, description = "No CSRF token or dashboard origin, a bearer credential (`session_required`), or the workspace's SSO enforcement is not satisfied."),
        (status = 404, description = "No such workspace for this user."),
        (status = 429, description = "Too many tokens for this session (`rate_limited`)."),
        (status = 503, description = "No signing key exists yet."),
    ),
    security(("session" = []))
)]
async fn create_token(
    State(app): State<AppState>,
    signed_in: SignedIn,
    Json(body): Json<CreateToken>,
) -> ApiResult<Response> {
    let allowance = app
        .limits
        .check(Policy::TokenMint, signed_in.session.uuid().as_bytes())?;
    let workspace = WorkspaceId::trusted(body.workspace_id.uuid());
    let standing = read_standing(&app.db, workspace, signed_in.user)
        .await?
        .filter(|standing| {
            standing.status == crate::domain::identity::MembershipStatus::Active
                && !standing.deleted
        })
        .ok_or_else(|| Problem::not_found("workspace"))?;
    if proof_holds(
        &standing.enforcing,
        &signed_in.row.proof(),
        crate::process::now().0,
    )
    .is_err()
    {
        return Err(Problem::forbidden(
            if signed_in.row.method == AuthMethod::BreakGlass {
                "A break-glass session acts only in the workspace whose single sign-on it stands in for."
            } else {
                "This workspace requires signing in through its SSO connection (within the last 24 hours)."
            },
        ));
    }
    // A break-glass session's token repairs the workspace and reaches none of its product data.
    let scopes = token_scopes(signed_in.row.method, standing.role);
    let minted = app
        .identity
        .tokens
        .mint(
            &app.db,
            &app.keys,
            &Grant {
                user: signed_in.user,
                session: signed_in.session,
                workspace: body.workspace_id,
                role: standing.role,
                scopes,
            },
        )
        .await
        .map_err(|error| match error {
            TokenError::NoKey => {
                tracing::error!("no signing key exists: run `norbelys-server admin keys rotate`");
                Problem::unavailable(60)
            }
            TokenError::Db(error) => error.into(),
            other => Problem::internal(&other),
        })?;
    sqlx::query!(
        "UPDATE sessions SET active_workspace_id = $2 WHERE id = $1 AND active_workspace_id IS DISTINCT FROM $2",
        signed_in.session.uuid(),
        workspace.uuid(),
    )
    .execute(app.db.pool())
    .await?;
    let mut response = (
        StatusCode::CREATED,
        axum::Json(TokenObject {
            token: minted.token,
            token_type: "Bearer",
            expires_at: minted.expires_at,
            workspace_id: body.workspace_id,
            role: standing.role,
            scopes: scopes.to_strings(),
        }),
    )
        .into_response();
    allowance.write(response.headers_mut());
    Ok(response)
}
