//! The authorization server's endpoints (see the parent module for the profile and the routes).
//!
//! # The code grant, end to end
//!
//! 1. The MCP client sends the browser to `GET /oauth/authorize` with its `client_id`,
//!    `redirect_uri`, an S256 `code_challenge`, `resource` (the MCP server), `scope` and `state`.
//!    The client and its redirect URI are checked first; while either is unknown nothing is
//!    redirected (the error is shown instead, RFC 6749 §4.1.2.1), afterwards every refusal goes
//!    back to the redirect URI with `error`, `state` and `iss`.
//! 2. The accepted request is sealed with the deployment key (bound to its purpose, valid ten
//!    minutes) and the browser goes to the dashboard's consent page with it, so no server-side
//!    state exists for a request nobody approves.
//! 3. The dashboard reads it (`GET /oauth/consent?request=…`: the client's name, its redirect
//!    host, the scopes) and posts the person's decision with the chosen workspace
//!    (`POST /oauth/consent`, cookie-authorised with the CSRF token). Approval creates the grant and
//!    a code; the answer is the URL the dashboard sends the browser to: the redirect URI with
//!    `code`, `state` and `iss` (RFC 9207), or with `error=access_denied`.
//! 4. The client exchanges the code at `POST /oauth/token` with its `code_verifier`.
//!
//! # The device grant
//!
//! The command-line client starts it at `POST /oauth/device_authorization` and shows the user
//! code; the person opens the dashboard's `/activate` page, which reads the code
//! (`GET /oauth/consent?user_code=…`) and posts the decision with the chosen workspace
//! (`POST /oauth/consent` with `user_code`); the client polls `POST /oauth/token` meanwhile.
//!
//! # Consent checks
//!
//! Approval needs a live session (any sign-in method but an owner's break-glass recovery), an
//! active membership of the chosen workspace, and, in a workspace that enforces single sign-on,
//! a session proven through its connection within the last 24 hours, exactly as minting a
//! workspace token does. The grant's scopes are what was asked, narrowed to the person's role.

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use super::clients::{self, Client};
use super::grants::{self, GrantRow, NewGrant};
use super::{OAuthError, Params, Refusal, no_store};
use crate::db::Tx;
use crate::domain::identity::{AuthMethod, Lasting, MembershipStatus, may_make, proof_holds};
use crate::domain::ids::{Grant, Id, Workspace, WorkspaceId};
use crate::domain::oauth::{
    self, Audience, Poll, RefreshRefusal, Resources, audience_for, grantable, pkce_holds,
    requested_scopes, valid_challenge,
};
use crate::domain::scope::ScopeSet;
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::{Json, Query};
use crate::http::ratelimit::{ClientAddress, Policy};
use crate::identity::audit::{self, Action, AuditActor};
use crate::identity::authority::read_standing;
use crate::identity::sessions::SignedIn;
use crate::identity::tokens::AccessGrant;
use crate::problem::{ApiResult, Problem};

/// The grant type of a device code poll (RFC 8628 §3.4).
const DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// The purpose a sealed consent request is bound to.
const CONSENT_CONTEXT: &[u8] = b"oauth.consent";

fn resources(app: &AppState) -> Resources {
    Resources::of(&app.settings.public_api_url)
}

/// The dashboard's origin, where the consent and activation pages are.
fn dashboard(app: &AppState) -> Result<url::Url, Problem> {
    app.identity
        .settings
        .dashboard()
        .cloned()
        .ok_or_else(|| Problem::internal(&"no dashboard origin is configured"))
}

/// `GET /.well-known/oauth-authorization-server`: the server's metadata (RFC 8414).
pub async fn server_metadata(State(app): State<AppState>) -> Response {
    let api = resources(&app).api;
    axum::Json(serde_json::json!({
        "issuer": api,
        "authorization_endpoint": format!("{api}/oauth/authorize"),
        "token_endpoint": format!("{api}/oauth/token"),
        "device_authorization_endpoint": format!("{api}/oauth/device_authorization"),
        "revocation_endpoint": format!("{api}/oauth/revoke"),
        "jwks_uri": format!("{api}/.well-known/jwks.json"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token", DEVICE_CODE],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none", "client_secret_basic"],
        "revocation_endpoint_auth_methods_supported": ["none", "client_secret_basic"],
        "scopes_supported": grantable().to_strings(),
        "authorization_response_iss_parameter_supported": true,
        "client_id_metadata_document_supported": true,
    }))
    .into_response()
}

