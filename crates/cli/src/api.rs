//! The API client: which API and which credential a run uses, sending requests with their
//! idempotency key, and reading answers as values or problems.
//!
//! # Which API, which credential
//!
//! The base URL is `--api-url`, else the profile's, else `https://api.norbelys.com`. The
//! credential is the first of: `--api-key` (or `NORBELYS_API_KEY`, for CI), the profile's API
//! key, the profile's CLI session (refreshed when it is about to expire). Without any, a command
//! that calls the API exits as unauthenticated and says how to log in. It is always sent as
//! `Authorization: Bearer`.
//!
//! # Idempotency
//!
//! Every `POST` and `PATCH` carries an `Idempotency-Key`: `--idempotency-key` when given, else
//! a new one for the run. The API applies a request once per key, so when an answer is lost on
//! the way back the error prints the key, and running the same command with it is safe: the
//! effect happens at most once and the stored answer comes back.
//!
//! # Answers
//!
//! A `2xx` answer is its JSON body (none for `204`). Anything else is a [`Problem`], read from
//! the RFC 9457 document the API answers with and printed by `output`; its HTTP status gives the
//! exit code. The client follows no redirect (the API never sends one, and a redirect could
//! carry the credential elsewhere), waits up to 10 seconds to connect and up to 60 for each
//! read, which covers the 25 seconds of a long poll.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use clap::ArgMatches;
use reqwest::header::CONTENT_TYPE;
use serde_json::Value;
use url::Url;

use crate::command::{Request, value};
use crate::config::{self, ConfigError, Profile};
use crate::login::{self, LoginError};
use crate::output::{self, Class};

/// The API a profile without a URL calls.
pub const DEFAULT_API_URL: &str = "https://api.norbelys.com";

/// How the CLI names itself to the API.
const USER_AGENT: &str = concat!("norbelys-cli/", env!("CARGO_PKG_VERSION"));

/// The longest wait for a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest wait for the next bytes of an answer; above the 25 seconds of a long poll.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// How many bytes of an unreadable answer an error quotes.
const EXCERPT_BYTES: usize = 200;

/// The global flags of a run, resolved.
pub struct Context {
    /// The profile's name.
    pub profile: String,
    /// The configuration file.
    pub config: PathBuf,
    /// `--api-key` or `NORBELYS_API_KEY`.
    pub api_key: Option<String>,
    /// `--api-url`.
    pub api_url: Option<String>,
    /// `--json`.
    pub json: bool,
    /// `--idempotency-key`.
    pub idempotency_key: Option<String>,
}

impl Context {
    /// The global flags as the innermost command's matches hold them (clap propagates them
    /// there from wherever they were written).
    ///
    /// # Errors
    ///
    /// When no `--config` is given and the environment names no configuration directory.
    pub fn from_matches(matches: &ArgMatches) -> Result<Self, ConfigError> {
        let config = match value(matches, "config") {
            Some(path) => PathBuf::from(path),
            None => config::default_path()?,
        };
        Ok(Self {
            profile: value(matches, "profile")
                .cloned()
                .unwrap_or_else(|| "default".to_owned()),
            config,
            api_key: value(matches, "api-key").cloned(),
            api_url: value(matches, "api-url").cloned(),
            json: matches.try_get_one::<bool>("json").ok().flatten() == Some(&true),
            idempotency_key: value(matches, "idempotency-key").cloned(),
        })
    }

    /// The API's base URL: `--api-url`, else the profile's, else the default.
    ///
    /// # Errors
    ///
    /// [`ApiError::InvalidUrl`] when it is not an `http` or `https` URL.
    pub fn base_url(&self, profile: &Profile) -> Result<Url, ApiError> {
        let text = self
            .api_url
            .as_deref()
            .or(profile.api_url.as_deref())
            .unwrap_or(DEFAULT_API_URL);
        Url::parse(text)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
            .ok_or_else(|| ApiError::InvalidUrl(text.to_owned()))
    }

    /// The `Idempotency-Key` of this run's effectful request: the given one, else a new one.
    #[must_use]
    pub fn idempotency_key(&self) -> String {
        self.idempotency_key
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string())
    }
}

