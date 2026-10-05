//! API tests of `messages` and `delivery_events`: the create contract and each of its bounds,
//! suppression, idempotency, the embedded children, lists with filters and pages, tenancy, and
//! `messages.cancel` and `messages.resolve`: the states that allow them, idempotency, tenancy.

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::domain::ids::{DeliveryEvent, Id, InboundMessage, Message};
use crate::domain::scope::{Scope, ScopeSet};
use crate::testing::{Reply, TestApp, TestDb, TestWorkspace};

/// A fresh idempotency key.
fn key() -> String {
    Uuid::now_v7().to_string()
}

/// Connects an SMTP login of `account` in `workspace` and answers its identity's id.
async fn sender(app: &TestApp, workspace: &TestWorkspace, account: &str) -> String {
    let created = app
        .post("/v1/connections")
        .bearer(&workspace.key)
        .idempotency(&key())
        .json(json!({
            "provider": "smtp",
            "account_email": account,
            "smtp": { "host": "smtp.acme.example", "port": 587, "security": "starttls", "password": "secret" },
        }))
        .send()
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    created.json["identities"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// `POST /v1/messages` with `body`.
async fn send(app: &TestApp, credential: &str, body: Value) -> Reply {
    app.post("/v1/messages")
        .bearer(credential)
        .idempotency(&key())
        .json(body)
        .send()
        .await
}

/// A valid direct message from Max to Ada.
fn message() -> Value {
    json!({
        "from": "max@acme.example",
        "to": ["ada@example.com"],
        "subject": "Hello {{ variables.name }}",
        "html": "<p>Hi {{ variables.name }}</p>",
        "variables": { "name": "Ada" },
    })
}

/// A message is accepted with `202`, its `Location` and the queued object: the envelope, the
/// rendered subject, its Message-ID and thread, empty children, and no tracking. It can be read
/// back, by its sender identity's id as well as its address.
#[tokio::test]
async fn a_message_is_accepted_and_readable() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let identity = sender(&app, &acme, "max@acme.example").await;
    let created = send(&app, &acme.key, message()).await;
    assert_eq!(created.status, StatusCode::ACCEPTED, "{}", created.json);
    let id = created.json["id"].as_str().unwrap();
    assert_eq!(
        created.header("location"),
        Some(format!("/v1/messages/{id}").as_str())
    );
    let object = &created.json;
    assert_eq!(object["kind"], "direct");
    assert_eq!(object["state"], "queued");
    assert_eq!(
        object["from"],
        json!({ "email": "max@acme.example", "name": null })
    );
    assert_eq!(object["to"], json!(["ada@example.com"]));
    assert_eq!(object["subject"], "Hello Ada");
    assert_eq!(object["sender_identity_id"], identity.as_str());
    assert!(object["thread_id"].as_str().unwrap().starts_with("thr_"));
    assert!(
        object["internet_message_id"]
            .as_str()
            .unwrap()
            .ends_with("@acme.example>")
    );
    assert_eq!(
        object["tracking"],
        json!({ "opens": false, "clicks": false, "hostname": null })
    );
    assert_eq!(object["attempts"], json!({ "data": [], "has_more": false }));
    assert_eq!(object["attempts_count"], 0);
    assert_eq!(
        object["events"],
        json!({ "data": [], "has_more": false, "url": format!("/v1/delivery_events?message_id={id}") })
    );
    assert_eq!(object["holds"], json!([]));
    let read = app
        .get(&format!("/v1/messages/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(&read.json, object);
    let mut by_id = message();
    by_id["from"] = json!(identity);
    assert_eq!(
        send(&app, &acme.key, by_id).await.status,
        StatusCode::ACCEPTED
    );
}

/// The public message contract has one authored body; a second text field is refused instead
/// of letting a caller send mismatched HTML and plain-text content.
#[tokio::test]
async fn a_second_body_is_refused() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    sender(&app, &acme, "max@acme.example").await;
    let mut body = message();
    body["text"] = json!("Different content");
    let refused = send(&app, &acme.key, body).await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.json
    );
}

/// Every bound of the direct form is refused with `422` at its own pointer, before anything is
/// written: recipients, the envelope's size and duplicates, the sender, the subject, the bodies,
/// the variables, the schedule, and templates that do not render.
#[tokio::test]
async fn each_bound_is_refused_at_its_pointer() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    sender(&app, &acme, "max@acme.example").await;
    let many = |n: usize| -> Value { (0..n).map(|i| json!(format!("p{i}@example.com"))).collect() };
    let later = |days: i64| {
        jiff::Timestamp::now()
            .checked_add(jiff::SignedDuration::from_hours(24 * days))
            .unwrap()
            .to_string()
    };
    let cases: Vec<(&str, Value, &str)> = vec![
        ("to", json!([]), "/to"),
        ("to", many(51), "/to"),
        ("to", json!(["not an address"]), "/to/0"),
        ("cc", json!(["ADA@example.com"]), "/cc/0"),
        ("from", json!("sid_123"), "/from"),
        ("from", json!("not an address"), "/from"),
        ("subject", json!(""), "/subject"),
        ("subject", json!("{% if %}"), "/subject"),
        ("subject", json!("Hi {{ person.given_name }}"), "/subject"),
        ("html", json!("x".repeat(256 * 1024 + 1)), "/html"),
        (
            "variables",
            json!({ "blob": "x".repeat(64 * 1024) }),
            "/variables",
        ),
        ("send_at", json!(later(8)), "/send_at"),
        ("expires_at", json!("2020-01-01T00:00:00Z"), "/expires_at"),
    ];
    for (field, value, pointer) in cases {
        let mut body = message();
        body[field] = value;
        let refused = send(&app, &acme.key, body).await;
        assert_eq!(
            refused.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{field}: {}",
            refused.json
        );
        assert_eq!(refused.json["code"], "validation_failed", "{field}");
        assert_eq!(
            refused.json["errors"][0]["pointer"], pointer,
            "{field}: {}",
            refused.json
        );
    }
    let mut crowded = message();
    crowded["cc"] = (0..100)
        .map(|i| json!(format!("c{i}@example.com")))
        .collect();
    crowded["bcc"] = (0..50)
        .map(|i| json!(format!("b{i}@example.com")))
        .collect();
    let refused = send(&app, &acme.key, crowded).await;
    assert_eq!(
        refused.json["errors"][0]["pointer"], "/to",
        "151 recipients in all"
    );
    let mut no_body = message();
    no_body.as_object_mut().unwrap().remove("html");
    let refused = send(&app, &acme.key, no_body).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        refused.json["errors"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("html")
    );
    let mut templates = message();
    templates["html"] = json!("<p>{{ variables.missing }}</p>");
    let refused = send(&app, &acme.key, templates).await;
    let pointers: Vec<&str> = refused.json["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|error| error["pointer"].as_str().unwrap())
        .collect();
    assert_eq!(pointers, ["/html"], "the authored body is reported");
    assert_eq!(refused.json["errors"][0]["code"], "template");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

/// A message from an address that is no live identity, or from an identity that is disabled,
/// is refused; a disabled one is a state, so it answers `409`.
#[tokio::test]
async fn the_sender_must_be_a_live_enabled_identity() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let identity = sender(&app, &acme, "max@acme.example").await;
    let mut body = message();
    body["from"] = json!("nobody@acme.example");
    assert_eq!(
        send(&app, &acme.key, body).await.status,
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE sender_identities SET enabled = false")
        .execute(test.system.pool())
        .await
        .unwrap();
    let mut body = message();
    body["from"] = json!(identity);
    let refused = send(&app, &acme.key, body).await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.json["code"], "invalid_state");
}

/// A suppressed recipient anywhere in the envelope refuses the message with `422 suppressed`,
/// naming the address and the reason but never the evidence.
#[tokio::test]
async fn a_suppressed_recipient_is_refused() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    sender(&app, &acme, "max@acme.example").await;
    let suppressed = app
        .post("/v1/suppressions")
        .bearer(&acme.key)
        .idempotency(&key())
        .json(json!({ "email": "Grace@Example.com" }))
        .send()
        .await;
    assert_eq!(
        suppressed.status,
        StatusCode::CREATED,
        "{}",
        suppressed.json
    );
    let mut body = message();
    body["bcc"] = json!(["grace@example.com"]);
    let refused = send(&app, &acme.key, body).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refused.json["code"], "suppressed");
    assert_eq!(
        refused.json["detail"],
        "`Grace@Example.com` is suppressed (manual); no message is sent to it."
    );
}

