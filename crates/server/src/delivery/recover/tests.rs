//! Recovery's store and gate tests, against real PostgreSQL with the worker login: a lease lost
//! before its Start's marker returns the message to the queue and releases its unit once; one
//! lost after it makes the message `uncertain`, consumes its unit and, on a mailbox, asks for
//! the Sent-folder check.

use super::sweep;
use crate::domain::ids::{Connection, Id};
use crate::testing::{SenderSpec, TestDb};

const OWNER: &str = "sender-a";

/// Expires every lease of `connection`, as a sender that died would leave them.
async fn expire_leases(test: &TestDb, connection: Id<Connection>) {
    sqlx::query("UPDATE delivery_queue SET lease_expires_at = now() - interval '1 second' WHERE connection_id = $1")
        .bind(connection.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
}

/// A sender that stalled after its claim, before any Start: its lease expires, its own late
/// Start then submits nothing, and recovery puts the row
/// back in the queue due when it was, the message `queued`, its attempt `released` and its unit
/// released, once: a second sweep finds nothing and the ledger stays at zero (it could not go
/// below). The connection's budget wait is cleared, since budget was freed.
#[tokio::test]
async fn a_lease_lost_before_its_start_returns_the_message_once() {
    let test = TestDb::new().await;
    let ws = test.workspace("stalled").await.id;
    let sender = test
        .sender(ws, &SenderSpec::mailbox("ada@example.test"))
        .await;
    let message = test
        .direct_message(ws, &sender, &["p@example.test"], -60)
        .await;
    let claimed = test.claimed(ws, &sender, OWNER).await;
    let run_at: crate::domain::time::Timestamp =
        sqlx::query_scalar("SELECT run_at FROM delivery_queue WHERE message_id = $1")
            .bind(message.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    sqlx::query("UPDATE connections SET next_claim_at = now() + interval '1 hour' WHERE id = $1")
        .bind(sender.connection.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    expire_leases(&test, sender.connection).await;
    assert_eq!(
        test.start(ws, &sender, OWNER, &claimed[0]).await,
        crate::delivery::start::Started::Lost,
        "the stalled sender's Start submits nothing"
    );

    let recovered = sweep(&test.worker).await.unwrap();
    assert_eq!((recovered.requeued, recovered.uncertain), (1, 0));
    let row: (String, Option<String>, crate::domain::time::Timestamp, String, String, String, bool) = sqlx::query_as(
        "SELECT q.state, q.lease_owner, q.run_at, m.state, a.outcome, a.quota_state, c.next_claim_at IS NULL
           FROM delivery_queue q JOIN messages m ON m.workspace_id = q.workspace_id AND m.id = q.message_id
           JOIN attempts a ON a.workspace_id = m.workspace_id AND a.message_id = m.id
           JOIN connections c ON c.workspace_id = q.workspace_id AND c.id = q.connection_id
          WHERE q.message_id = $1",
    )
    .bind(message.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "queued".to_owned(),
            None,
            run_at,
            "queued".to_owned(),
            "released".to_owned(),
            "released".to_owned(),
            true
        )
    );
    assert_eq!(test.ledger(sender.connection).await, (0, 0));
    assert_eq!(
        sweep(&test.worker).await.unwrap(),
        super::Recovered::default(),
        "once"
    );
    assert_eq!(test.ledger(sender.connection).await, (0, 0));
    assert!(test.told(ws, message.uuid()).await.is_empty());
}

/// A sender lost after its Start wrote the submission marker may have handed the message to
/// the provider, so recovery never queues it again: the message becomes `uncertain`, its queue
/// row is deleted, its attempt closes `uncertain` with its unit consumed (conservatively),
/// `message.uncertain` is told, and the mailbox's `connection.check` is asked to read the Sent
/// folder for it. A second sweep finds nothing.
#[tokio::test]
async fn a_lease_lost_after_its_start_makes_the_message_uncertain() {
    let test = TestDb::new().await;
    let ws = test.workspace("lost").await.id;
    let sender = test
        .sender(ws, &SenderSpec::mailbox("ada@example.test"))
        .await;
    let message = test
        .direct_message(ws, &sender, &["p@example.test"], -60)
        .await;
    let claimed = test.claimed(ws, &sender, OWNER).await;
    test.begun(ws, &sender, OWNER, &claimed[0]).await;
    expire_leases(&test, sender.connection).await;

    let recovered = sweep(&test.worker).await.unwrap();
    assert_eq!((recovered.requeued, recovered.uncertain), (0, 1));
    let row: (String, String, String, Option<String>, bool) = sqlx::query_as(
        "SELECT m.state, a.outcome, a.quota_state, a.category,
                EXISTS (SELECT 1 FROM delivery_queue q WHERE q.message_id = m.id)
           FROM messages m JOIN attempts a ON a.workspace_id = m.workspace_id AND a.message_id = m.id
          WHERE m.id = $1",
    )
    .bind(message.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "uncertain".to_owned(),
            "uncertain".to_owned(),
            "consumed".to_owned(),
            Some("uncertain".to_owned()),
            false
        )
    );
    assert_eq!(test.ledger(sender.connection).await, (0, 1));
    assert_eq!(test.told(ws, message.uuid()).await, ["message.uncertain"]);
    let checks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE workspace_id = $1 AND kind = 'connection.check' AND payload -> 'connection' = $2",
    )
    .bind(ws.uuid())
    .bind(serde_json::to_value(sender.connection).unwrap())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(checks, 1);
    assert_eq!(
        sweep(&test.worker).await.unwrap(),
        super::Recovered::default(),
        "once"
    );
}
