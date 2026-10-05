//! Periodic jobs: cron expressions in `job_schedules`, and the loop that enqueues them.
//!
//! A periodic kind declares its cron expression in code ([`super::Job::schedule`]). When a
//! worker starts, [`seed`] writes the registered schedules into `job_schedules` through the
//! `norbelys_system` pool (the worker login may read and advance schedules but not create
//! them); an operator may still switch one off with `enabled`, which seeding never touches.
//!
//! Every worker runs the schedule loop, about once a second. A tick competes for a
//! transaction-scoped advisory lock keyed on the table, so one worker at a time does the work
//! and a dead holder releases it with its transaction. The holder reads the due schedules,
//! enqueues each kind's singleton job in the `system` workspace and moves `next_run_at` to the
//! next instant after now, in the same transaction. A missed tick (no worker was running) is
//! therefore caught up once, never replayed once per missed instant, and a run still in
//! progress coalesces with the next tick through the job's unique key.
//!
//! Sending and the periodic jobs share one grid: every schedule is expressed in UTC on
//! 5-minute marks, except the outbox relay, which publishes customer webhooks every 5 seconds
//! because their latency is the product's. Daily jobs are staggered across the first hour.
//!
//! The cron dialect is the classic five fields (minute, hour, day of month, month, day of
//! week), optionally preceded by a seconds field; each field is `*`, a number, a range `a-b`, a
//! step `*/n` or `a-b/n`, or a comma list of those. Days of the week run from 0 (Sunday) to 6,
//! and 7 is Sunday too. When both day fields are restricted, a day matching either one runs,
//! as in cron. Names (`MON`, `JAN`) and the extensions `L`, `W`, `#` and `?` are refused.

use jiff::civil::{DateTime, Time};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp as Instant};

use super::{NewJob, Queue, Registry, SYSTEM_WORKSPACE};
use crate::db::{self, Database};
use crate::domain::time::Timestamp;

/// The longest search for a cron expression's next instant; a satisfiable expression finds one
/// within a few thousand steps (February 29th, four years ahead, is the worst).
const SEARCH_STEPS: u32 = 100_000;

/// A parsed cron expression, evaluated in UTC: one bit per allowed value of each field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    seconds: u64,
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    any_day: bool,
    any_weekday: bool,
}

/// Why a cron expression was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid cron expression `{expression}`: {reason}")]
pub struct CronError {
    expression: String,
    reason: &'static str,
}

impl Cron {
    /// Parses five fields, or six with a leading seconds field.
    ///
    /// # Errors
    ///
    /// A field is malformed or out of its range, or the count of fields is wrong.
    pub fn parse(expression: &str) -> Result<Self, CronError> {
        let error = |reason| CronError {
            expression: expression.to_owned(),
            reason,
        };
        let fields: Vec<&str> = expression.split_whitespace().collect();
        let (seconds, rest) = match fields.as_slice() {
            [second, rest @ ..] if rest.len() == 5 => (field(second, 0, 59).map_err(error)?, rest),
            rest if rest.len() == 5 => (1, rest),
            _ => {
                return Err(error(
                    "a cron expression has five fields, or six with seconds first",
                ));
            }
        };
        let [minute, hour, day, month, weekday] = rest else {
            return Err(error(
                "a cron expression has five fields, or six with seconds first",
            ));
        };
        let weekdays = field(weekday, 0, 7).map_err(error)?;
        Ok(Self {
            seconds,
            minutes: field(minute, 0, 59).map_err(error)?,
            hours: field(hour, 0, 23).map_err(error)?,
            days: field(day, 1, 31).map_err(error)?,
            months: field(month, 1, 12).map_err(error)?,
            // Day 7 is Sunday too.
            weekdays: (weekdays | (weekdays >> 7)) & 0x7f,
            any_day: *day == "*",
            any_weekday: *weekday == "*",
        })
    }

    /// The first instant strictly after `after`, in whole seconds, that the expression
    /// matches; `None` for an expression no date satisfies (February 30th).
    #[must_use]
    pub fn next_after(&self, after: Instant) -> Option<Instant> {
        let start = after.checked_add(Span::new().seconds(1)).ok()?;
        let mut t = start
            .to_zoned(TimeZone::UTC)
            .datetime()
            .with()
            .subsec_nanosecond(0)
            .build()
            .ok()?;
        for _ in 0..SEARCH_STEPS {
            t = if !bit(self.months, t.month()) {
                t.first_of_month()
                    .date()
                    .checked_add(Span::new().months(1))
                    .ok()?
                    .to_datetime(Time::midnight())
            } else if !self.day_matches(t) {
                t.date().tomorrow().ok()?.to_datetime(Time::midnight())
            } else if !bit(self.hours, t.hour()) {
                t.with()
                    .minute(0)
                    .second(0)
                    .build()
                    .ok()?
                    .checked_add(Span::new().hours(1))
                    .ok()?
            } else if !bit(self.minutes, t.minute()) {
                t.with()
                    .second(0)
                    .build()
                    .ok()?
                    .checked_add(Span::new().minutes(1))
                    .ok()?
            } else if !bit(self.seconds, t.second()) {
                t.checked_add(Span::new().seconds(1)).ok()?
            } else {
                return t
                    .to_zoned(TimeZone::UTC)
                    .ok()
                    .map(|zoned| zoned.timestamp());
            };
        }
        None
    }

