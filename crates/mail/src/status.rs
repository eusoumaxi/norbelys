//! Enhanced mail system status codes: `class.subject.detail`, defined by RFC 3463
//! (<https://www.rfc-editor.org/rfc/rfc3463>) and carried by SMTP replies (RFC 2034), delivery
//! status notifications (RFC 3464) and provider webhook events.
//!
//! The status is parsed once, here, into numbers, so every decision downstream compares
//! `(class, subject, detail)` instead of matching text. The class says how final the failure is
//! (2 success, 4 persistent transient failure, 5 permanent failure); the subject says what it
//! concerns (1 addressing, 2 the mailbox, 3 the mail system, 4 network and routing, 5 the
//! protocol, 6 the content, 7 security and policy).
//!
//! Invariant of [`EnhancedStatus`]: `class` is 2, 4 or 5; `subject` and `detail` have one to three
//! digits, without a leading zero unless the number is zero. Anything else is not a status and
//! does not parse, so a version number or an IP address in a reply text is never mistaken for
//! one.

use std::fmt;
use std::str::FromStr;

/// An RFC 3463 status such as `5.1.1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EnhancedStatus {
    class: u8,
    subject: u16,
    detail: u16,
}

/// Why a string is not an RFC 3463 status.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an enhanced status code (class.subject.detail, class 2, 4 or 5)")]
pub struct InvalidStatus;

impl EnhancedStatus {
    /// The class: 2 success, 4 persistent transient failure, 5 permanent failure.
    #[must_use]
    pub fn class(self) -> u8 {
        self.class
    }

    /// The subject: 1 addressing, 2 mailbox, 3 mail system, 4 network and routing, 5 protocol,
    /// 6 content, 7 security and policy (RFC 3463 §3).
    #[must_use]
    pub fn subject(self) -> u16 {
        self.subject
    }

    /// The detail within the subject.
    #[must_use]
    pub fn detail(self) -> u16 {
        self.detail
    }

    /// The first status in a reply or diagnostic text: the first word when it is one (RFC 2034
    /// puts it there), else the first word-sized token that parses, as `"550 5.1.1 unknown"`,
    /// `"smtp; 550 5.1.1 unknown"` or `"#5.1.1"` carry it.
    #[must_use]
    pub fn find(text: &str) -> Option<Self> {
        text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .map(|token| token.trim_matches('.'))
            .find_map(|token| token.parse().ok())
    }
}

impl FromStr for EnhancedStatus {
    type Err = InvalidStatus;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.trim().split('.');
        let (Some(class), Some(subject), Some(detail), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(InvalidStatus);
        };
        let class = match class {
            "2" => 2,
            "4" => 4,
            "5" => 5,
            _ => return Err(InvalidStatus),
        };
        Ok(Self {
            class,
            subject: number(subject)?,
            detail: number(detail)?,
        })
    }
}

fn number(part: &str) -> Result<u16, InvalidStatus> {
    let digits_only = part.bytes().all(|byte| byte.is_ascii_digit());
    let well_formed = (1..=3).contains(&part.len()) && (part.len() == 1 || !part.starts_with('0'));
    if !digits_only || !well_formed {
        return Err(InvalidStatus);
    }
    part.parse().map_err(|_| InvalidStatus)
}

impl fmt::Display for EnhancedStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.class, self.subject, self.detail)
    }
}

#[cfg(test)]
mod tests {
    use super::EnhancedStatus;

    fn status(class: u8, subject: u16, detail: u16) -> EnhancedStatus {
        EnhancedStatus {
            class,
            subject,
            detail,
        }
    }

    /// Only RFC 3463 statuses parse: class 2, 4 or 5 and one to three digits per part without a
    /// leading zero. Anything else must stay unparsed, or a version number or a stray number in
    /// a reply would steer a delivery decision.
    #[test]
    fn parses_only_rfc_3463_statuses() {
        assert_eq!("5.1.1".parse(), Ok(status(5, 1, 1)));
        assert_eq!("4.7.0".parse(), Ok(status(4, 7, 0)));
        assert_eq!(" 2.0.0 ".parse(), Ok(status(2, 0, 0)));
        assert_eq!("5.123.456".parse(), Ok(status(5, 123, 456)));
        for invalid in [
            "1.2.3", "3.1.1", "5.1", "5.1.1.1", "5.01.1", "5.1.1x", "5.1000.1", "", "a.b.c",
        ] {
            assert!(invalid.parse::<EnhancedStatus>().is_err(), "{invalid}");
        }
        assert_eq!(status(5, 7, 26).to_string(), "5.7.26");
    }

    /// The status is found where servers put it: first in an SMTP reply's text, after the type
    /// of a DSN diagnostic, after a hyphen or a hash; an IP address or a bare reply code is not
    /// a status.
    #[test]
    fn finds_the_status_in_reply_and_diagnostic_texts() {
        assert_eq!(
            EnhancedStatus::find("5.1.1 <a@b.c>: user unknown"),
            Some(status(5, 1, 1))
        );
        assert_eq!(
            EnhancedStatus::find("smtp; 550 5.2.2 mailbox full"),
            Some(status(5, 2, 2))
        );
        assert_eq!(
            EnhancedStatus::find("550-5.7.26 unauthenticated"),
            Some(status(5, 7, 26))
        );
        assert_eq!(
            EnhancedStatus::find("Remote server returned '#5.4.1'"),
            Some(status(5, 4, 1))
        );
        assert_eq!(EnhancedStatus::find("550 user unknown"), None);
        assert_eq!(EnhancedStatus::find("blocked from 10.5.1.10 today"), None);
    }
}
