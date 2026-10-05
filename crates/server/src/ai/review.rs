//! The human review queue of inbound messages, as metrics exported every minute: how many
//! reviews were asked in the last day, and the median time people took to decide one.
//!
//! A message goes to a person when the inbox's rules cannot act on what it proposes alone (a
//! suppression or an address change a person must confirm), and when an AI verdict is below the
//! workspace's confidence threshold or drawn into the review sample. The queue's arrivals and its
//! service time are the budget of that work: a threshold set too high, or a sample too large,
//! shows here before it overwhelms anyone, and the threshold is set against it. The figures are
//! read from the rows themselves, so they cover every workspace and every cause, and survive
//! restarts:
//!
//! - `norbelys_review_requested_last_day`: reviews asked in the last 24 hours;
//! - `norbelys_review_median_seconds`: the median time from a review's request to its decision,
//!   over the reviews decided in the last 7 days (not recorded while there is none).
//!
//! The worker reads them as the scheduler role, which sees only the two timestamps of the
//! messages that asked for a review, each behind a partial index: never a message.

use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::metrics::Gauge;

use crate::db::{self, Database};
use crate::process::Shutdown;

/// How often the figures are read.
const EVERY: Duration = Duration::from_secs(60);

static REQUESTED: LazyLock<Gauge<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_gauge("norbelys_review_requested_last_day")
        .with_description("Reviews of inbound messages asked of people in the last 24 hours.")
        .build()
});

static MEDIAN: LazyLock<Gauge<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_gauge("norbelys_review_median_seconds")
        .with_unit("s")
        .with_description(
            "Median time from a review's request to a person's decision, over the reviews decided \
             in the last 7 days.",
        )
        .build()
});

/// The review queue's figures, across every workspace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Figures {
    /// Reviews asked in the last 24 hours.
    pub requested: u64,
    /// The median seconds from request to decision of the reviews decided in the last 7 days;
    /// `None` when none was.
    pub median_seconds: Option<f64>,
}

/// Reads the review queue's figures of every workspace, as the scheduler.
///
/// # Errors
///
/// The database refused.
pub async fn read(db: &Database) -> Result<Figures, sqlx::Error> {
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM inbound_messages
                    WHERE review_requested_at >= now() - interval '1 day') AS "requested!",
                  (SELECT percentile_cont(0.5) WITHIN GROUP
                              (ORDER BY extract(epoch FROM reviewed_at - review_requested_at)::float8)
                     FROM inbound_messages
                    WHERE reviewed_at >= now() - interval '7 days') AS "median_seconds""#,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Figures {
        requested: u64::try_from(row.requested).unwrap_or(0),
        median_seconds: row.median_seconds,
    })
}

/// Reads the figures every minute and records them, until the process is asked to stop. A read
/// that fails is logged and tried again a minute later.
pub async fn export(db: Database, mut shutdown: Shutdown) {
    loop {
        match read(&db).await {
            Ok(figures) => {
                REQUESTED.record(figures.requested, &[]);
                if let Some(median) = figures.median_seconds {
                    MEDIAN.record(median, &[]);
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "the review queue's figures could not be read");
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
    use super::read;
    use crate::testing::{SenderSpec, TestDb};

    /// The figures cover every workspace, read through the scheduler's narrow view: the reviews
    /// asked in the last day (one asked earlier, and a message that asked for none, are not
    /// counted), and the median time to review of those decided in the last week (one decided
    /// earlier is not). Without the cross-workspace view, the queue's budget would be one
    /// tenant's share of it.
    #[tokio::test]
    async fn the_review_queue_is_read_across_workspaces() {
        let test = TestDb::new().await;
        // (workspace, hours since the review was asked, hours since it was decided)
        let reviews = [
            ("acme", vec![(Some(2), Some(1)), (Some(30), Some(27))]),
            (
                "globex",
                vec![(Some(5), None), (Some(240), Some(216)), (None, None)],
            ),
        ];
        for (slug, rows) in reviews {
            let workspace = test.workspace(slug).await.id;
            let sender = test
                .sender(workspace, &SenderSpec::relay(&format!("hello@{slug}.test")))
                .await;
            let binding: uuid::Uuid = sqlx::query_scalar(
                "INSERT INTO receive_bindings (workspace_id, connection_id) VALUES ($1, $2) RETURNING id",
            )
            .bind(workspace.uuid())
            .bind(sender.connection.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
            for (index, (asked, decided)) in rows.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO inbound_messages (workspace_id, receive_binding_id, connection_id, transport_identity,
                                                   transport_key, received_at, classification, classification_source,
                                                   evidence, review_requested_at, reviewed_at, review_decision)
                     VALUES ($1, $2, $3, '{}', $4, now(), 'unknown', 'rules', 'test',
                             now() - make_interval(hours => $5), now() - make_interval(hours => $6),
                             CASE WHEN $6::int IS NULL THEN NULL ELSE 'dismissed' END)",
                )
                .bind(workspace.uuid())
                .bind(binding)
                .bind(sender.connection.uuid())
                .bind(format!("test:{index}"))
                .bind(*asked)
                .bind(*decided)
                .execute(test.system.pool())
                .await
                .unwrap();
            }
        }
        let figures = read(&test.worker).await.unwrap();
        assert_eq!(figures.requested, 2);
        let median = figures.median_seconds.unwrap();
        assert!((median - 7_200.0).abs() < 1e-6, "{median}");
    }
}
