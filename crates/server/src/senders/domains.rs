//! Customer domain intent: managed sending, receiving, or custom tracking at a separate hostname.
//! Ownership is proven through DNS before a managed mailbox can be provisioned.
//!
//! A domain's ownership is a TXT record at `_norbelys.<hostname>` carrying
//! `norbelys-verification=<token>`, the token generated when the domain is created. With
//! tracking on, the hostname itself is a CNAME to the platform's tracking host. `verify` sets
//! `verifying` and enqueues `domain.verify`, which reads both records through the worker's DNS
//! resolver and decides the status ([`crate::domain::senders::domain_checked`]).
//!
//! A proven hostname belongs to one workspace: the database holds every hostname out of
//! `pending_verification` unique across workspaces, so asking to verify a hostname another
//! workspace holds is a conflict. A check that finds no ownership record returns a domain never
//! proven to `pending_verification`, which frees the hostname again, and suspends one proven
//! before, which keeps it.
//!
//! **Checks.** A person's `verify` checks at once. Then every proven domain (`verified`,
//! `pending_certificate`, `active`, `suspended`) is checked again when its `next_check_at` falls due, a day
//! after its last check: `domain.verify_due` enqueues `domain.verify` for each every five
//! minutes, which also retries a `verifying` domain whose check gave up. A proven domain whose
//! record is gone is suspended; a suspended one whose record is back is verified again.
//! Once TXT and CNAME are proven, `domain.certificate` checks valid HTTPS and the
//! ingress route's domain id before activating custom links. Certificate issuance is
//! the configured reverse proxy's responsibility, using its narrow permission route.
//!
//! **The managed MTA.** Publication instructions and the public DKIM key are prepared after
//! creation, before ownership verification. This grants no SMTP or IMAP access.
//! On a deployment with one, every check that finds the ownership record
//! registers the domain on the MTA's control API with that same record's value
//! ([`super::provision::Control::register_domain`]), so the one record proves the domain to
//! both and logins of `norbelys` connections can be created on it. The MTA's SPF, DMARC and DKIM
//! records join the records to publish, preserving its advice for existing DNS records. SPF and
//! DMARC remain unchecked: their publication is not evaluated by this ownership check. Each
//! check reads whether DNS carries the DKIM record (by its key, so a DNS
//! provider's spacing does not matter). While the MTA has not verified the domain, its key is
//! not created yet, or it could not be reached, the domain is checked again five minutes later,
//! then less and less often (`domain::retry::MTA_DOMAIN`: a step doubling up to the day, counted
//! by `mta_unready_checks`) rather than every five minutes forever, and `last_error` says why
//! (unless an ownership or tracking problem does). A check that finds the MTA ready, or a
//! person's `verify`, starts the count again.
//!
//! What the API shows is computed from the row: the records to publish, each with what the last
//! check observed (`dns_checks`, which also keeps the MTA's DKIM record).

mod certificate;
mod prepare;
mod usage;
pub use certificate::DomainCertificate;
pub use prepare::{DnsPreparation, DomainPrepare, MailExchange};
pub use usage::{DomainPurpose, TrackingDomainObject, mail_access, requested};

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::provision::{MtaDnsRecord, MtaDomain};
use super::{Env, Error, Settings};
use crate::crypto;
use crate::db::Tx;
use crate::domain::ids::{Id, SendingDomain, WorkspaceId};
use crate::domain::retry;
use crate::domain::senders::domain_checked;
use crate::domain::time::Timestamp;
use crate::http::versioning;
use crate::jobs::{self, Class, Effect, Job, JobContext, JobError, Outcome, Queue};

/// How long after a check a domain is checked again when nothing waits for the managed MTA.
const RECHECK: Duration = Duration::from_secs(86_400);
/// The label under a hostname that carries its ownership token.
const OWNERSHIP_LABEL: &str = "_norbelys";
/// What the ownership TXT record carries before the token.
const OWNERSHIP_PREFIX: &str = "norbelys-verification=";
/// The statuses checked again when their `next_check_at` falls due: the proven ones, and a
/// `verifying` one whose check gave up.
const RECHECKED: [&str; 5] = [
    "verifying",
    "verified",
    "pending_certificate",
    "active",
    "suspended",
];
/// Domains the fan-out enqueues per run; the rest wait for the next run, five minutes later.
const DUE_PER_RUN: i64 = 10_000;

/// A sending domain's lifecycle (`sending_domains.status`): `pending_verification` until its
/// ownership record is found (`verifying` while a check runs), then `verified`; with tracking,
/// `pending_certificate` until its tracking host serves, then `active`; `suspended` when its
/// proven ownership record is gone; `deleting` while it is removed.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumIter,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SendingDomainStatus {
    PendingVerification,
    Verifying,
    Verified,
    PendingCertificate,
    Active,
    Suspended,
    Deleting,
}

impl SendingDomainStatus {
    /// The status as stored and shown.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The type of a DNS record to publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum DnsRecordType {
    Txt,
    Cname,
    Mx,
}

/// What a DNS record proves or serves: ownership, tracking, or managed sending authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DnsRecordPurpose {
    Ownership,
    Tracking,
    /// Authorizes the managed MTA's sending address for the domain's envelope mail.
    Spf,
    /// Publishes the domain's policy for authentication aligned with the visible From address.
    Dmarc,
    Dkim,
    Mx,
}

