//! Store, gate and API tests of the inbox, against real PostgreSQL through the shared harness.
//!
//! The provider side of a poll (IMAP, the Gmail API, Graph) is the mail crate's and is tested
//! there against its fakes; these tests start where a page was read: they claim bindings as the
//! inbox role does, store pages of raw MIME written here, and read what the transaction left in
//! the database and through the API.

use axum::http::StatusCode;
use jiff::SignedDuration;
use norbelys_mail::receive::{RawMessage, Reset, ResetReason, TransportIdentity};
use serde_json::{Value, json};
use uuid::Uuid;

use super::poll::{self, Lease, Outcome, Page};
use crate::delivery::accept::{self, Accepted, NewMessage, Sender};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, InboundMessage, ReceiveBinding, Thread, WorkspaceId};
use crate::domain::time::Timestamp;
use crate::testing::{self, SenderSpec, TestDb, TestSender};

const INTERVAL: SignedDuration = SignedDuration::from_mins(5);

/// A workspace with one mailbox (`max@acme.example`) read through its `INBOX`.
struct World {
    test: TestDb,
    ws: WorkspaceId,
    key: String,
    sender: TestSender,
    binding: Id<ReceiveBinding>,
}

async fn world() -> World {
    let test = TestDb::new().await;
    let workspace = test.workspace("acme").await;
    let sender = test
        .sender(workspace.id, &SenderSpec::mailbox("max@acme.example"))
        .await;
    let binding = binding(&test, workspace.id, &sender, "INBOX").await;
    World {
        test,
        ws: workspace.id,
        key: workspace.key,
        sender,
        binding,
    }
}

async fn binding(
    test: &TestDb,
    ws: WorkspaceId,
    sender: &TestSender,
    folder: &str,
) -> Id<ReceiveBinding> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO receive_bindings (workspace_id, connection_id, folder) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(ws.uuid())
    .bind(sender.connection.uuid())
    .bind(folder)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    Id::from_uuid(id)
}

