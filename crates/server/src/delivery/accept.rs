//! Accepting a message: the one creation contract every message goes through, whoever creates
//! it.
//!
//! # Callers
//!
//! | Caller | Function | Kind |
//! |---|---|---|
//! | `POST /v1/messages`, the direct form | [`create`] without `reply` | `direct` |
//! | `POST /v1/messages`, the reply form (the inbox) | [`create`] with `reply` | `reply` |
//! | `POST /v1/messages`, the step-content form (a step for a person not enrolled, or its preview) | [`step_content`] | `direct` |
//! | `campaign.materialise`, `enrollment.advance`, the `message.generate` checkpoint | [`step`] | `campaign` |
//! | sign-in codes, a new user's welcome and invitations (identity) | [`transactional`] | `transactional` |
//! | connection health and failing webhook endpoint notices (`senders::health`, `webhooks::deliver`), break-glass notices (`admin owners break-glass`) | [`transactional`] | `transactional` |
//!
//! Each takes the caller's transaction and never commits: the message exists exactly when the
//! caller's change commits (an API request's idempotency row, a job's checkpoint, a sign-in
//! challenge). After the commit the caller sends the wake-up with [`wake`].
//!
//! # What one acceptance writes
//!
//! 1. The **thread**: a new one, whose root is this message, or the existing thread it continues
//!    (locked, its latest outbound ids moved to this message).
//! 2. The **message** row: kind, identity and connection (frozen), recipients, the rendered
//!    subject, the rendered bodies of mail that owns its content (a campaign message never holds a
//!    body: it points at its variant revision), the frozen render context, the Message-ID, the
//!    thread, the frozen tracking settings and `send_at`.
//! 3. Its **delivery queue** row (`queued`, due at `send_at`; `paced` for campaign mail only,
//!    so mail created through the API is claimed when due and never follows a mailbox's cold
//!    cadence), with the message's `expires_at` as its first deadline.
//! 4. `message.queued` in the outbox, when an endpoint of the workspace subscribes to it.
//! 5. For a step, the enrollment's pointer to the message.
//!
//! Before writing, it checks what every message must meet: a live, enabled sender identity; an
//! envelope of 1 to 50 `To` addresses and at most 150 in all, or fewer where the identity's
//! connection's provider takes fewer a message (Amazon SES 50, Gmail over SMTP 100; refused at
//! once, since the provider would refuse the message whole), each a valid address, none twice;
//! no recipient suppressed (`suppressed`, naming the reason, never the evidence); a schedule at
//! most seven days ahead whose `expires_at` comes after it; templates that render with the
//! frozen context (every part checked, each error with its pointer).
//!
//! # The Message-ID
//!
//! `<{message}.{thread}.{tag}@{domain of the From address}>` (`domain::messages`), the tag
//! computed with the deployment's Message-ID key. Message and thread ids are generated here, so
//! the id is known before the rows are written and a reply's `In-Reply-To` correlates to its
//! thread without any lookup ([`correlate`]). No directory row is written for it: the directory
//! (`message_id_directory`) holds only the ids a provider puts in place of ours (Amazon SES),
//! which the finish of the submission learns from the provider's reply.
//!
//! # A step's message, once
//!
//! [`step`] locks the enrollment and checks, under that lock, that it is `active`, at the
//! position the creator expects, that the step still has the revision the creator computed the
//! content from, and that no message of the step exists yet (`message_id` null). Only then does
//! it write, and set `message_id` (and the thread's root on the conversation's first message).
//! A second creator of the same step finds the pointer set and stops; a creator holding a stale
//! step finds another position or revision and stops; neither is an error ([`StepOutcome`]).
//!
//! The thread: the conversation's first message (`thread_root_message_id` null, also after a
//! sender was replaced and the step re-armed) opens a new thread and becomes its root; a later
//! step whose revision says `same_thread` continues the root's thread, answering its latest
//! outbound message; a later step that does not opens a thread of its own and leaves the root
//! as it is.
//!
//! # Transactional mail
//!
//! Sign-in codes, a new user's welcome, invitations and the platform's notices (connection
//! health, failing webhook endpoints, break-glass sessions) belong to the `system` workspace, which
//! owns the platform's own connections. [`transactional`] switches the caller's transaction to that
//! workspace for its writes and back afterwards, so a challenge, a first sign-in or an invitation
//! and its mail commit together. The sender is the system workspace's oldest enabled identity
//! tagged `transactional` on a live connection: an operator connects the platform's relay to the
//! system workspace through the ordinary API (`norbelys-server admin system-api-key` grants the
//! key) and tags its identity, and can move the mail to another identity by moving the tag,
//! without a restart. The values of a transactional message (a code, a link) are useless once its
//! `expires_at` passes, which is also the message's own deadline.
//!
//! Each variant is an [`Email`] of the one frame of the platform's own mail
//! (`rendering::frame`): a subject, a heading, a body of blocks, at most one button and a note,
//! all templates over the `variables` its variant fills, rendered at acceptance into an HTML body
//! and a text body with the same words. The HTML links the brand mark on the stand-in tracking
//! origin, which the sender replaces with the tracking origin when it prepares the message.
//!
//! A transactional message is not refused for a suppressed address at acceptance: a
//! suppression is a workspace's exclusion of its own outreach, while a sign-in code or an
//! invitation is mail its recipient asked for or expects.
//!
//! # Lock order
//!
//! The enrollment (a step), then the thread being continued. Both come before the rows this
//! module inserts, and no path here takes a connection, identity or queue lock, so it never
//! waits on a delivery path in the opposite order (they lock connections and queue rows, never
//! enrollments after them).

use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::crypto::Keys;
use crate::db::{self, Database, Tx};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{
    Campaign, Enrollment, Id, Message, Person, SenderIdentity, Step, Thread, Variant, WorkspaceId,
};
use crate::domain::messages::{self, Kind, RECIPIENTS_MAX, ScheduleError, TO_MAX};
use crate::domain::policy::delivery as policy;
use crate::domain::senders::Provider;
use crate::domain::time::Timestamp;
use crate::rendering::frame::{self, Block, Button, Email};
use crate::rendering::namespaces::{self, Namespaces};
use crate::rendering::templates::TemplateError;
use crate::rendering::{self, RENDER_VERSION, Rendered};
use crate::webhooks::EventType;
use crate::webhooks::outbox::{self, Event};

/// The tag that makes an identity of the `system` workspace the sender of transactional mail.
pub const TRANSACTIONAL_TAG: &str = "transactional";

/// A message accepted for sending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub message: Id<Message>,
    pub thread: Id<Thread>,
    /// Its `Message-ID`, angle brackets included.
    pub internet_message_id: String,
    /// When it is due.
    pub send_at: Timestamp,
}

