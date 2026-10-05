//! A local spool: a SQLite file where a role writes what it must not lose before it answers a
//! request, and from which a drain moves the records into PostgreSQL later. The route that writes
//! to it never waits on the database, and nothing it acknowledged is lost when the database is
//! unavailable or the process stops.
//!
//! Two roles use it: the tracking role spools every open and click before answering, and the
//! api's provider-webhook ingress spools the receipts the database could not take at once. Each
//! names its records through [`Record`] (the file's name, how a record is written and read back);
//! everything else, durability, the one writer, corruption handling and the bound, is this
//! module's.
//!
//! # Durability
//!
//! The file is opened in WAL mode with `synchronous = FULL`: a committed write is on disk (its WAL
//! frame synced) before it is acknowledged, so it survives a crash or a power loss
//! (<https://www.sqlite.org/pragma.html#pragma_synchronous>, <https://www.sqlite.org/wal.html>).
//! An append is acknowledged to the caller only after its commit. Appends that arrive together
//! are committed together (group commit): one transaction and one sync per group, so the cost of
//! the sync is shared by every request waiting on it rather than paid by each. The directory is
//! created readable by its owner only: the records may hold recipients' hashed addresses, their
//! clients' `User-Agent`, or a provider's event bodies.
//!
//! # One writer
//!
//! One thread owns the connection; callers send it commands over a bounded channel and wait for
//! the answer (an actor), so the single-writer database is never shared behind a lock across an
//! `.await`, and a flood of requests meets backpressure (the channel's bound) instead of growing
//! memory. Appends, the drain's reads and its acknowledgements go through the same thread, in
//! order.
//!
//! # Records
//!
//! Each row is `(seq, record, digest)`: `seq` the append order, `record` the bytes
//! [`Record::encode`] wrote (versioned by the record type, so a later format can be read beside
//! an older one), `digest` the SHA-256 of `record`. A record whose digest does not match, or that
//! [`Record::decode`] refuses, is corrupt: [`Spool::take`] moves it to the `rejected` table, with
//! its reason, for an operator to read, and hands out the others, so one damaged row never blocks
//! or loses the rest.
//!
//! # Bound
//!
//! The spool holds at most [`Limits::max_bytes`] of live data (pages in use, so space freed by
//! the drain counts as free again); beyond it an append is refused and the caller answers without
//! recording, rather than filling the disk. Readiness probes report a full spool through
//! [`Spool::ready`].

use std::marker::PhantomData;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use rusqlite::{Connection, params};
use tokio::sync::{mpsc, oneshot};

use crate::crypto;

/// Appends committed in one transaction at most.
const GROUP_MAX: usize = 512;
/// Commands waiting for the writer at most; beyond it callers wait (backpressure).
const QUEUE: usize = 4_096;

/// The tables, created on every open (idempotent).
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY, record BLOB NOT NULL, digest BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS rejected (seq INTEGER PRIMARY KEY, record BLOB, digest BLOB, reason TEXT NOT NULL,
                                     rejected_at TEXT NOT NULL);
";

/// What a spool holds: how one record is named, written and read back.
pub trait Record: Sized + Send + 'static {
    /// The spool's name: its file is `<NAME>.sqlite` in its directory, and its writer thread and
    /// log lines carry the name.
    const NAME: &'static str;

    /// The record's bytes as they are written, carrying their own format version.
    ///
    /// # Errors
    ///
    /// The record cannot be serialized (a programming error).
    fn encode(&self) -> Result<Vec<u8>, String>;

    /// The record read back from its bytes.
    ///
    /// # Errors
    ///
    /// The bytes are not a record of a format this build reads; the reason is kept with the
    /// record when it is set aside.
    fn decode(bytes: &[u8]) -> Result<Self, String>;
}

/// Why the spool could not do what was asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpoolError {
    /// The spool holds its bound of live data: the record was not written.
    #[error("the spool is full ({0} bytes in use)")]
    Full(u64),
    /// The writer thread has stopped (the spool was closed, or the thread failed).
    #[error("the spool's writer has stopped")]
    Stopped,
    /// SQLite refused (the disk, the file), or a record could not be encoded.
    #[error("the spool failed: {0}")]
    Storage(String),
}

impl From<rusqlite::Error> for SpoolError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

/// How large the spool may grow.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The live data the spool holds at most, in bytes.
    pub max_bytes: u64,
}

