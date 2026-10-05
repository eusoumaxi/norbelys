//! Retained content and the unified message history. Acceptance creates the outbound projection
//! in its business transaction; rendering replaces its bodies with the last prepared content.
//! Inbox polling stores decoded bodies in the transaction that advances its fenced cursor.
//!
//! Search is a bounded, literal, case-insensitive substring over subject, sender and complete
//! retained bodies. PostgreSQL's trigram index serves text queries; workspace/thread/connection
//! indexes serve history. Cursor order is ingestion order (UUIDv7), so late inbound mail never
//! moves a page boundary. Ordinary lists carry no bodies. Content retains the parent's lifetime;
//! archived outbound messages are available through history exports, not this online projection.

use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::attachments::{self, AttachmentObject};
use crate::db::Tx;
use crate::domain::ids::{Connection, Id, InboundMessage, Message, Thread, WorkspaceId};
use crate::domain::messages::Direction;
use crate::domain::scope::Scope;
use crate::domain::time::Timestamp;
use crate::http::{
    AppState,
    extract::{Json, Path, Query},
};
use crate::identity::authority::Principal;
use crate::pagination::{self, ListQuery, Page, PageParams};
use crate::problem::{ApiResult, Problem};

/// Either typed message id, preserving its resource prefix in a shared history.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum MailId {
    /// A sent or queued message.
    Outbound(Id<Message>),
    /// A received message.
    Inbound(Id<InboundMessage>),
}
impl MailId {
    fn uuid(&self) -> Uuid {
        match self {
            Self::Outbound(id) => id.uuid(),
            Self::Inbound(id) => id.uuid(),
        }
    }
}

/// One result in ingestion order, with no large bodies or download links.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct HistoryMessage {
    /// The original message id, either msg_ or inb_.
    pub id: MailId,
    /// The source of this message.
    pub direction: Direction,
    /// The correlated conversation; absent for unmatched inbound mail.
    pub thread_id: Option<Id<Thread>>,
    /// The connection used to send or receive it.
    pub connection_id: Id<Connection>,
    /// The decoded subject.
    pub subject: String,
    /// The sender's address.
    pub from_email: String,
    /// When this message entered Norbelys's history.
    pub created_at: Timestamp,
    /// True when retained MIME or its published attachment list is incomplete.
    pub truncated: bool,
}

/// Full retained content, isolated from ordinary list responses.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct MessageContent {
    /// Message identity and routing metadata.
    #[serde(flatten)]
    pub message: HistoryMessage,
    /// Primary recipient addresses.
    pub to: Vec<String>,
    /// Copy recipient addresses.
    pub cc: Vec<String>,
    /// Blind copy addresses visible to this workspace.
    pub bcc: Vec<String>,
    /// Complete retained HTML, never an excerpt.
    pub html: Option<String>,
    /// Complete retained plain text, never the classification excerpt.
    pub text: Option<String>,
    /// MIME header name/value pairs, retaining wire encodings and repeated fields.
    pub headers: Vec<(String, String)>,
    /// The latest composition time; null before outbound rendering or for received content.
    pub prepared_at: Option<Timestamp>,
    /// Five-minute download link for retained received MIME; null for outbound content.
    pub raw_download_url: Option<String>,
    /// Available files, with expiring download URLs.
    #[schema(max_items = 100)]
    pub attachments: Vec<AttachmentObject>,
}

/// A row of the retained projection. Database encoding stays in the standard storage types.
#[derive(sqlx::FromRow)]
struct ContentRow {
    id: Uuid,
    direction: String,
    thread_id: Option<Id<Thread>>,
    connection_id: Id<Connection>,
    subject: String,
    from_email: String,
    created_at: Timestamp,
    truncated: bool,
    to_addresses: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    html: Option<String>,
    text_body: Option<String>,
    headers: sqlx::types::Json<Vec<(String, String)>>,
    prepared_at: Option<Timestamp>,
}
impl ContentRow {
    fn history(&self) -> HistoryMessage {
        let inbound = self.direction == "inbound";
        HistoryMessage {
            id: if inbound {
                MailId::Inbound(Id::from_uuid(self.id))
            } else {
                MailId::Outbound(Id::from_uuid(self.id))
            },
            direction: if inbound {
                Direction::Inbound
            } else {
                Direction::Outbound
            },
            thread_id: self.thread_id,
            connection_id: self.connection_id,
            subject: self.subject.clone(),
            from_email: self.from_email.clone(),
            created_at: self.created_at,
            truncated: self.truncated,
        }
    }
}

