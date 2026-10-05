//! Store tests of the outbox, the relay and webhook deliveries, against real PostgreSQL and a
//! local [`Sink`] playing the customer's server.
//!
//! The relay and the delivery kind run through the runner's [`Harness`] with the worker's own
//! login, so row security, the fences and the scheduler role are all in play; the sink records
//! every request, so a test can check what actually left the process.

use std::time::Duration;

use serde_json::{Value, json};
use strum::IntoEnumIterator as _;
use uuid::Uuid;

use super::EventType;
use super::deliver::{self, Deliver, Reopen, ReopenError, Sender};
use super::endpoints::{self, EndpointObject, NewEndpoint};
use super::outbox::{self, Event, Relay};
use crate::crypto;
use crate::domain::ids::{Id, WebhookDelivery, WorkspaceId};
use crate::domain::time::Timestamp;
use crate::jobs::runner::Harness;
use crate::jobs::{self, Queue, Registry, SYSTEM_WORKSPACE};
use crate::testing::{self, Sink, TestDb};

/// A runner of the relay and the delivery kind, as the worker registers them; `allow_private`
/// lets deliveries reach the loopback sink (the development switch).
pub(super) fn harness(test: &TestDb, allow_private: bool) -> Harness {
    let mut registry = Registry::default();
    registry
        .register::<Relay>()
        .unwrap()
        .register::<Deliver>()
        .unwrap();
    let mut env = http::Extensions::new();
    env.insert(testing::keys());
    env.insert(Sender::new(allow_private).unwrap());
    Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        env,
        "worker-test",
    )
}

/// Runs the relay once, as its schedule would.
pub(super) async fn relay(test: &TestDb, runner: &Harness) {
    let mut tx = test.worker.begin_in(SYSTEM_WORKSPACE).await.unwrap();
    jobs::enqueue(&mut tx, SYSTEM_WORKSPACE, &Relay {}, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let outcomes: Vec<&str> = runner
        .run_once(Queue::Maintenance, 1)
        .await
        .into_iter()
        .map(|(_, outcome)| outcome)
        .collect();
    assert_eq!(outcomes, ["done"]);
}

/// Runs every due delivery job once; returns their outcomes.
async fn deliver_due(runner: &Harness) -> Vec<&'static str> {
    runner
        .run_once(Queue::Webhooks, 16)
        .await
        .into_iter()
        .map(|(_, outcome)| outcome)
        .collect()
}

