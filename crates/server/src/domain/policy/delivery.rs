//! What happens to a message, its recipients, its connection and its quota scope.
//!
//! # Retry and the breaker are separate
//!
//! **Retry** decides when one message is submitted again; the **breaker** decides whether a
//! connection or a quota scope may submit at all. A retry never opens a breaker by itself and a
//! breaker never drops a message: a message refused for now waits (until the provider's own wait
//! when it gave one, else the `DELIVERY` backoff), and stops at its deadline.
//!
//! # A submission's answer
//!
//! [`after_submission`] decides the message's next state from the transport's [`Answer`]:
//! accepted means sent and the reservation consumed; uncertain (the final reply was not read
//! after the content went out) consumes too and is never resent automatically; a permanent refusal
//! fails the message and releases its reservation; a transient one re-queues it, unless the
//! instant it would wait for reaches its deadline, in which case it fails as expired now (a
//! provider's wait is never shortened to fit). A full mailbox is a temporary condition of one
//! recipient whatever its class, so it is retried like a transient refusal while its recipient is
//! held.
//!
//! # Who an answer concerns
//!
//! Every refusal carries a [`RefusalScope`]. Only `connection` and `quota_scope` move a breaker
//! ([`connection_effect`], [`scope_effect`]); a recipient's trouble never stops a credential. A
//! limit of Norbelys's own Google project or Microsoft app (`platform`) belongs to no customer
//! row, so each replica backs off in process ([`platform_backoff`]). A refused credential moves
//! the connection to `authorization_required` at once; an account refused by policy or permission
//! (`403`) disables it; three policy rejections in a row (`5.7.x`) disable it too.
//!
//! # The breaker
//!
//! Closed (no pause): the first and second consecutive scoped failures only count, an accepted
//! submission resets the count. The third failure opens it until `now + backoff(failures − 3)`; a
//! throttle answer opens it at once, until the wait it carries or that same step. Open (paused
//! until a later instant): claims skip it. Half-open (the pause over, failures counted): exactly
//! one message at a time is admitted as the probe, named by its queue row and lease generation;
//! only the probe's acceptance, of a submission started after the latest opening, closes it, and
//! a failure reopens it at the next step ([`Breaker`], [`breaker_after`], [`admission`]).
//!
//! # Recipients
//!
//! Evidence suppresses an address only when it is authenticated and names the recipient
//! (`5.1.x` from the server we talked to, a signed complaint), or when a person or the
//! recipient asked; a full mailbox or a domain without mail service holds the address for a
//! while; a corroborated `5.1.x` (a report that matches our message without coming from the
//! server we talked to) holds it while a person reviews it, unless the workspace trusts its own
//! inbox's reports ([`DeliverySettings`]); everything weaker asks a person to review
//! ([`recipient_effect`]). A message is accepted for a connection only within its provider's
//! recipients per message ([`recipients_max`]).
//!
//! # Complaints
//!
//! Complaints also concern the connection that sent the mail: when they reach 0.3 % of what it
//! sent over the last seven days, at least three of them, the connection is disabled until a
//! person looks at its list and content and verifies it again ([`complaint_rate_exceeded`]).

use jiff::{SignedDuration, Timestamp};
use serde_json::Value;
use uuid::Uuid;

use crate::domain::messages::{Kind as MessageKind, RECIPIENTS_MAX, State as MessageState};
use crate::domain::preflight::Reason;
use crate::domain::retry;
use crate::domain::senders::Provider;
use crate::domain::suppressions::Reason as SuppressionReason;

/// The length of a slot of the sending grid: the wait of a message returned for a condition the
/// claim does not filter (a stopped enrollment, a disabled identity), so it is looked at again
/// once a slot rather than at every sweep.
const RECHECK: SignedDuration = SignedDuration::from_secs(300);

/// What one attempt came to (`attempts.outcome`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter, strum::IntoStaticStr, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = AttemptOutcome, rename_all = "snake_case")]
pub enum Outcome {
    /// The provider took the message.
    Accepted,
    /// Refused for now, or not submitted because of the connection; the message may go again.
    Transient,
    /// Refused for good.
    Permanent,
    /// The final reply was not read after the content went out: it may have been accepted.
    Uncertain,
    /// Returned to the queue unstarted (a final check of the Start, a stopping sender, a lease
    /// lost before its submission marker): the reservation is released.
    Released,
    /// A recipient is suppressed: never submitted, never again.
    Suppressed,
    /// Not submitted, for good: preflight found no way to reach it, its deadline passed, its
    /// sender was archived, or its content could not be prepared.
    Skipped,
}

impl Outcome {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// What a refusal means for the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter)]
pub enum Failure {
    /// Nothing was taken, or the provider refused for now.
    Transient,
    /// Refused for good.
    Permanent,
    /// It may have been taken; never resubmitted automatically.
    Uncertain,
}

/// The protocol step a submission ended in (`attempts.phase`, `delivery_events.phase`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter, strum::IntoStaticStr, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = SubmissionPhase, rename_all = "snake_case")]
pub enum Phase {
    /// Resolving, connecting, TLS, the greeting, `EHLO`, `STARTTLS`.
    Connect,
    /// `AUTH`, or acquiring an access token.
    Auth,
    /// `MAIL FROM` and the checks just before it.
    MailFrom,
    /// `RCPT TO`.
    RcptTo,
    /// `DATA`, the content and the final reply.
    Data,
    /// One request to a provider's HTTP API.
    Api,
}

impl Phase {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Who a refusal concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter)]
pub enum RefusalScope {
    /// One recipient's mailbox.
    Recipient,
    /// This message: its size, content, From address.
    Message,
    /// The connection's credential.
    Connection,
    /// A provider-side limit of an account the customer owns, shared by several connections.
    QuotaScope,
    /// A limit of Norbelys's own Google project or Microsoft app.
    Platform,
}

/// What kind of answer ended the submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter)]
pub enum Cause {
    /// The provider refused.
    Refused,
    /// The provider asked to slow down.
    Throttled,
    /// The provider refused the credential.
    Unauthorized,
    /// The provider refused the account by policy or permission.
    Forbidden,
    /// No usable answer: unreachable, reset, malformed, or too late.
    NoReply,
    /// Our own clock ended it before anything could be lost.
    Deadline,
    /// The message cannot go through this server as composed.
    Unsupported,
}

/// An RFC 3463 enhanced status (<https://www.rfc-editor.org/rfc/rfc3463>): the class (2, 4, 5),
/// the subject (1 addressing, 2 mailbox, 3 mail system, 4 routing, 5 protocol, 6 content, 7
/// security and policy) and the detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Enhanced {
    /// 2, 4 or 5.
    pub class: u8,
    /// What it concerns.
    pub subject: u16,
    /// The detail within the subject.
    pub detail: u16,
}

impl Enhanced {
    /// A full mailbox (`X.2.2`).
    #[must_use]
    pub fn mailbox_full(self) -> bool {
        self.subject == 2 && self.detail == 2
    }
}

/// A submission that did not end in acceptance, as facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    /// What it means for the message.
    pub failure: Failure,
    /// Where it happened.
    pub phase: Phase,
    /// Who it concerns.
    pub scope: RefusalScope,
    /// What kind of answer it was.
    pub cause: Cause,
    /// The reply code or HTTP status, when one was read.
    pub code: Option<u16>,
    /// The enhanced status the reply carried.
    pub status: Option<Enhanced>,
    /// The provider's own wait, as an absolute instant; a past or absent one counts as none.
    pub retry_after: Option<Timestamp>,
}

/// What the transport answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The provider took the message.
    Accepted,
    /// It did not.
    Refused(Refusal),
}

/// True for a state a message never leaves by itself: its queue row is gone, and only evidence
/// or a person moves an `uncertain` one.
#[must_use]
pub fn is_final(state: MessageState) -> bool {
    match state {
        MessageState::Sent
        | MessageState::Failed
        | MessageState::Cancelled
        | MessageState::Uncertain
        | MessageState::Suppressed => true,
        MessageState::Queued | MessageState::Claimed | MessageState::InFlight => false,
    }
}

/// What happens to an attempt's reservation of the daily budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Quota {
    /// It counts as used (the provider took the message, or may have).
    Consumed,
    /// It is given back.
    Released,
}

