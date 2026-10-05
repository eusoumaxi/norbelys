//! This node's bounded, durable queue of captured evidence awaiting Turso confirmation.
//!
//! One shared handle serializes every read and write. SQLite uses WAL, synchronous FULL,
//! and a hard page limit of one third of the configured byte budget. No reader holds a
//! transaction across calls; checkpointing truncates the WAL after every write. The remaining
//! budget covers the largest WAL and transaction overhead. Appends publish the source cursor
//! in the same transaction as its records. Only a confirmed remote commit permits deletion.
//! The canonical history, configuration and account directory never enter this file.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

/// The smallest usable budget, including SQLite and its WAL.
const MIN_BUDGET: u64 = 4 * 1024 * 1024;
/// Maximum encoded record; SMTP evidence is metadata, never a message body.
const MAX_RECORD: usize = 128 * 1024;
/// Maximum records and encoded bytes handed to one remote transaction.
const BATCH_RECORDS: usize = 100;
const BATCH_BYTES: usize = 256 * 1024;

/// A local durability, capacity, identity or encoding failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQLite cannot complete the operation; its full error is temporary backpressure.
    #[error("queue database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The queue file cannot be inspected or created.
    #[error("queue I/O: {0}")]
    Io(#[from] std::io::Error),
    /// A record cannot be encoded or decoded; it is never discarded.
    #[error("queue JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The configured budget or one record exceeds the supported bounds.
    #[error("invalid queue capacity or record size")]
    Capacity,
    /// This directory belongs to another node; pending records must not change owner.
    #[error("queue belongs to a different SMTP node")]
    Node,
}

/// A captured fact, or the source cursor after the facts that precede it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    /// Evidence pinned to its route at capture, with its stable replay key.
    Event {
        /// The stable replay identity assigned by its source.
        id: String,
        /// The provider webhook selected before the evidence was captured.
        route: String,
        /// The serialized evidence contract, without a message body.
        payload: String,
        /// Capture time, as Unix seconds.
        created: f64,
    },
    /// A log checkpoint. It is archived after every preceding queued event.
    Cursor {
        /// The immutable SMTP node identity that owns the log.
        node: String,
        /// The source file's inode, preserved across rename rotation.
        inode: String,
        /// The byte immediately after the last completely captured line.
        position: i64,
    },
}

/// A queue row's local sequence and decoded fact.
pub struct Pending {
    /// The immutable sequence, used only to acknowledge this exact row.
    pub sequence: i64,
    /// Its captured fact.
    pub record: Record,
}

/// One durable file, shared by the collector, notification intake and archiver.
#[derive(Clone)]
pub struct Queue {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
    pages: u64,
    node: String,
}

