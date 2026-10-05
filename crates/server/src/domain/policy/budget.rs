//! How much a connection may claim, and how long a spent budget makes it wait.
//!
//! # The budgets a claim reads
//!
//! - **The daily budget**: the connection's daily limit, scaled by its warm-up stage, against
//!   today's UTC bucket alone (used plus reserved, a missing bucket as zero). It is the product's
//!   own accounting per day, so a mailbox at its limit sends its whole limit every day.
//! - **The rolling limits**: the provider's own daily limits, which it counts over a rolling 24
//!   hours, checked against today's and yesterday's buckets together, so a limit of N can never
//!   admit 2N across midnight. They are a mailbox's provider cap ([`provider_cap`]) and a quota
//!   scope's messages and recipients a day.
//!
//! The allowance is the smallest of what those leave, the in-process limiters' permits, twice the
//! replica's free submission slots and the eligible rows ([`allowance`]); on a paced sender it is
//! at most one message, its oldest due mail created through the API first, else one cold message
//! ([`take`]), so mail created through the API never widens the cold allowance.
//!
//! # A spent budget
//!
//! When a budget has no room, the claim records how long the connection waits (`next_claim_at`)
//! and both candidate scans skip it until then ([`spent_until`]). The wait follows what spent it:
//! used units alone reaching a limit free nothing before the next UTC midnight (settlement only
//! moves reserved units into used, nothing takes any out), so the connection waits for midnight at
//! its phase; reservations still outstanding may be released any time (a Start returning a message,
//! a transient failure), so it waits one slot and looks again. A two-day limit may need a second
//! midnight, which the claim then finds.

use jiff::Timestamp;

use crate::domain::schedule::next_phase_at;
use crate::domain::senders::Provider;

const DAY_SECONDS: i64 = 86_400;

/// One day's bucket of a ledger: units used (settled as sent or possibly sent) and reserved
/// (claimed, not settled).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bucket {
    /// Settled units.
    pub used: i64,
    /// Units claimed and not settled yet.
    pub reserved: i64,
}

impl Bucket {
    fn total(self) -> i64 {
        self.used.saturating_add(self.reserved)
    }

    fn plus(self, other: Self) -> Self {
        Self {
            used: self.used.saturating_add(other.used),
            reserved: self.reserved.saturating_add(other.reserved),
        }
    }
}

/// A quota scope's daily limits and its two buckets of each unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScopeBudget {
    /// Messages a rolling day, when the scope limits them.
    pub messages_per_day: Option<i64>,
    /// Recipients a rolling day, when the scope limits them.
    pub recipients_per_day: Option<i64>,
    /// Messages today and yesterday.
    pub messages: [Bucket; 2],
    /// Recipients today and yesterday.
    pub recipients: [Bucket; 2],
}

/// Everything a claim weighs before it takes rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Budgets {
    /// The daily limit after warm-up.
    pub daily_limit: i64,
    /// The connection's messages today.
    pub today: Bucket,
    /// The connection's messages yesterday.
    pub yesterday: Bucket,
    /// The mailbox's provider cap over a rolling day, in messages, when its provider has one.
    pub provider_cap: Option<i64>,
    /// The quota scope's budget, when the connection has a scope.
    pub scope: Option<ScopeBudget>,
}

/// What the budgets leave: messages, and the scope's recipients when it limits them (a message
/// fits only if its recipients do).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Room {
    /// Messages that may still be reserved.
    pub messages: i64,
    /// Recipients the scope still allows, when it limits them.
    pub recipients: Option<i64>,
}

