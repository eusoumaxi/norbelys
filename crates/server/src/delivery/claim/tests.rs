//! The claim's store and gate tests, against real PostgreSQL with the worker login: the
//! candidate scans and their cutoffs, one message in progress per mailbox across replicas, the
//! budgets' waits, a quota scope's last unit, a half-open breaker's one probe, the idle clock, send
//! windows, and the clock arithmetic shared with the database.

use std::time::{Duration, Instant};

use jiff::SignedDuration;
use jiff::tz::TimeZone;
use serde_json::json;
use uuid::Uuid;

use super::{Candidate, Claim, Cursors, Scan, Wave, claim, page, turn};
use crate::db::Database;
use crate::delivery::finish::{self, Answered, Report, Reported};
use crate::delivery::start::{self, Start, Started};
use crate::domain::ids::{Connection, Id, Message, WorkspaceId};
use crate::domain::policy::delivery::{Answer, Source};
use crate::domain::schedule;
use crate::domain::time::Timestamp;
use crate::testing::{SenderSpec, TestDb, TestSender};

const OWNER: &str = "sender-test";

/// A candidate for `connection`, as the scans would hand it over.
fn candidate(
    workspace: WorkspaceId,
    connection: Id<Connection>,
    scope_half_open: bool,
) -> Candidate {
    Candidate {
        workspace,
        connection,
        scan: Scan::Clock,
        key: (Timestamp(jiff::Timestamp::UNIX_EPOCH), connection.uuid()),
        scope_half_open,
    }
}

/// One claim of `connection` on `db`, every limiter allowing.
async fn take_on(
    db: &Database,
    workspace: WorkspaceId,
    connection: Id<Connection>,
    half_open: bool,
) -> Claim {
    claim(
        db,
        OWNER,
        &candidate(workspace, connection, half_open),
        16,
        &mut |_| true,
    )
    .await
    .expect("the claim runs")
}

/// The wave a claim took, or a panic naming what it did instead.
fn wave(claimed: Claim) -> Wave {
    match claimed {
        Claim::Wave(wave) => wave,
        other => panic!("expected a wave, got {other:?}"),
    }
}

/// Starts and finishes `message` as accepted, as the sender does.
async fn send(
    test: &TestDb,
    workspace: WorkspaceId,
    sender: &TestSender,
    message: Id<Message>,
    generation: i64,
) {
    let started = start::start(
        &test.worker,
        &Start {
            workspace,
            connection: sender.connection,
            message,
            generation,
            owner: OWNER,
            budget: Duration::from_secs(300),
            retry_window: Duration::from_secs(86_400),
        },
    )
    .await
    .expect("the Start runs");
    let Started::Submit(begun) = started else {
        panic!("the Start did not submit: {started:?}");
    };
    finish::finish(
        &test.worker,
        workspace,
        sender.connection,
        OWNER,
        &[Report {
            message,
            generation,
            reported: Reported::Answered(Box::new(Answered {
                answer: Answer::Accepted,
                source: Source::Smtp,
                started: Some(begun.started),
                diagnostic: "250 2.0.0 OK".to_owned(),
                provider_message_id: None,
                recipients: Vec::new(),
                refused: Vec::new(),
            })),
        }],
    )
    .await
    .expect("the finish runs");
}

/// The connection's pacing clock and budget wait.
async fn clocks(test: &TestDb, connection: Id<Connection>) -> (Timestamp, Option<Timestamp>) {
    sqlx::query_as("SELECT next_send_at, next_claim_at FROM connections WHERE id = $1")
        .bind(connection.uuid())
        .fetch_one(test.system.pool())
        .await
        .expect("the connection")
}

/// The database's clock.
async fn now(test: &TestDb) -> Timestamp {
    sqlx::query_scalar("SELECT now()")
        .fetch_one(test.system.pool())
        .await
        .expect("now")
}

