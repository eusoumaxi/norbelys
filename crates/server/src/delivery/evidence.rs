//! Evidence: the one place that records what is observed about a message's fate, and what it
//! does to the message and to its recipients.
//!
//! A delivery event is one observation about one message (or about no known message: an
//! unmatched report is kept with no message for review) from a named source, with a confidence.
//! The submission's own answer (an acceptance, a recipient refused at `RCPT TO`, a permanent
//! refusal), a provider's signed callback, a delivery status notification, an abuse report, a
//! person's decision and our own read of a mailbox's Sent folder are all recorded here, through
//! [`record`], in the caller's transaction:
//!
//! 1. **The events**: one `delivery_events` row each, numbered by the database's clock, so a
//!    late report always lands in the current period, whatever happened to the message's own
//!    rows.
//! 2. **The message**: evidence settles only an `uncertain` message: trustworthy evidence that the
//!    provider took it makes it `sent`, a person's refusal makes it `failed`
//!    (`domain::policy::delivery::after_evidence`); `message.sent` or `message.failed` is told and
//!    a sent campaign message counted. Nothing else moves a message's state; a sent message keeps
//!    its state, and its fate is in its events.
//! 3. **The recipients**: an acceptance resolves the holds of the message that named its
//!    recipients; a full mailbox or a domain without mail holds the recipient for a while; only
//!    authenticated evidence that names the recipient (or the recipient's own unsubscribe)
//!    suppresses an address, through the one suppression operation
//!    (`people::suppressions::create`); a corroborated report that the address does not exist
//!    holds it while a person reviews the report, or suppresses it in a workspace that trusts its
//!    own inbox's reports (`workspaces.settings.delivery`, read only when such a report is
//!    recorded); weaker evidence comes back as a review proposal for the caller to attach to
//!    what it read (`domain::policy::delivery::recipient_effect`).
//! 4. **The counters**: deliveries, bounces, unsubscribes and complaints of campaign messages
//!    (acceptance is counted where a message becomes `sent`, never from its events).
//! 5. **The customer**: `delivery_event.recorded` for every event except the in-session
//!    acceptance, which `message.sent` already tells.
//! 6. **The complaint-rate breaker**: a complaint about a message asks for its connection's
//!    complaint rate ([`ComplaintRate`]), which disables the connection once complaints reach
//!    0.3 % of what it sent over seven days.
//!
//! Replays are refused before anything reaches here, by the source's own identity: the attempt
//! row for a submission's answer, the provider event key for a callback, the transport key for
//! mail-borne reports.
//!
//! Lock order: message rows first (the settlement of uncertain messages), then holds, then
//! suppressions, the order a submission's finish takes them too (it has locked its messages
//! before it records), so the two never wait on each other in opposite orders; a complaint-rate
//! count is enqueued last, an insert that locks no existing row (its lane is created with
//! `ON CONFLICT DO NOTHING`, a twin of the same key coalesces). A connection's row is never
//! locked here: the count's job locks it in a transaction of its own.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::db::Tx;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{
    Attempt, Connection, DeliveryEvent, Id, Message, ReceiveBinding, WorkspaceId,
};
use crate::domain::messages::State as MessageState;
use crate::domain::policy::delivery::{
    self as policy, Category, Confidence, DeliverySettings, EventKind, HoldReason, Metric, Phase,
    RecipientEffect, RecipientRef, Source,
};
use crate::domain::senders::{HealthEvent, Status};
use crate::domain::suppressions::Source as SuppressedBy;
use crate::domain::time::Timestamp;
use crate::jobs::{self, Effect, Job, JobContext, JobError, Outcome, Queue};
use crate::people::{self, suppressions};
use crate::problem::{FieldError, Problem};
use crate::senders::health;
use crate::webhooks::EventType;
use crate::webhooks::outbox::{self, Event};

/// The longest diagnostic kept on an event.
const DIAGNOSTIC_CHARS: usize = 1_000;

/// What a delivery status notification says happened to a recipient (RFC 3464 §2.3.3,
/// <https://www.rfc-editor.org/rfc/rfc3464#section-2.3.3>), when the evidence is one.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumIter, utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[schema(as = DsnAction, rename_all = "snake_case")]
pub enum Action {
    /// The message could not be delivered.
    Failed,
    /// Delivery is delayed and will be tried again.
    Delayed,
    /// It was delivered.
    Delivered,
    /// It was relayed to a system that does not report.
    Relayed,
    /// It was delivered and forwarded to more recipients.
    Expanded,
}