impl Quota {
    /// The stored spelling (`attempts.quota_state`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Why an outcome happened, in a closed vocabulary (`attempts.category`,
/// `delivery_events.category`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = EvidenceCategory, rename_all = "snake_case")]
pub enum Category {
    /// Taken by the provider.
    Accepted,
    /// The recipient's address does not exist or is refused as an address (`X.1.x`).
    InvalidRecipient,
    /// The recipient's mailbox is full (`X.2.2`).
    MailboxFull,
    /// The recipient's domain has no mail route (a null MX, no MX nor address record, no
    /// domain), or routing to it was refused.
    NoRoute,
    /// The address is not an address.
    InvalidAddress,
    /// The message itself was refused: size, content, protocol, its From address.
    ContentRejected,
    /// Refused by security or policy (`X.7.x`), naming the account or its IP.
    Policy,
    /// The provider asked to slow down.
    Throttled,
    /// The credential was refused.
    Unauthorized,
    /// The account was refused by policy or permission.
    Forbidden,
    /// The server could not be reached or stopped answering.
    ConnectionFailed,
    /// The submission's own budget ended it before anything could be lost.
    Deadline,
    /// The message cannot be sent through this server as composed.
    Unsupported,
    /// Any other refusal for now.
    Transient,
    /// Any other refusal for good.
    Rejected,
    /// The final reply was not read.
    Uncertain,
    /// The next hop or the recipient's server took it (a later report).
    Delivered,
    /// A recipient reported the message as unwanted.
    Complaint,
    /// A recipient asked to receive no more mail.
    Unsubscribed,
    /// A report names a new address for the recipient (`5.1.6`).
    AddressChanged,
    /// Its deadline passed before it could be submitted.
    Expired,
    /// A recipient is suppressed.
    Suppressed,
    /// Its sender was archived.
    SenderArchived,
    /// Its content could not be prepared.
    RenderFailed,
    /// Its workspace was deleted.
    WorkspaceDeleted,
}

impl Category {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Why `answer` happened, in the closed vocabulary: the cause first (a throttle, a refused
/// credential or account, our own deadline, an unsupported message), then a lost final reply,
/// then the enhanced status's subject, then the class.
#[must_use]
pub fn category(answer: &Answer) -> Category {
    let refusal = match answer {
        Answer::Accepted => return Category::Accepted,
        Answer::Refused(refusal) => refusal,
    };
    match refusal.cause {
        Cause::Throttled => return Category::Throttled,
        Cause::Unauthorized => return Category::Unauthorized,
        Cause::Forbidden => return Category::Forbidden,
        Cause::Deadline => return Category::Deadline,
        Cause::Unsupported => return Category::Unsupported,
        Cause::NoReply | Cause::Refused => {}
    }
    if refusal.failure == Failure::Uncertain {
        return Category::Uncertain;
    }
    if refusal.cause == Cause::NoReply || matches!(refusal.phase, Phase::Connect | Phase::Auth) {
        return Category::ConnectionFailed;
    }
    let at_recipient = refusal.scope == RefusalScope::Recipient;
    match refusal.status.map(|status| (status.subject, status.detail)) {
        Some((2, 2)) => Category::MailboxFull,
        Some((7, _)) => Category::Policy,
        Some((1, _)) if at_recipient => Category::InvalidRecipient,
        Some((4, _)) if at_recipient => Category::NoRoute,
        Some((1, _) | (5 | 6, _) | (3, 4) | (2, 3)) => Category::ContentRejected,
        _ => match refusal.failure {
            Failure::Transient => Category::Transient,
            Failure::Permanent | Failure::Uncertain => Category::Rejected,
        },
    }
}

/// A message's next step after an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Next {
    /// The message's new state.
    pub state: MessageState,
    /// The attempt's outcome.
    pub outcome: Outcome,
    /// What happens to the reservation.
    pub quota: Quota,
    /// When a re-queued message is due again; `None` for any other state.
    pub retry_at: Option<Timestamp>,
    /// True when the message failed because its deadline left no room for another attempt.
    pub expired: bool,
}

/// The message's next step after `answer`, for a message whose earlier attempts include
/// `prior_transients` transient ones, with its `deadline_at` (`None` before a first submission),
/// at `now`, with `draw` for the backoff's jitter (see the module).
#[must_use]
pub fn after_submission(
    answer: &Answer,
    prior_transients: u32,
    deadline_at: Option<Timestamp>,
    now: Timestamp,
    draw: u64,
) -> Next {
    let next = |state, outcome, quota| Next {
        state,
        outcome,
        quota,
        retry_at: None,
        expired: false,
    };
    let refusal = match answer {
        Answer::Accepted => {
            return next(MessageState::Sent, Outcome::Accepted, Quota::Consumed);
        }
        Answer::Refused(refusal) => refusal,
    };
    let full = refusal.status.is_some_and(Enhanced::mailbox_full);
    match refusal.failure {
        Failure::Uncertain => next(MessageState::Uncertain, Outcome::Uncertain, Quota::Consumed),
        Failure::Permanent if !full => {
            next(MessageState::Failed, Outcome::Permanent, Quota::Released)
        }
        Failure::Permanent | Failure::Transient => {
            let backoff =
                SignedDuration::try_from(retry::backoff(prior_transients, &retry::DELIVERY, draw))
                    .unwrap_or(SignedDuration::MAX);
            let not_before = refusal
                .retry_after
                .filter(|wait| *wait > now)
                .unwrap_or_else(|| now.saturating_add(backoff).unwrap_or(Timestamp::MAX));
            if deadline_at.is_some_and(|deadline| not_before >= deadline) {
                Next {
                    expired: true,
                    ..next(MessageState::Failed, Outcome::Transient, Quota::Released)
                }
            } else {
                Next {
                    retry_at: Some(not_before),
                    ..next(MessageState::Queued, Outcome::Transient, Quota::Released)
                }
            }
        }
    }
}

/// The next step of a message whose lease was lost: with the submission marker set the phase it
/// reached is unknown, so it is `uncertain` and its reservation consumed; without it nothing was
/// handed to a socket, so it is queued again and its reservation released.
#[must_use]
pub fn after_lost_lease(marked: bool) -> Next {
    if marked {
        Next {
            state: MessageState::Uncertain,
            outcome: Outcome::Uncertain,
            quota: Quota::Consumed,
            retry_at: None,
            expired: false,
        }
    } else {
        Next {
            state: MessageState::Queued,
            outcome: Outcome::Released,
            quota: Quota::Released,
            retry_at: None,
            expired: false,
        }
    }
}

/// A breaker's probe: the queue row (its message) and the lease generation admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    /// The message whose queue row is the probe.
    pub message: Uuid,
    /// The lease generation it was admitted in.
    pub generation: i64,
}

/// A connection's or a quota scope's breaker, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breaker {
    /// Scoped failures since the last success (`consecutive_failures`).
    pub failures: i32,
    /// Open until then (`paused_until`).
    pub paused_until: Option<Timestamp>,
    /// When it last opened (`breaker_opened_at`): only a submission started after it closes it.
    pub opened_at: Option<Timestamp>,
    /// The probe admitted while half-open.
    pub probe: Option<Probe>,
}

/// Where a breaker stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum BreakerState {
    /// Submissions flow.
    Closed,
    /// No submission until the pause ends.
    Open,
    /// The pause ended after failures: one probe at a time.
    HalfOpen,
}

impl Breaker {
    /// The breaker's state at `now`.
    #[must_use]
    pub fn state(&self, now: Timestamp) -> BreakerState {
        match self.paused_until {
            Some(until) if until > now => BreakerState::Open,
            Some(_) if self.failures > 0 => BreakerState::HalfOpen,
            Some(_) | None => BreakerState::Closed,
        }
    }
}

/// What a submission's answer means to one breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// The breaker's key worked: `probe` when this submission was admitted as its probe,
    /// `started` the instant its submission started.
    Success {
        probe: Option<Probe>,
        started: Option<Timestamp>,
    },
    /// A failure scoped to the breaker's key; a throttle carries the provider's wait, if any.
    Failure {
        throttled: bool,
        wait: Option<Timestamp>,
    },
}

/// What to write on a breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerChange {
    /// Nothing.
    Unchanged,
    /// The count back to zero (a success while closed).
    Reset,
    /// One more failure, still closed.
    Count { failures: i32 },
    /// Opened, or opened again: paused until `until`, opened at the change's `now`, its probe
    /// cleared.
    Open { failures: i32, until: Timestamp },
    /// Closed by its probe: count, pause, opening and probe cleared.
    Close,
}

/// The change `health` makes to `breaker` at `now`, with `draw` for the jitter of the step (see
/// the module). The third failure opens it, a throttle opens it at once; while it is not closed
/// only its own probe's success, started after the latest opening, closes it.
#[must_use]
pub fn breaker_after(
    breaker: &Breaker,
    health: Health,
    now: Timestamp,
    draw: u64,
) -> BreakerChange {
    match health {
        Health::Success { probe, started } => {
            let is_probe = probe.is_some() && probe == breaker.probe;
            let after_opening = match (breaker.opened_at, started) {
                (Some(opened), Some(started)) => started > opened,
                _ => false,
            };
            if is_probe && after_opening {
                BreakerChange::Close
            } else if breaker.opened_at.is_none() && breaker.failures > 0 {
                BreakerChange::Reset
            } else {
                BreakerChange::Unchanged
            }
        }
        Health::Failure { throttled, wait } => {
            let failures = breaker.failures.saturating_add(1);
            let step = u32::try_from(failures.saturating_sub(3)).unwrap_or(0);
            let backoff = SignedDuration::try_from(retry::backoff(step, &retry::DELIVERY, draw))
                .unwrap_or(SignedDuration::MAX);
            let stepped = now.saturating_add(backoff).unwrap_or(Timestamp::MAX);
            if throttled {
                BreakerChange::Open {
                    failures,
                    until: wait.filter(|wait| *wait > now).unwrap_or(stepped),
                }
            } else if failures >= 3 {
                BreakerChange::Open {
                    failures,
                    until: stepped,
                }
            } else {
                BreakerChange::Count { failures }
            }
        }
    }
}

/// What a claim may take from a connection, given its breaker and its quota scope's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Both closed: whatever the budgets allow.
    Free,
    /// One of them half-open with no live probe: exactly one message, recorded as the probe on
    /// the breakers marked here.
    Probe { connection: bool, scope: bool },
    /// Open, or half-open with a live probe: nothing.
    Nothing,
}

/// What a claim may take at `now`: `connection` and `scope` with whether each one's probe is
/// still live (its queue row leased in its generation).
#[must_use]
pub fn admission(
    connection: (&Breaker, bool),
    scope: Option<(&Breaker, bool)>,
    now: Timestamp,
) -> Admission {
    let wants = |(breaker, live): (&Breaker, bool)| match breaker.state(now) {
        BreakerState::Closed => Some(false),
        BreakerState::Open => None,
        BreakerState::HalfOpen if live => None,
        BreakerState::HalfOpen => Some(true),
    };
    let Some(connection) = wants(connection) else {
        return Admission::Nothing;
    };
    let scope = match scope.map(wants) {
        None => false,
        Some(None) => return Admission::Nothing,
        Some(Some(probe)) => probe,
    };
    if connection || scope {
        Admission::Probe { connection, scope }
    } else {
        Admission::Free
    }
}

