//! Store tests of the health notices: which changes enqueue the window's job and how a window's
//! moves coalesce into it, and the job's emails: one per person told, each listing what that
//! person is told about, as the window left it.

use uuid::Uuid;

use super::{HealthEmail, apply, changed};
use crate::domain::ids::WorkspaceId;
use crate::domain::senders::{HealthEvent, Status};
use crate::jobs::runner::Harness;
use crate::jobs::{Queue, Registry, SYSTEM_WORKSPACE};
use crate::testing::{self, SenderSpec, TestDb};

/// The `connection.health_email` jobs: each one's workspace, whether its key is a quarter hour
/// whose window holds the present, and whether it runs 16.5 minutes after that quarter hour.
async fn health_jobs(test: &TestDb) -> Vec<(Uuid, bool, bool)> {
    sqlx::query_as(
        "SELECT workspace_id,
                extract(epoch FROM unique_key::timestamptz)::bigint % 900 = 0
                  AND unique_key::timestamptz <= now()
                  AND now() < unique_key::timestamptz + interval '16 minutes',
                run_at = unique_key::timestamptz + interval '16 minutes 30 seconds'
           FROM jobs WHERE kind = 'connection.health_email' ORDER BY workspace_id",
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap()
}

/// A member of `workspace` holding `email`, with `role` and membership `status`; answers the
/// user's id.
async fn member(
    test: &TestDb,
    workspace: WorkspaceId,
    email: &str,
    role: &str,
    status: &str,
) -> Uuid {
    sqlx::query_scalar(
        "WITH u AS (INSERT INTO users (email, email_verified_at) VALUES ($1, now()) RETURNING id)
         INSERT INTO memberships (workspace_id, user_id, role, status)
         SELECT $2, id, $3, $4 FROM u RETURNING user_id",
    )
    .bind(email)
    .bind(workspace.uuid())
    .bind(role)
    .bind(status)
    .fetch_one(test.system.pool())
    .await
    .unwrap()
}

/// A runner of the health notices on the test's database, with the deployment's keys.
fn runner(test: &TestDb) -> Harness {
    let mut registry = Registry::default();
    registry.register::<HealthEmail>().unwrap();
    let mut env = http::Extensions::new();
    env.insert(testing::keys());
    Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        env,
        "health-test",
    )
}

/// Moves of the table to a status people are told about enqueue the job of their window, which
/// every move of the window joins, due once the window has closed and settled; a move to a step
/// of setting up, and a pause, enqueue nothing: how a storm of moves becomes one notice.
#[tokio::test]
async fn told_moves_of_a_window_enqueue_its_one_job() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let globex = test.workspace("globex").await;
    let a = test
        .sender(acme.id, &SenderSpec::mailbox("a@acme.example"))
        .await
        .connection;
    let b = test
        .sender(acme.id, &SenderSpec::mailbox("b@acme.example"))
        .await
        .connection;
    let c = test
        .sender(globex.id, &SenderSpec::mailbox("c@globex.example"))
        .await
        .connection;
    let mut tx = test.worker.begin_in(acme.id).await.unwrap();
    apply(
        &mut tx,
        acme.id,
        a,
        Status::Active,
        false,
        HealthEvent::CredentialLost,
        None,
    )
    .await
    .unwrap();
    apply(
        &mut tx,
        acme.id,
        b,
        Status::Active,
        false,
        HealthEvent::AccountBlocked,
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut tx = test.worker.begin_in(globex.id).await.unwrap();
    apply(
        &mut tx,
        globex.id,
        c,
        Status::Active,
        false,
        HealthEvent::VerifyRequested,
        None,
    )
    .await
    .unwrap();
    changed(&mut tx, globex.id, c, Status::Verifying, None, true)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(health_jobs(&test).await, [(acme.id.uuid(), true, true)]);
}

/// The window's job sends one email per person told: the workspace's active owners and admins
/// about every connection that stopped or started working, a member about the ones they created,
/// nobody else, and never about an archive. Each lists a connection as the window left it, so a
/// connection that broke and recovered within the window is shown working.
#[tokio::test]
async fn a_windows_moves_are_one_email_per_person() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let acme = test.workspace("acme").await;
    member(&test, acme.id, "admin@acme.example", "admin", "active").await;
    let max = member(&test, acme.id, "max@acme.example", "member", "active").await;
    member(&test, acme.id, "viewer@acme.example", "viewer", "active").await;
    member(&test, acme.id, "gone@acme.example", "admin", "suspended").await;
    let a = test
        .sender(acme.id, &SenderSpec::mailbox("a@acme.example"))
        .await
        .connection;
    let b = test
        .sender(acme.id, &SenderSpec::mailbox("b@acme.example"))
        .await
        .connection;
    let c = test
        .sender(acme.id, &SenderSpec::mailbox("c@acme.example"))
        .await
        .connection;
    sqlx::query("UPDATE connections SET created_by = $1 WHERE id = $2")
        .bind(max)
        .bind(b.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    let mut tx = test.worker.begin_in(acme.id).await.unwrap();
    let moves = [
        (a, Status::Active, HealthEvent::CredentialLost),
        (b, Status::Active, HealthEvent::AccountBlocked),
        (b, Status::Disabled, HealthEvent::VerifyRequested),
        (b, Status::Verifying, HealthEvent::CheckPassed),
        (c, Status::Active, HealthEvent::Archived),
    ];
    for (connection, current, event) in moves {
        let detail = (connection == a).then_some("The server refused the login.");
        apply(&mut tx, acme.id, connection, current, false, event, detail)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    // The window has not closed yet: bring its job forward.
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'connection.health_email'")
        .execute(test.system.pool())
        .await
        .unwrap();
    let ran = runner(&test).run_once(Queue::Transactional, 1).await;
    assert_eq!(
        ran.into_iter()
            .map(|(_, outcome)| outcome)
            .collect::<Vec<_>>(),
        ["done"]
    );
    let mails: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT to_addresses[1], subject, text_body FROM messages
          WHERE workspace_id = $1 AND kind = 'transactional' ORDER BY to_addresses[1]",
    )
    .bind(SYSTEM_WORKSPACE.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    let to: Vec<&str> = mails.iter().map(|(to, _, _)| to.as_str()).collect();
    assert_eq!(
        to,
        [
            "admin@acme.example",
            "max@acme.example",
            "owner@acme.example"
        ]
    );
    let lost =
        "- a@acme.example (smtp): needs to be connected again. The server refused the login.\n";
    let recovered = "- b@acme.example (smtp): working\n";
    for (to, subject, text) in &mails {
        let everything = to != "max@acme.example";
        let expected = if everything {
            "2 sender connections changed in acme"
        } else {
            "A sender connection changed in acme"
        };
        assert_eq!(subject, expected, "{to}");
        assert_eq!(text.contains(lost), everything, "{to}: {text}");
        assert!(text.contains(recovered), "{to}: {text}");
        assert!(!text.contains("c@acme.example"), "{to}: {text}");
    }
}
