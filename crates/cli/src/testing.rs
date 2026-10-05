//! Test support: a scripted fake HTTP server on the loopback interface, a scratch directory
//! per test, and a way to run the CLI in process with its output captured.
//!
//! The fake server answers every request with the next reply of its script, whatever the path,
//! and records each request; a test then asserts what the CLI sent. One script per server keeps
//! the order of a conversation (poll, poll, token) explicit in the test that expects it. A
//! request after the script ran out gets a `500`, which fails the test that did not expect it.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::output::Terminal;

/// One scripted answer.
#[derive(Clone)]
pub struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
    headers: Vec<(&'static str, String)>,
    delay: Duration,
}

impl Reply {
    /// A JSON answer.
    pub fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string(),
            headers: Vec::new(),
            delay: Duration::ZERO,
        }
    }

    /// A problem answer (`application/problem+json`).
    pub fn problem(status: u16, body: Value) -> Self {
        Self {
            content_type: "application/problem+json",
            ..Self::json(status, body)
        }
    }

    /// The same answer with one more header.
    pub fn with_header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }

    /// The same answer, sent after `delay`.
    pub fn after(self, delay: Duration) -> Self {
        Self { delay, ..self }
    }
}

/// A request the fake server received.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Seen {
    /// A header's value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// The first value of a query parameter.
    pub fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The body as form fields.
    pub fn form(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(&self.body)
            .into_owned()
            .collect()
    }

    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

#[derive(Default)]
struct Script {
    replies: VecDeque<Reply>,
    seen: Vec<Seen>,
}

/// A scripted HTTP server on `127.0.0.1`, serving until the test's runtime ends.
pub struct Fake {
    /// Its base URL, such as `http://127.0.0.1:50123`.
    pub url: String,
    script: Arc<Mutex<Script>>,
}

impl Fake {
    /// Starts a server that answers with `replies`, in order.
    pub async fn start(replies: Vec<Reply>) -> Self {
        let script = Arc::new(Mutex::new(Script {
            replies: replies.into(),
            seen: Vec::new(),
        }));
        let router = Router::new().fallback(answer).with_state(script.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        Self { url, script }
    }

    /// The requests received so far, in order.
    pub fn seen(&self) -> Vec<Seen> {
        self.script.lock().unwrap().seen.clone()
    }
}

async fn answer(
    State(script): State<Arc<Mutex<Script>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let reply = {
        let mut script = script.lock().unwrap();
        script.seen.push(Seen {
            method,
            path: uri.path().to_owned(),
            query: url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
                .into_owned()
                .collect(),
            headers,
            body: body.to_vec(),
        });
        script.replies.pop_front()
    };
    let Some(reply) = reply else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "no scripted reply left").into_response();
    };
    tokio::time::sleep(reply.delay).await;
    let status = StatusCode::from_u16(reply.status).unwrap();
    if reply.body == "null" {
        return status.into_response();
    }
    let mut response = (
        status,
        [(header::CONTENT_TYPE, reply.content_type)],
        reply.body,
    )
        .into_response();
    for (name, value) in reply.headers {
        response
            .headers_mut()
            .insert(name, header::HeaderValue::from_str(&value).unwrap());
    }
    response
}

/// A directory of the test's own under the system's temporary directory, removed afterwards.
pub struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("norbelys-cli-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    /// The configuration file's path, inside a directory the CLI creates.
    pub fn config(&self) -> PathBuf {
        self.dir.join("norbelys").join("config.json")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// What one in-process run of the CLI produced.
pub struct Ran {
    pub code: u8,
    pub out: String,
    pub err: String,
}

/// Runs `norbelys <args>` in process against the configuration file of `scratch`, with its
/// output captured and the browser left closed.
pub async fn run(scratch: &Scratch, args: &[&str]) -> Ran {
    execute(scratch, args, None).await
}

/// Runs `norbelys <args> listen --forward-to <target>` like [`run`], stopping after `events`
/// forwarded events.
pub async fn listen(scratch: &Scratch, args: &[&str], target: &str, events: usize) -> Ran {
    let args = [args, &["listen", "--forward-to", target]].concat();
    execute(scratch, &args, Some(events)).await
}

async fn execute(scratch: &Scratch, args: &[&str], listen_limit: Option<usize>) -> Ran {
    let config = scratch.config();
    let mut argv: Vec<OsString> = vec!["norbelys".into(), "--config".into(), config.into()];
    argv.extend(args.iter().map(OsString::from));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut terminal = Terminal {
        out: &mut out,
        err: &mut err,
        open: |_| {},
    };
    let code = crate::execute(argv, &mut terminal, listen_limit).await;
    Ran {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}
