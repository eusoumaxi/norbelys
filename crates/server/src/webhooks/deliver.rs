//! Webhook deliveries: the `webhook.deliver` job that makes each attempt, the guarded HTTP
//! client it uses, and the reads and reopening of deliveries the API serves.
//!
//! # One attempt
//!
//! The job loads its delivery, its endpoint and its event in a short read-only transaction and
//! commits before calling out; a database transaction is never held across a request to a
//! customer's server. It then POSTs the body `{"type", "timestamp", "data"}` with the Standard
//! Webhooks headers: `webhook-id` is the delivery's id (`whd_…`), stable across every attempt;
//! `webhook-timestamp` is the attempt's unix time in seconds; `webhook-signature` is one
//! `v1,<base64>` HMAC-SHA256 per valid secret, space-separated (two during the 24 hours after a
//! rotation). The request has a 15-second deadline, well inside the job's 60-second lease, so a
//! recovered job never runs while an earlier request is still out. Finally it records the
//! attempt in a chunk fenced by its lease: the attempt count, the start, the duration, the
//! status and the first kilobyte of the answer, and the next step. The request runs in its own
//! span (`webhook.delivery`), linked to the API request that caused the event when one did (the
//! event's `traceparent`; the job itself, enqueued by the outbox relay, keeps none).
//!
//! The delivery row owns the schedule (`attempt`, `next_attempt_at`, `state`); the job yields
//! until the next attempt is due, so the job's own failed-attempt count only grows on
//! infrastructure failures. Outcomes:
//!
//! - `2xx`: `delivered`; the endpoint's `failing_since` and `failure_notified_at` are cleared,
//!   which ends its failing period.
//! - `410 Gone`: the endpoint is disabled at once (`gone`) and its pending deliveries stop.
//! - anything else, a redirect (never followed), a timeout or a refused address: a failure; the
//!   next attempt waits for the schedule's next step (or the peer's `Retry-After` on `429`,
//!   `502`, `503` and `504`), and after the tenth attempt the delivery is `failed`. The
//!   endpoint's `failing_since` records the first failure after a success; 5 days without a
//!   success disable it (`failing`).
//!
//! The workspace's admins hear of a failing endpoint by email twice at most per failing period:
//! when the first of its deliveries uses up its retries (about three days; `failure_notified_at`
//! records it, so later exhausted deliveries of the period tell nobody again), and when it is
//! disabled for failing. Each email is a `webhook_endpoint.failure_email` job enqueued in the
//! attempt's chunk, sent through the platform's transactional mail; it says nothing when the news
//! no longer holds by the time it runs (the endpoint recovered, or was enabled again). A success,
//! or enabling the endpoint again, ends the period.
//!
//! # The guard
//!
//! The URL comes from a customer, so the client refuses requests that would reach our own
//! network: literal private, loopback, link-local, shared, documentation or reserved addresses
//! are refused before the request, and a name is resolved by our resolver, which refuses the
//! whole answer if any address is not public. The check runs at connection time, so DNS
//! rebinding cannot slip an inward address past an earlier check. `https` is required, proxies
//! from the environment are ignored and redirects are never followed. A development deployment
//! may allow private targets and `http`.
//!
//! # Delivery ids
//!
//! `webhook_deliveries` is partitioned by its event's id, and its key is
//! `(workspace_id, event_id, id)`: a delivery cannot be found by its id alone without visiting
//! every partition. So a delivery's id is a UUIDv7 whose first 48 bits (the unix millisecond)
//! are copied from its event's id, and the rest is fresh. From a delivery id we know its
//! event's millisecond, and a lookup bounds `event_id` to that one millisecond: one partition,
//! a few index entries. The ids stay unique (74 random bits) and time-ordered by event.
//!
//! # Lock order
//!
//! The endpoint row, then its delivery rows, then job rows. Recording an attempt locks the
//! endpoint before it updates the delivery, as disabling an endpoint (which then stops its
//! pending deliveries) and deleting one (which cascades to them) do, so a person disabling an
//! endpoint while one of its attempts is recorded never deadlocks with it.

use std::error::Error as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::Instrument as _;
use uuid::Uuid;

use super::EventType;
use super::endpoints::{self, DisabledReason, Secrets};
use crate::crypto::{self, Keys};
use crate::db::Tx;
use crate::delivery::accept::{self, Transactional};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, OutboxEvent, WebhookDelivery, WebhookEndpoint, WorkspaceId};
use crate::domain::retry;
use crate::domain::time::Timestamp;
use crate::identity::memberships;
use crate::jobs::{self, Effect, Job, JobContext, JobError, Outcome, Queue};

/// The deadline of one request, connection included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// The deadline of the connection alone.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How much of an answer's body is kept with the delivery.
const EXCERPT_BYTES: usize = 1_024;
/// How long an endpoint may fail without a success before it is disabled.
const FAILING_LIMIT_DAYS: i32 = 5;
/// The bounds of a peer's `Retry-After`: never sooner than the schedule's first step, never
/// later than its last.
const RETRY_AFTER_MIN: Duration = Duration::from_secs(5);
const RETRY_AFTER_MAX: Duration = Duration::from_secs(24 * 3_600);

static DELIVERIES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_webhook_deliveries_total")
        .with_description("Webhook delivery attempts, by outcome.")
        .build()
});

/// `webhook.deliver`: makes the next attempt of one delivery (see the module).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deliver {
    /// The delivery to attempt.
    pub delivery: Id<WebhookDelivery>,
}

impl Job for Deliver {
    const KIND: &'static str = "webhook.deliver";
    const QUEUE: Queue = Queue::Webhooks;
    const EFFECT: Effect = Effect::ExternalRetryable;

