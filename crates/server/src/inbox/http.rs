//! The inbox resources under `/v1`: `threads` and `inbound_messages`.
//!
//! Reads need `inbox:read`; updates and reviews need `inbox:write`. Both resources can be
//! updated, so each carries `version` (answered as `ETag` when one is returned alone), and their
//! updates take an optional `If-Match`, checked under the row's lock.
//!
//! # Threads
//!
//! A thread is one conversation with one person through one sender identity: our outbound
//! messages and the inbound messages that answer them. It shows its participants (the identity's
//! address, the addresses our messages went to and the senders of what came back, at most
//! [`PARTICIPANTS`]), its latest activity, whether something unread arrived, its `status` and its
//! latest message; retrieving it adds its latest [`THREAD_MESSAGES`] messages, outbound and
//! inbound, oldest first, with `has_more`.
//!
//! `status` is `open`, `snoozed` (until `snoozed_until`) or `archived`. A snooze ends by itself:
//! once `snoozed_until` passes, the thread reads and filters as `open` without anything waking
//! it; a person's answer arriving earlier opens it at once.
//!
//! # Inbound messages
//!
//! What the inbox read: headers, a bounded excerpt of the text, the classification with who
//! decided it (`rules`, `manual`, `ai`) and the evidence that decided, the sentiment, and the
//! review: whether a person was asked to decide, what confirming would apply, and the decision.
//! An update corrects `classification` and `sentiment` by hand: it marks them `manual` and bumps
//! the revision, so a later AI verdict never overrides it. `POST /inbound_messages/{id}/review`
//! confirms (applies the proposal: a suppression, or moving the person to the new address a
//! notice gave) or dismisses; nothing a person wrote is ever applied without that decision.

use std::collections::HashMap;

use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::db::Tx;
use crate::delivery::http::Address;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{
    Campaign, Connection, Id, InboundMessage, Message, Person, SenderIdentity, Thread, WorkspaceId,
};
use crate::domain::inbox::{Classification, ClassificationSource, ReviewProposal, Sentiment};
use crate::domain::messages::Direction;
use crate::domain::scope::Scope;
use crate::domain::suppressions::{Reason as SuppressionReason, Source as SuppressedBy};
use crate::domain::time::Timestamp;
use crate::http::AppState;
use crate::http::extract::{Json, Path, Query};
use crate::http::versioning::{self, IfMatch, Tagged};
use crate::identity::authority::Principal;
use crate::pagination::{self, COUNT_CAP, Include, ListQuery, Order, Page, PageParams, Sort};
use crate::people::suppressions::{self, NewSuppression};
use crate::people::{self, PersonChanges};
use crate::problem::{self, ApiResult, Problem};

/// The most participants a thread shows.
pub const PARTICIPANTS: usize = 20;
/// The most messages a retrieved thread shows.
pub const THREAD_MESSAGES: i64 = 50;
/// The most characters of an outbound message's text a thread shows.
const TEXT_CHARS: i32 = 2_000;

/// The routes of the inbox.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_threads))
        .routes(routes!(retrieve_thread, update_thread))
        .routes(routes!(list_inbound))
        .routes(routes!(retrieve_inbound, update_inbound))
        .routes(routes!(review_inbound))
}

// ───────────────────────────── threads ─────────────────────────────

/// A thread's status as shown: `snoozed` turns `open` once its time passes.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Open,
    Snoozed,
    Archived,
}

impl ThreadStatus {
    fn as_str(self) -> &'static str {
        self.into()
    }
}

/// One message of a thread, outbound or inbound.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ThreadMessage {
    pub direction: Direction,
    /// A message id (`msg_…`) for outbound mail, an inbound message id (`inb_…`) for inbound.
    pub id: String,
    pub from: Option<String>,
    /// The addresses an outbound message went to; empty for inbound mail.
    pub to: Vec<String>,
    pub subject: Option<String>,
    /// When it was sent (or created, while it waits) or received.
    pub at: Timestamp,
    /// An outbound message's state; null for inbound mail.
    #[schema(value_type = Option<crate::domain::messages::State>)]
    pub state: Option<String>,
    /// An inbound message's classification.
    pub classification: Option<Classification>,
    /// The start of its text, when it has one of its own.
    pub text: Option<String>,
}

/// A thread's latest messages.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ThreadMessages {
    /// Oldest first.
    #[schema(max_items = 50)]
    pub data: Vec<ThreadMessage>,
    /// Older messages exist.
    pub has_more: bool,
}

/// A conversation.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ThreadObject {
    pub id: Id<Thread>,
    pub status: ThreadStatus,
    /// Until when a `snoozed` thread sleeps.
    pub snoozed_until: Option<Timestamp>,
    pub subject: Option<String>,
    pub person_id: Option<Id<Person>>,
    pub campaign_id: Option<Id<Campaign>>,
    pub sender_identity_id: Id<SenderIdentity>,
    pub connection_id: Id<Connection>,
    /// The addresses taking part.
    pub participants: Vec<String>,
    /// Something arrived that nobody marked read.
    pub unread: bool,
    pub last_activity_at: Timestamp,
    pub last_message: Option<ThreadMessage>,
    /// On retrieve only: the latest messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<ThreadMessages>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub version: i64,
}

/// The filters of the thread list.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ThreadFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub person_id: Option<Id<Person>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub connection_id: Option<Id<Connection>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ThreadStatus>,
    /// Threads with an inbound message of this classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<Classification>,
}

struct ThreadRow {
    id: Id<Thread>,
    status: String,
    snoozed_until: Option<Timestamp>,
    subject: Option<String>,
    person_id: Option<Id<Person>>,
    campaign_id: Option<Id<Campaign>>,
    sender_identity_id: Id<SenderIdentity>,
    connection_id: Id<Connection>,
    identity_email: String,
    unread: bool,
    last_activity_at: Timestamp,
    created_at: Timestamp,
    updated_at: Timestamp,
}

/// One message of a thread as read.
struct EntryRow {
    thread_id: Uuid,
    outbound: bool,
    id: Uuid,
    from_email: Option<String>,
    to_addresses: Vec<String>,
    subject: Option<String>,
    at: Timestamp,
    state: Option<String>,
    classification: Option<String>,
    text: Option<String>,
}

impl From<EntryRow> for ThreadMessage {
    fn from(row: EntryRow) -> Self {
        Self {
            direction: if row.outbound {
                Direction::Outbound
            } else {
                Direction::Inbound
            },
            id: if row.outbound {
                Id::<Message>::from_uuid(row.id).to_string()
            } else {
                Id::<InboundMessage>::from_uuid(row.id).to_string()
            },
            from: row.from_email,
            to: row.to_addresses,
            subject: row.subject,
            at: row.at,
            state: row.state,
            classification: row
                .classification
                .and_then(|classification| classification.parse().ok()),
            text: row.text,
        }
    }
}

/// Which threads a read walks: its sort, where the page starts (the instant of the activity
/// sort, and the id that breaks its ties or is the whole position of the `id` sort) and its
/// direction.
#[derive(Debug, Clone, Copy)]
struct Walk {
    sort: Sort,
    after_at: Option<Timestamp>,
    after_id: Option<Uuid>,
    ascending: bool,
}