/// A direct message from the world's mailbox to `to`, accepted with its new thread.
async fn sent(world: &World, to: &str) -> Accepted {
    let mut tx = world.test.app.begin_in(world.ws).await.unwrap();
    let to = [EmailAddress::parse(to).unwrap()];
    let accepted = accept::create(
        &mut tx,
        &testing::keys(),
        world.ws,
        &NewMessage {
            from: Sender::Identity(world.sender.identity),
            to: &to,
            cc: &[],
            bcc: &[],
            subject: "Quick question",
            html: None,
            text: Some("Hello"),
            variables: None,
            send_at: None,
            expires_at: None,
            reply: None,
            idempotency_key: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    accepted
}

/// A raw message under an IMAP identity, received a minute from now: it always follows what the
/// test sent (stamped by the database's clock), whatever the skew between the two clocks.
fn mail(uid: u32, mime: &str) -> RawMessage {
    RawMessage {
        identity: TransportIdentity::Imap {
            uid_validity: 1,
            uid,
        },
        raw: mime.replace('\n', "\r\n").into_bytes(),
        size: None,
        truncated: false,
        received_at: jiff::Timestamp::now()
            .checked_add(SignedDuration::from_mins(1))
            .ok(),
    }
}

/// A plain message from `from`, answering `in_reply_to` when given.
fn letter(from: &str, id: &str, in_reply_to: Option<&str>, subject: &str, body: &str) -> String {
    let answers = in_reply_to.map_or(String::new(), |id| format!("In-Reply-To: {id}\n"));
    format!(
        "From: Someone <{from}>\nTo: max@acme.example\nSubject: {subject}\nMessage-ID: {id}\n{answers}Date: Thu, 01 Oct 2026 10:00:00 +0000\n\n{body}\n"
    )
}

/// A delivery status notification for `recipient` returning `message_id`.
fn bounce(recipient: &str, message_id: &str) -> String {
    format!(
        "From: Mail Delivery Subsystem <mailer-daemon@googlemail.com>\nTo: max@acme.example\nSubject: Delivery Status Notification (Failure)\nMessage-ID: <dsn-{recipient}@mx.google.com>\nDate: Thu, 01 Oct 2026 10:00:05 +0000\nMIME-Version: 1.0\nContent-Type: multipart/report; boundary=\"b1\"; report-type=delivery-status\n\n--b1\nContent-Type: text/plain\n\nAddress not found.\n\n--b1\nContent-Type: message/delivery-status\n\nReporting-MTA: dns; googlemail.com\n\nFinal-Recipient: rfc822; {recipient}\nAction: failed\nStatus: 5.1.1\nDiagnostic-Code: smtp; 550 5.1.1 The email account does not exist.\n\n--b1\nContent-Type: text/rfc822-headers\n\nFrom: Max <max@acme.example>\nTo: {recipient}\nSubject: Quick question\nMessage-ID: {message_id}\n\n--b1--\n"
    )
}

fn page(messages: Vec<RawMessage>, last_uid: u32) -> Page {
    Page {
        messages,
        cursor: json!({"uid_validity": 1, "last_uid": last_uid}),
        full: false,
        reset: None,
    }
}

async fn claim(world: &World, binding: Id<ReceiveBinding>) -> Lease {
    poll::claim(&world.test.worker, world.ws, binding, "inbox:test")
        .await
        .unwrap()
        .expect("the binding is claimable")
}

async fn store(world: &World, lease: &Lease, page: &Page) -> Outcome {
    poll::store(
        &world.test.worker,
        &testing::keys(),
        &world.test.storage,
        lease,
        page,
        crate::process::now(),
        INTERVAL,
    )
    .await
    .unwrap()
}

/// Claims `binding` and stores `messages` through it.
async fn read(world: &World, binding: Id<ReceiveBinding>, messages: Vec<RawMessage>) -> Outcome {
    due_now(world, binding).await;
    let lease = claim(world, binding).await;
    store(world, &lease, &page(messages, 1)).await
}

async fn due_now(world: &World, binding: Id<ReceiveBinding>) {
    sqlx::query("UPDATE receive_bindings SET next_poll_at = now() WHERE id = $1")
        .bind(binding.uuid())
        .execute(world.test.system.pool())
        .await
        .unwrap();
}

/// The inbound row stored under `transport_key`, as JSON.
async fn inbound(world: &World, transport_key: &str) -> Value {
    sqlx::query_scalar(
        "SELECT to_jsonb(i) FROM inbound_messages i WHERE workspace_id = $1 AND transport_key = $2",
    )
    .bind(world.ws.uuid())
    .bind(transport_key)
    .fetch_one(world.test.system.pool())
    .await
    .unwrap()
}

async fn scalar<T>(world: &World, sql: &'static str) -> T
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
{
    sqlx::query_scalar(sql)
        .bind(world.ws.uuid())
        .fetch_one(world.test.system.pool())
        .await
        .unwrap()
}

/// A poll whose lease was lost (it expired, was recovered and claimed again) records nothing:
/// neither its messages nor its cursor, because the cursor's advance is fenced by owner and
/// generation and fails the whole transaction. The current owner then stores the same page.
/// Without the fence a stale poller could move the cursor past mail nobody stored.
#[tokio::test]
async fn a_lost_lease_records_nothing() {
    let world = world().await;
    let first = claim(&world, world.binding).await;
    sqlx::query(
        "UPDATE receive_bindings SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(world.binding.uuid())
    .execute(world.test.system.pool())
    .await
    .unwrap();
    assert_eq!(poll::recover(&world.test.worker).await.unwrap(), 1);
    let second = claim(&world, world.binding).await;
    assert!(second.generation > first.generation);
    let read = page(
        vec![mail(
            1,
            &letter("ada@example.com", "<a1@example.com>", None, "Hi", "Hello"),
        )],
        1,
    );
    assert_eq!(store(&world, &first, &read).await, Outcome::Fenced);
    let count: i64 = scalar(
        &world,
        "SELECT count(*) FROM inbound_messages WHERE workspace_id = $1",
    )
    .await;
    assert_eq!(count, 0);
    assert!(matches!(
        store(&world, &second, &read).await,
        Outcome::Stored {
            read: 1,
            new: 1,
            full: false
        }
    ));
    let row: Value = scalar(
        &world,
        "SELECT jsonb_build_object('cursor', cursor, 'owner', lease_owner) FROM receive_bindings WHERE workspace_id = $1",
    )
    .await;
    assert_eq!(
        row,
        json!({"cursor": {"uid_validity": 1, "last_uid": 1}, "owner": null})
    );
}

/// A mailbox is read by one poll at a time: of two replicas claiming two folders of the same
/// mailbox at once, exactly one wins, and while its lease lives neither folder can be claimed
/// again. Two polls at once would double the provider requests a mailbox shares with sending.
#[tokio::test]
async fn a_mailbox_is_read_by_one_poll_at_a_time() {
    let world = world().await;
    let archive = binding(&world.test, world.ws, &world.sender, "Archive").await;
    let (a, b) = (
        world.test.worker_pool(2).await,
        world.test.worker_pool(2).await,
    );
    let (first, second) = tokio::join!(
        poll::claim(&a, world.ws, world.binding, "inbox:a"),
        poll::claim(&b, world.ws, archive, "inbox:b"),
    );
    let won = [first.unwrap(), second.unwrap()]
        .into_iter()
        .filter(Option::is_some)
        .count();
    assert_eq!(won, 1);
    for folder in [world.binding, archive] {
        assert!(
            poll::claim(&world.test.worker, world.ws, folder, "inbox:c")
                .await
                .unwrap()
                .is_none()
        );
    }
}

/// The transport identity is the only hard key: a message read twice (a re-poll after a lost
/// lease, a resync's overlap) is stored once and its effects (the customer's event) happen once.
#[tokio::test]
async fn a_message_read_twice_is_recorded_once() {
    let world = world().await;
    let sent = sent(&world, "ada@example.com").await;
    let reply = mail(
        7,
        &letter(
            "ada@example.com",
            "<r1@example.com>",
            Some(&sent.internet_message_id),
            "Re: Quick question",
            "Sounds good.",
        ),
    );
    assert!(matches!(
        read(&world, world.binding, vec![reply.clone()]).await,
        Outcome::Stored { new: 1, .. }
    ));
    assert!(matches!(
        read(&world, world.binding, vec![reply]).await,
        Outcome::Stored {
            read: 1,
            new: 0,
            ..
        }
    ));
    let rows: i64 = scalar(
        &world,
        "SELECT count(*) FROM inbound_messages WHERE workspace_id = $1",
    )
    .await;
    let told: i64 = scalar(
        &world,
        "SELECT count(*) FROM outbox_events WHERE workspace_id = $1 AND type = 'inbound_message.received'",
    )
    .await;
    assert_eq!((rows, told), (1, 1));
}

/// A cursor the provider no longer honours is a resync that overlaps what was read: the overlap
/// is absorbed by the transport identity, the new mail is stored, and when the resync could not
/// read everything since its start the binding says so: a visible gap, never a silent one.
#[tokio::test]
async fn a_cursor_reset_is_an_overlapping_resync_with_a_visible_gap() {
    let world = world().await;
    let gmail = |id: &str| RawMessage {
        identity: TransportIdentity::Provider {
            provider_message_id: id.to_owned(),
        },
        ..mail(
            0,
            &letter(
                "ada@example.com",
                &format!("<{id}@example.com>"),
                None,
                "Hi",
                "Hello",
            ),
        )
    };
    assert!(matches!(
        read(&world, world.binding, vec![gmail("m1")]).await,
        Outcome::Stored { new: 1, .. }
    ));
    due_now(&world, world.binding).await;
    let lease = claim(&world, world.binding).await;
    let resync = Page {
        messages: vec![gmail("m1"), gmail("m2")],
        cursor: json!({"history_id": "42"}),
        full: false,
        reset: Some(Reset {
            reason: ResetReason::History,
            since: jiff::Timestamp::now(),
            truncated: true,
        }),
    };
    assert!(matches!(
        store(&world, &lease, &resync).await,
        Outcome::Stored {
            read: 2,
            new: 1,
            ..
        }
    ));
    let detail: String = scalar(
        &world,
        "SELECT status_detail FROM receive_bindings WHERE workspace_id = $1",
    )
    .await;
    assert!(detail.contains("may be missing"), "{detail}");
}

/// A failed poll keeps its reason, releases the lease and waits by the inbox's retry policy: at
/// least the interval, within twice it after the first failure and four times it after the
/// second (the jitter spreads a provider outage's mailboxes over the step); a refused login also
/// asks for the connection's check, which moves its health; a later success clears the failures.
/// The backoff keeps a broken mailbox from being hammered, the reason is what a person reads to
/// fix it, and the check tells them it needs reconnecting.
#[tokio::test]
async fn failures_back_off_and_a_success_clears_them() {
    let world = world().await;
    for (failures, longest) in [(1, 10.0), (2, 20.0)] {
        due_now(&world, world.binding).await;
        let lease = claim(&world, world.binding).await;
        poll::fail(
            &world.test.worker,
            &lease,
            "the IMAP server did not answer",
            INTERVAL,
            false,
            None,
        )
        .await
        .unwrap();
        let row: Value = scalar(
            &world,
            "SELECT jsonb_build_object('failures', failures, 'detail', status_detail, 'owner', lease_owner,
                    'wait', extract(epoch FROM next_poll_at - now())::float8 / 60) FROM receive_bindings WHERE workspace_id = $1",
        )
        .await;
        let wait = row["wait"].as_f64().unwrap_or_default();
        assert!(
            (4.9..=longest).contains(&wait),
            "{failures} failures: a wait of {wait} minutes"
        );
        assert_eq!(
            [&row["failures"], &row["detail"], &row["owner"]],
            [
                &json!(failures),
                &json!("the IMAP server did not answer"),
                &Value::Null
            ]
        );
    }
    due_now(&world, world.binding).await;
    let lease = claim(&world, world.binding).await;
    poll::fail(
        &world.test.worker,
        &lease,
        "the IMAP server refused the login",
        INTERVAL,
        true,
        None,
    )
    .await
    .unwrap();
    let checks: i64 = scalar(
        &world,
        "SELECT count(*) FROM jobs WHERE workspace_id = $1 AND kind = 'connection.check'",
    )
    .await;
    assert_eq!(checks, 1);
    assert!(matches!(
        read(&world, world.binding, Vec::new()).await,
        Outcome::Stored { new: 0, .. }
    ));
    let row: Value = scalar(
        &world,
        "SELECT jsonb_build_object('failures', failures, 'detail', status_detail) FROM receive_bindings WHERE workspace_id = $1",
    )
    .await;
    assert_eq!(row, json!({"failures": 0, "detail": null}));
}

/// A person's answer to campaign mail correlates to its thread and message by our signed
/// Message-ID, ends the person's enrollment under the campaign's stop rules, counts a reply, and
/// marks the thread unread: the whole point of reading the inbox.
#[tokio::test]
async fn a_human_reply_stops_the_enrollment_and_counts() {
    let world = world().await;
    let campaign = world.test.campaign(world.ws, &world.sender, None).await;
    let message = world
        .test
        .campaign_message(world.ws, campaign, &world.sender, "ada@example.com", 0)
        .await;
    let thread: Uuid = sqlx::query_scalar(
        "WITH t AS (INSERT INTO threads (workspace_id, person_id, campaign_id, sender_identity_id, root_message_id, subject)
                    SELECT workspace_id, person_id, campaign_id, sender_identity_id, id, subject FROM messages WHERE id = $1
                    RETURNING id)
         UPDATE messages SET thread_id = (SELECT id FROM t) WHERE id = $1 RETURNING thread_id",
    )
    .bind(message.uuid())
    .fetch_one(world.test.system.pool())
    .await
    .unwrap();
    let ours = accept::internet_message_id(
        &testing::keys(),
        message,
        Id::<Thread>::from_uuid(thread),
        "max@acme.example",
    );
    let reply = letter(
        "Ada@Example.com",
        "<r1@example.com>",
        Some(&ours),
        "Re: Hi",
        "Yes, let's talk.",
    );
    assert!(matches!(
        read(&world, world.binding, vec![mail(1, &reply)]).await,
        Outcome::Stored { new: 1, .. }
    ));
    let row = inbound(&world, "imap:1:1").await;
    assert_eq!(row["thread_id"], json!(thread));
    assert_eq!(row["message_id"], json!(message.uuid()));
    assert_eq!(row["classification"], "human_reply");
    assert_eq!(row["classification_source"], "rules");
    assert_eq!(row["in_reply_to"], json!(ours));
    let state: Value = scalar(
        &world,
        "SELECT jsonb_build_object(
                'enrollment', (SELECT status FROM enrollments WHERE workspace_id = $1),
                'replied', (SELECT count(*) FROM stats_increments WHERE workspace_id = $1 AND metric = 'replied'),
                'unread', (SELECT unread FROM threads WHERE workspace_id = $1),
                'person', (SELECT replied_at IS NOT NULL FROM people WHERE workspace_id = $1))",
    )
    .await;
    assert_eq!(
        state,
        json!({"enrollment": "replied", "replied": 1, "unread": true, "person": true})
    );
}

/// A bounce report read through the mailbox that sent the message, returning our Message-ID
/// and naming one of its recipients, is `corroborated` evidence; it asks for a review instead of
/// suppressing by itself. The same report read through another mailbox matches only partly and
/// is `inferred`. Confidence decides what evidence may do on its own.
#[tokio::test]
async fn a_bounce_report_is_evidence_with_the_confidence_its_mailbox_gives() {
    let world = world().await;
    let sent = sent(&world, "ghost@example.org").await;
    let report = bounce("ghost@example.org", &sent.internet_message_id);
    assert!(matches!(
        read(&world, world.binding, vec![mail(1, &report)]).await,
        Outcome::Stored { new: 1, .. }
    ));
    let other = world
        .test
        .sender(world.ws, &SenderSpec::mailbox("sales@acme.example"))
        .await;
    let elsewhere = binding(&world.test, world.ws, &other, "INBOX").await;
    assert!(matches!(
        read(&world, elsewhere, vec![mail(1, &report)]).await,
        Outcome::Stored { new: 1, .. }
    ));
    let events: Value = scalar(
        &world,
        "SELECT jsonb_agg(jsonb_build_object('source', source, 'kind', kind, 'category', category,
                'confidence', confidence, 'recipient', recipient_email, 'message', message_id) ORDER BY id)
           FROM delivery_events WHERE workspace_id = $1 AND source = 'dsn'",
    )
    .await;
    let event = |confidence: &str| {
        json!({"source": "dsn", "kind": "bounced", "category": "invalid_recipient", "confidence": confidence,
               "recipient": "ghost@example.org", "message": sent.message.uuid()})
    };
    assert_eq!(events, json!([event("corroborated"), event("inferred")]));
    let row = inbound(&world, "imap:1:1").await;
    assert_eq!(row["classification"], "bounce");
    assert_eq!(
        row["review_proposal"],
        json!({"action": "suppress", "email": "ghost@example.org", "reason": "bounce"})
    );
    let suppressed: i64 = scalar(
        &world,
        "SELECT count(*) FROM suppressions WHERE workspace_id = $1",
    )
    .await;
    assert_eq!(suppressed, 0);
}

