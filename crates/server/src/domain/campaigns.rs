//! The rules of campaigns and enrollments that need no database: the lifecycles, what happens
//! to a conversation whose message ended or whose sender left the pool, how a sender is picked
//! from the pool, and when an enrollment's next step runs.
//!
//! # Lifecycles
//!
//! A campaign is `draft` until it is started. `start` (from `draft` or `paused`) makes it
//! `materialising`: its job checks it, makes it `active` and creates the messages already due.
//! `pause` (from `active` or `materialising`) stops new messages; messages already queued wait,
//! because the Start of a message of a paused campaign returns it to the queue unstarted. An
//! `active` campaign whose every enrollment ended is `completed`, and enrolling people again
//! makes it `active`. Deleting a campaign that has sent archives it: `archived` is final and
//! read-only.
//!
//! An enrollment is `active` (or `paused`, which only a later rule sets) while it is live; it
//! ends `completed` after its last step, `replied` when the person answered, `stopped` by a person,
//! a stop rule, a suppression or a removed sender, or `failed` when a step could not be sent.
//!
//! # A step's message, after it ends
//!
//! An enrollment points at its current step's message (`message_id`) until that message reaches
//! a final state; then [`settle`] decides what follows:
//!
//! | The message | What follows |
//! |---|---|
//! | `sent` | the next step, due its delay after the actual send, or `completed` after the last |
//! | `failed` without a submission (it expired unstarted, say) | the step is tried again with a new message, after a backoff, at most [`UNSTARTED_FAILURES_MAX`] times, then `failed` |
//! | `failed` after a submission (a provider refused it for good) | `failed`, with the message's reason |
//! | `suppressed` | `stopped`: the address may not be mailed |
//! | `cancelled` (by a person) | `stopped` |
//! | `queued`, `claimed`, `in_flight`, `uncertain` | nothing yet: an uncertain message may have been sent, so no follow-up goes out until it is resolved |
//!
//! # A sender that leaves the pool
//!
//! A conversation keeps its sender (its affinity) while the sender is only unavailable (paused,
//! breaker open, budget spent): it waits. When the sender leaves the campaign's pool (taken out
//! of `identity_ids`, a tag removed from the campaign or the identity, the identity disabled,
//! its connection archived), the campaign's [`OnSenderRemoved`] decides: `reassign` gives the
//! next step to another sender of the pool in a new thread, `stop` stops the enrollment. A
//! message of the removed sender that is still `queued` is cancelled ([`on_removed`]); one a
//! sender has `claimed` is left to its Start, which sees the removal and returns it to the queue,
//! to be cancelled on a later pass; one `in_flight` finishes as it is, because a started
//! submission is never recalled.
//!
//! # Picking a sender
//!
//! A new conversation takes the pool's usable sender that was assigned least recently in this
//! campaign (never assigned first, then the oldest assignment), ties broken by id
//! ([`Rotation`]). Rotation by last assignment spreads conversations evenly over the pool
//! whatever the order people arrive in, and a sender that joins the pool is picked first.
//!
//! # When a step runs
//!
//! Campaign mail lives on the 5-minute grid of the sending clock. `enrollment.advance` runs at
//! each mark and creates the messages of every enrollment due before the mark after next
//! ([`horizon`]): the current slot and one slot ahead, so a message is in the queue before its
//! sender's phase in the next slot comes round; a message created ahead waits for its due time.
//! A step's due time is its delay after the previous step's actual send ([`after_send`]); a due
//! time the campaign's send window does not admit moves to the window's next opening
//! ([`admit`]).
//!
//! # The next email
//!
//! A campaign shows when its next email may go ([`next_email`]): the earliest due time of its
//! active enrollments, as the window admits it. An enrollment whose step's message is already
//! created is due now: the message waits in the queue for its sender's turn, and a large
//! campaign's messages may wait there for hours. The due time stored on an enrollment is not
//! enough by itself, for two reasons: it moves into the window only when the pass reaches it,
//! and it is cleared while the step's message is on its way. A campaign that does not send (a
//! draft, a paused, completed or archived one) has no next email, whatever its enrollments say.

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

use super::messages::State as MessageState;
use super::schedule::{self, SLOT_SECONDS, Window};

/// Failures of one step that never reached a submission before the enrollment fails.
pub const UNSTARTED_FAILURES_MAX: i16 = 3;

