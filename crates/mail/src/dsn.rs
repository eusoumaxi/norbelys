//! Delivery status notifications (DSNs, RFC 3464, <https://www.rfc-editor.org/rfc/rfc3464>):
//! the bounce and delay reports mail servers send back, parsed from the MIME tree that
//! `mail-parser` builds.
//!
//! A DSN is a `multipart/report; report-type=delivery-status` message with three parts: a
//! human-readable explanation, a `message/delivery-status` part, and the returned message
//! (`message/rfc822`) or only its headers (`text/rfc822-headers`). The delivery-status part is a
//! sequence of header-style field groups separated by blank lines: one group about the message
//! (`Reporting-MTA`, `Original-Envelope-Id`), then one group per recipient (`Final-Recipient`,
//! `Action`, `Status`, `Diagnostic-Code`, `Remote-MTA`). The internationalised form of RFC 6533
//! (`message/global-delivery-status`, `message/global-headers`) is read the same way.
//!
//! The parser keeps only what is needed to recognise the caller's message and the recipient's
//! fate:
//! - the returned `Message-ID`, which carries the caller's ids, so a bounce correlates without
//!   any lookup when the reporter returned the headers;
//! - per recipient, the address (its `rfc822;` type stripped), the action, the RFC 3463 status,
//!   the reporting server's diagnostic and its SMTP code.
//!
//! A DSN proves nothing by itself: anybody can send one. Its weight comes from where it arrived
//! and whether it names the caller's message; that judgement belongs to the caller. Messages
//! that merely look like bounces (free-text notices) are not DSNs and return `None`.
//!
//! Bounds: at most [`MAX_REPORT_RECIPIENTS`] recipient groups are kept; every value is one bounded
//! line; message ids are returned without angle brackets.

use mail_parser::{Message, MessageParser, MimeHeaders as _, PartType};

use crate::status::EnhancedStatus;

/// The most recipient groups kept from one report.
pub const MAX_REPORT_RECIPIENTS: usize = 1_000;
/// The longest field value kept.
const VALUE_CHARS: usize = crate::text::HEADER_VALUE_CHARS;

/// Reduces a DSN line by line, keeping its MIME structure, status parts and returned headers.
/// Returned bodies and human explanations are drained without occupying the retained buffer.
/// Other messages retain their original bytes up to the supplied bound. This reducer is not
/// suitable for signature verification: feedback reports must retain their complete raw bytes.
#[derive(Debug)]
pub struct StreamReducer {
    bytes: Vec<u8>,
    headers: Vec<u8>,
    boundary: Option<Vec<u8>>,
    state: StreamState,
    limit: usize,
    fits: bool,
}

#[derive(Debug, Clone, Copy)]
enum StreamState {
    TopHeaders,
    PartHeaders,
    Status,
    ReturnedHeaders,
    Skip,
    Full,
}

