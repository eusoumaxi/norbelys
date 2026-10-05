//! Provider text made safe to keep: one line, bounded, without control characters.
//!
//! Providers answer with free text (SMTP replies, API error messages, report fields) that the
//! caller often persists or logs. Every diagnostic, reply and report field this crate returns
//! goes through [`bounded`], so a provider can never hand the caller an unbounded string, a
//! header injection (CR or LF) or terminal control characters.

/// The most characters of provider text kept on a rejection or an event: long enough for any
/// real SMTP reply or API error message, short enough to store on each submission record.
pub(crate) const DIAGNOSTIC_CHARS: usize = 2_000;

/// Bound for decoded report/header values, distinct from RFC 5322's physical wire line limit.
pub(crate) const HEADER_VALUE_CHARS: usize = 998;

/// `text` on one line: control characters become spaces, runs of spaces collapse, and at most
/// `max_chars` characters are kept.
pub(crate) fn bounded(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_chars));
    let mut count = 0;
    let mut space = false;
    for c in text.trim().chars() {
        if count >= max_chars {
            break;
        }
        if c.is_control() || c.is_whitespace() {
            if !space {
                out.push(' ');
                count += 1;
            }
            space = true;
        } else {
            out.push(c);
            count += 1;
            space = false;
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// A bounded diagnostic, or `None` when nothing is left after trimming.
pub(crate) fn diagnostic(text: &str) -> Option<String> {
    Some(bounded(text, DIAGNOSTIC_CHARS)).filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{bounded, diagnostic};

    /// Provider text reaches rows and logs only as one bounded line: CR and LF (which would
    /// inject a header or a log line) and other control characters become single spaces, and
    /// the cut counts characters, so a multi-byte character is never split.
    #[test]
    fn bounded_text_is_one_trimmed_line_cut_on_characters() {
        assert_eq!(
            bounded("  550 5.1.1\r\n  user\tunknown \u{7}x  ", 100),
            "550 5.1.1 user unknown x"
        );
        assert_eq!(bounded("ééééé", 3), "ééé");
        assert_eq!(bounded("abc   ", 4), "abc");
        assert_eq!(diagnostic(" \r\n "), None);
        assert_eq!(diagnostic("ok"), Some("ok".to_owned()));
    }
}