/// One observation to record.
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    /// The message it concerns, when known (from our Message-ID, a tag, the attempt).
    pub message: Option<Id<Message>>,
    /// The thread it concerns, when the Message-ID carried it.
    pub thread: Option<Uuid>,
    /// The attempt it came from, for a submission's answer.
    pub attempt_number: Option<i32>,
    /// The recipient's address, when known.
    pub recipient: Option<String>,
    /// Why the recipient is known; [`RecipientRef::Unknown`] whenever `recipient` is `None`.
    pub recipient_ref: RecipientRef,
    /// Where it came from.
    pub source: Source,
    /// The source's own identity of the observation (`attempt:<id>`, a DSN's Message-ID, a
    /// provider's event id): several events from one report share it.
    pub source_event_id: String,
    /// The receive binding a mail-borne report arrived through.
    pub received_via: Option<Id<ReceiveBinding>>,
    /// What was observed.
    pub kind: EventKind,
    /// A DSN's action, when it is one.
    pub action: Option<Action>,
    /// The protocol step, for a submission's answer.
    pub phase: Option<Phase>,
    /// The RFC 3463 status, as text (`5.1.1`).
    pub enhanced_status: Option<String>,
    /// Why, in the closed vocabulary.
    pub category: Category,
    /// The provider's or the reporter's text, bounded on recording.
    pub diagnostic: Option<String>,
    /// How much it can be trusted.
    pub confidence: Confidence,
    /// The provider callback receipt it was normalised from.
    pub receipt: Option<Uuid>,
    /// When the source observed it.
    pub observed_at: Timestamp,
}

/// What recording one observation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    /// The event's id.
    pub id: Id<DeliveryEvent>,
    /// What it did to its recipient's address; the proposal of a [`RecipientEffect::Review`] or
    /// a [`RecipientEffect::HoldAndReview`] is the caller's to attach to what it read (an inbound
    /// report's review proposal).
    pub effect: RecipientEffect,
}

/// A message whose state evidence settled.
struct Settled {
    id: Id<Message>,
    state: MessageState,
    category: Category,
    at: Timestamp,
}

/// Records `evidence` in `workspace`, in the caller's transaction, and applies what it does (see
/// the module). Returns one [`Recorded`] per observation, in order.
///
/// # Errors
///
/// The database refused.
pub async fn record(
    tx: &mut Tx,
    workspace: WorkspaceId,
    evidence: &[Evidence],
) -> Result<Vec<Recorded>, sqlx::Error> {
    if evidence.is_empty() {
        return Ok(Vec::new());
    }
    let ids = insert(tx, workspace, evidence).await?;
    let settings = settings_for(tx, workspace, evidence).await?;
    let recorded: Vec<Recorded> = ids
        .into_iter()
        .zip(evidence)
        .map(|(id, observed)| Recorded {
            id,
            effect: policy::recipient_effect(
                observed.kind,
                observed.category,
                reference(observed),
                observed.confidence,
                settings,
            ),
        })
        .collect();

    settle_messages(tx, workspace, evidence).await?;
    resolve_holds(tx, workspace, evidence).await?;
    hold(tx, workspace, evidence, &recorded).await?;
    suppress(tx, workspace, evidence, &recorded).await?;
    count(tx, workspace, evidence).await?;
    for (observed, event) in evidence.iter().zip(&recorded) {
        if observed.source.in_session() && observed.kind == EventKind::Accepted {
            continue;
        }
        outbox::record(
            tx,
            workspace,
            Event {
                kind: EventType::DeliveryEventRecorded,
                subject_type: "delivery_event",
                subject_id: event.id.uuid(),
                data: json!({
                    "delivery_event_id": event.id,
                    "message_id": observed.message,
                    "kind": observed.kind.as_str(),
                    "source": observed.source.as_str(),
                    "confidence": observed.confidence.as_str(),
                    "recipient": observed.recipient,
                }),
            },
        )
        .await?;
    }
    complaints(tx, workspace, evidence).await?;
    Ok(recorded)
}

