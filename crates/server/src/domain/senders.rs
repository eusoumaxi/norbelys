//! The pure rules of the Sending area: which provider is reached how, which connections are
//! paced and how their interval is kept on the 5-minute grid, what each way in proves about an
//! account, where an arriving account lands among the workspace's rows, how a connection's
//! health moves, and which of its moves people are told about by email.
//!
//! Every function here takes facts and returns a decision; the operations in `senders/` read the
//! facts, call these, and write the result. The tables are tested over every variant of their
//! enums, so a new provider, status or event fails the tests until its row is decided.
//!
//! # Pacing
//!
//! Cold mail runs on a grid of 5-minute slots. A **paced sender** sends its cold mail one message
//! per interval, on its fixed phase inside the slot: every mailbox (Google, Microsoft, an SMTP
//! login) is one, and so is an Amazon SES connection the customer sets to pace one From address.
//! The other relays (SendGrid, Mailgun) and the managed MTA are rate-paced: they have no
//! interval, and the delivery claim tells the two kinds apart by that alone. An interval is 5 to
//! 1,440 minutes and is rounded **up** to whole slots, so a cadence is never faster than asked:
//! 17 minutes becomes 20.
//!
//! # Identity
//!
//! A connection's account is the provider's immutable subject when the way in has one (an OAuth
//! ID token's `sub`, or Microsoft's `oid`), and its address otherwise, compared by ASCII
//! lowercase alone (dots and `+` tags are never folded). Each way in proves only what it can: an
//! OAuth consent proves the subject and the provider names the mailbox's address; a password
//! proves only that the server it names accepted the login; a relay or the managed MTA proves
//! that its own service accepted a credential, and the address is the customer's word. Nothing
//! is compared across workspaces, because a password accepted by an arbitrary server proves no
//! address.
//!
//! # Restoration
//!
//! Archiving keeps a connection's row (its history points at it) and erases its credential.
//! Connecting the same account again restores that row, with its id, history and identities: an
//! OAuth account by its subject, else by its address onto an archived row without a subject
//! (binding the subject); a password or relay account by its address onto an archived row
//! without a subject. A live row of the same subject is a conflict: the account is connected
//! already.

use serde::{Deserialize, Serialize};

/// The shortest interval between two cold sends of a paced sender, in minutes: one slot.
pub const INTERVAL_MIN: i32 = 5;
/// The longest interval, in minutes: a day.
pub const INTERVAL_MAX: i32 = 1_440;
/// The interval a mailbox gets when none is given, in minutes.
pub const INTERVAL_DEFAULT: i32 = 10;
/// The length of a slot of the sending grid, in minutes.
pub const SLOT_MINUTES: i32 = 5;

/// The provider behind a connection (`connections.provider`).
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
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// Any SMTP server reached with a login and a password (or an app password).
    Smtp,
    /// A Google mailbox connected through OAuth, sent and read through the Gmail API.
    Google,
    /// A Microsoft mailbox connected through OAuth, sent and read through Microsoft Graph.
    Microsoft,
    /// The customer's Amazon SES account, over its SMTP endpoint.
    Ses,
    /// The customer's SendGrid account, over its SMTP endpoint.
    Sendgrid,
    /// The customer's Mailgun account, over its SMTP endpoint.
    Mailgun,
    /// The managed MTA Norbelys operates.
    Norbelys,
}

impl Provider {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// How the account is reached.
    #[must_use]
    pub fn way_in(self) -> WayIn {
        match self {
            Self::Google | Self::Microsoft => WayIn::OAuth,
            Self::Smtp => WayIn::Password,
            Self::Ses | Self::Sendgrid | Self::Mailgun => WayIn::Relay,
            Self::Norbelys => WayIn::Managed,
        }
    }

    /// The transport its mail is submitted over: the provider's HTTP API for the OAuth
    /// mailboxes, SMTP for everything else (the relays' HTTP APIs are not transports here).
    #[must_use]
    pub fn transport(self) -> Transport {
        match self {
            Self::Google | Self::Microsoft => Transport::Api,
            Self::Smtp | Self::Ses | Self::Sendgrid | Self::Mailgun | Self::Norbelys => {
                Transport::Smtp
            }
        }
    }