/// What the last check of a DNS record observed: the record as expected, its absence, or no
/// check yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DnsRecordStatus {
    Verified,
    Missing,
    Unchecked,
}

/// A DNS record a sending domain publishes.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DnsRecord {
    #[serde(rename = "type")]
    pub kind: DnsRecordType,
    /// The record's name.
    pub name: String,
    /// The record's value.
    pub value: String,
    pub purpose: DnsRecordPurpose,
    pub status: DnsRecordStatus,
    /// Publication advice from the managed MTA, including how to preserve existing DNS records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The MX preference; absent for other record types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u16>,
}

/// A sending domain as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DomainObject {
    pub id: Id<SendingDomain>,
    /// The hostname, lowercase.
    pub hostname: String,
    /// Where the domain is in its lifecycle; new values may be added.
    #[schema(value_type = SendingDomainStatus)]
    pub status: String,
    /// Whether the hostname serves tracking links.
    pub tracking_enabled: bool,
    /// The intended use of this hostname; mail and CNAME tracking use separate names.
    pub purpose: DomainPurpose,
    /// Whether publication instructions are being prepared, ready, or unavailable on this installation.
    pub dns_preparation: DnsPreparation,
    /// Optional custom tracking hosted at another hostname.
    pub tracking_domain: Option<TrackingDomainObject>,
    /// Existing MX targets discovered during DNS preparation or verification.
    #[schema(max_items = 64)]
    pub existing_mx: Vec<MailExchange>,
    /// Existing-provider and DNS conflict advice; no existing DNS record is changed by the API.
    pub warnings: Vec<String>,
    /// The records to publish: the ownership TXT record, the tracking CNAME when the hostname
    /// serves tracking links, and the managed MTA's SPF, DMARC and DKIM records once rendered.
    /// SPF and DMARC are publication instructions and remain `unchecked`.
    #[schema(max_items = 5)]
    pub records: Vec<DnsRecord>,
    /// What the last check found wrong.
    #[schema(value_type = Option<jobs::http::LastError>)]
    pub last_error: Option<Value>,
    pub verified_at: Option<Timestamp>,
    pub checked_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The domain's version, also the response's `ETag`: `updated_at` in microseconds since the
    /// Unix epoch. An update sent with it in `If-Match` applies only to this version. Each DNS
    /// check moves it too.
    pub version: i64,
}

struct Row {
    id: Id<SendingDomain>,
    hostname: String,
    status: String,
    tracking_enabled: bool,
    purpose: String,
    tracking_domain_id: Option<Uuid>,
    ownership_token: String,
    dns_checks: Value,
    last_error: Option<Value>,
    verified_at: Option<Timestamp>,
    checked_at: Option<Timestamp>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl Row {
    fn into_object(self, settings: &Settings) -> Result<DomainObject, sqlx::Error> {
        let purpose = DomainPurpose::stored(&self.purpose)?;
        let existing_mx = prepare::exchanges(&self.dns_checks);
        let warnings = prepare::warnings(purpose, &self.hostname, settings, &self.dns_checks);
        let observed = |purpose: DnsRecordPurpose| match self
            .dns_checks
            .get(<&str>::from(purpose))
            .and_then(Value::as_bool)
        {
            Some(true) => DnsRecordStatus::Verified,
            Some(false) => DnsRecordStatus::Missing,
            None => DnsRecordStatus::Unchecked,
        };
        let mut records = vec![DnsRecord {
            kind: DnsRecordType::Txt,
            name: format!("{OWNERSHIP_LABEL}.{}", self.hostname),
            value: format!("{OWNERSHIP_PREFIX}{}", self.ownership_token),
            purpose: DnsRecordPurpose::Ownership,
            status: observed(DnsRecordPurpose::Ownership),
            note: None,
            priority: None,
        }];
        if self.tracking_enabled {
            records.push(DnsRecord {
                kind: DnsRecordType::Cname,
                name: self.hostname.clone(),
                value: settings.tracking_cname_target.clone(),
                purpose: DnsRecordPurpose::Tracking,
                status: observed(DnsRecordPurpose::Tracking),
                note: None,
                priority: None,
            });
        }
        if purpose.sends() {
            if let Some(include) = &settings.mta_spf_include {
                if self.dns_checks.get("spf_record").is_none_or(Value::is_null) {
                    records.push(DnsRecord { kind: DnsRecordType::Txt, name: self.hostname.clone(),
                    value: format!("v=spf1 include:{include} ~all"), purpose: DnsRecordPurpose::Spf,
                    status: DnsRecordStatus::Unchecked, priority: None,
                    note: Some("Merge this include into your existing SPF record; keep its other authorized senders and final policy.".to_owned()) });
                }
                if self
                    .dns_checks
                    .get("dmarc_record")
                    .is_none_or(Value::is_null)
                {
                    records.push(DnsRecord { kind: DnsRecordType::Txt, name: format!("_dmarc.{}", self.hostname),
                    value: "v=DMARC1; p=none".to_owned(), purpose: DnsRecordPurpose::Dmarc,
                    status: DnsRecordStatus::Unchecked, priority: None,
                    note: Some("Keep an existing DMARC policy; this is a starting policy for domains without one.".to_owned()) });
                }
            }
            for (purpose, key) in [
                (DnsRecordPurpose::Spf, "spf_record"),
                (DnsRecordPurpose::Dmarc, "dmarc_record"),
            ] {
                if let Some(record) = self
                    .dns_checks
                    .get(key)
                    .and_then(|record| serde_json::from_value::<MtaDnsRecord>(record.clone()).ok())
                {
                    records.push(DnsRecord {
                        kind: DnsRecordType::Txt,
                        name: record.name,
                        value: record.value,
                        purpose,
                        status: DnsRecordStatus::Unchecked,
                        note: Some(record.note),
                        priority: None,
                    });
                }
            }
            if let Some((name, value)) = self.dns_checks.get("dkim_record").and_then(|record| {
                Some((
                    record.get("name")?.as_str()?,
                    record.get("value")?.as_str()?,
                ))
            }) {
                records.push(DnsRecord {
                    kind: DnsRecordType::Txt,
                    name: name.to_owned(),
                    value: value.to_owned(),
                    purpose: DnsRecordPurpose::Dkim,
                    status: observed(DnsRecordPurpose::Dkim),
                    note: None,
                    priority: None,
                });
            }
        }
        if purpose.receives()
            && self
                .dns_checks
                .get("mta_configured")
                .and_then(Value::as_bool)
                == Some(true)
        {
            records.push(DnsRecord { kind: DnsRecordType::Mx, name: self.hostname.clone(),
                value: settings.mta_submission_host.clone(), purpose: DnsRecordPurpose::Mx,
                status: observed(DnsRecordPurpose::Mx), priority: Some(10),
                note: Some("Publish only to receive through Norbelys; keep another provider's MX for send-only use.".to_owned()) });
        }
        Ok(DomainObject {
            id: self.id,
            hostname: self.hostname,
            status: self.status,
            tracking_enabled: self.tracking_enabled,
            purpose,
            dns_preparation: prepare::preparation(purpose, &self.dns_checks),
            tracking_domain: None,
            existing_mx,
            warnings,
            records,
            last_error: self.last_error,
            verified_at: self.verified_at,
            checked_at: self.checked_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
            version: versioning::of(self.updated_at),
        })
    }
}

/// `text` as a hostname: ASCII lowercase, without a trailing dot, at least two labels of 1 to 63
/// letters, digits or inner hyphens, at most 253 characters; `None` otherwise.
#[must_use]
pub fn hostname(text: &str) -> Option<String> {
    let host = text.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let valid = host.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        });
    valid.then_some(host)
}

