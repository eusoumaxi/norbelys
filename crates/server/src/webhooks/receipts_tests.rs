//! Tests of provider evidence from webhooks: the ingress (verification per provider, receipts
//! committed once, replays, quarantine, the spool) and the normaliser (evidence recorded once,
//! matched to our messages, settling uncertain ones), against real PostgreSQL.
//!
//! Callbacks are signed here as each provider signs them (Mailgun's HMAC, SendGrid's ECDSA, the
//! managed MTA's Standard Webhooks HMAC, an SNS RSA signature whose certificate the test preloads
//! into the ingress), so the router verifies them exactly as in production. The normaliser runs
//! through the job runner's harness as the worker's own login, so row security applies.

use std::time::Duration;

use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair as RsaKeyPair, KeySize};
use aws_lc_rs::signature::{
    ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _, RSA_PKCS1_SHA256,
};
use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use norbelys_mail::webhooks::Receipt;
use secrecy::SecretString;
use serde_json::{Value, json};
use uuid::Uuid;

use super::ingress::{self, Arrival, Ingress, Kept, Stored, Unavailable};
use super::normalize::Normalize;
use crate::crypto;
use crate::db::{Database, PoolSettings};
use crate::delivery::accept;
use crate::domain::ids::{Id, Message, ProviderWebhook, Thread, WorkspaceId};
use crate::domain::senders::Provider;
use crate::domain::time::Timestamp;
use crate::jobs::runner::Harness;
use crate::jobs::{self, Queue, Registry};
use crate::senders::credentials;
use crate::spool::{Limits, Spool};
use crate::testing::{self, SenderSpec, Sink, TestApp, TestDb, TestSender};

const ARN: &str = "arn:aws:sns:us-east-1:123456789012:ses-events";
const CERT_URL: &str =
    "https://sns.us-east-1.amazonaws.com/SimpleNotificationService-0123456789abcdef.pem";
const MAILGUN_KEY: &str = "key-3ax6xnjp29jd6fds4gc373sgvjxteol0";

/// A connection of one workspace and its provider webhook.
struct Hook {
    workspace: WorkspaceId,
    /// An API key of the workspace with every scope.
    api_key: String,
    sender: TestSender,
    webhook: Id<ProviderWebhook>,
}

impl Hook {
    fn path(&self) -> String {
        format!("/webhooks/{}", self.webhook)
    }
}

/// A `provider` connection of a new workspace and its webhook, whose verification material is
/// `key` (`None`: not set yet).
async fn hook(test: &TestDb, provider: &'static str, key: Option<&str>) -> Hook {
    let created = test
        .workspace(&format!("hook{}", Uuid::now_v7().simple()))
        .await;
    let workspace = created.id;
    // An SES connection always shares its account's quota scope.
    let scope = if provider == "ses" {
        Some(
            test.quota_scope(workspace, provider, Some(50_000), None)
                .await,
        )
    } else {
        None
    };
    let sender = test
        .sender(
            workspace,
            &SenderSpec {
                provider,
                scope,
                ..SenderSpec::relay(&format!("{provider}@example.test"))
            },
        )
        .await;
    let webhook: Uuid = sqlx::query_scalar(
        "INSERT INTO provider_webhooks (workspace_id, connection_id, provider, name) VALUES ($1, $2, $3, 'Events')
         RETURNING id",
    )
    .bind(workspace.uuid())
    .bind(sender.connection.uuid())
    .bind(provider)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    let webhook = Id::from_uuid(webhook);
    if let Some(key) = key {
        let sealed = credentials::seal_webhook_key(
            &testing::keys(),
            workspace,
            webhook,
            &SecretString::from(key.to_owned()),
        )
        .unwrap();
        sqlx::query("UPDATE provider_webhooks SET signing_secret = $2 WHERE id = $1")
            .bind(webhook.uuid())
            .bind(sealed)
            .execute(test.system.pool())
            .await
            .unwrap();
    }
    Hook {
        workspace: created.id,
        api_key: created.key,
        sender,
        webhook,
    }
}

/// A message of `hook`'s connection to `to`, already `uncertain` when `uncertain`.
async fn message(test: &TestDb, hook: &Hook, to: &[&str], uncertain: bool) -> Id<Message> {
    let message = test
        .direct_message(hook.workspace, &hook.sender, to, 0)
        .await;
    if uncertain {
        sqlx::query("UPDATE messages SET state = 'uncertain' WHERE id = $1")
            .bind(message.uuid())
            .execute(test.system.pool())
            .await
            .unwrap();
    }
    message
}

fn arrival(hook: &Hook, provider: Provider, receipts: &[(&str, &[u8])]) -> Arrival {
    Arrival {
        workspace: hook.workspace,
        webhook: hook.webhook,
        provider,
        received_at: crate::process::now(),
        receipts: receipts
            .iter()
            .map(|(event_id, raw)| Receipt {
                event_id: (*event_id).to_owned(),
                raw: raw.to_vec(),
            })
            .collect(),
    }
}