/// Why a message was not accepted. The HTTP mapping is `delivery::http`'s.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A referenced resource does not exist in the workspace (`404`).
    #[error("no such {0}")]
    NotFound(&'static str),
    /// A field breaks a rule, at its JSON pointer (`422 validation_failed`).
    #[error("{pointer}: {detail}")]
    Invalid { pointer: String, detail: String },
    /// A resource's state forbids it (`409 invalid_state`).
    #[error("{0}")]
    InvalidState(String),
    /// Templates that do not render, one error per part (`422 validation_failed`).
    #[error("the templates do not render")]
    Template(Vec<TemplateError>),
    /// A recipient is suppressed (`422 suppressed`).
    #[error("`{email}` is suppressed ({reason})")]
    Suppressed { email: String, reason: String },
    /// The `system` workspace has no identity tagged `transactional` on a live connection.
    #[error("no transactional sender is configured")]
    NoTransactionalSender,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl Error {
    fn invalid(pointer: &str, detail: impl Into<String>) -> Self {
        Self::Invalid {
            pointer: pointer.to_owned(),
            detail: detail.into(),
        }
    }
}

/// Who a message is from: an identity by id, or the live identity holding an address.
#[derive(Debug, Clone, Copy)]
pub enum Sender<'a> {
    Identity(Id<SenderIdentity>),
    Address(&'a EmailAddress),
}

/// A reply's place in its thread.
#[derive(Debug, Clone, Copy)]
pub struct ReplyTo<'a> {
    /// The thread the reply continues; its identity sends it.
    pub thread: Id<Thread>,
    /// The `Message-ID` the reply answers (an inbound message's); the thread's latest outbound
    /// message when absent.
    pub in_reply_to: Option<&'a str>,
}

/// A message that owns its content: a direct message, or a reply.
#[derive(Debug, Clone)]
pub struct NewMessage<'a> {
    pub from: Sender<'a>,
    pub to: &'a [EmailAddress],
    pub cc: &'a [EmailAddress],
    pub bcc: &'a [EmailAddress],
    /// The subject template.
    pub subject: &'a str,
    /// The HTML body template; at least one body is required.
    pub html: Option<&'a str>,
    /// The text body template.
    pub text: Option<&'a str>,
    /// The `variables` namespace.
    pub variables: Option<Map<String, Value>>,
    pub send_at: Option<Timestamp>,
    pub expires_at: Option<Timestamp>,
    /// `Some` for a reply.
    pub reply: Option<ReplyTo<'a>>,
    /// The request's `Idempotency-Key`, kept on the row for display.
    pub idempotency_key: Option<&'a str>,
}

/// A step's message for one enrollment.
#[derive(Debug, Clone)]
pub struct StepMessage {
    pub enrollment: Id<Enrollment>,
    pub campaign: Id<Campaign>,
    pub step: Id<Step>,
    /// The position the creator expects the enrollment to be at (the step's).
    pub position: i32,
    /// The step revision the creator chose the variant from.
    pub step_revision: i32,
    pub variant: Id<Variant>,
    pub variant_version: i32,
    /// The identity that sends it (the conversation's, or the one the pool assigned).
    pub identity: Id<SenderIdentity>,
    /// The `variables` namespace: generated snippets, when the step uses them.
    pub variables: Option<Map<String, Value>>,
    /// When the step asks for personalisation snippets and none were written: why (`deadline`,
    /// `over_budget`, …), recorded on the message (`messages.snippets_fallback`) so the API
    /// shows that its template's defaults were used. `None` otherwise.
    pub snippets_fallback: Option<&'static str>,
    /// When it is due; now when absent.
    pub send_at: Option<Timestamp>,
}

/// What [`step`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// The message exists now.
    Created(Accepted),
    /// Nothing was written: the creator is late or stale.
    Stale(Stale),
}

/// Why a step's creator stopped without writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stale {
    /// The enrollment is not `active` (paused, stopped, replied, completed).
    NotActive,
    /// The enrollment is at another position.
    Moved,
    /// The step has another revision now, or another position.
    Revised,
    /// The step's message exists already.
    AlreadyCreated,
}

/// The platform's own mail, sent by the `system` workspace. The tests iterate its kinds
/// (`TransactionalKind`), so a new variant fails them until it renders in the frame.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(
    test,
    derive(strum::EnumDiscriminants),
    strum_discriminants(name(TransactionalKind), derive(strum::EnumIter))
)]
pub enum Transactional<'a> {
    /// A sign-in code and its link; useful until `expires_at`.
    SignInCode {
        to: &'a EmailAddress,
        code: &'a str,
        link: &'a str,
        expires_at: Timestamp,
    },
    /// The welcome of a user whose first sign-in created their account: what to do first, and
    /// where. Useful until `expires_at`, a few days: later it would arrive after the person found
    /// their way.
    Welcome {
        to: &'a EmailAddress,
        /// The dashboard's primary origin, which its button opens.
        dashboard: &'a str,
        expires_at: Timestamp,
    },
    /// An invitation to a workspace; useful until it expires.
    Invitation {
        to: &'a EmailAddress,
        workspace_name: &'a str,
        /// Who invited, as shown (a name or an address).
        inviter: Option<&'a str>,
        /// The membership role offered.
        role: &'a str,
        link: &'a str,
        expires_at: Timestamp,
    },
    /// The health of some of a workspace's sender connections changed within one 15-minute
    /// window, recoveries included: each listed as the window left it. Useful until `expires_at`.
    ConnectionHealth {
        to: &'a EmailAddress,
        workspace_name: &'a str,
        /// The window's start (UTC).
        from: Timestamp,
        /// The window's end.
        until: Timestamp,
        /// The connections, in the order shown.
        connections: &'a [HealthLine],
        expires_at: Timestamp,
    },
    /// An operator opened a break-glass session for an owner of the workspace: told to its
    /// other owners. Useful until the session ends (`expires_at`).
    BreakGlass {
        to: &'a EmailAddress,
        workspace_name: &'a str,
        /// The owner holding the session, as shown (a name or an address).
        owner: &'a str,
        /// The reason the operator recorded.
        reason: &'a str,
        expires_at: Timestamp,
    },
    /// A webhook endpoint of the workspace keeps failing: one of its deliveries used up its
    /// retries (`disabled` false), or it failed for five days without a success and was disabled
    /// (`disabled` true). Told to the workspace's admins; useful until `expires_at`.
    WebhookFailure {
        to: &'a EmailAddress,
        workspace_name: &'a str,
        /// The endpoint's URL.
        url: &'a str,
        /// When its failures began: its first failure after a success.
        failing_since: Timestamp,
        /// Whether it was disabled.
        disabled: bool,
        /// What its last attempt met.
        last_error: &'a str,
        expires_at: Timestamp,
    },
}

