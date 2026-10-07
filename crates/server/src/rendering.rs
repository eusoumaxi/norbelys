//! Rendering: from a message's row to the bytes a transport submits, and the template checks a
//! message passes when it is created.
//!
//! # The seam the sender calls
//!
//! [`prepare`] reads one message (in its workspace, as the sender's own login) and returns the
//! [`Prepared`] submission: the SMTP envelope, the MIME bytes and the Message-ID. It writes
//! nothing, so the sender may call it as often as it needs (each attempt renders again, and
//! renders the same: everything it reads was frozen when the message was created). Its
//! [`Error`] says whether the failure is permanent (the message can never render: fail it) or
//! transient (the database: try again). The sender builds its [`Settings`] once at start:
//!
//! ```text
//! let settings = rendering::Settings::new(keys, &args.rendering.tracking_url, retry_window)?;
//! let prepared = rendering::prepare(&db, &settings, workspace, message).await?;
//! ```
//!
//! # What a message is made of
//!
//! When a message is created (`delivery::accept`), the namespaces its templates may read are
//! frozen into `messages.render_context` ([`namespaces`]), and the templates are rendered once
//! with them ([`templates`]) under the same bounds as here, so a template that fails is refused
//! at creation (`422` with pointers) instead of failing later in the sender.
//! The public content contract has one authored HTML body. Its plain-text MIME alternative is
//! derived from that body, so authors do not maintain two versions of the same message.
//!
//! - Mail that owns its content (direct, reply, transactional) stores the rendered subject and
//!   bodies in its row; [`prepare`] uses them as they are. Transactional mail is drawn in the one
//!   frame of the platform's own mail ([`frame`]), whose HTML links the brand mark on
//!   [`TRACKING_ORIGIN_STAND_IN`]: the role creating it may not know the tracking host, so
//!   [`prepare`] puts the tracking origin in its place.
//! - Campaign mail never holds a body: its row stores the rendered subject and points at a
//!   variant's revision, whose preheader and bodies [`prepare`] renders with the frozen context
//!   and the recipient's unsubscribe link.
//!
//! Then, in this order: the additions around the body ([`footer`]: preheader, the identity's
//! signature, the unsubscribe link of campaign mail and of a campaign step's content sent to a
//! person who is not enrolled, whose body carries a stand-in link from its creation that is
//! replaced by the real one here). The text part is the body's own text (stored, or derived from
//! the rendered HTML body before anything is added to it) followed by the same additions in
//! text, so it ends with the text form of the signature, which a person writes once in either
//! form ([`footer::Signature`]), and never repeats the HTML part's hidden preheader. Then
//! tracking ([`tracking_rewrite`]: click links and the open pixel, when the message's frozen
//! `tracking` asks for them); and composition with
//! `norbelys_mail::compose`, which adds `List-Unsubscribe` and `List-Unsubscribe-Post`
//! (RFC 8058, <https://www.rfc-editor.org/rfc/rfc8058>) to the same messages (commercial mail to a
//! prospect, which the large mailbox providers' sender rules expect to be one click from
//! stopping), and the relay's own headers:
//!
//! - **Amazon SES**: the connection's configuration set (`X-SES-CONFIGURATION-SET`) and our
//!   message id as a message tag, so SES publishes the message's events and they name it;
//! - **SendGrid**: our message id as a unique argument;
//! - **Mailgun**: our message id as a user variable, and `X-Mailgun-Deliver-Within` with the
//!   message's remaining usefulness: until its delivery deadline, else its `expires_at`, else the
//!   deployment's retry window (`DELIVERY_RETRY_WINDOW_HOURS`, 24 hours unless configured, held
//!   by [`Settings`]); Mailgun clamps it to 5 minutes – 24 hours.
//!
//! The Gmail API and Microsoft Graph read recipients from the headers, so their messages keep
//! `Bcc`; SMTP takes them from the envelope and the header is dropped.
//!
//! Mail through the managed MTA carries Gmail's `Feedback-ID`
//! (<https://support.google.com/mail/answer/6254652>): `<campaign>:<workspace>:norbelys`, the
//! campaign's id or `direct` for the workspace's other mail, then the workspace's id, then the
//! platform's one sender id, so complaints from Gmail's feedback loop are counted per campaign and
//! per workspace, never per message. The managed MTA's DKIM signature covers the header, which is
//! also what lets its feedback-loop handler prove that a returned message was ours. Mail sent
//! through a customer's own mailbox or relay carries none: there the header would mark the
//! customer's mail as the platform's, and neither our signature nor our loop handler is behind it.
//!
//! # The envelope
//!
//! `MAIL FROM` is the From address, except on the managed MTA, whose messages use
//! `bounce+<message id as 32 lowercase hex digits>@<the MTA's mail host>`: the MTA records that
//! return path when it accepts the submission, so a bounce that comes back to it later is
//! matched to the message by the MTA's own record. The mail host is the connection's SMTP host,
//! which is the MTA's public name. The recipients are `To`, `Cc` and `Bcc`, in that order.
//!
//! # Tracking and unsubscribe links
//!
//! They live on the tracking host frozen when the message was created (`messages.tracking.hostname`):
//! a campaign's fixed host, or the sender domain's active custom host in automatic mode; otherwise the platform's
//! ([`Settings::new`], `PUBLIC_TRACKING_URL`). The unsubscribe link names the message's first
//! `To` address, the person it was written to.
//!
//! # Reads
//!
//! The sender's login (`norbelys_worker`) reads, inside the message's workspace: the message,
//! its connection's routing columns (provider, transport, SMTP settings), its identity's
//! signatures, its thread's root id, its delivery queue row's deadline and, for campaign mail,
//! the variant revision. It never reads a credential.