/// `GET /.well-known/oauth-protected-resource/mcp`: the MCP server's metadata (RFC 9728), which
/// tells an MCP client where to obtain a token.
pub async fn resource_metadata(State(app): State<AppState>) -> Response {
    let resources = resources(&app);
    axum::Json(serde_json::json!({
        "resource": resources.mcp,
        "resource_name": "Norbelys",
        "authorization_servers": [resources.api],
        "scopes_supported": grantable().to_strings(),
        "bearer_methods_supported": ["header"],
    }))
    .into_response()
}

/// An authorization request accepted by `/oauth/authorize`, sealed into the consent page's URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ConsentRequest {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    state: Option<String>,
    resource: String,
    scopes: Vec<String>,
    /// When the request stops being accepted (Unix seconds).
    expires_at: i64,
}

impl ConsentRequest {
    fn seal(&self, app: &AppState) -> Result<String, Problem> {
        let json = serde_json::to_vec(self).map_err(|error| Problem::internal(&error))?;
        let sealed = app
            .keys
            .seal(&json, CONSENT_CONTEXT)
            .map_err(|error| Problem::internal(&error))?;
        Ok(URL_SAFE_NO_PAD.encode(sealed))
    }

    /// Opens a sealed request that has not expired.
    fn open(app: &AppState, sealed: &str) -> Result<Self, Problem> {
        let refused = || {
            Problem::invalid_field(
                "/request",
                "invalid",
                "The consent request is unknown or expired; start again from the application.",
            )
        };
        let bytes = URL_SAFE_NO_PAD.decode(sealed).map_err(|_| refused())?;
        let json = app
            .keys
            .open(&bytes, CONSENT_CONTEXT)
            .map_err(|_| refused())?;
        let request: Self = serde_json::from_slice(&json).map_err(|_| refused())?;
        if request.expires_at <= crate::process::now().0.as_second()
            || !oauth::valid_redirect_uri(&request.redirect_uri)
        {
            return Err(refused());
        }
        Ok(request)
    }
}

/// `redirect_uri` with `params` added to its query, and `iss` (RFC 9207).
fn redirect_to(redirect_uri: &str, issuer: &str, params: &[(&str, Option<&str>)]) -> String {
    // All callers validate before sealing or opening consent; fail closed if that invariant
    // ever regresses. An invalid target must never reach a Location header or the dashboard.
    if !oauth::valid_redirect_uri(redirect_uri) {
        return String::new();
    }
    let Ok(mut url) = url::Url::parse(redirect_uri) else {
        return String::new();
    };
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in params {
            if let Some(value) = value {
                query.append_pair(name, value);
            }
        }
        query.append_pair("iss", issuer);
    }
    url.into()
}

fn see_other(location: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response()
}

/// `GET /oauth/authorize`: starts an authorization code grant (see the module).
pub async fn authorize(
    State(app): State<AppState>,
    client: ClientAddress,
    RawQuery(query): RawQuery,
) -> Response {
    if let Err(problem) = app
        .limits
        .check(Policy::OauthAddress, client.as_key().as_bytes())
    {
        return problem.into_response();
    }
    match start_authorization(&app, query.as_deref().unwrap_or_default()).await {
        Ok(response) => response,
        Err(refusal) => refusal.into_response(),
    }
}

