//! Tests of the client, at two levels.
//!
//! - The shared rules, called directly: the retry policy of every error, `Retry-After`, the
//!   configuration checks, how an answer becomes a [`Completion`](crate::Completion), and the
//!   redaction of `Debug` output.
//! - The behaviour over HTTP, against a fake provider on 127.0.0.1 that each test starts:
//!   success on both wire formats, the retries as the caller's schedule and the provider's
//!   `Retry-After` direct them, the deadline, the errors that end a call at once, and the
//!   checks that refuse a call before anything is sent.
//!
//! The retry schedules here wait a fixed 10 ms, so the tests are fast and need no random
//! draw; the schedule a deployment uses is the caller's policy and is tested where it lives.
//! The request fixture is shared with the wire formats' own tests, which check the bodies
//! they build and their reading of answers.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RETRY_AFTER};
use secrecy::SecretString;
use serde_json::{Value, json};
use strum::IntoEnumIterator as _;
use tokio::io::AsyncReadExt as _;
use tokio::net::TcpListener;
use url::Url;

use super::{
    Answer, Client, MAX_ANSWER_BYTES, Stop, anthropic, completion, openai, retry_after, retry_wait,
};
use crate::{AiError, Message, Outcome, Output, Provider, Request, RetrySchedule, Role, Wire};

/// The wait of every test schedule: short enough to keep the tests fast, long enough to be
/// seen between two attempts.
const WAIT: Duration = Duration::from_millis(10);

/// The JSON schema of the fixture request: one required string property and no other.
pub(super) fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {"classification": {"type": "string"}},
        "required": ["classification"],
        "additionalProperties": false,
    })
}

/// A request with every field set: a system prompt, a conversation with both roles (a
/// corrective retry's shape), a temperature and a schema.
pub(super) fn request() -> Request {
    Request {
        call_id: "call-1".to_owned(),
        system: "Classify the reply.".to_owned(),
        messages: vec![
            Message {
                role: Role::User,
                content: "Out of office until Monday.".to_owned(),
            },
            Message {
                role: Role::Assistant,
                content: "{}".to_owned(),
            },
            Message {
                role: Role::User,
                content: "The answer lacks `classification`.".to_owned(),
            },
        ],
        max_tokens: 256,
        temperature: Some(0.5),
        schema: Some(schema()),
    }
}

/// A provider of `wire` at `base_url` that enforces schemas, with a ten-second deadline and
/// two retries, each after [`WAIT`].
fn provider(wire: Wire, base_url: Url) -> Provider {
    Provider {
        wire,
        base_url,
        model: "model-x".to_owned(),
        api_key: SecretString::from("key-123"),
        structured_output: true,
        timeout: Duration::from_secs(10),
        retries: RetrySchedule {
            max_retries: 2,
            wait: Arc::new(|_| WAIT),
        },
    }
}

/// A schedule of `max_retries` retries, each after [`WAIT`], that records every argument the
/// client asks it for, so a test can see which retries were planned and in which order.
fn recording(max_retries: u32) -> (RetrySchedule, Arc<Mutex<Vec<u32>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&asked);
    let schedule = RetrySchedule {
        max_retries,
        wait: Arc::new(move |retries| {
            record.lock().unwrap().push(retries);
            WAIT
        }),
    };
    (schedule, asked)
}

/// A client for [`provider`].
fn client(wire: Wire, base_url: Url) -> Client {
    Client::new(&provider(wire, base_url)).expect("a valid configuration")
}

/// A client for [`provider`] that retries as `schedule` says.
fn scheduled(wire: Wire, base_url: Url, schedule: RetrySchedule) -> Client {
    let config = Provider {
        retries: schedule,
        ..provider(wire, base_url)
    };
    Client::new(&config).expect("a valid configuration")
}

