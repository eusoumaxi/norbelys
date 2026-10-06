//! Connections: one authenticated transport account each, with its credential, its account, its
//! health and the pacing settings the sender follows.
//!
//! # Connecting
//!
//! [`connect`] lands an account among the workspace's rows by the restoration order (a new row,
//! an archived row of the same account brought back with its id, history and identities, or a
//! conflict with the live row of that account), writes its sealed credential, its identities,
//! its read folders and, for a relay or the managed MTA, its provider webhook, and enqueues the
//! job that proves the credential (`connection.check`, or `provider.norbelys.provision`). The
//! connection answers `verifying` until that job decides.
//!
//! The database refuses a second live row of one provider account
//! (`connections_live_account`), a second live paced sender of one address
//! (`connections_live_paced_sender`) and a second live identity of one address
//! (`sender_identities_live_address`); [`connect`] checks each first so its answer can name the
//! live connection, and the indexes stay the net for a race. A restore is refused the same way
//! when one of the archived row's identities has a live twin elsewhere.
//!
//! # Changing, verifying, archiving
//!
//! A change of credential or server settings sends the connection back to `verifying` with a new
//! check, so a credential is never active before it was proven. Pausing is a reversible switch:
//! a paused connection keeps its status and is skipped by the sender. Archiving keeps the row and
//! its history, erases the credential, archives its identities (their addresses become free for
//! another account), stops reading its folders, and leaves its provider webhook active so late
//! evidence for its messages still arrives.
//!
//! Lock order: the connection's row (`FOR UPDATE`) first, then its identities, bindings and
//! provider webhook, as every writer of a connection takes them.
//!
//! # Versions
//!
//! A connection carries `version` (see `http::versioning`), and its object shows rows of other
//! tables: its identities, its folders, its provider webhook. Every write of those rows also
//! updates the connection's own row in its transaction, so the version moves with them: an
//! update always touches the row ([`update`]), connecting and archiving rewrite it, and a check
//! that confirms identities writes the connection's `checked_at` in the same transaction. A
//! client that sends `If-Match` with a version it read ([`lock_version`] reads the current one
//! under the row's lock) can therefore never replace an identity list or a folder list that
//! changed since.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::bindings::{self, ReceivingObject};
use super::check::ConnectionCheck;
use super::credentials::{self, ApiCredential, Credential};
use super::health;
use super::identities::{self, IdentityInput, IdentityObject};
use super::provision::NorbelysProvision;
use super::{Error, Settings};
use crate::crypto::{self, Keys};
use crate::db::Tx;
use crate::domain::ids::{Connection, Id, ProviderWebhook, QuotaScope, User, WorkspaceId};
use crate::domain::senders::{
    Candidate, HealthEvent, Placement, Provider, SendWindow, Status, Transition, Transport,
    WebhookKey, changed_interval, place,
};
use crate::domain::time::Timestamp;
use crate::http::versioning;
use crate::jobs;

/// How an SMTP session is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = SmtpSecurity)]
pub enum Security {
    /// TLS from the first byte (port 465).
    Tls,
    /// A required `STARTTLS` after the greeting (port 587).
    Starttls,
    /// No TLS: refused by the checks unless the deployment allows private hosts (development).
    Plain,
}

/// How an IMAP session is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImapSecurity {
    /// TLS from the first byte (port 993).
    Tls,
    /// No TLS: refused by the checks unless the deployment allows private hosts (development).
    Plain,
}

/// An SMTP endpoint, as stored in `connections.smtp` and shown; never the password.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SmtpSettings {
    /// The host name.
    pub host: String,
    /// The port.
    pub port: u16,
    /// How the session is secured.
    pub security: Security,
    /// The login.
    pub username: String,
    /// The Amazon SES configuration set every message names, so SES publishes its events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_set: Option<String>,
}

/// An IMAP endpoint, as stored in `connections.imap` and shown; it logs in with the SMTP login
/// and password.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ImapSettings {
    /// The host name.
    pub host: String,
    /// The port.
    pub port: u16,
    /// How the session is secured.
    pub security: ImapSecurity,
}

/// The account behind a connection.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Account {
    /// The mailbox's address, an SMTP login as it is, the From address a paced SES connection
    /// paces, or a name the customer gave a relay account.
    pub email: String,
    /// For OAuth: the ID token's issuer.
    pub issuer: Option<String>,
    /// For OAuth: the provider's immutable subject, kept through address changes and archive.
    pub subject: Option<String>,
}

/// A relay's provider webhook: where its callbacks are posted.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct WebhookObject {
    pub id: Id<ProviderWebhook>,
    /// The URL to configure at the provider (an SES connection's quota scope names the one its
    /// account subscribes).
    pub url: String,
    /// Whether its verification material is set: SendGrid's verification key, Mailgun's
    /// signing key, the SNS topic for SES; callbacks are refused until it is.
    pub key_set: bool,
}

/// A day's use of the daily budget.
#[derive(Debug, Clone, Copy, Default, Serialize, utoipa::ToSchema)]
pub struct UsageDay {
    /// Submissions the provider accepted (or may have).
    pub used: i32,
    /// Submissions claimed and not settled yet.
    pub reserved: i32,
}

/// The daily budget's use today and yesterday (UTC days).
#[derive(Debug, Clone, Copy, Default, Serialize, utoipa::ToSchema)]
pub struct Usage {
    pub today: UsageDay,
    pub yesterday: UsageDay,
}

/// A consent the browser must give: open `url`, from the browser that received the ceremony
/// cookie with this answer.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Authorization {
    /// The provider's consent page.
    pub url: String,
    /// When the ceremony expires.
    pub expires_at: Timestamp,
}