/// The ISO weekday (1 Monday to 7 Sunday) after today's, in UTC: a window open only then is
/// closed now and opens tomorrow.
fn tomorrow() -> u8 {
    let today = jiff::Timestamp::now()
        .to_zoned(TimeZone::UTC)
        .weekday()
        .to_monday_one_offset();
    u8::try_from(today % 7 + 1).expect("a weekday")
}

/// Tomorrow at 09:00 UTC.
fn tomorrow_at_nine() -> jiff::Timestamp {
    let date = jiff::Timestamp::now()
        .to_zoned(TimeZone::UTC)
        .date()
        .tomorrow()
        .expect("tomorrow");
    date.at(9, 0, 0, 0)
        .to_zoned(TimeZone::UTC)
        .expect("a UTC instant")
        .timestamp()
}

/// The pacing clock's arithmetic is one formula in two places, the Start's SQL statement and
/// `domain::schedule`: over every interval from 5 to 60 minutes and every Start from 0 to 299
/// seconds after its scheduled instant (16,800 cases) they move the clock to the same instant,
/// and `next_phase_at` agrees on instants around a mark, a microsecond apart, at many phases.
#[tokio::test]
async fn the_clock_arithmetic_agrees_with_the_database() {
    let test = TestDb::new().await;
    let rows: Vec<(i32, i64, Timestamp)> = sqlx::query_as(
        "SELECT m, d::bigint,
                next_phase_at(greatest(s + make_interval(mins => m),
                                       s + make_interval(secs => d) + make_interval(mins => m) - interval '30 seconds'), 41)
           FROM generate_series(5, 60) m, generate_series(0, 299) d,
                (SELECT TIMESTAMPTZ '2026-10-01 10:00:41+00' AS s) x",
    )
    .fetch_all(test.worker.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 16_800);
    let scheduled: jiff::Timestamp = "2026-10-01T10:00:41Z".parse().unwrap();
    for (minutes, delay, wanted) in rows {
        let started = scheduled
            .checked_add(SignedDuration::from_secs(delay))
            .unwrap();
        assert_eq!(
            schedule::after_cold_start(scheduled, started, minutes, 41),
            wanted.0,
            "{minutes} minutes, started {delay} s late"
        );
    }
    let instants: Vec<(Timestamp, i32, Timestamp)> = sqlx::query_as(
        "SELECT at, p, next_phase_at(at, p)
           FROM generate_series(0, 299, 13) p,
                LATERAL (SELECT t + (p % 3) * interval '1 microsecond' AS at
                           FROM generate_series(TIMESTAMPTZ '2026-10-01 09:59:58+00', TIMESTAMPTZ '2026-10-01 10:05:02+00',
                                                interval '997 milliseconds') t) x",
    )
    .fetch_all(test.worker.pool())
    .await
    .unwrap();
    assert!(instants.len() > 1_000);
    for (at, phase, wanted) in instants {
        assert_eq!(
            schedule::next_phase_at(at.0, phase),
            wanted.0,
            "{at} at phase {phase}"
        );
    }
}