impl StreamReducer {
    /// Creates a reducer with a fixed upper bound on retained evidence.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            headers: Vec::new(),
            boundary: None,
            state: StreamState::TopHeaders,
            limit,
            fits: true,
        }
    }

    /// Feeds a dot-unstuffed line without its line ending. Each retained line gains CRLF.
    pub fn line(&mut self, line: &[u8]) {
        let delimiter = self.boundary.as_ref().and_then(|boundary| {
            let marker = line.strip_prefix(b"--")?;
            if marker == boundary.as_slice() {
                Some(false)
            } else if marker.strip_suffix(b"--") == Some(boundary.as_slice()) {
                Some(true)
            } else {
                None
            }
        });
        if let Some(closing) = delimiter {
            self.keep(line);
            self.headers.clear();
            self.state = if closing {
                StreamState::Skip
            } else {
                StreamState::PartHeaders
            };
            return;
        }
        match self.state {
            StreamState::TopHeaders | StreamState::PartHeaders => {
                self.keep(line);
                if !line.is_empty() {
                    if self.headers.len().saturating_add(line.len() + 2) <= 64 * 1024 {
                        self.headers.extend_from_slice(line);
                        self.headers.extend_from_slice(b"\r\n");
                    } else {
                        self.fits = false;
                    }
                    return;
                }
                let parsed = MessageParser::default().parse_headers(&self.headers);
                let content = parsed.as_ref().and_then(|message| message.content_type());
                self.state = match self.state {
                    StreamState::TopHeaders => {
                        let boundary = content
                            .filter(|content| {
                                content.ctype().eq_ignore_ascii_case("multipart")
                                    && content
                                        .subtype()
                                        .is_some_and(|kind| kind.eq_ignore_ascii_case("report"))
                                    && content
                                        .attribute("report-type")
                                        .is_none_or(is_delivery_status_report)
                            })
                            .and_then(|content| content.attribute("boundary"))
                            .filter(|boundary| {
                                !boundary.is_empty()
                                    && boundary.len() <= 70
                                    && !boundary.contains(['\r', '\n'])
                            });
                        self.boundary = boundary.map(|boundary| boundary.as_bytes().to_vec());
                        if self.boundary.is_some() {
                            StreamState::Skip
                        } else {
                            StreamState::Full
                        }
                    }
                    _ => match content.map(|content| {
                        (
                            content.ctype().to_ascii_lowercase(),
                            content.subtype().unwrap_or_default().to_ascii_lowercase(),
                        )
                    }) {
                        Some((kind, subtype))
                            if kind == "message"
                                && ["delivery-status", "global-delivery-status"]
                                    .contains(&subtype.as_str()) =>
                        {
                            StreamState::Status
                        }
                        Some((kind, subtype))
                            if (kind == "message"
                                && ["rfc822", "global", "global-headers"]
                                    .contains(&subtype.as_str()))
                                || (kind == "text" && subtype == "rfc822-headers") =>
                        {
                            StreamState::ReturnedHeaders
                        }
                        _ => StreamState::Skip,
                    },
                };
                if matches!(self.state, StreamState::ReturnedHeaders) {
                    let encoded = String::from_utf8_lossy(&self.headers).lines().any(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-transfer-encoding:")
                            .is_some_and(|value| {
                                !["7bit", "8bit", "binary"].contains(&value.trim())
                            })
                    });
                    if encoded {
                        self.state = StreamState::Full;
                    }
                }
                self.headers.clear();
            }
            StreamState::Status | StreamState::Full => self.keep(line),
            StreamState::ReturnedHeaders => {
                self.keep(line);
                if line.is_empty() {
                    self.state = StreamState::Skip;
                }
            }
            StreamState::Skip => {}
        }
    }

    fn keep(&mut self, line: &[u8]) {
        self.fits &= self.bytes.len().saturating_add(line.len() + 2) <= self.limit;
        if self.fits {
            self.bytes.extend_from_slice(line);
            self.bytes.extend_from_slice(b"\r\n");
        }
    }

    /// Returns retained evidence, or refuses evidence that exceeded the configured bound.
    #[must_use]
    pub fn finish(self) -> Option<Vec<u8>> {
        self.fits.then_some(self.bytes)
    }
}

/// A parsed delivery status notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dsn {
    /// The server that wrote the report (`Reporting-MTA`, its type stripped).
    pub reporting_mta: Option<String>,
    /// The envelope id the original submission carried (`Original-Envelope-Id`).
    pub original_envelope_id: Option<String>,
    /// One entry per recipient group.
    pub recipients: Vec<DsnRecipient>,
    /// The `Message-ID` of the returned message or headers, without angle brackets.
    pub original_message_id: Option<String>,
}

/// What a report says about one recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DsnRecipient {
    /// The address the report is about (`Final-Recipient`).
    pub final_recipient: Option<String>,
    /// The address as originally submitted, when the report gives it (`Original-Recipient`).
    pub original_recipient: Option<String>,
    /// What happened (`Action`).
    pub action: Option<Action>,
    /// The RFC 3463 status (`Status`).
    pub status: Option<EnhancedStatus>,
    /// The remote server the reporter talked to (`Remote-MTA`).
    pub remote_mta: Option<String>,
    /// The remote server's own diagnostic (`Diagnostic-Code`, its type stripped).
    pub diagnostic: Option<String>,
    /// The SMTP code at the start of an `smtp;` diagnostic.
    pub smtp_code: Option<u16>,
}

/// The `Action` of a recipient group (RFC 3464 §2.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Delivery failed for good.
    Failed,
    /// Delivery is delayed; the reporter keeps trying.
    Delayed,
    /// Delivered (a positive report, when asked for).
    Delivered,
    /// Relayed to a system that does not send reports.
    Relayed,
    /// Delivered to a list or alias that expanded to other recipients.
    Expanded,
}

impl Action {
    /// The action spelling the caller records for the delivery event.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Delayed => "delayed",
            Self::Delivered => "delivered",
            Self::Relayed => "relayed",
            Self::Expanded => "expanded",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        let token = value.split_whitespace().next()?.to_ascii_lowercase();
        match token.as_str() {
            "failed" => Some(Self::Failed),
            "delayed" => Some(Self::Delayed),
            "delivered" => Some(Self::Delivered),
            "relayed" => Some(Self::Relayed),
            "expanded" => Some(Self::Expanded),
            _ => None,
        }
    }
}

/// The DSN in a raw message, or `None` when the message is not one.
#[must_use]
pub fn parse(raw: &[u8]) -> Option<Dsn> {
    from_message(&MessageParser::default().parse(raw)?)
}

/// Whether a `report-type` parameter (RFC 3464 `delivery-status` or RFC 6533
/// `global-delivery-status`) names a delivery-status report. Callers treat an absent parameter as
/// `delivery-status` — `from_message` via `unwrap_or`, the `StreamReducer` via `Option::is_none_or`
/// — matching the RFC 3464 default.
fn is_delivery_status_report(kind: &str) -> bool {
    kind.eq_ignore_ascii_case("delivery-status")
        || kind.eq_ignore_ascii_case("global-delivery-status")
}

