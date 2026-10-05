//! SQL over HTTP v3, shared by Turso and a self-hosted libSQL server.
//!
//! A connection owns one stream and always consumes its newest baton. Requests never retry
//! here: a lost response may follow a committed write. The caller retries only operations
//! whose keys make them idempotent. A dropped transaction explicitly rolls back; the server expires abandoned idle streams.
//! Bodies are bounded, redirects are refused, and bearer credentials are never logged.
//! See <https://github.com/tursodatabase/turso/blob/main/serverless/PROTOCOL.md>.

use std::cell::{Cell, RefCell};
use std::io::Read as _;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use rusqlite::types::Value;
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use url::Url;

use super::{Error, Result};

/// Maximum encoded response, independent of the size of the remote history.
const MAX_RESPONSE: u64 = 4 * 1024 * 1024;
/// Maximum encoded request; batches must stay within it.
const MAX_REQUEST: usize = 2 * 1024 * 1024;

/// The connection's HTTP transport and node namespace; cloned transports share their pool.
pub(crate) struct Remote {
    client: Client,
    origin: Url,
    prefix: String,
    stream: RefCell<Stream>,
    in_transaction: Cell<bool>,
}

#[derive(Clone)]
struct Stream {
    url: Url,
    baton: Option<String>,
}

/// A SQL value encoded without losing the precision of a 64-bit integer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum WireValue {
    Null,
    Integer { value: String },
    Float { value: f64 },
    Text { value: String },
    Blob { base64: String },
}

impl From<Value> for WireValue {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Integer(value) => Self::Integer {
                value: value.to_string(),
            },
            Value::Real(value) => Self::Float { value },
            Value::Text(value) => Self::Text { value },
            Value::Blob(value) => Self::Blob {
                base64: STANDARD.encode(value),
            },
        }
    }
}

impl TryFrom<WireValue> for Value {
    type Error = Error;

    fn try_from(value: WireValue) -> Result<Self> {
        match value {
            WireValue::Null => Ok(Self::Null),
            WireValue::Integer { value } => value
                .parse()
                .map(Self::Integer)
                .map_err(|_| Error::Protocol("invalid SQL integer")),
            WireValue::Float { value } if value.is_finite() => Ok(Self::Real(value)),
            WireValue::Float { .. } => Err(Error::Protocol("non-finite SQL number")),
            WireValue::Text { value } => Ok(Self::Text(value)),
            WireValue::Blob { base64 } => STANDARD
                .decode(base64)
                .map(Self::Blob)
                .map_err(|_| Error::Protocol("invalid SQL blob")),
        }
    }
}

/// A statement and its bound values; SQL and values are never interpolated together.
#[derive(Serialize)]
pub(crate) struct Stmt {
    sql: String,
    args: Vec<WireValue>,
    want_rows: bool,
}

/// The successful result of one statement.
#[derive(Default, Deserialize)]
pub(crate) struct StatementResult {
    #[serde(default)]
    pub(crate) rows: Vec<Vec<WireValue>>,
    #[serde(default)]
    pub(crate) affected_row_count: u64,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    Execute { stmt: Stmt },
    Sequence { sql: String },
    Close,
}

#[derive(Deserialize)]
struct Pipeline {
    baton: Option<String>,
    base_url: Option<String>,
    results: Vec<Outcome>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Outcome {
    Ok { response: Response },
    Error { error: SqlError },
}

#[derive(Deserialize)]
struct SqlError {
    code: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response {
    Execute { result: StatementResult },
    Sequence,
    Close,
}

impl Remote {
    /// Builds a blocking transport. Call this outside Tokio or on its blocking pool.
    pub(crate) fn new(raw: &str, token: &SecretString, node: &str) -> Result<Self> {
        let origin = endpoint(raw)?;
        let mut headers = HeaderMap::new();
        if !token.expose_secret().is_empty() {
            let mut value = HeaderValue::from_str(&format!("Bearer {}", token.expose_secret()))
                .map_err(|_| Error::Protocol("invalid database credential"))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        let client = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("norbelys-smtp/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            client,
            prefix: format!("nb_{}_", crate::crypto::sha256_hex(node.as_bytes())),
            stream: RefCell::new(Stream {
                url: origin.clone(),
                baton: None,
            }),
            origin,
            in_transaction: Cell::new(false),
        })
    }

