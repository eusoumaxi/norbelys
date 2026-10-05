//! The one backoff of the codebase: how long a unit of work waits before it is tried again
//! after a failure. Every retrying mechanism (the job runner, message retries, the breakers of
//! connections and quota scopes, calls to AI providers, the tracking drain, failed inbox polls
//! and the checks of a sending domain the managed MTA is not ready for) takes its waits from
//! here, so the formula exists once and only the named policies differ.
//!
//! The formula is `backoff(n, p) = uniform(p.floor, min(p.cap, p.base × 2ⁿ))`, where `n`
//! counts the unit's earlier failures (0 for its first retry) and `p` is a policy. It is "full
//! jitter above a minimum":
//!
//! - full jitter, because a thousand units that failed together (a database restart, a
//!   provider outage) then spread across the whole step instead of returning at the same
//!   instant; it does the least total work among the usual jitter strategies
//!   (<https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/>);
//! - a minimum, because some peers forbid immediate retries;
//! - the minimum is the lower bound of the random draw, never a clamp applied afterwards:
//!   clamping `max(floor, uniform(0, step))` would put every draw below the floor on the floor
//!   itself, sending a large share of a failed wave back at one instant.
//!
//! Customer webhooks do not double: they follow the retry schedule published by the Standard
//! Webhooks specification (<https://www.standardwebhooks.com/>), which consumers already
//! expect. The wait after a delivery's `n`-th failed attempt is drawn between the schedule's
//! `n`-th step and that step plus a tenth, so a consumer always gets at least the published
//! wait while deliveries that failed together still spread out.
//!
//! The random draw is an argument, so every function here is pure and its edge cases can be
//! checked without a random source. Arithmetic saturates: a huge failure count or policy never
//! overflows, it reaches the cap.

use std::time::Duration;

/// A named backoff policy: the doubling's first step, its largest step, and the smallest wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// The step before the first doubling (the upper bound of the first retry's draw).
    pub base: Duration,
    /// The largest step: the upper bound of every draw.
    pub cap: Duration,
    /// The smallest wait: the lower bound of every draw.
    pub floor: Duration,
}

/// The generic job runner's policy: a failed job waits between one second and a minute, then
/// up to two, four, … minutes, never more than an hour.
pub const JOBS: Policy = Policy {
    base: Duration::from_secs(60),
    cap: Duration::from_secs(3_600),
    floor: Duration::from_secs(1),
};

/// Message retries and breaker steps: a message refused for now waits between 30 seconds and a
/// minute, then up to two, four, … minutes, never more than an hour; a connection or a quota
/// scope whose breaker opens stays open as long. The floor is larger than the runner's because a
/// mailbox provider's temporary refusal rarely clears in seconds, every attempt still counts
/// against the provider's limits, and Gmail and Microsoft Graph forbid immediate retries.
pub const DELIVERY: Policy = Policy {
    base: Duration::from_secs(60),
    cap: Duration::from_secs(3_600),
    floor: Duration::from_secs(30),
};

/// Calls to an AI provider, within one job's lease: a call refused for now (a rate limit, a failing
/// provider, a provider that could not be reached) waits between half a second and a second, then up
/// to two seconds, never more than eight; [`AI_RETRIES`] retries at most. The provider's own
/// `Retry-After` takes precedence over this wait, and a wait that would pass the call's deadline
/// ends the call instead, so the job can hold its use case back as long as the provider asks.
pub const AI: Policy = Policy {
    base: Duration::from_secs(1),
    cap: Duration::from_secs(8),
    floor: Duration::from_millis(500),
};

/// The most retries of one AI call after its first attempt: a third failure in a row says the
/// provider needs longer than one call's deadline to recover.
pub const AI_RETRIES: u32 = 2;

/// The tracking drain's policy: a batch the database refused (it is unavailable, or a month's
/// partition is missing) is tried again within a second, then up to two, four, … seconds, never
/// more than half a minute apart, so the drain resumes soon after the database does while a long
/// outage costs one attempt every few seconds.
pub const DRAIN: Policy = Policy {
    base: Duration::from_secs(1),
    cap: Duration::from_secs(30),
    floor: Duration::from_millis(500),
};

/// The longest wait of [`inbox`]: a mailbox whose polls keep failing is still tried hourly.
const INBOX_CAP: Duration = Duration::from_secs(3_600);

/// Inbox polls after a failure, for a binding polled every `interval`: the first retry waits
/// between one interval and two, the next up to four intervals, then eight, … never more than an
/// hour. The floor is the interval itself, so a failing mailbox is never asked more often than a
/// healthy one (an interval above the hour waits the hour). A provider outage fails every
/// mailbox of that provider at once; the jitter spreads their retries across the step instead of
/// returning them together.
#[must_use]
pub fn inbox(interval: Duration) -> Policy {
    Policy {
        base: interval.saturating_mul(2),
        cap: INBOX_CAP,
        floor: interval.min(INBOX_CAP),
    }
}

/// Checks of a sending domain the managed MTA is not ready for (it has not verified the domain,
/// its DKIM key does not exist yet, or it could not be reached): five minutes after the first
/// such check, then up to ten, twenty, … minutes, never more than a day, the cadence at which
/// every proven domain is checked anyway. A domain the MTA verifies soon is ready within minutes;
/// one it never verifies costs a check a day instead of one every five minutes, forever.
pub const MTA_DOMAIN: Policy = Policy {
    base: Duration::from_secs(5 * 60),
    cap: Duration::from_secs(86_400),
    floor: Duration::from_secs(5 * 60),
};

