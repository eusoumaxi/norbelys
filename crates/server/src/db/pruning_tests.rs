//! Store tests of partition pruning: a read of a history table bounded by a period's id skips the
//! partitions older than the bound, whatever form the bound takes, so a query over the last days
//! costs the same however much history is still online.
//!
//! The history tables (messages, attempts, delivery events, the outbox and its deliveries,
//! receipts, engagement, increments, the Message-ID directory) are range-partitioned by period on
//! a UUIDv7 key, whose first 48 bits are the unix millisecond. `uuidv7_boundary(at)` is the
//! smallest key of an instant, so "the last seven days" is
//! `key >= uuidv7_boundary(now() - interval '7 days')`. PostgreSQL prunes partitions in two places
//! (<https://www.postgresql.org/docs/current/ddl-partitioning.html#DDL-PARTITION-PRUNING>):
//!
//! - **while planning**, when the bound is a constant: the older partitions never enter the plan;
//! - **when the executor starts**, when the bound is known only then (a stable expression such as
//!   one over `now()`, or a parameter of a generic prepared plan): the plan lists every partition
//!   and the older ones are removed before anything is read, which `EXPLAIN` reports as
//!   `Subplans Removed`.
//!
//! The application passes its bounds as parameters, and PostgreSQL may plan such a statement
//! once for any value (a generic plan), where only the second kind of pruning applies; so both
//! kinds are proven. The test creates a leaf sixty days old beside today's for every table, so an
//! old partition exists and must not be scanned, and reads the tables and their partition keys
//! from the partition policies and the catalogue, so a history table added later is covered
//! without a change here.

use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

use crate::testing::TestDb;

/// The history tables partitioned by period on a UUIDv7 key, each with its key's column.
async fn history_tables(connection: &mut PgConnection) -> Vec<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT p.table_name, a.attname::text
           FROM partition_policies p
           JOIN pg_class c ON c.relname = p.table_name AND c.relnamespace = 'public'::regnamespace
           JOIN pg_partitioned_table t ON t.partrelid = c.oid
           JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = t.partattrs[0]
          WHERE p.key_kind = 'uuidv7'
          ORDER BY p.table_name",
    )
    .fetch_all(&mut *connection)
    .await
    .unwrap()
}

/// The plan `EXPLAIN` prints for `statement` (our own text, never input), its lines joined.
async fn explain(connection: &mut PgConnection, statement: String) -> String {
    sqlx::query_scalar::<_, String>(AssertSqlSafe(statement.clone()))
        .fetch_all(&mut *connection)
        .await
        .unwrap_or_else(|error| panic!("{statement}: {error}"))
        .join("\n")
}

/// Fails unless `plan` scans today's leaf `live` and not the old leaf `old`, and reports pruning
/// at the executor's start exactly when the bound is known only then (`at_start`).
fn check(table: &str, bound: &str, plan: &str, old: &str, live: &str, at_start: bool) {
    assert!(
        !plan.contains(old),
        "{table}, bounded by {bound}: the leaf {old} is scanned\n{plan}"
    );
    assert!(
        plan.contains(live),
        "{table}, bounded by {bound}: today's leaf {live} is not scanned\n{plan}"
    );
    assert_eq!(
        plan.contains("Subplans Removed"),
        at_start,
        "{table}, bounded by {bound}: pruned at the executor's start? expected {at_start}\n{plan}"
    );
}

/// A seven-day id bound reads only the live partitions of every history table: a constant bound
/// leaves the sixty-day-old leaf out of the plan, and a stable expression or a parameter of a
/// generic plan has it removed when the executor starts, before any row is read. A query over the
/// recent past must never visit history the archive has not yet dropped, or its cost would grow
/// with the retention.
#[tokio::test]
async fn a_seven_day_id_bound_scans_only_the_live_partitions() {
    let test = TestDb::new().await;
    let mut connection = test.system.pool().acquire().await.unwrap();
    let tables = history_tables(&mut connection).await;
    assert!(
        tables
            .iter()
            .any(|(table, key)| table == "messages" && key == "id"),
        "{tables:?}"
    );
    for (table, key) in &tables {
        let old: String = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT ensure_partition('{table}', now() - interval '60 days')"
        )))
        .fetch_one(&mut *connection)
        .await
        .unwrap();
        let live: String = sqlx::query_scalar(
            "SELECT l.name FROM partition_leaves l WHERE l.parent = $1 AND l.lower <= now() AND now() < l.upper",
        )
        .bind(table.as_str())
        .fetch_one(&mut *connection)
        .await
        .unwrap();
        let bound: Uuid = sqlx::query_scalar("SELECT uuidv7_boundary(now() - interval '7 days')")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        let read = format!("SELECT 1 FROM {table} WHERE {key} >= ");

        let planned = explain(
            &mut connection,
            format!("EXPLAIN (COSTS OFF) {read}'{bound}'::uuid"),
        )
        .await;
        check(table, "a constant", &planned, &old, &live, false);

        let started = explain(
            &mut connection,
            format!("EXPLAIN (COSTS OFF) {read}uuidv7_boundary(now() - interval '7 days')"),
        )
        .await;
        check(table, "a stable expression", &started, &old, &live, true);

        sqlx::raw_sql(AssertSqlSafe(format!(
            "SET plan_cache_mode = force_generic_plan; PREPARE pruning_{table}(uuid) AS {read}$1"
        )))
        .execute(&mut *connection)
        .await
        .unwrap();
        let generic = explain(
            &mut connection,
            format!("EXPLAIN (COSTS OFF) EXECUTE pruning_{table}('{bound}')"),
        )
        .await;
        sqlx::raw_sql(AssertSqlSafe(format!(
            "DEALLOCATE pruning_{table}; RESET plan_cache_mode"
        )))
        .execute(&mut *connection)
        .await
        .unwrap();
        check(
            table,
            "a parameter of a generic plan",
            &generic,
            &old,
            &live,
            true,
        );
    }
}