/// One connection a [`Transactional::ConnectionHealth`] notice lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthLine {
    /// The connection's account (its address).
    pub account: String,
    /// Its provider, as stored (`google`, `smtp`, …).
    pub provider: String,
    /// What its status means for a person (`working`, `needs to be reconnected`, …).
    pub status: String,
    /// Its status detail: the provider's or the check's words, when there are some.
    pub detail: Option<String>,
    /// Whether a person paused it.
    pub paused: bool,
}

/// The sender identity of a message, as frozen into it.
struct Identity {
    id: Id<SenderIdentity>,
    connection: Uuid,
    email: String,
    name: Option<String>,
    reply_to: Option<String>,
    /// The most recipients a message may have through the identity's connection: its
    /// provider's per-message limit (`domain::policy::delivery::recipients_max`).
    recipients_max: usize,
}

/// Where a message goes in its thread.
enum ThreadPlan {
    /// A new thread, whose root is the message.
    New {
        person: Option<Uuid>,
        campaign: Option<Uuid>,
    },
    /// An existing thread, already locked, answering `in_reply_to`.
    Continue {
        thread: Id<Thread>,
        in_reply_to: Option<String>,
    },
}

/// A campaign message's ancestry.
struct Ancestry<'a> {
    step: &'a StepMessage,
}

/// Everything one insert writes.
struct Row<'a> {
    kind: Kind,
    identity: &'a Identity,
    thread: ThreadPlan,
    person: Option<Uuid>,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    html: Option<String>,
    text: Option<String>,
    context: Value,
    ancestry: Option<Ancestry<'a>>,
    tracking: Value,
    send_at: Timestamp,
    expires_at: Option<Timestamp>,
    idempotency_key: Option<&'a str>,
}

/// Accepts a direct message or a reply (see the module), in the caller's transaction.
///
/// # Errors
///
/// See [`Error`]: an unknown identity or thread, an envelope, schedule or template that breaks
/// a rule, a suppressed recipient, a disabled identity, or the database.
pub async fn create(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    new: &NewMessage<'_>,
) -> Result<Accepted, Error> {
    let identity = identity(tx, workspace, new.from).await?;
    let (thread, thread_person) = match new.reply {
        None => (None, None),
        Some(reply) => {
            let thread = sqlx::query!(
                "SELECT sender_identity_id, person_id, last_internet_message_id
                   FROM threads WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
                workspace.uuid(),
                reply.thread.uuid(),
            )
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::NotFound("thread"))?;
            if thread.sender_identity_id != identity.id.uuid() {
                return Err(Error::invalid(
                    "/from",
                    "A reply is sent from its thread's sender identity.",
                ));
            }
            let in_reply_to = reply
                .in_reply_to
                .map(str::to_owned)
                .or(thread.last_internet_message_id);
            (
                Some(ThreadPlan::Continue {
                    thread: reply.thread,
                    in_reply_to,
                }),
                thread.person_id,
            )
        }
    };
    let text = |addresses: &[EmailAddress]| -> Vec<String> {
        addresses.iter().map(|a| a.as_str().to_owned()).collect()
    };
    let (to, cc, bcc) = (text(new.to), text(new.cc), text(new.bcc));
    let recipients = check_envelope(&identity.email, &to, &cc, &bcc, identity.recipients_max)?;
    refuse_suppressed(tx, workspace, &recipients).await?;
    let send_at = schedule(new.send_at, new.expires_at)?;
    if new.html.is_none() && new.text.is_none() {
        return Err(Error::invalid(
            "/html",
            "Give an `html` or a `text` body, or both.",
        ));
    }
    let person =
        match thread_person {
            Some(person) => Some(person),
            None => match to.first() {
                Some(primary) => sqlx::query_scalar!(
                    "SELECT id FROM people WHERE workspace_id = $1 AND email_key = ascii_lower($2)",
                    workspace.uuid(),
                    primary,
                )
                .fetch_optional(&mut **tx)
                .await?,
                None => None,
            },
        };
    let person_namespace = match person {
        Some(person) => namespaces::person(tx, workspace, Id::<Person>::from_uuid(person)).await?,
        None => None,
    };
    let context = Namespaces {
        person: person_namespace,
        sender: namespaces::sender(&identity.email, identity.name.as_deref()),
        variables: namespaces::variables(new.variables.clone()),
        ..Namespaces::default()
    }
    .to_value();
    let rendered = rendering::render_own(new.subject, new.html, new.text, &context)
        .map_err(Error::Template)?;
    let kind = if new.reply.is_some() {
        Kind::Reply
    } else {
        Kind::Direct
    };
    insert(
        tx,
        keys,
        workspace,
        Row {
            kind,
            identity: &identity,
            thread: thread.unwrap_or(ThreadPlan::New {
                person,
                campaign: None,
            }),
            person,
            to,
            cc,
            bcc,
            subject: rendered.subject,
            html: rendered.html,
            text: rendered.text,
            context,
            ancestry: None,
            tracking: json!({}),
            send_at,
            expires_at: new.expires_at,
            idempotency_key: new.idempotency_key,
        },
    )
    .await
}

