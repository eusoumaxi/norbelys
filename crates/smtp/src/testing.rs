//! Shared test support, compiled only for tests: a temporary directory removed on drop, an
//! in-memory database with the schema, and the control API in process with requests signed
//! per Standard Webhooks. Tests use real SQLite, real files and real sockets on loopback;
//! nothing is mocked.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::db::Connection;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hickory_resolver::TokioResolver;
use tower::ServiceExt as _;

use crate::control::{self, Settings, State};
use crate::crypto::{self, Keys, WebhookKey};
use crate::db::{self, Db};

/// The installation secret of every test: the Standard Webhooks reference libraries' example
/// key, so signatures can be checked against their published vector.
pub const SECRET: &str = concat!("whsec_", "MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw");

/// A directory under the system's temporary directory, removed with everything in it on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    /// A new, empty, uniquely named directory.
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "norbelys-smtp-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create a temporary directory");
        Self(path)
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// A path inside the directory.
    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An in-memory database with the schema.
pub fn memory() -> Connection {
    db::open(Path::new(":memory:")).expect("open an in-memory database")
}

/// The control API's state over a database file in `dir`, with fixed settings: mail host
/// `mail.example.com`, address `192.0.2.10`, selector `norbelys`, evidence host `localhost`.
pub fn state(dir: &TempDir) -> State {
    State {
        db: Db::open(&dir.join("smtp.sqlite")).expect("open the database"),
        keys: Arc::new(Keys::from_secret(SECRET).expect("the test secret is valid")),
        settings: Arc::new(Settings {
            mail_host: "mail.example.com".to_owned(),
            public_ipv4: Ipv4Addr::new(192, 0, 2, 10),
            dkim_selector: "norbelys".to_owned(),
            evidence_hosts: vec!["localhost".to_owned()],
            trigger: dir.join("provision.trigger"),
        }),
        seen: Arc::new(Mutex::new(HashMap::new())),
        resolver: TokioResolver::builder_tokio()
            .expect("read the host's DNS configuration")
            .build()
            .expect("build a resolver"),
    }
}

/// A request signed with [`SECRET`] under a fresh `webhook-id` and the current time.
pub fn signed(method: &str, uri: &str, body: &str) -> Request<Body> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let id = format!("msg_test_{}", NEXT.fetch_add(1, Ordering::Relaxed));
    signed_as(method, uri, body, &id, jiff::Timestamp::now().as_second())
}

/// A request signed with [`SECRET`] under the given id and timestamp.
pub fn signed_as(method: &str, uri: &str, body: &str, id: &str, timestamp: i64) -> Request<Body> {
    let key = WebhookKey::new(&crypto::decode_secret(SECRET).expect("the test secret is valid"));
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("webhook-id", id)
        .header("norbelys-control-version", "2")
        .header("webhook-timestamp", timestamp.to_string())
        .header(
            "webhook-signature",
            key.sign(
                id,
                timestamp,
                &norbelys_mail::webhooks::control_payload(method, uri, body.as_bytes()),
            ),
        )
        .body(Body::from(body.to_owned()))
        .expect("a valid request")
}

/// Sends `request` through the router in process; returns the status and the JSON body
/// (`null` when the body is empty).
pub async fn call(router: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read the body");
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("a JSON body")
    };
    (status, json)
}

/// The control router over [`state`].
pub fn router(dir: &TempDir) -> (Router, State) {
    let state = state(dir);
    (control::router(state.clone()), state)
}

/// Marks `domain` verified directly in the database: tests cannot publish DNS records.
pub fn verify_domain(dir: &TempDir, domain: &str) {
    let conn = db::open(&dir.join("smtp.sqlite")).expect("open the database");
    conn.execute(
        "UPDATE domains SET verified_at = '2026-10-01T00:00:00Z' WHERE name = ?1",
        [domain],
    )
    .expect("mark the domain verified");
}

/// A loopback SQL-over-HTTP v3 endpoint backed by a real SQLite database. Each rotating baton
/// owns an independent connection, so transaction and replay tests exercise the wire protocol.
pub struct SqlServer {
    /// The public endpoint configuration of this fixture; its bearer token is a test constant.
    pub args: crate::config::DatabaseArgs,
    /// Causes the next committed transaction to lose its HTTP acknowledgement.
    pub lose_commit: Arc<std::sync::atomic::AtomicBool>,
    /// Refuses new HTTP operations while false, allowing outage and admission tests.
    pub available: Arc<std::sync::atomic::AtomicBool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    _dir: TempDir,
}