/// What `budgets` leave (see the module): never negative.
#[must_use]
pub fn room(budgets: &Budgets) -> Room {
    let rolling = budgets.today.plus(budgets.yesterday);
    let mut messages = budgets.daily_limit.saturating_sub(budgets.today.total());
    if let Some(cap) = budgets.provider_cap {
        messages = messages.min(cap.saturating_sub(rolling.total()));
    }
    let mut recipients = None;
    if let Some(scope) = budgets.scope {
        let [today, yesterday] = scope.messages;
        if let Some(limit) = scope.messages_per_day {
            messages = messages.min(limit.saturating_sub(today.plus(yesterday).total()));
        }
        let [today, yesterday] = scope.recipients;
        recipients = scope
            .recipients_per_day
            .map(|limit| limit.saturating_sub(today.plus(yesterday).total()).max(0));
    }
    if recipients == Some(0) {
        messages = 0;
    }
    Room {
        messages: messages.max(0),
        recipients,
    }
}

/// How many messages a claim may take: the smallest of what the budgets leave, the limiters'
/// `permits`, twice the replica's `free_slots` (so the next wave is ready while one is being
/// submitted, and claimed work never waits long in memory) and the `eligible` rows.
#[must_use]
pub fn allowance(room: &Room, permits: u32, free_slots: u32, eligible: u32) -> u32 {
    let budget = u32::try_from(room.messages.max(0)).unwrap_or(u32::MAX);
    budget
        .min(permits)
        .min(free_slots.saturating_mul(2))
        .min(eligible)
}

/// When a connection whose budgets have no room may be claimed again (`next_claim_at`), at its
/// `phase`, from `now`; `None` when every budget has room. Each spent budget waits for the next UTC
/// midnight when its used units alone reach its limit, else one slot; the connection waits for the
/// latest of them, since it can send nothing until all have room.
#[must_use]
pub fn spent_until(budgets: &Budgets, now: Timestamp, phase_seconds: i32) -> Option<Timestamp> {
    let midnight = next_phase_at(next_midnight(now), phase_seconds);
    let slot = next_phase_at(now, phase_seconds);
    let wait = |limit: i64, bucket: Bucket| -> Option<Timestamp> {
        if bucket.total() < limit {
            None
        } else if bucket.used >= limit {
            Some(midnight)
        } else {
            Some(slot)
        }
    };
    let rolling = budgets.today.plus(budgets.yesterday);
    let mut waits = vec![wait(budgets.daily_limit, budgets.today)];
    if let Some(cap) = budgets.provider_cap {
        waits.push(wait(cap, rolling));
    }
    if let Some(scope) = budgets.scope {
        let [today, yesterday] = scope.messages;
        if let Some(limit) = scope.messages_per_day {
            waits.push(wait(limit, today.plus(yesterday)));
        }
        let [today, yesterday] = scope.recipients;
        if let Some(limit) = scope.recipients_per_day {
            waits.push(wait(limit, today.plus(yesterday)));
        }
    }
    waits.into_iter().flatten().max()
}

/// The next midnight of UTC after `now` (a UTC day is 86,400 seconds of Unix time).
fn next_midnight(now: Timestamp) -> Timestamp {
    let day = now.as_second().div_euclid(DAY_SECONDS).saturating_add(1);
    Timestamp::from_second(day.saturating_mul(DAY_SECONDS)).unwrap_or(Timestamp::MAX)
}

/// A mailbox's provider cap over a rolling day, in messages: Gmail's 2,000 messages a paid user,
/// Exchange Online's 10,000 recipients (counted in messages: exact for one recipient each, and
/// beyond it Exchange's own quota refusal pauses the connection). A password login is capped by
/// what its SMTP host says it is; another server publishes no cap, and relays and the managed MTA
/// are bounded by their quota scopes and their own admission instead.
#[must_use]
pub fn provider_cap(provider: Provider, smtp_host: Option<&str>) -> Option<i64> {
    const GMAIL: i64 = 2_000;
    const EXCHANGE: i64 = 10_000;
    match provider {
        Provider::Google => Some(GMAIL),
        Provider::Microsoft => Some(EXCHANGE),
        Provider::Smtp => {
            let host = smtp_host?.trim_end_matches('.').to_ascii_lowercase();
            match host.as_str() {
                "smtp.gmail.com" | "smtp.googlemail.com" => Some(GMAIL),
                "smtp.office365.com" | "smtp-mail.outlook.com" => Some(EXCHANGE),
                _ => None,
            }
        }
        Provider::Ses | Provider::Sendgrid | Provider::Mailgun | Provider::Norbelys => None,
    }
}

