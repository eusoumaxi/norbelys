//! The in-process limiters every provider call is charged to: the short-window rate limits that
//! providers publish, held by each replica of the roles that call the providers as token
//! buckets.
//!
//! # Which limits
//!
//! A submission is charged to every limiter whose key it falls under, all of them or none:
//!
//! | Limiter | Key | Published limit | One submission is charged |
//! |---|---|---|---|
//! | Gmail API, per user | the mailbox | 6,000 units a minute | 100 units |
//! | Gmail API, per project | Norbelys's Google Cloud project | 1,200,000 units a minute | 100 units |
//! | Gmail API, per project per day | the same project | 80,000,000 units a day | 100 units |
//! | Microsoft Graph, per mailbox | the mailbox, in Norbelys's app | 10,000 requests in 10 minutes | 1 request |
//! | Microsoft Graph, per app | Norbelys's app, every tenant | 130,000 requests in 10 seconds | 1 request |
//! | Exchange Online, per mailbox | the mailbox | 30 messages a minute | 1 message |
//! | A quota scope's window | the customer's account (SES, SendGrid, Mailgun) | what the scope records (`window_limit` per `window_seconds`) | its unit: 1 request, the message's recipients, or 100 units |
//!
//! The inbox's reads and a connection's check are charged to the Gmail and Graph limiters too,
//! each call its own cost ([`Read`]): `history.list` 2 units, `messages.list` 5, `messages.get`
//! 20, `getProfile` and `settings.sendAs.list` 1, and one request for each Graph request to a
//! mailbox. Graph's `GET /me` and the identity platforms' token endpoints are other keys, called a
//! few times a day per connection, and left to their own throttling answers.
//!
//! Sources, read 2026-10-01: Gmail API usage limits
//! (<https://developers.google.com/workspace/gmail/api/reference/quota>), Microsoft Graph
//! throttling limits (<https://learn.microsoft.com/graph/throttling-limits>), Exchange Online
//! limits (<https://learn.microsoft.com/office365/servicedescriptions/exchange-online-service-description/exchange-online-limits>),
//! Amazon SES sending quotas (<https://docs.aws.amazon.com/ses/latest/dg/manage-sending-quotas.html>).
//!
//! The project and app limits are shared by every workspace, because mailboxes connected through
//! OAuth use Norbelys's own Google project and Microsoft app; a quota scope holds only the limits
//! of an account the customer owns.
//!
//! # Who holds what
//!
//! A limit that several roles consume (Gmail's units and Graph's requests) is split between them
//! in configuration ([`Split`]): sending 80 %, receiving 15 % and maintenance 5 % by default. A
//! limit only sending consumes (Exchange's messages, a relay's rate) is sending's whole. Each
//! role's part is divided by the number of its replicas ([`Shares`]). A role refuses to start
//! when the parts sum above the whole limit or a replica count is below one: the limit the
//! deployment would then run against is not the provider's, and clamping a wrong number in
//! silence would hide it. The replica counts change through a rolling restart in a safe order:
//! raised before a replica is added, lowered after one is removed.
//!
//! A replica holds its part with a token bucket whose rate `R` is that part and whose burst `B` is
//! the largest single charge (100 Gmail units, one request, or a message's recipients, at most
//! 150). A bucket starts empty and in any interval `t` admits at most `R × t + B` (RFC 3290,
//! appendix A.2, <https://www.rfc-editor.org/rfc/rfc3290#appendix-A.2>), so across the buckets of
//! one limit a provider's window sees at most the limit plus one charge per bucket.
//!
//! A submission the limiters refuse ends the claim's wave there ([`Limits::admit`]): the message
//! stays queued and the connection is claimed again at a later sweep, so a refusal costs nothing
//! but time. A read waits for its tokens instead ([`Limits::wait`]), up to the deadline of the
//! poll or check that makes it, since its lease holds the mailbox for it either way.
//!
//! # Platform waits
//!
//! When Google or Microsoft answers that Norbelys's own project or app is over its limit, the
//! finish returns how long to wait; [`Limits::pause_platform`] makes every later charge through
//! that provider wait until then, in this replica.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use opentelemetry::metrics::Counter;
use strum::IntoEnumIterator as _;
use uuid::Uuid;