/// An answer that stopped with `stop` after writing `text`, with the provider reason
/// `reason-x` and nothing else, as a wire format would read it.
fn answer(stop: Stop, text: &str) -> Answer {
    Answer {
        text: text.to_owned(),
        stop,
        finish_reason: Some("reason-x".to_owned()),
        usage: None,
        model: None,
        id: None,
    }
}

/// Every error has a decided retry policy. Within the call, only the errors the provider
/// cannot have billed are retried (a rate limit, a failing provider, an unreachable one), so
/// a retry never pays twice, and nothing is retried once the schedule's retries are spent.
/// Later, as a new call, every error is worth retrying unless the configuration, the key or
/// the request must change first. The provider's own wait takes precedence over the
/// schedule's, which is asked for the wait before the retry that follows the retries already
/// made. A new error variant fails here until its policy is written down.
#[test]
fn every_error_has_a_retry_policy() {
    let schedule = RetrySchedule {
        max_retries: 2,
        wait: Arc::new(|retries| Duration::from_millis(10 * u64::from(retries + 1))),
    };
    for error in AiError::iter() {
        let (within_the_call, later) = match error {
            AiError::RateLimited { .. } | AiError::Provider { .. } | AiError::Unreachable => {
                (true, true)
            }
            AiError::Transport | AiError::Timeout => (false, true),
            AiError::Config(_)
            | AiError::Unauthorized { .. }
            | AiError::Rejected { .. }
            | AiError::InvalidResponse => (false, false),
        };
        let first = retry_wait(&error, 0, &schedule);
        assert_eq!(first.is_some(), within_the_call, "{error:?}");
        assert_eq!(retry_wait(&error, 2, &schedule), None, "{error:?}");
        assert_eq!(error.is_retryable(), later, "{error:?}");
    }
    let provider_wait = Some(Duration::from_secs(7));
    let limited = AiError::RateLimited {
        retry_after: provider_wait,
    };
    assert_eq!(retry_wait(&limited, 1, &schedule), provider_wait);
    let failed = AiError::Provider {
        status: 503,
        retry_after: None,
    };
    assert_eq!(
        retry_wait(&failed, 1, &schedule),
        Some(Duration::from_millis(20))
    );
}

/// `Retry-After` is read as delay-seconds or as an HTTP date counted from now (RFC 9110,
/// section 10.2.3), and a value that gives no usable wait (zero, a date that is not in the
/// future, anything unparseable, no header at all) counts as absent, so the caller's schedule
/// decides the wait instead and a meaningless value never causes an immediate retry.
#[test]
fn retry_after_reads_seconds_and_http_dates() {
    let now: jiff::Timestamp = "1994-11-06T08:49:07Z".parse().unwrap();
    let wait = |value: &str| {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
        retry_after(&headers, now)
    };
    assert_eq!(wait("120"), Some(Duration::from_secs(120)));
    assert_eq!(wait(" 3 "), Some(Duration::from_secs(3)));
    assert_eq!(
        wait("Sun, 06 Nov 1994 08:49:37 GMT"),
        Some(Duration::from_secs(30))
    );
    for unusable in [
        "0",
        "Sun, 06 Nov 1994 08:49:07 GMT",
        "Sun, 06 Nov 1994 08:48:00 GMT",
        "-5",
        "soon",
        "",
    ] {
        assert_eq!(wait(unusable), None, "{unusable:?}");
    }
    assert_eq!(retry_after(&HeaderMap::new(), now), None);
}

