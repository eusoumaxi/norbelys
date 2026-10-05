//! Workspace single sign-on: SSO connections, the email domains they prove, how a sign-in is
//! routed to one, and their daily check.
//!
//! # Connections
//!
//! An administrator connects the workspace's OpenID Connect identity provider: its issuer, the
//! client id and secret (sealed with the deployment key, bound to the row, never shown again), the
//! default role of members it creates (`member` or `viewer`; `admin` only when an owner sets it),
//! just-in-time provisioning on or off, and enforcement on or off (owners only). SAML is not
//! supported. Every change to the issuer, the client, the domains, provisioning, enforcement or
//! the default role bumps the connection's `policy_version`: sessions record the version they were
//! proven under, so an edited policy stops counting every older proof at once.
//!
//! # Checks
//!
//! On every write, on `verify`, and daily (`sso.check_due` enqueues `sso.refresh_metadata` for
//! each connection), the connection is checked:
//!
//! - **Discovery.** `{issuer}/.well-known/openid-configuration` is read through the bounded
//!   fetcher of identity documents; it must name exactly the configured issuer (OpenID Connect
//!   Discovery 1.0 §4.3) and the endpoints a sign-in needs. A connection is `pending` until its
//!   discovery first succeeds and `active` from then on; a later failure is recorded in
//!   `status_detail` without stopping sign-in, which uses the provider's own answers anyway.
//! - **Domains.** An email domain routes to the connection once a TXT record at
//!   `_norbelys-sso.<domain>` carries `norbelys-sso=<token>`, the domain's ownership token. A
//!   verified domain belongs to one workspace (a unique index), so a domain another workspace
//!   proved stays unverified here. A verified domain whose record is definitively gone (no such
//!   name, or no record carrying the token) is unverified again; a failed lookup (a timeout, a
//!   server failure) changes nothing, so a DNS outage never drops a workspace's routing.
//!
//! # Routing and sign-in
//!
//! `POST /v1/auth/challenges { method: "sso", email }` is a public lookup by email domain
//! (`sso_route()`, which runs before any workspace is known): it answers the provider's
//! authorization URL when the domain is verified for an active connection, and `404 not_found`
//! otherwise. The sign-in itself is `oidc`'s, with this module's view of the connection
//! ([`for_sign_in`]).
//!
//! # Enforcement
//!
//! In a workspace with an enforcing connection, minting a workspace token needs a session proven
//! through it under its current policy version, from an identity-provider authentication younger
//! than 24 hours (`domain::identity::proof_holds`). Turning enforcement on requires the owner's
//! own current session to satisfy it already, so an owner cannot lock themself and everyone else
//! out with one click. The connection records when its enforcement turned on (`enforced_at`,
//! cleared when it turns off): an owner locked out later by a broken identity provider proves
//! themself to an operator with a recovery code registered before that instant, and gets a
//! break-glass session (`identity::recovery`).

use std::time::Duration;

use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::fetch::Fetcher;
use crate::crypto::{self, CryptoError, Keys};
use crate::db::Tx;
use crate::domain::ids::{Id, SsoConnection, WorkspaceId};
use crate::domain::scope::MembershipRole;
use crate::domain::time::Timestamp;
use crate::http::versioning;
use crate::jobs::{self, Class, Effect, Job, JobContext, JobError, Outcome, Queue};

/// The label under a domain that carries its ownership token.
pub const DNS_LABEL: &str = "_norbelys-sso";
/// What the ownership TXT record carries before the token.
pub const TXT_PREFIX: &str = "norbelys-sso=";
/// Connections a run of the daily check visits; the rest wait for the next run.
const DUE_PER_RUN: i64 = 5_000;