/// A provider's id (SES replaces our Message-ID) correlates through the directory only for mail
/// read through the thread identity's own mailbox and sent by one of the original envelope's
/// addresses; a stranger naming the same id, or the id read through another mailbox, stays
/// unmatched, because an unsigned id proves nothing by itself.
#[tokio::test]
async fn a_provider_id_correlates_only_through_its_mailbox_and_envelope() {
    let world = world().await;
    let sent = sent(&world, "ada@example.com").await;
    sqlx::query(
        "INSERT INTO message_id_directory (workspace_id, lookup_key, message_id, thread_id, recipients)
         VALUES ($1, '0102018f-token-000000', $2, $3, ARRAY['ada@example.com'])",
    )
    .bind(world.ws.uuid())
    .bind(sent.message.uuid())
    .bind(sent.thread.uuid())
    .execute(world.test.system.pool())
    .await
    .unwrap();
    let ses = "<0102018f-token-000000@eu-west-1.amazonses.com>";
    let other = world
        .test
        .sender(world.ws, &SenderSpec::mailbox("sales@acme.example"))
        .await;
    let elsewhere = binding(&world.test, world.ws, &other, "INBOX").await;
    read(
        &world,
        world.binding,
        vec![
            mail(
                1,
                &letter("ada@example.com", "<r1@x>", Some(ses), "Re: Hi", "Yes"),
            ),
            mail(
                2,
                &letter("mallory@example.com", "<r2@x>", Some(ses), "Re: Hi", "Yes"),
            ),
        ],
    )
    .await;
    read(
        &world,
        elsewhere,
        vec![mail(
            3,
            &letter("ada@example.com", "<r3@x>", Some(ses), "Re: Hi", "Yes"),
        )],
    )
    .await;
    let threads: Value = scalar(
        &world,
        "SELECT jsonb_object_agg(transport_key, thread_id) FROM inbound_messages WHERE workspace_id = $1",
    )
    .await;
    assert_eq!(
        threads,
        json!({"imap:1:1": sent.thread.uuid(), "imap:1:2": null, "imap:1:3": null})
    );
}