/// The configuration is checked once, when the client is built: HTTPS anywhere; plain HTTP
/// only to this machine or a private network, named `localhost` or given as a literal
/// address, since anywhere else the key would cross networks in clear text; no credentials,
/// query or fragment in the URL; a deadline above zero and at most 90 seconds; and a key that
/// fits in a header.
#[test]
fn new_accepts_safe_configurations_and_refuses_the_rest() {
    let at = |url: &str| provider(Wire::Anthropic, Url::parse(url).unwrap());
    for safe in [
        "https://api.anthropic.com",
        "https://gateway.example/anthropic/",
        "http://localhost:8080",
        "http://127.0.0.1:11434/v1",
        "http://10.0.0.5",
        "http://172.16.0.1",
        "http://192.168.1.20:8000",
        "http://169.254.1.1",
        "http://[::1]:8080",
        "http://[fd00::1]",
        "http://[fe80::1]",
    ] {
        assert!(Client::new(&at(safe)).is_ok(), "{safe}");
    }
    for unsafe_url in [
        "http://api.anthropic.com",
        "http://gpu.lan:8000",
        "http://8.8.8.8",
        "http://172.32.0.1",
        "http://[2001:db8::1]",
        "https://user:secret@api.anthropic.com",
        "https://api.anthropic.com/?key=1",
        "https://api.anthropic.com/#part",
        "ftp://api.anthropic.com",
    ] {
        assert!(
            matches!(Client::new(&at(unsafe_url)), Err(AiError::Config(_))),
            "{unsafe_url}"
        );
    }
    let mut config = at("https://api.anthropic.com");
    for (timeout, valid) in [
        (Duration::ZERO, false),
        (Duration::from_millis(1), true),
        (Duration::from_secs(90), true),
        (Duration::from_secs(91), false),
    ] {
        config.timeout = timeout;
        assert_eq!(Client::new(&config).is_ok(), valid, "{timeout:?}");
    }
    config.timeout = Duration::from_secs(10);
    config.api_key = SecretString::from("key\nwith a line break");
    assert!(matches!(Client::new(&config), Err(AiError::Config(_))));
}

/// How an answer becomes an outcome, for every stop with and without a schema: a finished
/// answer is its text, or its parsed JSON when a schema was asked for; a refusal and a
/// truncation keep the text; an unexpected stop is invalid output naming the provider's
/// reason. A finished answer that is not JSON when a schema was asked for is invalid output
/// too, with the parser's message, which can be shown to the model in a corrective retry. A
/// new stop variant fails here until it has an outcome.
#[test]
fn every_stop_becomes_an_outcome() {
    for stop in Stop::iter() {
        for schema in [false, true] {
            let text = if schema {
                r#"{"classification":"human_reply"}"#
            } else {
                "plain text"
            };
            let expected = match (stop, schema) {
                (Stop::Finished, false) => Outcome::Completed(Output::Text(text.to_owned())),
                (Stop::Finished, true) => {
                    Outcome::Completed(Output::Json(json!({"classification": "human_reply"})))
                }
                (Stop::Refused, _) => Outcome::Refused {
                    text: text.to_owned(),
                },
                (Stop::Truncated, _) => Outcome::Truncated {
                    text: text.to_owned(),
                },
                (Stop::Unexpected, _) => Outcome::InvalidOutput {
                    text: text.to_owned(),
                    violation: "the answer stopped for an unexpected reason: reason-x".to_owned(),
                },
            };
            let outcome = completion(answer(stop, text), None, schema, "model-x").outcome;
            assert!(outcome == expected, "{stop:?}, schema: {schema}");
        }
    }
    let fenced = "```json\n{}\n```";
    let outcome = completion(answer(Stop::Finished, fenced), None, true, "model-x").outcome;
    let Outcome::InvalidOutput { text, violation } = outcome else {
        panic!("a finished answer that is not JSON must be invalid output, got {outcome:?}");
    };
    assert_eq!(text, fenced);
    assert!(
        violation.starts_with("the answer is not valid JSON: "),
        "{violation}"
    );
}