/// Reads one sending domain of `workspace`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    settings: &Settings,
    workspace: WorkspaceId,
    id: Id<SendingDomain>,
) -> Result<Option<DomainObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"SELECT id AS "id: Id<SendingDomain>", hostname, status, purpose, tracking_domain_id, tracking_enabled, ownership_token, dns_checks,
                  last_error, verified_at AS "verified_at: Timestamp", checked_at AS "checked_at: Timestamp",
                  created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM sending_domains WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    let mut objects: Vec<_> = row
        .into_iter()
        .map(|row| Ok((row.tracking_domain_id, row.into_object(settings)?)))
        .collect::<Result<_, sqlx::Error>>()?;
    usage::attach(tx, settings, workspace, &mut objects).await?;
    Ok(objects.pop().map(|(_, object)| object))
}

/// One page of `workspace`'s sending domains in `status` (all when `None`) in id order, after
/// `cursor` when given; `limit` rows.
///
/// # Errors
///
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    settings: &Settings,
    workspace: WorkspaceId,
    status: Option<&str>,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<DomainObject>, sqlx::Error> {
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id AS "id: Id<SendingDomain>", hostname, status, purpose, tracking_domain_id, tracking_enabled, ownership_token, dns_checks,
                  last_error, verified_at AS "verified_at: Timestamp", checked_at AS "checked_at: Timestamp",
                  created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
             FROM sending_domains
            WHERE workspace_id = $1 AND ($2::text IS NULL OR status = $2)
              AND ($3::uuid IS NULL OR CASE WHEN $4 THEN id > $3 ELSE id < $3 END)
            ORDER BY CASE WHEN $4 THEN id END ASC, id DESC
            LIMIT $5"#,
        workspace.uuid(),
        status,
        cursor,
        ascending,
        limit,
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut objects: Vec<_> = rows
        .into_iter()
        .map(|row| Ok((row.tracking_domain_id, row.into_object(settings)?)))
        .collect::<Result<_, sqlx::Error>>()?;
    usage::attach(tx, settings, workspace, &mut objects).await?;
    Ok(objects.into_iter().map(|(_, object)| object).collect())
}

/// Counts `workspace`'s sending domains in `status`, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    status: Option<&str>,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM sending_domains WHERE workspace_id = $1 AND ($2::text IS NULL OR status = $2) LIMIT $3
           ) counted"#,
        workspace.uuid(),
        status,
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// Creates a hostname with explicit mail or tracking intent and an optional separate tracking host.
///
/// # Errors
/// Invalid combinations, a conflicting hostname, or unavailable storage.
pub async fn create_usage(
    tx: &mut Tx,
    workspace: WorkspaceId,
    hostname: &str,
    purpose: DomainPurpose,
    tracking_hostname: Option<&str>,
) -> Result<Id<SendingDomain>, Error> {
    if purpose == DomainPurpose::Tracking && tracking_hostname.is_some() {
        return Err(Error::invalid(
            "/tracking_hostname",
            "a tracking-only domain already names its tracking hostname",
        ));
    }
    let id = insert(tx, workspace, hostname, purpose).await?;
    if let Some(name) = tracking_hostname {
        let tracking = usage::tracking(tx, workspace, hostname, name).await?;
        sqlx::query("UPDATE sending_domains SET tracking_domain_id = $3 WHERE workspace_id = $1 AND id = $2")
            .bind(workspace.uuid()).bind(id.uuid()).bind(tracking.uuid()).execute(&mut **tx).await?;
    }
    Ok(id)
}