/// A notice a person wrote ("please remove me") is classified and proposes a suppression, but
/// nothing is applied until a person confirms the review; confirming applies it once, and a
/// second decision, or a review of a message that proposed nothing, is refused.
#[tokio::test]
async fn a_notice_waits_for_a_persons_review() {
    let world = world().await;
    let sent = sent(&world, "ada@example.com").await;
    read(
        &world,
        world.binding,
        vec![
            mail(
                1,
                &letter(
                    "ada@example.com",
                    "<r1@x>",
                    Some(&sent.internet_message_id),
                    "Re: Quick question",
                    "Please remove me from your list.",
                ),
            ),
            mail(
                2,
                &letter(
                    "ada@example.com",
                    "<r2@x>",
                    Some(&sent.internet_message_id),
                    "Re: Quick question",
                    "Tuesday works.",
                ),
            ),
        ],
    )
    .await;
    let notice = inbound(&world, "imap:1:1").await;
    assert_eq!(notice["classification"], "unsubscribe");
    assert_eq!(
        notice["review_proposal"],
        json!({"action": "suppress", "email": "ada@example.com", "reason": "unsubscribe"})
    );
    let suppressed = || async {
        scalar::<i64>(
            &world,
            "SELECT count(*) FROM suppressions WHERE workspace_id = $1",
        )
        .await
    };
    assert_eq!(suppressed().await, 0);

    let app = world.test.app();
    let id =
        Id::<InboundMessage>::from_uuid(Uuid::parse_str(notice["id"].as_str().unwrap()).unwrap())
            .to_string();
    let review = |key: &'static str, id: String| {
        app.post(&format!("/v1/inbound_messages/{id}/review"))
            .bearer(&world.key)
            .idempotency(key)
            .json(json!({"decision": "confirm"}))
            .send()
    };
    let confirmed = review("k1", id.clone()).await;
    assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.json);
    assert_eq!(confirmed.json["review"]["decision"], "confirmed");
    let row: Value = scalar(
        &world,
        "SELECT jsonb_build_object('reason', reason, 'source', source) FROM suppressions WHERE workspace_id = $1",
    )
    .await;
    assert_eq!(
        row,
        json!({"reason": "unsubscribe", "source": "inbound_notice"})
    );
    assert_eq!(review("k2", id).await.status, StatusCode::CONFLICT);
    let answer = inbound(&world, "imap:1:2").await;
    let answer =
        Id::<InboundMessage>::from_uuid(Uuid::parse_str(answer["id"].as_str().unwrap()).unwrap())
            .to_string();
    assert_eq!(review("k3", answer).await.status, StatusCode::CONFLICT);
}