/// The provider's request id header is preferred to the answer's own id, since it is what the
/// provider's support asks for; the answer's id fills in when the header is missing; and the
/// configured model stands in when the answer names none.
#[test]
fn completion_prefers_the_header_id_and_fills_in_the_model() {
    let named = || Answer {
        id: Some("msg_1".to_owned()),
        model: Some("model-x-2026".to_owned()),
        ..answer(Stop::Finished, "hi")
    };
    let with_header = completion(named(), Some("req_1".to_owned()), false, "model-x");
    assert_eq!(with_header.provider_request_id.as_deref(), Some("req_1"));
    assert_eq!(with_header.model, "model-x-2026");
    let without_header = completion(named(), None, false, "model-x");
    assert_eq!(without_header.provider_request_id.as_deref(), Some("msg_1"));
    let anonymous = completion(answer(Stop::Finished, "hi"), None, false, "model-x");
    assert_eq!(anonymous.provider_request_id, None);
    assert_eq!(anonymous.model, "model-x");
}

/// `Debug` output, which can end up in logs, never shows prompt or completion text (it may
/// hold personal data from customers' mailboxes) nor the provider's key: requests, messages,
/// every outcome, the provider configuration and the client print only their shape.
#[test]
fn debug_output_hides_content_and_keys() {
    const SECRET: &str = "SECRET-7f3a";
    let mut request = request();
    request.system = format!("{SECRET} instructions");
    for message in &mut request.messages {
        message.content = format!("{SECRET} turn");
    }
    let mut printed = vec![format!("{request:?}")];
    printed.extend(
        request
            .messages
            .iter()
            .map(|message| format!("{message:?}")),
    );
    for stop in Stop::iter() {
        for schema in [false, true] {
            let text = if schema {
                format!(r#"{{"note":"{SECRET}"}}"#)
            } else {
                format!("{SECRET} answer")
            };
            let completion = completion(answer(stop, &text), None, schema, "model-x");
            printed.push(format!("{completion:?}"));
        }
    }
    let mut config = provider(
        Wire::OpenAiCompatible,
        Url::parse("https://api.openai.com/v1").unwrap(),
    );
    config.api_key = SecretString::from(SECRET);
    printed.push(format!("{config:?}"));
    printed.push(format!("{:?}", Client::new(&config).unwrap()));
    for line in &printed {
        assert!(!line.contains(SECRET), "{line}");
    }
}

/// One scripted answer of the fake provider.
struct Scripted {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
    delay: Duration,
}

impl Scripted {
    /// An answer with `status` and `body`, sent at once.
    fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    /// The same answer with one more header.
    fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }

    /// The same answer, sent only after `delay`.
    fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

/// A request as the fake provider received it.
struct Received {
    path: String,
    headers: HeaderMap,
    body: Value,
    at: Instant,
}

/// A fake provider on 127.0.0.1, serving until the test's runtime stops. It answers each
/// request with the next scripted answer and records what it received. Once the script is
/// spent it answers `418`, a status the client never retries, so an attempt the test did not
/// expect shows up as a wrong result rather than as a hang.
struct Fake {
    url: Url,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Fake {
    /// Starts a fake provider that plays `script`.
    async fn start(script: Vec<Scripted>) -> Self {
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let received = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&received);
        let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let script = Arc::clone(&script);
            let recorder = Arc::clone(&recorder);
            async move {
                let at = Instant::now();
                let (parts, body) = request.into_parts();
                let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                recorder.lock().unwrap().push(Received {
                    path: parts.uri.path().to_owned(),
                    headers: parts.headers,
                    body: serde_json::from_slice(&body).unwrap(),
                    at,
                });
                let next = script.lock().unwrap().pop_front();
                let Some(next) = next else {
                    return StatusCode::IM_A_TEAPOT.into_response();
                };
                tokio::time::sleep(next.delay).await;
                let mut response = Response::new(axum::body::Body::from(next.body));
                *response.status_mut() = StatusCode::from_u16(next.status).unwrap();
                for (name, value) in next.headers {
                    response.headers_mut().insert(
                        HeaderName::from_static(name),
                        HeaderValue::from_str(&value).unwrap(),
                    );
                }
                response
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, received }
    }

    /// The requests received so far, oldest first.
    fn received(&self) -> MutexGuard<'_, Vec<Received>> {
        self.received.lock().unwrap()
    }
}

