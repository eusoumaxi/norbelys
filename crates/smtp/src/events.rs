//! The evidence outbox: delivers the delivery events the MTA journals (from Postfix's log,
//! [`crate::tail`], from returned delivery status notifications, [`crate::bounce`], and from
//! feedback-loop complaints, [`crate::feedback`]) to the core, each to the route (the core's
//! provider webhook, [`crate::control::routes`]) its login reported to when the event was
//! recorded. A tenant is never guessed: [`journal`] records an event only for a login with a
//! route, and an event whose route was deleted becomes a dead letter.
//!
//! **The contract between the MTA and the core**, verified on the core's side by
//! `norbelys_mail::webhooks::norbelys`, which states the same rules:
//!
//! - One HTTPS `POST` to the route's URL carries 1 to 100 events of that route, as JSON:
//!
//!   ```json
//!   {"type": "mta.events", "timestamp": "2026-10-01T12:00:00Z", "data": {"events": [
//!     {"event_id": "3f6c…", "internet_message_id": "<m1.t1.tag@mail.example.com>",
//!      "username": "relay@example.com", "recipient": "alice@example.net",
//!      "queue_id": "4ZQ1aB2cDe", "kind": "delivered", "enhanced_status": "2.0.0",
//!      "detail": "250 2.0.0 OK", "provenance": "smtp_reply",
//!      "observed_at": "2026-10-01T11:59:58.123456Z"}]}}
//!   ```
//!
//!   `type` is always `mta.events`; `timestamp` is when the request was made. Each event:
//!   `event_id` (1 to 256 visible ASCII characters, unique per route and stable across
//!   retries), `kind` (`delivered`: the next hop accepted the message, never a claim about the
//!   inbox; `deferred`; `bounced`; `complaint`: a recipient reported the message as spam;
//!   `accepted` is reserved), `provenance` (`smtp_reply`: the next hop's SMTP reply, read by the
//!   MTA itself; `verp_dsn`: a delivery status notification returned to the message's VERP
//!   return path; `fbl_arf_dkim`: an abuse report whose reporter, an enrolled feedback loop,
//!   passed DKIM; `feedback_id_only`: an abuse report whose only proof is a `Feedback-ID` our own
//!   signature covers) and `observed_at` (RFC 3339) are always present; `internet_message_id`
//!   (as the MTA saw it, angle brackets included), `username` (the MTA login that submitted the
//!   message), `recipient`, `queue_id` (Postfix's), `enhanced_status` (RFC 3463) and `detail`
//!   (at most 2,000 characters) are present when known, and null otherwise.
//! - Headers per Standard Webhooks (<https://www.standardwebhooks.com/>): `webhook-id`,
//!   `webhook-timestamp` (Unix seconds) and `webhook-signature` (`v1,` + base64 HMAC-SHA256 of
//!   `{webhook-id}.{webhook-timestamp}.{body}` under the route's secret). Every attempt carries
//!   a fresh `webhook-id` and a fresh timestamp; the receiver deduplicates on `event_id`, never
//!   on `webhook-id`, and may refuse a `webhook-id` it has already seen.
//! - The receiver answers `2xx` once it has stored every event of the batch.
//!
//! Throughput: four requests in flight. A batch is the due events of the route with the oldest
//! due event, read through the `events_route_due` index and leased by moving their `next_at` a
//! minute ahead, so a crash only delays them. Claim and settlement writes are batched over SQL
//! HTTP; settlement checks the exact lease deadline and pending state, fencing late callbacks. At 2,000 events a second a request must average
//! under 200 ms.
//!
//! Outcomes: `2xx` delivers the batch (`done = 1`). `400`, `409`, `410`, `413` and `422` make it
//! a dead letter at once (`done = 2`): the receiver refused these exact bytes, so resending
//! cannot help. Anything else (no answer, `401`, `403`, `404`, `429`, `5xx`, or a redirect,
//! which is never followed) retries after 2, 4, 8 … seconds capped at an hour, or after
//! `Retry-After` on `429` and `503`, until the event is 48 hours old; then it becomes a dead
//! letter, well inside the receiver's three-day deduplication window, so a late success can
//! never be counted twice. Every event and its delivery state stay in Turso as canonical history. Local pending records
//! are removed only after Turso confirms their commit; the dispatcher never downloads history.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::db::{Connection, OptionalExtension as _, params};
use jiff::Timestamp;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, HeaderMap, RETRY_AFTER};
use serde::Serialize;
use serde_json::value::RawValue;
use tokio::task::JoinSet;

