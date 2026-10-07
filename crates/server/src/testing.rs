//! The shared test harness of the server crate: real PostgreSQL databases, the role logins,
//! factories, the in-process router, and a local HTTP sink that plays a customer's server.
//!
//! # Databases
//!
//! Store and API tests run against real PostgreSQL, because what they prove (row security per
//! role, locks, fencing, constraints) only exists there. Every test gets a database of its own,
//! so tests run in parallel without seeing each other's rows:
//!
//! 1. Once per test run, a **template** database (`norbelys_test_template`) is created from
//!    the authoritative SQLx migrations. A fingerprint of their checksums and role provisioning
//!    is recorded as the database's comment; a template whose fingerprint
//!    differs (the schema changed) is dropped and built again. Creation holds a PostgreSQL
//!    advisory lock exclusively, so two test processes never build it at once.
//! 2. Each test **clones** the template with `CREATE DATABASE … TEMPLATE …` (a file-level copy,
//!    much faster than applying the schema), holding the same advisory lock shared, so a
//!    rebuild never pulls the template from under a clone.
//! 3. The test connects one small pool per role login (`norbelys_app` for the api,
//!    `norbelys_worker` for background roles, `norbelys_tracking` for the tracking drain,
//!    `norbelys_system` for the maintenance lane and the factories), exactly as the roles do in
//!    production, so row security applies. The template's partitions cover the day it was built
//!    and the next, and it is reused while the schema does not change, so each clone then
//!    ensures the partitions of the current day and the next, as the maintenance job would: a
//!    test inserting today's rows never depends on when the template was built.
//! 4. When the [`TestDb`] is dropped (also when the test panics), its database is dropped with
//!    `WITH (FORCE)`, best effort.
//!
//! At most a few databases are alive at once in one process, which keeps the connections well
//! under the server's limit whatever the number of test threads.
//!
//! Tests require an explicit local `TEST_DATABASE_URL` and
//! `NORBELYS_TEST_DATABASE_DISPOSABLE=1`. The whole cluster must be disposable because setup
//! provisions cluster roles and their development passwords. No `.env` fallback is read.
//!
//! # Requests
//!
//! [`TestDb::app`] builds the api's state and its product router in process; [`TestApp`] sends
//! requests through it with `tower::ServiceExt::oneshot` (no network, every middleware) and
//! answers a [`Reply`] with the status, the headers and the JSON body.
//!
//! # Customer servers
//!
//! [`Sink`] listens on a loopback port, records every request (method, path, headers, body),
//! and answers with the status its path names: `/200`, `/410`, `/503?retry_after=7` (a
//! `Retry-After` header), anything else `200`.

use std::str::FromStr as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use secrecy::SecretString;
use sqlx::{AssertSqlSafe, Connection as _, PgConnection};
use tokio::sync::{OnceCell, Semaphore, SemaphorePermit};
use tower::ServiceExt as _;
use uuid::Uuid;

use crate::crypto::Keys;
use crate::db::{Database, PoolSettings, schema};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, User, WorkspaceId};
use crate::domain::scope::ScopeSet;
use crate::http::{AppState, Settings, router};
use crate::identity::api_keys::{self, KeyMode};
use crate::identity::authority::Authority;
use crate::identity::workspaces;

/// The database every test database is cloned from.
const TEMPLATE: &str = "norbelys_test_template";
/// The advisory lock that orders building the template (exclusive) and cloning it (shared).
const TEMPLATE_LOCK: i64 = 0x6e62_7465_7374_706c;
/// The development password of the role logins.
const ROLE_PASSWORD: &str = "norbelys";
/// Test databases alive at once in this process: each holds up to eight connections.
const LIVE_DATABASES: usize = 6;
/// The dashboard's origin in the tests' api: sign-in and cookie requests come from it.
pub(crate) const DASHBOARD: &str = "https://app.norbelys.test";

/// Built once per process; holds nothing but the fact.
static TEMPLATE_READY: OnceCell<()> = OnceCell::const_new();
/// Bounds the databases alive at once.
static SLOTS: Semaphore = Semaphore::const_new(LIVE_DATABASES);

/// The administrator URL; a test without it cannot run.
fn admin_url() -> String {
    crate::config::test_admin_url().expect(
        "set TEST_DATABASE_URL to a dedicated local PostgreSQL test cluster and NORBELYS_TEST_DATABASE_DISPOSABLE=1",
    )
}

/// The URL of `login` on `database`, on the administrator's server.
fn login_url(admin: &str, login: &str, database: &str) -> String {
    let mut url = url::Url::parse(admin).expect("the administrator URL is a URL");
    url.set_username(login).expect("the URL takes a user name");
    url.set_password(Some(ROLE_PASSWORD))
        .expect("the URL takes a password");
    url.set_path(&format!("/{database}"));
    url.to_string()
}

/// Opens one administrator connection.
async fn admin_connection(admin: &str) -> PgConnection {
    PgConnection::connect(admin)
        .await
        .expect("the administrator login connects")
}

/// Runs one statement of our own making (database names are generated, never input).
async fn execute(connection: &mut PgConnection, statement: String) {
    sqlx::raw_sql(AssertSqlSafe(statement.clone()))
        .execute(connection)
        .await
        .unwrap_or_else(|error| panic!("`{statement}` failed: {error}"));
}