/// Records the outbound content beside the new message, without a separate commit.
///
/// # Errors
/// The database refuses the projection.
pub async fn accepted(
    tx: &mut Tx,
    workspace: WorkspaceId,
    message: Id<Message>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO message_contents (workspace_id,id,direction,thread_id,connection_id,subject,from_email,to_addresses,cc,bcc,html,text_body,recipient_text,created_at) SELECT workspace_id,id,'outbound',thread_id,connection_id,subject,from_email,to_addresses,cc,bcc,html,text_body,array_to_string(to_addresses || cc || bcc, E'\n'),created_at FROM messages WHERE workspace_id=$1 AND id=$2")
        .bind(workspace.uuid()).bind(message.uuid()).execute(&mut **tx).await?;
    Ok(())
}

/// Stores decoded inbound bodies with their row. A truncated MIME prefix remains explicitly
/// incomplete, and its partial attachments are never offered as complete files.
///
/// # Errors
/// The database refuses the projection.
pub async fn received(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<InboundMessage>,
    content: &norbelys_mail::inbound::Content,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO message_contents (workspace_id,id,direction,thread_id,connection_id,subject,from_email,to_addresses,cc,bcc,html,text_body,headers,truncated,recipient_text,created_at) SELECT workspace_id,id,'inbound',thread_id,connection_id,coalesce(subject,''),coalesce(from_email,''),$3,$4,$5,$6,$7,$8,(truncated OR $9),array_to_string($3::text[] || $4::text[] || $5::text[], E'\n'),created_at FROM inbound_messages WHERE workspace_id=$1 AND id=$2")
        .bind(workspace.uuid()).bind(id.uuid()).bind(&content.to).bind(&content.cc).bind(&content.bcc)
        .bind(&content.html).bind(&content.text).bind(sqlx::types::Json(&content.headers)).bind(content.attachments.len() > 100).execute(&mut **tx).await?;
    Ok(())
}

/// Writes the last composed body and headers before any provider submission. Locks the queue
/// row first and returns false when the caller no longer owns a live claimed lease; a stale
/// preparation cannot replace the next owner's content. The caller commits both locks together.
///
/// # Errors
/// The database refuses the update.
pub async fn prepared(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Message>,
    owner: &str,
    generation: i64,
    raw: &[u8],
) -> Result<bool, sqlx::Error> {
    let owned: Option<Uuid> = sqlx::query_scalar("SELECT message_id FROM delivery_queue WHERE workspace_id=$1 AND message_id=$2 AND state='claimed' AND lease_owner=$3 AND lease_generation=$4 AND lease_expires_at > clock_timestamp() FOR UPDATE")
        .bind(workspace.uuid()).bind(id.uuid()).bind(owner).bind(generation).fetch_optional(&mut **tx).await?;
    if owned.is_none() {
        return Ok(false);
    }
    let content = norbelys_mail::inbound::content(raw).unwrap_or_default();
    sqlx::query("UPDATE message_contents SET html=$3,text_body=$4,headers=$5,prepared_at=now() WHERE workspace_id=$1 AND id=$2")
        .bind(workspace.uuid()).bind(id.uuid()).bind(content.html).bind(content.text).bind(sqlx::types::Json(content.headers)).execute(&mut **tx).await?;
    Ok(true)
}

/// The public content and history routes.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(search))
        .routes(routes!(outbound))
        .routes(routes!(inbound))
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Search {
    q: Option<String>,
    thread_id: Option<Id<Thread>>,
    connection_id: Option<Id<Connection>>,
    direction: Option<Direction>,
    from: Option<Timestamp>,
    to: Option<Timestamp>,
}