/// Why a request got no answer the command can use.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The profile could not be read.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The session could not be refreshed.
    #[error(transparent)]
    Login(#[from] LoginError),
    /// No credential at all.
    #[error(
        "not logged in: run `norbelys login` (profile `{profile}`), or pass --api-key or set \
         NORBELYS_API_KEY"
    )]
    NotLoggedIn { profile: String },
    /// The base URL is not usable.
    #[error("`{0}` is not an http or https URL")]
    InvalidUrl(String),
    /// The HTTP client could not be built (no TLS roots on this system).
    #[error("cannot build the HTTP client: {0}")]
    Client(String),
    /// No answer arrived: the API is unreachable, or the answer was lost.
    #[error("cannot reach the API: {reason}{}", retry(idempotency_key.as_deref()))]
    Transport {
        reason: String,
        /// The key of an effectful request, which a safe retry repeats.
        idempotency_key: Option<String>,
    },
    /// The API refused the request.
    #[error("{0}")]
    Problem(Problem),
    /// A `2xx` answer that is not what the command expects.
    #[error("the API's answer is not what this command expects: {0}")]
    Unexpected(String),
}

/// The advice of a lost effectful request.
fn retry(idempotency_key: Option<&str>) -> String {
    idempotency_key.map_or_else(String::new, |key| {
        format!(
            ". The request may have been applied: run it again with --idempotency-key {key} to \
             apply it at most once"
        )
    })
}

impl ApiError {
    /// The exit class of the failure.
    #[must_use]
    pub fn class(&self) -> Class {
        match self {
            Self::Config(_) | Self::Client(_) | Self::Unexpected(_) => Class::Failure,
            Self::Login(error) => error.class(),
            Self::NotLoggedIn { .. } => Class::Unauthenticated,
            Self::InvalidUrl(_) => Class::Usage,
            Self::Transport { .. } => Class::Unreachable,
            Self::Problem(problem) => Class::of_status(problem.status),
        }
    }
}

/// An error answer of the API.
#[derive(Debug)]
pub struct Problem {
    /// The HTTP status.
    pub status: u16,
    /// The problem document, or `null` when the body was not JSON.
    pub body: Value,
    /// `X-Request-Id`, for when the body does not carry it.
    pub request_id: Option<String>,
    /// The start of the body, for an answer that is not a problem document.
    pub excerpt: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&output::problem(
            self.status,
            &self.body,
            self.request_id.as_deref(),
            &self.excerpt,
        ))
    }
}

/// A successful answer.
pub struct Answer {
    /// Its JSON body; none for `204`, a string for a body that is not JSON.
    pub body: Option<Value>,
    /// Its `ETag`: the version of a single resource that can be updated.
    pub etag: Option<String>,
}

/// The HTTP client for the API and its authorization server.
///
/// # Errors
///
/// [`ApiError::Client`] when the TLS configuration cannot be built.
pub fn http_client() -> Result<reqwest::Client, ApiError> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| ApiError::Client(chain(&error)))
}

/// An error with its causes, `: `-separated: `reqwest` keeps the useful part (connection
/// refused, certificate unknown) in the causes.
pub fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        text.push_str(": ");
        text.push_str(&next.to_string());
        cause = next.source();
    }
    text
}

/// The start of a body, as text, for an error message.
pub fn excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    text.chars()
        .take(EXCERPT_BYTES)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// The API, with the credential of a run.
pub struct Api {
    http: reqwest::Client,
    base: Url,
    bearer: String,
}

impl Api {
    /// The API and the credential the run's flags and profile name; a CLI session is
    /// refreshed first when it is about to expire.
    ///
    /// # Errors
    ///
    /// When no credential is configured, the session has ended or cannot be refreshed, the
    /// base URL is invalid or the profile cannot be read.
    pub async fn connect(context: &Context) -> Result<Self, ApiError> {
        let profile = config::load(&context.config)?
            .profiles
            .remove(&context.profile)
            .unwrap_or_default();
        let base = context.base_url(&profile)?;
        let http = http_client()?;
        let bearer = match (&context.api_key, profile.api_key, profile.session) {
            (Some(key), _, _) => key.clone(),
            (None, Some(key), _) => key,
            (None, None, Some(_)) => {
                login::access_token(&http, &base, &context.config, &context.profile).await?
            }
            (None, None, None) => {
                return Err(ApiError::NotLoggedIn {
                    profile: context.profile.clone(),
                });
            }
        };
        Ok(Self { http, base, bearer })
    }