    fn unique_key(&self) -> Option<String> {
        Some(self.delivery.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        let mut chunk = cx.begin().await?;
        let loaded = load(chunk.tx(), workspace, self.delivery).await?;
        drop(chunk);
        let Some(loaded) = loaded else {
            // The delivery went with its endpoint.
            return Ok(Outcome::Done);
        };
        if loaded.state != "pending" {
            return Ok(Outcome::Done);
        }
        if !loaded.enabled {
            let mut chunk = cx.begin().await?;
            sqlx::query!(
                "UPDATE webhook_deliveries SET state = 'disabled'
                  WHERE workspace_id = $1 AND event_id = $2 AND id = $3 AND state = 'pending'",
                workspace.uuid(),
                loaded.event_id,
                self.delivery.uuid(),
            )
            .execute(&mut **chunk.tx())
            .await?;
            cx.checkpoint(chunk, json!({ "attempts": loaded.attempt }))
                .await?;
            return Ok(Outcome::Done);
        }
        let wait = loaded
            .next_attempt_at
            .0
            .duration_since(crate::process::now().0);
        if wait > jiff::SignedDuration::from_secs(1) {
            return Ok(Outcome::Yield {
                after: Duration::try_from(wait).unwrap_or(Duration::ZERO),
            });
        }

        let sender = cx.env::<Sender>()?.clone();
        let keys = cx.env::<Keys>()?.clone();
        let secrets = loaded
            .secret
            .as_deref()
            .ok_or_else(|| JobError::Failed("the endpoint's secret is not readable".to_owned()))
            .and_then(|sealed| {
                endpoints::open_secrets(&keys, workspace, loaded.endpoint_id, sealed)
                    .map_err(|error| JobError::Failed(error.to_string()))
            })?;
        // The attempt's span links to the request that caused the event, when one did, never
        // continuing its trace. The event's own `traceparent` is the one to follow: this job was
        // enqueued by the outbox relay, not by a request, so the job keeps none.
        let span = tracing::info_span!("webhook.delivery", delivery_id = %self.delivery);
        if let Some(trace_parent) = &loaded.trace_parent {
            crate::telemetry::link(&span, trace_parent);
        }
        let attempt = sender
            .attempt(&loaded, self.delivery, &secrets)
            .instrument(span)
            .await;

        let mut chunk = cx.begin().await?;
        let recorded = record_attempt(
            chunk.tx(),
            workspace,
            &loaded,
            self.delivery,
            &attempt,
            jobs::draw(),
        )
        .await?;
        cx.checkpoint(chunk, json!({ "attempts": recorded.attempts }))
            .await?;
        crate::telemetry::unit(crate::telemetry::Event::WebhookDelivery);
        tracing::info!(
            event = "webhook.delivery",
            delivery_id = %self.delivery,
            job_id = %cx.id(),
            endpoint_id = %Id::<WebhookEndpoint>::from_uuid(loaded.endpoint_id),
            event_type = %loaded.event_type,
            attempt = recorded.attempts,
            status = attempt.status,
            duration_ms = attempt.duration_ms,
            outcome = recorded.outcome,
            "webhook delivery attempt"
        );
        DELIVERIES.add(1, &[KeyValue::new("outcome", recorded.outcome)]);
        Ok(match recorded.next {
            Some(after) => Outcome::Yield { after },
            None => Outcome::Done,
        })
    }
}

/// What one attempt needs, read before the request.
struct Loaded {
    state: String,
    attempt: i16,
    next_attempt_at: Timestamp,
    endpoint_id: Uuid,
    event_id: Uuid,
    url: String,
    enabled: bool,
    event_type: String,
    data: Option<Value>,
    created_at: Timestamp,
    secret: Option<Vec<u8>>,
    /// The W3C `traceparent` of the request that caused the event, when one did.
    trace_parent: Option<String>,
}

async fn load(
    tx: &mut Tx,
    workspace: WorkspaceId,
    delivery: Id<WebhookDelivery>,
) -> Result<Option<Loaded>, sqlx::Error> {
    let (low, high) = event_bounds(delivery.uuid());
    // The secret is read through its accessor: the worker login cannot select the column.
    sqlx::query_as!(
        Loaded,
        r#"SELECT d.state, d.attempt, d.next_attempt_at AS "next_attempt_at: Timestamp", d.endpoint_id, d.event_id,
                  e.url, e.enabled, ev.type AS event_type, ev.payload -> 'data' AS data,
                  ev.created_at AS "created_at: Timestamp", webhook_endpoint_secret($1, e.id) AS secret,
                  ev.trace_parent
             FROM webhook_deliveries d
             JOIN webhook_endpoints e ON e.workspace_id = d.workspace_id AND e.id = d.endpoint_id
             JOIN outbox_events ev ON ev.workspace_id = d.workspace_id AND ev.id = d.event_id
            WHERE d.workspace_id = $1 AND d.id = $2 AND d.event_id >= $3 AND d.event_id < $4"#,
        workspace.uuid(),
        delivery.uuid(),
        low,
        high,
    )
    .fetch_optional(&mut **tx)
    .await
}

/// The body of a webhook, in the order the Standard Webhooks specification shows it.
#[derive(Serialize)]
struct Body<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    timestamp: Timestamp,
    data: &'a Value,
}

/// What one attempt observed.
struct Attempt {
    started_at: Timestamp,
    duration_ms: i32,
    /// The answer's status; none when nothing was answered (timeout, refusal, network).
    status: Option<u16>,
    /// The answer's first kilobyte, or what went wrong when there was no answer.
    excerpt: Option<String>,
    /// The peer's `Retry-After`, on the statuses that honour it.
    retry_after: Option<Duration>,
}

/// The outbound client of webhook deliveries, with its address guard.
#[derive(Clone)]
pub struct Sender {
    client: reqwest::Client,
    allow_private: bool,
}