/// Why a connection could not be written.
#[derive(Debug, thiserror::Error)]
pub enum SsoError {
    /// A domain belongs to another connection of the workspace.
    #[error("`{0}` belongs to another SSO connection of this workspace")]
    DomainInUse(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

/// The DNS record that proves a domain.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SsoDnsRecord {
    /// Always `TXT`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// `_norbelys-sso.<domain>`.
    pub name: String,
    /// `norbelys-sso=<token>`.
    pub value: String,
}

/// An email domain of a connection.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SsoDomainObject {
    /// The domain, lowercase.
    pub domain: String,
    /// When its record was found; absent while it does not route.
    pub verified_at: Option<Timestamp>,
    /// The record to publish.
    pub record: SsoDnsRecord,
}

/// An SSO connection as the dashboard shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SsoConnectionObject {
    pub id: Id<SsoConnection>,
    /// A name for people.
    pub name: String,
    /// The OpenID Connect issuer.
    pub issuer: String,
    /// The client id registered at the provider.
    pub client_id: String,
    /// Whether a client secret is stored (it is never shown).
    pub client_secret_set: bool,
    /// The role of members created by just-in-time provisioning.
    pub default_role: MembershipRole,
    /// Whether signing in creates a membership for people of the proved domains.
    pub jit_provisioning: bool,
    /// Whether every credential step of the workspace requires this connection.
    pub enforced: bool,
    /// Moves with every policy change; older proofs stop counting.
    pub policy_version: i32,
    /// `pending` (discovery never succeeded), `active` or `disabled`; new values may be added.
    #[schema(extensions(("x-open-enum" = json!(true))))]
    pub status: String,
    /// What the last check found wrong.
    pub status_detail: Option<String>,
    /// When the discovery document was last read.
    pub metadata_fetched_at: Option<Timestamp>,
    /// The email domains it is authoritative for, at most 20.
    #[schema(max_items = 20)]
    pub domains: Vec<SsoDomainObject>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The version `If-Match` names.
    pub version: i64,
}

/// The sealing context of a connection's client secret: bound to its row.
pub(crate) fn secret_context(workspace: WorkspaceId, id: Uuid) -> String {
    format!("sso_connections.client_secret:{}:{id}", workspace.uuid())
}

/// The record that proves `domain` with `token`.
fn dns_record(domain: &str, token: &str) -> SsoDnsRecord {
    SsoDnsRecord {
        kind: "TXT",
        name: format!("{DNS_LABEL}.{domain}"),
        value: format!("{TXT_PREFIX}{token}"),
    }
}

/// Normalises an email domain: trimmed, ASCII lowercase, without a trailing dot; `None` for a
/// value that is not a host name with at least two labels.
#[must_use]
pub fn normalise_domain(domain: &str) -> Option<String> {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels_ok = domain.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    });
    (labels_ok && domain.contains('.') && domain.len() <= 253).then_some(domain)
}

/// What the discovery check found.
#[derive(Debug, Clone)]
pub enum Discovery {
    /// The document, naming the issuer and the endpoints a sign-in needs.
    Found(Value),
    /// Why it could not be used, for `status_detail`.
    Failed(String),
}

/// Reads and checks `issuer`'s discovery document (see the module).
pub async fn discover(fetcher: &Fetcher, issuer: &str) -> Discovery {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let Ok(url) = url::Url::parse(&url) else {
        return Discovery::Failed("the issuer is not a URL".to_owned());
    };
    let document = match fetcher.json(&url).await {
        Ok(document) => document,
        Err(error) => return Discovery::Failed(format!("discovery failed: {error}")),
    };
    match check_discovery(&document, issuer) {
        Ok(()) => Discovery::Found(document),
        Err(reason) => Discovery::Failed(reason),
    }
}

/// The checks of a discovery document: the issuer exactly as configured, and the endpoints a
/// sign-in needs.
///
/// # Errors
///
/// Why the document cannot be used.
pub fn check_discovery(document: &Value, issuer: &str) -> Result<(), String> {
    if document.get("issuer").and_then(Value::as_str) != Some(issuer) {
        return Err(format!(
            "the discovery document names another issuer than `{issuer}`"
        ));
    }
    for field in ["authorization_endpoint", "token_endpoint", "jwks_uri"] {
        if document.get(field).and_then(Value::as_str).is_none() {
            return Err(format!("the discovery document has no `{field}`"));
        }
    }
    serde_json::from_value::<openidconnect::core::CoreProviderMetadata>(document.clone())
        .map(|_| ())
        .map_err(|error| format!("the discovery document is not valid: {error}"))
}