    /// Whether its cold mail follows a pacing clock.
    #[must_use]
    pub fn pacing(self) -> Pacing {
        match self {
            Self::Smtp | Self::Google | Self::Microsoft => Pacing::Required,
            Self::Ses => Pacing::Optional,
            Self::Sendgrid | Self::Mailgun | Self::Norbelys => Pacing::Refused,
        }
    }

    /// True for a mailbox: a person's inbox, always a paced sender.
    #[must_use]
    pub fn is_mailbox(self) -> bool {
        self.pacing() == Pacing::Required
    }

    /// What verifies the provider's callbacks about our messages, for the providers that send
    /// them; `None` for the mailboxes, whose evidence arrives as mail.
    #[must_use]
    pub fn webhook_key(self) -> Option<WebhookKey> {
        match self {
            Self::Ses => Some(WebhookKey::TopicArn),
            Self::Sendgrid => Some(WebhookKey::VerificationKey),
            Self::Mailgun => Some(WebhookKey::SigningKey),
            Self::Norbelys => Some(WebhookKey::Generated),
            Self::Smtp | Self::Google | Self::Microsoft => None,
        }
    }

    /// The daily limit a new connection gets when none is given: a mailbox starts at a cold
    /// sender's careful pace, a relay at what an account sends in a day, and the managed MTA
    /// below the 1,000 messages its own per-login backstop allows, so the product's ledger is
    /// what a user meets first.
    #[must_use]
    pub fn default_daily_limit(self) -> i32 {
        match self {
            Self::Smtp | Self::Google | Self::Microsoft => 50,
            Self::Ses | Self::Sendgrid | Self::Mailgun => 10_000,
            Self::Norbelys => 500,
        }
    }
}

/// How a connection's account is reached, which decides what the connection proves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum WayIn {
    /// An OAuth consent at Google or Microsoft.
    OAuth,
    /// A login and password at an SMTP (and IMAP) server.
    Password,
    /// A relay's SMTP credential.
    Relay,
    /// A login Norbelys provisions on its own MTA.
    Managed,
}

/// What a way in proves about the account behind a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proof {
    /// The provider names an immutable subject (an OAuth ID token's `sub`, Microsoft's `oid`):
    /// the account's identity across address changes and archive.
    pub subject: bool,
    /// Where the connection's address comes from.
    pub address: AddressSource,
}

/// Where a connection's address comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressSource {
    /// The provider names the mailbox: Gmail's `users.getProfile`, Graph's `GET /me`.
    Provider,
    /// The login a server accepted, an address or a generic login, as it is.
    Login,
    /// The customer's word: the From address a paced SES connection paces, or a name for a
    /// rate-paced relay account.
    Declared,
}

impl WayIn {
    /// What this way in proves.
    #[must_use]
    pub fn proves(self) -> Proof {
        match self {
            Self::OAuth => Proof {
                subject: true,
                address: AddressSource::Provider,
            },
            Self::Password => Proof {
                subject: false,
                address: AddressSource::Login,
            },
            Self::Relay | Self::Managed => Proof {
                subject: false,
                address: AddressSource::Declared,
            },
        }
    }
}

/// The transport a connection submits over (`connections.transport`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumIter, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(rename_all = "snake_case")]
pub enum Transport {
    /// SMTP submission.
    Smtp,
    /// The Gmail API or Microsoft Graph.
    Api,
}

impl Transport {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Whether a provider's connections follow a pacing clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pacing {
    /// Always paced: a mailbox.
    Required,
    /// Paced when created with an interval, rate-paced otherwise, and never switched.
    Optional,
    /// Never paced.
    Refused,
}

/// The verification material of a provider's callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookKey {
    /// Amazon SES posts through an SNS topic, whose ARN pins which topic is accepted.
    TopicArn,
    /// SendGrid signs with ECDSA; it shows the public key once the webhook's URL is set.
    VerificationKey,
    /// Mailgun signs with the account's HTTP webhook signing key.
    SigningKey,
    /// Norbelys generates the Standard Webhooks secret its own MTA signs with.
    Generated,
}