/// Builds the template unless it holds the current schema.
async fn ensure_template(admin: &str) {
    let provision = include_str!("../provision.sql");
    let mut fingerprint = crate::crypto::Sha256::new();
    fingerprint.update(provision.as_bytes());
    for migration in schema::MIGRATIONS.iter() {
        fingerprint.update(&migration.version.to_be_bytes());
        fingerprint.update(&migration.checksum);
    }
    let wanted = crate::crypto::hex(&fingerprint.finish());
    let mut connection = admin_connection(&swap_database(admin, "postgres")).await;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(TEMPLATE_LOCK)
        .execute(&mut connection)
        .await
        .expect("lock");
    let recorded: Option<Option<String>> = sqlx::query_scalar(
        "SELECT shobj_description(oid, 'pg_database') FROM pg_database WHERE datname = $1",
    )
    .bind(TEMPLATE)
    .fetch_optional(&mut connection)
    .await
    .expect("read the template's fingerprint");
    if recorded.flatten().as_deref() != Some(wanted.as_str()) {
        let mut tx = connection.begin().await.expect("begin role provisioning");
        sqlx::raw_sql(provision)
            .execute(&mut *tx)
            .await
            .expect("provision test roles");
        for role in ["app", "worker", "system", "tracking", "metrics"] {
            // Both interpolated values are repository constants, never external input.
            sqlx::raw_sql(AssertSqlSafe(format!(
                "ALTER ROLE norbelys_{role} PASSWORD '{ROLE_PASSWORD}'"
            )))
            .execute(&mut *tx)
            .await
            .expect("set disposable role passwords");
        }
        tx.commit().await.expect("commit role provisioning");
        execute(
            &mut connection,
            format!("DROP DATABASE IF EXISTS {TEMPLATE} WITH (FORCE)"),
        )
        .await;
        execute(&mut connection, format!("CREATE DATABASE {TEMPLATE}")).await;
        let mut template = PgConnection::connect(&swap_database(admin, TEMPLATE))
            .await
            .expect("connect to the template");
        schema::MIGRATIONS
            .run(&mut template)
            .await
            .expect("install the current test schema atomically");
        template.close().await.expect("close the template");
        execute(
            &mut connection,
            format!("COMMENT ON DATABASE {TEMPLATE} IS '{wanted}'"),
        )
        .await;
    }
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(TEMPLATE_LOCK)
        .execute(&mut connection)
        .await
        .expect("unlock");
    connection.close().await.expect("close");
}

/// The administrator URL pointed at another database.
fn swap_database(admin: &str, database: &str) -> String {
    let mut url = url::Url::parse(admin).expect("the administrator URL is a URL");
    url.set_path(&format!("/{database}"));
    url.to_string()
}

/// The deployment keys of every test: fixed, so sealed values and cursors are reproducible.
#[must_use]
pub(crate) fn keys() -> Keys {
    Keys::from_deployment_key(&SecretString::from(STANDARD.encode([7_u8; 32])))
        .expect("a 32-byte key")
}

/// A pool of `login` on `database`, small and lazy.
async fn pool(
    admin: &str,
    login: &str,
    database: &str,
    application_name: &'static str,
) -> Database {
    Database::connect(
        &SecretString::from(login_url(admin, login, database)),
        PoolSettings {
            application_name,
            max_connections: 2,
            statement_timeout: Duration::from_secs(30),
            acquire_timeout: Duration::from_secs(30),
            min_connections: 0,
            request_deadline: None,
        },
    )
    .await
    .unwrap_or_else(|error| panic!("{login} connects to {database}: {error}"))
}

/// One test's database, cloned from the template, with a pool per role login. Dropping it drops
/// the database.
pub(crate) struct TestDb {
    /// The database's name.
    pub name: String,
    /// `norbelys_app`: the api's login, under row security.
    pub app: Database,
    /// `norbelys_worker`: the background roles' login, under row security, member of the
    /// scheduler role.
    pub worker: Database,
    /// `norbelys_tracking`: the tracking drain's login, which writes events and reads almost
    /// nothing.
    pub tracking: Database,
    /// `norbelys_system`: the maintenance lane's and the operator's login, bypassing row security.
    pub system: Database,
    /// The test's object store: a local directory of its own under the system's temporary
    /// directory, named after the database and removed with it. The api and the worker of a
    /// test share it, as they share a bucket in production.
    pub storage: crate::storage::Storage,
    admin: String,
    _slot: SemaphorePermit<'static>,
}

impl TestDb {
    /// A fresh database holding the schema and nothing else but the `system` workspace.
    pub(crate) async fn new() -> Self {
        let admin = admin_url();
        TEMPLATE_READY.get_or_init(|| ensure_template(&admin)).await;
        let slot = SLOTS.acquire().await.expect("the slots are never closed");
        let name = format!("norbelys_test_{}", Uuid::now_v7().simple());
        let mut connection = admin_connection(&admin).await;
        sqlx::query("SELECT pg_advisory_lock_shared($1)")
            .bind(TEMPLATE_LOCK)
            .execute(&mut connection)
            .await
            .expect("lock");
        execute(
            &mut connection,
            format!("CREATE DATABASE {name} TEMPLATE {TEMPLATE}"),
        )
        .await;
        sqlx::query("SELECT pg_advisory_unlock_shared($1)")
            .bind(TEMPLATE_LOCK)
            .execute(&mut connection)
            .await
            .expect("unlock");
        connection.close().await.expect("close");
        let system = pool(&admin, "norbelys_system", &name, "norbelys-test-system").await;
        sqlx::query("SELECT ensure_partitions_ahead(interval '1 day')")
            .execute(system.pool())
            .await
            .expect("ensure the partitions of today and tomorrow");
        Self {
            app: pool(&admin, "norbelys_app", &name, "norbelys-test-app").await,
            worker: pool(&admin, "norbelys_worker", &name, "norbelys-test-worker").await,
            tracking: pool(&admin, "norbelys_tracking", &name, "norbelys-test-tracking").await,
            system,
            storage: crate::storage::Storage::local(&std::env::temp_dir().join(&name))
                .expect("a temporary directory for the test's objects"),
            name,
            admin,
            _slot: slot,
        }
    }