use crate::crypto::{self, Keys, WebhookKey};
use crate::db::{self, Db};
use crate::serve::Shutdown;
use crate::telemetry;

/// Requests in flight.
const IN_FLIGHT: usize = 4;
/// Events per request.
const BATCH: i64 = 100;
/// How far a claim moves `next_at`: longer than a request may take.
const LEASE_SECONDS: f64 = 60.0;
/// An event older than this is not retried again.
const MAX_AGE_SECONDS: f64 = 48.0 * 3600.0;
/// How long the dispatcher waits when nothing is due.
const IDLE: Duration = Duration::from_millis(500);

/// One event of the contract, as it is journaled and later posted.
#[derive(Debug, Serialize)]
pub struct Evidence<'a> {
    /// Unique per route and stable across retries.
    pub event_id: &'a str,
    /// The message's `Message-ID`, angle brackets included, when known.
    pub internet_message_id: Option<&'a str>,
    /// The MTA login that submitted the message; it selects the route.
    pub username: &'a str,
    /// The recipient the event is about, when known: a feedback loop may redact the recipient
    /// of a complaint, and the event is still evidence about the message.
    pub recipient: Option<&'a str>,
    /// Postfix's queue id of the submission, when known.
    pub queue_id: Option<&'a str>,
    /// `delivered`, `deferred`, `bounced` or `complaint`.
    pub kind: &'static str,
    /// The RFC 3463 status, when known.
    pub enhanced_status: Option<&'a str>,
    /// The reporter's diagnostic, at most 2,000 characters.
    pub detail: Option<String>,
    /// `smtp_reply`, `verp_dsn`, `fbl_arf_dkim` or `feedback_id_only`.
    pub provenance: &'static str,
    /// When the MTA observed it, RFC 3339.
    pub observed_at: String,
}

/// What became of an event offered to the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Journaled {
    /// Recorded, to be posted to its route.
    Inserted,
    /// Already recorded under this `event_id` (a line or a notification read again).
    Duplicate,
    /// Its login reports to no route: not recorded, since no tenant may be guessed.
    Unrouted,
}

/// Captures `event` for the route its login reports to now. Committing the transaction
/// publishes it to the bounded local queue; the archiver then confirms it in Turso.
///
/// # Errors
///
/// The database fails, or the event cannot be serialized.
pub fn journal(tx: &db::Transaction<'_>, event: &Evidence<'_>) -> db::Result<Journaled> {
    let route: Option<String> = tx
        .prepare_cached("SELECT provider_webhook_id FROM account_routes WHERE username = ?1")?
        .query_row([event.username], |row| row.get(0))
        .optional()?;
    let Some(route) = route else {
        return Ok(Journaled::Unrouted);
    };
    let payload = serde_json::to_string(event)
        .map_err(|error| db::Error::ToSqlConversionFailure(Box::new(error)))?;
    let exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM events WHERE id = ?1",
            [event.event_id],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_some() {
        return Ok(Journaled::Duplicate);
    }
    tx.capture(crate::queue::Record::Event {
        id: event.event_id.to_owned(),
        route,
        payload,
        created: db::now(),
    })?;
    Ok(Journaled::Inserted)
}

/// One journaled event.
struct Row {
    id: String,
    payload: String,
    attempts: i64,
    created: f64,
    leased_until: f64,
}

/// One route's claimed events, with the route's URL and sealed secret when it still exists.
struct Batch {
    route: String,
    target: Option<(String, Vec<u8>)>,
    rows: Vec<Row>,
}