/// What DNS said about a domain's ownership record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainProof {
    /// A TXT record carries the token.
    Found,
    /// DNS answered, definitively, that no record carries it.
    Missing,
    /// The lookup failed; nothing is concluded.
    Unknown,
}

/// Whether any of `records` (each a TXT record's strings, joined) is exactly the ownership value
/// of `token`.
#[must_use]
pub fn records_prove(records: &[Vec<u8>], token: &str) -> bool {
    let expected = format!("{TXT_PREFIX}{token}");
    records.iter().any(|record| record == expected.as_bytes())
}

/// Looks up `domain`'s ownership record for `token`.
pub async fn prove(resolver: &crate::dns::Resolver, domain: &str, token: &str) -> DomainProof {
    match resolver.txt(&format!("{DNS_LABEL}.{domain}")).await {
        Ok(lookup) => {
            let records: Vec<Vec<u8>> = lookup
                .answers()
                .iter()
                .filter_map(|record| match &record.data {
                    hickory_resolver::proto::rr::RData::TXT(txt) => Some(
                        txt.txt_data
                            .iter()
                            .flat_map(|part| part.iter().copied())
                            .collect(),
                    ),
                    _ => None,
                })
                .collect();
            if records_prove(&records, token) {
                DomainProof::Found
            } else {
                DomainProof::Missing
            }
        }
        Err(error) if error.is_no_records_found() || error.is_nx_domain() => DomainProof::Missing,
        Err(_) => DomainProof::Unknown,
    }
}

/// A connection to create.
#[derive(Debug, Clone)]
pub struct NewConnection<'a> {
    /// A name for people.
    pub name: &'a str,
    /// The issuer.
    pub issuer: &'a str,
    /// The client id.
    pub client_id: &'a str,
    /// The client secret, for a confidential client.
    pub client_secret: Option<&'a SecretString>,
    /// The default role of provisioned members.
    pub default_role: MembershipRole,
    /// Just-in-time provisioning.
    pub jit_provisioning: bool,
    /// Enforcement.
    pub enforced: bool,
    /// The email domains, normalised.
    pub domains: &'a [String],
}

/// Creates a connection in `workspace` (its domains unverified; the checks come after).
///
/// # Errors
///
/// [`SsoError`].
pub async fn create(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    new: &NewConnection<'_>,
) -> Result<Id<SsoConnection>, SsoError> {
    let id = Id::<SsoConnection>::new();
    let secret = new
        .client_secret
        .map(|secret| {
            keys.seal(
                secret.expose_secret().as_bytes(),
                secret_context(workspace, id.uuid()).as_bytes(),
            )
        })
        .transpose()?;
    sqlx::query!(
        "INSERT INTO sso_connections (workspace_id, id, kind, name, issuer, client_id, client_secret,
                                      default_role, jit_provisioning, enforced, enforced_at)
         VALUES ($1, $2, 'oidc', $3, $4, $5, $6, $7, $8, $9, CASE WHEN $9 THEN now() END)",
        workspace.uuid(),
        id.uuid(),
        new.name,
        new.issuer,
        new.client_id,
        secret,
        new.default_role.as_str(),
        new.jit_provisioning,
        new.enforced,
    )
    .execute(&mut **tx)
    .await?;
    replace_domains(tx, workspace, id.uuid(), new.domains).await?;
    Ok(id)
}

