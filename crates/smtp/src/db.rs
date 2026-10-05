//! The managed MTA's canonical state in Turso, through SQL over HTTP.
//!
//! Every node owns a namespace of tables in the same remote database. The node's stable
//! digest qualifies schema identifiers; values always remain bound parameters. Separate
//! SMTP hosts cannot overwrite each other's accounts, keys, checkpoints or evidence.
//! Connection creation and all SQL run on blocking threads. No historical database file is
//! downloaded. Each operation owns its transaction; dropping it rolls back. Only captured
//! evidence and its source checkpoint enter the bounded local queue, after the canonical
//! transaction commits. A failed local commit leaves the log checkpoint unchanged for replay.

mod remote;

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use jiff::Timestamp;
use rusqlite::types::{FromSql, ToSql, ToSqlOutput, Value, ValueRef};
use tokio::task::JoinError;

/// Binds heterogeneous values without interpolating SQL.
pub use rusqlite::params;

use crate::config::DatabaseArgs;
use crate::queue::{self, Queue, Record};
use remote::Remote;

/// A transport, SQL, encoding or local durability failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The configured database could not be reached; its commit may be uncertain.
    #[error("database transport: {0}")]
    Http(#[from] reqwest::Error),
    /// The HTTP endpoint refused the operation.
    #[error("database HTTP status {0}")]
    Status(u16),
    /// SQL refused an operation; its diagnostic text is deliberately not logged.
    #[error("database SQL error {0}")]
    Sql(String),
    /// The endpoint returned an invalid or oversized response.
    #[error("database protocol: {0}")]
    Protocol(&'static str),
    /// A row expected by an operation was absent.
    #[error("query returned no row")]
    QueryReturnedNoRows,
    /// A bound or returned value could not be encoded.
    #[error("database value conversion failed")]
    ToSqlConversionFailure(Box<dyn std::error::Error + Send + Sync>),
    /// SQLite refused a value or a test fixture query.
    #[error("database value: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// JSON could not be encoded or decoded.
    #[error("database JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// An HTTP response could not be read.
    #[error("database I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Captured evidence could not be durably queued; the source must retry.
    #[error("local evidence queue: {0}")]
    Queue(#[from] queue::Error),
}

/// The result of a canonical database operation.
pub type Result<T> = std::result::Result<T, Error>;

/// Bound SQL parameters, converted to owned values before sending.
pub trait Params {
    /// Owns the values without interpolating them into SQL.
    fn values(self) -> Result<Vec<Value>>;
}

fn value(input: &dyn ToSql) -> Result<Value> {
    match input.to_sql()? {
        ToSqlOutput::Borrowed(value) => Ok(match value {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(v) => Value::Integer(v),
            ValueRef::Real(v) => Value::Real(v),
            ValueRef::Text(v) => Value::Text(
                std::str::from_utf8(v)
                    .map_err(|_| Error::Protocol("invalid SQL text"))?
                    .to_owned(),
            ),
            ValueRef::Blob(v) => Value::Blob(v.to_owned()),
        }),
        ToSqlOutput::Owned(value) => Ok(value),
        _ => Err(Error::Protocol("unsupported bound SQL value")),
    }
}

impl Params for &[&dyn ToSql] {
    fn values(self) -> Result<Vec<Value>> {
        self.iter().map(|item| value(*item)).collect()
    }
}
impl<const N: usize> Params for &[&dyn ToSql; N] {
    fn values(self) -> Result<Vec<Value>> {
        self.iter().map(|item| value(*item)).collect()
    }
}
impl Params for [(); 0] {
    fn values(self) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
}
impl Params for Vec<Value> {
    fn values(self) -> Result<Vec<Value>> {
        Ok(self)
    }
}
macro_rules! arrays {
    ($($n:literal),*) => { $(impl<T: ToSql> Params for [T; $n] {
        fn values(self) -> Result<Vec<Value>> { self.iter().map(|item| value(item)).collect() }
    })* };
}
arrays!(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16);

/// A decoded row, with the same typed SQL encodings as the local queue.
pub struct Row {
    values: Vec<Value>,
}

impl Row {
    /// Reads one column with checked indexing and a checked type conversion.
    pub fn get<I: TryInto<usize>, T: FromSql>(&self, column: I) -> Result<T> {
        let index = column
            .try_into()
            .map_err(|_| Error::Protocol("invalid column index"))?;
        let value = self
            .values
            .get(index)
            .ok_or(Error::Protocol("missing SQL column"))?;
        T::column_result(ValueRef::from(value))
            .map_err(|error| Error::ToSqlConversionFailure(Box::new(error)))
    }
}

/// Turns only an absent row into None; every other failure remains an error.
pub trait OptionalExtension<T> {
    /// Distinguishes absence from a transport or SQL failure.
    fn optional(self) -> Result<Option<T>>;
}
impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// The complete canonical schema. It is created idempotently, without importing old state.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS domains (name TEXT PRIMARY KEY, ownership_token TEXT NOT NULL, verified_at TEXT,
                                    dkim_selector TEXT NOT NULL, dkim_public TEXT, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS accounts (username TEXT PRIMARY KEY, domain TEXT NOT NULL REFERENCES domains(name),
                                     kind TEXT NOT NULL CHECK (kind IN ('mailbox','service','relay')),
                                     rate_class TEXT NOT NULL CHECK (rate_class IN ('customer','relay')),
                                     grant_domain TEXT, catch_all INTEGER NOT NULL DEFAULT 0,
                                     created_at TEXT NOT NULL, disabled_at TEXT);
CREATE TABLE IF NOT EXISTS routes (provider_webhook_id TEXT PRIMARY KEY, url TEXT NOT NULL, secret BLOB NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS account_routes (username TEXT PRIMARY KEY REFERENCES accounts(username),
                                           provider_webhook_id TEXT NOT NULL REFERENCES routes(provider_webhook_id));
CREATE TABLE IF NOT EXISTS submissions (queue_id TEXT PRIMARY KEY, username TEXT NOT NULL, internet_message_id TEXT,
                                        authenticated_at REAL NOT NULL);
CREATE TABLE IF NOT EXISTS returns (token TEXT PRIMARY KEY, username TEXT NOT NULL, internet_message_id TEXT, queue_id TEXT NOT NULL,
                                    created REAL NOT NULL);
CREATE TABLE IF NOT EXISTS events (id TEXT PRIMARY KEY, provider_webhook_id TEXT, payload TEXT NOT NULL, created REAL NOT NULL,
                                   done INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0,
                                   next_at REAL NOT NULL DEFAULT 0, last_status INTEGER);
CREATE INDEX IF NOT EXISTS events_due ON events (done, next_at);
CREATE INDEX IF NOT EXISTS events_route_due ON events (provider_webhook_id, done, next_at);
CREATE TABLE IF NOT EXISTS pending_changes (id INTEGER PRIMARY KEY, kind TEXT NOT NULL, payload TEXT NOT NULL, created REAL NOT NULL, applied REAL);
CREATE TABLE IF NOT EXISTS cursors (node TEXT PRIMARY KEY, inode TEXT NOT NULL, position INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS events_created ON events (created);
CREATE INDEX IF NOT EXISTS submissions_message ON submissions (internet_message_id);
";

enum Backend {
    Remote(Remote),
    #[cfg(test)]
    Local(RefCell<rusqlite::Connection>),
}

/// One SQL stream. A caller never shares it across concurrent transactions.
pub struct Connection {
    backend: Backend,
    queue: Option<Queue>,
}

impl Connection {
    /// Opens a fresh remote stream and creates this node's missing schema.
    pub fn remote(args: &DatabaseArgs, node: &str, queue: Option<Queue>) -> Result<Self> {
        if !crate::config::is_label(node) {
            return Err(Error::Protocol("node must be a lowercase DNS label"));
        }
        let conn = Self {
            backend: Backend::Remote(Remote::new(&args.database_url, &args.database_token, node)?),
            queue,
        };
        conn.execute_batch(SCHEMA)?;
        Ok(conn)
    }

    /// Executes SQL with bound values, returning the number of changed rows.
    pub fn execute<P: Params>(&self, sql: &str, params: P) -> Result<usize> {
        let (_, changed) = self.run(sql, params.values()?)?;
        usize::try_from(changed).map_err(|_| Error::Protocol("SQL change count is too large"))
    }

    /// Runs a trusted schema or fixture sequence. Its caller owns atomicity.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        match &self.backend {
            Backend::Remote(remote) => remote.sequence(sql),
            #[cfg(test)]
            Backend::Local(conn) => conn.borrow().execute_batch(sql).map_err(Error::from),
        }
    }

    /// Executes a bounded batch in one HTTP request, inside the current transaction.
    pub fn execute_many(&self, sql: &str, rows: Vec<Vec<Value>>) -> Result<()> {
        match &self.backend {
            Backend::Remote(remote) => remote.many(sql, rows),
            #[cfg(test)]
            Backend::Local(_) => {
                for row in rows {
                    self.execute(sql, row)?;
                }
                Ok(())
            }
        }
    }

    fn run(&self, sql: &str, values: Vec<Value>) -> Result<(Vec<Row>, u64)> {
        match &self.backend {
            Backend::Remote(remote) => {
                let result = remote.execute(sql, values)?;
                let rows = result
                    .rows
                    .into_iter()
                    .map(|values| {
                        let values = values
                            .into_iter()
                            .map(Value::try_from)
                            .collect::<Result<_>>()?;
                        Ok(Row { values })
                    })
                    .collect::<Result<_>>()?;
                Ok((rows, result.affected_row_count))
            }
            #[cfg(test)]
            Backend::Local(conn) => {
                let conn = conn.borrow();
                let mut statement = conn.prepare(sql)?;
                let columns = statement.column_count();
                let mut rows = Vec::new();
                let mut query = statement.query(rusqlite::params_from_iter(values))?;
                while let Some(row) = query.next()? {
                    let values = (0..columns)
                        .map(|index| row.get(index))
                        .collect::<rusqlite::Result<_>>()?;
                    rows.push(Row { values });
                }
                drop(query);
                Ok((rows, conn.changes()))
            }
        }
    }

    /// Decodes the first row, refusing an absent one.
    pub fn query_row<P: Params, T, F: FnOnce(&Row) -> Result<T>>(
        &self,
        sql: &str,
        params: P,
        decode: F,
    ) -> Result<T> {
        let (rows, _) = self.run(sql, params.values()?)?;
        decode(rows.first().ok_or(Error::QueryReturnedNoRows)?)
    }

    /// Keeps the statement's SQL beside its owning operation; values are bound on execution.
    pub fn prepare<'a>(&'a self, sql: &str) -> Result<Statement<'a>> {
        Ok(Statement {
            conn: self,
            sql: sql.to_owned(),
        })
    }

    /// Uses the transport's existing pool; the remote server owns its statement cache.
    pub fn prepare_cached<'a>(&'a self, sql: &str) -> Result<Statement<'a>> {
        self.prepare(sql)
    }

    /// Starts a write transaction. Its lock is remote and serializes competing writers.
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        self.execute_batch("BEGIN IMMEDIATE")?;
        Ok(Transaction {
            conn: self,
            active: Cell::new(true),
            captured: RefCell::new(Vec::new()),
        })
    }

    /// Reads the latest locally committed checkpoint, falling back to remote recovery state.
    pub fn cursor(&self, node: &str) -> Result<Option<(String, i64)>> {
        if let Some(queue) = &self.queue
            && let Some(cursor) = queue.cursor(node)?
        {
            return Ok(Some(cursor));
        }
        self.query_row(
            "SELECT inode, position FROM cursors WHERE node = ?1",
            [node],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
    }
}

