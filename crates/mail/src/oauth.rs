//! OAuth 2.0 for mailbox connections through Google and Microsoft: the authorization URL of the
//! consent, the authorization-code exchange (with PKCE), token refresh, the check of the scopes
//! actually granted, and the refresh errors that mean the grant is lost.
//!
//! The caller owns the ceremony (it stores `state`, `nonce` and the PKCE verifier, and verifies
//! the returned ID token with its OpenID Connect library); this module speaks the two providers'
//! token endpoints and tells their answers apart.
//!
//! Why each piece exists:
//! - Refresh tokens must be used regularly: Google revokes one unused for six months and
//!   Microsoft's expire after 90 days of inactivity, so the caller refreshes at least daily and
//!   learns about a lost grant from the refresh itself.
//! - Every token response's `scope` is checked ([`check_scopes`]): with granular consent a
//!   person may untick a Gmail permission, and a token without it fails later and less clearly.
//! - Only the OAuth `error` code is branched on (RFC 6749 §5.2,
//!   <https://www.rfc-editor.org/rfc/rfc6749#section-5.2>), as Microsoft asks; Microsoft's
//!   `AADSTS` numbers are kept for support logs only.
//!   - `invalid_grant`: the grant was revoked, expired, or the account changed (Google revokes
//!     tokens with Gmail scopes when the password changes; a deleted or disabled account too).
//!     The person reconnects.
//!   - Google's `admin_policy_enforced`: a Workspace admin restricted the app; the admin must
//!     trust it.
//!   - Microsoft's `interaction_required`: multifactor authentication or Conditional Access needs
//!     the person; they reconnect.
//!   - `invalid_client`, `unauthorized_client`, `invalid_scope`, `unsupported_grant_type`,
//!     `invalid_request`: our own app's configuration, not the person's.
//!   - `temporarily_unavailable`, `429` and `5xx`: try again later.
//!
//! Microsoft rotates the refresh token on every refresh: [`Tokens::refresh_token`] must replace
//! the stored one when present.

use aws_lc_rs::digest;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::{SignedDuration, Timestamp};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use tokio::time::Instant;
use url::Url;

use crate::http::{self, HttpClient};

/// Scopes for a Google mailbox read and sent through the Gmail API: `openid` (the ID token names
/// the account's immutable subject), sending, and reading (history, messages, the profile, the
/// Sent search, the send-as list).
pub const GOOGLE_API_SCOPES: &[&str] = &[
    "openid",
    "https://www.googleapis.com/auth/gmail.send",
    "https://www.googleapis.com/auth/gmail.readonly",
];

/// Scopes for a Google mailbox used over SMTP and IMAP with `XOAUTH2`.
pub const GOOGLE_XOAUTH2_SCOPES: &[&str] = &["openid", "https://mail.google.com/"];

/// Scopes for a Microsoft mailbox read and sent through Graph: `openid profile` (Microsoft
/// returns the `oid` claim only with `profile`), `offline_access` (a refresh token), `User.Read`
/// (`GET /me`), sending and reading mail.
pub const MICROSOFT_GRAPH_SCOPES: &[&str] = &[
    "openid",
    "profile",
    "offline_access",
    "https://graph.microsoft.com/User.Read",
    "https://graph.microsoft.com/Mail.Send",
    "https://graph.microsoft.com/Mail.Read",
];

/// Scopes for a Microsoft mailbox used over SMTP and IMAP with `XOAUTH2` (a token for the
/// Outlook resource; Graph tokens are not accepted there).
pub const MICROSOFT_XOAUTH2_SCOPES: &[&str] = &[
    "openid",
    "profile",
    "offline_access",
    "https://outlook.office.com/SMTP.Send",
    "https://outlook.office.com/IMAP.AccessAsUser.All",
];

/// The identity provider of a mailbox connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// Google (`accounts.google.com`).
    Google,
    /// Microsoft identity platform v2.0, for a tenant: `common`, `organizations`, or a tenant id.
    Microsoft {
        /// The tenant path segment.
        tenant: String,
    },
}

