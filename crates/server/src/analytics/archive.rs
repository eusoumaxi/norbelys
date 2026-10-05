//! `archive.export`: takes each partition leaf past its table's retention out of the database,
//! exporting it to Parquet in object storage first when the table is archived.
//!
//! # The sealed order
//!
//! A leaf is due once its whole period is older than its table's online window
//! (`upper <= now() − retention`). For each due leaf:
//!
//! 1. **Gate.** Each table has a condition that keeps live rows online ([`Gate`]): no queued
//!    message and no unsettled attempt in a messages period, no receipt still waiting for
//!    normalisation, every outbox event published, no webhook delivery pending, the increments
//!    counted by the rollup and their days recounted. A leaf failing its gate (or its dependent's)
//!    stays online as retention debt, counted in `norbelys_archive_debt_periods`; it is never
//!    forced out.
//! 2. **Dependents first.** An attempts leaf leaves before the messages leaf of its period, a
//!    webhook deliveries leaf before its outbox leaf: their rows reference the other's, and
//!    PostgreSQL refuses to detach a leaf that rows still reference.
//! 3. **References.** Before a messages leaf leaves, the long-lived rows that point into it are
//!    released: inbound messages and enrollments forget the message (`message_id = NULL`), and
//!    the period's recipient holds are deleted.
//! 4. **Four moves per leaf**: detach it with `DETACH PARTITION … CONCURRENTLY` (outside any
//!    transaction, as PostgreSQL requires; an interrupted detach is completed with `… FINALIZE`
//!    first); seal it by dropping its own outgoing foreign keys, so no delete elsewhere can
//!    cascade into it or be blocked by it (no role holds DML on a leaf, so the sealed table is
//!    frozen); export it to Parquet, verify the object (the rows written equal the leaf's count,
//!    and the stored object's SHA-256 equals the written file's) and write its manifest; drop it.
//!
//! Each leaf that leaves ends with one `archive.export` event (table, leaf, rows and bytes
//! exported, whether its object was verified, `dropped`); a leaf its gate holds back emits one
//! too, as a warning with `dropped = false`.
//!
//! The DDL runs as `norbelys_owner`, reached by `SET ROLE` from the system login, which is the
//! only login allowed to. The detach makes PostgreSQL check, as the owner, that no row elsewhere
//! still references the leaf; the owner's read-only `detach_check` policies let that check see
//! the rows, so a period still referenced is refused even if a gate were wrong.
//!
//! # Locks, crashes and repeats
//!
//! A leaf's whole order runs on one dedicated connection of the system pool holding the
//! retention lock shared (`pg_advisory_lock_shared` on the `partition_policies` table's OID), so
//! no change of a retention can interleave with it; the connection is closed rather than
//! returned to the pool, so its role, settings and lock never leak. Every step looks at what is
//! already done (detached, exported, dropped) and does only the rest, and each step's result is
//! recorded on the leaf's `partition_leaves` row under the job's fence, so a crash anywhere
//! resumes where it stopped: an export repeated after a crash writes the same object again.
//!
//! # The manifest
//!
//! Next to each Parquet object, `archive/<table>/<leaf>.json` records the leaf's bounds, its
//! columns and their encodings, the rows and the object's SHA-256: once the leaf is dropped,
//! object storage holds the only copy, and it must describe itself.

use std::collections::HashMap;
use std::io::{BufWriter, Read as _};
use std::str::FromStr as _;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use futures_util::{StreamExt as _, TryStreamExt as _};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Gauge;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::pool::PoolConnection;
use sqlx::{AssertSqlSafe, Postgres};
use uuid::Uuid;

use super::parquet::{self, quoted};
use super::rollup::RECOUNTED;
use crate::crypto;
use crate::domain::analytics::{self, Gate, Partitioned, uuidv7_boundary};
use crate::domain::time::Timestamp;
use crate::jobs::{Class, Effect, Job, JobContext, JobError, Outcome, Queue};
use crate::storage::{Storage, StorageError};

/// How often a long export renews the job's lease.
const RENEW: Duration = Duration::from_secs(15);
/// The piece of a file read and sent to the object store at a time.
const PIECE: usize = 8 << 20;