/// What became of a request.
#[derive(Debug, Clone, Copy)]
enum Outcome {
    Delivered,
    Retry(Option<f64>),
    Dead,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Retry(_) => "retry",
            Self::Dead => "dead",
        }
    }
}

/// A finished request, to be written back.
struct Settled {
    route: String,
    rows: Vec<Row>,
    outcome: Outcome,
    status: Option<u16>,
    duration: Duration,
}

#[derive(Serialize)]
struct Envelope<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    timestamp: String,
    data: Data<'a>,
}

#[derive(Serialize)]
struct Data<'a> {
    events: Vec<&'a RawValue>,
}

struct Instruments {
    events: Counter<u64>,
    duration: Histogram<f64>,
}

impl Instruments {
    fn new() -> Self {
        let meter = telemetry::meter();
        Self {
            events: meter
                .u64_counter("norbelys_mta_events_dispatched")
                .with_description("Events by the outcome of their request: delivered, retry, dead")
                .build(),
            duration: meter
                .f64_histogram("norbelys_mta_dispatch_duration_seconds")
                .with_unit("s")
                .with_description("Duration of one evidence request")
                .with_boundaries(vec![0.025, 0.05, 0.1, 0.2, 0.5, 1.0, 2.5, 5.0, 10.0, 15.0])
                .build(),
        }
    }
}

/// Claims up to `slots` batches, each the due events of the route with the oldest due event.
fn claim(conn: &mut Connection, slots: usize) -> db::Result<Vec<Batch>> {
    let now = db::now();
    let tx = conn.transaction()?;
    let mut batches = Vec::with_capacity(slots);
    for _ in 0..slots {
        // The tail never journals an event without a route.
        let route: Option<String> = tx
            .prepare_cached(
                "SELECT provider_webhook_id FROM events
                  WHERE done = 0 AND next_at <= ?1 AND provider_webhook_id IS NOT NULL ORDER BY next_at LIMIT 1",
            )?
            .query_row([now], |row| row.get(0))
            .optional()?;
        let Some(route) = route else { break };
        let rows = tx
            .prepare_cached(
                "SELECT id, payload, attempts, created FROM events
                  WHERE provider_webhook_id = ?1 AND done = 0 AND next_at <= ?2 ORDER BY next_at LIMIT ?3",
            )?
            .query_map(params![route, now, BATCH], |row| {
                Ok(Row {
                    id: row.get(0)?,
                    payload: row.get(1)?,
                    attempts: row.get(2)?,
                    created: row.get(3)?,
                    leased_until: now + LEASE_SECONDS,
                })
            })?
            .collect::<db::Result<Vec<_>>>()?;
        tx.execute_many(
            "UPDATE events SET next_at = ?1 WHERE id = ?2",
            rows.iter()
                .map(|row| vec![row.leased_until.into(), row.id.clone().into()])
                .collect(),
        )?;
        let target = tx
            .prepare_cached("SELECT url, secret FROM routes WHERE provider_webhook_id = ?1")?
            .query_row([&route], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        batches.push(Batch {
            route,
            target,
            rows,
        });
    }
    tx.commit()?;
    Ok(batches)
}

/// Sends a claim, bisecting an oversized request until each event fits or is refused alone.
/// A delivered half is settled independently; it is never resent with a failing sibling.
async fn send(client: reqwest::Client, keys: Arc<Keys>, batch: Batch) -> Vec<Settled> {
    let Batch {
        route,
        target,
        rows,
    } = batch;
    let missing_route = target.is_none();
    let target = target.and_then(|(url, sealed)| match keys.open(&sealed, route.as_bytes()) {
        Ok(secret) => Some((url, WebhookKey::new(&secret))),
        Err(_) => {
            tracing::error!(route = %route, error_code = "route_secret_invalid", "route secret cannot be opened");
            None
        }
    });
    let mut pending = vec![rows];
    let mut settled = Vec::new();
    while let Some(mut rows) = pending.pop() {
        let started = Instant::now();
        let (outcome, status) = match &target {
            Some((url, key)) => post(&client, url, key, &rows).await,
            None => (
                if missing_route {
                    Outcome::Dead
                } else {
                    Outcome::Retry(None)
                },
                None,
            ),
        };
        if status == Some(413) && rows.len() > 1 {
            let right = rows.split_off(rows.len() / 2);
            pending.push(right);
            pending.push(rows);
            continue;
        }
        settled.push(Settled {
            route: route.clone(),
            rows,
            outcome,
            status,
            duration: started.elapsed(),
        });
    }
    settled
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    key: &WebhookKey,
    rows: &[Row],
) -> (Outcome, Option<u16>) {
    let body = match body(rows) {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(error = %error, "a journaled event is not valid JSON");
            return (Outcome::Dead, None);
        }
    };
    // A fresh id for every attempt: the receiver deduplicates on `event_id`, never on this.
    let id = match crypto::random_token(16) {
        Ok(token) => format!("msg_{token}"),
        Err(error) => {
            tracing::error!(error = %error, "no webhook-id: the random source failed");
            return (Outcome::Retry(None), None);
        }
    };
    let timestamp = Timestamp::now().as_second();
    let signature = key.sign(&id, timestamp, &body);
    let span = tracing::info_span!(
        "smtp.evidence",
        otel.kind = "client",
        http.request.method = "POST",
        http.response.status_code = tracing::field::Empty,
        otel.status_code = tracing::field::Empty
    );
    use tracing::Instrument as _;
    let response = client
        .post(url)
        .header(CONTENT_TYPE, "application/json")
        .header("webhook-id", id)
        .header("webhook-timestamp", timestamp.to_string())
        .header("webhook-signature", signature)
        .body(body)
        .send()
        .instrument(span.clone())
        .await;
    match response {
        Ok(response) => {
            let status = response.status();
            span.record("http.response.status_code", status.as_u16());
            if !status.is_success() {
                span.record("otel.status_code", "ERROR");
            }
            (
                classify(status, retry_after(response.headers())),
                Some(status.as_u16()),
            )
        }
        Err(error) => {
            span.record("otel.status_code", "ERROR");
            tracing::warn!(error = %error.without_url(), error_code = "dispatch_transport", "evidence request failed");
            (Outcome::Retry(None), None)
        }
    }
}