    /// The absolute URL of a request: the base URL's path, then the request's segments, each
    /// percent-encoded, then its query.
    #[must_use]
    pub fn url(&self, request: &Request) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(&request.path);
        }
        if !request.query.is_empty() {
            url.query_pairs_mut().extend_pairs(&request.query);
        }
        url
    }

    /// Sends a request, with `idempotency_key` when it has an effect.
    ///
    /// # Errors
    ///
    /// [`ApiError::Transport`] when no answer arrived, [`ApiError::Problem`] for any answer
    /// but a `2xx`.
    pub async fn send(
        &self,
        request: &Request,
        idempotency_key: Option<&str>,
    ) -> Result<Answer, ApiError> {
        let lost = |error: reqwest::Error| ApiError::Transport {
            reason: chain(&error),
            idempotency_key: idempotency_key.map(str::to_owned),
        };
        let mut builder = self
            .http
            .request(request.method.clone(), self.url(request))
            .bearer_auth(&self.bearer);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(key) = idempotency_key {
            builder = builder.header("idempotency-key", key);
        }
        if let Some(payload) = &request.body {
            builder = builder
                .header(CONTENT_TYPE, &payload.content_type)
                .body(payload.bytes.clone());
        }
        let response = builder.send().await.map_err(lost)?;
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let request_id = header("x-request-id");
        let etag = header("etag");
        let bytes = response.bytes().await.map_err(lost)?;
        let json = serde_json::from_slice::<Value>(&bytes).ok();
        if (200..300).contains(&status) {
            let body = match json {
                Some(json) => Some(json),
                None if bytes.is_empty() => None,
                None => Some(Value::String(String::from_utf8_lossy(&bytes).into_owned())),
            };
            return Ok(Answer { body, etag });
        }
        Err(ApiError::Problem(Problem {
            status,
            body: json.unwrap_or(Value::Null),
            request_id,
            excerpt: excerpt(&bytes),
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::testing::{Fake, Reply, Scratch, run};

    fn page(items: serde_json::Value, next: Option<&str>) -> Reply {
        Reply::json(
            200,
            json!({ "data": items, "meta": { "has_more": next.is_some(), "next_cursor": next } }),
        )
    }

    /// A create sends the bearer credential, a JSON body and a fresh `Idempotency-Key`, and a
    /// given `--idempotency-key` is sent as it is; a read sends no key. The key is what makes a
    /// repeated command safe, so every effectful request must carry one.
    #[tokio::test]
    async fn effectful_requests_carry_the_credential_and_an_idempotency_key() {
        let person = json!({ "id": "per_1", "email": "ada@example.com" });
        let fake = Fake::start(vec![
            Reply::json(201, person.clone()),
            Reply::json(201, person.clone()),
            Reply::json(200, person),
        ])
        .await;
        let scratch = Scratch::new();
        let base = ["--api-url", fake.url.as_str(), "--api-key", "nb_test_key"];
        let created = run(
            &scratch,
            &[
                &base[..],
                &["people", "create", "--email", "ada@example.com"],
            ]
            .concat(),
        )
        .await;
        assert_eq!(created.code, 0, "{}", created.err);
        assert!(created.out.starts_with("id: per_1\n"), "{}", created.out);
        let keyed = run(
            &scratch,
            &[
                &base[..],
                &[
                    "--json",
                    "people",
                    "create",
                    "--email",
                    "ada@example.com",
                    "--idempotency-key",
                    "fixed-1",
                ],
            ]
            .concat(),
        )
        .await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&keyed.out).unwrap()["id"],
            "per_1"
        );
        let read = run(
            &scratch,
            &[&base[..], &["people", "retrieve", "per_1"]].concat(),
        )
        .await;
        assert_eq!(read.code, 0);

        let seen = fake.seen();
        assert_eq!(seen[0].method, "POST");
        assert_eq!(seen[0].path, "/v1/people");
        assert_eq!(seen[0].header("authorization"), Some("Bearer nb_test_key"));
        assert_eq!(seen[0].header("content-type"), Some("application/json"));
        assert_eq!(seen[0].json(), json!({ "email": "ada@example.com" }));
        let generated = seen[0].header("idempotency-key").unwrap();
        assert!(uuid::Uuid::parse_str(generated).is_ok());
        assert_eq!(seen[1].header("idempotency-key"), Some("fixed-1"));
        assert_eq!(seen[2].path, "/v1/people/per_1");
        assert!(seen[2].header("idempotency-key").is_none());
    }

    /// `--all` follows `next_cursor` to the last page, repeating every other parameter (the
    /// cursor is bound to the filters), and `--json` prints every item as one page.
    #[tokio::test]
    async fn all_follows_the_cursor_to_the_last_page() {
        let fake = Fake::start(vec![
            page(
                json!([{ "id": "per_3" }, { "id": "per_2" }]),
                Some("cursor-2"),
            ),
            page(json!([{ "id": "per_1" }]), None),
        ])
        .await;
        let scratch = Scratch::new();
        let ran = run(
            &scratch,
            &[
                "--api-url",
                &fake.url,
                "--api-key",
                "nb_test_key",
                "--json",
                "people",
                "list",
                "--all",
                "--limit",
                "2",
                "--q",
                "ad",
            ],
        )
        .await;
        assert_eq!(ran.code, 0, "{}", ran.err);
        let printed: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(
            printed,
            json!({ "data": [{ "id": "per_3" }, { "id": "per_2" }, { "id": "per_1" }], "meta": { "has_more": false, "next_cursor": null } })
        );
        let seen = fake.seen();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].query("cursor").is_none());
        assert_eq!(seen[1].query("cursor"), Some("cursor-2"));
        for request in &seen {
            assert_eq!(request.query("limit"), Some("2"));
            assert_eq!(request.query("q"), Some("ad"));
        }
    }

    /// A problem is printed with its code, detail, invalid fields and request id, and the exit
    /// code follows its status; with `--json` the problem document itself is printed for
    /// scripts. Programs branch on the exit code and the code, people read the rest.
    #[tokio::test]
    async fn problems_are_printed_and_exit_by_class() {
        let problem = json!({
            "type": "https://docs.norbelys.com/errors/validation_failed", "title": "Validation failed",
            "status": 422, "code": "validation_failed", "detail": "The body is invalid.",
            "errors": [{ "pointer": "/email", "code": "format", "detail": "not an email address" }],
            "request_id": "req_42"
        });
        let fake = Fake::start(vec![
            Reply::problem(422, problem.clone()),
            Reply::problem(422, problem.clone()),
            Reply::problem(
                404,
                json!({ "code": "not_found", "detail": "No such person.", "status": 404 }),
            ),
        ])
        .await;
        let scratch = Scratch::new();
        let base = ["--api-url", fake.url.as_str(), "--api-key", "nb_test_key"];
        let ran = run(
            &scratch,
            &[&base[..], &["people", "create", "--email", "nope"]].concat(),
        )
        .await;
        assert_eq!(ran.code, 7);
        assert!(ran.out.is_empty());
        assert_eq!(
            ran.err,
            "error: validation_failed (422): The body is invalid.\n  /email: format: not an email address\n  request id: req_42\n"
        );
        let json = run(
            &scratch,
            &[
                &base[..],
                &["--json", "people", "create", "--email", "nope"],
            ]
            .concat(),
        )
        .await;
        assert_eq!(json.code, 7);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json.out).unwrap(),
            problem
        );
        let missing = run(
            &scratch,
            &[&base[..], &["people", "retrieve", "per_9"]].concat(),
        )
        .await;
        assert_eq!(missing.code, 5);
    }

    /// A readable resource is followed by its `ETag` on standard error, with the `--if-match` that
    /// changes only that version, while `--json` prints the API's JSON alone: a person sees what
    /// to pass next, and a script's output stays parseable.
    #[tokio::test]
    async fn a_readable_resource_shows_its_etag() {
        let person = json!({ "id": "per_1", "version": 1_790_000_000_000_000_i64 });
        let etag = "\"1790000000000000\"";
        let fake = Fake::start(vec![
            Reply::json(200, person.clone()).with_header("etag", etag),
            Reply::json(200, person).with_header("etag", etag),
        ])
        .await;
        let scratch = Scratch::new();
        let base = ["--api-url", fake.url.as_str(), "--api-key", "nb_test_key"];
        let readable = run(
            &scratch,
            &[&base[..], &["people", "retrieve", "per_1"]].concat(),
        )
        .await;
        assert_eq!(readable.out, "id: per_1\nversion: 1790000000000000\n");
        assert_eq!(
            readable.err,
            "ETag \"1790000000000000\": --if-match 1790000000000000 changes only this version.\n"
        );
        let json = run(
            &scratch,
            &[&base[..], &["--json", "people", "retrieve", "per_1"]].concat(),
        )
        .await;
        assert!(json.err.is_empty());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json.out).unwrap()["version"],
            1_790_000_000_000_000_i64
        );
    }

    /// Without any credential a command exits as unauthenticated and says how to log in; when
    /// the API cannot be reached it exits as unreachable and prints the idempotency key of the
    /// lost request, which is what makes running it again safe.
    #[tokio::test]
    async fn missing_credentials_and_lost_answers_are_reported() {
        let scratch = Scratch::new();
        let anonymous = run(
            &scratch,
            &["--api-url", "http://127.0.0.1:9", "people", "list"],
        )
        .await;
        assert_eq!(anonymous.code, 3);
        assert!(anonymous.err.contains("norbelys login"));

        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let lost = run(
            &scratch,
            &[
                "--api-url",
                &url,
                "--api-key",
                "nb_test_key",
                "people",
                "create",
                "--email",
                "a@example.com",
                "--idempotency-key",
                "retry-me",
            ],
        )
        .await;
        assert_eq!(lost.code, 10);
        assert!(
            lost.err.contains("--idempotency-key retry-me"),
            "{}",
            lost.err
        );
    }
}
