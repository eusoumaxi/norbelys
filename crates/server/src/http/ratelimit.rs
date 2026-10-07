//! Rate limits: per-key budgets that bound how fast one workspace, one person, one client
//! address, one email address or one session may repeat a request; the middleware that applies
//! them to every `/v1` request; and the client address they key on.
//!
//! # Policies
//!
//! Every `/v1` request spends one unit of exactly one **request policy**, chosen by its
//! credential ([`layer`]); a few operations spend a policy of their own as well.
//!
//! | Policy | Key | Budget | Why |
//! |---|---|---|---|
//! | `default` | the workspace | 6,000 requests per minute | every request a program makes with the workspace's credential (an API key, a command-line or MCP access token): what a busy integration needs (100 a second), and a bound on one runaway loop's share of the api |
//! | `send` | the workspace | 3,000 messages per minute | the messages `POST /v1/messages` creates: one for a direct message or a reply, one per person of a step's `person_ids` (at most 100); spent before any work, besides the request's own `default` unit |
//! | `access` | the person, or the client address before sign-in | 1,200 requests per minute | the dashboard (a session cookie or a workspace token): a person clicking and polling in several tabs stays far below it; anonymous requests (signing in) share their address's budget |
//! | `code_email` | the email's comparison key | 5 per 15 minutes | each email-code challenge sends mail to that inbox |
//! | `code_address` | the client address | 5 per 15 minutes | one client cannot spray codes at many inboxes |
//! | `code_email_degraded`, `code_address_degraded` | as above | 1 per 15 minutes | while the captcha provider cannot be reached, challenges proceed under this tighter budget instead |
//! | `sign_in` | the client address | 10 per 15 minutes | finishing a sign-in (a code, a link, a passkey) is where guesses would happen |
//! | `token_mint` | the session | 60 per minute | a dashboard tab mints a workspace token every few minutes; more is a script |
//! | `device_start` | the client address | 10 per 15 minutes | starting a device login is anonymous and stores a code; a person logs in a few times a day |
//! | `token_endpoint` | the OAuth client (`client_id`) | 30 per minute | `POST /oauth/token`: a client refreshes every ten minutes or so per grant; more is a loop or a guess at a secret, counted before the client authenticates |
//! | `oauth_address` | the client address | 60 per minute | `GET /oauth/authorize`, `POST /oauth/token` and `POST /oauth/revoke`: the anonymous OAuth protocol endpoints, spent before any metadata fetch a `https://…` client id would trigger, so one address cannot exhaust the process-global fetch slots and deny onboarding of a new MCP server (`POST /oauth/device_authorization` is tighter, under `device_start`) |
//!
//! # Algorithm
//!
//! Each policy is a keyed GCRA limiter from `governor` (the generic cell rate algorithm, a
//! leaky bucket without a timer): a budget of `n` per window `w` is a burst of `n` that refills
//! one unit every `w / n`. A client that spends its burst waits `w / n` for the next unit, not
//! the rest of a fixed window, and there is no window edge to burst across. A spend of several
//! units (a request creating many messages) is admitted whole or not at all, and a refused spend
//! takes nothing. Buckets of keys that are full again are dropped every few thousand checks, so
//! memory follows the clients active in the last window.
//!
//! # Replicas
//!
//! The buckets live in the process, so no shared store sits on the request path. The request
//! policies (`default`, `send`, `access`) are budgets of the deployment: each of its api
//! replicas (`API_REPLICAS`) holds a share, the budget divided by their number, never less than
//! one request nor less than the 100 messages one request may create. The proxy in front spreads
//! requests over the replicas, so together they admit about the budget, and a client that paces
//! itself by the answers it gets is not refused. The sign-in policies are not divided: each
//! replica holds the whole budget, so a deployment of `r` replicas admits at most `r` times it,
//! which still bounds abuse; dividing a budget of a handful of attempts would refuse people whose
//! attempts happen to land on one replica.
//!
//! # Answers
//!
//! Every `/v1` response carries its request policy's fields from the IETF
//! `draft-ietf-httpapi-ratelimit-headers`
//! (<https://datatracker.ietf.org/doc/draft-ietf-httpapi-ratelimit-headers/>):
//! `RateLimit-Policy: "default";q=6000;w=60` (the quota this replica holds, per the window in
//! seconds) and `RateLimit: "default";r=5999;t=1` (what is left, and the seconds until the
//! budget is whole again). An operation that spends a policy of its own adds that policy's
//! fields, the lists being split over several field lines. A refused request answers
//! `429 rate_limited` with `Retry-After`, the seconds until the refused spend would be admitted,
//! and with `RateLimit` saying that nothing is left until then (`r=0`, `t` the same seconds),
//! since the draft asks that `Retry-After` not point before the end of that window.
//!
//! The middleware runs after authentication (the credential decides the policy) and before
//! idempotency, so every request spends a unit, a retry answered from its stored response too,
//! and the answers idempotency gives itself (a missing key, a key in use) carry the fields as
//! well. A stored response keeps the fields of an operation's own policy, so its replay shows
//! them as they were, followed by the current fields of the request policy. A request whose
//! credential authentication refuses (`401`), and a path with no operation, are answered before
//! any budget applies, without these fields.
//!
//! # The client address
//!
//! [`ClientAddress`] is the TCP peer of the request, or, behind a proxy the operator trusts
//! (`TRUST_FORWARDED_FOR` and `TRUSTED_PROXY_IPS`), the first untrusted address walking
//! `X-Forwarded-For` from right to left. The actual TCP peer must be trusted before any
//! header is read. Malformed or oversized chains fall back to the peer. Requests without a
//! TCP peer (tests in process) share the key `unknown`.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap, HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use governor::clock::{Clock as _, DefaultClock};
use governor::middleware::StateInformationMiddleware;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use strum::IntoEnumIterator as _;