impl Queue {
    /// Opens the node's pending file. A smaller budget never destroys existing records.
    /// Refuses a changed node identity, an unusable budget, or an already oversized database.
    pub fn open(path: &Path, node: &str, budget: u64) -> Result<Self, Error> {
        if !(MIN_BUDGET..=64 * 1024 * 1024 * 1024).contains(&budget) {
            return Err(Error::Capacity);
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "page_size", 4096)?;
        let size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        if size != 4096 {
            return Err(Error::Capacity);
        }
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "wal_autocheckpoint", 1)?;
        conn.pragma_update(None, "cache_spill", false)?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS queue_meta (node TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS pending (sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT UNIQUE, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS cursor (node TEXT PRIMARY KEY, inode TEXT NOT NULL, position INTEGER NOT NULL);")?;
        let owner: Option<String> = conn
            .query_row("SELECT node FROM queue_meta LIMIT 1", [], |row| row.get(0))
            .optional()?;
        match owner {
            Some(owner) if owner != node => return Err(Error::Node),
            Some(_) => {}
            None => {
                conn.execute("INSERT INTO queue_meta (node) VALUES (?1)", [node])?;
            }
        }
        let pages = budget / (3 * 4096);
        let actual =
            u64::try_from(conn.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))?)
                .map_err(|_| Error::Capacity)?;
        if actual > pages {
            return Err(Error::Capacity);
        }
        conn.pragma_update(
            None,
            "max_page_count",
            i64::try_from(pages).map_err(|_| Error::Capacity)?,
        )?;
        checkpoint(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: path.to_owned(),
            pages,
            node: node.to_owned(),
        })
    }

    /// Atomically stores captured facts and advances their source cursor. A full queue leaves
    /// both unchanged. Duplicate event keys add nothing, so a replay is safe.
    pub fn append(&self, records: Vec<Record>) -> Result<(), Error> {
        if records.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = conn.transaction()?;
        for record in records {
            if let Record::Cursor { node, .. } = &record
                && node != &self.node
            {
                return Err(Error::Node);
            }
            let encoded = serde_json::to_string(&record)?;
            if encoded.len() > MAX_RECORD {
                return Err(Error::Capacity);
            }
            let id = match &record {
                Record::Event { id, .. } => Some(id.as_str()),
                Record::Cursor { .. } => None,
            };
            tx.execute(
                "INSERT INTO pending (id, payload) VALUES (?1, ?2) ON CONFLICT (id) DO NOTHING",
                params![id, encoded],
            )?;
            if let Record::Cursor {
                node,
                inode,
                position,
            } = record
            {
                tx.execute("INSERT INTO cursor (node, inode, position) VALUES (?1, ?2, ?3)
                    ON CONFLICT (node) DO UPDATE SET inode = excluded.inode, position = excluded.position", params![node, inode, position])?;
            }
        }
        tx.commit()?;
        checkpoint(&conn)
    }

    /// Reads the oldest bounded batch. Rows remain durable until the remote confirms them.
    pub fn pending(&self) -> Result<Vec<Pending>, Error> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut statement =
            conn.prepare("SELECT sequence, payload FROM pending ORDER BY sequence LIMIT ?1")?;
        let rows = statement.query_map([i64::try_from(BATCH_RECORDS).unwrap_or(100)], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut records = Vec::new();
        let mut bytes: usize = 0;
        for row in rows {
            let (sequence, payload) = row?;
            if bytes.saturating_add(payload.len()) > BATCH_BYTES && !records.is_empty() {
                break;
            }
            bytes = bytes.saturating_add(payload.len());
            records.push(Pending {
                sequence,
                record: serde_json::from_str(&payload)?,
            });
        }
        Ok(records)
    }

    /// Deletes only the exact sequences whose remote transaction was confirmed.
    pub fn acknowledge(&self, sequences: &[i64]) -> Result<(), Error> {
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = conn.transaction()?;
        for sequence in sequences {
            tx.execute("DELETE FROM pending WHERE sequence = ?1", [sequence])?;
        }
        tx.commit()?;
        checkpoint(&conn)
    }

    /// Reads this source's last local checkpoint, without downloading remote history.
    pub fn cursor(&self, node: &str) -> Result<Option<(String, i64)>, Error> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(conn
            .query_row(
                "SELECT inode, position FROM cursor WHERE node = ?1",
                [node],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// Whether fewer than ten percent of the permitted SQLite pages remain for capture.
    /// Admission closes here before the hard bound can stop a pending source transaction.
    pub fn full(&self) -> Result<bool, Error> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let used =
            u64::try_from(conn.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))?)
                .map_err(|_| Error::Capacity)?;
        let free =
            u64::try_from(conn.query_row("PRAGMA freelist_count", [], |row| row.get::<_, i64>(0))?)
                .map_err(|_| Error::Capacity)?;
        Ok(used.saturating_sub(free) >= self.pages.saturating_mul(9) / 10)
    }

    /// The current physical size, including WAL and shared memory, for diagnostics.
    pub fn bytes(&self) -> Result<u64, Error> {
        let _guard = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let main = std::fs::metadata(&self.path)?.len();
        let mut total = main;
        for suffix in ["-wal", "-shm"] {
            let mut file = self.path.as_os_str().to_owned();
            file.push(suffix);
            let bytes = match std::fs::metadata(PathBuf::from(file)) {
                Ok(meta) => meta.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => return Err(error.into()),
            };
            total = total.saturating_add(bytes);
        }
        Ok(total)
    }
}

fn checkpoint(conn: &Connection) -> Result<(), Error> {
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(Error::Capacity);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    /// Capacity failure rolls back the entire append and its cursor; confirmed rows free
    /// reusable pages, and both SQLite and WAL remain below the physical budget.
    #[test]
    fn bounded_queue_survives_full_and_reuses_confirmed_space() {
        let dir = TempDir::new();
        let queue = Queue::open(&dir.join("pending.sqlite"), "node", MIN_BUDGET).unwrap();
        let mut accepted = 0;
        loop {
            let result = queue.append(vec![
                Record::Event {
                    id: format!("event-{accepted}"),
                    route: "route".to_owned(),
                    payload: "x".repeat(32 * 1024),
                    created: 1.0,
                },
                Record::Cursor {
                    node: "node".to_owned(),
                    inode: "1".to_owned(),
                    position: accepted,
                },
            ]);
            if result.is_err() {
                break;
            }
            accepted += 1;
        }
        assert!(accepted > 0);
        assert_eq!(
            queue.cursor("node").unwrap(),
            Some(("1".to_owned(), accepted - 1))
        );
        assert!(queue.bytes().unwrap() < MIN_BUDGET);
        while !queue.pending().unwrap().is_empty() {
            queue
                .acknowledge(
                    &queue
                        .pending()
                        .unwrap()
                        .iter()
                        .map(|row| row.sequence)
                        .collect::<Vec<_>>(),
                )
                .unwrap();
        }
        queue
            .append(vec![Record::Event {
                id: "fresh".to_owned(),
                route: "route".to_owned(),
                payload: "new".to_owned(),
                created: 2.0,
            }])
            .unwrap();
        drop(queue);
        let reopened = Queue::open(&dir.join("pending.sqlite"), "node", MIN_BUDGET).unwrap();
        assert_eq!(reopened.pending().unwrap().len(), 1);
        assert!(matches!(
            Queue::open(&dir.join("pending.sqlite"), "other", MIN_BUDGET),
            Err(Error::Node)
        ));
    }
}
