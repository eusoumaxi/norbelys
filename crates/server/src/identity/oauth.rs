//! Norbelys's own OAuth 2.1 authorization server: how MCP clients and the command-line client
//! obtain short-lived access tokens that act for one person in one workspace.
//!
//! # The profile
//!
//! A deliberately small profile, small enough to be tested completely (the decisions are in
//! `domain::oauth`):
//!
//! - **Grant types**: `authorization_code` with PKCE S256 (mandatory) for MCP clients;
//!   `urn:ietf:params:oauth:grant-type:device_code` (RFC 8628) for the command-line client only;
//!   `refresh_token` with rotation and reuse detection for both. Nothing else.
//! - **Clients**: registered clients (the command-line client `norbelys-cli`, public) and MCP
//!   clients identified by a Client ID Metadata Document ([`clients`]). No dynamic registration.
//! - **Resources** (RFC 8707): every authorization names its `resource`: the MCP server
//!   (`<api>/mcp`) for MCP clients, the API itself for the command-line client. The resource
//!   decides the access token: `nbo_` (type `mcp`) for the MCP server, `nbc_` (type `cli`) for the
//!   API; each surface accepts only its own.
//! - **Consent**: the person signs in to the dashboard (any method, the workspace's SSO
//!   enforcement respected), chooses the workspace, sees the client's name and redirect host and
//!   the scopes, and approves. The grant records the client, the person, the workspace, the
//!   resource, the scopes (narrowed to the person's role), and the consenting session's method,
//!   SSO connection and authentication time; it ends 90 days later.
//! - **Tokens**: access tokens live ten minutes (`identity::tokens`); refresh tokens are opaque,
//!   live 30 days and never past their grant, and are rotated on every use ([`grants`]).
//! - **Revocation**: `POST /oauth/revoke` (RFC 7009) and `DELETE /v1/me/grants/{id}` revoke the
//!   grant and its refresh chain; the authority's caches stop honouring its access tokens within
//!   60 seconds (at once in the process that revoked). A client revokes only the tokens issued to
//!   it; another client's token is refused (RFC 7009 §2.1).
//! - **Bounds**: the token endpoint admits 30 requests a minute per client (the `token_endpoint`
//!   rate limit); a person keeps at most 50 live grants, a consent beyond them ending the oldest.
//! - **Off**: a deployment keeps the whole server off while it has not passed its gate
//!   (`OAUTH_SERVER_DISABLED`): none of the routes below exists then, and the MCP server names no
//!   authorization server, so clients use API keys.
//!
//! # Routes
//!
//! | Route | What |
//! |---|---|
//! | `GET /.well-known/oauth-authorization-server` | server metadata (RFC 8414) |
//! | `GET /.well-known/oauth-protected-resource/mcp` | the MCP resource's metadata (RFC 9728) |
//! | `GET /oauth/authorize` | starts a code grant; redirects the browser to the dashboard's consent page |
//! | `GET`, `POST /oauth/consent` | the dashboard reads and decides a consent or a device code, from its session cookie |
//! | `POST /oauth/device_authorization` | starts a device grant (RFC 8628 §3.1) |
//! | `POST /oauth/token` | the one token endpoint: codes, refresh tokens, device codes |
//! | `POST /oauth/revoke` | revokes a token's grant (RFC 7009) |
//!
//! The protocol endpoints (`authorize`, `device_authorization`, `token`, `revoke`) are
//! form-encoded and answer errors as the OAuth specifications define them, `{"error": …,
//! "error_description": …}` with `400` (`401` for client authentication), and never as problem
//! documents: an OAuth client understands only that shape ([`OAuthError`]). They take no cookie
//! and no CSRF token. `/oauth/consent` is a dashboard call: a JSON mutation authorised by the
//! session cookie with the CSRF contract every cookie mutation follows, answered with problems
//! like every dashboard call. None of these routes is part of the `/v1` OpenAPI document; they are
//! protocol routes whose contract is the RFCs.

pub mod clients;
#[cfg(test)]
mod gate;
pub mod grants;
pub mod http;
#[cfg(test)]
pub(crate) mod tests;

use std::borrow::Cow;
use std::collections::HashMap;

use axum::Router;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::http::AppState;
use crate::identity::tokens::TokenError;
use crate::problem::Problem;

/// The routes of the authorization server (see the module).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(http::server_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(http::resource_metadata),
        )
        .route("/oauth/authorize", get(http::authorize))
        .route(
            "/oauth/consent",
            get(http::consent_details).post(http::consent),
        )
        .route(
            "/oauth/device_authorization",
            post(http::device_authorization),
        )
        .route("/oauth/token", post(http::token))
        .route("/oauth/revoke", post(http::revoke))
}

/// An OAuth error response (RFC 6749 §5.2, RFC 8628 §3.5, RFC 8707 §2): the error code, a
/// description for the developer, and the status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthError {
    /// The HTTP status: `400`, or `401` for client authentication.
    pub status: StatusCode,
    /// The error code (`invalid_grant`, `slow_down`, …).
    pub error: &'static str,
    /// What went wrong, for the developer.
    pub description: Cow<'static, str>,
    /// Whether the client tried HTTP Basic: a `401` then challenges for it.
    pub basic: bool,
}