/// One reusable statement. Its parameters and result are owned for each execution.
pub struct Statement<'a> {
    conn: &'a Connection,
    sql: String,
}
impl Statement<'_> {
    /// Executes the statement with bound values.
    pub fn execute<P: Params>(&mut self, params: P) -> Result<usize> {
        self.conn.execute(&self.sql, params)
    }
    /// Decodes one row or reports its absence.
    pub fn query_row<P: Params, T, F: FnOnce(&Row) -> Result<T>>(
        &self,
        params: P,
        decode: F,
    ) -> Result<T> {
        self.conn.query_row(&self.sql, params, decode)
    }
    /// Decodes a bounded result, preserving each row's conversion error.
    pub fn query_map<P: Params, T, F: FnMut(&Row) -> Result<T>>(
        &self,
        params: P,
        mut decode: F,
    ) -> Result<std::vec::IntoIter<Result<T>>> {
        let (rows, _) = self.conn.run(&self.sql, params.values()?)?;
        Ok(rows.iter().map(&mut decode).collect::<Vec<_>>().into_iter())
    }
}

/// A remote transaction with captured local facts staged until its commit succeeds.
pub struct Transaction<'a> {
    conn: &'a Connection,
    active: Cell<bool>,
    captured: RefCell<Vec<Record>>,
}
impl std::ops::Deref for Transaction<'_> {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        self.conn
    }
}
impl Transaction<'_> {
    /// Stages captured evidence or a source checkpoint for durable local publication.
    pub fn capture(&self, record: Record) -> Result<()> {
        #[cfg(test)]
        if matches!(self.conn.backend, Backend::Local(_)) && self.conn.queue.is_none() {
            match record {
                Record::Event {
                    id,
                    route,
                    payload,
                    created,
                } => {
                    self.execute("INSERT INTO events (id, provider_webhook_id, payload, created) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (id) DO NOTHING", params![id, route, payload, created])?;
                }
                Record::Cursor {
                    node,
                    inode,
                    position,
                } => {
                    self.execute("INSERT INTO cursors (node, inode, position) VALUES (?1, ?2, ?3) ON CONFLICT (node) DO UPDATE SET inode = excluded.inode, position = excluded.position", params![node, inode, position])?;
                }
            }
            return Ok(());
        }
        if self.conn.queue.is_none() {
            return Err(Error::Protocol("collector has no local queue"));
        }
        self.captured.borrow_mut().push(record);
        Ok(())
    }
    /// Publishes canonical changes first, then the local capture and checkpoint atomically.
    /// A queue failure keeps the source unadvanced, so replay can finish the operation.
    pub fn commit(self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        self.active.set(false);
        if let Some(queue) = &self.conn.queue {
            queue.append(self.captured.take())?;
        }
        Ok(())
    }
}
impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if self.active.get() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