impl Default for Limits {
    /// 2 GiB: a day of the floor's opens and clicks, or of provider callbacks during a database
    /// outage, with room to spare, far below the public host's disk.
    fn default() -> Self {
        Self { max_bytes: 2 << 30 }
    }
}

/// A batch handed to the drain: the oldest records, in append order.
#[derive(Debug)]
pub struct Batch<R> {
    /// The records and their sequence numbers, for [`Spool::ack`].
    pub records: Vec<(i64, R)>,
    /// Corrupt records moved to `rejected` while reading this batch.
    pub rejected: usize,
}

impl<R> Default for Batch<R> {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            rejected: 0,
        }
    }
}

enum Command<R> {
    Append {
        records: Vec<Vec<u8>>,
        reply: oneshot::Sender<Result<(), SpoolError>>,
    },
    Take {
        limit: usize,
        reply: oneshot::Sender<Result<Batch<R>, SpoolError>>,
    },
    Ack {
        seqs: Vec<i64>,
        reply: oneshot::Sender<Result<(), SpoolError>>,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
}

/// What the writer publishes for callers that must not wait on it (the readiness probe).
#[derive(Debug, Default)]
struct Shared {
    stopped: AtomicBool,
    full: AtomicBool,
    bytes: AtomicU64,
}

/// A handle to a spool of `R`; cheap to clone.
pub struct Spool<R> {
    commands: mpsc::Sender<Command<R>>,
    shared: Arc<Shared>,
}

impl<R> Clone for Spool<R> {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<R> std::fmt::Debug for Spool<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Spool")
            .field("shared", &self.shared)
            .finish_non_exhaustive()
    }
}

impl<R: Record> Spool<R> {
    /// Opens (creating when absent) the spool in `dir` and starts its writer thread.
    ///
    /// # Errors
    ///
    /// The directory or the file cannot be created or opened, or the thread cannot start.
    pub fn open(dir: &Path, limits: Limits) -> Result<Self, SpoolError> {
        create_private_dir(dir).map_err(|error| {
            SpoolError::Storage(format!("cannot create {}: {error}", dir.display()))
        })?;
        let connection = open_file(&dir.join(format!("{}.sqlite", R::NAME)))?;
        let (commands, receiver) = mpsc::channel(QUEUE);
        let shared = Arc::new(Shared::default());
        let writer = Writer {
            connection,
            limits,
            shared: Arc::clone(&shared),
            record: PhantomData::<fn() -> R>,
        };
        std::thread::Builder::new()
            .name(format!("{}-spool", R::NAME))
            .spawn(move || writer.run(receiver))
            .map_err(|error| SpoolError::Storage(format!("cannot start the writer: {error}")))?;
        Ok(Self { commands, shared })
    }

    /// Writes `record` durably; returns once it is committed.
    ///
    /// # Errors
    ///
    /// [`SpoolError::Full`] when the spool holds its bound, [`SpoolError::Stopped`] after
    /// [`Spool::close`], or [`SpoolError::Storage`].
    pub async fn append(&self, record: &R) -> Result<(), SpoolError> {
        self.append_all(std::slice::from_ref(record)).await
    }

    /// Writes `records` durably in one transaction: all of them or none.
    ///
    /// # Errors
    ///
    /// As [`Spool::append`].
    pub async fn append_all(&self, records: &[R]) -> Result<(), SpoolError> {
        let records = records
            .iter()
            .map(|record| record.encode().map_err(SpoolError::Storage))
            .collect::<Result<Vec<_>, _>>()?;
        self.ask(|reply| Command::Append { records, reply }).await?
    }

    /// The oldest records, at most `limit`, without removing them; corrupt records met on the
    /// way are moved to `rejected` and counted.
    ///
    /// # Errors
    ///
    /// [`SpoolError::Stopped`] or [`SpoolError::Storage`].
    pub async fn take(&self, limit: usize) -> Result<Batch<R>, SpoolError> {
        self.ask(|reply| Command::Take { limit, reply }).await?
    }

    /// Removes the records of `seqs`: the drain stored them.
    ///
    /// # Errors
    ///
    /// [`SpoolError::Stopped`] or [`SpoolError::Storage`].
    pub async fn ack(&self, seqs: Vec<i64>) -> Result<(), SpoolError> {
        self.ask(|reply| Command::Ack { seqs, reply }).await?
    }

    /// True while appends are accepted: the writer runs and the spool is under its bound.
    #[must_use]
    pub fn ready(&self) -> bool {
        !self.shared.stopped.load(Ordering::Relaxed) && !self.shared.full.load(Ordering::Relaxed)
    }