impl Sender {
    /// Builds the client. `allow_private` admits private and loopback targets and plain `http`,
    /// for development only.
    ///
    /// # Errors
    ///
    /// The TLS backend cannot be initialised.
    pub fn new(allow_private: bool) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("Norbelys-Webhooks/", env!("CARGO_PKG_VERSION")))
            .dns_resolver(Guard { allow_private })
            .build()?;
        Ok(Self {
            client,
            allow_private,
        })
    }

    async fn attempt(
        &self,
        loaded: &Loaded,
        delivery: Id<WebhookDelivery>,
        secrets: &Secrets,
    ) -> Attempt {
        let started_at = crate::process::now();
        let clock = Instant::now();
        let elapsed =
            |clock: Instant| i32::try_from(clock.elapsed().as_millis()).unwrap_or(i32::MAX);
        let refused = |reason: Refused| Attempt {
            started_at,
            duration_ms: 0,
            status: None,
            excerpt: Some(format!("refused: {reason}")),
            retry_after: None,
        };
        let url = match reqwest::Url::parse(&loaded.url) {
            Ok(url) => url,
            Err(_) => return refused(Refused::Url),
        };
        if let Err(reason) = check_target(&url, self.allow_private) {
            return refused(reason);
        }
        let data = loaded.data.clone().unwrap_or(Value::Null);
        let body = match serde_json::to_vec(&Body {
            kind: &loaded.event_type,
            timestamp: loaded.created_at,
            data: &data,
        }) {
            Ok(body) => body,
            Err(_) => return refused(Refused::Url),
        };
        let webhook_id = delivery.to_string();
        let timestamp = started_at.0.as_second();
        let signature = secrets
            .signing_keys(started_at)
            .iter()
            .map(|key| crypto::sign_webhook(key, &webhook_id, timestamp, &body))
            .collect::<Vec<_>>()
            .join(" ");
        let sent = self
            .client
            .post(url)
            .headers(
                secrets
                    .headers
                    .iter()
                    .filter_map(|(name, value)| {
                        Some((
                            reqwest::header::HeaderName::from_bytes(name.as_bytes()).ok()?,
                            reqwest::header::HeaderValue::from_str(value).ok()?,
                        ))
                    })
                    .collect(),
            )
            .header("content-type", "application/json")
            .header("webhook-id", &webhook_id)
            .header("webhook-timestamp", timestamp.to_string())
            .header("webhook-signature", signature)
            .body(body)
            .send()
            .await;
        match sent {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let retry_after = matches!(status, 429 | 502 | 503 | 504)
                    .then(|| {
                        response
                            .headers()
                            .get("retry-after")
                            .and_then(|value| value.to_str().ok())
                            .and_then(parse_retry_after)
                    })
                    .flatten();
                let mut excerpt = Vec::new();
                while excerpt.len() < EXCERPT_BYTES {
                    match response.chunk().await {
                        Ok(Some(bytes)) => excerpt.extend_from_slice(&bytes),
                        Ok(None) | Err(_) => break,
                    }
                }
                excerpt.truncate(EXCERPT_BYTES);
                let excerpt = String::from_utf8_lossy(&excerpt).into_owned();
                Attempt {
                    started_at,
                    duration_ms: elapsed(clock),
                    status: Some(status),
                    excerpt: (!excerpt.is_empty()).then_some(excerpt),
                    retry_after,
                }
            }
            Err(error) => Attempt {
                started_at,
                duration_ms: elapsed(clock),
                status: None,
                excerpt: Some(describe(&error)),
                retry_after: None,
            },
        }
    }
}

/// A short, safe description of a request that got no answer.
fn describe(error: &reqwest::Error) -> String {
    let mut source = error.source();
    while let Some(cause) = source {
        if let Some(refused) = cause.downcast_ref::<Refused>() {
            return format!("refused: {refused}");
        }
        source = cause.source();
    }
    if error.is_timeout() {
        format!(
            "timeout: no answer within {} seconds",
            REQUEST_TIMEOUT.as_secs()
        )
    } else if error.is_connect() {
        "connect: the endpoint could not be reached".to_owned()
    } else {
        "request: the request failed before an answer".to_owned()
    }
}

/// `Retry-After` as seconds or as an HTTP date, bounded.
fn parse_retry_after(value: &str) -> Option<Duration> {
    let wait = match value.trim().parse::<u64>() {
        Ok(seconds) => Duration::from_secs(seconds),
        Err(_) => {
            let at = jiff::fmt::rfc2822::parse(value.trim()).ok()?.timestamp();
            Duration::try_from(at.duration_since(jiff::Timestamp::now())).unwrap_or(Duration::ZERO)
        }
    };
    Some(wait.clamp(RETRY_AFTER_MIN, RETRY_AFTER_MAX))
}

/// Why the guard refused a target.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub(crate) enum Refused {
    #[error("the endpoint's URL is not a valid absolute URL")]
    Url,
    #[error("the endpoint must use https")]
    Scheme,
    #[error("the endpoint's host is a private, loopback or reserved address")]
    Private,
    #[error("the endpoint's host has no address")]
    Unresolved,
}

/// Checks what the URL itself says: its scheme, and a literal address or a local name.
///
/// # Errors
///
/// The scheme is not `https` (or `http` with private targets allowed), or the host names an
/// inward address.
pub(crate) fn check_target(url: &reqwest::Url, allow_private: bool) -> Result<(), Refused> {
    match url.scheme() {
        "https" => {}
        "http" if allow_private => {}
        _ => return Err(Refused::Scheme),
    }
    let inward = match url.host() {
        None => return Err(Refused::Url),
        Some(url::Host::Ipv4(address)) => !is_public(IpAddr::V4(address)),
        Some(url::Host::Ipv6(address)) => !is_public(IpAddr::V6(address)),
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
    };
    if inward && !allow_private {
        Err(Refused::Private)
    } else {
        Ok(())
    }
}

/// The resolver of the webhook client: refuses a whole answer when any address is not public.
/// Every client that fetches a URL a customer typed (a webhook endpoint, an identity provider's
/// documents) resolves through it.
pub(crate) struct Guard {
    /// Admit private and loopback addresses: development only.
    pub(crate) allow_private: bool,
}

