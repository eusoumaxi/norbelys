//! Fakes for this crate's protocol tests: scripted SMTP, IMAP and HTTP servers on the loopback
//! interface, and the connector and HTTP client that reach them.
//!
//! Each fake binds `127.0.0.1:0`, so tests run in parallel without sharing a port, and records
//! every line or request it receives in order, so a test can assert what the client sent and,
//! just as important, what it did not send (no `DATA` after a deadline stop, no second
//! connection when a session is reused). A test supplies the script: a function from what the
//! client sent to what the server answers. Nothing here touches the network beyond loopback.

use std::sync::{Arc, Mutex, PoisonError};

use hickory_resolver::Resolver;
use hickory_resolver::config::ResolverConfig;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;

use crate::net::{AddressPolicy, Connector};

pub(crate) mod samples;

/// A connector whose resolver has no name server: the fakes are reached by IP literal, which
/// resolves without a query.
pub(crate) fn connector(policy: AddressPolicy) -> Connector {
    let resolver =
        Resolver::builder_with_config(ResolverConfig::default(), TokioRuntimeProvider::new())
            .build()
            .expect("a resolver without name servers builds");
    Connector::new(resolver, policy).expect("the TLS configuration builds")
}

/// What a fake received, in order.
#[derive(Clone, Default)]
pub(crate) struct Transcript(Arc<Mutex<Vec<String>>>);

impl Transcript {
    /// Every entry so far.
    pub(crate) fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether any entry starts with `prefix` (ASCII case-insensitive).
    pub(crate) fn has(&self, prefix: &str) -> bool {
        let prefix = prefix.to_ascii_lowercase();
        self.lines()
            .iter()
            .any(|line| line.to_ascii_lowercase().starts_with(&prefix))
    }

    /// How many entries equal `entry`.
    pub(crate) fn count(&self, entry: &str) -> usize {
        self.lines().iter().filter(|line| *line == entry).count()
    }

    fn push(&self, entry: String) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry);
    }
}

/// What the fake SMTP server does after a line.
pub(crate) enum Smtp {
    /// Answer with these lines (CRLF added; multi-line replies separated by `\r\n`).
    Reply(String),
    /// Close the connection without answering.
    Close,
    /// Keep the connection open without ever answering.
    Silent,
}

/// A reply.
pub(crate) fn reply(text: &str) -> Smtp {
    Smtp::Reply(text.to_owned())
}

/// The answers every SMTP script shares: an `EHLO` advertising `AUTH PLAIN LOGIN XOAUTH2`,
/// `8BITMIME` and `SMTPUTF8`, a successful `AUTH`, `NOOP`, `RSET` and `QUIT`, and `250` to
/// `MAIL`, `RCPT` and the end of the content, `354` to `DATA`.
pub(crate) fn smtp_default(line: &str) -> Smtp {
    let upper = line.to_ascii_uppercase();
    if upper.starts_with("EHLO") {
        reply("250-fake.test\r\n250-AUTH PLAIN LOGIN XOAUTH2\r\n250-8BITMIME\r\n250 SMTPUTF8")
    } else if upper.starts_with("AUTH") {
        reply("235 2.7.0 Authentication successful")
    } else if upper.starts_with("DATA") {
        reply("354 End data with <CR><LF>.<CR><LF>")
    } else if line == "." {
        reply("250 2.0.0 Ok: queued as 4ABC123")
    } else if upper.starts_with("QUIT") {
        reply("221 2.0.0 Bye")
    } else {
        reply("250 2.0.0 OK")
    }
}