use super::AppState;
use crate::domain::ids::{Id, User, WorkspaceId};
use crate::identity::authority::{Credential, Principal};
use crate::identity::sessions::SignedIn;
use crate::problem::{Code, Problem};

/// Checks between two sweeps of full buckets.
const SWEEP_EVERY: u64 = 4_096;

/// The most messages one request creates: `POST /v1/messages` with 100 `person_ids`. No
/// replica's share of `send` is smaller, so every valid request can be admitted; a request
/// naming more people is refused by validation, and spends this many before it is.
pub const MOST_MESSAGES_PER_REQUEST: u32 = 100;

/// The header of the policy's quota and window.
static POLICY_HEADER: HeaderName = HeaderName::from_static("ratelimit-policy");
/// The header of what is left of the budget.
static LIMIT_HEADER: HeaderName = HeaderName::from_static("ratelimit");

type Limiter =
    RateLimiter<Vec<u8>, DefaultKeyedStateStore<Vec<u8>>, DefaultClock, StateInformationMiddleware>;

/// A rate-limit policy (see the module).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Policy {
    /// Email-code challenges per email address.
    CodeEmail,
    /// Email-code challenges per client address.
    CodeAddress,
    /// Email-code challenges per email address while the captcha provider is unreachable.
    CodeEmailDegraded,
    /// Email-code challenges per client address while the captcha provider is unreachable.
    CodeAddressDegraded,
    /// Sign-in finishes per client address.
    SignIn,
    /// Workspace tokens minted per session.
    TokenMint,
    /// Device logins started per client address.
    DeviceStart,
    /// Token requests per OAuth client.
    TokenEndpoint,
    /// Anonymous OAuth requests per client address, before any metadata fetch.
    OauthAddress,
    /// Requests per workspace, by a program's credential.
    Default,
    /// Messages created per workspace.
    Send,
    /// Requests per person on the dashboard, or per client address before sign-in.
    Access,
}

impl Policy {
    /// The policy's name in the headers.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The deployment's budget: this many units per this window.
    #[must_use]
    pub fn budget(self) -> (u32, Duration) {
        const FIFTEEN_MINUTES: Duration = Duration::from_secs(15 * 60);
        const MINUTE: Duration = Duration::from_secs(60);
        match self {
            Self::CodeEmail | Self::CodeAddress => (5, FIFTEEN_MINUTES),
            Self::CodeEmailDegraded | Self::CodeAddressDegraded => (1, FIFTEEN_MINUTES),
            Self::SignIn | Self::DeviceStart => (10, FIFTEEN_MINUTES),
            Self::TokenMint | Self::OauthAddress => (60, MINUTE),
            Self::TokenEndpoint => (30, MINUTE),
            Self::Default => (6_000, MINUTE),
            Self::Send => (3_000, MINUTE),
            Self::Access => (1_200, MINUTE),
        }
    }

    /// Whether the api replicas share the budget, each holding a part (the request policies),
    /// rather than each holding all of it (the sign-in policies; see the module).
    #[must_use]
    pub fn shared(self) -> bool {
        match self {
            Self::Default | Self::Send | Self::Access => true,
            Self::CodeEmail
            | Self::CodeAddress
            | Self::CodeEmailDegraded
            | Self::CodeAddressDegraded
            | Self::SignIn
            | Self::TokenMint
            | Self::DeviceStart
            | Self::TokenEndpoint
            | Self::OauthAddress => false,
        }
    }