impl reqwest::dns::Resolve for Guard {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(resolve_public(name.as_str().to_owned(), self.allow_private))
    }
}

/// Resolves `host` and refuses the whole answer when any address is not public (unless
/// private targets are allowed), so a name cannot mix a public and an inward address.
async fn resolve_public(
    host: String,
    allow_private: bool,
) -> Result<reqwest::dns::Addrs, Box<dyn std::error::Error + Send + Sync>> {
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
    if addresses.is_empty() {
        return Err(Refused::Unresolved.into());
    }
    if !allow_private && addresses.iter().any(|address| !is_public(address.ip())) {
        return Err(Refused::Private.into());
    }
    Ok(Box::new(addresses.into_iter()))
}

/// True for a globally routable unicast address. IPv4-mapped, NAT64 (`64:ff9b::/96`) and 6to4
/// (`2002::/16`) IPv6 addresses are judged by the IPv4 address they carry. The special-purpose
/// blocks follow the IANA registries
/// (<https://www.iana.org/assignments/iana-ipv4-special-registry/>,
/// <https://www.iana.org/assignments/iana-ipv6-special-registry/>).
#[must_use]
pub fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => is_public_v6(address),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    let shared = a == 100 && (64..=127).contains(&b); // 100.64.0.0/10, carrier-grade NAT
    let protocol = a == 192 && b == 0 && c == 0; // 192.0.0.0/24, IETF protocol assignments
    let benchmarking = a == 198 && (b == 18 || b == 19); // 198.18.0.0/15
    let relay = a == 192 && b == 88 && c == 99; // 192.88.99.0/24, the former 6to4 relay anycast
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_multicast()
        || address.is_documentation()
        || a == 0
        || a >= 240
        || shared
        || protocol
        || benchmarking
        || relay)
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    if let Some(v4) = address.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let carried = |high: u16, low: u16| Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
    match address.segments() {
        [0x64, 0xff9b, 0, 0, 0, 0, high, low] => is_public_v4(carried(high, low)),
        [0x2002, high, low, ..] => is_public_v4(carried(high, low)),
        [0x2001, second, ..] if second < 0x200 => false, // 2001::/23, IETF protocol assignments
        [0x2001, 0xdb8, ..] => false,                    // documentation
        [0x3fff, second, ..] if second < 0x1000 => false, // 3fff::/20, documentation
        [first, ..] => (0x2000..=0x3fff).contains(&first), // global unicast, 2000::/3
    }
}

/// A new delivery id for an event: a UUIDv7 carrying the event's millisecond (see the module).
#[must_use]
pub fn new_delivery_id(event: Uuid) -> Uuid {
    let mut bytes = *Uuid::now_v7().as_bytes();
    for (slot, byte) in bytes.iter_mut().zip(event.as_bytes()).take(6) {
        *slot = *byte;
    }
    Uuid::from_bytes(bytes)
}

/// The unix millisecond a UUIDv7 carries in its first 48 bits.
#[must_use]
pub fn uuid_millis(id: Uuid) -> u64 {
    let [a, b, c, d, e, f, ..] = *id.as_bytes();
    u64::from_be_bytes([0, 0, a, b, c, d, e, f])
}

/// The smallest UUIDv7 of a millisecond, as the schema's `uuidv7_boundary()` computes it.
fn boundary(millis: u64) -> Uuid {
    let [_, _, a, b, c, d, e, f] = millis.to_be_bytes();
    Uuid::from_bytes([a, b, c, d, e, f, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 0])
}

/// The range of event ids a delivery's event lies in: its one millisecond.
#[must_use]
pub fn event_bounds(delivery: Uuid) -> (Uuid, Uuid) {
    let millis = uuid_millis(delivery);
    (boundary(millis), boundary(millis.saturating_add(1)))
}

/// What recording an attempt decided.
struct Recorded {
    attempts: i16,
    outcome: &'static str,
    /// The delivery stays pending; its next attempt is after this long.
    next: Option<Duration>,
}