pub mod footer;
pub mod frame;
pub mod namespaces;
pub mod plain;
pub mod templates;
pub mod tracking_rewrite;

use std::time::Duration;

use norbelys_mail::compose::{
    self, ComposeError, Draft, FeedbackId, ListUnsubscribe, Mailbox, Relay,
};
use norbelys_mail::submission::{Envelope, EnvelopeError};
use serde_json::Value;
use uuid::Uuid;

use self::footer::{Additions, Signature, Unsubscribe};
use self::templates::{Part, TemplateError};
use crate::crypto::Keys;
use crate::db::Database;
use crate::domain::ids::{Id, Message, WorkspaceId};
use crate::domain::messages::Kind;
use crate::domain::time::Timestamp;
use crate::tracking::token::Token;

/// The version of the rendering rules a message was created under (`messages.render_version`).
pub const RENDER_VERSION: &str = "1";
/// The stand-in unsubscribe link of a creation check: campaign templates may print
/// `{{ unsubscribe_url }}`, whose real value exists only when the message is sent.
pub const UNSUBSCRIBE_STAND_IN: &str = "https://tracking.invalid/u/unsubscribe";
/// The stand-in tracking origin of the platform's own mail: its frame ([`frame`]) links the
/// brand mark on the tracking host, whose origin the role creating the message may not know (the
/// worker, which sends the notices, knows none), so the creation writes this origin and
/// [`prepare`] replaces it with the tracking origin. `.invalid` is reserved and resolves nowhere
/// (RFC 2606), so a stand-in that ever reached a reader would load nothing.
pub const TRACKING_ORIGIN_STAND_IN: &str = "https://tracking.invalid";
/// The sender id of every message's `Feedback-ID`: one for the platform's whole mail stream.
const FEEDBACK_SENDER: &str = "norbelys";

/// What the sender needs to render: the deployment's keys (tokens), the platform's tracking
/// origin and the retry window. Built once at start.
#[derive(Clone)]
pub struct Settings {
    keys: Keys,
    tracking_origin: String,
    /// How long delivery stays useful when a message names no deadline: the deployment's retry
    /// window (`DELIVERY_RETRY_WINDOW_HOURS`), the same the sender's Start retries within.
    retry_window: Duration,
    storage: Option<crate::storage::Storage>,
    attachment_slots: std::sync::Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Settings")
            .field("tracking_origin", &self.tracking_origin)
            .field("retry_window", &self.retry_window)
            .finish_non_exhaustive()
    }
}

