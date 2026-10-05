//! The delivery resources under `/v1`: `messages` and `delivery_events`.
//!
//! `POST /messages` needs `messages:send` and an `Idempotency-Key` (the idempotency middleware
//! enforces it before the handler runs), and answers `202` with the queued message and its
//! `Location`: the API never waits for a provider, and the message's `state` is what to poll.
//! Its step-content form sends a campaign step's content to people who are not enrolled (or
//! previews one person's version at another address) as direct messages, rendered now from the
//! variant (`delivery::accept::step_content`); for up to 100 `person_ids` it answers a result per
//! person, each the message or the problem a single request would have answered.
//! Reads need `messages:read`.
//!
//! # What a message shows
//!
//! Its envelope (from, reply-to, to, cc, bcc), subject, kind, state, the resources it belongs to
//! (connection, identity, person, thread, campaign ancestry), its Message-ID, its tracking, its
//! timestamps, and three children, in every response, list or retrieve:
//!
//! - `attempts`: its latest 20 attempts, newest first (`has_more`), and `attempts_count`; older
//!   attempts are read through an export;
//! - `events`: its first 20 delivery events in the order they were observed (`has_more`, and
//!   `url`: the delivery event list filtered on the message, which serves the rest);
//! - `holds`: one per held recipient, at most its 150 recipients.
//!
//! Each child is one indexed query for a whole page, batched over the page's message ids. The
//! bodies are not shown: a direct message's were rendered into its row and a campaign message's
//! are rendered from its variant when it is sent.
//!
//! # Delivery events
//!
//! Observations about a message's fate after (or during) submission, from a named source with a
//! confidence. Each shows `observed_at` (the source's own time), `received_at` (when Norbelys
//! received it: the time of the provider callback that carried it, else when it was recorded)
//! and `processed_at` (when it was recorded). The list filters by message, campaign, person,
//! kind and recipient; campaign and person are read from the event's message.
//!
//! # Further operations
//!
//! `messages.cancel` and `messages.resolve` (the delivery paths) and `messages.release_holds`
//! (evidence) are added here as further routes and answer [`read`]'s object.

use std::collections::HashMap;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use super::accept::{self, NewMessage, Sender};
use crate::db::Tx;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{
    Attempt, Campaign, Connection, DeliveryEvent, Enrollment, Id, Message, Person, SenderIdentity,
    Step, Thread, Variant, WorkspaceId,
};
use crate::domain::messages::{Kind as MessageKind, State as MessageState};
use crate::domain::policy::delivery::{
    Category as EvidenceCategory, Confidence as EvidenceConfidence, EventKind as DeliveryEventKind,
    HoldReason, Outcome as AttemptOutcome, Phase as SubmissionPhase, RecipientRef,
    Source as EvidenceSource,
};
use crate::domain::receipts;
use crate::domain::scope::Scope;
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::{self, Json, Path, Query};
use crate::identity::authority::Principal;
use crate::pagination::{COUNT_CAP, Include, ListQuery, Order, Page, PageParams};
use crate::problem::{self, ApiResult, Code, FieldError, Problem};

/// Attempts and events a message shows.
const CHILDREN: i64 = 20;
/// The largest `variables` object, serialized.
const VARIABLES_MAX: usize = 64 * 1024;
/// The largest body template, in bytes.
pub(crate) const BODY_MAX: usize = 256 * 1024;

/// The routes of messages and delivery events.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_messages, create_message))
        .routes(routes!(retrieve_message))
        .routes(routes!(cancel_message))
        .routes(routes!(resolve_message))
        .routes(routes!(release_holds))
        .routes(routes!(list_events))
        .routes(routes!(retrieve_event))
}

impl From<accept::Error> for Problem {
    fn from(error: accept::Error) -> Self {
        match error {
            accept::Error::NotFound(what) => Problem::not_found(what),
            accept::Error::Invalid { pointer, detail } => {
                Problem::invalid_field(&pointer, "invalid", detail)
            }
            accept::Error::InvalidState(detail) => Problem::invalid_state(detail),
            accept::Error::Template(errors) => Problem::validation(
                errors
                    .into_iter()
                    .map(|error| FieldError {
                        pointer: error.part.pointer().to_owned(),
                        code: "template".to_owned(),
                        detail: error.detail,
                    })
                    .collect(),
            ),
            accept::Error::Suppressed { email, reason } => Problem::new(
                Code::Suppressed,
                format!("`{email}` is suppressed ({reason}); no message is sent to it."),
            ),
            accept::Error::NoTransactionalSender => Problem::unavailable(60),
            accept::Error::Db(error) => Problem::from(error),
        }
    }
}

// ───────────────────────────── objects ─────────────────────────────

/// An address and its display name.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Address {
    pub email: String,
    pub name: Option<String>,
}

/// What a message tracks, frozen when it was created.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Tracking {
    /// An open pixel is added to its HTML.
    pub opens: bool,
    /// Its links lead through click links.
    pub clicks: bool,
    /// The campaign's own tracking host, when it has one; the platform's otherwise.
    pub hostname: Option<String>,
}

/// One submission effort of a message.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct AttemptObject {
    pub id: Id<Attempt>,
    /// 1 for the message's first attempt.
    pub number: i32,
    pub connection_id: Id<Connection>,
    /// What the attempt came to; null while it runs. New values may be added.
    #[schema(value_type = Option<AttemptOutcome>)]
    pub outcome: Option<String>,
    /// The protocol step it ended in. New values may be added.
    #[schema(value_type = Option<SubmissionPhase>)]
    pub phase: Option<String>,
    pub smtp_code: Option<i16>,
    pub enhanced_status: Option<String>,
    /// Why it ended as it did; null while it runs. New values may be added.
    #[schema(value_type = Option<EvidenceCategory>)]
    pub category: Option<String>,
    /// The provider's words, bounded and redacted.
    pub diagnostic: Option<String>,
    /// The provider's id of the accepted message, when it returns one.
    pub provider_message_id: Option<String>,
    pub recipient_count: i32,
    pub claimed_at: Timestamp,
    /// When the submission itself started.
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
}

/// A message's latest attempts.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = MessageAttempts)]
pub struct Attempts {
    /// At most 20, newest first.
    #[schema(max_items = 20)]
    pub data: Vec<AttemptObject>,
    /// More attempts exist than `data` shows; read them through an export.
    pub has_more: bool,
}

/// One observation about a message's fate.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DeliveryEventObject {
    pub id: Id<DeliveryEvent>,
    /// The message, when the evidence could be matched to one.
    pub message_id: Option<Id<Message>>,
    pub thread_id: Option<Id<Thread>>,
    /// The attempt it comes from, when its source is an attempt.
    pub attempt_number: Option<i32>,
    /// What was observed. New values may be added.
    #[schema(value_type = DeliveryEventKind)]
    pub kind: String,
    /// Where the observation came from. New values may be added.
    #[schema(value_type = EvidenceSource)]
    pub source: String,
    /// How far the observation can be trusted. New values may be added.
    #[schema(value_type = EvidenceConfidence)]
    pub confidence: String,
    /// The recipient it concerns; null when the evidence names none.
    pub recipient: Option<String>,
    /// Why the recipient is known. New values may be added.
    #[schema(value_type = RecipientRef)]
    pub recipient_ref: String,
    /// The RFC 3464 action, for a delivery status notification.
    #[schema(value_type = Option<super::evidence::Action>)]
    pub action: Option<String>,
    /// The protocol step a submission's answer came from, for the `smtp` and `provider_api`
    /// sources.
    #[schema(value_type = Option<SubmissionPhase>)]
    pub phase: Option<String>,
    pub enhanced_status: Option<String>,
    /// Why it happened, in the closed vocabulary of evidence. New values may be added.
    #[schema(value_type = EvidenceCategory)]
    pub category: String,
    /// An exact provider-specific rejection code found in `diagnostic`, when any; it does not
    /// change the event's kind, recipient effect, or retry decision.
    pub provider_code: Option<String>,
    pub diagnostic: Option<String>,
    /// When the source observed it.
    pub observed_at: Timestamp,
    /// When Norbelys received it.
    pub received_at: Timestamp,
    /// When Norbelys recorded it.
    pub processed_at: Timestamp,
}

