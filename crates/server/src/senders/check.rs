//! `connection.check`: proves a connection's credential and account, and `connection.check_due`,
//! its daily fan-out.
//!
//! # What a check proves
//!
//! - **OAuth (Google, Microsoft):** the token refresh itself, at least daily, so a refresh
//!   token never sits unused until the provider forgets it (Google after six months unused,
//!   Microsoft after 90 days inactive); the scopes the refreshed token carries, since a granular
//!   consent can drop one; then the mailbox answers with the new token (Gmail's
//!   `users.getProfile`, Graph's `GET /me`), and a Gmail mailbox lists the addresses it may send
//!   as (`users.settings.sendAs.list`), which marks its identities verified or not. Only the
//!   OAuth `error` is branched on: `invalid_grant` and `interaction_required` mean the person
//!   reconnects; Google's `admin_policy_enforced` and an API `403` mean an administrator
//!   restricted the app.
//! - **A password (SMTP logins, relays, the managed MTA):** an SMTP `AUTH` probe on a new
//!   session, and an IMAP login when the connection is read over IMAP. A `5xx` to `AUTH` (`535
//!   5.7.8`) or an IMAP login refused means the credential is lost; `454` and every failure to
//!   reach the server are temporary.
//! - **A relay's account, after its login passed:** SendGrid's key permissions, read with the
//!   key its login uses (`GET /v3/scopes`: `401` means the key was deleted or revoked, a list
//!   without `mail.send` that it can no longer send; both lose the credential). With the API
//!   credentials a customer may give an SES connection, Amazon SES's `GetAccount` in the Region
//!   of its SMTP host: sending paused by AWS (`SendingEnabled` false, or `SHUTDOWN`) pauses the
//!   account's quota scope for a day, until the next check reads it again, and a later check
//!   that finds it sending lifts that pause (never a breaker's); probation and the sandbox are
//!   noted in the connection's detail. Then `GetEmailIdentity` for each identity, an address or
//!   its domain, confirms or withdraws its send-as. A refused API credential is noted and
//!   changes nothing else, since the SMTP login is what sends. Mailgun's separate API key is
//!   checked daily with a one-event read of its SMTP domain in the same region; a refusal is
//!   noted without revoking the working SMTP credential.
//!
//! Gmail's reads (`getProfile`, `settings.sendAs.list`, and the Sent-folder searches of the last
//! step) are charged to maintenance's part of the Gmail limits first (`delivery::limits`); a
//! share spent past the check's budget makes the check temporary.
//!
//! # Outcomes
//!
//! A pass makes the connection `active` (a paced sender's clock then starts on its phase); a lost
//! credential moves it to `authorization_required` and a blocked account to `disabled`, both
//! told to customers and waiting for a person. A temporary failure changes no status: it writes
//! what happened into `status_detail`, stamps `checked_at` (so the daily fan-out does not pull
//! the retry forward), and the job retries on the runner's backoff.
//!
//! # Fencing
//!
//! The check reads the connection's `credential_version` and status before any request, and its
//! write locks the row and requires both unchanged: a credential replaced, a connection archived
//! or verified again meanwhile makes the check write nothing and run again at once, on the
//! current credential. Requests are made one at a time within 45 seconds, inside the lease that
//! protects the job, so none outlives it; with one check per connection (its unique key) a
//! mailbox never sees two of ours at once.
//!
//! Its last step, for a mailbox whose check passed, reconciles the mailbox's `uncertain` messages
//! through its Sent folder (`delivery::reconcile::sent_folder`), resuming there after a yield.

use std::collections::HashMap;
use std::time::Duration;

use norbelys_mail::http::ApiError;
use norbelys_mail::imap::{self, ImapAuth, ImapError, ImapServer};
use norbelys_mail::oauth::{self, OAuthError};
use norbelys_mail::relays::sigv4::AccessKey;
use norbelys_mail::relays::{mailgun, sendgrid, ses};
use norbelys_mail::smtp::{SmtpAuth, SmtpSecurity, SmtpServer};
use norbelys_mail::submission::Cause;
use norbelys_mail::{gmail, graph};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::time::Instant;
use uuid::Uuid;