    /// Returns a fresh stream using the same transport and namespace.
    pub(crate) fn fresh(&self) -> Self {
        Self {
            client: self.client.clone(),
            origin: self.origin.clone(),
            prefix: self.prefix.clone(),
            stream: RefCell::new(Stream {
                url: self.origin.clone(),
                baton: None,
            }),
            in_transaction: Cell::new(false),
        }
    }

    /// Executes a single statement on the stream.
    pub(crate) fn execute(&self, sql: &str, values: Vec<Value>) -> Result<StatementResult> {
        let mut results = self.operation(
            vec![Request::Execute {
                stmt: self.statement(sql, values),
            }],
            self.in_transaction.get(),
        )?;
        match results.pop() {
            Some(Response::Execute { result }) => Ok(result),
            _ => Err(Error::Protocol("missing SQL result")),
        }
    }

    /// Executes prepared statements in one HTTP request, inside the caller's transaction.
    pub(crate) fn many(&self, sql: &str, rows: Vec<Vec<Value>>) -> Result<()> {
        let requests = rows
            .into_iter()
            .map(|values| Request::Execute {
                stmt: self.statement(sql, values),
            })
            .collect();
        self.operation(requests, self.in_transaction.get())
            .map(|_| ())
    }

    /// Executes the schema or a trusted sequence; callers own its transaction boundary.
    pub(crate) fn sequence(&self, sql: &str) -> Result<()> {
        let begin = sql.trim().eq_ignore_ascii_case("BEGIN IMMEDIATE");
        let end = sql.trim().eq_ignore_ascii_case("COMMIT")
            || sql.trim().eq_ignore_ascii_case("ROLLBACK");
        let keep = begin || (self.in_transaction.get() && !end);
        let result = self
            .operation(
                vec![Request::Sequence {
                    sql: namespace(sql, &self.prefix),
                }],
                keep,
            )
            .map(|_| ());
        if result.is_ok() || end {
            self.in_transaction.set(keep);
        }
        result
    }

    /// Releases autocommit streams in the same request; only live transactions retain batons.
    fn operation(&self, mut requests: Vec<Request>, keep: bool) -> Result<Vec<Response>> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        if !keep {
            requests.push(Request::Close);
        }
        let mut results = self.pipeline(requests)?;
        if !keep && !matches!(results.pop(), Some(Response::Close)) {
            return Err(Error::Protocol("SQL stream did not confirm close"));
        }
        Ok(results)
    }

    fn statement(&self, sql: &str, values: Vec<Value>) -> Stmt {
        Stmt {
            sql: namespace(sql, &self.prefix),
            args: values.into_iter().map(WireValue::from).collect(),
            want_rows: true,
        }
    }

    fn pipeline(&self, requests: Vec<Request>) -> Result<Vec<Response>> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let count = requests.len();
        let mut stream = self.stream.borrow_mut();
        let body = serde_json::to_vec(
            &serde_json::json!({ "baton": stream.baton, "requests": requests }),
        )?;
        if body.len() > MAX_REQUEST {
            return Err(Error::Protocol("SQL request exceeds 2 MiB"));
        }
        let response = match self
            .client
            .post(stream.url.clone())
            .header("content-type", "application/json")
            .body(body)
            .send()
        {
            Ok(response) => response,
            Err(error) => {
                stream.baton = None;
                return Err(error.into());
            }
        };
        if !response.status().is_success() {
            stream.baton = None;
            return Err(Error::Status(response.status().as_u16()));
        }
        let mut bytes = Vec::new();
        if let Err(error) = response.take(MAX_RESPONSE + 1).read_to_end(&mut bytes) {
            stream.baton = None;
            return Err(error.into());
        }
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_RESPONSE {
            stream.baton = None;
            return Err(Error::Protocol("SQL response exceeds 4 MiB"));
        }
        let answer: Pipeline = match serde_json::from_slice(&bytes) {
            Ok(answer) => answer,
            Err(error) => {
                stream.baton = None;
                return Err(error.into());
            }
        };
        stream.baton = answer.baton;
        if let Some(base) = answer.base_url {
            let next = endpoint(&base)?;
            let same = next.origin() == self.origin.origin();
            let cloud = self.origin.scheme() == "https"
                && next.scheme() == "https"
                && self
                    .origin
                    .host_str()
                    .is_some_and(|host| host.ends_with(".turso.io"))
                && next
                    .host_str()
                    .is_some_and(|host| host.ends_with(".turso.io"));
            if !same && !cloud {
                return Err(Error::Protocol("SQL stream changed to an untrusted origin"));
            }
            stream.url = next;
        }
        if answer.results.len() != count {
            return Err(Error::Protocol("SQL result count differs from request"));
        }
        answer
            .results
            .into_iter()
            .map(|result| match result {
                Outcome::Ok { response } => Ok(response),
                Outcome::Error { error } => Err(Error::Sql(
                    error.code.unwrap_or_else(|| "UNKNOWN".to_owned()),
                )),
            })
            .collect()
    }
}