/// Starts a scripted SMTP server: it greets with `220`, records every line (`connect` for each
/// connection, `.` for the end of the content, and the content itself as `content:` plus the
/// bytes), and answers each line with `script`.
pub(crate) async fn smtp_server<F>(script: F) -> (u16, Transcript)
where
    F: Fn(&str) -> Smtp + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback binds");
    let port = listener.local_addr().expect("a bound address").port();
    let transcript = Transcript::default();
    let script = Arc::new(script);
    let recorded = transcript.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let script = Arc::clone(&script);
            let transcript = recorded.clone();
            tokio::spawn(async move {
                transcript.push("connect".to_owned());
                let (read, mut write) = socket.into_split();
                let mut lines = BufReader::new(read);
                if write.write_all(b"220 fake.test ESMTP\r\n").await.is_err() {
                    return;
                }
                let mut line = String::new();
                loop {
                    line.clear();
                    if lines.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let command = line.trim_end_matches(['\r', '\n']).to_owned();
                    transcript.push(command.clone());
                    let mut answer = script(&command);
                    let accepted_data = command.eq_ignore_ascii_case("DATA")
                        && matches!(&answer, Smtp::Reply(text) if text.starts_with("354"));
                    if accepted_data {
                        if let Smtp::Reply(text) = &answer {
                            let _ = write.write_all(format!("{text}\r\n").as_bytes()).await;
                        }
                        let mut content = String::new();
                        loop {
                            line.clear();
                            if lines.read_line(&mut line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            if line.trim_end_matches(['\r', '\n']) == "." {
                                break;
                            }
                            content.push_str(&line);
                        }
                        transcript.push(format!("content:{content}"));
                        transcript.push(".".to_owned());
                        answer = script(".");
                    }
                    match answer {
                        Smtp::Reply(text) => {
                            if write
                                .write_all(format!("{text}\r\n").as_bytes())
                                .await
                                .is_err()
                            {
                                return;
                            }
                            if command.eq_ignore_ascii_case("QUIT") {
                                return;
                            }
                        }
                        Smtp::Close => return,
                        Smtp::Silent => {
                            tokio::time::sleep(std::time::Duration::from_secs(3_600)).await;
                            return;
                        }
                    }
                }
            });
        }
    });
    (port, transcript)
}

/// What the fake IMAP server answers to one tagged command.
pub(crate) struct Imap {
    /// Untagged lines, each ending in CRLF, literals included.
    pub(crate) untagged: String,
    /// The tagged status after the tag: `OK …`, `NO …`, `BAD …`.
    pub(crate) status: String,
}

/// An answer with untagged lines and `OK`.
pub(crate) fn imap_ok(untagged: &str) -> Imap {
    Imap {
        untagged: untagged.to_owned(),
        status: "OK done".to_owned(),
    }
}

/// Starts a scripted IMAP server: it greets with `* OK`, records each command without its tag
/// (for `AUTHENTICATE`, the client's response line is appended after a space), answers with
/// `script`, and closes after `LOGOUT`.
pub(crate) async fn imap_server<F>(script: F) -> (u16, Transcript)
where
    F: Fn(&str) -> Imap + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback binds");
    let port = listener.local_addr().expect("a bound address").port();
    let transcript = Transcript::default();
    let script = Arc::new(script);
    let recorded = transcript.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let script = Arc::clone(&script);
            let transcript = recorded.clone();
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut lines = BufReader::new(read);
                if write
                    .write_all(b"* OK [CAPABILITY IMAP4rev1] fake ready\r\n")
                    .await
                    .is_err()
                {
                    return;
                }
                let mut line = String::new();
                loop {
                    line.clear();
                    if lines.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let text = line.trim_end_matches(['\r', '\n']).to_owned();
                    let Some((tag, command)) = text.split_once(' ') else {
                        return;
                    };
                    let mut command = command.to_owned();
                    if command.to_ascii_uppercase().starts_with("AUTHENTICATE") {
                        if write.write_all(b"+ \r\n").await.is_err() {
                            return;
                        }
                        line.clear();
                        if lines.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        command = format!("{command} {}", line.trim_end_matches(['\r', '\n']));
                    }
                    transcript.push(command.clone());
                    let answer = script(&command);
                    let out = format!("{}{tag} {}\r\n", answer.untagged, answer.status);
                    if write.write_all(out.as_bytes()).await.is_err() {
                        return;
                    }
                    if command.eq_ignore_ascii_case("LOGOUT") {
                        return;
                    }
                }
            });
        }
    });
    (port, transcript)
}

