//! Tests of owner recovery and the operator's sessions: recovery codes registered from the
//! dashboard, the break-glass an operator opens with one (the codes it accepts, the session it
//! makes, its audit row and notice) and what that session may then do, and impersonation.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, User, WorkspaceId};
use crate::identity::recovery::{self, BreakGlass, BreakGlassRequest, RecoveryError};
use crate::jobs::SYSTEM_WORKSPACE;
use crate::testing::{self, TestDb};

/// Why the operator opens the tests' break-glass sessions.
const REASON: &str = "The identity provider's application was deleted.";

/// Makes the user holding `email` (created when nobody does) a member of `workspace` with
/// `role`; answers the user.
async fn member(test: &TestDb, workspace: WorkspaceId, email: &str, role: &str) -> Id<User> {
    let user: Uuid = sqlx::query_scalar(
        "WITH u AS (INSERT INTO users (email, email_verified_at) VALUES ($1, now())
                    ON CONFLICT (email_key) DO UPDATE SET updated_at = now() RETURNING id)
         INSERT INTO memberships (workspace_id, user_id, role) SELECT $2, id, $3 FROM u RETURNING user_id",
    )
    .bind(email)
    .bind(workspace.uuid())
    .bind(role)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    Id::from_uuid(user)
}

/// An active SSO connection of `workspace` whose enforcement began `days` ago; answers its id.
async fn enforcing(test: &TestDb, workspace: WorkspaceId, days: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO sso_connections (workspace_id, kind, name, issuer, client_id, enforced, enforced_at, status)
         VALUES ($1, 'oidc', 'Okta', 'https://idp.example', 'norbelys', true, now() - make_interval(days => $2), 'active')
         RETURNING id",
    )
    .bind(workspace.uuid())
    .bind(days)
    .fetch_one(test.system.pool())
    .await
    .unwrap()
}

/// Registers a set of recovery codes for `user` as if `days` ago; answers them.
async fn codes(test: &TestDb, user: Id<User>, days: i32) -> Vec<String> {
    let mut tx = test.system.begin().await.unwrap();
    let registered = recovery::register(&mut tx, &testing::keys(), user)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE recovery_codes SET created_at = now() - make_interval(days => $2) WHERE user_id = $1",
    )
    .bind(user.uuid())
    .bind(days)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    registered.codes
}

/// The operator's break-glass for `owner` of `workspace` with `code`, committed when it opens.
async fn break_glass(
    test: &TestDb,
    workspace: &str,
    owner: &str,
    code: &str,
) -> Result<BreakGlass, RecoveryError> {
    let owner = EmailAddress::parse(owner).unwrap();
    let mut tx = test.system.begin().await.unwrap();
    let opened = recovery::break_glass(
        &mut tx,
        &testing::keys(),
        &BreakGlassRequest {
            workspace,
            owner: &owner,
            code,
            session: None,
            reason: REASON,
        },
    )
    .await;
    if opened.is_ok() {
        tx.commit().await.unwrap();
    }
    opened
}

/// Registering recovery codes answers ten codes of four groups of four digits, once, to a
/// signed-in browser only; registering again replaces the set.
#[tokio::test]
async fn recovery_codes_are_registered_from_the_dashboard() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let session = test.session("owner@acme.example").await;
    let first = app
        .post("/v1/me/recovery_codes")
        .browser(&session)
        .idempotency("first")
        .send()
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.json);
    let mut codes: Vec<&str> = first.json["codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|code| code.as_str().unwrap())
        .collect();
    assert_eq!(codes.len(), 10);
    for code in &codes {
        let groups: Vec<&str> = code.split('-').collect();
        assert_eq!(groups.len(), 4, "{code}");
        assert!(
            groups
                .iter()
                .all(|group| group.len() == 4 && group.bytes().all(|b| b.is_ascii_digit())),
            "{code}"
        );
    }
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(codes.len(), 10, "the codes are distinct");
    let again = app
        .post("/v1/me/recovery_codes")
        .browser(&session)
        .idempotency("again")
        .send()
        .await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.json);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM recovery_codes WHERE user_id = $1")
        .bind(acme.owner.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(stored, 10);
    let program = app
        .post("/v1/me/recovery_codes")
        .bearer(&acme.key)
        .idempotency("program")
        .send()
        .await;
    assert_eq!(program.status, StatusCode::FORBIDDEN, "{}", program.json);
}

