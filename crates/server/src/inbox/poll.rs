//! Polling a receive binding: the claim under a fenced lease, one bounded page read from the
//! provider, and its storage with the cursor's advance in one transaction.
//!
//! # The claim
//!
//! Workspaces take turns: an inbox replica takes the turn of the workspace whose `inbox_turn_at`
//! is oldest among those with a due binding ([`turn`]), moves it to the back, and claims up to
//! its free permits of that workspace's due bindings ([`due`], [`claim`]). Everything here is
//! read as the scheduler role, which sees the routing and lease columns of every workspace and
//! nothing else (never a cursor, a body or a credential).
//!
//! One claim is one transaction, in the lock order archiving takes too: the connection's row
//! first, `FOR UPDATE SKIP LOCKED` (a row another process holds is skipped, never waited for),
//! then the binding's. It claims only a binding that is enabled, due and unleased, and only while
//! no binding of the same connection, enabled or not, holds a live lease: a mailbox is read by
//! one poll at a time, which keeps the provider's concurrency per mailbox shared with sending.
//! The lease lasts [`LEASE`] and bumps `lease_generation`, the fence of everything the poll
//! writes. Leases that expired (a replica died mid-poll) are cleared by [`recover`], which the
//! role runs every few seconds; the next claim re-reads from the last committed cursor.
//!
//! # One poll
//!
//! 1. **Read the binding** inside its workspace: folder, cursor, last poll, the connection's
//!    provider, settings and opened credential.
//! 2. **Fetch one page** ([`Reader::fetch`]): at most [`PAGE`] new messages after the cursor,
//!    each read up to [`MAX_MESSAGE_BYTES`], through IMAP (`UIDVALIDITY` and the last UID), the
//!    Gmail API (`history.list`) or Microsoft Graph (a delta link). Requests are sequential and
//!    every one ends by the poll's deadline, [`MARGIN`] before the lease expires, so no request
//!    outlives the lease it was made under. An OAuth mailbox's token comes from the process's
//!    token cache (`senders::tokens`). Every Gmail and Graph request is first charged its cost
//!    to the inbox's part of the provider's rate limits (`delivery::limits`, receiving's share
//!    divided by the inbox replicas), waiting for it within the same deadline; a share spent
//!    past the deadline fails the poll for now, like any other temporary failure.
//! 3. **Store it** ([`store`]): each message's raw bytes go to object storage under a key derived
//!    from its transport identity (a re-read overwrites the same object), then one transaction
//!    inserts the inbound rows, applies their effects and advances the cursor `WHERE lease_owner
//!    = $owner AND lease_generation = $generation`. When that fence fails (the lease was
//!    recovered, the binding disabled or archived) the whole transaction rolls back: nothing the
//!    poll read is recorded, and the next owner reads the page again.
//!
//! Duplicates are refused by the transport key (`UNIQUE (workspace_id, receive_binding_id,
//! transport_key)`), the only hard key of an inbound message: a re-read after a lost lease, or
//! the overlap of a resync, inserts nothing and repeats no effect. The content hash is kept as a
//! hint only, since two distinct messages can share a body.
//!
//! # Cursor resets
//!
//! A cursor the provider no longer honours (`UIDVALIDITY` changed, a Gmail history too old, an
//! expired Graph delta) restarts the read at the last poll minus [`RESYNC_OVERLAP`] (or
//! [`FIRST_SYNC`] ago for a binding never read): a bounded overlapping resync, never a jump to
//! the newest message. When the provider says the resync could not read everything since then,
//! the binding's `status_detail` says so, a visible gap.
//!
//! # When it is polled next
//!
//! `next_poll_at` is the poll's start plus the interval, now when the page came back full (a
//! backlog drains without waiting), and after a failure the inbox's retry wait, a jittered step
//! doubling from the interval up to an hour (`domain::inbox::next_poll`, `domain::retry::inbox`);
//! a provider's `Retry-After` can extend that wait. Client-wide throttling also pauses this
//! replica's provider budget; a failure keeps its reason in `status_detail`, and a refused
//! credential enqueues the connection's check (`connection.check`), which moves its health.
//!
//! # Effects of new messages
//!
//! A page's new messages are first judged without writing anything (correlated and classified,
//! the ones whose transport key is stored already left out). Their effects then follow the lock
//! order of the paths they meet:
//!
//! 1. **The stop rules**, each in a transaction that holds nothing else, because they lock
//!    enrollments before queue rows and messages, as every path that stops or advances an
//!    enrollment does: a person's answer to a campaign thread ends the person's enrollments under
//!    the campaign's rules (`campaigns::enrollments::stop_for_reply`), and an unsubscribe request
//!    ends every live enrollment of its sender (`stop_suppressed`). Both are idempotent, so a page
//!    whose transaction later fails and is read again repeats nothing, and a stop that happened is
//!    true whoever read the mail.
//! 2. **The page's transaction**: the inbound rows; then the evidence of reports and unsubscribe
//!    requests (`inbox::classify`; message rows before holds and suppressions), a review the
//!    evidence asks for becoming the message's proposal; then for each message the reply counter,
//!    the thread (latest activity, unread, open again when a person answered; threads after
//!    message rows, as the finish of a submission takes them), the person's `replied_at`,
//!    `inbound_message.received` for the customer, and `inbox.classify` when the rules left it
//!    open and the workspace turned AI classification on; last the cursor's fenced advance.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use bytes::Bytes;
use jiff::SignedDuration;
use norbelys_mail::http::HttpClient;
use norbelys_mail::imap::{self, ImapAuth, ImapCursor, ImapServer};
use norbelys_mail::inbound::{self, Inbound};
use norbelys_mail::net::Connector;
use norbelys_mail::receive::{RawMessage, Reset, ResetReason, TransportIdentity};
use norbelys_mail::{gmail, graph};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use super::classify::{self, Classify, Origin};
use super::correlate::{self, Found};
use crate::campaigns::enrollments;
use crate::crypto::{self, Keys};
use crate::db::{self, Database, Tx};
use crate::delivery::evidence;
use crate::delivery::limits::{Exhausted, Limits, Read};
use crate::domain::ids::{Connection, Id, InboundMessage, ReceiveBinding, WorkspaceId};
use crate::domain::inbox::{self as rules, Authority, Polled, ReviewProposal, Verdict};
use crate::domain::policy::delivery::{Metric, RecipientEffect};
use crate::domain::senders::Provider;
use crate::domain::time::Timestamp;
use crate::jobs::{self, Queue};
use crate::senders::check::ConnectionCheck;
use crate::senders::connections::{ImapSecurity, ImapSettings, SmtpSettings};
use crate::senders::credentials::{self, Credential};
use crate::senders::tokens::{Grantee, TokenError, Tokens};
use crate::storage::{Storage, StorageError};
use crate::webhooks::EventType;
use crate::webhooks::outbox::{self, Event};

