//! Message files in the shared object store. JSON/base64 uploads are bounded to 512 KiB and
//! require messages:send. At most ten files are attached to one API message in its acceptance
//! transaction. Files are immutable once referenced; deletion locks the file before checking
//! references, in the same order as attachment, so a concurrent send cannot lose its bytes.
//!
//! Download links last five minutes. Read authorization follows the attached content's
//! direction; unattached uploads need messages:send. Received files are extracted before the
//! inbox's fenced transaction, then their rows are committed with the received message.

use crate::db::Tx;
use crate::domain::ids::{Attachment, Id, Message, WorkspaceId};
use crate::domain::scope::Scope;
use crate::domain::time::Timestamp;
use crate::http::{
    AppState,
    extract::{Json, Path},
};
use crate::identity::authority::Principal;
use crate::problem::{ApiResult, Problem};
use crate::storage::{Storage, StorageError};
use axum::{extract::State, http::StatusCode};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

/// Largest decoded upload. Ten uploads keep any API message's file payload below 5 MiB.
pub const UPLOAD_MAX: usize = 512 * 1024;

/// One immutable file with a short-lived download link.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct AttachmentObject {
    /// Typed file id.
    pub id: Id<Attachment>,
    /// Suggested download name; never a path in storage.
    pub filename: String,
    /// MIME media type.
    pub content_type: String,
    /// Inline Content-ID, when supplied by received MIME.
    pub content_id: Option<String>,
    /// Decoded length in bytes.
    pub size_bytes: i32,
    /// When this file entered storage.
    pub created_at: Timestamp,
    /// Bearer download link, valid for five minutes.
    pub download_url: String,
}

/// Internal metadata: object keys never appear in the public resource.
#[derive(Debug, sqlx::FromRow)]
pub struct File {
    /// The file identity.
    pub id: Id<Attachment>,
    /// MIME filename.
    pub filename: String,
    /// MIME type.
    pub content_type: String,
    /// Inline MIME identity.
    pub content_id: Option<String>,
    /// Decoded byte length.
    pub size_bytes: i32,
    /// Key built by the server, never the supplied filename.
    pub object_key: String,
    /// Storage instant.
    pub created_at: Timestamp,
}
impl File {
    /// Builds an expiring link after authorization; storage failures become API problems.
    ///
    /// # Errors
    /// Object storage cannot sign a download URL.
    pub async fn object(self, state: &AppState) -> ApiResult<AttachmentObject> {
        let download_url = state
            .storage
            .download_url(
                &state.keys,
                &state.settings.public_api_url,
                &self.object_key,
                Duration::from_secs(300),
            )
            .await?;
        Ok(AttachmentObject {
            id: self.id,
            filename: self.filename,
            content_type: self.content_type,
            content_id: self.content_id,
            size_bytes: self.size_bytes,
            created_at: self.created_at,
            download_url: download_url.to_string(),
        })
    }
}

/// File routes on the public API.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create))
        .routes(routes!(retrieve, delete))
}

#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct Upload {
    /// A filename without path separators or control characters, at most 255 bytes.
    #[garde(length(min = 1, max = 255))]
    filename: String,
    /// Media type, for example application/pdf.
    #[garde(length(min = 3, max = 127))]
    content_type: String,
    /// Standard base64 of at most 512 KiB of decoded data.
    #[garde(length(max = 699052))]
    content_base64: String,
}

/// Upload an immutable attachment for use in messages or replies.
#[utoipa::path(post,path = "/attachments",operation_id = "attachments.create",tag = "Messages",
    request_body(content=Upload,example=json!({"filename":"notes.txt","content_type":"text/plain","content_base64":"SGVsbG8="})),
    responses((status=201,description="Uploaded file. Unused uploads expire after 24 hours.",body=AttachmentObject),(status=401,description="No valid credential."),(status=403,description="Requires messages:send."),(status=422,description="Invalid filename, media type or base64; decoded data exceeds 512 KiB.")),security(("bearer"=[])))]
