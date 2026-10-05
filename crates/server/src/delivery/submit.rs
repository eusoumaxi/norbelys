//! The transports: how a prepared message reaches the provider behind its connection, and how the
//! provider's answer becomes the facts the Finish records.
//!
//! # One transport per way in
//!
//! | Connection | Transport | Concurrency, per replica |
//! |---|---|---|
//! | `google` | Gmail API `users.messages.send` with the raw MIME | 8 requests at once (host slots) |
//! | `microsoft` | Microsoft Graph `POST /me/sendMail` with the raw MIME | 8 requests at once |
//! | `smtp` (a mailbox with a password) | SMTP, the mailbox pool | 1 session per credential |
//! | `ses`, `sendgrid`, `mailgun` | SMTP to the relay's endpoint, one pool per relay | 8 sessions per credential |
//! | `norbelys` (the managed MTA) | SMTP on the private network | 8 sessions per credential |
//!
//! Across the SMTP pools a replica holds at most [`SMTP_SESSIONS`] sessions, parked or in use: the
//! pools share one cap (`norbelys_mail::smtp::SessionCap`), and a pool that finds it full closes
//! the longest-idle session of any of them to take its place.
//!
//! The protocol work (phases, deadlines, the reply rules, per-recipient refusals) is the mail
//! crate's; this module chooses the transport, holds the pools and the access tokens, and maps
//! answers. Each relay has its own pool because each asks for its own session handling: Amazon
//! SES closes idle sessions after about 10 seconds and asks senders to rotate sessions, SendGrid
//! allows 5,000 messages per connection, and their final `250` names the accepted message
//! differently (<https://docs.aws.amazon.com/ses/latest/dg/smtp-connect.html>,
//! <https://www.twilio.com/docs/sendgrid/for-developers/sending-email/smtp-errors-and-troubleshooting>).
//! Mailboxes keep `lettre`'s 60-second idle timeout: their cold sends are minutes apart.
//!
//! # Two steps: acquire, then submit
//!
//! What can fail before anything is sent happens in [`Transports::acquire`], under the claim's
//! lease and before the Start: opening and authenticating an SMTP session, or making sure an
//! access token is fresh (refreshing it with the provider and storing the new grant sealed). A
//! refusal there is a fact about the connection (its credential, its host), never about the
//! message, and the Finish counts it on the connection's breaker. [`Lane::submit`] then runs
//! inside the deadline the Start fixed.
//!
//! # Test mode: the fake transport
//!
//! A workspace in test mode never reaches a provider: its messages go through [`Lane::Fake`],
//! which opens no connection. So that integrations can exercise every outcome, the local part of
//! each envelope recipient decides, by its first word (case-insensitive, before any `+` tag):
//!
//! | Local part starts with | Outcome |
//! |---|---|
//! | `uncertain` | the reply after the content was lost: `uncertain`, never retried automatically |
//! | `throttle` | `421 4.7.0`: the connection is asked to slow down (`transient`, connection-scoped) |
//! | `defer` | `451 4.3.0`: refused for now (`transient`), tried again later |
//! | `blocked` | `550 5.7.1`: refused by policy for good (`permanent`) |
//! | `bounce` | that recipient is refused at `RCPT TO` with `550 5.1.1`; the others receive it, and if none is left the message fails (`permanent`) |
//! | anything else | accepted, with a provider id `fake-<uuid>` |
//!
//! When recipients name several outcomes, the first in this table wins.
//!
//! # Lock order
//!
//! Refreshing a token takes the connection's row (`FOR UPDATE`) only to compare its
//! `credential_version` with the one the grant was read at, and writes the new grant through
//! `set_connection_credential()`; no other lock is held, and no transaction is open during a
//! network call.

use std::sync::Arc;
use std::time::Duration;

use norbelys_mail::http::HttpClient;
use norbelys_mail::net::{AddressPolicy, Connector};
use norbelys_mail::oauth::OAuthError;
use norbelys_mail::smtp::{
    PoolConfig, ReplyId, SessionCap, SmtpAuth, SmtpPool, SmtpSecurity, SmtpServer, SmtpSession,
};
use norbelys_mail::submission::{self as mail, Envelope, RecipientRefusal, Rejection, Submission};
use norbelys_mail::{gmail, graph};
use secrecy::SecretString;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use uuid::Uuid;