    /// A live workspace with its owner and an API key holding every scope.
    pub(crate) async fn workspace(&self, slug: &str) -> TestWorkspace {
        let mut tx = self.system.begin().await.expect("begin");
        let owner = EmailAddress::parse(&format!("owner@{slug}.example")).expect("an address");
        let created = workspaces::create_with_owner(&mut tx, slug, slug, &owner, KeyMode::Live)
            .await
            .expect("create the workspace");
        tx.commit().await.expect("commit");
        TestWorkspace {
            id: WorkspaceId::trusted(created.workspace.uuid()),
            owner: created.owner,
            key: created.api_key_secret,
        }
    }

    /// A workspace in test mode with its owner and a test-mode API key (`nb_test_`) holding every
    /// scope: its sender submits through the fake transport and reaches no provider.
    pub(crate) async fn test_workspace(&self, slug: &str) -> TestWorkspace {
        let mut tx = self.system.begin().await.expect("begin");
        let owner = EmailAddress::parse(&format!("owner@{slug}.example")).expect("an address");
        let created = workspaces::create_with_owner(&mut tx, slug, slug, &owner, KeyMode::Test)
            .await
            .expect("create the workspace");
        tx.commit().await.expect("commit");
        TestWorkspace {
            id: WorkspaceId::trusted(created.workspace.uuid()),
            owner: created.owner,
            key: created.api_key_secret,
        }
    }

    /// Another live API key of `workspace`'s owner, holding only `scopes`.
    pub(crate) async fn api_key(&self, workspace: &TestWorkspace, scopes: ScopeSet) -> String {
        let mut tx = self.system.begin().await.expect("begin");
        let (_, key) = api_keys::create(
            &mut tx,
            workspace.id,
            workspace.owner,
            "Test key",
            scopes,
            KeyMode::Live,
            None,
        )
        .await
        .expect("create the key");
        tx.commit().await.expect("commit");
        key.secret
    }

    /// The `system` workspace's transactional sender (a relay connection and an identity tagged
    /// `transactional`), so sign-in codes and invitations are accepted for sending.
    pub(crate) async fn transactional_sender(&self) {
        sqlx::query(
            "WITH connection AS (
                INSERT INTO connections (workspace_id, provider, transport, account_email, smtp, status, daily_limit)
                VALUES ($1, 'sendgrid', 'smtp', 'relay@norbelys.test',
                        '{\"host\": \"smtp.norbelys.test\", \"port\": 587, \"security\": \"starttls\", \"username\": \"apikey\"}',
                        'active', 100000)
                RETURNING id)
             INSERT INTO sender_identities (workspace_id, connection_id, email, name, tags)
             SELECT $1, id, 'no-reply@norbelys.test', 'Norbelys', '{transactional}' FROM connection",
        )
        .bind(crate::jobs::SYSTEM_WORKSPACE.uuid())
        .execute(self.system.pool())
        .await
        .expect("the transactional sender");
    }

    /// A signed-in browser of the user holding `email` (created when nobody does), as an email
    /// code would sign them in.
    pub(crate) async fn session(&self, email: &str) -> TestSession {
        let email = EmailAddress::parse(email).expect("an address");
        let mut tx = self.app.begin().await.expect("begin");
        let (user, _) = crate::identity::users::find_or_create(&mut tx, &email)
            .await
            .expect("the user");
        let issued = crate::identity::sessions::create(
            &mut tx,
            &keys(),
            &crate::identity::sessions::NewSession {
                user: user.id,
                method: crate::domain::identity::AuthMethod::EmailCode,
                sso: None,
                authenticated_at: None,
                ip_hash: None,
                user_agent: None,
            },
        )
        .await
        .expect("the session");
        tx.commit().await.expect("commit");
        TestSession {
            user: user.id,
            session: issued.id,
            cookie: issued
                .cookie
                .split(';')
                .next()
                .expect("a cookie pair")
                .to_owned(),
            csrf: issued.csrf_token,
        }
    }

    /// A new key that signs workspace tokens, as the operator's `admin keys rotate` adds it;
    /// returns its id.
    pub(crate) async fn signing_key(&self) -> String {
        let mut tx = self.system.begin().await.expect("begin");
        let kid = crate::identity::tokens::rotate(&mut tx, &keys())
            .await
            .expect("rotate the signing keys");
        tx.commit().await.expect("commit");
        kid
    }

    /// A pool of the PostgreSQL metrics scraper's login, `norbelys_metrics` (`pg_monitor`, no
    /// table grants), on this database. Template setup provisions its development password.
    pub(crate) async fn metrics(&self) -> Database {
        pool(
            &self.admin,
            "norbelys_metrics",
            &self.name,
            "norbelys-test-metrics",
        )
        .await
    }

    /// The api's state on this database (the app login) and its product router.
    pub(crate) fn app(&self) -> TestApp {
        self.app_with(crate::senders::Settings::for_tests())
    }

    /// [`TestDb::app`] with the Sending area's settings of the test: OAuth apps, a provider
    /// client rebased onto a fake provider.
    pub(crate) fn app_with(&self, senders: crate::senders::Settings) -> TestApp {
        self.build(senders, crate::identity::Identity::for_tests())
    }

    /// [`TestDb::app`] with the identity module of the test: OpenID Connect providers on a fake
    /// provider, a captcha.
    pub(crate) fn app_with_identity(&self, identity: crate::identity::Identity) -> TestApp {
        self.build(crate::senders::Settings::for_tests(), identity)
    }

    /// [`TestDb::app_with`] with the provider-webhook ingress of the test: one whose SNS signing
    /// certificates the test preloaded, so callbacks it signed itself verify.
    pub(crate) fn app_with_ingress(
        &self,
        senders: crate::senders::Settings,
        ingress: crate::webhooks::ingress::Ingress,
    ) -> TestApp {
        self.build_with(senders, crate::identity::Identity::for_tests(), ingress, 1)
    }

    fn build(
        &self,
        senders: crate::senders::Settings,
        identity: crate::identity::Identity,
    ) -> TestApp {
        // No spool: a batch the database refuses is answered `503`; the spool has tests of its
        // own.
        let ingress = crate::webhooks::ingress::Ingress::start(self.app.clone(), None);
        self.build_with(senders, identity, ingress, 1)
    }

    /// [`TestDb::app`] as one api replica of a deployment of `replicas` holds its rate limits: a
    /// shared budget divided among them, so a test reaches its end, or counts exactly what is
    /// left of a budget that refills slowly, in a few requests.
    pub(crate) fn app_with_replicas(&self, replicas: u32) -> TestApp {
        let ingress = crate::webhooks::ingress::Ingress::start(self.app.clone(), None);
        self.build_with(
            crate::senders::Settings::for_tests(),
            crate::identity::Identity::for_tests(),
            ingress,
            replicas,
        )
    }

    fn build_with(
        &self,
        senders: crate::senders::Settings,
        identity: crate::identity::Identity,
        ingress: crate::webhooks::ingress::Ingress,
        replicas: u32,
    ) -> TestApp {
        let state = AppState {
            db: self.app.clone(),
            keys: keys(),
            authority: Authority::new(),
            settings: Arc::new(Settings {
                public_api_url: url::Url::parse("http://127.0.0.1:3001").expect("a URL"),
                senders,
                public_tracking_url: url::Url::parse("https://t.norbelys.test").expect("a URL"),
            }),
            storage: self.storage.clone(),
            // No name servers: tests never depend on the network, and every lookup fails at once.
            resolver: crate::dns::Resolver::offline(),
            identity,
            // Tests name their client address in `X-Forwarded-For` when a limit depends on it.
            limits: crate::http::ratelimit::Limits::new(
                true,
                replicas,
                vec!["127.0.0.1".parse().unwrap()],
            ),
            ingress,
        };
        TestApp {
            router: router::product(state),
        }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(&self.name));
        let admin = self.admin.clone();
        let name = self.name.clone();
        // Dropping runs inside the test's runtime, which cannot block on its own futures: a
        // thread with its own runtime drops the database, forcing out the pools' sessions.
        let dropped = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                if let Ok(mut connection) = PgConnection::connect(&admin).await {
                    let statement = format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)");
                    let _ = sqlx::raw_sql(AssertSqlSafe(statement))
                        .execute(&mut connection)
                        .await;
                    let _ = connection.close().await;
                }
            });
        });
        let _ = dropped.join();
    }
}