/// The workspace's delivery settings when some evidence depends on them (a corroborated report
/// that an address does not exist), else the defaults without a query, so recording any other
/// evidence never reads the workspace's row.
async fn settings_for(
    tx: &mut Tx,
    workspace: WorkspaceId,
    evidence: &[Evidence],
) -> Result<DeliverySettings, sqlx::Error> {
    let needed = evidence.iter().any(|observed| {
        observed.category == Category::InvalidRecipient
            && observed.confidence == Confidence::Corroborated
    });
    if !needed {
        return Ok(DeliverySettings::default());
    }
    let stored = sqlx::query_scalar!(
        r#"SELECT settings -> 'delivery' AS "delivery" FROM workspaces WHERE id = $1"#,
        workspace.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    // The settings were checked when they were written; a value that no longer reads (a field a
    // later version dropped) keeps the defaults rather than failing the evidence.
    Ok(DeliverySettings::parse(stored.as_ref()).unwrap_or_default())
}

/// Checks `settings.delivery` in a workspace update before it is stored: absent is fine (the
/// defaults apply); anything else must read as [`DeliverySettings`], and every invalid field is
/// answered at once, its pointer under `/settings/delivery`.
///
/// # Errors
///
/// `422 validation_failed` with one entry per invalid field.
pub fn check_settings(settings: &Map<String, Value>) -> Result<(), Problem> {
    let Some(delivery) = settings.get("delivery") else {
        return Ok(());
    };
    DeliverySettings::parse(Some(delivery))
        .map(|_| ())
        .map_err(|errors| {
            Problem::validation(
                errors
                    .into_iter()
                    .map(|error| FieldError {
                        pointer: format!("/settings/delivery{}", error.pointer),
                        code: "invalid".to_owned(),
                        detail: error.detail.to_owned(),
                    })
                    .collect(),
            )
        })
}

/// Asks for the complaint rate of every connection the evidence reports a complaint about
/// ([`ComplaintRate`]), in the caller's transaction: an insert into the jobs, which locks no
/// existing row (a twin of the same connection's count coalesces). The rate is computed, and the
/// connection's health moved, by that job after the commit under the connection's own lock:
/// every delivery path locks a connection before its messages, so locking the connection here,
/// after the evidence locked messages, could deadlock against a submission's finish. Nothing
/// wakes the maintenance lane for it: the runner's next poll claims it, which a count can wait
/// for.
async fn complaints(
    tx: &mut Tx,
    workspace: WorkspaceId,
    evidence: &[Evidence],
) -> Result<(), sqlx::Error> {
    let messages: Vec<Uuid> = evidence
        .iter()
        .filter(|observed| observed.kind == EventKind::Complaint)
        .filter_map(|observed| observed.message)
        .map(|message| message.uuid())
        .collect();
    if messages.is_empty() {
        return Ok(());
    }
    let connections = sqlx::query_scalar!(
        "SELECT DISTINCT connection_id FROM messages WHERE workspace_id = $1 AND id = ANY($2)",
        workspace.uuid(),
        &messages,
    )
    .fetch_all(&mut **tx)
    .await?;
    let checks: Vec<ComplaintRate> = connections
        .into_iter()
        .map(|connection| ComplaintRate {
            connection: Id::from_uuid(connection),
        })
        .collect();
    jobs::enqueue_many(tx, workspace, &checks, None).await?;
    Ok(())
}

/// `connection.complaint_rate`: the complaint-rate breaker of one connection. It counts the
/// complaints recorded about the connection's messages over the last
/// [`policy::COMPLAINT_WINDOW`] against the messages it had accepted in that window, and when
/// they reach the rate [`policy::complaint_rate_exceeded`] names, it disables the connection
/// through the health table (`connection.health_changed`, and the grouped email to the people
/// told), with the counts in its detail: the list or the content is the problem, so sending
/// resumes only when a person verifies the connection again. A connection not `verifying` or
/// `active` is left as it is.
///
/// Its key is the connection, so the complaints of one batch, or of several reports in a row,
/// coalesce into one count; a count runs again with the next complaint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplaintRate {
    /// The connection a complaint was recorded about.
    pub connection: Id<Connection>,
}

impl Job for ComplaintRate {
    const KIND: &'static str = "connection.complaint_rate";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::Idempotent;

