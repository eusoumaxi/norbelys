//! Reconciliation: settling an `uncertain` message, never by sending it again.
//!
//! A message is `uncertain` when its submission ended without a readable final answer (the reply
//! after the content was lost, a request timed out after it was sent) or when the sender that
//! started it was lost. The provider may have taken it, so it is never resubmitted automatically:
//! sending it again could deliver it twice. Only evidence settles it, through
//! [`super::evidence::record`], which makes it `sent` (trustworthy evidence that the provider
//! took it) or `failed` (a person's refusal):
//!
//! - **A person** ([`resolve`], `POST /v1/messages/{id}/resolve`): their evidence is recorded as a
//!   `manual` event with the state they chose.
//! - **The mailbox's Sent folder** ([`sent_folder`]), the last step of the mailbox's
//!   `connection.check`, its one maintenance job, which a finish or a recovery that leaves a
//!   message `uncertain` asks for: each `uncertain` message of the connection is searched by its
//!   Message-ID (Gmail's `messages.list` with `rfc822msgid:`, Graph's Sent Items filtered on
//!   `internetMessageId`, IMAP `SEARCH HEADER Message-ID` in the folder the server marks
//!   `\Sent`, RFC 6154). Found, it is `sent` with an `authenticated` `accepted` event from
//!   `sent_folder`; not found, it stays `uncertain` (finding nothing proves nothing: a search index
//!   may lag, and a generic SMTP server need not copy what it accepts anywhere). The step reads
//!   the connection's messages again until none is left unvisited, so only a message that became
//!   `uncertain` after its last read waits for the next check. A password mailbox read without
//!   IMAP has no folder to search: its messages wait for a person's `resolve`.
//! - **A relay's or the managed MTA's later event** naming the message, applied by the evidence
//!   normaliser.
//!
//! # Fencing and bounds
//!
//! The step reads the connection's credential and its version first and writes nothing unless
//! both the version and the status are unchanged under the connection's lock: a credential
//! replaced while it searched makes it stop, and the check runs again on the current one. Its
//! requests are made one at a time, each with a deadline, in chunks that end well inside the
//! job's lease (renewed by each chunk's checkpoint), so no request outlives the lease it was made
//! under; the visited messages are kept in the job's progress, so a yielded check resumes where
//! it stopped.
//!
//! Lock order: the connection's row, then message rows (the settlement), as every delivery path.

use std::time::Duration;

use norbelys_mail::imap::{self, ImapAuth, ImapServer};
use norbelys_mail::{gmail, graph};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use super::evidence::{self, Evidence};
use super::limits::Read;
use crate::crypto::Keys;
use crate::db::Tx;
use crate::domain::ids::{Connection, Id, Message, WorkspaceId};
use crate::domain::policy::delivery::{Category, Confidence, EventKind, RecipientRef, Source};
use crate::domain::senders::{Provider, Status};
use crate::jobs::{JobContext, JobError, Outcome};
use crate::senders::Env;
use crate::senders::connections::{ImapSecurity, ImapSettings, SmtpSettings};
use crate::senders::credentials::{self, Credential};

/// Messages searched per chunk.
const PAGE: i64 = 20;
/// How long a chunk's searches may take in all: well inside the job's 60-second lease.
const CHUNK_BUDGET: Duration = Duration::from_secs(40);
/// How long one search may take.
const REQUEST_BUDGET: Duration = Duration::from_secs(10);
/// Messages one check visits at most; the rest wait for the next check (the progress that keeps
/// the visited ones stays far below a job payload's bound).
const VISITS_MAX: usize = 1_000;

/// How a person settles an `uncertain` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// The provider took it.
    Sent,
    /// It was not sent.
    Failed,
}