fn body(rows: &[Row]) -> serde_json::Result<Vec<u8>> {
    let events = rows
        .iter()
        .map(|row| serde_json::from_str::<&RawValue>(&row.payload))
        .collect::<serde_json::Result<Vec<_>>>()?;
    serde_json::to_vec(&Envelope {
        kind: "mta.events",
        timestamp: Timestamp::now().to_string(),
        data: Data { events },
    })
}

fn classify(status: StatusCode, retry_after: Option<f64>) -> Outcome {
    match status.as_u16() {
        200..=299 => Outcome::Delivered,
        // Only an individually oversized event reaches this terminal outcome.
        400 | 409 | 410 | 413 | 422 => Outcome::Dead,
        429 | 503 => Outcome::Retry(retry_after),
        _ => Outcome::Retry(None),
    }
}

/// `Retry-After` in seconds, between one second and an hour; dates are ignored.
fn retry_after(headers: &HeaderMap) -> Option<f64> {
    let seconds = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    Some(f64::from(seconds.clamp(1, 3600)))
}

/// 2, 4, 8 … seconds after the `attempts`-th failure, capped at an hour.
fn backoff(attempts: i64) -> f64 {
    let exponent = u32::try_from(attempts.clamp(0, 11)).unwrap_or(11) + 1;
    f64::from(2_u32.pow(exponent)).min(3600.0)
}

