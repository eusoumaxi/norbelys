//! The PostgreSQL scraper: the database's own statistics, read every [`EVERY`] by the worker on
//! a dedicated connection of the `norbelys_metrics` login (a member of `pg_monitor`, with no
//! grant on any product table), and recorded as the `norbelys_pg_*` metrics.
//!
//! One owner reads them, so a deployment with several workers reports them several times under
//! different instances, never more often per instance; the queries read only statistics views
//! and catalogs, each a handful of rows, and nothing here can take a lock a product transaction
//! waits for.
//!
//! | Metric | What it reads |
//! |---|---|
//! | `pg_connections`, `pg_max_connections` | client sessions in `pg_stat_activity`; `max_connections` |
//! | `pg_lock_waits` | lock requests not granted (`pg_locks`) |
//! | `pg_longest_transaction_seconds` | the oldest open transaction of a client session |
//! | `pg_oldest_snapshot_seconds` | the oldest session holding back cleanup (`backend_xmin`) |
//! | `pg_waiting_sessions{wait_event_type}` | active client sessions waiting, by wait event type |
//! | `pg_notification_queue_usage` | the share of the `NOTIFY` queue in use |
//! | `pg_dead_tuples`, `pg_dead_tuples_ratio`, `pg_vacuum_running`, `pg_xid_age`, `pg_multixact_age` (by `table_class`) | `pg_stat_all_tables`, `pg_stat_progress_vacuum`, `pg_class` |
//! | `pg_wal_bytes_total`, `pg_wal_retained_bytes` | `pg_stat_wal`; the WAL replication slots hold |
//! | `pg_archive_lag_seconds` | how long the archiver has been failing (`pg_stat_archiver`) |
//! | `pg_checkpoint_seconds_total` | time checkpoints spent writing and syncing (`pg_stat_checkpointer`) |
//! | `pg_temp_files_total` | temporary files written (`pg_stat_database`) |
//! | `pg_replication_lag_seconds` | the longest replay lag of a standby, 0 without one |
//! | `pg_top_query_seconds{query_id}` | the 20 statements with the most execution time (`pg_stat_statements`) |
//!
//! Tables are grouped by class (`domain::telemetry::table_class`), a partition leaf under its
//! partitioned table, so the labels never grow with the schema. The dead-tuple ratio is the worst
//! table's of each class among tables of at least [`RATIO_FLOOR`] rows: a few dead rows in a
//! tiny table are not bloat. Cumulative statistics become counters by their increase between
//! readings (a statistics reset counts as a new start). `pg_top_query_seconds` exists only where
//! the `pg_stat_statements` extension is installed and preloaded; elsewhere it is left out, and
//! only the current top statements are ever reported, so the label stays bounded. Planning time
//! is not read: `pg_stat_statements.track_planning` stays off, since it contends on every
//! statement.
//!
//! Sources: PostgreSQL 18, "The Cumulative Statistics System"
//! (<https://www.postgresql.org/docs/18/monitoring-stats.html>), "pg_stat_statements"
//! (<https://www.postgresql.org/docs/18/pgstatstatements.html>).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge};
use sqlx::AssertSqlSafe;
use strum::IntoEnumIterator as _;

use crate::db::Database;
use crate::domain::telemetry::{TableClass, table_class};
use crate::process::Shutdown;

/// How often the statistics are read.
const EVERY: Duration = Duration::from_secs(15);
/// How many statements `pg_top_query_seconds` reports.
const TOP_QUERIES: usize = 20;
/// Tables with fewer rows (live and dead) are left out of the dead-tuple ratio.
const RATIO_FLOOR: i64 = 10_000;
/// The wait event types PostgreSQL reports; a type outside the list is not recorded, so the
/// label stays bounded.
const WAIT_TYPES: [&str; 10] = [
    "Activity",
    "BufferPin",
    "Client",
    "Extension",
    "IO",
    "IPC",
    "InjectionPoint",
    "LWLock",
    "Lock",
    "Timeout",
];