/// Why a message could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// No such message in the workspace (`404 not_found`).
    #[error("no such message")]
    NotFound,
    /// The message is not `uncertain` (`409 invalid_state`).
    #[error("only an uncertain message can be resolved; this one is {0}")]
    NotUncertain(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Settles `message` of `workspace`, which must be `uncertain`, as `resolution`, recording the
/// person's `evidence` as a `manual` event by `actor`, in the caller's transaction: the state,
/// the counters and `message.sent` or `message.failed` follow from the event
/// ([`super::evidence::record`]).
///
/// # Errors
///
/// [`ResolveError::NotFound`], [`ResolveError::NotUncertain`] naming its state, or the database
/// refused.
pub async fn resolve(
    tx: &mut Tx,
    workspace: WorkspaceId,
    message: Id<Message>,
    resolution: Resolution,
    evidence: &str,
    actor: &str,
) -> Result<(), ResolveError> {
    let row = sqlx::query!(
        "SELECT state, thread_id FROM messages WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        workspace.uuid(),
        message.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(ResolveError::NotFound)?;
    if row.state != "uncertain" {
        return Err(ResolveError::NotUncertain(row.state));
    }
    let (kind, category) = match resolution {
        Resolution::Sent => (EventKind::Accepted, Category::Accepted),
        Resolution::Failed => (EventKind::Rejected, Category::Rejected),
    };
    evidence::record(
        tx,
        workspace,
        &[Evidence {
            message: Some(message),
            thread: row.thread_id,
            attempt_number: None,
            recipient: None,
            recipient_ref: RecipientRef::Unknown,
            source: Source::Manual,
            source_event_id: format!("resolve:{actor}"),
            received_via: None,
            kind,
            action: None,
            phase: None,
            enhanced_status: None,
            category,
            diagnostic: Some(evidence.to_owned()),
            confidence: Confidence::Authenticated,
            receipt: None,
            observed_at: crate::process::now(),
        }],
    )
    .await?;
    Ok(())
}

/// Where the Sent-folder step stands, kept in the check's progress.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Progress {
    /// The credential version the step started on.
    version: i64,
    /// The messages already searched in this check.
    visited: Vec<Uuid>,
    /// How many it found and settled.
    settled: u64,
}

/// What the step reads before it calls anyone.
struct Loaded {
    provider: Provider,
    version: i64,
    smtp: Option<SmtpSettings>,
    imap: Option<ImapSettings>,
    credential: Option<Credential>,
}

/// True when the check's progress says its Sent-folder step was under way: a resumed check goes
/// straight back to it.
#[must_use]
pub fn resuming(progress: Option<&Value>) -> bool {
    progress.is_some_and(|progress| progress.get("reconcile").is_some())
}

/// The last step of a mailbox's `connection.check` (see the module): searches the connection's
/// `uncertain` messages in its Sent folder, one at a time, and settles those it finds. Returns
/// the check's outcome: done once none is left unvisited (or the folder cannot be searched), a
/// yield at a quantum's end (the progress keeps what was visited).
///
/// # Errors
///
/// The database failed, or the job's lease was lost.
pub async fn sent_folder(
    cx: &mut JobContext,
    workspace: WorkspaceId,
    connection: Id<Connection>,
) -> Result<Outcome, JobError> {
    let keys = cx.env::<Keys>()?.clone();
    let env = cx.env::<Env>()?.clone();
    let Some(loaded) = load(cx, &keys, workspace, connection).await? else {
        return Ok(Outcome::Done);
    };
    let mut progress: Progress = cx
        .progress()
        .and_then(|progress| progress.get("reconcile"))
        .and_then(|reconcile| serde_json::from_value(reconcile.clone()).ok())
        .unwrap_or_else(|| Progress {
            version: loaded.version,
            ..Progress::default()
        });
    if progress.version != loaded.version {
        // The credential changed since the step started: run the whole check again on it.
        let chunk = cx.begin().await?;
        cx.checkpoint(chunk, json!({})).await?;
        return Ok(Outcome::Yield {
            after: Duration::ZERO,
        });
    }
    loop {
        if cx.should_yield() {
            return Ok(Outcome::Yield {
                after: Duration::ZERO,
            });
        }
        if progress.visited.len() >= VISITS_MAX {
            return Ok(Outcome::Done);
        }
        let mut read = cx.db().begin_in(workspace).await?;
        let page = sqlx::query!(
            "SELECT id, internet_message_id FROM messages
              WHERE workspace_id = $1 AND connection_id = $2 AND state = 'uncertain' AND NOT (id = ANY($3))
              ORDER BY id LIMIT $4",
            workspace.uuid(),
            connection.uuid(),
            &progress.visited,
            PAGE,
        )
        .fetch_all(&mut *read)
        .await?;
        read.commit().await?;
        if page.is_empty() {
            return Ok(Outcome::Done);
        }
        let deadline = Instant::now() + CHUNK_BUDGET;
        let candidates: Vec<(Uuid, String)> = page
            .into_iter()
            .map(|row| (row.id, row.internet_message_id))
            .collect();
        let Some(search) = search(&env, connection, &loaded, &candidates, deadline).await else {
            // The folder cannot be searched now (or at all): nothing more this check.
            return Ok(Outcome::Done);
        };

        let mut chunk = cx.begin().await?;
        let fenced = sqlx::query!(
            "SELECT status, credential_version FROM connections WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
            workspace.uuid(),
            connection.uuid(),
        )
        .fetch_optional(&mut **chunk.tx())
        .await?
        .is_some_and(|row| {
            row.status == Status::Active.as_str() && row.credential_version == loaded.version
        });
        if !fenced {
            drop(chunk);
            return Ok(Outcome::Done);
        }
        let now = crate::process::now();
        let found: Vec<Evidence> = search
            .found
            .iter()
            .map(|message| Evidence {
                message: Some(Id::from_uuid(*message)),
                thread: None,
                attempt_number: None,
                recipient: None,
                recipient_ref: RecipientRef::Unknown,
                source: Source::SentFolder,
                source_event_id: format!("sent_folder:{message}"),
                received_via: None,
                kind: EventKind::Accepted,
                action: None,
                phase: None,
                enhanced_status: None,
                category: Category::Accepted,
                diagnostic: None,
                confidence: Confidence::Authenticated,
                receipt: None,
                observed_at: now,
            })
            .collect();
        evidence::record(chunk.tx(), workspace, &found).await?;
        progress.visited.extend(search.visited);
        progress.settled = progress
            .settled
            .saturating_add(u64::try_from(found.len()).unwrap_or(u64::MAX));
        cx.checkpoint(chunk, json!({ "verdict": "passed", "reconcile": progress }))
            .await?;
        if search.stopped {
            return Ok(Outcome::Done);
        }
    }
}

/// What one chunk's searches found.
struct Search {
    /// Messages searched (found or not).
    visited: Vec<Uuid>,
    /// Messages found in the Sent folder.
    found: Vec<Uuid>,
    /// The provider stopped answering: what was searched is recorded, the rest waits.
    stopped: bool,
}

/// Reads the connection, its credential through `connection_credential()` and its settings;
/// `None` when it is not an active mailbox.
async fn load(
    cx: &JobContext,
    keys: &Keys,
    workspace: WorkspaceId,
    connection: Id<Connection>,
) -> Result<Option<Loaded>, JobError> {
    let mut tx = cx.db().begin_in(workspace).await?;
    let row = sqlx::query!(
        "SELECT provider, status, credential_version, smtp, imap, connection_credential(workspace_id, id) AS credential
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
    let Ok(provider) = row.provider.parse::<Provider>() else {
        return Ok(None);
    };
    if !provider.is_mailbox() || row.status != Status::Active.as_str() {
        return Ok(None);
    }
    let credential = row
        .credential
        .map(|sealed| credentials::open(keys, workspace, connection, &sealed))
        .transpose()
        .map_err(|error| JobError::Failed(format!("the credential cannot be opened: {error}")))?;
    Ok(Some(Loaded {
        provider,
        version: row.credential_version,
        smtp: row.smtp.and_then(|smtp| serde_json::from_value(smtp).ok()),
        imap: row.imap.and_then(|imap| serde_json::from_value(imap).ok()),
        credential,
    }))
}

/// Searches each candidate's Message-ID in the connection's Sent folder, one request at a time,
/// until `deadline`; `None` when the folder cannot be searched at all (a password mailbox without
/// IMAP, a server that marks no Sent folder, a credential that cannot be used).
async fn search(
    env: &Env,
    connection: Id<Connection>,
    loaded: &Loaded,
    candidates: &[(Uuid, String)],
    deadline: Instant,
) -> Option<Search> {
    let mut result = Search {
        visited: Vec::new(),
        found: Vec::new(),
        stopped: false,
    };
    match (&loaded.credential, loaded.provider) {
        (Some(Credential::OAuth(grant)), Provider::Google | Provider::Microsoft) => {
            for (message, internet_message_id) in candidates {
                if Instant::now() >= deadline {
                    break;
                }
                // Each search is charged first to maintenance's part of the provider's limits
                // (`messages.list`, 5 Gmail units; one Graph request); a share spent past the
                // chunk's deadline leaves the rest for the next check.
                let read = if loaded.provider == Provider::Google {
                    Read::GmailList
                } else {
                    Read::Graph
                };
                if env.limits.wait(connection, read, deadline).await.is_err() {
                    result.stopped = true;
                    break;
                }
                let request = deadline.min(Instant::now() + REQUEST_BUDGET);
                let answer = if loaded.provider == Provider::Google {
                    gmail::find_sent(
                        &env.settings.http,
                        &grant.access_token,
                        internet_message_id,
                        request,
                    )
                    .await
                } else {
                    graph::find_sent(
                        &env.settings.http,
                        &grant.access_token,
                        internet_message_id,
                        request,
                    )
                    .await
                };
                match answer {
                    Ok(found) => {
                        result.visited.push(*message);
                        if found {
                            result.found.push(*message);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "a Sent-folder search failed; the rest waits for the next check");
                        result.stopped = true;
                        break;
                    }
                }
            }
            Some(result)
        }
        (Some(Credential::Password(password)), Provider::Smtp) => {
            let (Some(settings), Some(smtp)) = (&loaded.imap, &loaded.smtp) else {
                return None;
            };
            imap_search(
                env,
                settings,
                &smtp.username,
                password,
                candidates,
                deadline,
            )
            .await
        }
        _ => None,
    }
}

/// The IMAP searches of a password mailbox: one session for the chunk, in the folder the server
/// marks `\Sent`.
async fn imap_search(
    env: &Env,
    settings: &ImapSettings,
    username: &str,
    password: &SecretString,
    candidates: &[(Uuid, String)],
    deadline: Instant,
) -> Option<Search> {
    let server = ImapServer {
        host: &settings.host,
        port: settings.port,
        security: match settings.security {
            ImapSecurity::Tls => imap::ImapSecurity::Tls,
            ImapSecurity::Plain => imap::ImapSecurity::Plain,
        },
    };
    let auth = ImapAuth::Password { username, password };
    let mut session = match imap::connect(&env.connector, &server, &auth, 0, deadline).await {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(error = %error, "the IMAP server could not be reached for the Sent-folder search");
            return None;
        }
    };
    let folder = match session.sent_folder().await {
        Ok(Some(folder)) => folder,
        Ok(None) | Err(_) => {
            session.logout().await;
            return None;
        }
    };
    let mut result = Search {
        visited: Vec::new(),
        found: Vec::new(),
        stopped: false,
    };
    for (message, internet_message_id) in candidates {
        if Instant::now() >= deadline {
            break;
        }
        match session.find_message_id(&folder, internet_message_id).await {
            Ok(found) => {
                result.visited.push(*message);
                if found {
                    result.found.push(*message);
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "a Sent-folder search failed; the rest waits for the next check");
                result.stopped = true;
                break;
            }
        }
    }
    session.logout().await;
    Some(result)
}

#[cfg(test)]
mod tests;
