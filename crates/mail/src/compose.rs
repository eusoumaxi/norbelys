//! MIME composition over `lettre`'s message builder: the bytes every transport submits, with
//! the headers Norbelys owns.
//!
//! The caller renders the subject and bodies; this module turns a [`Draft`] into RFC 5322 bytes
//! with CRLF line endings, `MIME-Version`, a `Date` set at composition, `multipart/alternative`
//! when both a text and an HTML body exist, and RFC 2047 encoding of non-ASCII header text.
//!
//! Invariants:
//! - The `Message-ID` is the one the caller supplies, never generated here: Norbelys encodes its
//!   own message and thread ids in it, so a reply's `In-Reply-To` can be correlated without a
//!   lookup. It must be a well-formed `<left@right>`.
//! - `In-Reply-To` and `References` are written only when given, so a follow-up threads under
//!   the ids the recipient actually saw (a provider such as Amazon SES may have replaced ours).
//! - A campaign message carries `List-Unsubscribe` with an `https` URI (optionally a `mailto:`
//!   one too) and `List-Unsubscribe-Post: List-Unsubscribe=One-Click`, the one-click
//!   unsubscribe of RFC 8058 (<https://www.rfc-editor.org/rfc/rfc8058>) that Gmail and Yahoo
//!   require from bulk senders.
//! - A [`FeedbackId`], when given, is written as Gmail's `Feedback-ID: a:b:c:SenderId`
//!   (<https://support.google.com/mail/answer/6254652>): Gmail's feedback loop counts complaints
//!   per identifier, reading the fields from the right, for a sender whose DKIM signature (added
//!   after the header, by the sending domain) covers it. The format is checked here, so a
//!   malformed header is refused instead of being counted under the wrong identifier.
//! - A relay's own headers carry the caller's message id under [`MESSAGE_TAG`], so the relay's
//!   webhook events name the message without any parsing of headers; a message through Amazon
//!   SES always names its configuration set, because SES publishes events only for messages in
//!   a set with an event destination.
//! - `Bcc` is dropped unless [`Draft::keep_bcc`] is set: SMTP takes the recipients from the
//!   envelope, so the header would leak the blind recipients, while the Gmail API and Graph read
//!   the recipients from the headers and remove `Bcc` themselves.
//! - Header values never contain control characters, so no input can inject a header.

use std::time::Duration;

use lettre::message::header::{HeaderName, HeaderValue};
use lettre::message::{Mailbox as LettreMailbox, MultiPart, SinglePart};
use lettre::{Address, Message};
use uuid::Uuid;

/// The name under which the caller's message id travels in relay metadata: an SES message tag,
/// a SendGrid unique argument, a Mailgun user variable. The webhook parsers read it back.
pub const MESSAGE_TAG: &str = "norbelys_message_id";

/// One message to compose.
#[derive(Debug, Clone)]
pub struct Draft<'a> {
    /// The full `Message-ID`, angle brackets included (`<id@domain>`).
    pub message_id: &'a str,
    /// The sender identity.
    pub from: Mailbox<'a>,
    /// The identity's reply-to address, when it has one.
    pub reply_to: Option<Mailbox<'a>>,
    /// `To` recipients.
    pub to: &'a [Mailbox<'a>],
    /// `Cc` recipients.
    pub cc: &'a [Mailbox<'a>],
    /// `Bcc` recipients.
    pub bcc: &'a [Mailbox<'a>],
    /// Keep the `Bcc` header for a transport that reads recipients from the headers.
    pub keep_bcc: bool,
    /// The subject, as rendered.
    pub subject: &'a str,
    /// The plain-text body, as rendered.
    pub text: Option<&'a str>,
    /// The HTML body, as rendered and rewritten for tracking.
    pub html: Option<&'a str>,
    /// The `Message-ID` this message answers.
    pub in_reply_to: Option<&'a str>,
    /// The thread's earlier `Message-ID`s, oldest first.
    pub references: &'a [&'a str],
    /// The RFC 8058 unsubscribe headers; every campaign message has them.
    pub list_unsubscribe: Option<ListUnsubscribe<'a>>,
    /// Gmail's `Feedback-ID` header, when the caller sets one.
    pub feedback_id: Option<FeedbackId<'a>>,
    /// The relay's own headers, when the connection is a relay.
    pub relay: Option<Relay<'a>>,
}

