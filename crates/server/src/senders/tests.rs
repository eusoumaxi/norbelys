//! Store and API tests of the Sending area, against real PostgreSQL.
//!
//! The API runs in process ([`TestDb::app`]); the jobs run through the runner's [`Harness`] with
//! the worker's own login, so row security, the fences and the scheduler role are in play. The
//! providers are fakes on the loopback interface started by each test: an SMTP server whose
//! answer to `AUTH` the test chooses (and may hold, to change the row while a check is out), and
//! an OAuth and Gmail server that issues the tokens and profile a test configures.

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use norbelys_mail::http::HttpClient;
use norbelys_mail::net::AddressPolicy;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::check::{ConnectionCheck, ConnectionCheckDue};
use super::domains::DomainVerify;
use super::provision::{Control, NorbelysProvision};
use super::{Env, Settings, oauth};
use crate::domain::ids::{Id, User};
use crate::domain::scope::ScopeSet;
use crate::identity::api_keys::{self, KeyMode};
use crate::jobs::runner::Harness;
use crate::jobs::{Queue, Registry};
use crate::testing::{self, Reply, TestApp, TestDb, TestWorkspace};

// ───────────────────────────── helpers ─────────────────────────────

/// A fresh idempotency key.
fn key() -> String {
    Uuid::now_v7().to_string()
}

/// `POST /v1/connections` with `body`.
async fn connect(app: &TestApp, credential: &str, body: Value) -> Reply {
    app.post("/v1/connections")
        .bearer(credential)
        .idempotency(&key())
        .json(body)
        .send()
        .await
}

/// The body of an SMTP login at `host:port`.
fn smtp_login(account: &str, host: &str, port: u16, security: &str) -> Value {
    json!({
        "provider": "smtp",
        "account_email": account,
        "smtp": { "host": host, "port": port, "security": security, "password": "app-password" },
    })
}

/// Establish the workspace ownership precondition without depending on public DNS.
async fn verified_mail_domain(test: &TestDb, workspace: &TestWorkspace, name: &str) {
    sqlx::query("INSERT INTO sending_domains (workspace_id, hostname, status, purpose, ownership_token, ownership_expires_at, verified_at) VALUES ($1, $2, 'verified', 'send', 'test-proof', now() + interval '30 days', now())")
        .bind(workspace.id.uuid()).bind(name).execute(test.system.pool()).await.unwrap();
}