/// Search complete retained content, or list history when q is absent.
#[utoipa::path(get, path = "/messages/search", operation_id = "messages.search", tag = "Messages",
    params(
        ("q"=Option<String>,Query,description="Literal case-insensitive substring, 2 to 256 characters. Absent: history."),
        ("thread_id"=Option<Id<Thread>>,Query,description="Only this conversation."),
        ("connection_id"=Option<Id<Connection>>,Query,description="Only this connection."),
        ("direction"=Option<Direction>,Query,description="Inbound, outbound, or both when absent."),
        ("from"=Option<Timestamp>,Query,description="Inclusive ingestion instant."),
        ("to"=Option<Timestamp>,Query,description="Exclusive ingestion instant."),
        ("limit"=Option<i64>,Query,description="1 to 100, default 20."),
        ("cursor"=Option<String>,Query,description="Signed cursor from the previous page."),
        ("order"=Option<pagination::Order>,Query,description="Ingestion order, descending by default.")
    ),
    responses((status=200,description="A page of retained history.",body=Page<HistoryMessage>),
        (status=400,description="Invalid cursor."),(status=401,description="No valid credential."),
        (status=403,description="Requires messages:read for outbound and inbox:read for inbound; both when direction is absent."),
        (status=422,description="Invalid search or date bounds.")), security(("bearer"=[])))]
async fn search(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
    Query(query): Query<Search>,
) -> ApiResult<Json<Page<HistoryMessage>>> {
    principal.require(match query.direction {
        Some(Direction::Inbound) => Scope::InboxRead,
        _ => Scope::MessagesRead,
    })?;
    if query.direction.is_none() {
        principal.require(Scope::InboxRead)?;
    }
    crate::domain::messages::check_search(
        query.q.as_deref(),
        query.from.map(|at| at.0),
        query.to.map(|at| at.0),
    )
    .map_err(|error| {
        let field = match error {
            crate::domain::messages::SearchError::Text => "?q",
            crate::domain::messages::SearchError::Range => "?to",
        };
        Problem::invalid_field(field, "invalid", error.to_string())
    })?;
    if list.include.is_some() {
        return Err(Problem::invalid_field(
            "?include",
            "unsupported",
            "Search does not count matching bodies.",
        ));
    }
    let params = PageParams::from_query(
        &state.keys,
        principal.workspace,
        "messages.search",
        "id",
        &query,
        &list,
    )?;
    let pattern = query.q.as_ref().map(|q| {
        format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    });
    let mut tx = state.db.begin_in(principal.workspace).await?;
    // Select only metadata: body bytes are never loaded or serialized in history pages.
    let rows: Vec<ContentRow> = sqlx::query_as("SELECT id,direction,thread_id,connection_id,subject,from_email,created_at,truncated,'{}'::text[] AS to_addresses,'{}'::text[] AS cc,'{}'::text[] AS bcc,NULL::text AS html,NULL::text AS text_body,'[]'::jsonb AS headers,NULL::timestamptz AS prepared_at FROM message_contents c WHERE workspace_id=$1 AND ($2::text IS NULL OR search_text ILIKE $2) AND ($3::uuid IS NULL OR thread_id=$3) AND ($4::uuid IS NULL OR connection_id=$4) AND ($5::text IS NULL OR direction=$5) AND ($6::timestamptz IS NULL OR created_at >= $6) AND ($7::timestamptz IS NULL OR created_at < $7) AND ($8::uuid IS NULL OR CASE WHEN $9 THEN id > $8 ELSE id < $8 END) AND (direction='inbound' OR EXISTS(SELECT 1 FROM messages m WHERE m.workspace_id=c.workspace_id AND m.id=c.id AND m.deleted_at IS NULL)) ORDER BY CASE WHEN $9 THEN id END,id DESC LIMIT $10")
        .bind(principal.workspace.uuid()).bind(pattern).bind(query.thread_id.map(|id|id.uuid())).bind(query.connection_id.map(|id|id.uuid()))
        .bind(query.direction.map(Direction::as_str)).bind(query.from).bind(query.to).bind(params.after_id()).bind(params.ascending()).bind(params.fetch()).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(Page::new(
        &state.keys,
        &params,
        rows.iter().map(ContentRow::history).collect(),
        |row: &HistoryMessage| pagination::by_id(row.id.uuid()),
    )))
}