/// Norbelys's OAuth app at the provider.
#[derive(Clone)]
pub struct App {
    /// The client id.
    pub client_id: String,
    /// The client secret (a confidential client).
    pub client_secret: SecretString,
    /// The redirect URI registered with the provider (`https`).
    pub redirect_uri: Url,
}

impl std::fmt::Debug for App {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("App")
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri.as_str())
            .finish_non_exhaustive()
    }
}

/// One consent request: values the caller generated and stored for the callback.
#[derive(Debug, Clone, Copy)]
pub struct AuthorizationRequest<'a> {
    /// The anti-forgery state echoed to the callback.
    pub state: &'a str,
    /// The OpenID Connect nonce the ID token must carry.
    pub nonce: &'a str,
    /// The PKCE code verifier (RFC 7636); its S256 challenge goes in the URL.
    pub code_verifier: &'a SecretString,
    /// The account expected, so the provider preselects it on a reconnect.
    pub login_hint: Option<&'a str>,
    /// The scopes to ask for, such as [`GOOGLE_API_SCOPES`].
    pub scopes: &'a [&'a str],
}

/// A token endpoint's answer.
#[derive(Clone)]
pub struct Tokens {
    /// The access token for the provider's APIs.
    pub access_token: SecretString,
    /// A refresh token, when the provider issued one (every Microsoft refresh rotates it).
    pub refresh_token: Option<SecretString>,
    /// When the access token expires, from `expires_in` at the time the answer was read.
    pub expires_at: Timestamp,
    /// The scopes granted, space-separated, as the provider returned them.
    pub scope: String,
    /// The OpenID Connect ID token, for the caller to verify (issuer, audience, nonce,
    /// signature) and to read the account's immutable subject from.
    pub id_token: Option<String>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Tokens")
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// Why a token request failed, in the terms that decide what happens to the connection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OAuthError {
    /// `invalid_grant`: the grant is lost; the person reconnects.
    #[error("the grant was revoked or expired: {description}")]
    InvalidGrant {
        /// The provider's description, bounded.
        description: String,
        /// Microsoft's `AADSTS` error numbers, for support.
        codes: Vec<u64>,
    },
    /// Google's `admin_policy_enforced`: a Workspace admin restricted the app.
    #[error("a Workspace admin restricted this app: {description}")]
    AdminPolicyEnforced {
        /// The provider's description, bounded.
        description: String,
    },
    /// Microsoft's `interaction_required`: multifactor authentication or Conditional Access.
    #[error("the person must sign in again (multifactor or Conditional Access): {description}")]
    InteractionRequired {
        /// The provider's description, bounded.
        description: String,
        /// Microsoft's `AADSTS` error numbers, for support.
        codes: Vec<u64>,
    },
    /// The provider refused our own app or request: `invalid_client`, `unauthorized_client`,
    /// `invalid_scope`, `unsupported_grant_type`, `invalid_request`, or an unknown code.
    #[error("the provider refused the request ({error}): {description}")]
    Refused {
        /// The OAuth `error` code.
        error: String,
        /// The provider's description, bounded.
        description: String,
        /// Microsoft's `AADSTS` error numbers, for support.
        codes: Vec<u64>,
    },
    /// `temporarily_unavailable`, `429`, `5xx`, or no response: try again later.
    #[error("the token endpoint is unavailable: {0}")]
    Unavailable(String),
    /// The token works but lacks scopes the connection needs.
    #[error("the grant lacks scopes: {}", missing.join(" "))]
    ScopeMissing {
        /// The required scopes not granted.
        missing: Vec<String>,
    },
    /// The request or the answer is malformed (a redirect URI that is not `https`, a token
    /// response without an access token).
    #[error("invalid OAuth exchange: {0}")]
    Invalid(String),
}

