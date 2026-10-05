//! The outbox: business facts for the outside world, written in the same transaction as the
//! change they describe, and the relay that publishes them to webhook endpoints.
//!
//! Every event row is one envelope, whatever wrote it:
//! `payload = {"data": {...}, "synthetic"?: true, "webhook_endpoint_id"?: "<uuid>"}`.
//! `data` is what consumers receive. `synthetic` marks an event created on request
//! (`POST /events`) rather than by a real change; `webhook_endpoint_id` addresses a synthetic
//! event to one endpoint, which receives it whatever its subscriptions (that is how an
//! endpoint is tested). `subject_type` and `subject_id` name the object the event is about.
//!
//! `data` carries ids and the few fields a consumer needs to decide whether to fetch the
//! object; never message bodies, never embedded children.
//!
//! The relay (`outbox.relay`) is a fan-out singleton of the `system` workspace, run every 5
//! seconds. Its directory is the set of workspaces with unpublished events, read as the
//! scheduler role through the small partial index on unpublished rows; for each workspace it
//! publishes chunks of at most 100 events, each in that workspace's transaction: one delivery
//! per enabled endpoint the event reaches, the event marked published, and one
//! `webhook.deliver` job per delivery, all committed with the relay's fenced checkpoint. Events
//! are locked with `SKIP LOCKED` and deliveries are inserted with `ON CONFLICT DO NOTHING`, so a
//! late relay whose lease was lost can neither block nor duplicate the live one's work.

use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::metrics::Gauge;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::EventType;
use super::deliver::{self, Deliver};
use crate::db::Tx;
use crate::domain::ids::{
    Attempt, Campaign, Connection, DeliveryEvent, Enrollment, Export, Id, Import, InboundMessage,
    Message, OutboxEvent, Step, Suppression, Thread, WebhookDelivery, WebhookEndpoint, WorkspaceId,
};
use crate::domain::time::Timestamp;
use crate::jobs::{self, Class, Effect, Job, JobContext, JobError, Outcome, Queue};

/// Events published per chunk of the relay.
const CHUNK: i64 = 100;
/// Workspaces the relay visits per run; the rest wait for the next run, five seconds later.
const WORKSPACES_PER_RUN: i64 = 1_000;

static OUTBOX_LAG: LazyLock<Gauge<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_gauge("norbelys_outbox_lag_seconds")
        .with_unit("s")
        .with_description("Age of the oldest unpublished outbox event when the relay last ran.")
        .build()
});

/// An event to record.
#[derive(Debug, Clone)]
pub struct Event {
    /// What happened.
    pub kind: EventType,
    /// The resource the event is about, in the API's singular words (`message`, `campaign`).
    pub subject_type: &'static str,
    /// The id of that resource's row.
    pub subject_id: Uuid,
    /// What consumers receive: ids and the minimal fields to decide whether to fetch.
    pub data: Value,
}

/// Records `event` in `workspace`, inside the caller's transaction: it is published if and only
/// if the transaction commits.
///
/// # Errors
///
/// The database refused the row.
pub async fn record(
    tx: &mut Tx,
    workspace: WorkspaceId,
    event: Event,
) -> Result<Id<OutboxEvent>, sqlx::Error> {
    let mut event = event;
    routing(tx, workspace, &mut event).await?;
    insert(
        tx,
        workspace,
        event.kind,
        event.subject_type,
        event.subject_id,
        &json!({ "data": event.data }),
    )
    .await
}