/// Creates a step's message for one enrollment, once (see the module), in the caller's
/// transaction (a job's chunk).
///
/// # Errors
///
/// An unknown enrollment, step revision, variant revision or identity; a disabled identity; a
/// suppressed recipient (the creator stops the enrollment); templates that do not render for this
/// person (the creator fails the enrollment); or the database.
pub async fn step(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    new: &StepMessage,
) -> Result<StepOutcome, Error> {
    let enrollment = sqlx::query!(
        "SELECT campaign_id, person_id, status, current_position, message_id, thread_root_message_id
           FROM enrollments WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        workspace.uuid(),
        new.enrollment.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .filter(|row| row.campaign_id == new.campaign.uuid())
    .ok_or(Error::NotFound("enrollment"))?;
    if enrollment.status != "active" {
        return Ok(StepOutcome::Stale(Stale::NotActive));
    }
    if enrollment.current_position != new.position {
        return Ok(StepOutcome::Stale(Stale::Moved));
    }
    if enrollment.message_id.is_some() {
        return Ok(StepOutcome::Stale(Stale::AlreadyCreated));
    }
    let revision = sqlx::query!(
        "SELECT s.position, s.current_revision, r.same_thread
           FROM steps s JOIN step_revisions r ON r.workspace_id = s.workspace_id AND r.step_id = s.id AND r.revision = $4
          WHERE s.workspace_id = $1 AND s.id = $2 AND s.campaign_id = $3",
        workspace.uuid(),
        new.step.uuid(),
        new.campaign.uuid(),
        new.step_revision,
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound("step revision"))?;
    if revision.position != new.position || revision.current_revision != Some(new.step_revision) {
        return Ok(StepOutcome::Stale(Stale::Revised));
    }
    let variant = sqlx::query!(
        "SELECT v.subject, v.preheader, v.html, v.text, v.cc, v.bcc
           FROM step_revision_variants o
           JOIN variant_revisions v ON v.workspace_id = o.workspace_id AND v.variant_id = o.variant_id AND v.version = o.variant_version
          WHERE o.workspace_id = $1 AND o.step_id = $2 AND o.step_revision = $3 AND o.variant_id = $4 AND o.variant_version = $5",
        workspace.uuid(),
        new.step.uuid(),
        new.step_revision,
        new.variant.uuid(),
        new.variant_version,
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound("variant revision"))?;
    let identity = identity(tx, workspace, Sender::Identity(new.identity)).await?;
    let person = namespaces::person(tx, workspace, Id::from_uuid(enrollment.person_id))
        .await?
        .ok_or(Error::NotFound("person"))?;
    let to = vec![
        person
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    ];
    let recipients = check_envelope(
        &identity.email,
        &to,
        &variant.cc,
        &variant.bcc,
        identity.recipients_max,
    )?;
    refuse_suppressed(tx, workspace, &recipients).await?;
    let send_at = schedule(new.send_at, None)?;
    let (campaign, step) = namespaces::campaign_and_step(tx, workspace, new.campaign, new.step)
        .await?
        .ok_or(Error::NotFound("step"))?;
    let tracking = sqlx::query!(
        r#"SELECT c.track_opens, c.track_clicks, d.hostname AS "hostname?"
             FROM campaigns c
             LEFT JOIN sending_domains d ON d.workspace_id = c.workspace_id AND d.id = c.tracking_domain_id
                                        AND d.tracking_enabled AND d.status = 'active'
            WHERE c.workspace_id = $1 AND c.id = $2"#,
        workspace.uuid(),
        new.campaign.uuid(),
    )
    .fetch_one(&mut **tx)
    .await?;
    let context = Namespaces {
        person: Some(person),
        sender: namespaces::sender(&identity.email, identity.name.as_deref()),
        campaign: Some(campaign),
        step: Some(step),
        variables: namespaces::variables(new.variables.clone()),
    }
    .to_value();
    let subject = rendering::check_variant(
        &rendering::Variant {
            subject: &variant.subject,
            preheader: variant.preheader.as_deref(),
            html: &variant.html,
            text: variant.text.as_deref(),
        },
        &context,
    )
    .map_err(Error::Template)?;
    let thread = match enrollment.thread_root_message_id {
        None => None,
        Some(_) if !revision.same_thread => None,
        Some(root) => sqlx::query!(
            r#"SELECT id AS "id: Id<Thread>", last_internet_message_id FROM threads
                WHERE workspace_id = $1 AND root_message_id = $2 FOR UPDATE"#,
            workspace.uuid(),
            root,
        )
        .fetch_optional(&mut **tx)
        .await?
        .map(|thread| ThreadPlan::Continue {
            thread: thread.id,
            in_reply_to: thread.last_internet_message_id,
        }),
    };
    let accepted = insert(
        tx,
        keys,
        workspace,
        Row {
            kind: Kind::Campaign,
            identity: &identity,
            thread: thread.unwrap_or(ThreadPlan::New {
                person: Some(enrollment.person_id),
                campaign: Some(new.campaign.uuid()),
            }),
            person: Some(enrollment.person_id),
            to,
            cc: variant.cc,
            bcc: variant.bcc,
            subject,
            html: None,
            text: None,
            context,
            ancestry: Some(Ancestry { step: new }),
            tracking: json!({
                "opens": tracking.track_opens,
                "clicks": tracking.track_clicks,
                "hostname": tracking.hostname,
            }),
            send_at,
            expires_at: None,
            idempotency_key: None,
        },
    )
    .await?;
    sqlx::query!(
        "UPDATE enrollments SET message_id = $3, next_run_at = NULL,
                thread_root_message_id = coalesce(thread_root_message_id, $3)
          WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        new.enrollment.uuid(),
        accepted.message.uuid(),
    )
    .execute(&mut **tx)
    .await?;
    Ok(StepOutcome::Created(accepted))
}

/// A campaign step's content for one person who is not enrolled: the step-content form of
/// `POST /v1/messages`, and, with `to`, the preview of that person's version at another address.
#[derive(Debug, Clone)]
pub struct StepContent<'a> {
    pub from: Sender<'a>,
    pub campaign: Id<Campaign>,
    pub step: Id<Step>,
    /// The variant whose latest version is sent.
    pub variant: Id<Variant>,
    pub variant_version: i32,
    /// The person whose version it is: the templates read their fields.
    pub person: Id<Person>,
    /// Another address to send the person's version to (a preview): the variant's `cc` and
    /// `bcc` are left out, the message belongs to no person, and an unsubscribe link renders as a
    /// stand-in.
    pub to: Option<&'a EmailAddress>,
    /// The `variables` namespace.
    pub variables: Option<Map<String, Value>>,
    pub send_at: Option<Timestamp>,
    /// The request's `Idempotency-Key`, kept on the row for display.
    pub idempotency_key: Option<&'a str>,
}