use super::connections::{ImapSecurity, ImapSettings, Security, SmtpSettings};
use super::credentials::{self, ApiCredential, Credential, Grant};
use super::{Env, health, oauth as connection_oauth};
use crate::crypto::Keys;
use crate::db::Tx;
use crate::delivery::limits::Read;
use crate::domain::ids::{Connection, Id, WorkspaceId};
use crate::domain::senders::{HealthEvent, Provider, Status};
use crate::domain::time::Timestamp;
use crate::jobs::{self, Class, Effect, Job, JobContext, JobError, Outcome, Queue};

/// How long a check's provider requests may take in all: well inside the 60-second lease.
const BUDGET: Duration = Duration::from_secs(45);
/// Connections the fan-out enqueues per run; the rest wait for the next run, 5 minutes later.
const DUE_PER_RUN: i64 = 10_000;

/// `connection.check`: proves one connection's credential (see the module).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionCheck {
    /// The connection to check.
    pub connection: Id<Connection>,
}

impl Job for ConnectionCheck {
    const KIND: &'static str = "connection.check";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::ExternalRetryable;

    fn unique_key(&self) -> Option<String> {
        Some(self.connection.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        // A check that yielded while reconciling its mailbox's uncertain messages goes back to it.
        if crate::delivery::reconcile::resuming(cx.progress()) {
            return crate::delivery::reconcile::sent_folder(cx, workspace, self.connection).await;
        }
        let keys = cx.env::<Keys>()?.clone();
        let env = cx.env::<Env>()?.clone();
        let Some(loaded) = load(cx, &keys, workspace, self.connection).await? else {
            return Ok(Outcome::Done);
        };
        let test_mode = test_mode(cx, workspace).await?;
        let verdict = if test_mode {
            // A workspace in test mode submits through the fake transport and never reaches a
            // provider, so its connections are not checked against one either.
            Verdict::passed()
        } else {
            match (&loaded.credential, &loaded.smtp) {
                (Some(Credential::OAuth(grant)), _) => {
                    check_oauth(&env, self.connection, loaded.provider, grant.clone()).await
                }
                (Some(Credential::Password(password)), Some(smtp)) => {
                    match check_password(
                        &env,
                        loaded.provider,
                        smtp,
                        loaded.imap.as_ref(),
                        password,
                    )
                    .await
                    {
                        Verdict::Passed { .. } => {
                            check_account(&env, &loaded, smtp, password).await
                        }
                        other => other,
                    }
                }
                (Some(Credential::Password(_)), None) => Verdict::Lost(
                    "The connection holds a password but no SMTP settings; reconnect it."
                        .to_owned(),
                ),
                (None, _) => return Ok(Outcome::Done),
            }
        };
        let verdict = match verdict {
            Verdict::Lost(detail)
                if loaded.provider == Provider::Norbelys && loaded.status == Status::Verifying =>
            {
                Verdict::Temporary(format!("The managed login is not ready yet: {detail}"))
            }
            other => other,
        };
        let passed = matches!(verdict, Verdict::Passed { .. });
        let outcome = record(cx, &keys, workspace, self.connection, &loaded, verdict).await?;
        // Its last step, for a mailbox that passed: settle its uncertain messages by reading its
        // Sent folder (`delivery::reconcile`).
        if passed && outcome == Outcome::Done && loaded.provider.is_mailbox() && !test_mode {
            return crate::delivery::reconcile::sent_folder(cx, workspace, self.connection).await;
        }
        Ok(outcome)
    }
}

/// Whether `workspace` is in test mode, where nothing reaches a provider.
async fn test_mode(cx: &JobContext, workspace: WorkspaceId) -> Result<bool, JobError> {
    let mut tx = cx.db().begin_in(workspace).await?;
    let mode = sqlx::query_scalar!(
        "SELECT mode FROM workspaces WHERE id = $1",
        workspace.uuid()
    )
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(mode.as_deref() == Some("test"))
}

/// What a check reads before it calls anyone.
struct Loaded {
    provider: Provider,
    status: Status,
    version: i64,
    paused: bool,
    smtp: Option<SmtpSettings>,
    imap: Option<ImapSettings>,
    credential: Option<Credential>,
    /// A relay's API credential, sealed beside its password, when the customer gave one.
    api: Option<ApiCredential>,
    /// The connection's identities: id and folded address.
    identities: Vec<(Uuid, String)>,
}

/// Reads the connection, its credential through `connection_credential()` and its identities;
/// `None` when there is nothing to check (gone, or in a status that waits for a person).
async fn load(
    cx: &JobContext,
    keys: &Keys,
    workspace: WorkspaceId,
    connection: Id<Connection>,
) -> Result<Option<Loaded>, JobError> {
    let mut tx = cx.db().begin_in(workspace).await?;
    let row = sqlx::query!(
        "SELECT provider, status, credential_version, paused, smtp, imap, connection_credential(workspace_id, id) AS credential
           FROM connections WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    let identities = sqlx::query!(
        "SELECT id, email_key FROM sender_identities WHERE workspace_id = $1 AND connection_id = $2",
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| (row.id, row.email_key))
    .collect();
    tx.commit().await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let (Ok(provider), Ok(status)) = (
        row.provider.parse::<Provider>(),
        row.status.parse::<Status>(),
    ) else {
        return Err(JobError::Failed(
            "the connection's provider or status is not understood".to_owned(),
        ));
    };
    if !matches!(status, Status::Verifying | Status::Active) {
        return Ok(None);
    }
    let (credential, api) = match row.credential {
        Some(sealed) => {
            let (credential, api) = credentials::open_parts(keys, workspace, connection, &sealed)
                .map_err(|error| {
                JobError::Failed(format!("the credential cannot be opened: {error}"))
            })?;
            (Some(credential), api)
        }
        None => (None, None),
    };
    Ok(Some(Loaded {
        provider,
        status,
        version: row.credential_version,
        paused: row.paused,
        smtp: row.smtp.and_then(|smtp| serde_json::from_value(smtp).ok()),
        imap: row.imap.and_then(|imap| serde_json::from_value(imap).ok()),
        credential,
        api,
        identities,
    }))
}

/// What a check found.
#[derive(Debug)]
enum Verdict {
    /// The credential and the account work; an OAuth grant comes back refreshed, a Gmail mailbox
    /// or an SES account names the addresses it may send as (folded), the account's state at its
    /// provider may leave notes for the person (probation, the sandbox, a refused API key), and
    /// an SES account's paused sending pauses its quota scope.
    Passed {
        refreshed: Option<Grant>,
        send_as: Option<Vec<String>>,
        notes: Option<String>,
        scope: Option<ScopePause>,
    },
    /// The credential or the consent is lost.
    Lost(String),
    /// An administrator or the provider blocked the account.
    Blocked(String),
    /// Nothing could be decided now; the check runs again later.
    Temporary(String),
}

impl Verdict {
    /// A pass with nothing more to tell.
    fn passed() -> Self {
        Self::Passed {
            refreshed: None,
            send_as: None,
            notes: None,
            scope: None,
        }
    }
}

/// What a check does to the connection's quota scope, from its account's state at the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScopePause {
    /// The provider paused the account's sending: every connection of the scope waits a day,
    /// until the next check reads the account again, with this detail.
    Pause(String),
    /// The account sends: a pause a check set ([`SES_PAUSED`]) is lifted; a breaker's is not.
    Lift,
}

/// The beginning of the detail of a scope a check paused because Amazon SES paused the account's
/// sending: a later check lifts only a pause carrying it, never one a breaker set.
const SES_PAUSED: &str = "Amazon SES paused sending for this account";

/// The relay account checks that follow a passed login (see the module): a SendGrid key's
/// permissions, read with the key the login uses; with API credentials, an Amazon SES account's
/// sending state in the connection's Region and its identities' verification. A provider that
/// cannot be asked now leaves the pass as it is: the login itself worked.
async fn check_account(
    env: &Env,
    loaded: &Loaded,
    smtp: &SmtpSettings,
    password: &SecretString,
) -> Verdict {
    let deadline = Instant::now() + BUDGET;
    let http = &env.settings.http;
    match (loaded.provider, &loaded.api) {
        (Provider::Sendgrid, _) => match sendgrid::scopes(http, password, &smtp.host, deadline).await {
            Ok(scopes) if scopes.iter().any(|scope| scope == sendgrid::MAIL_SEND) => {
                Verdict::passed()
            }
            Ok(_) => Verdict::Lost(
                "SendGrid's API key no longer has the Mail Send permission; give it Mail Send, or save a key that has it."
                    .to_owned(),
            ),
            Err(ApiError::Unauthorized) => Verdict::Lost(
                "SendGrid no longer knows the API key the connection logs in with (deleted or revoked); save a new key."
                    .to_owned(),
            ),
            Err(error) => {
                tracing::warn!(error = %error, "SendGrid's key permissions could not be read");
                Verdict::passed()
            }
        },
        (Provider::Ses, Some(api)) => check_ses(env, loaded, smtp, api, deadline).await,
        (Provider::Mailgun, Some(api)) => {
            match mailgun::check_key(http, &api.secret, &smtp.host, &smtp.username, deadline).await {
                Ok(()) => Verdict::passed(),
                Err(error) => {
                    let note = match error {
                        ApiError::Unauthorized | ApiError::Forbidden { .. } =>
                            "Mailgun refused the API key for this domain's events; save a key with event-read access. SMTP authentication passed.",
                        _ => "Mailgun's events API could not be checked; the next daily check will retry. SMTP authentication passed.",
                    };
                    Verdict::Passed {
                        refreshed: None,
                        send_as: None,
                        notes: Some(note.to_owned()),
                        scope: None,
                    }
                }
            }
        }
        _ => Verdict::passed(),
    }
}

/// Amazon SES's account checks with the connection's API credentials: `GetAccount` in the
/// Region of its SMTP host (paused sending pauses the quota scope; probation and the sandbox are
/// noted), then `GetEmailIdentity` for each identity, which confirms or withdraws its send-as.
async fn check_ses(
    env: &Env,
    loaded: &Loaded,
    smtp: &SmtpSettings,
    api: &ApiCredential,
    deadline: Instant,
) -> Verdict {
    let http = &env.settings.http;
    let noted = |note: &str| Verdict::Passed {
        refreshed: None,
        send_as: None,
        notes: Some(note.to_owned()),
        scope: None,
    };
    let Some(region) = ses::region_of(&smtp.host) else {
        return noted(
            "The SMTP host is not an Amazon SES endpoint (`email-smtp.<region>.amazonaws.com`), so the account's state was not read.",
        );
    };
    let Some(id) = &api.id else {
        return noted("The API credential has no AWS access key id; save it again with one.");
    };
    let key = AccessKey {
        id: id.clone(),
        secret: api.secret.clone(),
    };
    let mut notes = Vec::new();
    let scope = match ses::account(http, &key, &region, deadline).await {
        Ok(account) => {
            if account.enforcement == ses::Enforcement::Probation {
                notes.push(format!(
                    "Amazon SES has the account under review (probation) in {region}: keep bounces and complaints low, or AWS may pause its sending."
                ));
            }
            if !account.production_access {
                notes.push(format!(
                    "The account is in the Amazon SES sandbox in {region}: it may only mail verified addresses until AWS grants it production access."
                ));
            }
            if account.sending_enabled && account.enforcement != ses::Enforcement::Shutdown {
                Some(ScopePause::Lift)
            } else {
                let detail = format!(
                    "{SES_PAUSED} in {region}; every connection of the account waits until AWS resumes it (see the AWS console) and a check reads it again."
                );
                notes.push(detail.clone());
                Some(ScopePause::Pause(detail))
            }
        }
        Err(ApiError::Unauthorized | ApiError::Forbidden { .. }) => {
            return noted(
                "Amazon SES refused the API credentials (an unknown key, or one without `ses:GetAccount` and `ses:GetEmailIdentity`); the account checks wait for new ones.",
            );
        }
        Err(error) => {
            notes.push(format!(
                "Amazon SES's account could not be read ({error}); the next check reads it again."
            ));
            None
        }
    };
    let send_as = verified_identities(env, &key, &region, &loaded.identities, deadline).await;
    Verdict::Passed {
        refreshed: None,
        send_as,
        notes: (!notes.is_empty()).then(|| notes.join(" ")),
        scope,
    }
}

/// The addresses among `identities` Amazon SES lets the account send as in `region`: an address
/// verified itself, or through its verified domain (SES answers `404` for an address it holds
/// only as part of its domain). `None` when any read failed, so a partial answer never withdraws
/// the send-as of an identity that was not read.
async fn verified_identities(
    env: &Env,
    key: &AccessKey,
    region: &str,
    identities: &[(Uuid, String)],
    deadline: Instant,
) -> Option<Vec<String>> {
    let http = &env.settings.http;
    let mut verified = Vec::new();
    for (_, address) in identities {
        let found = match ses::identity(http, key, region, address, deadline)
            .await
            .ok()?
        {
            Some(identity) => identity.verified_for_sending,
            None => {
                let (_, domain) = address.rsplit_once('@')?;
                ses::identity(http, key, region, domain, deadline)
                    .await
                    .ok()?
                    .is_some_and(|identity| identity.verified_for_sending)
            }
        };
        if found {
            verified.push(address.clone());
        }
    }
    Some(verified)
}

/// Writes what the check found about the connection's quota scope, before the connection's row
/// is locked, since every path locks a scope before its connections.
async fn pause_scope(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    pause: &ScopePause,
) -> Result<(), sqlx::Error> {
    match pause {
        ScopePause::Pause(detail) => {
            sqlx::query!(
                "UPDATE quota_scopes s
                    SET paused_until = greatest(coalesce(s.paused_until, now()), now() + interval '1 day'),
                        paused_detail = $3, updated_at = now()
                   FROM connections c
                  WHERE c.workspace_id = $1 AND c.id = $2
                    AND s.workspace_id = c.workspace_id AND s.id = c.quota_scope_id",
                workspace.uuid(),
                connection.uuid(),
                detail,
            )
            .execute(&mut **tx)
            .await?;
        }
        ScopePause::Lift => {
            sqlx::query!(
                "UPDATE quota_scopes s
                    SET paused_until = NULL, paused_detail = NULL, updated_at = now()
                   FROM connections c
                  WHERE c.workspace_id = $1 AND c.id = $2
                    AND s.workspace_id = c.workspace_id AND s.id = c.quota_scope_id
                    AND starts_with(s.paused_detail, $3)",
                workspace.uuid(),
                connection.uuid(),
                SES_PAUSED,
            )
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

/// The OAuth checks: refresh, scopes, the mailbox's answer, Gmail's send-as list. The Gmail reads
/// are charged to maintenance's part of the Gmail limits first; Graph's `GET /me` is the identity
/// service's, another key, left to its own throttling.
async fn check_oauth(
    env: &Env,
    connection: Id<Connection>,
    provider: Provider,
    grant: Grant,
) -> Verdict {
    let Some((identity_provider, app, scopes)) = env.settings.apps.app(provider) else {
        return Verdict::Temporary(format!(
            "Norbelys's {} app is not configured on this deployment; the check runs again later.",
            provider.as_str()
        ));
    };
    let http = &env.settings.http;
    let deadline = Instant::now() + BUDGET;
    let tokens = match oauth::refresh(
        http,
        &identity_provider,
        app,
        &grant.refresh_token,
        scopes,
        deadline,
    )
    .await
    {
        Ok(tokens) => tokens,
        Err(error) => return refusal(provider, &app.client_id, error),
    };
    if let Err(error) = oauth::check_scopes(&tokens.scope, scopes) {
        return refusal(provider, &app.client_id, error);
    }
    let refreshed = Grant {
        refresh_token: tokens.refresh_token.unwrap_or(grant.refresh_token),
        access_token: tokens.access_token,
        expires_at: Timestamp(tokens.expires_at),
        scope: tokens.scope,
    };
    let spent = || {
        Verdict::Temporary(
            "Norbelys's share of Gmail's rate limits for checks is spent for now; the check runs again later."
                .to_owned(),
        )
    };
    let send_as = match provider {
        Provider::Google => {
            if env
                .limits
                .wait(connection, Read::GmailProfile, deadline)
                .await
                .is_err()
            {
                return spent();
            }
            if let Err(error) = gmail::profile(http, &refreshed.access_token, deadline).await {
                return answered(error);
            }
            if env
                .limits
                .wait(connection, Read::GmailProfile, deadline)
                .await
                .is_err()
            {
                return spent();
            }
            match gmail::send_as(http, &refreshed.access_token, deadline).await {
                Ok(addresses) => Some(
                    addresses
                        .into_iter()
                        .filter(|address| {
                            address.is_primary
                                || address.verification_status.as_deref() == Some("accepted")
                        })
                        .map(|address| address.send_as_email.to_ascii_lowercase())
                        .collect(),
                ),
                Err(error) => return answered(error),
            }
        }
        _ => {
            if let Err(error) = graph::me(http, &refreshed.access_token, deadline).await {
                return answered(error);
            }
            None
        }
    };
    Verdict::Passed {
        refreshed: Some(refreshed),
        send_as,
        notes: None,
        scope: None,
    }
}

/// The verdict of a token endpoint's refusal.
fn refusal(provider: Provider, client_id: &str, error: OAuthError) -> Verdict {
    match error {
        OAuthError::InvalidGrant { description, .. } => Verdict::Lost(format!(
            "The grant was revoked or expired ({description}); reconnect the mailbox."
        )),
        OAuthError::InteractionRequired { .. } => Verdict::Lost(
            "The account must sign in again (multifactor authentication or Conditional Access); reconnect the mailbox."
                .to_owned(),
        ),
        OAuthError::ScopeMissing { missing } => Verdict::Lost(format!(
            "The consent lacks {}; reconnect and allow every permission.",
            missing.join(", ")
        )),
        OAuthError::AdminPolicyEnforced { .. } => Verdict::Blocked(connection_oauth::admin_steps(
            provider, client_id,
        )),
        error @ (OAuthError::Refused { .. } | OAuthError::Invalid(_)) => {
            tracing::error!(error = %error, provider = provider.as_str(), "the identity provider refused Norbelys's own app");
            Verdict::Temporary(format!(
                "{} refused Norbelys's app; the operators are told and the check runs again later.",
                provider.as_str()
            ))
        }
        OAuthError::Unavailable(reason) => Verdict::Temporary(format!(
            "The token endpoint is unavailable ({reason}); the check runs again later."
        )),
    }
}

/// The verdict of a provider API's error right after a successful refresh.
fn answered(error: ApiError) -> Verdict {
    match error {
        ApiError::Unauthorized => Verdict::Lost(
            "The provider refused a freshly refreshed token; reconnect the mailbox.".to_owned(),
        ),
        ApiError::Forbidden { reason } => Verdict::Blocked(format!(
            "The provider refused the account ({reason}): an administrator restricted the mailbox's API access."
        )),
        other => Verdict::Temporary(format!(
            "The provider could not be asked ({other}); the check runs again later."
        )),
    }
}

/// The password checks: an SMTP `AUTH` probe, then an IMAP login when the connection is read.
async fn check_password(
    env: &Env,
    provider: Provider,
    smtp: &SmtpSettings,
    imap_settings: Option<&ImapSettings>,
    password: &SecretString,
) -> Verdict {
    // A managed MTA login is read from the MTA itself first: one it disabled, forgot or left
    // without an evidence route fails the check whatever `AUTH` says.
    if provider == Provider::Norbelys
        && let Some(control) = &env.settings.control
    {
        use super::provision::LoginState;
        match control.login(&smtp.username).await {
            Ok(LoginState::Active) => {}
            Ok(LoginState::Disabled) => {
                return Verdict::Blocked(
                    "The managed MTA disabled this login; verify the connection to enable it again."
                        .to_owned(),
                );
            }
            Ok(LoginState::Missing) => {
                return Verdict::Lost(
                    "The managed MTA has no such login; verify the connection to provision it again."
                        .to_owned(),
                );
            }
            Ok(LoginState::Unrouted) => {
                return Verdict::Lost(
                    "The managed MTA posts this login's evidence nowhere; verify the connection to register its route again."
                        .to_owned(),
                );
            }
            Err(error) => {
                return Verdict::Temporary(format!(
                    "The managed MTA could not be asked ({error}); the check runs again later."
                ));
            }
        }
    }
    let deadline = Instant::now() + BUDGET;
    let pool = if provider == Provider::Norbelys {
        &env.mta_smtp
    } else {
        &env.smtp
    };
    let server = SmtpServer {
        host: &smtp.host,
        port: smtp.port,
        security: match smtp.security {
            Security::Tls => SmtpSecurity::Tls,
            Security::Starttls => SmtpSecurity::StartTls,
            Security::Plain => SmtpSecurity::Plain,
        },
    };
    let auth = SmtpAuth::Password {
        username: &smtp.username,
        password,
    };
    match pool.probe(&server, &auth, deadline).await {
        Ok(()) => {}
        Err(rejection) if rejection.cause == Cause::Unauthorized => {
            return Verdict::Lost(format!(
                "The SMTP server refused the login ({}); save a new password.",
                rejection.diagnostic
            ));
        }
        Err(rejection) => {
            return Verdict::Temporary(format!(
                "The SMTP server could not be checked ({}); the check runs again later.",
                rejection.diagnostic
            ));
        }
    }
    if let Some(settings) = imap_settings {
        let server = ImapServer {
            host: &settings.host,
            port: settings.port,
            security: match settings.security {
                ImapSecurity::Tls => imap::ImapSecurity::Tls,
                ImapSecurity::Plain => imap::ImapSecurity::Plain,
            },
        };
        let auth = ImapAuth::Password {
            username: &smtp.username,
            password,
        };
        match imap::connect(&env.connector, &server, &auth, 0, deadline).await {
            Ok(session) => session.logout().await,
            Err(ImapError::Unauthorized(reason)) => {
                return Verdict::Lost(format!(
                    "The IMAP server refused the login ({reason}); save a new password."
                ));
            }
            Err(other) => {
                return Verdict::Temporary(format!(
                    "The IMAP server could not be checked ({other}); the check runs again later."
                ));
            }
        }
    }
    Verdict::passed()
}

/// Writes the verdict under the fence: nothing when the credential or the status changed since
/// the check read them (the check then runs again at once).
async fn record(
    cx: &mut JobContext,
    keys: &Keys,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    loaded: &Loaded,
    verdict: Verdict,
) -> Result<Outcome, JobError> {
    let mut chunk = cx.begin().await?;
    // A scope's row comes before its connections' in every path's lock order, so what the check
    // found about the account's scope is written before the connection is locked.
    if let Verdict::Passed {
        scope: Some(pause), ..
    } = &verdict
    {
        pause_scope(chunk.tx(), workspace, connection, pause).await?;
    }
    let current = sqlx::query!(
        "SELECT status, credential_version FROM connections WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_optional(&mut **chunk.tx())
    .await?;
    let fenced = current.is_some_and(|row| {
        row.status == loaded.status.as_str() && row.credential_version == loaded.version
    });
    if !fenced {
        return Ok(Outcome::Yield {
            after: Duration::ZERO,
        });
    }
    let (event, detail, temporary) = match &verdict {
        Verdict::Passed { notes, .. } => (Some(HealthEvent::CheckPassed), notes.as_deref(), None),
        Verdict::Lost(detail) => (
            Some(HealthEvent::CredentialLost),
            Some(detail.as_str()),
            None,
        ),
        Verdict::Blocked(detail) => (
            Some(HealthEvent::AccountBlocked),
            Some(detail.as_str()),
            None,
        ),
        Verdict::Temporary(detail) => (None, Some(detail.as_str()), Some(detail.clone())),
    };
    sqlx::query!(
        "UPDATE connections SET checked_at = now(), status_detail = $3 WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        connection.uuid(),
        detail,
    )
    .execute(&mut **chunk.tx())
    .await?;
    if let Verdict::Passed {
        refreshed, send_as, ..
    } = &verdict
    {
        if let Some(grant) = refreshed {
            let sealed = credentials::seal(
                keys,
                workspace,
                connection,
                &Credential::OAuth(grant.clone()),
            )
            .map_err(|error| JobError::Failed(error.to_string()))?;
            sqlx::query_scalar!(
                "SELECT set_connection_credential($1, $2, $3)",
                workspace.uuid(),
                connection.uuid(),
                sealed,
            )
            .fetch_one(&mut **chunk.tx())
            .await?;
        }
        if let Some(addresses) = send_as {
            let confirmed: Vec<Uuid> = loaded
                .identities
                .iter()
                .filter(|(_, key)| addresses.contains(key))
                .map(|(id, _)| *id)
                .collect();
            sqlx::query!(
                "UPDATE sender_identities
                    SET verified_at = CASE WHEN id = ANY($3) THEN coalesce(verified_at, now()) END
                  WHERE workspace_id = $1 AND connection_id = $2",
                workspace.uuid(),
                connection.uuid(),
                &confirmed,
            )
            .execute(&mut **chunk.tx())
            .await?;
        }
    }
    if let Some(event) = event {
        health::apply(
            chunk.tx(),
            workspace,
            connection,
            loaded.status,
            loaded.paused,
            event,
            detail,
        )
        .await?;
    }
    let outcome = match &verdict {
        Verdict::Passed { .. } => "passed",
        Verdict::Lost(_) => "lost",
        Verdict::Blocked(_) => "blocked",
        Verdict::Temporary(_) => "temporary",
    };
    cx.checkpoint(chunk, json!({ "verdict": outcome })).await?;
    match temporary {
        Some(detail) => Err(JobError::Failed(detail)),
        None => Ok(Outcome::Done),
    }
}

/// `connection.check_due`: every 5 minutes, enqueues `connection.check` for each active
/// connection (paused ones included) last checked a day ago or never, read from the connections
/// themselves so a connection with no mail queued is checked too. A connection in any other
/// status waits for its person. It calls no provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnectionCheckDue {}

impl Job for ConnectionCheckDue {
    const KIND: &'static str = "connection.check_due";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::Idempotent;
    const CLASS: Class = Class::FanOut;

    fn unique_key(&self) -> Option<String> {
        Some("singleton".to_owned())
    }

    fn schedule() -> Option<(&'static str, Self)> {
        Some(("*/5 * * * *", Self {}))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let mut directory = cx.directory().await?;
        let due = sqlx::query!(
            "SELECT workspace_id, id FROM connections
              WHERE status = 'active' AND (checked_at IS NULL OR checked_at < now() - interval '1 day')
              ORDER BY workspace_id, id LIMIT $1",
            DUE_PER_RUN,
        )
        .fetch_all(&mut *directory)
        .await?;
        directory.commit().await?;
        let mut by_workspace: HashMap<Uuid, Vec<ConnectionCheck>> = HashMap::new();
        for row in due {
            by_workspace
                .entry(row.workspace_id)
                .or_default()
                .push(ConnectionCheck {
                    connection: Id::from_uuid(row.id),
                });
        }
        let mut enqueued = cx
            .progress()
            .and_then(|progress| progress.get("enqueued"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        for (workspace, checks) in by_workspace {
            if cx.should_yield() {
                return Ok(Outcome::Yield {
                    after: Duration::ZERO,
                });
            }
            let workspace = WorkspaceId::trusted(workspace);
            let mut chunk = cx.begin_in(workspace).await?;
            let added = jobs::enqueue_many(chunk.tx(), workspace, &checks, None).await?;
            enqueued = enqueued.saturating_add(added);
            cx.checkpoint(chunk, json!({ "enqueued": enqueued }))
                .await?;
        }
        if enqueued > 0 {
            jobs::wake(cx.db(), Queue::Maintenance).await;
        }
        Ok(Outcome::Done)
    }
}