async fn insert(
    tx: &mut Tx,
    workspace: WorkspaceId,
    hostname: &str,
    purpose: DomainPurpose,
) -> Result<Id<SendingDomain>, Error> {
    let token = crypto::random_token(24)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO sending_domains (workspace_id, hostname, status, ownership_token, ownership_expires_at, purpose, tracking_enabled) VALUES ($1, $2, 'pending_verification', $3, now() + interval '30 days', $4, $5) RETURNING id")
        .bind(workspace.uuid()).bind(hostname).bind(token).bind(purpose.as_str()).bind(purpose == DomainPurpose::Tracking)
        .fetch_one(&mut **tx).await?;
    let id = Id::from_uuid(id);
    jobs::enqueue(tx, workspace, &DomainPrepare { domain: id }, None).await?;
    Ok(id)
}

/// Change domain intent and the optional custom tracking association atomically.
/// Managed mailboxes retain their history and explicitly disabled sender identities.
///
/// # Errors
/// The domain is missing, a hostname is invalid, a conversion would detach live
/// mailboxes or another domain's tracking host, or storage is unavailable.
pub async fn update_usage(
    tx: &mut Tx,
    settings: &Settings,
    workspace: WorkspaceId,
    id: Id<SendingDomain>,
    purpose: Option<DomainPurpose>,
    tracking_hostname: Option<Option<String>>,
) -> Result<(), Error> {
    use sqlx::Row as _;
    let row = sqlx::query("SELECT hostname, purpose FROM sending_domains WHERE workspace_id = $1 AND id = $2 FOR UPDATE")
        .bind(workspace.uuid()).bind(id.uuid()).fetch_optional(&mut **tx).await?
        .ok_or(Error::NotFound("sending domain"))?;
    let name: String = row.try_get("hostname")?;
    let old = DomainPurpose::stored(&row.try_get::<String, _>("purpose")?)?;
    let next = purpose.unwrap_or(old);
    if next == DomainPurpose::Tracking && tracking_hostname.as_ref().is_some_and(Option::is_some) {
        return Err(Error::invalid(
            "/tracking_hostname",
            "a tracking-only hostname does not need a separate tracking hostname",
        ));
    }
    if next != old {
        let referenced: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sending_domains WHERE workspace_id = $1 AND tracking_domain_id = $2)")
            .bind(workspace.uuid()).bind(id.uuid()).fetch_one(&mut **tx).await?;
        if referenced && next != DomainPurpose::Tracking {
            return Err(Error::InvalidState(
                "Detach this tracking hostname from its mail domains before changing its use."
                    .to_owned(),
            ));
        }
        let mailboxes = sqlx::query("SELECT id FROM connections WHERE workspace_id = $1 AND provider = 'norbelys' AND split_part(account_email_key, '@', 2) = $2 AND status <> 'archived' ORDER BY id FOR UPDATE")
            .bind(workspace.uuid()).bind(&name).fetch_all(&mut **tx).await?;
        if next == DomainPurpose::Tracking && !mailboxes.is_empty() {
            return Err(Error::InvalidState("Archive this domain's managed mailboxes before using its hostname for tracking only.".to_owned()));
        }
        for mailbox in mailboxes {
            let connection = Id::from_uuid(mailbox.try_get("id")?);
            let imap = next.receives().then(|| super::connections::ImapSettings {
                host: settings.mta_submission_host.clone(),
                port: 993,
                security: super::connections::ImapSecurity::Tls,
            });
            sqlx::query("UPDATE connections SET imap = $3 WHERE workspace_id = $1 AND id = $2")
                .bind(workspace.uuid())
                .bind(connection.uuid())
                .bind(
                    imap.map(serde_json::to_value)
                        .transpose()
                        .map_err(|error| Error::InvalidState(error.to_string()))?,
                )
                .execute(&mut **tx)
                .await?;
            if next.receives() && !old.receives() {
                super::bindings::set(
                    tx,
                    workspace,
                    connection,
                    &[super::bindings::INBOX.to_owned()],
                )
                .await?;
            } else if !next.receives() {
                super::bindings::set(tx, workspace, connection, &[]).await?;
            }
            if !next.sends() {
                sqlx::query("UPDATE sender_identities SET enabled = false WHERE workspace_id = $1 AND connection_id = $2")
                    .bind(workspace.uuid()).bind(connection.uuid()).execute(&mut **tx).await?;
            }
        }
    }
    let tracking = match tracking_hostname {
        Some(Some(host)) => Some(Some(
            usage::tracking(tx, workspace, &name, &host).await?.uuid(),
        )),
        Some(None) => Some(None),
        None if next == DomainPurpose::Tracking => Some(None),
        None => None,
    };
    sqlx::query("UPDATE sending_domains SET purpose = $3, tracking_domain_id = CASE WHEN $4 THEN $5 ELSE tracking_domain_id END, status = CASE WHEN purpose <> $3 THEN 'verifying' ELSE status END, dns_checks = CASE WHEN purpose <> $3 THEN dns_checks || '{\"preparation\":\"preparing\"}'::jsonb ELSE dns_checks END WHERE workspace_id = $1 AND id = $2")
        .bind(workspace.uuid()).bind(id.uuid()).bind(next.as_str()).bind(tracking.is_some()).bind(tracking.flatten())
        .execute(&mut **tx).await?;
    jobs::enqueue(tx, workspace, &DomainPrepare { domain: id }, None).await?;
    if next != old {
        jobs::enqueue(tx, workspace, &DomainVerify { domain: id }, None).await?;
    }
    Ok(())
}