/// AI is asked only about what the rules left open, and only in a workspace that turned it on:
/// a human reply gets an `inbox.classify` job, an automatic reply does not, and without the
/// setting nothing is enqueued.
#[tokio::test]
async fn ai_is_asked_only_where_the_rules_were_silent() {
    let world = world().await;
    let sent = sent(&world, "ada@example.com").await;
    let auto = format!(
        "Auto-Submitted: auto-replied\n{}",
        letter(
            "ada@example.com",
            "<r2@x>",
            Some(&sent.internet_message_id),
            "Out of office",
            "Back Monday."
        )
    );
    read(
        &world,
        world.binding,
        vec![mail(
            1,
            &letter(
                "ada@example.com",
                "<r1@x>",
                Some(&sent.internet_message_id),
                "Re: Hi",
                "Sure.",
            ),
        )],
    )
    .await;
    sqlx::query("UPDATE workspaces SET settings = jsonb_set(settings, '{ai}', '{\"classify_replies\": true}') WHERE id = $1")
        .bind(world.ws.uuid())
        .execute(world.test.system.pool())
        .await
        .unwrap();
    read(
        &world,
        world.binding,
        vec![
            mail(2, &auto),
            mail(
                3,
                &letter(
                    "ada@example.com",
                    "<r3@x>",
                    Some(&sent.internet_message_id),
                    "Re: Hi",
                    "Also, Wednesday?",
                ),
            ),
        ],
    )
    .await;
    let asked: Value = scalar(
        &world,
        "SELECT jsonb_agg(payload -> 'inbound') FROM jobs WHERE workspace_id = $1 AND kind = 'inbox.classify'",
    )
    .await;
    let human = inbound(&world, "imap:1:3").await;
    let human =
        Id::<InboundMessage>::from_uuid(Uuid::parse_str(human["id"].as_str().unwrap()).unwrap());
    assert_eq!(asked, json!([human.to_string()]));
    assert_eq!(
        inbound(&world, "imap:1:2").await["classification"],
        "out_of_office"
    );
}