/// Records `events` in `workspace` with one statement, inside the caller's transaction, in the
/// order given: what [`record`] does for each, for a change that touches many objects at once
/// (a list of addresses suppressed in one request), so its events cost one round trip rather
/// than one per object. Nothing is written for an empty list.
///
/// # Errors
///
/// The database refused the rows.
pub async fn record_all(
    tx: &mut Tx,
    workspace: WorkspaceId,
    events: &[Event],
) -> Result<(), sqlx::Error> {
    if events.is_empty() {
        return Ok(());
    }
    let mut events = events.to_vec();
    let messages: Vec<Uuid> = events
        .iter()
        .filter_map(|event| {
            event
                .data
                .get("message_id")
                .and_then(Value::as_str)
                .and_then(|id| id.parse::<Id<Message>>().ok())
                .map(|id| id.uuid())
                .or_else(|| (event.subject_type == "message").then_some(event.subject_id))
        })
        .collect();
    if !messages.is_empty() {
        let dimensions: Vec<(Uuid,Uuid,Option<Uuid>)> = sqlx::query_as("SELECT id,connection_id,campaign_id FROM messages WHERE workspace_id=$1 AND id=ANY($2)").bind(workspace.uuid()).bind(messages).fetch_all(&mut **tx).await?;
        for event in &mut events {
            if let Some((_, connection, campaign)) =
                dimensions.iter().find(|(id, _, _)| *id == event.subject_id)
                && let Some(data) = event.data.as_object_mut()
            {
                data.insert(
                    "connection_id".to_owned(),
                    json!(Id::<Connection>::from_uuid(*connection)),
                );
                data.insert(
                    "campaign_id".to_owned(),
                    json!(campaign.map(Id::<Campaign>::from_uuid)),
                );
            }
        }
    }
    let kinds: Vec<String> = events
        .iter()
        .map(|event| event.kind.as_str().to_owned())
        .collect();
    let subject_types: Vec<String> = events
        .iter()
        .map(|event| event.subject_type.to_owned())
        .collect();
    let subject_ids: Vec<Uuid> = events.iter().map(|event| event.subject_id).collect();
    let payloads: Vec<Value> = events
        .iter()
        .map(|event| json!({ "data": event.data }))
        .collect();
    // An event a request caused keeps the request's `traceparent`: its deliveries link to it.
    // Rows are inserted in the order given, so their ids (UUIDv7) keep that order.
    sqlx::query!(
        "INSERT INTO outbox_events (workspace_id, type, subject_type, subject_id, payload, trace_parent)
         SELECT $1, event.type, event.subject_type, event.subject_id, event.payload, $6
           FROM unnest($2::text[], $3::text[], $4::uuid[], $5::jsonb[])
                WITH ORDINALITY AS event(type, subject_type, subject_id, payload, position)
          ORDER BY event.position",
        workspace.uuid(),
        &kinds,
        &subject_types,
        &subject_ids,
        &payloads,
        crate::http::context::trace_parent(),
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert(
    tx: &mut Tx,
    workspace: WorkspaceId,
    kind: EventType,
    subject_type: &str,
    subject_id: Uuid,
    payload: &Value,
) -> Result<Id<OutboxEvent>, sqlx::Error> {
    // An event a request caused keeps the request's `traceparent`: its deliveries link to it.
    sqlx::query_scalar!(
        r#"INSERT INTO outbox_events (workspace_id, type, subject_type, subject_id, payload, trace_parent)
           VALUES ($1, $2, $3, $4, $5, $6)
           RETURNING id AS "id: Id<OutboxEvent>""#,
        workspace.uuid(),
        kind.as_str(),
        subject_type,
        subject_id,
        payload,
        crate::http::context::trace_parent(),
    )
    .fetch_one(&mut **tx)
    .await
}

/// An event as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct EventObject {
    pub id: Id<OutboxEvent>,
    /// The event's type, such as `message.sent`. New types may be added.
    #[serde(rename = "type")]
    #[schema(value_type = EventType)]
    pub kind: String,
    /// What consumers receive.
    pub data: Value,
    /// True for an event created on request (`POST /events`) rather than by a real change.
    pub synthetic: bool,
    /// The one endpoint a synthetic event is addressed to, if any.
    pub webhook_endpoint_id: Option<Id<WebhookEndpoint>>,
    pub created_at: Timestamp,
}

struct EventRow {
    id: Id<OutboxEvent>,
    kind: String,
    payload: Value,
    created_at: Timestamp,
}

impl From<EventRow> for EventObject {
    fn from(row: EventRow) -> Self {
        Self {
            id: row.id,
            synthetic: row
                .payload
                .get("synthetic")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            webhook_endpoint_id: addressed(&row.payload).map(Id::from_uuid),
            data: row.payload.get("data").cloned().unwrap_or(Value::Null),
            kind: row.kind,
            created_at: row.created_at,
        }
    }
}

/// The endpoint a synthetic event is addressed to.
fn addressed(payload: &Value) -> Option<Uuid> {
    payload
        .get("webhook_endpoint_id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse().ok())
}

/// Reads one event of `workspace`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<OutboxEvent>,
) -> Result<Option<EventObject>, sqlx::Error> {
    let row = sqlx::query_as!(
        EventRow,
        r#"SELECT id AS "id: Id<OutboxEvent>", type AS kind, payload, created_at AS "created_at: Timestamp"
             FROM outbox_events WHERE workspace_id = $1 AND id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(EventObject::from))
}

/// The filters of the event list.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EventFilters {
    /// Only events of this type.
    pub kind: Option<String>,
    /// Only events after this one (a stream's position).
    pub after: Option<Uuid>,
}