    fn unique_key(&self) -> Option<String> {
        Some(self.connection.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        let since = Timestamp(
            jiff::Timestamp::now()
                .saturating_sub(policy::COMPLAINT_WINDOW)
                .unwrap_or(jiff::Timestamp::UNIX_EPOCH),
        );
        let mut chunk = cx.begin().await?;
        let row = sqlx::query!(
            r#"SELECT c.status, c.paused,
                      (SELECT count(*) FROM delivery_events e
                         JOIN messages m ON m.workspace_id = e.workspace_id AND m.id = e.message_id
                        WHERE e.workspace_id = c.workspace_id AND m.connection_id = c.id
                          AND e.kind = 'complaint' AND e.id >= uuidv7_boundary($3)) AS "complaints!",
                      (SELECT coalesce(sum(u.used), 0) FROM connection_usage u
                        WHERE u.workspace_id = c.workspace_id AND u.connection_id = c.id
                          AND u.day >= ($3 AT TIME ZONE 'UTC')::date) AS "sent!"
                 FROM connections c
                WHERE c.workspace_id = $1 AND c.id = $2
                  FOR UPDATE OF c"#,
            workspace.uuid(),
            self.connection.uuid(),
            since as _,
        )
        .fetch_optional(&mut **chunk.tx())
        .await?;
        let Some(row) = row else {
            return Ok(Outcome::Done);
        };
        let status = row.status.parse::<Status>().map_err(|_| {
            JobError::Failed("the connection's status is not understood".to_owned())
        })?;
        if policy::complaint_rate_exceeded(row.complaints, row.sent) {
            let detail = format!(
                "Recipients reported {} messages of this connection as spam over the last seven days, \
                 against {} it sent: at least 0.3 %, the rate mailbox providers tell senders never to \
                 reach. Review the list and the content, then verify the connection to send again.",
                row.complaints, row.sent
            );
            health::apply(
                chunk.tx(),
                workspace,
                self.connection,
                status,
                row.paused,
                HealthEvent::ComplaintRate,
                Some(&detail),
            )
            .await?;
        }
        cx.checkpoint(
            chunk,
            json!({ "complaints": row.complaints, "sent": row.sent }),
        )
        .await?;
        Ok(Outcome::Done)
    }
}

/// The recipient reference the database accepts: an event without a recipient is `unknown`.
fn reference(observed: &Evidence) -> RecipientRef {
    if observed.recipient.is_none() {
        RecipientRef::Unknown
    } else {
        observed.recipient_ref
    }
}

/// Inserts the events, numbered by the database's clock (the partition key), and returns their
/// ids in input order.
async fn insert(
    tx: &mut Tx,
    workspace: WorkspaceId,
    evidence: &[Evidence],
) -> Result<Vec<Id<DeliveryEvent>>, sqlx::Error> {
    let messages: Vec<Option<Uuid>> = evidence
        .iter()
        .map(|e| e.message.map(|m| m.uuid()))
        .collect();
    let threads: Vec<Option<Uuid>> = evidence.iter().map(|e| e.thread).collect();
    let attempts: Vec<Option<i32>> = evidence.iter().map(|e| e.attempt_number).collect();
    let recipients: Vec<Option<String>> = evidence.iter().map(|e| e.recipient.clone()).collect();
    let references: Vec<&str> = evidence.iter().map(|e| reference(e).as_str()).collect();
    let sources: Vec<&str> = evidence.iter().map(|e| e.source.as_str()).collect();
    let source_events: Vec<String> = evidence
        .iter()
        .map(|e| bounded(&e.source_event_id, 512))
        .collect();
    let bindings: Vec<Option<Uuid>> = evidence
        .iter()
        .map(|e| e.received_via.map(|b| b.uuid()))
        .collect();
    let kinds: Vec<&str> = evidence.iter().map(|e| e.kind.as_str()).collect();
    let actions: Vec<Option<&str>> = evidence
        .iter()
        .map(|e| e.action.map(<&str>::from))
        .collect();
    let phases: Vec<Option<&str>> = evidence
        .iter()
        .map(|e| e.phase.map(Phase::as_str))
        .collect();
    let statuses: Vec<Option<String>> =
        evidence.iter().map(|e| e.enhanced_status.clone()).collect();
    let categories: Vec<&str> = evidence.iter().map(|e| e.category.as_str()).collect();
    let diagnostics: Vec<Option<String>> = evidence
        .iter()
        .map(|e| {
            e.diagnostic
                .as_deref()
                .map(|d| bounded(d, DIAGNOSTIC_CHARS))
        })
        .collect();
    let confidences: Vec<&str> = evidence.iter().map(|e| e.confidence.as_str()).collect();
    let receipts: Vec<Option<Uuid>> = evidence.iter().map(|e| e.receipt).collect();
    let observed: Vec<Timestamp> = evidence.iter().map(|e| e.observed_at).collect();
    sqlx::query_scalar!(
        r#"WITH e AS (
               SELECT uuidv7() AS id, t.*
                 FROM unnest($2::uuid[], $3::uuid[], $4::int[], $5::text[], $6::text[], $7::text[], $8::text[],
                             $9::uuid[], $10::text[], $11::text[], $12::text[], $13::text[], $14::text[], $15::text[],
                             $16::text[], $17::uuid[], $18::timestamptz[])
                      WITH ORDINALITY AS t(message_id, thread_id, attempt_number, recipient_email, recipient_ref, source,
                                           source_event_id, received_via, kind, action, phase, enhanced_status, category,
                                           diagnostic, confidence, receipt_id, observed_at, n)),
           inserted AS (
               INSERT INTO delivery_events (workspace_id, id, message_id, thread_id, attempt_number, recipient_email,
                                            recipient_ref, source, source_event_id, received_via, kind, action, phase,
                                            enhanced_status, category, diagnostic, confidence, receipt_id, observed_at)
               SELECT $1, id, message_id, thread_id, attempt_number, recipient_email, recipient_ref, source,
                      source_event_id, received_via, kind, action, phase, enhanced_status, category, diagnostic,
                      confidence, receipt_id, observed_at
                 FROM e
               RETURNING 1)
           SELECT id AS "id!: Id<DeliveryEvent>" FROM e ORDER BY n"#,
        workspace.uuid(),
        &messages as _,
        &threads as _,
        &attempts as _,
        &recipients as _,
        &references as _,
        &sources as _,
        &source_events,
        &bindings as _,
        &kinds as _,
        &actions as _,
        &phases as _,
        &statuses as _,
        &categories as _,
        &diagnostics as _,
        &confidences as _,
        &receipts as _,
        &observed as _,
    )
    .fetch_all(&mut **tx)
    .await
}