/// Creates an enabled endpoint of `workspace` at `url` for `types`, as the API does; it is
/// returned with its secret.
async fn endpoint(
    test: &TestDb,
    workspace: WorkspaceId,
    url: &str,
    types: &[EventType],
) -> EndpointObject {
    let mut tx = test.app.begin_in(workspace).await.unwrap();
    let created = endpoints::create(
        &mut tx,
        &testing::keys(),
        workspace,
        &NewEndpoint {
            url,
            event_types: types,
            enabled: true,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    created
}

/// Records an event of `kind` in `workspace` in a transaction of the api's login, as a business
/// change would.
async fn record(test: &TestDb, workspace: WorkspaceId, kind: EventType) -> Uuid {
    let mut tx = test.app.begin_in(workspace).await.unwrap();
    let id = outbox::record(
        &mut tx,
        workspace,
        Event {
            kind,
            subject_type: "message",
            subject_id: Uuid::now_v7(),
            data: json!({ "message_id": "msg_0190f8a2b4c87a10b6d2e4f6a8c0e2f4" }),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id.uuid()
}

/// A delivery's row, as the tests inspect it.
#[derive(Debug)]
struct DeliveryRow {
    id: Uuid,
    endpoint_id: Uuid,
    state: String,
    attempt: i16,
    response_status: Option<i16>,
    response_excerpt: Option<String>,
    next_in_seconds: f64,
}

/// Every delivery, by event then delivery.
async fn deliveries(test: &TestDb) -> Vec<DeliveryRow> {
    sqlx::query_as!(
        DeliveryRow,
        r#"SELECT id, endpoint_id, state, attempt, response_status, response_excerpt,
                  extract(epoch FROM next_attempt_at - now())::float8 AS "next_in_seconds!"
             FROM webhook_deliveries ORDER BY event_id, id"#
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap()
}

/// An endpoint's `enabled`, `disabled_reason` and whether it records a failure.
async fn health(test: &TestDb, endpoint: &EndpointObject) -> (bool, Option<String>, bool) {
    let row = sqlx::query!(
        "SELECT enabled, disabled_reason, failing_since IS NOT NULL AS failing FROM webhook_endpoints WHERE id = $1",
        endpoint.id.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    (
        row.enabled,
        row.disabled_reason,
        row.failing.unwrap_or(false),
    )
}

/// Every event type reads back from its own wire name and an unknown name is refused, so a type
/// can be neither misspelled in a subscription nor silently dropped.
#[test]
fn event_types_round_trip_and_refuse_unknown_names() {
    for kind in EventType::iter() {
        let wire = serde_json::to_value(kind).unwrap();
        assert_eq!(wire, Value::String(kind.as_str().to_owned()));
        assert_eq!(serde_json::from_value::<EventType>(wire).unwrap(), kind);
    }
    assert!(serde_json::from_value::<EventType>(json!("message.exploded")).is_err());
}

/// The relay publishes each event once: one delivery per enabled endpoint subscribed to its
/// type, the event marked published, one delivery job per delivery; a disabled or unsubscribed
/// endpoint gets nothing, and running the relay again creates nothing more.
#[tokio::test]
async fn the_relay_publishes_each_event_once() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let runner = harness(&test, true);
    let sent_a = endpoint(
        &test,
        acme.id,
        "https://a.example/hooks",
        &[EventType::MessageSent],
    )
    .await;
    let sent_b = endpoint(
        &test,
        acme.id,
        "https://b.example/hooks",
        &[EventType::MessageSent, EventType::MessageFailed],
    )
    .await;
    let failed_only = endpoint(
        &test,
        acme.id,
        "https://c.example/hooks",
        &[EventType::MessageFailed],
    )
    .await;
    let disabled = endpoint(
        &test,
        acme.id,
        "https://d.example/hooks",
        &[EventType::MessageSent],
    )
    .await;
    let mut tx = test.app.begin_in(acme.id).await.unwrap();
    endpoints::disable(
        &mut tx,
        acme.id,
        disabled.id.uuid(),
        endpoints::DisabledReason::Manual,
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    record(&test, acme.id, EventType::MessageSent).await;
    record(&test, acme.id, EventType::MessageSent).await;
    record(&test, acme.id, EventType::MessageFailed).await;

    relay(&test, &runner).await;
    let created = deliveries(&test).await;
    let per_endpoint = |endpoint: &EndpointObject| {
        created
            .iter()
            .filter(|row| row.endpoint_id == endpoint.id.uuid())
            .count()
    };
    assert_eq!(
        (
            per_endpoint(&sent_a),
            per_endpoint(&sent_b),
            per_endpoint(&failed_only),
            per_endpoint(&disabled)
        ),
        (2, 3, 1, 0)
    );
    let unpublished = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM outbox_events WHERE published_at IS NULL"#
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    // The `webhook_endpoint.disabled` event of the disabled endpoint is published too.
    assert_eq!(unpublished, 0);
    let jobs = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM jobs WHERE kind = 'webhook.deliver'"#
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(jobs, 6);

    relay(&test, &runner).await;
    assert_eq!(deliveries(&test).await.len(), 6);
}

/// An event addressed to one endpoint (an endpoint's test) reaches that endpoint even when it
/// does not subscribe to the type, and no other endpoint.
#[tokio::test]
async fn an_addressed_event_reaches_only_its_endpoint() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let runner = harness(&test, true);
    let target = endpoint(
        &test,
        acme.id,
        "https://a.example/hooks",
        &[EventType::MessageSent],
    )
    .await;
    endpoint(
        &test,
        acme.id,
        "https://b.example/hooks",
        &[EventType::EndpointTest],
    )
    .await;
    let mut tx = test.app.begin_in(acme.id).await.unwrap();
    outbox::record_synthetic(&mut tx, acme.id, EventType::EndpointTest, Some(target.id))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    relay(&test, &runner).await;
    let created = deliveries(&test).await;
    assert_eq!(
        created
            .iter()
            .map(|row| row.endpoint_id)
            .collect::<Vec<_>>(),
        [target.id.uuid()]
    );
}

/// A delivery reaches the customer's server signed per Standard Webhooks: `webhook-id` is the
/// delivery's id, `webhook-timestamp` the attempt's time, and `webhook-signature` verifies with
/// the endpoint's secret over `id.timestamp.body`; the body is `{type, timestamp, data}`; the
/// delivery records the attempt as delivered and the job completes.
#[tokio::test]
async fn a_delivery_is_signed_per_standard_webhooks_and_recorded() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let sink = Sink::start().await;
    let runner = harness(&test, true);
    let target = endpoint(&test, acme.id, &sink.url("/200"), &[EventType::MessageSent]).await;
    record(&test, acme.id, EventType::MessageSent).await;
    relay(&test, &runner).await;
    assert_eq!(deliver_due(&runner).await, ["done"]);

    let requests = sink.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(
        (request.method.as_str(), request.path.as_str()),
        ("POST", "/200")
    );
    assert_eq!(request.header("content-type"), Some("application/json"));
    let delivery = &deliveries(&test).await[0];
    let webhook_id = request.header("webhook-id").unwrap();
    assert_eq!(
        webhook_id,
        Id::<WebhookDelivery>::from_uuid(delivery.id).to_string()
    );
    let timestamp: i64 = request
        .header("webhook-timestamp")
        .unwrap()
        .parse()
        .unwrap();
    assert!((crate::process::now().0.as_second() - timestamp).abs() < 60);
    let key = deliver::secret_bytes(target.secret.as_deref().unwrap()).unwrap();
    assert_eq!(
        request.header("webhook-signature").unwrap(),
        crypto::sign_webhook(&key, webhook_id, timestamp, &request.body)
    );
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["type"], "message.sent");
    assert_eq!(
        body["data"],
        json!({ "message_id": "msg_0190f8a2b4c87a10b6d2e4f6a8c0e2f4" })
    );
    assert!(body["timestamp"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        (
            delivery.state.as_str(),
            delivery.attempt,
            delivery.response_status
        ),
        ("delivered", 1, Some(200))
    );
    assert_eq!(health(&test, &target).await, (true, None, false));
}

/// A failed attempt keeps the delivery pending until the schedule's next step (5 seconds after
/// the first failure), records the answer, marks the endpoint as failing, and leaves the job
/// waiting without counting a failure of its own.
#[tokio::test]
async fn a_failed_attempt_waits_for_the_next_step_of_the_schedule() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let sink = Sink::start().await;
    let runner = harness(&test, true);
    let target = endpoint(&test, acme.id, &sink.url("/500"), &[EventType::MessageSent]).await;
    record(&test, acme.id, EventType::MessageSent).await;
    relay(&test, &runner).await;
    assert_eq!(deliver_due(&runner).await, ["yield"]);
    let delivery = &deliveries(&test).await[0];
    assert_eq!(
        (
            delivery.state.as_str(),
            delivery.attempt,
            delivery.response_status
        ),
        ("pending", 1, Some(500))
    );
    assert_eq!(delivery.response_excerpt.as_deref(), Some("sink"));
    assert!(
        delivery.next_in_seconds > 1.0 && delivery.next_in_seconds <= 5.5,
        "{}",
        delivery.next_in_seconds
    );
    assert_eq!(health(&test, &target).await, (true, None, true));
    let job = sqlx::query!("SELECT state, attempts FROM jobs WHERE kind = 'webhook.deliver'")
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!((job.state.as_str(), job.attempts), ("available", 0));
}

/// A `503` with `Retry-After` sets the next attempt to the peer's wait instead of the schedule's.
#[tokio::test]
async fn retry_after_sets_the_next_attempt() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let sink = Sink::start().await;
    let runner = harness(&test, true);
    endpoint(
        &test,
        acme.id,
        &sink.url("/503?retry_after=30"),
        &[EventType::MessageSent],
    )
    .await;
    record(&test, acme.id, EventType::MessageSent).await;
    relay(&test, &runner).await;
    assert_eq!(deliver_due(&runner).await, ["yield"]);
    let delivery = &deliveries(&test).await[0];
    assert_eq!(delivery.response_status, Some(503));
    assert!(
        delivery.next_in_seconds > 20.0 && delivery.next_in_seconds <= 30.5,
        "{}",
        delivery.next_in_seconds
    );
}

/// The tenth failed attempt ends the schedule: the delivery is `failed` and its job completes.
#[tokio::test]
async fn the_tenth_failure_fails_the_delivery() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let sink = Sink::start().await;
    let runner = harness(&test, true);
    endpoint(&test, acme.id, &sink.url("/500"), &[EventType::MessageSent]).await;
    record(&test, acme.id, EventType::MessageSent).await;
    relay(&test, &runner).await;
    sqlx::query!("UPDATE webhook_deliveries SET attempt = 9")
        .execute(test.system.pool())
        .await
        .unwrap();
    assert_eq!(deliver_due(&runner).await, ["done"]);
    let delivery = &deliveries(&test).await[0];
    assert_eq!((delivery.state.as_str(), delivery.attempt), ("failed", 10));
}

/// An endpoint that answers `410 Gone` is disabled at once: its other pending deliveries stop
/// without a request, and a `webhook_endpoint.disabled` event is recorded for the endpoints that
/// still work.
#[tokio::test]
async fn a_410_disables_the_endpoint_and_stops_its_deliveries() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let sink = Sink::start().await;
    let runner = harness(&test, true);
    let gone = endpoint(&test, acme.id, &sink.url("/410"), &[EventType::MessageSent]).await;
    record(&test, acme.id, EventType::MessageSent).await;
    record(&test, acme.id, EventType::MessageSent).await;
    relay(&test, &runner).await;
    assert_eq!(runner.run_once(Queue::Webhooks, 1).await.len(), 1);
    assert_eq!(health(&test, &gone).await.1.as_deref(), Some("gone"));
    let states: Vec<String> = deliveries(&test)
        .await
        .into_iter()
        .map(|row| row.state)
        .collect();
    assert_eq!(states, ["disabled", "disabled"]);
    assert_eq!(deliver_due(&runner).await, ["done"]);
    assert_eq!(sink.requests().len(), 1);
    let disabled_events = sqlx::query_scalar!(
        r#"SELECT payload -> 'data' ->> 'disabled_reason' AS "reason!" FROM outbox_events WHERE type = 'webhook_endpoint.disabled'"#
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert_eq!(disabled_events, ["gone"]);
}

/// An endpoint that has failed for five days without a success is disabled as `failing` by its
/// next failed attempt.
#[tokio::test]
async fn five_days_of_failures_disable_the_endpoint() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let sink = Sink::start().await;
    let runner = harness(&test, true);
    let failing = endpoint(&test, acme.id, &sink.url("/500"), &[EventType::MessageSent]).await;
    sqlx::query!("UPDATE webhook_endpoints SET failing_since = now() - interval '5 days 1 minute'")
        .execute(test.system.pool())
        .await
        .unwrap();
    record(&test, acme.id, EventType::MessageSent).await;
    relay(&test, &runner).await;
    deliver_due(&runner).await;
    assert_eq!(health(&test, &failing).await.1.as_deref(), Some("failing"));
}