/// What one reading found.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Reading {
    /// Client sessions.
    pub connections: i64,
    /// The server's `max_connections`.
    pub max_connections: i64,
    /// Lock requests waiting.
    pub lock_waits: i64,
    /// The oldest open transaction's age, in seconds.
    pub longest_transaction: f64,
    /// The age of the oldest session holding a snapshot, in seconds.
    pub oldest_snapshot: f64,
    /// The share of the notification queue in use.
    pub notification_queue: f64,
    /// Active client sessions waiting, by wait event type.
    pub waiting: BTreeMap<String, i64>,
    /// The tables, by class; every class is present.
    pub tables: BTreeMap<TableClass, Tables>,
    /// WAL bytes written since the statistics were reset.
    pub wal_bytes: i64,
    /// WAL the replication slots retain, in bytes.
    pub wal_retained: i64,
    /// How long the archiver has been failing, in seconds; 0 while it succeeds.
    pub archive_lag: f64,
    /// Seconds checkpoints spent writing and syncing since the statistics were reset.
    pub checkpoint_seconds: f64,
    /// Temporary files written since the statistics were reset.
    pub temp_files: i64,
    /// The longest replay lag of a standby, in seconds.
    pub replication_lag: f64,
    /// The statements with the most execution time (query id, seconds), or `None` where
    /// `pg_stat_statements` cannot be read.
    pub top_queries: Option<Vec<(i64, f64)>>,
    pub top_queries_state: &'static str,
}

/// One class of tables.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Tables {
    /// Dead tuples, summed.
    pub dead: i64,
    /// The worst dead-tuple ratio among the class's tables of at least [`RATIO_FLOOR`] rows.
    pub dead_ratio: f64,
    /// Tables being vacuumed now.
    pub vacuuming: i64,
    /// The oldest unfrozen transaction id's age, the worst table's.
    pub xid_age: i64,
    /// The oldest multixact id's age, the worst table's.
    pub multixact_age: i64,
    /// Tables seen.
    pub count: i64,
}