/// Settles the `uncertain` messages the evidence decides, each by its first deciding event,
/// under the message row's lock (the update itself, conditional on the state): tells the customer
/// and counts a sent campaign message.
async fn settle_messages(
    tx: &mut Tx,
    workspace: WorkspaceId,
    evidence: &[Evidence],
) -> Result<(), sqlx::Error> {
    let mut seen = HashSet::new();
    let mut decided: Vec<(Uuid, MessageState, Category, Timestamp)> = Vec::new();
    for observed in evidence {
        // A submission's own answer is recorded by its finish, which sets the state itself.
        let Some(message) = observed.message.filter(|_| !observed.source.in_session()) else {
            continue;
        };
        let Some(state) = policy::after_evidence(
            MessageState::Uncertain,
            observed.kind,
            observed.source,
            observed.confidence,
        ) else {
            continue;
        };
        if seen.insert(message.uuid()) {
            decided.push((
                message.uuid(),
                state,
                observed.category,
                observed.observed_at,
            ));
        }
    }
    if decided.is_empty() {
        return Ok(());
    }
    decided.sort_by_key(|(id, ..)| *id);
    let ids: Vec<Uuid> = decided.iter().map(|(id, ..)| *id).collect();
    let states: Vec<&str> = decided
        .iter()
        .map(|(_, state, ..)| state.as_str())
        .collect();
    let rows = sqlx::query!(
        r#"UPDATE messages m SET state = d.state,
                  sent_at = CASE WHEN d.state = 'sent' THEN coalesce(m.sent_at, now()) ELSE m.sent_at END
             FROM unnest($2::uuid[], $3::text[]) AS d(id, state)
            WHERE m.workspace_id = $1 AND m.id = d.id AND m.state = 'uncertain'
        RETURNING m.id AS "id: Id<Message>", m.state"#,
        workspace.uuid(),
        &ids,
        &states as _,
    )
    .fetch_all(&mut **tx)
    .await?;
    let settled: Vec<Settled> = rows
        .into_iter()
        .filter_map(|row| {
            let (_, state, category, at) = decided
                .iter()
                .find(|(id, ..)| *id == row.id.uuid())
                .copied()?;
            (row.state == state.as_str()).then_some(Settled {
                id: row.id,
                state,
                category,
                at,
            })
        })
        .collect();
    let sent: Vec<Id<Message>> = settled
        .iter()
        .filter(|s| s.state == MessageState::Sent)
        .map(|s| s.id)
        .collect();
    increment(tx, workspace, &sent, Metric::Sent).await?;
    for settled in settled {
        let kind = if settled.state == MessageState::Sent {
            EventType::MessageSent
        } else {
            EventType::MessageFailed
        };
        tell(
            tx,
            workspace,
            kind,
            settled.id,
            None,
            Some(settled.category),
            settled.at,
        )
        .await?;
    }
    Ok(())
}