/// Without the development switch the worker sends nothing to an inward target: plain `http`
/// and a loopback address are refused before any request, and the refusal is recorded as the
/// attempt's outcome.
#[tokio::test]
async fn the_guard_sends_nothing_to_inward_targets() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let sink = Sink::start().await;
    let runner = harness(&test, false);
    let plain = endpoint(&test, acme.id, &sink.url("/200"), &[EventType::MessageSent]).await;
    let loopback = endpoint(
        &test,
        acme.id,
        &sink.url("/200").replacen("http://", "https://", 1),
        &[EventType::MessageSent],
    )
    .await;
    record(&test, acme.id, EventType::MessageSent).await;
    relay(&test, &runner).await;
    assert_eq!(deliver_due(&runner).await, ["yield", "yield"]);
    assert!(sink.requests().is_empty());
    for row in deliveries(&test).await {
        let expected = if row.endpoint_id == plain.id.uuid() {
            "refused: the endpoint must use https"
        } else {
            assert_eq!(row.endpoint_id, loopback.id.uuid());
            "refused: the endpoint's host is a private, loopback or reserved address"
        };
        assert_eq!(
            (row.response_excerpt.as_deref(), row.response_status),
            (Some(expected), None)
        );
    }
}

/// Reopening (a manual retry, a replay) makes deliveries pending now with a job to run them,
/// within the replay window only: events older than the online retention minus one partition
/// period are refused, so no reopening touches a partition the archive may be examining.
#[tokio::test]
async fn reopening_respects_the_replay_window() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let sink = Sink::start().await;
    let runner = harness(&test, true);
    let target = endpoint(&test, acme.id, &sink.url("/200"), &[EventType::MessageSent]).await;
    record(&test, acme.id, EventType::MessageSent).await;
    relay(&test, &runner).await;
    assert_eq!(deliver_due(&runner).await, ["done"]);
    let delivered = Id::<WebhookDelivery>::from_uuid(deliveries(&test).await[0].id);

    let mut tx = test.app.begin_in(acme.id).await.unwrap();
    assert_eq!(
        deliver::reopen(&mut tx, acme.id, Reopen::Delivery(delivered))
            .await
            .unwrap(),
        [delivered]
    );
    let a_month_ago = crate::process::now().minus(Duration::from_secs(30 * 86_400));
    let too_old = deliver::reopen(
        &mut tx,
        acme.id,
        Reopen::Endpoint {
            endpoint: target.id,
            since: a_month_ago,
        },
    )
    .await;
    assert!(matches!(too_old, Err(ReopenError::TooOld)));
    tx.commit().await.unwrap();
    let delivery = &deliveries(&test).await[0];
    assert_eq!(delivery.state, "pending");
    assert!(delivery.next_in_seconds <= 0.0);
    assert_eq!(deliver_due(&runner).await, ["done"]);
    assert_eq!(sink.requests().len(), 2);
}

