//! `norbelys-server admin connections resync <connection>`: reads a connection's mailbox again
//! from shortly before its last poll.
//!
//! The inbox reads each receive binding after a provider cursor (an IMAP `UIDVALIDITY` and last
//! UID, a Gmail history id, a Graph delta link). When a provider stops honouring a cursor, the
//! inbox restarts the read shortly before the binding's last poll by itself, and inbound messages
//! are deduplicated by their transport identity, so the overlap records nothing twice. This
//! command asks for the same bounded, overlapping resync when the operators know a cursor is wrong
//! although the provider still accepts it (a mailbox restored from a backup, replies a support
//! case says were missed): the connection's row is locked, its enabled bindings forget their
//! cursors and are due at once (`senders::bindings::resync`), and the resync is recorded with the
//! operator's reason in the workspace's audit log, in one transaction. Archived connections read
//! nothing and are refused.

use anyhow::Context as _;
use serde::Serialize;
use serde_json::json;

use crate::db::Tx;
use crate::domain::ids::{Connection, Id, Workspace, WorkspaceId};
use crate::identity::audit::{self, Action, AuditActor};
use crate::senders::bindings;

/// What a resync did, as the command prints it.
#[derive(Debug, Serialize)]
pub(crate) struct Resynced {
    /// The connection's workspace.
    pub workspace: Id<Workspace>,
    /// The connection.
    pub connection: Id<Connection>,
    /// Its enabled bindings, now due with no cursor.
    pub bindings: u64,
}

/// Resyncs `connection`'s mailbox for `reason` (see the module), inside `tx`.
///
/// # Errors
///
/// No reason, no live connection with this id, or the database failed.
pub(crate) async fn resync(
    tx: &mut Tx,
    connection: Id<Connection>,
    reason: &str,
) -> anyhow::Result<Resynced> {
    let reason = reason.trim();
    anyhow::ensure!(
        !reason.is_empty(),
        "--reason is required: it is recorded in the workspace's audit log"
    );
    let workspace = sqlx::query_scalar!(
        "SELECT workspace_id FROM connections WHERE id = $1 AND status <> 'archived' FOR UPDATE",
        connection.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .context("no live connection has this id")?;
    let workspace = WorkspaceId::trusted(workspace);
    let resynced = bindings::resync(tx, workspace, connection).await?;
    audit::record(
        tx,
        workspace,
        AuditActor::System,
        Action::ConnectionResynced,
        Some(connection.to_string()),
        json!({ "reason": reason, "bindings": resynced }),
        None,
    )
    .await?;
    Ok(Resynced {
        workspace: workspace.id(),
        connection,
        bindings: resynced,
    })
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::resync;
    use crate::testing::{SenderSpec, TestDb};

    /// An operator's resync forgets the cursor of every enabled binding of the connection, makes
    /// it due at once with its backoff cleared, and moves its lease generation so a poll under way
    /// cannot write back the cursor it read, leaving that poll's lease to expire; a disabled
    /// binding and an archived connection are left alone; the audit log names the connection and
    /// the reason. This is the bounded resync of a provider's cursor reset, on request.
    #[tokio::test]
    async fn a_resync_forgets_the_cursors_of_a_connections_enabled_bindings() {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let sender = test
            .sender(acme.id, &SenderSpec::mailbox("ada@acme.example"))
            .await;
        let binding = |folder: &'static str, enabled: bool| {
            let pool = test.system.pool().clone();
            let workspace = acme.id.uuid();
            let connection = sender.connection.uuid();
            async move {
                sqlx::query_scalar::<_, Uuid>(
                    "INSERT INTO receive_bindings (workspace_id, connection_id, folder, enabled, cursor, next_poll_at,
                                                   lease_owner, lease_expires_at, lease_generation, failures)
                     VALUES ($1, $2, $3, $4, '{\"uid_validity\": 7, \"last_uid\": 40}', now() + interval '1 hour',
                             'inbox:test', now() + interval '1 minute', 3, 2)
                     RETURNING id",
                )
                .bind(workspace)
                .bind(connection)
                .bind(folder)
                .bind(enabled)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let read = binding("INBOX", true).await;
        let left = binding("Archive", false).await;

        let mut tx = test.system.begin().await.unwrap();
        assert!(resync(&mut tx, sender.connection, "  ").await.is_err());
        let done = resync(&mut tx, sender.connection, "Ticket 7: replies missed")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(done.bindings, 1);

        let row = |id: Uuid| {
            let pool = test.system.pool().clone();
            async move {
                sqlx::query_as::<_, (bool, bool, i64, i32, bool)>(
                    "SELECT cursor IS NULL, next_poll_at <= now(), lease_generation, failures, lease_owner IS NOT NULL
                       FROM receive_bindings WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!(row(read).await, (true, true, 4, 0, true));
        assert_eq!(row(left).await, (false, false, 3, 2, true));
        let audited: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_log WHERE workspace_id = $1 AND action = 'connection.resynced'
                AND target = $2 AND details ->> 'reason' = 'Ticket 7: replies missed'",
        )
        .bind(acme.id.uuid())
        .bind(sender.connection.to_string())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        assert_eq!(audited, 1);

        sqlx::query("UPDATE connections SET status = 'archived' WHERE id = $1")
            .bind(sender.connection.uuid())
            .execute(test.system.pool())
            .await
            .unwrap();
        let mut tx = test.system.begin().await.unwrap();
        assert!(resync(&mut tx, sender.connection, "again").await.is_err());
    }
}