    /// The budget one api process holds in a deployment of `replicas` (zero counts as one): a
    /// shared policy's budget divided among them, never below the most one request spends; any
    /// other policy's whole.
    #[must_use]
    pub fn share(self, replicas: u32) -> (u32, Duration) {
        let (count, window) = self.budget();
        if !self.shared() {
            return (count, window);
        }
        let floor = if self == Self::Send {
            MOST_MESSAGES_PER_REQUEST
        } else {
            1
        };
        let share = count.checked_div(replicas).unwrap_or(count);
        (share.max(floor), window)
    }

    /// What the refused units are, for the `429`'s detail.
    fn units(self) -> &'static str {
        match self {
            Self::Default | Self::Access => "requests",
            Self::Send => "messages",
            Self::CodeEmail
            | Self::CodeAddress
            | Self::CodeEmailDegraded
            | Self::CodeAddressDegraded
            | Self::SignIn
            | Self::TokenMint
            | Self::DeviceStart
            | Self::TokenEndpoint
            | Self::OauthAddress => "attempts",
        }
    }
}

/// The GCRA quota of `count` per `window`: a burst of `count` that refills one unit every
/// `window / count`.
fn quota(count: u32, window: Duration) -> Quota {
    let burst = NonZeroU32::new(count).unwrap_or(NonZeroU32::MIN);
    let period = window
        .checked_div(burst.get())
        .filter(|period| !period.is_zero())
        .unwrap_or(Duration::from_secs(1));
    Quota::with_period(period)
        .unwrap_or_else(|| Quota::per_second(NonZeroU32::MIN))
        .allow_burst(burst)
}

/// A spend its policy admitted, and what is left, for the `RateLimit` fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allowance {
    /// The policy that admitted it.
    pub policy: Policy,
    /// The quota of the budget that admitted it, as this process holds it.
    pub quota: u32,
    /// The budget's window.
    pub window: Duration,
    /// Units left in the budget after this spend.
    pub remaining: u32,
    /// Until the budget is whole again.
    pub reset: Duration,
}

impl Allowance {
    /// Adds the IETF `RateLimit-Policy` and `RateLimit` fields to `headers`, after any already
    /// there.
    pub fn write(&self, headers: &mut HeaderMap) {
        fields(
            headers,
            self.policy,
            self.quota,
            self.window,
            self.remaining,
            seconds_up(self.reset),
        );
    }
}

/// A spend its policy refused: what the `429` answer says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    /// The policy that refused it.
    pub policy: Policy,
    /// The quota of the budget, as this process holds it.
    pub quota: u32,
    /// The budget's window.
    pub window: Duration,
    /// Whole seconds, at least one, until the refused spend would be admitted.
    pub retry_after: u64,
}

impl Refusal {
    /// The `429 rate_limited` problem, with `Retry-After`.
    #[must_use]
    pub fn problem(&self) -> Problem {
        Problem {
            retry_after: Some(self.retry_after),
            ..Problem::new(
                Code::RateLimited,
                format!(
                    "Too many {}; try again in {} seconds.",
                    self.policy.units(),
                    self.retry_after
                ),
            )
        }
    }

    /// Adds the IETF fields of the refusal to `headers`: the policy, and nothing left until
    /// `Retry-After`.
    pub fn write(&self, headers: &mut HeaderMap) {
        fields(
            headers,
            self.policy,
            self.quota,
            self.window,
            0,
            self.retry_after,
        );
    }
}

impl IntoResponse for Refusal {
    /// The `429` problem with `Retry-After` and the policy's fields.
    fn into_response(self) -> Response {
        let mut response = self.problem().into_response();
        self.write(response.headers_mut());
        response
    }
}

/// Appends `policy`'s `RateLimit-Policy` (`quota` per `window`) and `RateLimit` (`remaining`
/// left, whole again in `reset` seconds) fields to `headers`.
fn fields(
    headers: &mut HeaderMap,
    policy: Policy,
    quota: u32,
    window: Duration,
    remaining: u32,
    reset: u64,
) {
    let name = policy.as_str();
    if let Ok(value) =
        HeaderValue::from_str(&format!("\"{name}\";q={quota};w={}", window.as_secs()))
    {
        headers.append(POLICY_HEADER.clone(), value);
    }
    if let Ok(value) = HeaderValue::from_str(&format!("\"{name}\";r={remaining};t={reset}")) {
        headers.append(LIMIT_HEADER.clone(), value);
    }
}

