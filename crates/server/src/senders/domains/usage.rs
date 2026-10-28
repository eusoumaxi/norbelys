//! Explicit domain intent and the separate hostname used for custom tracking.

use serde::{Deserialize, Serialize};
use sqlx::Row as _;

use crate::db::Tx;
use crate::domain::ids::{Id, SendingDomain, WorkspaceId};
use crate::domain::time::Timestamp;

use super::{DnsRecord, DomainObject, Error, Row, Settings, hostname};

/// What this hostname is used for. Mail and CNAME tracking use separate hostnames.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DomainPurpose {
    Tracking,
    Send,
    Receive,
    SendReceive,
}

impl DomainPurpose {
    /// The wire and database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// Whether this hostname permits managed outbound mail.
    #[must_use]
    pub fn sends(self) -> bool {
        matches!(self, Self::Send | Self::SendReceive)
    }

    /// Whether this hostname permits managed incoming mail.
    #[must_use]
    pub fn receives(self) -> bool {
        matches!(self, Self::Receive | Self::SendReceive)
    }

    pub(super) fn stored(value: &str) -> Result<Self, sqlx::Error> {
        match value {
            "tracking" => Ok(Self::Tracking),
            "send" => Ok(Self::Send),
            "receive" => Ok(Self::Receive),
            "send_receive" => Ok(Self::SendReceive),
            _ => Err(sqlx::Error::Decode("invalid sending domain purpose".into())),
        }
    }
}

/// A mail domain's separately configured tracking hostname.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct TrackingDomainObject {
    pub id: Id<SendingDomain>,
    pub hostname: String,
    pub status: String,
    #[schema(max_items = 2)]
    pub records: Vec<DnsRecord>,
    pub verified_at: Option<Timestamp>,
    pub checked_at: Option<Timestamp>,
}

impl From<DomainObject> for TrackingDomainObject {
    fn from(value: DomainObject) -> Self {
        Self {
            id: value.id,
            hostname: value.hostname,
            status: value.status,
            records: value.records,
            verified_at: value.verified_at,
            checked_at: value.checked_at,
        }
    }
}

/// Validate explicit intent against the retained legacy flag.
///
/// # Errors
/// The explicit intent contradicts the legacy tracking flag.
pub fn requested(
    purpose: Option<DomainPurpose>,
    tracking: Option<bool>,
) -> Result<DomainPurpose, Error> {
    if let (Some(purpose), Some(tracking)) = (purpose, tracking)
        && tracking != (purpose == DomainPurpose::Tracking)
    {
        return Err(Error::invalid(
            "/purpose",
            "purpose and tracking_enabled describe different uses",
        ));
    }
    Ok(purpose.unwrap_or(if tracking.unwrap_or(false) {
        DomainPurpose::Tracking
    } else {
        DomainPurpose::Send
    }))
}

/// Create or reuse a workspace's separate tracking hostname, without changing its existing use.
pub(super) async fn tracking(
    tx: &mut Tx,
    workspace: WorkspaceId,
    parent: &str,
    name: &str,
) -> Result<Id<SendingDomain>, Error> {
    let name = hostname(name)
        .ok_or_else(|| Error::invalid("/tracking_hostname", "invalid tracking hostname"))?;
    if name == parent {
        return Err(Error::invalid(
            "/tracking_hostname",
            "mail and tracking need separate hostnames",
        ));
    }
    let existing = sqlx::query("SELECT id, purpose FROM sending_domains WHERE workspace_id = $1 AND hostname = $2 FOR UPDATE")
        .bind(workspace.uuid()).bind(&name).fetch_optional(&mut **tx).await?;
    if let Some(row) = existing {
        if row.try_get::<String, _>("purpose")? != "tracking" {
            return Err(Error::invalid(
                "/tracking_hostname",
                "the requested tracking hostname already has a mail use",
            ));
        }
        return Ok(Id::from_uuid(row.try_get("id")?));
    }
    super::insert(tx, workspace, &name, DomainPurpose::Tracking).await
}

/// Attach tracking details in one query for a whole page, under the same tenant transaction.
pub(super) async fn attach(
    tx: &mut Tx,
    settings: &Settings,
    workspace: WorkspaceId,
    objects: &mut [(Option<uuid::Uuid>, DomainObject)],
) -> Result<(), sqlx::Error> {
    let ids: Vec<_> = objects.iter().filter_map(|(id, _)| *id).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let rows = sqlx::query_as!(Row,
        r#"SELECT id AS "id: Id<SendingDomain>", hostname, status, purpose, tracking_domain_id, tracking_enabled, ownership_token, dns_checks,
           last_error, verified_at AS "verified_at: Timestamp", checked_at AS "checked_at: Timestamp",
           created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
           FROM sending_domains WHERE workspace_id = $1 AND id = ANY($2) AND purpose = 'tracking'"#,
        workspace.uuid(), &ids,
    ).fetch_all(&mut **tx).await?;
    let tracking: std::collections::HashMap<_, _> = rows
        .into_iter()
        .map(|row| {
            let id = row.id.uuid();
            Ok((id, TrackingDomainObject::from(row.into_object(settings)?)))
        })
        .collect::<Result<_, sqlx::Error>>()?;
    for (id, object) in objects {
        object.tracking_domain = id.and_then(|id| tracking.get(&id).cloned());
    }
    Ok(())
}