impl TestDb {
    /// Keeps this database after the test instead of dropping it, and returns its name: for a live
    /// run whose rows a person inspects afterwards with `psql`, then drops by hand
    /// (`DROP DATABASE <name> WITH (FORCE)`). Its pools and its slot are leaked with it, which the
    /// end of the test process reclaims.
    pub(crate) fn keep(self) -> String {
        let name = self.name.clone();
        std::mem::forget(self);
        name
    }
}

/// A signed-in browser made by [`TestDb::session`].
#[derive(Debug, Clone)]
pub(crate) struct TestSession {
    /// The user.
    pub user: Id<User>,
    /// The session.
    pub session: Id<crate::domain::ids::Session>,
    /// The `Cookie` header value (`__Host-nb_session=…`).
    pub cookie: String,
    /// The session's CSRF token.
    pub csrf: String,
}

/// A workspace made by [`TestDb::workspace`].
pub(crate) struct TestWorkspace {
    /// The workspace.
    pub id: WorkspaceId,
    /// Its owner.
    pub owner: Id<User>,
    /// The secret of its first API key, with every scope.
    pub key: String,
}

/// The api in process.
pub(crate) struct TestApp {
    router: axum::Router,
}

impl TestApp {
    /// The product router itself, for code that drives the API in process on its own (the
    /// development seed).
    pub(crate) fn router(&self) -> axum::Router {
        self.router.clone()
    }

    /// A `GET` request.
    pub(crate) fn get(&self, path: &str) -> Call<'_> {
        self.call(Method::GET, path)
    }

    /// A `POST` request.
    pub(crate) fn post(&self, path: &str) -> Call<'_> {
        self.call(Method::POST, path)
    }

    /// A `PATCH` request.
    pub(crate) fn patch(&self, path: &str) -> Call<'_> {
        self.call(Method::PATCH, path)
    }

    /// A `DELETE` request.
    pub(crate) fn delete(&self, path: &str) -> Call<'_> {
        self.call(Method::DELETE, path)
    }

    fn call(&self, method: Method, path: &str) -> Call<'_> {
        Call {
            app: self,
            method,
            path: path.to_owned(),
            credential: None,
            idempotency_key: None,
            headers: Vec::new(),
            body: None,
            raw: None,
        }
    }
}

/// A request being built.
pub(crate) struct Call<'a> {
    app: &'a TestApp,
    method: Method,
    path: String,
    credential: Option<String>,
    idempotency_key: Option<String>,
    headers: Vec<(String, String)>,
    body: Option<serde_json::Value>,
    raw: Option<Bytes>,
}