/// The ids and addresses of a connection's identities, in order.
fn identity_ids(connection: &Value) -> Vec<(String, String)> {
    connection["identities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|identity| {
            (
                identity["id"].as_str().unwrap().to_owned(),
                identity["email"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// SMTP reuses a live HTTP key, but cannot borrow another tenant's domain or a paused service.
#[tokio::test]
async fn smtp_api_key_authentication_is_live_scoped_and_revocable() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let other = test.workspace("other").await;
    let sandbox = test.test_workspace("sandbox").await;
    let app = test.app();
    verified_mail_domain(&test, &acme, "acme.example").await;
    let created = connect(
        &app,
        &acme.key,
        json!({
            "provider": "norbelys", "account_email": "acme.example", "identities": []
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    let connection = created.json["id"]
        .as_str()
        .unwrap()
        .parse::<Id<crate::domain::ids::Connection>>()
        .unwrap();
    let path = "/v1/smtp/auth?domain=acme.example";
    let pending = app.get(path).bearer(&acme.key).send().await;
    assert_eq!(pending.status, StatusCode::NOT_FOUND);
    sqlx::query("UPDATE connections SET status = 'active' WHERE workspace_id = $1 AND id = $2")
        .bind(acme.id.uuid())
        .bind(connection.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    let allowed = app.get(path).bearer(&acme.key).send().await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.json);
    assert_eq!(
        allowed.json["username"],
        super::managed::username(acme.id, "acme.example")
    );
    assert_eq!(allowed.headers["cache-control"], "no-store");
    let foreign = app.get(path).bearer(&other.key).send().await;
    assert!(!foreign.status.is_success());
    let test_key = app.get(path).bearer(&sandbox.key).send().await;
    assert_eq!(test_key.status, StatusCode::FORBIDDEN);
    let unscoped_key = test.api_key(&acme, ScopeSet::default()).await;
    let unscoped = app.get(path).bearer(&unscoped_key).send().await;
    assert_eq!(unscoped.status, StatusCode::FORBIDDEN);
    sqlx::query("UPDATE connections SET paused = true WHERE workspace_id = $1 AND id = $2")
        .bind(acme.id.uuid())
        .bind(connection.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    let paused = app.get(path).bearer(&acme.key).send().await;
    assert_eq!(paused.status, StatusCode::NOT_FOUND);
    sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE workspace_id = $1")
        .bind(acme.id.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    let revoked = app.get(path).bearer(&acme.key).send().await;
    assert_eq!(revoked.status, StatusCode::UNAUTHORIZED);
}

/// Domain intent is explicit, tracking uses a separate hostname, and mail ownership
/// cannot be borrowed from another workspace to provision a managed mailbox.
#[tokio::test]
async fn domain_uses_and_managed_mailbox_access_are_workspace_scoped() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let other = test.workspace("other").await;
    let app = test.app();
    let domain = app.post("/v1/sending_domains").bearer(&acme.key).idempotency(&key())
        .json(json!({"hostname":"acme.example","purpose":"send_receive","tracking_hostname":"links.acme.example"})).send().await;
    assert_eq!(domain.status, StatusCode::CREATED, "{}", domain.json);
    assert_eq!(domain.json["purpose"], "send_receive");
    assert_eq!(domain.json["tracking_enabled"], false);
    assert_eq!(
        domain.json["tracking_domain"]["hostname"],
        "links.acme.example"
    );
    assert_eq!(
        domain.json["tracking_domain"]["records"][1]["type"],
        "CNAME"
    );
    let id = domain.json["id"].as_str().unwrap();
    let unverified = connect(
        &app,
        &acme.key,
        json!({"provider":"norbelys","account_email":"hello@acme.example"}),
    )
    .await;
    assert_eq!(unverified.status, StatusCode::UNPROCESSABLE_ENTITY);
    sqlx::query("UPDATE sending_domains SET status = 'verified', verified_at = now() WHERE workspace_id = $1 AND hostname = 'acme.example'")
        .bind(acme.id.uuid()).execute(test.system.pool()).await.unwrap();
    let stranger = connect(
        &app,
        &other.key,
        json!({"provider":"norbelys","account_email":"hello@acme.example"}),
    )
    .await;
    assert_eq!(stranger.status, StatusCode::UNPROCESSABLE_ENTITY);
    let mailbox = connect(
        &app,
        &acme.key,
        json!({"provider":"norbelys","account_email":"hello@acme.example"}),
    )
    .await;
    assert_eq!(mailbox.status, StatusCode::CREATED, "{}", mailbox.json);
    assert_eq!(mailbox.json["imap"]["port"], 993);
    assert_eq!(mailbox.json["receiving"]["folders"][0]["folder"], "INBOX");
    let path = format!("/v1/sending_domains/{id}");
    let receive_only = app
        .patch(&path)
        .bearer(&acme.key)
        .json(json!({"purpose":"receive","tracking_hostname":null}))
        .send()
        .await;
    assert_eq!(receive_only.status, StatusCode::OK, "{}", receive_only.json);
    assert_eq!(receive_only.json["tracking_domain"], Value::Null);
    let mailbox_path = format!("/v1/connections/{}", mailbox.json["id"].as_str().unwrap());
    let saved = app.get(&mailbox_path).bearer(&acme.key).send().await;
    assert_eq!(saved.json["identities"][0]["enabled"], false);
    // Restore only the DNS fixture so the next rejection exercises the receive-only
    // permission rather than the deliberately pending DNS recheck after editing.
    sqlx::query("UPDATE sending_domains SET status = 'verified' WHERE workspace_id = $1 AND hostname = 'acme.example'")
        .bind(acme.id.uuid()).execute(test.system.pool()).await.unwrap();
    let enabled = app.patch(&mailbox_path).bearer(&acme.key).json(json!({"identities":[{"id":saved.json["identities"][0]["id"],"email":"hello@acme.example","enabled":true}]})).send().await;
    assert_eq!(enabled.status, StatusCode::UNPROCESSABLE_ENTITY);
    let send_only = app
        .patch(&path)
        .bearer(&acme.key)
        .json(json!({"purpose":"send"}))
        .send()
        .await;
    assert_eq!(send_only.status, StatusCode::OK, "{}", send_only.json);
    let stopped = app.get(&mailbox_path).bearer(&acme.key).send().await;
    assert_eq!(stopped.json["imap"], Value::Null);
    assert_eq!(stopped.json["receiving"]["folders"][0]["enabled"], false);
    let conflicting = app
        .patch(&path)
        .bearer(&acme.key)
        .json(json!({"purpose":"tracking"}))
        .send()
        .await;
    assert_eq!(conflicting.status, StatusCode::CONFLICT);
    let same_hostname = app
        .patch(&path)
        .bearer(&acme.key)
        .json(json!({"tracking_hostname":"acme.example"}))
        .send()
        .await;
    assert_eq!(same_hostname.status, StatusCode::UNPROCESSABLE_ENTITY);
    let retained = app
        .get(&format!(
            "/v1/sending_domains/{}",
            domain.json["tracking_domain"]["id"].as_str().unwrap()
        ))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        retained.status,
        StatusCode::OK,
        "detaching tracking preserves its resource"
    );
    // Domain deletion revokes an already provisioned identity and receive route,
    // even when the managed MTA still retains its sealed account credential.
    sqlx::query("UPDATE sender_identities SET enabled = true WHERE workspace_id = $1 AND connection_id = $2")
        .bind(acme.id.uuid()).bind(mailbox.json["id"].as_str().unwrap().parse::<Id<crate::domain::ids::Connection>>().unwrap().uuid())
        .execute(test.system.pool()).await.unwrap();
    sqlx::query(
        "UPDATE receive_bindings SET enabled = true WHERE workspace_id = $1 AND connection_id = $2",
    )
    .bind(acme.id.uuid())
    .bind(
        mailbox.json["id"]
            .as_str()
            .unwrap()
            .parse::<Id<crate::domain::ids::Connection>>()
            .unwrap()
            .uuid(),
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    let removed = app.delete(&path).bearer(&acme.key).send().await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    let revoked = app.get(&mailbox_path).bearer(&acme.key).send().await;
    assert_eq!(revoked.json["identities"][0]["enabled"], false);
    assert_eq!(revoked.json["receiving"]["folders"][0]["enabled"], false);
}

/// Certificate permission needs fresh ownership and CNAME proof for a tracking use.
/// An authenticated tenant cannot inspect the row through the routing lookup.
#[tokio::test]
async fn tracking_certificate_permission_refuses_unproven_stale_and_mail_domains() {
    let test = TestDb::new().await;
    let workspace = test.workspace("tracking").await;
    let other = test.workspace("other").await;
    let app = test.app();
    let created = app
        .post("/v1/sending_domains")
        .bearer(&workspace.key)
        .idempotency(&key())
        .json(json!({"hostname":"links.acme.example","purpose":"tracking"}))
        .send()
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.json["id"]
        .as_str()
        .unwrap()
        .parse::<Id<crate::domain::ids::SendingDomain>>()
        .unwrap();
    let permission = "/internal/tracking-domains/allow?domain=links.acme.example";
    assert_eq!(
        app.get(permission).send().await.status,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE sending_domains SET status = 'pending_certificate', verified_at = now(), checked_at = now(), dns_checks = '{\"ownership\":true,\"tracking\":true}'::jsonb WHERE workspace_id = $1 AND id = $2")
        .bind(workspace.id.uuid()).bind(id.uuid()).execute(test.system.pool()).await.unwrap();
    assert_eq!(app.get(permission).send().await.status, StatusCode::OK);
    let proof = app
        .get("/.well-known/norbelys-tracking")
        .header("host", "links.acme.example")
        .send()
        .await;
    assert_eq!(proof.status, StatusCode::OK);
    assert_eq!(proof.header("cache-control"), Some("no-store"));
    let stranger = app
        .get(&format!("/v1/sending_domains/{id}"))
        .bearer(&other.key)
        .send()
        .await;
    assert_eq!(stranger.status, StatusCode::NOT_FOUND);
    for statement in [
        "UPDATE sending_domains SET checked_at = now() - interval '49 hours' WHERE workspace_id = $1 AND id = $2",
        "UPDATE sending_domains SET checked_at = now(), status = 'suspended' WHERE workspace_id = $1 AND id = $2",
        "UPDATE sending_domains SET status = 'verified', purpose = 'send' WHERE workspace_id = $1 AND id = $2",
    ] {
        sqlx::query(statement)
            .bind(workspace.id.uuid())
            .bind(id.uuid())
            .execute(test.system.pool())
            .await
            .unwrap();
        assert_eq!(
            app.get(permission).send().await.status,
            StatusCode::FORBIDDEN
        );
    }
}

/// The first field error's pointer of a `validation_failed` problem.
fn pointer(reply: &Reply) -> &str {
    reply.json["errors"][0]["pointer"]
        .as_str()
        .unwrap_or_default()
}

/// The job kinds queued in a workspace, with their unique keys, oldest first.
async fn queued(test: &TestDb, workspace: &TestWorkspace) -> Vec<(String, Option<String>)> {
    sqlx::query!(
        "SELECT kind, unique_key FROM jobs WHERE workspace_id = $1 AND state = 'available' ORDER BY id",
        workspace.id.uuid(),
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|row| (row.kind, row.unique_key))
    .collect()
}

/// The `connection.health_changed` events recorded for a connection, oldest first: their
/// status and whether it was paused.
async fn health_events(test: &TestDb, connection: &str) -> Vec<(String, bool)> {
    sqlx::query!(
        "SELECT payload FROM outbox_events WHERE type = 'connection.health_changed' AND payload -> 'data' ->> 'connection_id' = $1 ORDER BY id",
        connection,
    )
    .fetch_all(test.system.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        (
            row.payload["data"]["status"].as_str().unwrap_or_default().to_owned(),
            row.payload["data"]["paused"].as_bool().unwrap_or_default(),
        )
    })
    .collect()
}

/// A runner of the Sending kinds with the worker's environment over `settings`, reaching
/// loopback fakes, and the managed MTA's `control` API when given.
fn harness(test: &TestDb, settings: Settings, control: Option<Control>) -> Harness {
    let mut registry = Registry::default();
    registry
        .register::<ConnectionCheck>()
        .unwrap()
        .register::<ConnectionCheckDue>()
        .unwrap()
        .register::<super::domains::DomainPrepare>()
        .unwrap()
        .register::<DomainVerify>()
        .unwrap()
        .register::<super::domains::DomainCertificate>()
        .unwrap()
        .register::<NorbelysProvision>()
        .unwrap();
    let mut env = http::Extensions::new();
    env.insert(testing::keys());
    // The resolver has no name servers: the fakes are reached by IP literal, which resolves
    // without a query.
    env.insert(
        Env::assemble(
            settings,
            crate::dns::Resolver::offline(),
            AddressPolicy::Any,
            control,
        )
        .unwrap(),
    );
    Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        env,
        "worker-test",
    )
}

/// Runs the maintenance jobs due in one lane once; returns their outcomes.
async fn run_maintenance(runner: &Harness) -> Vec<&'static str> {
    runner
        .run_once(Queue::Maintenance, 1)
        .await
        .into_iter()
        .map(|(_, outcome)| outcome)
        .collect()
}

/// A fake SMTP server: it greets, offers `AUTH PLAIN LOGIN`, answers `AUTH` with
/// `answer` (after letting the test act first when a gate is set), and every other command with
/// `250`.
struct FakeSmtp {
    port: u16,
    gate: Arc<Mutex<Option<Gate>>>,
}

/// A hold on the next `AUTH`: the server says it was reached, then waits to be released.
struct Gate {
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

impl FakeSmtp {
    async fn start(answer: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let answer = answer.to_owned();
        let gate: Arc<Mutex<Option<Gate>>> = Arc::default();
        let shared_gate = Arc::clone(&gate);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (answer, gate) = (answer.clone(), Arc::clone(&shared_gate));
                tokio::spawn(async move {
                    let (read, mut write) = socket.into_split();
                    let mut lines = BufReader::new(read).lines();
                    let _ = write.write_all(b"220 fake.test ESMTP\r\n").await;
                    while let Ok(Some(line)) = lines.next_line().await {
                        let upper = line.to_ascii_uppercase();
                        let reply = if upper.starts_with("EHLO") {
                            "250-fake.test\r\n250 AUTH PLAIN LOGIN".to_owned()
                        } else if upper.starts_with("AUTH") {
                            let held = gate.lock().unwrap().take();
                            if let Some(held) = held {
                                let _ = held.reached.send(());
                                let _ = held.release.await;
                            }
                            answer.clone()
                        } else if upper.starts_with("QUIT") {
                            "221 2.0.0 Bye".to_owned()
                        } else {
                            "250 2.0.0 OK".to_owned()
                        };
                        if write
                            .write_all(format!("{reply}\r\n").as_bytes())
                            .await
                            .is_err()
                            || upper.starts_with("QUIT")
                        {
                            return;
                        }
                    }
                });
            }
        });
        Self { port, gate }
    }

    /// Holds the next `AUTH`: returns the signal that it arrived and the release.
    fn hold(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, reached_rx) = oneshot::channel();
        let (release_tx, release) = oneshot::channel();
        *self.gate.lock().unwrap() = Some(Gate { reached, release });
        (reached_rx, release_tx)
    }
}

/// What the fake OAuth and Gmail server answers: the subject and the address of the account
/// that consents, and the nonce its ID token answers.
#[derive(Clone, Default)]
struct Account {
    subject: String,
    email: String,
    nonce: String,
}

/// A fake Google: `POST /token` answers tokens with an unsigned ID token for the configured
/// account, `GET /gmail/v1/users/me/profile` its address.
async fn fake_google(account: Arc<Mutex<Account>>) -> String {
    let token_account = Arc::clone(&account);
    let app = axum::Router::new()
        .route(
            "/token",
            axum::routing::post(move || {
                let account = token_account.lock().unwrap().clone();
                async move {
                    let id_token = oauth::unsigned_id_token(&json!({
                        "iss": "https://accounts.google.com", "aud": "client-id", "sub": account.subject,
                        "nonce": account.nonce, "exp": jiff::Timestamp::now().as_second() + 600,
                    }));
                    axum::Json(json!({
                        "access_token": "access", "refresh_token": "refresh", "expires_in": 3600, "token_type": "Bearer",
                        "scope": "openid https://www.googleapis.com/auth/gmail.send https://www.googleapis.com/auth/gmail.readonly",
                        "id_token": id_token,
                    }))
                }
            }),
        )
        .route(
            "/gmail/v1/users/me/profile",
            axum::routing::get(move || {
                let email = account.lock().unwrap().email.clone();
                async move { axum::Json(json!({ "emailAddress": email, "historyId": "1" })) }
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    origin
}

/// The Sending settings of a test whose OAuth calls reach `origin`.
fn oauth_settings(origin: &str) -> Settings {
    Settings {
        apps: oauth::Apps::for_tests(norbelys_mail::oauth::App {
            client_id: "client-id".to_owned(),
            client_secret: secrecy::SecretString::from("client-secret"),
            redirect_uri: url::Url::parse("https://app.norbelys.test/api/v1/auth/callback")
                .unwrap(),
        }),
        http: HttpClient::rebased(origin).unwrap(),
        ..Settings::for_tests()
    }
}

/// A query parameter of a URL.
fn parameter(url: &str, name: &str) -> String {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

/// The ceremony cookie a reply sets, as a `Cookie` header value.
fn ceremony_cookie(reply: &Reply) -> String {
    reply
        .header("set-cookie")
        .and_then(|cookie| cookie.split(';').next())
        .unwrap()
        .to_owned()
}

/// A member of `workspace` (not an owner or admin) and an API key of theirs.
async fn member_key(test: &TestDb, workspace: &TestWorkspace) -> String {
    let mut tx = test.system.begin().await.unwrap();
    let user = sqlx::query_scalar!(
        r#"INSERT INTO users (email, email_verified_at) VALUES ('member@acme.example', now()) RETURNING id AS "id: Id<User>""#
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO memberships (workspace_id, user_id, role) VALUES ($1, $2, 'member')",
        workspace.id.uuid(),
        user.uuid(),
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let (_, key) = api_keys::create(
        &mut tx,
        workspace.id,
        user,
        "Member",
        ScopeSet::all(),
        KeyMode::Live,
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    key.secret
}

// ───────────────────────────── connections ─────────────────────────────

/// Creating an SMTP login answers `201` with the whole connection in `verifying`: the account
/// and its own address as the default identity, the inbox read over IMAP, the default pacing (a
/// mailbox sends cold mail every 10 minutes), the SMTP settings without the password, and a
/// `connection.check` queued to prove the credential; retrieving it shows the same object.
#[tokio::test]
async fn creating_a_login_answers_the_connection_and_queues_its_check() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let mut body = smtp_login("Ada@Example.com", "smtp.example.com", 587, "starttls");
    body["imap"] = json!({ "host": "imap.example.com", "port": 993, "security": "tls" });
    let created = connect(&app, &acme.key, body).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    let connection = &created.json;
    let id = connection["id"].as_str().unwrap();
    assert!(id.starts_with("con_"));
    assert_eq!(connection["status"], "verifying");
    assert_eq!(connection["transport"], "smtp");
    assert_eq!(connection["account"]["email"], "Ada@Example.com");
    assert_eq!(connection["smtp"]["username"], "Ada@Example.com");
    assert!(connection["smtp"].get("password").is_none());
    assert_eq!(connection["identities"][0]["email"], "Ada@Example.com");
    assert_eq!(connection["identities"][0]["verified"], false);
    assert_eq!(connection["receiving"]["folders"][0]["folder"], "INBOX");
    assert_eq!(connection["send_interval_minutes"], 10);
    assert_eq!(connection["daily_limit"], 50);
    assert_eq!(connection["webhook"], Value::Null);
    assert_eq!(connection["usage"]["today"]["used"], 0);
    assert_eq!(
        queued(&test, &acme).await,
        [("connection.check".to_owned(), Some(id.to_owned()))]
    );
    let retrieved = app
        .get(&format!("/v1/connections/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(retrieved.status, StatusCode::OK);
    assert_eq!(&retrieved.json, connection);
}

/// A list pages by signed cursor (newest first, every row once) and filters by provider,
/// status, an identity's tag and the account's prefix, each filter bound into its cursor.
#[tokio::test]
async fn connections_list_in_pages_and_filter() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    for account in ["ada@example.com", "bob@example.com", "carl@example.org"] {
        let mut body = smtp_login(account, "smtp.example.com", 465, "tls");
        body["identities"] = json!([{ "email": account, "tags": [if account.ends_with(".org") { "org" } else { "com" }] }]);
        assert_eq!(
            connect(&app, &acme.key, body).await.status,
            StatusCode::CREATED
        );
    }
    let first = app
        .get("/v1/connections?limit=2")
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.json["data"].as_array().unwrap().len(), 2);
    assert_eq!(first.json["meta"]["has_more"], true);
    assert_eq!(
        first.json["data"][0]["account"]["email"],
        "carl@example.org"
    );
    let cursor = first.json["meta"]["next_cursor"].as_str().unwrap();
    let second = app
        .get(&format!("/v1/connections?limit=2&cursor={cursor}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(second.json["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        second.json["data"][0]["account"]["email"],
        "ada@example.com"
    );
    assert_eq!(second.json["meta"]["has_more"], false);

    for (query, expected) in [
        ("tag=org", vec!["carl@example.org"]),
        ("q=B", vec!["bob@example.com"]),
        ("provider=ses", vec![]),
        (
            "status=verifying&include=total_count",
            vec!["carl@example.org", "bob@example.com", "ada@example.com"],
        ),
    ] {
        let page = app
            .get(&format!("/v1/connections?{query}"))
            .bearer(&acme.key)
            .send()
            .await;
        let accounts: Vec<&str> = page.json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["account"]["email"].as_str().unwrap())
            .collect();
        assert_eq!(accounts, expected, "{query}");
    }
    let filtered = app
        .get("/v1/connections?tag=org&limit=1")
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(filtered.json["meta"]["has_more"], false);
    let foreign = app
        .get(&format!("/v1/connections?tag=com&cursor={cursor}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        foreign.status,
        StatusCode::BAD_REQUEST,
        "a cursor is bound to its filters"
    );
}

/// Requests that break a provider's rules are refused with the field that breaks them, before
/// anything is written: an interval out of range or on a rate-paced relay, an SES connection
/// without its quota scope or configuration set, a relay without its SMTP login, an IMAP
/// endpoint on a relay, a send window crossing midnight, a time zone that is not IANA's, and
/// fields an OAuth mailbox takes from its provider.
#[tokio::test]
async fn provider_rules_are_refused_with_their_field() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let relay = |provider: &str| {
        json!({
            "provider": provider, "account_email": "sales account",
            "smtp": { "host": "smtp.relay.example", "port": 587, "security": "starttls", "username": "apikey", "password": "key" },
        })
    };
    let mut out_of_range = smtp_login("ada@example.com", "smtp.example.com", 587, "starttls");
    out_of_range["send_interval_minutes"] = json!(4);
    let mut paced_relay = relay("sendgrid");
    paced_relay["send_interval_minutes"] = json!(10);
    let mut no_set = relay("ses");
    no_set["smtp"]["configuration_set"] = Value::Null;
    let mut no_login = relay("mailgun");
    no_login["smtp"]["username"] = Value::Null;
    let mut relay_imap = relay("mailgun");
    relay_imap["imap"] = json!({ "host": "imap.example.com", "port": 993, "security": "tls" });
    let mut overnight = smtp_login("ada@example.com", "smtp.example.com", 587, "starttls");
    overnight["send_window"] = json!({ "days": [1, 2], "start": "22:00", "end": "06:00" });
    let mut zone = smtp_login("ada@example.com", "smtp.example.com", 587, "starttls");
    zone["timezone"] = json!("Mars/Olympus");
    let google_with_smtp = json!({ "provider": "google", "account_email": "ada@example.com" });
    for (body, field) in [
        (out_of_range, "/send_interval_minutes"),
        (paced_relay, "/send_interval_minutes"),
        (relay("ses"), "/smtp/configuration_set"),
        (no_set, "/smtp/configuration_set"),
        (no_login, "/smtp/username"),
        (relay_imap, "/imap"),
        (overnight, "/send_window"),
        (zone, "/timezone"),
        (google_with_smtp, "/account_email"),
    ] {
        let reply = connect(&app, &acme.key, body.clone()).await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {}",
            reply.json
        );
        assert_eq!(pointer(&reply), field, "{body}");
    }
    let mut ses = relay("ses");
    ses["smtp"]["configuration_set"] = json!("norbelys");
    let reply = connect(&app, &acme.key, ses).await;
    assert_eq!(
        pointer(&reply),
        "/quota_scope_id",
        "an SES connection names its scope"
    );
    assert_eq!(queued(&test, &acme).await, [], "nothing was written");
}

/// Within a workspace an account is one live connection and an address one live paced sender,
/// whatever the way in; the refusal names the live connection. A rate-paced relay account with
/// another name is no paced sender and is accepted.
#[tokio::test]
async fn an_account_is_one_live_connection_and_an_address_one_paced_sender() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let first = connect(
        &app,
        &acme.key,
        smtp_login("ada@example.com", "smtp.example.com", 587, "starttls"),
    )
    .await;
    let live = first.json["id"].as_str().unwrap().to_owned();
    let again = connect(
        &app,
        &acme.key,
        smtp_login("ADA@example.com", "smtp.other.example", 465, "tls"),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert!(
        again.json["detail"].as_str().unwrap().contains(&live),
        "{}",
        again.json
    );

    let scope = app
        .post("/v1/quota_scopes")
        .bearer(&acme.key)
        .idempotency(&key())
        .json(json!({ "provider": "ses", "scope_key": "123456789012:eu-west-1" }))
        .send()
        .await;
    let scope = scope.json["id"].as_str().unwrap().to_owned();
    let ses = |account: &str, interval: Value| {
        json!({
            "provider": "ses", "account_email": account, "quota_scope_id": scope, "send_interval_minutes": interval,
            "smtp": { "host": "email-smtp.eu-west-1.amazonaws.com", "port": 587, "security": "starttls",
                      "username": "AKIA", "password": "smtp-password", "configuration_set": "norbelys" },
        })
    };
    let paced = connect(&app, &acme.key, ses("ada@example.com", json!(10))).await;
    assert_eq!(paced.status, StatusCode::CONFLICT, "{}", paced.json);
    assert!(paced.json["detail"].as_str().unwrap().contains(&live));
    let rate_paced = connect(&app, &acme.key, ses("Acme transactional", Value::Null)).await;
    assert_eq!(
        rate_paced.status,
        StatusCode::CREATED,
        "{}",
        rate_paced.json
    );
    assert_eq!(rate_paced.json["send_interval_minutes"], Value::Null);
    assert_eq!(
        rate_paced.json["identities"],
        json!([]),
        "a name is no From address"
    );
}

/// Archiving keeps the row and its history: the answer is the archived connection, its
/// credential is erased, its folders are no longer read, and customers hear of it. Connecting
/// the same account again restores the **same** connection, its id and its identities, and
/// verifies it again; archiving twice changes nothing more.
#[tokio::test]
async fn archiving_and_reconnecting_restores_the_same_connection() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let mut body = smtp_login("ada@example.com", "smtp.example.com", 587, "starttls");
    body["imap"] = json!({ "host": "imap.example.com", "port": 993, "security": "tls" });
    body["identities"] = json!([{ "email": "ada@example.com", "name": "Ada" }, { "email": "sales@example.com", "tags": ["sales"] }]);
    let created = connect(&app, &acme.key, body.clone()).await;
    let id = created.json["id"].as_str().unwrap().to_owned();
    let identities = identity_ids(&created.json);

    let archived = app
        .delete(&format!("/v1/connections/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(archived.status, StatusCode::OK);
    assert_eq!(archived.json["status"], "archived");
    assert_eq!(archived.json["receiving"]["folders"][0]["enabled"], false);
    let credential = sqlx::query_scalar!(
        r#"SELECT credential IS NULL AS "erased!" FROM connections WHERE id = $1"#,
        Uuid::parse_str(id.trim_start_matches("con_")).unwrap(),
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    assert!(credential);
    let twice = app
        .delete(&format!("/v1/connections/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(twice.json["status"], "archived");
    assert_eq!(
        health_events(&test, &id).await,
        [("archived".to_owned(), false)]
    );
    let refused = app
        .patch(&format!("/v1/connections/{id}"))
        .bearer(&acme.key)
        .json(json!({ "paused": true }))
        .send()
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.json["code"], "invalid_state");

    body["identities"] = json!([]);
    let restored = connect(&app, &acme.key, body).await;
    assert_eq!(restored.status, StatusCode::CREATED, "{}", restored.json);
    assert_eq!(restored.json["id"], id.as_str());
    assert_eq!(restored.json["status"], "verifying");
    assert_eq!(identity_ids(&restored.json), identities);
    assert_eq!(restored.json["receiving"]["folders"][0]["enabled"], true);
    assert_eq!(
        health_events(&test, &id).await,
        [
            ("archived".to_owned(), false),
            ("verifying".to_owned(), false)
        ]
    );
}

/// An archived connection's identities stay for its history without holding their addresses: a
/// different account may then send as an archived mailbox's address, with its own row and its
/// own identity (here a relay). Restoring the mailbox while that live identity holds its address
/// is refused with `409 conflict` naming the live connection, and archiving that connection first
/// lets the mailbox come back with its identities.
#[tokio::test]
async fn an_archived_connections_addresses_are_free_until_it_is_restored() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let mailbox = smtp_login("ada@example.com", "smtp.example.com", 587, "starttls");
    let archived = connect(&app, &acme.key, mailbox.clone()).await;
    let archived_id = archived.json["id"].as_str().unwrap().to_owned();
    app.delete(&format!("/v1/connections/{archived_id}"))
        .bearer(&acme.key)
        .send()
        .await;

    let relay = connect(
        &app,
        &acme.key,
        json!({
            "provider": "sendgrid", "account_email": "acme sendgrid",
            "smtp": { "host": "smtp.sendgrid.net", "port": 587, "security": "starttls", "username": "apikey", "password": "key" },
            "identities": [{ "email": "Ada@example.com" }],
        }),
    )
    .await;
    assert_eq!(relay.status, StatusCode::CREATED, "{}", relay.json);
    let live_id = relay.json["id"].as_str().unwrap().to_owned();
    assert_ne!(live_id, archived_id);
    assert_eq!(relay.json["identities"][0]["email"], "Ada@example.com");
    assert_ne!(
        relay.json["identities"][0]["id"], archived.json["identities"][0]["id"],
        "a new identity"
    );

    let refused = connect(&app, &acme.key, mailbox.clone()).await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.json["code"], "conflict");
    assert!(
        refused.json["detail"].as_str().unwrap().contains(&live_id),
        "{}",
        refused.json
    );

    app.delete(&format!("/v1/connections/{live_id}"))
        .bearer(&acme.key)
        .send()
        .await;
    let restored = connect(&app, &acme.key, mailbox).await;
    assert_eq!(restored.status, StatusCode::CREATED, "{}", restored.json);
    assert_eq!(restored.json["id"], archived_id.as_str());
    assert_eq!(identity_ids(&restored.json), identity_ids(&archived.json));
}

/// A change applies what it names: a pause is told to customers (and a resume too), a new
/// interval is rounded up to whole slots, a send window and a time zone are stored, the
/// identities are replaced whole (one given by id keeps its id), and a new password sends the
/// connection back to `verifying` with a check queued.
#[tokio::test]
async fn updating_a_connection_applies_each_change() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let created = connect(
        &app,
        &acme.key,
        smtp_login("ada@example.com", "smtp.example.com", 587, "starttls"),
    )
    .await;
    let id = created.json["id"].as_str().unwrap().to_owned();
    let ada = created.json["identities"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("/v1/connections/{id}");
    let patch = |body: Value| app.patch(&path).bearer(&acme.key).json(body).send();

    let paused = patch(json!({ "paused": true, "send_interval_minutes": 17, "timezone": "Europe/Madrid",
                               "send_window": { "days": [5, 1], "start": "09:00", "end": "17:30" } })).await;
    assert_eq!(paused.status, StatusCode::OK, "{}", paused.json);
    assert_eq!(paused.json["paused"], true);
    assert_eq!(paused.json["send_interval_minutes"], 20);
    assert_eq!(paused.json["timezone"], "Europe/Madrid");
    assert_eq!(
        paused.json["send_window"],
        json!({ "days": [1, 5], "start": "09:00", "end": "17:30" })
    );
    let resumed = patch(json!({ "paused": false, "send_window": null })).await;
    assert_eq!(resumed.json["send_window"], Value::Null);
    assert_eq!(
        health_events(&test, &id).await,
        [
            ("verifying".to_owned(), true),
            ("verifying".to_owned(), false)
        ]
    );

    let replaced = patch(json!({ "identities": [
        { "id": ada, "email": "ada@example.com", "name": "Ada Lovelace" },
        { "email": "hello@example.com" },
    ] }))
    .await;
    assert_eq!(replaced.status, StatusCode::OK, "{}", replaced.json);
    let identities = replaced.json["identities"].as_array().unwrap();
    assert_eq!(identities.len(), 2);
    assert_eq!(identities[0]["id"], ada.as_str());
    assert_eq!(identities[0]["name"], "Ada Lovelace");
    assert_eq!(identities[1]["email"], "hello@example.com");
    let narrowed = patch(json!({ "identities": [{ "email": "solo@example.com" }] })).await;
    assert_eq!(narrowed.json["identities"].as_array().unwrap().len(), 1);

    sqlx::query!(
        "UPDATE connections SET status = 'active' WHERE id = $1",
        Uuid::parse_str(id.trim_start_matches("con_")).unwrap()
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query!("DELETE FROM jobs")
        .execute(test.system.pool())
        .await
        .unwrap();
    let rekeyed = patch(json!({ "smtp": { "password": "new-app-password" } })).await;
    assert_eq!(rekeyed.json["status"], "verifying");
    assert_eq!(
        queued(&test, &acme).await,
        [("connection.check".to_owned(), Some(id.clone()))]
    );
}

/// Replacing a connection's identities writes only rows of another table, yet it moves the
/// connection's version, which the identities are part of: the answer carries a new `ETag`, and
/// an update still holding the version read before is refused with `412` instead of undoing the
/// replacement.
#[tokio::test]
async fn replacing_identities_moves_the_connections_version() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let created = connect(
        &app,
        &acme.key,
        smtp_login("ada@example.com", "smtp.example.com", 587, "starttls"),
    )
    .await;
    let read = created.header("etag").unwrap().to_owned();
    assert_eq!(read, format!("\"{}\"", created.json["version"]));
    let ada = created.json["identities"][0]["id"].clone();
    let path = format!("/v1/connections/{}", created.json["id"].as_str().unwrap());
    let replace = |identities: Value| {
        app.patch(&path)
            .bearer(&acme.key)
            .header("if-match", &read)
            .json(json!({ "identities": identities }))
            .send()
    };

    let replaced = replace(json!([
        { "id": ada, "email": "ada@example.com" },
        { "email": "hello@example.com" },
    ]))
    .await;
    assert_eq!(replaced.status, StatusCode::OK, "{}", replaced.json);
    let current = replaced.header("etag").unwrap();
    assert_eq!(current, format!("\"{}\"", replaced.json["version"]));
    assert_ne!(current, read);

    let undoing = replace(json!([{ "id": ada, "email": "ada@example.com" }])).await;
    assert_eq!(undoing.status, StatusCode::PRECONDITION_FAILED);
    let kept = app.get(&path).bearer(&acme.key).send().await;
    assert_eq!(identity_ids(&kept.json).len(), 2);
}

/// `verify` sends a connection back to `verifying` with its check queued, coalescing with one
/// already queued; an archived connection cannot be verified.
#[tokio::test]
async fn verifying_queues_one_check_and_refuses_an_archived_connection() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let created = connect(
        &app,
        &acme.key,
        smtp_login("ada@example.com", "smtp.example.com", 587, "starttls"),
    )
    .await;
    let id = created.json["id"].as_str().unwrap().to_owned();
    let verify = || {
        app.post(&format!("/v1/connections/{id}/verify"))
            .bearer(&acme.key)
            .idempotency(&key())
            .send()
    };
    let verified = verify().await;
    assert_eq!(verified.status, StatusCode::OK, "{}", verified.json);
    assert_eq!(verified.json["status"], "verifying");
    assert_eq!(queued(&test, &acme).await.len(), 1, "the check coalesces");
    app.delete(&format!("/v1/connections/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    let refused = verify().await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.json["code"], "invalid_state");
}

/// A relay connection is issued its provider webhook: the URL to configure at the provider,
/// with its verification key missing until the customer pastes it (checked by the verifier that
/// will use it). The managed MTA's webhook gets a secret Norbelys generates, its SMTP settings
/// come from the deployment, and its provisioning is queued instead of a check.
#[tokio::test]
async fn relays_are_issued_their_provider_webhook() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let mailgun = connect(&app, &acme.key, json!({
        "provider": "mailgun", "account_email": "acme mailgun",
        "smtp": { "host": "smtp.mailgun.org", "port": 587, "security": "starttls", "username": "postmaster@mg.example.com", "password": "key" },
    })).await;
    assert_eq!(mailgun.status, StatusCode::CREATED, "{}", mailgun.json);
    let webhook = &mailgun.json["webhook"];
    let webhook_id = webhook["id"].as_str().unwrap();
    assert_eq!(
        webhook["url"],
        format!("https://hooks.norbelys.test/webhooks/{webhook_id}")
    );
    assert_eq!(webhook["key_set"], false);
    let path = format!("/v1/connections/{}", mailgun.json["id"].as_str().unwrap());
    let keyed = app
        .patch(&path)
        .bearer(&acme.key)
        .json(json!({ "webhook": { "key": "mailgun-signing-key" } }))
        .send()
        .await;
    assert_eq!(keyed.json["webhook"]["key_set"], true);
    assert_eq!(
        keyed.json["webhook"]["id"], webhook_id,
        "the URL never changes"
    );

    verified_mail_domain(&test, &acme, "acme.example").await;
    let norbelys = connect(
        &app,
        &acme.key,
        json!({ "provider": "norbelys", "account_email": "hello@acme.example" }),
    )
    .await;
    assert_eq!(norbelys.status, StatusCode::CREATED, "{}", norbelys.json);
    assert_eq!(norbelys.json["webhook"]["key_set"], true);
    assert_eq!(norbelys.json["smtp"]["host"], "smtp.norbelys.test");
    assert_eq!(norbelys.json["send_interval_minutes"], Value::Null);
    let kinds: Vec<String> = queued(&test, &acme)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(kinds, ["connection.check", "provider.norbelys.provision"]);
    let wrong = app
        .patch(&format!(
            "/v1/connections/{}",
            norbelys.json["id"].as_str().unwrap()
        ))
        .bearer(&acme.key)
        .json(json!({ "webhook": { "key": "x" } }))
        .send()
        .await;
    assert_eq!(
        pointer(&wrong),
        "/webhook/key",
        "the managed MTA's secret is generated"
    );
}

/// A member manages only the connections they created; an owner manages every one. Reading is
/// open to every member.
#[tokio::test]
async fn a_member_manages_only_their_own_connections() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let member = member_key(&test, &acme).await;
    let app = test.app();
    let owners = connect(
        &app,
        &acme.key,
        smtp_login("ada@example.com", "smtp.example.com", 587, "starttls"),
    )
    .await;
    let mine = connect(
        &app,
        &member,
        smtp_login("bob@example.com", "smtp.example.com", 587, "starttls"),
    )
    .await;
    assert_eq!(mine.status, StatusCode::CREATED);
    let pause = |id: &Value, credential: &str| {
        app.patch(&format!("/v1/connections/{}", id.as_str().unwrap()))
            .bearer(credential)
            .json(json!({ "paused": true }))
            .send()
    };
    assert_eq!(
        pause(&owners.json["id"], &member).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        pause(&mine.json["id"], &member).await.status,
        StatusCode::OK
    );
    assert_eq!(
        pause(&mine.json["id"], &acme.key).await.status,
        StatusCode::OK
    );
    let read = app
        .get(&format!(
            "/v1/connections/{}",
            owners.json["id"].as_str().unwrap()
        ))
        .bearer(&member)
        .send()
        .await;
    assert_eq!(read.status, StatusCode::OK);
}

/// Another workspace's ids are indistinguishable from absent ones: reading, changing, archiving
/// or verifying them, and naming another workspace's quota scope, all answer `404`.
#[tokio::test]
async fn another_workspaces_ids_are_not_found() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let globex = test.workspace("globex").await;
    let app = test.app();
    let connection = connect(
        &app,
        &acme.key,
        smtp_login("ada@example.com", "smtp.example.com", 587, "starttls"),
    )
    .await;
    let connection = connection.json["id"].as_str().unwrap().to_owned();
    let scope = app
        .post("/v1/quota_scopes")
        .bearer(&acme.key)
        .idempotency(&key())
        .json(json!({ "provider": "ses", "scope_key": "123456789012:eu-west-1" }))
        .send()
        .await;
    let scope = scope.json["id"].as_str().unwrap().to_owned();
    let domain = app
        .post("/v1/sending_domains")
        .bearer(&acme.key)
        .idempotency(&key())
        .json(json!({ "hostname": "links.acme.example" }))
        .send()
        .await;
    let domain = domain.json["id"].as_str().unwrap().to_owned();
    let replies = [
        app.get(&format!("/v1/connections/{connection}")).bearer(&globex.key).send().await,
        app.patch(&format!("/v1/connections/{connection}")).bearer(&globex.key).json(json!({ "paused": true })).send().await,
        app.delete(&format!("/v1/connections/{connection}")).bearer(&globex.key).send().await,
        app.post(&format!("/v1/connections/{connection}/verify")).bearer(&globex.key).idempotency(&key()).send().await,
        app.get(&format!("/v1/quota_scopes/{scope}")).bearer(&globex.key).send().await,
        app.delete(&format!("/v1/quota_scopes/{scope}")).bearer(&globex.key).send().await,
        app.get(&format!("/v1/sending_domains/{domain}")).bearer(&globex.key).send().await,
        app.post(&format!("/v1/sending_domains/{domain}/verify")).bearer(&globex.key).idempotency(&key()).send().await,
        connect(&app, &globex.key, json!({
            "provider": "ses", "account_email": "globex", "quota_scope_id": scope,
            "smtp": { "host": "email-smtp.eu-west-1.amazonaws.com", "port": 587, "security": "starttls",
                      "username": "AKIA", "password": "p", "configuration_set": "norbelys" },
        })).await,
    ];
    for reply in replies {
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.json);
    }
    let listed = app.get("/v1/connections").bearer(&globex.key).send().await;
    assert_eq!(listed.json["data"], json!([]));
}

// ───────────────────────────── quota scopes ─────────────────────────────

/// A quota scope holds an account's shared limits and, for SES, shows the webhook of its
/// earliest SES connection. It cannot be deleted while SES connections name it, archived ones
/// included, and the refusal names them; a scope no SES connection names is deleted.
#[tokio::test]
async fn a_quota_scope_named_by_ses_connections_is_not_deleted() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let created = app.post("/v1/quota_scopes").bearer(&acme.key).idempotency(&key())
        .json(json!({ "provider": "ses", "scope_key": "123456789012:eu-west-1", "messages_per_day": 50000,
                      "window_limit": 14, "window_unit": "recipients", "window_seconds": 1 })).send().await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    assert_eq!(created.json["webhook"], Value::Null);
    let scope = created.json["id"].as_str().unwrap().to_owned();
    let half_window = app
        .post("/v1/quota_scopes")
        .bearer(&acme.key)
        .idempotency(&key())
        .json(json!({ "provider": "ses", "scope_key": "other", "window_limit": 14 }))
        .send()
        .await;
    assert_eq!(pointer(&half_window), "/window_limit");
    let ses = connect(&app, &acme.key, json!({
        "provider": "ses", "account_email": "acme ses", "quota_scope_id": scope,
        "smtp": { "host": "email-smtp.eu-west-1.amazonaws.com", "port": 587, "security": "starttls",
                  "username": "AKIA", "password": "p", "configuration_set": "norbelys" },
    })).await;
    assert_eq!(ses.status, StatusCode::CREATED, "{}", ses.json);
    let read = app
        .get(&format!("/v1/quota_scopes/{scope}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(read.json["webhook"], ses.json["webhook"]);
    let updated = app
        .patch(&format!("/v1/quota_scopes/{scope}"))
        .bearer(&acme.key)
        .json(json!({ "messages_per_day": 60000 }))
        .send()
        .await;
    assert_eq!(updated.json["messages_per_day"], 60000);
    assert_eq!(updated.json["window_limit"], 14, "an absent limit stays");
    let cleared = app
        .patch(&format!("/v1/quota_scopes/{scope}"))
        .bearer(&acme.key)
        .json(json!({ "window_limit": null, "window_unit": null, "window_seconds": null }))
        .send()
        .await;
    assert_eq!(cleared.json["window_limit"], Value::Null, "null clears it");
    assert_eq!(cleared.json["messages_per_day"], 60000);

    let connection = ses.json["id"].as_str().unwrap();
    app.delete(&format!("/v1/connections/{connection}"))
        .bearer(&acme.key)
        .send()
        .await;
    let refused = app
        .delete(&format!("/v1/quota_scopes/{scope}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.json["code"], "invalid_state");
    assert!(
        refused.json["detail"]
            .as_str()
            .unwrap()
            .contains(connection)
    );

    let relay = app
        .post("/v1/quota_scopes")
        .bearer(&acme.key)
        .idempotency(&key())
        .json(json!({ "provider": "sendgrid", "scope_key": "acme" }))
        .send()
        .await;
    let relay = relay.json["id"].as_str().unwrap();
    let listed = app
        .get("/v1/quota_scopes?provider=sendgrid")
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(listed.json["data"].as_array().unwrap().len(), 1);
    let deleted = app
        .delete(&format!("/v1/quota_scopes/{relay}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.get(&format!("/v1/quota_scopes/{relay}"))
            .bearer(&acme.key)
            .send()
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

// ───────────────────────────── sending domains ─────────────────────────────

/// A sending domain shows the records to publish (the ownership TXT, and the tracking CNAME
/// while tracking is on); `verify` makes it `verifying` and queues `domain.verify`; a hostname
/// that is not one is refused; another workspace may claim the same hostname but not verify it
/// while this one holds it; deleting removes it.
#[tokio::test]
async fn sending_domains_show_their_records_and_are_held_once_verified() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let globex = test.workspace("globex").await;
    let app = test.app();
    let create = |credential: &str, hostname: &str| {
        app.post("/v1/sending_domains")
            .bearer(credential)
            .idempotency(&key())
            .json(json!({ "hostname": hostname, "tracking_enabled": true }))
            .send()
    };
    let created = create(&acme.key, "Links.Acme.Example.").await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    assert_eq!(created.json["hostname"], "links.acme.example");
    assert_eq!(created.json["status"], "pending_verification");
    let records = created.json["records"].as_array().unwrap();
    assert_eq!(records[0]["name"], "_norbelys.links.acme.example");
    assert!(
        records[0]["value"]
            .as_str()
            .unwrap()
            .starts_with("norbelys-verification=")
    );
    assert_eq!(records[0]["status"], "unchecked");
    assert_eq!(records[1]["value"], "tracking.norbelys.test");
    assert_eq!(pointer(&create(&acme.key, "localhost").await), "/hostname");
    assert_eq!(
        create(&acme.key, "links.acme.example").await.status,
        StatusCode::CONFLICT
    );
    let id = created.json["id"].as_str().unwrap().to_owned();

    let untracked = app
        .patch(&format!("/v1/sending_domains/{id}"))
        .bearer(&acme.key)
        .json(json!({ "tracking_enabled": false }))
        .send()
        .await;
    assert_eq!(untracked.json["records"].as_array().unwrap().len(), 1);
    let verified = app
        .post(&format!("/v1/sending_domains/{id}/verify"))
        .bearer(&acme.key)
        .idempotency(&key())
        .send()
        .await;
    assert_eq!(verified.json["status"], "verifying");
    assert_eq!(
        queued(&test, &acme).await,
        [
            ("domain.prepare".to_owned(), Some(id.clone())),
            ("domain.verify".to_owned(), Some(id.clone()))
        ]
    );

    let claimed = create(&globex.key, "links.acme.example").await;
    assert_eq!(
        claimed.status,
        StatusCode::CREATED,
        "a pending claim holds nothing"
    );
    let held = app
        .post(&format!(
            "/v1/sending_domains/{}/verify",
            claimed.json["id"].as_str().unwrap()
        ))
        .bearer(&globex.key)
        .idempotency(&key())
        .send()
        .await;
    assert_eq!(held.status, StatusCode::CONFLICT);

    let listed = app
        .get("/v1/sending_domains?status=verifying")
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(listed.json["data"][0]["id"], id.as_str());
    assert_eq!(
        app.delete(&format!("/v1/sending_domains/{id}"))
            .bearer(&acme.key)
            .send()
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.get(&format!("/v1/sending_domains/{id}"))
            .bearer(&acme.key)
            .send()
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

// ───────────────────────────── connection.check ─────────────────────────────

/// A check moves a connection's health by what its SMTP server answers to `AUTH`: `235` makes
/// it `active` (told to customers), `535` means the credential is lost
/// (`authorization_required`, with the server's words), and `454` is temporary: no status
/// changes, the detail says what happened, and the job is retried on the backoff.
#[tokio::test]
async fn a_check_moves_health_by_the_smtp_servers_answer() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let runner = harness(&test, Settings::for_tests(), None);
    for (answer, status, outcome, detail) in [
        (
            "235 2.7.0 Authentication successful",
            "active",
            "done",
            None,
        ),
        (
            "535 5.7.8 Username and Password not accepted",
            "authorization_required",
            "done",
            Some("refused the login"),
        ),
        (
            "454 4.7.0 Temporary authentication failure",
            "verifying",
            "retry",
            Some("runs again later"),
        ),
    ] {
        let smtp = FakeSmtp::start(answer).await;
        let account = format!("{}@example.com", &answer[..3]);
        let created = connect(
            &app,
            &acme.key,
            smtp_login(&account, "127.0.0.1", smtp.port, "plain"),
        )
        .await;
        let id = created.json["id"].as_str().unwrap().to_owned();
        assert_eq!(run_maintenance(&runner).await, [outcome], "{answer}");
        let checked = app
            .get(&format!("/v1/connections/{id}"))
            .bearer(&acme.key)
            .send()
            .await;
        assert_eq!(checked.json["status"], status, "{answer}: {}", checked.json);
        assert!(checked.json["checked_at"].is_string(), "{answer}");
        match detail {
            Some(words) => assert!(
                checked.json["status_detail"]
                    .as_str()
                    .unwrap()
                    .contains(words),
                "{}",
                checked.json
            ),
            None => assert_eq!(checked.json["status_detail"], Value::Null),
        }
        let told: Vec<String> = health_events(&test, &id)
            .await
            .into_iter()
            .map(|(status, _)| status)
            .collect();
        let expected: Vec<String> = if status == "verifying" {
            Vec::new()
        } else {
            vec![status.to_owned()]
        };
        assert_eq!(told, expected, "{answer}");
    }
}

/// A check is fenced on the credential it read: a password replaced while its `AUTH` is out
/// makes it write nothing (no status, no `checked_at`, no event) and run again at once, and the
/// new run proves the new credential.
#[tokio::test]
async fn a_check_of_a_replaced_credential_writes_nothing_and_runs_again() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let runner = harness(&test, Settings::for_tests(), None);
    let smtp = FakeSmtp::start("235 2.7.0 Authentication successful").await;
    let created = connect(
        &app,
        &acme.key,
        smtp_login("ada@example.com", "127.0.0.1", smtp.port, "plain"),
    )
    .await;
    let id = created.json["id"].as_str().unwrap().to_owned();
    let (reached, release) = smtp.hold();
    let replace = async {
        reached.await.unwrap();
        let replaced = app
            .patch(&format!("/v1/connections/{id}"))
            .bearer(&acme.key)
            .json(json!({ "smtp": { "password": "rotated" } }))
            .send()
            .await;
        assert_eq!(replaced.status, StatusCode::OK, "{}", replaced.json);
        release.send(()).unwrap();
    };
    let (outcomes, ()) = tokio::join!(run_maintenance(&runner), replace);
    assert_eq!(outcomes, ["yield"]);
    let fenced = app
        .get(&format!("/v1/connections/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(fenced.json["status"], "verifying");
    assert_eq!(fenced.json["checked_at"], Value::Null);
    assert_eq!(health_events(&test, &id).await, []);
    assert_eq!(run_maintenance(&runner).await, ["done"]);
    let proven = app
        .get(&format!("/v1/connections/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(proven.json["status"], "active");
}

/// The daily fan-out enqueues a check for each active connection checked a day ago or never,
/// paused ones included, and none for a connection checked recently or in another status.
#[tokio::test]
async fn the_daily_fan_out_checks_active_connections_due() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let mut ids = Vec::new();
    for account in [
        "due@example.com",
        "paused@example.com",
        "fresh@example.com",
        "lost@example.com",
    ] {
        let created = connect(
            &app,
            &acme.key,
            smtp_login(account, "smtp.example.com", 587, "starttls"),
        )
        .await;
        ids.push(
            Uuid::parse_str(
                created.json["id"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("con_"),
            )
            .unwrap(),
        );
    }
    sqlx::query!(
        "UPDATE connections SET status = CASE WHEN account_email LIKE 'lost%' THEN 'authorization_required' ELSE 'active' END,
                paused = account_email LIKE 'paused%',
                checked_at = CASE WHEN account_email LIKE 'fresh%' THEN now() - interval '1 hour'
                                  WHEN account_email LIKE 'due%' THEN now() - interval '25 hours' END"
    )
    .execute(test.system.pool())
    .await
    .unwrap();
    sqlx::query!("DELETE FROM jobs")
        .execute(test.system.pool())
        .await
        .unwrap();
    let mut tx = test
        .worker
        .begin_in(crate::jobs::SYSTEM_WORKSPACE)
        .await
        .unwrap();
    crate::jobs::enqueue(
        &mut tx,
        crate::jobs::SYSTEM_WORKSPACE,
        &ConnectionCheckDue {},
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let runner = harness(&test, Settings::for_tests(), None);
    assert_eq!(run_maintenance(&runner).await, ["done"]);
    let mut checked: Vec<String> = queued(&test, &acme)
        .await
        .into_iter()
        .filter_map(|(_, key)| key)
        .collect();
    checked.sort();
    let mut expected = vec![
        Id::<crate::domain::ids::Connection>::from_uuid(ids[0]).to_string(),
        Id::<crate::domain::ids::Connection>::from_uuid(ids[1]).to_string(),
    ];
    expected.sort();
    assert_eq!(checked, expected);
}

// ───────────────────────────── OAuth ─────────────────────────────

/// The OAuth round trip of a Google mailbox: the start answers the consent URL and binds the
/// ceremony to the browser's cookie, so a callback without that cookie finishes nothing; the
/// callback with it creates the connection from the account the provider proves (the ID token's
/// subject, Gmail's address) and is consumed once. A reconnect keeps the connection, its id and
/// its subject; a reconnect by another account is refused, naming both accounts.
#[tokio::test]
async fn an_oauth_mailbox_connects_and_reconnects_only_as_the_same_account() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let account = Arc::new(Mutex::new(Account::default()));
    let origin = fake_google(Arc::clone(&account)).await;
    let app = test.app_with(oauth_settings(&origin));
    let consent = |path: String, body: Option<Value>| {
        let call = app.post(&path).bearer(&acme.key).idempotency(&key());
        match body {
            Some(body) => call.json(body).send(),
            None => call.send(),
        }
    };
    let started = consent(
        "/v1/connections".to_owned(),
        Some(json!({ "provider": "google", "return_to": "/connections" })),
    )
    .await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{}", started.json);
    let url = started.json["authorization"]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(url.starts_with("https://accounts.google.com/"));
    let cookie = ceremony_cookie(&started);
    *account.lock().unwrap() = Account {
        subject: "108".to_owned(),
        email: "ada@example.com".to_owned(),
        nonce: parameter(&url, "nonce"),
    };
    let callback = |url: &str, cookie: Option<&str>| {
        let call = app.get(&format!(
            "/v1/auth/callback?state={}&code=the-code",
            parameter(url, "state")
        ));
        match cookie {
            Some(cookie) => call.header("cookie", cookie).send(),
            None => call.send(),
        }
    };
    assert_eq!(callback(&url, None).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        callback(&url, Some("__Host-nb_ceremony=another-browser"))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let finished = callback(&url, Some(&cookie)).await;
    assert_eq!(finished.status, StatusCode::SEE_OTHER, "{}", finished.json);
    let location = finished.header("location").unwrap().to_owned();
    assert!(
        location.starts_with("/connections?connection_id=con_"),
        "{location}"
    );
    let id = location
        .trim_start_matches("/connections?connection_id=")
        .to_owned();
    assert_eq!(
        callback(&url, Some(&cookie)).await.status,
        StatusCode::BAD_REQUEST,
        "consumed once"
    );

    let connection = app
        .get(&format!("/v1/connections/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(connection.json["provider"], "google");
    assert_eq!(connection.json["transport"], "api");
    assert_eq!(connection.json["status"], "verifying");
    assert_eq!(
        connection.json["account"],
        json!({ "email": "ada@example.com", "issuer": "https://accounts.google.com", "subject": "108" })
    );
    assert_eq!(connection.json["identities"][0]["verified"], true);
    assert_eq!(
        connection.json["receiving"]["folders"][0]["folder"],
        "INBOX"
    );

    let lose_grant = || {
        sqlx::query!(
            "UPDATE connections SET status = 'authorization_required' WHERE id = $1",
            Uuid::parse_str(id.trim_start_matches("con_")).unwrap()
        )
        .execute(test.system.pool())
    };
    lose_grant().await.unwrap();
    let reconnect = consent(
        format!("/v1/connections/{id}/verify?return_to=/connections/{id}"),
        None,
    )
    .await;
    assert_eq!(reconnect.status, StatusCode::OK, "{}", reconnect.json);
    assert_eq!(reconnect.json["status"], "authorization_required");
    let url = reconnect.json["authorization"]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(parameter(&url, "login_hint"), "ada@example.com");
    *account.lock().unwrap() = Account {
        subject: "108".to_owned(),
        email: "ada.renamed@example.com".to_owned(),
        nonce: parameter(&url, "nonce"),
    };
    let back = callback(&url, Some(&ceremony_cookie(&reconnect))).await;
    assert_eq!(
        back.header("location").unwrap(),
        format!("/connections/{id}?connection_id={id}")
    );
    let reconnected = app
        .get(&format!("/v1/connections/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(reconnected.json["status"], "verifying");
    assert_eq!(reconnected.json["account"]["subject"], "108");
    assert_eq!(
        reconnected.json["account"]["email"], "ada@example.com",
        "the subject is the account"
    );

    lose_grant().await.unwrap();
    let swap = consent(format!("/v1/connections/{id}/verify"), None).await;
    let url = swap.json["authorization"]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    *account.lock().unwrap() = Account {
        subject: "999".to_owned(),
        email: "eve@example.com".to_owned(),
        nonce: parameter(&url, "nonce"),
    };
    let refused = callback(&url, Some(&ceremony_cookie(&swap))).await;
    let location = refused.header("location").unwrap().to_owned();
    assert_eq!(
        parameter(&format!("https://app.test{location}"), "error"),
        "conflict"
    );
    let description = parameter(&format!("https://app.test{location}"), "error_description");
    assert!(
        description.contains("eve@example.com") && description.contains("ada@example.com"),
        "{description}"
    );
    let unchanged = app
        .get(&format!("/v1/connections/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(unchanged.json["status"], "authorization_required");
    assert_eq!(unchanged.json["account"]["subject"], "108");
}

// ───────────────────────────── the managed MTA ─────────────────────────────

/// One verified domain accepts many identities and new HTTP From addresses without creating
/// per-address connections; another workspace cannot borrow that domain's authorization.
#[tokio::test]
async fn managed_domain_senders_share_a_connection_and_remain_workspace_scoped() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let stranger = test.workspace("stranger").await;
    let app = test.app();
    verified_mail_domain(&test, &acme, "acme.example").await;
    let denied = connect(
        &app,
        &stranger.key,
        json!({"provider":"norbelys", "account_email":"acme.example"}),
    )
    .await;
    assert_eq!(denied.status, StatusCode::UNPROCESSABLE_ENTITY);
    let foreign = connect(&app, &acme.key, json!({"provider":"norbelys", "account_email":"acme.example", "identities":[{"email":"sender@foreign.example"}]})).await;
    assert_eq!(foreign.status, StatusCode::UNPROCESSABLE_ENTITY);
    let senders: Vec<Value> = (0..100)
        .map(|n| json!({"email":format!("sender{n}@acme.example")}))
        .collect();
    let connected = connect(
        &app,
        &acme.key,
        json!({"provider":"norbelys", "account_email":"acme.example", "identities":senders}),
    )
    .await;
    assert_eq!(connected.status, StatusCode::CREATED, "{}", connected.json);
    assert_eq!(connected.json["identities"].as_array().unwrap().len(), 100);
    assert!(
        connected.json["identities"]
            .as_array()
            .unwrap()
            .iter()
            .all(|sender| sender["verified"] == true)
    );
    assert!(connected.json["imap"].is_null());
    assert_eq!(connected.json["smtp"]["username"], "acme.example");
    let connection_path = format!("/v1/connections/{}", connected.json["id"].as_str().unwrap());
    for minutes in [3, 5, 6, 7, 8, 9] {
        let paced = app
            .patch(&connection_path)
            .bearer(&acme.key)
            .json(json!({"send_interval_minutes": minutes}))
            .send()
            .await;
        assert_eq!(paced.status, StatusCode::OK, "{}", paced.json);
        assert_eq!(paced.json["send_interval_minutes"], minutes);
    }
    let invalid = app
        .patch(&connection_path)
        .bearer(&acme.key)
        .json(json!({"send_interval_minutes": 0}))
        .send()
        .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);
    let message = app.post("/v1/messages").bearer(&acme.key).idempotency(&key()).json(json!({
        "from":"new@acme.example", "to":["recipient@example.com"], "subject":"Hello", "html":"<p>Hello</p>"
    })).send().await;
    assert_eq!(message.status, StatusCode::ACCEPTED, "{}", message.json);
    let unauthorized = app.post("/v1/messages").bearer(&stranger.key).idempotency(&key()).json(json!({
        "from":"new@acme.example", "to":["recipient@example.com"], "subject":"Hello", "html":"<p>Hello</p>"
    })).send().await;
    assert_eq!(unauthorized.status, StatusCode::NOT_FOUND);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM connections WHERE workspace_id = $1")
        .bind(acme.id.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    let saved = app
        .get(&format!(
            "/v1/connections/{}",
            connected.json["id"].as_str().unwrap()
        ))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(saved.json["identities"].as_array().unwrap().len(), 101);
}

/// What a fake control API received: method, path, headers and body.
type Received = Arc<Mutex<Vec<(String, String, http::HeaderMap, Vec<u8>)>>>;

/// A fake control API of the managed MTA: it knows no login yet, creates a login on
/// `acme.example` with a password and refuses any other domain as unverified, and accepts every
/// route.
async fn fake_control(received: Received) -> String {
    let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let received = Arc::clone(&received);
        async move {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, 65_536).await.unwrap_or_default();
            let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let path = parts.uri.path().to_owned();
            received.lock().unwrap().push((parts.method.to_string(), path.clone(), parts.headers, body.to_vec()));
            let acme = json["username"].as_str().is_some_and(|username| username.ends_with("@acme.example"));
            let (status, answer) = match (parts.method.as_str(), path.as_str()) {
                ("POST", "/v1/accounts") if acme => (StatusCode::CREATED, json!({ "username": json["username"], "password": "mta-password" })),
                ("POST", "/v1/accounts") => (StatusCode::CONFLICT, json!({ "code": "conflict", "detail": "the domain nope.example is not verified yet" })),
                ("PUT", _) => (StatusCode::OK, json!({ "id": path.trim_start_matches("/v1/routes/") })),
                _ => (StatusCode::NOT_FOUND, json!({ "code": "not_found", "detail": "no such account" })),
            };
            (status, axum::Json(answer))
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    origin
}

/// Provisioning a managed MTA login: the job asks the control API whether the login exists,
/// creates it, registers the connection's webhook as its evidence route with the generated
/// secret, signs every request per Standard Webhooks with the installation's secret, seals the
/// password it was answered once, and queues a real credential check before activation. A login whose domain the MTA
/// has not verified fails the connection with the MTA's words.
#[tokio::test]
async fn the_managed_mta_provisions_a_login_and_its_evidence_route() {
    use base64::Engine as _;
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let received: Received = Arc::default();
    let origin = fake_control(Arc::clone(&received)).await;
    let secret = [9_u8; 32];
    let control = Control::new(
        url::Url::parse(&origin).unwrap(),
        &secrecy::SecretString::from(format!(
            "whsec_{}",
            base64::engine::general_purpose::STANDARD.encode(secret)
        )),
        false,
    )
    .unwrap();
    let runner = harness(&test, Settings::for_tests(), Some(control));

    verified_mail_domain(&test, &acme, "acme.example").await;
    let created = connect(
        &app,
        &acme.key,
        json!({ "provider": "norbelys", "account_email": "Hello@acme.example" }),
    )
    .await;
    let id = created.json["id"].as_str().unwrap().to_owned();
    let webhook = created.json["webhook"]["id"].as_str().unwrap().to_owned();
    assert_eq!(run_maintenance(&runner).await, ["done"]);
    let provisioned = app
        .get(&format!("/v1/connections/{id}"))
        .bearer(&acme.key)
        .send()
        .await;
    assert_eq!(
        provisioned.json["status"], "verifying",
        "{}",
        provisioned.json
    );
    let uuid = Uuid::parse_str(id.trim_start_matches("con_")).unwrap();
    let sealed = sqlx::query_scalar!(
        r#"SELECT credential AS "credential!" FROM connections WHERE id = $1"#,
        uuid
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    let super::credentials::Credential::Password(password) =
        super::credentials::open(&testing::keys(), acme.id, Id::from_uuid(uuid), &sealed).unwrap()
    else {
        panic!("a managed MTA login holds a password");
    };
    assert_eq!(
        secrecy::ExposeSecret::expose_secret(&password),
        "mta-password"
    );

    let requests = received.lock().unwrap().clone();
    let calls: Vec<(String, String)> = requests
        .iter()
        .map(|(method, path, _, _)| (method.clone(), path.clone()))
        .collect();
    assert_eq!(
        calls,
        [
            (
                "GET".to_owned(),
                "/v1/accounts/hello@acme.example".to_owned()
            ),
            ("POST".to_owned(), "/v1/accounts".to_owned()),
            ("PUT".to_owned(), format!("/v1/routes/{webhook}")),
        ]
    );
    let route: Value = serde_json::from_slice(&requests[2].3).unwrap();
    assert_eq!(
        route["url"],
        format!("https://hooks.norbelys.test/webhooks/{webhook}")
    );
    assert_eq!(route["usernames"], json!(["hello@acme.example"]));
    assert!(route["secret"].as_str().unwrap().starts_with("whsec_"));
    for (method, path, headers, body) in &requests {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap()
                .to_owned()
        };
        let timestamp: i64 = header("webhook-timestamp").parse().unwrap();
        let expected = crate::crypto::sign_webhook(
            &secret,
            &header("webhook-id"),
            timestamp,
            &norbelys_mail::webhooks::control_payload(method, path, body),
        );
        assert_eq!(header("webhook-signature"), expected, "{path} is signed");
    }

    let refused = connect(
        &app,
        &acme.key,
        json!({ "provider": "norbelys", "account_email": "hi@nope.example" }),
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(pointer(&refused), "/account_email");
}

/// The opened credential of connection `id` (`con_…`): its password and the API credential sealed
/// beside it, with its version.
async fn sealed_parts(
    test: &TestDb,
    workspace: &TestWorkspace,
    id: &str,
) -> (String, Option<(Option<String>, String)>, i64) {
    let uuid = Uuid::parse_str(id.trim_start_matches("con_")).unwrap();
    let row = sqlx::query!(
        r#"SELECT credential AS "credential!", credential_version FROM connections WHERE id = $1"#,
        uuid
    )
    .fetch_one(test.system.pool())
    .await
    .unwrap();
    let (super::credentials::Credential::Password(password), api) = super::credentials::open_parts(
        &testing::keys(),
        workspace.id,
        Id::from_uuid(uuid),
        &row.credential,
    )
    .unwrap() else {
        panic!("a relay holds a password");
    };
    (
        secrecy::ExposeSecret::expose_secret(&password).to_owned(),
        api.map(|api| {
            (
                api.id,
                secrecy::ExposeSecret::expose_secret(&api.secret).to_owned(),
            )
        }),
        row.credential_version,
    )
}

/// A relay's API credential is set by `PATCH`, sealed beside its SMTP password: setting it keeps
/// the password, a new password keeps it, `null` removes it, and each change moves the
/// credential's version, so the connection is verified again with what was saved. A key of the
/// wrong shape for the provider (an SES key without its id, a SendGrid key with one) or on a
/// connection without such an API is refused, naming the field.
#[tokio::test]
async fn a_relay_api_credential_is_sealed_beside_its_password() {
    let test = TestDb::new().await;
    let acme = test.workspace("acme").await;
    let app = test.app();
    let created = connect(
        &app,
        &acme.key,
        json!({
            "provider": "sendgrid", "account_email": "acme sendgrid",
            "smtp": { "host": "smtp.sendgrid.net", "port": 587, "security": "starttls", "username": "apikey", "password": "SG.smtp" },
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    let id = created.json["id"].as_str().unwrap().to_owned();
    let path = format!("/v1/connections/{id}");
    let (_, none, first) = sealed_parts(&test, &acme, &id).await;
    assert!(none.is_none());

    let set = app
        .patch(&path)
        .bearer(&acme.key)
        .json(json!({ "api_credential": { "secret": "SG.activity" } }))
        .send()
        .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.json);
    assert_eq!(set.json["status"], "verifying");
    let (password, api, second) = sealed_parts(&test, &acme, &id).await;
    assert_eq!(password, "SG.smtp");
    assert_eq!(api, Some((None, "SG.activity".to_owned())));
    assert!(second > first);

    let rotated = app
        .patch(&path)
        .bearer(&acme.key)
        .json(json!({ "smtp": { "password": "SG.smtp-2" } }))
        .send()
        .await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.json);
    let (password, api, third) = sealed_parts(&test, &acme, &id).await;
    assert_eq!(password, "SG.smtp-2");
    assert_eq!(
        api,
        Some((None, "SG.activity".to_owned())),
        "a new password keeps the key"
    );
    assert!(third > second);

    let removed = app
        .patch(&path)
        .bearer(&acme.key)
        .json(json!({ "api_credential": null }))
        .send()
        .await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.json);
    let (password, api, _) = sealed_parts(&test, &acme, &id).await;
    assert_eq!(password, "SG.smtp-2");
    assert!(api.is_none());

    let with_id = app
        .patch(&path)
        .bearer(&acme.key)
        .json(
            json!({ "api_credential": { "id": "AKIAIOSFODNN7EXAMPLE", "secret": "SG.activity" } }),
        )
        .send()
        .await;
    assert_eq!(
        with_id.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        with_id.json
    );
    assert_eq!(with_id.json["errors"][0]["pointer"], "/api_credential/id");

    let login = connect(
        &app,
        &acme.key,
        smtp_login("ada@example.com", "smtp.example.com", 587, "starttls"),
    )
    .await;
    let refused = app
        .patch(&format!(
            "/v1/connections/{}",
            login.json["id"].as_str().unwrap()
        ))
        .bearer(&acme.key)
        .json(json!({ "api_credential": { "secret": "key" } }))
        .send()
        .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.json
    );
    assert_eq!(refused.json["errors"][0]["pointer"], "/api_credential");
}
