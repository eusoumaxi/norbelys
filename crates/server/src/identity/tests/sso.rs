//! Workspace single sign-on against the fake provider: connections and their checks, routing by a
//! proved domain, just-in-time membership and its tombstones, the proof an enforcing workspace
//! needs; and, at the store level, the lookup that routes before any workspace is known and the
//! recording of a connection's checks.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::{FakeIdp, identity_with, mint, parameter, set_cookie};
use crate::domain::ids::{Id, SsoConnection, WorkspaceId};
use crate::identity::sso::{self, Discovery, DomainProof, Report};
use crate::testing::{Reply, TestApp, TestDb, TestSession};

/// Marks every SSO email domain of `workspace` verified, as a found TXT record would (the tests'
/// resolver has no name servers).
async fn prove_domains(test: &TestDb, workspace: WorkspaceId) {
    sqlx::query("UPDATE sso_email_domains SET verified_at = now() WHERE workspace_id = $1")
        .bind(workspace.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
}

/// Creates an SSO connection to `idp` for `domains` through the API.
async fn connect(
    app: &TestApp,
    idp: &FakeIdp,
    token: &str,
    workspace: WorkspaceId,
    domains: &[&str],
) -> Reply {
    app.post(&format!("/v1/workspaces/{workspace}/sso_connections"))
        .bearer(token)
        .idempotency(&format!("connect-{}", domains.join(",")))
        .json(json!({
            "name": "Fake IdP",
            "issuer": idp.issuer,
            "client_id": "client",
            "client_secret": "secret",
            "domains": domains,
        }))
        .send()
        .await
}

/// Signs `email` in through the SSO connection its domain routes to: the challenge (from
/// `browser` when linking), the provider's answer for `subject` authenticated `ago` seconds
/// earlier, and the callback.
async fn through_sso(
    app: &TestApp,
    idp: &FakeIdp,
    email: &str,
    subject: &str,
    ago: i64,
    browser: Option<&TestSession>,
) -> Reply {
    let call = match browser {
        Some(browser) => app.post("/v1/auth/challenges").browser(browser),
        None => app.post("/v1/auth/challenges").dashboard(),
    };
    let started = call
        .json(json!({ "method": "sso", "email": email, "link": browser.is_some() }))
        .send()
        .await;
    assert_eq!(started.status, StatusCode::CREATED, "{}", started.json);
    let url = started.json["authorization_url"].as_str().unwrap();
    assert_eq!(parameter(url, "max_age"), "86400");
    assert_eq!(parameter(url, "login_hint"), email);
    idp.issue(
        subject,
        Some(email),
        true,
        &parameter(url, "nonce"),
        Some(ago),
    );
    let ceremony = set_cookie(&started, "__Host-nb_ceremony").unwrap();
    let cookie = match browser {
        Some(browser) => format!("{}; {ceremony}", browser.cookie),
        None => ceremony,
    };
    app.get(&format!(
        "/v1/auth/callback?state={}&code=c",
        started.json["id"].as_str().unwrap()
    ))
    .header("cookie", &cookie)
    .send()
    .await
}

/// The browser a sign-in through the callback left: its cookie, and the CSRF token `GET /me`
/// hands the dashboard.
async fn browser_of(app: &TestApp, reply: &Reply) -> TestSession {
    let cookie = set_cookie(reply, "__Host-nb_session").expect("a session cookie");
    let me = app.get("/v1/me").header("cookie", &cookie).send().await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.json);
    TestSession {
        user: me.json["id"].as_str().unwrap().parse().unwrap(),
        session: me.json["session_id"].as_str().unwrap().parse().unwrap(),
        cookie,
        csrf: me.json["csrf_token"].as_str().unwrap().to_owned(),
    }
}