static DEBT: LazyLock<Gauge<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_gauge("norbelys_archive_debt_periods")
        .with_description("Partition leaves past their retention that failed their archive gate.")
        .build()
});

/// A leaf of a partitioned table, as `partition_leaves` records it.
#[derive(Debug, Clone)]
struct Leaf {
    parent: String,
    name: String,
    lower: Timestamp,
    upper: Timestamp,
    archive: bool,
    archive_key: Option<String>,
}

/// `archive.export` (see the module).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArchiveExport {}

impl Job for ArchiveExport {
    const KIND: &'static str = "archive.export";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::ExternalRetryable;
    const CLASS: Class = Class::System;

    fn unique_key(&self) -> Option<String> {
        Some("singleton".to_owned())
    }

    fn schedule() -> Option<(&'static str, Self)> {
        Some(("30 0 * * *", Self {}))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        run(cx).await
    }
}

fn failed(error: impl std::fmt::Display) -> JobError {
    JobError::Failed(error.to_string())
}

async fn run(cx: &mut JobContext) -> Result<Outcome, JobError> {
    let storage = cx.env::<Storage>()?.clone();
    let due = {
        let mut tx = cx.system()?.begin().await?;
        let due = sqlx::query_as!(
            Leaf,
            r#"SELECT l.parent, l.name, l.lower AS "lower: Timestamp", l.upper AS "upper: Timestamp", p.archive, l.archive_key
                 FROM partition_leaves l JOIN partition_policies p ON p.table_name = l.parent
                WHERE l.dropped_at IS NULL AND l.upper <= now() - p.retention
                ORDER BY l.upper, l.parent"#
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        due
    };
    let mut debt: HashMap<Partitioned, u64> = HashMap::new();
    for leaf in &due {
        let Ok(table) = Partitioned::from_str(&leaf.parent) else {
            tracing::warn!(table = %leaf.parent, leaf = %leaf.name, "archive.export: a partitioned table without archive rules");
            continue;
        };
        // A dependent leaves with its partner, in the partner's order.
        if table.archived_with().is_some() {
            continue;
        }
        let dependent = match table.dependent() {
            Some(dependent) => due
                .iter()
                .find(|other| {
                    other.parent == dependent.as_str()
                        && other.lower == leaf.lower
                        && other.upper == leaf.upper
                })
                .map(|other| (dependent, other)),
            None => None,
        };
        let open = gate_open(cx, table, leaf).await?
            && match dependent {
                Some((dependent, other)) => gate_open(cx, dependent, other).await?,
                None => true,
            };
        if !open {
            *debt.entry(table).or_default() += 1;
            crate::telemetry::unit(crate::telemetry::Event::ArchiveExport);
            tracing::warn!(
                event = "archive.export",
                table = %leaf.parent,
                partition = %leaf.name,
                verified = false,
                dropped = false,
                "archive.export: the leaf failed its gate; it stays as retention debt"
            );
            continue;
        }
        let mut conn = cx.system()?.pool().acquire().await?;
        // The session's role, settings and lock die with the connection.
        conn.close_on_drop();
        sqlx::query!("SELECT pg_advisory_lock_shared('partition_policies'::regclass::oid::bigint)")
            .execute(&mut *conn)
            .await?;
        if let Some((_, other)) = dependent {
            move_out(cx, &mut conn, &storage, other).await?;
        }
        if table == Partitioned::Messages {
            release_references(cx, leaf).await?;
        }
        move_out(cx, &mut conn, &storage, leaf).await?;
        drop(conn);
        if cx.should_yield() {
            return Ok(Outcome::Yield {
                after: Duration::ZERO,
            });
        }
    }
    for table in <Partitioned as strum::IntoEnumIterator>::iter() {
        let label = [KeyValue::new("table", table.as_str())];
        DEBT.record(debt.get(&table).copied().unwrap_or(0), &label);
    }
    Ok(Outcome::Done)
}

/// Whether `leaf` of `table` passes its gate now.
async fn gate_open(cx: &JobContext, table: Partitioned, leaf: &Leaf) -> Result<bool, JobError> {
    let lo = uuidv7_boundary(leaf.lower);
    let hi = uuidv7_boundary(leaf.upper);
    let mut tx = cx.system()?.begin().await?;
    let open = match table.gate() {
        Gate::None => true,
        Gate::NothingInFlight => sqlx::query_scalar!(
            r#"SELECT NOT EXISTS (SELECT 1 FROM delivery_queue WHERE message_id >= $1 AND message_id < $2)
                  AND NOT EXISTS (SELECT 1 FROM attempts WHERE message_id >= $1 AND message_id < $2 AND finished_at IS NULL)
                  AS "open!""#,
            lo,
            hi,
        )
        .fetch_one(&mut *tx)
        .await?,
        Gate::ReceiptsNormalized => sqlx::query_scalar!(
            r#"SELECT NOT EXISTS (SELECT 1 FROM webhook_receipts WHERE id >= $1 AND id < $2 AND state = 'received') AS "open!""#,
            lo,
            hi,
        )
        .fetch_one(&mut *tx)
        .await?,
        Gate::OutboxPublished => sqlx::query_scalar!(
            r#"SELECT NOT EXISTS (SELECT 1 FROM outbox_events WHERE id >= $1 AND id < $2 AND published_at IS NULL) AS "open!""#,
            lo,
            hi,
        )
        .fetch_one(&mut *tx)
        .await?,
        Gate::DeliveriesSettled => sqlx::query_scalar!(
            r#"SELECT NOT EXISTS (SELECT 1 FROM webhook_deliveries WHERE event_id >= $1 AND event_id < $2 AND state = 'pending') AS "open!""#,
            lo,
            hi,
        )
        .fetch_one(&mut *tx)
        .await?,
        Gate::IncrementsCounted => {
            let marks = sqlx::query!(
                "SELECT name, processed_to FROM rollup_watermarks WHERE name IN ($1, $2)",
                super::WATERMARK,
                RECOUNTED,
            )
            .fetch_all(&mut *tx)
            .await?;
            let mark = |name: &str| {
                marks
                    .iter()
                    .find(|mark| mark.name == name)
                    .map(|mark| mark.processed_to)
            };
            analytics::increments_counted(
                leaf.upper,
                mark(super::WATERMARK).unwrap_or(Uuid::nil()),
                mark(RECOUNTED)
                    .and_then(analytics::uuidv7_instant)
                    .map(crate::domain::time::Date::utc_day),
            )
        }
    };
    tx.commit().await?;
    Ok(open)
}

/// Releases the long-lived rows that point into a messages leaf's period (see the module), in a
/// chunk of the job.
async fn release_references(cx: &mut JobContext, leaf: &Leaf) -> Result<(), JobError> {
    let lo = uuidv7_boundary(leaf.lower);
    let hi = uuidv7_boundary(leaf.upper);
    let mut chunk = cx.begin().await?;
    sqlx::query!(
        "UPDATE inbound_messages SET message_id = NULL WHERE message_id >= $1 AND message_id < $2",
        lo,
        hi,
    )
    .execute(&mut **chunk.tx())
    .await?;
    sqlx::query!(
        "UPDATE enrollments SET message_id = NULL WHERE message_id >= $1 AND message_id < $2",
        lo,
        hi,
    )
    .execute(&mut **chunk.tx())
    .await?;
    sqlx::query!(
        "DELETE FROM recipient_holds WHERE message_id >= $1 AND message_id < $2",
        lo,
        hi,
    )
    .execute(&mut **chunk.tx())
    .await?;
    cx.checkpoint(chunk, json!({ "released": leaf.name }))
        .await?;
    Ok(())
}

/// Detaches, seals, exports (when archived) and drops one leaf on `conn`, which holds the
/// retention lock; each step done is recorded, and a step already done is skipped.
async fn move_out(
    cx: &mut JobContext,
    conn: &mut PoolConnection<Postgres>,
    storage: &Storage,
    leaf: &Leaf,
) -> Result<(), JobError> {
    let started = Instant::now();
    // The rows and bytes this call exported and verified, when it exported the leaf.
    let mut written: Option<(i64, u64)> = None;
    let state = sqlx::query!(
        r#"SELECT to_regclass($1) IS NOT NULL AS "exists!",
                  (SELECT i.inhdetachpending FROM pg_inherits i WHERE i.inhrelid = to_regclass($1)) AS "pending?""#,
        leaf.name,
    )
    .fetch_one(&mut **conn)
    .await?;
    if state.exists {
        let parent = quoted(&leaf.parent);
        let name = quoted(&leaf.name);
        owner(conn, true).await?;
        match state.pending {
            Some(true) => {
                execute(
                    conn,
                    format!("ALTER TABLE {parent} DETACH PARTITION {name} FINALIZE"),
                )
                .await?;
            }
            Some(false) => {
                execute(
                    conn,
                    format!("ALTER TABLE {parent} DETACH PARTITION {name} CONCURRENTLY"),
                )
                .await?;
            }
            None => {}
        }
        let keys = sqlx::query_scalar!(
            r#"SELECT conname::text AS "name!" FROM pg_constraint
                WHERE conrelid = $1::text::regclass AND contype = 'f' AND conparentid = 0"#,
            leaf.name,
        )
        .fetch_all(&mut **conn)
        .await?;
        for key in keys {
            execute(
                conn,
                format!("ALTER TABLE {name} DROP CONSTRAINT {}", quoted(&key)),
            )
            .await?;
        }
        owner(conn, false).await?;
        if leaf.archive && leaf.archive_key.is_none() {
            let exported = export(cx, conn, storage, leaf).await?;
            let mut chunk = cx.begin().await?;
            sqlx::query!(
                "UPDATE partition_leaves SET archive_key = $2, archived_rows = $3, archive_sha256 = $4 WHERE name = $1",
                leaf.name,
                exported.key,
                exported.rows,
                exported.sha256,
            )
            .execute(&mut **chunk.tx())
            .await?;
            cx.checkpoint(
                chunk,
                json!({ "exported": leaf.name, "rows": exported.rows }),
            )
            .await?;
            written = Some((exported.rows, exported.bytes));
        }
        owner(conn, true).await?;
        execute(conn, format!("DROP TABLE {name}")).await?;
        owner(conn, false).await?;
    }
    let mut chunk = cx.begin().await?;
    sqlx::query!(
        "UPDATE partition_leaves SET dropped_at = coalesce(dropped_at, now()) WHERE name = $1",
        leaf.name,
    )
    .execute(&mut **chunk.tx())
    .await?;
    cx.checkpoint(chunk, json!({ "dropped": leaf.name }))
        .await?;
    crate::telemetry::unit(crate::telemetry::Event::ArchiveExport);
    tracing::info!(
        event = "archive.export",
        table = %leaf.parent,
        partition = %leaf.name,
        rows = written.map(|(rows, _)| rows),
        bytes = written.map(|(_, bytes)| bytes),
        verified = written.is_some() || leaf.archive_key.is_some(),
        dropped = true,
        duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "archive.export: the leaf left the database"
    );
    Ok(())
}

/// Switches `conn` to the owner (`true`) or back to its login.
async fn owner(conn: &mut PoolConnection<Postgres>, on: bool) -> Result<(), sqlx::Error> {
    let statement = if on {
        "SET ROLE norbelys_owner"
    } else {
        "RESET ROLE"
    };
    sqlx::query(statement).execute(&mut **conn).await?;
    Ok(())
}

/// Runs one DDL statement built from catalogue names (quoted).
async fn execute(
    conn: &mut PoolConnection<Postgres>,
    statement: String,
) -> Result<(), sqlx::Error> {
    sqlx::query(AssertSqlSafe(statement))
        .execute(&mut **conn)
        .await?;
    Ok(())
}

/// What an export stored.
struct Exported {
    key: String,
    rows: i64,
    sha256: String,
    /// The Parquet object's size.
    bytes: u64,
}

/// Exports the sealed `leaf` to `archive/<table>/<leaf>.parquet` with its manifest, and verifies
/// the stored object (see the module).
async fn export(
    cx: &mut JobContext,
    conn: &mut PoolConnection<Postgres>,
    storage: &Storage,
    leaf: &Leaf,
) -> Result<Exported, JobError> {
    let key = format!("archive/{}/{}.parquet", leaf.parent, leaf.name);
    let path =
        std::env::temp_dir().join(format!("norbelys-{}-{}.parquet", leaf.name, Uuid::now_v7()));
    let result = export_through(cx, conn, storage, leaf, &key, &path).await;
    if let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), error = %error, "archive.export: a temporary file was not removed");
    }
    result
}

