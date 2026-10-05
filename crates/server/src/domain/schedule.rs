//! The sending grid: when a paced sender's cold mail may go, how its pacing clock moves, when the
//! send windows of a campaign and of a connection are open, and how a warming connection's daily
//! limit ramps up.
//!
//! # The grid
//!
//! Cold mail runs on 5-minute slots that start on the 5-minute marks of UTC (:00, :05, …). Every
//! paced sender (a mailbox, or an Amazon SES connection set to pace one From address) has a
//! **phase**, a fixed second inside each slot (0 to 299) chosen at random when it is connected, so
//! the expected cold sends of thousands of mailboxes spread across the slot instead of bunching at
//! the mark. A **phase instant** is a slot's start plus the phase; [`next_phase_at`] is the first
//! one at or after an instant, the same arithmetic as the SQL function of the same name, which the
//! store tests compare over every interval and delay.
//!
//! # The pacing clock
//!
//! A paced sender's `next_send_at` holds its next scheduled cold send, always a phase instant, and
//! **only ever moves forward**:
//!
//! - The Start of a cold send moves it once, to
//!   `next_phase_at(max(scheduled + interval, started + interval − 30 s), phase)`
//!   (checked by the `after_cold_start` test oracle). A Start within 30 seconds of its scheduled instant keeps the schedule
//!   exactly (5, 10 or 15 minutes apart; an interval that is not a multiple of 5 minutes rounds up
//!   to whole slots, so 17 becomes 20). A later Start re-anchors the schedule on itself, never
//!   sooner than the interval minus 30 seconds after it. A strict minimum between actual Starts
//!   would push every send one slot later, since no Start falls exactly on its instant (the claim,
//!   the rendering and the session come first), so the guarantee carries that grace.
//! - Every other writer (creating, resuming or reactivating a mailbox, an idle mailbox looked at
//!   once a slot, a closed window that opens later) takes `max(clock, next_phase_at(at, phase))`
//!   (checked by the `forward` test oracle), so nothing but a Start ever brings the next cold send closer.
//!
//! # Send windows
//!
//! A campaign and a connection may each set a window: days of the week and an opening and a
//! closing time on 5-minute marks, in an IANA time zone. Cold mail goes only while both are open;
//! mail created through the API is not held by either. A [`Window`] answers whether it is open at
//! an instant and when it next opens; [`next_open_together`] finds the first instant every window
//! of a message is open. Local times are resolved in the window's own zone, so a window keeps its
//! local hours through daylight-saving changes; an opening time that does not exist on a
//! spring-forward day opens at the first local instant after the gap.
//!
//! # Warm-up
//!
//! A warming connection may use only a share of its daily limit, by stage ([`WARMUP_SHARES`]): a
//! new mailbox starts at a tenth and reaches its whole limit over eight clean days, as deliverability
//! practitioners advise for a mailbox new to cold sending. A clean day (mail sent, no complaint)
//! advances the stage; a complaint holds it ([`next_warmup_stage`]). Past the last stage the
//! connection is warm and its stage is cleared.

use std::time::Duration;

use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp, ToSpan as _};

use super::senders::{SendWindow, minute_of_day};

/// The length of a slot of the grid, in seconds.
pub const SLOT_SECONDS: i64 = 300;
/// The origin the slots are counted from: 2001-01-01 00:00:00 UTC, a 5-minute mark, the origin
/// the SQL function bins from (in microseconds since the Unix epoch).
const ORIGIN_MICROS: i64 = 978_307_200_000_000;
const MICROS_PER_SECOND: i64 = 1_000_000;
const SLOT_MICROS: i64 = SLOT_SECONDS * MICROS_PER_SECOND;
/// How far ahead the next opening of a set of windows is looked for: a week and a day covers every
/// weekly window; windows that never open together within it never do.
const OPENING_HORIZON: Duration = Duration::from_secs(8 * 24 * 3_600);

/// The first phase instant at or after `at`: the start of a 5-minute slot of UTC plus
/// `phase_seconds`. An instant on the grid maps to itself. Computed on microseconds, the
/// precision the database stores, exactly as the SQL function `next_phase_at` bins:
/// `date_bin('5 minutes', at − phase − 1 µs, 2001-01-01) + 5 minutes + phase`.
#[must_use]
pub fn next_phase_at(at: Timestamp, phase_seconds: i32) -> Timestamp {
    let phase = i64::from(phase_seconds).saturating_mul(MICROS_PER_SECOND);
    let source = at
        .as_microsecond()
        .saturating_sub(phase)
        .saturating_sub(1)
        .saturating_sub(ORIGIN_MICROS);
    let binned = source
        .div_euclid(SLOT_MICROS)
        .saturating_mul(SLOT_MICROS)
        .saturating_add(ORIGIN_MICROS);
    micros(binned.saturating_add(SLOT_MICROS).saturating_add(phase))
}