/// A later success of the message resolves its holds: an acceptance, or a delivery a provider
/// reports after it (a full mailbox that took the message on a later try), for the named
/// recipient, or every recipient for one that concerns the whole envelope.
async fn resolve_holds(
    tx: &mut Tx,
    workspace: WorkspaceId,
    evidence: &[Evidence],
) -> Result<(), sqlx::Error> {
    let (messages, keys): (Vec<Uuid>, Vec<Option<String>>) = evidence
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Accepted | EventKind::Delivered))
        .filter_map(|e| {
            let key = match reference(e) {
                RecipientRef::Named => Some(e.recipient.as_deref()?.to_ascii_lowercase()),
                RecipientRef::SingleEnvelope | RecipientRef::Unknown => None,
            };
            Some((e.message?.uuid(), key))
        })
        .unzip();
    if messages.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE recipient_holds h SET resolved_at = now(), resolution = 'delivered'
           FROM unnest($2::uuid[], $3::text[]) AS a(message_id, email_key)
          WHERE h.workspace_id = $1 AND h.message_id = a.message_id AND h.resolved_at IS NULL
            AND (a.email_key IS NULL OR h.email_key = a.email_key)",
        workspace.uuid(),
        &messages,
        &keys as _,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Holds the recipients the evidence holds, each until its reason's review: once per message and
