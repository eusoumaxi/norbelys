//! The reconciliation's store tests, against real PostgreSQL through the job runner and a fake
//! Mailgun events API on the loopback interface: what one run stores, that each event is stored
//! once whoever stored it first, and which connections are never read.

use std::sync::{Arc, Mutex};

use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use uuid::Uuid;

use super::ProviderReconcile;
use crate::domain::ids::{Id, ProviderWebhook, WorkspaceId};
use crate::jobs::{self, Queue, Registry};
use crate::senders::credentials::{self, ApiCredential};
use crate::testing::{self, SenderSpec, TestDb, TestSender};

/// A fake Mailgun events API for `mg.example.com`: the first page holds `items` and links the
/// next, which is empty. Answers its origin and how many pages it served.
async fn fake_mailgun(items: Value) -> (String, Arc<Mutex<usize>>) {
    let served: Arc<Mutex<usize>> = Arc::default();
    let first = Arc::clone(&served);
    let next = Arc::clone(&served);
    let app = axum::Router::new()
        .route(
            "/v3/mg.example.com/events",
            axum::routing::get(move || {
                let served = Arc::clone(&first);
                let items = items.clone();
                async move {
                    *served.lock().unwrap() += 1;
                    axum::Json(json!({
                        "items": items,
                        "paging": {"next": "https://api.mailgun.net/v3/mg.example.com/events/next"}
                    }))
                }
            }),
        )
        .route(
            "/v3/mg.example.com/events/next",
            axum::routing::get(move || {
                let served = Arc::clone(&next);
                async move {
                    *served.lock().unwrap() += 1;
                    axum::Json(json!({ "items": [], "paging": {} }))
                }
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (origin, served)
}

/// A Mailgun event of `kind` with id `id`, as the events API returns it.
fn event(id: &str, kind: &str) -> Value {
    json!({
        "id": id,
        "event": kind,
        "timestamp": 1_790_000_000.25,
        "recipient": "grace@example.org",
        "user-variables": {"norbelys_message_id": Uuid::now_v7().to_string()},
        "delivery-status": {"code": 250, "message": "OK"}
    })
}

/// A Mailgun connection of `workspace` (domain `mg.example.com`), its SMTP password sealed with a
/// private API key when `api` is true, and its active provider webhook.
async fn mailgun(
    test: &TestDb,
    workspace: WorkspaceId,
    api: bool,
) -> (TestSender, Id<ProviderWebhook>) {
    let sender = test
        .sender(
            workspace,
            &SenderSpec {
                provider: "mailgun",
                ..SenderSpec::relay("postmaster@mg.example.com")
            },
        )
        .await;
    let key = ApiCredential {
        id: None,
        secret: SecretString::from("key-private"),
    };
    let sealed = credentials::seal_relay(
        &testing::keys(),
        workspace,
        sender.connection,
        &SecretString::from("smtp-password"),
        api.then_some(&key),
    )
    .unwrap();
    sqlx::query("UPDATE connections SET credential = $2 WHERE id = $1")
        .bind(sender.connection.uuid())
        .bind(sealed)
        .execute(test.system.pool())
        .await
        .unwrap();
    let webhook: Uuid = sqlx::query_scalar(
        "INSERT INTO provider_webhooks (workspace_id, connection_id, provider, name)
         VALUES ($1, $2, 'mailgun', 'mg.example.com') RETURNING id",
    )
    .bind(workspace.uuid())
    .bind(sender.connection.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    (sender, Id::from_uuid(webhook))
}

/// Runs `provider.reconcile` for `webhook` once, its HTTP calls reaching `origin`; answers its
/// outcome.
async fn reconcile(
    test: &TestDb,
    workspace: WorkspaceId,
    webhook: Id<ProviderWebhook>,
    origin: &str,
) -> &'static str {
    let mut tx = test.system.begin().await.unwrap();
    jobs::enqueue(&mut tx, workspace, &ProviderReconcile { webhook }, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut registry = Registry::default();
    registry.register::<ProviderReconcile>().unwrap();
    let runner = testing::provider_runner(test, registry, origin);
    let ran = runner.run_once(Queue::Maintenance, 1).await;
    assert_eq!(ran.len(), 1);
    ran[0].1
}

/// The receipts stored for `webhook`: event id and state, by event id.
async fn receipts(test: &TestDb, webhook: Id<ProviderWebhook>) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT event_id, state FROM webhook_receipts WHERE provider_webhook_id = $1 ORDER BY event_id",
    )
    .bind(webhook.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap()
}

/// One run stores what the events API holds that no one stored yet, through the ingress's path:
/// the recorded kinds only (an open is left out), each new key once with its receipt and the
/// normaliser's job. An event a webhook already delivered is left as the webhook stored it, never
/// quarantined for its differently rendered body, and a second run stores nothing more.
#[tokio::test]
async fn reconciled_events_are_stored_once_whoever_stored_them_first() {
    let test = TestDb::new().await;
    let ws = test.workspace("reconcile").await.id;
    let (_, webhook) = mailgun(&test, ws, true).await;
    sqlx::query(
        "INSERT INTO provider_event_keys (provider_webhook_id, event_id, body_hash)
         VALUES ($1, 'by-the-webhook', '\\x00')",
    )
    .bind(webhook.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();
    let (origin, served) = fake_mailgun(json!([
        event("by-the-api", "delivered"),
        event("by-the-webhook", "delivered"),
        event("an-open", "opened")
    ]))
    .await;

    assert_eq!(reconcile(&test, ws, webhook, &origin).await, "done");
    assert_eq!(
        *served.lock().unwrap(),
        2,
        "the page and the empty one after it"
    );
    assert_eq!(
        receipts(&test, webhook).await,
        [("by-the-api".to_owned(), "received".to_owned())]
    );
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT event_id FROM provider_event_keys WHERE provider_webhook_id = $1 ORDER BY event_id",
    )
    .bind(webhook.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert_eq!(keys, ["by-the-api", "by-the-webhook"]);
    let normalize: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = 'receipts.normalize' AND workspace_id = $1",
    )
    .bind(ws.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(normalize, 1);

    assert_eq!(reconcile(&test, ws, webhook, &origin).await, "done");
    assert_eq!(
        receipts(&test, webhook).await,
        [("by-the-api".to_owned(), "received".to_owned())],
        "a second run stores nothing more"
    );
}

/// A connection without an API key, or archived, is never read: the run is done without a
/// request, so a key the customer never gave is never asked for, and an archived account's
/// events stay where they are.
#[tokio::test]
async fn keyless_and_archived_connections_are_not_read() {
    let test = TestDb::new().await;
    let ws = test.workspace("unread").await.id;
    let (origin, served) = fake_mailgun(json!([event("e1", "delivered")])).await;

    let (_, keyless) = mailgun(&test, ws, false).await;
    assert_eq!(reconcile(&test, ws, keyless, &origin).await, "done");

    let other = test.workspace("archived").await.id;
    let (archived, webhook) = mailgun(&test, other, true).await;
    sqlx::query("UPDATE connections SET status = 'archived', credential = NULL WHERE id = $1")
        .bind(archived.connection.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    assert_eq!(reconcile(&test, other, webhook, &origin).await, "done");

    assert_eq!(*served.lock().unwrap(), 0);
    assert!(receipts(&test, keyless).await.is_empty());
    assert!(receipts(&test, webhook).await.is_empty());
}
