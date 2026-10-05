//! The gauges read from the database rather than counted in a process: the state of the whole
//! deployment (every workspace, every replica's work), read by the worker every [`EVERY`] through
//! its system login, which sees every workspace's rows. One owner reads them, so they are neither
//! multiplied by the senders' and inboxes' replicas nor limited to what one replica sees.
//!
//! - `norbelys_delivery_scheduled_backlog_age_seconds`: how long the oldest queued message has
//!   been due, whatever holds it back.
//! - `norbelys_delivery_eligible_backlog_age_seconds`: the same among messages that could be
//!   claimed now, as the sender's turn sees them: an active, unpaused connection with no budget
//!   wait, no open breaker and no paused quota scope; on a paced sender, cold mail only once its
//!   pacing clock is due (and due since the later of the two instants), and nothing while another
//!   of its messages is in progress. Windows move the clock and the rows themselves, so a message
//!   waiting for its window is not due. A growing eligible age with healthy providers means the
//!   senders do not keep up; a growing scheduled age alone means work is held back on purpose.
//! - `norbelys_delivery_uncertain_open`: messages whose submission may have been accepted and is
//!   not settled yet (`uncertain`), never retried automatically.
//! - `norbelys_delivery_budget_utilisation{provider}`: today's (UTC) reserved and used messages
//!   over the daily limits of the active connections, by provider.
//! - `norbelys_inbox_poll_lag_seconds{provider}`: the longest time since an enabled binding of an
//!   active connection was last read, among bindings outside failure backoff (a failing binding is
//!   its connection's health, not a lag), by provider; a binding never read counts from when it
//!   became due. With a five-minute poll interval it sits under five minutes.
//! - `norbelys_partition_count{table}` and `norbelys_partition_expected{table}`: the partition
//!   leaves each partitioned table holds in the database, and how many its policy should keep
//!   online (`domain::telemetry::expected_leaves`: its online window, the current leaf, the leaves
//!   made ahead and one waiting for the archive). `archive-debt` alerts when the first is more
//!   than twice the second: leaves that do not leave, and every leaf costs planning time.
//! - `norbelys_restore_drill_last_success_timestamp_seconds`: when the newest restore drill an
//!   operator recorded (`admin restore-drill`) completed, and
//!   `norbelys_key_rotation_last_success_timestamp_seconds`: when the newest key that signs
//!   workspace tokens was made (`admin keys rotate`, monthly). Both schedules are monthly; with
//!   no record at all the gauge reads 0, so `restore-drill` and `key-rotation` fire for a
//!   schedule never kept as for one missed.
//!
//! A provider without connections or bindings reports 0, so a gauge never keeps a value that
//! stopped being true. Each statement reads the live queue and the bindings, both small and
//! indexed by their state, the policies with their leaves, and the newest drill and key, once a
//! minute.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Gauge;
use strum::IntoEnumIterator as _;

use crate::db::Database;
use crate::domain::senders::Provider;
use crate::domain::telemetry::expected_leaves;
use crate::process::Shutdown;

/// How often the gauges are read.
const EVERY: Duration = Duration::from_secs(60);

static SCHEDULED: LazyLock<Gauge<f64>> = LazyLock::new(|| {
    seconds(
        "norbelys_delivery_scheduled_backlog_age_seconds",
        "How long the oldest queued message has been due, whatever holds it back.",
    )
});

static ELIGIBLE: LazyLock<Gauge<f64>> = LazyLock::new(|| {
    seconds(
        "norbelys_delivery_eligible_backlog_age_seconds",
        "How long the oldest message that could be claimed now has been due.",
    )
});

static UNCERTAIN: LazyLock<Gauge<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_gauge("norbelys_delivery_uncertain_open")
        .with_description("Messages whose submission may have been accepted, not settled yet.")
        .build()
});

static BUDGET: LazyLock<Gauge<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_gauge("norbelys_delivery_budget_utilisation")
        .with_description(
            "Today's reserved and used messages over the active connections' daily limits.",
        )
        .build()
});