/// Why an interval was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IntervalError {
    /// Outside 5 to 1,440 minutes.
    #[error("the interval is between 5 and 1,440 minutes")]
    Range,
    /// The connection is rate-paced and never takes an interval.
    #[error("this connection is rate-paced and takes no interval")]
    NotPaced,
}

/// `minutes` rounded up to whole 5-minute slots. The caller has checked the range, inside which
/// the result never exceeds a day.
#[must_use]
pub fn round_to_slots(minutes: i32) -> i32 {
    minutes.saturating_add(SLOT_MINUTES - 1) / SLOT_MINUTES * SLOT_MINUTES
}

/// The interval a new connection of `provider` gets for the `requested` one: a mailbox's is
/// required and defaults to 10 minutes; an SES connection is paced only when given one; the
/// other relays and the managed MTA refuse one. A given interval is checked and rounded up to
/// whole slots.
///
/// # Errors
///
/// The interval is out of range, or the provider is never paced.
pub fn new_interval(
    provider: Provider,
    requested: Option<i32>,
) -> Result<Option<i32>, IntervalError> {
    match (provider.pacing(), requested) {
        (Pacing::Required, None) => Ok(Some(INTERVAL_DEFAULT)),
        (Pacing::Optional, None) | (Pacing::Refused, None) => Ok(None),
        (Pacing::Refused, Some(_)) => Err(IntervalError::NotPaced),
        (Pacing::Required | Pacing::Optional, Some(minutes)) => checked(minutes).map(Some),
    }
}

/// The interval after a change to `requested` on a connection whose interval is `current`: a
/// paced sender keeps being one and takes any interval in range; a rate-paced one never becomes
/// paced.
///
/// # Errors
///
/// The interval is out of range, or the connection is rate-paced.
pub fn changed_interval(current: Option<i32>, requested: i32) -> Result<i32, IntervalError> {
    match current {
        Some(_) => checked(requested),
        None => Err(IntervalError::NotPaced),
    }
}

fn checked(minutes: i32) -> Result<i32, IntervalError> {
    if (INTERVAL_MIN..=INTERVAL_MAX).contains(&minutes) {
        Ok(round_to_slots(minutes))
    } else {
        Err(IntervalError::Range)
    }
}

/// A connection's lifecycle (`connections.status`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
#[schema(as = ConnectionStatus)]
pub enum Status {
    /// Created without a credential to check yet.
    Unverified,
    /// A check of its credential is queued or running.
    Verifying,
    /// Its credential works; it sends unless paused or held by its breaker.
    Active,
    /// Its credential or consent was lost; a person reconnects or pastes a new one.
    AuthorizationRequired,
    /// It could not be set up (the managed MTA refused to provision it).
    Failed,
    /// The provider or an administrator blocked the account.
    Disabled,
    /// Ended by a person: readable for its history, credential erased, never sending again
    /// unless the same account is connected again.
    Archived,
}

impl Status {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Something that happens to a connection's health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum HealthEvent {
    /// A person asked for the credential to be checked: `verify`, a new credential, a reconnect.
    VerifyRequested,
    /// A check proved the credential and the account.
    CheckPassed,
    /// A check found the credential or the consent lost: a refused login, a revoked grant, a
    /// grant without the scopes the connection needs.
    CredentialLost,
    /// A check found the account blocked by policy: a Workspace or tenant administrator
    /// restricted the app, or the domain disabled API access.
    AccountBlocked,
    /// The managed MTA refused to provision the connection.
    ProvisionFailed,
    /// A person archived the connection.
    Archived,
    /// A person connected the account of an archived connection again, which restores that
    /// connection (its id, history and identities) with the new credential to be checked.
    Restored,
    /// Recipients reported the connection's mail as spam at a rate mailbox providers do not
    /// tolerate (the complaint-rate breaker): the list or the content needs a person.
    ComplaintRate,
}

/// What a health event does to a status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The connection moves to this status.
    To(Status),
    /// Nothing changes: the status already says it, or the event no longer applies (a check
    /// that finished after a person acted, whose fence already refuses it).
    Unchanged,
    /// The event is not allowed from this status: answered `409 invalid_state`.
    Refused,
}