/// A message's first delivery events.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = MessageEvents)]
pub struct Events {
    /// At most 20, in the order they were observed.
    #[schema(max_items = 20)]
    pub data: Vec<DeliveryEventObject>,
    /// More events exist; `url` lists them all.
    pub has_more: bool,
    /// The delivery event list of this message.
    pub url: String,
}

/// A temporary pause of one recipient of a message.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct HoldObject {
    pub email: String,
    /// Why the recipient is held; `invalid_recipient` is a reported invalid address waiting for a
    /// person's review. New values may be added.
    #[schema(value_type = HoldReason)]
    pub reason: String,
    pub observed_at: Timestamp,
    /// When the hold is checked again.
    pub review_after: Timestamp,
    pub resolved_at: Option<Timestamp>,
    /// How the hold ended; null while it holds. New values may be added.
    #[schema(value_type = Option<HoldResolution>)]
    pub resolution: Option<String>,
}

/// How a hold ended (`recipient_holds.resolution`): a later success of the message lifted it,
/// its time ran out, the address was suppressed, or a person released it. The holds are written
/// by the delivery paths in SQL; this is their vocabulary as the API shows it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumIter, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(rename_all = "snake_case")]
pub enum HoldResolution {
    /// A later success of the message lifted it.
    Delivered,
    /// Its time ran out.
    Expired,
    /// The address was suppressed.
    Suppressed,
    /// A person released it (`messages.release_holds`).
    Manual,
}

/// A message as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct MessageObject {
    pub id: Id<Message>,
    /// What the message is. New values may be added.
    #[schema(value_type = MessageKind)]
    pub kind: String,
    /// Where the message is in its life: `queued`, `claimed` and `in_flight` are live, the others
    /// final. New values may be added.
    #[schema(value_type = MessageState)]
    pub state: String,
    /// Why it is in its state, when that needs words.
    pub status_detail: Option<String>,
    pub from: Address,
    pub reply_to: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    /// The subject as sent.
    pub subject: String,
    pub connection_id: Id<Connection>,
    pub sender_identity_id: Id<SenderIdentity>,
    pub person_id: Option<Id<Person>>,
    pub thread_id: Option<Id<Thread>>,
    pub campaign_id: Option<Id<Campaign>>,
    pub step_id: Option<Id<Step>>,
    pub variant_id: Option<Id<Variant>>,
    pub enrollment_id: Option<Id<Enrollment>>,
    /// For a campaign step message whose step asks for personalisation snippets that were not
    /// written: why; its template's defaults were used instead. `null` otherwise. New values may
    /// be added.
    #[schema(value_type = Option<crate::ai::snippets::Fallback>)]
    pub snippets_fallback: Option<String>,
    /// The `Message-ID` header.
    pub internet_message_id: String,
    /// The `Message-ID` it answers.
    pub in_reply_to: Option<String>,
    pub tracking: Tracking,
    /// When it is due.
    pub send_at: Timestamp,
    /// When it stops being useful, while it waits to be sent.
    pub expires_at: Option<Timestamp>,
    /// When a provider accepted it.
    pub sent_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub attempts: Attempts,
    /// Every attempt it had.
    pub attempts_count: i64,
    pub events: Events,
    /// One per held recipient: at most the 150 addresses of the envelope.
    #[schema(max_items = 150)]
    pub holds: Vec<HoldObject>,
}

// ───────────────────────────── reads ─────────────────────────────

/// The filters of the message list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MessageFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub campaign_id: Option<Id<Campaign>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub enrollment_id: Option<Id<Enrollment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub person_id: Option<Id<Person>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub connection_id: Option<Id<Connection>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub thread_id: Option<Id<Thread>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<MessageState>,
    /// Bounds on creation time, flattened into the list's filter keys.
    #[serde(flatten)]
    pub created: extract::CreatedRange,
}

/// A message row before its children are attached.
struct MessageRow {
    id: Id<Message>,
    kind: String,
    state: String,
    status_detail: Option<String>,
    from_email: String,
    from_name: Option<String>,
    reply_to: Option<String>,
    to_addresses: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    connection_id: Id<Connection>,
    sender_identity_id: Id<SenderIdentity>,
    person_id: Option<Id<Person>>,
    thread_id: Option<Id<Thread>>,
    campaign_id: Option<Id<Campaign>>,
    step_id: Option<Id<Step>>,
    variant_id: Option<Id<Variant>>,
    enrollment_id: Option<Id<Enrollment>>,
    snippets_fallback: Option<String>,
    internet_message_id: String,
    in_reply_to: Option<String>,
    tracking: Value,
    send_at: Timestamp,
    expires_at: Option<Timestamp>,
    sent_at: Option<Timestamp>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

/// Reads one message of `workspace` with its children: what every message operation answers.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Message>,
) -> Result<Option<MessageObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        MessageRow,
        r#"SELECT m.id AS "id: Id<Message>", m.kind, m.state, m.status_detail, m.from_email, m.from_name, m.reply_to,
                  m.to_addresses, m.cc, m.bcc, m.subject, m.connection_id AS "connection_id: Id<Connection>",
                  m.sender_identity_id AS "sender_identity_id: Id<SenderIdentity>", m.person_id AS "person_id: Id<Person>",
                  m.thread_id AS "thread_id: Id<Thread>", m.campaign_id AS "campaign_id: Id<Campaign>",
                  m.step_id AS "step_id: Id<Step>", m.variant_id AS "variant_id: Id<Variant>",
                  m.enrollment_id AS "enrollment_id: Id<Enrollment>", m.snippets_fallback, m.internet_message_id, m.in_reply_to, m.tracking,
                  m.send_at AS "send_at: Timestamp", q.expires_at AS "expires_at?: Timestamp",
                  m.sent_at AS "sent_at: Timestamp", m.created_at AS "created_at: Timestamp",
                  m.updated_at AS "updated_at: Timestamp"
             FROM messages m LEFT JOIN delivery_queue q ON q.workspace_id = m.workspace_id AND q.message_id = m.id
            WHERE m.workspace_id = $1 AND m.id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(with_children(tx, workspace, row.into_iter().collect())
        .await?
        .pop())
}