/// Locks a sending domain's row for an update and returns its version, for the update's
/// `If-Match` to be checked against before anything is written; `None` when the workspace has no
/// such domain.
///
/// # Errors
///
/// The database is unavailable.
pub async fn lock_version(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<SendingDomain>,
) -> Result<Option<i64>, sqlx::Error> {
    let updated_at = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM sending_domains WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated_at.map(versioning::of))
}

/// Deletes a sending domain; returns false when the workspace has no such domain.
///
/// # Errors
///
/// The database refused.
pub async fn delete(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<SendingDomain>,
) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query_scalar!(
        "DELETE FROM sending_domains WHERE workspace_id = $1 AND id = $2 RETURNING id",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(deleted.is_some())
}

/// Asks for a DNS check now: the domain becomes `verifying` and `domain.verify` is enqueued
/// (coalescing with one queued or running).
///
/// # Errors
///
/// No such domain, the hostname is held by another workspace (`409 conflict`), or the database
/// refused.
pub async fn verify(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<SendingDomain>,
) -> Result<(), Error> {
    // A person asking starts the managed MTA's backoff again: whatever they fixed is checked
    // within minutes, not at the end of a long wait.
    let updated = sqlx::query_scalar!(
        "UPDATE sending_domains SET status = 'verifying', mta_unready_checks = 0
          WHERE workspace_id = $1 AND id = $2 RETURNING id",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        return Err(Error::NotFound("sending domain"));
    }
    jobs::enqueue(tx, workspace, &DomainVerify { domain: id }, None).await?;
    jobs::enqueue(tx, workspace, &DomainPrepare { domain: id }, None).await?;
    Ok(())
}

/// `domain.verify`: reads a sending domain's ownership TXT record and, with tracking on, its
/// CNAME, registers a proven domain on the managed MTA when the deployment has one, and records
/// what all of it says (see the module). It runs when a person asked (`verifying`) or when a
/// rechecked status fell due; otherwise (a twin ran meanwhile) it does nothing. Lookups are
/// retried on the runner's backoff when the resolver fails; a name without the record is an
/// answer, not a failure. The write is fenced on the status read, so a person's change in
/// between is never overwritten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainVerify {
    /// The domain to check.
    pub domain: Id<SendingDomain>,
}

impl Job for DomainVerify {
    const KIND: &'static str = "domain.verify";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::ExternalRetryable;