impl Call<'_> {
    /// Authenticates with `credential` (`Authorization: Bearer …`).
    #[must_use]
    pub(crate) fn bearer(mut self, credential: &str) -> Self {
        self.credential = Some(credential.to_owned());
        self
    }

    /// Sends the request as the dashboard's browser signed in with `session`: its cookie, the
    /// dashboard's `Origin` and the session's CSRF token.
    #[must_use]
    pub(crate) fn browser(self, session: &TestSession) -> Self {
        self.header("cookie", &session.cookie)
            .header("origin", DASHBOARD)
            .header("x-csrf-token", &session.csrf)
    }

    /// Sends the request as the dashboard's sign-in page: its `Origin` and the custom header that
    /// guards anonymous sign-in calls.
    #[must_use]
    pub(crate) fn dashboard(self) -> Self {
        self.header("origin", DASHBOARD).header("x-csrf-token", "1")
    }

    /// Sends `Idempotency-Key: key`.
    #[must_use]
    pub(crate) fn idempotency(mut self, key: &str) -> Self {
        self.idempotency_key = Some(key.to_owned());
        self
    }

    /// Sends the header `name: value` (a cookie, for example).
    #[must_use]
    pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Sends `body` as JSON.
    #[must_use]
    pub(crate) fn json(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }

    /// Sends `bytes` as the body with `content_type` (a CSV upload, for example).
    #[must_use]
    pub(crate) fn raw(mut self, content_type: &str, bytes: impl Into<Bytes>) -> Self {
        self.headers.push((
            header::CONTENT_TYPE.as_str().to_owned(),
            content_type.to_owned(),
        ));
        self.raw = Some(bytes.into());
        self
    }

    /// Sends the request through the router and reads the whole answer.
    pub(crate) async fn send(self) -> Reply {
        let mut request = Request::builder().method(self.method).uri(&self.path);
        if let Some(credential) = &self.credential {
            request = request.header(header::AUTHORIZATION, format!("Bearer {credential}"));
        }
        if let Some(key) = &self.idempotency_key {
            request = request.header("idempotency-key", key);
        }
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let body = match (&self.body, self.raw) {
            (Some(json), _) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(json).expect("JSON"))
            }
            (None, Some(raw)) => Body::from(raw),
            (None, None) => Body::empty(),
        };
        let mut request = request.body(body).expect("a request");
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = self
            .app
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("the router never fails");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body");
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
            })
        };
        Reply {
            status,
            headers,
            json,
            body: bytes,
        }
    }
}

/// An answer of the router.
#[derive(Debug)]
pub(crate) struct Reply {
    /// The status.
    pub status: StatusCode,
    /// The headers.
    pub headers: HeaderMap,
    /// The body as JSON (`null` when empty).
    pub json: serde_json::Value,
    /// The body as sent (an image, a page).
    pub body: Bytes,
}

impl Reply {
    /// A header's value as text.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// A request the [`Sink`] received.
#[derive(Debug, Clone)]
pub(crate) struct Captured {
    /// The method.
    pub method: Method,
    /// The path, without the query.
    pub path: String,
    /// The query, when there is one.
    pub query: Option<String>,
    /// The headers.
    pub headers: HeaderMap,
    /// The body.
    pub body: Bytes,
}

impl Captured {
    /// A header's value as text.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// A local HTTP server playing a customer's endpoint (see the module).
pub(crate) struct Sink {
    base: String,
    requests: Arc<Mutex<Vec<Captured>>>,
}

impl Sink {
    /// Starts the sink on a free loopback port, in the test's runtime.
    pub(crate) async fn start() -> Self {
        let requests: Arc<Mutex<Vec<Captured>>> = Arc::default();
        let recorded = Arc::clone(&requests);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let address = listener.local_addr().expect("its address");
        let app = axum::Router::new().fallback(move |request: Request<Body>| {
            let recorded = Arc::clone(&recorded);
            async move { answer(request, &recorded).await }
        });
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            base: format!("http://{address}"),
            requests,
        }
    }

    /// The sink's URL for `path` (such as `/410`). The path's first segment is the status it
    /// answers (`200` otherwise); a query `retry_after=<seconds>` adds `Retry-After`, and a query
    /// `bytes=<n>` makes the body `n` bytes long instead of `sink`.
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Every request received so far.
    pub(crate) fn requests(&self) -> Vec<Captured> {
        self.requests.lock().expect("not poisoned").clone()
    }
}

async fn answer(request: Request<Body>, recorded: &Mutex<Vec<Captured>>) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let path = parts.uri.path().to_owned();
    recorded.lock().expect("not poisoned").push(Captured {
        method: parts.method,
        path: path.clone(),
        query: parts.uri.query().map(str::to_owned),
        headers: parts.headers,
        body,
    });
    let status = path
        .trim_start_matches('/')
        .split('/')
        .next()
        .and_then(|segment| StatusCode::from_str(segment).ok())
        .unwrap_or(StatusCode::OK);
    let retry_after = parts
        .uri
        .query()
        .and_then(|query| query.strip_prefix("retry_after="))
        .and_then(|seconds| HeaderValue::from_str(seconds).ok());
    let body = parts
        .uri
        .query()
        .and_then(|query| query.strip_prefix("bytes="))
        .and_then(|bytes| bytes.parse::<usize>().ok())
        .map_or_else(|| "sink".to_owned(), |bytes| "x".repeat(bytes));
    let mut response = (status, body).into_response();
    if let Some(seconds) = retry_after {
        response.headers_mut().insert(header::RETRY_AFTER, seconds);
    }
    response
}

// ───────────────────────────── delivery fixtures ─────────────────────────────

/// A sending connection made for delivery tests, with its one sender identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TestSender {
    /// The connection.
    pub connection: Id<crate::domain::ids::Connection>,
    /// Its identity.
    pub identity: Id<crate::domain::ids::SenderIdentity>,
}

/// What a delivery test's connection is. [`SenderSpec::mailbox`] and [`SenderSpec::relay`] are
/// the two shapes; the fields adjust them.
#[derive(Debug, Clone)]
pub(crate) struct SenderSpec {
    /// `smtp` (a mailbox reached by password), `sendgrid`, `ses`, …
    pub provider: &'static str,
    /// The account's address, also the identity's.
    pub address: String,
    /// A paced sender's interval; `None` for a rate-paced relay.
    pub interval: Option<i32>,
    /// Its phase in the slot.
    pub phase: i16,
    /// The daily limit.
    pub daily_limit: i32,
    /// Its pacing clock, seconds from now (negative: due); `None` leaves the epoch (due at once).
    pub next_send_in: Option<i64>,
    /// Its quota scope.
    pub scope: Option<Uuid>,
}