/// The receipts of `hook`'s webhook: event id, state, whether the body is kept.
async fn receipts(test: &TestDb, hook: &Hook) -> Vec<(String, String, bool)> {
    sqlx::query_as(
        "SELECT event_id, state, raw IS NOT NULL FROM webhook_receipts WHERE provider_webhook_id = $1 ORDER BY id",
    )
    .bind(hook.webhook.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap()
}

/// The receipts.normalize jobs waiting, by their receipts.
async fn normalize_jobs(test: &TestDb) -> Vec<Value> {
    sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'receipts.normalize' ORDER BY id")
        .fetch_all(test.system.pool())
        .await
        .unwrap()
}

/// A runner of the normaliser as the worker registers it.
fn normaliser(test: &TestDb) -> Harness {
    let mut registry = Registry::default();
    registry.register::<Normalize>().unwrap();
    let mut env = http::Extensions::new();
    env.insert(testing::keys());
    Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        env,
        "worker-test",
    )
}

/// Runs every due normalisation; returns their outcomes.
async fn normalize_due(runner: &Harness) -> Vec<&'static str> {
    runner
        .run_once(Queue::Receipts, 16)
        .await
        .into_iter()
        .map(|(_, outcome)| outcome)
        .collect()
}

/// The delivery events of `hook`'s receipts: message, kind, category, confidence, recipient.
async fn events(
    test: &TestDb,
    hook: &Hook,
) -> Vec<(Option<Uuid>, String, String, String, Option<String>)> {
    sqlx::query_as(
        "SELECT e.message_id, e.kind, e.category, e.confidence, e.recipient_email
           FROM delivery_events e JOIN webhook_receipts r ON r.workspace_id = e.workspace_id AND r.id = e.receipt_id
          WHERE r.provider_webhook_id = $1 AND e.source = 'provider_webhook' ORDER BY e.id",
    )
    .bind(hook.webhook.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap()
}

async fn state(test: &TestDb, message: Id<Message>) -> String {
    sqlx::query_scalar("SELECT state FROM messages WHERE id = $1")
        .bind(message.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap()
}

fn sendgrid_event(
    event_id: &str,
    event: &str,
    message: Option<Id<Message>>,
    extra: Value,
) -> Vec<u8> {
    let mut body = json!({"email": "grace@example.org", "event": event, "sg_event_id": event_id,
                          "timestamp": jiff::Timestamp::now().as_second()});
    if let Some(message) = message {
        body["norbelys_message_id"] = json!(message.uuid().to_string());
    }
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        body.extend(extra.clone());
    }
    serde_json::to_vec(&body).unwrap()
}

// ───────────────────────────── receipts: committed once ─────────────────────────────

/// A provider retries until it hears success, sometimes days later, and our key window is longer
/// than its retries: a replayed event is answered as stored and stores nothing, whether it comes
/// at once or after the day's receipts partition is long gone, so no event is applied twice.
#[tokio::test]
async fn a_replay_is_answered_and_stores_nothing_whenever_it_comes() {
    let test = TestDb::new().await;
    let hook = hook(&test, "sendgrid", None).await;
    let body = sendgrid_event("sg-1", "delivered", None, json!({}));
    let once = arrival(&hook, Provider::Sendgrid, &[("sg-1", &body)]);
    let first = ingress::store(&test.app, hook.workspace, &[&once])
        .await
        .unwrap();
    assert_eq!(
        first,
        [Stored {
            received: 1,
            duplicates: 0,
            quarantined: 0
        }]
    );
    let again = ingress::store(&test.app, hook.workspace, &[&once])
        .await
        .unwrap();
    assert_eq!(
        again,
        [Stored {
            received: 0,
            duplicates: 1,
            quarantined: 0
        }]
    );
    // Two days later: the key was written then, and still stands.
    sqlx::query("UPDATE provider_event_keys SET received_at = now() - interval '2 days' WHERE event_id = 'sg-1'")
        .execute(test.system.pool())
        .await
        .unwrap();
    let later = ingress::store(&test.app, hook.workspace, &[&once])
        .await
        .unwrap();
    assert_eq!(
        later,
        [Stored {
            received: 0,
            duplicates: 1,
            quarantined: 0
        }]
    );
    assert_eq!(
        receipts(&test, &hook).await,
        [("sg-1".to_owned(), "received".to_owned(), true)]
    );
    assert_eq!(normalize_jobs(&test).await.len(), 1);
}

/// Providers deliver the same event from two servers at once: two concurrent transactions over
/// the same new key store it exactly once (one waits on the other's key, then sees it), and the
/// other answers success with nothing stored.
#[tokio::test]
async fn two_concurrent_deliveries_of_one_event_store_it_once() {
    let test = TestDb::new().await;
    let hook = hook(&test, "sendgrid", None).await;
    for round in 0..5 {
        let event_id = format!("sg-race-{round}");
        let body = sendgrid_event(&event_id, "delivered", None, json!({}));
        let one = arrival(&hook, Provider::Sendgrid, &[(&event_id, &body)]);
        let batch = [&one];
        // The test's api pool has two connections: each store runs on its own.
        let (a, b) = tokio::join!(
            ingress::store(&test.app, hook.workspace, &batch),
            ingress::store(&test.app, hook.workspace, &batch),
        );
        let mut outcomes = [a.unwrap()[0], b.unwrap()[0]];
        outcomes.sort_by_key(|stored| stored.received);
        assert_eq!(
            outcomes,
            [
                Stored {
                    received: 0,
                    duplicates: 1,
                    quarantined: 0
                },
                Stored {
                    received: 1,
                    duplicates: 0,
                    quarantined: 0
                },
            ]
        );
    }
    assert_eq!(receipts(&test, &hook).await.len(), 5);
    assert_eq!(normalize_jobs(&test).await.len(), 5);
}

/// A replay must repeat the first body: one whose body differs (in a later delivery, or twice in
/// one batch) is kept `quarantined` with its body for review and never normalised, while the
/// first body and its key stand.
#[tokio::test]
async fn a_replay_with_another_body_is_quarantined_and_the_first_stands() {
    let test = TestDb::new().await;
    let hook = hook(&test, "sendgrid", None).await;
    let first = sendgrid_event("sg-q", "delivered", None, json!({}));
    let changed = sendgrid_event("sg-q", "bounce", None, json!({"status": "5.1.1"}));
    let stored = ingress::store(
        &test.app,
        hook.workspace,
        &[&arrival(&hook, Provider::Sendgrid, &[("sg-q", &first)])],
    )
    .await
    .unwrap();
    assert_eq!(stored[0].received, 1);
    let replayed = ingress::store(
        &test.app,
        hook.workspace,
        &[&arrival(&hook, Provider::Sendgrid, &[("sg-q", &changed)])],
    )
    .await
    .unwrap();
    assert_eq!(
        replayed,
        [Stored {
            received: 0,
            duplicates: 0,
            quarantined: 1
        }]
    );
    let in_batch = ingress::store(
        &test.app,
        hook.workspace,
        &[&arrival(
            &hook,
            Provider::Sendgrid,
            &[("sg-b", &first), ("sg-b", &changed), ("sg-b", &first)],
        )],
    )
    .await
    .unwrap();
    assert_eq!(
        in_batch,
        [Stored {
            received: 1,
            duplicates: 1,
            quarantined: 1
        }]
    );
    let kept: Vec<u8> =
        sqlx::query_scalar("SELECT body_hash FROM provider_event_keys WHERE event_id = 'sg-q'")
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(kept, crypto::sha256(&first));
    let states: Vec<(String, String, bool)> = receipts(&test, &hook).await;
    assert_eq!(
        states,
        [
            ("sg-q".to_owned(), "received".to_owned(), true),
            ("sg-q".to_owned(), "quarantined".to_owned(), true),
            ("sg-b".to_owned(), "received".to_owned(), true),
            ("sg-b".to_owned(), "quarantined".to_owned(), true),
        ]
    );
    let jobs = normalize_jobs(&test).await;
    let queued: usize = jobs
        .iter()
        .map(|job| job["receipts"].as_array().map_or(0, Vec::len))
        .sum();
    assert_eq!(queued, 2, "only the first bodies are normalised: {jobs:?}");
}

/// While the database cannot take a batch, a verified callback is written to the local spool and
/// answered (Mailgun never retries delivery notifications); without a spool it is refused for a
/// retry. The drain then stores it as of its arrival, and a callback spooled twice is still
/// stored once.
#[tokio::test]
async fn a_batch_the_database_refuses_is_spooled_then_drained() {
    let test = TestDb::new().await;
    let hook = hook(&test, "sendgrid", None).await;
    let unreachable = Database::connect_lazy(
        &SecretString::from("postgres://norbelys_app:norbelys@127.0.0.1:1/none".to_owned()),
        PoolSettings {
            application_name: "norbelys-test-unreachable",
            max_connections: 1,
            statement_timeout: Duration::from_secs(1),
            acquire_timeout: Duration::from_secs(1),
            min_connections: 0,
            request_deadline: None,
        },
    )
    .unwrap();
    let dir = std::env::temp_dir().join(format!("norbelys-ingress-{}", Uuid::now_v7().simple()));
    let spool: Spool<Arrival> = Spool::open(&dir, Limits::default()).unwrap();
    let body = sendgrid_event("sg-spool", "delivered", None, json!({}));
    let mut arrived = arrival(&hook, Provider::Sendgrid, &[("sg-spool", &body)]);
    arrived.received_at = Timestamp(jiff::Timestamp::now() - jiff::SignedDuration::from_mins(7));

    let refused = Ingress::start(unreachable.clone(), None)
        .keep(arrived.clone())
        .await;
    assert_eq!(refused, Err(Unavailable));
    let spooling = Ingress::start(unreachable, Some(spool.clone()));
    assert_eq!(spooling.keep(arrived.clone()).await, Ok(Kept::Spooled));
    assert_eq!(spooling.keep(arrived.clone()).await, Ok(Kept::Spooled));

    assert!(ingress::drain_once(&test.app, &spool).await.unwrap());
    assert!(!ingress::drain_once(&test.app, &spool).await.unwrap());
    assert_eq!(receipts(&test, &hook).await.len(), 1);
    let received: Timestamp = sqlx::query_scalar(
        r#"SELECT received_at FROM webhook_receipts WHERE event_id = 'sg-spool'"#,
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(received.0.as_second(), arrived.received_at.0.as_second());
    spool.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

// ───────────────────────────── normalisation ─────────────────────────────

/// Each receipt becomes its evidence once: a signed relay's delivery and hard bounce about our
/// campaign message are recorded authenticated (the bounce suppressing its address and, after the
/// commit, stopping the person's enrollment), the receipt is `normalized` with its body dropped,
/// the webhook notes its last event, and running the same batch again records nothing more.
#[tokio::test]
async fn normalisation_records_each_receipt_once() {
    let test = TestDb::new().await;
    let hook = hook(&test, "sendgrid", None).await;
    let campaign = test.campaign(hook.workspace, &hook.sender, None).await;
    let message = test
        .campaign_message(
            hook.workspace,
            campaign,
            &hook.sender,
            "grace@example.org",
            0,
        )
        .await;
    let delivered = sendgrid_event("sg-d", "delivered", Some(message), json!({}));
    let bounced = sendgrid_event(
        "sg-b",
        "bounce",
        Some(message),
        json!({"status": "5.1.1", "reason": "550 5.1.1 unknown user"}),
    );
    ingress::store(
        &test.app,
        hook.workspace,
        &[&arrival(
            &hook,
            Provider::Sendgrid,
            &[("sg-d", &delivered), ("sg-b", &bounced)],
        )],
    )
    .await
    .unwrap();
    let runner = normaliser(&test);
    assert_eq!(normalize_due(&runner).await, ["done"]);
    let recorded = events(&test, &hook).await;
    assert_eq!(
        recorded,
        [
            (
                Some(message.uuid()),
                "delivered".to_owned(),
                "delivered".to_owned(),
                "authenticated".to_owned(),
                Some("grace@example.org".to_owned())
            ),
            (
                Some(message.uuid()),
                "bounced".to_owned(),
                "invalid_recipient".to_owned(),
                "authenticated".to_owned(),
                Some("grace@example.org".to_owned())
            ),
        ]
    );
    let suppressed: Option<String> = sqlx::query_scalar(
        "SELECT reason FROM suppressions WHERE workspace_id = $1 AND email_key = 'grace@example.org'",
    )
    .bind(hook.workspace.uuid())
    .fetch_optional(test.system.pool())
    .await
    .unwrap();
    assert_eq!(suppressed.as_deref(), Some("bounce"));
    let enrollment: String =
        sqlx::query_scalar("SELECT status FROM enrollments WHERE campaign_id = $1")
            .bind(campaign)
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(enrollment, "stopped");
    assert!(
        receipts(&test, &hook)
            .await
            .iter()
            .all(|(_, state, kept)| state == "normalized" && !kept)
    );
    let noted: Option<Timestamp> =
        sqlx::query_scalar("SELECT last_event_at FROM provider_webhooks WHERE id = $1")
            .bind(hook.webhook.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert!(noted.is_some());

    // The same batch again (a recovered lease runs it twice): nothing is still `received`.
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM webhook_receipts WHERE provider_webhook_id = $1")
            .bind(hook.webhook.uuid())
            .fetch_all(test.system.pool())
            .await
            .unwrap();
    let mut tx = test.worker.begin_in(hook.workspace).await.unwrap();
    jobs::enqueue(&mut tx, hook.workspace, &Normalize { receipts: ids }, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(normalize_due(&runner).await, ["done"]);
    assert_eq!(events(&test, &hook).await.len(), 2);
}

/// An `uncertain` message is never sent again; a provider's later event that proves it took the
/// message settles it `sent`: an SES event naming it by our message tag (authenticated), and the
/// managed MTA's event naming it by the Message-ID we composed, whose tag verifies (a bounce
/// returned to its VERP address: corroborated, which proves acceptance but not the reporter).
#[tokio::test]
async fn a_provider_event_settles_an_uncertain_message() {
    let test = TestDb::new().await;
    let ses = hook(&test, "ses", None).await;
    let tagged = message(&test, &ses, &["ada@example.org"], true).await;
    let now = jiff::Timestamp::now().to_string();
    let delivery = json!({"eventType": "Delivery",
        "mail": {"timestamp": now, "messageId": "0100018f-ses-token", "tags": {"norbelys_message_id": [tagged.uuid().to_string()]}},
        "delivery": {"timestamp": now, "recipients": ["ada@example.org"], "smtpResponse": "250 2.0.0 OK"}});
    ingress::store(
        &test.app,
        ses.workspace,
        &[&arrival(
            &ses,
            Provider::Ses,
            &[("sns-1", delivery.to_string().as_bytes())],
        )],
    )
    .await
    .unwrap();

    let mta = hook(&test, "norbelys", None).await;
    let correlated = message(&test, &mta, &["alan@example.org"], true).await;
    let internet_id = accept::internet_message_id(
        &testing::keys(),
        correlated,
        Id::<Thread>::new(),
        "relay@example.test",
    );
    let bounce = json!({"event_id": "mta-1", "internet_message_id": internet_id, "recipient": "alan@example.org",
                        "kind": "bounced", "enhanced_status": "5.1.1", "detail": "550 5.1.1 no such user",
                        "provenance": "verp_dsn", "observed_at": now});
    ingress::store(
        &test.app,
        mta.workspace,
        &[&arrival(
            &mta,
            Provider::Norbelys,
            &[("mta-1", bounce.to_string().as_bytes())],
        )],
    )
    .await
    .unwrap();

    // One claim serves one workspace's lane: one run per workspace.
    let runner = normaliser(&test);
    assert_eq!(normalize_due(&runner).await, ["done"]);
    assert_eq!(normalize_due(&runner).await, ["done"]);
    assert_eq!(state(&test, tagged).await, "sent");
    assert_eq!(state(&test, correlated).await, "sent");
    assert_eq!(events(&test, &ses).await[0].3, "authenticated");
    let verp = &events(&test, &mta).await[0];
    assert_eq!(
        (verp.0, verp.3.as_str()),
        (Some(correlated.uuid()), "corroborated")
    );
    let suppressed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM suppressions WHERE workspace_id = $1")
            .bind(mta.workspace.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(
        suppressed, 0,
        "a corroborated bounce asks a person, it never suppresses"
    );
}

/// An authenticated managed-MTA policy deferral stays a temporary message event with the
/// provider's exact code and diagnostic, and does not suppress the named recipient.
#[tokio::test]
async fn a_managed_mta_policy_deferral_appears_in_message_history() {
    let test = TestDb::new().await;
    let mta = hook(&test, "norbelys", None).await;
    let sent = message(&test, &mta, &["alan@example.org"], false).await;
    let internet_id = accept::internet_message_id(
        &testing::keys(),
        sent,
        Id::<Thread>::new(),
        "relay@example.test",
    );
    let diagnostic = "550 5.7.1 unusual invalid recipients (JFE050004)";
    let deferred = json!({
        "event_id": "mta-policy-1",
        "internet_message_id": internet_id,
        "recipient": "alan@example.org",
        "kind": "deferred",
        "enhanced_status": "4.7.1",
        "detail": diagnostic,
        "provenance": "smtp_reply",
        "observed_at": jiff::Timestamp::now().to_string()
    });
    ingress::store(
        &test.app,
        mta.workspace,
        &[&arrival(
            &mta,
            Provider::Norbelys,
            &[("mta-policy-1", deferred.to_string().as_bytes())],
        )],
    )
    .await
    .unwrap();

    let runner = normaliser(&test);
    assert_eq!(normalize_due(&runner).await, ["done"]);
    let history = test
        .app()
        .get(&format!("/v1/messages/{sent}"))
        .bearer(&mta.api_key)
        .send()
        .await;
    let event = &history.json["events"]["data"][0];
    assert_eq!(event["kind"], "deferred");
    assert_eq!(event["category"], "policy");
    assert_eq!(event["enhanced_status"], "4.7.1");
    assert_eq!(event["provider_code"], "JFE050004");
    assert_eq!(event["diagnostic"], diagnostic);
    let suppressed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM suppressions WHERE workspace_id = $1")
            .bind(mta.workspace.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(suppressed, 0);
}

/// Only our own messages, sent through the webhook's connection, are matched: an event with an
/// unknown tag and one naming another connection's message are kept with no message and
/// `inferred` (review and counters only), and a receipt that is not the provider's format is
/// quarantined with its body, with no effect.
#[tokio::test]
async fn events_naming_no_message_of_the_connection_are_kept_unmatched() {
    let test = TestDb::new().await;
    let hook = hook(&test, "sendgrid", None).await;
    let elsewhere = test
        .sender(hook.workspace, &SenderSpec::relay("other@example.test"))
        .await;
    let foreign = test
        .direct_message(hook.workspace, &elsewhere, &["eve@example.org"], 0)
        .await;
    let unknown = sendgrid_event("sg-u", "spamreport", Some(Id::new()), json!({}));
    let other = sendgrid_event("sg-o", "spamreport", Some(foreign), json!({}));
    ingress::store(
        &test.app,
        hook.workspace,
        &[&arrival(
            &hook,
            Provider::Sendgrid,
            &[("sg-u", &unknown), ("sg-o", &other), ("sg-x", b"not json")],
        )],
    )
    .await
    .unwrap();
    assert_eq!(normalize_due(&normaliser(&test)).await, ["done"]);
    let recorded = events(&test, &hook).await;
    assert_eq!(recorded.len(), 2);
    assert!(
        recorded
            .iter()
            .all(|(message, kind, _, confidence, _)| message.is_none()
                && kind == "complaint"
                && confidence == "inferred")
    );
    let suppressions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM suppressions WHERE workspace_id = $1")
            .bind(hook.workspace.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(suppressions, 0);
    assert_eq!(
        receipts(&test, &hook).await,
        [
            ("sg-u".to_owned(), "normalized".to_owned(), false),
            ("sg-o".to_owned(), "normalized".to_owned(), false),
            ("sg-x".to_owned(), "quarantined".to_owned(), true),
        ]
    );
}

// ───────────────────────────── the route, per provider ─────────────────────────────

/// SNS as a test plays it: an RSA key whose public half the ingress has cached under the signing
/// certificate's URL, so verification never reaches AWS.
struct Sns {
    pair: RsaKeyPair,
}

impl Sns {
    fn new(ingress: &Ingress) -> Self {
        let pair = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
        let spki = pair.public_key().as_der().unwrap().as_ref().to_vec();
        ingress.certificates().preload(CERT_URL, spki);
        Self { pair }
    }

    /// The message SNS would post with `fields` (in AWS's canonical order), signed (version 2).
    fn post(&self, fields: &[(&str, String)]) -> Vec<u8> {
        let canonical: String = fields
            .iter()
            .map(|(name, value)| format!("{name}\n{value}\n"))
            .collect();
        let mut signature = vec![0; self.pair.public_modulus_len()];
        self.pair
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                canonical.as_bytes(),
                &mut signature,
            )
            .unwrap();
        let mut body = serde_json::Map::new();
        for (name, value) in fields {
            body.insert((*name).to_owned(), json!(value));
        }
        body.insert("SignatureVersion".to_owned(), json!("2"));
        body.insert("Signature".to_owned(), json!(STANDARD.encode(&signature)));
        body.insert("SigningCertURL".to_owned(), json!(CERT_URL));
        serde_json::to_vec(&body).unwrap()
    }

    fn notification(&self, message_id: &str, message: &Value, topic: &str) -> Vec<u8> {
        self.post(&[
            ("Message", message.to_string()),
            ("MessageId", message_id.to_owned()),
            ("Timestamp", sns_now()),
            ("TopicArn", topic.to_owned()),
            ("Type", "Notification".to_owned()),
        ])
    }
}

fn sns_now() -> String {
    jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%S.%3fZ")
        .to_string()
}

/// A Mailgun callback as Mailgun signs it. The event's time is fixed across retries even when
/// each retry has a fresh signature timestamp and token.
fn mailgun_body(key: &str, event_id: &str, occurred_at: i64) -> Vec<u8> {
    let timestamp = jiff::Timestamp::now().as_second().to_string();
    let token = Uuid::now_v7().simple().to_string();
    let tag = aws_lc_rs::hmac::sign(
        &aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, key.as_bytes()),
        format!("{timestamp}{token}").as_bytes(),
    );
    let signature: String = tag
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    serde_json::to_vec(&json!({
        "signature": {"timestamp": timestamp, "token": token, "signature": signature},
        "event-data": {"id": event_id, "event": "delivered", "timestamp": occurred_at,
                       "recipient": "grace@example.org"}
    }))
    .unwrap()
}

/// A SendGrid batch's signature headers, over its timestamp and body.
fn sendgrid_headers(pair: &EcdsaKeyPair, body: &[u8]) -> [(&'static str, String); 2] {
    let timestamp = jiff::Timestamp::now().as_second().to_string();
    let mut message = timestamp.clone().into_bytes();
    message.extend_from_slice(body);
    let signature = pair.sign(&SystemRandom::new(), &message).unwrap();
    [
        (
            "x-twilio-email-event-webhook-signature",
            STANDARD.encode(signature.as_ref()),
        ),
        ("x-twilio-email-event-webhook-timestamp", timestamp),
    ]
}

/// The managed MTA's batch of one event and its Standard Webhooks headers, signed `seconds_ago`:
/// every call is a new request (a fresh `webhook-id` and signature) carrying the same event, as
/// the MTA's retries do.
fn mta_post(
    secret: &str,
    event_id: &str,
    seconds_ago: i64,
) -> (Vec<u8>, [(&'static str, String); 3]) {
    let body = serde_json::to_vec(&json!({"type": "mta.events", "timestamp": jiff::Timestamp::now().to_string(),
        "data": {"events": [{"event_id": event_id, "recipient": "grace@example.org", "kind": "delivered",
                             "enhanced_status": "2.0.0", "detail": "250 2.0.0 OK", "provenance": "smtp_reply",
                             "observed_at": "2026-10-01T11:59:58.123456Z"}]}}))
    .unwrap();
    let id = format!("msg_{}", Uuid::now_v7().simple());
    let timestamp = jiff::Timestamp::now().as_second() - seconds_ago;
    let bytes = super::deliver::secret_bytes(secret).unwrap();
    let signature = crypto::sign_webhook(&bytes, &id, timestamp, &body);
    (
        body,
        [
            ("webhook-id", id),
            ("webhook-timestamp", timestamp.to_string()),
            ("webhook-signature", signature),
        ],
    )
}

async fn post(app: &TestApp, hook: &Hook, body: Vec<u8>, headers: &[(&str, String)]) -> StatusCode {
    let mut call = app.post(&hook.path()).raw("application/json", body);
    for (name, value) in headers {
        call = call.header(name, value);
    }
    call.send().await.status
}

/// Each provider's signed callback verifies as that provider signs it and is committed as a
/// receipt before `200` is answered; posting it again answers `200` and stores nothing more.
#[tokio::test]
async fn each_providers_signed_callback_is_received_once() {
    let test = TestDb::new().await;
    let ingress = Ingress::start(test.app.clone(), None);
    let sns = Sns::new(&ingress);
    let app = test.app_with_ingress(crate::senders::Settings::for_tests(), ingress);

    let mailgun = hook(&test, "mailgun", Some(MAILGUN_KEY)).await;
    let occurred_at = jiff::Timestamp::now().as_second();
    let body = mailgun_body(MAILGUN_KEY, "mg-1", occurred_at);
    assert_eq!(
        post(&app, &mailgun, body.clone(), &[]).await,
        StatusCode::OK
    );
    // Mailgun's retry is signed anew: the same event, another signature.
    assert_eq!(
        post(
            &app,
            &mailgun,
            mailgun_body(MAILGUN_KEY, "mg-1", occurred_at),
            &[]
        )
        .await,
        StatusCode::OK
    );

    let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).unwrap();
    let public = STANDARD.encode(pair.public_key().as_der().unwrap().as_ref());
    let sendgrid = hook(&test, "sendgrid", Some(&public)).await;
    let batch = format!(
        "[{},{}]",
        String::from_utf8(sendgrid_event("sg-1", "delivered", None, json!({}))).unwrap(),
        String::from_utf8(sendgrid_event("sg-2", "processed", None, json!({}))).unwrap()
    )
    .into_bytes();
    for _ in 0..2 {
        let headers = sendgrid_headers(&pair, &batch);
        assert_eq!(
            post(&app, &sendgrid, batch.clone(), &headers).await,
            StatusCode::OK
        );
    }

    let secret = format!("whsec_{}", STANDARD.encode([7_u8; 32]));
    let mta = hook(&test, "norbelys", Some(&secret)).await;
    for _ in 0..2 {
        let (body, headers) = mta_post(&secret, "mta-1", 0);
        assert_eq!(post(&app, &mta, body, &headers).await, StatusCode::OK);
    }

    let ses = hook(&test, "ses", Some(ARN)).await;
    let event = json!({"eventType": "Send", "mail": {"timestamp": jiff::Timestamp::now().to_string(), "messageId": "token-1"}});
    for _ in 0..2 {
        assert_eq!(
            post(&app, &ses, sns.notification("sns-1", &event, ARN), &[]).await,
            StatusCode::OK
        );
    }

    let expected = [(&mailgun, 1), (&sendgrid, 2), (&mta, 1), (&ses, 1)];
    for (hook, count) in expected {
        let stored = receipts(&test, hook).await;
        assert_eq!(stored.len(), count, "{}: {stored:?}", hook.webhook);
        assert!(stored.iter().all(|(_, state, _)| state == "received"));
    }
}

/// Nothing unverified is stored: a wrong signature, a stale timestamp, another SNS topic, an
/// unknown webhook and a webhook whose key is not set yet are refused, Mailgun's refusals with
/// `406` (its "do not retry") and the missing key with `401` for every provider, so callbacks
/// sent before the key is pasted are retried rather than dropped.
#[tokio::test]
async fn callbacks_that_do_not_verify_are_refused() {
    let test = TestDb::new().await;
    let ingress = Ingress::start(test.app.clone(), None);
    let sns = Sns::new(&ingress);
    let app = test.app_with_ingress(crate::senders::Settings::for_tests(), ingress);

    let mailgun = hook(&test, "mailgun", Some(MAILGUN_KEY)).await;
    let forged = mailgun_body(
        "key-another-account",
        "mg-forged",
        jiff::Timestamp::now().as_second(),
    );
    assert_eq!(
        post(&app, &mailgun, forged, &[]).await,
        StatusCode::NOT_ACCEPTABLE
    );

    let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).unwrap();
    let other = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).unwrap();
    let sendgrid = hook(
        &test,
        "sendgrid",
        Some(&STANDARD.encode(pair.public_key().as_der().unwrap().as_ref())),
    )
    .await;
    let batch = format!(
        "[{}]",
        String::from_utf8(sendgrid_event("sg-1", "delivered", None, json!({}))).unwrap()
    )
    .into_bytes();
    let headers = sendgrid_headers(&other, &batch);
    assert_eq!(
        post(&app, &sendgrid, batch, &headers).await,
        StatusCode::UNAUTHORIZED
    );

    let secret = format!("whsec_{}", STANDARD.encode([9_u8; 32]));
    let mta = hook(&test, "norbelys", Some(&secret)).await;
    let (body, headers) = mta_post(&secret, "mta-stale", 3_600);
    assert_eq!(
        post(&app, &mta, body, &headers).await,
        StatusCode::UNAUTHORIZED
    );

    let ses = hook(&test, "ses", Some(ARN)).await;
    let event =
        json!({"eventType": "Send", "mail": {"timestamp": jiff::Timestamp::now().to_string()}});
    let elsewhere = sns.notification("sns-x", &event, "arn:aws:sns:us-east-1:999999999999:other");
    assert_eq!(
        post(&app, &ses, elsewhere, &[]).await,
        StatusCode::UNAUTHORIZED
    );

    let unknown = Hook {
        webhook: Id::new(),
        ..hook(&test, "sendgrid", None).await
    };
    assert_eq!(
        post(&app, &unknown, b"[]".to_vec(), &[]).await,
        StatusCode::NOT_FOUND
    );
    let unset = hook(&test, "mailgun", None).await;
    assert_eq!(
        post(
            &app,
            &unset,
            mailgun_body(MAILGUN_KEY, "mg-early", jiff::Timestamp::now().as_second()),
            &[],
        )
        .await,
        StatusCode::UNAUTHORIZED
    );

    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_receipts")
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(stored, 0);
}

/// SNS asks to confirm the subscription of our URL to the topic before it sends events: a signed
/// confirmation for the configured topic is confirmed at that topic's SNS host with its token
/// (the URL rebuilt from the topic, never taken from the message), and answered `200`.
#[tokio::test]
async fn an_sns_subscription_is_confirmed_at_the_topics_host() {
    let test = TestDb::new().await;
    let aws = Sink::start().await;
    let ingress = Ingress::start(test.app.clone(), None);
    let sns = Sns::new(&ingress);
    let settings = crate::senders::Settings {
        http: norbelys_mail::http::HttpClient::rebased(&aws.url("")).unwrap(),
        ..crate::senders::Settings::for_tests()
    };
    let app = test.app_with_ingress(settings, ingress);
    let ses = hook(&test, "ses", Some(ARN)).await;
    let confirmation = sns.post(&[
        (
            "Message",
            "You have chosen to subscribe to the topic.".to_owned(),
        ),
        (
            "MessageId",
            "165545c9-2a5c-472c-8df2-7ff2be2b3b1b".to_owned(),
        ),
        (
            "SubscribeURL",
            "https://attacker.example/confirm".to_owned(),
        ),
        ("Timestamp", sns_now()),
        ("Token", "2336412f37fb687f5d51e6e2425f004aed".to_owned()),
        ("TopicArn", ARN.to_owned()),
        ("Type", "SubscriptionConfirmation".to_owned()),
    ]);
    let reply = app
        .post(&ses.path())
        .raw("text/plain", confirmation)
        .send()
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json);
    let calls = aws.requests();
    assert_eq!(calls.len(), 1);
    let query = calls[0].query.clone().unwrap_or_default();
    assert!(query.contains("Action=ConfirmSubscription"), "{query}");
    assert!(
        query.contains("Token=2336412f37fb687f5d51e6e2425f004aed"),
        "{query}"
    );
    assert!(
        query.contains("TopicArn=arn%3Aaws%3Asns%3Aus-east-1%3A123456789012%3Ases-events"),
        "{query}"
    );
    assert!(
        calls[0]
            .headers
            .get("host")
            .is_none_or(|host| !host.as_bytes().starts_with(b"attacker"))
    );
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_receipts")
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(stored, 0);
}

// ───────────────────────────── holds ─────────────────────────────

/// A person who knows better than the evidence releases a message's holds: every open hold is
/// resolved `manual` (mail to the address flows again), the evidence is kept in the audit log,
/// the answer is the message showing it; another workspace cannot see the message, and a release
/// without evidence is refused.
#[tokio::test]
async fn a_person_releases_a_messages_holds_with_evidence() {
    let test = TestDb::new().await;
    let app = test.app();
    let hook = hook(&test, "sendgrid", None).await;
    let message = message(&test, &hook, &["ada@example.org"], false).await;
    sqlx::query(
        "INSERT INTO recipient_holds (workspace_id, message_id, email, reason, observed_at, review_after)
         VALUES ($1, $2, 'ada@example.org', 'mailbox_full', now(), now() + interval '6 hours')",
    )
    .bind(hook.workspace.uuid())
    .bind(message.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();
    let path = format!("/v1/messages/{message}/release_holds");
    let evidence =
        json!({"evidence": "The recipient confirmed by phone that their mailbox has room."});

    let stranger = hook_workspace_key(&test).await;
    let hidden = app
        .post(&path)
        .bearer(&stranger)
        .idempotency("release-1")
        .json(evidence.clone())
        .send()
        .await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);
    let empty = app
        .post(&path)
        .bearer(&hook.api_key)
        .idempotency("release-2")
        .json(json!({"evidence": ""}))
        .send()
        .await;
    assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);

    let released = app
        .post(&path)
        .bearer(&hook.api_key)
        .idempotency("release-3")
        .json(evidence)
        .send()
        .await;
    assert_eq!(released.status, StatusCode::OK, "{}", released.json);
    assert_eq!(released.json["id"], message.to_string());
    assert_eq!(released.json["holds"][0]["resolution"], "manual");
    assert!(released.json["holds"][0]["resolved_at"].is_string());
    let audited: (String, Value) = sqlx::query_as(
        "SELECT action, details FROM audit_log WHERE workspace_id = $1 AND target = $2",
    )
    .bind(hook.workspace.uuid())
    .bind(message.to_string())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(audited.0, "message.holds_released");
    assert_eq!(
        audited.1["evidence"],
        "The recipient confirmed by phone that their mailbox has room."
    );
    assert_eq!(audited.1["released"], json!(["ada@example.org"]));
}

/// An API key of another, new workspace.
async fn hook_workspace_key(test: &TestDb) -> String {
    test.workspace(&format!("other{}", Uuid::now_v7().simple()))
        .await
        .key
}