/// The provider's consent URL for `request`: the authorization-code flow with PKCE (S256).
/// Google is asked for a refresh token every time (`access_type=offline`, `prompt=consent`).
///
/// # Errors
///
/// The redirect URI is not an absolute `https` URL without credentials or fragment, or the
/// tenant is not a plain path segment.
pub fn authorization_url(
    provider: &Provider,
    app: &App,
    request: &AuthorizationRequest<'_>,
) -> Result<Url, OAuthError> {
    let redirect = &app.redirect_uri;
    if redirect.scheme() != "https"
        || redirect.host_str().is_none()
        || !redirect.username().is_empty()
        || redirect.password().is_some()
        || redirect.fragment().is_some()
    {
        return Err(OAuthError::Invalid(
            "the redirect URI must be https without credentials or fragment".to_owned(),
        ));
    }
    let challenge = URL_SAFE_NO_PAD.encode(digest::digest(
        &digest::SHA256,
        request.code_verifier.expose_secret().as_bytes(),
    ));
    let scope = request.scopes.join(" ");
    let mut url = parse(&endpoints(provider)?.0)?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("client_id", &app.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect.as_str())
            .append_pair("scope", &scope)
            .append_pair("state", request.state)
            .append_pair("nonce", request.nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        if let Some(hint) = request.login_hint {
            query.append_pair("login_hint", hint);
        }
        match provider {
            Provider::Google => {
                query
                    .append_pair("access_type", "offline")
                    .append_pair("prompt", "consent");
            }
            Provider::Microsoft { .. } => {
                query.append_pair("response_mode", "query");
            }
        }
    }
    Ok(url)
}

/// Exchanges the callback's authorization `code` for tokens.
///
/// # Errors
///
/// The provider refused the code or our app ([`OAuthError`]), or did not answer by `deadline`.
pub async fn exchange(
    http: &HttpClient,
    provider: &Provider,
    app: &App,
    code: &SecretString,
    code_verifier: &SecretString,
    deadline: Instant,
) -> Result<Tokens, OAuthError> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.expose_secret()),
        ("code_verifier", code_verifier.expose_secret()),
        ("redirect_uri", app.redirect_uri.as_str()),
    ];
    token_request(http, provider, app, &form, deadline).await
}

/// Refreshes the access token. Microsoft is asked for the same `scopes` again (its v2.0
/// endpoint issues a token for the scopes named); Google ignores them.
///
/// # Errors
///
/// The provider refused the refresh ([`OAuthError::InvalidGrant`] and the others decide the
/// connection's status), or did not answer by `deadline`.
pub async fn refresh(
    http: &HttpClient,
    provider: &Provider,
    app: &App,
    refresh_token: &SecretString,
    scopes: &[&str],
    deadline: Instant,
) -> Result<Tokens, OAuthError> {
    let scope = scopes.join(" ");
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.expose_secret()),
    ];
    if matches!(provider, Provider::Microsoft { .. }) {
        form.push(("scope", scope.as_str()));
    }
    token_request(http, provider, app, &form, deadline).await
}

/// Whether `granted` (a token response's space-separated `scope`) holds every scope in
/// `required`. Compared without regard to ASCII case and without Graph's resource prefix
/// (`https://graph.microsoft.com/`), since Microsoft may return either form; `openid`,
/// `profile` and `offline_access`, which providers do not echo, are never required here.
///
/// # Errors
///
/// [`OAuthError::ScopeMissing`] names the scopes not granted.
pub fn check_scopes(granted: &str, required: &[&str]) -> Result<(), OAuthError> {
    const IMPLICIT: [&str; 3] = ["openid", "profile", "offline_access"];
    let normal = |scope: &str| {
        let lower = scope.to_ascii_lowercase();
        lower
            .strip_prefix("https://graph.microsoft.com/")
            .map(str::to_owned)
            .unwrap_or(lower)
    };
    let granted: Vec<String> = granted.split_whitespace().map(normal).collect();
    let missing: Vec<String> = required
        .iter()
        .filter(|scope| !IMPLICIT.contains(&scope.to_ascii_lowercase().as_str()))
        .filter(|scope| !granted.contains(&normal(scope)))
        .map(|scope| (*scope).to_owned())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(OAuthError::ScopeMissing { missing })
    }
}

