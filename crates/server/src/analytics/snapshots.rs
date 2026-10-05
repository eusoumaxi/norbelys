//! The analytics role: report snapshots of the archive, computed with DuckDB over its Parquet
//! objects.
//!
//! Periods past their online window leave PostgreSQL as Parquet objects (`archive.export`), so
//! reports over them never scan the database. Every ten minutes this role looks for archived
//! periods of messages, attempts and delivery events (`partition_leaves.archive_key`) that have
//! no snapshot yet, copies each period's object to a local file, has DuckDB count its rows per
//! workspace, UTC day and outcome (a message's kind and state, an attempt's connection and
//! outcome, an event's kind and category), and writes the counts as
//! `reports/<table>/<leaf>.json` in object storage. A snapshot is computed once: an archived
//! period never changes. Workspaces erased since the period was archived (their deletion
//! completed) are left out, since their rows stay in the immutable archive until it expires.
//!
//! DuckDB is large to compile, so it is built only with the `analytics` feature (the analytics
//! image); without it this role refuses to start. Its memory is bounded (384 MB, two threads)
//! and it runs on a blocking thread, one period at a time.

use crate::db::Database;
use crate::process::Shutdown;
use crate::storage::Storage;
use anyhow::Context as _;
use std::collections::HashSet;
use std::time::Duration;

/// How often the role looks for periods without a snapshot.
const PASS: Duration = Duration::from_secs(600);

/// The tables with snapshots, with the instant that dates a row and the columns it is counted by.
const REPORTED: [(&str, &str, [&str; 2]); 3] = [
    ("messages", "created_at", ["kind", "state"]),
    ("attempts", "claimed_at", ["connection_id", "outcome"]),
    ("delivery_events", "created_at", ["kind", "category"]),
];

/// Compute missing snapshots until shutdown; freshness and failure metrics describe each pass.
pub(crate) async fn run(
    db: &Database,
    storage: &Storage,
    mut stop: Shutdown,
) -> anyhow::Result<()> {
    let meter = opentelemetry::global::meter("norbelys");
    let available = meter
        .u64_gauge("norbelys_analytics_snapshot_available")
        .build();
    let freshness = meter
        .u64_gauge("norbelys_analytics_snapshot_last_success_unix_seconds")
        .build();
    let outcomes = meter
        .u64_counter("norbelys_analytics_snapshot_passes_total")
        .build();
    available.record(0, &[]);
    freshness.record(0, &[]);
    loop {
        let started = std::time::Instant::now();
        let result = pass(db, storage).await;
        let (outcome, written) = match result {
            Ok(written) => {
                available.record(1, &[]);
                freshness.record(
                    u64::try_from(crate::process::now().0.as_second()).unwrap_or(0),
                    &[],
                );
                ("completed", written)
            }
            Err(_) => {
                available.record(0, &[]);
                tracing::warn!(
                    error_code = "snapshot_pass_failed",
                    "analytics snapshot pass failed; last successful gauges remain dated"
                );
                ("failed", 0)
            }
        };
        outcomes.add(1, &[opentelemetry::KeyValue::new("outcome", outcome)]);
        tracing::info!(
            event = "analytics.snapshot_pass",
            outcome,
            snapshots = written,
            duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "analytics.snapshot_pass"
        );
        tokio::select! {
            () = tokio::time::sleep(PASS) => {}
            () = stop.wait() => return Ok(()),
        }
    }
}

