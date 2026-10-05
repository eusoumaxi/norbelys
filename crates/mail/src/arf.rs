//! Abuse reports in the Abuse Reporting Format (ARF, RFC 5965,
//! <https://www.rfc-editor.org/rfc/rfc5965>): the complaints feedback loops send when a
//! recipient marks a message as spam, parsed from the MIME tree that `mail-parser` builds.
//!
//! An ARF report is a `multipart/report; report-type=feedback-report` message with a
//! human-readable part, a `message/feedback-report` part of header-style fields
//! (`Feedback-Type`, `User-Agent`, `Version`, `Original-Mail-From`, `Original-Rcpt-To`,
//! `Arrival-Date`, `Source-IP`, `Reported-Domain`, `Reporting-MTA`), and the reported message
//! (`message/rfc822`) or its headers (`text/rfc822-headers`).
//!
//! What is kept, and why:
//! - the feedback type: only `abuse` and `fraud` are complaints; `not-spam` withdraws one, and
//!   `auth-failure` (RFC 6591) and `virus` are not about the recipient's consent;
//! - the original recipient, as the report names it (`Original-Rcpt-To`, often redacted by the
//!   feedback loop, else the returned `To` header);
//! - the returned `Message-ID` (ids this library composed) and `Feedback-ID` header (RFC-less but
//!   used by Gmail and others to group complaints), so the complaint correlates to that message.
//!
//! A report proves nothing by itself: the reporter's authenticity (a DKIM-verified enrolled
//! feedback loop) is judged by the caller. Messages that are not ARF return `None`. Values are
//! bounded single lines; message ids are returned without angle brackets.

use mail_parser::{Message, MessageParser, MimeHeaders as _, PartType};

use crate::dsn::{field, field_groups, returned_message_id};

const VALUE_CHARS: usize = crate::text::HEADER_VALUE_CHARS;
/// The most `Original-Rcpt-To` and `Reported-Domain` values kept.
const MAX_VALUES: usize = 100;

/// A parsed ARF report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arf {
    /// What kind of feedback it is.
    pub feedback_type: FeedbackType,
    /// The software that wrote the report (`User-Agent`).
    pub user_agent: Option<String>,
    /// The envelope sender of the reported message (`Original-Mail-From`).
    pub original_mail_from: Option<String>,
    /// The reported message's recipients as the report names them (`Original-Rcpt-To`), often
    /// redacted.
    pub original_rcpt_to: Vec<String>,
    /// The `To` header of the returned message, when the report includes it.
    pub original_to: Option<String>,
    /// The IP address that sent the reported message (`Source-IP`).
    pub source_ip: Option<String>,
    /// The domains the report concerns (`Reported-Domain`).
    pub reported_domains: Vec<String>,
    /// The server that wrote the report (`Reporting-MTA`, its type stripped).
    pub reporting_mta: Option<String>,
    /// The returned message's `Message-ID`, without angle brackets.
    pub original_message_id: Option<String>,
    /// The returned message's `Feedback-ID` header.
    pub feedback_id: Option<String>,
}

/// The `Feedback-Type` of a report (RFC 5965 §7.3, RFC 6591).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeedbackType {
    /// Unsolicited mail: a spam complaint.
    Abuse,
    /// A phishing or fraud report.
    Fraud,
    /// The message carried a virus.
    Virus,
    /// An authentication failure report (RFC 6591): not about consent.
    AuthFailure,
    /// The recipient marked the message as not spam.
    NotSpam,
    /// Any other or unknown type.
    Other,
}

impl FeedbackType {
    fn parse(value: &str) -> Self {
        match value
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "abuse" => Self::Abuse,
            "fraud" => Self::Fraud,
            "virus" => Self::Virus,
            "auth-failure" => Self::AuthFailure,
            "not-spam" => Self::NotSpam,
            _ => Self::Other,
        }
    }

    /// Whether the report is a complaint about consent (`abuse` or `fraud`).
    #[must_use]
    pub fn is_complaint(self) -> bool {
        matches!(self, Self::Abuse | Self::Fraud)
    }
}

/// The ARF report in a raw message, or `None` when the message is not one.
#[must_use]
pub fn parse(raw: &[u8]) -> Option<Arf> {
    from_message(&MessageParser::default().parse(raw)?)
}

/// The ARF report in an already parsed message.
pub(crate) fn from_message(message: &Message<'_>) -> Option<Arf> {
    let report = message.content_type()?;
    let is_report = report.ctype().eq_ignore_ascii_case("multipart")
        && report
            .subtype()
            .is_some_and(|subtype| subtype.eq_ignore_ascii_case("report"))
        && report
            .attribute("report-type")
            .is_some_and(|kind| kind.eq_ignore_ascii_case("feedback-report"));
    if !is_report {
        return None;
    }
    let feedback = message
        .parts
        .iter()
        .find(|part| part.is_content_type("message", "feedback-report"))?;
    let text = String::from_utf8_lossy(feedback.contents());
    let fields: Vec<(String, String)> = field_groups(&text).into_iter().flatten().collect();
    let all = |name: &str| -> Vec<String> {
        fields
            .iter()
            .filter(|(field, _)| field == name)
            .take(MAX_VALUES)
            .map(|(_, value)| bounded(value.trim_start_matches('<').trim_end_matches('>')))
            .collect()
    };
    let (original_to, feedback_id) = returned_headers(message);
    Some(Arf {
        feedback_type: FeedbackType::parse(field(&fields, "feedback-type")?),
        user_agent: field(&fields, "user-agent").map(bounded),
        original_mail_from: field(&fields, "original-mail-from")
            .map(|value| bounded(value.trim_start_matches('<').trim_end_matches('>'))),
        original_rcpt_to: all("original-rcpt-to"),
        original_to,
        source_ip: field(&fields, "source-ip").map(bounded),
        reported_domains: all("reported-domain"),
        reporting_mta: field(&fields, "reporting-mta")
            .map(|value| bounded(value.split_once(';').map_or(value, |(_, rest)| rest))),
        original_message_id: returned_message_id(message),
        feedback_id,
    })
}

/// The message a feedback report in `raw` returns, as it was received: its `message/rfc822`
/// part, or its `text/rfc822-headers` part (the header block alone); `None` when `raw` is not a
/// report or returns neither. A caller verifies the signatures it carries ([`crate::dkim`]),
/// which is why the bytes are kept exactly.
#[must_use]
pub fn returned(raw: &[u8]) -> Option<Vec<u8>> {
    let message = MessageParser::default().parse(raw)?;
    from_message(&message)?;
    message.parts.iter().find_map(|part| match &part.body {
        PartType::Message(returned) => Some(returned.raw_message().to_vec()),
        _ if part.is_content_type("text", "rfc822-headers") => Some(part.contents().to_vec()),
        _ => None,
    })
}

/// The returned message's `To` and `Feedback-ID` headers.
fn returned_headers(message: &Message<'_>) -> (Option<String>, Option<String>) {
    let read = |headers: &Message<'_>| {
        let to = headers.header_raw("To").map(bounded);
        let feedback_id = headers.header_raw("Feedback-ID").map(bounded);
        (to, feedback_id)
    };
    for part in &message.parts {
        if let PartType::Message(returned) = &part.body {
            return read(returned);
        }
        if part.is_content_type("text", "rfc822-headers")
            && let Some(headers) = MessageParser::default().parse_headers(part.contents())
        {
            return read(&headers);
        }
    }
    (None, None)
}

fn bounded(value: &str) -> String {
    crate::text::bounded(value, VALUE_CHARS)
}

#[cfg(test)]
mod tests;
