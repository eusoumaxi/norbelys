//! Telemetry decisions: the canonical events, which request events and which spans are kept, how
//! PostgreSQL's tables are grouped in the database metrics, how many partition leaves a table is
//! expected to hold ([`expected_leaves`]), and the per-address count of denied authentications
//! that leaves a process as one number ([`DenialWindow`]).
//!
//! # Canonical events
//!
//! A canonical event is one wide log event per unit of work (a request, a claim, a wave, a poll,
//! a job run, a flush), emitted when the unit ends, with every field an operator asks about.
//! [`Event`] is their closed list: the `event` field of each carries its name, and the coverage
//! counters (`norbelys_telemetry_events_total`, `norbelys_telemetry_metric_events_total`) are
//! labelled by it. A name outside the list is never counted, so those labels stay bounded.
//!
//! # Which request events are kept
//!
//! A request answered `2xx` in under a second on a hot route (accepting a message, reading a
//! list) is recorded for 5 % of requests, chosen by a hash of its request id, so a request id is
//! kept or dropped alike wherever it is decided. Every other request is always recorded: an error
//! or a refusal (`4xx`, `5xx`), a slow answer, anything else, and everything on the access routes
//! (sign-in, sessions, the current user, workspaces with their keys and members, the OAuth
//! server), whose rare requests are the ones an investigation needs. A health probe answered
//! `2xx` is never recorded: the orchestrator asks every few seconds on every replica, so its
//! successes would spend the log budget on nothing, while its failures (`5xx`, the replica cannot
//! reach the database) are recorded like any error and every probe is counted in the request
//! metrics. Batch events are always recorded (they are already one per batch, never one per
//! message), and metrics are never sampled: the counts stay exact whatever the logs keep.
//!
//! # Which spans are kept
//!
//! Every role uses OpenTelemetry's trace-id ratio sampler, configurable with
//! `NORBELYS_TRACE_SAMPLE_PERCENT` (100 at launch). All spans with the same trace id
//! make the same decision; per-message children are retained with their wave.
//! Metrics are never sampled and error events are retained independently of traces.
//!
//! # Table classes
//!
//! The database metrics group tables in four classes instead of naming tables, which would make
//! the labels grow with the schema: `queue` (rows inserted, leased and deleted all day: dead tuples
//! and vacuum matter most), `counters` (rows updated in place by the rollups and the budgets),
//! `facts` (every other table of the product, mostly appended, partitioned by time) and `system`
//! (PostgreSQL's own catalogs). A partition leaf belongs to the class of its partitioned table.

use std::time::Duration;

/// The canonical events (see the module).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
)]
pub enum Event {
    /// One HTTP request, by the api's middleware.
    #[strum(serialize = "http.request")]
    HttpRequest,
    /// One claim transaction of the sender, for one connection.
    #[strum(serialize = "delivery.claim")]
    DeliveryClaim,
    /// One submission wave of the sender: a claim's messages, submitted.
    #[strum(serialize = "delivery.wave")]
    DeliveryWave,
    /// One finish micro-batch of a wave's reports.
    #[strum(serialize = "delivery.settle")]
    DeliverySettle,
    /// One recovery sweep of expired delivery leases.
    #[strum(serialize = "delivery.recover")]
    DeliveryRecover,
    /// One message's final failure (`failed`, expired included, or `uncertain`), at error level,
    /// once its ending has committed; a transient attempt is never one (its wave's event counts
    /// it), so a provider hiccup that heals pages nobody.
    #[strum(serialize = "delivery.failure")]
    DeliveryFailure,
    /// One poll of a receive binding.
    #[strum(serialize = "inbox.poll")]
    InboxPoll,
    /// One job run, from its claim to its yield or conclusion.
    #[strum(serialize = "job.run")]
    JobRun,
    /// One micro-batch of provider callbacks stored by the ingress.
    #[strum(serialize = "receipts.flush")]
    ReceiptsFlush,
    /// One chunk of receipts normalised into delivery events.
    #[strum(serialize = "receipts.normalize")]
    ReceiptsNormalize,
    /// One attempt to deliver a customer webhook.
    #[strum(serialize = "webhook.delivery")]
    WebhookDelivery,
    /// One drain batch of tracking events.
    #[strum(serialize = "tracking.drain")]
    TrackingDrain,
    /// One run of the analytics rollup.
    #[strum(serialize = "analytics.rollup")]
    AnalyticsRollup,
    /// One partition leaf archived and dropped, or held back by its gate.
    #[strum(serialize = "archive.export")]
    ArchiveExport,
    /// One authentication or authorization decision that denied, or one sign-in.
    #[strum(serialize = "auth.decision")]
    AuthDecision,
    /// One call to an AI provider.
    #[strum(serialize = "ai.call")]
    AiCall,
}