/// The thread contract: the list and the retrieve show participants, unread and the latest
/// message, the retrieve its messages in order; the status update requires a future
/// `snoozed_until` with `snoozed`, honours `If-Match`, and a snooze that ended reads as open.
/// Another workspace's key sees nothing of it.
#[tokio::test]
async fn threads_show_their_conversation_and_take_a_status() {
    let world = world().await;
    let sent = sent(&world, "ada@example.com").await;
    read(
        &world,
        world.binding,
        vec![mail(
            1,
            &letter(
                "ada@example.com",
                "<r1@x>",
                Some(&sent.internet_message_id),
                "Re: Quick question",
                "Sure.",
            ),
        )],
    )
    .await;
    let app = world.test.app();
    let id = sent.thread.to_string();
    let listed = app.get("/v1/threads").bearer(&world.key).send().await;
    assert_eq!(listed.status, StatusCode::OK);
    let first = &listed.json["data"][0];
    assert_eq!(first["id"], json!(id));
    assert_eq!(
        first["participants"],
        json!(["max@acme.example", "ada@example.com"])
    );
    assert_eq!(first["unread"], true);
    assert_eq!(first["last_message"]["direction"], "inbound");
    assert!(first.get("messages").is_none());

    let one = app
        .get(&format!("/v1/threads/{id}"))
        .bearer(&world.key)
        .send()
        .await;
    assert_eq!(one.status, StatusCode::OK);
    let directions: Vec<&str> = one.json["messages"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["direction"].as_str())
        .collect();
    assert_eq!(directions, ["outbound", "inbound"]);
    let etag = one.header("etag").unwrap().to_owned();

    let patch = |body: Value, if_match: Option<&str>| {
        let call = app
            .patch(&format!("/v1/threads/{id}"))
            .bearer(&world.key)
            .json(body);
        match if_match {
            Some(tag) => call.header("if-match", tag),
            None => call,
        }
        .send()
    };
    assert_eq!(
        patch(json!({"status": "snoozed"}), None).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let until = jiff::Timestamp::now()
        .checked_add(SignedDuration::from_hours(24))
        .unwrap()
        .to_string();
    let snoozed = patch(
        json!({"status": "snoozed", "snoozed_until": until}),
        Some(&etag),
    )
    .await;
    assert_eq!(snoozed.status, StatusCode::OK, "{}", snoozed.json);
    assert_eq!(snoozed.json["status"], "snoozed");
    assert_eq!(
        patch(json!({"unread": false}), Some(&etag)).await.status,
        StatusCode::PRECONDITION_FAILED
    );
    let filtered = app
        .get("/v1/threads?status=snoozed")
        .bearer(&world.key)
        .send()
        .await;
    assert_eq!(filtered.json["data"].as_array().map(Vec::len), Some(1));
    sqlx::query("UPDATE threads SET snoozed_until = now() - interval '1 second' WHERE id = $1")
        .bind(sent.thread.uuid())
        .execute(world.test.system.pool())
        .await
        .unwrap();
    let woke = app
        .get("/v1/threads?status=open")
        .bearer(&world.key)
        .send()
        .await;
    assert_eq!(woke.json["data"][0]["status"], "open");
    let read_now = patch(json!({"unread": false}), None).await;
    assert_eq!(read_now.json["unread"], false);

    let stranger = world.test.workspace("rival").await;
    let theirs = app
        .get(&format!("/v1/threads/{id}"))
        .bearer(&stranger.key)
        .send()
        .await;
    assert_eq!(theirs.status, StatusCode::NOT_FOUND);
    let none = app.get("/v1/threads").bearer(&stranger.key).send().await;
    assert_eq!(none.json["data"], json!([]));
}

/// A person's correction of a classification is `manual` and bumps the revision (so a later AI
/// verdict, applied by compare-and-set on the revision, never overrides it); an unknown sentiment
/// is refused, filters select it, counts follow it, and another workspace cannot see or change
/// it.
#[tokio::test]
async fn a_persons_classification_is_manual() {
    let world = world().await;
    read(
        &world,
        world.binding,
        vec![mail(
            1,
            &letter(
                "grace@example.org",
                "<g1@x>",
                None,
                "Hello",
                "Are you hiring?",
            ),
        )],
    )
    .await;
    let app = world.test.app();
    let listed = app
        .get("/v1/inbound_messages?classification=unknown")
        .bearer(&world.key)
        .send()
        .await;
    let id = listed.json["data"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(listed.json["data"][0]["from"]["email"], "grace@example.org");
    let path = format!("/v1/inbound_messages/{id}");
    let refused = app
        .patch(&path)
        .bearer(&world.key)
        .json(json!({"sentiment": "ecstatic"}))
        .send()
        .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    let changed = app
        .patch(&path)
        .bearer(&world.key)
        .json(json!({"classification": "human_reply", "sentiment": "positive"}))
        .send()
        .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.json);
    assert_eq!(changed.json["classification"], "human_reply");
    assert_eq!(changed.json["classification_source"], "manual");
    assert_eq!(changed.json["sentiment"], "positive");
    for (classification, count) in [("human_reply", 1), ("unknown", 0)] {
        let counted = app
            .get(&format!(
                "/v1/inbound_messages?classification={classification}&include=total_count"
            ))
            .bearer(&world.key)
            .send()
            .await;
        assert_eq!(
            counted.json["meta"]["total_count"], count,
            "{classification}"
        );
    }
    let revision: i32 = scalar(
        &world,
        "SELECT revision FROM inbound_messages WHERE workspace_id = $1",
    )
    .await;
    assert_eq!(revision, 2);
    let stranger = world.test.workspace("rival").await;
    for call in [
        app.get(&path).bearer(&stranger.key),
        app.patch(&path)
            .bearer(&stranger.key)
            .json(json!({"classification": "unknown"})),
    ] {
        assert_eq!(call.send().await.status, StatusCode::NOT_FOUND);
    }
}

/// The reply form of `POST /v1/messages`: a thread id and a body send from the thread's
/// identity to whoever wrote last, answering their message, with the thread's subject; a
/// thread of another workspace is not found.
#[tokio::test]
async fn a_reply_goes_from_the_threads_identity_to_who_wrote() {
    let world = world().await;
    let sent = sent(&world, "ada@example.com").await;
    read(
        &world,
        world.binding,
        vec![mail(
            1,
            &letter(
                "ada@example.com",
                "<r1@example.com>",
                Some(&sent.internet_message_id),
                "Re: Quick question",
                "Sure.",
            ),
        )],
    )
    .await;
    let app = world.test.app();
    let body =
        json!({"thread_id": sent.thread.to_string(), "html": "<p>Thanks Ada, Tuesday at 10.</p>"});
    let created = app
        .post("/v1/messages")
        .bearer(&world.key)
        .idempotency("reply-1")
        .json(body.clone())
        .send()
        .await;
    assert_eq!(created.status, StatusCode::ACCEPTED, "{}", created.json);
    assert_eq!(created.json["kind"], "reply");
    assert_eq!(created.json["from"]["email"], "max@acme.example");
    assert_eq!(created.json["to"], json!(["ada@example.com"]));
    assert_eq!(created.json["thread_id"], json!(sent.thread.to_string()));
    assert_eq!(created.json["in_reply_to"], "<r1@example.com>");
    assert_eq!(created.json["subject"], "Re: Quick question");
    let stranger = world.test.workspace("rival").await;
    let theirs = app
        .post("/v1/messages")
        .bearer(&stranger.key)
        .idempotency("reply-2")
        .json(body)
        .send()
        .await;
    assert_eq!(theirs.status, StatusCode::NOT_FOUND);
}

/// The `inbox.classify` kind applies AI's verdict by compare-and-set: the message the rules left
/// open becomes the verdict's classification and sentiment, `ai`, one revision later; a message
/// a person classified meanwhile keeps the person's classification, because a manual decision is
/// never overridden. Without that guard, a slow AI call could undo a person's correction.
#[tokio::test]
async fn ai_refines_only_what_nobody_decided() {
    use crate::ai::Ai;
    use crate::ai::fake::{Fake, json_answer};
    use crate::jobs::runner::Harness;
    use crate::jobs::{Queue, Registry};

    let fake = Fake::start(|_| {
        json_answer(&json!({
            "classification": "out_of_office",
            "sentiment": "neutral",
            "confidence": 0.95,
            "reasons": ["Says they are away until Monday"],
        }))
    })
    .await;
    let world = world().await;
    sqlx::query(
        "UPDATE workspaces SET settings = jsonb_set(settings, '{ai}', '{\"classify_replies\": true, \"monthly_budget_usd\": 10, \"review_sample\": 0}') WHERE id = $1",
    )
    .bind(world.ws.uuid())
    .execute(world.test.system.pool())
    .await
    .unwrap();
    let sent = sent(&world, "ada@example.com").await;
    let reply = |uid: u32| {
        mail(
            uid,
            &letter(
                "ada@example.com",
                &format!("<r{uid}@x>"),
                Some(&sent.internet_message_id),
                "Re: Quick question",
                "I am away until Monday.",
            ),
        )
    };
    read(&world, world.binding, vec![reply(1), reply(2)]).await;
    sqlx::query(
        "UPDATE inbound_messages SET classification_source = 'manual', revision = revision + 1
          WHERE workspace_id = $1 AND transport_key = 'imap:1:2'",
    )
    .bind(world.ws.uuid())
    .execute(world.test.system.pool())
    .await
    .unwrap();
    let mut registry = Registry::default();
    registry.register::<super::classify::Classify>().unwrap();
    let mut env = http::Extensions::new();
    env.insert(Ai::from_args(&fake.args(30)).unwrap());
    let runner = Harness::new(
        world.test.worker.clone(),
        world.test.system.clone(),
        registry,
        env,
        "worker-inbox-test",
    );
    let ran = runner.run_once(Queue::Ai, 2).await;
    assert_eq!(ran.len(), 2);
    let rows: Value = scalar(
        &world,
        "SELECT jsonb_object_agg(transport_key, jsonb_build_object('classification', classification,
                'source', classification_source, 'sentiment', sentiment, 'revision', revision))
           FROM inbound_messages WHERE workspace_id = $1",
    )
    .await;
    assert_eq!(
        rows,
        json!({
            "imap:1:1": {"classification": "out_of_office", "source": "ai", "sentiment": "neutral", "revision": 2},
            "imap:1:2": {"classification": "human_reply", "source": "manual", "sentiment": null, "revision": 2},
        })
    );
}

