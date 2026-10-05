//! `listen` and `trigger`: the workspace's webhooks on a developer's machine.
//!
//! # listen
//!
//! `norbelys listen --forward-to localhost:3000/hooks` long-polls the workspace's events
//! (`GET /v1/events?after=<last>&wait=25`, oldest first) and POSTs each one to the local URL as
//! a webhook endpoint receives it: the body `{"type", "timestamp", "data"}` with the Standard
//! Webhooks headers (<https://www.standardwebhooks.com/>):
//!
//! - `webhook-id`: the event's id, the receiver's key to deduplicate;
//! - `webhook-timestamp`: the forwarding time in Unix seconds, which receivers check against
//!   their clock;
//! - `webhook-signature`: `v1,` and the base64 HMAC-SHA256 of `{id}.{timestamp}.{body}`, keyed
//!   with the profile's secret: `whsec_` and 32 random bytes in base64, created by the first
//!   `listen` of the profile and printed whenever it starts, so the local handler verifies these
//!   events with the code that verifies real ones.
//!
//! The last forwarded event's id is the profile's cursor, saved after each event, so a
//! restarted `listen` forwards the events created while it was stopped. The first `listen` of a
//! profile starts after the newest event, forwarding only what happens from then on. A forward
//! that fails or is refused is printed and skipped: this is a development tool, and real
//! endpoints have their own retries. When the API cannot be reached or answers that it is
//! unavailable, `listen` waits (1 second, doubling up to 30) and polls again; any other refusal
//! (a revoked credential) stops it.
//!
//! # trigger
//!
//! `norbelys trigger message.sent` creates a synthetic event of that type with sample data
//! (`POST /v1/events`), delivered to the workspace's endpoints and seen by `listen` like a real
//! one; `--webhook-endpoint-id` addresses it to one endpoint, whatever its subscriptions.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lc_rs::{hmac, rand};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::Method;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

use crate::api::{self, Api, ApiError, Context};
use crate::command::{Payload, Request, UsageError};
use crate::config::{self, Listen};
use crate::output::{self, Class, Terminal};

/// The prefix of a Standard Webhooks secret.
const SECRET_PREFIX: &str = "whsec_";
/// Random bytes in a new secret (the specification asks for 24 to 64).
const SECRET_BYTES: usize = 32;
/// How long one poll waits for an event, in seconds: the API's longest wait.
const WAIT_SECONDS: &str = "25";
/// Events per poll.
const PAGE_SIZE: &str = "100";
/// How long a local handler has to answer, as a webhook endpoint has.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(15);
/// The first and the longest pause before polling an unreachable API again.
const FIRST_PAUSE: Duration = Duration::from_secs(1);
const LONGEST_PAUSE: Duration = Duration::from_secs(30);

/// An event of `GET /v1/events`.
#[derive(Deserialize)]
struct Event {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: Value,
    created_at: String,
}

/// A page of `GET /v1/events`.
#[derive(Deserialize)]
struct Events {
    data: Vec<Event>,
}

/// The body a webhook endpoint receives, members in the order the Standard Webhooks
/// specification shows them.
#[derive(Serialize)]
struct Webhook<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    timestamp: &'a str,
    data: &'a Value,
}

/// A new Standard Webhooks secret: `whsec_` and the base64 of 32 random bytes.
///
/// # Errors
///
/// [`crate::Error::Random`] when the system's random source fails.
pub fn new_secret() -> Result<String, crate::Error> {
    let mut bytes = [0_u8; SECRET_BYTES];
    rand::fill(&mut bytes).map_err(|_| crate::Error::Random)?;
    Ok(format!("{SECRET_PREFIX}{}", STANDARD.encode(bytes)))
}

/// The Standard Webhooks signature of a message: `v1,` and the base64 HMAC-SHA256, keyed with
/// the secret's bytes, of `{id}.{timestamp}.{body}`. `None` when the secret is not a `whsec_`
/// secret.
#[must_use]
pub fn sign(secret: &str, id: &str, timestamp: i64, body: &[u8]) -> Option<String> {
    let key = STANDARD.decode(secret.strip_prefix(SECRET_PREFIX)?).ok()?;
    let mut context = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, &key));
    context.update(id.as_bytes());
    context.update(b".");
    context.update(timestamp.to_string().as_bytes());
    context.update(b".");
    context.update(body);
    Some(format!("v1,{}", STANDARD.encode(context.sign().as_ref())))
}