/// The success answer of `wire`: schema-valid JSON text, with the request id `req-1` in the
/// wire's own header.
fn success(wire: Wire) -> Scripted {
    let text = r#"{"classification":"out_of_office"}"#;
    match wire {
        Wire::Anthropic => Scripted::new(
            200,
            json!({
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": "model-x-snapshot",
                "content": [{"type": "text", "text": text}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 30, "output_tokens": 5},
            })
            .to_string(),
        )
        .header("request-id", "req-1"),
        Wire::OpenAiCompatible => Scripted::new(
            200,
            json!({
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "model": "model-x-snapshot",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": text, "refusal": null},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 30, "completion_tokens": 5, "total_tokens": 35},
            })
            .to_string(),
        )
        .header("x-request-id", "req-1"),
    }
}

/// A call on each wire format reaches the provider's path (whether or not the base URL ends
/// with a slash) with the wire's key header, content type and version header, and with the
/// body the wire's module builds; the answer comes back completed, carrying the provider's
/// request id from the wire's header. This is the path every AI feature takes; a wire format
/// added later fails here until it has a scripted answer.
#[tokio::test]
async fn completes_on_both_wires() {
    for wire in Wire::iter() {
        let fake = Fake::start(vec![success(wire)]).await;
        let (base, path, body) = match wire {
            Wire::Anthropic => (
                fake.url.clone(),
                "/v1/messages",
                anthropic::body("model-x", &request()),
            ),
            Wire::OpenAiCompatible => (
                fake.url.join("v1/").unwrap(),
                "/v1/chat/completions",
                openai::body("model-x", &request()),
            ),
        };
        let completion = client(wire, base).complete(&request()).await.unwrap();
        let answer = json!({"classification": "out_of_office"});
        assert!(
            completion.outcome == Outcome::Completed(Output::Json(answer)),
            "{wire:?}: {completion:?}"
        );
        assert_eq!(completion.provider_request_id.as_deref(), Some("req-1"));

        let received = fake.received();
        let [sent] = received.as_slice() else {
            panic!(
                "{wire:?}: one request expected, {} received",
                received.len()
            );
        };
        let header = |name: &str| sent.headers.get(name).map(|value| value.to_str().unwrap());
        assert_eq!(sent.path, path);
        assert_eq!(
            sent.body,
            serde_json::from_slice::<Value>(&body.unwrap()).unwrap()
        );
        assert_eq!(header("content-type"), Some("application/json"));
        match wire {
            Wire::Anthropic => {
                assert_eq!(header("x-api-key"), Some("key-123"));
                assert_eq!(header("anthropic-version"), Some("2023-06-01"));
                assert_eq!(header("x-client-request-id"), None);
            }
            Wire::OpenAiCompatible => {
                assert_eq!(header("authorization"), Some("Bearer key-123"));
                assert_eq!(header("x-client-request-id"), Some("call-1"));
            }
        }
    }
}

/// A `429` with `Retry-After: 1` is retried once the provider's wait has passed, and the
/// retry's answer is returned; retrying sooner would hit the limit again. The provider's wait
/// takes precedence over the caller's schedule, which is not even asked for its own (10 ms).
#[tokio::test]
async fn a_rate_limit_is_retried_after_the_providers_wait() {
    let fake = Fake::start(vec![
        Scripted::new(429, "{}").header("retry-after", "1"),
        success(Wire::Anthropic),
    ])
    .await;
    let (schedule, asked) = recording(2);
    let completion = scheduled(Wire::Anthropic, fake.url.clone(), schedule)
        .complete(&request())
        .await
        .unwrap();
    assert!(matches!(completion.outcome, Outcome::Completed(_)));
    assert!(asked.lock().unwrap().is_empty());
    let received = fake.received();
    let [limited, retried] = received.as_slice() else {
        panic!("two requests expected, {} received", received.len());
    };
    let gap = retried.at.duration_since(limited.at);
    assert!(
        gap >= Duration::from_secs(1) && gap < Duration::from_secs(3),
        "{gap:?}"
    );
}