/// The start of the slot `at` falls in.
#[must_use]
pub fn slot_of(at: Timestamp) -> Timestamp {
    let since = at.as_microsecond().saturating_sub(ORIGIN_MICROS);
    micros(
        since
            .div_euclid(SLOT_MICROS)
            .saturating_mul(SLOT_MICROS)
            .saturating_add(ORIGIN_MICROS),
    )
}

/// Where the Start of a cold send moves its sender's clock: the first phase instant at or after
/// both the scheduled instant plus the interval and the actual Start plus the interval minus the
/// 30-second grace. `scheduled` is the clock as it stood (the instant this send was scheduled
/// for), `started` the Start's own clock, `interval_minutes` the sender's interval as stored (whole
/// slots, as connections store it): the same arithmetic as the Start's SQL statement.
#[cfg(test)]
#[must_use]
pub fn after_cold_start(
    scheduled: Timestamp,
    started: Timestamp,
    interval_minutes: i32,
    phase_seconds: i32,
) -> Timestamp {
    let interval = minutes(interval_minutes);
    let on_schedule = plus(scheduled, interval);
    let anchored = plus(
        started,
        interval.saturating_sub(SignedDuration::from_secs(30)),
    );
    next_phase_at(on_schedule.max(anchored), phase_seconds)
}

/// A move of the clock by any writer but a Start: never backward. `at` is now (a creation, a
/// resume, an idle look) or the instant a closed window opens.
#[cfg(test)]
#[must_use]
pub fn forward(clock: Timestamp, at: Timestamp, phase_seconds: i32) -> Timestamp {
    clock.max(next_phase_at(at, phase_seconds))
}

/// How long after its scheduled instant a cold Start ran (zero when early), for the
/// `delivery_cold_start_lag_seconds` histogram: beyond 30 seconds the cadence re-anchors.
#[must_use]
pub fn cold_start_lag(scheduled: Timestamp, started: Timestamp) -> Duration {
    Duration::try_from(started.duration_since(scheduled)).unwrap_or(Duration::ZERO)
}

/// A send window in its time zone, ready to evaluate.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// Bit `n` set: the window opens on ISO weekday `n` (1 Monday to 7 Sunday).
    days: u8,
    /// The opening minute of the local day.
    opens: u32,
    /// The closing minute of the local day, after the opening; 1,440 closes at midnight.
    closes: u32,
    zone: TimeZone,
}

/// Why a stored window cannot be evaluated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WindowError {
    /// The days or the times are not what a window allows (checked when it was saved).
    #[error("the send window is malformed")]
    Malformed,
    /// The time zone is not an IANA zone this deployment knows.
    #[error("`{0}` is not a known IANA time zone")]
    Zone(String),
}

impl Window {
    /// The window `window` describes in the IANA zone `zone`.
    ///
    /// # Errors
    ///
    /// The window is malformed or the zone unknown; the caller treats such a window as closed,
    /// so a broken setting never lets mail out at a time nobody chose.
    pub fn new(window: &SendWindow, zone: &str) -> Result<Self, WindowError> {
        let mut days = 0_u8;
        for day in &window.days {
            if !(1..=7).contains(day) {
                return Err(WindowError::Malformed);
            }
            days |= 1_u8.checked_shl(u32::from(*day)).unwrap_or(0);
        }
        let opens = minute_of_day(&window.start).filter(|minute| *minute < 24 * 60);
        let closes = minute_of_day(&window.end);
        let (Some(opens), Some(closes)) = (opens, closes) else {
            return Err(WindowError::Malformed);
        };
        if days == 0 || opens >= closes {
            return Err(WindowError::Malformed);
        }
        let zone = TimeZone::get(zone).map_err(|_| WindowError::Zone(zone.to_owned()))?;
        Ok(Self {
            days,
            opens,
            closes,
            zone,
        })
    }

    fn on(&self, date: Date) -> bool {
        let weekday = u32::try_from(date.weekday().to_monday_one_offset()).unwrap_or(0);
        self.days & 1_u8.checked_shl(weekday).unwrap_or(0) != 0
    }