/// The fields of Gmail's `Feedback-ID: a:b:c:SenderId`
/// (<https://support.google.com/mail/answer/6254652>), written joined by `:`. Each field is
/// printable ASCII without `:` or spaces.
#[derive(Debug, Clone, Copy)]
pub struct FeedbackId<'a> {
    /// Up to three identifiers, the most specific first (a campaign, then a customer): each
    /// shared by many messages, never unique to one, since Gmail counts complaints per
    /// identifier.
    pub identifiers: &'a [&'a str],
    /// The sender's own id, the last field: 5 to 15 characters, the same across the sender's
    /// whole mail stream.
    pub sender: &'a str,
}

/// A display name and an address.
#[derive(Debug, Clone, Copy)]
pub struct Mailbox<'a> {
    /// The display name, if any.
    pub name: Option<&'a str>,
    /// The address.
    pub address: &'a str,
}

/// The one-click unsubscribe URIs of RFC 8058.
#[derive(Debug, Clone, Copy)]
pub struct ListUnsubscribe<'a> {
    /// The `https` URI that a `POST` with `List-Unsubscribe=One-Click` unsubscribes.
    pub https: &'a str,
    /// An optional `mailto:` URI, for clients that only send mail.
    pub mailto: Option<&'a str>,
}

/// The headers a relay reads from a message submitted over its SMTP endpoint. Each carries the
/// caller's message id under [`MESSAGE_TAG`], so the relay returns it in its webhook events.
#[derive(Debug, Clone, Copy)]
pub enum Relay<'a> {
    /// Amazon SES: `X-SES-CONFIGURATION-SET` and `X-SES-MESSAGE-TAGS`, both removed by SES
    /// before sending.
    Ses {
        /// The SES configuration set name on the connection.
        configuration_set: &'a str,
        /// The caller's message id.
        message: Uuid,
    },
    /// SendGrid: `X-SMTPAPI` with the id as a unique argument.
    Sendgrid {
        /// The caller's message id.
        message: Uuid,
    },
    /// Mailgun: `X-Mailgun-Variables` with the id, and `X-Mailgun-Deliver-Within` with the
    /// message's remaining usefulness, raised to Mailgun's 5-minute minimum and capped at its
    /// 24 hours.
    Mailgun {
        /// The caller's message id.
        message: Uuid,
        /// How long delivery may still be useful.
        deliver_within: Duration,
    },
}

/// Why a draft cannot be composed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComposeError {
    /// An address is not a valid `local@domain`.
    #[error("`{0}` is not a valid address")]
    Address(String),
    /// A header value is not acceptable (control characters, a malformed id or URI).
    #[error("the {0} header is malformed")]
    Header(&'static str),
    /// The draft has neither a text nor an HTML body.
    #[error("a message needs a text or an HTML body")]
    NoBody,
    /// `lettre` refused the message (a missing From, no recipient).
    #[error("the message cannot be built: {0}")]
    Build(String),
}

/// The draft's MIME bytes, CRLF line endings, ready for any transport.
///
/// # Errors
///
/// An address, the `Message-ID`, a referenced id or an unsubscribe URI is malformed; the draft
/// has no body; or `lettre` refuses the message.
pub fn compose(draft: &Draft<'_>) -> Result<Vec<u8>, ComposeError> {
    compose_with_attachments(draft, &[])
}

/// A decoded immutable file appended to the MIME message.
#[derive(Debug, Clone)]
pub struct Attachment {
    /// Download filename, never a storage path.
    pub filename: String,
    /// MIME media type.
    pub content_type: String,
    /// Decoded file data.
    pub bytes: Vec<u8>,
}

/// Whether a media type can be represented by the MIME composer without header injection.
#[must_use]
pub fn valid_content_type(value: &str) -> bool {
    !value.chars().any(char::is_control)
        && value
            .parse::<lettre::message::header::ContentType>()
            .is_ok()
}