    fn day_matches(&self, t: DateTime) -> bool {
        let day = bit(self.days, t.day());
        let weekday = bit(self.weekdays, t.weekday().to_sunday_zero_offset());
        match (self.any_day, self.any_weekday) {
            (true, true) => true,
            (true, false) => weekday,
            (false, true) => day,
            (false, false) => day || weekday,
        }
    }
}

/// One field's allowed values as a bit mask.
fn field(text: &str, min: u32, max: u32) -> Result<u64, &'static str> {
    let mut mask = 0_u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (
                range,
                step.parse::<u32>()
                    .ok()
                    .filter(|step| *step > 0)
                    .ok_or("a step is a positive number")?,
            ),
            None => (part, 1),
        };
        let (low, high) = if range == "*" {
            (min, max)
        } else if let Some((low, high)) = range.split_once('-') {
            (number(low)?, number(high)?)
        } else if part.contains('/') {
            // `a/n` runs from `a` to the end of the field.
            (number(range)?, max)
        } else {
            let value = number(range)?;
            (value, value)
        };
        if low < min || high > max || low > high {
            return Err("a value is outside its field's range");
        }
        let mut value = low;
        while value <= high {
            mask |= 1_u64.checked_shl(value).unwrap_or(0);
            value = value.saturating_add(step);
        }
    }
    Ok(mask)
}

fn number(text: &str) -> Result<u32, &'static str> {
    text.parse::<u32>()
        .map_err(|_| "a field holds numbers, ranges, steps and lists only")
}

fn bit(mask: u64, value: i8) -> bool {
    u32::try_from(value).is_ok_and(|value| mask & 1_u64.checked_shl(value).unwrap_or(0) != 0)
}

/// Writes the registered schedules into `job_schedules`, one row per periodic kind named after
/// it. A new row is due at once (a fresh install runs every periodic kind on its first tick); a
/// changed expression also runs at once and then follows the new expression; `enabled` and
/// `last_run_at` are left alone.
///
/// # Errors
///
/// The database refused a row.
pub async fn seed(system: &Database, registry: &Registry) -> Result<usize, sqlx::Error> {
    let mut tx = system.begin().await?;
    let mut seeded = 0;
    for (kind, scheduled) in registry.scheduled() {
        sqlx::query!(
            "INSERT INTO job_schedules (name, kind, payload, cron, next_run_at) VALUES ($1, $1, $2, $3, now())
             ON CONFLICT (name) DO UPDATE SET kind = EXCLUDED.kind, payload = EXCLUDED.payload, cron = EXCLUDED.cron,
                    next_run_at = CASE WHEN job_schedules.cron = EXCLUDED.cron THEN job_schedules.next_run_at ELSE now() END",
            kind,
            scheduled.payload,
            scheduled.expression,
        )
        .execute(&mut *tx)
        .await?;
        seeded += 1;
    }
    tx.commit().await?;
    Ok(seeded)
}