/// The URL `--forward-to` names, `http://` unless it has a scheme.
///
/// # Errors
///
/// [`UsageError`] when it is not an `http` or `https` URL.
pub fn forward_url(target: &str) -> Result<Url, UsageError> {
    let text = if target.contains("://") {
        target.to_owned()
    } else {
        format!("http://{target}")
    };
    Url::parse(&text)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .ok_or_else(|| UsageError(format!("--forward-to `{target}` is not an http URL")))
}

/// `norbelys listen`: forwards events to `target` until interrupted, or, for tests, until
/// `stop_after` events were forwarded.
///
/// # Errors
///
/// When the API refuses the polling (a credential revoked meanwhile), the profile cannot be
/// saved, or the terminal cannot be written.
pub async fn listen(
    context: &Context,
    api: &Api,
    target: &Url,
    terminal: &mut Terminal<'_>,
    stop_after: Option<usize>,
) -> Result<(), crate::Error> {
    let candidate = new_secret()?;
    let state = config::update(&context.config, |config| {
        let profile = config.profiles.entry(context.profile.clone()).or_default();
        profile
            .listen
            .get_or_insert(Listen {
                secret: candidate,
                cursor: None,
            })
            .clone()
    })?;
    let mut cursor = match state.cursor {
        Some(cursor) => Some(cursor),
        None => newest(api).await?,
    };
    writeln!(
        terminal.out,
        "Ready: forwarding the events of profile `{}` to {target}.\n\
         They are signed with {} (Standard Webhooks). Ctrl-C to stop.",
        context.profile, state.secret
    )?;
    let forwarder = reqwest::Client::builder()
        .timeout(FORWARD_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| ApiError::Client(api::chain(&error)))?;
    let mut forwarded = 0_usize;
    let mut pause = FIRST_PAUSE;
    loop {
        let mut request = Request::new(Method::GET, "/v1/events");
        request.set_query("wait", WAIT_SECONDS);
        request.set_query("limit", PAGE_SIZE);
        match &cursor {
            Some(after) => request.set_query("after", after),
            None => request.set_query("order", "asc"),
        }
        let answer = match api.send(&request, None).await {
            Ok(answer) => answer,
            Err(error) if is_passing(&error) => {
                writeln!(
                    terminal.err,
                    "{error}; polling again in {} s.",
                    pause.as_secs()
                )?;
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(LONGEST_PAUSE);
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        pause = FIRST_PAUSE;
        let events: Events = serde_json::from_value(answer.body.unwrap_or_default())
            .map_err(|error| ApiError::Unexpected(error.to_string()))?;
        for event in events.data {
            let outcome = forward(&forwarder, target, &state.secret, &event).await;
            writeln!(
                terminal.out,
                "{}  {}  {}  -> {outcome}",
                event.created_at, event.kind, event.id
            )?;
            config::update(&context.config, |config| {
                let profile = config.profiles.entry(context.profile.clone()).or_default();
                let listen = profile.listen.get_or_insert_with(|| Listen {
                    secret: state.secret.clone(),
                    cursor: None,
                });
                listen.cursor = Some(event.id.clone());
            })?;
            cursor = Some(event.id);
            forwarded = forwarded.saturating_add(1);
            if stop_after.is_some_and(|stop| forwarded >= stop) {
                return Ok(());
            }
        }
    }
}

/// Whether a failed poll is worth repeating as it is: no answer, or an answer that the API is
/// unavailable or limiting for now.
fn is_passing(error: &ApiError) -> bool {
    match error {
        ApiError::Transport { .. } => true,
        ApiError::Problem(problem) => {
            matches!(Class::of_status(problem.status), Class::RetryLater)
        }
        ApiError::Config(_)
        | ApiError::Login(_)
        | ApiError::NotLoggedIn { .. }
        | ApiError::InvalidUrl(_)
        | ApiError::Client(_)
        | ApiError::Unexpected(_) => false,
    }
}

/// The newest event's id, where a first `listen` starts; none in a workspace without events.
async fn newest(api: &Api) -> Result<Option<String>, ApiError> {
    let mut request = Request::new(Method::GET, "/v1/events");
    request.set_query("limit", "1");
    let answer = api.send(&request, None).await?;
    let events: Events = serde_json::from_value(answer.body.unwrap_or_default())
        .map_err(|error| ApiError::Unexpected(error.to_string()))?;
    Ok(events.data.into_iter().next().map(|event| event.id))
}

/// POSTs one event to the local URL, signed; the outcome as the line `listen` prints shows it.
async fn forward(client: &reqwest::Client, target: &Url, secret: &str, event: &Event) -> String {
    let body = match serde_json::to_vec(&Webhook {
        kind: &event.kind,
        timestamp: &event.created_at,
        data: &event.data,
    }) {
        Ok(body) => body,
        Err(error) => return format!("not forwarded: {error}"),
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
    let Some(signature) = sign(secret, &event.id, timestamp, &body) else {
        return "not forwarded: the profile's signing secret is not a `whsec_` secret".to_owned();
    };
    let sent = client
        .post(target.clone())
        .header(CONTENT_TYPE, "application/json")
        .header("webhook-id", &event.id)
        .header("webhook-timestamp", timestamp.to_string())
        .header("webhook-signature", signature)
        .body(body)
        .send()
        .await;
    match sent {
        Ok(response) => response.status().to_string(),
        Err(error) => format!("failed: {}", api::chain(&error)),
    }
}

/// `norbelys trigger <type>`: creates a synthetic event and prints it.
///
/// # Errors
///
/// When the API refuses (an unknown type is `422`, an unknown endpoint `404`) or cannot be
/// reached, or the terminal cannot be written.
pub async fn trigger(
    context: &Context,
    api: &Api,
    kind: &str,
    endpoint: Option<&String>,
    terminal: &mut Terminal<'_>,
) -> Result<(), crate::Error> {
    let mut body = Map::new();
    body.insert("type".to_owned(), Value::String(kind.to_owned()));
    if let Some(endpoint) = endpoint {
        body.insert(
            "webhook_endpoint_id".to_owned(),
            Value::String(endpoint.clone()),
        );
    }
    let mut request = Request::new(Method::POST, "/v1/events");
    request.body = Some(Payload {
        content_type: "application/json".to_owned(),
        bytes: Value::Object(body).to_string().into_bytes(),
    });
    let answer = api.send(&request, Some(&context.idempotency_key())).await?;
    output::answer(
        terminal,
        context.json,
        answer.body.as_ref(),
        answer.etag.as_deref(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::sign;
    use crate::config;
    use crate::testing::{Fake, Reply, Scratch, Seen, run};

    /// Our signature equals the reference vector of the Standard Webhooks specification (the
    /// secret, message id, timestamp and payload its libraries test with), so a receiver
    /// verifying with any conforming library accepts what `listen` forwards.
    #[test]
    fn signatures_match_the_standard_webhooks_reference_vector() {
        assert_eq!(
            sign(
                concat!("whsec_", "MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"),
                "msg_p5jXN8AQM9LWM0D4loKWxJek",
                1_614_265_330,
                br#"{"test": 2432232314}"#
            )
            .as_deref(),
            Some("v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=")
        );
        assert!(sign("not-a-secret", "msg", 0, b"{}").is_none());
    }

    fn events(ids: &[&str]) -> Reply {
        let data: Vec<_> = ids
            .iter()
            .map(|id| json!({ "id": id, "type": "message.sent", "data": { "message_id": "msg_1" }, "synthetic": true, "webhook_endpoint_id": null, "created_at": "2026-10-02T10:00:00Z" }))
            .collect();
        Reply::json(
            200,
            json!({ "data": data, "meta": { "has_more": false, "next_cursor": null } }),
        )
    }

    /// The receiver got the webhook body with a `webhook-id` of the event, a current timestamp
    /// and a signature that verifies with the profile's secret.
    fn assert_signed(delivery: &Seen, id: &str, secret: &str) {
        assert_eq!(delivery.method, "POST");
        assert_eq!(delivery.path, "/hooks");
        assert_eq!(delivery.header("webhook-id"), Some(id));
        assert_eq!(
            delivery.json(),
            json!({ "type": "message.sent", "timestamp": "2026-10-02T10:00:00Z", "data": { "message_id": "msg_1" } })
        );
        let timestamp: i64 = delivery
            .header("webhook-timestamp")
            .unwrap()
            .parse()
            .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(timestamp.abs_diff(i64::try_from(now).unwrap()) < 60);
        assert_eq!(
            delivery.header("webhook-signature"),
            sign(secret, id, timestamp, &delivery.body).as_deref()
        );
    }

    /// A first `listen` starts after the newest event, forwards each later event signed with the
    /// profile's secret and saves the cursor; a second `listen` resumes after that cursor with
    /// the same secret. Without the cursor a restart would lose or repeat events, and a new
    /// secret would break the receiver's verification.
    #[tokio::test]
    async fn listen_forwards_signed_events_and_resumes_from_the_cursor() {
        let receiver = Fake::start(vec![
            Reply::json(200, json!({})),
            Reply::json(200, json!({})),
            Reply::json(500, json!({})),
        ])
        .await;
        let api = Fake::start(vec![events(&["evt_0"]), events(&["evt_1", "evt_2"])]).await;
        let scratch = Scratch::new();
        let hooks = format!("{}/hooks", receiver.url.trim_start_matches("http://"));
        let base = ["--api-url", api.url.as_str(), "--api-key", "nb_test_key"];
        let ran = crate::testing::listen(&scratch, &base, &hooks, 2).await;
        assert_eq!(ran.code, 0, "{}", ran.err);
        let profile = config::load(&scratch.config())
            .unwrap()
            .profiles
            .remove("default")
            .unwrap();
        let state = profile.listen.unwrap();
        assert!(state.secret.starts_with("whsec_"));
        assert_eq!(state.cursor.as_deref(), Some("evt_2"));
        assert!(ran.out.contains(&state.secret));
        assert!(ran.out.contains("evt_1") && ran.out.contains("-> 200 OK"));

        let polls = api.seen();
        assert_eq!(polls[0].query("limit"), Some("1"));
        assert_eq!(polls[1].query("after"), Some("evt_0"));
        assert_eq!(polls[1].query("wait"), Some("25"));
        assert_eq!(polls[1].header("authorization"), Some("Bearer nb_test_key"));
        let deliveries = receiver.seen();
        assert_signed(&deliveries[0], "evt_1", &state.secret);
        assert_signed(&deliveries[1], "evt_2", &state.secret);

        let resumed_api = Fake::start(vec![events(&["evt_3"])]).await;
        let base = [
            "--api-url",
            resumed_api.url.as_str(),
            "--api-key",
            "nb_test_key",
        ];
        let ran = crate::testing::listen(&scratch, &base, &hooks, 1).await;
        assert_eq!(ran.code, 0, "{}", ran.err);
        assert!(
            ran.out.contains("-> 500 Internal Server Error"),
            "{}",
            ran.out
        );
        let polls = resumed_api.seen();
        assert_eq!(polls.len(), 1);
        assert_eq!(polls[0].query("after"), Some("evt_2"));
        assert_signed(&receiver.seen()[2], "evt_3", &state.secret);
        let profile = config::load(&scratch.config())
            .unwrap()
            .profiles
            .remove("default")
            .unwrap();
        assert_eq!(profile.listen.unwrap().cursor.as_deref(), Some("evt_3"));
    }

    /// `trigger` creates a synthetic event of the type with `POST /v1/events`, an idempotency
    /// key and the endpoint when one is named, and prints the event the API returns.
    #[tokio::test]
    async fn trigger_creates_a_synthetic_event() {
        let event = json!({ "id": "evt_9", "type": "message.sent", "data": {}, "synthetic": true, "webhook_endpoint_id": "whe_1", "created_at": "2026-10-02T10:00:00Z" });
        let api = Fake::start(vec![Reply::json(201, event.clone())]).await;
        let scratch = Scratch::new();
        let ran = run(
            &scratch,
            &[
                "--api-url",
                &api.url,
                "--api-key",
                "nb_test_key",
                "--json",
                "trigger",
                "message.sent",
                "--webhook-endpoint-id",
                "whe_1",
            ],
        )
        .await;
        assert_eq!(ran.code, 0, "{}", ran.err);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&ran.out).unwrap(),
            event
        );
        let seen = api.seen();
        assert_eq!(seen[0].method, "POST");
        assert_eq!(seen[0].path, "/v1/events");
        assert_eq!(
            seen[0].json(),
            json!({ "type": "message.sent", "webhook_endpoint_id": "whe_1" })
        );
        assert!(seen[0].header("idempotency-key").is_some());
    }
}