/// The health table. Checks only ever start from `verifying` or `active` (a connection in any
/// other status waits for its person), so their outcomes change only those two; an archived
/// connection takes no request but its archive again, or its restoration, which sends it to
/// `verifying` with the credential just given. Only an archived connection is restored: a live
/// one already holds its account.
#[must_use]
pub fn transition(status: Status, event: HealthEvent) -> Transition {
    use HealthEvent as E;
    use Status as S;
    let to = |next: Status| {
        if next == status {
            Transition::Unchanged
        } else {
            Transition::To(next)
        }
    };
    match (status, event) {
        (S::Archived, E::VerifyRequested) => Transition::Refused,
        (S::Archived, E::Restored) => Transition::To(S::Verifying),
        (S::Archived, _) => Transition::Unchanged,
        (_, E::Archived) => Transition::To(S::Archived),
        (_, E::Restored) => Transition::Refused,
        (_, E::VerifyRequested) => to(S::Verifying),
        (S::Verifying | S::Active, E::CheckPassed) => to(S::Active),
        (S::Verifying | S::Active, E::CredentialLost) => to(S::AuthorizationRequired),
        (S::Verifying | S::Active, E::AccountBlocked | E::ComplaintRate) => to(S::Disabled),
        (S::Verifying, E::ProvisionFailed) => to(S::Failed),
        (
            S::Unverified | S::Active | S::AuthorizationRequired | S::Failed | S::Disabled,
            E::ProvisionFailed,
        )
        | (
            S::Unverified | S::AuthorizationRequired | S::Failed | S::Disabled,
            E::CheckPassed | E::CredentialLost | E::AccountBlocked | E::ComplaintRate,
        ) => Transition::Unchanged,
    }
}

/// Whether people are told by email that a connection moved to `status` (by the health table):
/// when it stops working (its authorization was lost, its setup failed, its provider or its
/// account's administrator blocked it) and when it works, again or for the first time. A step of
/// setting up and an archive are a person's own doing, whose result that person sees.
#[must_use]
pub fn told(status: Status) -> bool {
    match status {
        Status::Active | Status::AuthorizationRequired | Status::Failed | Status::Disabled => true,
        Status::Unverified | Status::Verifying | Status::Archived => false,
    }
}

/// A row of the workspace that may hold an arriving account, found by its subject or by its
/// address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate<I> {
    /// The row.
    pub id: I,
    /// Its provider.
    pub provider: Provider,
    /// Whether it is archived.
    pub archived: bool,
    /// Whether it holds a subject at all.
    pub has_subject: bool,
    /// Whether its subject is the arriving account's.
    pub same_subject: bool,
}

/// Where an arriving account lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement<I> {
    /// A new row.
    Create,
    /// This archived row comes back, with its id, history and identities; `bind_subject` when
    /// the row had no subject and the arriving OAuth account's subject is now bound to it.
    Restore { id: I, bind_subject: bool },
    /// The account is connected already, live, as this row.
    Conflict(I),
}

/// Where an account arriving through `provider` (with a subject when it has one) lands among
/// `candidates`, newest first. Live rows found by address are not decided here: the database's
/// unique indexes refuse a second live row of one account, or a second live paced sender of one
/// address, and the operation names the live row in its answer.
#[must_use]
pub fn place<I: Copy>(
    provider: Provider,
    has_subject: bool,
    candidates: &[Candidate<I>],
) -> Placement<I> {
    if has_subject && let Some(same) = candidates.iter().find(|candidate| candidate.same_subject) {
        return if same.archived {
            Placement::Restore {
                id: same.id,
                bind_subject: false,
            }
        } else {
            Placement::Conflict(same.id)
        };
    }
    let restorable = |candidate: &&Candidate<I>| {
        candidate.archived
            && !candidate.has_subject
            && match provider.way_in() {
                WayIn::OAuth | WayIn::Password => candidate.provider.is_mailbox(),
                WayIn::Relay | WayIn::Managed => candidate.provider == provider,
            }
    };
    match candidates.iter().find(restorable) {
        Some(row) => Placement::Restore {
            id: row.id,
            bind_subject: has_subject,
        },
        None => Placement::Create,
    }
}