/// Records one attempt on its delivery and its endpoint's health, inside the attempt's chunk.
/// The endpoint row is locked first (see the lock order in the module documentation).
async fn record_attempt(
    tx: &mut Tx,
    workspace: WorkspaceId,
    loaded: &Loaded,
    delivery: Id<WebhookDelivery>,
    attempt: &Attempt,
    draw: u64,
) -> Result<Recorded, sqlx::Error> {
    sqlx::query_scalar!(
        "SELECT 1 FROM webhook_endpoints WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        workspace.uuid(),
        loaded.endpoint_id,
    )
    .fetch_optional(&mut **tx)
    .await?;
    let attempts = loaded.attempt.saturating_add(1);
    let succeeded = attempt
        .status
        .is_some_and(|status| (200..300).contains(&status));
    let gone = attempt.status == Some(410);
    let (state, outcome, next) = if succeeded {
        ("delivered", "delivered", None)
    } else if gone {
        ("disabled", "disabled", None)
    } else {
        match retry::webhook_delay(u32::try_from(attempts).unwrap_or(u32::MAX), draw) {
            None => ("failed", "failed", None),
            Some(delay) => (
                "pending",
                "retrying",
                Some(attempt.retry_after.unwrap_or(delay)),
            ),
        }
    };
    let updated = sqlx::query_scalar!(
        "UPDATE webhook_deliveries SET attempt = attempt + 1, state = $4,
                next_attempt_at = CASE WHEN $4 = 'pending' THEN now() + make_interval(secs => $5) ELSE next_attempt_at END,
                response_status = $6, response_excerpt = $7, last_attempt_at = $8, last_duration_ms = $9,
                delivered_at = CASE WHEN $4 = 'delivered' THEN now() ELSE delivered_at END
          WHERE workspace_id = $1 AND event_id = $2 AND id = $3 AND state = 'pending'
         RETURNING attempt",
        workspace.uuid(),
        loaded.event_id,
        delivery.uuid(),
        state,
        next.unwrap_or_default().as_secs_f64(),
        attempt.status.and_then(|status| i16::try_from(status).ok()),
        attempt.excerpt,
        attempt.started_at as _,
        attempt.duration_ms,
    )
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        // Changed meanwhile (its endpoint was disabled, or it was reopened): nothing to schedule.
        return Ok(Recorded {
            attempts,
            outcome: "superseded",
            next: None,
        });
    }
    if succeeded {
        // A success ends the failing period, and with it the record that its admins were told.
        sqlx::query!(
            "UPDATE webhook_endpoints SET failing_since = NULL, failure_notified_at = NULL
              WHERE workspace_id = $1 AND id = $2 AND (failing_since IS NOT NULL OR failure_notified_at IS NOT NULL)",
            workspace.uuid(),
            loaded.endpoint_id,
        )
        .execute(&mut **tx)
        .await?;
    } else if gone {
        endpoints::disable(
            tx,
            workspace,
            loaded.endpoint_id,
            DisabledReason::Gone,
            Some("the endpoint answered 410 Gone"),
        )
        .await?;
    } else {
        let failing = sqlx::query_scalar!(
            r#"UPDATE webhook_endpoints SET failing_since = coalesce(failing_since, now())
                WHERE workspace_id = $1 AND id = $2
               RETURNING failing_since <= now() - make_interval(days => $3) AS "failing!""#,
            workspace.uuid(),
            loaded.endpoint_id,
            FAILING_LIMIT_DAYS,
        )
        .fetch_optional(&mut **tx)
        .await?;
        let disabled = if failing == Some(true) {
            let detail = attempt
                .excerpt
                .clone()
                .unwrap_or_else(|| "the endpoint kept failing".to_owned());
            endpoints::disable(
                tx,
                workspace,
                loaded.endpoint_id,
                DisabledReason::Failing,
                Some(&detail),
            )
            .await?
        } else {
            false
        };
        if disabled {
            tell_admins(tx, workspace, loaded.endpoint_id, true, attempt).await?;
        } else if state == "failed" {
            // The first delivery of a failing period to use up its retries tells the admins.
            let first = sqlx::query_scalar!(
                "UPDATE webhook_endpoints SET failure_notified_at = now()
                  WHERE workspace_id = $1 AND id = $2 AND failure_notified_at IS NULL RETURNING id",
                workspace.uuid(),
                loaded.endpoint_id,
            )
            .fetch_optional(&mut **tx)
            .await?;
            if first.is_some() {
                tell_admins(tx, workspace, loaded.endpoint_id, false, attempt).await?;
            }
        }
    }
    Ok(Recorded {
        attempts,
        outcome,
        next,
    })
}

/// Enqueues the email that tells the workspace's admins about `endpoint`: it keeps failing, or
/// it was `disabled` for it (see the module).
async fn tell_admins(
    tx: &mut Tx,
    workspace: WorkspaceId,
    endpoint: Uuid,
    disabled: bool,
    attempt: &Attempt,
) -> Result<(), sqlx::Error> {
    jobs::enqueue(
        tx,
        workspace,
        &FailureEmail {
            endpoint: Id::from_uuid(endpoint),
            disabled,
            last_error: last_error(attempt),
        },
        None,
    )
    .await?;
    Ok(())
}

/// What an attempt met, for a person: its status and the start of the answer, or what went
/// wrong when nothing was answered.
fn last_error(attempt: &Attempt) -> String {
    let excerpt: Option<String> = attempt
        .excerpt
        .as_deref()
        .map(|text| text.chars().take(200).collect());
    match (attempt.status, excerpt) {
        (Some(status), Some(excerpt)) => format!("HTTP {status}: {excerpt}"),
        (Some(status), None) => format!("HTTP {status}"),
        (None, Some(excerpt)) => excerpt,
        (None, None) => "no answer".to_owned(),
    }
}

/// `webhook_endpoint.failure_email`: tells the workspace's admins that one of its webhook
/// endpoints keeps failing, or was disabled for it (see the module).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEmail {
    /// The endpoint.
    pub endpoint: Id<WebhookEndpoint>,
    /// Whether it was disabled (the second email) rather than still failing (the first).
    pub disabled: bool,
    /// What its last attempt met.
    pub last_error: String,
}

impl Job for FailureEmail {
    const KIND: &'static str = "webhook_endpoint.failure_email";
    const QUEUE: Queue = Queue::Transactional;
    const EFFECT: Effect = Effect::Idempotent;

    fn unique_key(&self) -> Option<String> {
        let notice = if self.disabled { "disabled" } else { "failing" };
        Some(format!("{}:{notice}", self.endpoint))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        // A run whose chunk committed has sent everything: a recovered lease ends here.
        if cx.progress().is_some() {
            return Ok(Outcome::Done);
        }
        let keys = cx.env::<Keys>()?.clone();
        let workspace = cx.workspace();
        let mut chunk = cx.begin().await?;
        let sent = self.send(chunk.tx(), &keys, workspace).await?;
        cx.checkpoint(chunk, json!({ "sent": sent })).await?;
        if sent > 0 {
            accept::wake(cx.db()).await;
        }
        Ok(Outcome::Done)
    }
}