/// Reads every statistic once (see the module).
///
/// # Errors
///
/// The database refused one of the statistics queries; `pg_stat_statements` failing is not an
/// error (its metric is left out).
pub(crate) async fn read(db: &Database) -> Result<Reading, sqlx::Error> {
    let sessions = sqlx::query!(
        r#"SELECT count(*) FILTER (WHERE backend_type = 'client backend') AS "connections!",
                  current_setting('max_connections')::bigint AS "max_connections!",
                  coalesce(extract(epoch FROM max(now() - xact_start)
                           FILTER (WHERE backend_type = 'client backend' AND pid <> pg_backend_pid())), 0)::float8 AS "longest_transaction!",
                  coalesce(extract(epoch FROM max(now() - coalesce(xact_start, query_start))
                           FILTER (WHERE backend_xmin IS NOT NULL AND pid <> pg_backend_pid())), 0)::float8 AS "oldest_snapshot!",
                  (SELECT count(*) FROM pg_locks WHERE NOT granted) AS "lock_waits!",
                  pg_notification_queue_usage() AS "notification_queue!"
             FROM pg_stat_activity"#
    )
    .fetch_one(db.pool())
    .await?;
    let waiting = sqlx::query!(
        r#"SELECT wait_event_type AS "wait_event_type!", count(*) AS "sessions!"
             FROM pg_stat_activity
            WHERE backend_type = 'client backend' AND state = 'active' AND wait_event_type IS NOT NULL
              AND pid <> pg_backend_pid()
            GROUP BY wait_event_type"#
    )
    .fetch_all(db.pool())
    .await?
    .into_iter()
    .map(|row| (row.wait_event_type, row.sessions))
    .collect();
    let rows = sqlx::query!(
        r#"SELECT n.nspname::text AS "schema!", coalesce(r.relname, c.relname)::text AS "table!",
                  coalesce(s.n_dead_tup, 0) AS "dead!", coalesce(s.n_live_tup, 0) AS "live!",
                  age(c.relfrozenxid)::bigint AS "xid_age!", mxid_age(c.relminmxid)::bigint AS "multixact_age!",
                  EXISTS (SELECT 1 FROM pg_stat_progress_vacuum v WHERE v.relid = c.oid) AS "vacuuming!"
             FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace
             LEFT JOIN pg_stat_all_tables s ON s.relid = c.oid
             LEFT JOIN pg_class r ON r.oid = pg_partition_root(c.oid)
            WHERE c.relkind IN ('r', 'm', 't')"#
    )
    .fetch_all(db.pool())
    .await?;
    let mut tables: BTreeMap<TableClass, Tables> = TableClass::iter()
        .map(|class| (class, Tables::default()))
        .collect();
    for row in rows {
        let class = tables
            .entry(table_class(&row.schema, &row.table))
            .or_default();
        class.count += 1;
        class.dead = class.dead.saturating_add(row.dead);
        class.vacuuming += i64::from(row.vacuuming);
        class.xid_age = class.xid_age.max(row.xid_age);
        class.multixact_age = class.multixact_age.max(row.multixact_age);
        let all = row.dead.saturating_add(row.live);
        if all >= RATIO_FLOOR
            && let Some(share) = ratio(row.dead, all)
        {
            class.dead_ratio = class.dead_ratio.max(share);
        }
    }
    let storage = sqlx::query!(
        r#"SELECT (SELECT wal_bytes FROM pg_stat_wal)::bigint AS "wal_bytes!",
                  (SELECT coalesce(max(CASE WHEN pg_is_in_recovery() THEN NULL ELSE pg_current_wal_lsn() - restart_lsn END), 0)
                     FROM pg_replication_slots)::bigint AS "wal_retained!",
                  coalesce((SELECT CASE WHEN last_failed_time IS NOT NULL
                                             AND (last_archived_time IS NULL OR last_failed_time > last_archived_time)
                                        THEN extract(epoch FROM now() - coalesce(last_archived_time, stats_reset))
                                        ELSE 0 END
                              FROM pg_stat_archiver), 0)::float8 AS "archive_lag!",
                  coalesce((SELECT (write_time + sync_time) / 1000.0 FROM pg_stat_checkpointer), 0)::float8 AS "checkpoint_seconds!",
                  coalesce((SELECT sum(temp_files) FROM pg_stat_database), 0)::bigint AS "temp_files!",
                  coalesce((SELECT extract(epoch FROM max(replay_lag)) FROM pg_stat_replication), 0)::float8 AS "replication_lag!""#
    )
    .fetch_one(db.pool())
    .await?;
    let (top_queries, top_queries_state) = top_queries(db).await;
    Ok(Reading {
        connections: sessions.connections,
        max_connections: sessions.max_connections,
        lock_waits: sessions.lock_waits,
        longest_transaction: sessions.longest_transaction,
        oldest_snapshot: sessions.oldest_snapshot,
        notification_queue: sessions.notification_queue,
        waiting,
        tables,
        wal_bytes: storage.wal_bytes,
        wal_retained: storage.wal_retained,
        archive_lag: storage.archive_lag,
        checkpoint_seconds: storage.checkpoint_seconds,
        temp_files: storage.temp_files,
        replication_lag: storage.replication_lag,
        top_queries,
        top_queries_state,
    })
}

/// The statements with the most execution time, from `pg_stat_statements` in whatever schema
/// it was created in; `None` when it is not installed, or installed without being preloaded
/// (its view then refuses every read).
async fn top_queries(db: &Database) -> (Option<Vec<(i64, f64)>>, &'static str) {
    let schema = sqlx::query_scalar!(
        r#"SELECT n.nspname::text AS "schema!" FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
            WHERE e.extname = 'pg_stat_statements'"#
    )
    .fetch_optional(db.pool())
    .await;
    let schema = match schema {
        Ok(Some(schema)) => schema,
        Ok(None) => return (None, "disabled"),
        Err(_) => return (None, "unavailable"),
    };
    let statement = format!(
        "SELECT queryid, total_exec_time / 1000.0 FROM {}.pg_stat_statements
          WHERE queryid IS NOT NULL ORDER BY total_exec_time DESC LIMIT {TOP_QUERIES}",
        crate::analytics::parquet::quoted(&schema)
    );
    match sqlx::query_as::<_, (i64, f64)>(AssertSqlSafe(statement))
        .fetch_all(db.pool())
        .await
    {
        Ok(rows) => (Some(rows), "available"),
        Err(_) => (None, "unavailable"),
    }
}