/// When the provider's wait would outlast the call's deadline, the call does not sleep
/// through it: the rate limit comes back at once, carrying the wait, so the caller can hold
/// back every call of the same kind instead of each one blocking until its deadline.
#[tokio::test]
async fn a_wait_past_the_deadline_returns_the_rate_limit_at_once() {
    let fake = Fake::start(vec![
        Scripted::new(429, "{}").header("retry-after", "30"),
        success(Wire::Anthropic),
    ])
    .await;
    let started = Instant::now();
    let error = client(Wire::Anthropic, fake.url.clone())
        .complete(&request())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        AiError::RateLimited {
            retry_after: Some(Duration::from_secs(30))
        }
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(fake.received().len(), 1);
}

/// A failing provider (`529 Overloaded`) is retried as the caller's schedule says, and then
/// its error is returned: with two retries allowed there are three attempts in all, the
/// schedule is asked for the waits before the first and the second retry (counted from
/// zero), and each wait passes before the next attempt. Retrying past the schedule would pile
/// onto an overloaded provider against the caller's policy.
#[tokio::test]
async fn server_errors_are_retried_as_the_schedule_says() {
    let overloaded = || {
        Scripted::new(
            529,
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})
                .to_string(),
        )
    };
    let fake = Fake::start(vec![
        overloaded(),
        overloaded(),
        overloaded(),
        success(Wire::Anthropic),
    ])
    .await;
    let (schedule, asked) = recording(2);
    let error = scheduled(Wire::Anthropic, fake.url.clone(), schedule)
        .complete(&request())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        AiError::Provider {
            status: 529,
            retry_after: None
        }
    );
    assert_eq!(*asked.lock().unwrap(), [0, 1]);
    let received = fake.received();
    let [first, second, third] = received.as_slice() else {
        panic!("three attempts expected, {} received", received.len());
    };
    assert!(second.at.duration_since(first.at) >= WAIT);
    assert!(third.at.duration_since(second.at) >= WAIT);
}

/// Statuses that waiting cannot fix end the call after one attempt, each with its error: a
/// rejected or unauthorised key, an invalid request, an unknown model, and an account without
/// credit, including OpenAI's `429` with `insufficient_quota`, which looks like a rate limit
/// but never clears by itself. Retrying any of them would only spend time.
#[tokio::test]
async fn errors_no_wait_cures_end_the_call_at_once() {
    let out_of_credit = json!({"error": {
        "message": "You exceeded your current quota.",
        "type": "insufficient_quota",
        "code": "insufficient_quota",
    }})
    .to_string();
    let cases = [
        (401, "{}".to_owned(), AiError::Unauthorized { status: 401 }),
        (403, "{}".to_owned(), AiError::Unauthorized { status: 403 }),
        (400, "{}".to_owned(), AiError::Rejected { status: 400 }),
        (402, "{}".to_owned(), AiError::Rejected { status: 402 }),
        (404, "{}".to_owned(), AiError::Rejected { status: 404 }),
        (429, out_of_credit, AiError::Rejected { status: 429 }),
    ];
    for (status, body, expected) in cases {
        let fake = Fake::start(vec![
            Scripted::new(status, body),
            success(Wire::OpenAiCompatible),
        ])
        .await;
        let error = client(Wire::OpenAiCompatible, fake.url.clone())
            .complete(&request())
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(fake.received().len(), 1, "status {status}");
    }
}

