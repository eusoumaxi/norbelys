//! Managed domain services share one private delivery login; sender addresses are identities.

use crate::db::Tx;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, WorkspaceId};
use crate::problem::Problem;

use super::{domains, identities};

/// The verified domain named by a service or an existing managed mailbox.
pub fn domain(account: &str) -> Option<String> {
    EmailAddress::parse(account)
        .ok()
        .map(|address| address.domain())
        .or_else(|| domains::hostname(account))
}

/// A tenant-scoped, private relay login for a domain service; mailbox logins retain their address.
pub fn username(workspace: WorkspaceId, account: &str) -> String {
    if account.contains('@') {
        account.to_ascii_lowercase()
    } else {
        format!(
            "norbelys-{}@{}",
            workspace.uuid().simple(),
            account.to_ascii_lowercase()
        )
    }
}

/// Adds an HTTP sender on a connected, verified domain without issuing another credential.
pub async fn ensure_sender(
    tx: &mut Tx,
    workspace: WorkspaceId,
    address: &EmailAddress,
) -> Result<(), Problem> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sender_identities WHERE workspace_id = $1 AND email_key = $2 AND archived_at IS NULL)",
    )
    .bind(workspace.uuid())
    .bind(address.key())
    .fetch_one(&mut **tx)
    .await?;
    if exists {
        return Ok(());
    }
    let domain = address.domain();
    let connection: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM connections WHERE workspace_id = $1 AND provider = 'norbelys' AND account_email_key = $2 AND status <> 'archived'",
    )
    .bind(workspace.uuid())
    .bind(&domain)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(connection) = connection else {
        return Ok(());
    };
    if !domains::mail_access(tx, workspace, &domain).await?.sends() {
        return Err(Problem::invalid_state(
            "Enable sending on this domain first.",
        ));
    }
    // Domain intent precedes the connection lock, matching connection creation and editing.
    sqlx::query("SELECT id FROM connections WHERE workspace_id = $1 AND id = $2 AND status <> 'archived' FOR UPDATE")
        .bind(workspace.uuid())
        .bind(connection)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Problem::not_found("connection"))?;
    // The live-address uniqueness constraint arbitrates concurrent additions. The connection
    // lock also serializes this path with the sender editor and service archival.
    identities::add(
        tx,
        workspace,
        Id::from_uuid(connection),
        &[identities::IdentityInput::address(address.clone(), true)],
    )
    .await?;
    Ok(())
}