impl Event {
    /// The event's name, the value of its `event` field (`job.run`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The precision of the sampling shares: one part in ten thousand.
const SCALE: u64 = 10_000;
/// The share of hot-route success logs kept, in [`SCALE`] parts: 5 %.
pub const HEAD_SHARE: u64 = 500;
/// A request at least this slow is always recorded.
pub const SLOW: Duration = Duration::from_secs(1);
/// The route prefixes of the access routes, always recorded (see the module).
const ACCESS: [&str; 4] = ["/v1/auth", "/v1/me", "/v1/workspaces", "/oauth"];
/// The health probes' routes, whose successes are never recorded (see the module).
const PROBES: [&str; 2] = ["/health/live", "/health/ready"];
/// Whether the canonical event of a request is recorded: `method` and `route` (the matched route
/// template, `/v1/messages/{id}`), the answer's `status`, how long it took and its request id
/// (see the module).
#[must_use]
pub fn keep_request(
    method: &str,
    route: &str,
    status: u16,
    elapsed: Duration,
    request_id: &str,
) -> bool {
    let success = (200..300).contains(&status);
    if success && is_probe(route) {
        return false;
    }
    !success || elapsed >= SLOW || !hot(method, route) || bucket(request_id.as_bytes()) < HEAD_SHARE
}

/// Whether `route` is a health probe's (see the module).
#[must_use]
pub fn is_probe(route: &str) -> bool {
    PROBES.contains(&route)
}

/// Whether `method` on `route` is a hot route: accepting a message, or reading a list (a `GET` of
/// a `/v1` collection, whose template does not end with an id), outside the access routes.
#[must_use]
pub fn hot(method: &str, route: &str) -> bool {
    if is_access(route) {
        return false;
    }
    match method {
        "POST" => route == "/v1/messages",
        "GET" => route.starts_with("/v1/") && !route.ends_with('}'),
        _ => false,
    }
}

/// Whether `route` is one of the access routes, or under one.
#[must_use]
pub fn is_access(route: &str) -> bool {
    ACCESS.iter().any(|prefix| {
        route
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// FNV-1a (64 bits) of `bytes`, reduced to `0..10_000`: a stable, uniform enough bucket for ids
/// that are already random (UUIDs).
#[must_use]
pub fn bucket(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash % SCALE
}

/// The class of a table in the database metrics (see the module).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, strum::EnumIter, strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
pub enum TableClass {
    /// Appended facts and the product's other tables.
    Facts,
    /// Rows inserted, leased and deleted all day.
    Queue,
    /// Rows updated in place by the rollups and the budgets.
    Counters,
    /// PostgreSQL's own catalogs.
    System,
}

impl TableClass {
    /// The label value (`queue`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The queue tables.
const QUEUE_TABLES: [&str; 9] = [
    "delivery_queue",
    "dispatch_workspaces",
    "idempotency_keys",
    "job_lanes",
    "jobs",
    "outbox_events",
    "receive_bindings",
    "webhook_deliveries",
    "webhook_receipts",
];
/// The counter tables.
const COUNTER_TABLES: [&str; 7] = [
    "ai_usage",
    "campaign_daily_stats",
    "connection_usage",
    "message_engagement",
    "quota_scope_usage",
    "rollup_watermarks",
    "stats_increments",
];

/// The class of `table` in `schema`; a partition leaf is classified by its partitioned table's
/// name.
#[must_use]
pub fn table_class(schema: &str, table: &str) -> TableClass {
    if schema != "public" {
        TableClass::System
    } else if QUEUE_TABLES.contains(&table) {
        TableClass::Queue
    } else if COUNTER_TABLES.contains(&table) {
        TableClass::Counters
    } else {
        TableClass::Facts
    }
}

/// How far ahead of now the daily maintenance job keeps partition leaves: it calls
/// `ensure_partitions_ahead(interval '2 days')` (`partitions.create`), which also always makes
/// the leaf after the current one. The two must change together.
const LEAVES_AHEAD: Duration = Duration::from_secs(2 * 86_400);

/// The partition leaves a table partitioned by `period` and kept online for `retention` is
/// expected to hold: the periods of its online window (rounded up to whole periods), the current
/// period's leaf, the leaves made ahead (at least the next one), and one leaf past its retention
/// still waiting for the next daily archive run. `archive-debt` alerts on more than twice this:
/// leaves that do not leave (an archive gate that keeps failing, an archive that does not run) or
/// that are made too far ahead, and every leaf costs query planning time.
#[must_use]
pub fn expected_leaves(retention: Duration, period: Duration) -> u64 {
    let period = period.as_secs().max(1);
    let online = retention.as_secs().div_ceil(period);
    let ahead = LEAVES_AHEAD.as_secs().div_ceil(period).max(1);
    online.saturating_add(ahead).saturating_add(2)
}

/// Width and depth of a Count-Min sketch: fixed memory regardless of unique addresses.
const DENIAL_KEYS: usize = 16_384;
const DENIAL_ROWS: usize = 4;

/// An upper estimate of the busiest denied address over the current and previous minute.
///
/// Four independently seeded hash rows count every denial, including addresses first seen
/// after a large spray. Collisions can overestimate a frequency, but never hide a later
/// offender. Only the maximum estimate leaves the process; address hashes are never labels.
/// The sketch uses 512 KiB, resets on minute boundaries, and stores no address values.
#[derive(Debug)]
pub struct DenialWindow {
    minute: u64,
    counts: Vec<u64>,
    current: u64,
    previous: u64,
}

impl Default for DenialWindow {
    fn default() -> Self {
        Self {
            minute: 0,
            counts: vec![0; DENIAL_KEYS * DENIAL_ROWS],
            current: 0,
            previous: 0,
        }
    }
}

impl DenialWindow {
    /// Counts one denial of `key` at `minute` in bounded time and memory.
    pub fn deny(&mut self, key: &[u8], minute: u64) {
        use std::hash::{Hash as _, Hasher as _};
        self.roll(minute);
        let mut estimate = u64::MAX;
        for row in 0..DENIAL_ROWS {
            let mut hash = std::hash::DefaultHasher::new();
            row.hash(&mut hash);
            key.hash(&mut hash);
            let keys = u64::try_from(DENIAL_KEYS).unwrap_or(u64::MAX);
            let bucket = usize::try_from(hash.finish() % keys).unwrap_or(0);
            let Some(count) = self.counts.get_mut(row * DENIAL_KEYS + bucket) else {
                return;
            };
            *count = count.saturating_add(1);
            estimate = estimate.min(*count);
        }
        self.current = self.current.max(estimate);
    }

    /// The largest upper estimate in the current or previous whole minute.
    pub fn top(&mut self, minute: u64) -> u64 {
        self.roll(minute);
        self.current.max(self.previous)
    }

    fn roll(&mut self, minute: u64) {
        if minute <= self.minute {
            return;
        }
        self.previous = if minute == self.minute.saturating_add(1) {
            self.current
        } else {
            0
        };
        self.minute = minute;
        self.current = 0;
        self.counts.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::str::FromStr as _;

    use strum::IntoEnumIterator as _;

    use super::*;

    /// Every canonical event's name parses back to the event, names are unique, and a name
    /// outside the list does not parse: the coverage counters can only ever carry these labels,
    /// and an emitter whose name is misspelt is counted on no side, so the reconciliation shows it.
    #[test]
    fn every_event_name_round_trips_and_nothing_else_parses() {
        let mut names = BTreeSet::new();
        for event in Event::iter() {
            assert_eq!(Event::from_str(event.as_str()), Ok(event), "{event:?}");
            assert!(
                names.insert(event.as_str()),
                "{} is used twice",
                event.as_str()
            );
            assert!(
                event.as_str().contains('.'),
                "{} is `area.unit`",
                event.as_str()
            );
        }
        for unknown in ["", "delivery.submission", "job", "http.requests", "JOB.RUN"] {
            assert!(
                Event::from_str(unknown).is_err(),
                "{unknown} must not parse"
            );
        }
    }

    /// The decision for each kind of request: errors, refusals, slow answers, cold routes and
    /// the access routes are always recorded; only a fast success on a hot route is left to the
    /// 5 % draw. Losing an error's event would make it uninvestigable, which is why only
    /// successes are ever sampled.
    #[test]
    fn only_fast_successes_on_hot_routes_are_sampled() {
        let fast = Duration::from_millis(20);
        let slow = Duration::from_millis(1_500);
        // Statuses other than 2xx, whatever the route.
        for status in [100, 301, 304, 400, 401, 404, 409, 422, 429, 500, 503] {
            for request_id in ["a", "b", "c", "d"] {
                assert!(keep_request(
                    "POST",
                    "/v1/messages",
                    status,
                    fast,
                    request_id
                ));
            }
        }
        // Slow successes on hot routes.
        assert!(keep_request("GET", "/v1/people", 200, slow, "any"));
        // Cold routes and the access routes.
        for (method, route) in [
            ("GET", "/v1/messages/{id}"),
            ("PATCH", "/v1/people/{id}"),
            ("POST", "/v1/campaigns"),
            ("DELETE", "/v1/segments/{id}"),
            ("GET", "/v1/workspaces/{id}/api_keys"),
            ("GET", "/v1/me/memberships"),
            ("POST", "/v1/auth/sessions"),
            ("GET", "/oauth/authorize"),
            ("GET", "unmatched"),
        ] {
            assert!(!hot(method, route), "{method} {route} is not hot");
            for request_id in ["a", "b", "c", "d"] {
                assert!(keep_request(method, route, 200, fast, request_id));
            }
        }
        // The hot routes.
        for (method, route) in [
            ("POST", "/v1/messages"),
            ("GET", "/v1/messages"),
            ("GET", "/v1/people"),
            ("GET", "/v1/campaigns/{id}/messages"),
        ] {
            assert!(hot(method, route), "{method} {route} is hot");
        }
        // The draw is the request id's: the same id, the same answer.
        let kept = keep_request("GET", "/v1/people", 200, fast, "req-42");
        assert_eq!(keep_request("GET", "/v1/people", 204, fast, "req-42"), kept);
    }

    /// A health probe's success is never recorded, whatever its request id or how long it took,
    /// while its failure always is: the probes arrive every few seconds on every replica, and only
    /// a replica that cannot reach its database has something to say.
    #[test]
    fn probe_successes_are_never_recorded_and_failures_always() {
        let slow = Duration::from_millis(1_500);
        for route in PROBES {
            assert!(is_probe(route), "{route}");
            for request_id in ["a", "b", "c", "d", "kept-0"] {
                for status in [200, 204] {
                    for elapsed in [Duration::from_millis(1), slow] {
                        assert!(!keep_request("GET", route, status, elapsed, request_id));
                    }
                }
                assert!(keep_request(
                    "GET",
                    route,
                    503,
                    Duration::from_millis(1),
                    request_id
                ));
            }
        }
        assert!(!is_probe("/health"));
        assert!(!is_probe("/v1/health/ready"));
    }

    /// The share kept of hot successes is 5 %, within a point over a hundred thousand request
    /// ids: what the log budget assumes, neither every request nor none.
    #[test]
    fn the_hot_share_is_five_percent() {
        let fast = Duration::from_millis(5);
        let kept = (0..100_000)
            .filter(|i| {
                keep_request(
                    "POST",
                    "/v1/messages",
                    202,
                    fast,
                    &format!("{i:08x}-request"),
                )
            })
            .count();
        assert!((4_000..=6_000).contains(&kept), "kept {kept} of 100000");
    }

    /// The access prefixes match whole segments only: `/v1/meters` would not be an access route.
    #[test]
    fn access_prefixes_match_whole_segments() {
        assert!(is_access("/v1/me"));
        assert!(is_access("/v1/me/passkeys/{id}"));
        assert!(is_access("/oauth/token"));
        assert!(!is_access("/v1/meters"));
        assert!(!is_access("/v1/messages"));
    }

    /// Every class is reachable, the queue and counter tables are named exactly, any other
    /// product table is a fact, and anything outside `public` is the system's.
    #[test]
    fn tables_fall_in_their_class() {
        let expected = |class: TableClass| match class {
            TableClass::Facts => ("public", "messages"),
            TableClass::Queue => ("public", "delivery_queue"),
            TableClass::Counters => ("public", "stats_increments"),
            TableClass::System => ("pg_catalog", "pg_class"),
        };
        for class in TableClass::iter() {
            let (schema, table) = expected(class);
            assert_eq!(table_class(schema, table), class, "{schema}.{table}");
        }
        for table in QUEUE_TABLES {
            assert_eq!(table_class("public", table), TableClass::Queue, "{table}");
        }
        for table in COUNTER_TABLES {
            assert_eq!(
                table_class("public", table),
                TableClass::Counters,
                "{table}"
            );
        }
        assert_eq!(table_class("pg_toast", "pg_toast_1234"), TableClass::System);
        assert_eq!(table_class("public", "attempts"), TableClass::Facts);
    }

    /// The leaves a policy expects online: its window in whole periods (rounded up), the current
    /// leaf, the leaves made two days ahead (at least the next) and the one waiting for the
    /// archive. These are the shipped policies' counts, which `archive-debt` doubles.
    #[test]
    fn expected_leaves_count_the_window_the_current_leaf_those_ahead_and_one_due() {
        let day = Duration::from_secs(86_400);
        // PostgreSQL reads `interval '1 month'` as 30 days in seconds.
        let month = day * 30;
        assert_eq!(expected_leaves(day * 7, day), 7 + 1 + 2 + 1);
        assert_eq!(expected_leaves(day, day), 1 + 1 + 2 + 1);
        assert_eq!(expected_leaves(day * 30, day), 30 + 1 + 2 + 1);
        assert_eq!(expected_leaves(day * 30, month), 1 + 1 + 1 + 1);
        assert_eq!(
            expected_leaves(day + Duration::from_secs(1), day),
            2 + 1 + 2 + 1
        );
        // No period divides by zero.
        assert!(expected_leaves(day, Duration::ZERO) > 0);
    }

    /// The denial window reports the most denials of one key over the last complete minute or
    /// the current one: counts are per key, a quiet minute after a busy one brings the top to 0,
    /// a clock that steps back counts in the current minute, and a large address spray
    /// cannot hide an offender first seen later.
    #[test]
    fn the_denial_window_reports_the_top_key_of_the_last_two_minutes() {
        let mut window = DenialWindow::default();
        assert_eq!(window.top(1_000), 0);
        for _ in 0..3 {
            window.deny(b"a", 1_000);
        }
        window.deny(b"b", 1_000);
        assert_eq!(window.top(1_000), 3);
        // The next minute keeps the last one's top until a key passes it.
        assert_eq!(window.top(1_001), 3);
        for _ in 0..4 {
            window.deny(b"b", 1_001);
        }
        assert_eq!(window.top(1_001), 4);
        // A clock stepping back counts in the current minute.
        window.deny(b"b", 1_000);
        assert_eq!(window.top(1_001), 5);
        // A minute without denials, then the top is 0; a gap of minutes forgets at once.
        assert_eq!(window.top(1_002), 5);
        assert_eq!(window.top(1_003), 0);
        window.deny(b"c", 1_004);
        assert_eq!(window.top(1_010), 0);

        let mut full = DenialWindow::default();
        for key in 0..DENIAL_KEYS {
            full.deny(&key.to_be_bytes(), 7);
        }
        for _ in 0..101 {
            full.deny(b"late", 7);
        }
        assert!(full.top(7) >= 101);
        assert_eq!(full.counts.len(), DENIAL_KEYS * DENIAL_ROWS);
        assert_eq!(full.top(9), 0);
    }
}