/// Accepts a step's content for a person who is not enrolled (see [`StepContent`]), in the
/// caller's transaction. It is a `direct` message: it owns its content, rendered now from the
/// variant's templates with the person's, the sender's, the campaign's and the step's
/// namespaces. It is not campaign mail (no enrollment, no tracking, no place in the campaign's
/// counters) and no window or cadence holds it, but sent to the person it carries campaign mail's
/// one-click unsubscribe (link and `List-Unsubscribe` header): it is the same commercial content to
/// a prospect. A preview sent to another address carries neither.
///
/// # Errors
///
/// An unknown identity, person or variant revision; an envelope, schedule or template that
/// breaks a rule; a suppressed recipient; a disabled identity; or the database.
pub async fn step_content(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    new: &StepContent<'_>,
) -> Result<Accepted, Error> {
    let identity = identity(tx, workspace, new.from).await?;
    let variant = sqlx::query!(
        "SELECT c.subject, c.html, c.text, c.cc, c.bcc
           FROM variant_revisions c JOIN variants v ON v.workspace_id = c.workspace_id AND v.id = c.variant_id
          WHERE c.workspace_id = $1 AND c.variant_id = $2 AND c.version = $3 AND v.step_id = $4",
        workspace.uuid(),
        new.variant.uuid(),
        new.variant_version,
        new.step.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound("variant"))?;
    let person = namespaces::person(tx, workspace, new.person)
        .await?
        .ok_or(Error::NotFound("person"))?;
    let preview = new.to.is_some();
    let to = vec![match new.to {
        Some(address) => address.as_str().to_owned(),
        None => person
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }];
    let (cc, bcc) = if preview {
        (Vec::new(), Vec::new())
    } else {
        (variant.cc, variant.bcc)
    };
    let recipients = check_envelope(&identity.email, &to, &cc, &bcc, identity.recipients_max)?;
    refuse_suppressed(tx, workspace, &recipients).await?;
    let send_at = schedule(new.send_at, None)?;
    let (campaign, step) = namespaces::campaign_and_step(tx, workspace, new.campaign, new.step)
        .await?
        .ok_or(Error::NotFound("step"))?;
    let context = Namespaces {
        person: Some(person),
        sender: namespaces::sender(&identity.email, identity.name.as_deref()),
        campaign: Some(campaign),
        step: Some(step),
        variables: namespaces::variables(new.variables.clone()),
    }
    .to_value();
    // The unsubscribe link exists only when the message is sent: a stand-in renders now, and the
    // sender replaces it with the person's own link (a preview keeps the stand-in, and no header).
    let rendered_with = namespaces::with_unsubscribe(&context, rendering::UNSUBSCRIBE_STAND_IN);
    let rendered = rendering::render_own(
        &variant.subject,
        Some(&variant.html),
        variant.text.as_deref(),
        &rendered_with,
    )
    .map_err(Error::Template)?;
    let person = (!preview).then_some(new.person.uuid());
    insert(
        tx,
        keys,
        workspace,
        Row {
            kind: Kind::Direct,
            identity: &identity,
            thread: ThreadPlan::New {
                person,
                campaign: None,
            },
            person,
            to,
            cc,
            bcc,
            subject: rendered.subject,
            html: rendered.html,
            text: rendered.text,
            context,
            ancestry: None,
            tracking: json!({}),
            send_at,
            expires_at: None,
            idempotency_key: new.idempotency_key,
        },
    )
    .await
}

/// Accepts a transactional message of the `system` workspace inside the caller's transaction,
/// whatever workspace (or none) that transaction is in, and leaves it in that workspace again
/// (see the module).
///
/// # Errors
///
/// [`Error::NoTransactionalSender`] when the system workspace has no sender for it; a template or
/// schedule error (a programming error here); or the database.
pub async fn transactional(
    tx: &mut Tx,
    keys: &Keys,
    mail: &Transactional<'_>,
) -> Result<Accepted, Error> {
    let previous = db::switch_workspace(tx, crate::jobs::SYSTEM_WORKSPACE).await?;
    let accepted = transactional_in_system(tx, keys, mail).await;
    db::restore_workspace(tx, previous).await?;
    accepted
}

async fn transactional_in_system(
    tx: &mut Tx,
    keys: &Keys,
    mail: &Transactional<'_>,
) -> Result<Accepted, Error> {
    let workspace = crate::jobs::SYSTEM_WORKSPACE;
    let identity = sqlx::query!(
        r#"SELECT i.id AS "id: Id<SenderIdentity>", i.connection_id, i.email, i.name, i.reply_to
             FROM sender_identities i
             JOIN connections c ON c.workspace_id = i.workspace_id AND c.id = i.connection_id
            WHERE i.workspace_id = $1 AND i.enabled AND i.archived_at IS NULL AND c.status <> 'archived'
              AND $2 = ANY (i.tags)
            ORDER BY i.created_at, i.id LIMIT 1"#,
        workspace.uuid(),
        TRANSACTIONAL_TAG,
    )
    .fetch_optional(&mut **tx)
    .await?
    .map(|row| Identity {
        id: row.id,
        connection: row.connection_id,
        email: row.email,
        name: row.name,
        reply_to: row.reply_to,
        // Transactional mail has one recipient, within every provider's limit.
        recipients_max: RECIPIENTS_MAX,
    })
    .ok_or(Error::NoTransactionalSender)?;
    let content = content(
        mail,
        &identity.email,
        identity.name.as_deref(),
        crate::process::now(),
    )?;
    let recipients = vec![content.to.as_str().to_owned()];
    check_envelope(
        &identity.email,
        &recipients,
        &[],
        &[],
        identity.recipients_max,
    )?;
    let send_at = schedule(None, Some(content.expires_at))?;
    insert(
        tx,
        keys,
        workspace,
        Row {
            kind: Kind::Transactional,
            identity: &identity,
            thread: ThreadPlan::New {
                person: None,
                campaign: None,
            },
            person: None,
            to: recipients,
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: content.rendered.subject,
            html: content.rendered.html,
            text: content.rendered.text,
            context: content.context,
            ancestry: None,
            tracking: json!({}),
            send_at,
            expires_at: Some(content.expires_at),
            idempotency_key: None,
        },
    )
    .await
}

/// A transactional message before it is written: its recipient, its usefulness, its frozen
/// context and its content, rendered in the frame of the platform's own mail.
struct Content<'a> {
    to: &'a EmailAddress,
    expires_at: Timestamp,
    context: Value,
    rendered: Rendered,
}