/// Settles only the still-pending rows of this exact claim: a late callback cannot overwrite
/// a newer claim or reopen a completed event. Writes the whole bounded batch in one request.
fn settle(conn: &mut Connection, settled: &Settled) -> db::Result<()> {
    let now = db::now();
    let status = settled.status.map(i64::from);
    let tx = conn.transaction()?;
    let updates = settled
        .rows
        .iter()
        .map(|row| {
            let (done, next_at): (i64, f64) = match settled.outcome {
                Outcome::Delivered => (1, now),
                Outcome::Dead => (2, now),
                Outcome::Retry(_) if now - row.created > MAX_AGE_SECONDS => (2, now),
                Outcome::Retry(after) => (0, now + after.unwrap_or_else(|| backoff(row.attempts))),
            };
            vec![
                done.into(),
                row.attempts.saturating_add(1).into(),
                next_at.into(),
                status.map_or(
                    rusqlite::types::Value::Null,
                    rusqlite::types::Value::Integer,
                ),
                row.id.clone().into(),
                row.leased_until.into(),
            ]
        })
        .collect();
    // A re-claim is due only at or after the previous deadline, so its new deadline is
    // strictly later. The exact stored deadline fences old callbacks without another counter.
    tx.execute_many("UPDATE events SET done = ?1, attempts = ?2, next_at = ?3, last_status = ?4 WHERE id = ?5 AND done = 0 AND next_at = ?6", updates)?;
    tx.commit()
}

fn report(instruments: &Instruments, settled: &Settled) {
    let events = u64::try_from(settled.rows.len()).unwrap_or(u64::MAX);
    let outcome = settled.outcome.label();
    instruments
        .events
        .add(events, &[KeyValue::new("outcome", outcome)]);
    instruments
        .duration
        .record(settled.duration.as_secs_f64(), &[]);
    let duration_ms = u64::try_from(settled.duration.as_millis()).unwrap_or(u64::MAX);
    let status = settled.status.unwrap_or(0);
    telemetry::unit(telemetry::Event::Dispatch);
    if matches!(settled.outcome, Outcome::Delivered) {
        tracing::info!(event = "mta.dispatch", route = %settled.route, events, status, duration_ms, outcome, "mta.dispatch");
    } else {
        tracing::warn!(event = "mta.dispatch", route = %settled.route, events, status, duration_ms, outcome, "mta.dispatch");
    }
}

/// Runs the outbox until shutdown, then lets the requests in flight finish and records them.
///
/// # Errors
///
/// The HTTP client cannot be built, or the database fails.
pub async fn run(db: Db, keys: Arc<Keys>, mut shutdown: Shutdown) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .https_only(true)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("norbelys-smtp/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let instruments = Instruments::new();
    let mut in_flight: JoinSet<Vec<Settled>> = JoinSet::new();
    loop {
        let stopping = shutdown.requested();
        let free = IN_FLIGHT.saturating_sub(in_flight.len());
        let mut claimed = 0;
        if free > 0 && !stopping {
            let batches = db
                .call(move |conn| claim(conn, free).map_err(anyhow::Error::from))
                .await?;
            claimed = batches.len();
            for batch in batches {
                in_flight.spawn(send(client.clone(), Arc::clone(&keys), batch));
            }
        }
        let pause = if claimed > 0 { Duration::ZERO } else { IDLE };
        tokio::select! {
            Some(joined) = in_flight.join_next() => {
                for settled in joined? {
                    report(&instruments, &settled);
                    db.call(move |conn| settle(conn, &settled).map_err(anyhow::Error::from)).await?;
                }
            }
            () = tokio::time::sleep(pause), if !stopping => {}
            () = shutdown.wait(), if !stopping => {}
            else => return Ok(()),
        }
    }
}

/// The outbox's state, for the metrics.
#[derive(Debug, Clone, Copy)]
pub struct Backlog {
    /// Events not yet delivered.
    pub pending: u64,
    /// Dead letters awaiting review.
    pub dead: u64,
    /// Age of the oldest pending event, in seconds.
    pub oldest_seconds: f64,
}