async fn start_authorization(app: &AppState, query: &str) -> Result<Response, Refusal> {
    let params = Params::parse(query.as_bytes())?;
    let client_id = params.require("client_id")?;
    let redirect_uri = params.require("redirect_uri")?;
    if client_id.len() > 2048 || !oauth::valid_redirect_uri(redirect_uri) {
        return Err(OAuthError::invalid_request("The client or callback URI is invalid.").into());
    }
    let client = clients::find(
        &app.db,
        &app.identity.fetcher,
        client_id,
        Some(redirect_uri),
    )
    .await
    .map_err(|error| match error {
        clients::ClientError::Db(error) => Refusal::from(error),
        clients::ClientError::Document(reason) => {
            OAuthError::invalid_request(format!("The client is not usable: {reason}.")).into()
        }
    })?
    .ok_or_else(|| OAuthError::invalid_request("The client is unknown."))?;
    if !oauth::redirect_allowed(&client.redirect_uris, redirect_uri) {
        return Err(OAuthError::invalid_request(
            "The redirect URI is not registered for this client.",
        )
        .into());
    }
    let issuer = resources(app).api;
    let state = params.get("state");
    let refuse = |error: &str, description: &str| {
        Ok(see_other(&redirect_to(
            redirect_uri,
            &issuer,
            &[
                ("error", Some(error)),
                ("error_description", Some(description)),
                ("state", state),
            ],
        )))
    };
    if params.get("response_type") != Some("code") {
        return refuse(
            "unsupported_response_type",
            "Only `response_type=code` is supported.",
        );
    }
    let challenge = params.get("code_challenge").unwrap_or_default();
    if params.get("code_challenge_method") != Some("S256") || !valid_challenge(challenge) {
        return refuse(
            "invalid_request",
            "PKCE is required: an S256 `code_challenge` with `code_challenge_method=S256`.",
        );
    }
    let resources = resources(app);
    let Some(Ok(audience)) = params
        .get("resource")
        .map(|resource| audience_for(resource, client.is_cli(), &resources))
    else {
        return refuse(
            "invalid_target",
            "`resource` must be the MCP server's URL, as its protected resource metadata names it.",
        );
    };
    let Ok(scopes) = requested_scopes(params.get("scope")) else {
        return refuse("invalid_scope", "A requested scope is unknown.");
    };
    let request = ConsentRequest {
        client_id: client.client_id.clone(),
        redirect_uri: redirect_uri.to_owned(),
        code_challenge: challenge.to_owned(),
        state: state.map(str::to_owned),
        resource: match audience {
            Audience::Cli => resources.api,
            Audience::Mcp => resources.mcp,
        },
        scopes: scopes.to_strings(),
        expires_at: crate::process::now()
            .plus(oauth::CONSENT_LIFETIME)
            .0
            .as_second(),
    };
    let mut page = dashboard(app)?
        .join("/oauth/consent")
        .map_err(|error| Problem::internal(&error))?;
    page.query_pairs_mut()
        .append_pair("request", &request.seal(app)?);
    Ok(see_other(page.as_str()))
}

/// What the dashboard asks about: a sealed code request, or a device's user code.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsentQuery {
    /// The sealed request of `/oauth/authorize`.
    request: Option<String>,
    /// The user code a person typed on the activation page.
    user_code: Option<String>,
}

/// What the consent or activation page shows before the person decides.
#[derive(Debug, Serialize)]
pub struct ConsentDetails {
    /// The client's id.
    client_id: String,
    /// The client's name.
    client_name: String,
    /// Where the browser returns after a code grant (the redirect URI's host), so a person can
    /// tell a look-alike client from the real one; absent for a device code.
    #[serde(skip_serializing_if = "Option::is_none")]
    redirect_host: Option<String>,
    /// The scopes asked for (the grant narrows them to the person's role).
    scopes: Vec<String>,
    /// The resource the grant is for.
    resource: String,
    /// The user code, as shown on the device, for a device code.
    #[serde(skip_serializing_if = "Option::is_none")]
    user_code: Option<String>,
    /// When the request or the code expires.
    expires_at: Timestamp,
}

/// `GET /oauth/consent`: what the dashboard's consent or activation page shows (a cookie-authorised
/// read).
///
/// # Errors
///
/// `401` without a session, `422` for a malformed or expired request, `404` for a user code that
/// is unknown, expired or decided.
pub async fn consent_details(
    State(app): State<AppState>,
    _signed_in: SignedIn,
    Query(query): Query<ConsentQuery>,
) -> ApiResult<axum::Json<ConsentDetails>> {
    match (query.request, query.user_code) {
        (Some(sealed), None) => {
            let request = ConsentRequest::open(&app, &sealed)?;
            let client = consenting_client(&app, &request).await?;
            Ok(axum::Json(ConsentDetails {
                client_id: client.client_id,
                client_name: client.name,
                redirect_host: url::Url::parse(&request.redirect_uri)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_owned)),
                scopes: request.scopes,
                resource: request.resource,
                user_code: None,
                expires_at: Timestamp(
                    jiff::Timestamp::from_second(request.expires_at).unwrap_or_default(),
                ),
            }))
        }
        (None, Some(typed)) => {
            let mut tx = app.db.begin().await?;
            let device = pending_device(&mut tx, &typed).await?;
            tx.commit().await?;
            let client = clients::find(&app.db, &app.identity.fetcher, &device.client_id, None)
                .await
                .ok()
                .flatten()
                .ok_or_else(|| Problem::not_found("device code"))?;
            Ok(axum::Json(ConsentDetails {
                client_id: client.client_id,
                client_name: client.name,
                redirect_host: None,
                scopes: device.scopes.to_strings(),
                resource: device.resource,
                user_code: Some(oauth::display_user_code(&device.user_code)),
                expires_at: Timestamp(device.state.expires_at),
            }))
        }
        _ => Err(one_of()),
    }
}

fn one_of() -> Problem {
    Problem::invalid_field(
        "",
        "invalid",
        "Name exactly one of `request` (a consent request) and `user_code` (a device's code).",
    )
}