/// Replaces a connection's domains with `domains`: those kept keep their token and proof, new
/// ones get a fresh token.
async fn replace_domains(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Uuid,
    domains: &[String],
) -> Result<(), SsoError> {
    let taken = sqlx::query_scalar!(
        "SELECT domain FROM sso_email_domains
          WHERE workspace_id = $1 AND domain = ANY($2) AND sso_connection_id <> $3 LIMIT 1",
        workspace.uuid(),
        domains,
        connection,
    )
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(domain) = taken {
        return Err(SsoError::DomainInUse(domain));
    }
    sqlx::query!(
        "DELETE FROM sso_email_domains WHERE workspace_id = $1 AND sso_connection_id = $2 AND NOT (domain = ANY($3))",
        workspace.uuid(),
        connection,
        domains,
    )
    .execute(&mut **tx)
    .await?;
    for domain in domains {
        sqlx::query!(
            "INSERT INTO sso_email_domains (workspace_id, sso_connection_id, domain, ownership_token)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (workspace_id, domain) DO NOTHING",
            workspace.uuid(),
            connection,
            domain,
            crypto::random_token(18)?,
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// What an update may change.
#[derive(Debug, Clone, Default)]
pub struct Changes<'a> {
    /// A new name (no policy change).
    pub name: Option<&'a str>,
    /// A new issuer.
    pub issuer: Option<&'a str>,
    /// A new client id.
    pub client_id: Option<&'a str>,
    /// A new client secret.
    pub client_secret: Option<&'a SecretString>,
    /// A new default role.
    pub default_role: Option<MembershipRole>,
    /// Provisioning on or off.
    pub jit_provisioning: Option<bool>,
    /// Enforcement on or off.
    pub enforced: Option<bool>,
    /// The new list of domains, normalised.
    pub domains: Option<&'a [String]>,
}

impl Changes<'_> {
    /// Whether the change touches the policy (and so bumps its version).
    #[must_use]
    pub fn changes_policy(&self) -> bool {
        self.issuer.is_some()
            || self.client_id.is_some()
            || self.client_secret.is_some()
            || self.default_role.is_some()
            || self.jit_provisioning.is_some()
            || self.enforced.is_some()
            || self.domains.is_some()
    }
}

/// Locks connection `id` and answers its current version and its enforcement, for an update.
///
/// # Errors
///
/// The database failed.
pub async fn lock(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<SsoConnection>,
) -> Result<Option<Locked>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT updated_at AS "updated_at: Timestamp", enforced FROM sso_connections
            WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| Locked {
        version: versioning::of(row.updated_at),
        enforced: row.enforced,
    }))
}

/// A locked connection's state.
#[derive(Debug, Clone, Copy)]
pub struct Locked {
    /// Its version.
    pub version: i64,
    /// Whether it enforces.
    pub enforced: bool,
}

/// Applies `changes` to connection `id` (the caller holds its lock and checked `If-Match`).
///
/// # Errors
///
/// [`SsoError`].
pub async fn update(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    id: Id<SsoConnection>,
    changes: &Changes<'_>,
) -> Result<(), SsoError> {
    let secret = changes
        .client_secret
        .map(|secret| {
            keys.seal(
                secret.expose_secret().as_bytes(),
                secret_context(workspace, id.uuid()).as_bytes(),
            )
        })
        .transpose()?;
    sqlx::query!(
        "UPDATE sso_connections
            SET name = coalesce($3, name), issuer = coalesce($4, issuer), client_id = coalesce($5, client_id),
                client_secret = coalesce($6, client_secret), default_role = coalesce($7, default_role),
                jit_provisioning = coalesce($8, jit_provisioning), enforced = coalesce($9, enforced),
                enforced_at = CASE WHEN coalesce($9, enforced) THEN coalesce(enforced_at, now()) END,
                policy_version = policy_version + CASE WHEN $10 THEN 1 ELSE 0 END,
                status = CASE WHEN $4::text IS NOT NULL AND $4 <> issuer THEN 'pending' ELSE status END
          WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
        changes.name,
        changes.issuer,
        changes.client_id,
        secret,
        changes.default_role.map(MembershipRole::as_str),
        changes.jit_provisioning,
        changes.enforced,
        changes.changes_policy(),
    )
    .execute(&mut **tx)
    .await?;
    if let Some(domains) = changes.domains {
        replace_domains(tx, workspace, id.uuid(), domains).await?;
    }
    Ok(())
}