/// What an answer does to the connection that submitted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionEffect {
    /// Nothing: the answer concerns a recipient, the message, a quota scope or the platform.
    None,
    /// The credential worked.
    Success,
    /// A failure of the credential: counted by the breaker, a throttle opens it at once.
    Failure {
        throttled: bool,
        wait: Option<Timestamp>,
    },
    /// A policy rejection naming the account (`5.7.x`): counted as a failure, and the third in a
    /// row disables the connection.
    Policy,
    /// The credential was refused: `authorization_required` at once.
    CredentialLost,
    /// The account was refused by policy or permission: `disabled` at once.
    AccountBlocked,
}

/// What `answer` does to the connection (see the module).
#[must_use]
pub fn connection_effect(answer: &Answer) -> ConnectionEffect {
    let refusal = match answer {
        Answer::Accepted => return ConnectionEffect::Success,
        Answer::Refused(refusal) => refusal,
    };
    if refusal.scope != RefusalScope::Connection {
        return ConnectionEffect::None;
    }
    match refusal.cause {
        Cause::Unauthorized => ConnectionEffect::CredentialLost,
        Cause::Forbidden => ConnectionEffect::AccountBlocked,
        Cause::Throttled => ConnectionEffect::Failure {
            throttled: true,
            wait: refusal.retry_after,
        },
        Cause::Refused
            if refusal.failure == Failure::Permanent
                && refusal.status.is_some_and(|status| status.subject == 7) =>
        {
            ConnectionEffect::Policy
        }
        Cause::Refused | Cause::NoReply | Cause::Deadline | Cause::Unsupported => {
            ConnectionEffect::Failure {
                throttled: false,
                wait: None,
            }
        }
    }
}

/// What an answer does to the quota scope of the connection that submitted it: `None` unless it
/// succeeded or concerned the scope.
#[must_use]
pub fn scope_effect(answer: &Answer) -> Option<Health> {
    match answer {
        Answer::Accepted => Some(Health::Success {
            probe: None,
            started: None,
        }),
        Answer::Refused(refusal) if refusal.scope == RefusalScope::QuotaScope => {
            Some(Health::Failure {
                throttled: refusal.cause == Cause::Throttled,
                wait: refusal.retry_after,
            })
        }
        Answer::Refused(_) => None,
    }
}

/// How long this replica stops submitting through a platform limit (Norbelys's own Google project
/// or Microsoft app) after `answer`: until the provider's wait, else the `DELIVERY` step of the
/// `consecutive` platform throttles seen before; `None` when the answer concerns no platform
/// limit.
#[must_use]
pub fn platform_backoff(
    answer: &Answer,
    consecutive: u32,
    now: Timestamp,
    draw: u64,
) -> Option<Timestamp> {
    let Answer::Refused(refusal) = answer else {
        return None;
    };
    if refusal.scope != RefusalScope::Platform {
        return None;
    }
    let backoff = SignedDuration::try_from(retry::backoff(consecutive, &retry::DELIVERY, draw))
        .unwrap_or(SignedDuration::MAX);
    Some(
        refusal
            .retry_after
            .filter(|wait| *wait > now)
            .unwrap_or_else(|| now.saturating_add(backoff).unwrap_or(Timestamp::MAX)),
    )
}

/// True when the last three outcomes of a connection, oldest first, are all policy rejections:
/// the connection is then disabled.
#[must_use]
pub fn policy_streak(recent: &[Category]) -> bool {
    recent.len() >= 3
        && recent
            .iter()
            .rev()
            .take(3)
            .all(|category| *category == Category::Policy)
}

/// What kind of observation a delivery event is (`delivery_events.kind`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumIter,
    strum::IntoStaticStr,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
#[schema(as = DeliveryEventKind)]
pub enum EventKind {
    /// The provider took the message.
    Accepted,
    /// Delivery is delayed and will be tried again by the provider.
    Deferred,
    /// The next hop or the recipient's server took it.
    Delivered,
    /// It came back after acceptance.
    Bounced,
    /// It was refused before acceptance.
    Rejected,
    /// A recipient reported it as unwanted.
    Complaint,
    /// A recipient unsubscribed.
    Unsubscribed,
    /// A report names a new address.
    AddressChanged,
    /// Any other report about it.
    Reported,
}

impl EventKind {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// How much a delivery event can be trusted (`delivery_events.confidence`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter, strum::IntoStaticStr, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = EvidenceConfidence, rename_all = "snake_case")]
pub enum Confidence {
    /// The reporter is the party we talked to, or signed, or verified.
    Authenticated,
    /// The report matches our records, but the reporter is not authenticated.
    Corroborated,
    /// A partial match: kept for review and counters only.
    Inferred,
    /// A notice a person wrote: routed to review, never automatic.
    HumanText,
}

impl Confidence {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Why an event's recipient is known (`delivery_events.recipient_ref`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter, strum::IntoStaticStr, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = RecipientRef, rename_all = "snake_case")]
pub enum RecipientRef {
    /// The evidence names it.
    Named,
    /// The envelope had one recipient.
    SingleEnvelope,
    /// It is not known.
    Unknown,
}

impl RecipientRef {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Why an address is held (`recipient_holds.reason`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter, strum::IntoStaticStr, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = HoldReason, rename_all = "snake_case")]
pub enum HoldReason {
    /// Its mailbox is full.
    MailboxFull,
    /// Its server asked to come back later.
    Greylisted,
    /// Its domain has no mail route.
    NoRoute,
    /// A report that the address does not exist matched our records without coming from the
    /// server we talked to (a corroborated `5.1.x`): no mail goes to it while a person reviews
    /// the report.
    InvalidRecipient,
}

impl HoldReason {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// How long a hold lasts before mail to the address is tried again: a full mailbox and a
    /// greylisting clear within hours, a domain's mail records are re-checked after a day, and a
    /// reported invalid address waits a week for the person reviewing it (confirming the review
    /// suppresses it for good; a review left undecided lets mail try it again after the week).
    #[must_use]
    pub fn review_after(self) -> SignedDuration {
        match self {
            Self::MailboxFull => SignedDuration::from_hours(6),
            Self::Greylisted => SignedDuration::from_hours(1),
            Self::NoRoute => SignedDuration::from_hours(24),
            Self::InvalidRecipient => SignedDuration::from_hours(7 * 24),
        }
    }
}

/// What a person is asked to decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proposal {
    /// Suppress the address, for this reason.
    Suppress(SuppressionReason),
    /// Move the person to the new address the report names.
    AddressChange,
}

/// What evidence does to its recipient's address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipientEffect {
    /// Nothing (counters only).
    None,
    /// Suppress the address, irreversibly unless a person removes it.
    Suppress(SuppressionReason),
    /// Hold the address for a while.
    Hold(HoldReason),
    /// Ask a person.
    Review(Proposal),
    /// Hold the address while a person decides what the evidence proposes.
    HoldAndReview(HoldReason, Proposal),
}

/// A workspace's delivery settings, `workspaces.settings.delivery`. Every field has a default, so
/// an absent or partial object is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeliverySettings {
    /// "Trust own-inbox DSNs": a corroborated report that a recipient's address does not exist
    /// (`5.1.x` naming the recipient, typically a delivery status notification that came back to
    /// the sending mailbox's own inbox about a message whose Message-ID tag verifies) suppresses
    /// the address as authenticated evidence does, instead of holding it while a person reviews
    /// it. Off by default: such a report proves the message was ours, not who wrote the report,
    /// and a forged one could otherwise remove a good address for good.
    pub trust_own_inbox_dsns: bool,
}

/// One invalid field of `settings.delivery`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError {
    /// The field's path under `settings.delivery`, as an RFC 6901 pointer
    /// (`/trust_own_inbox_dsns`); empty for the object itself.
    pub pointer: String,
    /// What is wrong, for a person to read. It never repeats the value.
    pub detail: &'static str,
}

impl DeliverySettings {
    /// Reads `settings.delivery` as a workspace stores or sends it: an object whose fields are
    /// all optional; an absent value (`None` or `null`) is the defaults. Every invalid field is
    /// reported, unknown fields included, so a misspelt setting is refused rather than ignored.
    ///
    /// # Errors
    ///
    /// Every invalid field, in one list.
    pub fn parse(value: Option<&Value>) -> Result<Self, Vec<SettingsError>> {
        let mut settings = Self::default();
        let object = match value {
            None | Some(Value::Null) => return Ok(settings),
            Some(Value::Object(object)) => object,
            Some(_) => {
                return Err(vec![SettingsError {
                    pointer: String::new(),
                    detail: "the delivery settings are an object",
                }]);
            }
        };
        let mut errors = Vec::new();
        for (name, field) in object {
            let pointer = format!("/{}", name.replace('~', "~0").replace('/', "~1"));
            match (name.as_str(), field.as_bool()) {
                ("trust_own_inbox_dsns", Some(trust)) => settings.trust_own_inbox_dsns = trust,
                ("trust_own_inbox_dsns", None) => errors.push(SettingsError {
                    pointer,
                    detail: "`trust_own_inbox_dsns` is true or false",
                }),
                _ => errors.push(SettingsError {
                    pointer,
                    detail: "not a delivery setting",
                }),
            }
        }
        if errors.is_empty() {
            Ok(settings)
        } else {
            Err(errors)
        }
    }
}