impl SqlServer {
    /// Starts a real endpoint in a separate runtime; synchronous clients never block its server.
    pub fn new() -> Self {
        let dir = TempDir::new();
        let file = dir.join("remote.sqlite");
        let lose_commit = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let available = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let lost = Arc::clone(&lose_commit);
        let up = Arc::clone(&available);
        let (ready, address) = std::sync::mpsc::channel();
        let (stop, stopping) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async move {
                let streams: Arc<Mutex<HashMap<String, Arc<Mutex<rusqlite::Connection>>>>> = Arc::default();
                let app = Router::new().route("/v3/pipeline", axum::routing::post(move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<serde_json::Value>| {
                    let streams = Arc::clone(&streams);
                    let file = file.clone();
                    let lost = Arc::clone(&lost);
                    let up = Arc::clone(&up);
                    async move {
                        if headers.get("authorization").and_then(|value| value.to_str().ok()) != Some("Bearer sql-test") {
                            return (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({})));
                        }
                        if !up.load(Ordering::SeqCst) {
                            return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({})));
                        }
                        tokio::task::spawn_blocking(move || sql_pipeline(&streams, &file, &lost, &body)).await.unwrap()
                    }
                }));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                ready.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app).with_graceful_shutdown(async move { let _ = stopping.await; }).await.unwrap();
            });
        });
        Self {
            args: crate::config::DatabaseArgs {
                database_url: format!("http://{}", address.recv().unwrap()),
                database_token: secrecy::SecretString::from("sql-test".to_owned()),
            },
            lose_commit,
            available,
            stop: Some(stop),
            thread: Some(thread),
            _dir: dir,
        }
    }
}

impl Drop for SqlServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Executes one pipeline and rotates the baton exactly once. SQL errors remain HTTP 200;
/// the lost-commit gate returns HTTP 503 after SQLite has committed the entire transaction.
fn sql_pipeline(
    streams: &Mutex<HashMap<String, Arc<Mutex<rusqlite::Connection>>>>,
    file: &Path,
    lost: &std::sync::atomic::AtomicBool,
    body: &serde_json::Value,
) -> (StatusCode, axum::Json<serde_json::Value>) {
    use base64::Engine as _;
    use rusqlite::types::Value;
    let prior = body["baton"].as_str();
    let source = {
        let mut streams = streams.lock().unwrap();
        if let Some(baton) = prior {
            match streams.remove(baton) {
                Some(conn) => conn,
                None => return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({}))),
            }
        } else {
            let conn = rusqlite::Connection::open(file).unwrap();
            conn.busy_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            Arc::new(Mutex::new(conn))
        }
    };
    let conn = source.lock().unwrap();
    let mut committed = false;
    let mut closed = false;
    let results: Vec<serde_json::Value> = body["requests"].as_array().unwrap().iter().map(|request| {
        let result: rusqlite::Result<serde_json::Value> = (|| {
            match request["type"].as_str().unwrap() {
                "sequence" => {
                    let sql = request["sql"].as_str().unwrap();
                    conn.execute_batch(sql)?;
                    committed |= sql.eq_ignore_ascii_case("COMMIT");
                    Ok(serde_json::json!({"type": "sequence"}))
                }
                "execute" => {
                    let stmt = &request["stmt"];
                    let values: Vec<Value> = stmt["args"].as_array().unwrap().iter().map(|arg| match arg["type"].as_str().unwrap() {
                        "null" => Value::Null,
                        "integer" => Value::Integer(arg["value"].as_str().unwrap().parse().unwrap()),
                        "float" => Value::Real(arg["value"].as_f64().unwrap()),
                        "text" => Value::Text(arg["value"].as_str().unwrap().to_owned()),
                        "blob" => Value::Blob(base64::engine::general_purpose::STANDARD.decode(arg["base64"].as_str().unwrap()).unwrap()),
                        _ => unreachable!(),
                    }).collect();
                    let mut query = conn.prepare(stmt["sql"].as_str().unwrap())?;
                    let columns = query.column_count();
                    let mut rows = query.query(rusqlite::params_from_iter(values))?;
                    let mut records = Vec::new();
                    while let Some(row) = rows.next()? {
                        records.push((0..columns).map(|index| match row.get::<_, Value>(index).unwrap() {
                            Value::Null => serde_json::json!({"type":"null"}),
                            Value::Integer(value) => serde_json::json!({"type":"integer","value":value.to_string()}),
                            Value::Real(value) => serde_json::json!({"type":"float","value":value}),
                            Value::Text(value) => serde_json::json!({"type":"text","value":value}),
                            Value::Blob(value) => serde_json::json!({"type":"blob","base64":base64::engine::general_purpose::STANDARD.encode(value)}),
                        }).collect::<Vec<_>>());
                    }
                    Ok(serde_json::json!({"type":"execute", "result":{"rows":records,"affected_row_count":conn.changes()}}))
                }
                "close" => {
                    if !conn.is_autocommit() { conn.execute_batch("ROLLBACK")?; }
                    closed = true;
                    Ok(serde_json::json!({"type":"close"}))
                }
                _ => unreachable!(),
            }
        })();
        match result {
            Ok(response) => serde_json::json!({"type":"ok","response":response}),
            Err(error) => serde_json::json!({"type":"error","error":{"code":format!("{:?}",error.sqlite_error_code()),"message":error.to_string()}}),
        }
    }).collect();
    drop(conn);
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let baton = format!("baton-{}", NEXT.fetch_add(1, Ordering::SeqCst));
    if !closed {
        streams.lock().unwrap().insert(baton.clone(), source);
    }
    if committed && lost.swap(false, Ordering::SeqCst) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({})),
        );
    }
    (
        StatusCode::OK,
        axum::Json(
            serde_json::json!({"baton":if closed {None} else {Some(baton)},"base_url":null,"results":results}),
        ),
    )
}