/// `part / whole`, or `None` when `whole` is not positive.
fn ratio(part: i64, whole: i64) -> Option<f64> {
    if whole <= 0 {
        return None;
    }
    let millionths = i128::from(part.clamp(0, whole)) * 1_000_000 / i128::from(whole);
    u32::try_from(millionths)
        .ok()
        .map(|millionths| f64::from(millionths) / 1_000_000.0)
}

/// The instruments of the scraper.
struct Instruments {
    connections: Gauge<u64>,
    max_connections: Gauge<u64>,
    lock_waits: Gauge<u64>,
    longest_transaction: Gauge<f64>,
    oldest_snapshot: Gauge<f64>,
    waiting: Gauge<u64>,
    notification_queue: Gauge<f64>,
    dead_tuples: Gauge<u64>,
    dead_ratio: Gauge<f64>,
    vacuum_running: Gauge<u64>,
    xid_age: Gauge<u64>,
    multixact_age: Gauge<u64>,
    wal_bytes: Counter<u64>,
    wal_retained: Gauge<u64>,
    archive_lag: Gauge<f64>,
    checkpoint: Counter<f64>,
    temp_files: Counter<u64>,
    replication_lag: Gauge<f64>,
    /// The latest top statements, which the observable gauge reports at each collection.
    top: Arc<Mutex<Vec<(i64, f64)>>>,
}

/// The previous cumulative readings, to count their increase.
#[derive(Debug, Default)]
struct Previous {
    wal_bytes: Option<u64>,
    checkpoint_seconds: Option<f64>,
    temp_files: Option<u64>,
}

/// The increase of a cumulative `now` since `previous` (all of it after a reset or at the first
/// reading), and `now` kept as the next previous.
fn increase(previous: &mut Option<u64>, now: u64) -> u64 {
    let increase = match *previous {
        Some(before) if now >= before => now - before,
        _ => now,
    };
    *previous = Some(now);
    increase
}

/// [`increase`] for a reading in seconds.
fn increase_f64(previous: &mut Option<f64>, now: f64) -> f64 {
    let increase = match *previous {
        Some(before) if now >= before => now - before,
        _ => now,
    };
    *previous = Some(now);
    increase.max(0.0)
}

/// A count as a gauge's value: never negative.
fn count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