    /// The live bytes the writer last measured.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.shared.bytes.load(Ordering::Relaxed)
    }

    /// Stops the writer after the commands already sent: the WAL is checkpointed into the
    /// database file and the file closed. Later calls answer [`SpoolError::Stopped`].
    pub async fn close(&self) {
        let (reply, done) = oneshot::channel();
        if self.commands.send(Command::Close { reply }).await.is_ok() {
            let _ = done.await;
        }
    }

    /// Sends a command and waits for its answer.
    async fn ask<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> Command<R>,
    ) -> Result<T, SpoolError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .await
            .map_err(|_| SpoolError::Stopped)?;
        answer.await.map_err(|_| SpoolError::Stopped)
    }
}

/// Creates `dir` and its missing parents readable by the owner only (0700); an existing directory
/// is left as the deployment made it.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Opens the file with the settings of the module and its tables.
fn open_file(path: &Path) -> Result<Connection, SpoolError> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.execute_batch(SCHEMA)?;
    Ok(connection)
}

/// The thread that owns the connection.
struct Writer<R> {
    connection: Connection,
    limits: Limits,
    shared: Arc<Shared>,
    record: PhantomData<fn() -> R>,
}

impl<R: Record> Writer<R> {
    /// Serves commands in order until the spool is closed or every handle is dropped.
    fn run(mut self, mut commands: mpsc::Receiver<Command<R>>) {
        if let Ok(bytes) = self.live_bytes() {
            self.publish(bytes);
        }
        let mut next = None;
        loop {
            let Some(command) = next.take().or_else(|| commands.blocking_recv()) else {
                break;
            };
            match command {
                Command::Append { records, reply } => {
                    let mut group = vec![(records, reply)];
                    let mut count = group
                        .iter()
                        .map(|(records, _)| records.len())
                        .sum::<usize>();
                    while count < GROUP_MAX {
                        match commands.try_recv() {
                            Ok(Command::Append { records, reply }) => {
                                count = count.saturating_add(records.len());
                                group.push((records, reply));
                            }
                            Ok(other) => {
                                next = Some(other);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    let records: Vec<&[u8]> = group
                        .iter()
                        .flat_map(|(records, _)| records.iter().map(Vec::as_slice))
                        .collect();
                    let result = self.append(&records);
                    if let Err(error) = &result
                        && !matches!(error, SpoolError::Full(_))
                    {
                        tracing::error!(spool = R::NAME, error = %error, "the spool could not write");
                    }
                    for (_, reply) in group {
                        let _ = reply.send(result.clone());
                    }
                }
                Command::Take { limit, reply } => {
                    let _ = reply.send(self.take(limit));
                }
                Command::Ack { seqs, reply } => {
                    let _ = reply.send(self.ack(&seqs));
                }
                Command::Close { reply } => {
                    if let Err(error) =
                        self.connection
                            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                    {
                        tracing::warn!(spool = R::NAME, error = %error, "the spool was not checkpointed");
                    }
                    self.shared.stopped.store(true, Ordering::Relaxed);
                    let _ = reply.send(());
                    return;
                }
            }
        }
        self.shared.stopped.store(true, Ordering::Relaxed);
    }

    /// Records the measured size and whether it reached the bound.
    fn publish(&self, bytes: u64) {
        self.shared.bytes.store(bytes, Ordering::Relaxed);
        self.shared
            .full
            .store(bytes >= self.limits.max_bytes, Ordering::Relaxed);
    }

    /// The bytes of the pages in use: the file's pages less the free ones.
    fn live_bytes(&self) -> Result<u64, SpoolError> {
        let pragma = |name: &str| -> Result<u64, SpoolError> {
            let value: i64 = self
                .connection
                .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))?;
            Ok(u64::try_from(value).unwrap_or(0))
        };
        let pages = pragma("page_count")?.saturating_sub(pragma("freelist_count")?);
        Ok(pages.saturating_mul(pragma("page_size")?))
    }

    /// Commits `records` in one transaction, unless the spool holds its bound.
    fn append(&mut self, records: &[&[u8]]) -> Result<(), SpoolError> {
        let before = self.live_bytes()?;
        self.publish(before);
        if before >= self.limits.max_bytes {
            return Err(SpoolError::Full(before));
        }
        let transaction = self.connection.transaction()?;
        {
            let mut insert = transaction
                .prepare_cached("INSERT INTO events (record, digest) VALUES (?1, ?2)")?;
            for record in records {
                insert.execute(params![record, crypto::sha256(record)])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// The oldest `limit` records; corrupt ones are moved to `rejected` in the same transaction.
    fn take(&mut self, limit: usize) -> Result<Batch<R>, SpoolError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let transaction = self.connection.transaction()?;
        let rows: Vec<(i64, Vec<u8>, Vec<u8>)> = {
            let mut select = transaction
                .prepare_cached("SELECT seq, record, digest FROM events ORDER BY seq LIMIT ?1")?;
            select
                .query_map(params![limit], |row| {
                    Ok((row.get(0)?, read_blob(row, 1), read_blob(row, 2)))
                })?
                .collect::<Result<_, _>>()?
        };
        let mut batch = Batch::default();
        for (seq, record, digest) in rows {
            match decode::<R>(&record, &digest) {
                Ok(decoded) => batch.records.push((seq, decoded)),
                Err(reason) => {
                    transaction.execute(
                        "INSERT OR REPLACE INTO rejected (seq, record, digest, reason, rejected_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            seq,
                            record,
                            digest,
                            reason,
                            crate::process::now().to_string()
                        ],
                    )?;
                    transaction.execute("DELETE FROM events WHERE seq = ?1", params![seq])?;
                    tracing::error!(spool = R::NAME, seq, reason = %reason, "a corrupt spooled record was set aside");
                    batch.rejected += 1;
                }
            }
        }
        transaction.commit()?;
        Ok(batch)
    }

    /// Removes the drained records.
    fn ack(&mut self, seqs: &[i64]) -> Result<(), SpoolError> {
        let transaction = self.connection.transaction()?;
        {
            let mut delete = transaction.prepare_cached("DELETE FROM events WHERE seq = ?1")?;
            for seq in seqs {
                delete.execute(params![seq])?;
            }
        }
        transaction.commit()?;
        let bytes = self.live_bytes()?;
        self.publish(bytes);
        Ok(())
    }
}

/// A column as bytes, whatever SQLite stored there; a value of another type reads as empty, which
/// the digest then refuses.
fn read_blob(row: &rusqlite::Row<'_>, index: usize) -> Vec<u8> {
    row.get::<_, Vec<u8>>(index).unwrap_or_default()
}

/// Reads one record back, or says why it is corrupt.
fn decode<R: Record>(record: &[u8], digest: &[u8]) -> Result<R, String> {
    if crypto::sha256(record) != digest {
        return Err("the digest does not match the record".to_owned());
    }
    R::decode(record).map_err(|reason| format!("the record does not decode: {reason}"))
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    /// A record of the tests: one line of text, written as it is.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Note(String);

    impl Record for Note {
        const NAME: &'static str = "notes";

        fn encode(&self) -> Result<Vec<u8>, String> {
            Ok(format!("v1:{}", self.0).into_bytes())
        }

        fn decode(bytes: &[u8]) -> Result<Self, String> {
            std::str::from_utf8(bytes)
                .ok()
                .and_then(|text| text.strip_prefix("v1:"))
                .map(|text| Self(text.to_owned()))
                .ok_or_else(|| "not a version 1 note".to_owned())
        }
    }

    /// A directory of its own under the system's temporary directory, removed when dropped.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("norbelys-spool-{}", Uuid::now_v7().simple())))
        }

        fn file(&self) -> std::path::PathBuf {
            self.0.join("notes.sqlite")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn note(n: usize) -> Note {
        Note(format!("test note {n}"))
    }

    /// What `take` hands out, without the sequence numbers.
    fn notes(batch: &Batch<Note>) -> Vec<Note> {
        batch.records.iter().map(|(_, note)| note.clone()).collect()
    }

    /// Records appended are on disk: after the process stops between the append and the drain
    /// (the spool dropped without being closed, as a crash leaves it) a new spool on the same
    /// directory hands out every one of them, in order, exactly as written.
    #[tokio::test]
    async fn appended_records_survive_a_stop_before_the_drain() {
        let dir = Scratch::new();
        let written: Vec<Note> = (0..5).map(note).collect();
        let spool = Spool::open(&dir.0, Limits::default()).unwrap();
        spool.append_all(&written[..2]).await.unwrap();
        for note in &written[2..] {
            spool.append(note).await.unwrap();
        }
        drop(spool);
        // The writer thread ends when the last handle is dropped; give it a moment to let go of
        // the file before it is opened again.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let reopened = Spool::<Note>::open(&dir.0, Limits::default()).unwrap();
        let batch = reopened.take(100).await.unwrap();
        assert_eq!(notes(&batch), written);
        assert_eq!(batch.rejected, 0);
    }

    /// Acknowledged records are gone, the others stay, and `take` hands out the oldest first up
    /// to its limit: the drain moves the spool forward batch by batch.
    #[tokio::test]
    async fn take_and_ack_move_the_spool_forward() {
        let dir = Scratch::new();
        let spool = Spool::open(&dir.0, Limits::default()).unwrap();
        let written: Vec<Note> = (0..5).map(note).collect();
        spool.append_all(&written).await.unwrap();
        let first = spool.take(3).await.unwrap();
        assert_eq!(notes(&first), written[..3]);
        spool
            .ack(first.records.iter().map(|(seq, _)| *seq).collect())
            .await
            .unwrap();
        let rest = spool.take(10).await.unwrap();
        assert_eq!(notes(&rest), written[3..]);
        spool.close().await;
        assert_eq!(spool.append(&written[0]).await, Err(SpoolError::Stopped));
    }

    /// A record whose bytes were damaged (its digest no longer matches) and one its type refuses
    /// are set aside in `rejected` with their reasons, while every other record is handed out:
    /// one corrupt row neither blocks the drain nor loses its neighbours.
    #[tokio::test]
    async fn a_corrupt_record_is_set_aside_without_losing_the_others() {
        let dir = Scratch::new();
        let written: Vec<Note> = (0..4).map(note).collect();
        let spool = Spool::open(&dir.0, Limits::default()).unwrap();
        spool.append_all(&written).await.unwrap();
        spool.close().await;
        let file = Connection::open(dir.file()).unwrap();
        file.execute(
            "UPDATE events SET record = CAST(replace(CAST(record AS TEXT), 'test', 'tost') AS BLOB) WHERE seq = 2",
            [],
        )
        .unwrap();
        let junk = b"not a note".to_vec();
        file.execute(
            "UPDATE events SET record = ?1, digest = ?2 WHERE seq = 3",
            params![junk, crypto::sha256(&junk)],
        )
        .unwrap();
        drop(file);
        let spool = Spool::<Note>::open(&dir.0, Limits::default()).unwrap();
        let batch = spool.take(10).await.unwrap();
        assert_eq!(notes(&batch), [written[0].clone(), written[3].clone()]);
        assert_eq!(batch.rejected, 2);
        spool.close().await;
        let file = Connection::open(dir.file()).unwrap();
        let reasons: Vec<(i64, String)> = file
            .prepare("SELECT seq, reason FROM rejected ORDER BY seq")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(reasons.len(), 2);
        assert!(reasons[0].1.contains("digest"), "{reasons:?}");
        assert!(reasons[1].1.contains("does not decode"), "{reasons:?}");
        let left: i64 = file
            .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 2);
    }

    /// Appends that arrive together all commit (in groups) and come back in order; at its bound
    /// the spool refuses an append, says so in its readiness, and accepts again once the drain has
    /// freed room.
    #[tokio::test]
    async fn concurrent_appends_commit_and_the_bound_refuses() {
        let dir = Scratch::new();
        let spool = Spool::open(&dir.0, Limits::default()).unwrap();
        let written: Vec<Note> = (0..200).map(note).collect();
        let appends = written.iter().map(|note| spool.append(note));
        for result in futures_util::future::join_all(appends).await {
            result.unwrap();
        }
        let batch = spool.take(500).await.unwrap();
        assert_eq!(batch.records.len(), 200);
        spool.close().await;

        let tight = Spool::<Note>::open(
            &dir.0,
            Limits {
                max_bytes: spool_bytes(&dir.file()),
            },
        )
        .unwrap();
        assert!(matches!(
            tight.append(&note(1)).await,
            Err(SpoolError::Full(_))
        ));
        assert!(!tight.ready());
        let all = tight.take(500).await.unwrap();
        tight
            .ack(all.records.iter().map(|(seq, _)| *seq).collect())
            .await
            .unwrap();
        assert!(tight.ready());
        tight.append(&note(2)).await.unwrap();
    }

    /// The live bytes of the spool file at `path`, measured as the writer measures them.
    fn spool_bytes(path: &Path) -> u64 {
        let file = Connection::open(path).unwrap();
        let pragma = |name: &str| -> u64 {
            let value: i64 = file
                .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
                .unwrap();
            u64::try_from(value).unwrap()
        };
        (pragma("page_count") - pragma("freelist_count")) * pragma("page_size")
    }
}
