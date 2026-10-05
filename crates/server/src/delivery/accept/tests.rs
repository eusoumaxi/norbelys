//! Store tests of the creation contract: one message per step under concurrent creators, stale
//! creators that write nothing, threads across steps, the Message-ID, transactional mail of the
//! `system` workspace, and a step message prepared by the sender's own login; and one policy
//! test, every transactional email rendered in the frame of the platform's own mail.

use std::time::Duration;

use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::db::Database;
use crate::rendering::{self, Settings};
use crate::testing::{TestDb, TestWorkspace, keys};

/// What [`campaign`] made.
struct Fixture {
    workspace: WorkspaceId,
    campaign: Id<Campaign>,
    steps: [Id<Step>; 2],
    variants: [Id<Variant>; 2],
    enrollment: Id<Enrollment>,
    identity: Id<SenderIdentity>,
}

impl Fixture {
    /// The creator's request for step `position` (1 or 2), revision 1.
    fn step(&self, position: i32) -> StepMessage {
        let at = usize::try_from(position - 1).unwrap();
        StepMessage {
            enrollment: self.enrollment,
            campaign: self.campaign,
            step: self.steps[at],
            position,
            step_revision: 1,
            variant: self.variants[at],
            variant_version: 1,
            identity: self.identity,
            variables: None,
            snippets_fallback: None,
            send_at: None,
        }
    }
}