/// An unsubscribe request by mail (the subject our `mailto:` link asks mail clients to send) is
/// the recipient's own request: it suppresses the sender's address as `unsubscribe` at once, and
/// every live enrollment of that person ends, even though the request answers no message.
#[tokio::test]
async fn an_unsubscribe_request_by_mail_suppresses_and_stops() {
    let world = world().await;
    let campaign = world.test.campaign(world.ws, &world.sender, None).await;
    world
        .test
        .campaign_message(world.ws, campaign, &world.sender, "ada@example.com", 0)
        .await;
    let request = letter(
        "ada@example.com",
        "<u1@example.com>",
        None,
        "unsubscribe",
        "This message was sent by your mail client.",
    );
    assert!(matches!(
        read(&world, world.binding, vec![mail(1, &request)]).await,
        Outcome::Stored { new: 1, .. }
    ));
    assert_eq!(
        inbound(&world, "imap:1:1").await["classification"],
        "unsubscribe"
    );
    let state: Value = scalar(
        &world,
        "SELECT jsonb_build_object(
                'suppression', (SELECT jsonb_build_object('reason', reason, 'source', source)
                                  FROM suppressions WHERE workspace_id = $1),
                'enrollment', (SELECT status FROM enrollments WHERE workspace_id = $1))",
    )
    .await;
    assert_eq!(
        state["suppression"],
        json!({"reason": "unsubscribe", "source": "unsubscribe"})
    );
    assert_ne!(state["enrollment"], "active");
}

/// `GET /v1/threads?sort=last_activity_at` lists threads by their latest message, the inbox's
/// order, each tie broken by id: one page at a time, every thread comes exactly once and in that
/// order even where two share an instant, because each cursor carries the instant and the id of
/// the last thread shown. `order=asc` reverses it, and `include=total_count` counts what the
/// filters select.
#[tokio::test]
async fn threads_sort_by_their_latest_activity() {
    let world = world().await;
    let first = sent(&world, "ada@example.com").await.thread;
    let second = sent(&world, "grace@example.org").await.thread;
    let third = sent(&world, "linus@example.net").await.thread;
    // The oldest thread is the latest active; the two others share an earlier instant.
    for (thread, at) in [
        (first, "2026-09-01T12:00:00Z"),
        (second, "2026-09-01T11:00:00Z"),
        (third, "2026-09-01T11:00:00Z"),
    ] {
        sqlx::query("UPDATE threads SET last_activity_at = $2::timestamptz WHERE id = $1")
            .bind(thread.uuid())
            .bind(at)
            .execute(world.test.system.pool())
            .await
            .unwrap();
    }
    let app = world.test.app();
    let ids = |page: &Value| -> Vec<String> {
        page["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|thread| thread["id"].as_str().unwrap().to_owned())
            .collect()
    };

    let mut seen = Vec::new();
    let mut query = "sort=last_activity_at&limit=1".to_owned();
    loop {
        let page = app
            .get(&format!("/v1/threads?{query}"))
            .bearer(&world.key)
            .send()
            .await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.json);
        seen.extend(ids(&page.json));
        let Some(cursor) = page.json["meta"]["next_cursor"].as_str() else {
            break;
        };
        query = format!("sort=last_activity_at&limit=1&cursor={cursor}");
    }
    let expected = |threads: [Id<Thread>; 3]| threads.map(|thread| thread.to_string()).to_vec();
    assert_eq!(seen, expected([first, third, second]));

    let ascending = app
        .get("/v1/threads?sort=last_activity_at&order=asc")
        .bearer(&world.key)
        .send()
        .await;
    assert_eq!(ids(&ascending.json), expected([second, third, first]));

    for (status, count) in [("open", 3), ("archived", 0)] {
        let counted = app
            .get(&format!(
                "/v1/threads?status={status}&include=total_count&limit=1"
            ))
            .bearer(&world.key)
            .send()
            .await;
        assert_eq!(counted.json["meta"]["total_count"], count, "{status}");
    }
}