use super::claim::{Charge, ScopeWindow};
use crate::config::LimitShareArgs;
use crate::domain::ids::{Connection, Id};
use crate::domain::senders::Provider;
use crate::domain::time::Timestamp;

/// Gmail API units one `messages.send` costs: the largest single Gmail charge, so every Gmail
/// bucket's burst.
const GMAIL_SEND_UNITS: f64 = 100.0;
/// The most envelope recipients one message may have: the burst of a recipient-charged bucket.
const MAX_RECIPIENTS: f64 = 150.0;
/// Buckets not touched for this long are dropped (they start empty again when next needed).
const IDLE: Duration = Duration::from_secs(3_600);
/// The map is swept for idle buckets once it holds this many.
const SWEEP_AT: usize = 4_096;

static GMAIL_PROJECT_UNITS: LazyLock<Counter<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_counter("norbelys_gmail_project_units_total")
        .with_description(
            "Gmail API units charged to Norbelys's Google Cloud project by this process's \
             limiters (sending, polling, checks).",
        )
        .build()
});
/// How far above 1 the shares may sum and still be the whole limit: decimal fractions are not
/// exact in binary, so 0.8 + 0.15 + 0.05 is not exactly 1.
const SUM_TOLERANCE: f64 = 1e-9;

/// The roles that call the providers, each holding its own part of the limits they share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Role {
    /// The sender: submissions.
    Sending,
    /// The inbox: reading mailboxes (Gmail's `history.list` and `messages.get`, Graph's delta
    /// and message reads).
    Receiving,
    /// The worker's maintenance lane: `connection.check` (identity reads, the Sent-folder
    /// searches that settle uncertain messages).
    Maintenance,
}

impl Role {
    /// The role's name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// How the limits several roles consume are split between the roles: each one's fraction of
/// Gmail's units and Graph's requests. Every role is configured with the whole split, so
/// each can refuse a split that oversubscribes a limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Split {
    /// Sending's fraction.
    pub sending: f64,
    /// Receiving's fraction.
    pub receiving: f64,
    /// Maintenance's fraction.
    pub maintenance: f64,
}

impl Default for Split {
    fn default() -> Self {
        Self {
            sending: 0.8,
            receiving: 0.15,
            maintenance: 0.05,
        }
    }
}

impl From<LimitShareArgs> for Split {
    fn from(args: LimitShareArgs) -> Self {
        Self {
            sending: args.sending,
            receiving: args.receiving,
            maintenance: args.maintenance,
        }
    }
}

impl Split {
    /// `role`'s fraction.
    #[must_use]
    pub fn of(self, role: Role) -> f64 {
        match role {
            Role::Sending => self.sending,
            Role::Receiving => self.receiving,
            Role::Maintenance => self.maintenance,
        }
    }
}

/// What one replica's limiters hold: its role, the role's fraction of the shared limits, and the
/// number of the role's replicas, checked together ([`Shares::new`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shares {
    role: Role,
    part: f64,
    replicas: u32,
}

/// Why a role's limiter configuration is refused at start.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SharesError {
    /// A fraction is not between 0 and 1 (or not a number).
    #[error("the {role} share of the provider limits is {share}; a share is between 0 and 1")]
    OutOfRange { role: &'static str, share: f64 },
    /// The fractions take more than the whole limit.
    #[error(
        "the sending, receiving and maintenance shares of the provider limits sum to {sum}; \
         together they take at most the whole limit, 1"
    )]
    AboveWhole { sum: f64 },
    /// The role is configured with no replica.
    #[error("the {role} role is configured with {replicas} replicas; it runs at least one")]
    NoReplica { role: &'static str, replicas: u32 },
}