impl SenderSpec {
    /// A mailbox reached by password, paced every 5 minutes, its clock due.
    pub(crate) fn mailbox(address: &str) -> Self {
        Self {
            provider: "smtp",
            address: address.to_owned(),
            interval: Some(5),
            phase: 41,
            daily_limit: 100,
            next_send_in: Some(-60),
            scope: None,
        }
    }

    /// A rate-paced relay (SendGrid over SMTP).
    pub(crate) fn relay(address: &str) -> Self {
        Self {
            provider: "sendgrid",
            address: address.to_owned(),
            interval: None,
            phase: 0,
            daily_limit: 100_000,
            next_send_in: None,
            scope: None,
        }
    }
}

impl TestDb {
    /// An active connection of `workspace` as `spec` describes it, with one identity of its
    /// address, written directly (no check job).
    pub(crate) async fn sender(&self, workspace: WorkspaceId, spec: &SenderSpec) -> TestSender {
        let smtp = if spec.provider == "ses" {
            serde_json::json!({"host": "email-smtp.eu-west-1.amazonaws.com", "port": 587, "security": "starttls", "username": "AKIA", "configuration_set": "norbelys"})
        } else {
            serde_json::json!({"host": "smtp.example.test", "port": 587, "security": "starttls", "username": spec.address})
        };
        let (connection, identity): (Uuid, Uuid) = sqlx::query_as(
            "WITH c AS (
                INSERT INTO connections (workspace_id, provider, transport, account_email, smtp, status, daily_limit,
                                         send_interval_minutes, send_phase_seconds, next_send_at, quota_scope_id)
                VALUES ($1, $2, 'smtp', $3, $4, 'active', $5, $6, $7,
                        CASE WHEN $8::bigint IS NULL THEN 'epoch'::timestamptz ELSE now() + make_interval(secs => $8::bigint) END, $9)
                RETURNING workspace_id, id, account_email)
             INSERT INTO sender_identities (workspace_id, connection_id, email)
             SELECT workspace_id, id, account_email FROM c
             RETURNING connection_id, id",
        )
        .bind(workspace.uuid())
        .bind(spec.provider)
        .bind(&spec.address)
        .bind(smtp)
        .bind(spec.daily_limit)
        .bind(spec.interval)
        .bind(spec.phase)
        .bind(spec.next_send_in)
        .bind(spec.scope)
        .fetch_one(self.system.pool())
        .await
        .expect("the connection");
        TestSender {
            connection: Id::from_uuid(connection),
            identity: Id::from_uuid(identity),
        }
    }

    /// A quota scope of `workspace` limiting its connections to `messages_per_day` and
    /// `recipients_per_day` over a rolling day.
    pub(crate) async fn quota_scope(
        &self,
        workspace: WorkspaceId,
        provider: &str,
        messages_per_day: Option<i32>,
        recipients_per_day: Option<i32>,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO quota_scopes (workspace_id, provider, scope_key, messages_per_day, recipients_per_day)
             VALUES ($1, $2, gen_random_uuid()::text, $3, $4) RETURNING id",
        )
        .bind(workspace.uuid())
        .bind(provider)
        .bind(messages_per_day)
        .bind(recipients_per_day)
        .fetch_one(self.system.pool())
        .await
        .expect("the quota scope")
    }

    /// A direct message from `sender` to `to`, queued and due `due_in` seconds from now (negative:
    /// overdue), with an optional `expires_in` deadline.
    pub(crate) async fn direct_message(
        &self,
        workspace: WorkspaceId,
        sender: &TestSender,
        to: &[&str],
        due_in: i64,
    ) -> Id<crate::domain::ids::Message> {
        let to: Vec<String> = to.iter().map(|address| (*address).to_owned()).collect();
        let message: Uuid = sqlx::query_scalar(
            "WITH m AS (
                INSERT INTO messages (workspace_id, kind, sender_identity_id, connection_id, from_email, to_addresses, subject,
                                      html, text_body, render_version, rendered_at, internet_message_id, send_at)
                SELECT $1, 'direct', i.id, i.connection_id, i.email, $3, 'Hello', '<p>Hello</p>', 'Hello', '1', now(),
                       '<' || gen_random_uuid() || '@example.test>', now() + make_interval(secs => $4::bigint)
                  FROM sender_identities i WHERE i.workspace_id = $1 AND i.id = $2
                RETURNING workspace_id, id, connection_id, send_at)
             INSERT INTO delivery_queue (workspace_id, message_id, connection_id, run_at, paced)
             SELECT workspace_id, id, connection_id, send_at, false FROM m
             RETURNING message_id",
        )
        .bind(workspace.uuid())
        .bind(sender.identity.uuid())
        .bind(&to)
        .bind(due_in)
        .fetch_one(self.system.pool())
        .await
        .expect("the direct message");
        Id::from_uuid(message)
    }

    /// An active campaign of `workspace` with one published step and variant, whose pool names
    /// `sender`'s identity, open in `send_window` (`None`: always); returns its id.
    pub(crate) async fn campaign(
        &self,
        workspace: WorkspaceId,
        sender: &TestSender,
        send_window: Option<serde_json::Value>,
    ) -> Uuid {
        let campaign: Uuid = sqlx::query_scalar(
            "WITH c AS (
                INSERT INTO campaigns (workspace_id, name, status, send_window) VALUES ($1, 'Test', 'active', $3)
                RETURNING workspace_id, id),
             s AS (INSERT INTO steps (workspace_id, campaign_id, position, name) SELECT workspace_id, id, 1, 'First' FROM c
                   RETURNING workspace_id, id),
             r AS (INSERT INTO step_revisions (workspace_id, step_id, revision, delay_seconds, ranking_objective,
                                               observation_window_seconds, minimum_sample)
                   SELECT workspace_id, id, 1, 0, 'replies', 3600, 10 FROM s RETURNING workspace_id, step_id),
             v AS (INSERT INTO variants (workspace_id, step_id, name) SELECT workspace_id, step_id, 'A' FROM r
                   RETURNING workspace_id, id, step_id),
             vr AS (INSERT INTO variant_revisions (workspace_id, variant_id, version, subject, html)
                    SELECT workspace_id, id, 1, 'Hi', '<p>Hi</p>' FROM v RETURNING workspace_id, variant_id),
             o AS (INSERT INTO step_revision_variants (workspace_id, step_id, step_revision, variant_id, variant_version)
                   SELECT v.workspace_id, v.step_id, 1, v.id, 1 FROM v JOIN vr ON vr.variant_id = v.id RETURNING 1),
             p AS (INSERT INTO campaign_senders (workspace_id, campaign_id, sender_identity_id)
                   SELECT workspace_id, id, $2 FROM c RETURNING 1)
             SELECT id FROM c",
        )
        .bind(workspace.uuid())
        .bind(sender.identity.uuid())
        .bind(send_window)
        .fetch_one(self.system.pool())
        .await
        .expect("the campaign");
        sqlx::query(
            "UPDATE steps SET current_revision = 1 WHERE workspace_id = $1 AND campaign_id = $2",
        )
        .bind(workspace.uuid())
        .bind(campaign)
        .execute(self.system.pool())
        .await
        .expect("the step's revision");
        campaign
    }

    /// A cold message of `campaign` to a new person at `to`, from `sender`, enrolled and queued
    /// (paced), due `due_in` seconds from now.
    pub(crate) async fn campaign_message(
        &self,
        workspace: WorkspaceId,
        campaign: Uuid,
        sender: &TestSender,
        to: &str,
        due_in: i64,
    ) -> Id<crate::domain::ids::Message> {
        let message: Uuid = sqlx::query_scalar(
            "WITH person AS (INSERT INTO people (workspace_id, email) VALUES ($1, $4) RETURNING workspace_id, id),
                  enrollment AS (INSERT INTO enrollments (workspace_id, campaign_id, person_id)
                                 SELECT workspace_id, $2, id FROM person RETURNING workspace_id, id, person_id),
                  option AS (SELECT o.step_id, o.variant_id FROM step_revision_variants o JOIN steps s
                               ON s.workspace_id = o.workspace_id AND s.id = o.step_id
                              WHERE s.workspace_id = $1 AND s.campaign_id = $2 LIMIT 1),
                  m AS (INSERT INTO messages (workspace_id, kind, campaign_id, step_id, step_revision, variant_id, variant_version,
                                              enrollment_id, person_id, sender_identity_id, connection_id, from_email,
                                              to_addresses, subject, render_version, rendered_at, internet_message_id, send_at)
                        SELECT $1, 'campaign', $2, option.step_id, 1, option.variant_id, 1, enrollment.id, enrollment.person_id,
                               i.id, i.connection_id, i.email, ARRAY[$4], 'Hi', '1', now(),
                               '<' || gen_random_uuid() || '@example.test>', now() + make_interval(secs => $5::bigint)
                          FROM enrollment, option, sender_identities i WHERE i.workspace_id = $1 AND i.id = $3
                        RETURNING workspace_id, id, connection_id, send_at)
             INSERT INTO delivery_queue (workspace_id, message_id, connection_id, run_at, paced)
             SELECT workspace_id, id, connection_id, send_at, true FROM m
             RETURNING message_id",
        )
        .bind(workspace.uuid())
        .bind(campaign)
        .bind(sender.identity.uuid())
        .bind(to)
        .bind(due_in)
        .fetch_one(self.system.pool())
        .await
        .expect("the campaign message");
        Id::from_uuid(message)
    }

    /// Another pool of the worker login on this database, of `connections`, for a test that plays
    /// two sender replicas or runs sessions concurrently.
    pub(crate) async fn worker_pool(&self, connections: u32) -> Database {
        Database::connect(
            &SecretString::from(login_url(&self.admin, "norbelys_worker", &self.name)),
            PoolSettings {
                application_name: "norbelys-test-worker",
                max_connections: connections,
                statement_timeout: Duration::from_secs(30),
                acquire_timeout: Duration::from_secs(30),
                min_connections: 0,
                request_deadline: None,
            },
        )
        .await
        .expect("a worker pool")
    }

    /// Claims what `sender`'s connection has due, as the sender replica `owner` does: the scans'
    /// page of `workspace` (from fresh cursors), then one claim of the connection with sixteen
    /// slots free and every limiter allowing. The claim's own filters apply (a paused
    /// connection, an open breaker or a spent budget is not a candidate: [`Claim::Nothing`]), so
    /// a test changes what a Start or a Finish should find after its claim.
    ///
    /// [`Claim::Nothing`]: crate::delivery::claim::Claim::Nothing
    pub(crate) async fn claim(
        &self,
        workspace: WorkspaceId,
        sender: &TestSender,
        owner: &str,
    ) -> crate::delivery::claim::Claim {
        use crate::delivery::claim::{self, Claim, Cursors};
        let page = claim::page(&self.worker, workspace, &mut Cursors::default())
            .await
            .expect("the scans run");
        let Some(candidate) = page
            .candidates
            .iter()
            .find(|candidate| candidate.connection == sender.connection)
        else {
            return Claim::Nothing;
        };
        claim::claim(&self.worker, owner, candidate, 16, &mut |_| true)
            .await
            .expect("the claim runs")
    }

    /// The messages a claim of `sender` took as `owner`, or a panic naming what it did instead.
    pub(crate) async fn claimed(
        &self,
        workspace: WorkspaceId,
        sender: &TestSender,
        owner: &str,
    ) -> Vec<crate::delivery::claim::Claimed> {
        match self.claim(workspace, sender, owner).await {
            crate::delivery::claim::Claim::Wave(wave) => wave.messages,
            other => panic!("expected a wave, got {other:?}"),
        }
    }

    /// The Start of `claimed` on `sender` as `owner`, with an SMTP budget (300 seconds) and a
    /// day's retry window, as the sender role asks for it.
    pub(crate) async fn start(
        &self,
        workspace: WorkspaceId,
        sender: &TestSender,
        owner: &str,
        claimed: &crate::delivery::claim::Claimed,
    ) -> crate::delivery::start::Started {
        crate::delivery::start::start(
            &self.worker,
            &crate::delivery::start::Start {
                workspace,
                connection: sender.connection,
                message: claimed.message,
                generation: claimed.generation,
                owner,
                budget: Duration::from_secs(300),
                retry_window: Duration::from_secs(86_400),
            },
        )
        .await
        .expect("the Start runs")
    }

    /// The Start of `claimed`, which must submit: its submission marker.
    pub(crate) async fn begun(
        &self,
        workspace: WorkspaceId,
        sender: &TestSender,
        owner: &str,
        claimed: &crate::delivery::claim::Claimed,
    ) -> crate::domain::time::Timestamp {
        match self.start(workspace, sender, owner, claimed).await {
            crate::delivery::start::Started::Submit(begun) => begun.started,
            other => panic!("expected the Start to submit, got {other:?}"),
        }
    }

    /// The Finish of `reports` of `sender`'s connection as `owner`.
    pub(crate) async fn finish(
        &self,
        workspace: WorkspaceId,
        sender: &TestSender,
        owner: &str,
        reports: &[crate::delivery::finish::Report],
    ) -> crate::delivery::finish::Finished {
        crate::delivery::finish::finish(&self.worker, workspace, sender.connection, owner, reports)
            .await
            .expect("the Finish runs")
    }

    /// The `type`s of `workspace`'s outbox events about `subject`, oldest first.
    pub(crate) async fn told(&self, workspace: WorkspaceId, subject: Uuid) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT type FROM outbox_events WHERE workspace_id = $1 AND subject_id = $2 ORDER BY id",
        )
        .bind(workspace.uuid())
        .bind(subject)
        .fetch_all(self.system.pool())
        .await
        .expect("the outbox")
    }

    /// `connection`'s daily ledger summed over its days: units reserved, units used.
    pub(crate) async fn ledger(
        &self,
        connection: Id<crate::domain::ids::Connection>,
    ) -> (i64, i64) {
        sqlx::query_as(
            "SELECT coalesce(sum(reserved), 0)::bigint, coalesce(sum(used), 0)::bigint
               FROM connection_usage WHERE connection_id = $1",
        )
        .bind(connection.uuid())
        .fetch_one(self.system.pool())
        .await
        .expect("the ledger")
    }
}