/// A sending domain's lifecycle (`sending_domains.status`), as far as its DNS decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum DomainStatus {
    /// Its ownership was never proven; the hostname is not held.
    PendingVerification,
    /// Its ownership is proven.
    Verified,
    /// Proven, and its tracking hostname points at Norbelys: its certificate is next.
    PendingCertificate,
    /// It was proven once and its ownership record is gone; the hostname stays held until a
    /// person deletes the domain or the record comes back.
    Suspended,
}

impl DomainStatus {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// What a DNS check found missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum DomainProblem {
    /// The ownership TXT record does not carry the token.
    OwnershipRecordMissing,
    /// Tracking is on and the hostname's CNAME does not point at Norbelys.
    TrackingRecordMissing,
}

/// A sending domain's status after a DNS check that found its ownership record or not, and,
/// when tracking is on, its tracking CNAME or not; `ever_verified` when it was proven before.
#[must_use]
pub fn domain_checked(
    ownership: bool,
    tracking: Option<bool>,
    ever_verified: bool,
) -> (DomainStatus, Option<DomainProblem>) {
    match (ownership, tracking) {
        (false, _) if ever_verified => (
            DomainStatus::Suspended,
            Some(DomainProblem::OwnershipRecordMissing),
        ),
        (false, _) => (
            DomainStatus::PendingVerification,
            Some(DomainProblem::OwnershipRecordMissing),
        ),
        (true, Some(true)) => (DomainStatus::PendingCertificate, None),
        (true, Some(false)) => (
            DomainStatus::Verified,
            Some(DomainProblem::TrackingRecordMissing),
        ),
        (true, None) => (DomainStatus::Verified, None),
    }
}

/// The days and hours a connection may submit campaign mail, in its time zone: the days of the
/// week (1 is Monday, 7 is Sunday) and an opening and a closing time on 5-minute marks, the
/// opening first (a window never crosses midnight).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SendWindow {
    /// The days of the week, 1 (Monday) to 7 (Sunday), each once, in order.
    pub days: Vec<u8>,
    /// The opening time, `HH:MM`.
    pub start: String,
    /// The closing time, `HH:MM`, after the opening; `24:00` closes at midnight.
    pub end: String,
}

/// Why a send window was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WindowError {
    /// No day, a day outside 1 to 7, or a day twice.
    #[error("the days are 1 (Monday) to 7 (Sunday), at least one, each once")]
    Days,
    /// A time that is not `HH:MM` on a 5-minute mark.
    #[error("the times are `HH:MM` on 5-minute marks")]
    Time,
    /// The closing time is not after the opening time.
    #[error("the window closes after it opens, the same day")]
    Order,
}

impl SendWindow {
    /// Checks a window and puts its days in order.
    ///
    /// # Errors
    ///
    /// The days, a time or their order is invalid.
    pub fn parse(days: &[u8], start: &str, end: &str) -> Result<Self, WindowError> {
        let mut sorted = days.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.is_empty()
            || sorted.len() != days.len()
            || sorted.iter().any(|day| !(1..=7).contains(day))
        {
            return Err(WindowError::Days);
        }
        let opens = minute_of_day(start).filter(|minutes| *minutes < 24 * 60);
        let closes = minute_of_day(end);
        match (opens, closes) {
            (Some(opens), Some(closes)) if opens < closes => Ok(Self {
                days: sorted,
                start: start.to_owned(),
                end: end.to_owned(),
            }),
            (Some(_), Some(_)) => Err(WindowError::Order),
            _ => Err(WindowError::Time),
        }
    }
}