/// One page of `workspace`'s messages matching `filters`, in id order after `cursor`, with their
/// children. Fetches `limit` rows; the caller asks for one more than it shows.
async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &MessageFilters,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<MessageObject>, sqlx::Error> {
    let f = filters;
    let state = f.state.map(MessageState::as_str);
    // The two index walks use the same filters and cursor bindings.
    macro_rules! select {
        ($sql:literal) => {
            sqlx::query_as!(
                MessageRow,
                $sql,
                workspace.uuid(),
                cursor,
                f.campaign_id.map(|id| id.uuid()),
                f.enrollment_id.map(|id| id.uuid()),
                f.person_id.map(|id| id.uuid()),
                f.connection_id.map(|id| id.uuid()),
                f.thread_id.map(|id| id.uuid()),
                state,
                f.created.gte as _,
                f.created.gt as _,
                f.created.lte as _,
                f.created.lt as _,
                limit,
            )
            .fetch_all(&mut **tx)
            .await?
        };
    }
    // Two statements, one per direction, so each walks the primary key in its own order.
    let rows = if ascending {
        select!(
            r#"SELECT m.id AS "id: Id<Message>", m.kind, m.state, m.status_detail, m.from_email, m.from_name, m.reply_to,
                      m.to_addresses, m.cc, m.bcc, m.subject, m.connection_id AS "connection_id: Id<Connection>",
                      m.sender_identity_id AS "sender_identity_id: Id<SenderIdentity>", m.person_id AS "person_id: Id<Person>",
                      m.thread_id AS "thread_id: Id<Thread>", m.campaign_id AS "campaign_id: Id<Campaign>",
                      m.step_id AS "step_id: Id<Step>", m.variant_id AS "variant_id: Id<Variant>",
                      m.enrollment_id AS "enrollment_id: Id<Enrollment>", m.snippets_fallback, m.internet_message_id, m.in_reply_to, m.tracking,
                      m.send_at AS "send_at: Timestamp", q.expires_at AS "expires_at?: Timestamp",
                      m.sent_at AS "sent_at: Timestamp", m.created_at AS "created_at: Timestamp",
                      m.updated_at AS "updated_at: Timestamp"
                 FROM messages m LEFT JOIN delivery_queue q ON q.workspace_id = m.workspace_id AND q.message_id = m.id
                WHERE m.workspace_id = $1 AND ($2::uuid IS NULL OR m.id > $2)
                  AND ($3::uuid IS NULL OR m.campaign_id = $3) AND ($4::uuid IS NULL OR m.enrollment_id = $4)
                  AND ($5::uuid IS NULL OR m.person_id = $5) AND ($6::uuid IS NULL OR m.connection_id = $6)
                  AND ($7::uuid IS NULL OR m.thread_id = $7) AND ($8::text IS NULL OR m.state = $8)
                  AND ($9::timestamptz IS NULL OR m.created_at >= $9) AND ($10::timestamptz IS NULL OR m.created_at > $10)
                  AND ($11::timestamptz IS NULL OR m.created_at <= $11) AND ($12::timestamptz IS NULL OR m.created_at < $12)
                ORDER BY m.id LIMIT $13"#
        )
    } else {
        select!(
            r#"SELECT m.id AS "id: Id<Message>", m.kind, m.state, m.status_detail, m.from_email, m.from_name, m.reply_to,
                      m.to_addresses, m.cc, m.bcc, m.subject, m.connection_id AS "connection_id: Id<Connection>",
                      m.sender_identity_id AS "sender_identity_id: Id<SenderIdentity>", m.person_id AS "person_id: Id<Person>",
                      m.thread_id AS "thread_id: Id<Thread>", m.campaign_id AS "campaign_id: Id<Campaign>",
                      m.step_id AS "step_id: Id<Step>", m.variant_id AS "variant_id: Id<Variant>",
                      m.enrollment_id AS "enrollment_id: Id<Enrollment>", m.snippets_fallback, m.internet_message_id, m.in_reply_to, m.tracking,
                      m.send_at AS "send_at: Timestamp", q.expires_at AS "expires_at?: Timestamp",
                      m.sent_at AS "sent_at: Timestamp", m.created_at AS "created_at: Timestamp",
                      m.updated_at AS "updated_at: Timestamp"
                 FROM messages m LEFT JOIN delivery_queue q ON q.workspace_id = m.workspace_id AND q.message_id = m.id
                WHERE m.workspace_id = $1 AND ($2::uuid IS NULL OR m.id < $2)
                  AND ($3::uuid IS NULL OR m.campaign_id = $3) AND ($4::uuid IS NULL OR m.enrollment_id = $4)
                  AND ($5::uuid IS NULL OR m.person_id = $5) AND ($6::uuid IS NULL OR m.connection_id = $6)
                  AND ($7::uuid IS NULL OR m.thread_id = $7) AND ($8::text IS NULL OR m.state = $8)
                  AND ($9::timestamptz IS NULL OR m.created_at >= $9) AND ($10::timestamptz IS NULL OR m.created_at > $10)
                  AND ($11::timestamptz IS NULL OR m.created_at <= $11) AND ($12::timestamptz IS NULL OR m.created_at < $12)
                ORDER BY m.id DESC LIMIT $13"#
        )
    };
    with_children(tx, workspace, rows).await
}

/// Counts `workspace`'s messages matching `filters`, up to `cap + 1`.
async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &MessageFilters,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    let f = filters;
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM messages m
                WHERE m.workspace_id = $1
                  AND ($2::uuid IS NULL OR m.campaign_id = $2) AND ($3::uuid IS NULL OR m.enrollment_id = $3)
                  AND ($4::uuid IS NULL OR m.person_id = $4) AND ($5::uuid IS NULL OR m.connection_id = $5)
                  AND ($6::uuid IS NULL OR m.thread_id = $6) AND ($7::text IS NULL OR m.state = $7)
                  AND ($8::timestamptz IS NULL OR m.created_at >= $8) AND ($9::timestamptz IS NULL OR m.created_at > $9)
                  AND ($10::timestamptz IS NULL OR m.created_at <= $10) AND ($11::timestamptz IS NULL OR m.created_at < $11)
                LIMIT $12) counted"#,
        workspace.uuid(),
        f.campaign_id.map(|id| id.uuid()),
        f.enrollment_id.map(|id| id.uuid()),
        f.person_id.map(|id| id.uuid()),
        f.connection_id.map(|id| id.uuid()),
        f.thread_id.map(|id| id.uuid()),
        f.state.map(MessageState::as_str),
        f.created.gte as _,
        f.created.gt as _,
        f.created.lte as _,
        f.created.lt as _,
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// An attempt row with its message and its place among the message's attempts.
struct AttemptRow {
    message_id: Uuid,
    rank: i64,
    total: i64,
    attempt: AttemptObject,
}