/// A campaign's lifecycle (`campaigns.status`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum CampaignStatus {
    /// Being written; nothing is sent.
    Draft,
    /// Started; its job is checking it and creating the messages already due.
    Materialising,
    /// Sending.
    Active,
    /// Paused by a person; queued messages wait.
    Paused,
    /// Every enrollment ended; enrolling people again makes it active.
    Completed,
    /// Deleted after it sent: kept for its history, read-only.
    Archived,
}

impl CampaignStatus {
    /// The status as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The status `start` moves to: `materialising`, from `draft` or `paused` only.
    #[must_use]
    pub fn start(self) -> Option<Self> {
        match self {
            Self::Draft | Self::Paused => Some(Self::Materialising),
            Self::Materialising | Self::Active | Self::Completed | Self::Archived => None,
        }
    }

    /// The status `pause` moves to: `paused`, from `active` or `materialising` only.
    #[must_use]
    pub fn pause(self) -> Option<Self> {
        match self {
            Self::Active | Self::Materialising => Some(Self::Paused),
            Self::Draft | Self::Paused | Self::Completed | Self::Archived => None,
        }
    }

    /// Whether its settings, steps and pool may still change: everything but an archive.
    #[must_use]
    pub fn editable(self) -> bool {
        self != Self::Archived
    }

    /// Whether people may be enrolled: everything but an archive.
    #[must_use]
    pub fn enrolls(self) -> bool {
        self != Self::Archived
    }

    /// Whether its steps run now: `active`, or `materialising` on its way there. A draft has not
    /// started, a paused campaign holds its mail, and a completed or archived one has nobody
    /// left to mail.
    #[must_use]
    pub fn sends(self) -> bool {
        matches!(self, Self::Active | Self::Materialising)
    }
}

/// An enrollment's lifecycle (`enrollments.status`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentStatus {
    /// Live: its steps run.
    Active,
    /// Live, held until `paused_until`.
    Paused,
    /// Went through its last step.
    Completed,
    /// The person replied.
    Replied,
    /// Stopped by a person, a stop rule, a suppression or a removed sender.
    Stopped,
    /// A step could not be sent.
    Failed,
}

impl EnrollmentStatus {
    /// The status as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// Whether it is live: the one live enrollment a person may have in a campaign.
    #[must_use]
    pub fn is_live(self) -> bool {
        matches!(self, Self::Active | Self::Paused)
    }
}

/// What a campaign does with a conversation whose sender left its pool
/// (`campaigns.on_sender_removed`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum OnSenderRemoved {
    /// The next step goes from another sender of the pool, in a new thread.
    Reassign,
    /// The enrollment stops.
    Stop,
}

impl OnSenderRemoved {
    /// The rule as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Which enrollments a person's reply stops (`campaigns.stop_on_reply`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StopOnReply {
    /// The person's live enrollments in every campaign of the workspace.
    All,
    /// The person's enrollment in this campaign only.
    Campaign,
    /// None: the campaign goes on after a reply.
    None,
}

impl StopOnReply {
    /// The rule as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Where the message of a conversation whose sender left the pool is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Unsent {
    /// Waiting in the queue: nothing has touched it.
    Queued,
    /// A sender holds it and will run its Start.
    Claimed,
    /// Its submission started.
    InFlight,
}

/// What the removal does to one conversation's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// Cancel the message and re-arm its step: one new message from another sender of the pool,
    /// in a new thread.
    CancelAndReassign,
    /// Cancel the message and stop the enrollment.
    CancelAndStop,
    /// Leave it to its Start, which returns it to the queue; a later pass cancels it.
    Wait,
    /// Leave it: a started submission finishes as it is.
    Leave,
}

/// What a removed sender's `rule` does to a conversation whose message is `unsent`.
#[must_use]
pub fn on_removed(rule: OnSenderRemoved, unsent: Unsent) -> Removal {
    match (unsent, rule) {
        (Unsent::Queued, OnSenderRemoved::Reassign) => Removal::CancelAndReassign,
        (Unsent::Queued, OnSenderRemoved::Stop) => Removal::CancelAndStop,
        (Unsent::Claimed, _) => Removal::Wait,
        (Unsent::InFlight, _) => Removal::Leave,
    }
}

/// What follows a step's message (see the module's table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// Nothing yet: the message has not ended, or may have been sent.
    Wait,
    /// The next step, due `delay` after the message was sent.
    Advance { delay: Duration },
    /// The last step was sent.
    Complete,
    /// Try the step again with a new message, `failures` being the unstarted failures so far
    /// (this one included).
    Retry { failures: i16 },
    /// End the enrollment `failed`.
    Fail { detail: String },
    /// End the enrollment `stopped`.
    Stop { detail: String },
}