/// The client of a sealed request, still allowed its redirect URI.
async fn consenting_client(app: &AppState, request: &ConsentRequest) -> Result<Client, Problem> {
    clients::find(
        &app.db,
        &app.identity.fetcher,
        &request.client_id,
        Some(&request.redirect_uri),
    )
    .await
    .ok()
    .flatten()
    .filter(|client| oauth::redirect_allowed(&client.redirect_uris, &request.redirect_uri))
    .ok_or_else(|| {
        Problem::invalid_field(
            "/request",
            "invalid",
            "The client no longer allows this redirect; start again from the application.",
        )
    })
}

/// Locks the live, undecided device code a person typed.
async fn pending_device(tx: &mut Tx, typed: &str) -> Result<grants::DeviceRow, Problem> {
    let not_found = || Problem::not_found("device code");
    let code = oauth::normalize_user_code(typed).ok_or_else(not_found)?;
    let device = grants::lock_user_code(tx, &code)
        .await?
        .ok_or_else(not_found)?;
    let state = device.state;
    let live = !state.consumed
        && !state.denied
        && !state.approved
        && crate::process::now().0 < state.expires_at;
    if live { Ok(device) } else { Err(not_found()) }
}

/// The person's decision on a consent request or a device code.
#[derive(Debug, Deserialize, garde::Validate)]
#[serde(deny_unknown_fields)]
pub struct ConsentDecision {
    /// The sealed request of `/oauth/authorize`.
    #[garde(skip)]
    request: Option<String>,
    /// The user code of a device.
    #[garde(skip)]
    user_code: Option<String>,
    /// The workspace the grant acts in; required to approve.
    #[garde(skip)]
    workspace_id: Option<Id<Workspace>>,
    /// True to approve, false to deny.
    #[garde(skip)]
    approve: bool,
}

/// The answer to a decision.
#[derive(Debug, Serialize)]
pub struct ConsentAnswer {
    /// Whether the grant was approved.
    approved: bool,
    /// For a code grant: where the dashboard sends the browser (the client's redirect URI with the
    /// code or the refusal).
    #[serde(skip_serializing_if = "Option::is_none")]
    redirect_to: Option<String>,
    /// The grant created by an approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    grant_id: Option<Id<Grant>>,
}

/// `POST /oauth/consent`: the person approves or denies a consent request or a device code, from
/// the dashboard (cookie, `Origin` and CSRF token; see the module for the checks).
///
/// # Errors
///
/// `401` without a session; `403` without the CSRF token or dashboard origin, for a break-glass
/// session, or when the workspace's SSO enforcement is not satisfied; `404` for a workspace the
/// person is not an active member of, or a user code that is unknown, expired or decided; `422`
/// for a malformed or expired request.
pub async fn consent(
    State(app): State<AppState>,
    signed_in: SignedIn,
    Json(decision): Json<ConsentDecision>,
) -> ApiResult<axum::Json<ConsentAnswer>> {
    let issuer = resources(&app).api;
    match (&decision.request, &decision.user_code) {
        (Some(sealed), None) => {
            let request = ConsentRequest::open(&app, sealed)?;
            let client = consenting_client(&app, &request).await?;
            if !decision.approve {
                return Ok(axum::Json(ConsentAnswer {
                    approved: false,
                    redirect_to: Some(redirect_to(
                        &request.redirect_uri,
                        &issuer,
                        &[
                            ("error", Some("access_denied")),
                            ("error_description", Some("The person denied the request.")),
                            ("state", request.state.as_deref()),
                        ],
                    )),
                    grant_id: None,
                }));
            }
            let asked =
                ScopeSet::parse(request.scopes.iter().map(String::as_str)).unwrap_or_default();
            let (workspace, scopes) = approval(&app, &signed_in, &decision, asked).await?;
            let mut tx = app.db.begin().await?;
            let grant = grants::create(
                &mut tx,
                &NewGrant {
                    client_id: &client.client_id,
                    user: signed_in.user,
                    workspace,
                    resource: &request.resource,
                    scopes,
                    proof: signed_in.row.proof(),
                },
            )
            .await?;
            let code = grants::issue_code(
                &mut tx,
                &app.keys,
                grant,
                &request.redirect_uri,
                &request.code_challenge,
            )
            .await
            .map_err(|error| match error {
                grants::StoreError::Db(error) => Problem::from(error),
                grants::StoreError::Crypto(error) => Problem::internal(&error),
            })?;
            tx.commit().await?;
            Ok(axum::Json(ConsentAnswer {
                approved: true,
                redirect_to: Some(redirect_to(
                    &request.redirect_uri,
                    &issuer,
                    &[("code", Some(&code)), ("state", request.state.as_deref())],
                )),
                grant_id: Some(grant),
            }))
        }
        (None, Some(typed)) => {
            let mut tx = app.db.begin().await?;
            let device = pending_device(&mut tx, typed).await?;
            let grant = if decision.approve {
                let (workspace, scopes) =
                    approval(&app, &signed_in, &decision, device.scopes).await?;
                Some(
                    grants::create(
                        &mut tx,
                        &NewGrant {
                            client_id: &device.client_id,
                            user: signed_in.user,
                            workspace,
                            resource: &device.resource,
                            scopes,
                            proof: signed_in.row.proof(),
                        },
                    )
                    .await?,
                )
            } else {
                None
            };
            grants::decide_device(&mut tx, &device.hash, grant).await?;
            tx.commit().await?;
            Ok(axum::Json(ConsentAnswer {
                approved: grant.is_some(),
                redirect_to: None,
                grant_id: grant,
            }))
        }
        _ => Err(one_of()),
    }
}