/// The bounds a delivery id implies are exactly the schema's own partition boundaries for its
/// event's millisecond, so a lookup by delivery id prunes to the right partition.
#[tokio::test]
async fn delivery_bounds_match_the_schemas_boundaries() {
    let test = TestDb::new().await;
    for millis in [
        0_i64,
        1_759_276_800_000,
        1_759_363_199_999,
        4_102_444_800_123,
    ] {
        let at = Timestamp(jiff::Timestamp::from_millisecond(millis).unwrap());
        let next = Timestamp(jiff::Timestamp::from_millisecond(millis + 1).unwrap());
        let bounds = sqlx::query!(
            r#"SELECT uuidv7_boundary($1) AS "low!", uuidv7_boundary($2) AS "high!""#,
            at as _,
            next as _
        )
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        assert_eq!(
            deliver::event_bounds(bounds.low),
            (bounds.low, bounds.high),
            "{millis}"
        );
    }
}

/// A runner of the failing-endpoint emails, with the deployment's keys.
fn mailer(test: &TestDb) -> Harness {
    let mut registry = Registry::default();
    registry.register::<deliver::FailureEmail>().unwrap();
    let mut env = http::Extensions::new();
    env.insert(testing::keys());
    Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        env,
        "mail-test",
    )
}

/// Whether each failing-endpoint email asked for so far is the disabled one, oldest first.
async fn notices(test: &TestDb) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT payload ->> 'disabled' FROM jobs WHERE kind = 'webhook_endpoint.failure_email' ORDER BY id",
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap()
}