/// What evidence of `category` (with `kind`), naming its recipient by `recipient`, at
/// `confidence`, does to the recipient's address in a workspace with `settings` (see the
/// module).
#[must_use]
pub fn recipient_effect(
    kind: EventKind,
    category: Category,
    recipient: RecipientRef,
    confidence: Confidence,
    settings: DeliverySettings,
) -> RecipientEffect {
    let named = recipient == RecipientRef::Named;
    let known = recipient != RecipientRef::Unknown;
    let proven = named && confidence == Confidence::Authenticated;
    let corroborated = confidence == Confidence::Corroborated;
    match category {
        Category::InvalidRecipient if matches!(kind, EventKind::Bounced | EventKind::Rejected) => {
            if proven || (named && corroborated && settings.trust_own_inbox_dsns) {
                RecipientEffect::Suppress(SuppressionReason::Bounce)
            } else if known && corroborated {
                RecipientEffect::HoldAndReview(
                    HoldReason::InvalidRecipient,
                    Proposal::Suppress(SuppressionReason::Bounce),
                )
            } else {
                RecipientEffect::Review(Proposal::Suppress(SuppressionReason::Bounce))
            }
        }
        Category::MailboxFull if known => RecipientEffect::Hold(HoldReason::MailboxFull),
        Category::NoRoute if known => RecipientEffect::Hold(HoldReason::NoRoute),
        Category::Complaint if proven => RecipientEffect::Suppress(SuppressionReason::Complaint),
        Category::Complaint => {
            RecipientEffect::Review(Proposal::Suppress(SuppressionReason::Complaint))
        }
        Category::Unsubscribed if named => {
            RecipientEffect::Suppress(SuppressionReason::Unsubscribe)
        }
        Category::AddressChanged => RecipientEffect::Review(Proposal::AddressChange),
        Category::InvalidRecipient
        | Category::MailboxFull
        | Category::NoRoute
        | Category::Unsubscribed
        | Category::Accepted
        | Category::InvalidAddress
        | Category::ContentRejected
        | Category::Policy
        | Category::Throttled
        | Category::Unauthorized
        | Category::Forbidden
        | Category::ConnectionFailed
        | Category::Deadline
        | Category::Unsupported
        | Category::Transient
        | Category::Rejected
        | Category::Uncertain
        | Category::Delivered
        | Category::Expired
        | Category::Suppressed
        | Category::SenderArchived
        | Category::RenderFailed
        | Category::WorkspaceDeleted => RecipientEffect::None,
    }
}

/// What preflight found decides for the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    /// Every address has a route: submit.
    Proceed {
        /// Addresses without a mail route among others that have one: they are held (the
        /// provider refuses them alone).
        unroutable: Vec<usize>,
    },
    /// DNS did not answer for an address: try again later, nothing submitted.
    Defer,
    /// The message cannot reach anyone: an address is not an address, or no address has a
    /// route. `unroutable` are the addresses to hold.
    Fail {
        category: Category,
        unroutable: Vec<usize>,
    },
}

/// The decision over the preflight `reasons` of a message's envelope addresses, in envelope
/// order. An address that is not an address fails the message (no envelope can carry it); a
/// domain that accepts no mail (a null MX, RFC 7505, which asks senders not to queue such mail),
/// has no route or does not exist holds that address, and fails the message only when no address
/// is left to reach; a lookup DNS could not answer defers the message, so a resolver outage
/// refuses nobody.
#[must_use]
pub fn after_preflight(reasons: &[Reason]) -> Preflight {
    if reasons.contains(&Reason::Syntax) {
        return Preflight::Fail {
            category: Category::InvalidAddress,
            unroutable: Vec::new(),
        };
    }
    if reasons.contains(&Reason::DnsUnavailable) {
        return Preflight::Defer;
    }
    let unroutable: Vec<usize> = reasons
        .iter()
        .enumerate()
        .filter(|(_, reason)| matches!(reason, Reason::NullMx | Reason::NoRoute | Reason::NoDomain))
        .map(|(index, _)| index)
        .collect();
    if !reasons.is_empty() && unroutable.len() == reasons.len() {
        Preflight::Fail {
            category: Category::NoRoute,
            unroutable,
        }
    } else {
        Preflight::Proceed { unroutable }
    }
}

/// The most envelope recipients (`To`, `Cc` and `Bcc` together) one message may have through a
/// connection of `provider` that submits to `smtp_host`, checked when the message is accepted
/// for the connection, so a message its provider would refuse whole is refused at once, naming
/// the limit. The providers' own limits (read 2026-10-01): Gmail takes 100 recipients a message
/// over SMTP (`smtp.gmail.com`, `smtp-relay.gmail.com`, `smtp.googlemail.com`: a password
/// connection to a Gmail mailbox) and 500 through its API; Amazon SES 50, not adjustable; the
/// others are held to the product's own envelope limit, [`RECIPIENTS_MAX`], which also caps the
/// larger provider limits.
#[must_use]
pub fn recipients_max(provider: Provider, smtp_host: Option<&str>) -> usize {
    let gmail_smtp = smtp_host.is_some_and(|host| {
        matches!(
            host.trim()
                .trim_end_matches('.')
                .to_ascii_lowercase()
                .as_str(),
            "smtp.gmail.com" | "smtp-relay.gmail.com" | "smtp.googlemail.com"
        )
    });
    let provider_max = match provider {
        Provider::Google => 500,
        Provider::Ses => 50,
        Provider::Smtp if gmail_smtp => 100,
        Provider::Smtp
        | Provider::Microsoft
        | Provider::Sendgrid
        | Provider::Mailgun
        | Provider::Norbelys => RECIPIENTS_MAX,
    };
    provider_max.min(RECIPIENTS_MAX)
}

/// How far back a connection's complaint rate looks: the last seven days, long enough for a
/// small sender's rate to mean something, short enough that last month's list does not count.
pub const COMPLAINT_WINDOW: SignedDuration = SignedDuration::from_hours(7 * 24);

/// The fewest complaints in the window that can stop a connection: below it, one or two
/// complaints against a small sender's few messages would cross any rate.
pub const COMPLAINTS_MIN: i64 = 3;

/// The complaint-rate breaker: whether `complaints` (abuse reports and provider complaints about
/// the connection's messages, recorded in the window) against the `sent` messages it had accepted
/// in the window reach 0.3 %, with at least [`COMPLAINTS_MIN`] of them. 0.3 % is the spam rate
/// Gmail and Yahoo tell bulk senders never to reach (Google's email sender guidelines,
/// <https://support.google.com/a/answer/81126>; Amazon SES reviews an account at 0.1 % and may
/// pause it at 0.5 %); the complaints Norbelys sees are a part of all the complaints made, so
/// reaching it is reason enough to stop the connection until a person looks at its list and its
/// content.
#[must_use]
pub fn complaint_rate_exceeded(complaints: i64, sent: i64) -> bool {
    complaints >= COMPLAINTS_MIN
        && complaints.saturating_mul(1_000) >= sent.max(0).saturating_mul(3)
}

/// What the Start found under its locks, immediately before a submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartFacts {
    /// The Start's own clock.
    pub now: Timestamp,
    /// The message's deadline, once a first submission fixed it.
    pub deadline_at: Option<Timestamp>,
    /// The message's kind.
    pub kind: MessageKind,
    /// Its workspace was deleted.
    pub workspace_deleted: bool,
    /// Its connection is archived.
    pub connection_archived: bool,
    /// Its connection is `active` and not paused by its person.
    pub connection_usable: bool,
    /// An envelope address is suppressed.
    pub suppressed: bool,
    /// An envelope address is held by another message's hold, until then.
    pub held_until: Option<Timestamp>,
    /// Its sender identity is enabled and not archived.
    pub identity_usable: bool,
    /// Campaign mail: its campaign is active, its enrollment active, its identity still in the
    /// campaign's pool. Always true for other mail.
    pub campaign_usable: bool,
    /// Campaign mail: the campaign's and the connection's windows are open. Always true for
    /// other mail, which windows never hold.
    pub windows_open: bool,
    /// Cold mail on a paced sender: its clock is still due. Always true otherwise.
    pub clock_due: bool,
    /// The connection's or its scope's breaker is open, or half-open with another message as its
    /// probe.
    pub breaker_blocks: bool,
}

/// Why a Start returned its message to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Held {
    /// The connection is not active, or paused.
    ConnectionUnavailable,
    /// A breaker holds the connection.
    Breaker,
    /// A recipient is held.
    #[strum(serialize = "recipient_held")]
    Recipient,
    /// The identity is disabled.
    IdentityUnavailable,
    /// The campaign, the enrollment or the pool no longer allow it.
    CampaignUnavailable,
    /// A send window closed.
    WindowClosed,
    /// The pacing clock is not due.
    ClockNotDue,
}

/// What the Start decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartVerdict {
    /// Every check passed: renew the lease, mark the submission and submit.
    Submit,
    /// Return the message to the queue unstarted, due at `run_at` (unchanged when `None`); its
    /// reservation is released and the connection's budget wait cleared.
    Return {
        run_at: Option<Timestamp>,
        why: Held,
    },
    /// The message ends here without a submission.
    End {
        state: MessageState,
        category: Category,
    },
}