/// Two replicas claiming one mailbox never put two of its messages in progress: while one holds
/// the mailbox's row the other skips it; the claim takes its oldest due mail created through the
/// API first, though its cold rows are older; nothing more is claimed while that message is in
/// progress; after it is sent, the next API row; with none left and the clock due, one cold row.
#[tokio::test]
async fn one_mailbox_has_one_message_in_progress_whatever_the_replicas() {
    let test = TestDb::new().await;
    let ws = test.workspace("replicas").await.id;
    let sender = test
        .sender(ws, &SenderSpec::mailbox("ada@example.test"))
        .await;
    let campaign = test.campaign(ws, &sender, None).await;
    let cold = test
        .campaign_message(ws, campaign, &sender, "c1@example.test", -120)
        .await;
    test.campaign_message(ws, campaign, &sender, "c2@example.test", -120)
        .await;
    let api1 = test
        .direct_message(ws, &sender, &["a1@example.test"], -60)
        .await;
    let api2 = test
        .direct_message(ws, &sender, &["a2@example.test"], -60)
        .await;
    let replica = test.worker_pool(2).await;

    let mut held = test.system.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM connections WHERE id = $1 FOR UPDATE")
        .bind(sender.connection.uuid())
        .execute(&mut *held)
        .await
        .unwrap();
    assert_eq!(
        take_on(&replica, ws, sender.connection, false).await,
        Claim::Nothing,
        "the second replica skips the row the first holds"
    );
    held.rollback().await.unwrap();

    let first = wave(take_on(&test.worker, ws, sender.connection, false).await);
    assert_eq!(first.messages.len(), 1);
    assert_eq!(
        first.messages[0].message, api1,
        "the oldest due API row first"
    );
    assert!(!first.messages[0].paced);
    assert_eq!(
        take_on(&replica, ws, sender.connection, false).await,
        Claim::Nothing,
        "nothing more while a message is in progress"
    );
    send(&test, ws, &sender, api1, first.messages[0].generation).await;

    let second = wave(take_on(&replica, ws, sender.connection, false).await);
    assert_eq!(second.messages[0].message, api2);
    send(&test, ws, &sender, api2, second.messages[0].generation).await;

    let third = wave(take_on(&test.worker, ws, sender.connection, false).await);
    assert_eq!(third.messages.len(), 1);
    assert_eq!(
        third.messages[0].message, cold,
        "no API row left: one cold row, the clock due"
    );
    assert!(third.messages[0].paced);
    let busy: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM delivery_queue WHERE connection_id = $1 AND state <> 'queued'",
    )
    .bind(sender.connection.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(busy, 1);
}