/// Reads the outbox's state.
///
/// # Errors
///
/// A query fails.
pub fn backlog(conn: &Connection) -> db::Result<Backlog> {
    let (pending, oldest): (i64, Option<f64>) = conn.query_row(
        "SELECT count(*), min(created) FROM events WHERE done = 0",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let dead: i64 = conn.query_row("SELECT count(*) FROM events WHERE done = 2", [], |row| {
        row.get(0)
    })?;
    Ok(Backlog {
        pending: u64::try_from(pending).unwrap_or(0),
        dead: u64::try_from(dead).unwrap_or(0),
        oldest_seconds: oldest.map_or(0.0, |created| (db::now() - created).max(0.0)),
    })
}

#[cfg(test)]
mod tests {
    /// One oversized event cannot dead-letter its delivered siblings or make them retry.
    #[tokio::test]
    async fn oversized_batches_split_and_settle_independently() {
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let recorder = Arc::clone(&seen);
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let recorder = Arc::clone(&recorder);
                async move {
                    let ids: Vec<String> = body["data"]["events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|event| event["event_id"].as_str().unwrap().to_owned())
                        .collect();
                    recorder.lock().unwrap().push(ids.clone());
                    if ids.len() > 1 || ids[0] == "large" {
                        StatusCode::PAYLOAD_TOO_LARGE
                    } else {
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let keys = Arc::new(Keys::from_secret(SECRET).unwrap());
        let secret = crypto::decode_secret(SECRET).unwrap();
        let sealed = keys.seal(&secret, b"route").unwrap();
        let rows = ["small-a", "large", "small-b"]
            .into_iter()
            .map(|id| Row {
                id: id.into(),
                payload: serde_json::json!({ "event_id": id }).to_string(),
                attempts: 0,
                created: 0.0,
                leased_until: 0.0,
            })
            .collect();
        let settled = send(
            reqwest::Client::new(),
            keys,
            Batch {
                route: "route".into(),
                target: Some((url, sealed)),
                rows,
            },
        )
        .await;
        assert_eq!(settled.len(), 3);
        for part in &settled {
            assert_eq!(part.rows.len(), 1);
            assert_eq!(
                matches!(part.outcome, Outcome::Dead),
                part.rows[0].id == "large"
            );
            assert_eq!(
                matches!(part.outcome, Outcome::Delivered),
                part.rows[0].id != "large"
            );
        }
        for id in ["small-a", "small-b"] {
            assert_eq!(
                seen.lock()
                    .unwrap()
                    .iter()
                    .filter(|ids| ids.len() == 1 && ids[0] == id)
                    .count(),
                1
            );
        }
        server.abort();
    }

    use std::sync::Mutex;

    use axum::http::HeaderMap as Headers;
    use norbelys_mail::webhooks::{EventKind, Provenance, norbelys};

    use super::*;
    use crate::testing::{SECRET, memory};

    /// Inserts `count` pending events for `route`, created `age` seconds ago and due now.
    fn insert(conn: &Connection, route: &str, count: usize, age: f64) {
        let created = db::now() - age;
        for n in 0..count {
            conn.execute(
                "INSERT INTO events (id, provider_webhook_id, payload, created) VALUES (?1, ?2, ?3, ?4)",
                params![format!("{route}-{n}-{age}"), route, format!(r#"{{"event_id":"{route}-{n}"}}"#), created],
            )
            .unwrap();
        }
    }

    /// The receiver's answer decides each batch: `2xx` delivers it; `400`, `409`, `410`, `413`
    /// and `422` refuse these bytes for good; everything else, redirects included, is retried,
    /// honouring `Retry-After` (seconds only, clamped to an hour) on `429` and `503`.
    #[test]
    fn classifies_receiver_answers() {
        let outcome =
            |code: u16, wait: Option<f64>| classify(StatusCode::from_u16(code).unwrap(), wait);
        assert!(matches!(outcome(200, None), Outcome::Delivered));
        assert!(matches!(outcome(204, None), Outcome::Delivered));
        for dead in [400, 409, 410, 413, 422] {
            assert!(matches!(outcome(dead, None), Outcome::Dead), "{dead}");
        }
        for retry in [301, 401, 403, 404, 500, 502] {
            assert!(
                matches!(outcome(retry, Some(9.0)), Outcome::Retry(None)),
                "{retry}"
            );
        }
        assert!(matches!(outcome(429, Some(9.0)), Outcome::Retry(Some(9.0))));

        let mut headers = HeaderMap::new();
        for (value, expected) in [
            ("30", Some(30.0)),
            ("0", Some(1.0)),
            ("99999", Some(3600.0)),
            ("Wed, 21 Oct 2015 07:28:00 GMT", None),
        ] {
            headers.insert(RETRY_AFTER, value.parse().unwrap());
            assert_eq!(retry_after(&headers), expected, "{value}");
        }
    }

    /// Retries wait 2, 4, 8 … seconds after each failure (2,048 after the eleventh) and an
    /// hour from the twelfth on, never longer.
    #[test]
    fn backs_off_exponentially_to_an_hour() {
        let waits: Vec<f64> = (0..14).map(backoff).collect();
        assert_eq!(&waits[..4], &[2.0, 4.0, 8.0, 16.0]);
        assert_eq!(waits[10], 2048.0);
        assert!(
            waits[11..]
                .iter()
                .all(|w| (*w - 3600.0).abs() < f64::EPSILON)
        );
    }

    /// A batch holds one route's due events, at most 100, the route with the oldest due event
    /// first; claimed events are leased so no other claim takes them while in flight.
    #[test]
    fn claims_per_route_in_batches_of_100() {
        let mut conn = memory();
        insert(&conn, "pwh_old", 3, 120.0);
        insert(&conn, "pwh_new", 150, 60.0);
        conn.execute("UPDATE events SET next_at = created", [])
            .unwrap();
        let batches = claim(&mut conn, 4).unwrap();
        let shape: Vec<(String, usize)> = batches
            .iter()
            .map(|b| (b.route.clone(), b.rows.len()))
            .collect();
        assert_eq!(
            shape,
            [
                ("pwh_old".to_owned(), 3),
                ("pwh_new".to_owned(), 100),
                ("pwh_new".to_owned(), 50)
            ]
        );
        assert!(batches.iter().all(|b| b.target.is_none()));
        assert!(claim(&mut conn, 4).unwrap().is_empty());
    }

    /// Writing a batch back: delivered and dead rows are done; a retry is due after its
    /// backoff, unless the event is older than 48 hours, which makes it a dead letter.
    #[test]
    fn settles_each_outcome() {
        let mut conn = memory();
        insert(&conn, "pwh_1", 1, 10.0);
        insert(&conn, "pwh_1", 1, MAX_AGE_SECONDS + 10.0);
        let rows = |conn: &mut Connection| claim(conn, 1).unwrap().remove(0).rows;
        let state = |conn: &Connection| -> Vec<(i64, i64, Option<i64>)> {
            conn.prepare("SELECT done, attempts, last_status FROM events ORDER BY created DESC")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<db::Result<_>>()
                .unwrap()
        };
        let settled = |rows, outcome, status| Settled {
            route: "pwh_1".to_owned(),
            rows,
            outcome,
            status,
            duration: Duration::ZERO,
        };

        let claimed = rows(&mut conn);
        settle(
            &mut conn,
            &settled(claimed, Outcome::Retry(None), Some(503)),
        )
        .unwrap();
        assert_eq!(state(&conn), [(0, 1, Some(503)), (2, 1, Some(503))]);
        let next_at: f64 = conn
            .query_row(
                "SELECT next_at - ?1 FROM events WHERE done = 0",
                [db::now()],
                |r| r.get(0),
            )
            .unwrap();
        assert!((1.0..=2.5).contains(&next_at));

        conn.execute("UPDATE events SET next_at = 0 WHERE done = 0", [])
            .unwrap();
        let claimed = rows(&mut conn);
        settle(&mut conn, &settled(claimed, Outcome::Delivered, Some(200))).unwrap();
        assert_eq!(state(&conn)[0], (1, 2, Some(200)));
    }

    /// A callback from an expired claim cannot delay a newer claim or reopen its completed
    /// event; the exact deadline and pending state protect canonical delivery history.
    #[test]
    fn settlement_is_fenced_by_the_current_claim() {
        let mut conn = memory();
        insert(&conn, "pwh_1", 1, 10.0);
        let first = claim(&mut conn, 1).unwrap().remove(0);
        conn.execute("UPDATE events SET next_at = 0", []).unwrap();
        let second = claim(&mut conn, 1).unwrap().remove(0);
        let newer_deadline = second.rows[0].leased_until;
        let old = Settled {
            route: first.route,
            rows: first.rows,
            outcome: Outcome::Retry(None),
            status: Some(503),
            duration: Duration::ZERO,
        };
        settle(&mut conn, &old).unwrap();
        let state = || {
            conn.query_row("SELECT done, attempts, next_at FROM events", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })
            .unwrap()
        };
        assert_eq!(state(), (0, 0, newer_deadline));
        settle(
            &mut conn,
            &Settled {
                route: second.route,
                rows: second.rows,
                outcome: Outcome::Delivered,
                status: Some(200),
                duration: Duration::ZERO,
            },
        )
        .unwrap();
        settle(&mut conn, &old).unwrap();
        assert_eq!(
            conn.query_row("SELECT done, attempts FROM events", [], |row| Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?
            )))
            .unwrap(),
            (1, 1)
        );
    }

    /// The contract holds end to end: a batch posted by the outbox is accepted by the core's
    /// own verifier (`norbelys_mail::webhooks::norbelys`), signature, envelope and every event
    /// field included, and each attempt carries a fresh `webhook-id`.
    #[tokio::test]
    async fn posts_batches_the_core_verifies() {
        let seen: Arc<Mutex<Vec<(Headers, String)>>> = Arc::default();
        let recorder = Arc::clone(&seen);
        let app = axum::Router::new().route(
            "/webhooks/pwh_1",
            axum::routing::post(move |headers: Headers, body: String| async move {
                recorder.lock().unwrap().push((headers, body));
                StatusCode::NO_CONTENT
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/webhooks/pwh_1", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let secret = crypto::decode_secret(SECRET).unwrap();
        let payload = serde_json::to_string(&Evidence {
            event_id: "e1",
            internet_message_id: Some("<m1.t1.tag@mail.example.com>"),
            username: "relay@example.com",
            recipient: Some("alice@example.net"),
            queue_id: Some("4ZQ1aB2cDe"),
            kind: "bounced",
            enhanced_status: Some("5.1.1"),
            detail: Some("550 5.1.1 user unknown".to_owned()),
            provenance: "verp_dsn",
            observed_at: "2026-10-01T11:59:58.123456Z".to_owned(),
        })
        .unwrap();
        let rows = || {
            vec![Row {
                id: "e1".to_owned(),
                payload: payload.clone(),
                attempts: 0,
                created: 0.0,
                leased_until: 0.0,
            }]
        };
        let client = reqwest::Client::new();
        let key = WebhookKey::new(&secret);
        let (outcome, status) = post(&client, &url, &key, &rows()).await;
        assert!(matches!(outcome, Outcome::Delivered));
        assert_eq!(status, Some(204));
        post(&client, &url, &key, &rows()).await;

        let seen = seen.lock().unwrap();
        let (headers, body) = &seen[0];
        let core = norbelys::NorbelysKey::new(&secret).unwrap();
        let receipts = norbelys::verify(&core, headers, body.as_bytes(), Timestamp::now()).unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].event_id, "e1");
        let event = norbelys::events(&receipts[0].raw).unwrap().remove(0);
        assert_eq!(
            (event.kind, event.provenance),
            (EventKind::Bounced, Some(Provenance::VerpDsn))
        );
        assert_eq!(
            event.internet_message_id.as_deref(),
            Some("<m1.t1.tag@mail.example.com>")
        );
        assert_eq!(event.recipient.as_deref(), Some("alice@example.net"));
        assert_eq!(event.provider_message_id.as_deref(), Some("4ZQ1aB2cDe"));
        assert_eq!(
            event.status.map(|s| s.to_string()).as_deref(),
            Some("5.1.1")
        );
        assert_eq!(
            (event.smtp_code, event.diagnostic.as_deref()),
            (Some(550), Some("550 5.1.1 user unknown"))
        );
        assert_eq!(event.observed_at.to_string(), "2026-10-01T11:59:58.123456Z");
        assert_ne!(seen[1].0["webhook-id"], seen[0].0["webhook-id"]);
    }
}