/// A remote database factory. Each call gets an independent SQL stream on a blocking thread.
#[derive(Clone)]
pub struct Db {
    source: Arc<Mutex<Connection>>,
}
impl Db {
    /// Builds and initializes a remote database outside Tokio's async threads.
    pub fn remote(args: &DatabaseArgs, node: &str, queue: Option<Queue>) -> Result<Self> {
        Ok(Self {
            source: Arc::new(Mutex::new(Connection::remote(args, node, queue)?)),
        })
    }
    /// Runs an operation on the blocking pool, preserving its own typed error.
    pub async fn call<T, E, F>(&self, f: F) -> std::result::Result<T, E>
    where
        F: FnOnce(&mut Connection) -> std::result::Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: From<JoinError> + Send + 'static,
    {
        let source = Arc::clone(&self.source);
        tokio::task::spawn_blocking(move || {
            let mut guard = source.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut guard)
        })
        .await?
    }
    /// An independent stream for a collector, sharing HTTP connections and its node namespace.
    pub fn connection(&self, queue: Option<Queue>) -> Result<Connection> {
        let guard = self.source.lock().unwrap_or_else(PoisonError::into_inner);
        match &guard.backend {
            Backend::Remote(remote) => Ok(Connection {
                backend: Backend::Remote(remote.fresh()),
                queue,
            }),
            #[cfg(test)]
            Backend::Local(_) => Err(Error::Protocol("use a separate test fixture connection")),
        }
    }
    /// An independent database stream on the same HTTP pool, optionally capturing evidence.
    pub fn fork(&self, queue: Option<Queue>) -> Result<Self> {
        Ok(Self {
            source: Arc::new(Mutex::new(self.connection(queue)?)),
        })
    }
    /// Opens a real SQLite fixture, compiled only into the test binary.
    #[cfg(test)]
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            source: Arc::new(Mutex::new(open(path)?)),
        })
    }
}