/// The Start's final checks over `facts`, in order: an expired deadline or a deleted workspace
/// ends the message; a suppressed recipient suppresses it, except the platform's transactional mail (a
/// sign-in code still reaches someone who once refused other mail); a direct, reply or transactional
/// message whose connection was archived fails (its caller chose that sender, so no other is
/// substituted); anything else that does not hold now returns it to the queue. A condition the
/// claim already filters (the connection's status, breakers, windows, the clock) keeps the
/// message's due time; one it does not filter (a recipient's hold, a disabled identity, a stopped
/// enrollment) looks again at the hold's end or a slot later, never at every sweep.
#[must_use]
pub fn start_verdict(facts: &StartFacts) -> StartVerdict {
    let returned = |why, run_at| StartVerdict::Return { run_at, why };
    let later = facts.now.saturating_add(RECHECK).unwrap_or(Timestamp::MAX);
    if facts
        .deadline_at
        .is_some_and(|deadline| deadline <= facts.now)
    {
        return StartVerdict::End {
            state: MessageState::Failed,
            category: Category::Expired,
        };
    }
    if facts.workspace_deleted {
        return StartVerdict::End {
            state: MessageState::Failed,
            category: Category::WorkspaceDeleted,
        };
    }
    if facts.suppressed && facts.kind != MessageKind::Transactional {
        return StartVerdict::End {
            state: MessageState::Suppressed,
            category: Category::Suppressed,
        };
    }
    if facts.connection_archived && facts.kind != MessageKind::Campaign {
        return StartVerdict::End {
            state: MessageState::Failed,
            category: Category::SenderArchived,
        };
    }
    if !facts.connection_usable {
        return returned(Held::ConnectionUnavailable, None);
    }
    if facts.breaker_blocks {
        return returned(Held::Breaker, None);
    }
    if let Some(until) = facts.held_until {
        return returned(Held::Recipient, Some(until.max(facts.now)));
    }
    if !facts.identity_usable {
        return returned(Held::IdentityUnavailable, Some(later));
    }
    if !facts.campaign_usable {
        return returned(Held::CampaignUnavailable, Some(later));
    }
    if !facts.windows_open {
        return returned(Held::WindowClosed, None);
    }
    if !facts.clock_due {
        return returned(Held::ClockNotDue, None);
    }
    StartVerdict::Submit
}

/// Where a delivery event came from (`delivery_events.source`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter, strum::IntoStaticStr, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = EvidenceSource, rename_all = "snake_case")]
pub enum Source {
    /// The SMTP session's own reply.
    Smtp,
    /// The Gmail API's or Microsoft Graph's own answer.
    ProviderApi,
    /// A provider's signed callback.
    ProviderWebhook,
    /// A delivery status notification (RFC 3464).
    Dsn,
    /// An abuse report (RFC 5965).
    Arf,
    /// A notice a person wrote.
    InboundNotice,
    /// Our own check of an address before submission.
    Preflight,
    /// The recipient's own unsubscribe.
    Unsubscribe,
    /// A person's decision.
    Manual,
    /// Our own read of the mailbox's Sent folder, which found the message.
    SentFolder,
}

impl Source {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// True for the answer read during the submission itself, whose outcome the attempt already
    /// records and the message's state already says.
    #[must_use]
    pub fn in_session(self) -> bool {
        match self {
            Self::Smtp | Self::ProviderApi => true,
            Self::ProviderWebhook
            | Self::Dsn
            | Self::Arf
            | Self::InboundNotice
            | Self::Preflight
            | Self::Unsubscribe
            | Self::Manual
            | Self::SentFolder => false,
        }
    }
}

/// What evidence does to the state of the message it names: an `uncertain` message is `sent`
/// once trustworthy evidence shows the provider took it (its acceptance, a delivery, a deferral, a
/// bounce, a complaint or an unsubscribe all come after acceptance), from a party we
/// authenticated or a report that matches our records; a person's decision may settle it either
/// way. No other state moves on evidence: a sent message stays sent whatever happens to it later,
/// and its fate is in its events.
#[must_use]
pub fn after_evidence(
    state: MessageState,
    kind: EventKind,
    source: Source,
    confidence: Confidence,
) -> Option<MessageState> {
    if state != MessageState::Uncertain {
        return None;
    }
    let trusted = matches!(
        confidence,
        Confidence::Authenticated | Confidence::Corroborated
    );
    match kind {
        EventKind::Accepted
        | EventKind::Delivered
        | EventKind::Deferred
        | EventKind::Bounced
        | EventKind::Complaint
        | EventKind::Unsubscribed
            if trusted =>
        {
            Some(MessageState::Sent)
        }
        EventKind::Rejected if source == Source::Manual && trusted => Some(MessageState::Failed),
        EventKind::Accepted
        | EventKind::Delivered
        | EventKind::Deferred
        | EventKind::Bounced
        | EventKind::Rejected
        | EventKind::Complaint
        | EventKind::Unsubscribed
        | EventKind::AddressChanged
        | EventKind::Reported => None,
    }
}

/// A campaign counter (`stats_increments.metric`) the delivery engine writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::EnumIter, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Metric {
    /// A provider accepted the message.
    Sent,
    /// The recipient's server took it.
    Delivered,
    /// It could not reach its recipient for good.
    Bounced,
    /// The recipient unsubscribed.
    Unsubscribed,
    /// The recipient complained.
    Complained,
    /// A person answered it (written by the inbox when it correlates a reply).
    Replied,
}

