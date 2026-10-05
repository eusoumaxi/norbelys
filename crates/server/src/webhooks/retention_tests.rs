//! Gate test of the retention lock between the reopening of a webhook delivery and a change of the
//! outbox's retention.
//!
//! A delivery may be reopened (a manual retry, a replay) only inside the replay window, which the
//! outbox's retention fixes, and the archive examines the partitions older than that window. So
//! the reopening holds the retention lock (an advisory lock keyed on `partition_policies`) shared
//! for its whole transaction, and a change of a retention takes it exclusively (a statement trigger
//! on `partition_policies` takes it, so the test's change takes no lock of its own): the change waits
//! for every running reopening, which therefore never reopens a row in a partition that a shorter
//! retention would hand to the archive.

use std::time::{Duration, Instant};

use serde_json::json;
use uuid::Uuid;

use super::EventType;
use super::deliver::{self, Reopen};
use super::endpoints::{self, NewEndpoint};
use super::outbox::{self, Event};
use super::tests::{harness, relay};
use crate::domain::ids::{Id, WebhookDelivery};
use crate::testing::{self, TestDb};

/// A change of the outbox's retention, run while the api's reopening of a delivery is still in its
/// transaction, waits for it: the change's exclusive retention lock stays blocked by the
/// reopening's shared one and commits only after the reopening did, and the delivery is reopened
/// (`pending`, due now). Without the wait, a retention shortened during a reopening could let the
/// archive seal the partition holding the row being reopened.
#[tokio::test]
async fn a_retention_change_waits_for_a_running_reopening() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let mut tx = test.app.begin_in(acme.id).await.unwrap();
    endpoints::create(
        &mut tx,
        &testing::keys(),
        acme.id,
        &NewEndpoint {
            url: "https://hooks.acme.example/norbelys",
            event_types: &[EventType::MessageSent],
            enabled: true,
        },
    )
    .await
    .unwrap();
    outbox::record(
        &mut tx,
        acme.id,
        Event {
            kind: EventType::MessageSent,
            subject_type: "message",
            subject_id: Uuid::now_v7(),
            data: json!({ "message_id": "msg_0190f8a2b4c87a10b6d2e4f6a8c0e2f4" }),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    relay(&test, &harness(&test, false)).await;
    let failed: Uuid = sqlx::query_scalar(
        "UPDATE webhook_deliveries SET state = 'failed', attempt = 10 RETURNING id",
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    let delivery = Id::<WebhookDelivery>::from_uuid(failed);

    let mut reopening = test.app.begin_in(acme.id).await.unwrap();
    assert_eq!(
        deliver::reopen(&mut reopening, acme.id, Reopen::Delivery(delivery))
            .await
            .unwrap(),
        [delivery]
    );
    let system = test.system.clone();
    let change = tokio::spawn(async move {
        let started = Instant::now();
        let mut tx = system.begin().await.unwrap();
        // No explicit lock: the table's statement trigger takes it exclusively.
        sqlx::query(
            "UPDATE partition_policies SET retention = interval '8 days' WHERE table_name IN ('outbox_events', 'webhook_deliveries')",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        started.elapsed()
    });
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(
        !change.is_finished(),
        "the retention change did not wait for the reopening"
    );
    reopening.commit().await.unwrap();
    let waited = change.await.unwrap();
    assert!(
        waited >= Duration::from_secs(1),
        "the change waited only {waited:?}"
    );

    let (state, due): (String, bool) = sqlx::query_as(
        "SELECT state, next_attempt_at <= now() FROM webhook_deliveries WHERE id = $1",
    )
    .bind(delivery.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!((state.as_str(), due), ("pending", true));
    let changed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM partition_policies
          WHERE table_name IN ('outbox_events', 'webhook_deliveries') AND retention = interval '8 days'",
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(changed, 2);
}