/// Checks that the signed-in person may approve in the chosen workspace (see the module) and
/// answers the workspace and the grant's scopes: what was asked, narrowed to their role.
async fn approval(
    app: &AppState,
    signed_in: &SignedIn,
    decision: &ConsentDecision,
    asked: ScopeSet,
) -> Result<(WorkspaceId, ScopeSet), Problem> {
    if !may_make(signed_in.row.method, Lasting::Grant) {
        return Err(Problem::forbidden(
            if signed_in.row.method == AuthMethod::Impersonation {
                "An impersonation session cannot grant an application access in the person's name."
            } else {
                "A break-glass recovery session cannot grant access to an application; sign in normally."
            },
        ));
    }
    let workspace = decision.workspace_id.ok_or_else(|| {
        Problem::invalid_field(
            "/workspace_id",
            "required",
            "Choose the workspace the application acts in.",
        )
    })?;
    let workspace = WorkspaceId::trusted(workspace.uuid());
    let standing = read_standing(&app.db, workspace, signed_in.user)
        .await?
        .filter(|standing| standing.status == MembershipStatus::Active && !standing.deleted)
        .ok_or_else(|| Problem::not_found("workspace"))?;
    if proof_holds(
        &standing.enforcing,
        &signed_in.row.proof(),
        crate::process::now().0,
    )
    .is_err()
    {
        return Err(Problem::forbidden(
            "This workspace requires signing in through its SSO connection (within the last 24 hours).",
        ));
    }
    Ok((
        workspace,
        asked
            .intersect(grantable())
            .intersect(standing.role.scopes()),
    ))
}

/// `POST /oauth/device_authorization`: starts a device grant for the command-line client
/// (RFC 8628 §3.1 and §3.2), at most ten per client address every 15 minutes: the request is
/// anonymous and stores a code.
pub async fn device_authorization(
    State(app): State<AppState>,
    client: ClientAddress,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(problem) = app
        .limits
        .check(Policy::DeviceStart, client.as_key().as_bytes())
    {
        return problem.into_response();
    }
    match start_device(&app, &headers, &body).await {
        Ok(response) => response,
        Err(refusal) => refusal.into_response(),
    }
}