/// The facts [`settle`] decides from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended<'a> {
    /// The message's state.
    pub state: MessageState,
    /// Whether any submission of it started (`attempt_number > 0`).
    pub submitted: bool,
    /// The message's own reason, when it has one.
    pub detail: Option<&'a str>,
    /// The step's unstarted failures before this one (`enrollments.attempts`).
    pub failures: i16,
    /// The next step's delay in seconds, `None` after the last step.
    pub next_delay_seconds: Option<i32>,
}

/// What follows the message of an enrollment's current step (see the module's table).
#[must_use]
pub fn settle(ended: &Ended<'_>) -> Settlement {
    match ended.state {
        MessageState::Queued
        | MessageState::Claimed
        | MessageState::InFlight
        | MessageState::Uncertain => Settlement::Wait,
        MessageState::Sent => match ended.next_delay_seconds {
            Some(seconds) => Settlement::Advance {
                delay: Duration::from_secs(u64::try_from(seconds).unwrap_or(0)),
            },
            None => Settlement::Complete,
        },
        MessageState::Failed if ended.submitted => Settlement::Fail {
            detail: ended
                .detail
                .unwrap_or("The provider refused the message.")
                .to_owned(),
        },
        MessageState::Failed => {
            let failures = ended.failures.saturating_add(1);
            if failures < UNSTARTED_FAILURES_MAX {
                Settlement::Retry { failures }
            } else {
                Settlement::Fail {
                    detail: format!(
                        "The step could not be sent after {failures} tries: {}",
                        ended.detail.unwrap_or("it never reached a submission.")
                    ),
                }
            }
        }
        MessageState::Suppressed => Settlement::Stop {
            detail: "The address is suppressed.".to_owned(),
        },
        MessageState::Cancelled => Settlement::Stop {
            detail: "The step's message was cancelled.".to_owned(),
        },
    }
}

/// A sender of a campaign's pool, as the rotation sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The sender identity.
    pub id: Uuid,
    /// Whether it can send now: its connection is active, not paused, its breaker closed and
    /// its daily budget not spent. An unusable sender is never picked for a new conversation.
    pub usable: bool,
    /// When this campaign last assigned it a conversation; `None` if never.
    pub last_assigned_at: Option<Timestamp>,
}

/// The pool's rotation during one pass: picks senders for new conversations and remembers each
/// pick, so a pass that assigns many conversations spreads them as one assignment at a time would.
#[derive(Debug, Clone, Default)]
pub struct Rotation {
    candidates: Vec<Candidate>,
}

impl Rotation {
    /// The rotation over `candidates`.
    #[must_use]
    pub fn new(candidates: Vec<Candidate>) -> Self {
        Self { candidates }
    }

    /// Whether `id` is in the pool, usable or not.
    #[must_use]
    pub fn contains(&self, id: Uuid) -> bool {
        self.candidates.iter().any(|candidate| candidate.id == id)
    }

    /// Whether `id` is in the pool and usable now.
    #[must_use]
    pub fn usable(&self, id: Uuid) -> bool {
        self.candidates
            .iter()
            .any(|candidate| candidate.id == id && candidate.usable)
    }

    /// The usable sender assigned least recently (never assigned first), ties by id, recorded
    /// as assigned at `at` (or just after the latest assignment, so it goes to the back of the
    /// rotation even when the clock has not moved); `None` when no sender is usable.
    pub fn pick(&mut self, at: Timestamp) -> Option<Uuid> {
        let latest = self
            .candidates
            .iter()
            .filter_map(|candidate| candidate.last_assigned_at)
            .max();
        let chosen = self
            .candidates
            .iter_mut()
            .filter(|candidate| candidate.usable)
            .min_by(|a, b| {
                a.last_assigned_at
                    .cmp(&b.last_assigned_at)
                    .then(a.id.cmp(&b.id))
            })?;
        let after_latest = latest
            .and_then(|latest| latest.checked_add(SignedDuration::from_micros(1)).ok())
            .unwrap_or(at);
        chosen.last_assigned_at = Some(at.max(after_latest));
        Some(chosen.id)
    }