impl FailureEmail {
    /// Accepts one email per admin of `workspace` inside `tx` while the news still holds (the
    /// endpoint still fails, or is still disabled), and answers how many.
    async fn send(
        &self,
        tx: &mut Tx,
        keys: &Keys,
        workspace: WorkspaceId,
    ) -> Result<usize, JobError> {
        let Some(endpoint) = sqlx::query!(
            r#"SELECT e.url, e.enabled, e.failing_since AS "failing_since: Timestamp", w.name AS workspace_name,
                      now() + interval '1 day' AS "expires_at!: Timestamp"
                 FROM webhook_endpoints e
                 JOIN workspaces w ON w.id = e.workspace_id
                WHERE e.workspace_id = $1 AND e.id = $2"#,
            workspace.uuid(),
            self.endpoint.uuid(),
        )
        .fetch_optional(&mut **tx)
        .await?
        else {
            return Ok(0);
        };
        let Some(failing_since) = endpoint.failing_since else {
            return Ok(0);
        };
        if endpoint.enabled == self.disabled {
            // Enabled again since it was disabled, or disabled since it was failing (that email
            // is on its way): this one is no longer news.
            return Ok(0);
        }
        let mut sent = 0;
        for admin in memberships::told(tx, workspace, &[]).await? {
            let Ok(to) = EmailAddress::parse(&admin.email) else {
                continue;
            };
            accept::transactional(
                tx,
                keys,
                &Transactional::WebhookFailure {
                    to: &to,
                    workspace_name: &endpoint.workspace_name,
                    url: &endpoint.url,
                    failing_since,
                    disabled: self.disabled,
                    last_error: &self.last_error,
                    expires_at: endpoint.expires_at,
                },
            )
            .await
            .map_err(|error| match error {
                accept::Error::Db(error) => JobError::Db(error),
                other => JobError::Failed(other.to_string()),
            })?;
            sent += 1;
        }
        Ok(sent)
    }
}

/// Where a webhook delivery is (`webhook_deliveries.state`): `pending` while an attempt is due
/// at `next_attempt_at`, `delivered`, `failed` once the schedule is exhausted, or `disabled`
/// when its endpoint was disabled.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumIter,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[schema(as = WebhookDeliveryState)]
pub enum DeliveryState {
    Pending,
    Delivered,
    Failed,
    Disabled,
}

impl DeliveryState {
    /// The state as stored and shown.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// A delivery as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DeliveryObject {
    pub id: Id<WebhookDelivery>,
    pub event_id: Id<OutboxEvent>,
    /// The event's type.
    #[schema(value_type = EventType)]
    pub event_type: String,
    pub webhook_endpoint_id: Id<WebhookEndpoint>,
    /// Where the delivery is. New values may be added.
    #[schema(value_type = DeliveryState)]
    pub state: String,
    /// Attempts made so far.
    pub attempts: i16,
    /// The latest attempt, failures included.
    pub last_attempt: Option<LastAttempt>,
    /// When the next attempt is due, while `pending`.
    pub next_attempt_at: Option<Timestamp>,
    pub delivered_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

/// The latest attempt of a delivery.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct LastAttempt {
    pub started_at: Timestamp,
    pub duration_ms: Option<i32>,
    /// The answer's status; null when nothing was answered.
    pub response_status: Option<i16>,
    /// The answer's first kilobyte, or what went wrong when nothing was answered.
    pub response_excerpt: Option<String>,
}

struct DeliveryRow {
    id: Id<WebhookDelivery>,
    event_id: Id<OutboxEvent>,
    event_type: String,
    endpoint_id: Id<WebhookEndpoint>,
    state: String,
    attempt: i16,
    last_attempt_at: Option<Timestamp>,
    last_duration_ms: Option<i32>,
    response_status: Option<i16>,
    response_excerpt: Option<String>,
    next_attempt_at: Timestamp,
    delivered_at: Option<Timestamp>,
    created_at: Timestamp,
}

impl From<DeliveryRow> for DeliveryObject {
    fn from(row: DeliveryRow) -> Self {
        Self {
            id: row.id,
            event_id: row.event_id,
            event_type: row.event_type,
            webhook_endpoint_id: row.endpoint_id,
            next_attempt_at: (row.state == "pending").then_some(row.next_attempt_at),
            state: row.state,
            attempts: row.attempt,
            last_attempt: row.last_attempt_at.map(|started_at| LastAttempt {
                started_at,
                duration_ms: row.last_duration_ms,
                response_status: row.response_status,
                response_excerpt: row.response_excerpt,
            }),
            delivered_at: row.delivered_at,
            created_at: row.created_at,
        }
    }
}

/// Reads one delivery of `workspace` by its id alone, in its event's partition.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<WebhookDelivery>,
) -> Result<Option<DeliveryObject>, sqlx::Error> {
    let (low, high) = event_bounds(id.uuid());
    let row = sqlx::query_as!(
        DeliveryRow,
        r#"SELECT d.id AS "id: Id<WebhookDelivery>", d.event_id AS "event_id: Id<OutboxEvent>", ev.type AS event_type,
                  d.endpoint_id AS "endpoint_id: Id<WebhookEndpoint>", d.state, d.attempt,
                  d.last_attempt_at AS "last_attempt_at: Timestamp", d.last_duration_ms, d.response_status, d.response_excerpt,
                  d.next_attempt_at AS "next_attempt_at: Timestamp", d.delivered_at AS "delivered_at: Timestamp",
                  d.created_at AS "created_at: Timestamp"
             FROM webhook_deliveries d
             JOIN outbox_events ev ON ev.workspace_id = d.workspace_id AND ev.id = d.event_id
            WHERE d.workspace_id = $1 AND d.id = $2 AND d.event_id >= $3 AND d.event_id < $4"#,
        workspace.uuid(),
        id.uuid(),
        low,
        high,
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(DeliveryObject::from))
}

/// The filters of the delivery list.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeliveryFilters {
    pub endpoint: Option<Uuid>,
    pub event: Option<Uuid>,
    pub state: Option<String>,
}