impl Shares {
    /// The part of a replica of `role` when the shared limits are split as `split` and the role
    /// runs `replicas` replicas.
    ///
    /// # Errors
    ///
    /// A fraction is outside 0 to 1, the fractions sum above 1, or `replicas` is 0: the role must
    /// refuse to start.
    pub fn new(role: Role, split: Split, replicas: u32) -> Result<Self, SharesError> {
        for each in Role::iter() {
            let share = split.of(each);
            if !(0.0..=1.0).contains(&share) {
                return Err(SharesError::OutOfRange {
                    role: each.as_str(),
                    share,
                });
            }
        }
        let sum = split.sending + split.receiving + split.maintenance;
        if sum > 1.0 + SUM_TOLERANCE {
            return Err(SharesError::AboveWhole { sum });
        }
        if replicas == 0 {
            return Err(SharesError::NoReplica {
                role: role.as_str(),
                replicas,
            });
        }
        Ok(Self {
            role,
            part: split.of(role),
            replicas,
        })
    }

    /// The default split's part of `role`, held by its only replica: development and tests.
    #[must_use]
    pub fn single(role: Role) -> Self {
        Self {
            role,
            part: Split::default().of(role),
            replicas: 1,
        }
    }

    /// The fraction of a shared limit this replica holds.
    fn shared(self) -> f64 {
        self.part / f64::from(self.replicas)
    }

    /// The fraction of a limit only sending consumes that this replica holds: its replicas'
    /// share of the whole for the sender, nothing for the roles that never submit.
    fn whole(self) -> f64 {
        match self.role {
            Role::Sending => 1.0 / f64::from(self.replicas),
            Role::Receiving | Role::Maintenance => 0.0,
        }
    }
}

impl Default for Shares {
    fn default() -> Self {
        Self::single(Role::Sending)
    }
}

/// A provider call that is not a submission, made by the inbox or by a connection's check, with
/// what its provider charges for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Read {
    /// Gmail `users.history.list`: 2 units.
    GmailHistory,
    /// Gmail `users.messages.list` (a resync's listing, a Sent-folder search): 5 units.
    GmailList,
    /// Gmail `users.messages.get`: 20 units.
    GmailGet,
    /// Gmail `users.getProfile` or `users.settings.sendAs.list`: 1 unit.
    GmailProfile,
    /// One Microsoft Graph request to a mailbox (a delta page, a message's metadata or its
    /// content, a Sent-folder search): 1 request.
    Graph,
}

impl Read {
    /// The provider whose limits it is charged to.
    #[must_use]
    pub fn provider(self) -> Provider {
        match self {
            Self::GmailHistory | Self::GmailList | Self::GmailGet | Self::GmailProfile => {
                Provider::Google
            }
            Self::Graph => Provider::Microsoft,
        }
    }

    /// What it costs: Gmail's quota units, or Graph's requests.
    #[must_use]
    pub fn cost(self) -> f64 {
        match self {
            Self::GmailHistory => 2.0,
            Self::GmailList => 5.0,
            Self::GmailGet => 20.0,
            Self::GmailProfile | Self::Graph => 1.0,
        }
    }
}

/// A read's tokens would come only after its deadline, or never (a share of zero): the read is
/// not made, and the poll or check that wanted it ends for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "Norbelys's share of the provider's rate limit is spent past the deadline; it is read again later"
)]
pub struct Exhausted;

/// Which limit a bucket holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    GmailUser(Id<Connection>),
    GmailProjectMinute,
    GmailProjectDay,
    GraphMailbox(Id<Connection>),
    GraphApp,
    ExchangeMailbox(Id<Connection>),
    /// A quota scope's window, keyed by its parameters too: a changed window starts a new bucket.
    Scope {
        scope: Uuid,
        limit: i32,
        seconds: i32,
        unit: Unit,
    },
}

/// What a quota scope's window counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Unit {
    Requests,
    Recipients,
    Units,
}

/// One token bucket: `rate` tokens a second up to `burst`, starting empty.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn empty(rate: f64, burst: f64, now: Instant) -> Self {
        Self {
            rate,
            burst,
            tokens: 0.0,
            at: now,
        }
    }

    /// The tokens available at `now`.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.at = now;
    }
}