    /// Every sender this rotation assigned, with its last assignment, to record.
    #[must_use]
    pub fn assigned(&self) -> Vec<(Uuid, Timestamp)> {
        self.candidates
            .iter()
            .filter_map(|candidate| Some((candidate.id, candidate.last_assigned_at?)))
            .collect()
    }
}

/// The end of what one `enrollment.advance` run creates: the mark after next, so the current
/// slot and one slot ahead.
#[must_use]
pub fn horizon(now: Timestamp) -> Timestamp {
    let two_slots = SignedDuration::from_secs(SLOT_SECONDS.saturating_mul(2));
    schedule::slot_of(now)
        .checked_add(two_slots)
        .unwrap_or(Timestamp::MAX)
}

/// When the step after one sent at `sent_at` is due: its delay later.
#[must_use]
pub fn after_send(sent_at: Timestamp, delay: Duration) -> Timestamp {
    SignedDuration::try_from(delay)
        .ok()
        .and_then(|delay| sent_at.checked_add(delay).ok())
        .unwrap_or(Timestamp::MAX)
}

/// When a step due at `due` may go, as of `now`, through the campaign's send window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admitted {
    /// At this instant: the later of `due` and `now`, which the window admits.
    At(Timestamp),
    /// Not within a week: the window cannot be evaluated (an unknown time zone). The step waits,
    /// and is looked at again on the next pass.
    Never,
}

/// When a step due at `due` goes: the later of `due` and `now`, moved to the window's next
/// opening when the window is closed then (no window: always open).
#[must_use]
pub fn admit(due: Timestamp, now: Timestamp, window: Option<&Window>) -> Admitted {
    let at = due.max(now);
    match window {
        None => Admitted::At(at),
        Some(window) => window.next_open(at).map_or(Admitted::Never, Admitted::At),
    }
}