/// Why the rendering settings were refused at start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("PUBLIC_TRACKING_URL must be an https URL (List-Unsubscribe accepts nothing else)")]
pub struct SettingsError;

impl Settings {
    /// Supplies the shared object store used to read immutable attachments.
    #[must_use]
    pub fn with_storage(mut self, storage: crate::storage::Storage) -> Self {
        self.storage = Some(storage);
        self
    }

    /// The settings of a deployment whose tracking host answers at `tracking_url` (an `https`
    /// URL; only its origin is kept) and whose sender retries a message within `retry_window`.
    ///
    /// # Errors
    ///
    /// The URL is not `https`.
    pub fn new(
        keys: Keys,
        tracking_url: &url::Url,
        retry_window: Duration,
    ) -> Result<Self, SettingsError> {
        if tracking_url.scheme() != "https" || tracking_url.host().is_none() {
            return Err(SettingsError);
        }
        Ok(Self {
            keys,
            tracking_origin: tracking_url.origin().ascii_serialization(),
            retry_window,
            storage: None,
            attachment_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(4)),
        })
    }
}

/// A message ready for its transport.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// `MAIL FROM` and `RCPT TO` (the HTTP transports read the headers instead).
    pub envelope: Envelope,
    /// The MIME bytes, CRLF line endings.
    pub raw: Vec<u8>,
    /// Holds the large-file composition permit until submission releases these bytes.
    _attachment_slot: Option<std::sync::Arc<tokio::sync::OwnedSemaphorePermit>>,
    /// The `Message-ID` the bytes carry.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the sender submits the envelope and the bytes only; the tests read it to prove the bytes carry the message's own Message-ID"
        )
    )]
    pub internet_message_id: String,
}