/// The same request repeated with its `Idempotency-Key` answers the stored response and writes
/// no second message; the key reused for another body is refused.
#[tokio::test]
async fn a_repeated_request_creates_one_message() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    sender(&app, &acme, "max@acme.example").await;
    let call = |body: Value| {
        app.post("/v1/messages")
            .bearer(&acme.key)
            .idempotency("same-key")
            .json(body)
            .send()
    };
    let first = call(message()).await;
    let again = call(message()).await;
    assert_eq!(again.status, StatusCode::ACCEPTED);
    assert_eq!(again.header("idempotent-replayed"), Some("true"));
    assert_eq!(again.json, first.json);
    assert_eq!(again.header("location"), first.header("location"));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    let mut other = message();
    other["subject"] = json!("Another");
    let mismatch = call(other).await;
    assert_eq!(mismatch.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(mismatch.json["code"], "idempotency_mismatch");
}

/// A message to an address that is a person of the workspace belongs to that person (its
/// templates may read `person`), and the list filters by person, connection and state, and pages
/// by signed cursor, every message once.
#[tokio::test]
async fn messages_list_with_filters_and_pages() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    sender(&app, &acme, "max@acme.example").await;
    let other = sender(&app, &acme, "eve@acme.example").await;
    let person = app
        .post("/v1/people")
        .bearer(&acme.key)
        .idempotency(&key())
        .json(json!({ "email": "Ada@Example.com", "given_name": "Ada" }))
        .send()
        .await;
    let person_id = person.json["id"].as_str().unwrap().to_owned();
    let mut personal = message();
    personal["subject"] = json!("Hi {{ person.given_name }}");
    let first = send(&app, &acme.key, personal).await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.json);
    assert_eq!(first.json["person_id"], person_id.as_str());
    assert_eq!(first.json["subject"], "Hi Ada");
    send(&app, &acme.key, message()).await;
    let mut from_other = message();
    from_other["from"] = json!(other);
    from_other["to"] = json!(["grace@example.com"]);
    let third = send(&app, &acme.key, from_other).await;

    let list = |query: String| {
        app.get(&format!("/v1/messages{query}"))
            .bearer(&acme.key)
            .send()
    };
    let page = list("?limit=2".to_owned()).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.json["data"].as_array().unwrap().len(), 2);
    assert_eq!(page.json["data"][0]["id"], third.json["id"], "newest first");
    assert_eq!(page.json["meta"]["has_more"], true);
    let cursor = page.json["meta"]["next_cursor"].as_str().unwrap();
    let rest = list(format!("?limit=2&cursor={cursor}")).await;
    assert_eq!(rest.json["data"].as_array().unwrap().len(), 1);
    assert_eq!(rest.json["data"][0]["id"], first.json["id"]);
    assert_eq!(rest.json["meta"]["has_more"], false);
    let oldest = list("?order=asc&limit=2".to_owned()).await;
    assert_eq!(
        oldest.json["data"][0]["id"], first.json["id"],
        "oldest first"
    );
    let cursor = oldest.json["meta"]["next_cursor"].as_str().unwrap();
    let after = list(format!("?order=asc&limit=2&cursor={cursor}")).await;
    assert_eq!(after.json["data"][0]["id"], third.json["id"]);

    let ids = |reply: &Reply| -> Vec<Value> {
        reply.json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["id"].clone())
            .collect()
    };
    let by_person = list(format!("?person_id={person_id}")).await;
    assert_eq!(ids(&by_person).len(), 2, "both messages to Ada are hers");
    let connection = third.json["connection_id"].as_str().unwrap();
    let by_connection = list(format!("?connection_id={connection}")).await;
    assert_eq!(ids(&by_connection), [third.json["id"].clone()]);
    let queued = list("?state=queued&include=total_count".to_owned()).await;
    assert_eq!(queued.json["meta"]["total_count"], 3);
    assert_eq!(ids(&list("?state=sent".to_owned()).await).len(), 0);
    let unknown_state = list("?state=lost".to_owned()).await;
    assert_eq!(unknown_state.status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// A message shows its children in every response: its latest 20 attempts newest first with
/// the count of all, its first 20 delivery events with `has_more`, and its holds; the delivery
/// event list serves the rest, filtered by message, kind and recipient, counts them on request
/// (`include=total_count`), and retrieves one. Provider codes remain distinct in the embedded
/// history and event list, including when a policy block is only a temporary deferral.
#[tokio::test]
async fn a_message_embeds_its_children() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    sender(&app, &acme, "max@acme.example").await;
    let created = send(&app, &acme.key, message()).await;
    let id = created.json["id"].as_str().unwrap().to_owned();
    let message: Id<Message> = id.parse().unwrap();
    let ws = acme.id.uuid();
    let mut tx = test.system.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO connection_usage (workspace_id, connection_id, day)
         SELECT workspace_id, connection_id, (now() AT TIME ZONE 'UTC')::date FROM messages WHERE id = $1",
    )
    .bind(message.uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO attempts (workspace_id, message_id, attempt_number, connection_id, reserved_day, recipient_count,
                               quota_state, lease_owner, finished_at, outcome, phase, smtp_code)
         SELECT m.workspace_id, m.id, n, m.connection_id, (now() AT TIME ZONE 'UTC')::date, 1, 'released', 'test',
                now(), 'transient', 'rcpt_to', 451
           FROM messages m, generate_series(1, 21) n WHERE m.id = $1",
    )
    .bind(message.uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO delivery_events (workspace_id, message_id, recipient_email, recipient_ref, source, source_event_id,
                                      kind, category, enhanced_status, diagnostic, confidence, observed_at)
         SELECT $1, $2, 'ada@example.com', 'named', 'smtp', 'attempt:' || n, CASE WHEN n = 1 THEN 'bounced' ELSE 'deferred' END,
                CASE WHEN n <= 2 THEN 'policy' ELSE 'mailbox_full' END,
                CASE WHEN n = 1 THEN '5.7.1' WHEN n = 2 THEN '4.7.1' END,
                CASE WHEN n = 1 THEN '550 5.7.1 filtered (JFE040004)'
                     WHEN n = 2 THEN '550 5.7.1 unusual invalid recipients (JFE050004)' END,
                'authenticated', now() + n * interval '1 second'
           FROM generate_series(1, 21) n",
    )
    .bind(ws)
    .bind(message.uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recipient_holds (workspace_id, message_id, email, reason, observed_at, review_after)
         VALUES ($1, $2, 'ada@example.com', 'mailbox_full', now(), now() + interval '1 day')",
    )
    .bind(ws)
    .bind(message.uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let read = app
        .get(&format!("/v1/messages/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    let object = &read.json;
    assert_eq!(object["attempts_count"], 21);
    assert_eq!(object["attempts"]["has_more"], true);
    let numbers: Vec<i64> = object["attempts"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|attempt| attempt["number"].as_i64().unwrap())
        .collect();
    assert_eq!(
        numbers,
        (2..=21).rev().collect::<Vec<_>>(),
        "the latest 20, newest first"
    );
    assert_eq!(object["attempts"]["data"][0]["outcome"], "transient");
    assert_eq!(object["events"]["data"].as_array().unwrap().len(), 20);
    assert_eq!(
        object["events"]["data"][0]["kind"], "bounced",
        "the first observed first"
    );
    assert_eq!(object["events"]["data"][0]["provider_code"], "JFE040004");
    assert_eq!(object["events"]["data"][1]["kind"], "deferred");
    assert_eq!(object["events"]["data"][1]["provider_code"], "JFE050004");
    assert_eq!(object["events"]["data"][2]["provider_code"], Value::Null);
    assert_eq!(object["events"]["has_more"], true);
    assert_eq!(object["holds"][0]["reason"], "mailbox_full");
    let listed = app.get("/v1/messages").bearer(&acme.key).send().await;
    assert_eq!(
        &listed.json["data"][0], object,
        "a list shows the same object"
    );

    let events = |query: &str| {
        app.get(&format!("/v1/delivery_events{query}"))
            .bearer(&acme.key)
            .send()
    };
    let all = events(&format!("?message_id={id}&limit=100")).await;
    assert_eq!(all.json["data"].as_array().unwrap().len(), 21);
    assert!(
        all.json["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| { event["provider_code"] == "JFE040004" && event["kind"] == "bounced" })
    );
    assert!(
        all.json["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| { event["provider_code"] == "JFE050004" && event["kind"] == "deferred" })
    );
    let counted = events(&format!("?message_id={id}&limit=1&include=total_count")).await;
    assert_eq!(counted.json["meta"]["total_count"], 21);
    assert_eq!(counted.json["meta"]["total_count_capped"], false);
    let counted = events("?kind=bounced&include=total_count").await;
    assert_eq!(
        counted.json["meta"]["total_count"], 1,
        "the count applies the filters"
    );
    let ascending = events(&format!("?message_id={id}&order=asc&limit=100")).await;
    assert_eq!(
        ascending.json["data"][0]["id"], all.json["data"][20]["id"],
        "oldest first"
    );
    let bounced = events("?kind=bounced").await;
    assert_eq!(bounced.json["data"].as_array().unwrap().len(), 1);
    let event_id = bounced.json["data"][0]["id"].as_str().unwrap();
    assert_eq!(
        events("?recipient=ADA@example.com&limit=1").await.json["meta"]["has_more"],
        true
    );
    assert_eq!(
        events("?recipient=nobody@example.com").await.json["data"],
        json!([])
    );
    let one = app
        .get(&format!("/v1/delivery_events/{event_id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(one.status, StatusCode::OK);
    assert_eq!(one.json, bounced.json["data"][0]);
    assert_eq!(one.json["received_at"], one.json["processed_at"]);
}

/// Another workspace's messages and events do not exist for a credential: retrieving them is
/// `404`, listing shows none, and its identity cannot send a message here.
#[tokio::test]
async fn another_workspaces_messages_are_not_found() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let rival = test.workspace("rival").await;
    let app = test.app();
    let identity = sender(&app, &acme, "max@acme.example").await;
    let created = send(&app, &acme.key, message()).await;
    let id = created.json["id"].as_str().unwrap();
    let foreign = app
        .get(&format!("/v1/messages/{id}"))
        .bearer(&rival.key)
        .send()
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
    let listed = app.get("/v1/messages").bearer(&rival.key).send().await;
    assert_eq!(listed.json["data"], json!([]));
    let mut forged = message();
    forged["from"] = json!(identity);
    assert_eq!(
        send(&app, &rival.key, forged).await.status,
        StatusCode::NOT_FOUND
    );
    let event = format!("dev_{}", Uuid::now_v7().simple());
    let missing = app
        .get(&format!("/v1/delivery_events/{event}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

/// Sending needs `messages:send` and reading `messages:read`.
#[tokio::test]
async fn sending_and_reading_need_their_scopes() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    sender(&app, &acme, "max@acme.example").await;
    let reader = test
        .api_key(&acme, ScopeSet::from_iter([Scope::MessagesRead]))
        .await;
    assert_eq!(
        send(&app, &reader, message()).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.get("/v1/messages").bearer(&reader).send().await.status,
        StatusCode::OK
    );
    let writer = test
        .api_key(&acme, ScopeSet::from_iter([Scope::MessagesSend]))
        .await;
    assert_eq!(
        send(&app, &writer, message()).await.status,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        app.get("/v1/delivery_events")
            .bearer(&writer)
            .send()
            .await
            .status,
        StatusCode::FORBIDDEN
    );
}

/// Leaves the message `id` (`msg_…`) in `state` as the delivery engine would: claimed or in
/// flight under a sender's lease, or ended with its queue row gone.
async fn leave_in(test: &TestDb, id: &str, state: &str) {
    let message: Id<Message> = id.parse().unwrap();
    let mut tx = test.system.begin().await.unwrap();
    if state == "claimed" || state == "in_flight" {
        sqlx::query(
            "UPDATE delivery_queue SET state = $2, lease_owner = 'sender-a', lease_expires_at = now() + interval '2 minutes'
              WHERE message_id = $1",
        )
        .bind(message.uuid())
        .bind(state)
        .execute(&mut *tx)
        .await
        .unwrap();
    } else {
        sqlx::query("DELETE FROM delivery_queue WHERE message_id = $1")
            .bind(message.uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE messages SET state = $2, sent_at = CASE WHEN $2 = 'sent' THEN now() END WHERE id = $1")
        .bind(message.uuid())
        .bind(state)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// `POST /v1/messages/{id}/{action}` with `credential` and a fresh idempotency key.
async fn act(
    app: &TestApp,
    credential: &str,
    id: &str,
    action: &str,
    body: Option<Value>,
) -> Reply {
    let call = app
        .post(&format!("/v1/messages/{id}/{action}"))
        .bearer(credential)
        .idempotency(&key());
    match body {
        Some(body) => call.json(body).send().await,
        None => call.send().await,
    }
}

/// The `type`s of the outbox events about the message `id`.
async fn told(test: &TestDb, id: &str) -> Vec<String> {
    let message: Id<Message> = id.parse().unwrap();
    sqlx::query_scalar("SELECT type FROM outbox_events WHERE subject_id = $1 ORDER BY id")
        .bind(message.uuid())
        .fetch_all(test.system.pool())
        .await
        .unwrap()
}

/// Only a queued message can be cancelled. Cancelling one answers `200` with the message
/// `cancelled`, deletes its queue row and tells `message.cancelled`. A message a sender has
/// claimed or started is on its way and cannot be recalled, and an ended one (sent, or the one
/// just cancelled) has nothing left to cancel: each answers `409 invalid_state` naming its
/// state, and stays as it was.
#[tokio::test]
async fn only_a_queued_message_can_be_cancelled() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    sender(&app, &acme, "max@acme.example").await;
    let mut ids = Vec::new();
    for _ in 0..4 {
        let created = send(&app, &acme.key, message()).await;
        ids.push(created.json["id"].as_str().unwrap().to_owned());
    }

    let cancelled = act(&app, &acme.key, &ids[0], "cancel", None).await;
    assert_eq!(cancelled.status, StatusCode::OK, "{}", cancelled.json);
    assert_eq!(cancelled.json["id"], ids[0]);
    assert_eq!(cancelled.json["state"], "cancelled");
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_queue WHERE message_id = $1")
            .bind(ids[0].parse::<Id<Message>>().unwrap().uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(queued, 0);
    assert_eq!(told(&test, &ids[0]).await, ["message.cancelled"]);

    leave_in(&test, &ids[1], "claimed").await;
    leave_in(&test, &ids[2], "in_flight").await;
    leave_in(&test, &ids[3], "sent").await;
    for (id, state) in [
        (&ids[0], "cancelled"),
        (&ids[1], "claimed"),
        (&ids[2], "in_flight"),
        (&ids[3], "sent"),
    ] {
        let refused = act(&app, &acme.key, id, "cancel", None).await;
        assert_eq!(refused.status, StatusCode::CONFLICT, "{state}");
        assert_eq!(refused.json["code"], "invalid_state", "{state}");
        assert!(
            refused.json["detail"].as_str().unwrap().contains(state),
            "{}",
            refused.json
        );
        let read = app
            .get(&format!("/v1/messages/{id}"))
            .bearer(&acme.key)
            .send()
            .await;
        assert_eq!(read.json["state"], state);
    }
    assert_eq!(
        told(&test, &ids[0]).await,
        ["message.cancelled"],
        "told once"
    );
}

/// Only an uncertain message can be resolved. A person's `sent` makes it `sent` and `failed`
/// makes it `failed`, each answering `200` with the message; the decision is recorded as an
/// authenticated `manual` event carrying the evidence, and `message.sent` or `message.failed`
/// is told. A queued message, or one already resolved, answers `409 invalid_state`; empty
/// evidence is refused with `422`.
#[tokio::test]
async fn only_an_uncertain_message_can_be_resolved() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    sender(&app, &acme, "max@acme.example").await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        let created = send(&app, &acme.key, message()).await;
        ids.push(created.json["id"].as_str().unwrap().to_owned());
    }
    leave_in(&test, &ids[0], "uncertain").await;
    leave_in(&test, &ids[1], "uncertain").await;

    let empty = act(
        &app,
        &acme.key,
        &ids[0],
        "resolve",
        Some(json!({ "state": "sent", "evidence": "" })),
    )
    .await;
    assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(empty.json["code"], "validation_failed");

    for (id, resolution, kind, event) in [
        (&ids[0], "sent", "accepted", "message.sent"),
        (&ids[1], "failed", "rejected", "message.failed"),
    ] {
        let evidence = format!("Checked by hand: {resolution}.");
        let resolved = act(
            &app,
            &acme.key,
            id,
            "resolve",
            Some(json!({ "state": resolution, "evidence": evidence })),
        )
        .await;
        assert_eq!(resolved.status, StatusCode::OK, "{}", resolved.json);
        assert_eq!(resolved.json["state"], resolution);
        let recorded: (String, String, String, Option<String>) = sqlx::query_as(
            "SELECT source, kind, confidence, diagnostic FROM delivery_events WHERE message_id = $1",
        )
        .bind(id.parse::<Id<Message>>().unwrap().uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        assert_eq!(
            recorded,
            (
                "manual".to_owned(),
                kind.to_owned(),
                "authenticated".to_owned(),
                Some(evidence)
            )
        );
        assert!(told(&test, id).await.contains(&event.to_owned()), "{event}");
    }

    for (id, state) in [(&ids[2], "queued"), (&ids[0], "sent")] {
        let refused = act(
            &app,
            &acme.key,
            id,
            "resolve",
            Some(json!({ "state": "failed", "evidence": "Never mind." })),
        )
        .await;
        assert_eq!(refused.status, StatusCode::CONFLICT, "{state}");
        assert_eq!(refused.json["code"], "invalid_state");
        assert!(
            refused.json["detail"].as_str().unwrap().contains(state),
            "{}",
            refused.json
        );
    }
}

/// Another workspace's message does not exist for a credential: cancelling or resolving it with
/// a forged id answers `404` and leaves it as it was, as does an id that names no message.
/// Both actions need `messages:send`.
#[tokio::test]
async fn cancel_and_resolve_stay_in_their_workspace() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let rival = test.workspace("rival").await;
    let app = test.app();
    sender(&app, &acme, "max@acme.example").await;
    let queued = send(&app, &acme.key, message()).await.json["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let uncertain = send(&app, &acme.key, message()).await.json["id"]
        .as_str()
        .unwrap()
        .to_owned();
    leave_in(&test, &uncertain, "uncertain").await;
    let resolution = json!({ "state": "sent", "evidence": "Found in the Sent folder." });
    let nowhere = Id::<Message>::from_uuid(Uuid::now_v7()).to_string();

    for (credential, id) in [(&rival.key, &queued), (&acme.key, &nowhere)] {
        assert_eq!(
            act(&app, credential, id, "cancel", None).await.status,
            StatusCode::NOT_FOUND
        );
    }
    for (credential, id) in [(&rival.key, &uncertain), (&acme.key, &nowhere)] {
        assert_eq!(
            act(&app, credential, id, "resolve", Some(resolution.clone()))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }
    let reader = test
        .api_key(&acme, ScopeSet::from_iter([Scope::MessagesRead]))
        .await;
    assert_eq!(
        act(&app, &reader, &queued, "cancel", None).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        act(&app, &reader, &uncertain, "resolve", Some(resolution))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    for (id, state) in [(&queued, "queued"), (&uncertain, "uncertain")] {
        let read = app
            .get(&format!("/v1/messages/{id}"))
            .bearer(&acme.key)
            .send()
            .await;
        assert_eq!(read.json["state"], state, "unchanged");
    }
}

/// A cancellation or a resolution repeated with its `Idempotency-Key` (a retry after a lost
/// response) answers the stored response, marked replayed, and acts once: one
/// `message.cancelled`, one manual event.
#[tokio::test]
async fn a_repeated_cancel_or_resolve_acts_once() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    sender(&app, &acme, "max@acme.example").await;
    let queued = send(&app, &acme.key, message()).await.json["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let uncertain = send(&app, &acme.key, message()).await.json["id"]
        .as_str()
        .unwrap()
        .to_owned();
    leave_in(&test, &uncertain, "uncertain").await;

    let cancel = || {
        app.post(&format!("/v1/messages/{queued}/cancel"))
            .bearer(&acme.key)
            .idempotency("cancel-once")
            .send()
    };
    let first = cancel().await;
    let again = cancel().await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.header("idempotent-replayed"), Some("true"));
    assert_eq!(again.json, first.json);
    assert_eq!(told(&test, &queued).await, ["message.cancelled"]);

    let resolve = || {
        app.post(&format!("/v1/messages/{uncertain}/resolve"))
            .bearer(&acme.key)
            .idempotency("resolve-once")
            .json(json!({ "state": "sent", "evidence": "Found in the Sent folder." }))
            .send()
    };
    let first = resolve().await;
    let again = resolve().await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.header("idempotent-replayed"), Some("true"));
    assert_eq!(again.json, first.json);
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_events WHERE message_id = $1")
            .bind(uncertain.parse::<Id<Message>>().unwrap().uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(events, 1);
}

/// Reading a message or a delivery event whose period left the database answers `404 archived`,
/// whose detail points at exports, rather than `not_found`: so a client holding an old id learns
/// where its row went. A period has left when the id's instant is older than the table's online
/// window (messages 7 days, events 3), or when the leaf that covered it was dropped, which also
/// covers a window lengthened after the drop. An absent id of a live period stays `not_found`,
/// and so does any id of a table that keeps no periods (inbound messages).
#[tokio::test]
async fn an_id_of_an_archived_period_answers_archived() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let read = |path: String| app.get(&path).bearer(&acme.key).send();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days_ago = |days: u64| {
        Uuid::new_v7(uuid::Timestamp::from_unix(
            uuid::NoContext,
            now - days * 86_400,
            0,
        ))
    };

    let old_message = Id::<Message>::from_uuid(days_ago(10));
    let old_event = Id::<DeliveryEvent>::from_uuid(days_ago(5));
    for path in [
        format!("/v1/messages/{old_message}"),
        format!("/v1/delivery_events/{old_event}"),
    ] {
        let reply = read(path).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.json["code"], "archived", "{}", reply.json);
        assert!(
            reply.json["detail"]
                .as_str()
                .unwrap()
                .contains("POST /v1/exports")
        );
    }

    sqlx::query(
        "INSERT INTO partition_leaves (parent, name, lower, upper, dropped_at)
         VALUES ('messages', 'messages_dropped_early', now() - interval '3 days',
                 now() - interval '1 day', now())",
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    let in_dropped_leaf = Id::<Message>::from_uuid(days_ago(2));
    let reply = read(format!("/v1/messages/{in_dropped_leaf}")).await;
    assert_eq!(reply.json["code"], "archived", "{}", reply.json);

    let absent = Id::<Message>::from_uuid(Uuid::now_v7());
    let reply = read(format!("/v1/messages/{absent}")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.json["code"], "not_found");
    let inbound = Id::<InboundMessage>::from_uuid(days_ago(400));
    let reply = read(format!("/v1/inbound_messages/{inbound}")).await;
    assert_eq!(reply.json["code"], "not_found");
}
