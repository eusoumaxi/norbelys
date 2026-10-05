//! `delivery.expire`'s store and gate test, against real PostgreSQL through the job runner: the
//! fan-out over workspaces and what it fails, and what it leaves alone.

use super::DeliveryExpire;
use crate::domain::ids::{Id, Message};
use crate::jobs::runner::Harness;
use crate::jobs::{self, Queue, Registry, SYSTEM_WORKSPACE};
use crate::telemetry::Event;
use crate::telemetry::capture::Capture;
use crate::testing::{SenderSpec, TestDb};

/// Sets `message`'s deadline `seconds` from now (negative: passed).
async fn deadline(test: &TestDb, message: Id<Message>, seconds: i64) {
    sqlx::query("UPDATE delivery_queue SET deadline_at = now() + make_interval(secs => $2) WHERE message_id = $1")
        .bind(message.uuid())
        .bind(seconds)
        .execute(test.system.pool())
        .await
        .unwrap();
}

/// `message`'s state and whether it still has a queue row.
async fn fate(test: &TestDb, message: Id<Message>) -> (String, Option<String>) {
    sqlx::query_as(
        "SELECT m.state, q.state FROM messages m
           LEFT JOIN delivery_queue q ON q.workspace_id = m.workspace_id AND q.message_id = m.id
          WHERE m.id = $1",
    )
    .bind(message.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap()
}

/// A queued message whose connection is paused, held by its breaker or waiting for a person is
/// never claimed, so no Start fails it at its deadline: `delivery.expire`, enqueued in the
/// system workspace as its schedule does, visits every workspace with such rows and fails each
/// queued message past its deadline (`failed`, its queue row deleted, the holds it caused
/// resolved as `expired`, `message.failed` told with the category `expired`, and the error-level
/// `delivery.failure` event emitted once, after its chunk committed). A message whose deadline is
/// still ahead stays queued, and one a sender holds is left to its Start, which fails it there.
#[tokio::test]
async fn delivery_expire_fails_the_queued_messages_past_their_deadline() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await.id;
    let rival = test.workspace("rival").await.id;
    let sender = test
        .sender(acme, &SenderSpec::relay("relay@acme.test"))
        .await;
    let claimed = test
        .direct_message(acme, &sender, &["c@example.test"], -60)
        .await;
    assert_eq!(
        test.claimed(acme, &sender, "sender-a").await[0].message,
        claimed
    );
    let expired = test
        .direct_message(acme, &sender, &["e@example.test"], 3_600)
        .await;
    let ahead = test
        .direct_message(acme, &sender, &["a@example.test"], 3_600)
        .await;
    let other = test
        .sender(rival, &SenderSpec::relay("relay@rival.test"))
        .await;
    let elsewhere = test
        .direct_message(rival, &other, &["r@example.test"], 3_600)
        .await;
    for (message, seconds) in [
        (claimed, -1),
        (expired, -1),
        (ahead, 3_600),
        (elsewhere, -1),
    ] {
        deadline(&test, message, seconds).await;
    }
    sqlx::query(
        "INSERT INTO recipient_holds (workspace_id, message_id, email, reason, observed_at, review_after)
         VALUES ($1, $2, 'e@example.test', 'mailbox_full', now(), now() + interval '1 hour')",
    )
    .bind(acme.uuid())
    .bind(expired.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();

    let mut registry = Registry::default();
    registry.register::<DeliveryExpire>().unwrap();
    let runner = Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        http::Extensions::new(),
        "expire-test",
    );
    let mut tx = test.system.begin().await.unwrap();
    jobs::enqueue(&mut tx, SYSTEM_WORKSPACE, &DeliveryExpire {}, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let capture = Capture::default();
    let _guard = capture.install();
    let ran = runner.run_once(Queue::Maintenance, 1).await;
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].1, "done");
    let mut failures: Vec<(String, String, String)> = capture
        .named_since(0, Event::DeliveryFailure.as_str())
        .iter()
        .map(|event| {
            (
                event.field("message_id").to_owned(),
                event.field("state").to_owned(),
                event.field("category").to_owned(),
            )
        })
        .collect();
    failures.sort();
    let mut reported = vec![
        (
            expired.to_string(),
            "failed".to_owned(),
            "expired".to_owned(),
        ),
        (
            elsewhere.to_string(),
            "failed".to_owned(),
            "expired".to_owned(),
        ),
    ];
    reported.sort();
    assert_eq!(failures, reported);

    for (workspace, message) in [(acme, expired), (rival, elsewhere)] {
        assert_eq!(fate(&test, message).await, ("failed".to_owned(), None));
        let told: Vec<(String, serde_json::Value)> = sqlx::query_as(
            "SELECT type, payload FROM outbox_events WHERE workspace_id = $1 AND subject_id = $2",
        )
        .bind(workspace.uuid())
        .bind(message.uuid())
        .fetch_all(test.system.pool())
        .await
        .unwrap();
        assert_eq!(told.len(), 1);
        assert_eq!(told[0].0, "message.failed");
        assert_eq!(told[0].1["data"]["category"], "expired", "{}", told[0].1);
    }
    let resolution: Option<String> =
        sqlx::query_scalar("SELECT resolution FROM recipient_holds WHERE message_id = $1")
            .bind(expired.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(resolution.as_deref(), Some("expired"));
    assert_eq!(
        fate(&test, ahead).await,
        ("queued".to_owned(), Some("queued".to_owned()))
    );
    assert_eq!(
        fate(&test, claimed).await,
        ("claimed".to_owned(), Some("claimed".to_owned()))
    );
}