/// Why a message could not be prepared.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The message does not exist in the workspace.
    #[error("no such message")]
    NotFound,
    /// A campaign message's content no longer renders.
    #[error("the message's content does not render: {0}")]
    Template(#[from] TemplateError),
    /// The HTML could not be rewritten.
    #[error("the message's HTML could not be rewritten: {0}")]
    Rewrite(String),
    /// The MIME could not be composed.
    #[error("the message could not be composed: {0}")]
    Compose(#[from] ComposeError),
    /// The envelope is not valid.
    #[error("the message's envelope is not valid: {0}")]
    Envelope(#[from] EnvelopeError),
    /// What the message points at is inconsistent (a campaign message without its variant
    /// revision, an SES connection without its configuration set).
    #[error("{0}")]
    Inconsistent(String),
    /// The database failed.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    /// An attachment could not be read from object storage; retry later.
    #[error(transparent)]
    Storage(#[from] crate::storage::StorageError),
}

impl Error {
    /// True when the message can never be prepared and should fail; false for a failure that
    /// may pass on a later attempt (the database).
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        !matches!(self, Self::Db(_) | Self::Storage(_))
    }
}

impl From<lol_html::errors::RewritingError> for Error {
    fn from(error: lol_html::errors::RewritingError) -> Self {
        Self::Rewrite(error.to_string())
    }
}

/// Everything [`assemble`] needs, as read from the database.
#[derive(Debug, Clone)]
struct Source {
    workspace: WorkspaceId,
    message: Id<Message>,
    kind: Kind,
    /// The campaign the message was created for, if any (`messages.campaign_id`).
    campaign: Option<Uuid>,
    /// The message belongs to one of the workspace's people (`messages.person_id` is set); a
    /// preview of a step sent to another address belongs to nobody.
    person: bool,
    provider: String,
    transport: String,
    smtp: Option<Value>,
    from_email: String,
    from_name: Option<String>,
    reply_to: Option<String>,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    html: Option<String>,
    text: Option<String>,
    /// A campaign message's variant revision: preheader, HTML and text templates.
    variant: Option<(Option<String>, String, Option<String>)>,
    context: Value,
    internet_message_id: String,
    in_reply_to: Option<String>,
    thread_root: Option<String>,
    tracking: Value,
    signature_html: Option<String>,
    signature_text: Option<String>,
    deadline: Option<Timestamp>,
}

/// Prepares `message` of `workspace` for submission (see the module): reads it inside the
/// workspace, renders what is rendered at sending, adds the footer and tracking, composes.
/// Writes nothing.
///
/// # Errors
///
/// [`Error::is_permanent`] tells a message that can never be prepared from a database failure.
pub async fn prepare(
    db: &Database,
    settings: &Settings,
    workspace: WorkspaceId,
    message: Id<Message>,
) -> Result<Prepared, Error> {
    let mut tx = db.begin_in(workspace).await?;
    let row = sqlx::query!(
        r#"SELECT m.kind, m.campaign_id, m.person_id IS NOT NULL AS "has_person!", m.from_email, m.from_name, m.reply_to, m.to_addresses, m.cc, m.bcc, m.subject,
                  m.html, m.text_body, m.render_context, m.internet_message_id, m.in_reply_to, m.tracking,
                  m.variant_id, m.variant_version, c.provider, c.transport, c.smtp,
                  i.signature_html, i.signature_text,
                  t.root_internet_message_id AS "thread_root?",
                  coalesce(q.deadline_at, q.expires_at) AS "deadline?: Timestamp"
             FROM messages m
             JOIN connections c ON c.workspace_id = m.workspace_id AND c.id = m.connection_id
             JOIN sender_identities i ON i.workspace_id = m.workspace_id AND i.id = m.sender_identity_id
             LEFT JOIN threads t ON t.workspace_id = m.workspace_id AND t.id = m.thread_id
             LEFT JOIN delivery_queue q ON q.workspace_id = m.workspace_id AND q.message_id = m.id
            WHERE m.workspace_id = $1 AND m.id = $2"#,
        workspace.uuid(),
        message.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    let kind: Kind = row
        .kind
        .parse()
        .map_err(|_| Error::Inconsistent(format!("unknown message kind `{}`", row.kind)))?;
    let variant = match (kind, row.variant_id, row.variant_version) {
        (Kind::Campaign, Some(variant), Some(version)) => {
            let revision = sqlx::query!(
                "SELECT preheader, html, text FROM variant_revisions
                  WHERE workspace_id = $1 AND variant_id = $2 AND version = $3",
                workspace.uuid(),
                variant,
                version,
            )
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| {
                Error::Inconsistent("the message's variant revision is missing".to_owned())
            })?;
            Some((revision.preheader, revision.html, revision.text))
        }
        (Kind::Campaign, _, _) => {
            return Err(Error::Inconsistent(
                "a campaign message names no variant revision".to_owned(),
            ));
        }
        (Kind::Direct | Kind::Reply | Kind::Transactional, _, _) => None,
    };
    let files = crate::delivery::attachments::list(&mut tx, workspace, message.uuid()).await?;
    tx.commit().await?;
    let attachment_slot = if files.is_empty() {
        None
    } else {
        Some(std::sync::Arc::new(
            settings
                .attachment_slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Error::Inconsistent("attachment preparation is closed".to_owned()))?,
        ))
    };
    let attachments = if files.is_empty() {
        Vec::new()
    } else {
        let storage = settings.storage.as_ref().ok_or_else(|| {
            Error::Inconsistent("attachment storage is not configured".to_owned())
        })?;
        crate::delivery::attachments::load(storage, files).await?
    };
    let source = Source {
        workspace,
        message,
        kind,
        campaign: row.campaign_id,
        person: row.has_person,
        provider: row.provider,
        transport: row.transport,
        smtp: row.smtp,
        from_email: row.from_email,
        from_name: row.from_name,
        reply_to: row.reply_to,
        to: row.to_addresses,
        cc: row.cc,
        bcc: row.bcc,
        subject: row.subject,
        html: row.html,
        text: row.text_body,
        variant,
        context: row.render_context,
        internet_message_id: row.internet_message_id,
        in_reply_to: row.in_reply_to,
        thread_root: row.thread_root,
        tracking: row.tracking,
        signature_html: row.signature_html,
        signature_text: row.signature_text,
        deadline: row.deadline,
    };
    let mut prepared = assemble_attached(&source, settings, crate::process::now(), &attachments)?;
    prepared._attachment_slot = attachment_slot;
    Ok(prepared)
}

/// The pure part of [`prepare`]: everything after the reads, with `now` given.
#[cfg(test)]
fn assemble(source: &Source, settings: &Settings, now: Timestamp) -> Result<Prepared, Error> {
    assemble_attached(source, settings, now, &[])
}

/// Composes the same body and headers with any immutable files.
fn assemble_attached(
    source: &Source,
    settings: &Settings,
    now: Timestamp,
    attachments: &[compose::Attachment],
) -> Result<Prepared, Error> {
    let origin = source
        .tracking
        .get("hostname")
        .and_then(Value::as_str)
        .map_or_else(
            || settings.tracking_origin.clone(),
            |host| format!("https://{host}"),
        );
    // Campaign mail carries the one-click unsubscribe, and so does a campaign step's content sent
    // to a person who is not enrolled: both are commercial mail to a prospect, which RFC 8058 and
    // the large mailbox providers' sender rules expect to be one click from stopping. A preview of
    // a step (sent to another address, belonging to no person) and the other direct, reply and
    // transactional mail carry none.
    let unsubscribable = match source.kind {
        Kind::Campaign => true,
        Kind::Direct => {
            source.person
                && source
                    .context
                    .get("step")
                    .is_some_and(|step| !step.is_null())
        }
        Kind::Reply | Kind::Transactional => false,
    };
    let unsubscribe_url = match (unsubscribable, source.to.first()) {
        (true, Some(recipient)) => Some(
            Token::Unsubscribe {
                workspace: source.workspace,
                message: source.message,
                email: recipient.clone(),
            }
            .url(&settings.keys, &origin),
        ),
        _ => None,
    };
    let unsubscribe = unsubscribe_url.as_deref().map(|url| Unsubscribe {
        url,
        token: url.rsplit('/').next().unwrap_or(url),
    });

    // The bodies: a campaign message's are rendered now from its variant; the others' were
    // rendered at creation.
    let (preheader, html, text) = match &source.variant {
        Some((preheader, html, text)) => {
            let context = namespaces::with_unsubscribe(
                &source.context,
                unsubscribe_url.as_deref().unwrap_or(UNSUBSCRIBE_STAND_IN),
            );
            let preheader = preheader
                .as_deref()
                .map(|source| templates::render(Part::Preheader, source, &context))
                .transpose()?;
            let html = templates::render(Part::Html, html, &context)?;
            let text = text
                .as_deref()
                .map(|source| templates::render(Part::Text, source, &context))
                .transpose()?;
            (preheader, Some(html), text)
        }
        // A step's content was rendered at creation with a stand-in unsubscribe link, and the
        // platform's own mail with its brand mark on a stand-in tracking origin; the real ones
        // exist only now. Only the HTML of transactional mail holds that origin (the frame's mark),
        // and printed values are escaped there, so no value a person wrote is ever rewritten.
        None => {
            let real = |body: &String| match unsubscribe_url.as_deref() {
                Some(url) => body.replace(UNSUBSCRIBE_STAND_IN, url),
                None => body.clone(),
            };
            let html = source
                .html
                .as_ref()
                .map(real)
                .map(|html| match source.kind {
                    Kind::Transactional => html.replace(
                        &format!("{TRACKING_ORIGIN_STAND_IN}/"),
                        &format!("{origin}/"),
                    ),
                    Kind::Campaign | Kind::Direct | Kind::Reply => html,
                });
            (None, html, source.text.as_ref().map(real))
        }
    };
    // The identity wrote its signature once, in either form or both; each part gets its own.
    let signature = Signature::of(
        source.signature_html.as_deref(),
        source.signature_text.as_deref(),
    )?;
    let additions = Additions {
        preheader: preheader.as_deref(),
        signature_html: signature.html.as_deref(),
        signature_text: signature.text.as_deref(),
        unsubscribe,
    };
    // The text part is the body's own text (stored, or derived from the rendered HTML body before
    // anything is added to it) followed by the additions in text, so it ends with the text form of
    // the signature and never repeats the HTML part's hidden preheader.
    let text = match (text, html.as_deref()) {
        (Some(text), _) => Some(footer::text(&text, &additions)),
        (None, Some(html)) => Some(footer::text(
            &plain::from_html(html).map_err(|error| Error::Rewrite(error.to_string()))?,
            &additions,
        )),
        (None, None) => None,
    };
    let html = html
        .map(|html| footer::html(&html, &additions))
        .transpose()?;
    let flag = |name: &str| {
        source
            .tracking
            .get(name)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let html = match html {
        Some(html) if flag("opens") || flag("clicks") => Some(tracking_rewrite::rewrite(
            &html,
            &tracking_rewrite::Tracking {
                keys: &settings.keys,
                origin: &origin,
                workspace: source.workspace,
                message: source.message,
                opens: flag("opens"),
                clicks: flag("clicks"),
            },
        )?),
        other => other,
    };

    // The headers.
    let (to, cc, bcc) = (
        mailboxes(&source.to),
        mailboxes(&source.cc),
        mailboxes(&source.bcc),
    );
    let references: Vec<&str> = match source.in_reply_to.as_deref() {
        Some(parent) => source
            .thread_root
            .as_deref()
            .filter(|root| *root != parent)
            .into_iter()
            .chain([parent])
            .collect(),
        None => Vec::new(),
    };
    let mailto = format!("mailto:{}?subject=unsubscribe", source.from_email);
    let list_unsubscribe = unsubscribe_url.as_deref().map(|https| ListUnsubscribe {
        https,
        mailto: mailto.is_ascii().then_some(mailto.as_str()),
    });
    let smtp_setting = |name: &str| {
        source
            .smtp
            .as_ref()
            .and_then(|smtp| smtp.get(name))
            .and_then(Value::as_str)
    };
    let message = source.message.uuid();
    let relay = match source.provider.as_str() {
        "ses" => Some(Relay::Ses {
            configuration_set: smtp_setting("configuration_set").ok_or_else(|| {
                Error::Inconsistent("the SES connection names no configuration set".to_owned())
            })?,
            message,
        }),
        "sendgrid" => Some(Relay::Sendgrid { message }),
        "mailgun" => Some(Relay::Mailgun {
            message,
            deliver_within: source.deadline.map_or(settings.retry_window, |deadline| {
                deadline
                    .0
                    .duration_since(now.0)
                    .try_into()
                    .unwrap_or(Duration::ZERO)
            }),
        }),
        _ => None,
    };
    // Gmail's feedback loop counts complaints per campaign of a workspace (all its other mail as
    // `direct`), under the platform's one sender id; only the managed MTA signs it and handles the
    // loop's reports, so only its mail carries it.
    let feedback_campaign = source.campaign.map_or_else(
        || "direct".to_owned(),
        |campaign| campaign.simple().to_string(),
    );
    let feedback_workspace = source.workspace.uuid().simple().to_string();
    let feedback_identifiers = [feedback_campaign.as_str(), feedback_workspace.as_str()];
    let raw = compose::compose_with_attachments(
        &Draft {
            message_id: &source.internet_message_id,
            from: Mailbox {
                name: source.from_name.as_deref(),
                address: &source.from_email,
            },
            reply_to: source.reply_to.as_deref().map(|address| Mailbox {
                name: None,
                address,
            }),
            to: &to,
            cc: &cc,
            bcc: &bcc,
            keep_bcc: source.transport == "api",
            subject: &source.subject,
            text: text.as_deref(),
            html: html.as_deref(),
            in_reply_to: source.in_reply_to.as_deref(),
            references: &references,
            list_unsubscribe,
            feedback_id: (source.provider == "norbelys").then_some(FeedbackId {
                identifiers: &feedback_identifiers,
                sender: FEEDBACK_SENDER,
            }),
            relay,
        },
        attachments,
    )?;

    // The envelope.
    let mail_from = match (source.provider.as_str(), smtp_setting("host")) {
        ("norbelys", Some(host)) => format!("bounce+{}@{host}", message.simple()),
        ("norbelys", None) => {
            return Err(Error::Inconsistent(
                "the managed MTA's connection names no host".to_owned(),
            ));
        }
        _ => source.from_email.clone(),
    };
    let envelope = Envelope::new(
        &mail_from,
        source
            .to
            .iter()
            .chain(&source.cc)
            .chain(&source.bcc)
            .map(String::as_str),
    )?;
    Ok(Prepared {
        envelope,
        raw,
        _attachment_slot: None,
        internet_message_id: source.internet_message_id.clone(),
    })
}

/// Mailboxes without display names, for recipients.
fn mailboxes(addresses: &[String]) -> Vec<Mailbox<'_>> {
    addresses
        .iter()
        .map(|address| Mailbox {
            name: None,
            address,
        })
        .collect()
}

/// The content a message that owns it (direct, reply, transactional) stores, rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub subject: String,
    pub html: Option<String>,
    pub text: Option<String>,
}

