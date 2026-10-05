//! Receive bindings: the folders of a mailbox the inbox role reads, each with its
//! provider-owned cursor and a lease fenced by owner and generation.
//!
//! A connection reads none, one or several folders, at most [`MAX_FOLDERS`]; they come inside it
//! as `receiving`. A binding is never deleted (inbound messages point at it): a folder left out
//! of the list, or every folder of an archived connection, is disabled and its lease generation
//! bumped, so a poll already running cannot advance the cursor it read; its lease is left to
//! expire, so no new poll of the mailbox starts while a request of the old one may be out. A
//! folder given again is enabled again, with the cursor it had.
//!
//! Only mailboxes are read: Google and Microsoft through their APIs, an SMTP login through its
//! IMAP settings.
//!
//! Lock order: the caller holds the connection's row; bindings are written after it.

use std::collections::HashMap;

use serde::Serialize;
use uuid::Uuid;

use crate::db::Tx;
use crate::domain::ids::{Connection, Id, ReceiveBinding, WorkspaceId};
use crate::domain::time::Timestamp;

/// The most folders one connection reads.
pub const MAX_FOLDERS: usize = 10;
/// The folder a mailbox reads when none is named.
pub const INBOX: &str = "INBOX";

/// What a connection reads, as the API shows it.
#[derive(Debug, Clone, Default, Serialize, utoipa::ToSchema)]
pub struct ReceivingObject {
    /// The folders, oldest first; a disabled one is no longer read. Every enabled folder is shown,
    /// and disabled ones while there is room.
    #[schema(max_items = 10)]
    pub folders: Vec<FolderObject>,
}

/// One folder a connection reads.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct FolderObject {
    pub id: Id<ReceiveBinding>,
    /// The folder's name at the provider (`INBOX`).
    pub folder: String,
    /// Whether it is read.
    pub enabled: bool,
    /// When it was last read.
    pub polled_at: Option<Timestamp>,
    /// Consecutive failed reads.
    pub failures: i32,
    /// What the last failed read said.
    pub status_detail: Option<String>,
}

/// The receiving of `connections`, by connection: one query for a page.
///
/// # Errors
///
/// The database is unavailable.
pub async fn of_connections(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connections: &[Uuid],
) -> Result<HashMap<Uuid, ReceivingObject>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT connection_id, id AS "id: Id<ReceiveBinding>", folder, enabled, polled_at AS "polled_at: Timestamp",
                  failures, status_detail
             FROM receive_bindings WHERE workspace_id = $1 AND connection_id = ANY($2) ORDER BY id"#,
        workspace.uuid(),
        connections,
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut by_connection: HashMap<Uuid, ReceivingObject> = HashMap::new();
    for row in rows {
        by_connection
            .entry(row.connection_id)
            .or_default()
            .folders
            .push(FolderObject {
                id: row.id,
                folder: row.folder,
                enabled: row.enabled,
                polled_at: row.polled_at,
                failures: row.failures,
                status_detail: row.status_detail,
            });
    }
    // A connection reads at most MAX_FOLDERS folders (its write refuses more), but a disabled
    // binding is kept, so the folders it once read can outnumber them. The object shows every
    // enabled folder and fills what room is left with disabled ones, oldest first, so it stays
    // bounded however often the folders changed.
    for receiving in by_connection.values_mut() {
        receiving.folders.sort_by_key(|folder| !folder.enabled);
        receiving.folders.truncate(MAX_FOLDERS);
        receiving.folders.sort_by_key(|folder| folder.id.uuid());
    }
    Ok(by_connection)
}

/// Makes `folders` the connection's read folders: each is enabled (created when new), every
/// other one disabled with its lease generation bumped.
///
/// # Errors
///
/// The database refused.
pub async fn set(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    folders: &[String],
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE receive_bindings SET enabled = false, lease_generation = lease_generation + 1
          WHERE workspace_id = $1 AND connection_id = $2 AND enabled AND NOT (folder = ANY($3))",
        workspace.uuid(),
        connection.uuid(),
        folders,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "INSERT INTO receive_bindings (workspace_id, connection_id, folder)
         SELECT $1, $2, folder FROM unnest($3::text[]) AS folder
         ON CONFLICT (workspace_id, connection_id, folder) DO UPDATE SET enabled = true WHERE NOT receive_bindings.enabled",
        workspace.uuid(),
        connection.uuid(),
        folders,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Forgets the cursors of `connection`'s enabled bindings and makes their polls due at once: the
/// bounded, overlapping resync a provider's cursor reset starts by itself, asked for by an
/// operator. The inbox then reads each binding again from shortly before its last poll and
/// deduplicates by transport identity, so the overlap records nothing twice. The lease generation
/// moves, so a poll under way cannot write back the cursor it read; its lease is left to expire,
/// as a disabled binding's is. Answers how many bindings were resynced.
///
/// # Errors
///
/// The database failed.
pub async fn resync(
    tx: &mut Tx,
    workspace: WorkspaceId,
    connection: Id<Connection>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        "UPDATE receive_bindings
            SET cursor = NULL, next_poll_at = now(), failures = 0, lease_generation = lease_generation + 1
          WHERE workspace_id = $1 AND connection_id = $2 AND enabled",
        workspace.uuid(),
        connection.uuid(),
    )
    .execute(&mut **tx)
    .await?
    .rows_affected())
}