    fn unique_key(&self) -> Option<String> {
        Some(self.domain.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        let env = cx.env::<Env>()?.clone();
        let mut tx = cx.db().begin_in(workspace).await?;
        let row = sqlx::query!(
            r#"SELECT hostname, status, ownership_token, tracking_enabled, dns_checks, mta_unready_checks, purpose,
                      verified_at IS NOT NULL AS "ever_verified!", next_check_at <= now() AS "due!",
                      updated_at AS "updated_at: Timestamp"
                 FROM sending_domains WHERE workspace_id = $1 AND id = $2"#,
            workspace.uuid(),
            self.domain.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some(row) = row else {
            return Ok(Outcome::Done);
        };
        let scheduled = row.due && RECHECKED.contains(&row.status.as_str());
        if row.status != "verifying" && !scheduled {
            return Ok(Outcome::Done);
        }
        let expected = format!("{OWNERSHIP_PREFIX}{}", row.ownership_token);
        let ownership = txt_matches(
            &env,
            &format!("{OWNERSHIP_LABEL}.{}.", row.hostname),
            |text| text == expected,
        )
        .await?;
        let tracking = if row.tracking_enabled {
            Some(
                cname_is(
                    &env,
                    &format!("{}.", row.hostname),
                    &env.settings.tracking_cname_target,
                )
                .await?,
            )
        } else {
            None
        };
        let (status, problem) = domain_checked(ownership, tracking, row.ever_verified);
        let managed = if row.purpose == "tracking" {
            Managed::default()
        } else {
            managed(
                &env,
                &row.hostname,
                &expected,
                ownership,
                DomainPurpose::stored(&row.purpose)?.sends(),
                &row.dns_checks,
            )
            .await?
        };
        let mut checks = row.dns_checks.clone();
        let observations = prepare::discover(&env, &row.hostname).await?;
        if let (Some(checks), Some(observations)) =
            (checks.as_object_mut(), observations.as_object())
        {
            checks.extend(observations.clone());
        }
        let result = json!({
            "ownership": ownership,
            "tracking": tracking,
            "dkim": managed.published,
            "dkim_record": managed.record,
            "spf_record": managed.spf,
            "dmarc_record": managed.dmarc,
        });
        if let (Some(checks), Some(result)) = (checks.as_object_mut(), result.as_object()) {
            checks.extend(result.clone());
        }
        let last_error = match problem {
            Some(problem) => {
                let detail = match problem {
                    crate::domain::senders::DomainProblem::OwnershipRecordMissing => format!(
                        "No TXT record at {OWNERSHIP_LABEL}.{} carries `{expected}`.",
                        row.hostname
                    ),
                    crate::domain::senders::DomainProblem::TrackingRecordMissing => format!(
                        "{} is not a CNAME to {}.",
                        row.hostname, env.settings.tracking_cname_target
                    ),
                };
                Some(
                    json!({ "code": <&str>::from(problem), "detail": detail, "at": crate::process::now() }),
                )
            }
            None => managed.problem.as_ref().map(
                |(code, detail)| json!({ "code": code, "detail": detail, "at": crate::process::now() }),
            ),
        };
        // A domain the MTA is not ready for is checked again soon, then less and less often, as
        // its unready checks add up; any other outcome waits for the daily recheck.
        let wait = if managed.soon {
            retry::backoff(
                u32::try_from(row.mta_unready_checks).unwrap_or(0),
                &retry::MTA_DOMAIN,
                jobs::draw(),
            )
        } else {
            RECHECK
        };
        let mut chunk = cx.begin().await?;
        let changed = sqlx::query!(
            "UPDATE sending_domains
                SET status = $3, dns_checks = $4, last_error = $5, checked_at = now(),
                    mta_unready_checks = CASE WHEN $7 THEN mta_unready_checks + 1 ELSE 0 END,
                    next_check_at = now() + make_interval(secs => $9),
                    verified_at = CASE WHEN $6 THEN coalesce(verified_at, now()) ELSE verified_at END
              WHERE workspace_id = $1 AND id = $2 AND status = $8 AND updated_at = $10",
            workspace.uuid(),
            self.domain.uuid(),
            status.as_str(),
            checks,
            last_error,
            ownership,
            managed.soon,
            row.status,
            wait.as_secs_f64(),
            row.updated_at as _,
        )
        .execute(&mut **chunk.tx())
        .await?.rows_affected();
        if changed > 0 && status.as_str() == "pending_certificate" {
            jobs::enqueue(
                chunk.tx(),
                workspace,
                &DomainCertificate {
                    domain: self.domain,
                },
                None,
            )
            .await?;
        }
        cx.checkpoint(chunk, json!({ "status": status.as_str() }))
            .await?;
        Ok(Outcome::Done)
    }
}

/// What a check learned from the managed MTA: nothing on a deployment without one.
#[derive(Debug, Default)]
struct Managed {
    /// The MTA's SPF publication instruction, retained through a failed ownership or MTA check.
    spf: Option<MtaDnsRecord>,
    /// The MTA's DMARC publication instruction, retained through a failed ownership or MTA check.
    dmarc: Option<MtaDnsRecord>,
    /// The DKIM record the domain publishes for the MTA, `{name, value}`: the one the MTA just
    /// rendered, else the one a previous check kept.
    record: Option<Value>,
    /// Whether DNS carries that record; `None` when it was not looked up.
    published: Option<bool>,
    /// What went wrong with the MTA or its record: a code and a detail for `last_error`.
    problem: Option<(&'static str, String)>,
    /// Check again before the daily recheck, by the MTA's backoff: the MTA has not verified the
    /// domain, its key is pending, or it could not be reached.
    soon: bool,
}

/// Registers the domain `hostname`, whose ownership record carries `ownership` (proven when
/// `proven`), on the managed MTA, and reads whether DNS carries its DKIM record; `previous` holds
/// the records a previous check kept in `dns_checks`. An unproven domain is not registered, and
/// keeps its instructions. DKIM publication is checked only for sending; receiving
/// does not require outbound authentication records. SPF and DMARC remain instructions.
async fn managed(
    env: &Env,
    hostname: &str,
    ownership: &str,
    proven: bool,
    sending: bool,
    previous: &Value,
) -> Result<Managed, JobError> {
    let Some(control) = &env.control else {
        return Ok(Managed::default());
    };
    let mut managed = Managed {
        record: previous
            .get("dkim_record")
            .filter(|record| !record.is_null())
            .cloned(),
        spf: previous
            .get("spf_record")
            .and_then(|record| serde_json::from_value(record.clone()).ok()),
        dmarc: previous
            .get("dmarc_record")
            .and_then(|record| serde_json::from_value(record.clone()).ok()),
        ..Managed::default()
    };
    if !proven {
        return Ok(managed);
    }
    match control.register_domain(hostname, ownership).await {
        Ok(MtaDomain::Verified { spf, dmarc, dkim }) => {
            managed.spf = spf;
            managed.dmarc = dmarc;
            if let Some((name, value)) = dkim {
                managed.record = Some(json!({ "name": name, "value": value }));
            } else if sending {
                managed.soon = true;
            }
        }
        Ok(MtaDomain::Unverified(detail)) => {
            managed.soon = true;
            managed.problem = Some((
                "managed_mta_unverified",
                format!("The managed MTA has not verified {hostname} yet: {detail}"),
            ));
        }
        Err(error) => {
            managed.soon = true;
            managed.problem = Some(("managed_mta_unavailable", error.to_string()));
        }
    }
    let record = managed.record.as_ref().and_then(|record| {
        Some((
            record.get("name")?.as_str()?.to_owned(),
            record.get("value")?.as_str()?.to_owned(),
        ))
    });
    if sending && let Some((name, value)) = record {
        let published = dkim_published(env, &name, &value).await?;
        managed.published = Some(published);
        if !published && managed.problem.is_none() {
            managed.problem = Some((
                "dkim_record_missing",
                format!("No TXT record at {name} carries the managed MTA's DKIM key."),
            ));
        }
    }
    Ok(managed)
}

/// Whether a TXT record at `name` carries the DKIM key of the record `value`: compared by the key
/// itself, so a DNS provider's spacing or tag order does not matter. A name without TXT records
/// answers false.
async fn dkim_published(env: &Env, name: &str, value: &str) -> Result<bool, JobError> {
    use norbelys_mail::dkim::KeyRecord;
    let Ok(expected) = KeyRecord::parse(value) else {
        return Ok(false);
    };
    txt_matches(env, &format!("{name}."), |text| {
        KeyRecord::parse(text).is_ok_and(|published| published.key == expected.key)
    })
    .await
}

/// `domain.verify_due`: every five minutes, enqueues `domain.verify` for each sending domain
/// whose next check fell due in a rechecked status (the proven ones, and a `verifying` one whose
/// check gave up), read as the scheduler across workspaces, oldest due first. It reads no DNS
/// itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DomainVerifyDue {}

impl Job for DomainVerifyDue {
    const KIND: &'static str = "domain.verify_due";
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
        let statuses: Vec<String> = RECHECKED
            .iter()
            .map(|status| (*status).to_owned())
            .collect();
        let mut directory = cx.directory().await?;
        let due = sqlx::query!(
            "SELECT workspace_id, id FROM sending_domains
              WHERE next_check_at <= now() AND status = ANY($1)
              ORDER BY next_check_at LIMIT $2",
            &statuses,
            DUE_PER_RUN,
        )
        .fetch_all(&mut *directory)
        .await?;
        directory.commit().await?;
        let mut by_workspace: HashMap<Uuid, Vec<DomainVerify>> = HashMap::new();
        for row in due {
            by_workspace
                .entry(row.workspace_id)
                .or_default()
                .push(DomainVerify {
                    domain: Id::from_uuid(row.id),
                });
        }
        let mut enqueued = cx
            .progress()
            .and_then(|progress| progress.get("enqueued"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        for (workspace, verifies) in by_workspace {
            if cx.should_yield() {
                return Ok(Outcome::Yield {
                    after: Duration::ZERO,
                });
            }
            let workspace = WorkspaceId::trusted(workspace);
            let mut chunk = cx.begin_in(workspace).await?;
            let added = jobs::enqueue_many(chunk.tx(), workspace, &verifies, None).await?;
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

/// Whether a TXT record at `name` satisfies `matches`, given its strings joined (a long record is
/// split into strings). A name without TXT records answers false.
async fn txt_matches(
    env: &Env,
    name: &str,
    matches: impl Fn(&str) -> bool,
) -> Result<bool, JobError> {
    match env.resolver.txt(name).await {
        Ok(lookup) => Ok(lookup.answers().iter().any(|record| match &record.data {
            hickory_resolver::proto::rr::RData::TXT(txt) => {
                let joined: Vec<u8> = txt
                    .txt_data
                    .iter()
                    .flat_map(|part| part.iter().copied())
                    .collect();
                matches(&String::from_utf8_lossy(&joined))
            }
            _ => false,
        })),
        Err(error) if error.is_no_records_found() || error.is_nx_domain() => Ok(false),
        Err(error) => Err(JobError::Failed(format!(
            "the TXT lookup of {name} failed: {error}"
        ))),
    }
}

/// Whether `name` is a CNAME to `target` (compared without the trailing dot, ASCII
/// case-insensitive). A name without a CNAME answers false.
async fn cname_is(env: &Env, name: &str, target: &str) -> Result<bool, JobError> {
    use hickory_resolver::proto::rr::RData;
    match env.resolver.cname(name).await {
        Ok(lookup) => Ok(lookup.answers().iter().any(|record| match &record.data {
            RData::CNAME(cname) => cname
                .0
                .to_ascii()
                .trim_end_matches('.')
                .eq_ignore_ascii_case(target.trim_end_matches('.')),
            _ => false,
        })),
        Err(error) if error.is_no_records_found() || error.is_nx_domain() => Ok(false),
        Err(error) => Err(JobError::Failed(format!(
            "the CNAME lookup of {name} failed: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::{DomainVerifyDue, Row, hostname};
    use crate::domain::ids::{Id, SendingDomain};
    use crate::jobs::runner::Harness;
    use crate::jobs::{self, Queue, Registry};
    use crate::senders::Settings;
    use crate::testing::TestDb;

    /// A hostname is checked and normalised once at the boundary: ASCII lowercase without the
    /// trailing dot, at least two labels of letters, digits and inner hyphens; anything else is
    /// refused before it reaches the database or a DNS query.
    #[test]
    fn hostnames_are_normalised_or_refused() {
        assert_eq!(
            hostname("Mail.Example.COM.").as_deref(),
            Some("mail.example.com")
        );
        assert_eq!(
            hostname("a-b.example.com").as_deref(),
            Some("a-b.example.com")
        );
        for refused in [
            "",
            "localhost",
            "-a.example.com",
            "a-.example.com",
            "a..example.com",
            "a_b.example.com",
            "exa mple.com",
            "ex\u{e4}mple.com",
        ] {
            assert_eq!(hostname(refused), None, "{refused}");
        }
        assert_eq!(hostname(&format!("{}.com", "a".repeat(64))), None);
    }

    /// The records a sending domain shows include the MTA's authentication instructions once
    /// a check keeps them. SPF and DMARC retain publication advice and remain unchecked even
    /// if the saved observations claim success; DKIM keeps its actual observation. Without
    /// managed instructions only the domain's own records are shown.
    #[test]
    fn shows_the_managed_mtas_authentication_records() {
        let row = |dns_checks: serde_json::Value| Row {
            id: Id::from_uuid(Uuid::nil()),
            hostname: "acme.example".to_owned(),
            status: "verified".to_owned(),
            tracking_enabled: false,
            purpose: "send".to_owned(),
            tracking_domain_id: None,
            ownership_token: "t".to_owned(),
            dns_checks,
            last_error: None,
            verified_at: None,
            checked_at: None,
            created_at: crate::process::now(),
            updated_at: crate::process::now(),
        };
        let shown = |dns_checks| -> Vec<(String, String, String, String, Option<String>)> {
            row(dns_checks)
                .into_object(&Settings::for_tests())
                .expect("valid domain row")
                .records
                .into_iter()
                .map(|record| {
                    (
                        <&str>::from(record.purpose).to_owned(),
                        record.name,
                        record.value,
                        <&str>::from(record.status).to_owned(),
                        record.note,
                    )
                })
                .collect()
        };
        assert_eq!(shown(json!({ "ownership": true })).len(), 1);
        assert_eq!(
            shown(json!({
                "ownership": true,
                "dkim": false,
                "spf": true,
                "dmarc": true,
                "spf_record": {
                    "name": "acme.example",
                    "value": "v=spf1 ip4:192.0.2.10 ~all",
                    "note": "Merge into existing SPF",
                },
                "dmarc_record": {
                    "name": "_dmarc.acme.example",
                    "value": "v=DMARC1; p=none",
                    "note": "Keep an existing DMARC policy",
                },
                "dkim_record": {
                    "name": "norbelys._domainkey.acme.example",
                    "value": "v=DKIM1; k=rsa; p=MIIB",
                },
            })),
            [
                (
                    "ownership".to_owned(),
                    "_norbelys.acme.example".to_owned(),
                    "norbelys-verification=t".to_owned(),
                    "verified".to_owned(),
                    None,
                ),
                (
                    "spf".to_owned(),
                    "acme.example".to_owned(),
                    "v=spf1 ip4:192.0.2.10 ~all".to_owned(),
                    "unchecked".to_owned(),
                    Some("Merge into existing SPF".to_owned()),
                ),
                (
                    "dmarc".to_owned(),
                    "_dmarc.acme.example".to_owned(),
                    "v=DMARC1; p=none".to_owned(),
                    "unchecked".to_owned(),
                    Some("Keep an existing DMARC policy".to_owned()),
                ),
                (
                    "dkim".to_owned(),
                    "norbelys._domainkey.acme.example".to_owned(),
                    "v=DKIM1; k=rsa; p=MIIB".to_owned(),
                    "missing".to_owned(),
                    None,
                ),
            ]
        );
    }

    /// The fan-out enqueues one `domain.verify`, keyed by the domain, for each domain whose next
    /// check fell due in a rechecked status, across workspaces (a proven one, a suspended one, a
    /// `verifying` one whose check gave up), and none for a domain not due yet or one that waits
    /// for its person (`pending_verification`).
    #[tokio::test]
    async fn the_fan_out_rechecks_the_domains_due() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let globex = test.workspace("globex").await;
        let mut expected = Vec::new();
        for (workspace, hostname, status, due) in [
            (&acme, "proven.acme.example", "verified", true),
            (&acme, "fresh.acme.example", "verified", false),
            (&acme, "pending.acme.example", "pending_verification", true),
            (&globex, "gone.globex.example", "suspended", true),
            (&globex, "stuck.globex.example", "verifying", true),
        ] {
            let id = sqlx::query_scalar!(
                "INSERT INTO sending_domains (workspace_id, hostname, status, ownership_token, ownership_expires_at, next_check_at)
                 VALUES ($1, $2, $3, 't', now() + interval '30 days',
                         CASE WHEN $4 THEN now() - interval '1 minute' ELSE now() + interval '1 hour' END)
                 RETURNING id",
                workspace.id.uuid(),
                hostname,
                status,
                due,
            )
            .fetch_one(test.system.pool())
            .await
            .unwrap();
            if due && status != "pending_verification" {
                expected.push((
                    workspace.id.uuid(),
                    Id::<SendingDomain>::from_uuid(id).to_string(),
                ));
            }
        }
        let mut tx = test.worker.begin_in(jobs::SYSTEM_WORKSPACE).await.unwrap();
        jobs::enqueue(&mut tx, jobs::SYSTEM_WORKSPACE, &DomainVerifyDue {}, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut registry = Registry::default();
        registry.register::<DomainVerifyDue>().unwrap();
        let runner = Harness::new(
            test.worker.clone(),
            test.system.clone(),
            registry,
            http::Extensions::new(),
            "worker-test",
        );
        let outcomes: Vec<&str> = runner
            .run_once(Queue::Maintenance, 1)
            .await
            .into_iter()
            .map(|(_, outcome)| outcome)
            .collect();
        assert_eq!(outcomes, ["done"]);
        let mut queued: Vec<(Uuid, String)> = sqlx::query!(
            r#"SELECT workspace_id, unique_key AS "unique_key!" FROM jobs
                WHERE kind = 'domain.verify' AND state = 'available'"#
        )
        .fetch_all(test.system.pool())
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.workspace_id, row.unique_key))
        .collect();
        queued.sort();
        expected.sort();
        assert_eq!(queued, expected);
    }
}