impl Metric {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The campaign counter an event moves: a delivery, a recipient refused or bounced for good (an
/// unknown address, no route, any other permanent refusal of its recipient), an unsubscribe or a
/// complaint. Acceptance is counted where the message's state becomes `sent`, never from its
/// events, so it is counted once.
#[must_use]
pub fn evidence_metric(kind: EventKind, category: Category) -> Option<Metric> {
    match kind {
        EventKind::Delivered => Some(Metric::Delivered),
        EventKind::Bounced | EventKind::Rejected => matches!(
            category,
            Category::InvalidRecipient | Category::NoRoute | Category::Rejected
        )
        .then_some(Metric::Bounced),
        EventKind::Unsubscribed => Some(Metric::Unsubscribed),
        EventKind::Complaint => Some(Metric::Complained),
        EventKind::Accepted
        | EventKind::Deferred
        | EventKind::AddressChanged
        | EventKind::Reported => None,
    }
}

#[cfg(test)]
mod tests {
    use jiff::{SignedDuration, Timestamp};
    use strum::IntoEnumIterator as _;
    use uuid::Uuid;

    use super::{
        Admission, Answer, Breaker, BreakerChange, BreakerState, Category, Cause, Confidence,
        ConnectionEffect, DeliverySettings, Enhanced, EventKind, Failure, Health, Held, HoldReason,
        MessageKind, MessageState, Metric, Next, Outcome, Phase, Preflight, Probe, Proposal,
        Provider, Quota, RecipientEffect, RecipientRef, Refusal, RefusalScope, Source, StartFacts,
        StartVerdict, SuppressionReason, admission, after_evidence, after_lost_lease,
        after_preflight, after_submission, breaker_after, category, complaint_rate_exceeded,
        connection_effect, evidence_metric, platform_backoff, policy_streak, recipient_effect,
        recipients_max, scope_effect, start_verdict,
    };

    /// Evidence moves only an `uncertain` message: trustworthy evidence that comes after
    /// acceptance sends it, a person's refusal fails it, and nothing else moves any state; over
    /// every state, kind, source and confidence.
    #[test]
    fn evidence_settles_only_uncertain_messages() {
        for state in MessageState::iter() {
            for kind in EventKind::iter() {
                for source in Source::iter() {
                    for confidence in Confidence::iter() {
                        let trusted = matches!(
                            confidence,
                            Confidence::Authenticated | Confidence::Corroborated
                        );
                        let after_acceptance = matches!(
                            kind,
                            EventKind::Accepted
                                | EventKind::Delivered
                                | EventKind::Deferred
                                | EventKind::Bounced
                                | EventKind::Complaint
                                | EventKind::Unsubscribed
                        );
                        let expected = match state {
                            MessageState::Uncertain if trusted && after_acceptance => {
                                Some(MessageState::Sent)
                            }
                            MessageState::Uncertain
                                if trusted
                                    && kind == EventKind::Rejected
                                    && source == Source::Manual =>
                            {
                                Some(MessageState::Failed)
                            }
                            _ => None,
                        };
                        assert_eq!(after_evidence(state, kind, source, confidence), expected);
                    }
                }
            }
        }
        let sources: Vec<&str> = Source::iter().map(Source::as_str).collect();
        assert_eq!(
            sources,
            [
                "smtp",
                "provider_api",
                "provider_webhook",
                "dsn",
                "arf",
                "inbound_notice",
                "preflight",
                "unsubscribe",
                "manual",
                "sent_folder"
            ]
        );
        let in_session: Vec<Source> = Source::iter()
            .filter(|source| source.in_session())
            .collect();
        assert_eq!(in_session, [Source::Smtp, Source::ProviderApi]);
    }

    /// The counters events move, over every kind and category: deliveries, recipients refused or
    /// bounced for good, unsubscribes and complaints; never acceptance, which is counted once with
    /// the `sent` state.
    #[test]
    fn events_move_their_counters() {
        for kind in EventKind::iter() {
            for category in Category::iter() {
                let permanent = matches!(
                    category,
                    Category::InvalidRecipient | Category::NoRoute | Category::Rejected
                );
                let expected = match kind {
                    EventKind::Delivered => Some(Metric::Delivered),
                    EventKind::Bounced | EventKind::Rejected if permanent => Some(Metric::Bounced),
                    EventKind::Unsubscribed => Some(Metric::Unsubscribed),
                    EventKind::Complaint => Some(Metric::Complained),
                    _ => None,
                };
                assert_eq!(
                    evidence_metric(kind, category),
                    expected,
                    "{kind:?} {category:?}"
                );
            }
        }
        let metrics: Vec<&str> = Metric::iter().map(Metric::as_str).collect();
        assert_eq!(
            metrics,
            [
                "sent",
                "delivered",
                "bounced",
                "unsubscribed",
                "complained",
                "replied"
            ]
        );
    }
    use crate::domain::preflight::Reason;
    use crate::domain::retry;

    fn now() -> Timestamp {
        "2026-10-01T10:00:00Z".parse().unwrap()
    }

    fn plus(seconds: i64) -> Timestamp {
        now()
            .checked_add(SignedDuration::from_secs(seconds))
            .unwrap()
    }

    fn status(class: u8, subject: u16, detail: u16) -> Option<Enhanced> {
        Some(Enhanced {
            class,
            subject,
            detail,
        })
    }

    fn refusal(failure: Failure, scope: RefusalScope, cause: Cause) -> Refusal {
        Refusal {
            failure,
            phase: Phase::RcptTo,
            scope,
            cause,
            code: None,
            status: None,
            retry_after: None,
        }
    }

    /// Every outcome, phase, state, category and reason keeps a stored spelling that the
    /// database's constraints accept: a new variant fails here until its spelling is decided.
    #[test]
    fn stored_spellings_match_the_schema() {
        let outcomes: Vec<&str> = Outcome::iter().map(Outcome::as_str).collect();
        assert_eq!(
            outcomes,
            [
                "accepted",
                "transient",
                "permanent",
                "uncertain",
                "released",
                "suppressed",
                "skipped"
            ]
        );
        let phases: Vec<&str> = Phase::iter().map(Phase::as_str).collect();
        assert_eq!(
            phases,
            ["connect", "auth", "mail_from", "rcpt_to", "data", "api"]
        );
        let kinds: Vec<&str> = EventKind::iter().map(EventKind::as_str).collect();
        assert_eq!(
            kinds,
            [
                "accepted",
                "deferred",
                "delivered",
                "bounced",
                "rejected",
                "complaint",
                "unsubscribed",
                "address_changed",
                "reported"
            ]
        );
        let quotas: Vec<&str> = Quota::iter().map(Quota::as_str).collect();
        assert_eq!(quotas, ["consumed", "released"]);
        let reasons: Vec<&str> = SuppressionReason::iter()
            .map(SuppressionReason::as_str)
            .collect();
        assert_eq!(
            reasons,
            [
                "unsubscribe",
                "bounce",
                "complaint",
                "manual",
                "address_changed",
                "account_closed",
                "no_mail_service"
            ]
        );
        let holds: Vec<&str> = HoldReason::iter().map(HoldReason::as_str).collect();
        assert_eq!(
            holds,
            [
                "mailbox_full",
                "greylisted",
                "no_route",
                "invalid_recipient"
            ]
        );
        for state in MessageState::iter() {
            assert_eq!(
                super::is_final(state),
                !matches!(
                    state,
                    MessageState::Queued | MessageState::Claimed | MessageState::InFlight
                ),
                "{state:?}"
            );
        }
    }

    /// Every refusal over every failure, scope and cause has a category, decided cause first:
    /// throttles, refused credentials and accounts, our own deadline and unsupported messages
    /// name themselves; then a lost final reply; then an unreachable server; then the enhanced
    /// status; then the class. Acceptance is `accepted`.
    #[test]
    fn every_answer_has_its_category() {
        assert_eq!(category(&Answer::Accepted), Category::Accepted);
        for failure in Failure::iter() {
            for scope in RefusalScope::iter() {
                for cause in Cause::iter() {
                    let answer = Answer::Refused(refusal(failure, scope, cause));
                    let expected = match (cause, failure) {
                        (Cause::Throttled, _) => Category::Throttled,
                        (Cause::Unauthorized, _) => Category::Unauthorized,
                        (Cause::Forbidden, _) => Category::Forbidden,
                        (Cause::Deadline, _) => Category::Deadline,
                        (Cause::Unsupported, _) => Category::Unsupported,
                        (_, Failure::Uncertain) => Category::Uncertain,
                        (Cause::NoReply, _) => Category::ConnectionFailed,
                        (Cause::Refused, Failure::Transient) => Category::Transient,
                        (Cause::Refused, Failure::Permanent) => Category::Rejected,
                    };
                    assert_eq!(
                        category(&answer),
                        expected,
                        "{failure:?} {scope:?} {cause:?}"
                    );
                }
            }
        }
        let with = |status, scope, phase| {
            category(&Answer::Refused(Refusal {
                status,
                phase,
                ..refusal(Failure::Permanent, scope, Cause::Refused)
            }))
        };
        assert_eq!(
            with(status(5, 1, 1), RefusalScope::Recipient, Phase::RcptTo),
            Category::InvalidRecipient
        );
        assert_eq!(
            with(status(5, 1, 7), RefusalScope::Message, Phase::MailFrom),
            Category::ContentRejected
        );
        assert_eq!(
            with(status(5, 2, 2), RefusalScope::Recipient, Phase::RcptTo),
            Category::MailboxFull
        );
        assert_eq!(
            with(status(5, 7, 1), RefusalScope::Connection, Phase::Data),
            Category::Policy
        );
        assert_eq!(
            with(status(5, 4, 4), RefusalScope::Recipient, Phase::RcptTo),
            Category::NoRoute
        );
        assert_eq!(
            with(status(5, 3, 4), RefusalScope::Message, Phase::Data),
            Category::ContentRejected
        );
        assert_eq!(
            with(status(5, 6, 0), RefusalScope::Message, Phase::Data),
            Category::ContentRejected
        );
        assert_eq!(
            with(None, RefusalScope::Connection, Phase::Connect),
            Category::ConnectionFailed
        );
    }

    /// The message's next step over every answer: acceptance sends and consumes; uncertainty
    /// consumes and is final; a permanent refusal fails and releases; a transient one re-queues at
    /// the provider's wait when it gave a later one, else at the `DELIVERY` backoff, and fails as
    /// expired when that instant reaches the deadline; a full mailbox is retried whatever its class.
    #[test]
    fn the_message_moves_by_its_answer() {
        assert_eq!(
            after_submission(&Answer::Accepted, 0, None, now(), 0),
            Next {
                state: MessageState::Sent,
                outcome: Outcome::Accepted,
                quota: Quota::Consumed,
                retry_at: None,
                expired: false,
            }
        );
        for failure in Failure::iter() {
            for scope in RefusalScope::iter() {
                for cause in Cause::iter() {
                    let answer = Answer::Refused(refusal(failure, scope, cause));
                    let next = after_submission(&answer, 0, None, now(), 0);
                    let expected = match failure {
                        Failure::Uncertain => {
                            (MessageState::Uncertain, Outcome::Uncertain, Quota::Consumed)
                        }
                        Failure::Permanent => {
                            (MessageState::Failed, Outcome::Permanent, Quota::Released)
                        }
                        Failure::Transient => {
                            (MessageState::Queued, Outcome::Transient, Quota::Released)
                        }
                    };
                    assert_eq!(
                        (next.state, next.outcome, next.quota),
                        expected,
                        "{failure:?}"
                    );
                    let floor = SignedDuration::try_from(retry::DELIVERY.floor).unwrap();
                    assert_eq!(
                        next.retry_at,
                        (failure == Failure::Transient).then(|| now().checked_add(floor).unwrap()),
                        "a transient refusal waits the backoff's floor at the smallest draw"
                    );
                }
            }
        }
        let throttled = Answer::Refused(Refusal {
            retry_after: Some(plus(600)),
            ..refusal(
                Failure::Transient,
                RefusalScope::Connection,
                Cause::Throttled,
            )
        });
        assert_eq!(
            after_submission(&throttled, 0, None, now(), 0).retry_at,
            Some(plus(600))
        );
        let stale_wait = Answer::Refused(Refusal {
            retry_after: Some(plus(-5)),
            ..refusal(
                Failure::Transient,
                RefusalScope::Connection,
                Cause::Throttled,
            )
        });
        assert_eq!(
            after_submission(&stale_wait, 0, None, now(), 0).retry_at,
            Some(plus(30))
        );
        let expired = after_submission(&throttled, 0, Some(plus(600)), now(), 0);
        assert_eq!(
            (
                expired.state,
                expired.outcome,
                expired.expired,
                expired.retry_at
            ),
            (MessageState::Failed, Outcome::Transient, true, None),
            "a wait reaching the deadline fails the message now"
        );
        assert_eq!(
            after_submission(&throttled, 0, Some(plus(601)), now(), 0).state,
            MessageState::Queued
        );
        let full = Answer::Refused(Refusal {
            status: status(5, 2, 2),
            ..refusal(Failure::Permanent, RefusalScope::Recipient, Cause::Refused)
        });
        assert_eq!(
            after_submission(&full, 2, None, now(), 0).state,
            MessageState::Queued
        );
        assert_eq!(after_lost_lease(true).state, MessageState::Uncertain);
        assert_eq!(after_lost_lease(true).quota, Quota::Consumed);
        assert_eq!(after_lost_lease(false).outcome, Outcome::Released);
        assert_eq!(after_lost_lease(false).quota, Quota::Released);
    }

    fn breaker(
        failures: i32,
        paused_until: Option<Timestamp>,
        opened_at: Option<Timestamp>,
    ) -> Breaker {
        Breaker {
            failures,
            paused_until,
            opened_at,
            probe: Some(Probe {
                message: Uuid::from_u128(7),
                generation: 2,
            }),
        }
    }

    /// The breaker's states: no pause is closed, a pause in the future is open, a pause over
    /// with failures counted is half-open, a pause over without failures is closed again.
    #[test]
    fn a_breaker_is_closed_open_or_half_open() {
        for state in BreakerState::iter() {
            let (b, expected) = match state {
                BreakerState::Closed => (breaker(2, None, None), state),
                BreakerState::Open => (breaker(3, Some(plus(1)), Some(plus(-60))), state),
                BreakerState::HalfOpen => (breaker(3, Some(now()), Some(plus(-60))), state),
            };
            assert_eq!(b.state(now()), expected);
        }
        assert_eq!(
            breaker(0, Some(plus(-1)), None).state(now()),
            BreakerState::Closed
        );
    }

    /// The breaker's transitions: closed, two failures only count and the third opens it for the
    /// first `DELIVERY` step; a throttle opens it at once until its wait (or the step when the wait
    /// is unusable); a success while closed resets the count; while open or half-open only the
    /// probe's success, started after the latest opening, closes it, and a stale probe or an older
    /// submission's success changes nothing; a failure while half-open reopens it a step further.
    #[test]
    fn the_breaker_opens_counts_and_closes_only_through_its_probe() {
        let probe = Some(Probe {
            message: Uuid::from_u128(7),
            generation: 2,
        });
        let fail = Health::Failure {
            throttled: false,
            wait: None,
        };
        assert_eq!(
            breaker_after(&breaker(0, None, None), fail, now(), 0),
            BreakerChange::Count { failures: 1 }
        );
        assert_eq!(
            breaker_after(&breaker(1, None, None), fail, now(), 0),
            BreakerChange::Count { failures: 2 }
        );
        assert_eq!(
            breaker_after(&breaker(2, None, None), fail, now(), 0),
            BreakerChange::Open {
                failures: 3,
                until: plus(30)
            }
        );
        assert_eq!(
            breaker_after(
                &breaker(3, Some(now()), Some(plus(-60))),
                fail,
                now(),
                90_000
            ),
            BreakerChange::Open {
                failures: 4,
                until: plus(120)
            },
            "half-open, the probe failed: the next step, whose largest draw is two minutes"
        );
        let throttle = |wait| Health::Failure {
            throttled: true,
            wait,
        };
        assert_eq!(
            breaker_after(&breaker(0, None, None), throttle(Some(plus(900))), now(), 0),
            BreakerChange::Open {
                failures: 1,
                until: plus(900)
            }
        );
        assert_eq!(
            breaker_after(&breaker(0, None, None), throttle(Some(plus(-1))), now(), 0),
            BreakerChange::Open {
                failures: 1,
                until: plus(30)
            }
        );
        let success = |probe, started| Health::Success { probe, started };
        assert_eq!(
            breaker_after(&breaker(2, None, None), success(None, None), now(), 0),
            BreakerChange::Reset
        );
        assert_eq!(
            breaker_after(&breaker(0, None, None), success(None, None), now(), 0),
            BreakerChange::Unchanged
        );
        let half_open = breaker(3, Some(plus(-1)), Some(plus(-60)));
        assert_eq!(
            breaker_after(&half_open, success(probe, Some(plus(-30))), now(), 0),
            BreakerChange::Close
        );
        assert_eq!(
            breaker_after(&half_open, success(probe, Some(plus(-90))), now(), 0),
            BreakerChange::Unchanged,
            "started before the latest opening"
        );
        assert_eq!(
            breaker_after(&half_open, success(None, Some(plus(-30))), now(), 0),
            BreakerChange::Unchanged,
            "not the probe"
        );
        let stale = Some(Probe {
            message: Uuid::from_u128(7),
            generation: 1,
        });
        assert_eq!(
            breaker_after(&half_open, success(stale, Some(plus(-30))), now(), 0),
            BreakerChange::Unchanged
        );
    }

    /// Admission over every pair of breaker states and probe liveness: closed and closed take
    /// whatever the budgets allow; anything open takes nothing; a half-open breaker with a live
    /// probe takes nothing; one without admits exactly one message as its probe, on whichever
    /// breakers are half-open.
    #[test]
    fn admission_follows_both_breakers() {
        let of = |state| match state {
            BreakerState::Closed => breaker(0, None, None),
            BreakerState::Open => breaker(3, Some(plus(60)), Some(plus(-1))),
            BreakerState::HalfOpen => breaker(3, Some(plus(-1)), Some(plus(-60))),
        };
        for connection in BreakerState::iter() {
            for connection_live in [false, true] {
                for scope in BreakerState::iter() {
                    for scope_live in [false, true] {
                        let expected = match (
                            wants_alone(connection, connection_live),
                            wants_alone(scope, scope_live),
                        ) {
                            (Some(false), Some(false)) => Admission::Free,
                            (Some(c), Some(s)) => Admission::Probe {
                                connection: c,
                                scope: s,
                            },
                            _ => Admission::Nothing,
                        };
                        assert_eq!(
                            admission(
                                (&of(connection), connection_live),
                                Some((&of(scope), scope_live)),
                                now()
                            ),
                            expected,
                            "{connection:?}/{connection_live} {scope:?}/{scope_live}"
                        );
                    }
                }
                let alone = match wants_alone(connection, connection_live) {
                    Some(false) => Admission::Free,
                    Some(true) => Admission::Probe {
                        connection: true,
                        scope: false,
                    },
                    None => Admission::Nothing,
                };
                assert_eq!(
                    admission((&of(connection), connection_live), None, now()),
                    alone
                );
            }
        }
    }

    fn wants_alone(state: BreakerState, live: bool) -> Option<bool> {
        match state {
            BreakerState::Closed => Some(false),
            BreakerState::Open => None,
            BreakerState::HalfOpen => (!live).then_some(true),
        }
    }

    /// Who an answer concerns, over every failure, scope and cause: only connection-scoped
    /// refusals touch the connection (a refused credential asks for authorization, a refused
    /// account disables, a throttle opens the breaker at once, a permanent `5.7.x` is a policy
    /// rejection, anything else counts); only scope-scoped ones the quota scope; only platform
    /// ones the in-process backoff. Acceptance is a success for both breakers.
    #[test]
    fn an_answer_reaches_only_what_its_scope_names() {
        assert_eq!(
            connection_effect(&Answer::Accepted),
            ConnectionEffect::Success
        );
        assert!(matches!(
            scope_effect(&Answer::Accepted),
            Some(Health::Success { .. })
        ));
        assert_eq!(platform_backoff(&Answer::Accepted, 0, now(), 0), None);
        for failure in Failure::iter() {
            for scope in RefusalScope::iter() {
                for cause in Cause::iter() {
                    let answer = Answer::Refused(refusal(failure, scope, cause));
                    let expected = match (scope, cause) {
                        (RefusalScope::Connection, Cause::Unauthorized) => {
                            ConnectionEffect::CredentialLost
                        }
                        (RefusalScope::Connection, Cause::Forbidden) => {
                            ConnectionEffect::AccountBlocked
                        }
                        (RefusalScope::Connection, Cause::Throttled) => ConnectionEffect::Failure {
                            throttled: true,
                            wait: None,
                        },
                        (RefusalScope::Connection, _) => ConnectionEffect::Failure {
                            throttled: false,
                            wait: None,
                        },
                        _ => ConnectionEffect::None,
                    };
                    assert_eq!(
                        connection_effect(&answer),
                        expected,
                        "{failure:?} {scope:?} {cause:?}"
                    );
                    assert_eq!(
                        scope_effect(&answer),
                        (scope == RefusalScope::QuotaScope).then_some(Health::Failure {
                            throttled: cause == Cause::Throttled,
                            wait: None
                        })
                    );
                    assert_eq!(
                        platform_backoff(&answer, 0, now(), 0),
                        (scope == RefusalScope::Platform).then(|| plus(30))
                    );
                }
            }
        }
        let policy = Answer::Refused(Refusal {
            status: status(5, 7, 1),
            ..refusal(Failure::Permanent, RefusalScope::Connection, Cause::Refused)
        });
        assert_eq!(connection_effect(&policy), ConnectionEffect::Policy);
        assert!(policy_streak(&[
            Category::Accepted,
            Category::Policy,
            Category::Policy,
            Category::Policy
        ]));
        assert!(!policy_streak(&[
            Category::Policy,
            Category::Policy,
            Category::Accepted
        ]));
        assert!(!policy_streak(&[Category::Policy, Category::Policy]));
    }

    /// What evidence does to its recipient, over every kind, category, recipient reference,
    /// confidence and the workspace's trust in its own inbox's reports: authenticated evidence
    /// naming the recipient suppresses a bounced or refused address or a complainant, and so does
    /// a corroborated `5.1.x` naming it once the workspace trusts its own inbox's reports; without
    /// that trust a corroborated `5.1.x` about a known recipient holds the address while a person
    /// reviews it, so no more mail reaches a probably dead address and none is lost for good on a
    /// report nobody verified; an unsubscribe naming its recipient suppresses it; a full mailbox
    /// or a domain without mail holds a known recipient; weaker evidence of an invalid address or
    /// a complaint, and every address change, goes to review; nothing else touches the address.
    #[test]
    fn evidence_suppresses_only_when_proven_or_trusted() {
        for trust in [false, true] {
            let settings = DeliverySettings {
                trust_own_inbox_dsns: trust,
            };
            for kind in EventKind::iter() {
                for category in Category::iter() {
                    for recipient in RecipientRef::iter() {
                        for confidence in Confidence::iter() {
                            let named = recipient == RecipientRef::Named;
                            let proven = named && confidence == Confidence::Authenticated;
                            let corroborated = confidence == Confidence::Corroborated;
                            let trusted = named && corroborated && trust;
                            let known = recipient != RecipientRef::Unknown;
                            let refusal_kind =
                                matches!(kind, EventKind::Bounced | EventKind::Rejected);
                            let expected = match category {
                                Category::InvalidRecipient
                                    if refusal_kind && (proven || trusted) =>
                                {
                                    RecipientEffect::Suppress(SuppressionReason::Bounce)
                                }
                                Category::InvalidRecipient
                                    if refusal_kind && known && corroborated =>
                                {
                                    RecipientEffect::HoldAndReview(
                                        HoldReason::InvalidRecipient,
                                        Proposal::Suppress(SuppressionReason::Bounce),
                                    )
                                }
                                Category::InvalidRecipient if refusal_kind => {
                                    RecipientEffect::Review(Proposal::Suppress(
                                        SuppressionReason::Bounce,
                                    ))
                                }
                                Category::MailboxFull if known => {
                                    RecipientEffect::Hold(HoldReason::MailboxFull)
                                }
                                Category::NoRoute if known => {
                                    RecipientEffect::Hold(HoldReason::NoRoute)
                                }
                                Category::Complaint if proven => {
                                    RecipientEffect::Suppress(SuppressionReason::Complaint)
                                }
                                Category::Complaint => RecipientEffect::Review(Proposal::Suppress(
                                    SuppressionReason::Complaint,
                                )),
                                Category::Unsubscribed if named => {
                                    RecipientEffect::Suppress(SuppressionReason::Unsubscribe)
                                }
                                Category::AddressChanged => {
                                    RecipientEffect::Review(Proposal::AddressChange)
                                }
                                _ => RecipientEffect::None,
                            };
                            let effect =
                                recipient_effect(kind, category, recipient, confidence, settings);
                            assert_eq!(
                                effect, expected,
                                "{trust} {kind:?} {category:?} {recipient:?} {confidence:?}"
                            );
                            if let RecipientEffect::Suppress(reason) = effect {
                                assert!(
                                    named
                                        && (confidence == Confidence::Authenticated
                                            || reason == SuppressionReason::Unsubscribe
                                            || trusted),
                                    "a suppression from evidence that does not prove its recipient"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// A message's recipients are held to its connection's provider: 50 through Amazon SES, 100
    /// through Gmail's SMTP servers (a password connection to a Gmail mailbox, by its host,
    /// however it is written), and the product's 150 for every other provider, Gmail's API
    /// included, whose own 500 the product's limit caps. Generated over every provider, so a new
    /// one fails until its limit is decided.
    #[test]
    fn recipients_are_capped_by_provider() {
        for provider in Provider::iter() {
            let expected = match provider {
                Provider::Ses => 50,
                Provider::Google
                | Provider::Microsoft
                | Provider::Smtp
                | Provider::Sendgrid
                | Provider::Mailgun
                | Provider::Norbelys => 150,
            };
            assert_eq!(recipients_max(provider, None), expected, "{provider:?}");
            assert_eq!(
                recipients_max(provider, Some("smtp.example.net")),
                expected,
                "{provider:?}"
            );
        }
        for host in [
            "smtp.gmail.com",
            "SMTP-Relay.Gmail.com.",
            "smtp.googlemail.com",
        ] {
            assert_eq!(recipients_max(Provider::Smtp, Some(host)), 100, "{host}");
        }
        assert_eq!(
            recipients_max(Provider::Ses, Some("email-smtp.us-east-1.amazonaws.com")),
            50
        );
    }

    /// The delivery settings read what a workspace stores: absent or `null` is the defaults, the
    /// one field takes only a boolean, and an unknown field is refused with its pointer, so a
    /// misspelt setting is never silently ignored.
    #[test]
    fn delivery_settings_are_read_strictly() {
        assert_eq!(
            DeliverySettings::parse(None),
            Ok(DeliverySettings::default())
        );
        assert_eq!(
            DeliverySettings::parse(Some(&serde_json::Value::Null)),
            Ok(DeliverySettings {
                trust_own_inbox_dsns: false
            })
        );
        assert_eq!(
            DeliverySettings::parse(Some(&serde_json::json!({ "trust_own_inbox_dsns": true }))),
            Ok(DeliverySettings {
                trust_own_inbox_dsns: true
            })
        );
        let errors = DeliverySettings::parse(Some(&serde_json::json!({
            "trust_own_inbox_dsns": "yes",
            "trust/everything": true
        })))
        .unwrap_err();
        let pointers: Vec<&str> = errors.iter().map(|error| error.pointer.as_str()).collect();
        assert_eq!(pointers.len(), 2);
        assert!(pointers.contains(&"/trust_own_inbox_dsns"), "{pointers:?}");
        assert!(pointers.contains(&"/trust~1everything"), "{pointers:?}");
        assert_eq!(
            DeliverySettings::parse(Some(&serde_json::json!([true]))).unwrap_err()[0].pointer,
            ""
        );
    }

    /// The complaint-rate breaker trips at 0.3 % of the window's sent messages with at least
    /// three complaints: two complaints never trip it, however small the sender; three trip it
    /// up to 1,000 messages and not at 1,001; complaints about mail sent before the window, with
    /// nothing sent in it, trip it too.
    #[test]
    fn the_complaint_rate_breaker_trips_at_its_threshold() {
        let cases = [
            (2, 0, false),
            (2, 10, false),
            (3, 0, true),
            (3, 1_000, true),
            (3, 1_001, false),
            (10, 3_333, true),
            (10, 3_334, false),
            (3_000, 1_000_000, true),
        ];
        for (complaints, sent, expected) in cases {
            assert_eq!(
                complaint_rate_exceeded(complaints, sent),
                expected,
                "{complaints} of {sent}"
            );
        }
    }

    /// Preflight over the envelope: an address that is not one fails the message; a lookup DNS
    /// could not answer defers it; unroutable addresses are held, and fail the message only when
    /// none is left to reach; routable ones proceed.
    #[test]
    fn preflight_fails_defers_or_proceeds() {
        for reason in Reason::iter() {
            let decision = after_preflight(&[reason]);
            let expected = match reason {
                Reason::Mx | Reason::ImplicitMx => Preflight::Proceed { unroutable: vec![] },
                Reason::Syntax => Preflight::Fail {
                    category: Category::InvalidAddress,
                    unroutable: vec![],
                },
                Reason::DnsUnavailable => Preflight::Defer,
                Reason::NullMx | Reason::NoRoute | Reason::NoDomain => Preflight::Fail {
                    category: Category::NoRoute,
                    unroutable: vec![0],
                },
            };
            assert_eq!(decision, expected, "{reason:?}");
        }
        assert_eq!(
            after_preflight(&[Reason::Mx, Reason::NullMx]),
            Preflight::Proceed {
                unroutable: vec![1]
            }
        );
        assert_eq!(
            after_preflight(&[Reason::NullMx, Reason::DnsUnavailable]),
            Preflight::Defer
        );
    }

    fn facts() -> StartFacts {
        StartFacts {
            now: now(),
            deadline_at: Some(plus(3_600)),
            kind: MessageKind::Campaign,
            workspace_deleted: false,
            connection_archived: false,
            connection_usable: true,
            suppressed: false,
            held_until: None,
            identity_usable: true,
            campaign_usable: true,
            windows_open: true,
            clock_due: true,
            breaker_blocks: false,
        }
    }

    /// The Start's checks in their order: every check passing submits; an expired deadline, a
    /// deleted workspace and a suppressed recipient end the message whatever else holds (a
    /// suppression never holds back the platform's transactional mail); an
    /// archived sender fails mail a caller addressed through it but returns campaign mail (the
    /// removal job re-arms it); every other failing check returns the message, keeping its due
    /// time when the claim filters the condition and looking again later when it does not.
    #[test]
    fn the_start_submits_ends_or_returns_by_its_checks() {
        assert_eq!(start_verdict(&facts()), StartVerdict::Submit);
        let ended = |state, category| StartVerdict::End { state, category };
        assert_eq!(
            start_verdict(&StartFacts {
                deadline_at: Some(now()),
                suppressed: true,
                ..facts()
            }),
            ended(MessageState::Failed, Category::Expired)
        );
        assert_eq!(
            start_verdict(&StartFacts {
                workspace_deleted: true,
                ..facts()
            }),
            ended(MessageState::Failed, Category::WorkspaceDeleted)
        );
        assert_eq!(
            start_verdict(&StartFacts {
                suppressed: true,
                connection_usable: false,
                ..facts()
            }),
            ended(MessageState::Suppressed, Category::Suppressed)
        );
        assert_eq!(
            start_verdict(&StartFacts {
                suppressed: true,
                kind: MessageKind::Transactional,
                ..facts()
            }),
            StartVerdict::Submit,
            "the platform's own mail is not held back by a suppression"
        );
        for kind in MessageKind::iter() {
            let verdict = start_verdict(&StartFacts {
                kind,
                connection_archived: true,
                connection_usable: false,
                ..facts()
            });
            let expected = if kind == MessageKind::Campaign {
                StartVerdict::Return {
                    run_at: None,
                    why: Held::ConnectionUnavailable,
                }
            } else {
                ended(MessageState::Failed, Category::SenderArchived)
            };
            assert_eq!(verdict, expected, "{kind:?}");
        }
        let later = Some(plus(300));
        for why in Held::iter() {
            let (changed, run_at) = match why {
                Held::ConnectionUnavailable => (
                    StartFacts {
                        connection_usable: false,
                        ..facts()
                    },
                    None,
                ),
                Held::Breaker => (
                    StartFacts {
                        breaker_blocks: true,
                        ..facts()
                    },
                    None,
                ),
                Held::Recipient => (
                    StartFacts {
                        held_until: Some(plus(7_200)),
                        ..facts()
                    },
                    Some(plus(7_200)),
                ),
                Held::IdentityUnavailable => (
                    StartFacts {
                        identity_usable: false,
                        ..facts()
                    },
                    later,
                ),
                Held::CampaignUnavailable => (
                    StartFacts {
                        campaign_usable: false,
                        ..facts()
                    },
                    later,
                ),
                Held::WindowClosed => (
                    StartFacts {
                        windows_open: false,
                        ..facts()
                    },
                    None,
                ),
                Held::ClockNotDue => (
                    StartFacts {
                        clock_due: false,
                        ..facts()
                    },
                    None,
                ),
            };
            assert_eq!(
                start_verdict(&changed),
                StartVerdict::Return { run_at, why },
                "{why:?}"
            );
        }
        assert_eq!(
            start_verdict(&StartFacts {
                deadline_at: None,
                ..facts()
            }),
            StartVerdict::Submit
        );
    }
}
