//! The Start's store and gate tests, against real PostgreSQL with the worker login: every final
//! check returns its message unstarted or ends it, with exactly what that writes; the lease's
//! renewal fence and its clock; the pacing clock moved once for cold mail on a paced sender and
//! never for mail created through the API; and a removal against a Start in both orders.

use std::time::{Duration, Instant};

use serde_json::json;
use strum::IntoEnumIterator as _;
use uuid::Uuid;

use super::Started;
use crate::delivery::claim::Claimed;
use crate::delivery::recover;
use crate::domain::ids::{Id, Message, WorkspaceId};
use crate::domain::messages::State as MessageState;
use crate::domain::policy::delivery::{Category, Held};
use crate::domain::time::Timestamp;
use crate::testing::{SenderSpec, TestDb, TestSender};

const OWNER: &str = "sender-a";

/// What a Start that returns its message does to its due time.
enum Due {
    /// Kept: the claim filters the condition itself, so the message is not claimed and returned
    /// at every sweep.
    Unchanged,
    /// At this instant: the hold's end.
    At(Timestamp),
    /// One recheck later (five minutes): the claim does not filter the condition.
    Later,
}

/// The queue row of `message` as a test reads it.
#[derive(Debug, sqlx::FromRow)]
struct QueueRow {
    state: String,
    lease_owner: Option<String>,
    run_at: Timestamp,
    submission_started_at: Option<Timestamp>,
    reserved: bool,
}

/// `message`'s queue row, if it still has one.
async fn queue_row(test: &TestDb, message: Id<Message>) -> Option<QueueRow> {
    sqlx::query_as(
        "SELECT state, lease_owner, run_at, submission_started_at, reserved_day IS NOT NULL AS reserved
           FROM delivery_queue WHERE message_id = $1",
    )
    .bind(message.uuid())
    .fetch_optional(test.system.pool())
    .await
    .unwrap()
}