/// A connection as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ConnectionObject {
    pub id: Id<Connection>,
    /// The provider of the account; new values may be added.
    #[schema(value_type = Provider)]
    pub provider: String,
    /// What it submits over: SMTP, or the Gmail API or Microsoft Graph (`api`).
    #[schema(value_type = Transport)]
    pub transport: String,
    pub account: Account,
    /// Its health: `active` sends (unless `paused`), `authorization_required` waits for a new
    /// credential or consent, `archived` ended. New values may be added.
    #[schema(value_type = Status)]
    pub status: String,
    /// The health text: what went wrong and what to do.
    pub status_detail: Option<String>,
    /// The person's pause: sending stops, conversations wait.
    pub paused: bool,
    /// The breaker holds the connection until then.
    pub paused_until: Option<Timestamp>,
    /// The last check of the credential.
    pub checked_at: Option<Timestamp>,
    /// The SMTP endpoint (relays, SMTP logins, the managed MTA).
    pub smtp: Option<SmtpSettings>,
    /// The IMAP endpoint of an SMTP login.
    pub imap: Option<ImapSettings>,
    /// The service's sender addresses.
    pub identities: Vec<IdentityObject>,
    /// The folders read, at most 10.
    pub receiving: ReceivingObject,
    /// A relay's or the managed MTA's provider webhook.
    pub webhook: Option<WebhookObject>,
    /// Submissions a UTC day.
    pub daily_limit: i32,
    /// Minutes between campaign emails: exact whole minutes for Norbelys, 5-minute slots for mailboxes and SES; null when rate-paced.
    pub send_interval_minutes: Option<i32>,
    /// When campaign mail may be submitted, in `timezone`.
    pub send_window: Option<SendWindow>,
    /// The IANA time zone of the send window.
    pub timezone: String,
    /// The warm-up stage; null when not warming.
    pub warmup_stage: Option<i16>,
    pub usage: Usage,
    #[schema(value_type = Option<String>)]
    pub quota_scope_id: Option<Id<QuotaScope>>,
    #[schema(value_type = Option<String>)]
    pub created_by: Option<Id<User>>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The connection's version, also the response's `ETag`: `updated_at` in microseconds since
    /// the Unix epoch. An update sent with it in `If-Match` applies only to this version, so a
    /// replacement of `identities` or of the folders read never undoes a change made since it
    /// was read. Health checks and the sender's pacing move it too.
    pub version: i64,
    /// Present when the browser must give consent first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization: Option<Authorization>,
}

struct Row {
    id: Id<Connection>,
    provider: String,
    transport: String,
    account_email: String,
    account_issuer: Option<String>,
    account_subject: Option<String>,
    status: String,
    status_detail: Option<String>,
    paused: bool,
    paused_until: Option<Timestamp>,
    checked_at: Option<Timestamp>,
    smtp: Option<Value>,
    imap: Option<Value>,
    daily_limit: i32,
    send_interval_minutes: Option<i32>,
    send_window: Option<Value>,
    timezone: String,
    warmup_stage: Option<i16>,
    quota_scope_id: Option<Id<QuotaScope>>,
    created_by: Option<Id<User>>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

/// The filters of the connection list.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Filters {
    /// Only connections in this status.
    pub status: Option<String>,
    /// Only connections of this provider.
    pub provider: Option<String>,
    /// Only connections with an identity carrying this tag.
    pub tag: Option<String>,
    /// Only connections naming this quota scope.
    pub quota_scope_id: Option<Uuid>,
    /// Only connections whose account starts with this text (ASCII case-insensitive).
    pub q: Option<String>,
}

/// Reads one connection of `workspace` with everything it contains.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    settings: &Settings,
    workspace: WorkspaceId,
    id: Id<Connection>,
) -> Result<Option<ConnectionObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"SELECT id AS "id: Id<Connection>", provider, transport, account_email, account_issuer, account_subject,
                  status, status_detail, paused, paused_until AS "paused_until: Timestamp",
                  checked_at AS "checked_at: Timestamp", smtp, imap, daily_limit, send_interval_minutes, send_window,
                  timezone, warmup_stage, quota_scope_id AS "quota_scope_id: Id<QuotaScope>",
                  created_by AS "created_by: Id<User>", created_at AS "created_at: Timestamp",
                  updated_at AS "updated_at: Timestamp"
             FROM connections WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(assemble(tx, settings, workspace, vec![row]).await?.pop())
}

/// One page of `workspace`'s connections in id order, after `cursor` when given; `limit` rows.
///
/// # Errors
///
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    settings: &Settings,
    workspace: WorkspaceId,
    filters: &Filters,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<ConnectionObject>, sqlx::Error> {
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT c.id AS "id: Id<Connection>", c.provider, c.transport, c.account_email, c.account_issuer,
                  c.account_subject, c.status, c.status_detail, c.paused, c.paused_until AS "paused_until: Timestamp",
                  c.checked_at AS "checked_at: Timestamp", c.smtp, c.imap, c.daily_limit, c.send_interval_minutes,
                  c.send_window, c.timezone, c.warmup_stage, c.quota_scope_id AS "quota_scope_id: Id<QuotaScope>",
                  c.created_by AS "created_by: Id<User>", c.created_at AS "created_at: Timestamp",
                  c.updated_at AS "updated_at: Timestamp"
             FROM connections c
            WHERE c.workspace_id = $1
              AND ($2::text IS NULL OR c.status = $2)
              AND ($3::text IS NULL OR c.provider = $3)
              AND ($4::text IS NULL OR EXISTS (SELECT 1 FROM sender_identities i
                                                WHERE i.workspace_id = c.workspace_id AND i.connection_id = c.id
                                                  AND $4 = ANY (i.tags)))
              AND ($5::uuid IS NULL OR c.quota_scope_id = $5)
              AND ($6::text IS NULL OR starts_with(c.account_email_key, ascii_lower($6)))
              AND ($7::uuid IS NULL OR CASE WHEN $8 THEN c.id > $7 ELSE c.id < $7 END)
            ORDER BY CASE WHEN $8 THEN c.id END ASC, c.id DESC
            LIMIT $9"#,
        workspace.uuid(),
        filters.status,
        filters.provider,
        filters.tag,
        filters.quota_scope_id,
        filters.q,
        cursor,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    assemble(tx, settings, workspace, rows).await
}