/// One page of `workspace`'s events in id order (time order), after `cursor` when given.
/// Fetches `limit` rows; the caller asks for one more than it shows.
///
/// # Errors
///
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &EventFilters,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<EventObject>, sqlx::Error> {
    let rows = if ascending {
        sqlx::query_as!(
            EventRow,
            r#"SELECT id AS "id: Id<OutboxEvent>", type AS kind, payload, created_at AS "created_at: Timestamp"
                 FROM outbox_events
                WHERE workspace_id = $1 AND ($2::text IS NULL OR type = $2) AND ($3::uuid IS NULL OR id > $3)
                  AND ($4::uuid IS NULL OR id > $4)
                ORDER BY id LIMIT $5"#,
            workspace.uuid(),
            filters.kind,
            filters.after,
            cursor,
            limit,
        )
        .fetch_all(&mut **tx)
        .await?
    } else {
        sqlx::query_as!(
            EventRow,
            r#"SELECT id AS "id: Id<OutboxEvent>", type AS kind, payload, created_at AS "created_at: Timestamp"
                 FROM outbox_events
                WHERE workspace_id = $1 AND ($2::text IS NULL OR type = $2) AND ($3::uuid IS NULL OR id > $3)
                  AND ($4::uuid IS NULL OR id < $4)
                ORDER BY id DESC LIMIT $5"#,
            workspace.uuid(),
            filters.kind,
            filters.after,
            cursor,
            limit,
        )
        .fetch_all(&mut **tx)
        .await?
    };
    Ok(rows.into_iter().map(EventObject::from).collect())
}

/// Counts `workspace`'s events matching `filters`, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &EventFilters,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM outbox_events
                WHERE workspace_id = $1 AND ($2::text IS NULL OR type = $2) AND ($3::uuid IS NULL OR id > $3)
                LIMIT $4) counted"#,
        workspace.uuid(),
        filters.kind,
        filters.after,
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// Records a synthetic event of `kind` with sample data of the type's shape (fresh ids, fixed
/// values), addressed to `endpoint` when given. The relay publishes it like any other event;
/// that is how the CLI triggers an event and how an endpoint is tested.
///
/// # Errors
///
/// The database refused the row.
pub async fn record_synthetic(
    tx: &mut Tx,
    workspace: WorkspaceId,
    kind: EventType,
    endpoint: Option<Id<WebhookEndpoint>>,
) -> Result<Id<OutboxEvent>, sqlx::Error> {
    let (subject_type, subject_id, data) = sample(kind, workspace, endpoint);
    let mut payload = json!({ "data": data, "synthetic": true });
    if let (Some(endpoint), Some(object)) = (endpoint, payload.as_object_mut()) {
        object.insert(
            "webhook_endpoint_id".to_owned(),
            Value::String(endpoint.uuid().to_string()),
        );
    }
    insert(tx, workspace, kind, subject_type, subject_id, &payload).await
}