/// Retrieve an outbound message's retained authored or last prepared content.
#[utoipa::path(get,path = "/messages/{id}/content",operation_id = "messages.content",tag = "Messages",
    params(("id"=Id<Message>,Path,description="Message id.")),responses((status=200,description="Retained content.",body=MessageContent),(status=401,description="No valid credential."),(status=403,description="Requires messages:read."),(status=404,description="No retained message.")),security(("bearer"=[])))]
async fn outbound(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Message>>,
) -> ApiResult<Json<MessageContent>> {
    principal.require(Scope::MessagesRead)?;
    read(&state, principal.workspace, id.uuid(), Direction::Outbound)
        .await
        .map(Json)
}

/// Retrieve full received content and available attachments, with truncation explicit.
#[utoipa::path(get,path = "/inbound_messages/{id}/content",operation_id = "inbound_messages.content",tag = "Inbox",
    params(("id"=Id<InboundMessage>,Path,description="Inbound message id.")),responses((status=200,description="Retained content.",body=MessageContent),(status=401,description="No valid credential."),(status=403,description="Requires inbox:read."),(status=404,description="No retained message.")),security(("bearer"=[])))]
async fn inbound(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<InboundMessage>>,
) -> ApiResult<Json<MessageContent>> {
    principal.require(Scope::InboxRead)?;
    read(&state, principal.workspace, id.uuid(), Direction::Inbound)
        .await
        .map(Json)
}

async fn read(
    state: &AppState,
    workspace: WorkspaceId,
    id: Uuid,
    direction: Direction,
) -> ApiResult<MessageContent> {
    let mut tx = state.db.begin_in(workspace).await?;
    let row: Option<ContentRow>=sqlx::query_as("SELECT id,direction,thread_id,connection_id,subject,from_email,created_at,truncated,to_addresses,cc,bcc,html,text_body,headers,prepared_at FROM message_contents c WHERE workspace_id=$1 AND id=$2 AND direction=$3 AND (direction='inbound' OR EXISTS(SELECT 1 FROM messages m WHERE m.workspace_id=c.workspace_id AND m.id=c.id AND m.deleted_at IS NULL))")
        .bind(workspace.uuid()).bind(id).bind(direction.as_str()).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Err(match direction {
            Direction::Outbound => {
                crate::problem::missing(&mut tx, "messages", id, "message content").await
            }
            Direction::Inbound => Problem::not_found("message content"),
        });
    };
    let raw_key: Option<String> = if matches!(direction, Direction::Inbound) {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT body_object_key FROM inbound_messages WHERE workspace_id=$1 AND id=$2",
        )
        .bind(workspace.uuid())
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .flatten()
    } else {
        None
    };
    let files = attachments::list(&mut tx, workspace, id).await?;
    tx.commit().await?;
    let raw_download_url = match raw_key {
        Some(key) => Some(
            state
                .storage
                .download_url(
                    &state.keys,
                    &state.settings.public_api_url,
                    &key,
                    std::time::Duration::from_secs(300),
                )
                .await?
                .to_string(),
        ),
        None => None,
    };
    let mut attachments = Vec::with_capacity(files.len());
    for file in files {
        attachments.push(file.object(state).await?);
    }
    Ok(MessageContent {
        message: row.history(),
        to: row.to_addresses,
        cc: row.cc,
        bcc: row.bcc,
        html: row.html,
        text: row.text_body,
        headers: row.headers.0,
        prepared_at: row.prepared_at,
        raw_download_url,
        attachments,
    })
}