/// Counts `workspace`'s connections matching `filters`, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &Filters,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM connections c
                WHERE c.workspace_id = $1
                  AND ($2::text IS NULL OR c.status = $2)
                  AND ($3::text IS NULL OR c.provider = $3)
                  AND ($4::text IS NULL OR EXISTS (SELECT 1 FROM sender_identities i
                                                    WHERE i.workspace_id = c.workspace_id AND i.connection_id = c.id
                                                      AND $4 = ANY (i.tags)))
                  AND ($5::uuid IS NULL OR c.quota_scope_id = $5)
                  AND ($6::text IS NULL OR starts_with(c.account_email_key, ascii_lower($6)))
                LIMIT $7) counted"#,
        workspace.uuid(),
        filters.status,
        filters.provider,
        filters.tag,
        filters.quota_scope_id,
        filters.q,
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// The rows with their embedded parts: one query per part for the whole page.
async fn assemble(
    tx: &mut Tx,
    settings: &Settings,
    workspace: WorkspaceId,
    rows: Vec<Row>,
) -> Result<Vec<ConnectionObject>, sqlx::Error> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id.uuid()).collect();
    let mut identities = identities::of_connections(tx, workspace, &ids).await?;
    let mut receiving = bindings::of_connections(tx, workspace, &ids).await?;
    let webhooks: HashMap<Uuid, WebhookObject> = sqlx::query!(
        r#"SELECT connection_id, id AS "id: Id<ProviderWebhook>", signing_secret IS NOT NULL AS "key_set!"
             FROM provider_webhooks WHERE workspace_id = $1 AND connection_id = ANY($2)"#,
        workspace.uuid(),
        &ids,
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| {
        (
            row.connection_id,
            WebhookObject {
                id: row.id,
                url: settings.webhook_url(row.id),
                key_set: row.key_set,
            },
        )
    })
    .collect();
    let mut usage: HashMap<Uuid, Usage> = HashMap::new();
    for row in sqlx::query!(
        r#"SELECT connection_id, day = (now() AT TIME ZONE 'UTC')::date AS "today!", used, reserved
             FROM connection_usage
            WHERE workspace_id = $1 AND connection_id = ANY($2) AND day >= (now() AT TIME ZONE 'UTC')::date - 1"#,
        workspace.uuid(),
        &ids,
    )
    .fetch_all(&mut **tx)
    .await?
    {
        let entry = usage.entry(row.connection_id).or_default();
        let day = UsageDay {
            used: row.used,
            reserved: row.reserved,
        };
        if row.today {
            entry.today = day;
        } else {
            entry.yesterday = day;
        }
    }
    Ok(rows
        .into_iter()
        .map(|row| {
            let key = row.id.uuid();
            let mut smtp: Option<SmtpSettings> =
                row.smtp.and_then(|smtp| serde_json::from_value(smtp).ok());
            if row.provider == Provider::Norbelys.as_str()
                && !row.account_email.contains('@')
                && let Some(smtp) = &mut smtp
            {
                // Customers authenticate with the domain and API key; the worker loads
                // its private delivery login directly from the stored settings.
                smtp.username.clone_from(&row.account_email);
            }
            ConnectionObject {
                id: row.id,
                provider: row.provider,
                transport: row.transport,
                account: Account {
                    email: row.account_email,
                    issuer: row.account_issuer,
                    subject: row.account_subject,
                },
                status: row.status,
                status_detail: row.status_detail,
                paused: row.paused,
                paused_until: row.paused_until,
                checked_at: row.checked_at,
                smtp,
                imap: row.imap.and_then(|imap| serde_json::from_value(imap).ok()),
                identities: identities.remove(&key).unwrap_or_default(),
                receiving: receiving.remove(&key).unwrap_or_default(),
                webhook: webhooks.get(&key).cloned(),
                daily_limit: row.daily_limit,
                send_interval_minutes: row.send_interval_minutes,
                send_window: row
                    .send_window
                    .and_then(|window| serde_json::from_value(window).ok()),
                timezone: row.timezone,
                warmup_stage: row.warmup_stage,
                usage: usage.get(&key).copied().unwrap_or_default(),
                quota_scope_id: row.quota_scope_id,
                created_by: row.created_by,
                created_at: row.created_at,
                updated_at: row.updated_at,
                version: versioning::of(row.updated_at),
                authorization: None,
            }
        })
        .collect())
}

/// Locks a connection's row for an update and returns its version, for the update's `If-Match`
/// to be checked against before anything is written; `None` when the workspace has no such
/// connection. [`update`] then takes the same row lock again, which the transaction already
/// holds.
///
/// # Errors
///
/// The database is unavailable.
pub async fn lock_version(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Connection>,
) -> Result<Option<i64>, sqlx::Error> {
    let updated_at = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM connections WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated_at.map(versioning::of))
}

/// The member who created a connection (`None` inside for one an administrator manages), or
/// `None` when the workspace has no such connection: what the member-own rule needs.
///
/// # Errors
///
/// The database is unavailable.
pub async fn owner(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Connection>,
) -> Result<Option<Option<Id<User>>>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT created_by AS "created_by: Id<User>" FROM connections WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await
}

/// An OAuth account's immutable identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// The ID token's issuer.
    pub issuer: String,
    /// Google's `sub`, Microsoft's `oid`.
    pub subject: String,
}