/// One pass: a snapshot for every archived period that has none; returns how many were written.
///
/// # Errors
///
/// The database, the object store or DuckDB failed.
pub(crate) async fn pass(db: &Database, storage: &Storage) -> anyhow::Result<usize> {
    let mut tx = db.begin().await?;
    let erased: HashSet<String> = sqlx::query_scalar!(
        "SELECT workspace_id::text AS \"workspace!\" FROM workspace_deletions WHERE completed_at IS NOT NULL"
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let leaves = sqlx::query!(
        r#"SELECT parent, name, archive_key AS "archive_key!", lower AS "lower: crate::domain::time::Timestamp",
                  upper AS "upper: crate::domain::time::Timestamp"
             FROM partition_leaves WHERE archive_key IS NOT NULL AND parent = ANY($1) ORDER BY lower"#,
        &REPORTED
            .iter()
            .map(|(table, _, _)| (*table).to_owned())
            .collect::<Vec<String>>(),
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut written = 0;
    for (table, time, dimensions) in REPORTED {
        let done: HashSet<String> = storage
            .list(&format!("reports/{table}"))
            .await?
            .into_iter()
            .collect();
        for leaf in leaves.iter().filter(|leaf| leaf.parent == table) {
            let key = format!("reports/{table}/{}.json", leaf.name);
            if done.contains(&key) {
                continue;
            }
            let path = std::env::temp_dir().join(format!("norbelys-report-{}.parquet", leaf.name));
            std::fs::write(&path, storage.get(&leaf.archive_key).await?).with_context(|| {
                format!("cannot copy {} to {}", leaf.archive_key, path.display())
            })?;
            let counted = {
                let path = path.clone();
                tokio::task::spawn_blocking(move || count(&path, time, dimensions)).await?
            };
            let _ = std::fs::remove_file(&path);
            let groups: Vec<serde_json::Value> = counted?
                .into_iter()
                .filter(|group| {
                    group["workspace_id"]
                        .as_str()
                        .is_none_or(|workspace| !erased.contains(workspace))
                })
                .collect();
            let snapshot = serde_json::json!({
                "table": table,
                "leaf": leaf.name,
                "lower": leaf.lower,
                "upper": leaf.upper,
                "computed_at": crate::process::now(),
                "groups": groups,
            });
            storage
                .put(&key, bytes::Bytes::from(serde_json::to_vec(&snapshot)?))
                .await?;
            written += 1;
        }
    }
    Ok(written)
}

/// Counts the rows of the Parquet file at `path` per workspace, UTC day of `time` and the two
/// `dimensions`, with DuckDB.
///
/// # Errors
///
/// DuckDB failed (the file is not the archive's Parquet, or the memory bound was reached).
pub(crate) fn count(
    path: &std::path::Path,
    time: &str,
    dimensions: [&str; 2],
) -> anyhow::Result<Vec<serde_json::Value>> {
    let connection = duckdb::Connection::open_in_memory()?;
    connection.execute_batch("SET memory_limit = '384MB'; SET threads = 2;")?;
    let [first, second] = dimensions;
    // The UTC day from the microseconds since the epoch: independent of any time zone setting.
    let statement = format!(
        "SELECT CAST(workspace_id AS VARCHAR),
                CAST(DATE '1970-01-01' + CAST(epoch_us({time}) // 86400000000 AS INTEGER) AS VARCHAR),
                CAST({first} AS VARCHAR), CAST({second} AS VARCHAR), count(*)
           FROM read_parquet('{}')
          GROUP BY ALL ORDER BY ALL",
        path.display().to_string().replace('\'', "''")
    );
    let mut prepared = connection.prepare(&statement)?;
    let rows = prepared.query_map([], |row| {
        let mut group = serde_json::Map::new();
        group.insert(
            "workspace_id".to_owned(),
            row.get::<_, Option<String>>(0)?.into(),
        );
        group.insert("day".to_owned(), row.get::<_, Option<String>>(1)?.into());
        group.insert(first.to_owned(), row.get::<_, Option<String>>(2)?.into());
        group.insert(second.to_owned(), row.get::<_, Option<String>>(3)?.into());
        group.insert("count".to_owned(), row.get::<_, i64>(4)?.into());
        Ok(serde_json::Value::Object(group))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use crate::analytics::parquet::{Cell, Column, Kind, Writer};

    /// DuckDB reads the archive's Parquet encoding as written (text ids, UTC microsecond
    /// timestamps) and counts per workspace, UTC day and outcome: a snapshot that misread the
    /// encoding would report wrong history for every archived period.
    #[test]
    fn duckdb_counts_the_archive_encoding() {
        let columns = vec![
            Column {
                name: "workspace_id".to_owned(),
                kind: Kind::Text,
            },
            Column {
                name: "created_at".to_owned(),
                kind: Kind::Timestamp,
            },
            Column {
                name: "kind".to_owned(),
                kind: Kind::Text,
            },
            Column {
                name: "state".to_owned(),
                kind: Kind::Text,
            },
        ];
        let path = std::env::temp_dir().join(format!("duckdb-{}.parquet", uuid::Uuid::now_v7()));
        let mut writer = Writer::new(std::fs::File::create(&path).unwrap(), columns).unwrap();
        // 2026-10-01T23:59:59Z twice and 2026-10-02T00:00:00Z once.
        for micros in [
            1_790_899_199_000_000_i64,
            1_790_899_199_000_000,
            1_790_899_200_000_000,
        ] {
            writer
                .push(vec![
                    Cell::Text("ws".to_owned()),
                    Cell::Timestamp(micros),
                    Cell::Text("campaign".to_owned()),
                    Cell::Text("sent".to_owned()),
                ])
                .unwrap();
        }
        writer.finish().unwrap();
        let counted = super::count(&path, "created_at", ["kind", "state"]).unwrap();
        std::fs::remove_file(&path).unwrap();
        let days: Vec<(String, i64)> = counted
            .iter()
            .map(|group| {
                (
                    group["day"].as_str().unwrap().to_owned(),
                    group["count"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            days,
            vec![("2026-10-01".to_owned(), 2), ("2026-10-02".to_owned(), 1)]
        );
    }
}