/// `message`'s state and its latest attempt's outcome, reservation and category.
async fn message_and_attempt(
    test: &TestDb,
    message: Id<Message>,
) -> (String, Option<String>, String, Option<String>) {
    sqlx::query_as(
        "SELECT m.state, a.outcome, a.quota_state, a.category
           FROM messages m JOIN attempts a ON a.workspace_id = m.workspace_id AND a.message_id = m.id
          WHERE m.id = $1 ORDER BY a.attempt_number DESC LIMIT 1",
    )
    .bind(message.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap()
}

/// The database's clock.
async fn clock(test: &TestDb) -> Timestamp {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(test.system.pool())
        .await
        .unwrap()
}

/// Runs `sql` as the system login with `id` as `$1`.
async fn exec(test: &TestDb, sql: &'static str, id: Uuid) {
    sqlx::query(sql)
        .bind(id)
        .execute(test.system.pool())
        .await
        .unwrap();
}

/// The ISO weekday (1 Monday to 7 Sunday) after today's, in UTC: a window open only then is
/// closed now.
fn tomorrow() -> i8 {
    jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .weekday()
        .to_monday_one_offset()
        % 7
        + 1
}

/// Every check the Start makes that the claim did not settle returns the message to the queue
/// unstarted when it fails, generated from `Held` so a new reason fails here until it has an
/// expected due time. Each case is claimed first and its condition written after the claim, as
/// a removal racing the claim would: the Start answers `Returned` with the reason; the queue row
/// is unleased with no marker or reservation day, due as the reason implies (unchanged when the
/// claim filters the condition, the hold's end for a held recipient, a recheck later otherwise);
/// the message is `queued`; the attempt is closed `released` and its unit back in the ledger,
/// once; the connection's budget wait is cleared, since budget was freed; nothing is told.
#[tokio::test]
async fn a_failed_check_returns_the_message_unstarted() {
    let test = TestDb::new().await;
    let ws = test.workspace("returns").await.id;
    for why in Held::iter() {
        let name: &'static str = why.into();
        let sender = test
            .sender(ws, &SenderSpec::mailbox(&format!("{name}@example.test")))
            .await;
        let campaign_mail = matches!(
            why,
            Held::CampaignUnavailable | Held::WindowClosed | Held::ClockNotDue
        );
        let to = format!("to-{name}@example.test");
        let (message, campaign) = if campaign_mail {
            let campaign = test.campaign(ws, &sender, None).await;
            (
                test.campaign_message(ws, campaign, &sender, &to, -60).await,
                Some(campaign),
            )
        } else {
            (test.direct_message(ws, &sender, &[&to], -60).await, None)
        };
        let claimed = test.claimed(ws, &sender, OWNER).await;
        assert_eq!(
            claimed[0].message, message,
            "{name}: the message is claimed"
        );
        let run_at = queue_row(&test, message).await.unwrap().run_at;
        exec(
            &test,
            "UPDATE connections SET next_claim_at = now() + interval '1 hour' WHERE id = $1",
            sender.connection.uuid(),
        )
        .await;

        let connection = sender.connection.uuid();
        let due = match why {
            Held::ConnectionUnavailable => {
                exec(
                    &test,
                    "UPDATE connections SET paused = true WHERE id = $1",
                    connection,
                )
                .await;
                Due::Unchanged
            }
            Held::Breaker => {
                exec(
                    &test,
                    "UPDATE connections SET consecutive_failures = 3, paused_until = now() + interval '1 hour',
                            breaker_opened_at = now() WHERE id = $1",
                    connection,
                )
                .await;
                Due::Unchanged
            }
            Held::Recipient => {
                let other = test.direct_message(ws, &sender, &[&to], 3_600).await;
                let until: Timestamp = sqlx::query_scalar(
                    "INSERT INTO recipient_holds (workspace_id, message_id, email, reason, observed_at, review_after)
                     VALUES ($1, $2, upper($3), 'mailbox_full', now(), now() + interval '1 hour')
                     RETURNING review_after",
                )
                .bind(ws.uuid())
                .bind(other.uuid())
                .bind(&to)
                .fetch_one(test.system.pool())
                .await
                .unwrap();
                Due::At(until)
            }
            Held::IdentityUnavailable => {
                exec(
                    &test,
                    "UPDATE sender_identities SET enabled = false WHERE connection_id = $1",
                    connection,
                )
                .await;
                Due::Later
            }
            Held::CampaignUnavailable => {
                exec(
                    &test,
                    "UPDATE campaigns SET status = 'paused' WHERE id = $1",
                    campaign.unwrap(),
                )
                .await;
                Due::Later
            }
            Held::WindowClosed => {
                sqlx::query("UPDATE campaigns SET send_window = $2 WHERE id = $1")
                    .bind(campaign.unwrap())
                    .bind(json!({ "days": [tomorrow()], "start": "09:00", "end": "10:00" }))
                    .execute(test.system.pool())
                    .await
                    .unwrap();
                Due::Unchanged
            }
            Held::ClockNotDue => {
                exec(
                    &test,
                    "UPDATE connections SET next_send_at = now() + interval '10 minutes' WHERE id = $1",
                    connection,
                )
                .await;
                Due::Unchanged
            }
        };

        let before = clock(&test).await;
        let started = test.start(ws, &sender, OWNER, &claimed[0]).await;
        assert_eq!(started, Started::Returned(why), "{name}");
        let row = queue_row(&test, message).await.unwrap();
        assert_eq!(row.state, "queued", "{name}");
        assert_eq!(row.lease_owner, None, "{name}");
        assert_eq!(row.submission_started_at, None, "{name}");
        assert!(!row.reserved, "{name}");
        match due {
            Due::Unchanged => assert_eq!(row.run_at, run_at, "{name}: due when it was"),
            Due::At(until) => assert_eq!(row.run_at, until, "{name}: due at the hold's end"),
            Due::Later => assert!(
                row.run_at >= before.plus(Duration::from_secs(300))
                    && row.run_at <= clock(&test).await.plus(Duration::from_secs(300)),
                "{name}: due a recheck later, {}",
                row.run_at
            ),
        }
        let (state, outcome, quota, _) = message_and_attempt(&test, message).await;
        assert_eq!(
            (state.as_str(), outcome.as_deref(), quota.as_str()),
            ("queued", Some("released"), "released"),
            "{name}"
        );
        assert_eq!(
            test.ledger(sender.connection).await,
            (0, 0),
            "{name}: the unit is back once"
        );
        let wait: Option<Timestamp> =
            sqlx::query_scalar("SELECT next_claim_at FROM connections WHERE id = $1")
                .bind(connection)
                .fetch_one(test.system.pool())
                .await
                .unwrap();
        assert_eq!(wait, None, "{name}: the budget wait is cleared");
        assert!(test.told(ws, message.uuid()).await.is_empty(), "{name}");
    }
}

/// The checks that end a message end it without a submission: an expired deadline fails it as
/// `expired` and resolves the holds it caused, a deleted workspace fails it, a suppressed
/// recipient suppresses it, and an archived sender fails a direct message. Each ends with its
/// queue row deleted, its attempt closed (`suppressed` or `skipped`, with the category) and its
/// unit released, and `message.failed` told with the category. The platform's transactional
/// mail to the same suppressed address is still submitted: a sign-in code reaches someone who
/// once refused other mail.
#[tokio::test]
async fn a_final_check_ends_the_message_without_a_submission() {
    let test = TestDb::new().await;
    let cases = [
        (Category::Expired, MessageState::Failed, "skipped"),
        (Category::WorkspaceDeleted, MessageState::Failed, "skipped"),
        (Category::Suppressed, MessageState::Suppressed, "suppressed"),
        (Category::SenderArchived, MessageState::Failed, "skipped"),
    ];
    for (category, state, outcome) in cases {
        let name = category.as_str();
        let ws = test.workspace(&name.replace('_', "-")).await.id;
        let sender = test
            .sender(ws, &SenderSpec::relay("relay@example.test"))
            .await;
        let message = test
            .direct_message(ws, &sender, &["ada@example.test"], -60)
            .await;
        let claimed = test.claimed(ws, &sender, OWNER).await;
        match category {
            Category::Expired => {
                sqlx::query("UPDATE delivery_queue SET deadline_at = now() - interval '1 second' WHERE message_id = $1")
                    .bind(message.uuid())
                    .execute(test.system.pool())
                    .await
                    .unwrap();
                sqlx::query(
                    "INSERT INTO recipient_holds (workspace_id, message_id, email, reason, observed_at, review_after)
                     VALUES ($1, $2, 'ada@example.test', 'mailbox_full', now(), now() + interval '1 hour')",
                )
                .bind(ws.uuid())
                .bind(message.uuid())
                .execute(test.system.pool())
                .await
                .unwrap();
            }
            Category::WorkspaceDeleted => {
                exec(
                    &test,
                    "UPDATE workspaces SET deleted_at = now() WHERE id = $1",
                    ws.uuid(),
                )
                .await;
            }
            Category::Suppressed => {
                sqlx::query("INSERT INTO suppressions (workspace_id, email, reason) VALUES ($1, 'Ada@Example.test', 'manual')")
                    .bind(ws.uuid())
                    .execute(test.system.pool())
                    .await
                    .unwrap();
            }
            Category::SenderArchived => {
                exec(
                    &test,
                    "UPDATE connections SET status = 'archived' WHERE id = $1",
                    sender.connection.uuid(),
                )
                .await;
            }
            _ => unreachable!("only the ending checks are listed"),
        }

        let started = test.start(ws, &sender, OWNER, &claimed[0]).await;
        assert_eq!(started, Started::Ended(state), "{name}");
        assert!(
            queue_row(&test, message).await.is_none(),
            "{name}: the queue row is gone"
        );
        let (stored, attempt, quota, attempt_category) = message_and_attempt(&test, message).await;
        assert_eq!(stored, state.as_str(), "{name}");
        assert_eq!(attempt.as_deref(), Some(outcome), "{name}");
        assert_eq!(quota, "released", "{name}");
        assert_eq!(attempt_category.as_deref(), Some(name), "{name}");
        assert_eq!(test.ledger(sender.connection).await, (0, 0), "{name}");
        let told: Vec<(String, serde_json::Value)> =
            sqlx::query_as("SELECT type, payload FROM outbox_events WHERE subject_id = $1")
                .bind(message.uuid())
                .fetch_all(test.system.pool())
                .await
                .unwrap();
        assert_eq!(told.len(), 1, "{name}");
        assert_eq!(told[0].0, "message.failed", "{name}");
        assert_eq!(told[0].1["data"]["category"], name, "{name}: {}", told[0].1);
        if category == Category::Expired {
            let resolution: Option<String> =
                sqlx::query_scalar("SELECT resolution FROM recipient_holds WHERE message_id = $1")
                    .bind(message.uuid())
                    .fetch_one(test.system.pool())
                    .await
                    .unwrap();
            assert_eq!(
                resolution.as_deref(),
                Some("expired"),
                "the hold it caused is resolved"
            );
        }
        if category == Category::Suppressed {
            let platform = test
                .sender(ws, &SenderSpec::relay("codes@example.test"))
                .await;
            let code = test
                .direct_message(ws, &platform, &["ada@example.test"], -60)
                .await;
            sqlx::query("UPDATE messages SET kind = 'transactional' WHERE id = $1")
                .bind(code.uuid())
                .execute(test.system.pool())
                .await
                .unwrap();
            let claimed = test.claimed(ws, &platform, OWNER).await;
            let started = test.start(ws, &platform, OWNER, &claimed[0]).await;
            assert!(
                matches!(started, Started::Submit(_)),
                "transactional mail is not held by a suppression: {started:?}"
            );
        }
    }
}

/// The renewal is fenced by owner, generation, state and a live lease, and counted from its own
/// statement's clock. A Start whose lease expired before it (a stalled sender) submits nothing
/// and writes nothing; once recovery returned the row and another replica claimed it again, the
/// old generation is refused and the new one starts. A Start held up between its transaction's
/// beginning and its renewal (observably blocked by the identity's lock) marks the submission
/// after the lock is released, and its lease runs the whole budget plus the margin from then, not from the
/// transaction's start: the submission never outlives the lease that protects it.
#[tokio::test]
async fn the_renewal_is_fenced_and_counted_from_its_own_clock() {
    let test = TestDb::new().await;
    let ws = test.workspace("renewal").await.id;
    let sender = test
        .sender(ws, &SenderSpec::relay("relay@example.test"))
        .await;
    let message = test
        .direct_message(ws, &sender, &["ada@example.test"], -60)
        .await;
    let first = test.claimed(ws, &sender, OWNER).await[0];
    sqlx::query("UPDATE delivery_queue SET lease_expires_at = now() - interval '1 second' WHERE message_id = $1")
        .bind(message.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    assert_eq!(test.start(ws, &sender, OWNER, &first).await, Started::Lost);
    let row = queue_row(&test, message).await.unwrap();
    assert_eq!(
        (row.state.as_str(), row.submission_started_at),
        ("claimed", None),
        "nothing written"
    );
    assert_eq!(message_and_attempt(&test, message).await.2, "reserved");

    assert_eq!(recover::sweep(&test.worker).await.unwrap().requeued, 1);
    let second = test.claimed(ws, &sender, "sender-b").await[0];
    assert_eq!(second.generation, first.generation + 1);
    assert_eq!(
        test.start(ws, &sender, "sender-b", &first).await,
        Started::Lost,
        "the old generation"
    );

    let mut holder = test.system.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM sender_identities WHERE id = $1 FOR UPDATE")
        .bind(sender.identity.uuid())
        .execute(&mut *holder)
        .await
        .unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    let (started, released) = tokio::join!(test.start(ws, &sender, "sender-b", &second), async {
        crate::testing::wait_for_lock(&test, blocker).await;
        let released: Timestamp = sqlx::query_scalar("SELECT clock_timestamp() FROM pg_sleep(0.1)")
            .fetch_one(&mut *holder)
            .await
            .unwrap();
        holder.commit().await.unwrap();
        released
    });
    let Started::Submit(begun) = started else {
        panic!("the new generation starts: {started:?}");
    };
    assert!(
        begun.started >= released,
        "the marker is written after the wait"
    );
    let (lease, first_submitted, deadline): (Timestamp, Timestamp, Timestamp) = sqlx::query_as(
        "SELECT lease_expires_at, first_submitted_at, deadline_at FROM delivery_queue WHERE message_id = $1",
    )
    .bind(message.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert!(
        lease >= begun.started.plus(Duration::from_secs(330)),
        "the lease runs the budget and its margin from the renewal: {lease} for a marker at {}",
        begun.started
    );
    assert_eq!(first_submitted, begun.started);
    assert_eq!(
        deadline,
        begun.started.plus(Duration::from_secs(86_400)),
        "the retry window"
    );
}

/// The pacing clock is the Start's to move, once per cold send on a paced sender: mail created
/// through the API starts on a mailbox without touching it (and carries no scheduled instant);
/// a cold message moves it from the instant it was scheduled for to
/// `next_phase_at(max(scheduled + interval, started + interval − 30 s), phase)`, one interval,
/// not two, whatever the claim did to it before.
#[tokio::test]
async fn the_clock_moves_once_for_cold_mail_and_never_for_api_mail() {
    let test = TestDb::new().await;
    let ws = test.workspace("clock").await.id;
    let sender = test
        .sender(ws, &SenderSpec::mailbox("ada@example.test"))
        .await;
    let campaign = test.campaign(ws, &sender, None).await;
    let cold = test
        .campaign_message(ws, campaign, &sender, "p@example.test", -120)
        .await;
    let api = test
        .direct_message(ws, &sender, &["d@example.test"], -60)
        .await;
    let pool = test.system.clone();
    let next_send_at = || {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Timestamp>("SELECT next_send_at FROM connections WHERE id = $1")
                .bind(sender.connection.uuid())
                .fetch_one(pool.pool())
                .await
                .unwrap()
        }
    };

    let claimed = test.claimed(ws, &sender, OWNER).await;
    assert_eq!(claimed[0].message, api);
    let untouched = next_send_at().await;
    let Started::Submit(begun) = test.start(ws, &sender, OWNER, &claimed[0]).await else {
        panic!("the API message starts");
    };
    assert_eq!(begun.scheduled, None);
    assert_eq!(
        next_send_at().await,
        untouched,
        "API mail never moves the clock"
    );
    test.finish(
        ws,
        &sender,
        OWNER,
        &[crate::testing::answered(
            &claimed[0],
            crate::domain::policy::delivery::Answer::Accepted,
            begun.started,
        )],
    )
    .await;

    let claimed: Vec<Claimed> = test.claimed(ws, &sender, OWNER).await;
    assert_eq!(claimed[0].message, cold);
    let scheduled = next_send_at().await;
    let Started::Submit(begun) = test.start(ws, &sender, OWNER, &claimed[0]).await else {
        panic!("the cold message starts");
    };
    assert_eq!(begun.scheduled, Some(scheduled));
    let expected: Timestamp = sqlx::query_scalar(
        "SELECT next_phase_at(greatest($1 + interval '5 minutes', $2 + interval '5 minutes' - interval '30 seconds'), 41)",
    )
    .bind(scheduled)
    .bind(begun.started)
    .fetch_one(test.worker.pool())
    .await
    .unwrap();
    assert_eq!(
        next_send_at().await,
        expected,
        "moved once, from its scheduled instant"
    );
}

/// A removal and a Start serialise on the sender identity's row, in both orders. Removal first:
/// a transaction disabling the identity holds its row; the Start waits for it, then sees the
/// identity disabled and returns the message unstarted. Start first: a Start held after its
/// locks (here behind the message's attempt row) makes the removal wait until it committed, and
/// the started message stays in flight: a removal never pulls a message out from under a
/// submission it did not see.
#[tokio::test]
async fn a_removal_and_a_start_serialise_on_the_identity() {
    let test = TestDb::new().await;
    let ws: WorkspaceId = test.workspace("removal").await.id;
    let sender: TestSender = test
        .sender(ws, &SenderSpec::relay("relay@example.test"))
        .await;
    let first = test
        .direct_message(ws, &sender, &["a@example.test"], -60)
        .await;
    let second = test
        .direct_message(ws, &sender, &["b@example.test"], -60)
        .await;
    let claimed = test.claimed(ws, &sender, OWNER).await;
    let of = |message: Id<Message>| *claimed.iter().find(|c| c.message == message).unwrap();
    let disable = "UPDATE sender_identities SET enabled = false WHERE id = $1";

    let mut removal = test.system.begin().await.unwrap();
    sqlx::query(disable)
        .bind(sender.identity.uuid())
        .execute(&mut *removal)
        .await
        .unwrap();
    let ((started, waited), ()) = tokio::join!(
        async {
            let at = Instant::now();
            (
                test.start(ws, &sender, OWNER, &of(first)).await,
                at.elapsed(),
            )
        },
        async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            removal.commit().await.unwrap();
        }
    );
    assert!(
        waited >= Duration::from_millis(900),
        "the Start waited for the removal: {waited:?}"
    );
    assert_eq!(started, Started::Returned(Held::IdentityUnavailable));
    sqlx::query("UPDATE sender_identities SET enabled = true WHERE id = $1")
        .bind(sender.identity.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();

    let in_flight = of(second);
    let mut gate = test.system.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM attempts WHERE message_id = $1 FOR UPDATE")
        .bind(second.uuid())
        .execute(&mut *gate)
        .await
        .unwrap();
    let (started, waited, ()) = tokio::join!(
        test.start(ws, &sender, OWNER, &in_flight),
        async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let at = Instant::now();
            sqlx::query(disable)
                .bind(sender.identity.uuid())
                .execute(test.system.pool())
                .await
                .unwrap();
            at.elapsed()
        },
        async {
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            gate.rollback().await.unwrap();
        }
    );
    assert!(
        matches!(started, Started::Submit(_)),
        "the Start came first: {started:?}"
    );
    assert!(
        waited >= Duration::from_millis(900),
        "the removal waited for the Start: {waited:?}"
    );
    assert_eq!(queue_row(&test, second).await.unwrap().state, "in_flight");
}