/// A connection to land among the workspace's rows.
#[derive(Debug, Clone)]
pub struct NewConnection {
    /// The provider, which decides the way in and the transport.
    pub provider: Provider,
    /// The account's address or login (see [`Account::email`]).
    pub account_email: String,
    /// The OAuth subject, for an OAuth way in.
    pub subject: Option<Subject>,
    /// The SMTP endpoint (everything but the OAuth mailboxes).
    pub smtp: Option<SmtpSettings>,
    /// The IMAP endpoint of an SMTP login that is read.
    pub imap: Option<ImapSettings>,
    /// The credential to seal; `None` for the managed MTA, whose login is provisioned.
    pub credential: Option<Credential>,
    /// Identities to add (the account's own address among them when it is one).
    pub identities: Vec<IdentityInput>,
    /// The folders to read.
    pub folders: Vec<String>,
    /// A relay's webhook verification material, when given.
    pub webhook_key: Option<SecretString>,
    /// Submissions a UTC day.
    pub daily_limit: i32,
    /// A paced sender's interval, checked and rounded; `None` for a rate-paced connection.
    pub send_interval_minutes: Option<i32>,
    /// When campaign mail may be submitted, checked.
    pub send_window: Option<SendWindow>,
    /// The send window's IANA time zone, checked.
    pub timezone: String,
    /// The warm-up stage to start at.
    pub warmup_stage: Option<i16>,
    /// The quota scope of the account.
    pub quota_scope: Option<Id<QuotaScope>>,
    /// The member who connects it: a member manages only their own connections.
    pub created_by: Id<User>,
}

/// How an account landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    /// A new connection.
    Created(Id<Connection>),
    /// An archived connection of the same account came back.
    Restored(Id<Connection>),
}

impl Landed {
    /// The connection.
    #[must_use]
    pub fn id(self) -> Id<Connection> {
        match self {
            Self::Created(id) | Self::Restored(id) => id,
        }
    }
}

/// Checks that `scope` is a quota scope of the workspace for `provider`'s connections (an SES
/// connection must name one).
///
/// # Errors
///
/// No such scope, a scope of another provider, or an SES connection without one.
pub async fn check_scope(
    tx: &mut Tx,
    workspace: WorkspaceId,
    provider: Provider,
    scope: Option<Id<QuotaScope>>,
) -> Result<(), Error> {
    let Some(scope) = scope else {
        return if provider == Provider::Ses {
            Err(Error::invalid(
                "/quota_scope_id",
                "An SES connection names its account's quota scope.",
            ))
        } else {
            Ok(())
        };
    };
    let scope_provider = sqlx::query_scalar!(
        "SELECT provider FROM quota_scopes WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        scope.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound("quota scope"))?;
    if scope_provider == provider.as_str() {
        Ok(())
    } else {
        Err(Error::invalid(
            "/quota_scope_id",
            format!(
                "A {} connection names a quota scope of its own provider.",
                provider.as_str()
            ),
        ))
    }
}

/// Refuses a landing that would make a second live row of one provider account, or a second
/// live paced sender of one address, naming the live connection.
async fn check_live(
    tx: &mut Tx,
    workspace: WorkspaceId,
    provider: Provider,
    account_email: &str,
    paced: bool,
) -> Result<(), Error> {
    let live = sqlx::query!(
        r#"SELECT id AS "id: Id<Connection>", provider, send_interval_minutes IS NOT NULL AS "paced!"
             FROM connections
            WHERE workspace_id = $1 AND account_email_key = ascii_lower($2) AND status <> 'archived'"#,
        workspace.uuid(),
        account_email,
    )
    .fetch_all(&mut **tx)
    .await?;
    for row in live {
        if row.provider == provider.as_str() {
            return Err(Error::Conflict(format!(
                "`{account_email}` is already connected as {}; archive it first to connect it again.",
                row.id
            )));
        }
        if paced && row.paced {
            return Err(Error::Conflict(format!(
                "`{account_email}` already sends cold mail as {}: an address is one paced sender of a workspace. Archive it first.",
                row.id
            )));
        }
    }
    Ok(())
}

/// Lands `new` among the workspace's rows (see the module): creates or restores the connection
/// with its credential, identities, folders and provider webhook, and enqueues the job that
/// proves the credential. The caller wakes the maintenance queue after the commit.
///
/// # Errors
///
/// The account is connected already (`Conflict`), the quota scope is absent or of another
/// provider, an identity's address belongs to a live identity of another connection (for a
/// restore, one of the archived row's identities has a live twin: `Conflict`, naming its
/// connection), or the database refused.
pub async fn connect(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    new: &NewConnection,
) -> Result<Landed, Error> {
    check_scope(tx, workspace, new.provider, new.quota_scope).await?;
    let (issuer, subject) = match &new.subject {
        Some(subject) => (
            Some(subject.issuer.as_str()),
            Some(subject.subject.as_str()),
        ),
        None => (None, None),
    };
    let candidates: Vec<Candidate<Id<Connection>>> = sqlx::query!(
        r#"SELECT id AS "id: Id<Connection>", provider, status = 'archived' AS "archived!",
                  account_subject IS NOT NULL AS "has_subject!",
                  coalesce(account_issuer = $3 AND account_subject = $4, false) AS "same_subject!"
             FROM connections
            WHERE workspace_id = $1
              AND (account_email_key = ascii_lower($2) OR (account_issuer = $3 AND account_subject = $4))
            ORDER BY id DESC"#,
        workspace.uuid(),
        new.account_email,
        issuer,
        subject,
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .filter_map(|row| {
        Some(Candidate {
            id: row.id,
            provider: row.provider.parse().ok()?,
            archived: row.archived,
            has_subject: row.has_subject,
            same_subject: row.same_subject,
        })
    })
    .collect();
    let landed = match place(new.provider, new.subject.is_some(), &candidates) {
        Placement::Conflict(id) => {
            return Err(Error::Conflict(format!(
                "This account is already connected as {id}."
            )));
        }
        Placement::Create => Landed::Created(Id::new()),
        Placement::Restore { id, .. } => Landed::Restored(id),
    };
    check_live(
        tx,
        workspace,
        new.provider,
        &new.account_email,
        new.send_interval_minutes.is_some(),
    )
    .await?;
    let id = landed.id();
    let sealed = new
        .credential
        .as_ref()
        .map(|credential| credentials::seal(keys, workspace, id, credential))
        .transpose()?;
    let smtp = new
        .smtp
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| Error::invalid("/smtp", "The SMTP settings are not valid."))?;
    let imap = new
        .imap
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| Error::invalid("/imap", "The IMAP settings are not valid."))?;
    let window = new
        .send_window
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| Error::invalid("/send_window", "The send window is not valid."))?;
    // Creation and restoration persist the same account settings in the caller's transaction.
    macro_rules! write_connection {
        ($sql:literal $(, $extra:expr)* $(,)?) => {
            sqlx::query!($sql,
                workspace.uuid(),
                id.uuid(),
                new.provider.as_str(),
                new.provider.transport().as_str(),
                new.account_email,
                issuer,
                subject,
                smtp,
                imap,
                sealed,
                new.daily_limit,
                new.send_interval_minutes,
                window,
                new.timezone,
                new.warmup_stage,
                new.quota_scope.map(|scope| scope.uuid()),
                $($extra,)*
            ).execute(&mut **tx).await?
        };
    }
    match landed {
        Landed::Created(_) => {
            write_connection!(
                "INSERT INTO connections (workspace_id, id, provider, transport, account_email, account_issuer, account_subject,
                                          smtp, imap, credential, status, daily_limit, send_interval_minutes, send_window,
                                          timezone, warmup_stage, quota_scope_id, created_by)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'verifying', $11, $12, $13, $14, $15, $16, $17)",
                new.created_by.uuid(),
            );
        }
        Landed::Restored(_) => {
            // The row is locked first, as every writer of a connection takes it; a connect of the
            // same account that restored it meanwhile leaves nothing to restore.
            let locked = lock(tx, workspace, id).await?;
            if locked.status != Status::Archived {
                return Err(Error::Conflict(format!(
                    "This account is already connected as {id}."
                )));
            }
            // Leave archived before writing a credential: archived rows must carry no secret.
            // The lock and transaction keep this transition, its event and the new credential
            // invisible until the whole restoration (including identity conflicts) commits.
            health::apply(
                tx,
                workspace,
                id,
                locked.status,
                false,
                HealthEvent::Restored,
                None,
            )
            .await?;
            // A row with a subject keeps its address (the subject is the account); a row without
            // one takes the arriving subject, bound to the address it was restored by. Its status
            // has already moved through the health table in this transaction.
            write_connection!(
                "UPDATE connections
                    SET provider = $3, transport = $4,
                        account_email = CASE WHEN account_subject IS NULL THEN $5 ELSE account_email END,
                        account_issuer = coalesce(account_issuer, $6), account_subject = coalesce(account_subject, $7),
                        smtp = $8, imap = $9, credential = $10, credential_version = credential_version + 1,
                        paused = false, daily_limit = $11,
                        send_interval_minutes = $12, send_window = $13, timezone = $14, warmup_stage = $15,
                        quota_scope_id = $16, next_claim_at = NULL, consecutive_failures = 0, paused_until = NULL,
                        breaker_opened_at = NULL, probe_message_id = NULL, probe_generation = NULL
                  WHERE workspace_id = $1 AND id = $2",
            );
            identities::restore(tx, workspace, id).await?;
        }
    }
    identities::add(tx, workspace, id, &new.identities).await?;
    bindings::set(tx, workspace, id, &new.folders).await?;
    if let Some(kind) = new.provider.webhook_key() {
        let webhook = issue_webhook(tx, workspace, id, new.provider, &new.account_email).await?;
        let key = match (kind, &new.webhook_key) {
            (WebhookKey::Generated, _) => Some(generated_secret()?),
            (_, given) => given.clone(),
        };
        if let Some(key) = key {
            set_webhook_key(tx, keys, workspace, webhook, &key).await?;
        }
    }
    enqueue_proof(tx, workspace, id, new.provider).await?;
    Ok(landed)
}