/// Opens a canonical-state SQLite adapter used only by the shared test harness.
#[cfg(test)]
pub fn open(path: &Path) -> Result<Connection> {
    let local = rusqlite::Connection::open(path)?;
    local.busy_timeout(std::time::Duration::from_secs(5))?;
    local.pragma_update(None, "journal_mode", "WAL")?;
    local.pragma_update(None, "foreign_keys", "ON")?;
    local.execute_batch(SCHEMA)?;
    Ok(Connection {
        backend: Backend::Local(RefCell::new(local)),
        queue: None,
    })
}
/// Now, as the `REAL` Unix seconds of the schema.
#[must_use]
pub fn now() -> f64 {
    seconds(Timestamp::now())
}

/// `at` as the `REAL` Unix seconds of the schema.
#[must_use]
pub fn seconds(at: Timestamp) -> f64 {
    at.as_duration().as_secs_f64()
}

/// Now, as the RFC 3339 text of the schema.
#[must_use]
pub fn now_text() -> String {
    Timestamp::now().to_string()
}

/// Creates `dir` and its missing parents readable by the owner only (0700): the database holds
/// sealed route secrets, pending credentials and recipient addresses. An existing directory is
/// left as the deployment made it.
///
/// # Errors
///
/// The directory cannot be created.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::SqlServer;

    /// Bound values retain 64-bit integers, nulls, UTF-8 and binary bytes over the wire;
    /// an abandoned transaction rolls back and SQL diagnostics do not expose bound secrets.
    #[test]
    fn remote_values_and_transactions_preserve_the_sql_contract() {
        let server = SqlServer::new();
        let mut conn = Connection::remote(&server.args, "mail-a", None).unwrap();
        let bytes = vec![0u8, 255, 1];
        let row = conn
            .query_row(
                "SELECT ?1, ?2, ?3, ?4, ?5",
                params![i64::MAX, Option::<String>::None, "it's café", bytes, 1.25],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, f64>(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row, (i64::MAX, None, "it's café".to_owned(), bytes, 1.25));
        {
            let tx = conn.transaction().unwrap();
            tx.execute(
                "INSERT INTO events (id,payload,created) VALUES ('discard','{}',1)",
                [],
            )
            .unwrap();
        }
        assert_eq!(
            conn.query_row("SELECT count(*) FROM events", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let error = conn
            .execute(
                "INSERT INTO events (id,payload,created) VALUES (NULL,?1,NULL)",
                ["sensitive-event"],
            )
            .unwrap_err();
        assert!(!error.to_string().contains("sensitive-event"));
        assert!(Connection::remote(&server.args, "invalid.node", None).is_err());
    }

    /// Captured evidence is locally durable before remote archival; restart reads its local
    /// checkpoint, while confirmed transfer restores the same checkpoint from canonical state.
    #[test]
    fn capture_checkpoint_follows_evidence_across_restart_and_confirmation() {
        let server = SqlServer::new();
        let dir = crate::testing::TempDir::new();
        let queue = Queue::open(&dir.join("pending.sqlite"), "mail-a", 4 * 1024 * 1024).unwrap();
        let mut collector =
            Connection::remote(&server.args, "mail-a", Some(queue.clone())).unwrap();
        let mut cloud = Connection::remote(&server.args, "mail-a", None).unwrap();
        let tx = collector.transaction().unwrap();
        tx.capture(Record::Event {
            id: "e1".to_owned(),
            route: "r1".to_owned(),
            payload: "{}".to_owned(),
            created: 1.0,
        })
        .unwrap();
        tx.capture(Record::Cursor {
            node: "mail-a".to_owned(),
            inode: "42".to_owned(),
            position: 100,
        })
        .unwrap();
        tx.commit().unwrap();
        assert!(cloud.cursor("mail-a").unwrap().is_none());
        assert_eq!(
            cloud
                .query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(collector);
        drop(queue);
        let restored = Queue::open(&dir.join("pending.sqlite"), "mail-a", 4 * 1024 * 1024).unwrap();
        let reader = Connection::remote(&server.args, "mail-a", Some(restored.clone())).unwrap();
        assert_eq!(
            reader.cursor("mail-a").unwrap(),
            Some(("42".to_owned(), 100))
        );
        crate::archive::transfer(&mut cloud, &restored).unwrap();
        assert!(restored.pending().unwrap().is_empty());
        assert_eq!(
            cloud.cursor("mail-a").unwrap(),
            reader.cursor("mail-a").unwrap()
        );
        assert!(restored.bytes().unwrap() < 4 * 1024 * 1024);
    }
}
