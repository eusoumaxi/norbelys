//! Messages: their kinds and states, the bounds of their envelope and schedule, and the shape of
//! the Message-ID Norbelys gives every message it sends.
//!
//! # The Message-ID
//!
//! Every outbound message gets `<{message}.{thread}.{tag}@{domain}>`: the message's and its
//! thread's uuids as 32 lowercase hexadecimal digits each, a tag, and the domain of the From
//! address. A reply names the id it answers in `In-Reply-To` and `References`, so the id alone
//! tells which message and which thread it answers: correlation needs no lookup, and keeps
//! working after the message's own row has been archived (threads outlive messages). The tag is
//! a truncated MAC of the two ids under the deployment's key (`crypto::Keys::message_id_tag`), so
//! an id someone made up cannot steer an inbound message into a thread. Because the id is
//! derived from the row's own key it is unique without any index.
//!
//! The format and its parser live here, pure; computing and checking the tag needs the keys and
//! happens where the keys are (`delivery::accept`).
//!
//! # Schedule
//!
//! A message created through the API may be scheduled at most seven days ahead
//! ([`SCHEDULE_HORIZON`]): a live message must never sit in a period of the message table that
//! is old enough to be archived, and campaigns create each step's message only when it is due
//! anyway. A `send_at` in the past means now. A message's own usefulness (`expires_at`: a
//! sign-in code's ten minutes, an invitation's expiry) must end after it is due, or it could
//! never be sent.

use std::time::Duration;

use uuid::Uuid;

/// Which side of a conversation supplied a message, shared by thread and history reads.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Mail created by Norbelys.
    Outbound,
    /// Mail read from a connected inbox.
    Inbound,
}

impl Direction {
    /// The stored direction and its wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Outbound => "outbound",
            Self::Inbound => "inbound",
        }
    }
}

/// What a message is, as `messages.kind` stores it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[schema(as = MessageKind, rename_all = "snake_case")]
pub enum Kind {
    /// One step of a campaign for one enrolled person.
    Campaign,
    /// Created through the API with its own content.
    Direct,
    /// An answer in an existing thread.
    Reply,
    /// The platform's own mail (sign-in codes, invitations), sent by the `system` workspace.
    Transactional,
}

impl Kind {
    /// The kind as stored and shown.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Where a message is in its life, as `messages.state` stores it. `queued`, `claimed` and
/// `in_flight` are live (the message has a row in the delivery queue); the others are final.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
#[schema(as = MessageState)]
pub enum State {
    /// Waiting to be due, or for its connection.
    Queued,
    /// A sender holds it and prepares its submission.
    Claimed,
    /// Its submission started.
    InFlight,
    /// A provider accepted it (acceptance, not delivery).
    Sent,
    /// It will never be sent: refused for good, or expired.
    Failed,
    /// Cancelled while queued.
    Cancelled,
    /// Its submission ended without a readable answer: it may have been sent, and it is never
    /// resent automatically.
    Uncertain,
    /// Every recipient was suppressed by the time it was due.
    Suppressed,
}

impl State {
    /// The state as stored and shown.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The most `To` recipients of one message.
pub const TO_MAX: usize = 50;
/// The most recipients of one message, `To`, `Cc` and `Bcc` together: the envelope bound every
/// transport accepts (providers that allow fewer are held to their own limit by preflight).
pub const RECIPIENTS_MAX: usize = 150;
/// How far ahead a message may be scheduled.
pub const SCHEDULE_HORIZON: Duration = Duration::from_secs(7 * 24 * 3_600);

/// Why a schedule is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    /// `send_at` is more than seven days ahead.
    #[error("a message may be scheduled at most 7 days ahead")]
    TooFar,
    /// `expires_at` is not after the instant the message is due.
    #[error("`expires_at` must be after the message is due")]
    ExpiresFirst,
}

/// The instant a message is due: `send_at` when given and not past, else `now`.
///
/// # Errors
///
/// [`ScheduleError::TooFar`] beyond [`SCHEDULE_HORIZON`]; [`ScheduleError::ExpiresFirst`] when
/// `expires_at` is not after the due instant.
pub fn schedule(
    send_at: Option<jiff::Timestamp>,
    expires_at: Option<jiff::Timestamp>,
    now: jiff::Timestamp,
) -> Result<jiff::Timestamp, ScheduleError> {
    let horizon = jiff::SignedDuration::try_from(SCHEDULE_HORIZON)
        .ok()
        .and_then(|horizon| now.checked_add(horizon).ok())
        .unwrap_or(jiff::Timestamp::MAX);
    let due = send_at.map_or(now, |send_at| send_at.max(now));
    if due > horizon {
        return Err(ScheduleError::TooFar);
    }
    if expires_at.is_some_and(|expires_at| expires_at <= due) {
        return Err(ScheduleError::ExpiresFirst);
    }
    Ok(due)
}

/// The bytes a Message-ID's tag is computed over: the message's uuid, then its thread's.
#[must_use]
pub fn tag_payload(message: Uuid, thread: Uuid) -> [u8; 32] {
    let mut payload = [0_u8; 32];
    for (out, byte) in payload
        .iter_mut()
        .zip(message.as_bytes().iter().chain(thread.as_bytes()))
    {
        *out = *byte;
    }
    payload
}

/// The Message-ID of `message` in `thread`, with its `tag`, at `domain` (see the module).
#[must_use]
pub fn internet_message_id(message: Uuid, thread: Uuid, tag: &[u8], domain: &str) -> String {
    use std::fmt::Write as _;
    let hex = tag.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    });
    format!(
        "<{}.{}.{hex}@{}>",
        message.simple(),
        thread.simple(),
        domain.to_ascii_lowercase()
    )
}