impl Instruments {
    fn new() -> Self {
        let meter = opentelemetry::global::meter("norbelys");
        let top: Arc<Mutex<Vec<(i64, f64)>>> = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&top);
        let _ = meter
            .f64_observable_gauge("norbelys_pg_top_query_seconds")
            .with_unit("s")
            .with_description(
                "Execution time of the statements that took the most, by query id (the top 20).",
            )
            .with_callback(move |observer| {
                if let Ok(top) = observed.lock() {
                    for (query_id, seconds) in top.iter() {
                        observer
                            .observe(*seconds, &[KeyValue::new("query_id", query_id.to_string())]);
                    }
                }
            })
            .build();
        let gauge = |name: &'static str, description: &'static str| {
            meter.u64_gauge(name).with_description(description).build()
        };
        let seconds = |name: &'static str, description: &'static str| {
            meter
                .f64_gauge(name)
                .with_unit("s")
                .with_description(description)
                .build()
        };
        Self {
            connections: gauge("norbelys_pg_connections", "Client sessions open on the server."),
            max_connections: gauge("norbelys_pg_max_connections", "The server's max_connections."),
            lock_waits: gauge("norbelys_pg_lock_waits", "Lock requests waiting to be granted."),
            longest_transaction: seconds(
                "norbelys_pg_longest_transaction_seconds",
                "Age of the oldest open transaction of a client session.",
            ),
            oldest_snapshot: seconds(
                "norbelys_pg_oldest_snapshot_seconds",
                "Age of the oldest session holding a snapshot that keeps vacuum from cleaning up.",
            ),
            waiting: gauge(
                "norbelys_pg_waiting_sessions",
                "Active client sessions waiting, by wait event type.",
            ),
            notification_queue: meter
                .f64_gauge("norbelys_pg_notification_queue_usage")
                .with_description("The share of the NOTIFY queue in use.")
                .build(),
            dead_tuples: gauge(
                "norbelys_pg_dead_tuples",
                "Dead tuples waiting for vacuum, by table class.",
            ),
            dead_ratio: meter
                .f64_gauge("norbelys_pg_dead_tuples_ratio")
                .with_description(
                    "The worst share of dead tuples of a table of the class (at least 10,000 rows).",
                )
                .build(),
            vacuum_running: gauge(
                "norbelys_pg_vacuum_running",
                "Tables of the class being vacuumed now.",
            ),
            xid_age: gauge(
                "norbelys_pg_xid_age",
                "Age of the oldest unfrozen transaction id of the class's tables.",
            ),
            multixact_age: gauge(
                "norbelys_pg_multixact_age",
                "Age of the oldest multixact id of the class's tables.",
            ),
            wal_bytes: meter
                .u64_counter("norbelys_pg_wal_bytes_total")
                .with_description("WAL written, in bytes.")
                .build(),
            wal_retained: gauge(
                "norbelys_pg_wal_retained_bytes",
                "WAL the replication slots retain, in bytes.",
            ),
            archive_lag: seconds(
                "norbelys_pg_archive_lag_seconds",
                "How long WAL archiving has been failing; 0 while it succeeds.",
            ),
            checkpoint: meter
                .f64_counter("norbelys_pg_checkpoint_seconds_total")
                .with_description("Seconds checkpoints spent writing and syncing files.")
                .build(),
            temp_files: meter
                .u64_counter("norbelys_pg_temp_files_total")
                .with_description("Temporary files queries wrote (work_mem exceeded).")
                .build(),
            replication_lag: seconds(
                "norbelys_pg_replication_lag_seconds",
                "The longest replay lag of a standby; 0 without one.",
            ),
            top,
        }
    }

    fn record(&self, reading: &Reading, previous: &mut Previous) {
        self.connections.record(count(reading.connections), &[]);
        self.max_connections
            .record(count(reading.max_connections), &[]);
        self.lock_waits.record(count(reading.lock_waits), &[]);
        self.longest_transaction
            .record(reading.longest_transaction.max(0.0), &[]);
        self.oldest_snapshot
            .record(reading.oldest_snapshot.max(0.0), &[]);
        self.notification_queue
            .record(reading.notification_queue.max(0.0), &[]);
        for wait_type in WAIT_TYPES {
            let sessions = reading.waiting.get(wait_type).copied().unwrap_or(0);
            self.waiting.record(
                count(sessions),
                &[KeyValue::new("wait_event_type", wait_type)],
            );
        }
        for (class, tables) in &reading.tables {
            // A class with no table at all (a database without the product's schema) says
            // nothing about vacuum or freezing.
            if tables.count == 0 {
                continue;
            }
            let label = [KeyValue::new("table_class", class.as_str())];
            self.dead_tuples.record(count(tables.dead), &label);
            self.dead_ratio.record(tables.dead_ratio, &label);
            self.vacuum_running.record(count(tables.vacuuming), &label);
            self.xid_age.record(count(tables.xid_age), &label);
            self.multixact_age
                .record(count(tables.multixact_age), &label);
        }
        self.wal_bytes.add(
            increase(&mut previous.wal_bytes, count(reading.wal_bytes)),
            &[],
        );
        self.wal_retained.record(count(reading.wal_retained), &[]);
        self.archive_lag.record(reading.archive_lag.max(0.0), &[]);
        self.checkpoint.add(
            increase_f64(&mut previous.checkpoint_seconds, reading.checkpoint_seconds),
            &[],
        );
        self.temp_files.add(
            increase(&mut previous.temp_files, count(reading.temp_files)),
            &[],
        );
        self.replication_lag
            .record(reading.replication_lag.max(0.0), &[]);
        if let Ok(mut top) = self.top.lock() {
            *top = reading
                .top_queries
                .clone()
                .unwrap_or_default()
                .into_iter()
                .take(TOP_QUERIES)
                .collect();
        }
    }
}