/// Records an event, relays it and fails its delivery's tenth and last attempt.
async fn exhaust_a_delivery(test: &TestDb, runner: &Harness, workspace: WorkspaceId) {
    record(test, workspace, EventType::MessageSent).await;
    relay(test, runner).await;
    sqlx::query("UPDATE webhook_deliveries SET attempt = 9 WHERE state = 'pending'")
        .execute(test.system.pool())
        .await
        .unwrap();
    assert_eq!(deliver_due(runner).await, ["done"]);
}

/// The admins of a workspace hear of a failing endpoint twice at most per failing period: when
/// its first delivery uses up its retries (a later one tells nobody), and when five days of
/// failures disable it. A member is not told; the emails are the platform's transactional mail.
#[tokio::test]
async fn a_failing_endpoint_is_told_to_the_admins_once_then_when_disabled() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let acme = test.workspace("acme").await;
    sqlx::query(
        "WITH u AS (INSERT INTO users (email, email_verified_at) VALUES ('max@acme.example', now()) RETURNING id)
         INSERT INTO memberships (workspace_id, user_id, role) SELECT $1, id, 'member' FROM u",
    )
    .bind(acme.id.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();
    let sink = Sink::start().await;
    let runner = harness(&test, true);
    let failing = endpoint(&test, acme.id, &sink.url("/500"), &[EventType::MessageSent]).await;
    exhaust_a_delivery(&test, &runner, acme.id).await;
    exhaust_a_delivery(&test, &runner, acme.id).await;
    assert_eq!(notices(&test).await, ["false"]);
    let mailer = mailer(&test);
    let sent: Vec<&str> = mailer
        .run_once(Queue::Transactional, 4)
        .await
        .into_iter()
        .map(|(_, outcome)| outcome)
        .collect();
    assert_eq!(sent, ["done"]);

    sqlx::query("UPDATE webhook_endpoints SET failing_since = now() - interval '5 days 1 minute'")
        .execute(test.system.pool())
        .await
        .unwrap();
    record(&test, acme.id, EventType::MessageSent).await;
    relay(&test, &runner).await;
    deliver_due(&runner).await;
    assert_eq!(health(&test, &failing).await.1.as_deref(), Some("failing"));
    assert_eq!(notices(&test).await, ["false", "true"]);
    assert_eq!(mailer.run_once(Queue::Transactional, 4).await.len(), 1);

    let mails: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT to_addresses[1], subject, text_body FROM messages
          WHERE workspace_id = $1 AND kind = 'transactional' ORDER BY subject, to_addresses[1]",
    )
    .bind(SYSTEM_WORKSPACE.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    let told: Vec<(&str, &str)> = mails
        .iter()
        .map(|(to, subject, _)| (to.as_str(), subject.as_str()))
        .collect();
    assert_eq!(
        told,
        [
            (
                "owner@acme.example",
                "A webhook endpoint of acme keeps failing"
            ),
            (
                "owner@acme.example",
                "A webhook endpoint of acme was disabled"
            ),
        ]
    );
    for (_, _, text) in &mails {
        assert!(text.contains(&sink.url("/500")), "{text}");
        assert!(text.contains("The last attempt met: HTTP 500"), "{text}");
    }
}