/// One tick of the schedule loop: when this worker holds the advisory lock, enqueues every due
/// schedule's job in the `system` workspace and advances the schedule, all in one transaction.
/// Returns the queues that received work, for the caller to wake after the commit. A schedule
/// whose kind this worker does not run is left due for a worker that does.
///
/// # Errors
///
/// The database failed; nothing was enqueued or advanced.
pub(super) async fn tick(db: &Database, registry: &Registry) -> Result<Vec<Queue>, sqlx::Error> {
    let mut tx = db.begin().await?;
    let held = sqlx::query_scalar!(
        r#"SELECT pg_try_advisory_xact_lock('job_schedules'::regclass::oid::bigint) AS "held!""#
    )
    .fetch_one(&mut *tx)
    .await?;
    if !held {
        tx.commit().await?;
        return Ok(Vec::new());
    }
    db::set_workspace(&mut tx, SYSTEM_WORKSPACE).await?;
    let now = sqlx::query_scalar!(r#"SELECT now() AS "now!: Timestamp""#)
        .fetch_one(&mut *tx)
        .await?;
    let due = sqlx::query!("SELECT name, kind FROM job_schedules WHERE enabled AND next_run_at <= now() ORDER BY next_run_at FOR UPDATE")
        .fetch_all(&mut *tx)
        .await?;
    let mut woken = Vec::new();
    for schedule in due {
        let Some((kind, scheduled)) = registry
            .scheduled()
            .find(|(kind, _)| *kind == schedule.kind)
        else {
            // Normal during a rolling release, when an older or newer worker runs the kind; logged
            // at debug level because this repeats on every tick this worker wins.
            tracing::debug!(schedule = %schedule.name, kind = %schedule.kind, "a schedule names a kind this worker does not run");
            continue;
        };
        let Some(entry) = registry.get(kind) else {
            continue;
        };
        let new = NewJob {
            kind,
            queue: entry.queue,
            max_attempts: entry.max_attempts,
            payload: scheduled.payload.clone(),
            unique_key: scheduled.unique_key.clone(),
        };
        super::enqueue_value(&mut tx, SYSTEM_WORKSPACE, &new, None).await?;
        let next = scheduled.cron.next_after(now.0).map(Timestamp);
        sqlx::query!(
            "UPDATE job_schedules SET last_run_at = now(), next_run_at = coalesce($2::timestamptz, 'infinity'::timestamptz) WHERE name = $1",
            schedule.name,
            next as _,
        )
        .execute(&mut *tx)
        .await?;
        if !woken.contains(&entry.queue) {
            woken.push(entry.queue);
        }
    }
    tx.commit().await?;
    Ok(woken)
}

#[cfg(test)]
mod tests {
    use super::Cron;

    /// The next instant after `after` (RFC 3339) as RFC 3339.
    fn next(expression: &str, after: &str) -> Option<String> {
        Cron::parse(expression)
            .unwrap()
            .next_after(after.parse().unwrap())
            .map(|instant| instant.to_string())
    }

    /// Malformed expressions are refused instead of being read as something else: a wrong number
    /// of fields, values outside their field, inverted ranges, zero steps, empty list items,
    /// names and the extensions this dialect does not have.
    #[test]
    fn malformed_expressions_are_refused() {
        for expression in [
            "",
            "* * * *",
            "* * * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "5-1 * * * *",
            "*/0 * * * *",
            "1,,2 * * * *",
            "-1 * * * *",
            "* * * JAN *",
            "* * L * *",
            "* * ? * *",
        ] {
            assert!(
                Cron::parse(expression).is_err(),
                "`{expression}` was accepted"
            );
        }
    }

    /// The product's schedules land on their marks: the outbox relay on 5-second marks, the grid
    /// on 5-minute marks (also across midnight and the year), the partitions at 00:05 UTC. The
    /// next instant is strictly after the given one, even when that one matches, so a schedule
    /// never runs twice for one mark.
    #[test]
    fn the_product_schedules_land_on_their_marks() {
        assert_eq!(
            next("*/5 * * * * *", "2026-10-02T04:12:03.4Z").as_deref(),
            Some("2026-10-02T04:12:05Z")
        );
        assert_eq!(
            next("*/5 * * * * *", "2026-10-02T04:12:05Z").as_deref(),
            Some("2026-10-02T04:12:10Z")
        );
        assert_eq!(
            next("*/5 * * * *", "2026-10-02T04:12:00Z").as_deref(),
            Some("2026-10-02T04:15:00Z")
        );
        assert_eq!(
            next("*/5 * * * *", "2026-10-02T23:58:00Z").as_deref(),
            Some("2026-10-03T00:00:00Z")
        );
        assert_eq!(
            next("5 0 * * *", "2026-10-02T00:05:00Z").as_deref(),
            Some("2026-10-03T00:05:00Z")
        );
        assert_eq!(
            next("0 * * * *", "2026-12-31T23:30:00Z").as_deref(),
            Some("2027-01-01T00:00:00Z")
        );
    }

    /// The day fields follow cron: with both restricted a day matching either runs, with one
    /// restricted only it counts, 7 is Sunday like 0, a leap day is found years ahead, and a
    /// date that never exists (February 30th) yields no instant instead of searching forever.
    #[test]
    fn day_fields_follow_cron() {
        // 2026-10-02 is a Friday.
        assert_eq!(
            next("0 0 13 * 1", "2026-10-02T00:00:00Z").as_deref(),
            Some("2026-10-05T00:00:00Z")
        );
        assert_eq!(
            next("0 0 13 * *", "2026-10-02T00:00:00Z").as_deref(),
            Some("2026-10-13T00:00:00Z")
        );
        assert_eq!(
            next("0 0 * * 0", "2026-10-02T00:00:00Z").as_deref(),
            Some("2026-10-04T00:00:00Z")
        );
        assert_eq!(
            next("0 0 * * 7", "2026-10-02T00:00:00Z").as_deref(),
            Some("2026-10-04T00:00:00Z")
        );
        assert_eq!(
            next("0 0 29 2 *", "2026-10-02T00:00:00Z").as_deref(),
            Some("2028-02-29T00:00:00Z")
        );
        assert_eq!(next("0 0 30 2 *", "2026-10-02T00:00:00Z"), None);
    }
}
