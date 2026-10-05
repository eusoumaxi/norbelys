//! Store tests and a measurement of the claim's due queries over many mailboxes: the turn, which
//! picks the workspace whose turn it is among those with work due, and a workspace's page of due
//! connections.
//!
//! With thousands of mailboxes a sender's work per loop is first "which connections are due", so
//! its cost is the cost of these two reads, run as the scheduler role across every workspace
//! before any row is locked. A paused connection must be invisible to both: their predicate skips
//! it (the predicate the partial indexes of due connections carry), never a later check, or every
//! sweep would lock and examine paused mailboxes for nothing.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use uuid::Uuid;

use super::claim::{Cursors, Page, page, turn};
use crate::testing::{SenderSpec, TestDb};

/// The connections of a page, whichever scan found them.
fn connections(page: &Page) -> HashSet<Uuid> {
    page.candidates
        .iter()
        .map(|candidate| candidate.connection.uuid())
        .collect()
}

/// The due queries find exactly the connections that can send now, across workspaces: a mailbox
/// whose clock and mail are due is found; one whose clock and mail are an hour away is not; a
/// paused one is not, though its mail is due, and a workspace whose only due mail waits on a paused
/// mailbox never gets the turn; resumed, the mailbox is due again at once.
#[tokio::test]
async fn the_due_queries_skip_paused_and_future_connections() {
    let test = TestDb::new().await;
    let open = test.workspace("open").await;
    let held = test.workspace("held").await;
    let due = test
        .sender(open.id, &SenderSpec::mailbox("due@open.example"))
        .await;
    let later = test
        .sender(
            open.id,
            &SenderSpec {
                next_send_in: Some(3_600),
                ..SenderSpec::mailbox("later@open.example")
            },
        )
        .await;
    let paused = test
        .sender(open.id, &SenderSpec::mailbox("paused@open.example"))
        .await;
    let held_paused = test
        .sender(held.id, &SenderSpec::mailbox("paused@held.example"))
        .await;
    test.direct_message(open.id, &due, &["ada@example.com"], -60)
        .await;
    test.direct_message(open.id, &later, &["bob@example.com"], 3_600)
        .await;
    test.direct_message(open.id, &paused, &["cy@example.com"], -60)
        .await;
    test.direct_message(held.id, &held_paused, &["dee@example.com"], -60)
        .await;
    for sender in [&paused, &held_paused] {
        sqlx::query("UPDATE connections SET paused = true WHERE id = $1")
            .bind(sender.connection.uuid())
            .execute(test.system.pool())
            .await
            .unwrap();
    }

    // The turn moves to the back each time it is taken: two turns in a row go to `open` only
    // because `held` has nothing due.
    assert_eq!(turn(&test.worker).await.unwrap(), Some(open.id));
    assert_eq!(turn(&test.worker).await.unwrap(), Some(open.id));
    let found = page(&test.worker, open.id, &mut Cursors::default())
        .await
        .unwrap();
    assert_eq!(connections(&found), HashSet::from([due.connection.uuid()]));
    let found = page(&test.worker, held.id, &mut Cursors::default())
        .await
        .unwrap();
    assert!(found.candidates.is_empty(), "{:?}", found.candidates);

    sqlx::query("UPDATE connections SET paused = false WHERE id = $1")
        .bind(paused.connection.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    let found = page(&test.worker, open.id, &mut Cursors::default())
        .await
        .unwrap();
    assert_eq!(
        connections(&found),
        HashSet::from([due.connection.uuid(), paused.connection.uuid()])
    );
}

/// The slowest one due query may take in the measurement below: generous, because the machine
/// running it is shared; what it guards is the order of magnitude (a few milliseconds, where
/// reading every mailbox's queue would take a large part of a second).
const BOUND: Duration = Duration::from_millis(250);

/// A measurement with its correctness kept: 6,000 active mailboxes and 600 paused ones across 50
/// workspaces, ten queued messages each (the first created through the API, the rest cold mail).
/// While everything is due, fifty turns visit each workspace once, and each page holds due
/// mailboxes only, never a paused one; once every clock and message is an hour away, the turn
/// finds nothing. Each turn and page stays under [`BOUND`], an idle turn included, which reads
/// every workspace to find that none has work.
#[tokio::test]
#[ignore = "a measurement over 6,600 mailboxes and 66,000 queued messages (about a minute to build on a laptop); run with --ignored"]
async fn the_due_queries_stay_fast_over_six_thousand_mailboxes() {
    let test = TestDb::new().await;
    sqlx::raw_sql(
        r#"INSERT INTO workspaces (id, slug, name)
           SELECT ('00000000-0000-7000-8000-' || lpad(to_hex(g), 12, '0'))::uuid, 'ws' || g, 'ws' || g
             FROM generate_series(1, 50) g;
           INSERT INTO connections (workspace_id, id, provider, transport, account_email, smtp, status, paused, daily_limit,
                                    send_interval_minutes, send_phase_seconds, next_send_at)
           SELECT ('00000000-0000-7000-8000-' || lpad(to_hex(1 + g % 50), 12, '0'))::uuid, uuidv7(), 'smtp', 'smtp',
                  'm' || g || '@x.test', '{"host": "smtp.x.test", "port": 587, "security": "starttls", "username": "m"}',
                  'active', g > 6000, 2000, 5, g % 300, now() - make_interval(secs => g % 90)
             FROM generate_series(1, 6600) g;
           INSERT INTO sender_identities (workspace_id, connection_id, email)
           SELECT workspace_id, id, account_email FROM connections WHERE account_email LIKE 'm%@x.test';
           WITH m AS (
             INSERT INTO messages (workspace_id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject,
                                   html, render_version, rendered_at, internet_message_id, send_at)
             SELECT i.workspace_id, 'direct', i.id, i.connection_id, i.email, ARRAY['r' || n || '@example.test'], 's' || n,
                    '<p>x</p>', '1', now(), '<' || gen_random_uuid() || '@x.test>', now() - interval '1 minute'
               FROM sender_identities i CROSS JOIN generate_series(1, 10) n
              WHERE i.email LIKE 'm%@x.test'
             RETURNING workspace_id, id, connection_id, send_at, subject)
           INSERT INTO delivery_queue (workspace_id, message_id, connection_id, run_at, paced)
           SELECT workspace_id, id, connection_id, send_at, subject <> 's1' FROM m;"#,
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    let paused: HashSet<Uuid> =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM connections WHERE paused")
            .fetch_all(test.system.pool())
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert_eq!(paused.len(), 600);

    let mut slowest_turn = Duration::ZERO;
    let mut slowest_page = Duration::ZERO;
    let mut visited = HashSet::new();
    for _ in 0..50 {
        let started = Instant::now();
        let workspace = turn(&test.worker)
            .await
            .unwrap()
            .expect("every workspace has due work");
        slowest_turn = slowest_turn.max(started.elapsed());
        let started = Instant::now();
        let found = page(&test.worker, workspace, &mut Cursors::default())
            .await
            .unwrap();
        slowest_page = slowest_page.max(started.elapsed());
        let found = connections(&found);
        assert!(!found.is_empty(), "a turn without a due mailbox");
        assert!(found.is_disjoint(&paused), "a paused mailbox was found due");
        visited.insert(workspace);
    }
    assert_eq!(visited.len(), 50, "every workspace takes its turn once");

    sqlx::raw_sql(
        "UPDATE connections SET next_send_at = now() + interval '1 hour';
         UPDATE delivery_queue SET run_at = now() + interval '1 hour';",
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    let mut slowest_idle = Duration::ZERO;
    for _ in 0..5 {
        let started = Instant::now();
        assert_eq!(turn(&test.worker).await.unwrap(), None);
        slowest_idle = slowest_idle.max(started.elapsed());
    }
    assert!(
        slowest_turn < BOUND && slowest_page < BOUND && slowest_idle < BOUND,
        "slowest turn {slowest_turn:?}, page {slowest_page:?}, idle turn {slowest_idle:?} (bound {BOUND:?})"
    );
}