/// The waits between a webhook delivery's attempts, as the Standard Webhooks specification
/// publishes them: 5 seconds, 5 minutes, 30 minutes, 2, 5, 10, 14, 20 and 24 hours after the
/// previous attempt; ten attempts and 75 h 35 min 5 s in all, before jitter.
pub const WEBHOOKS: [Duration; 9] = [
    Duration::from_secs(5),
    Duration::from_secs(5 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(2 * 3_600),
    Duration::from_secs(5 * 3_600),
    Duration::from_secs(10 * 3_600),
    Duration::from_secs(14 * 3_600),
    Duration::from_secs(20 * 3_600),
    Duration::from_secs(24 * 3_600),
];

/// The wait before the next try of a unit that has already failed `failures + 1` times
/// (`failures` is 0 for the first retry), for a uniformly random `draw`.
#[must_use]
pub fn backoff(failures: u32, policy: &Policy, draw: u64) -> Duration {
    let floor = millis(policy.floor);
    let doubled =
        millis(policy.base).saturating_mul(1_u64.checked_shl(failures).unwrap_or(u64::MAX));
    let step = doubled.min(millis(policy.cap)).max(floor);
    uniform(floor, step, draw)
}

/// The wait after a webhook delivery's `failed_attempts`-th failed attempt (1 for the first
/// failure), for a uniformly random `draw`. `None` once the schedule is exhausted: the delivery
/// then fails for good.
#[must_use]
pub fn webhook_delay(failed_attempts: u32, draw: u64) -> Option<Duration> {
    let index = usize::try_from(failed_attempts).ok()?.checked_sub(1)?;
    let step = millis(*WEBHOOKS.get(index)?);
    Some(uniform(step, step.saturating_add(step / 10), draw))
}

/// A duration in `[low, high]` milliseconds chosen by `draw` (the modulo bias of a 64-bit draw
/// over spans of hours is negligible).
fn uniform(low: u64, high: u64, draw: u64) -> Duration {
    let span = high.saturating_sub(low).saturating_add(1);
    Duration::from_millis(low.saturating_add(draw % span))
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{AI, DELIVERY, JOBS, MTA_DOMAIN, WEBHOOKS, backoff, inbox, webhook_delay};

    /// Every draw of each named policy (the runner's, the deliveries', the AI calls', a failing
    /// mailbox's, an unready domain's) lies between its floor and the doubled step capped by the
    /// policy: the floor bounds the draw instead of clamping it (the smallest draw is the floor,
    /// not a pile-up below it), and an absurd failure count reaches the cap without overflowing.
    #[test]
    fn backoff_stays_between_the_floor_and_the_capped_step() {
        for policy in [
            JOBS,
            DELIVERY,
            AI,
            MTA_DOMAIN,
            inbox(Duration::from_secs(300)),
        ] {
            for failures in [0_u32, 1, 2, 5, 6, 7, 31, 32, 63, 64, 200] {
                let step = policy
                    .base
                    .saturating_mul(2_u32.saturating_pow(failures))
                    .min(policy.cap);
                for draw in [0, 1, 999, 59_999, 60_000, u64::MAX / 2, u64::MAX] {
                    let wait = backoff(failures, &policy, draw);
                    assert!(
                        wait >= policy.floor && wait <= step,
                        "{failures} failures, draw {draw}: {wait:?} outside [{:?}, {step:?}]",
                        policy.floor
                    );
                }
            }
            assert_eq!(backoff(0, &policy, 0), policy.floor);
        }
    }

    /// A failed inbox poll waits at least the binding's own interval, so a failing mailbox is
    /// never asked more often than a healthy one; at most twice the interval after its first
    /// failure; and an hour at most however long it keeps failing. An interval beyond the hour
    /// waits exactly the hour.
    #[test]
    fn a_failing_mailbox_waits_from_its_interval_up_to_an_hour() {
        let interval = Duration::from_secs(300);
        let policy = inbox(interval);
        assert_eq!(policy.floor, interval);
        assert_eq!(backoff(0, &policy, 0), interval);
        assert!(backoff(0, &policy, u64::MAX) <= interval * 2);
        assert!(backoff(40, &policy, u64::MAX) <= Duration::from_secs(3_600));
        let slow = inbox(Duration::from_secs(7_200));
        for draw in [0, 1, u64::MAX] {
            assert_eq!(backoff(0, &slow, draw), Duration::from_secs(3_600));
        }
    }

    /// Webhook waits follow the published Standard Webhooks schedule: after the n-th failure
    /// the wait lies between the schedule's n-th step and that step plus a tenth, and after the
    /// tenth attempt there is no next one (the delivery fails).
    #[test]
    fn webhook_delays_follow_the_published_schedule() {
        for (index, step) in WEBHOOKS.iter().enumerate() {
            let failed = u32::try_from(index + 1).unwrap();
            assert_eq!(webhook_delay(failed, 0), Some(*step));
            let longest = webhook_delay(failed, u64::MAX).unwrap();
            assert!(
                longest >= *step && longest <= *step + *step / 10,
                "step {failed}: {longest:?}"
            );
        }
        assert_eq!(webhook_delay(0, 0), None);
        assert_eq!(webhook_delay(10, 0), None);
    }
}