/// What `mail` says, sent by the identity whose address is `sender` (named `sender_name`): its
/// variant's [`Email`] rendered in the frame with the values the variant fills. `now` is the
/// instant a code's remaining minutes are counted from. Writes nothing.
///
/// # Errors
///
/// [`Error::Template`] when an email does not render with its variant's values: a programming
/// error, which the tests rule out for every variant.
fn content<'a>(
    mail: &Transactional<'a>,
    sender: &str,
    sender_name: Option<&str>,
    now: Timestamp,
) -> Result<Content<'a>, Error> {
    let (to, expires_at, email, variables) = match *mail {
        Transactional::SignInCode {
            to,
            code,
            link,
            expires_at,
        } => {
            let seconds = expires_at.0.duration_since(now.0).as_secs().max(0);
            let minutes = ((seconds + 59) / 60).max(1);
            (
                to,
                expires_at,
                &SIGN_IN,
                json!({ "code": code, "link": link, "minutes": minutes }),
            )
        }
        Transactional::Welcome {
            to,
            dashboard,
            expires_at,
        } => (
            to,
            expires_at,
            &WELCOME,
            json!({ "email": to.as_str(), "dashboard": dashboard }),
        ),
        Transactional::Invitation {
            to,
            workspace_name,
            inviter,
            role,
            link,
            expires_at,
        } => (
            to,
            expires_at,
            &INVITATION,
            json!({
                "workspace": workspace_name,
                "inviter": inviter,
                "role": role,
                "link": link,
                "expires_on": jiff::Zoned::new(expires_at.0, jiff::tz::TimeZone::UTC).date().to_string(),
            }),
        ),
        Transactional::ConnectionHealth {
            to,
            workspace_name,
            from,
            until,
            connections,
            expires_at,
        } => {
            let utc = |instant: Timestamp| jiff::Zoned::new(instant.0, jiff::tz::TimeZone::UTC);
            let lines = connections
                .iter()
                .map(|line| {
                    json!({
                        "account": line.account,
                        "provider": line.provider,
                        "status": line.status,
                        "detail": line.detail,
                        "paused": line.paused,
                    })
                })
                .collect::<Vec<_>>();
            (
                to,
                expires_at,
                &HEALTH,
                json!({
                    "workspace": workspace_name,
                    "count": connections.len(),
                    "day": utc(from).date().to_string(),
                    "from": utc(from).strftime("%H:%M").to_string(),
                    "until": utc(until).strftime("%H:%M").to_string(),
                    "connections": lines,
                }),
            )
        }
        Transactional::BreakGlass {
            to,
            workspace_name,
            owner,
            reason,
            expires_at,
        } => (
            to,
            expires_at,
            &BREAK_GLASS,
            json!({
                "workspace": workspace_name,
                "owner": owner,
                "reason": reason,
                "ends": jiff::Zoned::new(expires_at.0, jiff::tz::TimeZone::UTC).strftime("%Y-%m-%d %H:%M").to_string(),
            }),
        ),
        Transactional::WebhookFailure {
            to,
            workspace_name,
            url,
            failing_since,
            disabled,
            last_error,
            expires_at,
        } => (
            to,
            expires_at,
            &WEBHOOK,
            json!({
                "workspace": workspace_name,
                "url": url,
                "since": jiff::Zoned::new(failing_since.0, jiff::tz::TimeZone::UTC).strftime("%Y-%m-%d %H:%M").to_string(),
                "disabled": disabled,
                "last_error": last_error,
            }),
        ),
    };
    let context = Namespaces {
        sender: namespaces::sender(sender, sender_name),
        variables: namespaces::prune(variables),
        ..Namespaces::default()
    }
    .to_value();
    let rendered = rendering::render_own(
        email.subject,
        Some(&frame::html(email)),
        Some(&frame::text(email)),
        &context,
    )
    .map_err(Error::Template)?;
    Ok(Content {
        to,
        expires_at,
        context,
        rendered,
    })
}

/// Sends the sender's wake-up after the transaction that accepted messages has committed: a
/// `NOTIFY` of its own, so a slow listener never holds the business transaction, and a failure
/// changes nothing (it is logged; the sender's sweep finds the message within seconds anyway).
pub async fn wake(db: &Database) {
    if let Err(error) = sqlx::query!("SELECT pg_notify($1, 'delivery')", crate::jobs::CHANNEL)
        .execute(db.pool())
        .await
    {
        tracing::warn!(error = %error, "delivery wake-up not sent");
    }
}

/// The message and thread a `Message-ID` names, when it is one of ours and its tag is this
/// deployment's: how a reply's `In-Reply-To` or `References` finds its thread without a lookup.
#[must_use]
pub fn correlate(keys: &Keys, value: &str) -> Option<(Id<Message>, Id<Thread>)> {
    let parsed = messages::parse_internet_message_id(value)?;
    keys.verify_message_id_tag(
        &messages::tag_payload(parsed.message, parsed.thread),
        &parsed.tag,
    )
    .then(|| (Id::from_uuid(parsed.message), Id::from_uuid(parsed.thread)))
}

/// The `Message-ID` of `message` in `thread`, sent from `from` (its domain is the id's).
#[must_use]
pub fn internet_message_id(
    keys: &Keys,
    message: Id<Message>,
    thread: Id<Thread>,
    from: &str,
) -> String {
    let domain = from.rsplit_once('@').map_or("", |(_, domain)| domain);
    let domain = if domain.is_ascii() && !domain.is_empty() {
        domain.to_owned()
    } else {
        // An internationalised domain is written in its ASCII form, as a header requires.
        match url::Host::parse(domain) {
            Ok(url::Host::Domain(ascii)) => ascii,
            _ => "message.invalid".to_owned(),
        }
    };
    let tag = keys.message_id_tag(&messages::tag_payload(message.uuid(), thread.uuid()));
    messages::internet_message_id(message.uuid(), thread.uuid(), &tag, &domain)
}

/// The live identity a message is from: not archived, on a connection that is not archived, and
/// enabled.
async fn identity(
    tx: &mut Tx,
    workspace: WorkspaceId,
    from: Sender<'_>,
) -> Result<Identity, Error> {
    let (id, address) = match from {
        Sender::Identity(id) => (Some(id.uuid()), None),
        Sender::Address(address) => (None, Some(address.as_str())),
    };
    let row = sqlx::query!(
        r#"SELECT i.id AS "id: Id<SenderIdentity>", i.connection_id, i.email, i.name, i.reply_to, i.enabled,
                  c.provider, c.smtp ->> 'host' AS smtp_host
             FROM sender_identities i
             JOIN connections c ON c.workspace_id = i.workspace_id AND c.id = i.connection_id
            WHERE i.workspace_id = $1 AND i.archived_at IS NULL AND c.status <> 'archived'
              AND ($2::uuid IS NULL OR i.id = $2) AND ($3::text IS NULL OR i.email_key = ascii_lower($3))"#,
        workspace.uuid(),
        id,
        address,
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound("sender identity"))?;
    if !row.enabled {
        return Err(Error::InvalidState(format!(
            "The sender identity {} is disabled; enable it to send from it.",
            row.id
        )));
    }
    let recipients_max = row
        .provider
        .parse::<Provider>()
        .map_or(RECIPIENTS_MAX, |provider| {
            policy::recipients_max(provider, row.smtp_host.as_deref())
        });
    Ok(Identity {
        id: row.id,
        connection: row.connection_id,
        email: row.email,
        name: row.name,
        reply_to: row.reply_to,
        recipients_max,
    })
}