    /// True when the window is open at `at`: a day of the window, at or after its opening and
    /// before its closing, in local time.
    #[must_use]
    pub fn is_open(&self, at: Timestamp) -> bool {
        let local = at.to_zoned(self.zone.clone());
        let minute = u32::from(local.hour().unsigned_abs())
            .saturating_mul(60)
            .saturating_add(u32::from(local.minute().unsigned_abs()));
        self.on(local.date()) && self.opens <= minute && minute < self.closes
    }

    /// The first instant at or after `at` when the window is open: `at` itself when it is open,
    /// else its next opening within a week. `None` only for a zone whose local times cannot be
    /// resolved.
    #[must_use]
    pub fn next_open(&self, at: Timestamp) -> Option<Timestamp> {
        if self.is_open(at) {
            return Some(at);
        }
        let today = at.to_zoned(self.zone.clone()).date();
        let opening = Time::new(
            i8::try_from(self.opens / 60).ok()?,
            i8::try_from(self.opens % 60).ok()?,
            0,
            0,
        )
        .ok()?;
        (0..=7_i64)
            .filter_map(|offset| today.checked_add(offset.days()).ok())
            .filter(|date| self.on(*date))
            .filter_map(|date| self.zone.to_zoned(date.to_datetime(opening)).ok())
            .map(|zoned| zoned.timestamp())
            .find(|opens| *opens > at)
    }
}

/// True when every window in `windows` is open at `at` (no window: always open).
#[must_use]
pub fn open_together(windows: &[&Window], at: Timestamp) -> bool {
    windows.iter().all(|window| window.is_open(at))
}

/// The first instant at or after `at` when every window in `windows` is open; `None` when they
/// never open together within the next eight days (a campaign open on Mondays behind a connection
/// open on Tuesdays). Each step moves to the next opening of a window that is closed, so the
/// search ends.
#[must_use]
pub fn next_open_together(windows: &[&Window], at: Timestamp) -> Option<Timestamp> {
    let horizon = plus(at, SignedDuration::try_from(OPENING_HORIZON).ok()?);
    let mut candidate = at;
    while candidate <= horizon {
        match windows.iter().find(|window| !window.is_open(candidate)) {
            None => return Some(candidate),
            Some(closed) => candidate = closed.next_open(candidate)?,
        }
    }
    None
}

/// The share of its daily limit, in percent, a warming connection may use at each stage: from a
/// tenth to the whole limit over eight stages.
pub const WARMUP_SHARES: [u32; 8] = [10, 20, 30, 45, 60, 75, 90, 100];

/// The daily limit a connection may use today: its whole limit when it is not warming (`stage`
/// null), else its stage's share, rounded up and at least one message. A stage past the ramp uses
/// the whole limit.
#[must_use]
pub fn warmed_limit(daily_limit: i32, stage: Option<i16>) -> i32 {
    let Some(stage) = stage else {
        return daily_limit;
    };
    let share = usize::try_from(stage)
        .ok()
        .and_then(|index| WARMUP_SHARES.get(index))
        .copied()
        .unwrap_or(100);
    let limit = i64::from(daily_limit.max(1));
    let warmed = limit
        .saturating_mul(i64::from(share))
        .saturating_add(99)
        .saturating_div(100);
    i32::try_from(warmed.max(1)).unwrap_or(daily_limit)
}

/// How a warming connection's day went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmupDay {
    /// Messages the provider accepted that day.
    pub sent: u32,
    /// Complaints recorded about that day's mail.
    pub complaints: u32,
}

/// The stage after a day: a clean day (mail sent, no complaint) advances one stage, and past the
/// last stage the connection is warm (`None`); a day with a complaint, or with nothing sent (no
/// evidence either way), holds the stage. A connection that is not warming stays so.
#[must_use]
pub fn next_warmup_stage(stage: Option<i16>, day: WarmupDay) -> Option<i16> {
    let stage = stage?;
    if day.complaints > 0 || day.sent == 0 {
        return Some(stage);
    }
    let next = stage.saturating_add(1);
    let last = i16::try_from(WARMUP_SHARES.len()).unwrap_or(i16::MAX);
    (next < last).then_some(next)
}

fn micros(value: i64) -> Timestamp {
    Timestamp::from_microsecond(value).unwrap_or(if value < 0 {
        Timestamp::MIN
    } else {
        Timestamp::MAX
    })
}