/// An SMTP login for `max@acme.example` made through the API, and its identity.
async fn identity(test: &TestDb, workspace: &TestWorkspace) -> Id<SenderIdentity> {
    let created = test
        .app()
        .post("/v1/connections")
        .bearer(&workspace.key)
        .idempotency("connection")
        .json(json!({
            "provider": "smtp",
            "account_email": "max@acme.example",
            "smtp": { "host": "smtp.acme.example", "port": 587, "security": "starttls", "password": "secret" },
        }))
        .send()
        .await;
    created.json["identities"][0]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// A campaign of two steps (the second in the same thread when `same_thread`), one variant each,
/// opens tracked, and Ada enrolled at the first step; written as the operator would.
async fn campaign(test: &TestDb, workspace: &TestWorkspace, same_thread: bool) -> Fixture {
    let identity = identity(test, workspace).await;
    let ws = workspace.id.uuid();
    let (campaign, enrollment, person) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let steps = [Uuid::now_v7(), Uuid::now_v7()];
    let variants = [Uuid::now_v7(), Uuid::now_v7()];
    let mut tx = test.system.begin().await.unwrap();
    sqlx::query!(
        "INSERT INTO campaigns (workspace_id, id, name, status, track_opens) VALUES ($1, $2, 'Launch', 'active', true)",
        ws,
        campaign
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    for (at, (step, variant)) in steps.iter().zip(&variants).enumerate() {
        let position = i32::try_from(at + 1).unwrap();
        sqlx::query!(
            "INSERT INTO steps (workspace_id, id, campaign_id, position, name) VALUES ($1, $2, $3, $4, $5)",
            ws,
            step,
            campaign,
            position,
            format!("Step {position}"),
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO step_revisions (workspace_id, step_id, revision, delay_seconds, same_thread, ranking_objective,
                                         observation_window_seconds, minimum_sample)
             VALUES ($1, $2, 1, 0, $3, 'replies', 86400, 100)",
            ws,
            step,
            same_thread || at == 0,
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query!(
            "UPDATE steps SET current_revision = 1 WHERE workspace_id = $1 AND id = $2",
            ws,
            step
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO variants (workspace_id, id, step_id, name) VALUES ($1, $2, $3, 'A')",
            ws,
            variant,
            step
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO variant_revisions (workspace_id, variant_id, version, subject, preheader, html, text)
             VALUES ($1, $2, 1, $3, 'For {{ person.given_name }}',
                     '<p>Hi {{ person.given_name }} at {{ person.fields.company_size | default(\"your team\") }}</p>',
                     'Hi {{ person.given_name }}')",
            ws,
            variant,
            format!("Step {position} for {{{{ person.given_name }}}}"),
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO step_revision_variants (workspace_id, step_id, step_revision, variant_id, variant_version)
             VALUES ($1, $2, 1, $3, 1)",
            ws,
            step,
            variant
        )
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    sqlx::query!(
        "INSERT INTO people (workspace_id, id, email, given_name) VALUES ($1, $2, 'ada@example.com', 'Ada')",
        ws,
        person
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO enrollments (workspace_id, id, campaign_id, person_id, next_run_at) VALUES ($1, $2, $3, $4, now())",
        ws,
        enrollment,
        campaign,
        person
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    Fixture {
        workspace: workspace.id,
        campaign: Id::from_uuid(campaign),
        steps: steps.map(Id::from_uuid),
        variants: variants.map(Id::from_uuid),
        enrollment: Id::from_uuid(enrollment),
        identity,
    }
}

/// Runs [`step`] in a transaction of its own on `db` and commits it.
async fn create_step(db: &Database, fixture: &Fixture, position: i32) -> StepOutcome {
    let mut tx = db.begin_in(fixture.workspace).await.unwrap();
    let outcome = step(&mut tx, &keys(), fixture.workspace, &fixture.step(position))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    outcome
}

/// The enrollment's pointer, its thread root and the number of its messages.
async fn enrollment(test: &TestDb, fixture: &Fixture) -> (Option<Uuid>, Option<Uuid>, i64) {
    let row = sqlx::query!(
        r#"SELECT e.message_id, e.thread_root_message_id,
                  (SELECT count(*) FROM messages m WHERE m.workspace_id = e.workspace_id AND m.enrollment_id = e.id) AS "messages!"
             FROM enrollments e WHERE e.workspace_id = $1 AND e.id = $2"#,
        fixture.workspace.uuid(),
        fixture.enrollment.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    (row.message_id, row.thread_root_message_id, row.messages)
}

/// Two creators of the same step racing on the enrollment leave exactly one message: the second
/// waits for the first's lock, then finds the pointer set and stops without writing.
#[tokio::test]
async fn two_creators_of_one_step_leave_one_message() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let fixture = campaign(&test, &acme, true).await;
    let mut first = test.worker.begin_in(fixture.workspace).await.unwrap();
    let created = step(&mut first, &keys(), fixture.workspace, &fixture.step(1))
        .await
        .unwrap();
    let second = {
        let db = test.app.clone();
        let request = fixture.step(1);
        let workspace = fixture.workspace;
        tokio::spawn(async move {
            let mut tx = db.begin_in(workspace).await.unwrap();
            let outcome = step(&mut tx, &keys(), workspace, &request).await.unwrap();
            tx.commit().await.unwrap();
            outcome
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !second.is_finished(),
        "the second creator waits for the enrollment's lock"
    );
    first.commit().await.unwrap();
    assert_eq!(
        second.await.unwrap(),
        StepOutcome::Stale(Stale::AlreadyCreated)
    );
    let StepOutcome::Created(accepted) = created else {
        panic!("the first creator created nothing: {created:?}");
    };
    assert_eq!(
        enrollment(&test, &fixture).await,
        (
            Some(accepted.message.uuid()),
            Some(accepted.message.uuid()),
            1
        )
    );
    let queued = sqlx::query!(
        "SELECT state, paced, connection_id FROM delivery_queue WHERE workspace_id = $1 AND message_id = $2",
        fixture.workspace.uuid(),
        accepted.message.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!((queued.state.as_str(), queued.paced), ("queued", true));
}

/// A creator holding a stale view writes nothing: the enrollment moved to another position, the
/// step got a new revision, or the enrollment is no longer active.
#[tokio::test]
async fn a_stale_creator_stops() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let fixture = campaign(&test, &acme, true).await;
    assert_eq!(
        create_step(&test.worker, &fixture, 2).await,
        StepOutcome::Stale(Stale::Moved)
    );
    let ws = fixture.workspace.uuid();
    let step = fixture.steps[0].uuid();
    sqlx::query!(
        "INSERT INTO step_revisions (workspace_id, step_id, revision, delay_seconds, ranking_objective, observation_window_seconds, minimum_sample)
         VALUES ($1, $2, 2, 0, 'replies', 86400, 100)",
        ws,
        step
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE steps SET current_revision = 2 WHERE workspace_id = $1 AND id = $2",
        ws,
        step
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        create_step(&test.worker, &fixture, 1).await,
        StepOutcome::Stale(Stale::Revised)
    );
    sqlx::query!(
        "UPDATE enrollments SET status = 'paused' WHERE workspace_id = $1",
        ws
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        create_step(&test.worker, &fixture, 1).await,
        StepOutcome::Stale(Stale::NotActive)
    );
    assert_eq!(enrollment(&test, &fixture).await, (None, None, 0));
}

/// The first step opens a thread whose root is its message; a later step whose revision keeps
/// the thread continues it, answering the thread's latest message, while one that does not
/// opens a thread of its own and leaves the conversation's root as it was.
#[tokio::test]
async fn follow_ups_continue_or_leave_the_first_thread() {
    for same_thread in [true, false] {
        let test = TestDb::new().await;
        let acme = test.workspace("acme").await;
        let fixture = campaign(&test, &acme, same_thread).await;
        let StepOutcome::Created(first) = create_step(&test.worker, &fixture, 1).await else {
            panic!("the first step was not created");
        };
        // The campaign moves the enrollment on once the first message is settled.
        sqlx::query!(
            "UPDATE enrollments SET message_id = NULL, current_position = 2 WHERE workspace_id = $1",
            fixture.workspace.uuid()
        )
        .execute(test.system.pool())
        .await
        .unwrap();
        let StepOutcome::Created(second) = create_step(&test.worker, &fixture, 2).await else {
            panic!("the second step was not created");
        };
        let row = sqlx::query!(
            r#"SELECT m.thread_id AS "thread_id!", m.in_reply_to, t.root_message_id, t.last_message_id,
                      t.last_internet_message_id
                 FROM messages m JOIN threads t ON t.workspace_id = m.workspace_id AND t.id = m.thread_id
                WHERE m.workspace_id = $1 AND m.id = $2"#,
            fixture.workspace.uuid(),
            second.message.uuid()
        )
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        assert_eq!(row.last_message_id, Some(second.message.uuid()));
        assert_eq!(
            row.last_internet_message_id.as_deref(),
            Some(second.internet_message_id.as_str())
        );
        if same_thread {
            assert_eq!(row.thread_id, first.thread.uuid());
            assert_eq!(row.root_message_id, Some(first.message.uuid()));
            assert_eq!(row.in_reply_to, Some(first.internet_message_id.clone()));
        } else {
            assert_ne!(row.thread_id, first.thread.uuid());
            assert_eq!(row.root_message_id, Some(second.message.uuid()));
            assert_eq!(row.in_reply_to, None);
        }
        let (pointer, root, messages) = enrollment(&test, &fixture).await;
        assert_eq!(
            (pointer, root, messages),
            (Some(second.message.uuid()), Some(first.message.uuid()), 2)
        );
    }
}

/// A message's Message-ID names its message and thread, under a tag only this deployment can
/// make: a reply naming it correlates without a lookup, while an altered tag, another
/// deployment's id or a foreign id correlates to nothing.
#[tokio::test]
async fn the_message_id_names_message_and_thread() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let fixture = campaign(&test, &acme, true).await;
    let StepOutcome::Created(accepted) = create_step(&test.worker, &fixture, 1).await else {
        panic!("not created");
    };
    let id = &accepted.internet_message_id;
    assert!(id.ends_with("@acme.example>"), "{id}");
    let stored = sqlx::query_scalar!(
        "SELECT internet_message_id FROM messages WHERE workspace_id = $1 AND id = $2",
        fixture.workspace.uuid(),
        accepted.message.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(&stored, id);
    assert_eq!(
        correlate(&keys(), id),
        Some((accepted.message, accepted.thread))
    );
    let tampered = id.replacen(
        &accepted.thread.uuid().simple().to_string(),
        &Uuid::now_v7().simple().to_string(),
        1,
    );
    assert_eq!(correlate(&keys(), &tampered), None);
    let other = Keys::from_deployment_key(&secrecy::SecretString::from(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        [9_u8; 32],
    )))
    .unwrap();
    assert_eq!(correlate(&other, id), None);
    assert_eq!(correlate(&keys(), "<CAF=xyz@mail.gmail.com>"), None);
}

/// The sender's own login prepares a step's message: it reads the variant revision, renders it
/// with the person frozen at creation, adds the unsubscribe link and the open pixel. Every
/// table `prepare` reads is granted to the sender's login.
#[tokio::test]
async fn the_sender_login_prepares_a_step_message() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let fixture = campaign(&test, &acme, true).await;
    let StepOutcome::Created(accepted) = create_step(&test.worker, &fixture, 1).await else {
        panic!("not created");
    };
    sqlx::query!(
        "UPDATE people SET given_name = 'Changed' WHERE workspace_id = $1",
        fixture.workspace.uuid()
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    let settings = Settings::new(
        keys(),
        &url::Url::parse("https://t.norbelys.test").unwrap(),
        Duration::from_secs(86_400),
    )
    .unwrap();
    let prepared = rendering::prepare(&test.worker, &settings, fixture.workspace, accepted.message)
        .await
        .unwrap();
    let parsed = mail_parser::MessageParser::default()
        .parse(&prepared.raw)
        .unwrap();
    assert_eq!(parsed.subject(), Some("Step 1 for Ada"));
    assert_eq!(
        parsed.header_raw("List-Unsubscribe-Post").map(str::trim),
        Some("List-Unsubscribe=One-Click")
    );
    let html = parsed.body_html(0).unwrap();
    assert!(
        html.contains("<p>Hi Ada at your team</p>"),
        "the person as frozen at creation: {html}"
    );
    assert!(html.contains("https://t.norbelys.test/t/o/"), "{html}");
    let recipients: Vec<String> = prepared
        .envelope
        .recipients()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(recipients, ["ada@example.com"]);
}

/// Transactional mail is accepted in the `system` workspace inside the caller's transaction,
/// which is back in its own workspace afterwards: without a sender tagged `transactional` it is
/// refused, with one it is queued, unpaced, with its usefulness as its first deadline.
#[tokio::test]
async fn transactional_mail_belongs_to_the_system_workspace() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let to = EmailAddress::parse("new.user@example.com").unwrap();
    let expires_at = crate::process::now().plus(Duration::from_secs(600));
    let mail = Transactional::SignInCode {
        to: &to,
        code: "123456",
        link: "https://app.norbelys.test/sign-in?token=abc",
        expires_at,
    };
    let mut tx = test.app.begin_in(acme.id).await.unwrap();
    assert!(matches!(
        transactional(&mut tx, &keys(), &mail).await,
        Err(Error::NoTransactionalSender)
    ));
    tx.rollback().await.unwrap();

    let system = crate::jobs::SYSTEM_WORKSPACE.uuid();
    let connection = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO connections (workspace_id, id, provider, transport, account_email, smtp, status, daily_limit)
           VALUES ($1, $2, 'sendgrid', 'smtp', 'Norbelys relay', '{"host": "smtp.sendgrid.net", "port": 587, "security": "starttls", "username": "apikey"}', 'active', 10000)"#,
        system,
        connection
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO sender_identities (workspace_id, connection_id, email, name, tags)
         VALUES ($1, $2, 'no-reply@norbelys.test', 'Norbelys', '{transactional}')",
        system,
        connection
    )
    .execute(test.system.pool())
    .await
    .unwrap();

    let mut tx = test.app.begin_in(acme.id).await.unwrap();
    let accepted = transactional(&mut tx, &keys(), &mail).await.unwrap();
    let seen: Option<Uuid> = sqlx::query_scalar("SELECT current_workspace()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        seen,
        Some(acme.id.uuid()),
        "the caller's workspace is restored"
    );
    tx.commit().await.unwrap();
    let mut anonymous = test.app.begin().await.unwrap();
    transactional(&mut anonymous, &keys(), &mail).await.unwrap();
    let seen: Option<Uuid> = sqlx::query_scalar("SELECT current_workspace()")
        .fetch_one(&mut *anonymous)
        .await
        .unwrap();
    assert_eq!(
        seen, None,
        "a transaction without a workspace has none again"
    );
    anonymous.commit().await.unwrap();

    let row = sqlx::query!(
        r#"SELECT m.kind, m.subject, m.text_body, m.from_email, q.paced, q.deadline_at AS "deadline_at: Timestamp"
             FROM messages m JOIN delivery_queue q ON q.workspace_id = m.workspace_id AND q.message_id = m.id
            WHERE m.workspace_id = $1 AND m.id = $2"#,
        system,
        accepted.message.uuid()
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(row.kind, "transactional");
    assert_eq!(row.subject, "123456 is your Norbelys sign-in code");
    assert!(
        row.text_body
            .unwrap()
            .contains("https://app.norbelys.test/sign-in?token=abc")
    );
    assert_eq!(row.from_email, "no-reply@norbelys.test");
    assert!(!row.paced);
    assert_eq!(
        row.deadline_at.map(|at| at.0.as_second()),
        Some(expires_at.0.as_second())
    );
}

/// The words of `text`, without the punctuation around them, list numbers (`1.`) left out.
fn words(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .filter(|word| {
            !word
                .strip_suffix('.')
                .is_some_and(|number| number.chars().all(|c| c.is_ascii_digit()))
        })
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|word| !word.is_empty())
        .collect()
}

/// Every transactional email renders in the one frame of the platform's own mail, whatever its
/// variant: the HTML is the frame's document, with the mark on the stand-in tracking origin, one
/// heading and the footer; no template syntax is left in the subject or either body; and the text
/// version says what the HTML shows, word for word both ways, links' addresses included, with no
/// markup. The variants come from the enum, so a new one fails here until it renders.
#[test]
fn every_transactional_email_renders_in_the_frame() {
    use strum::IntoEnumIterator as _;

    let to = EmailAddress::parse("ada@example.com").unwrap();
    let now = crate::process::now();
    let later = now.plus(Duration::from_secs(600));
    let link = "https://app.norbelys.test/sign-in/link#token=t0k3n&email=ada%40example.com";
    let lines = [HealthLine {
        account: "max@acme.example".to_owned(),
        provider: "smtp".to_owned(),
        status: "needs to be connected again".to_owned(),
        detail: Some("The server refused the login.".to_owned()),
        paused: true,
    }];
    for kind in TransactionalKind::iter() {
        let mail = match kind {
            TransactionalKind::SignInCode => Transactional::SignInCode {
                to: &to,
                code: "482913",
                link,
                expires_at: later,
            },
            TransactionalKind::Welcome => Transactional::Welcome {
                to: &to,
                dashboard: "https://app.norbelys.test/",
                expires_at: later,
            },
            TransactionalKind::Invitation => Transactional::Invitation {
                to: &to,
                workspace_name: "Acme",
                inviter: Some("Grace Hopper"),
                role: "admin",
                link,
                expires_at: later,
            },
            TransactionalKind::ConnectionHealth => Transactional::ConnectionHealth {
                to: &to,
                workspace_name: "Acme",
                from: now,
                until: later,
                connections: &lines,
                expires_at: later,
            },
            TransactionalKind::BreakGlass => Transactional::BreakGlass {
                to: &to,
                workspace_name: "Acme",
                owner: "Grace Hopper",
                reason: "Their identity provider is down.",
                expires_at: later,
            },
            TransactionalKind::WebhookFailure => Transactional::WebhookFailure {
                to: &to,
                workspace_name: "Acme",
                url: "https://hooks.acme.example/norbelys",
                failing_since: now,
                disabled: true,
                last_error: "HTTP 500",
                expires_at: later,
            },
        };
        let Rendered {
            subject,
            html: Some(html),
            text: Some(text),
        } = content(&mail, "no-reply@norbelys.test", Some("Norbelys"), now)
            .unwrap()
            .rendered
        else {
            panic!("{kind:?}: both bodies");
        };
        assert!(html.starts_with("<!DOCTYPE html>"), "{kind:?}");
        assert!(
            html.contains("<img src=\"https://tracking.invalid/brand/v1/email-mark.png\" width=\"24\" height=\"24\" alt=\"Norbelys\""),
            "{kind:?}"
        );
        assert_eq!(html.matches("<h1 ").count(), 1, "{kind:?}");
        assert!(
            html.contains(
                "Norbelys · Open-source outbound email · <a href=\"https://norbelys.com\""
            ),
            "{kind:?}"
        );
        for part in [&subject, &html, &text] {
            for syntax in ["{{", "}}", "{%", "%}", "{#", "#}"] {
                assert!(
                    !part.contains(syntax),
                    "{kind:?}: `{syntax}` left in {part}"
                );
            }
        }
        assert!(!text.contains('<'), "{kind:?}: markup in {text}");
        assert!(
            text.ends_with("\n\n-- \nNorbelys · Open-source outbound email · https://norbelys.com"),
            "{kind:?}: {text}"
        );
        let shown = rendering::plain::from_html(&html).unwrap();
        for word in words(&text) {
            assert!(
                shown.contains(word),
                "{kind:?}: `{word}` is not in the HTML: {shown}"
            );
        }
        for word in words(&shown) {
            assert!(
                text.contains(word),
                "{kind:?}: `{word}` is not in the text: {text}"
            );
        }
    }
}