/// Deletes connection `id` and its domains; true when it existed.
///
/// # Errors
///
/// The database failed.
pub async fn delete(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<SsoConnection>,
) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query!(
        "DELETE FROM sso_connections WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(deleted > 0)
}

/// The issuer and the domains (with their tokens) of connection `id`, for its checks.
///
/// # Errors
///
/// The database failed.
pub async fn check_targets(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<(String, Vec<(String, String)>)>, sqlx::Error> {
    let issuer = sqlx::query_scalar!(
        "SELECT issuer FROM sso_connections WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id,
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(issuer) = issuer else {
        return Ok(None);
    };
    let domains = sqlx::query!(
        "SELECT domain, ownership_token FROM sso_email_domains
          WHERE workspace_id = $1 AND sso_connection_id = $2 ORDER BY domain",
        workspace.uuid(),
        id,
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| (row.domain, row.ownership_token))
    .collect();
    Ok(Some((issuer, domains)))
}

/// What the checks of a connection found.
#[derive(Debug, Clone)]
pub struct Report {
    /// The discovery check, for the issuer it read.
    pub issuer: String,
    /// Its outcome.
    pub discovery: Discovery,
    /// Each domain's proof.
    pub domains: Vec<(String, DomainProof)>,
}

/// Runs the checks of a connection: discovery, and each domain's proof (see the module). No
/// database work: the caller loads the targets, commits, calls this, then records it.
pub async fn check(
    fetcher: &Fetcher,
    resolver: &crate::dns::Resolver,
    issuer: &str,
    domains: &[(String, String)],
) -> Report {
    let discovery = discover(fetcher, issuer).await;
    let mut proofs = Vec::with_capacity(domains.len());
    for (domain, token) in domains {
        proofs.push((domain.clone(), prove(resolver, domain, token).await));
    }
    Report {
        issuer: issuer.to_owned(),
        discovery,
        domains: proofs,
    }
}

/// Records `report` on connection `id`: its status, metadata and detail, and its domains'
/// proofs. A domain another workspace verified stays unverified here (its detail says so).
///
/// # Errors
///
/// The database failed.
pub async fn record(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Uuid,
    report: &Report,
) -> Result<(), sqlx::Error> {
    let mut details = Vec::new();
    match &report.discovery {
        Discovery::Found(document) => {
            sqlx::query!(
                "UPDATE sso_connections SET metadata = $3, metadata_fetched_at = now(), status = 'active'
                  WHERE workspace_id = $1 AND id = $2 AND issuer = $4",
                workspace.uuid(),
                id,
                document,
                report.issuer,
            )
            .execute(&mut **tx)
            .await?;
        }
        Discovery::Failed(reason) => details.push(reason.clone()),
    }
    for (domain, proof) in &report.domains {
        match proof {
            DomainProof::Found => {
                // A domain another workspace verified makes the update a unique violation; the
                // savepoint keeps the rest of the record.
                let mut savepoint = sqlx::Acquire::begin(&mut **tx).await?;
                let verified = sqlx::query!(
                    "UPDATE sso_email_domains SET verified_at = now()
                      WHERE workspace_id = $1 AND sso_connection_id = $2 AND domain = $3 AND verified_at IS NULL",
                    workspace.uuid(),
                    id,
                    domain,
                )
                .execute(&mut *savepoint)
                .await;
                match verified {
                    Ok(_) => savepoint.commit().await?,
                    Err(sqlx::Error::Database(error))
                        if error.code().as_deref() == Some("23505") =>
                    {
                        savepoint.rollback().await?;
                        details.push(format!("`{domain}` is verified by another workspace"));
                    }
                    Err(error) => return Err(error),
                }
            }
            DomainProof::Missing => {
                sqlx::query!(
                    "UPDATE sso_email_domains SET verified_at = NULL
                      WHERE workspace_id = $1 AND sso_connection_id = $2 AND domain = $3",
                    workspace.uuid(),
                    id,
                    domain,
                )
                .execute(&mut **tx)
                .await?;
                details.push(format!(
                    "no TXT record at `{DNS_LABEL}.{domain}` carries the domain's token"
                ));
            }
            DomainProof::Unknown => details.push(format!("the DNS lookup of `{domain}` failed")),
        }
    }
    let detail = (!details.is_empty()).then(|| details.join("; "));
    sqlx::query!(
        "UPDATE sso_connections SET status_detail = $3 WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id,
        detail,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

struct ConnectionRow {
    id: Id<SsoConnection>,
    name: String,
    issuer: String,
    client_id: String,
    secret_set: bool,
    default_role: String,
    jit_provisioning: bool,
    enforced: bool,
    policy_version: i32,
    status: String,
    status_detail: Option<String>,
    metadata_fetched_at: Option<Timestamp>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

/// Connections `ids` of `workspace` with their domains, in the order of `ids`.
async fn objects(
    tx: &mut Tx,
    workspace: WorkspaceId,
    rows: Vec<ConnectionRow>,
) -> Result<Vec<SsoConnectionObject>, sqlx::Error> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id.uuid()).collect();
    let domains = sqlx::query!(
        r#"SELECT sso_connection_id, domain, ownership_token, verified_at AS "verified_at: Timestamp"
             FROM sso_email_domains WHERE workspace_id = $1 AND sso_connection_id = ANY($2)
            ORDER BY domain"#,
        workspace.uuid(),
        &ids,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(SsoConnectionObject {
                domains: domains
                    .iter()
                    .filter(|domain| domain.sso_connection_id == row.id.uuid())
                    .map(|domain| SsoDomainObject {
                        record: dns_record(&domain.domain, &domain.ownership_token),
                        domain: domain.domain.clone(),
                        verified_at: domain.verified_at,
                    })
                    .collect(),
                id: row.id,
                name: row.name,
                issuer: row.issuer,
                client_id: row.client_id,
                client_secret_set: row.secret_set,
                default_role: row.default_role.parse().ok()?,
                jit_provisioning: row.jit_provisioning,
                enforced: row.enforced,
                policy_version: row.policy_version,
                status: row.status,
                status_detail: row.status_detail,
                metadata_fetched_at: row.metadata_fetched_at,
                created_at: row.created_at,
                version: versioning::of(row.updated_at),
                updated_at: row.updated_at,
            })
        })
        .collect())
}

/// Connection `id` of `workspace`.
///
/// # Errors
///
/// The database failed.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<SsoConnection>,
) -> Result<Option<SsoConnectionObject>, sqlx::Error> {
    let rows = sqlx::query_as!(
        ConnectionRow,
        r#"SELECT id AS "id: Id<SsoConnection>", name, issuer, client_id, client_secret IS NOT NULL AS "secret_set!",
                  default_role, jit_provisioning, enforced, policy_version, status, status_detail,
                  metadata_fetched_at AS "metadata_fetched_at: Timestamp", created_at AS "created_at: Timestamp",
                  updated_at AS "updated_at: Timestamp"
             FROM sso_connections WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(objects(tx, workspace, rows).await?.into_iter().next())
}

/// One page of `workspace`'s connections by id.
///
/// # Errors
///
/// The database failed.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    after: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<SsoConnectionObject>, sqlx::Error> {
    let rows = sqlx::query_as!(
        ConnectionRow,
        r#"SELECT id AS "id: Id<SsoConnection>", name, issuer, client_id, client_secret IS NOT NULL AS "secret_set!",
                  default_role, jit_provisioning, enforced, policy_version, status, status_detail,
                  metadata_fetched_at AS "metadata_fetched_at: Timestamp", created_at AS "created_at: Timestamp",
                  updated_at AS "updated_at: Timestamp"
             FROM sso_connections
            WHERE workspace_id = $1 AND ($2::uuid IS NULL OR CASE WHEN $3 THEN id > $2 ELSE id < $2 END)
            ORDER BY CASE WHEN $3 THEN id END, id DESC LIMIT $4"#,
        workspace.uuid(),
        after,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    objects(tx, workspace, rows).await
}

/// The workspace and connection an email domain routes to, found before any workspace is known.
///
/// # Errors
///
/// The database failed.
pub async fn route(tx: &mut Tx, domain: &str) -> Result<Option<(WorkspaceId, Uuid)>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT workspace_id AS "workspace_id!", sso_connection_id AS "sso_connection_id!"
             FROM sso_route($1)"#,
        domain
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| {
        (
            WorkspaceId::trusted(row.workspace_id),
            row.sso_connection_id,
        )
    }))
}

