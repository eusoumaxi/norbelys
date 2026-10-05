//! The facts of an inbound message that correlation and classification need, read once from its
//! raw MIME by `mail-parser`.
//!
//! The caller decides what an inbound message is (a human reply, an automatic reply, a bounce, a
//! complaint, an unsubscribe request) and which thread it belongs to; this module only reads the
//! facts those decisions take, so the caller never parses MIME itself:
//! - the ids: `Message-ID`, `In-Reply-To` and `References`, without angle brackets. A reply to
//!   a message this library composed names that `Message-ID`, which carries message and thread ids;
//! - the sender, the subject and the `Date`;
//! - the automatic-mail markers: `Auto-Submitted` (RFC 3834,
//!   <https://www.rfc-editor.org/rfc/rfc3834>) and `Precedence`;
//! - a bounded excerpt of the text body (an HTML-only body converted to text);
//! - a delivery status notification ([`crate::dsn`]) or an abuse report ([`crate::arf`]), when
//!   the message is one.
//!
//! Bounds: at most [`MAX_IDS`] ids per header; header values are bounded single lines; the
//! excerpt keeps line breaks but no other control characters.

use jiff::Timestamp;
use mail_parser::{HeaderValue, MessageParser};

use crate::arf::{self, Arf};
use crate::dsn::{self, Dsn};

/// The most ids kept from `In-Reply-To` or `References`.
pub const MAX_IDS: usize = 100;
const VALUE_CHARS: usize = crate::text::HEADER_VALUE_CHARS;

/// The facts of one inbound message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    /// Its own `Message-ID`, without angle brackets.
    pub message_id: Option<String>,
    /// The ids it answers (`In-Reply-To`), without angle brackets.
    pub in_reply_to: Vec<String>,
    /// The thread's ids (`References`), oldest first, without angle brackets.
    pub references: Vec<String>,
    /// The first `From` mailbox.
    pub from: Option<Sender>,
    /// The subject, decoded.
    pub subject: Option<String>,
    /// The `Date` header.
    pub date: Option<Timestamp>,
    /// The `Auto-Submitted` value, lowercased (`auto-replied`, `auto-generated`, `no`).
    pub auto_submitted: Option<String>,
    /// The `Precedence` value, lowercased (`bulk`, `list`, `junk`, `auto_reply`).
    pub precedence: Option<String>,
    /// The start of the text body, at most the caller's number of characters.
    pub excerpt: Option<String>,
    /// The report the message is, if any.
    pub report: Option<Report>,
}

/// A display name and an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sender {
    /// The display name, decoded.
    pub name: Option<String>,
    /// The address.
    pub address: String,
}

/// A machine-readable report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// A delivery status notification (RFC 3464).
    Dsn(Dsn),
    /// An abuse report (RFC 5965).
    Arf(Arf),
}

/// The facts of `raw`, the excerpt at most `excerpt_chars` characters; `None` when `raw` has no
/// header at all.
#[must_use]
pub fn read(raw: &[u8], excerpt_chars: usize) -> Option<Inbound> {
    let message = MessageParser::default().parse(raw)?;
    let report = dsn::from_message(&message)
        .map(Report::Dsn)
        .or_else(|| arf::from_message(&message).map(Report::Arf));
    let from = message
        .from()
        .and_then(|from| from.first())
        .and_then(|first| {
            let address = first.address()?;
            Some(Sender {
                name: first
                    .name()
                    .map(|name| crate::text::bounded(name, 200))
                    .filter(|name| !name.is_empty()),
                address: crate::text::bounded(address, 320),
            })
        });
    let token = |name: &str| {
        message
            .header_raw(name)
            .and_then(|value| {
                value
                    .split([';', ' ', '\t', '\r', '\n'])
                    .find(|part| !part.is_empty())
            })
            .map(|value| crate::text::bounded(value, 64).to_ascii_lowercase())
    };
    Some(Inbound {
        message_id: message.message_id().map(bounded),
        in_reply_to: ids(message.in_reply_to()),
        references: ids(message.references()),
        from,
        subject: message.subject().map(bounded),
        date: message
            .date()
            .and_then(|date| Timestamp::from_second(date.to_timestamp()).ok()),
        auto_submitted: token("Auto-Submitted"),
        precedence: token("Precedence"),
        excerpt: message
            .body_text(0)
            .map(|text| excerpt(&text, excerpt_chars))
            .filter(|text| !text.is_empty()),
        report,
    })
}