impl Walk {
    /// The walk of one page of a list.
    fn of(params: &PageParams) -> Self {
        Self {
            sort: params.sort(),
            after_at: params.after_at(),
            after_id: params.after_id(),
            ascending: params.ascending(),
        }
    }

    /// Newest first by id, from the start: the walk of a read by id.
    const ONE: Self = Self {
        sort: Sort::Id,
        after_at: None,
        after_id: None,
        ascending: false,
    };
}

/// `workspace`'s threads matching `filters` (one thread with `only`), in the order of `walk`,
/// with their participants and latest message.
async fn threads(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &ThreadFilters,
    only: Option<Id<Thread>>,
    walk: Walk,
    limit: i64,
) -> Result<Vec<ThreadObject>, sqlx::Error> {
    let f = filters;
    let status = f.status.map(ThreadStatus::as_str);
    let classification = f.classification.map(Classification::as_str);
    let cursor = walk.after_id;
    // One statement per order and direction, so each walks its index in its own order: the
    // primary key for the `id` sort, `threads_by_activity` for the activity sort (whose column
    // is never NULL, and whose ties the id breaks). A status filter matches the stored status
    // first (a snooze that ended is still stored `snoozed`, so `open` reads both), which lets the
    // activity walk of a status use `threads_inbox` (workspace, status, activity, id); the second
    // condition then applies what each status means now.
    let rows = match (walk.sort, walk.ascending) {
        (Sort::LastActivityAt, true) => {
            sqlx::query_as!(
                ThreadRow,
                r#"SELECT t.id AS "id: Id<Thread>",
                          CASE WHEN t.status = 'snoozed' AND t.snoozed_until <= now() THEN 'open' ELSE t.status END AS "status!",
                          CASE WHEN t.status = 'snoozed' AND t.snoozed_until > now() THEN t.snoozed_until END AS "snoozed_until: Timestamp",
                          t.subject, t.person_id AS "person_id: Id<Person>", t.campaign_id AS "campaign_id: Id<Campaign>",
                          t.sender_identity_id AS "sender_identity_id: Id<SenderIdentity>",
                          i.connection_id AS "connection_id: Id<Connection>", i.email AS identity_email, t.unread,
                          t.last_activity_at AS "last_activity_at: Timestamp", t.created_at AS "created_at: Timestamp",
                          t.updated_at AS "updated_at: Timestamp"
                     FROM threads t JOIN sender_identities i ON i.workspace_id = t.workspace_id AND i.id = t.sender_identity_id
                    WHERE t.workspace_id = $1 AND ($2::uuid IS NULL OR t.id = $2)
                      AND ($9::timestamptz IS NULL OR (t.last_activity_at, t.id) > ($9::timestamptz, $3::uuid))
                      AND ($4::uuid IS NULL OR t.person_id = $4) AND ($5::uuid IS NULL OR i.connection_id = $5)
                      AND ($6::text IS NULL OR ((t.status = $6 OR ($6 = 'open' AND t.status = 'snoozed'))
                                                AND $6 = CASE WHEN t.status = 'snoozed' AND t.snoozed_until <= now() THEN 'open' ELSE t.status END))
                      AND ($7::text IS NULL OR EXISTS (SELECT 1 FROM inbound_messages m
                                                        WHERE m.workspace_id = t.workspace_id AND m.thread_id = t.id
                                                          AND m.classification = $7 AND m.deleted_at IS NULL))
                    ORDER BY t.last_activity_at, t.id LIMIT $8"#,
                workspace.uuid(),
                only.map(|id| id.uuid()),
                cursor,
                f.person_id.map(|id| id.uuid()),
                f.connection_id.map(|id| id.uuid()),
                status,
                classification,
                limit,
                walk.after_at as _,
            )
            .fetch_all(&mut **tx)
            .await?
        }
        (Sort::LastActivityAt, false) => {
            sqlx::query_as!(
                ThreadRow,
                r#"SELECT t.id AS "id: Id<Thread>",
                          CASE WHEN t.status = 'snoozed' AND t.snoozed_until <= now() THEN 'open' ELSE t.status END AS "status!",
                          CASE WHEN t.status = 'snoozed' AND t.snoozed_until > now() THEN t.snoozed_until END AS "snoozed_until: Timestamp",
                          t.subject, t.person_id AS "person_id: Id<Person>", t.campaign_id AS "campaign_id: Id<Campaign>",
                          t.sender_identity_id AS "sender_identity_id: Id<SenderIdentity>",
                          i.connection_id AS "connection_id: Id<Connection>", i.email AS identity_email, t.unread,
                          t.last_activity_at AS "last_activity_at: Timestamp", t.created_at AS "created_at: Timestamp",
                          t.updated_at AS "updated_at: Timestamp"
                     FROM threads t JOIN sender_identities i ON i.workspace_id = t.workspace_id AND i.id = t.sender_identity_id
                    WHERE t.workspace_id = $1 AND ($2::uuid IS NULL OR t.id = $2)
                      AND ($9::timestamptz IS NULL OR (t.last_activity_at, t.id) < ($9::timestamptz, $3::uuid))
                      AND ($4::uuid IS NULL OR t.person_id = $4) AND ($5::uuid IS NULL OR i.connection_id = $5)
                      AND ($6::text IS NULL OR ((t.status = $6 OR ($6 = 'open' AND t.status = 'snoozed'))
                                                AND $6 = CASE WHEN t.status = 'snoozed' AND t.snoozed_until <= now() THEN 'open' ELSE t.status END))
                      AND ($7::text IS NULL OR EXISTS (SELECT 1 FROM inbound_messages m
                                                        WHERE m.workspace_id = t.workspace_id AND m.thread_id = t.id
                                                          AND m.classification = $7 AND m.deleted_at IS NULL))
                    ORDER BY t.last_activity_at DESC, t.id DESC LIMIT $8"#,
                workspace.uuid(),
                only.map(|id| id.uuid()),
                cursor,
                f.person_id.map(|id| id.uuid()),
                f.connection_id.map(|id| id.uuid()),
                status,
                classification,
                limit,
                walk.after_at as _,
            )
            .fetch_all(&mut **tx)
            .await?
        }
        (Sort::Id | Sort::UpdatedAt, true) => {
            sqlx::query_as!(
                ThreadRow,
                r#"SELECT t.id AS "id: Id<Thread>",
                          CASE WHEN t.status = 'snoozed' AND t.snoozed_until <= now() THEN 'open' ELSE t.status END AS "status!",
                          CASE WHEN t.status = 'snoozed' AND t.snoozed_until > now() THEN t.snoozed_until END AS "snoozed_until: Timestamp",
                          t.subject, t.person_id AS "person_id: Id<Person>", t.campaign_id AS "campaign_id: Id<Campaign>",
                          t.sender_identity_id AS "sender_identity_id: Id<SenderIdentity>",
                          i.connection_id AS "connection_id: Id<Connection>", i.email AS identity_email, t.unread,
                          t.last_activity_at AS "last_activity_at: Timestamp", t.created_at AS "created_at: Timestamp",
                          t.updated_at AS "updated_at: Timestamp"
                     FROM threads t JOIN sender_identities i ON i.workspace_id = t.workspace_id AND i.id = t.sender_identity_id
                    WHERE t.workspace_id = $1 AND ($2::uuid IS NULL OR t.id = $2) AND ($3::uuid IS NULL OR t.id > $3)
                      AND ($4::uuid IS NULL OR t.person_id = $4) AND ($5::uuid IS NULL OR i.connection_id = $5)
                      AND ($6::text IS NULL OR ((t.status = $6 OR ($6 = 'open' AND t.status = 'snoozed'))
                                                AND $6 = CASE WHEN t.status = 'snoozed' AND t.snoozed_until <= now() THEN 'open' ELSE t.status END))
                      AND ($7::text IS NULL OR EXISTS (SELECT 1 FROM inbound_messages m
                                                        WHERE m.workspace_id = t.workspace_id AND m.thread_id = t.id
                                                          AND m.classification = $7 AND m.deleted_at IS NULL))
                    ORDER BY t.id LIMIT $8"#,
                workspace.uuid(),
                only.map(|id| id.uuid()),
                cursor,
                f.person_id.map(|id| id.uuid()),
                f.connection_id.map(|id| id.uuid()),
                status,
                classification,
                limit,
            )
            .fetch_all(&mut **tx)
            .await?
        }
        (Sort::Id | Sort::UpdatedAt, false) => {
            sqlx::query_as!(
                ThreadRow,
                r#"SELECT t.id AS "id: Id<Thread>",
                          CASE WHEN t.status = 'snoozed' AND t.snoozed_until <= now() THEN 'open' ELSE t.status END AS "status!",
                          CASE WHEN t.status = 'snoozed' AND t.snoozed_until > now() THEN t.snoozed_until END AS "snoozed_until: Timestamp",
                          t.subject, t.person_id AS "person_id: Id<Person>", t.campaign_id AS "campaign_id: Id<Campaign>",
                          t.sender_identity_id AS "sender_identity_id: Id<SenderIdentity>",
                          i.connection_id AS "connection_id: Id<Connection>", i.email AS identity_email, t.unread,
                          t.last_activity_at AS "last_activity_at: Timestamp", t.created_at AS "created_at: Timestamp",
                          t.updated_at AS "updated_at: Timestamp"
                     FROM threads t JOIN sender_identities i ON i.workspace_id = t.workspace_id AND i.id = t.sender_identity_id
                    WHERE t.workspace_id = $1 AND ($2::uuid IS NULL OR t.id = $2) AND ($3::uuid IS NULL OR t.id < $3)
                      AND ($4::uuid IS NULL OR t.person_id = $4) AND ($5::uuid IS NULL OR i.connection_id = $5)
                      AND ($6::text IS NULL OR ((t.status = $6 OR ($6 = 'open' AND t.status = 'snoozed'))
                                                AND $6 = CASE WHEN t.status = 'snoozed' AND t.snoozed_until <= now() THEN 'open' ELSE t.status END))
                      AND ($7::text IS NULL OR EXISTS (SELECT 1 FROM inbound_messages m
                                                        WHERE m.workspace_id = t.workspace_id AND m.thread_id = t.id
                                                          AND m.classification = $7 AND m.deleted_at IS NULL))
                    ORDER BY t.id DESC LIMIT $8"#,
                workspace.uuid(),
                only.map(|id| id.uuid()),
                cursor,
                f.person_id.map(|id| id.uuid()),
                f.connection_id.map(|id| id.uuid()),
                status,
                classification,
                limit,
            )
            .fetch_all(&mut **tx)
            .await?
        }
    };
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id.uuid()).collect();
    let mut taking = HashMap::<Uuid, Vec<String>>::new();
    for row in sqlx::query!(
        r#"SELECT thread_id AS "thread_id!", address AS "address!" FROM (
               SELECT m.thread_id, unnest(m.to_addresses || m.cc) AS address
                 FROM messages m WHERE m.workspace_id = $1 AND m.thread_id = ANY($2)
               UNION
               SELECT i.thread_id, i.from_email FROM inbound_messages i
                WHERE i.workspace_id = $1 AND i.thread_id = ANY($2) AND i.from_email IS NOT NULL AND i.deleted_at IS NULL
           ) p ORDER BY thread_id, address"#,
        workspace.uuid(),
        &ids,
    )
    .fetch_all(&mut **tx)
    .await?
    {
        taking.entry(row.thread_id).or_default().push(row.address);
    }
    let mut latest: HashMap<Uuid, ThreadMessage> = entries(tx, workspace, &ids, None)
        .await?
        .into_iter()
        .map(|row| (row.thread_id, row.into()))
        .collect();
    Ok(rows
        .into_iter()
        .map(|row| {
            let mut participants = vec![row.identity_email];
            let mut seen: Vec<String> = participants.iter().map(|a| key(a)).collect();
            for address in taking.remove(&row.id.uuid()).unwrap_or_default() {
                if participants.len() >= PARTICIPANTS {
                    break;
                }
                let k = key(&address);
                if !seen.contains(&k) {
                    seen.push(k);
                    participants.push(address);
                }
            }
            ThreadObject {
                id: row.id,
                status: row.status.parse().unwrap_or(ThreadStatus::Open),
                snoozed_until: row.snoozed_until,
                subject: row.subject,
                person_id: row.person_id,
                campaign_id: row.campaign_id,
                sender_identity_id: row.sender_identity_id,
                connection_id: row.connection_id,
                participants,
                unread: row.unread,
                last_activity_at: row.last_activity_at,
                last_message: latest.remove(&row.id.uuid()),
                messages: None,
                created_at: row.created_at,
                updated_at: row.updated_at,
                version: versioning::of(row.updated_at),
            }
        })
        .collect())
}