/// The minute of the day `HH:MM` names, on a 5-minute mark, up to `24:00`. The sender evaluates
/// stored send windows with it (`domain::schedule`), so a window is read the way it was checked.
pub(crate) fn minute_of_day(text: &str) -> Option<u32> {
    let (hours, minutes) = text.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours: u32 = hours.parse().ok()?;
    let minutes: u32 = minutes.parse().ok()?;
    let total = hours.checked_mul(60)?.checked_add(minutes)?;
    (minutes < 60 && total <= 24 * 60 && minutes.is_multiple_of(5)).then_some(total)
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::{
        AddressSource, Candidate, HealthEvent, IntervalError, Pacing, Placement, Provider,
        SendWindow, Status, Transition, WayIn, WindowError, changed_interval, new_interval, place,
        round_to_slots, told, transition,
    };

    /// Every provider has its documented pacing: the three mailbox kinds are always paced, SES
    /// may be, SendGrid, Mailgun and the managed MTA never are; and what that means for a new
    /// connection's interval: a mailbox gets 10 minutes by default, an SES connection is paced
    /// only when asked, the others refuse any interval.
    #[test]
    fn each_provider_has_its_pacing_and_default_interval() {
        for provider in Provider::iter() {
            let (pacing, default, given) = match provider {
                Provider::Smtp | Provider::Google | Provider::Microsoft => {
                    (Pacing::Required, Ok(Some(10)), Ok(Some(20)))
                }
                Provider::Ses => (Pacing::Optional, Ok(None), Ok(Some(20))),
                Provider::Sendgrid | Provider::Mailgun | Provider::Norbelys => {
                    (Pacing::Refused, Ok(None), Err(IntervalError::NotPaced))
                }
            };
            assert_eq!(provider.pacing(), pacing, "{provider:?}");
            assert_eq!(new_interval(provider, None), default, "{provider:?}");
            assert_eq!(new_interval(provider, Some(17)), given, "{provider:?}");
            assert_eq!(provider.is_mailbox(), pacing == Pacing::Required);
        }
    }

    /// An interval is 5 to 1,440 minutes and is rounded up, never down, to whole 5-minute
    /// slots, so the cadence a person sets is never exceeded: 17 and 18 become 20, a multiple
    /// of 5 stays, the bounds stay, and anything outside them is refused.
    #[test]
    fn intervals_round_up_to_whole_slots_within_range() {
        for (minutes, rounded) in [
            (5, 5),
            (6, 10),
            (10, 10),
            (17, 20),
            (18, 20),
            (1_436, 1_440),
            (1_440, 1_440),
        ] {
            assert_eq!(round_to_slots(minutes), rounded);
            assert_eq!(
                new_interval(Provider::Smtp, Some(minutes)),
                Ok(Some(rounded))
            );
        }
        for minutes in [i32::MIN, -5, 0, 4, 1_441, i32::MAX] {
            assert_eq!(
                new_interval(Provider::Google, Some(minutes)),
                Err(IntervalError::Range),
                "{minutes}"
            );
        }
    }

    /// A paced sender stays paced and a rate-paced connection never becomes paced: changing the
    /// interval checks and rounds it only when the connection already has one.
    #[test]
    fn a_change_keeps_a_connection_paced_or_rate_paced() {
        assert_eq!(changed_interval(Some(10), 17), Ok(20));
        assert_eq!(changed_interval(Some(10), 2_000), Err(IntervalError::Range));
        assert_eq!(changed_interval(None, 10), Err(IntervalError::NotPaced));
    }

    /// Each way in proves what its table says: OAuth a subject and a provider-named address, a
    /// password only the login a server accepted, a relay or the managed MTA the customer's word;
    /// and each provider is reached by its way in, over its transport.
    #[test]
    fn each_way_in_proves_what_it_can() {
        for way in WayIn::iter() {
            let proof = way.proves();
            let (subject, address) = match way {
                WayIn::OAuth => (true, AddressSource::Provider),
                WayIn::Password => (false, AddressSource::Login),
                WayIn::Relay | WayIn::Managed => (false, AddressSource::Declared),
            };
            assert_eq!(
                (proof.subject, proof.address),
                (subject, address),
                "{way:?}"
            );
        }
        for provider in Provider::iter() {
            let expected = match provider {
                Provider::Google | Provider::Microsoft => WayIn::OAuth,
                Provider::Smtp => WayIn::Password,
                Provider::Ses | Provider::Sendgrid | Provider::Mailgun => WayIn::Relay,
                Provider::Norbelys => WayIn::Managed,
            };
            assert_eq!(provider.way_in(), expected, "{provider:?}");
            assert_eq!(
                provider.transport().as_str(),
                if expected == WayIn::OAuth {
                    "api"
                } else {
                    "smtp"
                }
            );
            assert_eq!(
                provider.webhook_key().is_some(),
                matches!(expected, WayIn::Relay | WayIn::Managed),
                "{provider:?}"
            );
        }
    }

    fn candidate(
        id: u8,
        provider: Provider,
        archived: bool,
        has_subject: bool,
        same_subject: bool,
    ) -> Candidate<u8> {
        Candidate {
            id,
            provider,
            archived,
            has_subject,
            same_subject,
        }
    }

    /// The restoration order, for every way in: an OAuth account comes back by its subject
    /// first (a live row of that subject is a conflict), then by its address onto an archived
    /// mailbox row without a subject, binding the subject; a password onto an archived mailbox
    /// row without a subject only; a relay or the managed MTA onto an archived row of its own
    /// provider without a subject. Anything else is a new row: a different OAuth account that
    /// now has the address of an archived row carrying another subject gets its own row.
    #[test]
    fn arriving_accounts_restore_in_the_documented_order() {
        let archived_subject = candidate(1, Provider::Google, true, true, false);
        let archived_password = candidate(2, Provider::Smtp, true, false, false);
        let live_password = candidate(3, Provider::Smtp, false, false, false);
        for provider in Provider::iter() {
            let oauth = provider.way_in() == WayIn::OAuth;
            let same = candidate(9, provider, true, true, true);
            let expected_same = if oauth {
                Placement::Restore {
                    id: 9,
                    bind_subject: false,
                }
            } else {
                Placement::Create
            };
            let same_by_address = Candidate {
                same_subject: false,
                ..same
            };
            assert_eq!(
                place(
                    provider,
                    oauth,
                    &[if oauth { same } else { same_by_address }]
                ),
                expected_same,
                "{provider:?}"
            );
            if oauth {
                let live = Candidate {
                    archived: false,
                    ..same
                };
                assert_eq!(
                    place(provider, true, &[live, archived_password]),
                    Placement::Conflict(9)
                );
            }
            let expected_address = match provider.way_in() {
                WayIn::OAuth => Placement::Restore {
                    id: 2,
                    bind_subject: true,
                },
                WayIn::Password => Placement::Restore {
                    id: 2,
                    bind_subject: false,
                },
                WayIn::Relay | WayIn::Managed => Placement::Create,
            };
            assert_eq!(
                place(
                    provider,
                    oauth,
                    &[archived_subject, live_password, archived_password]
                ),
                expected_address,
                "{provider:?}"
            );
            let own = candidate(4, provider, true, false, false);
            let expected_own = if oauth {
                Placement::Create
            } else {
                Placement::Restore {
                    id: 4,
                    bind_subject: false,
                }
            };
            if !oauth {
                assert_eq!(
                    place(provider, false, &[archived_subject, own]),
                    expected_own,
                    "{provider:?}"
                );
            }
            assert_eq!(
                place(provider, oauth, &[archived_subject, live_password]),
                Placement::Create
            );
        }
    }

    /// People are told by email when a connection starts or stops working, never about a step of
    /// setting up or an archive, which the person doing them sees: the rule that keeps the health
    /// notices to news.
    #[test]
    fn people_are_told_when_a_connection_starts_or_stops_working() {
        for status in Status::iter() {
            let expected = match status {
                Status::Active
                | Status::AuthorizationRequired
                | Status::Failed
                | Status::Disabled => true,
                Status::Unverified | Status::Verifying | Status::Archived => false,
            };
            assert_eq!(told(status), expected, "{status:?}");
        }
    }

    /// The health table, over every status and event: a request to verify moves any live
    /// connection to `verifying` and is refused once archived; checks change only `verifying`
    /// and `active` connections (a pass activates, a lost credential asks for authorization, a
    /// blocked account disables), and so does the complaint-rate breaker, which disables them
    /// too; a failed provisioning fails only a connection being verified;
    /// archiving ends every status; connecting an archived account again restores it to
    /// `verifying`, and nothing live is restored; nothing else changes.
    #[test]
    fn health_moves_by_the_table() {
        use HealthEvent as E;
        use Status as S;
        for status in Status::iter() {
            for event in HealthEvent::iter() {
                let checked = matches!(status, S::Verifying | S::Active);
                let expected = match (status, event) {
                    (S::Archived, E::VerifyRequested) => Transition::Refused,
                    (S::Archived, E::Restored) => Transition::To(S::Verifying),
                    (S::Archived, _) => Transition::Unchanged,
                    (_, E::Archived) => Transition::To(S::Archived),
                    (_, E::Restored) => Transition::Refused,
                    (S::Verifying, E::VerifyRequested) => Transition::Unchanged,
                    (_, E::VerifyRequested) => Transition::To(S::Verifying),
                    (S::Active, E::CheckPassed) => Transition::Unchanged,
                    (_, E::CheckPassed) if checked => Transition::To(S::Active),
                    (_, E::CredentialLost) if checked => Transition::To(S::AuthorizationRequired),
                    (_, E::AccountBlocked | E::ComplaintRate) if checked => {
                        Transition::To(S::Disabled)
                    }
                    (S::Verifying, E::ProvisionFailed) => Transition::To(S::Failed),
                    _ => Transition::Unchanged,
                };
                assert_eq!(
                    transition(status, event),
                    expected,
                    "{status:?} + {event:?}"
                );
            }
        }
    }

    /// A DNS check decides a sending domain's status over every combination of what it found:
    /// without the ownership record a domain never proven stays pending (and does not hold its
    /// hostname) while one proven before is suspended (and keeps it); with it, the domain is
    /// verified, and with tracking on its CNAME either moves it on to its certificate or is
    /// reported missing.
    #[test]
    fn a_dns_check_decides_the_domain_status() {
        use super::{DomainProblem as P, DomainStatus as S, domain_checked};
        for ownership in [false, true] {
            for tracking in [None, Some(false), Some(true)] {
                for ever in [false, true] {
                    let expected = match (ownership, tracking, ever) {
                        (false, _, false) => {
                            (S::PendingVerification, Some(P::OwnershipRecordMissing))
                        }
                        (false, _, true) => (S::Suspended, Some(P::OwnershipRecordMissing)),
                        (true, None, _) => (S::Verified, None),
                        (true, Some(false), _) => (S::Verified, Some(P::TrackingRecordMissing)),
                        (true, Some(true), _) => (S::PendingCertificate, None),
                    };
                    assert_eq!(domain_checked(ownership, tracking, ever), expected);
                }
            }
        }
    }

    /// A send window takes the days 1 to 7 once each (put in order) and two `HH:MM` times on
    /// 5-minute marks, the opening before the closing on the same day, `24:00` closing at
    /// midnight.
    #[test]
    fn send_windows_open_before_they_close_on_the_grid() {
        let window = SendWindow::parse(&[5, 1, 3], "09:00", "17:30").unwrap();
        assert_eq!(window.days, [1, 3, 5]);
        assert!(SendWindow::parse(&[7], "18:00", "24:00").is_ok());
        for (days, start, end, error) in [
            (&[][..], "09:00", "17:00", WindowError::Days),
            (&[0][..], "09:00", "17:00", WindowError::Days),
            (&[8][..], "09:00", "17:00", WindowError::Days),
            (&[1, 1][..], "09:00", "17:00", WindowError::Days),
            (&[1][..], "09:07", "17:00", WindowError::Time),
            (&[1][..], "9:00", "17:00", WindowError::Time),
            (&[1][..], "24:00", "24:00", WindowError::Time),
            (&[1][..], "09:00", "24:05", WindowError::Time),
            (&[1][..], "17:00", "09:00", WindowError::Order),
            (&[1][..], "09:00", "09:00", WindowError::Order),
        ] {
            assert_eq!(
                SendWindow::parse(days, start, end),
                Err(error),
                "{days:?} {start}-{end}"
            );
        }
    }
}