/// A provider slower than the deadline ends the call at the deadline with `Timeout`, and the
/// call is not retried: the provider may still finish and bill the first attempt, so a retry
/// could pay twice for one answer.
#[tokio::test]
async fn the_deadline_ends_a_slow_call_without_retry() {
    let fake = Fake::start(vec![
        success(Wire::Anthropic).after(Duration::from_secs(5)),
        success(Wire::Anthropic),
    ])
    .await;
    let mut slow = provider(Wire::Anthropic, fake.url.clone());
    slow.timeout = Duration::from_secs(1);
    let started = Instant::now();
    let error = Client::new(&slow)
        .unwrap()
        .complete(&request())
        .await
        .unwrap_err();
    let elapsed = started.elapsed();
    assert_eq!(error, AiError::Timeout);
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(3),
        "{elapsed:?}"
    );
    assert_eq!(fake.received().len(), 1);
}

/// A connection that breaks after the request was sent ends the call with `Transport` after
/// one attempt: the provider may have run and billed the call, so a retry could pay twice.
#[tokio::test]
async fn a_connection_lost_after_sending_is_not_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            // Read the request, then hang up without answering.
            let mut buffer = vec![0_u8; 64 * 1024];
            let _ = socket.read(&mut buffer).await;
        }
    });
    let error = client(Wire::Anthropic, url)
        .complete(&request())
        .await
        .unwrap_err();
    assert_eq!(error, AiError::Transport);
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
}

/// A provider that refuses connections is retried as the caller's schedule says, since
/// nothing was sent and nothing billed, and then reported as `Unreachable`: the schedule is
/// asked for the waits before both of its retries.
#[tokio::test]
async fn an_unreachable_provider_is_retried_then_reported() {
    // A port that was free a moment ago and is closed again: connections to it are refused.
    let address = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let (schedule, asked) = recording(2);
    let url = Url::parse(&format!("http://{address}")).unwrap();
    let error = scheduled(Wire::Anthropic, url, schedule)
        .complete(&request())
        .await
        .unwrap_err();
    assert_eq!(error, AiError::Unreachable);
    assert_eq!(*asked.lock().unwrap(), [0, 1]);
}

/// A success status whose body is not an answer of the wire format, or is larger than the
/// 4 MiB bound, ends the call with `InvalidResponse` after one attempt: the provider may have
/// billed it, so it is not retried, and the bound keeps a misbehaving endpoint from filling
/// memory.
#[tokio::test]
async fn unreadable_success_answers_are_invalid_responses() {
    for body in [
        "<html>Down for maintenance</html>".to_owned(),
        "x".repeat(MAX_ANSWER_BYTES + 1),
    ] {
        let fake = Fake::start(vec![Scripted::new(200, body), success(Wire::Anthropic)]).await;
        let error = client(Wire::Anthropic, fake.url.clone())
            .complete(&request())
            .await
            .unwrap_err();
        assert_eq!(error, AiError::InvalidResponse);
        assert_eq!(fake.received().len(), 1);
    }
}

/// The checks that need no network refuse a call before anything is sent, so nothing is
/// billed: a schema asked of a provider that does not enforce one (its unconstrained answer
/// would be paid for and then fail to parse), and a call id that is empty, longer than 512
/// bytes, or not visible ASCII (it travels in a header, and OpenAI accepts only that).
#[tokio::test]
async fn preflight_refusals_send_nothing() {
    let fake = Fake::start(Vec::new()).await;
    let mut lax = provider(Wire::OpenAiCompatible, fake.url.clone());
    lax.structured_output = false;
    let refused = Client::new(&lax).unwrap().complete(&request()).await;
    assert!(matches!(refused, Err(AiError::Config(_))), "{refused:?}");
    let strict = client(Wire::OpenAiCompatible, fake.url.clone());
    for call_id in [
        String::new(),
        "x".repeat(513),
        "café".to_owned(),
        "with space".to_owned(),
        "line\nbreak".to_owned(),
    ] {
        let invalid = Request {
            call_id,
            ..request()
        };
        let refused = strict.complete(&invalid).await;
        assert!(
            matches!(refused, Err(AiError::Config(_))),
            "{:?}",
            invalid.call_id
        );
    }
    assert!(fake.received().is_empty());
}