async fn create(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<Upload>,
) -> ApiResult<(StatusCode, Json<AttachmentObject>)> {
    principal.require(Scope::MessagesSend)?;
    if body
        .filename
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
        || matches!(body.filename.as_str(), "." | "..")
    {
        return Err(Problem::invalid_field(
            "/filename",
            "format",
            "Use a filename without path separators or control characters.",
        ));
    }
    if !norbelys_mail::compose::valid_content_type(&body.content_type) {
        return Err(Problem::invalid_field(
            "/content_type",
            "format",
            "Use a valid MIME media type.",
        ));
    }
    let bytes = STANDARD
        .decode(&body.content_base64)
        .map_err(|_| Problem::invalid_field("/content_base64", "format", "Use standard base64."))?;
    if bytes.len() > UPLOAD_MAX {
        return Err(Problem::invalid_field(
            "/content_base64",
            "size",
            "The decoded file exceeds 512 KiB.",
        ));
    }
    let id = Id::<Attachment>::new();
    let key = key(principal.workspace, id);
    state.storage.put(&key, Bytes::from(bytes.clone())).await?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let file = insert(
        &mut tx,
        principal.workspace,
        NewFile {
            id,
            filename: &body.filename,
            content_type: &body.content_type,
            content_id: None,
            key: &key,
            size: bytes.len(),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(file.object(&state).await?)))
}

/// Retrieve attachment metadata and a fresh download URL.
#[utoipa::path(get,path = "/attachments/{id}",operation_id = "attachments.retrieve",tag = "Messages",
    params(("id"=Id<Attachment>,Path,description="Attachment id (fil_…).")),
    responses((status=200,description="File metadata and download link.",body=AttachmentObject),(status=401,description="No valid credential."),(status=403,description="Requires the attached message's read scope, or messages:send for unused uploads."),(status=404,description="No file in this workspace.")),security(("bearer"=[])))]
async fn retrieve(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Attachment>>,
) -> ApiResult<Json<AttachmentObject>> {
    principal.require(Scope::MessagesRead).or_else(|_| {
        principal
            .require(Scope::InboxRead)
            .or_else(|_| principal.require(Scope::MessagesSend))
    })?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let file = read(&mut tx, principal.workspace, id, false)
        .await?
        .ok_or_else(|| Problem::not_found("attachment"))?;
    let directions:Vec<String>=sqlx::query_scalar("SELECT DISTINCT c.direction FROM message_attachments a JOIN message_contents c ON c.workspace_id=a.workspace_id AND c.id=a.message_id WHERE a.workspace_id=$1 AND a.attachment_id=$2")
        .bind(principal.workspace.uuid()).bind(id.uuid()).fetch_all(&mut *tx).await?;
    if directions.iter().any(|d| d == "inbound") {
        principal.require(Scope::InboxRead)?;
    }
    if directions.iter().any(|d| d == "outbound") {
        principal.require(Scope::MessagesRead)?;
    }
    if directions.is_empty() {
        principal.require(Scope::MessagesSend)?;
    }
    tx.commit().await?;
    Ok(Json(file.object(&state).await?))
}

/// Delete an unused attachment; a referenced file remains immutable.
#[utoipa::path(delete,path = "/attachments/{id}",operation_id = "attachments.delete",tag = "Messages",
    params(("id"=Id<Attachment>,Path,description="Attachment id.")),
    responses((status=204,description="Unused attachment deleted."),(status=401,description="No valid credential."),(status=403,description="Requires messages:send."),(status=404,description="No file in this workspace."),(status=409,description="The attachment is referenced by a message.")),security(("bearer"=[])))]
async fn delete(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Attachment>>,
) -> ApiResult<StatusCode> {
    principal.require(Scope::MessagesSend)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let file = read(&mut tx, principal.workspace, id, true)
        .await?
        .ok_or_else(|| Problem::not_found("attachment"))?;
    let used:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM message_attachments WHERE workspace_id=$1 AND attachment_id=$2)").bind(principal.workspace.uuid()).bind(id.uuid()).fetch_one(&mut *tx).await?;
    if used {
        return Err(Problem::conflict(
            "The attachment is referenced by a message.",
        ));
    }
    sqlx::query("DELETE FROM attachments WHERE workspace_id=$1 AND id=$2")
        .bind(principal.workspace.uuid())
        .bind(id.uuid())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    state.storage.delete(&file.object_key).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Attaches uploads while acceptance still owns the message transaction; sorted file locks