/// A status filter on the activity sort, matched on the stored status as the threads list
/// writes it, walks `threads_inbox` in the keyset's own order: one stored status, then activity
/// and id, in both directions, with no sort of the matching threads. Without it a page of the
/// archived threads of a large inbox reads every thread of the workspace.
#[tokio::test]
async fn a_status_filtered_activity_page_walks_its_index_in_order() {
    let world = world().await;
    let mut tx = world.test.system.begin().await.unwrap();
    // An empty table gives the two indexes the same cost. A populated inbox with a selective
    // status proves the useful access path without forcing the planner to choose an index.
    sqlx::query(
        "INSERT INTO threads (workspace_id, sender_identity_id, status, last_activity_at)
         SELECT $1, $2, CASE WHEN n % 100 = 0 THEN 'archived' ELSE 'open' END,
                now() - make_interval(secs => n)
           FROM generate_series(1, 10000) AS n",
    )
    .bind(world.ws.uuid())
    .bind(world.sender.identity.uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("ANALYZE threads")
        .execute(&mut *tx)
        .await
        .unwrap();
    let (workspace, after) = (world.ws.uuid(), Uuid::now_v7());
    for (direction, comparison) in [("DESC", "<"), ("ASC", ">")] {
        let statement = format!(
            "EXPLAIN (COSTS OFF) SELECT t.id FROM threads t
              WHERE t.workspace_id = '{workspace}'
                AND ((t.status = 'archived' OR ('archived' = 'open' AND t.status = 'snoozed'))
                     AND 'archived' = CASE WHEN t.status = 'snoozed' AND t.snoozed_until <= now()
                                           THEN 'open' ELSE t.status END)
                AND (t.last_activity_at, t.id) {comparison} (now(), '{after}'::uuid)
              ORDER BY t.last_activity_at {direction}, t.id {direction} LIMIT 21"
        );
        let plan = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(statement))
            .fetch_all(&mut *tx)
            .await
            .unwrap()
            .join("\n");
        assert!(
            plan.contains("using threads_inbox") && !plan.contains("Sort"),
            "{direction}:\n{plan}"
        );
    }
}

/// Polling publishes complete body content and decoded files with its cursor; content beyond
/// the classification excerpt is searchable, and truncated MIME never advertises a partial file.
#[tokio::test]
async fn received_content_and_files_follow_the_fenced_poll() {
    let world = world().await;
    let body = format!("{} retained-tail", "word ".repeat(1000));
    let raw = format!(
        "From: Someone <ada@example.com>\nTo: max@acme.example\nSubject: File received\nMessage-ID: <file@example.com>\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=parts\n\n--parts\nContent-Type: text/plain; charset=utf-8\n\n{body}\n--parts\nContent-Type: application/octet-stream\nContent-Disposition: attachment; filename=notes.bin\nContent-Transfer-Encoding: base64\n\nAAH/\n--parts--\n"
    );
    assert!(matches!(
        read(&world, world.binding, vec![mail(91, &raw)]).await,
        Outcome::Stored { new: 1, .. }
    ));
    let app = world.test.app();
    let search = app
        .get("/v1/messages/search?direction=inbound&q=retained-tail")
        .bearer(&world.key)
        .send()
        .await;
    assert_eq!(search.status, StatusCode::OK, "{}", search.json);
    assert_eq!(search.json["data"].as_array().unwrap().len(), 1);
    let id = search.json["data"][0]["id"].as_str().unwrap();
    let content = app
        .get(&format!("/v1/inbound_messages/{id}/content"))
        .bearer(&world.key)
        .send()
        .await;
    assert_eq!(content.status, StatusCode::OK, "{}", content.json);
    assert!(
        content.json["text"]
            .as_str()
            .unwrap()
            .contains("retained-tail")
    );
    assert_eq!(content.json["attachments"][0]["filename"], "notes.bin");
    assert_eq!(content.json["attachments"][0]["size_bytes"], 3);
    assert_eq!(content.json["truncated"], false);
    let mut truncated = mail(
        92,
        &raw.replace("file@example.com", "truncated@example.com"),
    );
    truncated.truncated = true;
    assert!(matches!(
        read(&world, world.binding, vec![truncated]).await,
        Outcome::Stored { new: 1, .. }
    ));
    let search = app
        .get("/v1/messages/search?direction=inbound&q=retained-tail&limit=1")
        .bearer(&world.key)
        .send()
        .await;
    let id = search.json["data"][0]["id"].as_str().unwrap();
    let content = app
        .get(&format!("/v1/inbound_messages/{id}/content"))
        .bearer(&world.key)
        .send()
        .await;
    assert_eq!(content.json["truncated"], true);
    assert_eq!(content.json["attachments"], json!([]));
}

/// Retry-After may exceed the one-hour local backoff; an expired provider hint never makes a
/// failed binding immediately due. Both cases release the fenced lease without losing its cursor.
#[tokio::test]
async fn provider_retry_after_extends_but_never_shortens_the_local_backoff() {
    let world = world().await;
    let later = Timestamp(
        jiff::Timestamp::from_microsecond(
            crate::process::now()
                .plus(std::time::Duration::from_secs(7_200))
                .0
                .as_microsecond(),
        )
        .unwrap(),
    );
    for (until, expected_later) in [(later, true), (crate::process::now(), false)] {
        due_now(&world, world.binding).await;
        let lease = claim(&world, world.binding).await;
        poll::fail(
            &world.test.worker,
            &lease,
            "receive_throttled",
            INTERVAL,
            false,
            Some(until),
        )
        .await
        .unwrap();
        let next: Timestamp = scalar(
            &world,
            "SELECT next_poll_at FROM receive_bindings WHERE workspace_id = $1",
        )
        .await;
        if expected_later {
            assert_eq!(next, later);
        } else {
            assert!(next > crate::process::now().plus(std::time::Duration::from_secs(290)));
        }
        let owner: Option<String> = scalar(
            &world,
            "SELECT lease_owner FROM receive_bindings WHERE workspace_id = $1",
        )
        .await;
        assert_eq!(owner, None);
    }
}
