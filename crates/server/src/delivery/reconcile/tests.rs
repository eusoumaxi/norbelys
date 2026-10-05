//! The Sent-folder step's store and gate tests, against real PostgreSQL through the job runner
//! and a fake Gmail on the loopback interface: what it settles, what it leaves, and its fence
//! on the credential. A person's `resolve` is proven through its route (`messages.resolve`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use secrecy::SecretString;
use serde_json::json;
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::db::Database;
use crate::domain::ids::{Id, Message, WorkspaceId};
use crate::jobs::{self, Queue, Registry};
use crate::senders::check::ConnectionCheck;
use crate::senders::credentials::{self, Credential, Grant};
use crate::testing::{self, TestDb, TestSender};

/// The Message-IDs (without angle brackets) a fake Gmail was asked about, in order.
type Asked = Arc<Mutex<Vec<String>>>;

/// A fake Gmail answering `users.messages.list` in the Sent label: one message for a
/// `rfc822msgid:` search of an id in `found`, none otherwise. When `replace` names a connection,
/// each search first replaces its credential (its version moves), as a person reconnecting the
/// mailbox meanwhile would. Answers its origin and what it was asked.
async fn fake_gmail(found: Vec<String>, replace: Option<(Database, Uuid)>) -> (String, Asked) {
    let asked: Asked = Arc::default();
    let found = Arc::new(found);
    let recorded = Arc::clone(&asked);
    let app = axum::Router::new().route(
        "/gmail/v1/users/me/messages",
        axum::routing::get(move |uri: http::Uri| {
            let found = Arc::clone(&found);
            let recorded = Arc::clone(&recorded);
            let replace = replace.clone();
            async move {
                let id = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
                    .find(|(name, _)| name == "q")
                    .map(|(_, value)| value.trim_start_matches("rfc822msgid:").to_owned())
                    .unwrap_or_default();
                recorded.lock().unwrap().push(id.clone());
                if let Some((db, connection)) = replace {
                    sqlx::query("UPDATE connections SET credential_version = credential_version + 1 WHERE id = $1")
                        .bind(connection)
                        .execute(db.pool())
                        .await
                        .unwrap();
                }
                let messages = if found.contains(&id) {
                    json!([{ "id": "18f0c0ffee", "threadId": "18f0c0ffee" }])
                } else {
                    json!([])
                };
                axum::Json(json!({ "messages": messages, "resultSizeEstimate": 0 }))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (origin, asked)
}

/// An active Gmail mailbox of `workspace` with its identity and a sealed OAuth grant.
async fn gmail_mailbox(test: &TestDb, workspace: WorkspaceId) -> TestSender {
    let (connection, identity): (Uuid, Uuid) = sqlx::query_as(
        "WITH c AS (
            INSERT INTO connections (workspace_id, provider, transport, account_email, status, daily_limit, send_interval_minutes)
            VALUES ($1, 'google', 'api', 'ada@example.test', 'active', 100, 10)
            RETURNING workspace_id, id, account_email)
         INSERT INTO sender_identities (workspace_id, connection_id, email)
         SELECT workspace_id, id, account_email FROM c RETURNING connection_id, id",
    )
    .bind(workspace.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    let sealed = credentials::seal(
        &testing::keys(),
        workspace,
        Id::from_uuid(connection),
        &Credential::OAuth(Grant {
            refresh_token: SecretString::from("refresh"),
            access_token: SecretString::from("access"),
            expires_at: crate::process::now().plus(Duration::from_secs(3_600)),
            scope: "https://www.googleapis.com/auth/gmail.send".to_owned(),
        }),
    )
    .unwrap();
    sqlx::query("UPDATE connections SET credential = $2, credential_version = credential_version + 1 WHERE id = $1")
        .bind(connection)
        .bind(sealed)
        .execute(test.system.pool())
        .await
        .unwrap();
    TestSender {
        connection: Id::from_uuid(connection),
        identity: Id::from_uuid(identity),
    }
}

/// A message of `sender` that ended `uncertain` (its queue row gone).
async fn uncertain(
    test: &TestDb,
    workspace: WorkspaceId,
    sender: &TestSender,
    to: &str,
) -> (Id<Message>, String) {
    let message = test.direct_message(workspace, sender, &[to], -60).await;
    let internet_message_id: String = sqlx::query_scalar(
        "WITH q AS (DELETE FROM delivery_queue WHERE message_id = $1)
         UPDATE messages SET state = 'uncertain' WHERE id = $1 RETURNING internet_message_id",
    )
    .bind(message.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    let bare = internet_message_id
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_owned();
    (message, bare)
}

/// Runs the mailbox's `connection.check` resumed in its Sent-folder step (as a check that
/// passed and yielded there is), with its HTTP calls reaching `origin`; answers its outcome.
async fn run_sent_folder_step(
    test: &TestDb,
    workspace: WorkspaceId,
    sender: &TestSender,
    origin: &str,
) -> &'static str {
    let mut tx = test.system.begin().await.unwrap();
    jobs::enqueue(
        &mut tx,
        workspace,
        &ConnectionCheck {
            connection: sender.connection,
        },
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    sqlx::query(
        "UPDATE jobs SET progress = jsonb_build_object('verdict', 'passed', 'reconcile',
                jsonb_build_object('version', c.credential_version, 'visited', '[]'::jsonb, 'settled', 0))
           FROM connections c WHERE c.id = $1 AND jobs.kind = 'connection.check'",
    )
    .bind(sender.connection.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();
    let mut registry = Registry::default();
    registry.register::<ConnectionCheck>().unwrap();
    let runner = testing::provider_runner(test, registry, origin);
    let ran = runner.run_once(Queue::Maintenance, 1).await;
    assert_eq!(ran.len(), 1);
    ran[0].1
}

/// The state of `message` and its delivery events as `(source, kind, confidence)`.
async fn settled(test: &TestDb, message: Id<Message>) -> (String, Vec<(String, String, String)>) {
    let state: String = sqlx::query_scalar("SELECT state FROM messages WHERE id = $1")
        .bind(message.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    let events = sqlx::query_as(
        "SELECT source, kind, confidence FROM delivery_events WHERE message_id = $1",
    )
    .bind(message.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    (state, events)
}

/// The last step of a mailbox's check searches each of its `uncertain` messages in the Sent
/// folder by its Message-ID, once: one found is `sent`, through an `authenticated` `accepted`
/// event from `sent_folder`, and `message.sent` is told; one not found stays `uncertain` with
/// nothing recorded (finding nothing proves nothing), and the check ends done.
#[tokio::test]
async fn the_sent_folder_settles_what_it_finds() {
    let test = TestDb::new().await;
    let ws = test.workspace("folder").await.id;
    let mailbox = gmail_mailbox(&test, ws).await;
    let (found, found_id) = uncertain(&test, ws, &mailbox, "found@example.test").await;
    let (missing, missing_id) = uncertain(&test, ws, &mailbox, "missing@example.test").await;
    let (origin, asked) = fake_gmail(vec![found_id.clone()], None).await;

    assert_eq!(
        run_sent_folder_step(&test, ws, &mailbox, &origin).await,
        "done"
    );
    let mut searched = asked.lock().unwrap().clone();
    searched.sort();
    let mut expected = vec![found_id, missing_id];
    expected.sort();
    assert_eq!(searched, expected, "each uncertain message searched once");
    assert_eq!(
        settled(&test, found).await,
        (
            "sent".to_owned(),
            vec![(
                "sent_folder".to_owned(),
                "accepted".to_owned(),
                "authenticated".to_owned()
            )]
        )
    );
    assert_eq!(test.told(ws, found.uuid()).await, ["message.sent"]);
    assert_eq!(
        settled(&test, missing).await,
        ("uncertain".to_owned(), Vec::new())
    );
    assert!(test.told(ws, missing.uuid()).await.is_empty());
}

/// The step writes nothing unless the credential it searched with is still the connection's:
/// a credential replaced while it searched (its version moved) makes it settle nothing, though
/// the folder held the message.
#[tokio::test]
async fn a_credential_replaced_during_the_search_settles_nothing() {
    let test = TestDb::new().await;
    let ws = test.workspace("fence").await.id;
    let mailbox = gmail_mailbox(&test, ws).await;
    let (message, id) = uncertain(&test, ws, &mailbox, "found@example.test").await;
    let (origin, asked) = fake_gmail(
        vec![id],
        Some((test.system.clone(), mailbox.connection.uuid())),
    )
    .await;

    assert_eq!(
        run_sent_folder_step(&test, ws, &mailbox, &origin).await,
        "done"
    );
    assert_eq!(asked.lock().unwrap().len(), 1, "it searched");
    assert_eq!(
        settled(&test, message).await,
        ("uncertain".to_owned(), Vec::new())
    );
    assert!(test.told(ws, message.uuid()).await.is_empty());
}