use super::finish::{Answered, RefusedRecipient};
use crate::crypto::Keys;
use crate::db::Database;
use crate::domain::ids::{Connection, Id, WorkspaceId};
use crate::domain::policy::delivery::{
    Answer, Cause, Enhanced, Failure, Phase, Refusal, RefusalScope, Source,
};
use crate::domain::senders::Provider;
use crate::domain::time::Timestamp;
use crate::rendering::Prepared;
use crate::senders::connections::{Security, SmtpSettings};
use crate::senders::credentials::{self, Credential};
use crate::senders::oauth::Apps;
use crate::senders::tokens::{Grantee, TokenError, Tokens};

/// HTTP requests to one provider API at once, per replica.
const HOST_SLOTS: usize = 8;
/// SMTP sessions one sender replica holds open at once across all its pools, parked or in use:
/// the sockets and TLS state a replica's memory budget allows. A busy pool closes another's
/// longest-idle session to take its place.
const SMTP_SESSIONS: usize = 48;

/// What the transports are built from.
#[derive(Clone)]
pub struct Config {
    /// The deployment's keys: credentials are sealed with them.
    pub keys: Keys,
    /// The client of every provider API.
    pub http: HttpClient,
    /// Norbelys's OAuth apps, to refresh mailbox grants.
    pub apps: Apps,
    /// The process's resolver.
    pub resolver: crate::dns::Resolver,
    /// Whether tenant hosts may be private addresses and plaintext (development only).
    pub allow_private_hosts: bool,
}

/// The sender's transports: SMTP pools per way in, the provider APIs' host slots and the cache
/// of fresh access tokens. Built once per process; cheap to clone.
#[derive(Clone)]
pub struct Transports {
    http: HttpClient,
    mailbox: SmtpPool,
    ses: SmtpPool,
    sendgrid: SmtpPool,
    mailgun: SmtpPool,
    mta: SmtpPool,
    gmail_slots: Arc<Semaphore>,
    graph_slots: Arc<Semaphore>,
    tokens: Tokens,
}

impl std::fmt::Debug for Transports {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Transports").finish_non_exhaustive()
    }
}