/// Sample data for a synthetic event: the shape consumers receive for that type.
fn sample(
    kind: EventType,
    workspace: WorkspaceId,
    endpoint: Option<Id<WebhookEndpoint>>,
) -> (&'static str, Uuid, Value) {
    let now = crate::process::now();
    match kind {
        EventType::MessageQueued | EventType::MessageCancelled => {
            let message = Id::<Message>::new();
            (
                "message",
                message.uuid(),
                json!({ "message_id": message, "occurred_at": now }),
            )
        }
        EventType::MessageSent | EventType::MessageFailed | EventType::MessageUncertain => {
            let message = Id::<Message>::new();
            let category = if kind == EventType::MessageFailed {
                json!("mailbox_unavailable")
            } else {
                Value::Null
            };
            (
                "message",
                message.uuid(),
                json!({ "message_id": message, "attempt_id": Id::<Attempt>::new(), "occurred_at": now, "category": category }),
            )
        }
        EventType::MessageSnippetsFallback => {
            let message = Id::<Message>::new();
            (
                "message",
                message.uuid(),
                json!({ "message_id": message, "enrollment_id": Id::<Enrollment>::new(),
                        "campaign_id": Id::<Campaign>::new(), "step_id": Id::<Step>::new(), "reason": "deadline" }),
            )
        }
        EventType::DeliveryEventRecorded => {
            let event = Id::<DeliveryEvent>::new();
            let data = json!({ "delivery_event_id": event, "message_id": Id::<Message>::new(), "kind": "delivered",
                               "source": "provider_webhook", "confidence": "authenticated", "recipient": "person@example.com" });
            ("delivery_event", event.uuid(), data)
        }
        EventType::InboundMessageReceived => {
            let inbound = Id::<InboundMessage>::new();
            (
                "inbound_message",
                inbound.uuid(),
                json!({ "inbound_message_id": inbound, "classification": "human_reply", "thread_id": Id::<Thread>::new() }),
            )
        }
        EventType::EnrollmentStopped | EventType::EnrollmentCompleted => {
            let enrollment = Id::<Enrollment>::new();
            let reason = if kind == EventType::EnrollmentStopped {
                "replied"
            } else {
                "completed"
            };
            (
                "enrollment",
                enrollment.uuid(),
                json!({ "enrollment_id": enrollment, "reason": reason }),
            )
        }
        EventType::CampaignStatusChanged => {
            let campaign = Id::<Campaign>::new();
            (
                "campaign",
                campaign.uuid(),
                json!({ "campaign_id": campaign, "status": "active" }),
            )
        }
        EventType::ConnectionHealthChanged => {
            let connection = Id::<Connection>::new();
            (
                "connection",
                connection.uuid(),
                json!({ "connection_id": connection, "status": "active", "status_detail": null }),
            )
        }
        EventType::ImportCompleted => {
            let import = Id::<Import>::new();
            (
                "import",
                import.uuid(),
                json!({ "import_id": import, "total": 0, "imported": 0, "skipped": 0, "invalid": 0 }),
            )
        }
        EventType::ExportCompleted => {
            let export = Id::<Export>::new();
            (
                "export",
                export.uuid(),
                json!({ "export_id": export, "rows": 0 }),
            )
        }
        EventType::SuppressionCreated => {
            let suppression = Id::<Suppression>::new();
            (
                "suppression",
                suppression.uuid(),
                json!({ "suppression_id": suppression, "reason": "manual", "source": "manual" }),
            )
        }
        EventType::AiBudgetWarning | EventType::AiBudgetExceeded => {
            let month = now
                .0
                .to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%Y-%m")
                .to_string();
            let spent: u64 = if kind == EventType::AiBudgetWarning {
                4_000_000
            } else {
                5_000_000
            };
            (
                "workspace",
                workspace.uuid(),
                json!({ "month": month, "spent_micros": spent, "budget_micros": 5_000_000 }),
            )
        }
        EventType::EndpointTest => {
            let endpoint = endpoint.unwrap_or_default();
            (
                "webhook_endpoint",
                endpoint.uuid(),
                json!({ "webhook_endpoint_id": endpoint }),
            )
        }
        EventType::WebhookEndpointDisabled => {
            let endpoint = endpoint.unwrap_or_default();
            (
                "webhook_endpoint",
                endpoint.uuid(),
                json!({ "webhook_endpoint_id": endpoint, "disabled_reason": "manual", "last_error": null }),
            )
        }
    }
}

