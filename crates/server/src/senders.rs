//! The Sending area: connections, their sender identities and receive bindings, quota scopes,
//! sending domains, and the maintenance that keeps them healthy.
//!
//! # What lives here
//!
//! - A **connection** is one authenticated transport account: a Google or Microsoft mailbox
//!   connected through OAuth, any SMTP login, the customer's Amazon SES, SendGrid or Mailgun
//!   account over its SMTP endpoint, or a login on the managed MTA. It holds the sealed
//!   credential, the account (its address and, for OAuth, the provider's issuer and immutable
//!   subject), its health, its pause, and the pacing settings the sender follows: daily limit,
//!   interval, send window, warm-up stage, quota scope ([`connections`]).
//! - Its **sender identities** are the From addresses that share its budget ([`identities`]); its
//!   **receive bindings** are the folders the inbox role reads ([`bindings`]); a relay's
//!   **provider webhook** is the signed callback URL its evidence arrives at, issued with it.
//! - A **quota scope** is a provider-side limit of an account the customer owns, shared by its
//!   connections ([`scopes`]); a **sending domain** is a customer domain whose ownership is
//!   proven through DNS ([`domains`]).
//! - Health moves through one table ([`crate::domain::senders::transition`]), applied by
//!   [`health`], each change told to customers as `connection.health_changed`.
//! - Background kinds: `connection.check` and its daily fan-out `connection.check_due`
//!   ([`check`]), `domain.verify` and its fan-out `domain.verify_due` ([`domains`]), and
//!   `provider.norbelys.provision` ([`provision`]).
//! - Mailboxes are connected through an OAuth ceremony started here and finished at the one
//!   `GET /v1/auth/callback` ([`oauth`]).
//!
//! # Design
//!
//! The API never waits on a provider: a write commits its rows and enqueues the job that talks
//! to the provider, in one transaction, and answers with the connection in its intermediate
//! status (`verifying`). Every job reads the connection's `credential_version` and status before
//! it calls anyone, and writes back only while both are unchanged, so a check of a credential
//! replaced meanwhile writes nothing and runs again on the current one.
//!
//! Credentials are sealed with the deployment key and bound to their row ([`credentials`]); the
//! background roles read them only through `connection_credential()` and write them only through
//! `set_connection_credential()`, which bumps the version the fences compare.
//!
//! # Lock order
//!
//! A quota scope's row, then a connection's row, then its bindings and identities. Every path
//! that writes a connection locks its row first (`FOR UPDATE`): the API's update, verify and
//! archive, and each job's fenced write. Archiving takes the connection, then its bindings, as
//! an inbox claim does.

pub mod bindings;
pub mod check;
pub mod connections;
pub mod credentials;
pub mod domains;
pub mod health;
pub mod http;
pub mod identities;
pub mod oauth;
pub mod provision;
pub mod scopes;
#[cfg(test)]
mod tests;
pub mod tokens;
pub mod warmup;

use norbelys_mail::http::HttpClient;
use norbelys_mail::net::{AddressPolicy, Connector};
use norbelys_mail::smtp::{PoolConfig, SmtpPool};
use url::Url;

use crate::config::MailArgs;
use crate::crypto::CryptoError;
use crate::delivery::limits::{Limits, Role, Shares};
use crate::domain::ids::{Id, ProviderWebhook};

/// The Sending area's configuration, read once at start by the api and the worker.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Norbelys's OAuth apps at Google and Microsoft, when configured.
    pub apps: oauth::Apps,
    /// The client every provider API call goes through (token endpoints, Gmail, Graph).
    pub http: HttpClient,
    /// The origin provider callbacks are posted to: `<origin>/webhooks/{id}`.
    pub public_webhooks_url: Url,
    /// The host a sending domain's tracking hostname points its CNAME at.
    pub tracking_cname_target: String,
    /// The managed MTA's submission host, written into `norbelys` connections.
    pub mta_submission_host: String,
    /// A stable, configured SPF include; self-hosted installations can choose their own.
    pub mta_spf_include: Option<String>,
}

/// Why the Sending area's configuration was refused at start.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("the provider HTTP client could not be built: {0}")]
    Http(#[from] norbelys_mail::http::ClientError),
    #[error("the TLS configuration of mail sessions could not be built: {0}")]
    Connector(#[from] norbelys_mail::net::ConnectorError),
    #[error("{0}")]
    Invalid(String),
}

impl Settings {
    /// The settings `args` describe.
    ///
    /// # Errors
    ///
    /// An OAuth app is half configured, its redirect URL is not `https`, or the HTTP client
    /// cannot be built.
    pub fn from_args(args: &MailArgs) -> Result<Self, SettingsError> {
        let mta_spf_include = args
            .mta_spf_include
            .as_deref()
            .map(|name| {
                domains::hostname(name).ok_or_else(|| {
                    SettingsError::Invalid(
                        "MTA_SPF_INCLUDE must be a fully qualified hostname".to_owned(),
                    )
                })
            })
            .transpose()?;
        Ok(Self {
            apps: oauth::Apps::from_args(args)?,
            http: HttpClient::new()?,
            public_webhooks_url: args.public_webhooks_url.clone(),
            tracking_cname_target: args.tracking_cname_target.trim().to_ascii_lowercase(),
            mta_submission_host: args.mta_submission_host.clone(),
            mta_spf_include,
        })
    }

    /// The URL providers post a provider webhook's callbacks to.
    #[must_use]
    pub fn webhook_url(&self, webhook: Id<ProviderWebhook>) -> String {
        format!(
            "{}/webhooks/{webhook}",
            self.public_webhooks_url.as_str().trim_end_matches('/')
        )
    }

    /// Settings for tests: no OAuth app, the default addresses.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self {
            apps: oauth::Apps::default(),
            http: HttpClient::new().expect("the HTTP client builds"),
            public_webhooks_url: Url::parse("https://hooks.norbelys.test").expect("a URL"),
            tracking_cname_target: "tracking.norbelys.test".to_owned(),
            mta_submission_host: "smtp.norbelys.test".to_owned(),
            mta_spf_include: None,
        }
    }
}