/// A success ends an endpoint's failing period, so the first delivery to use up its retries in
/// the next period tells the admins again.
#[tokio::test]
async fn a_success_ends_the_failing_period() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let acme = test.workspace("acme").await;
    let sink = Sink::start().await;
    let runner = harness(&test, true);
    let flaky = endpoint(&test, acme.id, &sink.url("/500"), &[EventType::MessageSent]).await;
    exhaust_a_delivery(&test, &runner, acme.id).await;
    // The first period's email goes out, so the next period's is a job of its own.
    assert_eq!(
        mailer(&test).run_once(Queue::Transactional, 4).await.len(),
        1
    );
    let point = |path: &str| {
        sqlx::query("UPDATE webhook_endpoints SET url = $1 WHERE id = $2")
            .bind(sink.url(path))
            .bind(flaky.id.uuid())
            .execute(test.system.pool())
    };
    point("/200").await.unwrap();
    record(&test, acme.id, EventType::MessageSent).await;
    relay(&test, &runner).await;
    assert_eq!(deliver_due(&runner).await, ["done"]);
    let cleared: (bool, bool) = sqlx::query_as(
        "SELECT failing_since IS NULL, failure_notified_at IS NULL FROM webhook_endpoints WHERE id = $1",
    )
    .bind(flaky.id.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(cleared, (true, true));
    point("/500").await.unwrap();
    exhaust_a_delivery(&test, &runner, acme.id).await;
    assert_eq!(notices(&test).await, ["false", "false"]);
}