/// The DSN in an already parsed message.
pub(crate) fn from_message(message: &Message<'_>) -> Option<Dsn> {
    let report = message.content_type()?;
    let is_report = report.ctype().eq_ignore_ascii_case("multipart")
        && report
            .subtype()
            .is_some_and(|subtype| subtype.eq_ignore_ascii_case("report"));
    let kind = report.attribute("report-type").unwrap_or("delivery-status");
    if !is_report || !is_delivery_status_report(kind) {
        return None;
    }
    let status = message.parts.iter().find(|part| {
        part.is_content_type("message", "delivery-status")
            || part.is_content_type("message", "global-delivery-status")
    })?;
    let text = String::from_utf8_lossy(status.contents());
    let groups = field_groups(&text);
    let message_fields: &[(String, String)] = groups.first().map_or(&[], Vec::as_slice);
    // Every group naming a recipient, the first included: some servers omit the blank line
    // between the per-message fields and the first recipient.
    let recipients = groups
        .iter()
        .filter(|group| {
            field(group, "final-recipient").is_some() || field(group, "action").is_some()
        })
        .take(MAX_REPORT_RECIPIENTS)
        .map(|group| recipient(group))
        .collect();
    Some(Dsn {
        reporting_mta: field(message_fields, "reporting-mta").map(typed_value),
        original_envelope_id: field(message_fields, "original-envelope-id").map(bounded),
        recipients,
        original_message_id: returned_message_id(message),
    })
}

fn recipient(group: &[(String, String)]) -> DsnRecipient {
    let diagnostic = field(group, "diagnostic-code");
    let is_smtp =
        diagnostic.is_some_and(|value| value.trim_start().to_ascii_lowercase().starts_with("smtp"));
    let diagnostic = diagnostic
        .map(typed_value)
        .filter(|value| !value.is_empty());
    let smtp_code = diagnostic
        .as_deref()
        .filter(|_| is_smtp)
        .and_then(|value| value.get(..3))
        .and_then(|digits| digits.parse::<u16>().ok())
        .filter(|code| (200..600).contains(code));
    DsnRecipient {
        final_recipient: field(group, "final-recipient").map(address),
        original_recipient: field(group, "original-recipient").map(address),
        action: field(group, "action").and_then(Action::parse),
        status: field(group, "status")
            .and_then(|value| value.split_whitespace().next())
            .and_then(|code| code.parse().ok()),
        remote_mta: field(group, "remote-mta").map(typed_value),
        diagnostic,
        smtp_code,
    }
}

/// The `Message-ID` of the returned message (`message/rfc822`, `message/global`) or of the
/// returned headers (`text/rfc822-headers`, `message/global-headers`).
pub(crate) fn returned_message_id(message: &Message<'_>) -> Option<String> {
    message.parts.iter().find_map(|part| {
        if let PartType::Message(returned) = &part.body {
            return returned.message_id().map(bounded);
        }
        let headers_only = part.is_content_type("text", "rfc822-headers")
            || part.is_content_type("message", "global-headers");
        headers_only
            .then(|| MessageParser::default().parse_headers(part.contents()))
            .flatten()
            .and_then(|headers| headers.message_id().map(bounded))
    })
}

/// Header-style field groups separated by blank lines; continuation lines are unfolded and
/// names are lowercased.
pub(crate) fn field_groups(text: &str) -> Vec<Vec<(String, String)>> {
    let mut groups = Vec::new();
    let mut group: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !group.is_empty() {
                groups.push(std::mem::take(&mut group));
            }
            continue;
        }
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = group.last_mut()
                && value.len() < VALUE_CHARS * 4
            {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            group.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    if !group.is_empty() {
        groups.push(group);
    }
    groups
}

/// The first value of field `name` (lowercase) in `group`.
pub(crate) fn field<'a>(group: &'a [(String, String)], name: &str) -> Option<&'a str> {
    group
        .iter()
        .find(|(field, _)| field == name)
        .map(|(_, value)| value.as_str())
}

/// A typed value (`rfc822; user@example.com`, `dns; mx.example.com`) without its type.
fn typed_value(value: &str) -> String {
    let untyped = value.split_once(';').map_or(value, |(_, rest)| rest);
    bounded(untyped)
}

/// A typed address without its type and angle brackets.
fn address(value: &str) -> String {
    let untyped = value.split_once(';').map_or(value, |(_, rest)| rest).trim();
    bounded(untyped.trim_start_matches('<').trim_end_matches('>'))
}

fn bounded(value: &str) -> String {
    crate::text::bounded(value, VALUE_CHARS)
}

#[cfg(test)]
mod tests;