/// The envelope's rules: 1 to [`TO_MAX`] `To` addresses, at most `max` in all (the sender's
/// provider's limit per message, never above [`RECIPIENTS_MAX`]), every one an address (the syntax
/// every address passes, then the transports' own parser), none twice by its comparison key.
/// Returns the recipients' keys.
fn check_envelope(
    from: &str,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    max: usize,
) -> Result<Vec<String>, Error> {
    if to.is_empty() || to.len() > TO_MAX {
        return Err(Error::invalid(
            "/to",
            format!("Give 1 to {TO_MAX} `to` addresses."),
        ));
    }
    let all: Vec<(String, &String)> = [("to", to), ("cc", cc), ("bcc", bcc)]
        .into_iter()
        .flat_map(|(field, addresses)| {
            addresses
                .iter()
                .enumerate()
                .map(move |(at, address)| (format!("/{field}/{at}"), address))
        })
        .collect();
    if all.len() > max {
        return Err(Error::invalid(
            "/to",
            if max < RECIPIENTS_MAX {
                format!(
                    "This sender's provider takes at most {max} recipients a message, `to`, `cc` and `bcc` together."
                )
            } else {
                format!(
                    "A message has at most {RECIPIENTS_MAX} recipients, `to`, `cc` and `bcc` together."
                )
            },
        ));
    }
    let mut seen = std::collections::HashMap::new();
    let mut keys = Vec::with_capacity(all.len());
    for (pointer, address) in &all {
        let key = EmailAddress::parse(address)
            .map_err(|error| Error::invalid(pointer, error.to_string()))?
            .key();
        if let Some(first) = seen.insert(key.clone(), pointer) {
            return Err(Error::invalid(
                pointer,
                format!("`{address}` is already a recipient at `{first}`."),
            ));
        }
        keys.push(key);
    }
    norbelys_mail::submission::Envelope::new(from, all.iter().map(|(_, address)| address.as_str()))
        .map_err(|error| {
            let pointer = match &error {
                norbelys_mail::submission::EnvelopeError::Address(bad) => all
                    .iter()
                    .find(|(_, address)| *address == bad)
                    .map_or("/from", |(pointer, _)| pointer.as_str()),
                _ => "/to",
            };
            Error::invalid(pointer, error.to_string())
        })?;
    Ok(keys)
}

/// Refuses a message to a suppressed address (by the recipients' comparison `keys`), naming the
/// first and its reason.
async fn refuse_suppressed(
    tx: &mut Tx,
    workspace: WorkspaceId,
    keys: &[String],
) -> Result<(), Error> {
    let suppressed = sqlx::query!(
        "SELECT email, reason FROM suppressions WHERE workspace_id = $1 AND email_key = ANY ($2::text[])
          ORDER BY email_key LIMIT 1",
        workspace.uuid(),
        keys,
    )
    .fetch_optional(&mut **tx)
    .await?;
    match suppressed {
        Some(row) => Err(Error::Suppressed {
            email: row.email,
            reason: row.reason,
        }),
        None => Ok(()),
    }
}

/// The due instant of a message (`domain::messages::schedule`), now as the clock reads it.
fn schedule(send_at: Option<Timestamp>, expires_at: Option<Timestamp>) -> Result<Timestamp, Error> {
    messages::schedule(
        send_at.map(|at| at.0),
        expires_at.map(|at| at.0),
        jiff::Timestamp::now(),
    )
    .map(Timestamp)
    .map_err(|error| match error {
        ScheduleError::TooFar => Error::invalid(
            "/send_at",
            "A message may be scheduled at most 7 days ahead.",
        ),
        ScheduleError::ExpiresFirst => Error::invalid(
            "/expires_at",
            "`expires_at` must come after the message is due (`send_at`, or now).",
        ),
    })
}

/// Writes the thread, the message, its queue row and its outbox event (see the module).
async fn insert(
    tx: &mut Tx,
    keys: &Keys,
    workspace: WorkspaceId,
    row: Row<'_>,
) -> Result<Accepted, Error> {
    let message = Id::<Message>::new();
    let thread = match &row.thread {
        ThreadPlan::New { .. } => Id::<Thread>::new(),
        ThreadPlan::Continue { thread, .. } => *thread,
    };
    let internet_message_id = internet_message_id(keys, message, thread, &row.identity.email);
    let in_reply_to = match &row.thread {
        ThreadPlan::New { person, campaign } => {
            sqlx::query!(
                "INSERT INTO threads (workspace_id, id, person_id, campaign_id, sender_identity_id, root_message_id,
                                      root_internet_message_id, last_message_id, last_internet_message_id, subject)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $6, $7, $8)",
                workspace.uuid(),
                thread.uuid(),
                *person,
                *campaign,
                row.identity.id.uuid(),
                message.uuid(),
                internet_message_id,
                row.subject,
            )
            .execute(&mut **tx)
            .await?;
            None
        }
        ThreadPlan::Continue { in_reply_to, .. } => {
            sqlx::query!(
                "UPDATE threads SET last_message_id = $3, last_internet_message_id = $4, last_activity_at = now()
                  WHERE workspace_id = $1 AND id = $2",
                workspace.uuid(),
                thread.uuid(),
                message.uuid(),
                internet_message_id,
            )
            .execute(&mut **tx)
            .await?;
            in_reply_to.clone()
        }
    };
    let step = row.ancestry.as_ref().map(|ancestry| ancestry.step);
    sqlx::query!(
        "INSERT INTO messages (workspace_id, id, kind, campaign_id, step_id, step_revision, variant_id, variant_version,
                               enrollment_id, sender_identity_id, connection_id, from_email, from_name, person_id,
                               to_addresses, cc, bcc, reply_to, subject, html, text_body, render_context, render_version,
                               rendered_at, internet_message_id, in_reply_to, thread_id, tracking, send_at, idempotency_key,
                               snippets_fallback, trace_parent)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23,
                 now(), $24, $25, $26, $27, $28, $29, $30, $31)",
        workspace.uuid(),
        message.uuid(),
        row.kind.as_str(),
        step.map(|step| step.campaign.uuid()),
        step.map(|step| step.step.uuid()),
        step.map(|step| step.step_revision),
        step.map(|step| step.variant.uuid()),
        step.map(|step| step.variant_version),
        step.map(|step| step.enrollment.uuid()),
        row.identity.id.uuid(),
        row.identity.connection,
        row.identity.email,
        row.identity.name,
        row.person,
        &row.to,
        &row.cc,
        &row.bcc,
        row.identity.reply_to,
        row.subject,
        row.html,
        row.text,
        row.context,
        RENDER_VERSION,
        internet_message_id,
        in_reply_to,
        thread.uuid(),
        row.tracking,
        row.send_at as _,
        row.idempotency_key,
        step.and_then(|step| step.snippets_fallback),
        // A message a request created keeps its `traceparent`: the wave that sends it links to it.
        crate::http::context::trace_parent(),
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "INSERT INTO delivery_queue (workspace_id, message_id, connection_id, run_at, paced, expires_at, deadline_at)
         VALUES ($1, $2, $3, $4, $5, $6, $6)",
        workspace.uuid(),
        message.uuid(),
        row.identity.connection,
        row.send_at as _,
        row.kind == Kind::Campaign,
        row.expires_at as _,
    )
    .execute(&mut **tx)
    .await?;
    super::content::accepted(tx, workspace, message).await?;
    let subscribed = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM webhook_endpoints
                           WHERE workspace_id = $1 AND enabled AND $2 = ANY (event_types)) AS "subscribed!""#,
        workspace.uuid(),
        EventType::MessageQueued.as_str(),
    )
    .fetch_one(&mut **tx)
    .await?;
    if subscribed {
        outbox::record(
            tx,
            workspace,
            Event {
                kind: EventType::MessageQueued,
                subject_type: "message",
                subject_id: message.uuid(),
                data: json!({
                    "message_id": message,
                    "kind": row.kind.as_str(),
                    "thread_id": thread,
                    "campaign_id": step.map(|step| step.campaign),
                    "send_at": row.send_at,
                }),
            },
        )
        .await?;
    }
    Ok(Accepted {
        message,
        thread,
        internet_message_id,
        send_at: row.send_at,
    })
}