/// Why the transports could not be built.
#[derive(Debug, thiserror::Error)]
#[error("the TLS configuration of mail sessions could not be built: {0}")]
pub struct BuildError(#[from] norbelys_mail::net::ConnectorError);

impl Transports {
    /// The transports `config` describes.
    ///
    /// # Errors
    ///
    /// TLS cannot be configured.
    pub fn new(config: Config) -> Result<Self, BuildError> {
        let policy = if config.allow_private_hosts {
            AddressPolicy::Any
        } else {
            AddressPolicy::PublicOnly
        };
        let tenant = Connector::new(config.resolver.hickory(), policy)?;
        let private = Connector::new(config.resolver.hickory(), AddressPolicy::Any)?;
        let relay = |idle: u64, max_messages: u32, reply_id: ReplyId| PoolConfig {
            max_open: 8,
            max_idle: 8,
            idle_timeout: Duration::from_secs(idle),
            max_messages,
            max_age: Duration::from_secs(300),
            reply_id,
        };
        let cap = SessionCap::new(SMTP_SESSIONS);
        Ok(Self {
            tokens: Tokens::new(config.keys, config.http.clone(), config.apps),
            http: config.http,
            mailbox: SmtpPool::capped(
                tenant.clone(),
                PoolConfig {
                    max_open: 1,
                    max_idle: 1,
                    ..PoolConfig::default()
                },
                &cap,
            ),
            // SES closes a session idle for about 10 seconds; stay below it.
            ses: SmtpPool::capped(tenant.clone(), relay(8, 100, ReplyId::SesToken), &cap),
            // SendGrid allows 5,000 messages per connection.
            sendgrid: SmtpPool::capped(tenant.clone(), relay(30, 4_000, ReplyId::QueuedAs), &cap),
            mailgun: SmtpPool::capped(tenant, relay(30, 1_000, ReplyId::None), &cap),
            mta: SmtpPool::capped(private, relay(30, 1_000, ReplyId::QueuedAs), &cap),
            gmail_slots: Arc::new(Semaphore::new(HOST_SLOTS)),
            graph_slots: Arc::new(Semaphore::new(HOST_SLOTS)),
        })
    }

    /// Opens what `target` needs to submit by `deadline`: an authenticated SMTP session, or a
    /// fresh access token and a host slot (see the module).
    ///
    /// # Errors
    ///
    /// A [`Rejection`] in the `connect` or `auth` phase, always `transient` and about the
    /// connection (or Norbelys's own app); nothing about the message was sent.
    pub async fn acquire(
        &self,
        db: &Database,
        target: &Target,
        deadline: Instant,
    ) -> Result<Lane, Rejection> {
        match target.provider {
            Provider::Google | Provider::Microsoft => {
                let token = self.access_token(db, target, deadline).await?;
                let slots = if target.provider == Provider::Google {
                    &self.gmail_slots
                } else {
                    &self.graph_slots
                };
                let permit = tokio::time::timeout_at(deadline, Arc::clone(slots).acquire_owned())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .ok_or_else(|| {
                        local(
                            Phase::Connect,
                            RefusalScope::Connection,
                            Cause::Deadline,
                            "no request slot to the provider freed before the deadline",
                        )
                    })?;
                Ok(if target.provider == Provider::Google {
                    Lane::Gmail {
                        token,
                        _permit: permit,
                    }
                } else {
                    Lane::Graph {
                        token,
                        _permit: permit,
                    }
                })
            }
            Provider::Smtp
            | Provider::Ses
            | Provider::Sendgrid
            | Provider::Mailgun
            | Provider::Norbelys => {
                let Some(smtp) = &target.smtp else {
                    return Err(local(
                        Phase::Connect,
                        RefusalScope::Connection,
                        Cause::Unsupported,
                        "the connection has no SMTP settings",
                    ));
                };
                let Some(Credential::Password(password)) = &target.credential else {
                    return Err(local(
                        Phase::Auth,
                        RefusalScope::Connection,
                        Cause::Unauthorized,
                        "the connection has no password",
                    ));
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
                let pool = match target.provider {
                    Provider::Ses => &self.ses,
                    Provider::Sendgrid => &self.sendgrid,
                    Provider::Mailgun => &self.mailgun,
                    Provider::Norbelys => &self.mta,
                    Provider::Smtp | Provider::Google | Provider::Microsoft => &self.mailbox,
                };
                pool.session(&server, &auth, deadline)
                    .await
                    .map(|session| Lane::Smtp(Box::new(session)))
            }
        }
    }

    /// A fresh access token for an OAuth mailbox, from the process's shared cache
    /// (`senders::tokens`), mapped to a refusal of the connection when none can be had.
    async fn access_token(
        &self,
        db: &Database,
        target: &Target,
        deadline: Instant,
    ) -> Result<SecretString, Rejection> {
        let grantee = Grantee {
            workspace: target.workspace,
            connection: target.connection,
            provider: target.provider,
            credential: target.credential.as_ref(),
            version: target.version,
        };
        self.tokens
            .access_token(db, &grantee, deadline)
            .await
            .map_err(|error| match error {
                TokenError::NoGrant => local(
                    Phase::Auth,
                    RefusalScope::Connection,
                    Cause::Unauthorized,
                    "the connection has no OAuth grant; reconnect the mailbox",
                ),
                TokenError::NoApp => local(
                    Phase::Auth,
                    RefusalScope::Platform,
                    Cause::NoReply,
                    "Norbelys's OAuth app for this provider is not configured on this deployment",
                ),
                TokenError::Refresh(error) => refresh_refusal(error),
            })
    }
}

/// What a submission through one connection needs: its provider, its SMTP settings and its
/// credential, read once per wave.
#[derive(Clone)]
pub struct Target {
    /// The connection's workspace.
    pub workspace: WorkspaceId,
    /// The connection.
    pub connection: Id<Connection>,
    /// How it is reached.
    pub provider: Provider,
    /// Its SMTP endpoint, for the SMTP ways in.
    pub smtp: Option<SmtpSettings>,
    /// Its credential, opened.
    pub credential: Option<Credential>,
    /// The `credential_version` the credential was read at.
    pub version: i64,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Target")
            .field("connection", &self.connection)
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

/// Why a connection's target could not be read.
#[derive(Debug, thiserror::Error)]
pub enum TargetError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    /// The stored credential does not open, or the provider is not understood.
    #[error("{0}")]
    Invalid(String),
}

/// Reads what submitting through `connection` needs, as the worker inside its workspace;
/// `None` when the connection is gone.
///
/// # Errors
///
/// The database refused, or the stored credential does not open with this deployment's keys.
pub async fn target(
    db: &Database,
    keys: &Keys,
    workspace: WorkspaceId,
    connection: Id<Connection>,
) -> Result<Option<Target>, TargetError> {
    let mut tx = db.begin_in(workspace).await?;
    let row = sqlx::query!(
        "SELECT provider, smtp, credential_version, connection_credential(workspace_id, id) AS credential
           FROM connections WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        connection.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let provider = row
        .provider
        .parse::<Provider>()
        .map_err(|_| TargetError::Invalid(format!("unknown provider `{}`", row.provider)))?;
    let credential = row
        .credential
        .map(|sealed| credentials::open(keys, workspace, connection, &sealed))
        .transpose()
        .map_err(|error| TargetError::Invalid(format!("the credential does not open: {error}")))?;
    Ok(Some(Target {
        workspace,
        connection,
        provider,
        smtp: row.smtp.and_then(|smtp| serde_json::from_value(smtp).ok()),
        credential,
        version: row.credential_version,
    }))
}

/// An acquired way to submit one message.
pub enum Lane {
    /// An authenticated SMTP session (boxed: it is much larger than the other lanes).
    Smtp(Box<SmtpSession>),
    /// The Gmail API with a fresh token and a host slot.
    Gmail {
        token: SecretString,
        _permit: OwnedSemaphorePermit,
    },
    /// Microsoft Graph with a fresh token and a host slot.
    Graph {
        token: SecretString,
        _permit: OwnedSemaphorePermit,
    },
    /// The fake transport of a workspace in test mode.
    Fake,
}

impl std::fmt::Debug for Lane {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Smtp(_) => "Lane::Smtp",
            Self::Gmail { .. } => "Lane::Gmail",
            Self::Graph { .. } => "Lane::Graph",
            Self::Fake => "Lane::Fake",
        })
    }
}