/// One page of `workspace`'s deliveries in the table's key order (by event, then by delivery),
/// after the `(event, delivery)` position `after` when given.
///
/// # Errors
///
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &DeliveryFilters,
    after: Option<(Uuid, Uuid)>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<DeliveryObject>, sqlx::Error> {
    let (after_event, after_id) = after.unzip();
    let rows = if ascending {
        sqlx::query_as!(
            DeliveryRow,
            r#"SELECT d.id AS "id: Id<WebhookDelivery>", d.event_id AS "event_id: Id<OutboxEvent>", ev.type AS event_type,
                      d.endpoint_id AS "endpoint_id: Id<WebhookEndpoint>", d.state, d.attempt,
                      d.last_attempt_at AS "last_attempt_at: Timestamp", d.last_duration_ms, d.response_status, d.response_excerpt,
                      d.next_attempt_at AS "next_attempt_at: Timestamp", d.delivered_at AS "delivered_at: Timestamp",
                      d.created_at AS "created_at: Timestamp"
                 FROM webhook_deliveries d
                 JOIN outbox_events ev ON ev.workspace_id = d.workspace_id AND ev.id = d.event_id
                WHERE d.workspace_id = $1 AND ($2::uuid IS NULL OR d.endpoint_id = $2) AND ($3::uuid IS NULL OR d.event_id = $3)
                  AND ($4::text IS NULL OR d.state = $4) AND ($5::uuid IS NULL OR (d.event_id, d.id) > ($5, $6))
                ORDER BY d.event_id, d.id LIMIT $7"#,
            workspace.uuid(),
            filters.endpoint,
            filters.event,
            filters.state,
            after_event,
            after_id,
            limit,
        )
        .fetch_all(&mut **tx)
        .await?
    } else {
        sqlx::query_as!(
            DeliveryRow,
            r#"SELECT d.id AS "id: Id<WebhookDelivery>", d.event_id AS "event_id: Id<OutboxEvent>", ev.type AS event_type,
                      d.endpoint_id AS "endpoint_id: Id<WebhookEndpoint>", d.state, d.attempt,
                      d.last_attempt_at AS "last_attempt_at: Timestamp", d.last_duration_ms, d.response_status, d.response_excerpt,
                      d.next_attempt_at AS "next_attempt_at: Timestamp", d.delivered_at AS "delivered_at: Timestamp",
                      d.created_at AS "created_at: Timestamp"
                 FROM webhook_deliveries d
                 JOIN outbox_events ev ON ev.workspace_id = d.workspace_id AND ev.id = d.event_id
                WHERE d.workspace_id = $1 AND ($2::uuid IS NULL OR d.endpoint_id = $2) AND ($3::uuid IS NULL OR d.event_id = $3)
                  AND ($4::text IS NULL OR d.state = $4) AND ($5::uuid IS NULL OR (d.event_id, d.id) < ($5, $6))
                ORDER BY d.event_id DESC, d.id DESC LIMIT $7"#,
            workspace.uuid(),
            filters.endpoint,
            filters.event,
            filters.state,
            after_event,
            after_id,
            limit,
        )
        .fetch_all(&mut **tx)
        .await?
    };
    Ok(rows.into_iter().map(DeliveryObject::from).collect())
}

/// Counts `workspace`'s deliveries matching `filters`, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &DeliveryFilters,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM webhook_deliveries d
                WHERE d.workspace_id = $1 AND ($2::uuid IS NULL OR d.endpoint_id = $2) AND ($3::uuid IS NULL OR d.event_id = $3)
                  AND ($4::text IS NULL OR d.state = $4)
                LIMIT $5) counted"#,
        workspace.uuid(),
        filters.endpoint,
        filters.event,
        filters.state,
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// What to reopen: one delivery (a manual retry), or an endpoint's deliveries for events at or
/// after an instant (a replay).
#[derive(Debug, Clone, Copy)]
pub enum Reopen {
    Delivery(Id<WebhookDelivery>),
    Endpoint {
        endpoint: Id<WebhookEndpoint>,
        since: Timestamp,
    },
}