/// `duration` in whole seconds, rounded up.
fn seconds_up(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() > 0))
}

/// One policy's limiter and the budget it holds in this process.
struct Bucket {
    limiter: Limiter,
    quota: u32,
    window: Duration,
}

struct Inner {
    buckets: HashMap<Policy, Bucket>,
    checks: AtomicU64,
    trust_forwarded_for: bool,
    trusted_proxy_ips: Vec<IpAddr>,
}

/// The rate limiters of one api process.
#[derive(Clone)]
pub struct Limits {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Limits {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Limits(..)")
    }
}

impl Limits {
    /// Fresh limiters for every policy, holding the budgets of one api process in a deployment
    /// of `api_replicas` (see the module). `trust_forwarded_for` takes the client address from
    /// the `X-Forwarded-For` header a trusted proxy sets.
    #[must_use]
    pub fn new(
        trust_forwarded_for: bool,
        api_replicas: u32,
        trusted_proxy_ips: Vec<IpAddr>,
    ) -> Self {
        let buckets = Policy::iter()
            .map(|policy| {
                let (count, window) = policy.share(api_replicas);
                let limiter = RateLimiter::keyed(quota(count, window))
                    .with_middleware::<StateInformationMiddleware>();
                (
                    policy,
                    Bucket {
                        limiter,
                        quota: count,
                        window,
                    },
                )
            })
            .collect();
        Self {
            inner: Arc::new(Inner {
                buckets,
                checks: AtomicU64::new(0),
                trust_forwarded_for,
                trusted_proxy_ips,
            }),
        }
    }

    /// Whether the client address comes from `X-Forwarded-For`.
    #[must_use]
    pub fn trust_forwarded_for(&self) -> bool {
        self.inner.trust_forwarded_for
    }

    /// TCP peers authorized to provide a forwarding chain.
    #[must_use]
    pub fn trusted_proxy_ips(&self) -> &[IpAddr] {
        &self.inner.trusted_proxy_ips
    }

    /// Spends one request of `key`'s budget under `policy`.
    ///
    /// # Errors
    ///
    /// `429 rate_limited` with the seconds until the next request is admitted.
    pub fn check(&self, policy: Policy, key: &[u8]) -> Result<Allowance, Problem> {
        self.spend(policy, key, NonZeroU32::MIN)
            .map_err(|refusal| refusal.problem())
    }

    /// Spends `units` of `key`'s budget under `policy`: all of them, or none.
    ///
    /// # Errors
    ///
    /// The [`Refusal`] when the budget cannot take them now; a spend larger than the whole
    /// budget, which no valid request makes, is refused with a wait of one window.
    pub fn spend(
        &self,
        policy: Policy,
        key: &[u8],
        units: NonZeroU32,
    ) -> Result<Allowance, Refusal> {
        self.sweep();
        let Some(bucket) = self.inner.buckets.get(&policy) else {
            // `new` makes a bucket for every policy, so this cannot happen; admitting is the
            // answer that locks nobody out.
            tracing::error!(
                policy = policy.as_str(),
                "a rate-limit policy has no limiter"
            );
            let (quota, window) = policy.budget();
            return Ok(Allowance {
                policy,
                quota,
                window,
                remaining: quota,
                reset: Duration::ZERO,
            });
        };
        let refusal = |retry_after: u64| Refusal {
            policy,
            quota: bucket.quota,
            window: bucket.window,
            retry_after: retry_after.max(1),
        };
        match bucket.limiter.check_key_n(&key.to_vec(), units) {
            Ok(Ok(snapshot)) => {
                let quota = snapshot.quota();
                let remaining = snapshot.remaining_burst_capacity();
                let spent = quota.burst_size().get().saturating_sub(remaining);
                let reset = quota
                    .replenish_interval()
                    .checked_mul(spent)
                    .unwrap_or(bucket.window);
                Ok(Allowance {
                    policy,
                    quota: bucket.quota,
                    window: bucket.window,
                    remaining,
                    reset,
                })
            }
            Ok(Err(not_until)) => Err(refusal(seconds_up(
                not_until.wait_time_from(bucket.limiter.clock().now()),
            ))),
            Err(_) => Err(refusal(bucket.window.as_secs())),
        }
    }