/// One request the fake HTTP server received.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    /// `GET`, `POST`.
    pub(crate) method: String,
    /// The path and query, as sent.
    pub(crate) target: String,
    /// The headers, names lowercased.
    pub(crate) headers: Vec<(String, String)>,
    /// The body.
    pub(crate) body: Vec<u8>,
}

impl Request {
    /// The first value of header `name` (lowercase).
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }

    /// The decoded value of query parameter `name`.
    pub(crate) fn query(&self, name: &str) -> Option<String> {
        let url = url::Url::parse(&format!("http://fake.test{}", self.target)).ok()?;
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    /// The path, without the query.
    pub(crate) fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }
}

/// What the fake HTTP server answers.
pub(crate) struct Response {
    /// The status code.
    pub(crate) status: u16,
    /// Extra headers.
    pub(crate) headers: Vec<(&'static str, String)>,
    /// The body.
    pub(crate) body: Vec<u8>,
}

impl Response {
    /// No answer at all: the connection closes after the request was read, as a reset after
    /// the body was sent looks to the client.
    pub(crate) fn hang_up() -> Self {
        Self {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// A JSON answer.
    pub(crate) fn json(status: u16, body: &serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type", "application/json".to_owned())],
            body: body.to_string().into_bytes(),
        }
    }

    /// An answer with a raw body.
    pub(crate) fn raw(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }

    /// The same answer with one more header.
    pub(crate) fn with(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }
}

/// The requests a fake HTTP server received.
#[derive(Clone, Default)]
pub(crate) struct Requests(Arc<Mutex<Vec<Request>>>);

impl Requests {
    /// Every request so far.
    pub(crate) fn all(&self) -> Vec<Request> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Starts a fake HTTP/1.1 server answering each request with `handler(request, origin)`, where
/// `origin` is the server's own `http://127.0.0.1:<port>` (for links it returns). Every answer
/// closes its connection.
pub(crate) async fn http_server<F>(handler: F) -> (String, Requests)
where
    F: Fn(&Request, &str) -> Response + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback binds");
    let origin = format!("http://{}", listener.local_addr().expect("a bound address"));
    let requests = Requests::default();
    let handler = Arc::new(handler);
    let recorded = requests.clone();
    let own = origin.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let handler = Arc::clone(&handler);
            let requests = recorded.clone();
            let origin = own.clone();
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut reader = BufReader::new(read);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_owned();
                let target = parts.next().unwrap_or_default().to_owned();
                let mut headers = Vec::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let header = line.trim_end_matches(['\r', '\n']);
                    if header.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
                    }
                }
                let length = headers
                    .iter()
                    .find(|(name, _)| name == "content-length")
                    .and_then(|(_, value)| value.parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).await.is_err() {
                    return;
                }
                let request = Request {
                    method,
                    target,
                    headers,
                    body,
                };
                let response = handler(&request, &origin);
                requests
                    .0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(request);
                if response.status == 0 {
                    // Hang up: the request was read, no answer is sent.
                    return;
                }
                let mut head = format!(
                    "HTTP/1.1 {} Fake\r\nContent-Length: {}\r\nConnection: close\r\n",
                    response.status,
                    response.body.len()
                );
                for (name, value) in &response.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str("\r\n");
                let _ = write.write_all(head.as_bytes()).await;
                let _ = write.write_all(&response.body).await;
                let _ = write.shutdown().await;
            });
        }
    });
    (origin, requests)
}

/// An `http://127.0.0.1:<port>` origin nothing listens on: connecting is refused.
pub(crate) async fn refused_origin() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback binds");
    let origin = format!("http://{}", listener.local_addr().expect("a bound address"));
    drop(listener);
    origin
}