/// Validates and normalizes an endpoint, without allowing credentials inside its URL.
fn endpoint(raw: &str) -> Result<Url> {
    let normalized = raw
        .strip_prefix("libsql://")
        .or_else(|| raw.strip_prefix("turso://"))
        .map_or_else(|| raw.to_owned(), |host| format!("https://{host}"));
    let mut url = Url::parse(&normalized).map_err(|_| Error::Protocol("invalid database URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Protocol(
            "database URL must be HTTP(S), without credentials, query or fragment",
        ));
    }
    url.set_path("/v3/pipeline");
    Ok(url)
}

/// Qualifies only table and index identifiers from the closed schema inventory. Strings,
/// columns and values are untouched. A node's stable digest cannot become SQL syntax.
fn namespace(sql: &str, prefix: &str) -> String {
    const TABLES: &[&str] = &[
        "domains",
        "accounts",
        "routes",
        "account_routes",
        "submissions",
        "returns",
        "events",
        "pending_changes",
        "cursors",
    ];
    const INDEXES: &[&str] = &[
        "events_due",
        "events_route_due",
        "events_created",
        "submissions_message",
    ];
    let mut output = String::with_capacity(sql.len());
    let mut chars = sql.char_indices().peekable();
    let mut expected = false;
    let mut index = false;
    let mut quoted = None;
    while let Some((start, ch)) = chars.next() {
        if let Some(quote) = quoted {
            output.push(ch);
            if ch == quote {
                if chars.peek().is_some_and(|(_, next)| *next == quote) {
                    if let Some((_, next)) = chars.next() {
                        output.push(next);
                    }
                } else {
                    quoted = None;
                }
            }
            continue;
        }
        if matches!(ch, '\'' | '"') {
            quoted = Some(ch);
            output.push(ch);
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == '_' {
            let mut end = start + ch.len_utf8();
            while chars
                .peek()
                .is_some_and(|(_, next)| next.is_ascii_alphanumeric() || *next == '_')
            {
                if let Some((at, next)) = chars.next() {
                    end = at + next.len_utf8();
                }
            }
            let word = sql.get(start..end).unwrap_or_default();
            let upper = word.to_ascii_uppercase();
            if expected && !matches!(upper.as_str(), "IF" | "NOT" | "EXISTS") {
                if TABLES.contains(&word) || INDEXES.contains(&word) {
                    output.push_str(prefix);
                }
                expected = false;
            }
            output.push_str(word);
            match upper.as_str() {
                "FROM" | "JOIN" | "INTO" | "UPDATE" | "TABLE" | "REFERENCES" => expected = true,
                "INDEX" => {
                    expected = true;
                    index = true;
                }
                "ON" if index => {
                    expected = true;
                    index = false;
                }
                _ => {}
            }
        } else {
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Node isolation qualifies tables and indexes while preserving literals and columns.
    #[test]
    fn namespaces_schema_identifiers_only() {
        assert_eq!(
            namespace(
                "CREATE INDEX IF NOT EXISTS events_due ON events (done)",
                "a_"
            ),
            "CREATE INDEX IF NOT EXISTS a_events_due ON a_events (done)"
        );
        assert_eq!(
            namespace(
                "SELECT events, 'FROM accounts', 'it''s events' FROM events",
                "b_"
            ),
            "SELECT events, 'FROM accounts', 'it''s events' FROM b_events"
        );
        assert_eq!(
            namespace(
                "INSERT INTO returns SELECT token FROM submissions ON CONFLICT (token) DO NOTHING",
                "a_"
            ),
            "INSERT INTO a_returns SELECT token FROM a_submissions ON CONFLICT (token) DO NOTHING"
        );
    }

    /// Cloud URL schemes normalize to HTTPS and refuse credentials in URLs.
    #[test]
    fn endpoints_do_not_carry_secrets() {
        assert_eq!(
            endpoint("libsql://example.turso.io").unwrap().as_str(),
            "https://example.turso.io/v3/pipeline"
        );
        for url in [
            "file:local.db",
            "https://user:secret@example.com",
            "https://example.com?token=secret",
        ] {
            assert!(endpoint(url).is_err());
        }
    }
}