impl Lane {
    /// Who answers through this lane, as evidence records it.
    #[must_use]
    pub fn source(&self) -> Source {
        match self {
            Self::Smtp(_) | Self::Fake => Source::Smtp,
            Self::Gmail { .. } | Self::Graph { .. } => Source::ProviderApi,
        }
    }

    /// The transport's budget for one whole submission: SMTP 300 seconds, an HTTP request 20.
    #[must_use]
    pub fn budget(&self) -> Duration {
        match self {
            Self::Smtp(_) | Self::Fake => Duration::from_secs(300),
            Self::Gmail { .. } | Self::Graph { .. } => norbelys_mail::http::REQUEST_TIMEOUT,
        }
    }

    /// Submits `prepared` by `deadline`, which the Start fixed.
    ///
    /// # Errors
    ///
    /// The provider's refusal, or the phase that lost its answer.
    pub async fn submit(
        self,
        http: &HttpClient,
        prepared: &Prepared,
        deadline: Instant,
    ) -> Result<Submission, Rejection> {
        match self {
            Self::Smtp(session) => {
                (*session)
                    .submit(&prepared.envelope, &prepared.raw, deadline)
                    .await
            }
            Self::Gmail { token, _permit } => {
                gmail::send(http, &token, &prepared.raw, deadline).await
            }
            Self::Graph { token, _permit } => {
                graph::send_mail(http, &token, &prepared.raw, deadline).await
            }
            Self::Fake => fake(&prepared.envelope),
        }
    }
}

impl Transports {
    /// The client the provider APIs are called through.
    #[must_use]
    pub fn http(&self) -> &HttpClient {
        &self.http
    }
}