/// Check the workspace's current ownership and mail intent before provisioning a managed login.
///
/// # Errors
/// The workspace has no verified mail domain, or storage is unavailable.
pub async fn mail_access(
    tx: &mut Tx,
    workspace: WorkspaceId,
    domain: &str,
) -> Result<DomainPurpose, Error> {
    let row = sqlx::query("SELECT purpose, status FROM sending_domains WHERE workspace_id = $1 AND hostname = $2 FOR SHARE")
        .bind(workspace.uuid()).bind(domain).fetch_optional(&mut **tx).await?
        .ok_or_else(|| Error::invalid("/account_email", "add and verify this mail domain in your workspace first"))?;
    let purpose = DomainPurpose::stored(&row.try_get::<String, _>("purpose")?)?;
    if purpose == DomainPurpose::Tracking || row.try_get::<String, _>("status")? != "verified" {
        return Err(Error::invalid(
            "/account_email",
            "the address needs a verified mail domain in your workspace",
        ));
    }
    Ok(purpose)
}

/// The managed connections of `name`: a mailbox at `name` (`hello@name`) or the domain itself
/// (`name`), locked so a revocation or restore serializes with delivery Starts and a poll's claim.
async fn connections(
    tx: &mut Tx,
    workspace: WorkspaceId,
    name: &str,
) -> Result<Vec<Id<crate::domain::ids::Connection>>, sqlx::Error> {
    use sqlx::Row as _;
    let rows = sqlx::query("SELECT id FROM connections WHERE workspace_id = $1 AND provider = 'norbelys' AND (account_email_key = $2 OR split_part(account_email_key, '@', 2) = $2) AND status <> 'archived' ORDER BY id FOR UPDATE")
        .bind(workspace.uuid()).bind(name).fetch_all(&mut **tx).await?;
    rows.into_iter()
        .map(|row| Ok(Id::from_uuid(row.try_get("id")?)))
        .collect()
}

/// Stop managed mail when its domain is removed or loses ownership. The domain row is locked
/// first; connection locks serialize this revocation with delivery Starts. Existing identities,
/// receive cursors and messages remain available for review. What this disables it marks
/// `suspended_by_domain`, so a later [`restore_mail`] re-enables only that: an identity the
/// person explicitly disabled, or a folder they stopped reading, keeps its flag off and stays off.
pub(super) async fn suspend_mail(
    tx: &mut Tx,
    workspace: WorkspaceId,
    name: &str,
) -> Result<(), sqlx::Error> {
    for connection in connections(tx, workspace, name).await? {
        sqlx::query("UPDATE sender_identities SET enabled = false, suspended_by_domain = true WHERE workspace_id = $1 AND connection_id = $2 AND enabled")
            .bind(workspace.uuid()).bind(connection.uuid()).execute(&mut **tx).await?;
        // Mark the bindings being disabled before `set` clears them, so `restore_mail` re-enables
        // only these: a folder the person has already stopped reading stays disabled.
        sqlx::query("UPDATE receive_bindings SET suspended_by_domain = true WHERE workspace_id = $1 AND connection_id = $2 AND enabled")
            .bind(workspace.uuid()).bind(connection.uuid()).execute(&mut **tx).await?;
        crate::senders::bindings::set(tx, workspace, connection, &[]).await?;
    }
    Ok(())
}

/// Restore managed mail when a suspended domain is verified again, the inverse of
/// [`suspend_mail`]: an identity or binding the suspension disabled (`suspended_by_domain`) is
/// enabled again, while one the person turned off deliberately keeps its flag off and stays
/// disabled. The domain row is locked first, as the suspension is; cursors, history and messages
/// are kept. A binding's lease is left to expire and be reaped, as a disabled binding's is, so a
/// poll already running under the old lease cannot have written back its cursor.
pub(super) async fn restore_mail(
    tx: &mut Tx,
    workspace: WorkspaceId,
    name: &str,
) -> Result<(), sqlx::Error> {
    for connection in connections(tx, workspace, name).await? {
        sqlx::query("UPDATE sender_identities SET enabled = true, suspended_by_domain = false WHERE workspace_id = $1 AND connection_id = $2 AND suspended_by_domain")
            .bind(workspace.uuid()).bind(connection.uuid()).execute(&mut **tx).await?;
        sqlx::query("UPDATE receive_bindings SET enabled = true, suspended_by_domain = false WHERE workspace_id = $1 AND connection_id = $2 AND suspended_by_domain")
            .bind(workspace.uuid()).bind(connection.uuid()).execute(&mut **tx).await?;
    }
    Ok(())
}