    /// Spends `count` messages of `workspace`'s `send` budget: at least one, and at most the
    /// most one request creates (a request naming more is refused by its validation anyway).
    ///
    /// # Errors
    ///
    /// The [`Refusal`] when the budget cannot take them now.
    pub fn spend_messages(
        &self,
        workspace: WorkspaceId,
        count: usize,
    ) -> Result<Allowance, Refusal> {
        let units = u32::try_from(count)
            .unwrap_or(u32::MAX)
            .clamp(1, MOST_MESSAGES_PER_REQUEST);
        self.spend(
            Policy::Send,
            &workspace_key(workspace),
            NonZeroU32::new(units).unwrap_or(NonZeroU32::MIN),
        )
    }

    /// Drops the buckets that are full again, every few thousand checks.
    fn sweep(&self) {
        let checks = self.inner.checks.fetch_add(1, Ordering::Relaxed);
        if checks % SWEEP_EVERY == SWEEP_EVERY - 1 {
            for bucket in self.inner.buckets.values() {
                bucket.limiter.retain_recent();
                bucket.limiter.shrink_to_fit();
            }
        }
    }
}

/// Middleware on `/v1`, after authentication and before idempotency (see the module): spends
/// one unit of the request's policy and adds its fields to the answer, or answers `429`.
pub async fn layer(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let (policy, key) = subject(
        request.extensions(),
        request.headers(),
        state.limits.trust_forwarded_for(),
        state.limits.trusted_proxy_ips(),
    );
    match state.limits.spend(policy, &key, NonZeroU32::MIN) {
        Ok(allowance) => {
            let mut response = next.run(request).await;
            allowance.write(response.headers_mut());
            response
        }
        Err(refusal) => refusal.into_response(),
    }
}

/// The request policy a request spends, and the key of its budget: a program's credential (an
/// API key, a command-line or MCP access token) spends its workspace's `default`; the dashboard
/// (a workspace token, or the session cookie alone) its person's `access`; an anonymous request
/// its client address's `access`.
fn subject(
    extensions: &Extensions,
    headers: &HeaderMap,
    trust_forwarded_for: bool,
    trusted_proxy_ips: &[IpAddr],
) -> (Policy, Vec<u8>) {
    if let Some(principal) = extensions.get::<Principal>() {
        return match principal.credential {
            Credential::ApiKey | Credential::OAuth => {
                (Policy::Default, workspace_key(principal.workspace))
            }
            Credential::WorkspaceToken => (Policy::Access, user_key(principal.actor.user())),
        };
    }
    if let Some(signed_in) = extensions.get::<SignedIn>() {
        return (Policy::Access, user_key(signed_in.user));
    }
    let client = ClientAddress::of(headers, extensions, trust_forwarded_for, trusted_proxy_ips);
    (
        Policy::Access,
        format!("address:{}", client.as_key()).into_bytes(),
    )
}

/// The budget key of a workspace.
fn workspace_key(workspace: WorkspaceId) -> Vec<u8> {
    workspace.uuid().as_bytes().to_vec()
}

/// The budget key of a person; it never equals an address's (`address:…`).
fn user_key(user: Id<User>) -> Vec<u8> {
    format!("user:{}", user.uuid()).into_bytes()
}

/// The address a request comes from, as far as the api can tell (see the module).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientAddress(pub Option<IpAddr>);

impl ClientAddress {
    /// The address of the request with these `headers` and `extensions`.
    #[must_use]
    pub fn of(
        headers: &HeaderMap,
        extensions: &Extensions,
        trust_forwarded_for: bool,
        trusted_proxy_ips: &[IpAddr],
    ) -> Self {
        let peer = extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(address)| address.ip());
        let Some(mut client) = peer else {
            return Self(None);
        };
        if !trust_forwarded_for || !trusted_proxy_ips.contains(&client) {
            return Self(peer);
        }
        let Some(header) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= 1024)
        else {
            return Self(peer);
        };
        let chain = header
            .split(',')
            .map(|hop| hop.trim().parse::<IpAddr>())
            .collect::<Result<Vec<_>, _>>();
        let Ok(chain) = chain else {
            return Self(peer);
        };
        if chain.len() > 16 {
            return Self(peer);
        }
        for hop in chain.into_iter().rev() {
            if !trusted_proxy_ips.contains(&client) {
                break;
            }
            client = hop;
        }
        Self(Some(client))
    }

    /// The address as text, `unknown` when there is none: what limits and hashes key on.
    #[must_use]
    pub fn as_key(&self) -> String {
        self.0
            .map_or_else(|| "unknown".to_owned(), |address| address.to_string())
    }
}

impl FromRequestParts<AppState> for ClientAddress {
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self::of(
            &parts.headers,
            &parts.extensions,
            state.limits.trust_forwarded_for(),
            state.limits.trusted_proxy_ips(),
        ))
    }
}

#[cfg(test)]
mod tests;