/// A connection is checked when created (its discovery makes it `active`), routes nobody until a
/// domain is proved, then signs the domain's people in through the provider with `max_age`
/// requested, creates their membership with the default role (`sso_jit`), records the proof on the
/// session (the provider's `auth_time`), and refuses an authentication older than a day.
#[tokio::test]
async fn a_connection_routes_proved_domains_and_provisions_members() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let idp = FakeIdp::start().await;
    let app = test.app_with_identity(identity_with(&idp));
    let acme = test.workspace("acme").await;
    let owner = test.session("owner@acme.example").await;
    let token = mint(&app, &owner, acme.id).await;

    let created = connect(&app, &idp, &token, acme.id, &["Acme.Test"]).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    assert_eq!(created.json["status"], "active");
    assert_eq!(created.json["client_secret_set"], true);
    assert_eq!(created.json["policy_version"], 1);
    assert_eq!(created.json["domains"][0]["domain"], "acme.test");
    assert!(created.json["domains"][0]["verified_at"].is_null());
    assert_eq!(
        created.json["domains"][0]["record"]["name"],
        "_norbelys-sso.acme.test"
    );
    assert!(created.header("etag").is_some());

    let unproved = app
        .post("/v1/auth/challenges")
        .dashboard()
        .json(json!({ "method": "sso", "email": "ada@acme.test" }))
        .send()
        .await;
    assert_eq!(unproved.status, StatusCode::NOT_FOUND);

    prove_domains(&test, acme.id).await;
    let signed_in = through_sso(&app, &idp, "ada@acme.test", "ada", 60, None).await;
    assert_eq!(signed_in.status, StatusCode::SEE_OTHER);
    assert_eq!(signed_in.header("location"), Some("/?signed_in=true"));
    let ada = browser_of(&app, &signed_in).await;
    let (role, source): (String, String) = sqlx::query_as(
        "SELECT role, source FROM memberships WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(acme.id.uuid())
    .bind(ada.user.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!((role.as_str(), source.as_str()), ("member", "sso_jit"));
    let (method, connection, proven): (String, Option<Uuid>, bool) = sqlx::query_as(
        "SELECT auth_method, sso_connection_id, authenticated_at < now() - interval '50 seconds'
           FROM sessions WHERE id = $1",
    )
    .bind(ada.session.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(method, "sso");
    assert!(connection.is_some());
    assert!(proven, "the session's proof is the provider's auth_time");
    assert!(mint(&app, &ada, acme.id).await.starts_with("nbs_"));

    let stale = through_sso(&app, &idp, "old@acme.test", "old", 25 * 3600, None).await;
    assert!(
        stale
            .header("location")
            .unwrap()
            .contains("error=sign_in_failed")
    );
}

/// Enforcement needs a fresh proof through the connection: the owner cannot turn it on from an
/// email-code session, links the provider's identity and signs in through it, turns it on (which
/// moves the policy version), and then every older proof, the owner's own included, stops counting
/// until a new sign-in through the provider.
#[tokio::test]
async fn enforcement_needs_a_fresh_proof_through_the_connection() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let idp = FakeIdp::start().await;
    let app = test.app_with_identity(identity_with(&idp));
    let acme = test.workspace("acme").await;
    let owner = test.session("owner@acme.example").await;
    let token = mint(&app, &owner, acme.id).await;
    let created = connect(&app, &idp, &token, acme.id, &["acme.example"]).await;
    let path = format!(
        "/v1/workspaces/{}/sso_connections/{}",
        acme.id,
        created.json["id"].as_str().unwrap()
    );
    prove_domains(&test, acme.id).await;

    let too_early = app
        .patch(&path)
        .bearer(&token)
        .json(json!({ "enforced": true }))
        .send()
        .await;
    assert_eq!(too_early.status, StatusCode::CONFLICT);

    let unlinked = through_sso(&app, &idp, "owner@acme.example", "owner", 5, None).await;
    assert!(
        unlinked
            .header("location")
            .unwrap()
            .contains("error=needs_link")
    );
    let linked = through_sso(&app, &idp, "owner@acme.example", "owner", 5, Some(&owner)).await;
    assert_eq!(linked.header("location"), Some("/?linked=true"));
    let through = through_sso(&app, &idp, "owner@acme.example", "owner", 5, None).await;
    let sso_owner = browser_of(&app, &through).await;
    assert_eq!(sso_owner.user, owner.user);
    let sso_token = mint(&app, &sso_owner, acme.id).await;

    let enforced = app
        .patch(&path)
        .bearer(&sso_token)
        .json(json!({ "enforced": true }))
        .send()
        .await;
    assert_eq!(enforced.status, StatusCode::OK, "{}", enforced.json);
    assert_eq!(enforced.json["enforced"], true);
    assert_eq!(enforced.json["policy_version"], 2);

    for browser in [&owner, &sso_owner] {
        let refused = app
            .post("/v1/auth/tokens")
            .browser(browser)
            .json(json!({ "workspace_id": acme.id.to_string() }))
            .send()
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.json);
    }
    let workspace_path = format!("/v1/workspaces/{}", acme.id);
    assert_eq!(
        app.get(&workspace_path)
            .bearer(&sso_token)
            .send()
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let fresh = through_sso(&app, &idp, "owner@acme.example", "owner", 5, None).await;
    let fresh = browser_of(&app, &fresh).await;
    let token = mint(&app, &fresh, acme.id).await;
    assert_eq!(
        app.get(&workspace_path).bearer(&token).send().await.status,
        StatusCode::OK
    );
}

/// A removed member stays removed: signing in through SSO with their linked identity signs them in
/// but never revives the tombstone, so removal cannot be undone by the identity provider.
#[tokio::test]
async fn single_sign_on_never_revives_a_removed_member() {
    let test = TestDb::new().await;
    test.signing_key().await;
    let idp = FakeIdp::start().await;
    let app = test.app_with_identity(identity_with(&idp));
    let acme = test.workspace("acme").await;
    let owner = test.session("owner@acme.example").await;
    let token = mint(&app, &owner, acme.id).await;
    connect(&app, &idp, &token, acme.id, &["acme.example"]).await;
    prove_domains(&test, acme.id).await;
    let gone = test.session("gone@acme.example").await;
    sqlx::query(
        "INSERT INTO memberships (workspace_id, user_id, role, status) VALUES ($1, $2, 'member', 'removed')",
    )
    .bind(acme.id.uuid())
    .bind(gone.user.uuid())
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO identity_links (issuer, subject, user_id) VALUES ($1, 'gone', $2)")
        .bind(&idp.issuer)
        .bind(gone.user.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    let back = through_sso(&app, &idp, "gone@acme.example", "gone", 5, None).await;
    assert_eq!(back.header("location"), Some("/?signed_in=true"));
    let status: String = sqlx::query_scalar(
        "SELECT status FROM memberships WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(acme.id.uuid())
    .bind(gone.user.uuid())
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert_eq!(status, "removed");
}

/// Inserts an SSO connection of `workspace` with `status` and one domain, verified or not.
async fn connection_row(
    test: &TestDb,
    workspace: WorkspaceId,
    status: &str,
    domain: &str,
    verified: bool,
) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO sso_connections (workspace_id, kind, name, issuer, client_id, status)
         VALUES ($1, 'oidc', 'IdP', 'https://idp.example.com', 'client', $2) RETURNING id",
    )
    .bind(workspace.uuid())
    .bind(status)
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sso_email_domains (workspace_id, sso_connection_id, domain, ownership_token, verified_at)
         VALUES ($1, $2, $3, 'token', CASE WHEN $4 THEN now() END)",
    )
    .bind(workspace.uuid())
    .bind(id)
    .bind(domain)
    .bind(verified)
    .execute(test.system.pool())
    .await
    .unwrap();
    id
}