/// The sign-in code; `variables` holds `code`, `link` and `minutes`.
const SIGN_IN: Email = Email {
    subject: "{{ variables.code }} is your Norbelys sign-in code",
    heading: "Your sign-in code",
    body: &[
        Block::Text(
            "Enter it in the browser where you asked for it. It works for {{ variables.minutes }} minute{% if variables.minutes != 1 %}s{% endif %}.",
        ),
        Block::Code("{{ variables.code }}"),
        Block::Text("Reading this on another device?"),
    ],
    button: Some(Button {
        label: "Sign in with this link",
        href: "{{ variables.link }}",
    }),
    note: "Didn't ask to sign in? Ignore this email: nobody gets in without the code or the link.",
};

/// The welcome of a new user; `variables` holds `email` (their address) and `dashboard` (the
/// dashboard's origin).
const WELCOME: Email = Email {
    subject: "Your Norbelys account is ready",
    heading: "Welcome to Norbelys",
    body: &[
        Block::Text(
            "Your account for <b>{{ variables.email }}</b> is ready. Three steps to your first campaign:",
        ),
        Block::Steps(&[
            (
                "Connect a mailbox.",
                "Google, Microsoft or any SMTP server.",
            ),
            (
                "Get a key.",
                "In the dashboard, or <code>norbelys login</code> from a terminal.",
            ),
            ("Approve and send.", "Nothing leaves until you approve it."),
        ]),
    ],
    button: Some(Button {
        label: "Open your dashboard",
        href: "{{ variables.dashboard }}",
    }),
    note: "The API reference is at <a href=\"https://docs.norbelys.com\">https://docs.norbelys.com</a>.",
};

/// The invitation; `variables` holds `workspace`, `inviter` (when known), `role`, `link` and
/// `expires_on`.
const INVITATION: Email = Email {
    subject: "{% if variables.inviter %}{{ variables.inviter }} invited you{% else %}You are invited{% endif %} to {{ variables.workspace }} on Norbelys",
    heading: "Join {{ variables.workspace }} on Norbelys",
    body: &[Block::Text(
        "{% if variables.inviter %}<b>{{ variables.inviter }}</b> invited you{% else %}You are invited{% endif %} to join <b>{{ variables.workspace }}</b> as {{ variables.role }}.",
    )],
    button: Some(Button {
        label: "Accept the invitation",
        href: "{{ variables.link }}",
    }),
    note: "The invitation expires on {{ variables.expires_on }}. If you did not expect it, ignore this email.",
};

/// The connection-health notice; `variables` holds `workspace`, `count`, `day`, `from` and
/// `until` (the window, UTC), and `connections` (each `account`, `provider`, `status`, `paused`
/// and, when there is one, `detail`).
const HEALTH: Email = Email {
    subject: "{% if variables.count == 1 %}A sender connection{% else %}{{ variables.count }} sender connections{% endif %} changed in {{ variables.workspace }}",
    heading: "{% if variables.count == 1 %}A sender connection changed{% else %}{{ variables.count }} sender connections changed{% endif %}",
    body: &[
        Block::Text(
            "Between {{ variables.from }} and {{ variables.until }} UTC on {{ variables.day }}, the health of {% if variables.count == 1 %}this connection{% else %}these connections{% endif %} in <b>{{ variables.workspace }}</b> changed. Where each stands now:",
        ),
        Block::Rows {
            each: "connection in variables.connections",
            title: "{{ connection.account }}",
            note: "{{ connection.provider }}",
            status: "{{ connection.status }}{% if connection.paused %}, paused{% endif %}",
            detail: "connection.detail",
        },
    ],
    button: None,
    note: "A connection that is not working sends nothing until someone fixes it, and the campaigns that use it wait. Open Norbelys to see each connection and what it needs.",
};

/// The break-glass notice; `variables` holds `workspace`, `owner`, `reason` and `ends` (the
/// session's end, UTC).
const BREAK_GLASS: Email = Email {
    subject: "A break-glass session was opened in {{ variables.workspace }}",
    heading: "A break-glass session was opened",
    body: &[
        Block::Text(
            "Norbelys support opened a break-glass session for <b>{{ variables.owner }}</b>, an owner of <b>{{ variables.workspace }}</b>, because the workspace's single sign-on could not be used.",
        ),
        Block::Text(
            "The session may change the workspace's single sign-on settings and its members, and nothing else. It ends at {{ variables.ends }} UTC at the latest.",
        ),
        Block::Text("The reason recorded: {{ variables.reason }}"),
    ],
    button: None,
    note: "The session is in the workspace's audit log. If you did not expect it, contact Norbelys support.",
};

/// The failing webhook endpoint's notice; `variables` holds `workspace`, `url`, `since` (its first
/// failure, UTC), `disabled` and `last_error`.
const WEBHOOK: Email = Email {
    subject: "{% if variables.disabled %}A webhook endpoint of {{ variables.workspace }} was disabled{% else %}A webhook endpoint of {{ variables.workspace }} keeps failing{% endif %}",
    heading: "{% if variables.disabled %}A webhook endpoint was disabled{% else %}A webhook endpoint keeps failing{% endif %}",
    body: &[
        Block::Text(
            "{% if variables.disabled %}Norbelys disabled the webhook endpoint <b>{{ variables.url }}</b> of <b>{{ variables.workspace }}</b>: every delivery to it has failed since {{ variables.since }} UTC, five days without a success. Nothing is sent to it until it is enabled again.{% else %}Norbelys could not deliver an event to the webhook endpoint <b>{{ variables.url }}</b> of <b>{{ variables.workspace }}</b>: every attempt failed for about three days, and the endpoint has been failing since {{ variables.since }} UTC. Deliveries go on; after five days without a success the endpoint is disabled.{% endif %}",
        ),
        Block::Text(
            "{% if variables.disabled %}Once it answers again, enable it and replay what it missed: its deliveries stay replayable for at least a day after it was disabled.{% else %}Once it answers again, its failed deliveries can be retried or replayed.{% endif %}",
        ),
    ],
    button: None,
    note: "The last attempt met: {{ variables.last_error }}",
};

#[cfg(test)]
mod tests;