/// Why a reopening was refused.
#[derive(Debug, thiserror::Error)]
pub enum ReopenError {
    /// The events are older than the replay window: their partitions may be being archived.
    #[error("the events are older than the replay window")]
    TooOld,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Reopens deliveries: each becomes `pending` with its next attempt now, and gets a
/// `webhook.deliver` job (a waiting one is brought forward; there is never a second run beside
/// the automatic one). Returns the reopened deliveries.
///
/// Manual retries and replays share this one statement, whose condition carries the replay
/// window: events older than the outbox's online retention minus one partition period cannot
/// be reopened. The statement takes the shared advisory lock that every operation relying on a
/// retention value takes (a change of the retention takes it exclusively), so no path can
/// reopen a row in a partition the archive is examining.
///
/// # Errors
///
/// [`ReopenError::TooOld`], or the database refused.
pub async fn reopen(
    tx: &mut Tx,
    workspace: WorkspaceId,
    which: Reopen,
) -> Result<Vec<Id<WebhookDelivery>>, ReopenError> {
    sqlx::query!(
        "SELECT pg_advisory_xact_lock_shared('partition_policies'::regclass::oid::bigint)"
    )
    .execute(&mut **tx)
    .await?;
    let window = sqlx::query_scalar!(
        r#"SELECT uuidv7_boundary(now() - (retention - period)) AS "floor!" FROM partition_policies WHERE table_name = 'outbox_events'"#
    )
    .fetch_one(&mut **tx)
    .await?;
    let (delivery, endpoint, low, high) = match which {
        Reopen::Delivery(id) => {
            let (low, high) = event_bounds(id.uuid());
            (Some(id.uuid()), None, low, high)
        }
        Reopen::Endpoint { endpoint, since } => {
            let since = boundary(u64::try_from(since.0.as_millisecond()).unwrap_or_default());
            (None, Some(endpoint.uuid()), since, Uuid::max())
        }
    };
    if low < window {
        return Err(ReopenError::TooOld);
    }
    let reopened = sqlx::query_scalar!(
        r#"UPDATE webhook_deliveries SET state = 'pending', next_attempt_at = now()
            WHERE workspace_id = $1 AND event_id >= $2 AND event_id >= $3 AND event_id < $4
              AND ($5::uuid IS NULL OR id = $5) AND ($6::uuid IS NULL OR endpoint_id = $6)
           RETURNING id AS "id: Id<WebhookDelivery>""#,
        workspace.uuid(),
        window,
        low,
        high,
        delivery,
        endpoint,
    )
    .fetch_all(&mut **tx)
    .await?;
    let jobs: Vec<Deliver> = reopened
        .iter()
        .map(|id| Deliver { delivery: *id })
        .collect();
    jobs::enqueue_many(tx, workspace, &jobs, None).await?;
    Ok(reopened)
}

/// The base64 secret bytes behind a `whsec_` secret.
pub(crate) fn secret_bytes(secret: &str) -> Option<Vec<u8>> {
    STANDARD.decode(secret.strip_prefix("whsec_")?).ok()
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::time::Duration;

    use uuid::Uuid;

    use super::{
        Refused, check_target, event_bounds, is_public, new_delivery_id, parse_retry_after,
        secret_bytes, uuid_millis,
    };
    use crate::crypto;

    /// Our signature equals the reference vector of the Standard Webhooks specification
    /// (secret, message id, timestamp and payload of its reference libraries), so any conforming
    /// consumer library verifies our deliveries.
    #[test]
    fn signatures_match_the_standard_webhooks_reference_vector() {
        let key = secret_bytes("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
        assert_eq!(
            crypto::sign_webhook(
                &key,
                "msg_p5jXN8AQM9LWM0D4loKWxJek",
                1_614_265_330,
                br#"{"test": 2432232314}"#
            ),
            "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="
        );
    }

    /// Only globally routable unicast addresses count as public: private, loopback, link-local,
    /// carrier-grade NAT, documentation, benchmarking, multicast, broadcast and reserved blocks do
    /// not, and the IPv6 forms that carry an IPv4 address (mapped, NAT64, 6to4) are judged by it.
    #[test]
    fn only_global_unicast_addresses_are_public() {
        for (address, public) in [
            ("8.8.8.8", true),
            ("1.1.1.1", true),
            ("10.0.0.1", false),
            ("172.16.5.4", false),
            ("192.168.1.1", false),
            ("127.0.0.1", false),
            ("169.254.169.254", false),
            ("100.64.0.1", false),
            ("192.0.0.8", false),
            ("192.0.2.1", false),
            ("198.18.0.1", false),
            ("203.0.113.9", false),
            ("224.0.0.1", false),
            ("255.255.255.255", false),
            ("0.0.0.0", false),
            ("240.0.0.1", false),
            ("192.88.99.1", false),
            ("2606:4700:4700::1111", true),
            ("::ffff:8.8.8.8", true),
            ("::1", false),
            ("::", false),
            ("fe80::1", false),
            ("fc00::1", false),
            ("ff02::1", false),
            ("2001:db8::1", false),
            ("2001::1", false),
            ("::ffff:10.0.0.1", false),
            ("64:ff9b::a00:1", false),
            ("2002:a00:1::1", false),
        ] {
            assert_eq!(
                is_public(address.parse::<IpAddr>().unwrap()),
                public,
                "{address}"
            );
        }
    }

    /// The guard refuses what a URL itself says is inward (plain `http`, a loopback, private or
    /// local host, written as an address or as a `localhost` name) unless private targets are
    /// allowed for development, and never admits another scheme.
    #[test]
    fn the_guard_refuses_inward_targets_unless_allowed() {
        let check = |url: &str, allow_private| {
            check_target(&reqwest::Url::parse(url).unwrap(), allow_private)
        };
        assert!(check("https://hooks.example.com/norbelys", false).is_ok());
        assert!(matches!(
            check("http://hooks.example.com/norbelys", false),
            Err(Refused::Scheme)
        ));
        for inward in [
            "https://127.0.0.1/hooks",
            "https://[::1]/hooks",
            "https://10.1.2.3/hooks",
            "https://localhost/hooks",
            "https://api.localhost./hooks",
        ] {
            assert!(
                matches!(check(inward, false), Err(Refused::Private)),
                "{inward}"
            );
            assert!(
                check(inward, true).is_ok(),
                "{inward} with private targets allowed"
            );
        }
        assert!(check("http://127.0.0.1:9911/hooks", true).is_ok());
        assert!(matches!(
            check("ftp://hooks.example.com/x", true),
            Err(Refused::Scheme)
        ));
    }

    /// A delivery id carries its event's millisecond, so the bounds computed from the delivery id
    /// alone always contain its event's id (one partition, a few index entries), while the rest
    /// of the id is fresh: two deliveries of one event never share an id.
    #[test]
    fn a_delivery_id_locates_its_event() {
        for _ in 0..1_000 {
            let event = Uuid::now_v7();
            let delivery = new_delivery_id(event);
            assert_eq!(uuid_millis(delivery), uuid_millis(event));
            assert_eq!(delivery.get_version_num(), 7);
            let (low, high) = event_bounds(delivery);
            assert!(low <= event && event < high);
            assert_ne!(new_delivery_id(event), delivery);
        }
    }

    /// `Retry-After` is read as seconds or as an HTTP date and kept within the schedule's range
    /// (5 seconds to 24 hours), so a peer can neither make us hammer it nor park a delivery
    /// forever; any other text is ignored and the schedule applies.
    #[test]
    fn retry_after_is_read_and_bounded() {
        assert_eq!(parse_retry_after("7"), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after(" 0 "), Some(Duration::from_secs(5)));
        assert_eq!(
            parse_retry_after("999999999"),
            Some(Duration::from_secs(24 * 3_600))
        );
        let in_a_minute = jiff::Timestamp::now()
            .checked_add(jiff::SignedDuration::from_secs(60))
            .unwrap();
        let http_date = jiff::fmt::rfc2822::DateTimePrinter::new()
            .timestamp_to_rfc9110_string(&in_a_minute)
            .unwrap();
        let wait = parse_retry_after(&http_date).unwrap();
        assert!(
            wait > Duration::from_secs(50) && wait <= Duration::from_secs(60),
            "{http_date}: {wait:?}"
        );
        assert_eq!(parse_retry_after("soon"), None);
    }
}