/// Which rows a claim takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// Nothing.
    Nothing,
    /// A paced sender's one message: its oldest due row created through the API.
    Api,
    /// A paced sender's one message: its cold row, its clock being due and its windows open.
    Cold,
    /// A rate-paced connection's due rows, cold or not alike, in due order, at most this many.
    Due(u32),
}

/// What a claim takes from a connection with `allowance`: a paced sender takes one message, its
/// oldest due API mail first (`api_due`), else one cold message when `cold_ready` (its clock due,
/// a cold row due whose windows are open); a rate-paced connection takes up to its allowance.
#[must_use]
pub fn take(paced: bool, allowance: u32, api_due: bool, cold_ready: bool) -> Take {
    match (paced, allowance) {
        (_, 0) => Take::Nothing,
        (true, _) if api_due => Take::Api,
        (true, _) if cold_ready => Take::Cold,
        (true, _) => Take::Nothing,
        (false, allowance) => Take::Due(allowance),
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use strum::IntoEnumIterator as _;

    use super::{
        Bucket, Budgets, Room, ScopeBudget, Take, allowance, provider_cap, room, spent_until, take,
    };
    use crate::domain::senders::Provider;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn bucket(used: i64, reserved: i64) -> Bucket {
        Bucket { used, reserved }
    }

    /// The daily budget counts today alone, so a connection that spent all of yesterday has its
    /// whole limit today; a provider cap counts both days, so a cap of N never admits 2N across
    /// midnight; a scope's messages and recipients count both days too, and a scope out of
    /// recipients leaves no room at all.
    #[test]
    fn budgets_leave_room_by_their_own_windows() {
        let fresh = Budgets {
            daily_limit: 10,
            yesterday: bucket(10, 0),
            ..Budgets::default()
        };
        assert_eq!(
            room(&fresh).messages,
            10,
            "yesterday's limit spent, today's whole"
        );
        let capped = Budgets {
            daily_limit: 2_000,
            today: bucket(500, 20),
            yesterday: bucket(1_400, 0),
            provider_cap: Some(2_000),
            ..Budgets::default()
        };
        assert_eq!(room(&capped).messages, 80, "the rolling cap over both days");
        let scoped = Budgets {
            daily_limit: 100,
            scope: Some(ScopeBudget {
                messages_per_day: Some(10),
                recipients_per_day: Some(50),
                messages: [bucket(4, 1), bucket(3, 0)],
                recipients: [bucket(20, 5), bucket(10, 0)],
            }),
            ..Budgets::default()
        };
        assert_eq!(
            room(&scoped),
            Room {
                messages: 2,
                recipients: Some(15)
            }
        );
        let no_recipients = Budgets {
            daily_limit: 100,
            scope: Some(ScopeBudget {
                recipients_per_day: Some(30),
                recipients: [bucket(25, 5), bucket(0, 0)],
                ..ScopeBudget::default()
            }),
            ..Budgets::default()
        };
        assert_eq!(room(&no_recipients).messages, 0);
        let over = Budgets {
            daily_limit: 5,
            today: bucket(7, 0),
            ..Budgets::default()
        };
        assert_eq!(
            room(&over).messages,
            0,
            "never negative after a lowered limit"
        );
    }

    /// The allowance is the smallest of the budgets' room, the limiters' permits, twice the free
    /// slots and the eligible rows.
    #[test]
    fn the_allowance_is_the_smallest_bound() {
        let room = Room {
            messages: 40,
            recipients: None,
        };
        assert_eq!(allowance(&room, 100, 100, 100), 40);
        assert_eq!(allowance(&room, 7, 100, 100), 7);
        assert_eq!(allowance(&room, 100, 3, 100), 6);
        assert_eq!(allowance(&room, 100, 100, 2), 2);
        assert_eq!(allowance(&room, 100, 0, 100), 0);
    }

    /// A spent budget waits by what spent it: used units alone wait for the next UTC midnight at
    /// the connection's phase; reservations outstanding wait one slot; with several spent, the
    /// latest wait wins; a budget with room waits for nothing.
    #[test]
    fn a_spent_budget_waits_by_what_spent_it() {
        let now = at("2026-10-01T23:40:10Z");
        let used = Budgets {
            daily_limit: 10,
            today: bucket(10, 0),
            ..Budgets::default()
        };
        assert_eq!(
            spent_until(&used, now, 41),
            Some(at("2026-10-02T00:00:41Z"))
        );
        let reserved = Budgets {
            daily_limit: 10,
            today: bucket(9, 1),
            ..Budgets::default()
        };
        assert_eq!(
            spent_until(&reserved, now, 41),
            Some(at("2026-10-01T23:40:41Z"))
        );
        let both = Budgets {
            provider_cap: Some(15),
            yesterday: bucket(6, 0),
            ..reserved
        };
        assert_eq!(
            spent_until(&both, now, 41),
            Some(at("2026-10-02T00:00:41Z")),
            "the daily budget waits a slot, the cap's used units midnight: the latest wins"
        );
        let cap_used = Budgets {
            daily_limit: 100,
            today: bucket(10, 0),
            yesterday: bucket(5, 0),
            provider_cap: Some(15),
            ..Budgets::default()
        };
        assert_eq!(
            spent_until(&cap_used, now, 41),
            Some(at("2026-10-02T00:00:41Z"))
        );
        let open = Budgets {
            daily_limit: 10,
            today: bucket(3, 2),
            ..Budgets::default()
        };
        assert_eq!(spent_until(&open, now, 41), None);
    }

    /// Only mailboxes have a provider cap, and a password login only when its host is Gmail's or
    /// Exchange Online's; relays and the managed MTA have none.
    #[test]
    fn provider_caps_belong_to_mailboxes() {
        for provider in Provider::iter() {
            let expected = match provider {
                Provider::Google => Some(2_000),
                Provider::Microsoft => Some(10_000),
                Provider::Smtp
                | Provider::Ses
                | Provider::Sendgrid
                | Provider::Mailgun
                | Provider::Norbelys => None,
            };
            assert_eq!(provider_cap(provider, None), expected, "{provider:?}");
        }
        assert_eq!(
            provider_cap(Provider::Smtp, Some("SMTP.Gmail.com.")),
            Some(2_000)
        );
        assert_eq!(
            provider_cap(Provider::Smtp, Some("smtp.office365.com")),
            Some(10_000)
        );
        assert_eq!(provider_cap(Provider::Smtp, Some("mail.example.com")), None);
    }

    /// A paced sender takes one message, its API mail first, then a cold message when one is
    /// ready, and nothing without an allowance; a rate-paced connection takes its allowance.
    #[test]
    fn a_paced_sender_takes_one_message_api_mail_first() {
        for paced in [false, true] {
            for allowance in [0, 1, 5] {
                for api_due in [false, true] {
                    for cold_ready in [false, true] {
                        let expected = match (paced, allowance, api_due, cold_ready) {
                            (_, 0, _, _) => Take::Nothing,
                            (true, _, true, _) => Take::Api,
                            (true, _, false, true) => Take::Cold,
                            (true, _, false, false) => Take::Nothing,
                            (false, n, _, _) => Take::Due(n),
                        };
                        assert_eq!(take(paced, allowance, api_due, cold_ready), expected);
                    }
                }
            }
        }
    }
}