static INBOX_LAG: LazyLock<Gauge<f64>> = LazyLock::new(|| {
    seconds(
        "norbelys_inbox_poll_lag_seconds",
        "The longest time since an enabled binding outside failure backoff was last read.",
    )
});

static LEAVES: LazyLock<Gauge<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_gauge("norbelys_partition_count")
        .with_description("Partition leaves still in the database, by partitioned table.")
        .build()
});

static EXPECTED: LazyLock<Gauge<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_gauge("norbelys_partition_expected")
        .with_description(
            "Partition leaves each table's policy should keep online, by partitioned table.",
        )
        .build()
});

static RESTORE_DRILL: LazyLock<Gauge<f64>> = LazyLock::new(|| {
    seconds(
        "norbelys_restore_drill_last_success_timestamp_seconds",
        "When the newest recorded restore drill completed, in Unix seconds; 0 for none.",
    )
});

static KEY_ROTATION: LazyLock<Gauge<f64>> = LazyLock::new(|| {
    seconds(
        "norbelys_key_rotation_last_success_timestamp_seconds",
        "When the newest key signing workspace tokens was made, in Unix seconds; 0 for none.",
    )
});

fn seconds(name: &'static str, description: &'static str) -> Gauge<f64> {
    opentelemetry::global::meter("norbelys")
        .f64_gauge(name)
        .with_unit("s")
        .with_description(description)
        .build()
}

/// One reading of the gauges.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Fleet {
    /// Seconds the oldest due message has waited.
    pub scheduled_age: f64,
    /// Seconds the oldest claimable message has waited.
    pub eligible_age: f64,
    /// Messages `uncertain`.
    pub uncertain: i64,
    /// Today's share of the daily limits, by provider.
    pub budget: HashMap<Provider, f64>,
    /// The longest time since a binding was read, by provider.
    pub inbox_lag: HashMap<Provider, f64>,
    /// Every partitioned table's leaves, in table order.
    pub partitions: Vec<Partitions>,
    /// When the newest restore drill completed, in Unix seconds.
    pub restore_drill_at: Option<f64>,
    /// When the newest key signing workspace tokens was made, in Unix seconds.
    pub key_rotation_at: Option<f64>,
}

/// One partitioned table's leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Partitions {
    /// The partitioned table.
    pub table: String,
    /// Its leaves still in the database.
    pub leaves: u64,
    /// The leaves its policy should keep online.
    pub expected: u64,
}