/// `outbox.relay`: publishes unpublished events to webhook endpoints (see the module).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Relay {}

impl Job for Relay {
    const KIND: &'static str = "outbox.relay";
    const QUEUE: Queue = Queue::Maintenance;
    const EFFECT: Effect = Effect::Idempotent;
    const CLASS: Class = Class::FanOut;

    fn unique_key(&self) -> Option<String> {
        Some("singleton".to_owned())
    }

    fn schedule() -> Option<(&'static str, Self)> {
        Some(("*/5 * * * * *", Self {}))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let mut directory = cx.directory().await?;
        let oldest = sqlx::query_scalar!(
            "SELECT id FROM outbox_events WHERE published_at IS NULL ORDER BY id LIMIT 1"
        )
        .fetch_optional(&mut *directory)
        .await?;
        let workspaces = sqlx::query_scalar!(
            r#"SELECT DISTINCT workspace_id AS "workspace_id!" FROM outbox_events WHERE published_at IS NULL LIMIT $1"#,
            WORKSPACES_PER_RUN,
        )
        .fetch_all(&mut *directory)
        .await?;
        directory.commit().await?;
        OUTBOX_LAG.record(
            oldest.map_or(0.0, |id| age_seconds(id, crate::process::now())),
            &[],
        );

        let mut published = cx
            .progress()
            .and_then(|progress| progress.get("published"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        for workspace in workspaces.into_iter().map(WorkspaceId::trusted) {
            loop {
                if cx.should_yield() {
                    return Ok(Outcome::Yield {
                        after: Duration::ZERO,
                    });
                }
                let (events, deliveries) = publish(cx, workspace, published).await?;
                published = published.saturating_add(events);
                if deliveries > 0 {
                    jobs::wake(cx.db(), Queue::Webhooks).await;
                }
                if events < u64::try_from(CHUNK).unwrap_or(u64::MAX) {
                    break;
                }
            }
        }
        Ok(Outcome::Done)
    }
}

/// Publishes one chunk of `workspace`'s unpublished events; returns the events published and the
/// deliveries created.
async fn publish(
    cx: &mut JobContext,
    workspace: WorkspaceId,
    published: u64,
) -> Result<(u64, usize), JobError> {
    let mut chunk = cx.begin_in(workspace).await?;
    let events = sqlx::query!(
        r#"SELECT id, type AS kind, payload ->> 'webhook_endpoint_id' AS "addressed"
             FROM outbox_events WHERE workspace_id = $1 AND published_at IS NULL
            ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED"#,
        workspace.uuid(),
        CHUNK,
    )
    .fetch_all(&mut **chunk.tx())
    .await?;
    if events.is_empty() {
        return Ok((0, 0));
    }
    let endpoints = sqlx::query!(
        "SELECT id, event_types FROM webhook_endpoints WHERE workspace_id = $1 AND enabled",
        workspace.uuid(),
    )
    .fetch_all(&mut **chunk.tx())
    .await?;

    let payloads: Vec<(Uuid, Value)> = sqlx::query_as(
        "SELECT id, payload FROM outbox_events WHERE workspace_id=$1 AND id=ANY($2)",
    )
    .bind(workspace.uuid())
    .bind(events.iter().map(|e| e.id).collect::<Vec<_>>())
    .fetch_all(&mut **chunk.tx())
    .await?;
    let configurations: Vec<(Uuid, sqlx::types::Json<crate::domain::webhooks::Filters>)> =
        sqlx::query_as(
            "SELECT id, filters FROM webhook_endpoints WHERE workspace_id=$1 AND enabled",
        )
        .bind(workspace.uuid())
        .fetch_all(&mut **chunk.tx())
        .await?;
    let (mut ids, mut endpoint_ids, mut event_ids) = (Vec::new(), Vec::new(), Vec::new());
    for event in &events {
        let addressed = event
            .addressed
            .as_deref()
            .and_then(|id| id.parse::<Uuid>().ok());
        for endpoint in &endpoints {
            let reaches = match addressed {
                Some(target) => target == endpoint.id,
                None => {
                    endpoint.event_types.contains(&event.kind)
                        && configurations
                            .iter()
                            .find(|(id, _)| *id == endpoint.id)
                            .is_some_and(|(_, filters)| {
                                payloads
                                    .iter()
                                    .find(|(id, _)| *id == event.id)
                                    .and_then(|(_, p)| p.get("data"))
                                    .is_some_and(|data| filters.matches(data))
                            })
                }
            };
            if reaches {
                ids.push(deliver::new_delivery_id(event.id));
                endpoint_ids.push(endpoint.id);
                event_ids.push(event.id);
            }
        }
    }
    let created = sqlx::query_scalar!(
        "INSERT INTO webhook_deliveries (workspace_id, id, endpoint_id, event_id)
         SELECT $1, delivery.id, delivery.endpoint_id, delivery.event_id
           FROM unnest($2::uuid[], $3::uuid[], $4::uuid[]) AS delivery(id, endpoint_id, event_id)
         ON CONFLICT DO NOTHING
         RETURNING id",
        workspace.uuid(),
        &ids,
        &endpoint_ids,
        &event_ids,
    )
    .fetch_all(&mut **chunk.tx())
    .await?;
    let published_ids: Vec<Uuid> = events.iter().map(|event| event.id).collect();
    sqlx::query!(
        "UPDATE outbox_events SET published_at = now() WHERE workspace_id = $1 AND id = ANY($2)",
        workspace.uuid(),
        &published_ids,
    )
    .execute(&mut **chunk.tx())
    .await?;
    let deliveries: Vec<Deliver> = created
        .iter()
        .map(|id| Deliver {
            delivery: Id::<WebhookDelivery>::from_uuid(*id),
        })
        .collect();
    jobs::enqueue_many(chunk.tx(), workspace, &deliveries, None).await?;
    let count = u64::try_from(events.len()).unwrap_or(u64::MAX);
    cx.checkpoint(
        chunk,
        json!({ "published": published.saturating_add(count) }),
    )
    .await?;
    Ok((count, deliveries.len()))
}

/// Seconds between the instant a UUIDv7 encodes (its first 48 bits are unix milliseconds) and
/// `now`.
fn age_seconds(id: Uuid, now: Timestamp) -> f64 {
    let millis = deliver::uuid_millis(id);
    let now = u64::try_from(now.0.as_millisecond()).unwrap_or_default();
    let age = Duration::from_millis(now.saturating_sub(millis));
    age.as_secs_f64()
}

/// Freezes routing dimensions with the business fact so delivery does not depend on later rows.
async fn routing(
    tx: &mut Tx,
    workspace: WorkspaceId,
    event: &mut Event,
) -> Result<(), sqlx::Error> {
    let message = event
        .data
        .get("message_id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse::<Id<Message>>().ok())
        .map(|id| id.uuid())
        .or_else(|| (event.subject_type == "message").then_some(event.subject_id));
    let dimensions: Option<(Uuid, Option<Uuid>)> = if let Some(message) = message {
        sqlx::query_as(
            "SELECT connection_id, campaign_id FROM messages WHERE workspace_id=$1 AND id=$2",
        )
        .bind(workspace.uuid())
        .bind(message)
        .fetch_optional(&mut **tx)
        .await?
    } else if event.subject_type == "inbound_message" {
        sqlx::query_as("SELECT i.connection_id, t.campaign_id FROM inbound_messages i LEFT JOIN threads t ON t.workspace_id=i.workspace_id AND t.id=i.thread_id WHERE i.workspace_id=$1 AND i.id=$2")
            .bind(workspace.uuid()).bind(event.subject_id).fetch_optional(&mut **tx).await?
    } else {
        None
    };
    if let Some((connection, campaign)) = dimensions
        && let Some(data) = event.data.as_object_mut()
    {
        data.insert(
            "connection_id".to_owned(),
            json!(Id::<Connection>::from_uuid(connection)),
        );
        data.insert(
            "campaign_id".to_owned(),
            json!(campaign.map(Id::<Campaign>::from_uuid)),
        );
    }
    Ok(())
}