/// Composes the ordinary body inside multipart/mixed when files are present.
///
/// # Errors
/// The ordinary composition validation fails, or an attachment has an invalid media type/name.
pub fn compose_with_attachments(
    draft: &Draft<'_>,
    attachments: &[Attachment],
) -> Result<Vec<u8>, ComposeError> {
    check_id(draft.message_id, "Message-ID")?;
    let mut builder = Message::builder()
        .message_id(Some(draft.message_id.to_owned()))
        .from(mailbox(&draft.from)?)
        .subject(draft.subject);
    if let Some(reply_to) = &draft.reply_to {
        builder = builder.reply_to(mailbox(reply_to)?);
    }
    for to in draft.to {
        builder = builder.to(mailbox(to)?);
    }
    for cc in draft.cc {
        builder = builder.cc(mailbox(cc)?);
    }
    for bcc in draft.bcc {
        builder = builder.bcc(mailbox(bcc)?);
    }
    if draft.keep_bcc {
        builder = builder.keep_bcc();
    }
    if let Some(id) = draft.in_reply_to {
        check_id(id, "In-Reply-To")?;
        builder = builder.in_reply_to(id.to_owned());
    }
    if !draft.references.is_empty() {
        for id in draft.references {
            check_id(id, "References")?;
        }
        builder = builder.references(draft.references.join(" "));
    }
    if let Some(unsubscribe) = &draft.list_unsubscribe {
        builder = builder
            .raw_header(header("List-Unsubscribe", list_unsubscribe(unsubscribe)?)?)
            .raw_header(header(
                "List-Unsubscribe-Post",
                "List-Unsubscribe=One-Click".to_owned(),
            )?);
    }
    if let Some(feedback) = &draft.feedback_id {
        builder = builder.raw_header(header("Feedback-ID", feedback_id(feedback)?)?);
    }
    for (name, value) in relay_headers(draft.relay.as_ref())? {
        builder = builder.raw_header(header(name, value)?);
    }
    if !attachments.is_empty() {
        let mut mixed = match (draft.text, draft.html) {
            (Some(text), Some(html)) => MultiPart::mixed().multipart(
                MultiPart::alternative_plain_html(text.to_owned(), html.to_owned()),
            ),
            (Some(text), None) => MultiPart::mixed().singlepart(SinglePart::plain(text.to_owned())),
            (None, Some(html)) => MultiPart::mixed().singlepart(SinglePart::html(html.to_owned())),
            (None, None) => return Err(ComposeError::NoBody),
        };
        for attachment in attachments {
            if attachment.filename.chars().any(char::is_control) {
                return Err(ComposeError::Header("attachment filename"));
            }
            let content_type = attachment
                .content_type
                .parse()
                .map_err(|_| ComposeError::Header("attachment content type"))?;
            mixed = mixed.singlepart(
                lettre::message::Attachment::new(attachment.filename.clone())
                    .body(attachment.bytes.clone(), content_type),
            );
        }
        return builder
            .multipart(mixed)
            .map(|message| message.formatted())
            .map_err(|error| ComposeError::Build(error.to_string()));
    }
    let message = match (draft.text, draft.html) {
        (Some(text), Some(html)) => builder.multipart(MultiPart::alternative_plain_html(
            text.to_owned(),
            html.to_owned(),
        )),
        (Some(text), None) => builder.singlepart(SinglePart::plain(text.to_owned())),
        (None, Some(html)) => builder.singlepart(SinglePart::html(html.to_owned())),
        (None, None) => return Err(ComposeError::NoBody),
    }
    .map_err(|error| ComposeError::Build(error.to_string()))?;
    Ok(message.formatted())
}

fn mailbox(mailbox: &Mailbox<'_>) -> Result<LettreMailbox, ComposeError> {
    let address: Address = mailbox
        .address
        .trim()
        .parse()
        .map_err(|_| ComposeError::Address(mailbox.address.to_owned()))?;
    let name = mailbox.name.map(str::trim).filter(|name| !name.is_empty());
    if name.is_some_and(|name| name.chars().any(char::is_control)) {
        return Err(ComposeError::Header("display name"));
    }
    Ok(LettreMailbox::new(name.map(str::to_owned), address))
}