/// A fresh Standard Webhooks secret: `whsec_` and the base64 of 32 random bytes.
fn generated_secret() -> Result<SecretString, Error> {
    Ok(SecretString::from(format!(
        "whsec_{}",
        STANDARD.encode(crypto::random_bytes(32)?)
    )))
}

/// The connection's provider webhook, created when it has none yet (a restored connection keeps
/// the one it had, so a subscription to its URL keeps delivering).
async fn issue_webhook(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    provider: Provider,
    name: &str,
) -> Result<Id<ProviderWebhook>, sqlx::Error> {
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO provider_webhooks (workspace_id, connection_id, provider, name) VALUES ($1, $2, $3, $4)
           ON CONFLICT (workspace_id, connection_id) DO NOTHING
           RETURNING id AS "id: Id<ProviderWebhook>""#,
        workspace.uuid(),
        connection.uuid(),
        provider.as_str(),
        name,
    )
    .fetch_optional(&mut **tx)
    .await?;
    match inserted {
        Some(id) => Ok(id),
        None => {
            sqlx::query_scalar!(
                r#"SELECT id AS "id: Id<ProviderWebhook>" FROM provider_webhooks WHERE workspace_id = $1 AND connection_id = $2"#,
                workspace.uuid(),
                connection.uuid(),
            )
            .fetch_one(&mut **tx)
            .await
        }
    }
}

