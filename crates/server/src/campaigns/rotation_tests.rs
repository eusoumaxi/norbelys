//! Gate test of the pool's rotation under concurrency: two creation passes of one campaign giving
//! new conversations their senders at the same time.
//!
//! A pass gives each new conversation the pool's least recently assigned sender and records the
//! assignment in `campaign_sender_rotation`; two passes reading the rotation at once would both
//! give the same sender. Every pass therefore locks its campaigns' rows before it reads the pool,
//! so the second pass reads the rotation only once the first has committed its assignment.

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use super::creator;
use crate::domain::ids::{Campaign, Id};
use crate::domain::time::Timestamp;
use crate::testing::{SenderSpec, TestApp, TestDb, keys};

/// `POST path` with `body` and a fresh idempotency key; answers the created object.
async fn create(app: &TestApp, key: &str, path: &str, body: Value) -> Value {
    let reply = app
        .post(path)
        .bearer(key)
        .idempotency(&Uuid::now_v7().to_string())
        .json(body)
        .send()
        .await;
    assert_eq!(
        reply.status,
        StatusCode::CREATED,
        "{path}: {:?}",
        reply.json
    );
    reply.json
}

/// The uuid of a wire id (`cmp_…`).
fn uuid(id: &Value) -> Uuid {
    let (_, hex) = id.as_str().unwrap().split_once('_').unwrap();
    Uuid::parse_str(hex).unwrap()
}

/// Two passes assigning one new conversation each in one campaign, whose pool holds a named
/// identity and one selected by tag only, serialise on the campaign's row: the second, started
/// while the first is still in its transaction, waits for the first's commit, then reads the
/// rotation it recorded and gives its conversation the other identity; both assignments are
/// recorded, the tag-only identity's included. Without the lock both passes would read an empty
/// rotation and give their conversations the same sender.
#[tokio::test]
async fn two_assignments_in_one_campaign_serialise_on_its_row() {
    let test = TestDb::new().await;
    let ws = test.workspace("rotation").await;
    let app = test.app();
    let named = test
        .sender(ws.id, &SenderSpec::mailbox("named@rotation.example"))
        .await;
    let tagged = test
        .sender(ws.id, &SenderSpec::mailbox("tagged@rotation.example"))
        .await;
    sqlx::query("UPDATE sender_identities SET tags = '{pool}' WHERE id = $1")
        .bind(tagged.identity.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    let campaign = create(
        &app,
        &ws.key,
        "/v1/campaigns",
        json!({
            "name": "Rotation",
            "steps": [{"name": "Intro", "variants": [{"subject": "Hi", "html": "<p>Hello</p>"}]}],
            "senders": {"identity_ids": [named.identity.to_string()], "tags": ["pool"]},
        }),
    )
    .await["id"]
        .clone();
    let mut people = Vec::new();
    for name in ["ada", "bob"] {
        let person = create(
            &app,
            &ws.key,
            "/v1/people",
            json!({"email": format!("{name}@rotation.example"), "given_name": name}),
        )
        .await;
        people.push(person["id"].clone());
    }
    create(
        &app,
        &ws.key,
        "/v1/enrollments",
        json!({"campaign_id": campaign, "person_ids": people}),
    )
    .await;
    let campaign = uuid(&campaign);
    sqlx::query("UPDATE campaigns SET status = 'active' WHERE id = $1")
        .bind(campaign)
        .execute(test.system.pool())
        .await
        .unwrap();
    // The second pass starts after the first one's enrollment, as the next chunk would.
    let after_first: (Timestamp, Uuid) = sqlx::query_as(
        "SELECT next_run_at, id FROM enrollments WHERE campaign_id = $1 ORDER BY next_run_at, id LIMIT 1",
    )
    .bind(campaign)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    let only = Some(Id::<Campaign>::from_uuid(campaign));

    let mut first = test.worker.begin_in(ws.id).await.unwrap();
    let made = creator::pass(
        &mut first,
        &keys(),
        ws.id,
        only,
        jiff::Timestamp::now(),
        None,
        1,
    )
    .await
    .unwrap();
    assert_eq!(made.created, 1);
    let other = test.worker_pool(2).await;
    let workspace = ws.id;
    let deployment = keys();
    let second = tokio::spawn(async move {
        let started = Instant::now();
        let mut tx = other.begin_in(workspace).await.unwrap();
        let made = creator::pass(
            &mut tx,
            &deployment,
            workspace,
            only,
            jiff::Timestamp::now(),
            Some(after_first),
            1,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        (made.created, started.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(
        !second.is_finished(),
        "the second pass did not wait for the campaign's lock"
    );
    first.commit().await.unwrap();
    let (created, waited) = second.await.unwrap();
    assert_eq!(created, 1);
    assert!(
        waited >= Duration::from_secs(1),
        "the second pass waited only {waited:?}"
    );

    let mut chosen: Vec<Uuid> =
        sqlx::query_scalar("SELECT sender_identity_id FROM messages WHERE campaign_id = $1")
            .bind(campaign)
            .fetch_all(test.system.pool())
            .await
            .unwrap();
    chosen.sort_unstable();
    let mut pool = vec![named.identity.uuid(), tagged.identity.uuid()];
    pool.sort_unstable();
    assert_eq!(
        chosen, pool,
        "the two conversations did not get one sender each"
    );
    let recorded: i64 =
        sqlx::query_scalar("SELECT count(*) FROM campaign_sender_rotation WHERE campaign_id = $1")
            .bind(campaign)
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(recorded, 2);
}