impl OAuthError {
    /// An error with status `400`.
    #[must_use]
    pub fn new(error: &'static str, description: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error,
            description: description.into(),
            basic: false,
        }
    }

    /// `invalid_request`: a parameter is missing, repeated or malformed.
    #[must_use]
    pub fn invalid_request(description: impl Into<Cow<'static, str>>) -> Self {
        Self::new("invalid_request", description)
    }

    /// `invalid_grant`: the code, refresh token or device code is not valid for this client.
    #[must_use]
    pub fn invalid_grant(description: impl Into<Cow<'static, str>>) -> Self {
        Self::new("invalid_grant", description)
    }

    /// `invalid_client` (`401`): the client is unknown or failed to authenticate.
    #[must_use]
    pub fn invalid_client(basic: bool) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            basic,
            ..Self::new(
                "invalid_client",
                "The client is unknown or did not authenticate as it registered.",
            )
        }
    }
}

impl IntoResponse for OAuthError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": self.error,
            "error_description": self.description,
        });
        let mut response = (self.status, axum::Json(body)).into_response();
        no_store(response.headers_mut());
        if self.basic {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"norbelys\""),
            );
        }
        response
    }
}

/// Marks an answer that carries or refuses a credential as never to be cached (RFC 6749 §5.1).
pub fn no_store(headers: &mut HeaderMap) {
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
}

/// Why a protocol request failed: an OAuth error the client acts on, or an infrastructure
/// failure answered as a problem (`503` when the database is unavailable).
#[derive(Debug)]
pub enum Refusal {
    /// An OAuth error.
    OAuth(OAuthError),
    /// A failure of ours.
    Problem(Problem),
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        match self {
            Self::OAuth(error) => error.into_response(),
            Self::Problem(problem) => problem.into_response(),
        }
    }
}

impl From<OAuthError> for Refusal {
    fn from(error: OAuthError) -> Self {
        Self::OAuth(error)
    }
}

impl From<Problem> for Refusal {
    fn from(problem: Problem) -> Self {
        Self::Problem(problem)
    }
}

impl From<sqlx::Error> for Refusal {
    fn from(error: sqlx::Error) -> Self {
        Self::Problem(error.into())
    }
}

impl From<grants::StoreError> for Refusal {
    fn from(error: grants::StoreError) -> Self {
        match error {
            grants::StoreError::Db(error) => error.into(),
            grants::StoreError::Crypto(error) => Self::Problem(Problem::internal(&error)),
        }
    }
}

impl From<TokenError> for Refusal {
    fn from(error: TokenError) -> Self {
        Self::Problem(token_problem(error))
    }
}

/// The problem a token minting failure answers: `503` without a signing key (logged for the
/// operator), the database's own failure, or an internal error.
#[must_use]
pub fn token_problem(error: TokenError) -> Problem {
    match error {
        TokenError::NoKey => {
            tracing::error!("no signing key exists: run `norbelys-server admin keys rotate`");
            Problem::unavailable(60)
        }
        TokenError::Db(error) => error.into(),
        other => Problem::internal(&other),
    }
}

/// Parameters of a protocol request (a query string or a form body), each named once: a
/// repeated parameter is refused, as RFC 6749 §3.1 requires.
#[derive(Debug, Default)]
pub struct Params(HashMap<String, String>);

impl Params {
    /// Parses `application/x-www-form-urlencoded` text.
    ///
    /// # Errors
    ///
    /// `invalid_request` for a repeated parameter.
    pub fn parse(text: &[u8]) -> Result<Self, OAuthError> {
        let mut params = HashMap::new();
        for (name, value) in url::form_urlencoded::parse(text) {
            if params
                .insert(name.clone().into_owned(), value.into_owned())
                .is_some()
            {
                return Err(OAuthError::invalid_request(format!(
                    "The parameter `{name}` is repeated."
                )));
            }
        }
        Ok(Self(params))
    }

    /// Parses a form body, which must be declared `application/x-www-form-urlencoded`.
    ///
    /// # Errors
    ///
    /// `invalid_request` for another content type or a repeated parameter.
    pub fn form(headers: &HeaderMap, body: &[u8]) -> Result<Self, OAuthError> {
        let is_form = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            });
        if !is_form {
            return Err(OAuthError::invalid_request(
                "The body must be `application/x-www-form-urlencoded`.",
            ));
        }
        Self::parse(body)
    }

    /// The parameter `name`, when present and not empty.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .get(name)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }

    /// The parameter `name`.
    ///
    /// # Errors
    ///
    /// `invalid_request` when it is missing or empty.
    pub fn require(&self, name: &str) -> Result<&str, OAuthError> {
        self.get(name)
            .ok_or_else(|| OAuthError::invalid_request(format!("`{name}` is required.")))
    }
}