/// address, the latest observation winning.
async fn hold(
    tx: &mut Tx,
    workspace: WorkspaceId,
    evidence: &[Evidence],
    recorded: &[Recorded],
) -> Result<(), sqlx::Error> {
    let mut seen = HashSet::new();
    let mut holds: Vec<(Uuid, String, HoldReason, Timestamp)> = Vec::new();
    for (observed, event) in evidence.iter().zip(recorded).rev() {
        let (RecipientEffect::Hold(reason) | RecipientEffect::HoldAndReview(reason, _)) =
            event.effect
        else {
            continue;
        };
        let (Some(message), Some(email)) = (observed.message, observed.recipient.as_ref()) else {
            continue;
        };
        if seen.insert((message.uuid(), email.to_ascii_lowercase())) {
            holds.push((message.uuid(), email.clone(), reason, observed.observed_at));
        }
    }
    if holds.is_empty() {
        return Ok(());
    }
    let messages: Vec<Uuid> = holds.iter().map(|(m, ..)| *m).collect();
    let emails: Vec<String> = holds.iter().map(|(_, e, ..)| e.clone()).collect();
    let reasons: Vec<&str> = holds.iter().map(|(_, _, r, _)| r.as_str()).collect();
    let observed: Vec<Timestamp> = holds.iter().map(|(.., at)| *at).collect();
    let reviews: Vec<Timestamp> = holds
        .iter()
        .map(|(_, _, reason, at)| {
            Timestamp(
                at.0.saturating_add(reason.review_after())
                    .unwrap_or(jiff::Timestamp::MAX),
            )
        })
        .collect();
    sqlx::query!(
        "INSERT INTO recipient_holds (workspace_id, message_id, email, reason, observed_at, review_after)
         SELECT $1, h.message_id, h.email, h.reason, h.observed_at, h.review_after
           FROM unnest($2::uuid[], $3::text[], $4::text[], $5::timestamptz[], $6::timestamptz[])
                AS h(message_id, email, reason, observed_at, review_after)
         ON CONFLICT (workspace_id, message_id, email_key) DO UPDATE
            SET reason = EXCLUDED.reason, observed_at = EXCLUDED.observed_at,
                review_after = EXCLUDED.review_after, resolved_at = NULL, resolution = NULL",
        workspace.uuid(),
        &messages,
        &emails,
        &reasons as _,
        &observed as _,
        &reviews as _,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Who suppresses an address, by the evidence's source: the source itself for evidence from a
/// party the suppression can name, `None` for our own checks (preflight, the Sent folder), which
/// never suppress.
fn suppressed_by(source: Source) -> Option<SuppressedBy> {
    match source {
        Source::Smtp => Some(SuppressedBy::Smtp),
        Source::ProviderApi => Some(SuppressedBy::ProviderApi),
        Source::ProviderWebhook => Some(SuppressedBy::ProviderWebhook),
        Source::Dsn => Some(SuppressedBy::Dsn),
        Source::Arf => Some(SuppressedBy::Arf),
        Source::InboundNotice => Some(SuppressedBy::InboundNotice),
        Source::Unsubscribe => Some(SuppressedBy::Unsubscribe),
        Source::Manual => Some(SuppressedBy::Manual),
        Source::Preflight | Source::SentFolder => None,
    }
}

/// Suppresses the addresses the evidence suppresses, through the one suppression operation; an
/// address suppressed already stays as it was.
async fn suppress(
    tx: &mut Tx,
    workspace: WorkspaceId,
    evidence: &[Evidence],
    recorded: &[Recorded],
) -> Result<(), sqlx::Error> {
    for (observed, event) in evidence.iter().zip(recorded) {
        let RecipientEffect::Suppress(reason) = event.effect else {
            continue;
        };
        let (Some(recipient), Some(by)) = (&observed.recipient, suppressed_by(observed.source))
        else {
            continue;
        };
        let Ok(email) = EmailAddress::parse(recipient) else {
            continue;
        };
        let summary: Value = json!({
            "kind": observed.kind.as_str(),
            "category": observed.category.as_str(),
            "enhanced_status": observed.enhanced_status,
            "source": observed.source.as_str(),
            "observed_at": observed.observed_at,
        });
        match suppressions::create(
            tx,
            workspace,
            &suppressions::NewSuppression {
                email: &email,
                reason,
                source: by,
                source_event: Some(event.id.uuid()),
                evidence: Some(summary),
                created_by: "system".to_owned(),
            },
        )
        .await
        {
            Ok(_) | Err(people::Error::Conflict(_)) => {}
            Err(people::Error::Db(error)) => return Err(error),
            Err(other) => return Err(sqlx::Error::Protocol(other.to_string())),
        }
    }
    Ok(())
}

/// Counts the campaign counters the evidence moves.
async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    evidence: &[Evidence],
) -> Result<(), sqlx::Error> {
    for metric in [
        Metric::Delivered,
        Metric::Bounced,
        Metric::Unsubscribed,
        Metric::Complained,
    ] {
        let messages: Vec<Id<Message>> = evidence
            .iter()
            .filter(|e| policy::evidence_metric(e.kind, e.category) == Some(metric))
            .filter_map(|e| e.message)
            .collect();
        increment(tx, workspace, &messages, metric).await?;
    }
    Ok(())
}

/// Adds one to `metric` for each message among `messages`, on today's UTC day, in the
/// caller's transaction (the rollup reads these rows).
///
/// # Errors
///
/// The database refused.
pub(crate) async fn increment(
    tx: &mut Tx,
    workspace: WorkspaceId,
    messages: &[Id<Message>],
    metric: Metric,
) -> Result<(), sqlx::Error> {
    if messages.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO stats_increments (workspace_id, connection_id, message_kind, campaign_id, step_id, step_revision, variant_id, variant_version, day, metric, delta)
         SELECT m.workspace_id, m.connection_id, m.kind, m.campaign_id, m.step_id, m.step_revision, m.variant_id, m.variant_version,
                (now() AT TIME ZONE 'UTC')::date, $3, 1
           FROM unnest($2::uuid[]) AS i(message_id)
           JOIN messages m ON m.workspace_id = $1 AND m.id = i.message_id"
    ).bind(workspace.uuid()).bind(messages.iter().map(|id|id.uuid()).collect::<Vec<_>>()).bind(metric.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Tells the customer that a message changed state (`message.sent`, `message.failed`,
/// `message.uncertain`, `message.cancelled`), in the caller's transaction: the message, its
/// attempt (the latest when not given), the new state's category and when it happened. The one
/// shape of every `message.*` event the delivery engine writes ([`told`]).
///
/// # Errors
///
/// The database refused.
pub(crate) async fn tell(
    tx: &mut Tx,
    workspace: WorkspaceId,
    kind: EventType,
    message: Id<Message>,
    attempt: Option<Id<Attempt>>,
    category: Option<Category>,
    at: Timestamp,
) -> Result<(), sqlx::Error> {
    let attempt = match attempt {
        Some(attempt) => Some(attempt),
        None => {
            sqlx::query_scalar!(
                r#"SELECT id AS "id: Id<Attempt>" FROM attempts
                WHERE workspace_id = $1 AND message_id = $2 ORDER BY attempt_number DESC LIMIT 1"#,
                workspace.uuid(),
                message.uuid(),
            )
            .fetch_optional(&mut **tx)
            .await?
        }
    };
    outbox::record(tx, workspace, told(kind, message, attempt, category, at)).await?;
    Ok(())
}

/// Tells the customer that each of `messages` changed state the same way, as [`tell`] does for
/// one with its latest attempt, with one lookup of their attempts and one statement for their
/// events, in the order given: what a change that ends many messages at once writes (a list of
/// suppressed addresses stopping their enrollments cancels up to one queued message each).
///
/// # Errors
///
/// The database refused.
pub(crate) async fn tell_all(
    tx: &mut Tx,
    workspace: WorkspaceId,
    kind: EventType,
    messages: &[Uuid],
    category: Option<Category>,
    at: Timestamp,
) -> Result<(), sqlx::Error> {
    if messages.is_empty() {
        return Ok(());
    }
    let latest: HashMap<Uuid, Id<Attempt>> = sqlx::query!(
        r#"SELECT DISTINCT ON (message_id) message_id, id AS "id: Id<Attempt>" FROM attempts
            WHERE workspace_id = $1 AND message_id = ANY($2)
            ORDER BY message_id, attempt_number DESC"#,
        workspace.uuid(),
        messages,
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| (row.message_id, row.id))
    .collect();
    let events: Vec<Event> = messages
        .iter()
        .map(|message| {
            let attempt = latest.get(message).copied();
            told(kind, Id::from_uuid(*message), attempt, category, at)
        })
        .collect();
    outbox::record_all(tx, workspace, &events).await
}

/// The event of a message's change of state: the message, its attempt, the new state's category
/// and when it happened.
fn told(
    kind: EventType,
    message: Id<Message>,
    attempt: Option<Id<Attempt>>,
    category: Option<Category>,
    at: Timestamp,
) -> Event {
    Event {
        kind,
        subject_type: "message",
        subject_id: message.uuid(),
        data: json!({
            "message_id": message,
            "attempt_id": attempt,
            "category": category.map(Category::as_str),
            "occurred_at": at,
        }),
    }
}

/// Reports one message's final failure as the error-level `delivery.failure` event: `state` is
/// what the message became, `category` why. Only `failed` (an expiry included) and `uncertain`
/// are failures; any other state reports nothing, and a transient attempt never reaches here (it
/// is counted in its wave's event), so a provider hiccup that heals pages nobody.
///
/// Call it once the transaction that ended the message has committed, for each message it ended:
/// an ending that rolls back reports nothing, and one done again by another owner after a lost
/// lease reports once. A message that was `uncertain` and that later evidence settles as failed
/// is not reported again: it was reported when it became uncertain.
pub(crate) fn report_failure(
    workspace: WorkspaceId,
    message: Id<Message>,
    state: MessageState,
    category: Option<Category>,
) {
    if !matches!(state, MessageState::Failed | MessageState::Uncertain) {
        return;
    }
    crate::telemetry::unit(crate::telemetry::Event::DeliveryFailure);
    tracing::error!(
        event = "delivery.failure",
        workspace_id = %workspace,
        message_id = %message,
        state = state.as_str(),
        category = category.map(Category::as_str),
        "delivery.failure"
    );
}

/// `text` cut to at most `limit` characters, on one line.
fn bounded(text: &str, limit: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests;