#[cfg(test)]
fn minutes(count: i32) -> SignedDuration {
    SignedDuration::from_mins(i64::from(count))
}

fn plus(at: Timestamp, duration: SignedDuration) -> Timestamp {
    at.saturating_add(duration).unwrap_or(Timestamp::MAX)
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::{
        WARMUP_SHARES, WarmupDay, Window, WindowError, after_cold_start, cold_start_lag, forward,
        next_open_together, next_phase_at, next_warmup_stage, open_together, slot_of, warmed_limit,
    };
    use crate::domain::senders::SendWindow;

    fn at(text: &str) -> Timestamp {
        text.parse().expect("an RFC 3339 instant")
    }

    fn window(days: &[u8], start: &str, end: &str, zone: &str) -> Window {
        Window::new(
            &SendWindow::parse(days, start, end).expect("a valid window"),
            zone,
        )
        .expect("a known zone")
    }

    /// Phase instants are the 5-minute marks of UTC plus the phase: an instant on the grid maps to
    /// itself, a microsecond after it to the next slot, an instant before the phase to the same
    /// slot, and a window opening on a mark to the mark plus the phase (the cases the schema's
    /// own checks assert of the SQL function).
    #[test]
    fn phase_instants_are_marks_plus_the_phase() {
        for (instant, phase, wanted) in [
            ("2026-10-01T10:00:41Z", 41, "2026-10-01T10:00:41Z"),
            ("2026-10-01T10:00:41.000001Z", 41, "2026-10-01T10:05:41Z"),
            ("2026-10-01T10:00:12Z", 41, "2026-10-01T10:00:41Z"),
            ("2026-10-02T09:00:00Z", 41, "2026-10-02T09:00:41Z"),
            ("2026-10-01T10:04:59Z", 0, "2026-10-01T10:05:00Z"),
            ("2026-10-01T10:05:00Z", 0, "2026-10-01T10:05:00Z"),
            ("2026-10-01T10:00:00Z", 299, "2026-10-01T10:04:59Z"),
            ("1970-01-01T00:00:00Z", 10, "1970-01-01T00:00:10Z"),
        ] {
            assert_eq!(
                next_phase_at(at(instant), phase),
                at(wanted),
                "{instant} at phase {phase}"
            );
        }
        assert_eq!(
            slot_of(at("2026-10-01T10:07:13Z")),
            at("2026-10-01T10:05:00Z")
        );
    }

    /// The Start's move over named cases: a Start 3 s late keeps a 15-minute schedule exactly; a
    /// 5-minute sender started 29 s late keeps the next slot and one 31 s late re-anchors a slot
    /// later; 17 minutes become 20 and 16 never 15; a Start 116 s late lands 18 min 4 s after
    /// itself; a stale clock (the epoch: never sent) lands on the phase after the Start.
    #[test]
    fn the_start_moves_the_clock_by_the_named_cases() {
        for (scheduled, started, interval, wanted) in [
            (
                "2026-10-01T10:00:41Z",
                "2026-10-01T10:00:44Z",
                15,
                "2026-10-01T10:15:41Z",
            ),
            (
                "2026-10-01T10:00:41Z",
                "2026-10-01T10:01:10Z",
                5,
                "2026-10-01T10:05:41Z",
            ),
            (
                "2026-10-01T10:00:41Z",
                "2026-10-01T10:01:12Z",
                5,
                "2026-10-01T10:10:41Z",
            ),
            (
                "2026-10-01T10:00:41Z",
                "2026-10-01T10:00:44Z",
                17,
                "2026-10-01T10:20:41Z",
            ),
            (
                "2026-10-01T10:00:41Z",
                "2026-10-01T10:00:41Z",
                16,
                "2026-10-01T10:20:41Z",
            ),
            (
                "2026-10-01T10:00:41Z",
                "2026-10-01T10:02:37Z",
                15,
                "2026-10-01T10:20:41Z",
            ),
            (
                "1970-01-01T00:00:00Z",
                "2026-10-01T10:02:37Z",
                10,
                "2026-10-01T10:15:41Z",
            ),
        ] {
            assert_eq!(
                after_cold_start(at(scheduled), at(started), interval, 41),
                at(wanted),
                "{interval} min, scheduled {scheduled}, started {started}"
            );
        }
    }

    /// Over every interval from 5 to 60 minutes and every Start from 0 to 299 s after its
    /// scheduled instant (16,800 cases): the next send is a phase instant, never closer than the
    /// interval to the scheduled one; a Start within the 30-second grace keeps the schedule at
    /// exactly the interval rounded up to whole slots; from the actual Start, the next send is
    /// never sooner than the interval minus 30 s, nor later than the rounded interval plus a slot.
    #[test]
    fn the_start_keeps_its_guarantees_over_the_whole_sweep() {
        let scheduled = at("2026-10-01T10:00:41Z");
        let mut cases = 0;
        for interval in 5..=60_i32 {
            let rounded = jiff::SignedDuration::from_mins(i64::from((interval + 4) / 5 * 5));
            let minimum = jiff::SignedDuration::from_mins(i64::from(interval));
            for delay in 0..300_i64 {
                let started = scheduled
                    .checked_add(jiff::SignedDuration::from_secs(delay))
                    .unwrap();
                let next = after_cold_start(scheduled, started, interval, 41);
                cases += 1;
                assert_eq!(next, next_phase_at(next, 41), "not a phase instant");
                assert!(
                    next.duration_since(scheduled) >= minimum,
                    "closer than the interval"
                );
                if delay <= 30 {
                    assert_eq!(
                        next.duration_since(scheduled),
                        rounded,
                        "the schedule moved"
                    );
                }
                let since_start = next.duration_since(started);
                assert!(since_start >= minimum - jiff::SignedDuration::from_secs(30));
                assert!(since_start <= rounded + jiff::SignedDuration::from_mins(5));
            }
        }
        assert_eq!(cases, 16_800);
    }

    /// Every writer but the Start only moves the clock forward: an instant before the clock leaves
    /// it, a later one moves it to that instant's phase; the lag of a cold Start is zero when it
    /// ran early.
    #[test]
    fn other_writers_never_move_the_clock_back() {
        let clock = at("2026-10-01T10:20:41Z");
        assert_eq!(forward(clock, at("2026-10-01T10:10:00Z"), 41), clock);
        assert_eq!(
            forward(clock, at("2026-10-01T11:20:00Z"), 41),
            at("2026-10-01T11:20:41Z")
        );
        assert_eq!(
            cold_start_lag(clock, at("2026-10-01T10:21:00Z")),
            std::time::Duration::from_secs(19)
        );
        assert_eq!(
            cold_start_lag(clock, at("2026-10-01T10:20:00Z")),
            std::time::Duration::ZERO
        );
    }

    /// A window is evaluated in its own zone: Bogota's 09:00 to 17:00 on weekdays is open at
    /// 14:00 UTC on a Thursday and closed at 23:00 UTC; it next opens at 09:00 local the same
    /// day, or the following Monday after Friday's close, and an open window opens "now".
    #[test]
    fn a_window_opens_and_closes_in_its_own_zone() {
        let bogota = window(&[1, 2, 3, 4, 5], "09:00", "17:00", "America/Bogota");
        // 2026-10-01 is a Thursday; Bogota is UTC-5 all year.
        assert!(bogota.is_open(at("2026-10-01T14:00:00Z")));
        assert!(!bogota.is_open(at("2026-10-01T23:00:00Z")));
        assert!(
            !bogota.is_open(at("2026-10-01T22:00:00Z")),
            "17:00 local is closed"
        );
        assert!(bogota.is_open(at("2026-10-01T21:55:00Z")));
        assert_eq!(
            bogota.next_open(at("2026-10-01T10:00:00Z")),
            Some(at("2026-10-01T14:00:00Z"))
        );
        assert_eq!(
            bogota.next_open(at("2026-10-02T23:00:00Z")),
            Some(at("2026-10-05T14:00:00Z")),
            "Friday after closing: Monday morning"
        );
        assert_eq!(
            bogota.next_open(at("2026-10-01T15:00:00Z")),
            Some(at("2026-10-01T15:00:00Z"))
        );
    }

    /// Local hours survive daylight-saving changes: New York's 09:00 opens at 13:00 UTC in
    /// summer and 14:00 UTC in winter, and an opening inside the spring-forward gap opens at the
    /// first local instant after it.
    #[test]
    fn a_window_keeps_its_local_hours_through_daylight_saving() {
        let new_york = window(&[1, 2, 3, 4, 5, 6, 7], "09:00", "17:00", "America/New_York");
        assert_eq!(
            new_york.next_open(at("2026-07-01T05:00:00Z")),
            Some(at("2026-07-01T13:00:00Z"))
        );
        assert_eq!(
            new_york.next_open(at("2026-12-01T05:00:00Z")),
            Some(at("2026-12-01T14:00:00Z"))
        );
        let gap = window(&[7], "02:30", "04:00", "America/New_York");
        // 2026-03-08 (a Sunday) at 02:00 local jumps to 03:00.
        assert_eq!(
            gap.next_open(at("2026-03-08T05:00:00Z")),
            Some(at("2026-03-08T07:30:00Z")),
            "02:30 does not exist; 03:30 EDT is 07:30 UTC"
        );
    }

    /// Cold mail needs every window open: a campaign open 09:00 to 17:00 behind a connection open
    /// from 09:30 opens together at 09:30; windows that never meet within the horizon never open
    /// together; no window at all is always open.
    #[test]
    fn cold_windows_open_together() {
        let campaign = window(&[1, 2, 3, 4, 5], "09:00", "17:00", "UTC");
        let connection = window(&[1, 2, 3, 4, 5], "09:30", "18:00", "UTC");
        assert_eq!(
            next_open_together(&[&campaign, &connection], at("2026-10-01T08:00:00Z")),
            Some(at("2026-10-01T09:30:00Z"))
        );
        assert!(open_together(
            &[&campaign, &connection],
            at("2026-10-01T10:00:00Z")
        ));
        assert!(!open_together(
            &[&campaign, &connection],
            at("2026-10-01T09:10:00Z")
        ));
        let monday = window(&[1], "09:00", "10:00", "UTC");
        let tuesday = window(&[2], "09:00", "10:00", "UTC");
        assert_eq!(
            next_open_together(&[&monday, &tuesday], at("2026-10-01T08:00:00Z")),
            None
        );
        assert!(open_together(&[], at("2026-10-01T03:00:00Z")));
        assert_eq!(
            next_open_together(&[], at("2026-10-01T03:00:00Z")),
            Some(at("2026-10-01T03:00:00Z"))
        );
    }

    /// A stored window that is malformed or names an unknown zone is refused, so the sender
    /// treats it as closed rather than guessing.
    #[test]
    fn a_broken_window_is_refused() {
        let ok = SendWindow::parse(&[1], "09:00", "17:00").unwrap();
        assert_eq!(
            Window::new(&ok, "Mars/Olympus"),
            Err(WindowError::Zone("Mars/Olympus".to_owned()))
        );
        let reversed = SendWindow {
            days: vec![1],
            start: "17:00".to_owned(),
            end: "09:00".to_owned(),
        };
        assert_eq!(Window::new(&reversed, "UTC"), Err(WindowError::Malformed));
        let no_day = SendWindow {
            days: vec![9],
            start: "09:00".to_owned(),
            end: "17:00".to_owned(),
        };
        assert_eq!(Window::new(&no_day, "UTC"), Err(WindowError::Malformed));
    }

    /// The warm-up ramp: no stage means the whole limit; each stage its share, rounded up and at
    /// least one message; a stage past the ramp the whole limit. A clean day advances one stage
    /// and the last stage ends the warm-up; a complaint or a day without mail holds the stage.
    #[test]
    fn warming_ramps_the_daily_limit_by_stage() {
        assert_eq!(warmed_limit(50, None), 50);
        let ramp: Vec<i32> = (0..8).map(|stage| warmed_limit(50, Some(stage))).collect();
        assert_eq!(ramp, [5, 10, 15, 23, 30, 38, 45, 50]);
        assert_eq!(warmed_limit(3, Some(0)), 1);
        assert_eq!(warmed_limit(50, Some(40)), 50);
        assert_eq!(WARMUP_SHARES.last(), Some(&100));
        let clean = WarmupDay {
            sent: 12,
            complaints: 0,
        };
        assert_eq!(next_warmup_stage(Some(0), clean), Some(1));
        assert_eq!(next_warmup_stage(Some(6), clean), Some(7));
        assert_eq!(next_warmup_stage(Some(7), clean), None, "warm");
        assert_eq!(
            next_warmup_stage(
                Some(3),
                WarmupDay {
                    sent: 12,
                    complaints: 1
                }
            ),
            Some(3)
        );
        assert_eq!(
            next_warmup_stage(
                Some(3),
                WarmupDay {
                    sent: 0,
                    complaints: 0
                }
            ),
            Some(3)
        );
        assert_eq!(next_warmup_stage(None, clean), None);
    }
}
