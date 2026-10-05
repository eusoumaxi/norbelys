//! Identity: who signs in and how, the sessions and tokens that follow, the principal a
//! credential proves, the scopes it carries, and the workspaces, members, invitations, keys and
//! single sign-on connections that decide what a principal may do.
//!
//! - **Principals.** Every handler that touches a workspace takes an [`authority::Principal`];
//!   the only authorization call is [`authority::Principal::require`] (plus the surface and owner
//!   checks of the dashboard's operations).
//! - **Sign-in.** One challenge (`POST /v1/auth/challenges`), one finish (`POST
//!   /v1/auth/sessions`) and one redirect return (`GET /v1/auth/callback`) serve every method:
//!   email codes, passkeys, OpenID Connect and workspace single sign-on. Success issues a fresh
//!   session cookie ([`sessions`]); the dashboard then mints short-lived workspace tokens from it
//!   ([`tokens`]).
//! - **The dashboard surface.** The account (`/v1/me`), workspaces, members, invitations, API
//!   keys, SSO connections and the audit log accept only a browser session: the cookie for the
//!   account and for listing and creating workspaces, a workspace token for everything under
//!   `/v1/workspaces/{id}`. API keys and every other bearer get `403 session_required`.
//! - **Recovery.** Owners register recovery codes ahead of need; an operator opens an audited
//!   break-glass session for an owner locked out by single sign-on, or a 10-minute
//!   impersonation for support ([`recovery`]).
//!
//! [`Identity`] is what the module keeps for the api process: its settings, the signing keys,
//! the fetcher of identity documents, the captcha, the OpenID Connect providers and the passkeys'
//! relying party.

pub mod api_keys;
pub mod audit;
pub mod authority;
pub mod captcha;
pub mod ceremonies;
pub mod codes;
pub mod fetch;
pub mod http;
pub mod invitations;
pub mod memberships;
pub mod oauth;
pub mod oidc;
pub mod passkeys;
pub mod recovery;
pub mod sessions;
pub mod sso;
#[cfg(test)]
mod tests;
pub mod tokens;
pub mod users;
pub mod workspaces;

use std::sync::Arc;

use url::Url;

use crate::config::{IdentityArgs, MailArgs};

/// Why the identity configuration was refused at start.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// A setting is missing or contradicts another.
    #[error("{0}")]
    Invalid(String),
    /// The captcha is half configured.
    #[error(transparent)]
    Captcha(#[from] captcha::CaptchaError),
    /// An HTTP client could not be built.
    #[error("an identity HTTP client could not be built: {0}")]
    Client(#[from] reqwest::Error),
}

/// The settings handlers read.
#[derive(Debug)]
pub struct Settings {
    /// The dashboard's origins: the `Origin` allow-list of sign-in and cookie-authorised
    /// requests. The first is where links in mail (sign-in links, invitations) point.
    pub dashboard_origins: Vec<Url>,
}

impl Settings {
    /// The dashboard's primary origin, where links in mail point.
    #[must_use]
    pub fn dashboard(&self) -> Option<&Url> {
        self.dashboard_origins.first()
    }
}

/// What the identity module keeps for the api process.
#[derive(Clone, Debug)]
pub struct Identity {
    /// The settings.
    pub settings: Arc<Settings>,
    /// The keys that sign and verify workspace tokens.
    pub tokens: tokens::KeyRing,
    /// The fetcher of identity providers' documents.
    pub fetcher: fetch::Fetcher,
    /// The captcha of the email-code challenge, when configured.
    pub captcha: Option<captcha::Captcha>,
    /// The OpenID Connect providers offered to everyone, and the one callback URL.
    pub providers: oidc::Providers,
    /// The passkeys' relying party.
    pub webauthn: Arc<webauthn_rs::Webauthn>,
    /// Whether the OAuth authorization server runs: off (`OAUTH_SERVER_DISABLED`), its routes answer
    /// `404` and the MCP server names no authorization server, so clients use API keys.
    pub oauth_server: bool,
}

impl Identity {
    /// The identity module of an api process configured by `args`, with Google sign-in through
    /// Norbelys's Google app and the one callback URL of `mail` when they are configured.
    ///
    /// # Errors
    ///
    /// No dashboard origin is configured, the relying party id does not fit it, the captcha is
    /// half configured, or a client cannot be built.
    pub fn from_args(args: &IdentityArgs, mail: &MailArgs) -> Result<Self, SettingsError> {
        if args.dashboard_origins.is_empty() {
            return Err(SettingsError::Invalid(
                "DASHBOARD_ORIGINS needs at least one origin".to_owned(),
            ));
        }
        let hostnames = args
            .dashboard_origins
            .iter()
            .filter_map(|origin| origin.host_str().map(str::to_owned))
            .collect();
        Ok(Self {
            settings: Arc::new(Settings {
                dashboard_origins: args.dashboard_origins.clone(),
            }),
            tokens: tokens::KeyRing::new(),
            fetcher: fetch::Fetcher::new(args.identity_allow_private_issuers)?,
            captcha: captcha::Captcha::new(
                args.captcha_provider,
                args.captcha_site_key.clone(),
                args.captcha_secret.clone(),
                args.recaptcha_project.as_deref(),
                hostnames,
            )?,
            providers: oidc::Providers::new(
                mail.google_oauth_client_id
                    .clone()
                    .zip(mail.google_oauth_client_secret.clone()),
                mail.oauth_redirect_url.clone(),
            ),
            webauthn: Arc::new(passkeys::relying_party(
                &args.webauthn_rp_id,
                &args.dashboard_origins,
            )?),
            oauth_server: !args.oauth_server_disabled,
        })
    }

    /// The identity module of the tests: the dashboard at `https://app.norbelys.test`, identity
    /// documents fetchable from the loopback interface, no captcha.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn for_tests() -> Self {
        let dashboard = Url::parse(crate::testing::DASHBOARD)
            .unwrap_or_else(|_| unreachable!("the tests' dashboard origin is a URL"));
        let redirect = dashboard
            .join("/api/v1/auth/callback")
            .unwrap_or_else(|_| unreachable!("the tests' callback is a URL"));
        Self {
            providers: oidc::Providers::for_tests(Vec::new(), redirect),
            webauthn: Arc::new(
                passkeys::relying_party("norbelys.test", std::slice::from_ref(&dashboard))
                    .unwrap_or_else(|error| unreachable!("the tests' relying party: {error}")),
            ),
            settings: Arc::new(Settings {
                dashboard_origins: vec![dashboard],
            }),
            tokens: tokens::KeyRing::new(),
            fetcher: fetch::Fetcher::new(true)
                .unwrap_or_else(|error| unreachable!("the test fetcher builds: {error}")),
            captcha: None,
            oauth_server: true,
        }
    }
}