/// The report of `claimed`, which the transport answered with `answer` after a Start that marked
/// the submission at `started`: an SMTP answer, its diagnostic the reply a server would give.
pub(crate) fn answered(
    claimed: &crate::delivery::claim::Claimed,
    answer: crate::domain::policy::delivery::Answer,
    started: crate::domain::time::Timestamp,
) -> crate::delivery::finish::Report {
    use crate::domain::policy::delivery::Answer;
    let diagnostic = match answer {
        Answer::Accepted => "250 2.0.0 OK",
        Answer::Refused(_) => "451 4.3.0 Try again later",
    };
    crate::delivery::finish::Report {
        message: claimed.message,
        generation: claimed.generation,
        reported: crate::delivery::finish::Reported::Answered(Box::new(
            crate::delivery::finish::Answered {
                answer,
                source: crate::domain::policy::delivery::Source::Smtp,
                started: Some(started),
                diagnostic: diagnostic.to_owned(),
                provider_message_id: None,
                recipients: Vec::new(),
                refused: Vec::new(),
            },
        )),
    }
}

/// A refusal of the whole submission with no reply code: `failure` for the message, concerning
/// `scope`, for `cause`, at `DATA`.
pub(crate) fn refusal(
    failure: crate::domain::policy::delivery::Failure,
    scope: crate::domain::policy::delivery::RefusalScope,
    cause: crate::domain::policy::delivery::Cause,
) -> crate::domain::policy::delivery::Answer {
    crate::domain::policy::delivery::Answer::Refused(crate::domain::policy::delivery::Refusal {
        failure,
        phase: crate::domain::policy::delivery::Phase::Data,
        scope,
        cause,
        code: None,
        status: None,
        retry_after: None,
    })
}