/// An address's comparison key: the normalised address, or the text as it is.
fn key(address: &str) -> String {
    EmailAddress::parse(address).map_or_else(|_| address.to_owned(), |address| address.key())
}

/// The latest message of each thread of `ids`, or with `limit`, the latest `limit` messages of
/// the one thread in `ids`, newest first.
async fn entries(
    tx: &mut Tx,
    workspace: WorkspaceId,
    ids: &[Uuid],
    limit: Option<i64>,
) -> Result<Vec<EntryRow>, sqlx::Error> {
    sqlx::query_as!(
        EntryRow,
        r#"WITH e AS (
               SELECT m.thread_id AS thread_id, true AS outbound, m.id, m.from_email, m.to_addresses, m.subject,
                      coalesce(m.sent_at, m.created_at) AS at, m.state, NULL::text AS classification,
                      left(m.text_body, $4) AS text
                 FROM messages m WHERE m.workspace_id = $1 AND m.thread_id = ANY($2)
               UNION ALL
               SELECT i.thread_id, false, i.id, i.from_email, '{}'::text[], i.subject, i.received_at, NULL, i.classification,
                      i.body_text
                 FROM inbound_messages i WHERE i.workspace_id = $1 AND i.thread_id = ANY($2) AND i.deleted_at IS NULL)
           SELECT thread_id AS "thread_id!", outbound AS "outbound!", id AS "id!", from_email, to_addresses AS "to_addresses!",
                  subject, at AS "at!: Timestamp", state, classification, text
             FROM (SELECT e.*, row_number() OVER (PARTITION BY e.thread_id ORDER BY e.at DESC, e.id DESC) AS n FROM e) r
            WHERE n <= coalesce($3::bigint, 1)
            ORDER BY thread_id, at DESC, id DESC"#,
        workspace.uuid(),
        ids,
        limit,
        TEXT_CHARS,
    )
    .fetch_all(&mut **tx)
    .await
}