/// The authorization and token endpoints.
fn endpoints(provider: &Provider) -> Result<(String, String), OAuthError> {
    match provider {
        Provider::Google => Ok((
            "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
            "https://oauth2.googleapis.com/token".to_owned(),
        )),
        Provider::Microsoft { tenant } => {
            let valid = !tenant.is_empty()
                && tenant.len() <= 64
                && tenant
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'));
            if !valid {
                return Err(OAuthError::Invalid(format!("`{tenant}` is not a tenant")));
            }
            let base = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0");
            Ok((format!("{base}/authorize"), format!("{base}/token")))
        }
    }
}

async fn token_request(
    http: &HttpClient,
    provider: &Provider,
    app: &App,
    form: &[(&str, &str)],
    deadline: Instant,
) -> Result<Tokens, OAuthError> {
    #[derive(Deserialize)]
    struct Answer {
        access_token: SecretString,
        refresh_token: Option<SecretString>,
        expires_in: serde_json::Value,
        #[serde(default)]
        scope: String,
        token_type: String,
        id_token: Option<String>,
    }
    let mut fields: Vec<(&str, &str)> = form.to_vec();
    fields.push(("client_id", app.client_id.as_str()));
    fields.push(("client_secret", app.client_secret.expose_secret()));
    let request = http.post(parse(&endpoints(provider)?.1)?).form(&fields);
    let response = http::send(request, deadline)
        .await
        .map_err(|error| OAuthError::Unavailable(error.to_string()))?;
    let status = response.status();
    let body = http::read_body(response, 64 * 1024)
        .await
        .map_err(|error| OAuthError::Unavailable(error.to_string()))?;
    if !status.is_success() {
        return Err(refusal(status.as_u16(), &body));
    }
    let answer: Answer =
        serde_json::from_slice(&body).map_err(|error| OAuthError::Invalid(error.to_string()))?;
    let lifetime = match &answer.expires_in {
        serde_json::Value::Number(number) => number.as_i64(),
        serde_json::Value::String(text) => text.parse().ok(),
        _ => None,
    }
    .filter(|seconds| (1..=86_400 * 366).contains(seconds))
    .ok_or_else(|| {
        OAuthError::Invalid("expires_in is not a positive number of seconds".to_owned())
    })?;
    if !answer.token_type.eq_ignore_ascii_case("bearer")
        || answer.access_token.expose_secret().is_empty()
    {
        return Err(OAuthError::Invalid("not a bearer access token".to_owned()));
    }
    let expires_at = Timestamp::now()
        .checked_add(SignedDuration::from_secs(lifetime))
        .map_err(|error| OAuthError::Invalid(error.to_string()))?;
    Ok(Tokens {
        access_token: answer.access_token,
        refresh_token: answer
            .refresh_token
            .filter(|token| !token.expose_secret().is_empty()),
        expires_at,
        scope: answer.scope,
        id_token: answer.id_token,
    })
}

/// The meaning of a token endpoint's error answer.
fn refusal(status: u16, body: &[u8]) -> OAuthError {
    #[derive(Deserialize, Default)]
    struct Answer {
        #[serde(default)]
        error: String,
        #[serde(default)]
        error_description: String,
        #[serde(default)]
        error_codes: Vec<u64>,
    }
    let answer: Answer = serde_json::from_slice(body).unwrap_or_default();
    let description = crate::text::bounded(&answer.error_description, 500);
    let codes = answer.error_codes;
    match answer.error.as_str() {
        "invalid_grant" => OAuthError::InvalidGrant { description, codes },
        "admin_policy_enforced" => OAuthError::AdminPolicyEnforced { description },
        "interaction_required" => OAuthError::InteractionRequired { description, codes },
        "temporarily_unavailable" => OAuthError::Unavailable(description),
        _ if status == 429 || status >= 500 => {
            OAuthError::Unavailable(format!("{status} {description}"))
        }
        "" => OAuthError::Refused {
            error: status.to_string(),
            description,
            codes,
        },
        error => OAuthError::Refused {
            error: crate::text::bounded(error, 100),
            description,
            codes,
        },
    }
}

fn parse(url: &str) -> Result<Url, OAuthError> {
    Url::parse(url).map_err(|error| OAuthError::Invalid(error.to_string()))
}

#[cfg(test)]
mod tests;