/// What the worker's sending kinds reach through `JobContext::env`: the settings, the SMTP
/// pools, the IMAP connector, the DNS resolver and the managed MTA's control API.
#[derive(Clone)]
pub struct Env {
    /// The configuration shared with the api.
    pub settings: Settings,
    /// Sessions to hosts the tenants typed (mailboxes, relays): public addresses only, unless
    /// the development switch allows private ones.
    pub smtp: SmtpPool,
    /// Sessions to the managed MTA, on the private network.
    pub mta_smtp: SmtpPool,
    /// IMAP connections to the hosts the tenants typed, under the same policy as `smtp`.
    pub connector: Connector,
    /// The process's one DNS resolver: sending domains' records, and the hosts of the sessions
    /// above (through their connectors, which share its cache).
    pub resolver: crate::dns::Resolver,
    /// The managed MTA's control API, when configured.
    pub control: Option<provision::Control>,
    /// This replica's part of the Gmail and Graph limits maintenance shares with sending and
    /// receiving: every Gmail and Graph read of a connection's check waits for its tokens.
    pub limits: Limits,
}

impl std::fmt::Debug for Env {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Env")
            .field("settings", &self.settings)
            .field("control", &self.control.is_some())
            .finish_non_exhaustive()
    }
}

impl Env {
    /// The worker's environment from its arguments, over the process's one `resolver`, which the
    /// connectors and the domain checks share, with `limits`, the worker's part of the provider
    /// rate limits.
    ///
    /// # Errors
    ///
    /// The settings are refused, or TLS cannot be configured.
    pub fn from_args(
        args: &MailArgs,
        resolver: crate::dns::Resolver,
        limits: Limits,
    ) -> Result<Self, SettingsError> {
        let policy = if args.mail_allow_private_hosts {
            AddressPolicy::Any
        } else {
            AddressPolicy::PublicOnly
        };
        let control = match (&args.mta_control_url, &args.mta_control_secret) {
            (Some(url), Some(secret)) => Some(provision::Control::new(
                url.clone(),
                secret,
                args.mta_control_private_transport,
            )?),
            (None, None) => None,
            _ => {
                return Err(SettingsError::Invalid(
                    "MTA_CONTROL_URL and MTA_CONTROL_SECRET are set together".to_owned(),
                ));
            }
        };
        let mut env = Self::assemble(Settings::from_args(args)?, resolver, policy, control)?;
        env.limits = limits;
        Ok(env)
    }

    /// The environment of `settings` over `resolver`, with tenant hosts under `policy`, holding
    /// the default split's maintenance share as the only worker replica would (the worker's
    /// [`Env::from_args`] puts its configured share in its place).
    ///
    /// # Errors
    ///
    /// TLS cannot be configured.
    pub(crate) fn assemble(
        settings: Settings,
        resolver: crate::dns::Resolver,
        policy: AddressPolicy,
        control: Option<provision::Control>,
    ) -> Result<Self, SettingsError> {
        let connector = Connector::new(resolver.hickory(), policy)?;
        let private = Connector::new(resolver.hickory(), AddressPolicy::Any)?;
        Ok(Self {
            settings,
            smtp: SmtpPool::new(connector.clone(), PoolConfig::default()),
            mta_smtp: SmtpPool::new(private, PoolConfig::default()),
            connector,
            resolver,
            control,
            limits: Limits::new(Shares::single(Role::Maintenance)),
        })
    }
}

/// Why a Sending operation was refused. Converted into a problem at the HTTP boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No such row in this workspace (`404 not_found`).
    #[error("no such {0}")]
    NotFound(&'static str),
    /// A field of the request breaks a rule (`422 validation_failed`), at its JSON pointer.
    #[error("{pointer}: {detail}")]
    Invalid { pointer: String, detail: String },
    /// The resource's current status does not allow it (`409 invalid_state`).
    #[error("{0}")]
    InvalidState(String),
    /// A live resource holds what the request would take (`409 conflict`).
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Credential(#[from] credentials::CredentialError),
    #[error(transparent)]
    Ceremony(#[from] crate::identity::ceremonies::CeremonyError),
}

impl Error {
    /// A field error at `pointer`.
    pub(crate) fn invalid(pointer: &str, detail: impl Into<String>) -> Self {
        Self::Invalid {
            pointer: pointer.to_owned(),
            detail: detail.into(),
        }
    }
}