/// How long a claim leases its binding.
pub const LEASE: Duration = Duration::from_secs(300);
/// Every provider request ends this long before the lease expires.
pub const MARGIN: Duration = Duration::from_secs(30);
/// The most new messages one poll reads.
pub const PAGE: u32 = 50;
/// The most bytes of one message read and kept; a larger message is marked truncated.
pub const MAX_MESSAGE_BYTES: usize = 1 << 20;
/// The most characters of the text body kept in the row.
pub const EXCERPT_CHARS: usize = 4_000;
/// How far before the last poll a resync restarts.
pub const RESYNC_OVERLAP: SignedDuration = SignedDuration::from_hours(1);
/// How far back the first read of a binding goes.
pub const FIRST_SYNC: SignedDuration = SignedDuration::from_hours(24);

static POLLS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_inbox_polls_total")
        .with_description("Polls of receive bindings, by provider and outcome.")
        .build()
});

static CLASSIFIED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_inbox_classified_total")
        .with_description(
            "Inbound messages classified, by classification and source (rules at the poll, ai \
             afterwards).",
        )
        .build()
});

/// Counts one inbound message classified as `classification` by `source`.
pub(crate) fn count_classified(classification: &'static str, source: &'static str) {
    CLASSIFIED.add(
        1,
        &[
            KeyValue::new("classification", classification),
            KeyValue::new("source", source),
        ],
    );
}

/// Records one poll: its `inbox.poll` event and `norbelys_inbox_polls_total`. `provider` is
/// `None` when the binding could not be read (gone, or its connection unreadable).
fn observe(
    lease: &Lease,
    provider: Option<Provider>,
    reset: bool,
    outcome: &Outcome,
    elapsed: Duration,
) {
    let (label, fetched, inserted, full, error) = match outcome {
        Outcome::Stored { read, new, full } => ("stored", *read, *new, *full, None),
        Outcome::Fenced => ("fenced", 0, 0, false, None),
        Outcome::Failed(_) => ("failed", 0, 0, false, Some("receive_failed")),
        Outcome::Gone => ("gone", 0, 0, false, None),
    };
    let provider = provider.map_or("unknown", Provider::as_str);
    POLLS.add(
        1,
        &[
            KeyValue::new("provider", provider),
            KeyValue::new("outcome", label),
        ],
    );
    crate::telemetry::unit(crate::telemetry::Event::InboxPoll);
    tracing::info!(
        event = "inbox.poll",
        workspace_id = %lease.workspace,
        binding_id = %lease.binding,
        connection_id = %lease.connection,
        provider,
        outcome = label,
        fetched,
        inserted,
        duplicates = fetched.saturating_sub(inserted),
        full,
        cursor_reset = reset,
        error,
        duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        "inbox.poll"
    );
}

