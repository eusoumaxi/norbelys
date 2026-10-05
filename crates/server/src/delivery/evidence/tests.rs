//! Store tests of the evidence recorder, against real PostgreSQL: its own job through the job
//! runner (the complaint-rate breaker, `connection.complaint_rate`), and the events it tells
//! about many messages at once.

use serde_json::{Value, json};
use uuid::Uuid;

use super::ComplaintRate;
use crate::domain::ids::{Attempt, Connection, Id, WorkspaceId};
use crate::jobs::runner::Harness;
use crate::jobs::{self, Queue, Registry};
use crate::testing::{SenderSpec, TestDb};
use crate::webhooks::EventType;

/// A relay of `workspace` that accepted `sent` messages today and drew `complaints` authenticated
/// complaints about one of its messages, recorded now; answers its connection.
async fn connection(
    test: &TestDb,
    workspace: WorkspaceId,
    address: &str,
    sent: i32,
    complaints: i32,
) -> Id<Connection> {
    let sender = test.sender(workspace, &SenderSpec::relay(address)).await;
    let message = test
        .direct_message(workspace, &sender, &["reader@example.test"], 0)
        .await;
    sqlx::query(
        "INSERT INTO connection_usage (workspace_id, connection_id, day, used)
         VALUES ($1, $2, (now() AT TIME ZONE 'UTC')::date, $3)",
    )
    .bind(workspace.uuid())
    .bind(sender.connection.uuid())
    .bind(sent)
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO delivery_events (workspace_id, message_id, recipient_ref, source, source_event_id, kind,
                                      category, confidence, observed_at)
         SELECT $1, $2, 'unknown', 'provider_webhook', 'complaint-' || n, 'complaint', 'complaint',
                'authenticated', now()
           FROM generate_series(1, $3) n",
    )
    .bind(workspace.uuid())
    .bind(message.uuid())
    .bind(complaints)
    .execute(test.system.pool())
    .await
    .unwrap();
    sender.connection
}

/// `connection`'s status and its detail.
async fn health(test: &TestDb, connection: Id<Connection>) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, status_detail FROM connections WHERE id = $1")
        .bind(connection.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap()
}

/// The status and detail of each `connection.health_changed` recorded about `connection`.
async fn health_changes(
    test: &TestDb,
    connection: Id<Connection>,
) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT payload -> 'data' ->> 'status', payload -> 'data' ->> 'status_detail'
           FROM outbox_events WHERE type = 'connection.health_changed' AND subject_id = $1",
    )
    .bind(connection.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap()
}

/// The complaint-rate breaker disables a connection once its complaints over the window reach
/// 0.3 % of what it sent, and not before: 3 complaints against 900 accepted (0.33 %) disable it
/// through the health table, with the counts in its detail and `connection.health_changed`
/// told, so sending stops until a person verifies it again; 2 against 900 (below the minimum
/// count) and 3 against 2,000 (0.15 %) leave it active and tell nothing. A breaker that tripped
/// early would stop healthy senders on noise; one that never tripped would let a sender keep
/// mailing a list that providers are about to block.
#[tokio::test]
async fn the_complaint_rate_disables_a_connection_at_the_rate_and_not_before() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await.id;
    let tripped = connection(&test, acme, "tripped@acme.test", 900, 3).await;
    let few = connection(&test, acme, "few@acme.test", 900, 2).await;
    let diluted = connection(&test, acme, "diluted@acme.test", 2_000, 3).await;

    let mut tx = test.system.begin().await.unwrap();
    for connection in [tripped, few, diluted] {
        jobs::enqueue(&mut tx, acme, &ComplaintRate { connection }, None)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let mut registry = Registry::default();
    registry.register::<ComplaintRate>().unwrap();
    let runner = Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        http::Extensions::new(),
        "complaint-rate-test",
    );
    // The maintenance lane of a workspace runs one job at a time: one claim per count.
    for _ in 0..3 {
        let ran = runner.run_once(Queue::Maintenance, 1).await;
        assert_eq!(ran.len(), 1);
        assert_eq!(ran[0].1, "done");
    }
    assert!(runner.run_once(Queue::Maintenance, 1).await.is_empty());

    let (status, detail) = health(&test, tripped).await;
    assert_eq!(status, "disabled");
    let detail = detail.expect("the breaker's detail");
    assert!(
        detail.contains("reported 3 messages") && detail.contains("against 900 it sent"),
        "{detail}"
    );
    assert_eq!(
        health_changes(&test, tripped).await,
        [("disabled".to_owned(), Some(detail))]
    );
    for connection in [few, diluted] {
        assert_eq!(health(&test, connection).await, ("active".to_owned(), None));
        assert!(test.told(acme, connection.uuid()).await.is_empty());
    }
}

/// Telling many messages at once (`tell_all`, what ending many enrollments writes for their
/// cancelled messages) gives each event its own message's latest attempt, or none for a message
/// never attempted, in the order given. A consumer fetches the attempt an event names to learn
/// what decided its message, so a batch must never pair a message with another's attempt or an
/// older one of its own.
#[tokio::test]
async fn telling_many_messages_names_each_ones_latest_attempt() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await.id;
    let sender = test
        .sender(acme, &SenderSpec::relay("relay@acme.test"))
        .await;
    let tried = test
        .direct_message(acme, &sender, &["tried@example.test"], 0)
        .await;
    let fresh = test
        .direct_message(acme, &sender, &["fresh@example.test"], 0)
        .await;
    sqlx::query(
        "INSERT INTO connection_usage (workspace_id, connection_id, day)
         VALUES ($1, $2, (now() AT TIME ZONE 'UTC')::date)",
    )
    .bind(acme.uuid())
    .bind(sender.connection.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();
    let attempts: Vec<(i32, Uuid)> = sqlx::query_as(
        "INSERT INTO attempts (workspace_id, message_id, attempt_number, connection_id, reserved_day, recipient_count,
                               quota_state, lease_owner, finished_at, outcome, phase, smtp_code)
         SELECT $1, $2, n, $3, (now() AT TIME ZONE 'UTC')::date, 1, 'released', 'test', now(), 'transient', 'rcpt_to', 451
           FROM generate_series(1, 2) n
         RETURNING attempt_number, id",
    )
    .bind(acme.uuid())
    .bind(tried.uuid())
    .bind(sender.connection.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    let latest = attempts
        .iter()
        .find(|(number, _)| *number == 2)
        .map(|(_, id)| Id::<Attempt>::from_uuid(*id))
        .unwrap();

    let mut tx = test.worker.begin_in(acme).await.unwrap();
    super::tell_all(
        &mut tx,
        acme,
        EventType::MessageCancelled,
        &[fresh.uuid(), tried.uuid()],
        None,
        crate::process::now(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let told: Vec<(Value, Value)> = sqlx::query_scalar::<_, Value>(
        "SELECT payload -> 'data' FROM outbox_events
          WHERE workspace_id = $1 AND type = 'message.cancelled' ORDER BY id",
    )
    .bind(acme.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|data| (data["message_id"].clone(), data["attempt_id"].clone()))
    .collect();
    assert_eq!(
        told,
        [(json!(fresh), Value::Null), (json!(tried), json!(latest))]
    );
}