/// A connection as a sign-in through it needs it.
#[derive(Debug, Clone)]
pub struct SignInConnection {
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// The connection.
    pub id: Uuid,
    /// The issuer.
    pub issuer: String,
    /// The client id.
    pub client_id: String,
    /// The client secret, opened.
    pub client_secret: Option<SecretString>,
    /// The policy version now.
    pub policy_version: i32,
    /// The default role of provisioned members.
    pub default_role: MembershipRole,
    /// Just-in-time provisioning.
    pub jit_provisioning: bool,
    /// Whether it is active.
    pub active: bool,
    /// Its verified domains.
    pub verified_domains: Vec<String>,
}

/// Connection `id` of `workspace` for a sign-in, its secret opened; read inside the workspace.
///
/// # Errors
///
/// The database failed, or the secret does not open.
pub async fn for_sign_in(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<SignInConnection>, SsoError> {
    let row = sqlx::query!(
        "SELECT issuer, client_id, client_secret, policy_version, default_role, jit_provisioning, status
           FROM sso_connections WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id,
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let client_secret = row
        .client_secret
        .map(|sealed| keys.open(&sealed, secret_context(workspace, id).as_bytes()))
        .transpose()?
        .map(|plain| SecretString::from(String::from_utf8_lossy(&plain).into_owned()));
    let verified_domains = sqlx::query_scalar!(
        "SELECT domain FROM sso_email_domains
          WHERE workspace_id = $1 AND sso_connection_id = $2 AND verified_at IS NOT NULL",
        workspace.uuid(),
        id,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(Some(SignInConnection {
        workspace,
        id,
        issuer: row.issuer,
        client_id: row.client_id,
        client_secret,
        policy_version: row.policy_version,
        default_role: row.default_role.parse().unwrap_or(MembershipRole::Member),
        jit_provisioning: row.jit_provisioning,
        active: row.status == "active",
        verified_domains,
    }))
}

/// `sso.check_due`: every day at 01:00 UTC, enqueues `sso.refresh_metadata` for every SSO
/// connection of every workspace, read from the scheduler's view of `sso_connections` (ids and
/// status only); it fetches nothing itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SsoCheckDue {}

impl Job for SsoCheckDue {
    const KIND: &'static str = "sso.check_due";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::Idempotent;
    const CLASS: Class = Class::FanOut;

    fn unique_key(&self) -> Option<String> {
        Some("singleton".to_owned())
    }

    fn schedule() -> Option<(&'static str, Self)> {
        Some(("0 1 * * *", Self {}))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let mut directory = cx.directory().await?;
        let due = sqlx::query!(
            "SELECT workspace_id, id FROM sso_connections ORDER BY workspace_id, id LIMIT $1",
            DUE_PER_RUN,
        )
        .fetch_all(&mut *directory)
        .await?;
        directory.commit().await?;
        let mut by_workspace: std::collections::BTreeMap<Uuid, Vec<SsoRefreshMetadata>> =
            std::collections::BTreeMap::new();
        for row in due {
            by_workspace
                .entry(row.workspace_id)
                .or_default()
                .push(SsoRefreshMetadata { connection: row.id });
        }
        let mut enqueued = cx
            .progress()
            .and_then(|progress| progress.get("enqueued"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        for (workspace, refreshes) in by_workspace {
            if cx.should_yield() {
                return Ok(Outcome::Yield {
                    after: Duration::ZERO,
                });
            }
            let workspace = WorkspaceId::trusted(workspace);
            let mut chunk = cx.begin_in(workspace).await?;
            let added = jobs::enqueue_many(chunk.tx(), workspace, &refreshes, None).await?;
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

/// `sso.refresh_metadata`: checks one connection (discovery and its domains' proofs) and records
/// what it found (see the module). It loads its targets, commits, calls out, then records in a
/// chunk; a repeat is harmless.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SsoRefreshMetadata {
    /// The connection.
    pub connection: Uuid,
}

impl Job for SsoRefreshMetadata {
    const KIND: &'static str = "sso.refresh_metadata";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::ExternalRetryable;

    fn unique_key(&self) -> Option<String> {
        Some(self.connection.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        let mut chunk = cx.begin().await?;
        let targets = check_targets(chunk.tx(), workspace, self.connection).await?;
        cx.checkpoint(chunk, json!({ "step": "loaded" })).await?;
        let Some((issuer, domains)) = targets else {
            return Ok(Outcome::Done);
        };
        let fetcher = cx.env::<Fetcher>()?.clone();
        let resolver = cx.env::<crate::dns::Resolver>()?.clone();
        let report = check(&fetcher, &resolver, &issuer, &domains).await;
        let mut chunk = cx.begin().await?;
        record(chunk.tx(), workspace, self.connection, &report).await?;
        cx.checkpoint(chunk, json!({ "step": "recorded" })).await?;
        Ok(Outcome::Done)
    }
}

/// Counts matching rows without materializing their data, bounded to `cap + 1`.
///
/// # Errors
///
/// The database failed.
pub async fn count(tx: &mut Tx, workspace: WorkspaceId, cap: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM (SELECT 1 FROM sso_connections WHERE workspace_id = $1 LIMIT $2) counted").bind(workspace.uuid()).bind(cap.saturating_add(1)).fetch_one(&mut **tx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Domains are read as host names in lowercase without their trailing dot, and anything that
    /// is not one (a single label, an address, characters a host name cannot hold) is refused.
    #[test]
    fn domains_normalise_or_are_refused() {
        assert_eq!(
            normalise_domain(" Example.COM. ").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            normalise_domain("a-b.example.co.uk").as_deref(),
            Some("a-b.example.co.uk")
        );
        for refused in [
            "localhost",
            "-a.example.com",
            "a_b.example.com",
            "",
            "ex ample.com",
            "a..b",
        ] {
            assert_eq!(normalise_domain(refused), None, "{refused}");
        }
    }

    /// Only a record that is exactly the ownership value proves a domain: another token, a value
    /// with something around it or no record at all does not.
    #[test]
    fn only_the_exact_record_proves_a_domain() {
        let token = "t0k3n";
        assert!(records_prove(&[b"norbelys-sso=t0k3n".to_vec()], token));
        assert!(records_prove(
            &[b"v=spf1 -all".to_vec(), b"norbelys-sso=t0k3n".to_vec()],
            token
        ));
        for records in [
            vec![b"norbelys-sso=other".to_vec()],
            vec![b"norbelys-sso=t0k3n extra".to_vec()],
            vec![b"t0k3n".to_vec()],
            vec![],
        ] {
            assert!(!records_prove(&records, token), "{records:?}");
        }
    }

    /// A discovery document is usable only when it names exactly the configured issuer (a
    /// trailing slash is another issuer) and the endpoints a sign-in needs.
    #[test]
    fn discovery_documents_must_name_their_issuer() {
        let issuer = "https://idp.example.com";
        let document = json!({
            "issuer": issuer,
            "authorization_endpoint": "https://idp.example.com/authorize",
            "token_endpoint": "https://idp.example.com/token",
            "jwks_uri": "https://idp.example.com/jwks",
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
        });
        assert_eq!(check_discovery(&document, issuer), Ok(()));
        assert!(check_discovery(&document, "https://idp.example.com/").is_err());
        for field in ["authorization_endpoint", "token_endpoint", "jwks_uri"] {
            let mut incomplete = document.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(check_discovery(&incomplete, issuer).is_err(), "{field}");
        }
    }
}