/// Renders a message's own templates with its frozen `context`: the creation check, whose
/// result the message stores. Every part is rendered, so one answer names every part at fault.
///
/// # Errors
///
/// One [`TemplateError`] per part that does not render.
pub fn render_own(
    subject: &str,
    html: Option<&str>,
    text: Option<&str>,
    context: &Value,
) -> Result<Rendered, Vec<TemplateError>> {
    let subject = templates::render(Part::Subject, subject, context);
    let html = html
        .map(|html| templates::render(Part::Html, html, context))
        .transpose();
    let text = text
        .map(|text| templates::render(Part::Text, text, context))
        .transpose();
    match (subject, html, text) {
        (Ok(subject), Ok(html), Ok(text)) => {
            let text = match (text, html.as_deref()) {
                (Some(text), _) => Some(text),
                (None, Some(html)) => Some(plain::from_html(html).map_err(|error| {
                    vec![TemplateError {
                        part: Part::Html,
                        detail: format!("the HTML cannot be converted to plain text: {error}"),
                    }]
                })?),
                (None, None) => None,
            };
            Ok(Rendered {
                subject,
                html,
                text,
            })
        }
        (subject, html, text) => Err([subject.err(), html.err(), text.err()]
            .into_iter()
            .flatten()
            .collect()),
    }
}