/// The routing lookup runs before any workspace is known and finds only a proved domain of an
/// active connection, while the tables behind it stay dark to the api's login without a
/// workspace: a sign-in learns where to go and nothing else.
#[tokio::test]
async fn the_route_lookup_sees_only_proved_domains_of_active_connections() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let routed = connection_row(&test, acme.id, "active", "routed.test", true).await;
    connection_row(&test, acme.id, "pending", "pending.test", true).await;
    connection_row(&test, acme.id, "active", "unproved.test", false).await;
    let mut tx = test.app.begin().await.unwrap();
    assert_eq!(
        sso::route(&mut tx, "routed.test").await.unwrap(),
        Some((acme.id, routed))
    );
    for domain in ["pending.test", "unproved.test", "absent.test"] {
        assert_eq!(sso::route(&mut tx, domain).await.unwrap(), None, "{domain}");
    }
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM sso_email_domains")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(visible, 0);
}

/// Recording a connection's checks makes it `active` with the discovery document, un-verifies a
/// domain whose record is definitively gone, leaves one whose lookup failed as it was, and keeps a
/// domain another workspace proved unverified here (the rest of the record still lands).
#[tokio::test]
async fn checks_are_recorded_without_trusting_a_failed_lookup() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let globex = test.workspace("globex").await;
    connection_row(&test, globex.id, "active", "shared.test", true).await;
    let id = connection_row(&test, acme.id, "pending", "gone.test", true).await;
    for (domain, verified) in [("blip.test", true), ("shared.test", false)] {
        sqlx::query(
            "INSERT INTO sso_email_domains (workspace_id, sso_connection_id, domain, ownership_token, verified_at)
             VALUES ($1, $2, $3, 't', CASE WHEN $4 THEN now() END)",
        )
        .bind(acme.id.uuid())
        .bind(id)
        .bind(domain)
        .bind(verified)
        .execute(test.system.pool())
        .await
        .unwrap();
    }
    let report = Report {
        issuer: "https://idp.example.com".to_owned(),
        discovery: Discovery::Found(json!({ "issuer": "https://idp.example.com" })),
        domains: vec![
            ("gone.test".to_owned(), DomainProof::Missing),
            ("blip.test".to_owned(), DomainProof::Unknown),
            ("shared.test".to_owned(), DomainProof::Found),
        ],
    };
    let mut tx = test.app.begin_in(acme.id).await.unwrap();
    sso::record(&mut tx, acme.id, id, &report).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = test.app.begin_in(acme.id).await.unwrap();
    let connection = sso::read(&mut tx, acme.id, Id::<SsoConnection>::from_uuid(id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(connection.status, "active");
    let verified = |domain: &str| {
        connection
            .domains
            .iter()
            .find(|candidate| candidate.domain == domain)
            .unwrap()
            .verified_at
            .is_some()
    };
    assert!(!verified("gone.test"));
    assert!(verified("blip.test"));
    assert!(!verified("shared.test"));
    let detail = connection.status_detail.unwrap();
    assert!(
        detail.contains("gone.test") && detail.contains("another workspace"),
        "{detail}"
    );
}