/// Takes the turn of the workspace whose inbox turn is oldest among those with a due binding,
/// and moves it to the back; `None` when no workspace has one. Read as the scheduler; a turn
/// another replica is taking at the same instant is skipped.
///
/// # Errors
///
/// The database is unavailable.
pub async fn turn(db: &Database) -> Result<Option<WorkspaceId>, sqlx::Error> {
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    let workspace = sqlx::query_scalar!(
        "SELECT d.workspace_id FROM dispatch_workspaces d
          WHERE EXISTS (SELECT 1 FROM receive_bindings b
                          JOIN connections c ON c.workspace_id = b.workspace_id AND c.id = b.connection_id
                         WHERE b.workspace_id = d.workspace_id AND b.enabled AND b.lease_owner IS NULL
                           AND b.next_poll_at <= now())
          ORDER BY d.inbox_turn_at
          LIMIT 1
            FOR UPDATE OF d SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(workspace) = workspace {
        sqlx::query!(
            "UPDATE dispatch_workspaces SET inbox_turn_at = now() WHERE workspace_id = $1",
            workspace,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(workspace.map(WorkspaceId::trusted))
}

/// At most `limit` due bindings of `workspace`, the longest due first. Read as the scheduler
/// without a lock; [`claim`] checks each again under its locks.
///
/// # Errors
///
/// The database is unavailable.
pub async fn due(
    db: &Database,
    workspace: WorkspaceId,
    limit: usize,
) -> Result<Vec<Id<ReceiveBinding>>, sqlx::Error> {
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    let ids = sqlx::query_scalar!(
        r#"SELECT b.id AS "id: Id<ReceiveBinding>" FROM receive_bindings b
             JOIN connections c ON c.workspace_id = b.workspace_id AND c.id = b.connection_id
            WHERE b.workspace_id = $1 AND b.enabled AND b.lease_owner IS NULL AND b.next_poll_at <= now()
            ORDER BY b.next_poll_at
            LIMIT $2"#,
        workspace.uuid(),
        i64::try_from(limit).unwrap_or(i64::MAX),
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(ids)
}

/// A binding claimed by one poll.
#[derive(Debug, Clone)]
pub struct Lease {
    /// The binding's workspace.
    pub workspace: WorkspaceId,
    /// The binding.
    pub binding: Id<ReceiveBinding>,
    /// Its connection.
    pub connection: Id<Connection>,
    /// The claiming process.
    pub owner: String,
    /// The generation this claim set: the fence of every write of the poll.
    pub generation: i64,
    /// When the poll's provider requests must have ended: [`MARGIN`] before the lease expires,
    /// counted from before the claim, so never later than the database's own expiry.
    pub deadline: Instant,
}

/// Claims `binding` for `owner` (see the module); `None` when it is no longer claimable: its
/// connection is not active or is locked by another process, the binding is disabled, leased
/// or not due, or another binding of its mailbox holds a live lease.
///
/// # Errors
///
/// The database is unavailable.
pub async fn claim(
    db: &Database,
    workspace: WorkspaceId,
    binding: Id<ReceiveBinding>,
    owner: &str,
) -> Result<Option<Lease>, sqlx::Error> {
    let deadline = Instant::now() + LEASE.saturating_sub(MARGIN);
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    let connection = sqlx::query_scalar!(
        "SELECT c.id FROM receive_bindings b
           JOIN connections c ON c.workspace_id = b.workspace_id AND c.id = b.connection_id
          WHERE b.workspace_id = $1 AND b.id = $2
            FOR UPDATE OF c SKIP LOCKED",
        workspace.uuid(),
        binding.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    if connection.is_none() {
        tx.commit().await?;
        return Ok(None);
    }
    let claimed = sqlx::query!(
        r#"UPDATE receive_bindings b
              SET lease_owner = $3, lease_generation = lease_generation + 1,
                  lease_expires_at = now() + make_interval(secs => $4)
            WHERE b.workspace_id = $1 AND b.id = $2 AND b.enabled AND b.lease_owner IS NULL AND b.next_poll_at <= now()
              AND NOT EXISTS (SELECT 1 FROM receive_bindings o
                               WHERE o.workspace_id = b.workspace_id AND o.connection_id = b.connection_id
                                 AND o.lease_expires_at > now())
        RETURNING b.connection_id AS "connection_id: Id<Connection>", b.lease_generation"#,
        workspace.uuid(),
        binding.uuid(),
        owner,
        f64::from(u32::try_from(LEASE.as_secs()).unwrap_or(u32::MAX)),
    )
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(claimed.map(|row| Lease {
        workspace,
        binding,
        connection: row.connection_id,
        owner: owner.to_owned(),
        generation: row.lease_generation,
        deadline,
    }))
}

/// Clears every expired lease, so its binding can be claimed again; returns how many. The
/// generation stays: the lost owner's fenced writes already fail, its owner being gone.
///
/// # Errors
///
/// The database is unavailable.
pub async fn recover(db: &Database) -> Result<u64, sqlx::Error> {
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    // The partial receive_bindings_expired_lease index covers only outstanding leases.
    let cleared = sqlx::query!(
        "UPDATE receive_bindings SET lease_owner = NULL, lease_expires_at = NULL WHERE lease_expires_at <= now()"
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(cleared)
}

/// What a poll needs of its binding and connection.
struct Mailbox {
    provider: Provider,
    folder: String,
    cursor: Option<Value>,
    polled_at: Option<Timestamp>,
    smtp: Option<SmtpSettings>,
    imap: Option<ImapSettings>,
    credential: Option<Credential>,
    version: i64,
}

/// One page read from the provider.
#[derive(Debug, Clone)]
pub struct Page {
    /// The new messages, oldest first where the provider orders them.
    pub messages: Vec<RawMessage>,
    /// The cursor to store once they are.
    pub cursor: Value,
    /// The provider has more waiting.
    pub full: bool,
    /// The previous cursor was not honoured and the page is a resync.
    pub reset: Option<Reset>,
}

/// Why a poll failed.
#[derive(Debug, thiserror::Error)]
pub enum PollError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Token(#[from] TokenError),
    #[error(transparent)]
    Receive(#[from] norbelys_mail::receive::Error),
    /// The inbox's share of the provider's rate limits is spent past the poll's deadline.
    #[error(transparent)]
    Limited(#[from] Exhausted),
    /// The connection cannot be read, or its stored settings cannot be used.
    #[error("{0}")]
    Unreadable(String),
}

impl PollError {
    /// The provider's earliest next attempt, shared by every receive protocol.
    fn retry_after(&self) -> Option<Timestamp> {
        match self {
            Self::Receive(error) => error.retry_after.map(Timestamp),
            _ => None,
        }
    }

    /// A client-wide throttle applies even when the provider gives no usable Retry-After.
    fn client_pause_until(&self, now: Timestamp) -> Option<Timestamp> {
        if let Self::Receive(error) = self
            && error.failure == norbelys_mail::receive::Failure::Throttled
            && error.scope == norbelys_mail::receive::LimitScope::Client
        {
            return Some(
                self.retry_after()
                    .filter(|until| *until > now)
                    .unwrap_or_else(|| now.plus(Duration::from_secs(60))),
            );
        }
        None
    }

    /// Pause only this replica's shared API-client budget; other providers remain independent.
    fn pause_client(&self, limits: &Limits, provider: Option<Provider>, now: Timestamp) {
        if let (Some(provider), Some(until)) = (provider, self.client_pause_until(now)) {
            limits.pause_platform(provider, until);
        }
    }

    /// Whether the provider refused the connection's credential or account (as opposed to being
    /// unreachable for now).
    fn refused_credential(&self) -> bool {
        match self {
            Self::Receive(error) => error.refused_credential(),
            Self::Token(TokenError::NoGrant) => true,
            Self::Token(TokenError::Refresh(error)) => {
                !matches!(error, norbelys_mail::oauth::OAuthError::Unavailable(_))
            }
            _ => false,
        }
    }
}

/// How a poll ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The page is stored: how many messages were read and how many were new.
    Stored { read: usize, new: usize, full: bool },
    /// The lease was lost before the page could be stored; nothing was recorded.
    Fenced,
    /// The poll failed; the reason is the binding's `status_detail`.
    Failed(String),
    /// The binding is no longer enabled, or its connection is gone.
    Gone,
}

/// What reading mailboxes needs: built once per process.
pub struct Reader {
    /// The deployment's keys: credentials open with them, our Message-IDs verify with them.
    pub keys: Keys,
    /// The process's OAuth token cache.
    pub tokens: Tokens,
    /// The client of the Gmail API and Microsoft Graph.
    pub http: HttpClient,
    /// IMAP connections, under the deployment's address policy.
    pub connector: Connector,
    /// Where raw messages are kept.
    pub storage: Storage,
    /// The poll interval.
    pub interval: SignedDuration,
    /// This replica's part of the Gmail and Graph limits receiving shares with sending and
    /// maintenance: every Gmail and Graph read waits for its tokens.
    pub limits: Limits,
}

impl Reader {
    /// Polls the binding `lease` holds, once (see the module), and records how it ended.
    ///
    /// # Errors
    ///
    /// The database refused the record of a failure; every other failure is recorded on the
    /// binding and returned as [`Outcome::Failed`].
    pub async fn poll(&self, db: &Database, lease: &Lease) -> Result<Outcome, sqlx::Error> {
        let start = crate::process::now();
        let started = std::time::Instant::now();
        let mut provider = None;
        let mut reset = false;
        let result = crate::process::guarded("inbox.poll", async {
            let result = async {
                let Some(mailbox) = self.mailbox(db, lease).await? else {
                    return Ok(None);
                };
                provider = Some(mailbox.provider);
                let since = mailbox
                    .polled_at
                    .map_or(start.0.saturating_sub(FIRST_SYNC), |at| {
                        at.0.saturating_sub(RESYNC_OVERLAP)
                    })
                    .unwrap_or(start.0);
                let page = self.fetch(db, lease, &mailbox, since).await?;
                reset = page.reset.is_some();
                store(
                    db,
                    &self.keys,
                    &self.storage,
                    lease,
                    &page,
                    start,
                    self.interval,
                )
                .await
                .map(Some)
            }
            .await;
            let outcome = match result {
                Ok(Some(outcome)) => outcome,
                Ok(None) => {
                    release(db, lease).await?;
                    Outcome::Gone
                }
                Err(error) => {
                    let detail = error.to_string();
                    error.pause_client(&self.limits, provider, crate::process::now());
                    fail(
                        db,
                        lease,
                        &detail,
                        self.interval,
                        error.refused_credential(),
                        error.retry_after(),
                    )
                    .await?;
                    Outcome::Failed(detail)
                }
            };
            Ok(outcome)
        })
        .await;
        let result = result
            .unwrap_or_else(|()| Err(sqlx::Error::Protocol("inbox poll task panicked".into())));
        match &result {
            Ok(outcome) => observe(lease, provider, reset, outcome, started.elapsed()),
            Err(_) => observe(
                lease,
                provider,
                reset,
                &Outcome::Failed("poll_record_failed".into()),
                started.elapsed(),
            ),
        }
        result
    }

    /// The binding and its connection, as the worker inside the workspace; `None` when the
    /// binding is disabled or gone.
    async fn mailbox(&self, db: &Database, lease: &Lease) -> Result<Option<Mailbox>, PollError> {
        let mut tx = db.begin_in(lease.workspace).await?;
        let row = sqlx::query!(
            r#"SELECT b.folder, b.cursor, b.polled_at AS "polled_at: Timestamp", c.provider, c.smtp, c.imap,
                      c.credential_version, connection_credential(c.workspace_id, c.id) AS credential
                 FROM receive_bindings b JOIN connections c ON c.workspace_id = b.workspace_id AND c.id = b.connection_id
                WHERE b.workspace_id = $1 AND b.id = $2 AND b.enabled"#,
            lease.workspace.uuid(),
            lease.binding.uuid(),
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
            .map_err(|_| PollError::Unreadable(format!("unknown provider `{}`", row.provider)))?;
        let credential = row
            .credential
            .map(|sealed| credentials::open(&self.keys, lease.workspace, lease.connection, &sealed))
            .transpose()
            .map_err(|error| {
                PollError::Unreadable(format!("the stored credential does not open: {error}"))
            })?;
        Ok(Some(Mailbox {
            provider,
            folder: row.folder,
            cursor: row.cursor,
            polled_at: row.polled_at,
            smtp: row.smtp.and_then(|smtp| serde_json::from_value(smtp).ok()),
            imap: row.imap.and_then(|imap| serde_json::from_value(imap).ok()),
            credential,
            version: row.credential_version,
        }))
    }

    /// One page of `mailbox` after its cursor (see the module).
    async fn fetch(
        &self,
        db: &Database,
        lease: &Lease,
        mailbox: &Mailbox,
        since: jiff::Timestamp,
    ) -> Result<Page, PollError> {
        let deadline = lease.deadline;
        match mailbox.provider {
            Provider::Google | Provider::Microsoft => {
                let grantee = Grantee {
                    workspace: lease.workspace,
                    connection: lease.connection,
                    provider: mailbox.provider,
                    credential: mailbox.credential.as_ref(),
                    version: mailbox.version,
                };
                let token = self.tokens.access_token(db, &grantee, deadline).await?;
                let charge = |read| self.limits.wait(lease.connection, read, deadline);
                let (ids, cursor, full, reset) = if mailbox.provider == Provider::Google {
                    let cursor: Option<gmail::GmailCursor> = stored_cursor(mailbox.cursor.as_ref());
                    // The listing is `history.list` after a cursor, and `getProfile` with
                    // `messages.list` for a first read or a reset: each is charged its cost, a
                    // reset's after it happened.
                    if cursor
                        .as_ref()
                        .is_some_and(|cursor| cursor.pending.is_some())
                    {
                        // The saved provider page already contains these IDs; no listing request.
                    } else if cursor
                        .as_ref()
                        .is_some_and(|cursor| cursor.resync.is_some())
                    {
                        charge(Read::GmailList).await?;
                    } else if cursor.is_some() {
                        charge(Read::GmailHistory).await?;
                    } else {
                        charge(Read::GmailProfile).await?;
                        charge(Read::GmailList).await?;
                    }
                    let page = gmail::changes(
                        &self.http,
                        &token,
                        &mailbox.folder,
                        cursor.as_ref(),
                        since,
                        PAGE,
                        deadline,
                    )
                    .await?;
                    if cursor.is_some() && page.reset.is_some() {
                        charge(Read::GmailProfile).await?;
                        charge(Read::GmailList).await?;
                    }
                    (page.ids, json!(page.cursor), page.more, page.reset)
                } else {
                    let cursor: Option<graph::GraphCursor> = stored_cursor(mailbox.cursor.as_ref());
                    if !cursor
                        .as_ref()
                        .is_some_and(|cursor| cursor.pending.is_some())
                    {
                        charge(Read::Graph).await?;
                    }
                    let page = graph::changes(
                        &self.http,
                        &token,
                        &mailbox.folder,
                        cursor.as_ref(),
                        since,
                        PAGE,
                        deadline,
                    )
                    .await?;
                    // An expired delta link is read again from the start: a second request.
                    if cursor.is_some() && page.reset.is_some() {
                        charge(Read::Graph).await?;
                    }
                    (page.ids, json!(page.cursor), page.more, page.reset)
                };
                let mut messages = Vec::with_capacity(ids.len());
                for id in &ids {
                    let read = if mailbox.provider == Provider::Google {
                        charge(Read::GmailGet).await?;
                        let read =
                            gmail::message(&self.http, &token, id, MAX_MESSAGE_BYTES, deadline)
                                .await;
                        // A message too large to read whole is read again as its headers: a
                        // second `messages.get`, charged after it.
                        if read.as_ref().is_ok_and(|message| message.truncated) {
                            charge(Read::GmailGet).await?;
                        }
                        read
                    } else {
                        // Its metadata, then its content: two requests.
                        charge(Read::Graph).await?;
                        charge(Read::Graph).await?;
                        graph::message(&self.http, &token, id, MAX_MESSAGE_BYTES, deadline).await
                    };
                    match read {
                        Ok(message) => messages.push(message),
                        // Deleted between the listing and the read: nothing to keep.
                        Err(norbelys_mail::receive::Error {
                            status: Some(404), ..
                        }) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(Page {
                    messages,
                    cursor,
                    full,
                    reset,
                })
            }
            Provider::Smtp | Provider::Norbelys => {
                let (Some(smtp), Some(settings)) = (&mailbox.smtp, &mailbox.imap) else {
                    return Err(PollError::Unreadable(
                        "the connection has no IMAP settings to read its mailbox with".to_owned(),
                    ));
                };
                let Some(Credential::Password(password)) = &mailbox.credential else {
                    return Err(PollError::Unreadable(
                        "the connection has no password; save one again".to_owned(),
                    ));
                };
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
                let mut session =
                    imap::connect(&self.connector, &server, &auth, MAX_MESSAGE_BYTES, deadline)
                        .await
                        .map_err(norbelys_mail::receive::Error::from)?;
                let stored: Option<ImapCursor> = stored_cursor(mailbox.cursor.as_ref());
                let read = async {
                    let page = session
                        .changes(&mailbox.folder, stored.as_ref(), since, PAGE)
                        .await?;
                    let messages = session
                        .fetch(&mailbox.folder, page.cursor.uid_validity, &page.ids)
                        .await?;
                    Ok::<_, norbelys_mail::receive::Error>((page, messages))
                }
                .await;
                session.logout().await;
                let (page, messages) = read?;
                Ok(Page {
                    messages,
                    cursor: json!(page.cursor),
                    full: page.more,
                    reset: page.reset,
                })
            }
            Provider::Ses | Provider::Sendgrid | Provider::Mailgun => Err(PollError::Unreadable(
                "a relay has no mailbox to read".to_owned(),
            )),
        }
    }
}

/// A stored cursor as the provider's cursor type; `None` (a first read) when it is absent or of
/// another provider's shape.
fn stored_cursor<C: serde::de::DeserializeOwned>(value: Option<&Value>) -> Option<C> {
    value.and_then(|value| serde_json::from_value(value.clone()).ok())
}

/// The object key of a raw message: by workspace, binding and a hash of its transport key, so a
/// re-read writes the same object and no key is built from what the mail says.
fn object_key(lease: &Lease, identity: &TransportIdentity) -> String {
    format!(
        "inbound/{}/{}/{}.eml",
        lease.workspace.uuid(),
        lease.binding.uuid(),
        crypto::hex(&crypto::sha256(identity.key().as_bytes()))
    )
}

/// The status detail of a resync, naming the gap when there is one.
fn resync_detail(reset: &Reset) -> String {
    let reason = match reset.reason {
        ResetReason::UidValidity => "the folder's UIDVALIDITY changed",
        ResetReason::History => "Gmail no longer had the mailbox's history",
        ResetReason::Delta => "Microsoft Graph no longer knew the delta link",
    };
    if reset.truncated {
        format!(
            "The mailbox was read again from {} because {reason}; more mail arrived since then than one resync reads, so some messages may be missing.",
            reset.since
        )
    } else {
        format!(
            "The mailbox was read again from {} because {reason}.",
            reset.since
        )
    }
}

/// One new message of a page, read and judged before anything is written.
struct Judged<'a> {
    raw: &'a RawMessage,
    object_key: String,
    inbound: Inbound,
    found: Option<Found>,
    verdict: Verdict,
    person: Option<Uuid>,
    received_at: Timestamp,
}

/// Stores `page` under `lease` (see the module), in four steps: judge the new messages (one read
/// transaction), keep their raw bytes in object storage, apply the stop rules they trigger (each
/// in a transaction of its own), then one transaction with the inbound rows, their effects and
/// the cursor's advance, fenced by the lease.
///
/// # Errors
///
/// Object storage or the database refused; the page's rows and cursor are not recorded then (a
/// stop already applied stays: it is true, and applying it again changes nothing).
pub async fn store(
    db: &Database,
    keys: &Keys,
    storage: &Storage,
    lease: &Lease,
    page: &Page,
    start: Timestamp,
    interval: SignedDuration,
) -> Result<Outcome, PollError> {
    let workspace = lease.workspace;
    let mut tx = db.begin_in(workspace).await?;
    let transport_keys: Vec<String> = page.messages.iter().map(|raw| raw.identity.key()).collect();
    let stored: HashSet<String> = sqlx::query_scalar!(
        "SELECT transport_key FROM inbound_messages
          WHERE workspace_id = $1 AND receive_binding_id = $2 AND transport_key = ANY($3)",
        workspace.uuid(),
        lease.binding.uuid(),
        &transport_keys,
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let ai = crate::ai::store::read_settings(&mut tx, workspace)
        .await?
        .classify_replies;
    let mut judged = Vec::new();
    for raw in &page.messages {
        if !stored.contains(&raw.identity.key()) {
            judged.push(judge(&mut tx, keys, lease, raw).await?);
        }
    }
    tx.commit().await?;

    for item in &judged {
        storage
            .put(&item.object_key, Bytes::copy_from_slice(&item.raw.raw))
            .await?;
    }
    for item in &judged {
        stop(db, workspace, item).await?;
    }
    let mut contents = Vec::with_capacity(judged.len());
    for item in &judged {
        let content = inbound::content(&item.raw.raw).unwrap_or_default();
        let mut files = Vec::new();
        if !item.raw.truncated {
            for file in content.attachments.iter().take(100) {
                let id = Id::<crate::domain::ids::Attachment>::new();
                let key = crate::delivery::attachments::key(workspace, id);
                storage
                    .put(&key, Bytes::copy_from_slice(&file.bytes))
                    .await?;
                files.push((id, key));
            }
        }
        contents.push((content, files));
    }

    let mut tx = db.begin_in(workspace).await?;
    let mut new = Vec::with_capacity(judged.len());
    for (item, (content, files)) in judged.iter().zip(&contents) {
        if let Some(id) = insert(&mut tx, lease, item).await? {
            crate::delivery::content::received(&mut tx, workspace, id, content).await?;
            for (file, (file_id, key)) in content.attachments.iter().zip(files) {
                crate::delivery::attachments::insert(
                    &mut tx,
                    workspace,
                    crate::delivery::attachments::NewFile {
                        id: *file_id,
                        filename: &file.filename,
                        content_type: &file.content_type,
                        content_id: file.content_id.as_deref(),
                        key,
                        size: file.bytes.len(),
                    },
                )
                .await?;
                sqlx::query("INSERT INTO message_attachments(workspace_id,message_id,attachment_id) VALUES($1,$2,$3)").bind(workspace.uuid()).bind(id.uuid()).bind(file_id.uuid()).execute(&mut *tx).await?;
            }
            new.push((id, item));
        }
    }
    for (id, item) in &new {
        record_evidence(&mut tx, lease, *id, item).await?;
    }
    let mut classify = false;
    for (id, item) in &new {
        classify |= effects(&mut tx, workspace, *id, item, ai).await?;
    }
    let now = crate::process::now();
    // A page waits no backoff: its schedule takes no draw.
    let next = rules::next_poll(
        start.0,
        now.0,
        interval,
        Polled::Page { full: page.full },
        0,
    );
    let advanced = sqlx::query!(
        "UPDATE receive_bindings
            SET cursor = $5, polled_at = $6, failures = 0,
                status_detail = CASE WHEN $7::text IS NOT NULL THEN $7 WHEN failures > 0 THEN NULL ELSE status_detail END,
                next_poll_at = $8, lease_owner = NULL, lease_expires_at = NULL
          WHERE workspace_id = $1 AND id = $2 AND lease_owner = $3 AND lease_generation = $4",
        workspace.uuid(),
        lease.binding.uuid(),
        lease.owner.as_str(),
        lease.generation,
        &page.cursor,
        start as _,
        page.reset.as_ref().map(resync_detail),
        Timestamp(next) as _,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if advanced == 0 {
        tx.rollback().await?;
        return Ok(Outcome::Fenced);
    }
    tx.commit().await?;
    for (_, item) in &new {
        count_classified(
            item.verdict.classification.as_str(),
            rules::ClassificationSource::Rules.as_str(),
        );
    }
    if classify {
        jobs::wake(db, Queue::Ai).await;
    }
    Ok(Outcome::Stored {
        read: page.messages.len(),
        new: new.len(),
        full: page.full,
    })
}

/// Reads one message's facts, correlates it and classifies it, writing nothing.
async fn judge<'a>(
    tx: &mut Tx,
    keys: &Keys,
    lease: &Lease,
    raw: &'a RawMessage,
) -> Result<Judged<'a>, PollError> {
    let workspace = lease.workspace;
    let inbound = inbound::read(&raw.raw, EXCERPT_CHARS).unwrap_or_else(|| Inbound {
        message_id: None,
        in_reply_to: Vec::new(),
        references: Vec::new(),
        from: None,
        subject: None,
        date: None,
        auto_submitted: None,
        precedence: None,
        excerpt: None,
        report: None,
    });
    let (ids, address) = classify::correlation_keys(&inbound);
    let found =
        correlate::find(tx, keys, workspace, lease.connection.uuid(), address, &ids).await?;
    let verdict = rules::classify(&classify::facts(&inbound, found.is_some()));
    let person =
        match found.as_ref().and_then(|found| found.thread.person) {
            Some(person) => Some(person.uuid()),
            None => match &inbound.from {
                Some(from) => sqlx::query_scalar!(
                    "SELECT id FROM people WHERE workspace_id = $1 AND email_key = ascii_lower($2)",
                    workspace.uuid(),
                    from.address,
                )
                .fetch_optional(&mut **tx)
                .await?,
                None => None,
            },
        };
    let received_at = raw
        .received_at
        .or(inbound.date)
        .map_or_else(crate::process::now, Timestamp);
    Ok(Judged {
        raw,
        object_key: object_key(lease, &raw.identity),
        inbound,
        found,
        verdict,
        person,
        received_at,
    })
}

/// The stop rules a new message triggers: a person's answer to a campaign thread ends the
/// person's enrollments under the campaign's rules, and an unsubscribe request ends every live
/// enrollment of its sender. Each runs in a transaction that holds nothing else, because the stop
/// rules lock enrollments before queue rows and messages, the order every path that stops or
/// advances an enrollment takes; both are idempotent, so a page read again repeats nothing.
async fn stop(db: &Database, workspace: WorkspaceId, item: &Judged<'_>) -> Result<(), sqlx::Error> {
    let answered = item
        .found
        .as_ref()
        .filter(|_| item.verdict.is_answer())
        .and_then(|found| found.thread.campaign.zip(found.thread.person));
    if let Some((campaign, person)) = answered {
        let mut tx = db.begin_in(workspace).await?;
        enrollments::stop_for_reply(&mut tx, workspace, campaign, person).await?;
        tx.commit().await?;
    }
    if item.verdict.authority == Authority::UnsubscribeRequest
        && let Some(from) = &item.inbound.from
    {
        let mut tx = db.begin_in(workspace).await?;
        enrollments::stop_suppressed(&mut tx, workspace, &from.address).await?;
        tx.commit().await?;
    }
    Ok(())
}

/// Inserts one message's row; `None` when its transport key was stored already.
async fn insert(
    tx: &mut Tx,
    lease: &Lease,
    item: &Judged<'_>,
) -> Result<Option<Id<InboundMessage>>, sqlx::Error> {
    let (inbound, raw) = (&item.inbound, item.raw);
    let bracketed = |id: &String| format!("<{id}>");
    let from = inbound.from.as_ref();
    let proposal = item
        .verdict
        .proposal
        .as_ref()
        .and_then(|proposal| serde_json::to_value(proposal).ok());
    let size = raw
        .size
        .or_else(|| u64::try_from(raw.raw.len()).ok())
        .and_then(|size| i32::try_from(size).ok());
    sqlx::query_scalar!(
        r#"INSERT INTO inbound_messages (workspace_id, receive_binding_id, connection_id, transport_identity, transport_key,
                                         internet_message_id, in_reply_to, references_ids, thread_id, message_id, person_id,
                                         from_email, from_name, subject, received_at, classification, classification_source,
                                         review_requested_at, review_proposal, evidence, body_text, body_object_key, size_bytes,
                                         truncated, content_hash)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, 'rules',
                   CASE WHEN $17::jsonb IS NULL THEN NULL ELSE now() END, $17, $18, $19, $20, $21, $22, $23)
           ON CONFLICT (workspace_id, receive_binding_id, transport_key) DO NOTHING
           RETURNING id AS "id: Id<InboundMessage>""#,
        lease.workspace.uuid(),
        lease.binding.uuid(),
        lease.connection.uuid(),
        json!(raw.identity),
        raw.identity.key(),
        inbound.message_id.as_ref().map(bracketed),
        inbound.in_reply_to.first().map(bracketed),
        &inbound.references.iter().map(bracketed).collect::<Vec<_>>(),
        item.found.as_ref().map(|found| found.thread.id.uuid()),
        item.found
            .as_ref()
            .and_then(|found| found.message)
            .map(|id| id.uuid()),
        item.person,
        from.map(|from| from.address.as_str()),
        from.and_then(|from| from.name.as_deref()),
        inbound.subject.as_deref(),
        item.received_at as _,
        item.verdict.classification.as_str(),
        proposal,
        item.verdict.evidence.as_str(),
        inbound.excerpt.as_deref(),
        item.object_key.as_str(),
        size,
        raw.truncated,
        crypto::sha256(&raw.raw),
    )
    .fetch_optional(&mut **tx)
    .await
}

/// Records the evidence a new message is (a report, an unsubscribe request) and attaches the
/// review the evidence asks for as the message's proposal, unless a notice proposed one already.
async fn record_evidence(
    tx: &mut Tx,
    lease: &Lease,
    id: Id<InboundMessage>,
    item: &Judged<'_>,
) -> Result<(), sqlx::Error> {
    let origin = Origin {
        binding: lease.binding,
        connection: lease.connection.uuid(),
        source_event_id: item
            .inbound
            .message_id
            .as_ref()
            .map_or_else(|| item.raw.identity.key(), |id| format!("<{id}>")),
        observed_at: item.received_at,
    };
    let observations =
        classify::evidence(&item.inbound, &item.verdict, item.found.as_ref(), &origin);
    if observations.is_empty() {
        return Ok(());
    }
    let recorded = evidence::record(tx, lease.workspace, &observations).await?;
    let asked = recorded
        .iter()
        .zip(&observations)
        .find_map(|(recorded, observed)| match recorded.effect {
            RecipientEffect::Review(proposal) | RecipientEffect::HoldAndReview(_, proposal) => {
                observed
                    .recipient
                    .as_deref()
                    .map(|email| ReviewProposal::from_evidence(proposal, email))
            }
            RecipientEffect::None | RecipientEffect::Suppress(_) | RecipientEffect::Hold(_) => None,
        });
    if item.verdict.proposal.is_none()
        && let Some(asked) = asked
    {
        sqlx::query!(
            "UPDATE inbound_messages SET review_proposal = $3, review_requested_at = now()
              WHERE workspace_id = $1 AND id = $2",
            lease.workspace.uuid(),
            id.uuid(),
            json!(asked),
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// The rest of a new message's effects (see the module): the reply counter, the thread, the
/// person's last reply, the customer's event, and AI where the rules left it open. Returns whether
/// AI classification was asked for.
async fn effects(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<InboundMessage>,
    item: &Judged<'_>,
    ai: bool,
) -> Result<bool, sqlx::Error> {
    let answer = item.verdict.is_answer();
    if answer && let Some(message) = item.found.as_ref().and_then(|found| found.message) {
        evidence::increment(tx, workspace, &[message], Metric::Replied).await?;
    }
    if let Some(found) = &item.found {
        sqlx::query!(
            "UPDATE threads
                SET last_activity_at = greatest(last_activity_at, $3), unread = true,
                    status = CASE WHEN $4 THEN 'open' ELSE status END,
                    snoozed_until = CASE WHEN $4 THEN NULL ELSE snoozed_until END
              WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            found.thread.id.uuid(),
            item.received_at as _,
            answer,
        )
        .execute(&mut **tx)
        .await?;
    }
    if answer && let Some(person) = item.person {
        sqlx::query!(
            "UPDATE people SET replied_at = greatest(replied_at, $3) WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            person,
            item.received_at as _,
        )
        .execute(&mut **tx)
        .await?;
    }
    outbox::record(
        tx,
        workspace,
        Event {
            kind: EventType::InboundMessageReceived,
            subject_type: "inbound_message",
            subject_id: id.uuid(),
            data: json!({
                "inbound_message_id": id,
                "classification": item.verdict.classification.as_str(),
                "thread_id": item.found.as_ref().map(|found| found.thread.id),
            }),
        },
    )
    .await?;
    let asked = ai && item.verdict.classification.open_to_ai();
    if asked {
        jobs::enqueue(
            tx,
            workspace,
            &Classify {
                inbound: id,
                revision: 1,
            },
            None,
        )
        .await?;
    }
    Ok(asked)
}

/// Records a failed poll under the fence: one more consecutive failure, its reason, the doubled
/// wait, and the lease released. When the provider refused the credential (`check`), the
/// connection's check is enqueued in the same transaction, so its health moves through the one
/// place that judges credentials (`senders::check`) instead of the inbox backing off for hours
/// in silence. A provider's `retry_after` extends the wait when it is later than the local
/// backoff. Nothing when the lease was lost meanwhile.
///
/// # Errors
///
/// The database refused.
pub(super) async fn fail(
    db: &Database,
    lease: &Lease,
    detail: &str,
    interval: SignedDuration,
    check: bool,
    retry_after: Option<Timestamp>,
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin_in(lease.workspace).await?;
    let failures = sqlx::query_scalar!(
        "SELECT failures FROM receive_bindings
          WHERE workspace_id = $1 AND id = $2 AND lease_owner = $3 AND lease_generation = $4
            FOR UPDATE",
        lease.workspace.uuid(),
        lease.binding.uuid(),
        lease.owner.as_str(),
        lease.generation,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(failures) = failures {
        let failures = u32::try_from(failures.saturating_add(1)).unwrap_or(u32::MAX);
        let now = crate::process::now();
        let next = rules::next_poll(
            now.0,
            now.0,
            interval,
            Polled::Failed { failures },
            jobs::draw(),
        );
        let next = retry_after.map_or(next, |until| next.max(until.0));
        let detail: String = detail.chars().take(1_000).collect();
        sqlx::query!(
            "UPDATE receive_bindings
                SET failures = failures + 1, status_detail = $3, next_poll_at = $4,
                    lease_owner = NULL, lease_expires_at = NULL
              WHERE workspace_id = $1 AND id = $2",
            lease.workspace.uuid(),
            lease.binding.uuid(),
            detail,
            Timestamp(next) as _,
        )
        .execute(&mut *tx)
        .await?;
        if check {
            jobs::enqueue(
                &mut tx,
                lease.workspace,
                &ConnectionCheck {
                    connection: lease.connection,
                },
                None,
            )
            .await?;
        }
    }
    tx.commit().await?;
    if check {
        jobs::wake(db, Queue::Maintenance).await;
    }
    Ok(())
}

/// Releases the lease without a poll (the binding was disabled or its connection is gone).
async fn release(db: &Database, lease: &Lease) -> Result<(), sqlx::Error> {
    let mut tx = db.begin_in(lease.workspace).await?;
    sqlx::query!(
        "UPDATE receive_bindings SET lease_owner = NULL, lease_expires_at = NULL
          WHERE workspace_id = $1 AND id = $2 AND lease_owner = $3 AND lease_generation = $4",
        lease.workspace.uuid(),
        lease.binding.uuid(),
        lease.owner.as_str(),
        lease.generation,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// The owner name of an inbox process.
#[must_use]
pub fn owner() -> String {
    format!("inbox:{}", Uuid::now_v7().simple())
}

#[cfg(test)]
mod receive_budget_tests {
    use super::*;
    use norbelys_mail::receive::{Error, Failure, LimitScope};

    /// A missing or expired header must not let every binding hammer a throttled API client.
    /// Account-only limits and credential refusals never pause unrelated mailboxes.
    #[test]
    fn client_throttles_pause_with_or_without_a_future_retry_hint() {
        let now = Timestamp("2026-10-05T00:00:00Z".parse().unwrap());
        let mut error = Error {
            failure: Failure::Throttled,
            status: Some(429),
            retry_after: None,
            scope: LimitScope::Client,
        };
        assert_eq!(
            PollError::Receive(error.clone()).client_pause_until(now),
            Some(now.plus(Duration::from_secs(60)))
        );
        error.retry_after = Some(now.minus(Duration::from_secs(1)).0);
        assert_eq!(
            PollError::Receive(error.clone()).client_pause_until(now),
            Some(now.plus(Duration::from_secs(60)))
        );
        let later = now.plus(Duration::from_secs(7200));
        error.retry_after = Some(later.0);
        assert_eq!(
            PollError::Receive(error.clone()).client_pause_until(now),
            Some(later)
        );
        error.scope = LimitScope::Account;
        assert_eq!(
            PollError::Receive(error.clone()).client_pause_until(now),
            None
        );
        error.scope = LimitScope::Client;
        error.failure = Failure::Credential;
        assert_eq!(PollError::Receive(error).client_pause_until(now), None);
    }
}