/// The fake transport's answer for `envelope` (see the module's table).
fn fake(envelope: &Envelope) -> Result<Submission, Rejection> {
    let word = |address: &str| {
        let local = address.rsplit_once('@').map_or(address, |(local, _)| local);
        local
            .split('+')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    let words: Vec<(String, String)> = envelope
        .recipients()
        .iter()
        .map(|address| {
            let address = address.to_string();
            let word = word(&address);
            (address, word)
        })
        .collect();
    let has = |prefix: &str| words.iter().any(|(_, word)| word.starts_with(prefix));
    let reply = |failure, phase, scope, cause, code: u16, status: &str, text: &str| Rejection {
        failure,
        phase,
        scope,
        cause,
        code: Some(code),
        status: norbelys_mail::status::EnhancedStatus::find(status),
        retry_after: None,
        diagnostic: format!("{code} {status} {text} (test mode)"),
        refused: Vec::new(),
    };
    if has("uncertain") {
        return Err(Rejection {
            code: None,
            status: None,
            diagnostic: "the reply after the content was lost (test mode)".to_owned(),
            ..reply(
                mail::Failure::Uncertain,
                mail::Phase::Data,
                mail::Scope::Message,
                mail::Cause::NoReply,
                0,
                "",
                "",
            )
        });
    }
    if has("throttle") {
        return Err(reply(
            mail::Failure::Transient,
            mail::Phase::MailFrom,
            mail::Scope::Connection,
            mail::Cause::Throttled,
            421,
            "4.7.0",
            "Try again later",
        ));
    }
    if has("defer") {
        return Err(reply(
            mail::Failure::Transient,
            mail::Phase::Data,
            mail::Scope::Message,
            mail::Cause::Refused,
            451,
            "4.3.0",
            "Temporary failure",
        ));
    }
    if has("blocked") {
        return Err(reply(
            mail::Failure::Permanent,
            mail::Phase::Data,
            mail::Scope::Message,
            mail::Cause::Refused,
            550,
            "5.7.1",
            "Message refused by policy",
        ));
    }
    let refused: Vec<RecipientRefusal> = words
        .iter()
        .filter(|(_, word)| word.starts_with("bounce"))
        .map(|(recipient, _)| RecipientRefusal {
            recipient: recipient.clone(),
            code: 550,
            status: norbelys_mail::status::EnhancedStatus::find("5.1.1"),
            diagnostic: "550 5.1.1 No such user (test mode)".to_owned(),
        })
        .collect();
    if !refused.is_empty() && refused.len() == words.len() {
        return Err(Rejection {
            refused,
            ..reply(
                mail::Failure::Permanent,
                mail::Phase::RcptTo,
                mail::Scope::Recipient,
                mail::Cause::Refused,
                550,
                "5.1.1",
                "No such user",
            )
        });
    }
    Ok(Submission {
        provider_message_id: Some(format!("fake-{}", Uuid::now_v7().simple())),
        reply: Some("250 2.0.0 Ok (test mode)".to_owned()),
        refused,
    })
}

/// What the Finish records of a transport's answer.
#[must_use]
pub fn answered(
    result: Result<Submission, Rejection>,
    source: Source,
    started: Option<Timestamp>,
    recipients: Vec<String>,
) -> Answered {
    match result {
        Ok(submission) => Answered {
            answer: Answer::Accepted,
            source,
            started,
            diagnostic: submission.reply.unwrap_or_else(|| "accepted".to_owned()),
            provider_message_id: submission.provider_message_id,
            recipients,
            refused: submission.refused.iter().map(refused).collect(),
        },
        Err(rejection) => Answered {
            answer: Answer::Refused(refusal(&rejection)),
            source,
            started,
            diagnostic: rejection.diagnostic.clone(),
            provider_message_id: None,
            recipients,
            refused: rejection.refused.iter().map(refused).collect(),
        },
    }
}

/// The policy's facts of a mail-crate rejection.
#[must_use]
pub fn refusal(rejection: &Rejection) -> Refusal {
    Refusal {
        failure: match rejection.failure {
            mail::Failure::Transient => Failure::Transient,
            mail::Failure::Permanent => Failure::Permanent,
            mail::Failure::Uncertain => Failure::Uncertain,
        },
        phase: match rejection.phase {
            mail::Phase::Connect => Phase::Connect,
            mail::Phase::Auth => Phase::Auth,
            mail::Phase::MailFrom => Phase::MailFrom,
            mail::Phase::RcptTo => Phase::RcptTo,
            mail::Phase::Data => Phase::Data,
            mail::Phase::Api => Phase::Api,
        },
        scope: match rejection.scope {
            mail::Scope::Recipient => RefusalScope::Recipient,
            mail::Scope::Message => RefusalScope::Message,
            mail::Scope::Connection => RefusalScope::Connection,
            mail::Scope::QuotaScope => RefusalScope::QuotaScope,
            mail::Scope::Platform => RefusalScope::Platform,
        },
        cause: match rejection.cause {
            mail::Cause::Refused => Cause::Refused,
            mail::Cause::Throttled => Cause::Throttled,
            mail::Cause::Unauthorized => Cause::Unauthorized,
            mail::Cause::Forbidden => Cause::Forbidden,
            mail::Cause::NoReply => Cause::NoReply,
            mail::Cause::Deadline => Cause::Deadline,
            mail::Cause::Unsupported => Cause::Unsupported,
        },
        code: rejection.code,
        status: rejection.status.map(enhanced),
        retry_after: rejection.retry_after,
    }
}

fn enhanced(status: norbelys_mail::status::EnhancedStatus) -> Enhanced {
    Enhanced {
        class: status.class(),
        subject: status.subject(),
        detail: status.detail(),
    }
}

fn refused(refusal: &RecipientRefusal) -> RefusedRecipient {
    RefusedRecipient {
        recipient: refusal.recipient.clone(),
        code: refusal.code,
        status: refusal.status.map(enhanced),
        diagnostic: refusal.diagnostic.clone(),
    }
}

/// A refusal decided here, without a provider's reply.
fn local(phase: Phase, scope: RefusalScope, cause: Cause, diagnostic: &str) -> Rejection {
    Rejection {
        failure: mail::Failure::Transient,
        phase: match phase {
            Phase::Connect => mail::Phase::Connect,
            Phase::Auth => mail::Phase::Auth,
            Phase::MailFrom => mail::Phase::MailFrom,
            Phase::RcptTo => mail::Phase::RcptTo,
            Phase::Data => mail::Phase::Data,
            Phase::Api => mail::Phase::Api,
        },
        scope: match scope {
            RefusalScope::Recipient => mail::Scope::Recipient,
            RefusalScope::Message => mail::Scope::Message,
            RefusalScope::Connection => mail::Scope::Connection,
            RefusalScope::QuotaScope => mail::Scope::QuotaScope,
            RefusalScope::Platform => mail::Scope::Platform,
        },
        cause: match cause {
            Cause::Refused => mail::Cause::Refused,
            Cause::Throttled => mail::Cause::Throttled,
            Cause::Unauthorized => mail::Cause::Unauthorized,
            Cause::Forbidden => mail::Cause::Forbidden,
            Cause::NoReply => mail::Cause::NoReply,
            Cause::Deadline => mail::Cause::Deadline,
            Cause::Unsupported => mail::Cause::Unsupported,
        },
        code: None,
        status: None,
        retry_after: None,
        diagnostic: diagnostic.to_owned(),
        refused: Vec::new(),
    }
}

/// A token endpoint's refusal as a connection-scoped rejection in the `auth` phase: a lost grant
/// or consent is `unauthorized`, an administrator's block `forbidden`, Norbelys's own app
/// refused a platform fault, anything else a temporary unavailability.
fn refresh_refusal(error: OAuthError) -> Rejection {
    let (scope, cause, detail) = match &error {
        OAuthError::InvalidGrant { .. }
        | OAuthError::InteractionRequired { .. }
        | OAuthError::ScopeMissing { .. } => (
            RefusalScope::Connection,
            Cause::Unauthorized,
            "the mailbox's grant is no longer valid; reconnect it",
        ),
        OAuthError::AdminPolicyEnforced { .. } => (
            RefusalScope::Connection,
            Cause::Forbidden,
            "an administrator restricted the mailbox for this app",
        ),
        OAuthError::Refused { .. } | OAuthError::Invalid(_) => (
            RefusalScope::Platform,
            Cause::NoReply,
            "the identity provider refused Norbelys's app",
        ),
        OAuthError::Unavailable(_) => (
            RefusalScope::Connection,
            Cause::NoReply,
            "the token endpoint is unavailable",
        ),
    };
    local(Phase::Auth, scope, cause, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(recipients: &[&str]) -> Envelope {
        Envelope::new("sender@norbelys.test", recipients.iter().copied()).unwrap()
    }

    /// Each word of the fake transport's table gives its outcome, so a test-mode integration can
    /// exercise acceptance, a per-recipient refusal, a full refusal, a retry, a throttle and an
    /// uncertain ending without any provider.
    #[test]
    fn the_fake_transport_answers_by_the_recipients_local_part() {
        let outcome = |recipients: &[&str]| match fake(&envelope(recipients)) {
            Ok(submission) => format!("accepted refused={}", submission.refused.len()),
            Err(rejection) => format!(
                "{} {} {:?}",
                rejection.failure.as_str(),
                rejection.phase.as_str(),
                rejection.code
            ),
        };
        assert_eq!(outcome(&["ada@example.com"]), "accepted refused=0");
        assert_eq!(
            outcome(&["ada@example.com", "bounce@example.com"]),
            "accepted refused=1"
        );
        assert_eq!(
            outcome(&["bounce+1@example.com"]),
            "permanent rcpt_to Some(550)"
        );
        assert_eq!(outcome(&["defer@example.com"]), "transient data Some(451)");
        assert_eq!(
            outcome(&["Throttle@example.com"]),
            "transient mail_from Some(421)"
        );
        assert_eq!(
            outcome(&["blocked@example.com"]),
            "permanent data Some(550)"
        );
        assert_eq!(outcome(&["uncertain@example.com"]), "uncertain data None");
        // The first outcome in the table wins.
        assert_eq!(
            outcome(&["bounce@example.com", "uncertain@example.com"]),
            "uncertain data None"
        );
    }

    /// The mail crate's rejection keeps every fact in the policy's vocabulary: the phase, the
    /// scope (so a recipient's refusal never pauses the connection), the cause, the code and the
    /// enhanced status.
    #[test]
    fn a_rejection_keeps_its_facts_in_the_policy() {
        let Err(rejection) = fake(&envelope(&["throttle@example.com"])) else {
            panic!("a throttle is refused");
        };
        let facts = refusal(&rejection);
        assert_eq!(
            (
                facts.failure,
                facts.phase,
                facts.scope,
                facts.cause,
                facts.code
            ),
            (
                Failure::Transient,
                Phase::MailFrom,
                RefusalScope::Connection,
                Cause::Throttled,
                Some(421)
            )
        );
        assert_eq!(
            facts.status,
            Some(Enhanced {
                class: 4,
                subject: 7,
                detail: 0
            })
        );
        let answered = answered(
            fake(&envelope(&["ada@example.com", "bounce@example.com"])),
            Source::Smtp,
            None,
            vec![
                "ada@example.com".to_owned(),
                "bounce@example.com".to_owned(),
            ],
        );
        assert_eq!(answered.answer, Answer::Accepted);
        assert_eq!(answered.refused.len(), 1);
    }

    /// A lost grant is the connection's (`unauthorized`, so it asks for a reconnect), an
    /// administrator's block is `forbidden`, and a refusal of Norbelys's own app is a platform
    /// fault that must never count against the customer's connection.
    #[test]
    fn a_token_refusal_names_who_it_concerns() {
        let lost = refresh_refusal(OAuthError::InvalidGrant {
            description: "revoked".to_owned(),
            codes: Vec::new(),
        });
        assert_eq!(
            (lost.scope, lost.cause, lost.phase),
            (
                mail::Scope::Connection,
                mail::Cause::Unauthorized,
                mail::Phase::Auth
            )
        );
        let ours = refresh_refusal(OAuthError::Invalid("bad client".to_owned()));
        assert_eq!(ours.scope, mail::Scope::Platform);
    }
}