/// One limiter a charge needs, with what it takes from it.
struct Need {
    key: Key,
    rate: f64,
    burst: f64,
    amount: f64,
}

/// The replica's limiters. Cheap to clone; every clone shares the buckets.
#[derive(Debug, Clone)]
pub struct Limits {
    shares: Shares,
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    buckets: HashMap<Key, Bucket>,
    platform_until: HashMap<Provider, Timestamp>,
}

impl Limits {
    /// Limiters holding this replica's part of each limit.
    #[must_use]
    pub fn new(shares: Shares) -> Self {
        Self {
            shares,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Charges one submission to every limiter it falls under, at `now`: true when each had the
    /// tokens (they are taken), false when one did not (nothing is taken).
    pub fn admit(&self, charge: &Charge<'_>) -> bool {
        self.admit_at(charge, Instant::now(), crate::process::now())
    }

    fn admit_at(&self, charge: &Charge<'_>, now: Instant, wall: Timestamp) -> bool {
        self.take(&needs(self.shares, charge), charge.provider, now, wall)
            .is_ok()
    }

    /// Charges one `read` of `connection`'s mailbox to its provider's limiters, waiting until
    /// they all hold its cost (then taken from each), or a platform wait ends.
    ///
    /// # Errors
    ///
    /// [`Exhausted`] when the tokens would come only after `deadline`, or never: the read must
    /// not be made.
    pub async fn wait(
        &self,
        connection: Id<Connection>,
        read: Read,
        deadline: tokio::time::Instant,
    ) -> Result<(), Exhausted> {
        let needs = read_needs(self.shares, connection, read);
        loop {
            let now = tokio::time::Instant::now();
            let pause = match self.take(
                &needs,
                read.provider(),
                now.into_std(),
                crate::process::now(),
            ) {
                Ok(()) => return Ok(()),
                Err(Some(pause))
                    if now
                        .checked_add(pause)
                        .is_some_and(|ready| ready <= deadline) =>
                {
                    pause
                }
                Err(_) => return Err(Exhausted),
            };
            tokio::time::sleep(pause).await;
        }
    }

    /// Stops every charge through `provider`'s platform limits (Norbelys's Google project or
    /// Microsoft app) until `until`, in this replica.
    pub fn pause_platform(&self, provider: Provider, until: Timestamp) {
        if let Ok(mut state) = self.state.lock() {
            let entry = state.platform_until.entry(provider).or_insert(until);
            if until > *entry {
                *entry = until;
            }
        }
    }

    /// Takes every one of `needs` at `now` when each bucket holds its amount, all of them or
    /// none; otherwise returns how long until they all would, `None` when one never would (a rate
    /// of zero, or a poisoned lock). Every bucket is created and refilled before any is checked:
    /// a short-circuit would leave the later ones uncreated, so they would start filling only at
    /// a later charge. A bucket's burst grows to the largest burst a charge of it needs, so a
    /// charge larger than the one that created it can still be admitted.
    fn take(
        &self,
        needs: &[Need],
        provider: Provider,
        now: Instant,
        wall: Timestamp,
    ) -> Result<(), Option<Duration>> {
        let Ok(mut state) = self.state.lock() else {
            return Err(None);
        };
        if let Some(until) = state
            .platform_until
            .get(&provider)
            .copied()
            .filter(|until| *until > wall)
        {
            return Err(Some(
                Duration::try_from(until.0.duration_since(wall.0)).unwrap_or(Duration::ZERO),
            ));
        }
        let mut wait = Some(Duration::ZERO);
        for need in needs {
            let bucket = state
                .buckets
                .entry(need.key)
                .or_insert_with(|| Bucket::empty(need.rate, need.burst, now));
            bucket.burst = bucket.burst.max(need.burst);
            bucket.refill(now);
            if bucket.tokens < need.amount {
                // A rate of zero gives an infinite wait, which no duration holds: never. The
                // microsecond more makes sleeping the wait enough despite float rounding.
                let short =
                    Duration::try_from_secs_f64((need.amount - bucket.tokens) / bucket.rate)
                        .ok()
                        .map(|short| short.saturating_add(Duration::from_micros(1)));
                wait = wait.zip(short).map(|(longest, this)| longest.max(this));
            }
        }
        let result = match wait {
            Some(wait) if wait.is_zero() => {
                for need in needs {
                    if let Some(bucket) = state.buckets.get_mut(&need.key) {
                        bucket.tokens -= need.amount;
                    }
                    // Every unit of Norbelys's Google project passes here once, whichever role
                    // spends it: the day's limiter is the project's daily threshold.
                    if matches!(need.key, Key::GmailProjectDay) {
                        GMAIL_PROJECT_UNITS.add(need.amount, &[]);
                    }
                }
                Ok(())
            }
            other => Err(other),
        };
        if state.buckets.len() >= SWEEP_AT {
            state
                .buckets
                .retain(|_, bucket| now.saturating_duration_since(bucket.at) < IDLE);
        }
        result
    }
}

/// The Gmail limiters of a charge of `amount` units to `connection`'s mailbox, at the `shared`
/// fraction of each limit.
fn gmail(connection: Id<Connection>, shared: f64, amount: f64) -> [Need; 3] {
    [
        Need {
            key: Key::GmailUser(connection),
            rate: 6_000.0 / 60.0 * shared,
            burst: GMAIL_SEND_UNITS,
            amount,
        },
        Need {
            key: Key::GmailProjectMinute,
            rate: 1_200_000.0 / 60.0 * shared,
            burst: GMAIL_SEND_UNITS,
            amount,
        },
        Need {
            key: Key::GmailProjectDay,
            rate: 80_000_000.0 / 86_400.0 * shared,
            burst: GMAIL_SEND_UNITS,
            amount,
        },
    ]
}

/// The Graph limiters of one request to `connection`'s mailbox, at the `shared` fraction of each
/// limit.
fn graph(connection: Id<Connection>, shared: f64) -> [Need; 2] {
    [
        Need {
            key: Key::GraphMailbox(connection),
            rate: 10_000.0 / 600.0 * shared,
            burst: 1.0,
            amount: 1.0,
        },
        Need {
            key: Key::GraphApp,
            rate: 130_000.0 / 10.0 * shared,
            burst: 1.0,
            amount: 1.0,
        },
    ]
}

/// The limiters `charge` falls under, with their rate (tokens a second, this replica's part),
/// burst and what the submission takes.
fn needs(shares: Shares, charge: &Charge<'_>) -> Vec<Need> {
    let shared = shares.shared();
    let whole = shares.whole();
    let mut needs = Vec::with_capacity(4);
    match charge.provider {
        Provider::Google => needs.extend(gmail(charge.connection, shared, GMAIL_SEND_UNITS)),
        Provider::Microsoft => {
            needs.extend(graph(charge.connection, shared));
            needs.push(Need {
                key: Key::ExchangeMailbox(charge.connection),
                rate: 30.0 / 60.0 * whole,
                burst: 1.0,
                amount: 1.0,
            });
        }
        Provider::Smtp
        | Provider::Ses
        | Provider::Sendgrid
        | Provider::Mailgun
        | Provider::Norbelys => {}
    }
    if let Some(window) = charge.window
        && let Some(need) = scope_need(window, charge.recipients, whole)
    {
        needs.push(need);
    }
    needs
}

/// The limiters one `read` of `connection`'s mailbox falls under: its provider's shared ones,
/// never those only sending consumes.
fn read_needs(shares: Shares, connection: Id<Connection>, read: Read) -> Vec<Need> {
    let shared = shares.shared();
    match read.provider() {
        Provider::Google => gmail(connection, shared, read.cost()).into(),
        Provider::Microsoft
        | Provider::Smtp
        | Provider::Ses
        | Provider::Sendgrid
        | Provider::Mailgun
        | Provider::Norbelys => graph(connection, shared).into(),
    }
}

/// The quota scope's window as a limiter, or `None` when it records no usable rate.
fn scope_need(window: &ScopeWindow, recipients: i32, whole: f64) -> Option<Need> {
    if window.limit <= 0 || window.seconds <= 0 {
        return None;
    }
    let (unit, amount, burst) = match window.unit.as_str() {
        "recipients" => {
            let recipients = f64::from(recipients.max(1));
            (Unit::Recipients, recipients, MAX_RECIPIENTS.max(recipients))
        }
        "units" => (Unit::Units, GMAIL_SEND_UNITS, GMAIL_SEND_UNITS),
        _ => (Unit::Requests, 1.0, 1.0),
    };
    Some(Need {
        key: Key::Scope {
            scope: window.scope,
            limit: window.limit,
            seconds: window.seconds,
            unit,
        },
        rate: f64::from(window.limit) / f64::from(window.seconds) * whole,
        burst,
        amount,
    })
}

#[cfg(test)]
mod tests {

    use super::*;

    fn charge(provider: Provider, window: Option<&ScopeWindow>, recipients: i32) -> Charge<'_> {
        Charge {
            connection: Id::from_uuid(Uuid::nil()),
            provider,
            window,
            recipients,
        }
    }

    /// Every provider has a decided set of limiters: the OAuth mailboxes are charged to their
    /// mailbox and to Norbelys's project or app, the SMTP ways in only to their quota scope. A new
    /// provider fails here until its limiters are decided.
    #[test]
    fn every_provider_falls_under_its_published_limits() {
        for provider in Provider::iter() {
            let keys: Vec<Key> = needs(Shares::default(), &charge(provider, None, 1))
                .into_iter()
                .map(|need| need.key)
                .collect();
            let expected = match provider {
                Provider::Google => 3,
                Provider::Microsoft => 3,
                Provider::Smtp
                | Provider::Ses
                | Provider::Sendgrid
                | Provider::Mailgun
                | Provider::Norbelys => 0,
            };
            assert_eq!(keys.len(), expected, "{provider:?}");
        }
    }

    /// A bucket starts empty and admits at most `R × t + B`: a Gmail mailbox at sending's 80 %
    /// share earns 80 units a second, so its first send waits 1.25 s and two sends need 2.5 s.
    /// Starting empty is what keeps a restarted replica from bursting past the provider's window.
    #[test]
    fn a_bucket_starts_empty_and_refills_at_its_rate() {
        let limits = Limits::new(Shares::default());
        let start = Instant::now();
        let wall = crate::process::now();
        let gmail = charge(Provider::Google, None, 1);
        assert!(!limits.admit_at(&gmail, start, wall));
        assert!(limits.admit_at(&gmail, start + Duration::from_millis(1_250), wall));
        assert!(!limits.admit_at(&gmail, start + Duration::from_millis(1_300), wall));
        assert!(limits.admit_at(&gmail, start + Duration::from_millis(2_500), wall));
    }

    /// A charge takes from every limiter or from none: a refused message does not spend the
    /// tokens of the limiters that had them, so it cannot starve the next message.
    #[test]
    fn a_refused_charge_takes_nothing() {
        let limits = Limits::new(Shares::default());
        let start = Instant::now();
        let wall = crate::process::now();
        let window = ScopeWindow {
            scope: Uuid::nil(),
            limit: 10,
            unit: "recipients".to_owned(),
            seconds: 1,
        };
        // 10 recipients a second: after 1 s the bucket holds 10, short of a 20-recipient message.
        let big = charge(Provider::Ses, Some(&window), 20);
        let small = charge(Provider::Ses, Some(&window), 10);
        assert!(
            !limits.admit_at(&small, start, wall),
            "a new bucket is empty"
        );
        assert!(!limits.admit_at(&big, start + Duration::from_secs(1), wall));
        assert!(limits.admit_at(&small, start + Duration::from_secs(1), wall));
    }

    /// A bucket created by a small charge grows its burst for a larger one: a scope bucket made
    /// for a 10-recipient message (burst 150) still admits a 200-recipient message once it has
    /// refilled, where a burst fixed at creation would never hold 200 tokens and refuse that
    /// message forever.
    #[test]
    fn a_larger_charge_grows_its_bucket() {
        let limits = Limits::new(Shares::default());
        let start = Instant::now();
        let wall = crate::process::now();
        let window = ScopeWindow {
            scope: Uuid::nil(),
            limit: 1_000,
            unit: "recipients".to_owned(),
            seconds: 1,
        };
        assert!(!limits.admit_at(&charge(Provider::Ses, Some(&window), 10), start, wall));
        assert!(limits.admit_at(
            &charge(Provider::Ses, Some(&window), 200),
            start + Duration::from_secs(1),
            wall
        ));
    }

    /// A share is divided by the replicas: two replicas each hold half of Exchange's 30 messages
    /// a minute, so each needs 4 s per message instead of 2.
    #[test]
    fn each_replica_holds_its_part_of_a_limit() {
        let limits = Limits::new(Shares::new(Role::Sending, Split::default(), 2).unwrap());
        let start = Instant::now();
        let wall = crate::process::now();
        let window_free = charge(Provider::Microsoft, None, 1);
        assert!(
            !limits.admit_at(&window_free, start, wall),
            "a new bucket is empty"
        );
        // Graph's own buckets fill fast; Exchange's 0.25 messages a second binds.
        assert!(!limits.admit_at(&window_free, start + Duration::from_secs(3), wall));
        assert!(limits.admit_at(&window_free, start + Duration::from_secs(4), wall));
    }

    /// A platform wait stops every charge through that provider until it ends, and only that
    /// provider's: Google's project being throttled does not hold back a relay.
    #[test]
    fn a_platform_wait_holds_only_its_provider() {
        let limits = Limits::new(Shares::default());
        let start = Instant::now();
        let wall = crate::process::now();
        let gmail = charge(Provider::Google, None, 1);
        assert!(
            !limits.admit_at(&gmail, start, wall),
            "a new bucket is empty"
        );
        limits.pause_platform(Provider::Google, wall.plus(Duration::from_secs(60)));
        assert!(!limits.admit_at(&gmail, start + Duration::from_secs(10), wall));
        assert!(limits.admit_at(
            &gmail,
            start + Duration::from_secs(10),
            wall.plus(Duration::from_secs(61))
        ));
        assert!(limits.admit_at(&charge(Provider::Smtp, None, 1), start, wall));
    }

    /// A role starts only with a split that fits the whole limit and at least one replica: the
    /// default split (80, 15 and 5 %, not exactly 1 in binary) is accepted for every role, while
    /// shares summing above the whole, a share outside 0 to 1 or not a number, and a replica
    /// count of zero are refused, never clamped, so a deployment never runs against a limit
    /// other than the provider's without knowing.
    #[test]
    fn shares_fit_the_whole_limit_and_need_a_replica() {
        for role in Role::iter() {
            let shares = Shares::new(role, Split::default(), 3).unwrap();
            assert_eq!(shares.part, Split::default().of(role), "{role:?}");
            assert_eq!(shares.replicas, 3);
            assert_eq!(
                Shares::new(role, Split::default(), 0),
                Err(SharesError::NoReplica {
                    role: role.as_str(),
                    replicas: 0
                })
            );
        }
        let over = Split {
            maintenance: 0.1,
            ..Split::default()
        };
        assert!(matches!(
            Shares::new(Role::Receiving, over, 1),
            Err(SharesError::AboveWhole { .. })
        ));
        for bad in [1.2, -0.1, f64::NAN] {
            let split = Split {
                receiving: bad,
                ..Split::default()
            };
            assert!(
                matches!(
                    Shares::new(Role::Sending, split, 1),
                    Err(SharesError::OutOfRange {
                        role: "receiving",
                        ..
                    })
                ),
                "{bad}"
            );
        }
    }

    /// Every read is charged its documented cost to its own provider: `history.list` 2 Gmail
    /// units, `messages.list` 5, `messages.get` 20, `getProfile` and `sendAs.list` 1, one Graph
    /// request each. A new read fails here until its cost is decided.
    #[test]
    fn every_read_is_charged_its_documented_cost() {
        for read in Read::iter() {
            let expected = match read {
                Read::GmailHistory => (Provider::Google, 2.0),
                Read::GmailList => (Provider::Google, 5.0),
                Read::GmailGet => (Provider::Google, 20.0),
                Read::GmailProfile => (Provider::Google, 1.0),
                Read::Graph => (Provider::Microsoft, 1.0),
            };
            assert_eq!((read.provider(), read.cost()), expected, "{read:?}");
        }
    }

    /// A read falls under its provider's shared limits only, at its role's part divided by the
    /// role's replicas: receiving's 15 % over two replicas earns a mailbox 7.5 Gmail units a
    /// second, so a message read (20 units) waits 2.67 s from an empty bucket and then goes;
    /// maintenance's 5 % earns 5 units a second, so an identity read waits 0.2 s. A Graph read
    /// is never charged to Exchange's messages a minute, which sending alone consumes.
    #[test]
    fn each_role_holds_its_part_of_the_shared_limits() {
        let mailbox = Id::from_uuid(Uuid::nil());
        let start = Instant::now();
        let wall = crate::process::now();

        let receiving = Limits::new(Shares::new(Role::Receiving, Split::default(), 2).unwrap());
        let get = read_needs(receiving.shares, mailbox, Read::GmailGet);
        assert_eq!(get.len(), 3);
        let Err(Some(wait)) = receiving.take(&get, Provider::Google, start, wall) else {
            panic!("a new bucket is empty");
        };
        assert!((wait.as_secs_f64() - 20.0 / 7.5).abs() < 0.001, "{wait:?}");
        assert_eq!(
            receiving.take(&get, Provider::Google, start + wait, wall),
            Ok(())
        );

        let maintenance = Limits::new(Shares::single(Role::Maintenance));
        let profile = read_needs(maintenance.shares, mailbox, Read::GmailProfile);
        let Err(Some(wait)) = maintenance.take(&profile, Provider::Google, start, wall) else {
            panic!("a new bucket is empty");
        };
        assert!((wait.as_secs_f64() - 0.2).abs() < 0.001, "{wait:?}");

        let keys: Vec<Key> = read_needs(maintenance.shares, mailbox, Read::Graph)
            .into_iter()
            .map(|need| need.key)
            .collect();
        assert_eq!(keys, [Key::GraphMailbox(mailbox), Key::GraphApp]);
    }

    /// A read waits for its tokens and goes, but never past its deadline, and a role holding no
    /// share of a limit never reads at all: the poll or check then ends for now instead of
    /// making a request the provider would throttle. Maintenance's 5 % earns a mailbox 5 Gmail
    /// units a second: an identity read waits 0.2 s, a search right after it would wait a
    /// second.
    #[tokio::test]
    async fn a_read_waits_for_its_tokens_until_its_deadline() {
        let mailbox = Id::from_uuid(Uuid::nil());
        let limits = Limits::new(Shares::single(Role::Maintenance));
        let started = tokio::time::Instant::now();
        assert_eq!(
            limits
                .wait(
                    mailbox,
                    Read::GmailProfile,
                    started + Duration::from_secs(5)
                )
                .await,
            Ok(())
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
        let now = tokio::time::Instant::now();
        assert_eq!(
            limits
                .wait(mailbox, Read::GmailList, now + Duration::from_millis(50))
                .await,
            Err(Exhausted)
        );
        assert!(
            now.elapsed() < Duration::from_millis(50),
            "a refusal does not wait"
        );

        let none = Split {
            maintenance: 0.0,
            ..Split::default()
        };
        let starved = Limits::new(Shares::new(Role::Maintenance, none, 1).unwrap());
        assert_eq!(
            starved
                .wait(
                    mailbox,
                    Read::GmailProfile,
                    now + Duration::from_secs(3_600)
                )
                .await,
            Err(Exhausted)
        );
    }
}