/// Seals `key` as the provider webhook's verification material.
async fn set_webhook_key(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    webhook: Id<ProviderWebhook>,
    key: &SecretString,
) -> Result<(), Error> {
    let sealed = credentials::seal_webhook_key(keys, workspace, webhook, key)?;
    sqlx::query!(
        "UPDATE provider_webhooks SET signing_secret = $3 WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        webhook.uuid(),
        sealed,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Enqueues what proves a connection's credential: provisioning for the managed MTA, a check
/// for everything else. The key of each kind coalesces it with one already queued or running.
async fn enqueue_proof(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    provider: Provider,
) -> Result<(), sqlx::Error> {
    if provider == Provider::Norbelys {
        jobs::enqueue(tx, workspace, &NorbelysProvision { connection }, None).await?;
    } else {
        jobs::enqueue(tx, workspace, &ConnectionCheck { connection }, None).await?;
    }
    Ok(())
}

/// The row a change is applied to, locked.
struct Locked {
    provider: Provider,
    status: Status,
    paused: bool,
    send_interval_minutes: Option<i32>,
    smtp: Option<SmtpSettings>,
    account_email: String,
}

async fn lock(tx: &mut Tx, workspace: WorkspaceId, id: Id<Connection>) -> Result<Locked, Error> {
    let row = sqlx::query!(
        "SELECT provider, status, paused, send_interval_minutes, smtp, account_email
           FROM connections WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound("connection"))?;
    let unknown =
        || Error::InvalidState("The connection's stored state is not understood.".to_owned());
    Ok(Locked {
        provider: row.provider.parse().map_err(|_| unknown())?,
        status: row.status.parse().map_err(|_| unknown())?,
        paused: row.paused,
        send_interval_minutes: row.send_interval_minutes,
        smtp: row.smtp.and_then(|smtp| serde_json::from_value(smtp).ok()),
        account_email: row.account_email,
    })
}

/// A change of an SMTP endpoint: the fields given replace the stored ones.
#[derive(Debug, Clone, Default)]
pub struct SmtpChange {
    /// A new host.
    pub host: Option<String>,
    /// A new port.
    pub port: Option<u16>,
    /// A new security mode.
    pub security: Option<Security>,
    /// A new login.
    pub username: Option<String>,
    /// A new password, sealed as a new credential.
    pub password: Option<SecretString>,
    /// A new SES configuration set.
    pub configuration_set: Option<String>,
}

/// What a `PATCH` changes; `None` leaves a field as it is, and `Some(None)` clears a nullable
/// one.
#[derive(Debug, Clone, Default)]
pub struct Changes {
    /// Pause or resume.
    pub paused: Option<bool>,
    /// A new daily limit.
    pub daily_limit: Option<i32>,
    /// A paced sender's new interval, before its check and rounding.
    pub send_interval_minutes: Option<i32>,
    /// A new send window (checked), or none.
    pub send_window: Option<Option<SendWindow>>,
    /// A new IANA time zone (checked).
    pub timezone: Option<String>,
    /// A new warm-up stage, or none.
    pub warmup_stage: Option<Option<i16>>,
    /// The whole new list of identities.
    pub identities: Option<Vec<IdentityInput>>,
    /// The whole new list of folders to read.
    pub folders: Option<Vec<String>>,
    /// New SMTP settings or credential.
    pub smtp: Option<SmtpChange>,
    /// New IMAP settings, or none.
    pub imap: Option<Option<ImapSettings>>,
    /// Another quota scope, or none.
    pub quota_scope: Option<Option<Id<QuotaScope>>>,
    /// A relay's webhook verification material.
    pub webhook_key: Option<SecretString>,
    /// A relay's API credential (SES, SendGrid, Mailgun), or none: sealed beside its password,
    /// and the connection is verified again.
    pub api_credential: Option<Option<ApiCredential>>,
}

/// Applies `changes` to a connection: pacing settings clear the claim's budget wait; a new
/// credential or new server settings send it back to `verifying` with a new check; a pause or
/// a resume is told to customers, a resume also putting a paced sender's clock on its next
/// phase instant. The connection's version moves whatever the change.
///
/// # Errors
///
/// No such connection, it is archived, a change breaks a rule of its provider, or the
/// database refused.
pub async fn update(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    id: Id<Connection>,
    changes: Changes,
) -> Result<(), Error> {
    let locked = lock(tx, workspace, id).await?;
    if locked.status == Status::Archived {
        return Err(Error::InvalidState(
            "The connection is archived; connect the account again to restore it.".to_owned(),
        ));
    }
    // Every update moves the version, also one that changes only rows of other tables the
    // connection shows (its identities, its folders, its webhook's key), which write no column
    // of the connection's own row: the `UPDATE` fires the row's `updated_at` trigger.
    sqlx::query!(
        "UPDATE connections SET updated_at = now() WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    let provider = locked.provider;
    let interval = changes
        .send_interval_minutes
        .map(|minutes| {
            if provider == Provider::Norbelys {
                crate::domain::senders::managed_interval(minutes)
            } else {
                changed_interval(locked.send_interval_minutes, minutes)
            }
        })
        .transpose()
        .map_err(|error| Error::invalid("/send_interval_minutes", error.to_string()))?;
    if let Some(scope) = changes.quota_scope {
        check_scope(tx, workspace, provider, scope).await?;
    }
    let pacing = changes.daily_limit.is_some()
        || interval.is_some()
        || changes.send_window.is_some()
        || changes.timezone.is_some()
        || changes.warmup_stage.is_some()
        || changes.quota_scope.is_some();
    if pacing {
        let window = match &changes.send_window {
            Some(Some(window)) => Some(
                serde_json::to_value(window)
                    .map_err(|_| Error::invalid("/send_window", "The send window is not valid."))?,
            ),
            _ => None,
        };
        sqlx::query!(
            "UPDATE connections
                SET daily_limit = coalesce($3, daily_limit),
                    send_interval_minutes = coalesce($4, send_interval_minutes),
                    send_window = CASE WHEN $5 THEN $6 ELSE send_window END,
                    timezone = coalesce($7, timezone),
                    warmup_stage = CASE WHEN $8 THEN $9 ELSE warmup_stage END,
                    quota_scope_id = CASE WHEN $10 THEN $11 ELSE quota_scope_id END,
                    next_claim_at = NULL
              WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            id.uuid(),
            changes.daily_limit,
            interval,
            changes.send_window.is_some(),
            window,
            changes.timezone,
            changes.warmup_stage.is_some(),
            changes.warmup_stage.flatten(),
            changes.quota_scope.is_some(),
            changes.quota_scope.flatten().map(|scope| scope.uuid()),
        )
        .execute(&mut **tx)
        .await?;
    }
    let mut reverify = false;
    let mut password = None;
    if let Some(change) = changes.smtp {
        let Some(current) = locked.smtp.clone() else {
            return Err(Error::invalid(
                "/smtp",
                "This connection has no SMTP settings to change.",
            ));
        };
        if provider == Provider::Norbelys {
            return Err(Error::invalid(
                "/smtp",
                "The managed MTA's settings are provisioned, not edited.",
            ));
        }
        let configuration_set = change.configuration_set.or(current.configuration_set);
        if provider == Provider::Ses && configuration_set.is_none() {
            return Err(Error::invalid(
                "/smtp/configuration_set",
                "An SES connection names its configuration set.",
            ));
        }
        let smtp = SmtpSettings {
            host: change.host.unwrap_or(current.host),
            port: change.port.unwrap_or(current.port),
            security: change.security.unwrap_or(current.security),
            username: change.username.unwrap_or(current.username),
            configuration_set,
        };
        let smtp = serde_json::to_value(&smtp)
            .map_err(|_| Error::invalid("/smtp", "The SMTP settings are not valid."))?;
        sqlx::query!(
            "UPDATE connections SET smtp = $3 WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            id.uuid(),
            smtp,
        )
        .execute(&mut **tx)
        .await?;
        password = change.password;
        reverify = true;
    }
    if let Some(api) = &changes.api_credential {
        check_api_credential(provider, api.as_ref())?;
    }
    if password.is_some() || changes.api_credential.is_some() {
        replace_credential(tx, keys, workspace, id, password, changes.api_credential).await?;
        reverify = true;
    }
    if let Some(imap) = changes.imap {
        if provider != Provider::Smtp {
            return Err(Error::invalid(
                "/imap",
                "Only an SMTP login is read over IMAP.",
            ));
        }
        let imap = imap
            .map(|imap| serde_json::to_value(&imap))
            .transpose()
            .map_err(|_| Error::invalid("/imap", "The IMAP settings are not valid."))?;
        sqlx::query!(
            "UPDATE connections SET imap = $3 WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            id.uuid(),
            imap,
        )
        .execute(&mut **tx)
        .await?;
        reverify = true;
    }
    if let Some(inputs) = &changes.identities {
        check_paced_relay_identities(
            provider,
            locked.send_interval_minutes,
            &locked.account_email,
            inputs,
        )?;
        identities::replace(tx, workspace, id, inputs).await?;
    }
    if let Some(folders) = &changes.folders {
        let reads = sqlx::query_scalar!(
            r#"SELECT provider IN ('google', 'microsoft') OR imap IS NOT NULL AS "reads!" FROM connections
                WHERE workspace_id = $1 AND id = $2"#,
            workspace.uuid(),
            id.uuid(),
        )
        .fetch_one(&mut **tx)
        .await?;
        if !reads && !folders.is_empty() {
            return Err(Error::invalid(
                "/receiving/folders",
                "Only a mailbox is read: Google, Microsoft, or an SMTP login with IMAP settings.",
            ));
        }
        bindings::set(tx, workspace, id, folders).await?;
    }
    if let Some(key) = &changes.webhook_key {
        match provider.webhook_key() {
            Some(WebhookKey::Generated) | None => {
                return Err(Error::invalid(
                    "/webhook/key",
                    "Only a relay's webhook takes the provider's key.",
                ));
            }
            Some(kind) => {
                check_webhook_key(kind, key)?;
                let webhook =
                    issue_webhook(tx, workspace, id, provider, &locked.account_email).await?;
                set_webhook_key(tx, keys, workspace, webhook, key).await?;
            }
        }
    }
    if let Some(paused) = changes.paused
        && paused != locked.paused
    {
        sqlx::query!(
            "UPDATE connections
                SET paused = $3,
                    next_send_at = CASE WHEN NOT $3 AND send_interval_minutes IS NOT NULL
                                        THEN greatest(next_send_at, next_phase_at(now(), send_phase_seconds::int))
                                        ELSE next_send_at END
              WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            id.uuid(),
            paused,
        )
        .execute(&mut **tx)
        .await?;
        health::changed(tx, workspace, id, locked.status, None, paused).await?;
    }
    if reverify {
        health::apply(
            tx,
            workspace,
            id,
            locked.status,
            changes.paused.unwrap_or(locked.paused),
            HealthEvent::VerifyRequested,
            None,
        )
        .await?;
        enqueue_proof(tx, workspace, id, provider).await?;
    }
    Ok(())
}

/// Seals a relay's or a login's credential anew through `set_connection_credential()`, which
/// bumps the version every check fences on: `password` replaces the stored password when given,
/// `api` replaces (`Some(Some)`) or removes (`Some(None)`) the API credential sealed beside it,
/// and whatever is not given is kept from the stored document, so a new password never drops an
/// API key and a new API key never drops the password.
///
/// # Errors
///
/// The stored credential does not open, the connection holds no password to keep, or the
/// database refused.
async fn replace_credential(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    id: Id<Connection>,
    password: Option<SecretString>,
    api: Option<Option<ApiCredential>>,
) -> Result<(), Error> {
    let stored = sqlx::query_scalar!(
        "SELECT connection_credential($1, $2)",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_one(&mut **tx)
    .await?;
    let (stored_password, stored_api) = match stored {
        Some(sealed) => match credentials::open_parts(keys, workspace, id, &sealed)? {
            (Credential::Password(password), api) => (Some(password), api),
            (Credential::OAuth(_), _) => (None, None),
        },
        None => (None, None),
    };
    let Some(password) = password.or(stored_password) else {
        return Err(Error::invalid(
            "/api_credential",
            "This connection holds no password to keep an API credential beside; save its SMTP credential with it.",
        ));
    };
    let api = api.unwrap_or(stored_api);
    let sealed = credentials::seal_relay(keys, workspace, id, &password, api.as_ref())?;
    sqlx::query_scalar!(
        "SELECT set_connection_credential($1, $2, $3)",
        workspace.uuid(),
        id.uuid(),
        sealed,
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(())
}

/// Checks a relay's API credential against its provider: an AWS access key (its id, letters and
/// digits, and its secret) for SES; a private API key alone for SendGrid and Mailgun; nothing
/// for any other provider, whose connections have no API to check through.
///
/// # Errors
///
/// The credential does not fit the provider.
pub fn check_api_credential(provider: Provider, api: Option<&ApiCredential>) -> Result<(), Error> {
    use secrecy::ExposeSecret as _;
    let Some(api) = api else {
        return match provider {
            Provider::Ses | Provider::Sendgrid | Provider::Mailgun => Ok(()),
            _ => Err(Error::invalid(
                "/api_credential",
                "Only an SES, SendGrid or Mailgun connection takes an API credential.",
            )),
        };
    };
    let secret = api.secret.expose_secret();
    if secret.is_empty() || secret.len() > 4_096 || secret.chars().any(char::is_control) {
        return Err(Error::invalid(
            "/api_credential/secret",
            "The API credential's secret is the key the provider shows, on one line.",
        ));
    }
    let id_valid = |id: &str| {
        (16..=128).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
    };
    match (provider, api.id.as_deref()) {
        (Provider::Ses, Some(id)) if id_valid(id) => Ok(()),
        (Provider::Ses, _) => Err(Error::invalid(
            "/api_credential/id",
            "An SES API credential is an AWS access key: its id (`AKIA…`) and its secret.",
        )),
        (Provider::Sendgrid | Provider::Mailgun, None) => Ok(()),
        (Provider::Sendgrid | Provider::Mailgun, Some(_)) => Err(Error::invalid(
            "/api_credential/id",
            "A SendGrid or Mailgun API credential is the API key alone, without an id.",
        )),
        _ => Err(Error::invalid(
            "/api_credential",
            "Only an SES, SendGrid or Mailgun connection takes an API credential.",
        )),
    }
}

/// A paced SES connection sends as one From address, its account's: one identity, that one.
///
/// # Errors
///
/// The inputs name another address or more than one.
pub fn check_paced_relay_identities(
    provider: Provider,
    interval: Option<i32>,
    account_email: &str,
    inputs: &[IdentityInput],
) -> Result<(), Error> {
    let maximum = if provider == Provider::Norbelys {
        1_000
    } else {
        50
    };
    if inputs.len() > maximum {
        return Err(Error::invalid(
            "/identities",
            format!("Give at most {maximum} sender identities per request."),
        ));
    }
    if provider != Provider::Ses || interval.is_none() {
        return Ok(());
    }
    let one = inputs.len() == 1
        && inputs
            .iter()
            .all(|input| input.email.key() == account_email.to_ascii_lowercase());
    if one {
        Ok(())
    } else {
        Err(Error::invalid(
            "/identities",
            "A paced SES connection sends as one address, its account's.",
        ))
    }
}

/// Checks a relay's webhook verification material with the verifier that will use it.
///
/// # Errors
///
/// The key is not what the provider's verifier takes.
pub fn check_webhook_key(kind: WebhookKey, key: &SecretString) -> Result<(), Error> {
    use norbelys_mail::webhooks::{mailgun, sendgrid, ses};
    use secrecy::ExposeSecret as _;
    let valid = match kind {
        WebhookKey::SigningKey => mailgun::MailgunKey::new(key).is_ok(),
        WebhookKey::VerificationKey => sendgrid::SendgridKey::new(key.expose_secret()).is_ok(),
        WebhookKey::TopicArn => ses::SnsTopic::new(key.expose_secret()).is_ok(),
        WebhookKey::Generated => false,
    };
    if valid {
        Ok(())
    } else {
        Err(Error::invalid(
            "/webhook/key",
            match kind {
                WebhookKey::SigningKey => "Mailgun's HTTP webhook signing key is not empty.",
                WebhookKey::VerificationKey => {
                    "SendGrid's verification key is the base64 public key its webhook settings show."
                }
                WebhookKey::TopicArn => {
                    "The SNS topic is an ARN: `arn:aws:sns:<region>:<account>:<name>`."
                }
                WebhookKey::Generated => "This webhook's secret is generated by Norbelys.",
            },
        ))
    }
}

/// Archives a connection: its credential is erased, its identities are archived (their addresses
/// become free for another account), its folders are no longer read, its provider webhook stays
/// active for late evidence. Archiving an archived connection changes nothing.
///
/// # Errors
///
/// No such connection, or the database refused.
pub async fn archive(tx: &mut Tx, workspace: WorkspaceId, id: Id<Connection>) -> Result<(), Error> {
    let locked = lock(tx, workspace, id).await?;
    if locked.status == Status::Archived {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE connections SET credential = NULL WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    health::apply(
        tx,
        workspace,
        id,
        locked.status,
        locked.paused,
        HealthEvent::Archived,
        None,
    )
    .await?;
    identities::archive(tx, workspace, id).await?;
    // Its identities left every pool: `senders.removed` applies each campaign's rule to their
    // conversations and fails its queued mail that is not campaign mail.
    crate::campaigns::removal::enqueue(
        tx,
        workspace,
        crate::campaigns::removal::Scope::Connection(id),
    )
    .await?;
    bindings::set(tx, workspace, id, &[]).await?;
    Ok(())
}

/// What `verify` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verified {
    /// The connection is `verifying`, its proof enqueued.
    Checking,
    /// An OAuth connection whose grant is lost: the browser must consent again.
    Consent {
        /// The provider to consent at.
        provider: Provider,
        /// The account to preselect.
        account_email: String,
    },
}

/// Checks a connection's credential now: an OAuth connection whose grant is lost asks for a
/// new consent; any other goes to `verifying` with its proof enqueued (coalescing with one
/// queued or running).
///
/// # Errors
///
/// No such connection, it is archived, or the database refused.
pub async fn verify(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Connection>,
) -> Result<Verified, Error> {
    let locked = lock(tx, workspace, id).await?;
    if locked.provider.way_in() == crate::domain::senders::WayIn::OAuth
        && locked.status == Status::AuthorizationRequired
    {
        return Ok(Verified::Consent {
            provider: locked.provider,
            account_email: locked.account_email,
        });
    }
    let applied = health::apply(
        tx,
        workspace,
        id,
        locked.status,
        locked.paused,
        HealthEvent::VerifyRequested,
        None,
    )
    .await?;
    if applied == Transition::Refused {
        return Err(Error::InvalidState(
            "The connection is archived; connect the account again to restore it.".to_owned(),
        ));
    }
    enqueue_proof(tx, workspace, id, locked.provider).await?;
    Ok(Verified::Checking)
}