async fn start_device(
    app: &AppState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, Refusal> {
    let params = Params::form(headers, body)?;
    let client = authenticated_client(app, headers, &params).await?;
    if !client.is_cli() {
        return Err(OAuthError::new(
            "unauthorized_client",
            "Only the Norbelys command-line client may use the device authorization grant.",
        )
        .into());
    }
    let resources = resources(app);
    if params
        .get("resource")
        .is_none_or(|resource| audience_for(resource, true, &resources).is_err())
    {
        return Err(OAuthError::new("invalid_target", "`resource` must be the API's URL.").into());
    }
    let scopes = requested_scopes(params.get("scope"))
        .map_err(|_| OAuthError::new("invalid_scope", "A requested scope is unknown."))?;
    let mut tx = app.db.begin().await?;
    let started = grants::start_device(
        &mut tx,
        &app.keys,
        &client.client_id,
        &resources.api,
        scopes,
    )
    .await?;
    tx.commit().await?;
    let activate = dashboard(app)?
        .join("/activate")
        .map_err(|error| Problem::internal(&error))?;
    let user_code = oauth::display_user_code(&started.user_code);
    let mut complete = activate.clone();
    complete
        .query_pairs_mut()
        .append_pair("user_code", &user_code);
    let mut response = axum::Json(serde_json::json!({
        "device_code": started.device_code,
        "user_code": user_code,
        "verification_uri": activate.as_str(),
        "verification_uri_complete": complete.as_str(),
        "expires_in": oauth::DEVICE_LIFETIME.as_secs(),
        "interval": oauth::DEVICE_INTERVAL,
    }))
    .into_response();
    no_store(response.headers_mut());
    Ok(response)
}

/// The client of a protocol request, authenticated by its registered method.
async fn authenticated_client(
    app: &AppState,
    headers: &HeaderMap,
    params: &Params,
) -> Result<Client, Refusal> {
    let unknown = || OAuthError::invalid_client(headers.contains_key(header::AUTHORIZATION));
    let client_id = clients::client_id_of(headers, params.get("client_id")).ok_or_else(unknown)?;
    let client = clients::find(&app.db, &app.identity.fetcher, &client_id, None)
        .await
        .map_err(|error| match error {
            clients::ClientError::Db(error) => Refusal::from(error),
            clients::ClientError::Document(_) => unknown().into(),
        })?
        .ok_or_else(unknown)?;
    clients::authenticate(&client, &app.keys, headers, params.get("client_id"))
        .map_err(|failed| OAuthError::invalid_client(failed.basic))?;
    Ok(client)
}

/// A successful token answer (RFC 6749 §5.1).
#[derive(Debug, Serialize)]
struct Tokens {
    access_token: String,
    token_type: &'static str,
    expires_in: u64,
    refresh_token: String,
    scope: String,
}

/// `POST /oauth/token`: the one token endpoint, for authorization codes, refresh tokens and
/// device codes, at most 30 requests a minute per client (`token_endpoint`), counted before the
/// client authenticates so a guessed secret spends a unit too.
pub async fn token(
    State(app): State<AppState>,
    address: ClientAddress,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(problem) = app
        .limits
        .check(Policy::OauthAddress, address.as_key().as_bytes())
    {
        return problem.into_response();
    }
    let answered = async {
        let params = Params::form(&headers, &body)?;
        if let Some(client_id) = clients::client_id_of(&headers, params.get("client_id")) {
            app.limits
                .check(Policy::TokenEndpoint, client_id.as_bytes())?;
        }
        let client = authenticated_client(&app, &headers, &params).await?;
        match params.require("grant_type")? {
            "authorization_code" => exchange_code(&app, &client, &params).await,
            "refresh_token" => exchange_refresh(&app, &client, &params).await,
            DEVICE_CODE => exchange_device(&app, &client, &params).await,
            _ => Err(OAuthError::new(
                "unsupported_grant_type",
                "The grant types are `authorization_code`, `refresh_token` and the device code.",
            )
            .into()),
        }
    }
    .await;
    match answered {
        Ok(tokens) => {
            let mut response = axum::Json(tokens).into_response();
            no_store(response.headers_mut());
            response
        }
        Err(refusal) => refusal.into_response(),
    }
}

/// Mints an access token and the refresh token `refresh` for `grant`, whose scopes are narrowed
/// to `scopes`, inside `tx`, and commits.
async fn issue(
    app: &AppState,
    mut tx: Tx,
    grant: &GrantRow,
    scopes: ScopeSet,
    parent: Option<&[u8]>,
) -> Result<Tokens, Refusal> {
    let resources = resources(app);
    let audience = audience_for(&grant.resource, grant.client_id == clients::CLI, &resources)
        .map_err(|_| {
            OAuthError::invalid_grant("The grant is not for a resource of this server.")
        })?;
    let refresh_expires = Timestamp(oauth::refresh_expiry(
        crate::process::now().0,
        grant.expires_at.0,
    ));
    let refresh_token =
        grants::issue_refresh(&mut tx, &app.keys, grant.id, parent, refresh_expires).await?;
    grants::touch(&mut tx, grant.id).await?;
    let minted = app
        .identity
        .tokens
        .mint_access(
            &app.db,
            &app.keys,
            audience,
            &resources,
            &AccessGrant {
                grant: grant.id,
                user: grant.user,
                workspace: grant.workspace,
                client_id: grant.client_id.clone(),
                scopes,
            },
        )
        .await?;
    tx.commit().await?;
    Ok(Tokens {
        access_token: minted.token,
        token_type: "Bearer",
        expires_in: oauth::ACCESS_LIFETIME.as_secs(),
        refresh_token,
        scope: scopes.to_strings().join(" "),
    })
}

/// The grant's scopes narrowed to its person's current role, when the grant may still be used:
/// it is live, its person is an active member of a workspace that still exists, and the
/// workspace's SSO enforcement accepts the consenting session's proof.
async fn usable(app: &AppState, grant: &GrantRow) -> Result<ScopeSet, Refusal> {
    let ended = || OAuthError::invalid_grant("The grant has ended; authorize again.");
    if !grant.active(crate::process::now()) {
        return Err(ended().into());
    }
    let standing = read_standing(
        &app.db,
        WorkspaceId::trusted(grant.workspace.uuid()),
        grant.user,
    )
    .await?
    .filter(|standing| standing.status == MembershipStatus::Active && !standing.deleted)
    .ok_or_else(ended)?;
    proof_holds(&standing.enforcing, &grant.proof, crate::process::now().0).map_err(|_| {
        OAuthError::invalid_grant(
            "The workspace requires a recent sign-in through its SSO connection; authorize again.",
        )
    })?;
    Ok(grant.scopes.intersect(standing.role.scopes()))
}

/// Revokes `grant` inside `tx` (recording it in the workspace's audit log), commits, and drops it
/// from this process's authority cache.
async fn revoke_grant(
    app: &AppState,
    mut tx: Tx,
    grant: Id<Grant>,
    actor: AuditActor,
    reason: &'static str,
) -> Result<(), Refusal> {
    if let Some(workspace) = grants::revoke(&mut tx, grant, None).await? {
        crate::db::set_workspace(&mut tx, workspace).await?;
        audit::record(
            &mut tx,
            workspace,
            actor,
            Action::GrantRevoked,
            Some(grant.to_string()),
            serde_json::json!({ "reason": reason }),
            None,
        )
        .await?;
    }
    tx.commit().await?;
    app.authority.forget_grant(grant).await;
    Ok(())
}

async fn exchange_code(
    app: &AppState,
    client: &Client,
    params: &Params,
) -> Result<Tokens, Refusal> {
    let code = params.require("code")?;
    let redirect_uri = params.require("redirect_uri")?;
    let verifier = params.require("code_verifier")?;
    let mut tx = app.db.begin().await?;
    let Some(taken) = grants::take_code(&mut tx, &app.keys, code).await? else {
        return Err(OAuthError::invalid_grant("The code is unknown.").into());
    };
    if taken.replayed {
        // A code presented twice was intercepted, or the first answer was: every token of its
        // grant is suspect (RFC 6749 §4.1.2).
        revoke_grant(app, tx, taken.grant, AuditActor::System, "code_replayed").await?;
        return Err(
            OAuthError::invalid_grant("The code was already used; its grant is revoked.").into(),
        );
    }
    let grant = grants::by_id(&mut tx, taken.grant).await?;
    let refused = match &grant {
        None => Some("The code's grant no longer exists."),
        Some(grant) if grant.client_id != client.client_id => {
            Some("The code was issued to another client.")
        }
        Some(_) if taken.expires_at <= crate::process::now() => Some("The code expired."),
        Some(_) if taken.redirect_uri != redirect_uri => {
            Some("`redirect_uri` is not the one the code was issued to.")
        }
        Some(_) if !pkce_holds(&taken.code_challenge, verifier) => {
            Some("`code_verifier` does not match the code challenge.")
        }
        Some(_) => None,
    };
    let (Some(grant), None) = (grant, refused) else {
        // The code stays consumed: it was presented once.
        tx.commit().await?;
        return Err(OAuthError::invalid_grant(refused.unwrap_or("The code is not valid.")).into());
    };
    let scopes = match usable(app, &grant).await {
        Ok(scopes) => scopes,
        Err(refusal) => {
            tx.commit().await?;
            return Err(refusal);
        }
    };
    issue(app, tx, &grant, scopes, None).await
}

async fn exchange_refresh(
    app: &AppState,
    client: &Client,
    params: &Params,
) -> Result<Tokens, Refusal> {
    let token = params.require("refresh_token")?;
    let mut tx = app.db.begin().await?;
    let Some(mut row) = grants::lock_refresh(&mut tx, &app.keys, token).await? else {
        return Err(OAuthError::invalid_grant("The refresh token is unknown.").into());
    };
    row.state.same_client = row.client_id == client.client_id;
    match oauth::refresh(&row.state, crate::process::now().0) {
        Ok(()) => {}
        Err(RefreshRefusal::Reused) => {
            // The revocation commits before the error is answered: whoever holds the newer
            // token loses it too (RFC 9700 §4.14.2).
            revoke_grant(
                app,
                tx,
                row.grant,
                AuditActor::System,
                "refresh_token_reused",
            )
            .await?;
            return Err(OAuthError::invalid_grant(
                "The refresh token was already used; its grant is revoked.",
            )
            .into());
        }
        Err(refusal) => {
            let description: &'static str = refusal.into();
            return Err(OAuthError::invalid_grant(format!(
                "The refresh token is not valid ({}).",
                description.replace('_', " ")
            ))
            .into());
        }
    }
    let grant = grants::by_id(&mut tx, row.grant)
        .await?
        .ok_or_else(|| OAuthError::invalid_grant("The grant no longer exists."))?;
    let scopes = usable(app, &grant).await?;
    grants::consume_refresh(&mut tx, &row.hash).await?;
    issue(app, tx, &grant, scopes, Some(&row.hash)).await
}

async fn exchange_device(
    app: &AppState,
    client: &Client,
    params: &Params,
) -> Result<Tokens, Refusal> {
    let device_code = params.require("device_code")?;
    let mut tx = app.db.begin().await?;
    let device = grants::lock_device_code(&mut tx, &app.keys, device_code)
        .await?
        .filter(|device| device.client_id == client.client_id)
        .ok_or_else(|| OAuthError::invalid_grant("The device code is unknown."))?;
    let answer = |error: &'static str, description: &'static str| -> Result<Tokens, Refusal> {
        Err(OAuthError::new(error, description).into())
    };
    match oauth::poll(&device.state, crate::process::now().0) {
        Poll::Consumed => answer("invalid_grant", "The device code was already redeemed."),
        Poll::Expired => answer("expired_token", "The device code expired; start again."),
        Poll::Denied => answer("access_denied", "The person denied the request."),
        Poll::SlowDown => {
            grants::record_poll(
                &mut tx,
                &device.hash,
                device
                    .state
                    .interval_seconds
                    .saturating_add(oauth::SLOW_DOWN_STEP),
            )
            .await?;
            tx.commit().await?;
            answer(
                "slow_down",
                "Polled too often: wait 5 more seconds between polls.",
            )
        }
        Poll::Pending => {
            grants::record_poll(&mut tx, &device.hash, device.state.interval_seconds).await?;
            tx.commit().await?;
            answer("authorization_pending", "The person has not decided yet.")
        }
        Poll::Issue => {
            let grant_id = device
                .grant
                .ok_or_else(|| OAuthError::invalid_grant("The device code has no grant."))?;
            let grant = grants::by_id(&mut tx, grant_id)
                .await?
                .ok_or_else(|| OAuthError::invalid_grant("The grant no longer exists."))?;
            let scopes = usable(app, &grant).await?;
            grants::consume_device(&mut tx, &device.hash).await?;
            issue(app, tx, &grant, scopes, None).await
        }
    }
}

/// `POST /oauth/revoke` (RFC 7009): revokes the grant behind a refresh or access token of the
/// authenticated client, with its whole refresh chain. Answers `200` whether or not the token was
/// known, so the endpoint tells nothing about tokens it did not issue. A token issued to another
/// client is refused with `400 invalid_grant` and its grant left as it is: the server checks that
/// the token was issued to the client asking (RFC 7009 §2.1), so one application cannot end
/// another's access.
pub async fn revoke(
    State(app): State<AppState>,
    address: ClientAddress,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(problem) = app
        .limits
        .check(Policy::OauthAddress, address.as_key().as_bytes())
    {
        return problem.into_response();
    }
    let answered: Result<(), Refusal> = async {
        let params = Params::form(&headers, &body)?;
        let client = authenticated_client(&app, &headers, &params).await?;
        let token = params.require("token")?;
        let resources = resources(&app);
        let audience = [Audience::Cli, Audience::Mcp]
            .into_iter()
            .find(|audience| token.starts_with(audience.prefix()));
        let grant = match audience {
            Some(audience) => app
                .identity
                .tokens
                .verify_access(&app.db, &app.keys, token, audience, &resources)
                .await
                .ok()
                .map(|verified| verified.grant),
            None => {
                let mut tx = app.db.begin().await?;
                let grant = grants::grant_of_refresh(&mut tx, &app.keys, token).await?;
                tx.commit().await?;
                grant
            }
        };
        let Some(grant) = grant else {
            return Ok(());
        };
        let mut tx = app.db.begin().await?;
        let Some(row) = grants::by_id(&mut tx, grant).await? else {
            return Ok(());
        };
        if row.client_id != client.client_id {
            return Err(OAuthError::invalid_grant(
                "The token was issued to another client; a client revokes only its own tokens.",
            )
            .into());
        }
        revoke_grant(
            &app,
            tx,
            grant,
            AuditActor::OAuth(grant),
            "revoked_by_client",
        )
        .await?;
        Ok(())
    }
    .await;
    match answered {
        Ok(()) => {
            let mut response = StatusCode::OK.into_response();
            no_store(response.headers_mut());
            response
        }
        Err(refusal) => refusal.into_response(),
    }
}
