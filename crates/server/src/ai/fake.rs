//! A fake AI provider for the tests: an HTTP server on the loopback interface that speaks
//! OpenAI's chat completions, answers each request as the test's function decides, and records
//! every request body, so a test can see what was sent (and what was not).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use secrecy::SecretString;
use serde_json::{Value, json};

use crate::config::AiArgs;

/// The model every use case is routed to in the tests, at $1 and $5 per million tokens: one the
/// capability matrix marks strict on the chat completions wire, which the fake speaks.
pub const MODEL: &str = "openai/gpt-6-luna";

/// How the fake answers one request.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A finished answer whose content is `content`, with the usage it reports (input, output).
    Answer {
        /// The answer's text (JSON for a schema).
        content: String,
        /// The finish reason: `stop`, `length`, `content_filter`.
        finish: &'static str,
        /// The usage reported, if any.
        usage: Option<(u32, u32)>,
    },
    /// A refusal, as OpenAI's `message.refusal`, billed (input 100, output 5).
    Refusal,
    /// An error status, with a `Retry-After` in seconds when given.
    Status {
        /// The status.
        status: u16,
        /// The `Retry-After` header, in seconds.
        retry_after: Option<u64>,
    },
    /// The reply after a delay, to outlast a deadline.
    Late(Duration, Box<Reply>),
}

/// A finished answer carrying `value` as its JSON, with usage of 100 input and 20 output tokens.
pub fn json_answer(value: &Value) -> Reply {
    Reply::Answer {
        content: value.to_string(),
        finish: "stop",
        usage: Some((100, 20)),
    }
}

/// The fake's answering function: the request's JSON body in, the reply out.
type Answerer = Arc<dyn Fn(&Value) -> Reply + Send + Sync>;

/// A running fake provider.
pub struct Fake {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Fake {
    /// Starts a fake on a free loopback port that answers with `answer`.
    pub async fn start(answer: impl Fn(&Value) -> Reply + Send + Sync + 'static) -> Self {
        Self::start_on("127.0.0.1:0", answer).await
    }

    /// Starts a fake on `address`, such as a port reserved for a live run.
    pub async fn start_on(
        address: &str,
        answer: impl Fn(&Value) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
        let recorded = Arc::clone(&requests);
        let answer: Answerer = Arc::new(answer);
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .expect("a loopback port");
        let address = listener.local_addr().expect("its address");
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |body: Bytes| {
                let recorded = Arc::clone(&recorded);
                let answer = Arc::clone(&answer);
                async move {
                    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let reply = answer(&request);
                    recorded.lock().expect("not poisoned").push(request);
                    respond(reply).await
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { address, requests }
    }

    /// Every request body received so far.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("not poisoned").clone()
    }

    /// The worker's AI configuration with every use case on [`MODEL`] at this fake, and a
    /// deadline of `timeout_seconds` per call.
    pub fn args(&self, timeout_seconds: u64) -> AiArgs {
        AiArgs {
            anthropic_api_key: None,
            anthropic_base_url: "https://api.anthropic.com".parse().expect("a URL"),
            openai_api_key: Some(SecretString::from("fake-key")),
            openai_base_url: format!("http://{}/v1", self.address)
                .parse()
                .expect("a URL"),
            ai_models: vec![format!("{MODEL}=1:5").parse().expect("an entry")],
            ai_prices_read_on: jiff::civil::date(2026, 10, 1),
            ai_classification_model: MODEL.parse().expect("a model"),
            ai_snippets_model: MODEL.parse().expect("a model"),
            ai_hints_model: MODEL.parse().expect("a model"),
            ai_timeout_seconds: timeout_seconds,
            ai_canary_margin_points: 5,
        }
    }
}

/// The text of the last user turn of a request: what a fake answering by its input reads.
pub fn last_user_turn(request: &Value) -> String {
    request["messages"]
        .as_array()
        .and_then(|turns| {
            turns
                .iter()
                .rev()
                .find(|turn| turn["role"] == "user")
                .and_then(|turn| turn["content"].as_str())
        })
        .unwrap_or_default()
        .to_owned()
}

/// The HTTP answer of `reply`.
fn respond(reply: Reply) -> std::pin::Pin<Box<dyn Future<Output = Response> + Send>> {
    Box::pin(async move {
        match reply {
            Reply::Late(delay, reply) => {
                tokio::time::sleep(delay).await;
                respond(*reply).await
            }
            Reply::Status {
                status,
                retry_after,
            } => {
                let status = StatusCode::from_u16(status).expect("a status");
                let mut response =
                    (status, axum::Json(json!({"error": {"type": "fake"}}))).into_response();
                if let Some(seconds) = retry_after {
                    response.headers_mut().insert(
                        header::RETRY_AFTER,
                        HeaderValue::from_str(&seconds.to_string()).expect("a header"),
                    );
                }
                response
            }
            Reply::Refusal => axum::Json(json!({
                "id": "chatcmpl-fake",
                "model": "fake-model",
                "choices": [{
                    "message": {"content": null, "refusal": "I can't help with that."},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 100, "completion_tokens": 5},
            }))
            .into_response(),
            Reply::Answer {
                content,
                finish,
                usage,
            } => {
                let mut body = json!({
                    "id": "chatcmpl-fake",
                    "model": "fake-model",
                    "choices": [{"message": {"content": content}, "finish_reason": finish}],
                });
                if let Some((input, output)) = usage {
                    body["usage"] = json!({"prompt_tokens": input, "completion_tokens": output});
                }
                axum::Json(body).into_response()
            }
        }
    })
}
