//! The Finish's store and gate tests, against real PostgreSQL with the worker login: settlement
//! once per attempt against its own reservation day, a lost lease that records nothing, the
//! connection's and the quota scope's breakers as batches move them (including a success racing
//! a scoped failure), a release clearing the budget wait, an uncertain answer on a mailbox asking
//! for its Sent-folder check, the Message-ID directory of Amazon SES, and the error-level event of
//! a final failure.

use std::time::{Duration, Instant};

use uuid::Uuid;

use super::{Finished, Report, Reported};
use crate::delivery::claim::{Claim, Claimed};
use crate::delivery::recover;
use crate::domain::ids::{Id, Message, WorkspaceId};
use crate::domain::policy::delivery::{Answer, Cause, Failure, RefusalScope};
use crate::domain::time::Timestamp;
use crate::telemetry::Event;
use crate::telemetry::capture::Capture;
use crate::telemetry::mirror;
use crate::testing::{SenderSpec, TestDb, TestSender, answered, refusal};

const OWNER: &str = "sender-a";

/// A breaker as stored: failures, paused until, opened at, the probe's message.
type BreakerRow = (i32, Option<Timestamp>, Option<Timestamp>, Option<Uuid>);

/// `connection`'s breaker.
async fn connection_breaker(test: &TestDb, sender: &TestSender) -> BreakerRow {
    sqlx::query_as(
        "SELECT consecutive_failures, paused_until, breaker_opened_at, probe_message_id
           FROM connections WHERE id = $1",
    )
    .bind(sender.connection.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap()
}

/// `scope`'s breaker.
async fn scope_breaker(test: &TestDb, scope: Uuid) -> BreakerRow {
    sqlx::query_as(
        "SELECT consecutive_failures, paused_until, breaker_opened_at, probe_message_id
           FROM quota_scopes WHERE id = $1",
    )
    .bind(scope)
    .fetch_one(test.system.pool())
    .await
    .unwrap()
}

/// A relay of `scope` (no scope when `None`) sending as `address`.
async fn relay(test: &TestDb, ws: WorkspaceId, address: &str, scope: Option<Uuid>) -> TestSender {
    test.sender(
        ws,
        &SenderSpec {
            scope,
            ..SenderSpec::relay(address)
        },
    )
    .await
}

/// Claims and starts everything `sender` has due: each claimed message with its marker.
async fn claim_and_start(
    test: &TestDb,
    ws: WorkspaceId,
    sender: &TestSender,
) -> Vec<(Claimed, Timestamp)> {
    let mut started = Vec::new();
    for claimed in test.claimed(ws, sender, OWNER).await {
        started.push((claimed, test.begun(ws, sender, OWNER, &claimed).await));
    }
    started
}

/// Each `(claimed, marker)` answered with `answer`.
fn all(started: &[(Claimed, Timestamp)], answer: Answer) -> Vec<Report> {
    started
        .iter()
        .map(|(claimed, at)| answered(claimed, answer, *at))
        .collect()
}

/// A transient refusal concerning `scope`.
fn transient(scope: RefusalScope) -> Answer {
    refusal(Failure::Transient, scope, Cause::Refused)
}

/// Ends the pause of `table`'s open breaker row `id` as if it had been waited out: the pause is
/// over and the opening ten minutes older, so the breaker is half-open and a probe started now
/// is after the opening on any clock (an opening is stamped by the application's clock, a
/// submission's start by the database's).
async fn pause_over(test: &TestDb, table: &str, id: Uuid) {
    let sql = format!(
        "UPDATE {table} SET paused_until = now() - interval '1 second',
                breaker_opened_at = breaker_opened_at - interval '10 minutes'
          WHERE id = $1"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .execute(test.system.pool())
        .await
        .unwrap();
}

/// Each attempt settles exactly once, against its own reservation day and scope: a batch whose
/// first message reserved yesterday (claimed before midnight) and whose second reserved today
/// moves each unit from `reserved` to `used` on its own day, on the connection's ledger and on
/// the scope's messages and recipients. The same batch delivered again (its acknowledgement
/// lost) finds no lease it still holds and settles nothing: the ledgers, whose units may never
/// go below zero, are unchanged, and each message was told sent once.
#[tokio::test]
async fn each_attempt_settles_once_against_its_own_day() {
    let test = &TestDb::new().await;
    let ws = test.workspace("midnight").await.id;
    let scope = test
        .quota_scope(ws, "sendgrid", Some(1_000), Some(1_000))
        .await;
    let sender = relay(test, ws, "relay@example.test", Some(scope)).await;
    let before_midnight = test
        .direct_message(ws, &sender, &["a@example.test"], -120)
        .await;
    let after_midnight = test
        .direct_message(ws, &sender, &["b@example.test", "c@example.test"], -60)
        .await;
    let started = claim_and_start(test, ws, &sender).await;
    assert_eq!(started.len(), 2);
    let mut tx = test.system.begin().await.unwrap();
    for sql in [
        "INSERT INTO connection_usage (workspace_id, connection_id, day, reserved)
         SELECT workspace_id, connection_id, reserved_day - 1, 1 FROM attempts WHERE message_id = $1
         ON CONFLICT (workspace_id, connection_id, day) DO UPDATE SET reserved = connection_usage.reserved + 1",
        "UPDATE connection_usage u SET reserved = u.reserved - 1 FROM attempts a
          WHERE a.message_id = $1 AND u.connection_id = a.connection_id AND u.day = a.reserved_day",
        "INSERT INTO quota_scope_usage (workspace_id, scope_id, day, messages_reserved, recipients_reserved)
         SELECT workspace_id, quota_scope_id, reserved_day - 1, 1, recipient_count FROM attempts WHERE message_id = $1
         ON CONFLICT (workspace_id, scope_id, day) DO UPDATE
            SET messages_reserved = quota_scope_usage.messages_reserved + 1,
                recipients_reserved = quota_scope_usage.recipients_reserved + excluded.recipients_reserved",
        "UPDATE quota_scope_usage u SET messages_reserved = u.messages_reserved - 1,
                recipients_reserved = u.recipients_reserved - a.recipient_count
           FROM attempts a WHERE a.message_id = $1 AND u.scope_id = a.quota_scope_id AND u.day = a.reserved_day",
        "UPDATE attempts SET reserved_day = reserved_day - 1 WHERE message_id = $1",
    ] {
        sqlx::query(sql)
            .bind(before_midnight.uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();

    let reports = all(&started, Answer::Accepted);
    let finished = test.finish(ws, &sender, OWNER, &reports).await;
    assert_eq!(
        (finished.settled, finished.consumed, finished.lost),
        (2, 2, 0)
    );
    let days = move || async move {
        let connection: Vec<(i32, i32)> = sqlx::query_as(
            "SELECT reserved, used FROM connection_usage WHERE connection_id = $1 ORDER BY day",
        )
        .bind(sender.connection.uuid())
        .fetch_all(test.system.pool())
        .await
        .unwrap();
        let scope: Vec<(i32, i32, i32, i32)> = sqlx::query_as(
            "SELECT messages_reserved, messages_used, recipients_reserved, recipients_used
               FROM quota_scope_usage WHERE scope_id = $1 ORDER BY day",
        )
        .bind(scope)
        .fetch_all(test.system.pool())
        .await
        .unwrap();
        (connection, scope)
    };
    let settled = days().await;
    assert_eq!(
        settled.0,
        [(0, 1), (0, 1)],
        "yesterday's unit and today's, each on its day"
    );
    assert_eq!(
        settled.1,
        [(0, 1, 0, 1), (0, 1, 0, 2)],
        "the scope's messages and recipients"
    );

    let again = test.finish(ws, &sender, OWNER, &reports).await;
    assert_eq!(
        again,
        Finished {
            lost: 2,
            ..Finished::default()
        }
    );
    assert_eq!(days().await, settled, "nothing settles twice");
    for message in [before_midnight, after_midnight] {
        assert_eq!(test.told(ws, message.uuid()).await, ["message.sent"]);
    }
}

/// A report whose lease was lost records nothing: neither for a message recovery made
/// `uncertain` (its Start had marked it) nor for one recovery returned and another replica claimed
/// again in a new generation. The batch counts both as lost; the message states, the attempts,
/// the ledger, the evidence and the outbox stay as recovery and the new claim left them.
#[tokio::test]
async fn a_lost_lease_records_nothing() {
    let test = &TestDb::new().await;
    let ws = test.workspace("lost").await.id;
    let sender = relay(test, ws, "relay@example.test", None).await;
    let marked = test
        .direct_message(ws, &sender, &["a@example.test"], -120)
        .await;
    let unmarked = test
        .direct_message(ws, &sender, &["b@example.test"], -60)
        .await;
    let claimed = test.claimed(ws, &sender, OWNER).await;
    let started = test.begun(ws, &sender, OWNER, &claimed[0]).await;
    assert_eq!(claimed[0].message, marked);
    sqlx::query("UPDATE delivery_queue SET lease_expires_at = now() - interval '1 second' WHERE connection_id = $1")
        .bind(sender.connection.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    let recovered = recover::sweep(&test.worker).await.unwrap();
    assert_eq!((recovered.requeued, recovered.uncertain), (1, 1));
    let again = test.claimed(ws, &sender, "sender-b").await;
    assert_eq!(again[0].message, unmarked);

    let snapshot = move || async move {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT m.state, a.quota_state, coalesce(a.outcome, '') FROM messages m
               JOIN attempts a ON a.workspace_id = m.workspace_id AND a.message_id = m.id
              WHERE m.workspace_id = $1 ORDER BY m.id, a.attempt_number",
        )
        .bind(ws.uuid())
        .fetch_all(test.system.pool())
        .await
        .unwrap();
        let events: i64 =
            sqlx::query_scalar("SELECT count(*) FROM delivery_events WHERE workspace_id = $1")
                .bind(ws.uuid())
                .fetch_one(test.system.pool())
                .await
                .unwrap();
        let told: i64 =
            sqlx::query_scalar("SELECT count(*) FROM outbox_events WHERE workspace_id = $1")
                .bind(ws.uuid())
                .fetch_one(test.system.pool())
                .await
                .unwrap();
        (rows, events, told, test.ledger(sender.connection).await)
    };
    let before = snapshot().await;
    let finished = test
        .finish(
            ws,
            &sender,
            OWNER,
            &[
                answered(&claimed[0], Answer::Accepted, started),
                answered(&claimed[1], Answer::Accepted, started),
            ],
        )
        .await;
    assert_eq!((finished.lost, finished.settled), (2, 0));
    assert_eq!(snapshot().await, before);
}

/// The connection's breaker as batches move it: three failures of the connection in one batch
/// count to two and open it on the third (paused, its opening stamped), the messages going back
/// to the queue for a retry; once the pause is over the claim admits one probe and records it;
/// the probe's acceptance, started after the opening, closes the breaker (count, pause, opening
/// and probe cleared).
#[tokio::test]
async fn the_third_failure_opens_the_connection_and_its_probe_closes_it() {
    let test = &TestDb::new().await;
    let ws = test.workspace("breaker").await.id;
    let sender = relay(test, ws, "relay@example.test", None).await;
    for to in ["a@example.test", "b@example.test", "c@example.test"] {
        test.direct_message(ws, &sender, &[to], -60).await;
    }
    let started = claim_and_start(test, ws, &sender).await;
    assert_eq!(started.len(), 3);
    test.finish(
        ws,
        &sender,
        OWNER,
        &all(&started, transient(RefusalScope::Connection)),
    )
    .await;
    let (failures, paused_until, opened_at, probe) = connection_breaker(test, &sender).await;
    assert_eq!((failures, probe), (3, None));
    let opened_at = opened_at.expect("the opening is stamped");
    assert!(
        paused_until.is_some_and(|until| until > opened_at),
        "paused"
    );
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM delivery_queue WHERE connection_id = $1 AND state = 'queued' AND run_at > now()",
    )
    .bind(sender.connection.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(queued, 3, "each goes back for a later attempt");

    pause_over(test, "connections", sender.connection.uuid()).await;
    let probe = test
        .direct_message(ws, &sender, &["d@example.test"], -60)
        .await;
    let Claim::Wave(wave) = test.claim(ws, &sender, OWNER).await else {
        panic!("the half-open breaker admits a probe");
    };
    assert!(wave.probe);
    assert_eq!(wave.messages.len(), 1);
    assert_eq!(wave.messages[0].message, probe);
    assert_eq!(
        connection_breaker(test, &sender).await.3,
        Some(probe.uuid())
    );
    let at = test.begun(ws, &sender, OWNER, &wave.messages[0]).await;
    test.finish(
        ws,
        &sender,
        OWNER,
        &[answered(&wave.messages[0], Answer::Accepted, at)],
    )
    .await;
    assert_eq!(
        connection_breaker(test, &sender).await,
        (0, None, None, None)
    );
}

/// A quota scope's breaker closes only through its own probe, started after its latest opening,
/// across the scope's connections. Three refusals naming the scope open it (with the provider's
/// text as the pause's detail) and leave the connection's own breaker alone. Half-open, a claim
/// admits a probe; a success of another connection's message, started before the opening,
/// leaves the scope as it is; a later scoped failure of that connection reopens it after the
/// probe started, so the probe's success closes nothing. When the pause is over again, a new
/// probe, started after the latest opening, closes it and clears the detail.
#[tokio::test]
async fn a_scope_closes_only_through_its_own_probe() {
    let test = &TestDb::new().await;
    let ws = test.workspace("scope").await.id;
    let scope = test.quota_scope(ws, "sendgrid", None, None).await;
    let a = relay(test, ws, "a@relay.test", Some(scope)).await;
    let b = relay(test, ws, "b@relay.test", Some(scope)).await;
    for to in ["b1@example.test", "b2@example.test"] {
        test.direct_message(ws, &b, &[to], -60).await;
    }
    let in_flight = claim_and_start(test, ws, &b).await;
    for to in ["a1@example.test", "a2@example.test", "a3@example.test"] {
        test.direct_message(ws, &a, &[to], -60).await;
    }
    let failing = claim_and_start(test, ws, &a).await;
    test.finish(
        ws,
        &a,
        OWNER,
        &all(&failing, transient(RefusalScope::QuotaScope)),
    )
    .await;
    let (failures, paused_until, opened_at, _) = scope_breaker(test, scope).await;
    assert_eq!(failures, 3);
    assert!(
        paused_until.is_some() && opened_at.is_some(),
        "the scope is open"
    );
    let detail: Option<String> =
        sqlx::query_scalar("SELECT paused_detail FROM quota_scopes WHERE id = $1")
            .bind(scope)
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(detail.as_deref(), Some("451 4.3.0 Try again later"));
    assert_eq!(
        connection_breaker(test, &a).await,
        (0, None, None, None),
        "the connection is not blamed"
    );

    pause_over(test, "quota_scopes", scope).await;
    let first_probe = test.direct_message(ws, &a, &["p1@example.test"], -60).await;
    let probe = test.claimed(ws, &a, OWNER).await;
    assert_eq!(probe.len(), 1);
    assert_eq!(scope_breaker(test, scope).await.3, Some(first_probe.uuid()));
    let half_open = scope_breaker(test, scope).await;
    test.finish(
        ws,
        &b,
        OWNER,
        &[answered(&in_flight[0].0, Answer::Accepted, in_flight[0].1)],
    )
    .await;
    assert_eq!(
        scope_breaker(test, scope).await,
        half_open,
        "another message's success changes nothing"
    );
    let probe_started = test.begun(ws, &a, OWNER, &probe[0]).await;
    test.finish(
        ws,
        &b,
        OWNER,
        &[answered(
            &in_flight[1].0,
            transient(RefusalScope::QuotaScope),
            in_flight[1].1,
        )],
    )
    .await;
    let reopened = scope_breaker(test, scope).await;
    assert_eq!(
        (reopened.0, reopened.3),
        (4, None),
        "reopened, its probe cleared"
    );
    assert!(
        reopened.2 > half_open.2
            && reopened
                .1
                .is_some_and(|until| until > crate::process::now()),
        "a new opening, paused again"
    );
    test.finish(
        ws,
        &a,
        OWNER,
        &[answered(&probe[0], Answer::Accepted, probe_started)],
    )
    .await;
    assert_eq!(
        scope_breaker(test, scope).await,
        reopened,
        "a probe started before the reopening closes nothing"
    );

    pause_over(test, "quota_scopes", scope).await;
    test.direct_message(ws, &a, &["p2@example.test"], -60).await;
    let probe = test.claimed(ws, &a, OWNER).await;
    let at = test.begun(ws, &a, OWNER, &probe[0]).await;
    test.finish(ws, &a, OWNER, &[answered(&probe[0], Answer::Accepted, at)])
        .await;
    assert_eq!(scope_breaker(test, scope).await, (0, None, None, None));
    let detail: Option<String> =
        sqlx::query_scalar("SELECT paused_detail FROM quota_scopes WHERE id = $1")
            .bind(scope)
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(detail, None);
}

/// A success decides whether to lock its scope from a read taken before any lock, so it never
/// waits on a scoped failure it cannot see and never deadlocks with one. Another connection's
/// finish holds the scope's row (then its connection's) with a failure not yet committed: a
/// success that read the count at zero takes no scope lock and does not wait, and the failure it
/// did not see stays counted; once the count is one, a success locks the scope first, waits for
/// the failure's commit, and resets the count.
#[tokio::test]
async fn a_success_waits_for_a_scoped_failure_only_when_it_saw_failures() {
    let test = &TestDb::new().await;
    let ws = test.workspace("race").await.id;
    let scope = test.quota_scope(ws, "sendgrid", None, None).await;
    let a = relay(test, ws, "a@relay.test", Some(scope)).await;
    let b = relay(test, ws, "b@relay.test", Some(scope)).await;
    for to in ["b1@example.test", "b2@example.test"] {
        test.direct_message(ws, &b, &[to], -60).await;
    }
    let started = claim_and_start(test, ws, &b).await;
    let failure = move || async move {
        let mut tx = test.system.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM quota_scopes WHERE id = $1 FOR UPDATE")
            .bind(scope)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("SELECT 1 FROM connections WHERE id = $1 FOR UPDATE")
            .bind(a.connection.uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE quota_scopes SET consecutive_failures = consecutive_failures + 1 WHERE id = $1",
        )
        .bind(scope)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx
    };
    let success = move |index: usize| {
        let (claimed, at) = started[index];
        async move {
            let begun = Instant::now();
            test.finish(ws, &b, OWNER, &[answered(&claimed, Answer::Accepted, at)])
                .await;
            begun.elapsed()
        }
    };
    let release = |tx: crate::db::Tx| async move {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        tx.commit().await.unwrap();
    };

    let held = failure().await;
    let (waited, ()) = tokio::join!(success(0), release(held));
    assert!(
        waited < Duration::from_millis(1_000),
        "no scope lock taken: {waited:?}"
    );
    assert_eq!(
        scope_breaker(test, scope).await.0,
        1,
        "the failure it did not see stays counted"
    );

    let held = failure().await;
    let (waited, ()) = tokio::join!(success(1), release(held));
    assert!(
        waited >= Duration::from_millis(1_000),
        "the scope locked first: {waited:?}"
    );
    assert_eq!(
        scope_breaker(test, scope).await.0,
        0,
        "reset after the failure committed"
    );
}

/// A message returned unstarted (a deferral before its Start, a stopping sender) frees its unit,
/// so the Finish clears the connection's budget wait with the release: the connection is a
/// candidate again at once instead of waiting for a budget that is no longer spent. The message
/// is queued again and its attempt closed `released`.
#[tokio::test]
async fn a_release_clears_the_budget_wait() {
    let test = &TestDb::new().await;
    let ws = test.workspace("release").await.id;
    let sender = relay(test, ws, "relay@example.test", None).await;
    let message = test
        .direct_message(ws, &sender, &["a@example.test"], -60)
        .await;
    let claimed = test.claimed(ws, &sender, OWNER).await;
    sqlx::query("UPDATE connections SET next_claim_at = now() + interval '1 hour' WHERE id = $1")
        .bind(sender.connection.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    let finished = test
        .finish(
            ws,
            &sender,
            OWNER,
            &[Report {
                message,
                generation: claimed[0].generation,
                reported: Reported::Released { run_at: None },
            }],
        )
        .await;
    assert_eq!((finished.settled, finished.released), (1, 1));
    let (wait, state, outcome): (Option<Timestamp>, String, Option<String>) = sqlx::query_as(
        "SELECT c.next_claim_at, m.state, a.outcome FROM connections c, messages m, attempts a
          WHERE c.id = $1 AND m.id = $2 AND a.message_id = $2",
    )
    .bind(sender.connection.uuid())
    .bind(message.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        (wait, state.as_str(), outcome.as_deref()),
        (None, "queued", Some("released"))
    );
    assert_eq!(test.ledger(sender.connection).await, (0, 0));
}

/// An `uncertain` answer (the reply after the content was lost) on a mailbox asks the mailbox's
/// `connection.check` to read its Sent folder for the message, in the Finish's transaction; on a
/// relay, which has no folder to read, nothing is asked. Either way the message is `uncertain`,
/// its unit consumed (the provider may have it), its queue row gone, `message.uncertain` told,
/// and the connection's breaker untouched (the answer concerns the message).
#[tokio::test]
async fn an_uncertain_answer_on_a_mailbox_asks_for_its_check() {
    let test = &TestDb::new().await;
    let ws = test.workspace("uncertain").await.id;
    let mailbox = test
        .sender(ws, &SenderSpec::mailbox("ada@example.test"))
        .await;
    let relay = relay(test, ws, "relay@example.test", None).await;
    let lost = refusal(Failure::Uncertain, RefusalScope::Message, Cause::NoReply);
    for sender in [mailbox, relay] {
        let message = test
            .direct_message(ws, &sender, &["p@example.test"], -60)
            .await;
        let started = claim_and_start(test, ws, &sender).await;
        test.finish(ws, &sender, OWNER, &all(&started, lost)).await;
        let (state, quota, queued): (String, String, bool) = sqlx::query_as(
            "SELECT m.state, a.quota_state, EXISTS (SELECT 1 FROM delivery_queue q WHERE q.message_id = m.id)
               FROM messages m JOIN attempts a ON a.workspace_id = m.workspace_id AND a.message_id = m.id
              WHERE m.id = $1",
        )
        .bind(message.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        assert_eq!(
            (state.as_str(), quota.as_str(), queued),
            ("uncertain", "consumed", false)
        );
        assert_eq!(test.told(ws, message.uuid()).await, ["message.uncertain"]);
        assert_eq!(
            connection_breaker(test, &sender).await,
            (0, None, None, None)
        );
    }
    let checks: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload -> 'connection' FROM jobs WHERE workspace_id = $1 AND kind = 'connection.check'",
    )
    .bind(ws.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert_eq!(checks, [serde_json::to_value(mailbox.connection).unwrap()]);
}

/// Amazon SES replaces our Message-ID with its own, so its acceptance files the token SES
/// returned in the Message-ID directory, with the message's thread and envelope, and clears the
/// thread's latest Message-ID when it names this message: a reply naming the header the
/// recipient saw still finds its thread, and the next follow-up never cites an id the recipient
/// never saw.
#[tokio::test]
async fn an_ses_acceptance_files_its_message_id() {
    let test = &TestDb::new().await;
    let ws = test.workspace("ses").await.id;
    let account = test.quota_scope(ws, "ses", None, None).await;
    let sender = test
        .sender(
            ws,
            &SenderSpec {
                provider: "ses",
                scope: Some(account),
                ..SenderSpec::relay("hello@example.test")
            },
        )
        .await;
    let message: Id<Message> = test
        .direct_message(ws, &sender, &["ada@example.test", "bob@example.test"], -60)
        .await;
    let thread: Uuid = sqlx::query_scalar(
        "WITH t AS (INSERT INTO threads (workspace_id, sender_identity_id, last_message_id, last_internet_message_id)
                    SELECT workspace_id, sender_identity_id, id, internet_message_id FROM messages WHERE id = $1
                    RETURNING workspace_id, id)
         UPDATE messages m SET thread_id = t.id FROM t WHERE m.workspace_id = t.workspace_id AND m.id = $1
         RETURNING t.id",
    )
    .bind(message.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    let started = claim_and_start(test, ws, &sender).await;
    let token = "0102018f4c3b2a10-6e1d7c3a-5b2f-4e8d-9a1c-2f3e4d5c6b7a-000000";
    let mut report = answered(&started[0].0, Answer::Accepted, started[0].1);
    if let Reported::Answered(answer) = &mut report.reported {
        answer.provider_message_id = Some(token.to_owned());
    }
    test.finish(ws, &sender, OWNER, &[report]).await;
    let filed: (Uuid, Uuid, Vec<String>) = sqlx::query_as(
        "SELECT message_id, thread_id, recipients FROM message_id_directory WHERE workspace_id = $1 AND lookup_key = $2",
    )
    .bind(ws.uuid())
    .bind(token)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        filed,
        (
            message.uuid(),
            thread,
            vec!["ada@example.test".to_owned(), "bob@example.test".to_owned()]
        )
    );
    let latest: Option<String> =
        sqlx::query_scalar("SELECT last_internet_message_id FROM threads WHERE id = $1")
            .bind(thread)
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(latest, None);
}

/// A message's final failure is one error-level `delivery.failure` event, emitted once its finish
/// has committed: a permanent refusal reports its message as `failed`, while a transient refusal
/// in the same batch, which only goes back to the queue, reports nothing (the wave's event counts
/// it), so a provider hiccup that heals pages nobody. The event is counted on both sides of the
/// coverage reconciliation.
#[tokio::test]
async fn only_a_final_failure_is_reported_as_an_error_event() {
    let test = &TestDb::new().await;
    let ws = test.workspace("failure").await.id;
    let sender = relay(test, ws, "relay@example.test", None).await;
    let permanent = test
        .direct_message(ws, &sender, &["p@example.test"], -60)
        .await;
    test.direct_message(ws, &sender, &["t@example.test"], -60)
        .await;
    let started = claim_and_start(test, ws, &sender).await;
    assert_eq!(started.len(), 2);
    let reports: Vec<Report> = started
        .iter()
        .map(|(claimed, at)| {
            let failure = if claimed.message == permanent {
                Failure::Permanent
            } else {
                Failure::Transient
            };
            answered(
                claimed,
                refusal(failure, RefusalScope::Message, Cause::Refused),
                *at,
            )
        })
        .collect();

    let capture = Capture::default();
    let _guard = capture.install();
    test.finish(ws, &sender, OWNER, &reports).await;
    let failures = capture.named_since(0, Event::DeliveryFailure.as_str());
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].field("message_id"), permanent.to_string());
    assert_eq!(failures[0].field("state"), "failed");
    let (seen, units) = mirror::counts();
    assert_eq!(seen.get(&Event::DeliveryFailure).copied(), Some(1));
    assert_eq!(seen, units);
}