/// Custom authentication stays sealed and is sent after rotation; frozen connection filters
/// exclude unrelated events while an explicitly addressed test still reaches the endpoint.
#[tokio::test]
async fn subscription_filters_and_sealed_headers_reach_the_delivery() {
    use crate::domain::ids::{Connection, Id};
    let test = TestDb::new().await;
    let workspace = test.workspace("filtered-hooks").await;
    let sink = Sink::start().await;
    let runner = harness(&test, true);
    let connection = Id::<Connection>::new();
    let app = test.app();
    let created=app.post("/v1/webhook_endpoints").bearer(&workspace.key).idempotency(&Uuid::now_v7().to_string()).json(json!({"url":sink.url("/200"),"event_types":["message.sent"],"filters":{"connection_ids":[connection]},"headers":{"Authorization":"Bearer customer-credential"}})).send().await;
    assert_eq!(
        created.status,
        http::StatusCode::CREATED,
        "{}",
        created.json
    );
    let id = created.json["id"].as_str().unwrap();
    assert_eq!(created.json["header_names"], json!(["authorization"]));
    assert!(!created.json.to_string().contains("customer-credential"));
    assert_eq!(
        app.post(&format!("/v1/webhook_endpoints/{id}/rotate_secret"))
            .bearer(&workspace.key)
            .idempotency(&Uuid::now_v7().to_string())
            .json(json!({}))
            .send()
            .await
            .status,
        http::StatusCode::OK
    );
    let mut tx = test.app.begin_in(workspace.id).await.unwrap();
    for matching in [false, true] {
        outbox::record(&mut tx,workspace.id,Event{kind:EventType::MessageSent,subject_type:"message",subject_id:Uuid::now_v7(),data:json!({"connection_id":if matching {connection}else{Id::<Connection>::new()}})}).await.unwrap();
    }
    tx.commit().await.unwrap();
    relay(&test, &runner).await;
    assert_eq!(deliver_due(&runner).await, ["done"]);
    assert_eq!(sink.requests().len(), 1);
    assert_eq!(
        sink.requests().first().unwrap().header("authorization"),
        Some("Bearer customer-credential")
    );
    let read = app
        .get(&format!("/v1/webhook_endpoints/{id}"))
        .bearer(&workspace.key)
        .send()
        .await;
    assert_eq!(read.json["filters"]["connection_ids"], json!([connection]));
    assert!(read.json.get("secret").is_none());
}