/// Attaches the children of `rows`: one query per child for the whole page.
async fn with_children(
    tx: &mut Tx,
    workspace: WorkspaceId,
    rows: Vec<MessageRow>,
) -> Result<Vec<MessageObject>, sqlx::Error> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id.uuid()).collect();
    let mut attempts: HashMap<Uuid, (Vec<AttemptObject>, i64)> = HashMap::new();
    for row in sqlx::query!(
        r#"SELECT message_id AS "message_id!", place AS "place!", total AS "total!", id AS "id!: Id<Attempt>",
                  attempt_number AS "attempt_number!", connection_id AS "connection_id!: Id<Connection>",
                  outcome AS "outcome?", phase AS "phase?", smtp_code AS "smtp_code?", enhanced_status AS "enhanced_status?",
                  category AS "category?", diagnostic AS "diagnostic?", provider_message_id AS "provider_message_id?",
                  recipient_count AS "recipient_count!", claimed_at AS "claimed_at!: Timestamp",
                  smtp_started_at AS "smtp_started_at?: Timestamp", finished_at AS "finished_at?: Timestamp"
             FROM (SELECT a.*, row_number() OVER (PARTITION BY a.message_id ORDER BY a.attempt_number DESC) AS place,
                          count(*) OVER (PARTITION BY a.message_id) AS total
                     FROM attempts a WHERE a.workspace_id = $1 AND a.message_id = ANY ($2)) ranked
            WHERE place <= $3
            ORDER BY message_id, place"#,
        workspace.uuid(),
        &ids,
        CHILDREN + 1,
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| AttemptRow {
        message_id: row.message_id,
        rank: row.place,
        total: row.total,
        attempt: AttemptObject {
            id: row.id,
            number: row.attempt_number,
            connection_id: row.connection_id,
            outcome: row.outcome,
            phase: row.phase,
            smtp_code: row.smtp_code,
            enhanced_status: row.enhanced_status,
            category: row.category,
            diagnostic: row.diagnostic,
            provider_message_id: row.provider_message_id,
            recipient_count: row.recipient_count,
            claimed_at: row.claimed_at,
            started_at: row.smtp_started_at,
            finished_at: row.finished_at,
        },
    }) {
        let entry = attempts.entry(row.message_id).or_default();
        entry.1 = row.total;
        if row.rank <= CHILDREN {
            entry.0.push(row.attempt);
        }
    }
    let mut events: HashMap<Uuid, Vec<DeliveryEventObject>> = HashMap::new();
    for row in sqlx::query_as!(
        EventRecord,
        r#"SELECT id AS "id!: Id<DeliveryEvent>", message_id AS "message_id?: Id<Message>",
                  thread_id AS "thread_id?: Id<Thread>", attempt_number AS "attempt_number?", kind AS "kind!",
                  source AS "source!", confidence AS "confidence!", recipient_email AS "recipient_email?",
                  recipient_ref AS "recipient_ref!", action AS "action?", phase AS "phase?",
                  enhanced_status AS "enhanced_status?", category AS "category!", diagnostic AS "diagnostic?",
                  observed_at AS "observed_at!: Timestamp",
                  coalesce(uuid_extract_timestamp(receipt_id), created_at) AS "received_at!: Timestamp",
                  created_at AS "created_at!: Timestamp"
             FROM (SELECT e.*, row_number() OVER (PARTITION BY e.message_id ORDER BY e.observed_at, e.id) AS place
                     FROM delivery_events e WHERE e.workspace_id = $1 AND e.message_id = ANY ($2)) ranked
            WHERE place <= $3
            ORDER BY message_id, place"#,
        workspace.uuid(),
        &ids,
        CHILDREN + 1,
    )
    .fetch_all(&mut **tx)
    .await?
    {
        let Some(message) = row.message_id else {
            continue;
        };
        events.entry(message.uuid()).or_default().push(row.into());
    }
    let mut holds: HashMap<Uuid, Vec<HoldObject>> = HashMap::new();
    for row in sqlx::query!(
        r#"SELECT message_id, email, reason, observed_at AS "observed_at: Timestamp",
                  review_after AS "review_after: Timestamp", resolved_at AS "resolved_at: Timestamp", resolution
             FROM recipient_holds WHERE workspace_id = $1 AND message_id = ANY ($2)
            ORDER BY message_id, email_key"#,
        workspace.uuid(),
        &ids,
    )
    .fetch_all(&mut **tx)
    .await?
    {
        holds.entry(row.message_id).or_default().push(HoldObject {
            email: row.email,
            reason: row.reason,
            observed_at: row.observed_at,
            review_after: row.review_after,
            resolved_at: row.resolved_at,
            resolution: row.resolution,
        });
    }
    Ok(rows
        .into_iter()
        .map(|row| {
            let id = row.id.uuid();
            let (attempts, attempts_count) = attempts.remove(&id).unwrap_or_default();
            let mut events = events.remove(&id).unwrap_or_default();
            let events_count = i64::try_from(events.len()).unwrap_or(i64::MAX);
            events.truncate(usize::try_from(CHILDREN).unwrap_or(usize::MAX));
            let tracking = Tracking {
                opens: row
                    .tracking
                    .get("opens")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                clicks: row
                    .tracking
                    .get("clicks")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                hostname: row
                    .tracking
                    .get("hostname")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            };
            MessageObject {
                attempts: Attempts {
                    has_more: attempts_count > CHILDREN,
                    data: attempts,
                },
                attempts_count,
                events: Events {
                    has_more: events_count > CHILDREN,
                    data: events,
                    url: format!("/v1/delivery_events?message_id={}", row.id),
                },
                holds: holds.remove(&id).unwrap_or_default(),
                id: row.id,
                kind: row.kind,
                state: row.state,
                status_detail: row.status_detail,
                from: Address {
                    email: row.from_email,
                    name: row.from_name,
                },
                reply_to: row.reply_to,
                to: row.to_addresses,
                cc: row.cc,
                bcc: row.bcc,
                subject: row.subject,
                connection_id: row.connection_id,
                sender_identity_id: row.sender_identity_id,
                person_id: row.person_id,
                thread_id: row.thread_id,
                campaign_id: row.campaign_id,
                step_id: row.step_id,
                variant_id: row.variant_id,
                enrollment_id: row.enrollment_id,
                snippets_fallback: row.snippets_fallback,
                internet_message_id: row.internet_message_id,
                in_reply_to: row.in_reply_to,
                tracking,
                send_at: row.send_at,
                expires_at: row.expires_at,
                sent_at: row.sent_at,
                created_at: row.created_at,
                updated_at: row.updated_at,
            }
        })
        .collect())
}

// ───────────────────────────── messages ─────────────────────────────