/// A campaign variant's revision, as templates.
#[derive(Debug, Clone, Copy)]
pub struct Variant<'a> {
    pub subject: &'a str,
    pub preheader: Option<&'a str>,
    pub html: &'a str,
    pub text: Option<&'a str>,
}

/// Checks that a variant renders with a message's frozen `context` (and a stand-in unsubscribe
/// link), as it will when sent, and returns its rendered subject, which the message stores.
///
/// # Errors
///
/// One [`TemplateError`] per part that does not render.
pub fn check_variant(variant: &Variant<'_>, context: &Value) -> Result<String, Vec<TemplateError>> {
    let context = namespaces::with_unsubscribe(context, UNSUBSCRIBE_STAND_IN);
    let subject = templates::render(Part::Subject, variant.subject, &context);
    let preheader = variant
        .preheader
        .map(|source| templates::render(Part::Preheader, source, &context))
        .transpose();
    let html = templates::render(Part::Html, variant.html, &context);
    let text = variant
        .text
        .map(|source| templates::render(Part::Text, source, &context))
        .transpose();
    match (subject, preheader, html, text) {
        (Ok(subject), Ok(_), Ok(_), Ok(_)) => Ok(subject),
        (subject, preheader, html, text) => {
            Err([subject.err(), preheader.err(), html.err(), text.err()]
                .into_iter()
                .flatten()
                .collect())
        }
    }
}

#[cfg(test)]
mod tests;