/// Reads the gauges through `system`, the worker's system login (see the module).
///
/// # Errors
///
/// The database refused.
pub(crate) async fn read(system: &Database) -> Result<Fleet, sqlx::Error> {
    let mut tx = system.begin().await?;
    let backlog = sqlx::query!(
        r#"SELECT coalesce((SELECT extract(epoch FROM now() - min(q.run_at))
                              FROM delivery_queue q WHERE q.state = 'queued' AND q.run_at <= now()), 0)::float8 AS "scheduled!",
                  coalesce((SELECT extract(epoch FROM now() - min(CASE WHEN q.paced AND c.send_interval_minutes IS NOT NULL
                                                                     THEN greatest(q.run_at, c.next_send_at)
                                                                     ELSE q.run_at END))
                              FROM delivery_queue q
                              JOIN connections c ON (c.workspace_id, c.id) = (q.workspace_id, q.connection_id)
                              LEFT JOIN quota_scopes s ON (s.workspace_id, s.id) = (c.workspace_id, c.quota_scope_id)
                             WHERE q.state = 'queued' AND q.run_at <= now()
                               AND c.status = 'active' AND NOT c.paused
                               AND (c.next_claim_at IS NULL OR c.next_claim_at <= now())
                               AND (c.paused_until IS NULL OR c.paused_until <= now())
                               AND (s.paused_until IS NULL OR s.paused_until <= now())
                               AND (NOT q.paced OR c.send_interval_minutes IS NULL OR c.next_send_at <= now())
                               AND (c.send_interval_minutes IS NULL
                                    OR NOT EXISTS (SELECT 1 FROM delivery_queue p
                                                    WHERE (p.workspace_id, p.connection_id) = (c.workspace_id, c.id)
                                                      AND p.state <> 'queued'))), 0)::float8 AS "eligible!",
                  (SELECT count(*) FROM messages WHERE state = 'uncertain') AS "uncertain!""#
    )
    .fetch_one(&mut *tx)
    .await?;
    let budget = sqlx::query!(
        r#"SELECT c.provider AS "provider!",
                  (sum(coalesce(u.used, 0) + coalesce(u.reserved, 0))::float8 / sum(c.daily_limit)::float8) AS "utilisation!"
             FROM connections c
             LEFT JOIN connection_usage u
                    ON (u.workspace_id, u.connection_id) = (c.workspace_id, c.id) AND u.day = (now() AT TIME ZONE 'UTC')::date
            WHERE c.status = 'active' AND NOT c.paused
            GROUP BY c.provider"#
    )
    .fetch_all(&mut *tx)
    .await?;
    let inbox = sqlx::query!(
        r#"SELECT c.provider AS "provider!",
                  extract(epoch FROM now() - min(coalesce(b.polled_at, b.next_poll_at)))::float8 AS "lag!"
             FROM receive_bindings b
             JOIN connections c ON (c.workspace_id, c.id) = (b.workspace_id, b.connection_id)
            WHERE b.enabled AND b.failures = 0 AND c.status = 'active'
            GROUP BY c.provider"#
    )
    .fetch_all(&mut *tx)
    .await?;
    let partitions = sqlx::query!(
        r#"SELECT p.table_name AS "table!",
                  extract(epoch FROM p.retention)::int8 AS "retention!",
                  extract(epoch FROM p.period)::int8 AS "period!",
                  (SELECT count(*) FROM partition_leaves l
                    WHERE l.parent = p.table_name AND l.dropped_at IS NULL) AS "leaves!"
             FROM partition_policies p
            ORDER BY p.table_name"#
    )
    .fetch_all(&mut *tx)
    .await?;
    let schedules = sqlx::query!(
        r#"SELECT (SELECT extract(epoch FROM max(completed_at)) FROM restore_drills)::float8 AS restore_drill,
                  (SELECT extract(epoch FROM max(created_at)) FROM signing_keys)::float8 AS key_rotation"#
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let duration = |secs: i64| Duration::from_secs(u64::try_from(secs).unwrap_or(0));
    Ok(Fleet {
        scheduled_age: backlog.scheduled.max(0.0),
        eligible_age: backlog.eligible.max(0.0),
        uncertain: backlog.uncertain,
        budget: budget
            .into_iter()
            .filter_map(|row| Some((row.provider.parse::<Provider>().ok()?, row.utilisation)))
            .collect(),
        inbox_lag: inbox
            .into_iter()
            .filter_map(|row| Some((row.provider.parse::<Provider>().ok()?, row.lag.max(0.0))))
            .collect(),
        partitions: partitions
            .into_iter()
            .map(|row| Partitions {
                expected: expected_leaves(duration(row.retention), duration(row.period)),
                leaves: u64::try_from(row.leaves).unwrap_or(0),
                table: row.table,
            })
            .collect(),
        restore_drill_at: schedules.restore_drill,
        key_rotation_at: schedules.key_rotation,
    })
}

/// Records `fleet`; a provider it does not name records 0.
fn record(fleet: &Fleet) {
    SCHEDULED.record(fleet.scheduled_age, &[]);
    ELIGIBLE.record(fleet.eligible_age, &[]);
    UNCERTAIN.record(u64::try_from(fleet.uncertain).unwrap_or(0), &[]);
    for provider in Provider::iter() {
        let label = [KeyValue::new("provider", provider.as_str())];
        BUDGET.record(fleet.budget.get(&provider).copied().unwrap_or(0.0), &label);
        INBOX_LAG.record(
            fleet.inbox_lag.get(&provider).copied().unwrap_or(0.0),
            &label,
        );
    }
    for partitions in &fleet.partitions {
        let label = [KeyValue::new("table", partitions.table.clone())];
        LEAVES.record(partitions.leaves, &label);
        EXPECTED.record(partitions.expected, &label);
    }
    RESTORE_DRILL.record(fleet.restore_drill_at.unwrap_or(0.0), &[]);
    KEY_ROTATION.record(fleet.key_rotation_at.unwrap_or(0.0), &[]);
}