/// List the workspace's messages, newest first by default.
#[utoipa::path(
    get,
    path = "/messages",
    tag = "Messages",
    operation_id = "messages.list",
    params(
        ("limit" = Option<i64>, Query, description = "Messages per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("campaign_id" = Option<Id<Campaign>>, Query, description = "Messages of this campaign."),
        ("enrollment_id" = Option<Id<Enrollment>>, Query, description = "Messages of this enrollment."),
        ("person_id" = Option<Id<Person>>, Query, description = "Messages to this person."),
        ("connection_id" = Option<Id<Connection>>, Query, description = "Messages sent through this connection."),
        ("thread_id" = Option<Id<Thread>>, Query, description = "Messages of this thread."),
        ("state" = Option<MessageState>, Query, description = "Messages in this state."),
        ("created_at[gte]" = Option<String>, Query, description = "Created at or after this instant (RFC 3339)."),
        ("created_at[gt]" = Option<String>, Query, description = "Created after this instant."),
        ("created_at[lte]" = Option<String>, Query, description = "Created at or before this instant."),
        ("created_at[lt]" = Option<String>, Query, description = "Created before this instant."),
    ),
    responses(
        (status = 200, description = "A page of messages.", body = Page<MessageObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `messages:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_messages(
    principal: Principal,
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
    Query(filters): Query<MessageFilters>,
) -> ApiResult<Json<Page<MessageObject>>> {
    principal.require(Scope::MessagesRead)?;
    let ws = principal.workspace;
    let params = PageParams::from_query(&state.keys, ws, "messages", "id", &filters, &query)?;
    let mut tx = state.db.begin_in(ws).await?;
    let rows = list(
        &mut tx,
        ws,
        &filters,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(count(&mut tx, ws, &filters, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    Ok(Json(Page::from_ids(
        &state.keys,
        &params,
        rows,
        total,
        |message| message.id.uuid(),
    )))
}

/// The direct form of `POST /messages`: a message with its own content from one
/// of the workspace's sender identities.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateMessage {
    /// Uploaded attachments from this workspace, at most ten.
    #[garde(length(max = 10))]
    #[serde(default)]
    attachments: Vec<Id<crate::domain::ids::Attachment>>,
    /// The sender: a sender identity's id (`sid_…`) or its address. It must be live and enabled.
    #[garde(length(min = 1, max = 254))]
    from: String,
    /// 1 to 50 addresses.
    #[garde(length(min = 1, max = 50))]
    #[schema(value_type = Vec<String>)]
    to: Vec<EmailAddress>,
    /// At most 150 recipients in all, `to`, `cc` and `bcc` together, none twice.
    #[garde(length(max = 149))]
    #[schema(value_type = Option<Vec<String>>)]
    cc: Option<Vec<EmailAddress>>,
    #[garde(length(max = 149))]
    #[schema(value_type = Option<Vec<String>>)]
    bcc: Option<Vec<EmailAddress>>,
    /// A template: `{{ variables.name }}`, `{{ sender.name }}`, `{{ person.given_name }}` (when a
    /// `to` address is a person of the workspace). Printing a value that does not exist is an
    /// error; `| default("…")` or `{% if … %}` handle one that may be missing.
    #[garde(length(chars, min = 1, max = 1_000))]
    subject: String,
    /// The one authored body, an HTML template; a plain-text alternative is derived from it.
    /// Printed values are HTML-escaped. At most 256 KiB.
    #[garde(length(min = 1, max = BODY_MAX))]
    html: String,
    /// Values the templates read as `variables`; at most 64 KiB.
    #[garde(skip)]
    #[schema(value_type = Option<Object>)]
    variables: Option<Map<String, Value>>,
    /// When to send it, at most 7 days ahead; now when absent or past.
    #[garde(skip)]
    send_at: Option<Timestamp>,
    /// When it stops being useful: not sent after this instant.
    #[garde(skip)]
    expires_at: Option<Timestamp>,
}

/// The step-content form of `POST /messages` for one person who is not enrolled: the campaign
/// step's content in that person's version, or a preview of it at another address. Answers the
/// message.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateStepMessage {
    /// The step (`stp_…`): its winner's content, else its first variant's.
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    step_id: Option<Id<Step>>,
    /// The variant (`var_…`), at its latest version; with `step_id`, one of the step's.
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    variant_id: Option<Id<Variant>>,
    /// The person whose version is sent (`per_…`).
    #[garde(skip)]
    #[schema(value_type = String)]
    person_id: Id<Person>,
    /// That person's version goes to this address instead, which is how a step is previewed;
    /// the variant's `cc` and `bcc` are left out.
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    to: Option<EmailAddress>,
    /// The sender: a sender identity's id (`sid_…`) or its address; the campaign pool's next
    /// usable sender when absent.
    #[garde(length(min = 1, max = 254))]
    from: Option<String>,
    /// Values the templates read as `variables`; at most 64 KiB.
    #[garde(skip)]
    #[schema(value_type = Option<Object>)]
    variables: Option<Map<String, Value>>,
    /// When to send it, at most 7 days ahead; now when absent or past.
    #[garde(skip)]
    send_at: Option<Timestamp>,
}

/// The step-content form of `POST /messages` for several people who are not enrolled: each is
/// sent their version of the campaign step. Answers a result per person, the message or the
/// problem a single request would have answered.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateStepMessages {
    /// The step (`stp_…`): its winner's content, else its first variant's.
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    step_id: Option<Id<Step>>,
    /// The variant (`var_…`), at its latest version; with `step_id`, one of the step's.
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    variant_id: Option<Id<Variant>>,
    /// 1 to 100 people (`per_…`), each sent their version.
    #[garde(length(min = 1, max = 100))]
    #[schema(value_type = Vec<String>)]
    person_ids: Vec<Id<Person>>,
    /// The sender: a sender identity's id (`sid_…`) or its address; the campaign pool's next
    /// usable sender when absent.
    #[garde(length(min = 1, max = 254))]
    from: Option<String>,
    /// Values the templates read as `variables`; at most 64 KiB.
    #[garde(skip)]
    #[schema(value_type = Option<Object>)]
    variables: Option<Map<String, Value>>,
    /// When to send them, at most 7 days ahead; now when absent or past.
    #[garde(skip)]
    send_at: Option<Timestamp>,
}

/// The step-content forms, as [`create_step_message`] takes them.
enum StepForm {
    One(CreateStepMessage),
    Many(CreateStepMessages),
}

/// The forms of `POST /messages`: a direct message, a reply in a thread, or a campaign step's
/// content for one person or for several. Each form refuses the members it does not name, and any
/// two forms differ in a member one requires and the other refuses, so a body is one form only.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
#[allow(
    dead_code,
    reason = "the request's schema: the handler reads each form by itself"
)]
enum MessageForm {
    Direct(CreateMessage),
    Step(CreateStepMessage),
    Steps(CreateStepMessages),
    Reply(crate::inbox::http::CreateReply),
}

/// What `POST /messages` answers: the message, or, for `person_ids`, a result per person. The
/// operation's `x-norbelys-overloads` says which form receives which.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
enum MessagesCreated {
    One(Box<MessageObject>),
    Many(StepResults),
}

/// The results of the step-content form for several people, in the order they were given.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct StepResults {
    /// One per person given, at most 100.
    #[schema(max_items = 100)]
    data: Vec<StepResult>,
}

/// One person's result: the message queued, or why there is none.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct StepResult {
    #[schema(value_type = String)]
    person_id: Id<Person>,
    message: Option<MessageObject>,
    error: Option<StepFailure>,
}

/// Why one person's message was refused, as the problem a single request would have answered.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct StepFailure {
    /// The problem's code: `not_found`, `suppressed`, `validation_failed` or `invalid_state`.
    #[schema(value_type = Code)]
    code: String,
    detail: String,
    errors: Vec<FieldError>,
}

/// A sender as a request names it.
enum Origin {
    Identity(Id<SenderIdentity>),
    Address(EmailAddress),
}

impl Origin {
    fn parse(from: &str) -> Result<Self, Problem> {
        if from.starts_with("sid_") {
            return from.parse().map(Self::Identity).map_err(|_| {
                Problem::invalid_field("/from", "format", "Not a sender identity id (`sid_…`).")
            });
        }
        EmailAddress::parse(from)
            .map(Self::Address)
            .map_err(|error| Problem::invalid_field("/from", "format", error.to_string()))
    }

    fn sender(&self) -> Sender<'_> {
        match self {
            Self::Identity(id) => Sender::Identity(*id),
            Self::Address(address) => Sender::Address(address),
        }
    }
}

/// Send a message: it is queued now and sent when due, outside any campaign's cadence.
///
/// Four forms: a direct message with its own content; a reply in a thread (`thread_id` and a
/// body), sent from the thread's identity to the sender of its latest inbound message unless `to`
/// says otherwise; or a campaign step's content (`step_id` or `variant_id`) for people who are not
/// enrolled, with `person_id` (and `to` to preview that person's version at another address),
/// answered with the message, or with up to 100 `person_ids`, answered per person.
#[utoipa::path(
    post,
    path = "/messages",
    tag = "Messages",
    operation_id = "messages.create",
    // Which answer each form receives, for generated clients: one message, except for
    // `person_ids`. The invariants test holds every form and answer named here to the
    // operation's body and responses.
    extensions(("x-norbelys-overloads" = json!([
        { "request": { "$ref": "#/components/schemas/CreateMessage" }, "response": { "$ref": "#/components/schemas/MessageObject" } },
        { "request": { "$ref": "#/components/schemas/CreateReply" }, "response": { "$ref": "#/components/schemas/MessageObject" } },
        { "request": { "$ref": "#/components/schemas/CreateStepMessage" }, "response": { "$ref": "#/components/schemas/MessageObject" } },
        { "request": { "$ref": "#/components/schemas/CreateStepMessages" }, "response": { "$ref": "#/components/schemas/StepResults" } }
    ]))),
    request_body(content = MessageForm, examples(
        ("Direct" = (summary = "A message with its own content", value = json!({
            "from": "max@acme.example",
            "to": ["ada@example.com"],
            "subject": "Quick question, {{ variables.first_name }}",
            "html": "<p>Hi {{ variables.first_name }},</p><p>Do you have ten minutes this week?</p>",
            "variables": {"first_name": "Ada"},
            "send_at": "2026-10-02T09:00:00Z"
        }))),
        ("Reply" = (summary = "An answer in a thread, from the thread's identity", value = json!({
            "thread_id": "thr_0190f8a2b4c87a10b6d2e4f6a8c0e2f4",
            "html": "<p>Thanks Ada, Tuesday at 10 works for me.</p>"
        }))),
        ("Step preview" = (summary = "One person's version of a step, sent to another address", value = json!({
            "step_id": "stp_0190f8a2b4c87a10b6d2e4f6a8c0e2f4",
            "person_id": "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4",
            "to": "max@acme.example"
        }))),
        ("Step for several people" = (summary = "A step's content for people who are not enrolled, a result each", value = json!({
            "step_id": "stp_0190f8a2b4c87a10b6d2e4f6a8c0e2f4",
            "person_ids": ["per_0190f8a2b4c87a10b6d2e4f6a8c0e2f4", "per_0190f8a2b4c87a10b6d2e4f6a8c0e2f5"]
        })))
    )),
    responses(
        (status = 202, description = "The message, queued; `Location` names it. For `person_ids`, a result per person.", body = MessagesCreated,
         headers(("Location" = String, description = "The message's path (one message)."))),
        (status = 400, description = "No `Idempotency-Key`, or malformed JSON."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `messages:send`."),
        (status = 404, description = "No such sender identity, step, variant or person in this workspace."),
        (status = 409, description = "The sender identity is disabled, or the campaign's pool has no usable sender (`invalid_state`)."),
        (status = 422, description = "The body is invalid (`validation_failed`, with a pointer per field and per template), or a recipient is suppressed (`suppressed`)."),
    ),
    security(("bearer" = []))
)]
async fn create_message(
    principal: Principal,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(extract::Object(form)): Json<extract::Object>,
) -> ApiResult<Response> {
    principal.require(Scope::MessagesSend)?;
    // The body is read whole once to tell its form: one naming `thread_id` is a reply, one
    // naming `step_id` or `variant_id` the step-content form, any other the direct form.
    // Each message the request asks for spends a unit of the workspace's `send` budget before
    // any work: one for a direct message or a reply, one per person of a step's `person_ids`.
    // The answer shows that budget, before the request's own (`http::ratelimit`).
    let count = form
        .get("person_ids")
        .and_then(Value::as_array)
        .map_or(1, Vec::len);
    let allowance = match state.limits.spend_messages(principal.workspace, count) {
        Ok(allowance) => allowance,
        Err(refusal) => return Ok(refusal.into_response()),
    };
    let mut response = create_form(principal, state, headers, form).await?;
    allowance.write(response.headers_mut());
    Ok(response)
}

/// The forms of [`create_message`], once the workspace's `send` budget admitted them.
async fn create_form(
    principal: Principal,
    state: AppState,
    headers: HeaderMap,
    form: Map<String, Value>,
) -> ApiResult<Response> {
    let bytes = serde_json::to_vec(&form).map_err(|error| Problem::internal(&error))?;
    let step_form = form.contains_key("step_id") || form.contains_key("variant_id");
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok());
    if form.contains_key("thread_id") {
        let body = extract::parse(&bytes)?;
        return crate::inbox::http::create_reply(&principal, &state, body, idempotency_key)
            .await
            .map(one);
    }
    if step_form {
        // The two step forms differ by `person_id` and `person_ids`. Telling them apart first
        // names each mistake as such, rather than as a member the other form does not know.
        let form = match (
            form.contains_key("person_id"),
            form.contains_key("person_ids"),
        ) {
            (true, false) => StepForm::One(extract::parse(&bytes)?),
            (false, true) if form.contains_key("to") => {
                return Err(Problem::invalid_field(
                    "/to",
                    "invalid",
                    "`to` previews one person's version: give it with `person_id`.",
                ));
            }
            (false, true) => StepForm::Many(extract::parse(&bytes)?),
            _ => {
                return Err(Problem::invalid_field(
                    "/person_id",
                    "required",
                    "Give exactly one of `person_id` or `person_ids`.",
                ));
            }
        };
        return create_step_message(&principal, &state, form, idempotency_key).await;
    }
    let body: CreateMessage = extract::parse(&bytes)?;
    let ws = principal.workspace;
    let from = Origin::parse(&body.from)?;
    check_variables(body.variables.as_ref())?;
    let mut tx = state.db.begin_in(ws).await?;
    let accepted = accept::create(
        &mut tx,
        &state.keys,
        ws,
        &NewMessage {
            from: from.sender(),
            to: &body.to,
            cc: body.cc.as_deref().unwrap_or_default(),
            bcc: body.bcc.as_deref().unwrap_or_default(),
            subject: &body.subject,
            html: Some(&body.html),
            text: None,
            variables: body.variables,
            send_at: body.send_at,
            expires_at: body.expires_at,
            reply: None,
            idempotency_key,
        },
    )
    .await?;
    crate::delivery::attachments::attach(&mut tx, ws, accepted.message, &body.attachments).await?;
    let message = read(&mut tx, ws, accepted.message)
        .await?
        .ok_or_else(|| Problem::internal(&"an accepted message is not readable"))?;
    tx.commit().await?;
    accept::wake(&state.db).await;
    Ok(one(message))
}

/// The `202` of one message, with its `Location`.
fn one(message: MessageObject) -> Response {
    (
        StatusCode::ACCEPTED,
        [(header::LOCATION, format!("/v1/messages/{}", message.id))],
        Json(MessagesCreated::One(Box::new(message))),
    )
        .into_response()
}

/// Refuses a `variables` object above 64 KiB.
pub(crate) fn check_variables(variables: Option<&Map<String, Value>>) -> Result<(), Problem> {
    if let Some(variables) = variables
        && serde_json::to_vec(variables).map_or(0, |bytes| bytes.len()) > VARIABLES_MAX
    {
        return Err(Problem::invalid_field(
            "/variables",
            "length",
            "`variables` is at most 64 KiB.",
        ));
    }
    Ok(())
}

/// The step-content form (see [`create_message`]): each person's version of the step, created
/// through `accept::step_content` in one transaction.
async fn create_step_message(
    principal: &Principal,
    state: &AppState,
    form: StepForm,
    idempotency_key: Option<&str>,
) -> ApiResult<Response> {
    let ws = principal.workspace;
    // What the forms share, then whom: one person (perhaps previewed at `to`), or several.
    let (step_id, variant_id, from, variables, send_at, people, to, single) = match form {
        StepForm::One(body) => (
            body.step_id,
            body.variant_id,
            body.from,
            body.variables,
            body.send_at,
            vec![body.person_id],
            body.to,
            true,
        ),
        StepForm::Many(body) => (
            body.step_id,
            body.variant_id,
            body.from,
            body.variables,
            body.send_at,
            body.person_ids,
            None,
            false,
        ),
    };
    check_variables(variables.as_ref())?;
    let from = from.as_deref().map(Origin::parse).transpose()?;
    let mut tx = state.db.begin_in(ws).await?;
    let (campaign, step, variant, version) =
        crate::campaigns::steps::content_of(&mut tx, ws, step_id, variant_id)
            .await?
            .ok_or_else(|| {
                Problem::not_found(if variant_id.is_some() {
                    "variant"
                } else {
                    "step"
                })
            })?;
    let from = match from {
        Some(from) => from,
        None => Origin::Identity(
            crate::campaigns::creator::pool_sender(&mut tx, ws, campaign)
                .await?
                .ok_or_else(|| {
                    Problem::invalid_state(
                        "No sender of the campaign's pool can send now; name one in `from`.",
                    )
                })?,
        ),
    };
    let mut results = Vec::with_capacity(people.len());
    for person in &people {
        let accepted = accept::step_content(
            &mut tx,
            &state.keys,
            ws,
            &accept::StepContent {
                from: from.sender(),
                campaign,
                step,
                variant,
                variant_version: version,
                person: *person,
                to: to.as_ref(),
                variables: variables.clone(),
                send_at,
                idempotency_key,
            },
        )
        .await;
        let result = match accepted {
            Ok(accepted) => Ok(read(&mut tx, ws, accepted.message)
                .await?
                .ok_or_else(|| Problem::internal(&"an accepted message is not readable"))?),
            Err(accept::Error::Db(error)) => return Err(error.into()),
            Err(error) => Err(Problem::from(error)),
        };
        results.push((*person, result));
    }
    tx.commit().await?;
    if results.iter().any(|(_, result)| result.is_ok()) {
        accept::wake(&state.db).await;
    }
    if single {
        return match results.pop() {
            Some((_, Ok(message))) => Ok(one(message)),
            Some((_, Err(problem))) => Err(problem),
            None => Err(Problem::internal(&"no result for the person")),
        };
    }
    let data = results
        .into_iter()
        .map(|(person_id, result)| match result {
            Ok(message) => StepResult {
                person_id,
                message: Some(message),
                error: None,
            },
            Err(problem) => StepResult {
                person_id,
                message: None,
                error: Some(StepFailure {
                    code: problem.code.as_str().to_owned(),
                    detail: problem.detail,
                    errors: problem.errors,
                }),
            },
        })
        .collect();
    Ok((
        StatusCode::ACCEPTED,
        Json(MessagesCreated::Many(StepResults { data })),
    )
        .into_response())
}

/// Retrieve a message, with its latest attempts, its first delivery events and its holds.
#[utoipa::path(
    get,
    path = "/messages/{id}",
    tag = "Messages",
    operation_id = "messages.retrieve",
    params(("id" = Id<Message>, Path, description = "The message id (`msg_…`).")),
    responses(
        (status = 200, description = "The message.", body = MessageObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `messages:read`."),
        (status = 404, description = "No such message in this workspace (`not_found`), or its period was archived (`archived`): `POST /v1/exports` reads it."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_message(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Message>>,
) -> ApiResult<Json<MessageObject>> {
    principal.require(Scope::MessagesRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let Some(message) = read(&mut tx, principal.workspace, id).await? else {
        return Err(problem::missing(&mut tx, "messages", id.uuid(), "message").await);
    };
    tx.commit().await?;
    Ok(Json(message))
}

// ───────────────────────────── delivery events ─────────────────────────────

/// The filters of the delivery event list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct EventFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_id: Option<Id<Message>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    campaign_id: Option<Id<Campaign>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    person_id: Option<Id<Person>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<DeliveryEventKind>,
    /// The events about this recipient (ignoring ASCII case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recipient: Option<String>,
}

/// A delivery event as read.
struct EventRecord {
    id: Id<DeliveryEvent>,
    message_id: Option<Id<Message>>,
    thread_id: Option<Id<Thread>>,
    attempt_number: Option<i32>,
    kind: String,
    source: String,
    confidence: String,
    recipient_email: Option<String>,
    recipient_ref: String,
    action: Option<String>,
    phase: Option<String>,
    enhanced_status: Option<String>,
    category: String,
    diagnostic: Option<String>,
    observed_at: Timestamp,
    received_at: Timestamp,
    created_at: Timestamp,
}

impl From<EventRecord> for DeliveryEventObject {
    fn from(row: EventRecord) -> Self {
        Self {
            id: row.id,
            message_id: row.message_id,
            thread_id: row.thread_id,
            attempt_number: row.attempt_number,
            kind: row.kind,
            source: row.source,
            confidence: row.confidence,
            recipient: row.recipient_email,
            recipient_ref: row.recipient_ref,
            action: row.action,
            phase: row.phase,
            enhanced_status: row.enhanced_status,
            category: row.category,
            provider_code: row.diagnostic.as_deref().and_then(receipts::provider_code),
            diagnostic: row.diagnostic,
            observed_at: row.observed_at,
            received_at: row.received_at,
            processed_at: row.created_at,
        }
    }
}

/// One page of `workspace`'s delivery events matching `filters`, in id order after `cursor`.
/// Campaign and person are read from each event's message. Fetches `limit` rows.
async fn events(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &EventFilters,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<DeliveryEventObject>, sqlx::Error> {
    let f = filters;
    let kind = f.kind.map(<&'static str>::from);
    // Two statements, one per direction, so each walks the primary key in its own order.
    let rows = if ascending {
        sqlx::query_as!(
            EventRecord,
            r#"SELECT e.id AS "id: Id<DeliveryEvent>", e.message_id AS "message_id: Id<Message>",
                      e.thread_id AS "thread_id: Id<Thread>", e.attempt_number, e.kind, e.source, e.confidence,
                      e.recipient_email, e.recipient_ref, e.action, e.phase, e.enhanced_status, e.category, e.diagnostic,
                      e.observed_at AS "observed_at: Timestamp",
                      coalesce(uuid_extract_timestamp(e.receipt_id), e.created_at) AS "received_at!: Timestamp",
                      e.created_at AS "created_at: Timestamp"
                 FROM delivery_events e
                WHERE e.workspace_id = $1 AND ($2::uuid IS NULL OR e.id > $2)
                  AND ($3::uuid IS NULL OR e.message_id = $3) AND ($4::text IS NULL OR e.kind = $4)
                  AND ($5::text IS NULL OR e.recipient_email_key = ascii_lower($5))
                  AND (($6::uuid IS NULL AND $7::uuid IS NULL) OR EXISTS (
                        SELECT 1 FROM messages m WHERE m.workspace_id = e.workspace_id AND m.id = e.message_id
                           AND ($6::uuid IS NULL OR m.campaign_id = $6) AND ($7::uuid IS NULL OR m.person_id = $7)))
                ORDER BY e.id LIMIT $8"#,
            workspace.uuid(),
            cursor,
            f.message_id.map(|id| id.uuid()),
            kind,
            f.recipient,
            f.campaign_id.map(|id| id.uuid()),
            f.person_id.map(|id| id.uuid()),
            limit,
        )
        .fetch_all(&mut **tx)
        .await?
    } else {
        sqlx::query_as!(
            EventRecord,
            r#"SELECT e.id AS "id: Id<DeliveryEvent>", e.message_id AS "message_id: Id<Message>",
                      e.thread_id AS "thread_id: Id<Thread>", e.attempt_number, e.kind, e.source, e.confidence,
                      e.recipient_email, e.recipient_ref, e.action, e.phase, e.enhanced_status, e.category, e.diagnostic,
                      e.observed_at AS "observed_at: Timestamp",
                      coalesce(uuid_extract_timestamp(e.receipt_id), e.created_at) AS "received_at!: Timestamp",
                      e.created_at AS "created_at: Timestamp"
                 FROM delivery_events e
                WHERE e.workspace_id = $1 AND ($2::uuid IS NULL OR e.id < $2)
                  AND ($3::uuid IS NULL OR e.message_id = $3) AND ($4::text IS NULL OR e.kind = $4)
                  AND ($5::text IS NULL OR e.recipient_email_key = ascii_lower($5))
                  AND (($6::uuid IS NULL AND $7::uuid IS NULL) OR EXISTS (
                        SELECT 1 FROM messages m WHERE m.workspace_id = e.workspace_id AND m.id = e.message_id
                           AND ($6::uuid IS NULL OR m.campaign_id = $6) AND ($7::uuid IS NULL OR m.person_id = $7)))
                ORDER BY e.id DESC LIMIT $8"#,
            workspace.uuid(),
            cursor,
            f.message_id.map(|id| id.uuid()),
            kind,
            f.recipient,
            f.campaign_id.map(|id| id.uuid()),
            f.person_id.map(|id| id.uuid()),
            limit,
        )
        .fetch_all(&mut **tx)
        .await?
    };
    Ok(rows.into_iter().map(DeliveryEventObject::from).collect())
}

/// One delivery event of `workspace`.
async fn event(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<DeliveryEvent>,
) -> Result<Option<DeliveryEventObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        EventRecord,
        r#"SELECT e.id AS "id: Id<DeliveryEvent>", e.message_id AS "message_id: Id<Message>",
                  e.thread_id AS "thread_id: Id<Thread>", e.attempt_number, e.kind, e.source, e.confidence,
                  e.recipient_email, e.recipient_ref, e.action, e.phase, e.enhanced_status, e.category, e.diagnostic,
                  e.observed_at AS "observed_at: Timestamp",
                  coalesce(uuid_extract_timestamp(e.receipt_id), e.created_at) AS "received_at!: Timestamp",
                  e.created_at AS "created_at: Timestamp"
             FROM delivery_events e WHERE e.workspace_id = $1 AND e.id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(DeliveryEventObject::from))
}

/// List the workspace's delivery events, newest first by default.
#[utoipa::path(
    get,
    path = "/delivery_events",
    tag = "Messages",
    operation_id = "delivery_events.list",
    params(
        ("limit" = Option<i64>, Query, description = "Events per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("message_id" = Option<Id<Message>>, Query, description = "Events about this message."),
        ("campaign_id" = Option<Id<Campaign>>, Query, description = "Events about this campaign's messages."),
        ("person_id" = Option<Id<Person>>, Query, description = "Events about messages to this person."),
        ("kind" = Option<DeliveryEventKind>, Query, description = "Events of this kind."),
        ("recipient" = Option<String>, Query, description = "Events about this recipient (ignoring ASCII case)."),
    ),
    responses(
        (status = 200, description = "A page of delivery events.", body = Page<DeliveryEventObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `messages:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_events(
    principal: Principal,
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
    Query(filters): Query<EventFilters>,
) -> ApiResult<Json<Page<DeliveryEventObject>>> {
    principal.require(Scope::MessagesRead)?;
    let ws = principal.workspace;
    let params =
        PageParams::from_query(&state.keys, ws, "delivery_events", "id", &filters, &query)?;
    let mut tx = state.db.begin_in(ws).await?;
    let rows = events(
        &mut tx,
        ws,
        &filters,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(count_events(&mut tx, ws, &filters, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    Ok(Json(Page::from_ids(
        &state.keys,
        &params,
        rows,
        total,
        |event| event.id.uuid(),
    )))
}

/// Counts `workspace`'s delivery events matching `filters`, up to `cap + 1` (so a capped count is
/// told from an exact one).
async fn count_events(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &EventFilters,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    let f = filters;
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM delivery_events e
                WHERE e.workspace_id = $1
                  AND ($2::uuid IS NULL OR e.message_id = $2) AND ($3::text IS NULL OR e.kind = $3)
                  AND ($4::text IS NULL OR e.recipient_email_key = ascii_lower($4))
                  AND (($5::uuid IS NULL AND $6::uuid IS NULL) OR EXISTS (
                        SELECT 1 FROM messages m WHERE m.workspace_id = e.workspace_id AND m.id = e.message_id
                           AND ($5::uuid IS NULL OR m.campaign_id = $5) AND ($6::uuid IS NULL OR m.person_id = $6)))
                LIMIT $7) counted"#,
        workspace.uuid(),
        f.message_id.map(|id| id.uuid()),
        f.kind.map(<&'static str>::from),
        f.recipient,
        f.campaign_id.map(|id| id.uuid()),
        f.person_id.map(|id| id.uuid()),
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// Retrieve a delivery event.
#[utoipa::path(
    get,
    path = "/delivery_events/{id}",
    tag = "Messages",
    operation_id = "delivery_events.retrieve",
    params(("id" = Id<DeliveryEvent>, Path, description = "The delivery event id (`dev_…`).")),
    responses(
        (status = 200, description = "The delivery event.", body = DeliveryEventObject),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `messages:read`."),
        (status = 404, description = "No such delivery event in this workspace (`not_found`), or its period was archived (`archived`): `POST /v1/exports` reads it."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_event(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<DeliveryEvent>>,
) -> ApiResult<Json<DeliveryEventObject>> {
    principal.require(Scope::MessagesRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let Some(event) = event(&mut tx, principal.workspace, id).await? else {
        return Err(
            problem::missing(&mut tx, "delivery_events", id.uuid(), "delivery event").await,
        );
    };
    tx.commit().await?;
    Ok(Json(event))
}

// ───────────────────────────── cancel and resolve (the delivery paths) ─────────────────────────────

impl From<super::expire::CancelError> for Problem {
    fn from(error: super::expire::CancelError) -> Self {
        match error {
            super::expire::CancelError::NotFound => Problem::not_found("message"),
            super::expire::CancelError::NotQueued(state) => Problem::invalid_state(format!(
                "Only a queued message can be cancelled; this one is {state}."
            )),
            super::expire::CancelError::Db(error) => Problem::from(error),
        }
    }
}

impl From<super::reconcile::ResolveError> for Problem {
    fn from(error: super::reconcile::ResolveError) -> Self {
        match error {
            super::reconcile::ResolveError::NotFound => Problem::not_found("message"),
            super::reconcile::ResolveError::NotUncertain(state) => Problem::invalid_state(format!(
                "Only an uncertain message can be resolved; this one is {state}."
            )),
            super::reconcile::ResolveError::Db(error) => Problem::from(error),
        }
    }
}

/// Cancel a queued message.
///
/// Only a `queued` message can be cancelled: one a sender has claimed or started is on its way
/// and cannot be recalled (`409 invalid_state`), and a sent, failed or cancelled one has nothing
/// left to cancel. The message becomes `cancelled` and `message.cancelled` is sent to the
/// workspace's webhook endpoints.
#[utoipa::path(
    post,
    path = "/messages/{id}/cancel",
    tag = "Messages",
    operation_id = "messages.cancel",
    params(("id" = Id<Message>, Path, description = "The message id (`msg_…`).")),
    responses(
        (status = 200, description = "The message, cancelled.", body = MessageObject),
        (status = 400, description = "No `Idempotency-Key`."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `messages:send`."),
        (status = 404, description = "No such message in this workspace."),
        (status = 409, description = "The message is not queued (`invalid_state`)."),
    ),
    security(("bearer" = []))
)]
async fn cancel_message(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Message>>,
) -> ApiResult<Json<MessageObject>> {
    principal.require(Scope::MessagesSend)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    super::expire::cancel(&mut tx, principal.workspace, id).await?;
    let message = read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("message"))?;
    tx.commit().await?;
    Ok(Json(message))
}

/// The body of `POST /messages/{id}/resolve`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct ResolveMessage {
    /// `sent` when the provider took the message, `failed` when it was not sent.
    #[garde(skip)]
    state: super::reconcile::Resolution,
    /// What shows it, for the record: where the message was found, or why it was not sent. At
    /// most 2,000 characters.
    #[garde(length(chars, min = 1, max = 2_000))]
    evidence: String,
}

/// Resolve an uncertain message.
///
/// A message is `uncertain` when its submission ended without a readable final answer: the
/// provider may have taken it, so it is never sent again automatically. A person who knows what
/// happened settles it here, as `sent` or `failed`, with the evidence; the decision is recorded
/// as a `manual` delivery event, the counters follow, and `message.sent` or `message.failed` is
/// sent to the workspace's webhook endpoints. Only an `uncertain` message can be resolved
/// (`409 invalid_state`).
#[utoipa::path(
    post,
    path = "/messages/{id}/resolve",
    tag = "Messages",
    operation_id = "messages.resolve",
    params(("id" = Id<Message>, Path, description = "The message id (`msg_…`).")),
    request_body(content = ResolveMessage, example = json!({
        "state": "sent",
        "evidence": "Found in the mailbox's Sent folder on 2026-10-02 at 09:14 UTC."
    })),
    responses(
        (status = 200, description = "The message, resolved.", body = MessageObject),
        (status = 400, description = "No `Idempotency-Key`, or malformed JSON."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `messages:send`."),
        (status = 404, description = "No such message in this workspace."),
        (status = 409, description = "The message is not uncertain (`invalid_state`)."),
        (status = 422, description = "The body is invalid (`validation_failed`)."),
    ),
    security(("bearer" = []))
)]
async fn resolve_message(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Message>>,
    Json(body): Json<ResolveMessage>,
) -> ApiResult<Json<MessageObject>> {
    principal.require(Scope::MessagesSend)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    super::reconcile::resolve(
        &mut tx,
        principal.workspace,
        id,
        body.state,
        &body.evidence,
        &principal.actor.id(),
    )
    .await?;
    let message = read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("message"))?;
    tx.commit().await?;
    Ok(Json(message))
}

/// The body of `POST /messages/{id}/release_holds`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct ReleaseHolds {
    /// What shows the holds no longer apply, for the record: what the person checked. At most
    /// 2,000 characters.
    #[garde(length(chars, min = 1, max = 2_000))]
    evidence: String,
}

/// Release a message's holds.
///
/// A recipient is held when evidence about this message said its mailbox was full, its server
/// asked to come back later, or its domain had no mail route: mail to the address waits until a
/// later success of the message lifts the hold, the hold's time runs out, or a person who knows
/// better releases it here. Every open hold of the message is resolved as `manual`, so mail to
/// those addresses flows again, and the evidence is kept in the workspace's audit log. A message
/// without open holds is answered as it is.
#[utoipa::path(
    post,
    path = "/messages/{id}/release_holds",
    tag = "Messages",
    operation_id = "messages.release_holds",
    params(("id" = Id<Message>, Path, description = "The message id (`msg_…`).")),
    request_body(content = ReleaseHolds, example = json!({
        "evidence": "The recipient confirmed by phone that their mailbox has room again."
    })),
    responses(
        (status = 200, description = "The message, its holds released.", body = MessageObject),
        (status = 400, description = "No `Idempotency-Key`, or malformed JSON."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `messages:send`."),
        (status = 404, description = "No such message in this workspace."),
        (status = 422, description = "The body is invalid (`validation_failed`)."),
    ),
    security(("bearer" = []))
)]
async fn release_holds(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Message>>,
    Json(body): Json<ReleaseHolds>,
) -> ApiResult<Json<MessageObject>> {
    principal.require(Scope::MessagesSend)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let released = sqlx::query_scalar!(
        "UPDATE recipient_holds SET resolved_at = now(), resolution = 'manual'
          WHERE workspace_id = $1 AND message_id = $2 AND resolved_at IS NULL
      RETURNING email",
        principal.workspace.uuid(),
        id.uuid(),
    )
    .fetch_all(&mut *tx)
    .await?;
    let message = read(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("message"))?;
    if !released.is_empty() {
        crate::identity::audit::record(
            &mut tx,
            principal.workspace,
            principal.actor.into(),
            crate::identity::audit::Action::MessageHoldsReleased,
            Some(id.to_string()),
            serde_json::json!({ "evidence": body.evidence, "released": released }),
            None,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(message))
}

#[cfg(test)]
mod tests;