/// One thread with its latest messages.
async fn thread(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Thread>,
) -> Result<Option<ThreadObject>, sqlx::Error> {
    let Some(mut thread) = threads(
        tx,
        workspace,
        &ThreadFilters::default(),
        Some(id),
        Walk::ONE,
        1,
    )
    .await?
    .pop() else {
        return Ok(None);
    };
    let mut rows = entries(tx, workspace, &[id.uuid()], Some(THREAD_MESSAGES + 1)).await?;
    let has_more = rows.len() > usize::try_from(THREAD_MESSAGES).unwrap_or(usize::MAX);
    rows.truncate(usize::try_from(THREAD_MESSAGES).unwrap_or(usize::MAX));
    rows.reverse();
    thread.messages = Some(ThreadMessages {
        data: rows.into_iter().map(ThreadMessage::from).collect(),
        has_more,
    });
    Ok(Some(thread))
}

/// List the workspace's threads, newest first by default.
///
/// `sort=last_activity_at` orders them by their latest message, sent or received, instead: the
/// inbox's order. A thread moves when a message arrives, so it can move between pages while
/// they are read.
#[utoipa::path(
    get,
    path = "/threads",
    tag = "Inbox",
    operation_id = "threads.list",
    params(
        ("limit" = Option<i64>, Query, description = "Threads per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("sort" = Option<String>, Query, description = "`id` (default, the creation order) or `last_activity_at` (the latest message, sent or received); ties are ordered by id."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("person_id" = Option<Id<Person>>, Query, description = "Threads with this person."),
        ("connection_id" = Option<Id<Connection>>, Query, description = "Threads of this connection's identities."),
        ("status" = Option<ThreadStatus>, Query, description = "Threads in this status (a snooze that ended is `open`)."),
        ("classification" = Option<Classification>, Query, description = "Threads with an inbound message of this classification."),
    ),
    responses(
        (status = 200, description = "A page of threads.", body = Page<ThreadObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `inbox:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_threads(
    principal: Principal,
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
    Query(filters): Query<ThreadFilters>,
) -> ApiResult<Json<Page<ThreadObject>>> {
    principal.require(Scope::InboxRead)?;
    let ws = principal.workspace;
    let params = PageParams::sorted(
        &state.keys,
        ws,
        "threads",
        &[Sort::LastActivityAt],
        &filters,
        &query,
    )?;
    let mut tx = state.db.begin_in(ws).await?;
    let rows = threads(
        &mut tx,
        ws,
        &filters,
        None,
        Walk::of(&params),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(count_threads(&mut tx, ws, &filters, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&state.keys, &params, rows, |thread| {
        params.position(thread.last_activity_at, thread.id.uuid())
    });
    Ok(Json(match total {
        Some(total) => page.with_total(total),
        None => page,
    }))
}

/// Counts `workspace`'s threads matching `filters`, up to `cap + 1` (so a capped count is told
/// from an exact one).
async fn count_threads(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &ThreadFilters,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    let f = filters;
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM threads t JOIN sender_identities i ON i.workspace_id = t.workspace_id AND i.id = t.sender_identity_id
                WHERE t.workspace_id = $1
                  AND ($2::uuid IS NULL OR t.person_id = $2) AND ($3::uuid IS NULL OR i.connection_id = $3)
                  AND ($4::text IS NULL OR ((t.status = $4 OR ($4 = 'open' AND t.status = 'snoozed'))
                                                AND $4 = CASE WHEN t.status = 'snoozed' AND t.snoozed_until <= now() THEN 'open' ELSE t.status END))
                  AND ($5::text IS NULL OR EXISTS (SELECT 1 FROM inbound_messages m
                                                    WHERE m.workspace_id = t.workspace_id AND m.thread_id = t.id
                                                      AND m.classification = $5 AND m.deleted_at IS NULL))
                LIMIT $6) counted"#,
        workspace.uuid(),
        f.person_id.map(|id| id.uuid()),
        f.connection_id.map(|id| id.uuid()),
        f.status.map(ThreadStatus::as_str),
        f.classification.map(Classification::as_str),
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// Retrieve a thread with its latest 50 messages, outbound and inbound, oldest first.
#[utoipa::path(
    get,
    path = "/threads/{id}",
    tag = "Inbox",
    operation_id = "threads.retrieve",
    params(("id" = Id<Thread>, Path, description = "The thread id (`thr_…`).")),
    responses(
        (status = 200, description = "The thread.", body = ThreadObject,
         headers(("ETag" = String, description = "The thread's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `inbox:read`."),
        (status = 404, description = "No such thread in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_thread(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Thread>>,
) -> ApiResult<Tagged<ThreadObject>> {
    principal.require(Scope::InboxRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let thread = thread(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("thread"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: thread.version,
        body: thread,
    })
}

/// The body of `PATCH /threads/{id}`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateThread {
    /// `open`, `snoozed` (with `snoozed_until`) or `archived`.
    #[garde(skip)]
    status: Option<ThreadStatus>,
    /// Until when a snoozed thread sleeps: required with `snoozed`, refused otherwise.
    #[garde(skip)]
    snoozed_until: Option<Timestamp>,
    /// `false` marks it read.
    #[garde(skip)]
    unread: Option<bool>,
}

/// Change a thread's status (open, snooze or archive it) or mark it read.
#[utoipa::path(
    patch,
    path = "/threads/{id}",
    tag = "Inbox",
    operation_id = "threads.update",
    params(("id" = Id<Thread>, Path, description = "The thread id (`thr_…`)."), IfMatch),
    request_body(content = UpdateThread, example = json!({"status": "snoozed", "snoozed_until": "2026-10-09T08:00:00Z"})),
    responses(
        (status = 200, description = "The thread.", body = ThreadObject,
         headers(("ETag" = String, description = "The thread's new `version`, quoted."))),
        (status = 400, description = "The body is not JSON, or `If-Match` is malformed (`invalid_request`)."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `inbox:write`."),
        (status = 404, description = "No such thread in this workspace."),
        (status = 412, description = "`If-Match` names a version that is no longer current (`precondition_failed`); nothing changed."),
        (status = 422, description = "The body is invalid: `snoozed` without a future `snoozed_until`, or `snoozed_until` without `snoozed`."),
    ),
    security(("bearer" = []))
)]
async fn update_thread(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Thread>>,
    if_match: IfMatch,
    Json(body): Json<UpdateThread>,
) -> ApiResult<Tagged<ThreadObject>> {
    principal.require(Scope::InboxWrite)?;
    let snoozed_until = match (body.status, body.snoozed_until) {
        (Some(ThreadStatus::Snoozed), Some(until)) if until > crate::process::now() => Some(until),
        (Some(ThreadStatus::Snoozed), _) => {
            return Err(Problem::invalid_field(
                "/snoozed_until",
                "required",
                "A snoozed thread needs a future `snoozed_until`.",
            ));
        }
        (_, Some(_)) => {
            return Err(Problem::invalid_field(
                "/snoozed_until",
                "unexpected",
                "`snoozed_until` goes with `status: snoozed`.",
            ));
        }
        (_, None) => None,
    };
    let ws = principal.workspace;
    let mut tx = state.db.begin_in(ws).await?;
    let current = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM threads WHERE workspace_id = $1 AND id = $2 FOR UPDATE"#,
        ws.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| Problem::not_found("thread"))?;
    if_match.check(versioning::of(current))?;
    sqlx::query!(
        "UPDATE threads SET status = coalesce($3, status),
                snoozed_until = CASE WHEN $3::text IS NULL THEN snoozed_until ELSE $4 END,
                unread = coalesce($5, unread)
          WHERE workspace_id = $1 AND id = $2",
        ws.uuid(),
        id.uuid(),
        body.status.map(ThreadStatus::as_str),
        snoozed_until as _,
        body.unread,
    )
    .execute(&mut *tx)
    .await?;
    let thread = thread(&mut tx, ws, id)
        .await?
        .ok_or_else(|| Problem::not_found("thread"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: thread.version,
        body: thread,
    })
}

// ───────────────────────────── inbound messages ─────────────────────────────

/// A person's decision on what an inbound message proposed.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::IntoStaticStr,
    strum::EnumString,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Confirmed,
    Dismissed,
}

/// Whether a person was asked to decide, what confirming applies, and the decision.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct InboundReview {
    /// When a rule or the classifier asked for a review; `null` when none was asked.
    pub requested_at: Option<Timestamp>,
    /// What confirming applies; `null` when confirming only records the decision.
    pub proposal: Option<ReviewProposal>,
    pub decision: Option<ReviewDecision>,
    pub reviewed_at: Option<Timestamp>,
}

/// A message the inbox read.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct InboundMessageObject {
    pub id: Id<InboundMessage>,
    pub connection_id: Id<Connection>,
    pub thread_id: Option<Id<Thread>>,
    /// The outbound message it answers or reports on, when correlated.
    pub message_id: Option<Id<Message>>,
    pub person_id: Option<Id<Person>>,
    pub from: Option<Address>,
    pub subject: Option<String>,
    /// Its `Message-ID` header.
    pub internet_message_id: Option<String>,
    /// The `Message-ID` it answers.
    pub in_reply_to: Option<String>,
    /// Its `References`, oldest first.
    pub references: Vec<String>,
    /// The start of its text body.
    pub text: Option<String>,
    /// Its size as the provider reported it.
    pub size_bytes: Option<i32>,
    /// It was larger than what the inbox keeps.
    pub truncated: bool,
    pub classification: Classification,
    pub classification_source: ClassificationSource,
    /// Which header or field decided the classification.
    pub evidence: String,
    /// The writer's attitude, when known.
    #[schema(value_type = Option<Sentiment>)]
    pub sentiment: Option<String>,
    pub review: InboundReview,
    pub received_at: Timestamp,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub version: i64,
}

struct InboundRow {
    id: Id<InboundMessage>,
    connection_id: Id<Connection>,
    thread_id: Option<Id<Thread>>,
    message_id: Option<Id<Message>>,
    person_id: Option<Id<Person>>,
    from_email: Option<String>,
    from_name: Option<String>,
    subject: Option<String>,
    internet_message_id: Option<String>,
    in_reply_to: Option<String>,
    references_ids: Vec<String>,
    body_text: Option<String>,
    size_bytes: Option<i32>,
    truncated: bool,
    classification: String,
    classification_source: String,
    evidence: String,
    sentiment: Option<String>,
    review_requested_at: Option<Timestamp>,
    review_proposal: Option<Value>,
    review_decision: Option<String>,
    reviewed_at: Option<Timestamp>,
    received_at: Timestamp,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl From<InboundRow> for InboundMessageObject {
    fn from(row: InboundRow) -> Self {
        Self {
            id: row.id,
            connection_id: row.connection_id,
            thread_id: row.thread_id,
            message_id: row.message_id,
            person_id: row.person_id,
            from: row.from_email.map(|email| Address {
                email,
                name: row.from_name,
            }),
            subject: row.subject,
            internet_message_id: row.internet_message_id,
            in_reply_to: row.in_reply_to,
            references: row.references_ids,
            text: row.body_text,
            size_bytes: row.size_bytes,
            truncated: row.truncated,
            classification: row
                .classification
                .parse()
                .unwrap_or(Classification::Unknown),
            classification_source: row
                .classification_source
                .parse()
                .unwrap_or(ClassificationSource::Rules),
            evidence: row.evidence,
            sentiment: row.sentiment,
            review: InboundReview {
                requested_at: row.review_requested_at,
                proposal: row
                    .review_proposal
                    .and_then(|proposal| serde_json::from_value(proposal).ok()),
                decision: row
                    .review_decision
                    .and_then(|decision| decision.parse().ok()),
                reviewed_at: row.reviewed_at,
            },
            received_at: row.received_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
            version: versioning::of(row.updated_at),
        }
    }
}

/// The filters of the inbound message list.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct InboundFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub thread_id: Option<Id<Thread>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub connection_id: Option<Id<Connection>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<Classification>,
    /// `true`: waiting for a person's decision; `false`: not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_requested: Option<bool>,
    /// Received at or after this instant.
    #[serde(
        default,
        rename = "received_at[gte]",
        skip_serializing_if = "Option::is_none"
    )]
    pub received_gte: Option<Timestamp>,
}

/// `workspace`'s inbound messages matching `filters` (one with `only`), in id order after
/// `cursor`.
async fn inbound(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &InboundFilters,
    only: Option<Id<InboundMessage>>,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<InboundMessageObject>, sqlx::Error> {
    let f = filters;
    let classification = f.classification.map(Classification::as_str);
    // Two statements, one per direction, so each walks the primary key in its own order.
    let rows = if ascending {
        sqlx::query_as!(
            InboundRow,
            r#"SELECT id AS "id: Id<InboundMessage>", connection_id AS "connection_id: Id<Connection>",
                      thread_id AS "thread_id: Id<Thread>", message_id AS "message_id: Id<Message>",
                      person_id AS "person_id: Id<Person>", from_email, from_name, subject, internet_message_id, in_reply_to,
                      references_ids, body_text, size_bytes, truncated, classification, classification_source, evidence,
                      sentiment, review_requested_at AS "review_requested_at: Timestamp", review_proposal, review_decision,
                      reviewed_at AS "reviewed_at: Timestamp", received_at AS "received_at: Timestamp",
                      created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
                 FROM inbound_messages
                WHERE workspace_id = $1 AND deleted_at IS NULL AND ($2::uuid IS NULL OR id = $2) AND ($3::uuid IS NULL OR id > $3)
                  AND ($4::uuid IS NULL OR thread_id = $4) AND ($5::uuid IS NULL OR connection_id = $5)
                  AND ($6::text IS NULL OR classification = $6)
                  AND ($7::bool IS NULL OR $7 = (review_requested_at IS NOT NULL AND review_decision IS NULL))
                  AND ($8::timestamptz IS NULL OR received_at >= $8)
                ORDER BY id LIMIT $9"#,
            workspace.uuid(),
            only.map(|id| id.uuid()),
            cursor,
            f.thread_id.map(|id| id.uuid()),
            f.connection_id.map(|id| id.uuid()),
            classification,
            f.review_requested,
            f.received_gte as _,
            limit,
        )
        .fetch_all(&mut **tx)
        .await?
    } else {
        sqlx::query_as!(
            InboundRow,
            r#"SELECT id AS "id: Id<InboundMessage>", connection_id AS "connection_id: Id<Connection>",
                      thread_id AS "thread_id: Id<Thread>", message_id AS "message_id: Id<Message>",
                      person_id AS "person_id: Id<Person>", from_email, from_name, subject, internet_message_id, in_reply_to,
                      references_ids, body_text, size_bytes, truncated, classification, classification_source, evidence,
                      sentiment, review_requested_at AS "review_requested_at: Timestamp", review_proposal, review_decision,
                      reviewed_at AS "reviewed_at: Timestamp", received_at AS "received_at: Timestamp",
                      created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
                 FROM inbound_messages
                WHERE workspace_id = $1 AND deleted_at IS NULL AND ($2::uuid IS NULL OR id = $2) AND ($3::uuid IS NULL OR id < $3)
                  AND ($4::uuid IS NULL OR thread_id = $4) AND ($5::uuid IS NULL OR connection_id = $5)
                  AND ($6::text IS NULL OR classification = $6)
                  AND ($7::bool IS NULL OR $7 = (review_requested_at IS NOT NULL AND review_decision IS NULL))
                  AND ($8::timestamptz IS NULL OR received_at >= $8)
                ORDER BY id DESC LIMIT $9"#,
            workspace.uuid(),
            only.map(|id| id.uuid()),
            cursor,
            f.thread_id.map(|id| id.uuid()),
            f.connection_id.map(|id| id.uuid()),
            classification,
            f.review_requested,
            f.received_gte as _,
            limit,
        )
        .fetch_all(&mut **tx)
        .await?
    };
    Ok(rows.into_iter().map(InboundMessageObject::from).collect())
}

/// One inbound message of `workspace`.
async fn one(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<InboundMessage>,
) -> Result<Option<InboundMessageObject>, sqlx::Error> {
    Ok(inbound(
        tx,
        workspace,
        &InboundFilters::default(),
        Some(id),
        None,
        false,
        1,
    )
    .await?
    .pop())
}

/// Locks an inbound message's row and returns its version; `None` when it does not exist.
async fn lock_inbound(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<InboundMessage>,
) -> Result<Option<i64>, sqlx::Error> {
    let updated_at = sqlx::query_scalar!(
        r#"SELECT updated_at AS "updated_at: Timestamp" FROM inbound_messages
            WHERE workspace_id = $1 AND id = $2 AND deleted_at IS NULL FOR UPDATE"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(updated_at.map(versioning::of))
}

/// List the messages the inbox read, newest first by default.
#[utoipa::path(
    get,
    path = "/inbound_messages",
    tag = "Inbox",
    operation_id = "inbound_messages.list",
    params(
        ("limit" = Option<i64>, Query, description = "Messages per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("thread_id" = Option<Id<Thread>>, Query, description = "Messages of this thread."),
        ("connection_id" = Option<Id<Connection>>, Query, description = "Messages read from this connection's mailbox."),
        ("classification" = Option<Classification>, Query, description = "Messages of this classification."),
        ("review_requested" = Option<bool>, Query, description = "`true`: waiting for a person's decision."),
        ("received_at[gte]" = Option<String>, Query, description = "Received at or after this instant (RFC 3339)."),
    ),
    responses(
        (status = 200, description = "A page of inbound messages.", body = Page<InboundMessageObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `inbox:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_inbound(
    principal: Principal,
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
    Query(filters): Query<InboundFilters>,
) -> ApiResult<Json<Page<InboundMessageObject>>> {
    principal.require(Scope::InboxRead)?;
    let ws = principal.workspace;
    let params =
        PageParams::from_query(&state.keys, ws, "inbound_messages", "id", &filters, &query)?;
    let mut tx = state.db.begin_in(ws).await?;
    let rows = inbound(
        &mut tx,
        ws,
        &filters,
        None,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(count_inbound(&mut tx, ws, &filters, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&state.keys, &params, rows, |message| {
        pagination::by_id(message.id.uuid())
    });
    Ok(Json(match total {
        Some(total) => page.with_total(total),
        None => page,
    }))
}

/// Counts `workspace`'s inbound messages matching `filters`, up to `cap + 1` (so a capped count
/// is told from an exact one).
async fn count_inbound(
    tx: &mut Tx,
    workspace: WorkspaceId,
    filters: &InboundFilters,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    let f = filters;
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM inbound_messages
                WHERE workspace_id = $1 AND deleted_at IS NULL
                  AND ($2::uuid IS NULL OR thread_id = $2) AND ($3::uuid IS NULL OR connection_id = $3)
                  AND ($4::text IS NULL OR classification = $4)
                  AND ($5::bool IS NULL OR $5 = (review_requested_at IS NOT NULL AND review_decision IS NULL))
                  AND ($6::timestamptz IS NULL OR received_at >= $6)
                LIMIT $7) counted"#,
        workspace.uuid(),
        f.thread_id.map(|id| id.uuid()),
        f.connection_id.map(|id| id.uuid()),
        f.classification.map(Classification::as_str),
        f.review_requested,
        f.received_gte as _,
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

/// Retrieve a message the inbox read.
#[utoipa::path(
    get,
    path = "/inbound_messages/{id}",
    tag = "Inbox",
    operation_id = "inbound_messages.retrieve",
    params(("id" = Id<InboundMessage>, Path, description = "The inbound message id (`inb_…`).")),
    responses(
        (status = 200, description = "The inbound message.", body = InboundMessageObject,
         headers(("ETag" = String, description = "The message's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `inbox:read`."),
        (status = 404, description = "No such inbound message in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_inbound(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<InboundMessage>>,
) -> ApiResult<Tagged<InboundMessageObject>> {
    principal.require(Scope::InboxRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let Some(message) = one(&mut tx, principal.workspace, id).await? else {
        return Err(
            problem::missing(&mut tx, "inbound_messages", id.uuid(), "inbound message").await,
        );
    };
    tx.commit().await?;
    Ok(Tagged {
        version: message.version,
        body: message,
    })
}

/// The body of `PATCH /inbound_messages/{id}`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateInbound {
    /// The corrected classification.
    #[garde(skip)]
    classification: Option<Classification>,
    /// The corrected sentiment; `null` clears it.
    #[serde(default, deserialize_with = "nullable")]
    #[garde(skip)]
    #[schema(value_type = Option<Sentiment>)]
    sentiment: Option<Option<String>>,
}

fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Correct a message's classification or sentiment by hand; AI never overrides it afterwards.
#[utoipa::path(
    patch,
    path = "/inbound_messages/{id}",
    tag = "Inbox",
    operation_id = "inbound_messages.update",
    params(("id" = Id<InboundMessage>, Path, description = "The inbound message id (`inb_…`)."), IfMatch),
    request_body(content = UpdateInbound, example = json!({"classification": "human_reply", "sentiment": "positive"})),
    responses(
        (status = 200, description = "The inbound message.", body = InboundMessageObject,
         headers(("ETag" = String, description = "The message's new `version`, quoted."))),
        (status = 400, description = "The body is not JSON, or `If-Match` is malformed (`invalid_request`)."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `inbox:write`."),
        (status = 404, description = "No such inbound message in this workspace."),
        (status = 412, description = "`If-Match` names a version that is no longer current (`precondition_failed`); nothing changed."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn update_inbound(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<InboundMessage>>,
    if_match: IfMatch,
    Json(body): Json<UpdateInbound>,
) -> ApiResult<Tagged<InboundMessageObject>> {
    principal.require(Scope::InboxWrite)?;
    if let Some(Some(sentiment)) = &body.sentiment
        && sentiment.parse::<Sentiment>().is_err()
    {
        return Err(Problem::invalid_field(
            "/sentiment",
            "enum",
            "`positive`, `neutral` or `negative`.",
        ));
    }
    let ws = principal.workspace;
    let mut tx = state.db.begin_in(ws).await?;
    let current = lock_inbound(&mut tx, ws, id)
        .await?
        .ok_or_else(|| Problem::not_found("inbound message"))?;
    if_match.check(current)?;
    if body.classification.is_some() || body.sentiment.is_some() {
        sqlx::query!(
            "UPDATE inbound_messages
                SET classification = coalesce($3, classification),
                    sentiment = CASE WHEN $4 THEN $5 ELSE sentiment END,
                    classification_source = 'manual', revision = revision + 1
              WHERE workspace_id = $1 AND id = $2",
            ws.uuid(),
            id.uuid(),
            body.classification.map(Classification::as_str),
            body.sentiment.is_some(),
            body.sentiment.flatten(),
        )
        .execute(&mut *tx)
        .await?;
    }
    let message = one(&mut tx, ws, id)
        .await?
        .ok_or_else(|| Problem::not_found("inbound message"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: message.version,
        body: message,
    })
}

/// What a person decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum Decision {
    /// Apply the proposal.
    Confirm,
    /// Discard it.
    Dismiss,
}

/// The body of `POST /inbound_messages/{id}/review`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct Review {
    #[garde(skip)]
    decision: Decision,
}

/// Confirm or dismiss what an inbound message proposed.
///
/// Confirming applies it: a suppression, or moving the person to the new address a notice gave.
#[utoipa::path(
    post,
    path = "/inbound_messages/{id}/review",
    tag = "Inbox",
    operation_id = "inbound_messages.review",
    params(("id" = Id<InboundMessage>, Path, description = "The inbound message id (`inb_…`).")),
    request_body(content = Review, example = json!({"decision": "confirm"})),
    responses(
        (status = 200, description = "The inbound message, with the decision.", body = InboundMessageObject,
         headers(("ETag" = String, description = "The message's new `version`, quoted."))),
        (status = 400, description = "No `Idempotency-Key`, or the body is not JSON."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `inbox:write`."),
        (status = 404, description = "No such inbound message in this workspace."),
        (status = 409, description = "No review was asked for, or it was decided already (`invalid_state`); or the new address belongs to another person (`conflict`)."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn review_inbound(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<InboundMessage>>,
    Json(body): Json<Review>,
) -> ApiResult<Tagged<InboundMessageObject>> {
    principal.require(Scope::InboxWrite)?;
    let ws = principal.workspace;
    let mut tx = state.db.begin_in(ws).await?;
    let row = sqlx::query!(
        "SELECT review_requested_at IS NOT NULL AS \"requested!\", review_decision, review_proposal, evidence
           FROM inbound_messages WHERE workspace_id = $1 AND id = $2 AND deleted_at IS NULL FOR UPDATE",
        ws.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| Problem::not_found("inbound message"))?;
    if !row.requested {
        return Err(Problem::invalid_state(
            "Nothing was proposed for review on this message.",
        ));
    }
    if row.review_decision.is_some() {
        return Err(Problem::invalid_state("This review was decided already."));
    }
    let decision = match body.decision {
        Decision::Confirm => {
            if let Some(proposal) = row
                .review_proposal
                .and_then(|proposal| serde_json::from_value::<ReviewProposal>(proposal).ok())
            {
                let source = match row.evidence.as_str() {
                    crate::domain::inbox::DSN_EVIDENCE => SuppressedBy::Dsn,
                    crate::domain::inbox::ARF_EVIDENCE => SuppressedBy::Arf,
                    _ => SuppressedBy::InboundNotice,
                };
                apply(&mut tx, ws, id, &principal, &proposal, source).await?;
            }
            ReviewDecision::Confirmed
        }
        Decision::Dismiss => ReviewDecision::Dismissed,
    };
    sqlx::query!(
        "UPDATE inbound_messages SET review_decision = $3, reviewed_at = now() WHERE workspace_id = $1 AND id = $2",
        ws.uuid(),
        id.uuid(),
        <&'static str>::from(decision),
    )
    .execute(&mut *tx)
    .await?;
    let message = one(&mut tx, ws, id)
        .await?
        .ok_or_else(|| Problem::not_found("inbound message"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: message.version,
        body: message,
    })
}

/// Applies a confirmed proposal: an AI verdict a person agreed with (the classification applied as the
/// AI's, unless a person set one meanwhile), the suppression it names (an address suppressed
/// already stays as it is), or the person moved to the new address and the old one suppressed as
/// `address_changed`; then the live enrollments of whoever still has the suppressed address end.
/// The review's transaction holds no queue or message lock, as ending enrollments requires.
async fn apply(
    tx: &mut Tx,
    workspace: WorkspaceId,
    inbound: Id<InboundMessage>,
    principal: &Principal,
    proposal: &ReviewProposal,
    source: SuppressedBy,
) -> Result<(), Problem> {
    let parse = |pointer: &str, email: &str| {
        EmailAddress::parse(email).map_err(|error| {
            Problem::invalid_state(format!(
                "The proposal's {pointer} is not an address ({error}); dismiss it instead."
            ))
        })
    };
    if let ReviewProposal::Classify {
        classification,
        sentiment,
        ..
    } = proposal
    {
        sqlx::query!(
            "UPDATE inbound_messages
                SET classification = $3, sentiment = $4, classification_source = 'ai', revision = revision + 1
              WHERE workspace_id = $1 AND id = $2 AND classification_source <> 'manual'",
            workspace.uuid(),
            inbound.uuid(),
            classification,
            sentiment,
        )
        .execute(&mut **tx)
        .await?;
        return Ok(());
    }
    let (email, reason) = match proposal {
        // Applied above: a classification suppresses nobody.
        ReviewProposal::Classify { .. } => return Ok(()),
        ReviewProposal::Suppress { email, reason } => (parse("address", email)?, *reason),
        ReviewProposal::ChangeAddress { email, new_email } => {
            let old = parse("address", email)?;
            if let Some(new_email) = new_email {
                let new = parse("new address", new_email)?;
                let person = sqlx::query_scalar!(
                    r#"SELECT id AS "id: Id<Person>" FROM people WHERE workspace_id = $1 AND email_key = $2"#,
                    workspace.uuid(),
                    old.key(),
                )
                .fetch_optional(&mut **tx)
                .await?;
                if let Some(person) = person {
                    people::update(
                        tx,
                        workspace,
                        person,
                        &PersonChanges {
                            email: Some(new),
                            given_name: None,
                            family_name: None,
                            company: None,
                            fields: None,
                            group_ids: None,
                        },
                    )
                    .await?;
                }
            }
            (old, SuppressionReason::AddressChanged)
        }
    };
    let created = suppressions::create(
        tx,
        workspace,
        &NewSuppression {
            email: &email,
            reason,
            source,
            source_event: None,
            evidence: Some(serde_json::json!({ "inbound_message_id": inbound })),
            created_by: principal.actor.id(),
        },
    )
    .await;
    match created {
        Ok(_) | Err(people::Error::Conflict(_)) => {}
        Err(error) => return Err(error.into()),
    }
    // The address may not be mailed: every live enrollment of its person ends now, in the same
    // transaction, rather than at its next step (a person moved to a new address keeps theirs).
    crate::campaigns::enrollments::stop_suppressed(tx, workspace, email.as_str()).await?;
    Ok(())
}

// ───────────────────────────── replies ─────────────────────────────

/// The reply form of `POST /messages`: an answer in a thread, sent from the thread's own sender
/// identity, answering the thread's latest inbound message (or, without one, its latest
/// outbound message).
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateReply {
    /// Uploaded attachments from this workspace, at most ten.
    #[garde(length(max = 10))]
    #[serde(default)]
    attachments: Vec<Id<crate::domain::ids::Attachment>>,
    /// The thread (`thr_…`).
    #[garde(skip)]
    #[schema(value_type = String)]
    thread_id: Id<Thread>,
    /// 1 to 50 addresses; the sender of the thread's latest inbound message when absent.
    #[garde(length(min = 1, max = 50))]
    #[schema(value_type = Option<Vec<String>>)]
    to: Option<Vec<EmailAddress>>,
    /// At most 150 recipients in all, `to`, `cc` and `bcc` together, none twice.
    #[garde(length(max = 149))]
    #[schema(value_type = Option<Vec<String>>)]
    cc: Option<Vec<EmailAddress>>,
    #[garde(length(max = 149))]
    #[schema(value_type = Option<Vec<String>>)]
    bcc: Option<Vec<EmailAddress>>,
    /// A template; `Re: ` and the thread's subject, as it is, when absent.
    #[garde(length(chars, min = 1, max = 1_000))]
    subject: Option<String>,
    /// The one authored body, an HTML template; a plain-text alternative is derived from it.
    /// At most 256 KiB.
    #[garde(length(min = 1, max = crate::delivery::http::BODY_MAX))]
    html: String,
    /// Values the templates read as `variables`; at most 64 KiB.
    #[garde(skip)]
    #[schema(value_type = Option<Object>)]
    variables: Option<serde_json::Map<String, Value>>,
    /// When to send it, at most 7 days ahead; now when absent or past.
    #[garde(skip)]
    send_at: Option<Timestamp>,
    /// When it stops being useful: not sent after this instant.
    #[garde(skip)]
    expires_at: Option<Timestamp>,
}

/// Accepts the reply form (see [`CreateReply`]) and returns the queued message; the caller
/// answers it as `messages.create` answers any one message. Without `to`, the reply goes to the
/// sender of the thread's latest inbound message that is not a delivery or abuse report, so a
/// mailer daemon is never answered, and answers that message.
///
/// # Errors
///
/// `404` for an unknown thread; `422` when no recipient or subject can be inferred, or for what
/// acceptance refuses (an envelope, a schedule, a template, a suppressed recipient); `409` for a
/// disabled identity.
pub(crate) async fn create_reply(
    principal: &Principal,
    state: &AppState,
    body: CreateReply,
    idempotency_key: Option<&str>,
) -> ApiResult<crate::delivery::http::MessageObject> {
    use crate::delivery::accept::{self, NewMessage, ReplyTo, Sender};

    crate::delivery::http::check_variables(body.variables.as_ref())?;
    let ws = principal.workspace;
    let mut tx = state.db.begin_in(ws).await?;
    let thread = sqlx::query!(
        r#"SELECT t.sender_identity_id AS "identity: Id<SenderIdentity>", t.subject,
                  i.from_email AS "from_email?", i.internet_message_id AS "internet_message_id?"
             FROM threads t
             LEFT JOIN LATERAL (SELECT from_email, internet_message_id FROM inbound_messages
                                 WHERE workspace_id = t.workspace_id AND thread_id = t.id AND deleted_at IS NULL
                                   AND classification NOT IN ('bounce', 'complaint')
                                 ORDER BY received_at DESC, id DESC LIMIT 1) i ON true
            WHERE t.workspace_id = $1 AND t.id = $2"#,
        ws.uuid(),
        body.thread_id.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| Problem::not_found("thread"))?;
    let to = match body.to {
        Some(to) => to,
        None => vec![
            thread
                .from_email
                .as_deref()
                .and_then(|from| EmailAddress::parse(from).ok())
                .ok_or_else(|| {
                    Problem::invalid_field(
                        "/to",
                        "required",
                        "The thread has no inbound message to answer: name the recipients.",
                    )
                })?,
        ],
    };
    let subject = match (body.subject, thread.subject) {
        (Some(subject), _) => subject,
        // The thread's subject is text, not a template: printed as it is.
        (None, Some(subject)) if !subject.contains("endraw") => {
            let answered = subject
                .get(..3)
                .is_some_and(|start| start.eq_ignore_ascii_case("re:"));
            let subject = if answered {
                subject
            } else {
                format!("Re: {subject}")
            };
            format!("{{% raw %}}{subject}{{% endraw %}}")
        }
        (None, _) => {
            return Err(Problem::invalid_field(
                "/subject",
                "required",
                "The thread has no subject to answer: give one.",
            ));
        }
    };
    let accepted = accept::create(
        &mut tx,
        &state.keys,
        ws,
        &NewMessage {
            from: Sender::Identity(thread.identity),
            to: &to,
            cc: body.cc.as_deref().unwrap_or_default(),
            bcc: body.bcc.as_deref().unwrap_or_default(),
            subject: &subject,
            html: Some(&body.html),
            text: None,
            variables: body.variables,
            send_at: body.send_at,
            expires_at: body.expires_at,
            reply: Some(ReplyTo {
                thread: body.thread_id,
                in_reply_to: thread.internet_message_id.as_deref(),
            }),
            idempotency_key,
        },
    )
    .await
    .map_err(Problem::from)?;
    crate::delivery::attachments::attach(&mut tx, ws, accepted.message, &body.attachments).await?;
    let message = crate::delivery::http::read(&mut tx, ws, accepted.message)
        .await?
        .ok_or_else(|| Problem::internal(&"an accepted message is not readable"))?;
    tx.commit().await?;
    accept::wake(&state.db).await;
    Ok(message)
}