/// A spent daily budget makes the mailbox wait, its clock untouched: used units alone wait for
/// the next UTC midnight at its phase; with a reservation still outstanding, one slot; and both
/// scans skip it while it waits.
#[tokio::test]
async fn a_spent_budget_waits_by_what_spent_it() {
    let test = TestDb::new().await;
    let ws = test.workspace("spent").await.id;
    let sender = test
        .sender(ws, &SenderSpec::mailbox("ada@example.test"))
        .await;
    test.direct_message(ws, &sender, &["a@example.test"], -60)
        .await;
    sqlx::query(
        "INSERT INTO connection_usage (workspace_id, connection_id, day, used)
         VALUES ($1, $2, (now() AT TIME ZONE 'UTC')::date, 100)",
    )
    .bind(ws.uuid())
    .bind(sender.connection.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();
    let (clock, _) = clocks(&test, sender.connection).await;
    let midnight: Timestamp = sqlx::query_scalar(
        "SELECT next_phase_at(((now() AT TIME ZONE 'UTC')::date + 1)::timestamp AT TIME ZONE 'UTC', 41)",
    )
    .fetch_one(test.worker.pool())
    .await
    .unwrap();
    assert_eq!(
        take_on(&test.worker, ws, sender.connection, false).await,
        Claim::Spent { until: midnight }
    );
    assert_eq!(
        clocks(&test, sender.connection).await,
        (clock, Some(midnight)),
        "the clock is untouched"
    );
    let mut cursors = Cursors::default();
    let found = page(&test.worker, ws, &mut cursors).await.unwrap();
    assert!(
        found.candidates.is_empty(),
        "both scans skip a waiting connection"
    );

    sqlx::query("UPDATE connection_usage SET used = 99, reserved = 1 WHERE connection_id = $1")
        .bind(sender.connection.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE connections SET next_claim_at = NULL WHERE id = $1")
        .bind(sender.connection.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    let Claim::Spent { until } = take_on(&test.worker, ws, sender.connection, false).await else {
        panic!("a budget spent with a reservation outstanding waits");
    };
    let now = now(&test).await;
    assert!(
        until > now && until <= now.plus(Duration::from_secs(300)),
        "one slot: {until}"
    );
}

/// Two connections of one quota scope race its last daily unit: the claim whose check-and-reserve
/// comes second (here behind another transaction holding the scope's bucket with the last unit
/// reserved) finds no room under the bucket's lock and rolls back whole: no attempt, no claimed
/// row, and the scope holds exactly its limit.
#[tokio::test]
async fn a_quota_scope_admits_exactly_its_last_unit() {
    let test = TestDb::new().await;
    let ws = test.workspace("scope").await.id;
    let scope = test.quota_scope(ws, "sendgrid", Some(10), None).await;
    let _one = test
        .sender(
            ws,
            &SenderSpec {
                scope: Some(scope),
                ..SenderSpec::relay("one@example.test")
            },
        )
        .await;
    let two = test
        .sender(
            ws,
            &SenderSpec {
                scope: Some(scope),
                ..SenderSpec::relay("two@example.test")
            },
        )
        .await;
    test.direct_message(ws, &two, &["r@example.test"], -60)
        .await;
    sqlx::query(
        "INSERT INTO quota_scope_usage (workspace_id, scope_id, day, messages_used)
         VALUES ($1, $2, (now() AT TIME ZONE 'UTC')::date, 9), ($1, $2, (now() AT TIME ZONE 'UTC')::date - 1, 0)",
    )
    .bind(ws.uuid())
    .bind(scope)
    .execute(test.system.pool())
    .await
    .unwrap();
    // The first connection's claim holds the bucket with the last unit reserved.
    let mut first = test.system.begin().await.unwrap();
    sqlx::query(
        "UPDATE quota_scope_usage SET messages_reserved = messages_reserved + 1
          WHERE scope_id = $1 AND day = (now() AT TIME ZONE 'UTC')::date",
    )
    .bind(scope)
    .execute(&mut *first)
    .await
    .unwrap();
    let replica = test.worker_pool(2).await;
    let connection = two.connection;
    let racing = tokio::spawn(async move {
        let started = Instant::now();
        let claimed = take_on(&replica, ws, connection, false).await;
        (claimed, started.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    first.commit().await.unwrap();
    let (claimed, waited) = racing.await.unwrap();
    assert_eq!(claimed, Claim::Nothing, "the loser rolls back");
    assert!(
        waited >= Duration::from_millis(400),
        "it waited for the bucket's lock: {waited:?}"
    );
    let reserved: i32 = sqlx::query_scalar(
        "SELECT messages_used + messages_reserved FROM quota_scope_usage
          WHERE scope_id = $1 AND day = (now() AT TIME ZONE 'UTC')::date",
    )
    .bind(scope)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(reserved, 10);
    let attempts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM attempts WHERE connection_id = $1")
            .bind(two.connection.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(attempts, 0);
    let claimed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM delivery_queue WHERE connection_id = $1 AND state <> 'queued'",
    )
    .bind(two.connection.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(claimed, 0);
}

/// A half-open quota scope admits one probe at a time across its connections and replicas: a
/// claim that finds the scope's row held admits nothing; the first claim admits exactly one
/// message and records it, with its lease generation, as the scope's probe; another connection
/// of the scope admits nothing while that probe is live; once its lease expires the slot frees
/// and a new probe is admitted on a real row.
#[tokio::test]
async fn a_half_open_scope_admits_one_probe_at_a_time() {
    let test = TestDb::new().await;
    let ws = test.workspace("probe").await.id;
    let scope = test.quota_scope(ws, "sendgrid", None, None).await;
    sqlx::query(
        "UPDATE quota_scopes SET consecutive_failures = 3, paused_until = now() - interval '1 minute',
                breaker_opened_at = now() - interval '5 minutes' WHERE id = $1",
    )
    .bind(scope)
    .execute(test.system.pool())
    .await
    .unwrap();
    let a = test
        .sender(
            ws,
            &SenderSpec {
                scope: Some(scope),
                ..SenderSpec::relay("a@example.test")
            },
        )
        .await;
    let b = test
        .sender(
            ws,
            &SenderSpec {
                scope: Some(scope),
                ..SenderSpec::relay("b@example.test")
            },
        )
        .await;
    for to in ["r1@example.test", "r2@example.test"] {
        test.direct_message(ws, &a, &[to], -60).await;
        test.direct_message(ws, &b, &[to], -60).await;
    }
    let probe = || async {
        sqlx::query_as::<_, (Option<Uuid>, Option<i64>)>(
            "SELECT probe_message_id, probe_generation FROM quota_scopes WHERE id = $1",
        )
        .bind(scope)
        .fetch_one(test.system.pool())
        .await
        .unwrap()
    };

    let mut held = test.system.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM quota_scopes WHERE id = $1 FOR UPDATE")
        .bind(scope)
        .execute(&mut *held)
        .await
        .unwrap();
    assert_eq!(
        take_on(&test.worker, ws, b.connection, true).await,
        Claim::Nothing
    );
    held.rollback().await.unwrap();

    let admitted = wave(take_on(&test.worker, ws, a.connection, true).await);
    assert!(admitted.probe);
    assert_eq!(admitted.messages.len(), 1, "a probe is one message");
    let first = admitted.messages[0];
    assert_eq!(
        probe().await,
        (Some(first.message.uuid()), Some(first.generation))
    );
    assert_eq!(
        take_on(&test.worker, ws, b.connection, true).await,
        Claim::Nothing,
        "the probe is live"
    );

    sqlx::query("UPDATE delivery_queue SET lease_expires_at = now() - interval '1 second' WHERE message_id = $1")
        .bind(first.message.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    let next = wave(take_on(&test.worker, ws, b.connection, true).await);
    assert!(next.probe);
    assert_eq!(next.messages.len(), 1);
    assert_eq!(
        probe().await,
        (
            Some(next.messages[0].message.uuid()),
            Some(next.messages[0].generation)
        )
    );
}

/// The scans page after each replica's cursor and end their sweeps: eight relays sort first
/// (their clocks never move), the due mailbox follows on the next page, and a short page ends
/// the sweep; a connection created after the sweep's cutoff waits for the next sweep though its
/// key falls after the cursor, as does a clock that came due again after the cutoff.
#[tokio::test]
async fn sweeps_page_after_their_cursor_and_keep_newcomers_out() {
    let test = TestDb::new().await;
    let ws = test.workspace("pages").await.id;
    let mut relays = Vec::new();
    for n in 0..8 {
        let relay = test
            .sender(ws, &SenderSpec::relay(&format!("relay{n}@example.test")))
            .await;
        test.direct_message(ws, &relay, &["r@example.test"], -60)
            .await;
        relays.push(relay.connection);
    }
    let mailbox = test
        .sender(ws, &SenderSpec::mailbox("ada@example.test"))
        .await;
    let mut cursors = Cursors::default();

    let first = page(&test.worker, ws, &mut cursors).await.unwrap();
    let clock: Vec<Id<Connection>> = first
        .candidates
        .iter()
        .filter(|c| c.scan == Scan::Clock)
        .map(|c| c.connection)
        .collect();
    assert_eq!(clock, relays, "the relays first, in id order");
    assert!(!first.clock_ended);
    for reached in first.candidates.iter().filter(|c| c.scan == Scan::Clock) {
        cursors.reached(reached);
    }
    let newcomer = test
        .sender(ws, &SenderSpec::relay("new@example.test"))
        .await;
    test.direct_message(ws, &newcomer, &["r@example.test"], -3_600)
        .await;

    let second = page(&test.worker, ws, &mut cursors).await.unwrap();
    let clock: Vec<Id<Connection>> = second
        .candidates
        .iter()
        .filter(|c| c.scan == Scan::Clock)
        .map(|c| c.connection)
        .collect();
    assert_eq!(
        clock,
        [mailbox.connection],
        "the mailbox behind the relays, not the newcomer"
    );
    assert!(second.clock_ended, "a short page ends the sweep");
    assert!(
        second
            .candidates
            .iter()
            .all(|c| c.connection != newcomer.connection),
        "the API scan keeps the newcomer out too, though its mail is backdated"
    );

    // A clock that came due after this sweep's cutoff stays out of it.
    if let Some(sweep) = cursors.clock.get_mut(&ws) {
        sweep.after_at = Timestamp(jiff::Timestamp::UNIX_EPOCH);
        sweep.after_id = Uuid::nil();
        sweep.sweep_at = now(&test).await.minus(Duration::from_secs(60));
    }
    sqlx::query(
        "UPDATE connections SET next_send_at = now() - interval '59 seconds' WHERE id = $1",
    )
    .bind(mailbox.connection.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();
    let pushed = page(&test.worker, ws, &mut cursors).await.unwrap();
    assert!(
        pushed
            .candidates
            .iter()
            .all(|c| c.scan != Scan::Clock || c.connection != mailbox.connection),
        "the cutoff keeps a clock that moved past it out of the sweep"
    );

    cursors.end(ws, Scan::Clock);
    cursors.end(ws, Scan::Api);
    let mut reached = Vec::new();
    for _ in 0..3 {
        let next = page(&test.worker, ws, &mut cursors).await.unwrap();
        for candidate in &next.candidates {
            cursors.reached(candidate);
            reached.push(candidate.connection);
        }
    }
    assert!(
        reached.contains(&newcomer.connection),
        "the next sweep reaches the newcomer, after the older relays"
    );
    assert_eq!(
        turn(&test.worker).await.unwrap(),
        Some(ws),
        "the workspace has a turn"
    );
}

/// An idle paced sender is looked at once a slot: a claim that finds nothing moves its clock
/// forward to its next phase instant; one whose due cold mail is outside its campaign's window
/// moves it to the first phase instant of the window's opening, and never back.
#[tokio::test]
async fn an_idle_clock_moves_forward_to_its_next_chance() {
    let test = TestDb::new().await;
    let ws = test.workspace("idle").await.id;
    let idle = test
        .sender(ws, &SenderSpec::mailbox("idle@example.test"))
        .await;
    assert_eq!(
        take_on(&test.worker, ws, idle.connection, false).await,
        Claim::Nothing
    );
    let (clock, _) = clocks(&test, idle.connection).await;
    let now = now(&test).await;
    assert!(
        clock > now && clock <= now.plus(Duration::from_secs(300)),
        "within the next slot: {clock}"
    );
    assert_eq!(
        clock.0,
        schedule::next_phase_at(clock.0, 41),
        "on its phase"
    );

    let closed = test
        .sender(ws, &SenderSpec::mailbox("closed@example.test"))
        .await;
    let window = json!({ "days": [tomorrow()], "start": "09:00", "end": "10:00" });
    let campaign = test.campaign(ws, &closed, Some(window)).await;
    test.campaign_message(ws, campaign, &closed, "p@example.test", -60)
        .await;
    assert_eq!(
        take_on(&test.worker, ws, closed.connection, false).await,
        Claim::Nothing
    );
    let (clock, _) = clocks(&test, closed.connection).await;
    assert_eq!(
        clock.0,
        schedule::next_phase_at(tomorrow_at_nine(), 41),
        "the window's opening, at the phase"
    );
}

/// A rate-paced relay never takes campaign mail outside its windows: the claim takes the mail
/// created through the API and pushes the closed campaign's row to its window's opening, so the
/// row is not claimed and returned at every sweep.
#[tokio::test]
async fn a_relay_pushes_closed_window_mail_to_its_opening() {
    let test = TestDb::new().await;
    let ws = test.workspace("windows").await.id;
    let relay = test
        .sender(ws, &SenderSpec::relay("relay@example.test"))
        .await;
    let window = json!({ "days": [tomorrow()], "start": "09:00", "end": "10:00" });
    let campaign = test.campaign(ws, &relay, Some(window)).await;
    let cold = test
        .campaign_message(ws, campaign, &relay, "p@example.test", -120)
        .await;
    let direct = test
        .direct_message(ws, &relay, &["d@example.test"], -60)
        .await;
    let taken = wave(take_on(&test.worker, ws, relay.connection, false).await);
    let ids: Vec<Id<Message>> = taken.messages.iter().map(|m| m.message).collect();
    assert_eq!(ids, [direct]);
    let run_at: Timestamp =
        sqlx::query_scalar("SELECT run_at FROM delivery_queue WHERE message_id = $1")
            .bind(cold.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(run_at.0, tomorrow_at_nine());
}

/// A measurement, not a check of behavior: 6,000 mailboxes across 50 workspaces, each with one
/// message due, claimed one transaction per mailbox through the turns and pages, as a slot's
/// cold mail would be (spread over the slot in production, back to back here). Every mailbox is
/// claimed exactly once.
#[tokio::test]
#[ignore = "a measurement of 6,000 claims (tens of seconds on a laptop); run with --ignored"]
async fn six_thousand_mailboxes_are_claimed_once_each() {
    let test = TestDb::new().await;
    sqlx::query(
        "INSERT INTO workspaces (id, slug, name)
         SELECT ('00000000-0000-7000-8000-' || lpad(to_hex(g), 12, '0'))::uuid, 'ws' || g, 'ws' || g FROM generate_series(1, 50) g",
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO connections (workspace_id, id, provider, transport, account_email, smtp, status, daily_limit,
                                  send_interval_minutes, send_phase_seconds, next_send_at)
         SELECT ('00000000-0000-7000-8000-' || lpad(to_hex(1 + g % 50), 12, '0'))::uuid, uuidv7(), 'smtp', 'smtp', 'm' || g || '@x.test',
                '{\"host\": \"smtp.x.test\", \"port\": 587, \"security\": \"starttls\", \"username\": \"m\"}', 'active', 2000, 5, g % 300,
                now() - interval '5 minutes'
           FROM generate_series(1, 6000) g",
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query(
        "WITH i AS (INSERT INTO sender_identities (workspace_id, connection_id, email)
                    SELECT workspace_id, id, account_email FROM connections WHERE account_email LIKE 'm%@x.test'
                    RETURNING workspace_id, id, connection_id, email),
              m AS (INSERT INTO messages (workspace_id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject,
                                          html, render_version, rendered_at, internet_message_id, send_at)
                    SELECT workspace_id, 'direct', id, connection_id, email, ARRAY['r@example.test'], 's', '<p>x</p>', '1', now(),
                           '<' || gen_random_uuid() || '@x.test>', now() - interval '1 minute' FROM i
                    RETURNING workspace_id, id, connection_id, send_at)
         INSERT INTO delivery_queue (workspace_id, message_id, connection_id, run_at, paced)
         SELECT workspace_id, id, connection_id, send_at, false FROM m",
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    let started = Instant::now();
    let mut cursors = Cursors::default();
    let mut claimed = 0;
    while let Some(workspace) = turn(&test.worker).await.unwrap() {
        let found = page(&test.worker, workspace, &mut cursors).await.unwrap();
        for candidate in &found.candidates {
            cursors.reached(candidate);
            if let Claim::Wave(wave) = claim(&test.worker, OWNER, candidate, 16, &mut |_| true)
                .await
                .unwrap()
            {
                claimed += wave.messages.len();
            }
        }
        if found.clock_ended {
            cursors.end(workspace, Scan::Clock);
        }
        if found.api_ended {
            cursors.end(workspace, Scan::Api);
        }
        assert!(
            started.elapsed() < Duration::from_secs(600),
            "the claims take too long"
        );
    }
    assert_eq!(
        claimed,
        6_000,
        "every mailbox once, in {:?}",
        started.elapsed()
    );
}