/// The parts of a Message-ID in our format, before its tag is checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// The message the id names.
    pub message: Uuid,
    /// Its thread.
    pub thread: Uuid,
    /// The tag as written.
    pub tag: Vec<u8>,
}

/// Reads a Message-ID of our format (with or without its angle brackets); `None` for any other
/// id, which is the normal case for mail we did not write.
#[must_use]
pub fn parse_internet_message_id(value: &str) -> Option<Parsed> {
    let value = value.trim();
    let value = value
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .unwrap_or(value);
    let (local, domain) = value.split_once('@')?;
    if domain.is_empty() {
        return None;
    }
    let mut parts = local.split('.');
    let (Some(message), Some(thread), Some(tag), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let lower_hex = |text: &str, len: usize| {
        text.len() == len
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if !lower_hex(message, 32) || !lower_hex(thread, 32) || !lower_hex(tag, 16) {
        return None;
    }
    let tag = (0..tag.len())
        .step_by(2)
        .map(|at| {
            tag.get(at..at + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<u8>>>()?;
    Some(Parsed {
        message: Uuid::parse_str(message).ok()?,
        thread: Uuid::parse_str(thread).ok()?,
        tag,
    })
}

/// Why a message history query is refused before any storage read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    /// Text must be useful and bounded, including when it is only whitespace.
    #[error("Search text must contain 2 to 256 characters and cannot be blank.")]
    Text,
    /// A half-open interval cannot be empty or reversed.
    #[error("The end must be after the start.")]
    Range,
}

/// Validates message search independently of HTTP and SQL. An absent query means history.
///
/// # Errors
/// Text is blank/outside the character bounds, or the supplied time interval is reversed.
pub fn check_search(
    q: Option<&str>,
    from: Option<jiff::Timestamp>,
    to: Option<jiff::Timestamp>,
) -> Result<(), SearchError> {
    if q.is_some_and(|q| !(2..=256).contains(&q.chars().count()) || q.trim().is_empty()) {
        return Err(SearchError::Text);
    }
    if from.zip(to).is_some_and(|(from, to)| from >= to) {
        return Err(SearchError::Range);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Unicode character bounds and half-open time bounds are decided before storage access.
    #[test]
    fn search_bounds() {
        use super::{SearchError, check_search};
        assert_eq!(check_search(None, None, None), Ok(()));
        assert_eq!(check_search(Some("éé"), None, None), Ok(()));
        for text in ["", "a", "  "] {
            assert_eq!(check_search(Some(text), None, None), Err(SearchError::Text));
        }
        assert_eq!(
            check_search(Some(&"é".repeat(257)), None, None),
            Err(SearchError::Text)
        );
        let now = jiff::Timestamp::from_second(0).unwrap();
        assert_eq!(
            check_search(None, Some(now), Some(now)),
            Err(SearchError::Range)
        );
    }

    use std::str::FromStr as _;

    use strum::IntoEnumIterator as _;

    use super::*;

    /// Every kind and every state reads back from the text the schema's `CHECK` lists, so a
    /// variant added here without its value there (or the reverse) is caught before a row is.
    #[test]
    fn kinds_and_states_are_the_schemas_vocabulary() {
        let kinds = ["campaign", "direct", "reply", "transactional"];
        assert_eq!(Kind::iter().map(Kind::as_str).collect::<Vec<_>>(), kinds);
        let states = [
            "queued",
            "claimed",
            "in_flight",
            "sent",
            "failed",
            "cancelled",
            "uncertain",
            "suppressed",
        ];
        assert_eq!(State::iter().map(State::as_str).collect::<Vec<_>>(), states);
        for state in State::iter() {
            assert_eq!(State::from_str(state.as_str()), Ok(state));
        }
    }

    /// The schedule: absent or past means now, up to seven days ahead is kept, beyond is
    /// refused, and a message whose usefulness ends before it is due is refused, because it
    /// could never be sent.
    #[test]
    fn a_schedule_is_bounded_and_ends_after_it_is_due() {
        let now: jiff::Timestamp = "2026-10-02T12:00:00Z".parse().unwrap();
        let at = |text: &str| text.parse::<jiff::Timestamp>().unwrap();
        assert_eq!(schedule(None, None, now), Ok(now));
        assert_eq!(
            schedule(Some(at("2020-01-01T00:00:00Z")), None, now),
            Ok(now)
        );
        let edge = at("2026-10-09T12:00:00Z");
        assert_eq!(schedule(Some(edge), None, now), Ok(edge));
        assert_eq!(
            schedule(Some(at("2026-10-09T12:00:01Z")), None, now),
            Err(ScheduleError::TooFar)
        );
        assert_eq!(
            schedule(None, Some(now), now),
            Err(ScheduleError::ExpiresFirst)
        );
        assert_eq!(
            schedule(Some(edge), Some(at("2026-10-05T00:00:00Z")), now),
            Err(ScheduleError::ExpiresFirst)
        );
        assert_eq!(
            schedule(None, Some(at("2026-10-02T12:10:00Z")), now),
            Ok(now)
        );
    }

    /// Our Message-ID reads back into the ids and the tag it was made of, with or without its
    /// angle brackets, whatever the case of its domain.
    #[test]
    fn our_message_id_reads_back() {
        let message = Uuid::now_v7();
        let thread = Uuid::now_v7();
        let tag = [0xde, 0xad, 0xbe, 0xef, 0x01, 0x23, 0x45, 0x67];
        let id = internet_message_id(message, thread, &tag, "Mail.Example.COM");
        assert_eq!(
            id,
            format!(
                "<{}.{}.deadbeef01234567@mail.example.com>",
                message.simple(),
                thread.simple()
            )
        );
        let parsed = parse_internet_message_id(&id).unwrap();
        assert_eq!(parsed.message, message);
        assert_eq!(parsed.thread, thread);
        assert_eq!(parsed.tag, tag);
        assert_eq!(
            parse_internet_message_id(id.trim_start_matches('<').trim_end_matches('>')),
            Some(parsed)
        );
    }

    /// Ids of other formats are not ours: another number of parts, uppercase or short hex, no
    /// domain. They are the normal case (mail we did not write), never an error.
    #[test]
    fn other_message_ids_are_not_ours() {
        let message = Uuid::now_v7().simple().to_string();
        let thread = Uuid::now_v7().simple().to_string();
        for other in [
            "<CAF=abc@mail.gmail.com>".to_owned(),
            format!("<{message}.{thread}@example.com>"),
            format!("<{message}.{thread}.deadbeef01234567.x@example.com>"),
            format!("<{message}.{thread}.DEADBEEF01234567@example.com>"),
            format!("<{message}.{thread}.deadbeef@example.com>"),
            format!("<{message}.{thread}.deadbeef01234567@>"),
            format!("<{}.{thread}.deadbeef01234567@example.com>", &message[1..]),
        ] {
            assert_eq!(parse_internet_message_id(&other), None, "{other}");
        }
    }
}