/// Reads and records the gauges every [`EVERY`] until `shutdown`; a failed reading is logged and
/// tried again at the next.
pub(crate) async fn export(system: Database, mut shutdown: Shutdown) {
    loop {
        match read(&system).await {
            Ok(fleet) => record(&fleet),
            Err(error) => {
                tracing::warn!(error = %error, "the deployment's gauges could not be read")
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
    use crate::testing::{SenderSpec, TestDb};

    /// A message due a minute ago on an idle relay is both scheduled and eligible, and its age is
    /// read across workspaces by the system login; a paused connection's message ages in the
    /// scheduled backlog only, so a pause can never look like the senders falling behind, nor
    /// hide stalled work.
    #[tokio::test]
    async fn the_backlog_ages_tell_eligible_from_held_work() {
        let test = TestDb::new().await;
        let ws = test.workspace("acme").await.id;
        let relay = test.sender(ws, &SenderSpec::relay("hello@acme.test")).await;
        test.direct_message(ws, &relay, &["ada@example.com"], -60)
            .await;
        let fleet = read(&test.system).await.unwrap();
        assert!(fleet.scheduled_age >= 59.0, "{fleet:?}");
        assert!(fleet.eligible_age >= 59.0, "{fleet:?}");
        assert_eq!(fleet.uncertain, 0);

        sqlx::query("UPDATE connections SET paused = true WHERE workspace_id = $1")
            .bind(ws.uuid())
            .execute(test.system.pool())
            .await
            .unwrap();
        let fleet = read(&test.system).await.unwrap();
        assert!(fleet.scheduled_age >= 59.0, "{fleet:?}");
        assert!(fleet.eligible_age < 1.0, "{fleet:?}");
    }

    /// Every partitioned table is read with its leaves and the count its policy expects, and a
    /// fresh database holds no more than expected; the newest restore drill and signing key are
    /// read as Unix seconds, and none at all reads `None` (recorded as 0). The `archive-debt`,
    /// `restore-drill` and `key-rotation` alerts compare exactly these.
    #[tokio::test]
    async fn partitions_restore_drills_and_key_rotations_are_read() {
        let test = TestDb::new().await;
        sqlx::query("DELETE FROM signing_keys")
            .execute(test.system.pool())
            .await
            .unwrap();
        let fleet = read(&test.system).await.unwrap();
        assert_eq!(
            (fleet.restore_drill_at, fleet.key_rotation_at),
            (None, None)
        );
        let policies: Vec<String> =
            sqlx::query_scalar("SELECT table_name FROM partition_policies ORDER BY table_name")
                .fetch_all(test.system.pool())
                .await
                .unwrap();
        let tables: Vec<String> = fleet.partitions.iter().map(|p| p.table.clone()).collect();
        assert_eq!(tables, policies);
        for partitions in &fleet.partitions {
            assert!(
                (1..=partitions.expected).contains(&partitions.leaves),
                "{partitions:?}"
            );
        }

        sqlx::query(
            "INSERT INTO restore_drills (completed_at, minutes) VALUES ('2026-08-01T00:00:00Z', 50), ('2026-09-01T00:00:00Z', 42)",
        )
        .execute(test.system.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO signing_keys (kid, algorithm, private_key, public_jwk, created_at)
             VALUES ('old', 'EdDSA', decode('00', 'hex'), '{}', '2029-12-01T00:00:00Z'),
                    ('new', 'EdDSA', decode('00', 'hex'), '{}', '2030-01-01T00:00:00Z')",
        )
        .execute(test.system.pool())
        .await
        .unwrap();
        let fleet = read(&test.system).await.unwrap();
        // 2026-09-01 and 2030-01-01 at midnight UTC.
        assert_eq!(fleet.restore_drill_at, Some(1_788_220_800.0));
        assert_eq!(fleet.key_rotation_at, Some(1_893_456_000.0));
    }
}