/// A break-glass takes an unused code of an active owner registered before the workspace began
/// enforcing single sign-on, and nothing else: it turns the owner's live session into a 24-hour
/// session standing in for the enforcing connection, records the reason in the audit log, tells
/// the other owners, and spends the code.
#[tokio::test]
async fn break_glass_takes_a_code_registered_before_enforcement() {
    let test = TestDb::new().await;
    test.transactional_sender().await;
    let acme = test.workspace("acme").await;
    test.workspace("globex").await;
    member(&test, acme.id, "co@acme.example", "owner").await;
    let admin = member(&test, acme.id, "admin@acme.example", "admin").await;
    let connection = enforcing(&test, acme.id, 1).await;
    let early = codes(&test, acme.owner, 2).await;
    codes(&test, admin, 2).await;
    let owner = "owner@acme.example";

    assert!(matches!(
        break_glass(&test, "acme", owner, &early[0]).await,
        Err(RecoveryError::NoSession)
    ));
    let session = test.session(owner).await;
    let refusals = [
        ("nowhere", owner, early[0].as_str()),
        ("globex", "owner@globex.example", early[0].as_str()),
        ("acme", "admin@acme.example", early[0].as_str()),
        ("acme", owner, "0000-0000-0000-0000"),
    ];
    let mut refused = Vec::new();
    for (workspace, owner, code) in refusals {
        let opened = break_glass(&test, workspace, owner, code).await;
        refused.push(format!("{:?}", opened.err()));
    }
    assert_eq!(
        refused,
        [
            "Some(NoWorkspace)",
            "Some(NotEnforced)",
            "Some(NotAnOwner)",
            "Some(CodeRefused)"
        ]
    );

    let opened = break_glass(
        &test,
        &acme.id.to_string(),
        owner,
        &early[0].replace('-', " "),
    )
    .await
    .unwrap();
    assert_eq!(
        (opened.session, opened.user, opened.owners_told),
        (session.session, acme.owner, 1)
    );
    let row: (String, Option<Uuid>, bool, Option<Uuid>) = sqlx::query_as(
        "SELECT auth_method, sso_connection_id, expires_at <= now() + interval '24 hours', active_workspace_id
           FROM sessions WHERE id = $1",
    )
    .bind(session.session.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "break_glass".to_owned(),
            Some(connection),
            true,
            Some(acme.id.uuid())
        )
    );
    let audited: Vec<(String, String)> = sqlx::query_as(
        "SELECT actor_kind, details ->> 'reason' FROM audit_log
          WHERE workspace_id = $1 AND action = 'break_glass.started'",
    )
    .bind(acme.id.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert_eq!(audited, [("system".to_owned(), REASON.to_owned())]);
    let told: Vec<(String, String)> = sqlx::query_as(
        "SELECT to_addresses[1], subject FROM messages WHERE workspace_id = $1 AND kind = 'transactional'",
    )
    .bind(SYSTEM_WORKSPACE.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert_eq!(
        told,
        [(
            "co@acme.example".to_owned(),
            "A break-glass session was opened in acme".to_owned()
        )]
    );

    assert!(matches!(
        break_glass(&test, "acme", owner, &early[0]).await,
        Err(RecoveryError::CodeRefused)
    ));
    let late = codes(&test, acme.owner, 0).await;
    assert!(matches!(
        break_glass(&test, "acme", owner, &late[0]).await,
        Err(RecoveryError::CodeRefused)
    ));
}

/// A break-glass session mints workspace tokens for the workspace it repairs alone, carrying the
/// repair scopes: the workspace's members are within reach, its people are not, a key made with
/// the token cannot reach further, and another workspace of the same owner refuses the session.
#[tokio::test]
async fn a_break_glass_session_repairs_its_workspace_and_nothing_else() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let acme = test.workspace("acme").await;
    let globex = test.workspace("globex").await;
    member(&test, globex.id, "owner@acme.example", "owner").await;
    enforcing(&test, acme.id, 1).await;
    let early = codes(&test, acme.owner, 2).await;
    let app = test.app();
    let session = test.session("owner@acme.example").await;
    let mint = |workspace: WorkspaceId| {
        app.post("/v1/auth/tokens")
            .browser(&session)
            .json(json!({ "workspace_id": workspace.to_string() }))
            .send()
    };
    assert_eq!(mint(acme.id).await.status, StatusCode::FORBIDDEN);
    break_glass(&test, "acme", "owner@acme.example", &early[0])
        .await
        .unwrap();

    let minted = mint(acme.id).await;
    assert_eq!(minted.status, StatusCode::CREATED, "{}", minted.json);
    assert_eq!(
        minted.json["scopes"],
        json!(["workspace:read", "workspace:manage"])
    );
    let token = minted.json["token"].as_str().unwrap();
    let members = app
        .get(&format!("/v1/workspaces/{}/members", acme.id))
        .bearer(token)
        .send()
        .await;
    assert_eq!(members.status, StatusCode::OK, "{}", members.json);
    let people = app.get("/v1/people").bearer(token).send().await;
    assert_eq!(people.status, StatusCode::FORBIDDEN, "{}", people.json);
    let key = app
        .post(&format!("/v1/workspaces/{}/api_keys", acme.id))
        .bearer(token)
        .idempotency("key")
        .json(json!({ "name": "Escape", "scopes": ["people:read"] }))
        .send()
        .await;
    assert_eq!(key.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", key.json);
    assert_eq!(mint(globex.id).await.status, StatusCode::FORBIDDEN);
}

/// An impersonation is a session of 10 minutes at most, recorded with its reason in every
/// workspace where the person is an active member; it needs a reason, and its printed cookie
/// signs the operator's browser in as the person.
#[tokio::test]
async fn an_impersonation_is_a_short_audited_session() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let globex = test.workspace("globex").await;
    member(&test, globex.id, "owner@acme.example", "member").await;
    let person = EmailAddress::parse("owner@acme.example").unwrap();
    let reason = "Ticket 4711: the campaign editor shows no steps";
    let mut tx = test.system.begin().await.unwrap();
    assert!(matches!(
        recovery::impersonate(&mut tx, &testing::keys(), &person, "  ").await,
        Err(RecoveryError::NoReason)
    ));
    let opened = recovery::impersonate(&mut tx, &testing::keys(), &person, reason)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut audited: Vec<Uuid> = opened.workspaces.iter().map(|id| id.uuid()).collect();
    audited.sort_unstable();
    let mut members = vec![acme.id.uuid(), globex.id.uuid()];
    members.sort_unstable();
    assert_eq!(audited, members);
    let (method, short): (String, bool) = sqlx::query_as(
        "SELECT auth_method, expires_at <= now() + interval '10 minutes' AND idle_expires_at <= expires_at
           FROM sessions WHERE id = $1",
    )
    .bind(opened.session.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!((method.as_str(), short), ("impersonation", true));
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'impersonation.started' AND details ->> 'reason' = $1",
    )
    .bind(reason)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(rows, 2);
    let me = test
        .app()
        .get("/v1/me")
        .header("cookie", &opened.cookie)
        .send()
        .await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.json);
}