/// A job runner whose mail-provider HTTP calls reach the test's fake origin, with offline DNS.
/// The supplied registry determines the jobs it can claim; it holds only test credentials.
pub(crate) fn provider_runner(
    test: &TestDb,
    registry: crate::jobs::Registry,
    origin: &str,
) -> crate::jobs::runner::Harness {
    let mut env = http::Extensions::new();
    env.insert(keys());
    env.insert(
        crate::senders::Env::assemble(
            crate::senders::Settings {
                http: norbelys_mail::http::HttpClient::rebased(origin).unwrap(),
                ..crate::senders::Settings::for_tests()
            },
            crate::dns::Resolver::offline(),
            norbelys_mail::net::AddressPolicy::Any,
        )
        .unwrap(),
    );
    crate::jobs::runner::Harness::new(
        test.worker.clone(),
        test.system.clone(),
        registry,
        env,
        "provider-test",
    )
}

/// Waits until PostgreSQL reports a session blocked by the transaction on `blocker`.
/// A bounded wait makes concurrency tests prove the lock was reached before releasing it,
/// independently of host scheduling or the database VM's wall clock.
pub(crate) async fn wait_for_lock(test: &TestDb, blocker: i32) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))"
            ).bind(blocker).fetch_one(test.system.pool()).await.unwrap();
            if waiting { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("a session waits for the held lock");
}