/// When a campaign in `status` may send its next email, as of `now` (see the module): the
/// earliest due time `waiting` of its active enrollments that wait for their step, or now when
/// one of them has its step's message on its way (`on_its_way`), admitted by the campaign's
/// `window` ([`admit`]). Admitting the earliest due time is the earliest of every admitted due
/// time, because the window's next opening never moves back as the instant it starts from moves
/// forward. `None` when the campaign does not send ([`CampaignStatus::sends`]), when nothing is
/// due, or when the window cannot be evaluated.
#[must_use]
pub fn next_email(
    status: CampaignStatus,
    waiting: Option<Timestamp>,
    on_its_way: bool,
    now: Timestamp,
    window: Option<&Window>,
) -> Option<Timestamp> {
    if !status.sends() {
        return None;
    }
    let due = if on_its_way { Some(now) } else { waiting }?;
    match admit(due, now, window) {
        Admitted::At(at) => Some(at),
        Admitted::Never => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use jiff::Timestamp;
    use strum::IntoEnumIterator as _;
    use uuid::Uuid;

    use super::{
        Admitted, CampaignStatus, Candidate, Ended, EnrollmentStatus, OnSenderRemoved, Removal,
        Rotation, Settlement, StopOnReply, UNSTARTED_FAILURES_MAX, Unsent, admit, after_send,
        horizon, next_email, on_removed, settle,
    };
    use crate::domain::messages::State as MessageState;
    use crate::domain::schedule::Window;
    use crate::domain::senders::SendWindow;

    fn at(text: &str) -> Timestamp {
        text.parse().expect("a timestamp")
    }

    /// `start` and `pause` move a campaign only from the statuses the API documents (start from
    /// draft or paused into materialising, pause from active or materialising), only an active or
    /// materialising campaign sends, everything but an archive stays editable and enrollable, and
    /// every status round-trips its stored name.
    #[test]
    fn campaign_transitions_follow_the_lifecycle() {
        for status in CampaignStatus::iter() {
            let (start, pause, sends) = match status {
                CampaignStatus::Draft | CampaignStatus::Paused => {
                    (Some(CampaignStatus::Materialising), None, false)
                }
                CampaignStatus::Active | CampaignStatus::Materialising => {
                    (None, Some(CampaignStatus::Paused), true)
                }
                CampaignStatus::Completed | CampaignStatus::Archived => (None, None, false),
            };
            assert_eq!(status.start(), start, "{status:?}");
            assert_eq!(status.pause(), pause, "{status:?}");
            assert_eq!(status.sends(), sends, "{status:?}");
            assert_eq!(status.editable(), status != CampaignStatus::Archived);
            assert_eq!(status.enrolls(), status != CampaignStatus::Archived);
            assert_eq!(status.as_str().parse::<CampaignStatus>().ok(), Some(status));
        }
    }

    /// Only `active` and `paused` enrollments are live, the set the database keeps unique per
    /// person and campaign; every enum's stored names round-trip.
    #[test]
    fn only_active_and_paused_enrollments_are_live() {
        for status in EnrollmentStatus::iter() {
            let live = matches!(status, EnrollmentStatus::Active | EnrollmentStatus::Paused);
            assert_eq!(status.is_live(), live, "{status:?}");
            assert_eq!(
                status.as_str().parse::<EnrollmentStatus>().ok(),
                Some(status)
            );
        }
        for rule in OnSenderRemoved::iter() {
            assert_eq!(rule.as_str().parse::<OnSenderRemoved>().ok(), Some(rule));
        }
        for rule in StopOnReply::iter() {
            assert_eq!(rule.as_str().parse::<StopOnReply>().ok(), Some(rule));
        }
    }

    /// A removed sender's queued message is cancelled and its conversation reassigned or
    /// stopped as the campaign says; a claimed one is left to its Start (a later pass cancels
    /// it); one in flight finishes. Generated over both enums.
    #[test]
    fn a_removal_cancels_only_what_no_sender_has_touched() {
        for rule in OnSenderRemoved::iter() {
            for unsent in Unsent::iter() {
                let expected = match (unsent, rule) {
                    (Unsent::Queued, OnSenderRemoved::Reassign) => Removal::CancelAndReassign,
                    (Unsent::Queued, OnSenderRemoved::Stop) => Removal::CancelAndStop,
                    (Unsent::Claimed, _) => Removal::Wait,
                    (Unsent::InFlight, _) => Removal::Leave,
                };
                assert_eq!(on_removed(rule, unsent), expected, "{rule:?} {unsent:?}");
            }
        }
    }

    /// Every message state has its follow-up (see the module's table), generated from the
    /// enum so a new state fails here until it has one: a sent step advances by the next
    /// step's delay or completes; an unstarted failure retries until the limit; a refused
    /// submission fails; a suppression or a cancellation stops; anything unfinished, uncertain
    /// included, waits.
    #[test]
    fn every_message_state_has_its_follow_up() {
        for state in MessageState::iter() {
            let ended = |submitted, failures, next| Ended {
                state,
                submitted,
                detail: Some("550 no such user"),
                failures,
                next_delay_seconds: next,
            };
            let followed = settle(&ended(false, 0, Some(86_400)));
            let expected = match state {
                MessageState::Queued
                | MessageState::Claimed
                | MessageState::InFlight
                | MessageState::Uncertain => Settlement::Wait,
                MessageState::Sent => Settlement::Advance {
                    delay: Duration::from_secs(86_400),
                },
                MessageState::Failed => Settlement::Retry { failures: 1 },
                MessageState::Suppressed => Settlement::Stop {
                    detail: "The address is suppressed.".to_owned(),
                },
                MessageState::Cancelled => Settlement::Stop {
                    detail: "The step's message was cancelled.".to_owned(),
                },
            };
            assert_eq!(followed, expected, "{state:?}");
            if state == MessageState::Sent {
                assert_eq!(settle(&ended(true, 0, None)), Settlement::Complete);
            }
            if state == MessageState::Failed {
                assert_eq!(
                    settle(&ended(true, 0, Some(60))),
                    Settlement::Fail {
                        detail: "550 no such user".to_owned()
                    }
                );
                assert!(matches!(
                    settle(&ended(false, UNSTARTED_FAILURES_MAX - 1, Some(60))),
                    Settlement::Fail { .. }
                ));
            }
        }
    }

    fn candidate(id: u128, usable: bool, last: Option<&str>) -> Candidate {
        Candidate {
            id: Uuid::from_u128(id),
            usable,
            last_assigned_at: last.map(at),
        }
    }

    /// The rotation takes the usable sender assigned least recently (never assigned first, ties
    /// by id), never an unusable one, and remembers each pick so consecutive picks in one pass
    /// go round the pool; an empty or wholly unusable pool picks nothing.
    #[test]
    fn the_rotation_spreads_conversations_over_usable_senders() {
        let now = at("2026-10-02T09:00:00Z");
        let mut rotation = Rotation::new(vec![
            candidate(3, true, Some("2026-10-02T08:00:00Z")),
            candidate(2, true, None),
            candidate(1, false, None),
            candidate(4, true, Some("2026-10-02T07:00:00Z")),
        ]);
        let picks: Vec<u128> = (0..6)
            .map(|_| rotation.pick(now).expect("a usable sender").as_u128())
            .collect();
        assert_eq!(picks, [2, 4, 3, 2, 4, 3]);
        assert!(rotation.contains(Uuid::from_u128(1)));
        assert!(!rotation.usable(Uuid::from_u128(1)));
        assert_eq!(rotation.assigned().len(), 3);
        assert_eq!(
            Rotation::new(vec![candidate(1, false, None)]).pick(now),
            None
        );
        assert_eq!(Rotation::default().pick(now), None);
    }

    /// One pass creates up to the mark after next: from anywhere in a slot, the end of the next
    /// slot. A step is due its delay after the actual send.
    #[test]
    fn a_pass_reaches_one_slot_ahead() {
        for now in [
            "2026-10-02T09:05:00Z",
            "2026-10-02T09:07:31Z",
            "2026-10-02T09:09:59Z",
        ] {
            assert_eq!(horizon(at(now)), at("2026-10-02T09:15:00Z"), "{now}");
        }
        assert_eq!(
            after_send(at("2026-10-02T09:03:10Z"), Duration::from_secs(86_400)),
            at("2026-10-03T09:03:10Z")
        );
    }

    /// A due step goes at the later of its due time and now, moved to the next opening of a
    /// closed window, and stays where it is in an open one.
    #[test]
    fn a_window_moves_a_due_step_to_its_next_opening() {
        let window = Window::new(
            &SendWindow::parse(&[1, 2, 3, 4, 5], "09:00", "17:00").expect("a window"),
            "Europe/Madrid",
        )
        .expect("a zone");
        // Friday 2026-10-02 18:00 in Madrid (16:00 UTC) is closed: Monday 09:00 Madrid.
        let now = at("2026-10-02T16:00:00Z");
        assert_eq!(
            admit(at("2026-10-02T15:00:00Z"), now, Some(&window)),
            Admitted::At(at("2026-10-05T07:00:00Z"))
        );
        // Friday 10:00 in Madrid is open.
        let open = at("2026-10-02T08:00:00Z");
        assert_eq!(
            admit(open, at("2026-10-02T07:59:00Z"), Some(&window)),
            Admitted::At(open)
        );
        assert_eq!(admit(open, now, None), Admitted::At(now));
    }

    /// A campaign's next email is the earliest due time of its active enrollments (one whose
    /// message is on its way counting as due now), never before now and moved to the send
    /// window's next opening; a campaign that does not send has none, whatever is due. Generated
    /// over every campaign status, so a new status takes its answer from `sends`.
    #[test]
    fn the_next_email_is_the_earliest_due_step_the_window_admits() {
        let window = Window::new(
            &SendWindow::parse(&[1, 2, 3, 4, 5], "09:00", "17:00").expect("a window"),
            "Europe/Madrid",
        )
        .expect("a zone");
        // Friday 2026-10-02 in Madrid: 10:00 is open; 18:00 is closed until Monday 09:00.
        let (open, closed, monday) = (
            at("2026-10-02T08:00:00Z"),
            at("2026-10-02T16:00:00Z"),
            at("2026-10-05T07:00:00Z"),
        );
        let (overdue, later) = (at("2026-10-01T08:00:00Z"), at("2026-10-02T09:00:00Z"));
        let saturday = at("2026-10-03T08:00:00Z");
        // (the earliest waiting step, a message on its way, now, the window, the next email of a
        // campaign that sends)
        let cases = [
            (None, false, open, None, None),
            (Some(overdue), false, open, None, Some(open)),
            (Some(later), false, open, None, Some(later)),
            (Some(later), true, open, None, Some(open)),
            (None, true, open, None, Some(open)),
            (Some(later), false, open, Some(&window), Some(later)),
            (Some(saturday), false, closed, Some(&window), Some(monday)),
            (None, true, closed, Some(&window), Some(monday)),
        ];
        for status in CampaignStatus::iter() {
            for (case, (waiting, on_its_way, now, window, sending)) in cases.iter().enumerate() {
                let expected = if status.sends() { *sending } else { None };
                assert_eq!(
                    next_email(status, *waiting, *on_its_way, *now, *window),
                    expected,
                    "{status:?}, case {case}"
                );
            }
        }
    }
}