fn ids(value: &HeaderValue<'_>) -> Vec<String> {
    match value {
        HeaderValue::Text(id) => vec![bounded(id)],
        HeaderValue::TextList(list) => list.iter().take(MAX_IDS).map(|id| bounded(id)).collect(),
        _ => Vec::new(),
    }
}

fn bounded(value: &str) -> String {
    crate::text::bounded(value, VALUE_CHARS)
}

/// At most `max_chars` characters of `text`, line breaks kept as `\n`, other control characters
/// as spaces.
fn excerpt(text: &str, max_chars: usize) -> String {
    text.replace("\r\n", "\n")
        .chars()
        .take(max_chars)
        .map(|c| if c == '\n' || !c.is_control() { c } else { ' ' })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Complete decoded MIME content within the caller's raw-message byte limit.
#[derive(Debug, Clone, Default)]
pub struct Content {
    /// The complete first plain-text body.
    pub text: Option<String>,
    /// The complete first HTML body.
    pub html: Option<String>,
    /// Ordered name/value pairs, retaining repeated MIME headers.
    pub headers: Vec<(String, String)>,
    /// Primary recipients.
    pub to: Vec<String>,
    /// Copy recipients.
    pub cc: Vec<String>,
    /// Blind copy recipients, if present in the received MIME.
    pub bcc: Vec<String>,
    /// Decoded regular and inline file parts.
    pub attachments: Vec<ContentAttachment>,
}

/// One decoded MIME attachment. Callers must not publish parts from truncated MIME as complete.
#[derive(Debug, Clone)]
pub struct ContentAttachment {
    /// The decoded filename, with a fallback for nameless parts.
    pub filename: String,
    /// The MIME media type, without parameters.
    pub content_type: String,
    /// The Content-ID of an inline part, without angle brackets.
    pub content_id: Option<String>,
    /// Bytes after content-transfer decoding.
    pub bytes: Vec<u8>,
}

/// Parses complete retained content using the same MIME parser as classification.
/// Returns none for input without a header; the caller owns the raw byte limit and truncation.
#[must_use]
pub fn content(raw: &[u8]) -> Option<Content> {
    use mail_parser::MimeHeaders as _;
    let message = MessageParser::default().parse(raw)?;
    let addresses = |value: Option<&mail_parser::Address<'_>>| {
        value
            .into_iter()
            .flat_map(|value| value.iter())
            .filter_map(|address| address.address().map(str::to_owned))
            .collect()
    };
    Some(Content {
        text: message.text_part(0).and_then(|part| match &part.body {
            mail_parser::PartType::Text(text) => Some(text.to_string()),
            _ => None,
        }),
        html: message.html_part(0).and_then(|part| match &part.body {
            mail_parser::PartType::Html(html) => Some(html.to_string()),
            _ => None,
        }),
        headers: message
            .headers_raw()
            .map(|(name, value)| (name.to_owned(), value.trim().to_owned()))
            .collect(),
        to: addresses(message.to()),
        cc: addresses(message.cc()),
        bcc: addresses(message.bcc()),
        attachments: message
            .attachments()
            .map(|part| {
                let content_type = part.content_type().map_or_else(
                    || "application/octet-stream".to_owned(),
                    |kind| {
                        format!(
                            "{}/{}",
                            kind.ctype(),
                            kind.subtype().unwrap_or("octet-stream")
                        )
                    },
                );
                ContentAttachment {
                    filename: part.attachment_name().unwrap_or("attachment").to_owned(),
                    content_type,
                    content_id: part.content_id().map(str::to_owned),
                    bytes: part.contents().to_vec(),
                }
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests;