/// serialize concurrent delete operations. Received files cannot be reused as uploads.
///
/// # Errors
/// A file is absent, expired, from another workspace, received, or repeated; database failure.
pub async fn attach(
    tx: &mut Tx,
    workspace: WorkspaceId,
    message: Id<Message>,
    ids: &[Id<Attachment>],
) -> ApiResult<()> {
    let mut ordered: Vec<_> = ids.iter().map(|id| id.uuid()).collect();
    ordered.sort_unstable();
    ordered.dedup();
    if ordered.len() != ids.len() || ids.len() > 10 {
        return Err(Problem::invalid_field(
            "/attachments",
            "invalid",
            "Use at most ten distinct attachment ids.",
        ));
    }
    for id in ordered {
        let file = read(tx, workspace, Id::from_uuid(id), true)
            .await?
            .ok_or_else(|| Problem::not_found("attachment"))?;
        let inbound:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM message_attachments a JOIN message_contents c ON c.workspace_id=a.workspace_id AND c.id=a.message_id WHERE a.workspace_id=$1 AND a.attachment_id=$2 AND c.direction='inbound')").bind(workspace.uuid()).bind(id).fetch_one(&mut **tx).await?;
        if inbound || file.size_bytes > i32::try_from(UPLOAD_MAX).unwrap_or(i32::MAX) {
            return Err(Problem::invalid_field(
                "/attachments",
                "invalid",
                "Upload the file before attaching it to an outgoing message.",
            ));
        }
        sqlx::query("INSERT INTO message_attachments(workspace_id,message_id,attachment_id) VALUES($1,$2,$3)").bind(workspace.uuid()).bind(message.uuid()).bind(id).execute(&mut **tx).await?;
    }
    Ok(())
}

/// Returns one message's files, in stable upload order, inside its workspace.
///
/// # Errors
/// The database is unavailable.
pub async fn list(
    tx: &mut Tx,
    workspace: WorkspaceId,
    message: Uuid,
) -> Result<Vec<File>, sqlx::Error> {
    sqlx::query_as("SELECT a.id,a.filename,a.content_type,a.content_id,a.size_bytes,a.object_key,a.created_at FROM attachments a JOIN message_attachments m ON m.workspace_id=a.workspace_id AND m.attachment_id=a.id WHERE m.workspace_id=$1 AND m.message_id=$2 ORDER BY a.id")
        .bind(workspace.uuid()).bind(message).fetch_all(&mut **tx).await
}

async fn read(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Attachment>,
    lock: bool,
) -> Result<Option<File>, sqlx::Error> {
    let query = if lock {
        "SELECT id,filename,content_type,content_id,size_bytes,object_key,created_at FROM attachments WHERE workspace_id=$1 AND id=$2 FOR UPDATE"
    } else {
        "SELECT id,filename,content_type,content_id,size_bytes,object_key,created_at FROM attachments WHERE workspace_id=$1 AND id=$2"
    };
    sqlx::query_as(query)
        .bind(workspace.uuid())
        .bind(id.uuid())
        .fetch_optional(&mut **tx)
        .await
}

/// Metadata of a file whose immutable object has already been written.
pub struct NewFile<'a> {
    /// Minted file identity.
    pub id: Id<Attachment>,
    /// Suggested filename.
    pub filename: &'a str,
    /// MIME media type.
    pub content_type: &'a str,
    /// Inline MIME identity, when present.
    pub content_id: Option<&'a str>,
    /// Server-owned storage key.
    pub key: &'a str,
    /// Decoded object size.
    pub size: usize,
}

/// Stores file metadata whose object was already written, with the caller's business commit.
///
/// # Errors
/// The database refuses the row or the size cannot fit the storage representation.
pub async fn insert(
    tx: &mut Tx,
    workspace: WorkspaceId,
    file: NewFile<'_>,
) -> Result<File, sqlx::Error> {
    let NewFile {
        id,
        filename,
        content_type,
        content_id,
        key,
        size,
    } = file;
    sqlx::query_as("INSERT INTO attachments(workspace_id,id,filename,content_type,content_id,object_key,size_bytes) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id,filename,content_type,content_id,size_bytes,object_key,created_at")
        .bind(workspace.uuid()).bind(id.uuid()).bind(filename).bind(content_type).bind(content_id).bind(key).bind(i32::try_from(size).unwrap_or(i32::MAX)).fetch_one(&mut **tx).await
}

/// Workspace-separated object key for a minted file id.
#[must_use]
pub fn key(workspace: WorkspaceId, id: Id<Attachment>) -> String {
    format!("attachments/{}/{}", workspace.uuid(), id.uuid())
}

/// Reads the referenced immutable files for the single MIME composer.
///
/// # Errors
/// A stored object cannot be read.
pub async fn load(
    storage: &Storage,
    files: Vec<File>,
) -> Result<Vec<norbelys_mail::compose::Attachment>, StorageError> {
    let mut result = Vec::with_capacity(files.len());
    for file in files {
        result.push(norbelys_mail::compose::Attachment {
            filename: file.filename,
            content_type: file.content_type,
            bytes: storage.get(&file.object_key).await?.to_vec(),
        });
    }
    Ok(result)
}