/// Reads the statistics every [`EVERY`] until `shutdown`; a failed reading is logged and the
/// next one tried at its time.
pub(crate) async fn scrape(db: Database, mut shutdown: Shutdown) {
    let instruments = Instruments::new();
    let mut previous = Previous::default();
    let meter = opentelemetry::global::meter("norbelys");
    let available = meter.u64_gauge("norbelys_pg_scrape_available").build();
    let last_success = meter
        .u64_gauge("norbelys_pg_scrape_last_success_unix_seconds")
        .build();
    let optional = meter.u64_gauge("norbelys_pg_query_stats_state").build();
    let mut status = None;
    let mut failing = false;
    loop {
        match read(&db).await {
            Ok(reading) => {
                available.record(1, &[]);
                last_success.record(
                    u64::try_from(jiff::Timestamp::now().as_second()).unwrap_or_default(),
                    &[],
                );
                for state in ["available", "disabled", "unavailable"] {
                    optional.record(
                        u64::from(state == reading.top_queries_state),
                        &[KeyValue::new("state", state)],
                    );
                }
                if status != Some(reading.top_queries_state) {
                    tracing::info!(
                        state = reading.top_queries_state,
                        "optional PostgreSQL statement statistics changed availability"
                    );
                    status = Some(reading.top_queries_state);
                }
                failing = false;
                instruments.record(&reading, &mut previous);
            }
            Err(_) => {
                available.record(0, &[]);
                if !failing {
                    tracing::warn!(
                        error_code = "postgres_scrape_failed",
                        "PostgreSQL statistics unavailable; last-success time remains unchanged"
                    );
                }
                failing = true;
            }
        }
        tokio::select! {
            () = tokio::time::sleep(EVERY) => {}
            () = shutdown.wait() => return,
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::testing::TestDb;

    /// Every statistics query runs under the scraper's own login, which holds `pg_monitor` and no
    /// grant on a product table: the readings are sane (this session counts, the queue and fact
    /// tables are found by class, a partition leaf under its partitioned table), and a product
    /// table stays unreadable to it. The scraper can see everything it needs and nothing a
    /// leaked connection string could abuse.
    #[tokio::test]
    async fn the_scraper_reads_every_statistic_as_its_own_login() {
        let test = TestDb::new().await;
        let metrics = test.metrics().await;
        let reading = read(&metrics).await.unwrap();
        assert!(reading.connections >= 1, "{reading:?}");
        assert!(reading.max_connections >= reading.connections);
        assert!(reading.lock_waits >= 0);
        assert!((0.0..=1.0).contains(&reading.notification_queue));
        assert!(reading.wal_bytes >= 0 && reading.temp_files >= 0);
        assert!(reading.archive_lag >= 0.0 && reading.replication_lag >= 0.0);
        for class in TableClass::iter() {
            assert!(reading.tables.contains_key(&class), "{class:?}");
        }
        for class in [
            TableClass::Queue,
            TableClass::Counters,
            TableClass::Facts,
            TableClass::System,
        ] {
            assert!(
                reading
                    .tables
                    .get(&class)
                    .is_some_and(|tables| tables.count > 0),
                "{class:?} has tables"
            );
        }
        if let Some(top) = &reading.top_queries {
            assert!(top.len() <= TOP_QUERIES);
        }
        let refused = sqlx::query("SELECT 1 FROM messages LIMIT 1")
            .execute(metrics.pool())
            .await;
        assert!(
            refused.is_err(),
            "the scraper's login reads no product table"
        );
    }

    /// Cumulative statistics count their increase between readings; the first reading and a
    /// reset count the whole value, so the counter goes on from where the database's own does.
    #[test]
    fn cumulative_statistics_count_their_increase() {
        let mut previous = None;
        assert_eq!(increase(&mut previous, 100), 100);
        assert_eq!(increase(&mut previous, 130), 30);
        assert_eq!(increase(&mut previous, 130), 0);
        assert_eq!(increase(&mut previous, 5), 5);
        let mut seconds = None;
        assert!((increase_f64(&mut seconds, 1.5) - 1.5).abs() < f64::EPSILON);
        assert!((increase_f64(&mut seconds, 2.0) - 0.5).abs() < f64::EPSILON);
        assert_eq!(ratio(1, 4), Some(0.25));
        assert_eq!(ratio(0, 0), None);
        assert_eq!(ratio(-3, 10), Some(0.0));
    }
}