/// A `msg-id` (RFC 5322 §3.6.4): `<left@right>`, printable ASCII, no spaces.
fn check_id(id: &str, header: &'static str) -> Result<(), ComposeError> {
    let inner = id.strip_prefix('<').and_then(|rest| rest.strip_suffix('>'));
    let valid = inner.is_some_and(|inner| {
        inner
            .split_once('@')
            .is_some_and(|(left, right)| !left.is_empty() && !right.is_empty())
            && inner
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b'<' && byte != b'>')
    });
    if valid {
        Ok(())
    } else {
        Err(ComposeError::Header(header))
    }
}

fn list_unsubscribe(unsubscribe: &ListUnsubscribe<'_>) -> Result<String, ComposeError> {
    let uri_ok = |uri: &str, scheme: &str| {
        uri.get(..scheme.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
            && uri
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b'<' && byte != b'>')
    };
    if !uri_ok(unsubscribe.https, "https://") {
        return Err(ComposeError::Header("List-Unsubscribe"));
    }
    match unsubscribe.mailto {
        Some(mailto) if !uri_ok(mailto, "mailto:") => Err(ComposeError::Header("List-Unsubscribe")),
        Some(mailto) => Ok(format!("<{}>, <{mailto}>", unsubscribe.https)),
        None => Ok(format!("<{}>", unsubscribe.https)),
    }
}

/// The `Feedback-ID` value: the identifiers, then the sender's id, joined by `:`; refused when
/// there are more than three identifiers, a field is empty or holds anything but printable ASCII
/// other than `:`, or the sender's id is not 5 to 15 characters long.
fn feedback_id(feedback: &FeedbackId<'_>) -> Result<String, ComposeError> {
    let field = |value: &str| {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b':')
    };
    let valid = feedback.identifiers.len() <= 3
        && feedback.identifiers.iter().copied().all(field)
        && field(feedback.sender)
        && (5..=15).contains(&feedback.sender.len());
    if !valid {
        return Err(ComposeError::Header("Feedback-ID"));
    }
    let mut fields = feedback.identifiers.to_vec();
    fields.push(feedback.sender);
    Ok(fields.join(":"))
}

fn relay_headers(relay: Option<&Relay<'_>>) -> Result<Vec<(&'static str, String)>, ComposeError> {
    let Some(relay) = relay else {
        return Ok(Vec::new());
    };
    Ok(match relay {
        Relay::Ses {
            configuration_set,
            message,
        } => {
            let valid = (1..=64).contains(&configuration_set.len())
                && configuration_set
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
            if !valid {
                return Err(ComposeError::Header("X-SES-CONFIGURATION-SET"));
            }
            vec![
                ("X-SES-CONFIGURATION-SET", (*configuration_set).to_owned()),
                ("X-SES-MESSAGE-TAGS", format!("{MESSAGE_TAG}={message}")),
            ]
        }
        Relay::Sendgrid { message } => {
            let unique = serde_json::json!({ "unique_args": { MESSAGE_TAG: message.to_string() } });
            vec![("X-SMTPAPI", unique.to_string())]
        }
        Relay::Mailgun {
            message,
            deliver_within,
        } => {
            let variables = serde_json::json!({ MESSAGE_TAG: message.to_string() });
            let minutes = (deliver_within.as_secs() / 60).clamp(5, 24 * 60);
            vec![
                ("X-Mailgun-Variables", variables.to_string()),
                ("X-Mailgun-Deliver-Within", format!("{minutes}m")),
            ]
        }
    })
}

fn header(name: &'static str, value: String) -> Result<HeaderValue, ComposeError> {
    if value.chars().any(char::is_control) {
        return Err(ComposeError::Header(name));
    }
    let name =
        HeaderName::new_from_ascii(name.to_owned()).map_err(|_| ComposeError::Header(name))?;
    Ok(HeaderValue::new(name, value))
}

#[cfg(test)]
mod tests;