async fn export_through(
    cx: &mut JobContext,
    conn: &mut PoolConnection<Postgres>,
    storage: &Storage,
    leaf: &Leaf,
    key: &str,
    path: &std::path::Path,
) -> Result<Exported, JobError> {
    let columns = parquet::columns(conn, &leaf.name).await?;
    let file = std::fs::File::create(path).map_err(failed)?;
    let mut writer = parquet::Writer::new(BufWriter::new(file), columns.clone()).map_err(failed)?;
    // A leaf can take longer to read than the pool's statement timeout.
    sqlx::query("SET statement_timeout = 0")
        .execute(&mut **conn)
        .await?;
    let mut renewed = Instant::now();
    {
        let statement = format!(
            "SELECT {} FROM {}",
            parquet::select_list(&columns),
            quoted(&leaf.name)
        );
        let mut rows = sqlx::query(AssertSqlSafe(statement)).fetch(&mut **conn);
        while let Some(row) = rows.try_next().await? {
            writer
                .push(parquet::cells(&row, &columns)?)
                .map_err(failed)?;
            if renewed.elapsed() >= RENEW {
                cx.heartbeat().await?;
                renewed = Instant::now();
            }
        }
    }
    let rows = writer.finish().map_err(failed)?;
    let counted: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT count(*) FROM {}",
        quoted(&leaf.name)
    )))
    .fetch_one(&mut **conn)
    .await?;
    sqlx::query("RESET statement_timeout")
        .execute(&mut **conn)
        .await?;
    if counted != rows {
        return Err(JobError::Failed(format!(
            "{} holds {counted} rows but {rows} were written",
            leaf.name
        )));
    }

    // Upload, hashing and counting what is sent.
    let mut sent = crypto::Sha256::new();
    let mut size: u64 = 0;
    let mut upload = storage.writer(key).await.map_err(failed)?;
    let mut file = std::fs::File::open(path).map_err(failed)?;
    let mut piece = vec![0_u8; PIECE];
    loop {
        let read = match file.read(&mut piece) {
            Ok(read) => read,
            Err(error) => {
                upload.abort().await;
                return Err(failed(error));
            }
        };
        let Some(bytes) = piece.get(..read).filter(|bytes| !bytes.is_empty()) else {
            break;
        };
        sent.update(bytes);
        size = size.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if let Err(error) = upload.write(bytes).await {
            upload.abort().await;
            return Err(failed(error));
        }
        if renewed.elapsed() >= RENEW {
            cx.heartbeat().await?;
            renewed = Instant::now();
        }
    }
    upload.finish().await.map_err(failed)?;
    let sha256 = crypto::hex(&sent.finish());

    // Verify: the stored object hashes to what was sent.
    let stored = stored_sha256(storage, key).await.map_err(failed)?;
    if stored != sha256 {
        return Err(JobError::Failed(format!(
            "the stored {key} does not match the file written (sha256 {stored}, expected {sha256})"
        )));
    }
    let manifest = json!({
        "table": leaf.parent,
        "leaf": leaf.name,
        "lower": leaf.lower,
        "upper": leaf.upper,
        "object": key,
        "rows": rows,
        "sha256": sha256,
        "columns": columns.iter().map(|column| json!({
            "name": column.name,
            "kind": format!("{:?}", column.kind).to_lowercase(),
        })).collect::<Vec<_>>(),
        "exported_at": crate::process::now(),
    });
    storage
        .put(
            &format!("archive/{}/{}.json", leaf.parent, leaf.name),
            bytes::Bytes::from(serde_json::to_vec(&manifest).map_err(failed)?),
        )
        .await
        .map_err(failed)?;
    Ok(Exported {
        key: key.to_owned(),
        rows,
        sha256,
        bytes: size,
    })
}

/// The SHA-256 (hex) of the stored object at `key`, read as a stream.
async fn stored_sha256(storage: &Storage, key: &str) -> Result<String, StorageError> {
    let (_, body) = storage.body(key).await?;
    let mut digest = crypto::Sha256::new();
    let mut stream = body.into_data_stream();
    while let Some(bytes) = stream.next().await {
        let bytes = bytes.map_err(|error| StorageError::Config(error.to_string()))?;
        digest.update(&bytes);
    }
    Ok(crypto::hex(&digest.finish()))
}
