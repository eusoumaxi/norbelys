//! Exports: a file of workspace data, written by a job and downloaded through a short-lived
//! link.
//!
//! `POST /exports` names a resource and the filters of its list, and answers `202` with the
//! export (`queued`); the `export.run` job writes the file to object storage and makes the
//! export `ready`, and every read of a ready export carries a fresh link valid for 15 minutes
//! (a presigned URL on S3, or the api's signed `/files` route on a local store). The file is
//! kept for 7 days; after that the export reads as `expired` and has no link, and the next
//! `retention.prune` deletes its file, then the export itself. An export is the
//! way to read a complete list (a list's pages may shift while it is read) and, as other areas
//! add their resources here, archived history.
//!
//! For `people`, with the filters of `GET /people`: one row per person in id order, as CSV (the
//! person's attributes, then `group_ids` joined by `;`, then one column per custom field, named by
//! its key, so the file imports back) or as JSON lines (the person as the API shows it).
//!
//! For history (`messages`, `attempts`, `delivery_events`, `inbound_messages`), the table's own
//! rows with the filters of the resource's list, including the periods the archive has taken
//! out of the database: the only way to read archived ranges and long histories ([`history`]).
//!
//! # The job
//!
//! The file is one multipart upload streamed from pages of 5,000 people, each read in a short
//! transaction of its own (no transaction is open while the store is written). An upload cannot
//! outlive the run that started it, so this job does not yield at its quantum: it renews its
//! lease while it streams instead, and runs in the exports lane, where it holds one of the
//! workspace's two slots. A cancellation takes effect when the run ends. If the run dies, the
//! abandoned upload is discarded by the bucket's rule for incomplete uploads, and recovery runs
//! the export again from the start, which writes the same object again: the effect is safe to
//! repeat.

pub mod history;

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{Error, PeopleFilters, PersonObject, Selection};
use crate::db::{Database, Tx};
use crate::domain::ids::{Export, Id, WorkspaceId};
use crate::domain::scope::Scope;
use crate::domain::time::Timestamp;
use crate::jobs::http::LastError;
use crate::jobs::{self, Effect, Job, JobContext, JobError, JobId, Outcome, Queue};
use crate::storage::{Links, Storage, StorageError, Writer};
use crate::webhooks::EventType;
use crate::webhooks::outbox::{self, Event};

/// How long a finished file is kept.
const KEPT: Duration = Duration::from_secs(7 * 86_400);
/// How long a download link lives.
const LINK: Duration = Duration::from_secs(15 * 60);
/// People read per page.
const PAGE: i64 = 5_000;
/// How often a streaming run renews its lease.
const RENEW: Duration = Duration::from_secs(15);

/// What an export contains.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[schema(as = ExportResource)]
pub enum Resource {
    /// The workspace's people, with the filters of `GET /people`.
    People,
    /// Messages, archived periods included.
    Messages,
    /// Delivery attempts, archived periods included.
    Attempts,
    /// Delivery events, archived periods included.
    DeliveryEvents,
    /// Inbound messages.
    InboundMessages,
}

impl Resource {
    /// The scope that reads this resource, and so its exports and their download links: an
    /// export holds the same rows its list serves, so a credential that cannot list them must not
    /// fetch them as a file either.
    #[must_use]
    pub fn read_scope(self) -> Scope {
        match self {
            Self::People => Scope::PeopleRead,
            Self::Messages | Self::Attempts | Self::DeliveryEvents => Scope::MessagesRead,
            Self::InboundMessages => Scope::InboxRead,
        }
    }

    /// The scope that requests an export of this resource: `people:write` for people, and for a
    /// history the scope that reads its list. `POST /exports` checks it, and the MCP server shows
    /// the export tool to a grant holding any of them.
    #[must_use]
    pub fn create_scope(self) -> Scope {
        match self {
            Self::People => Scope::PeopleWrite,
            history => history.read_scope(),
        }
    }
}

/// The file's format.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[schema(as = ExportFormat)]
pub enum Format {
    /// Comma-separated values with a header.
    #[default]
    Csv,
    /// One JSON object per line.
    Jsonl,
}

impl Format {
    fn extension(self) -> &'static str {
        self.into()
    }
}

/// Where an export is, as the API shows it: `queued` and `running` while its job writes the file,
/// `ready` with a link, `failed`, or `expired` once its file's days are over. The row stores the
/// first four; `expired` is read from `expires_at`, and a job that ended without finishing reads
/// as `failed`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    strum::IntoStaticStr,
    strum::EnumIter,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ExportStatus {
    Queued,
    Running,
    Ready,
    Failed,
    Expired,
}

/// An export as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ExportObject {
    pub id: Id<Export>,
    /// What the file contains. New values may be added.
    #[schema(value_type = Resource)]
    pub resource: String,
    /// The filters of the resource's list the file applies.
    #[schema(value_type = Object)]
    pub filters: Value,
    /// The file's format.
    #[schema(value_type = Format)]
    pub format: String,
    /// Where the export is. New values may be added.
    #[schema(value_type = ExportStatus)]
    pub status: String,
    /// The rows in the file, once it is ready.
    pub rows: Option<i64>,
    /// The file, when `ready`: a link that works for 15 minutes from this read.
    pub url: Option<String>,
    /// When the file is deleted.
    pub expires_at: Timestamp,
    /// The job doing the work.
    #[schema(value_type = Option<Id<crate::domain::ids::Job>>)]
    pub job_id: Option<JobId>,
    /// Why the export failed.
    pub last_error: Option<LastError>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The `export.run` job: writes one export's file (see the module).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRun {
    pub export: Id<Export>,
}

impl Job for ExportRun {
    const KIND: &'static str = "export.run";
    const QUEUE: Queue = Queue::Exports;
    const EFFECT: Effect = Effect::ExternalRetryable;

    fn unique_key(&self) -> Option<String> {
        Some(self.export.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        run(self.export, cx).await
    }
}

/// Where the file of export `id` of `workspace` is kept: `exports/<workspace>/<export>.<format>`.
/// The job writes it there (and records it as the export's `object_key` once it is ready);
/// `retention.prune` deletes it there when the export expires, ready or not.
pub(crate) fn object_key(workspace: WorkspaceId, id: Id<Export>, format: Format) -> String {
    format!(
        "exports/{}/{}.{}",
        workspace.uuid(),
        id.uuid(),
        format.extension()
    )
}

// ───────────────────────────── the API side ─────────────────────────────

/// Requests an export: commits it with its job and wakes the exports queue. The filters, those of
/// `resource`'s list as stored JSON, were checked by the caller (for people, their group and
/// segment exist; for history, [`history::check`]).
///
/// # Errors
///
/// The database refused.
pub async fn create(
    db: &Database,
    links: Links<'_>,
    workspace: WorkspaceId,
    requested_by: &str,
    resource: Resource,
    filters: Value,
    format: Format,
) -> Result<ExportObject, Error> {
    let id = Id::<Export>::new();
    let mut tx = db.begin_in(workspace).await?;
    let job = jobs::enqueue(&mut tx, workspace, &ExportRun { export: id }, None).await?;
    sqlx::query!(
        "INSERT INTO exports (workspace_id, id, kind, filter, format, status, job_id, requested_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, 'queued', $6, $7, $8)",
        workspace.uuid(),
        id.uuid(),
        <&'static str>::from(resource),
        filters,
        format.extension(),
        job.uuid(),
        requested_by,
        crate::process::now().plus(KEPT) as _,
    )
    .execute(&mut *tx)
    .await?;
    let export = read(&mut tx, links, workspace, id)
        .await?
        .ok_or(Error::NotFound("export"))?;
    tx.commit().await?;
    jobs::wake(db, Queue::Exports).await;
    Ok(export)
}

/// An export's row with its job's state.
struct ExportRow {
    id: Id<Export>,
    kind: String,
    filter: Value,
    format: String,
    status: String,
    object_key: Option<String>,
    rows: Option<i64>,
    last_error: Option<Value>,
    expires_at: Timestamp,
    job_id: Option<JobId>,
    created_at: Timestamp,
    updated_at: Timestamp,
    job_state: Option<String>,
    job_error: Option<String>,
    job_updated_at: Option<Timestamp>,
}

impl ExportRow {
    async fn object(self, links: Links<'_>) -> Result<ExportObject, Error> {
        let abandoned = matches!(self.status.as_str(), "queued" | "running")
            && matches!(self.job_state.as_deref(), Some("failed" | "cancelled"));
        let (status, last_error) = if abandoned {
            let text = self.job_error.unwrap_or_default();
            let (code, detail) = text.split_once(": ").unwrap_or(("error", text.as_str()));
            (
                "failed".to_owned(),
                Some(LastError {
                    code: code.to_owned(),
                    detail: detail.to_owned(),
                    at: self.job_updated_at.unwrap_or(self.updated_at),
                }),
            )
        } else if self.expires_at <= crate::process::now() {
            ("expired".to_owned(), None)
        } else {
            (
                self.status,
                self.last_error
                    .and_then(|error| serde_json::from_value::<LastError>(error).ok()),
            )
        };
        let url = match (&self.object_key, status.as_str()) {
            (Some(key), "ready") => Some(links.url(key, LINK).await?.to_string()),
            _ => None,
        };
        Ok(ExportObject {
            id: self.id,
            resource: self.kind,
            filters: self.filter,
            format: self.format,
            status,
            rows: self.rows,
            url,
            expires_at: self.expires_at,
            job_id: self.job_id,
            last_error,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Reads one export of `workspace`, with a fresh link when it is ready.
///
/// # Errors
///
/// The database is unavailable, or the link cannot be signed.
pub async fn read(
    tx: &mut Tx,
    links: Links<'_>,
    workspace: WorkspaceId,
    id: Id<Export>,
) -> Result<Option<ExportObject>, Error> {
    let row = sqlx::query_as!(
        ExportRow,
        r#"SELECT e.id AS "id!: Id<Export>", e.kind AS "kind!", e.filter AS "filter!", e.format AS "format!", e.status AS "status!",
                  e.object_key, e.rows, e.last_error,
                  e.expires_at AS "expires_at!: Timestamp", e.job_id AS "job_id: JobId",
                  e.created_at AS "created_at!: Timestamp", e.updated_at AS "updated_at!: Timestamp",
                  j.state AS "job_state?", j.last_error AS "job_error?", j.updated_at AS "job_updated_at?: Timestamp"
             FROM exports e LEFT JOIN jobs j ON j.workspace_id = e.workspace_id AND j.id = e.job_id
            WHERE e.workspace_id = $1 AND e.id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    match row {
        Some(row) => Ok(Some(row.object(links).await?)),
        None => Ok(None),
    }
}

/// One page of `workspace`'s exports of the resources `kinds` names (the ones the caller may
/// read), in the page's id order, optionally of one stored `status`. Fetches one extra row to
/// determine whether another page exists.
///
/// # Errors
///
/// The database is unavailable, or a link cannot be signed.
pub async fn list(
    tx: &mut Tx,
    links: Links<'_>,
    workspace: WorkspaceId,
    kinds: &[String],
    status: Option<&str>,
    page: &crate::pagination::PageParams,
) -> Result<Vec<ExportObject>, Error> {
    // A workspace's exports are few (they expire): one statement serves both orders.
    let rows = sqlx::query_as!(
        ExportRow,
        r#"SELECT e.id AS "id!: Id<Export>", e.kind AS "kind!", e.filter AS "filter!", e.format AS "format!", e.status AS "status!",
                  e.object_key, e.rows, e.last_error,
                  e.expires_at AS "expires_at!: Timestamp", e.job_id AS "job_id: JobId",
                  e.created_at AS "created_at!: Timestamp", e.updated_at AS "updated_at!: Timestamp",
                  j.state AS "job_state?", j.last_error AS "job_error?", j.updated_at AS "job_updated_at?: Timestamp"
             FROM exports e LEFT JOIN jobs j ON j.workspace_id = e.workspace_id AND j.id = e.job_id
            WHERE e.workspace_id = $1 AND ($2::text IS NULL OR e.status = $2)
              AND ($3::uuid IS NULL OR CASE WHEN $4 THEN e.id > $3 ELSE e.id < $3 END)
              AND e.kind = ANY($6)
            ORDER BY CASE WHEN $4 THEN e.id END, e.id DESC LIMIT $5"#,
        workspace.uuid(),
        status,
        page.after_id(),
        page.ascending(),
        page.fetch(),
        kinds,
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut exports = Vec::with_capacity(rows.len());
    for row in rows {
        exports.push(row.object(links).await?);
    }
    Ok(exports)
}

/// Counts `workspace`'s exports of the resources `kinds` names, optionally of one stored
/// `status`, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    kinds: &[String],
    status: Option<&str>,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM exports WHERE workspace_id = $1 AND ($2::text IS NULL OR status = $2)
                  AND kind = ANY($4) LIMIT $3) counted"#,
        workspace.uuid(),
        status,
        cap.saturating_add(1),
        kinds,
    )
    .fetch_one(&mut **tx)
    .await
}

// ───────────────────────────── the job ─────────────────────────────

fn failed(error: StorageError) -> JobError {
    JobError::Failed(error.to_string())
}

async fn run(id: Id<Export>, cx: &mut JobContext) -> Result<Outcome, JobError> {
    let workspace = cx.workspace();
    let storage = cx.env::<Storage>()?.clone();
    let export = {
        let mut tx = cx.db().begin_in(workspace).await?;
        let row = sqlx::query!(
            "SELECT status, kind, filter, format FROM exports WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            id.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        match row {
            Some(row) if matches!(row.status.as_str(), "queued" | "running") => row,
            _ => return Ok(Outcome::Done),
        }
    };
    let Ok(format) = export.format.parse::<Format>() else {
        return fail(
            cx,
            id,
            "invalid_export",
            "The export's format is not known.",
        )
        .await;
    };
    let Ok(resource) = export.kind.parse::<Resource>() else {
        return fail(
            cx,
            id,
            "invalid_export",
            "The export's resource is not known.",
        )
        .await;
    };
    if resource != Resource::People {
        let Ok(filters) = serde_json::from_value::<history::HistoryFilters>(export.filter) else {
            return fail(
                cx,
                id,
                "invalid_export",
                "The export's filters are not readable.",
            )
            .await;
        };
        return produce(
            cx,
            &storage,
            id,
            format,
            Source::History {
                resource,
                filters: &filters,
            },
        )
        .await;
    }
    let Ok(filters) = serde_json::from_value::<PeopleFilters>(export.filter) else {
        return fail(
            cx,
            id,
            "invalid_export",
            "The export's filters are not readable.",
        )
        .await;
    };

    // The selection and the columns, as the workspace is now.
    let (selection, definitions) = {
        let mut tx = cx.db().begin_in(workspace).await?;
        let segment = match filters.segment_id {
            None => None,
            Some(segment) => match super::segments::compiled(&mut tx, workspace, segment).await {
                Ok(compiled) => Some(compiled),
                Err(Error::NotFound(_)) => {
                    return fail(
                        cx,
                        id,
                        "segment_missing",
                        "The export's segment was deleted.",
                    )
                    .await;
                }
                Err(error) => return Err(JobError::Failed(error.to_string())),
            },
        };
        let definitions = super::fields::definitions(&mut tx, workspace).await?;
        tx.commit().await?;
        (Selection::new(&filters, segment), definitions)
    };
    let keys: Vec<String> = definitions
        .into_iter()
        .map(|definition| definition.key)
        .collect();

    produce(
        cx,
        &storage,
        id,
        format,
        Source::People {
            selection: &selection,
            keys: &keys,
        },
    )
    .await
}

/// What an export's file is written from.
enum Source<'a> {
    /// The selected people, with the workspace's custom field keys.
    People {
        selection: &'a Selection,
        keys: &'a [String],
    },
    /// A history resource's rows.
    History {
        resource: Resource,
        filters: &'a history::HistoryFilters,
    },
}

/// Marks the export running, writes its file from `source` in one upload, and makes it ready
/// with its row count and the `export.completed` event, under the job's fence.
async fn produce(
    cx: &mut JobContext,
    storage: &Storage,
    id: Id<Export>,
    format: Format,
    source: Source<'_>,
) -> Result<Outcome, JobError> {
    let workspace = cx.workspace();
    let mut chunk = cx.begin().await?;
    sqlx::query!(
        "UPDATE exports SET status = 'running' WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
    )
    .execute(&mut **chunk.tx())
    .await?;
    cx.checkpoint(chunk, json!({ "rows": 0 })).await?;

    let key = object_key(workspace, id, format);
    let mut writer = storage.writer(&key).await.map_err(failed)?;
    let streamed = match source {
        Source::People { selection, keys } => {
            stream(cx, &mut writer, selection, format, keys).await
        }
        Source::History { resource, filters } => {
            history::stream(cx, storage, &mut writer, resource, filters, format).await
        }
    };
    let rows = match streamed {
        Ok(rows) => rows,
        Err(error) => {
            writer.abort().await;
            return Err(error);
        }
    };
    writer.finish().await.map_err(failed)?;

    let mut chunk = cx.begin().await?;
    sqlx::query!(
        "UPDATE exports SET status = 'ready', object_key = $3, rows = $4, expires_at = $5, last_error = NULL
          WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        id.uuid(),
        key,
        rows,
        crate::process::now().plus(KEPT) as _,
    )
    .execute(&mut **chunk.tx())
    .await?;
    outbox::record(
        chunk.tx(),
        workspace,
        Event {
            kind: EventType::ExportCompleted,
            subject_type: "export",
            subject_id: id.uuid(),
            data: json!({ "export_id": id, "rows": rows }),
        },
    )
    .await?;
    cx.checkpoint(chunk, json!({ "rows": rows })).await?;
    Ok(Outcome::Done)
}

/// Streams every selected person into `writer`; returns how many were written.
async fn stream(
    cx: &mut JobContext,
    writer: &mut Writer,
    selection: &Selection,
    format: Format,
    keys: &[String],
) -> Result<i64, JobError> {
    let workspace = cx.workspace();
    if format == Format::Csv {
        let mut header: Vec<String> = [
            "id",
            "email",
            "given_name",
            "family_name",
            "company",
            "group_ids",
            "last_sent_at",
            "replied_at",
            "created_at",
            "updated_at",
        ]
        .map(str::to_owned)
        .to_vec();
        header.extend(keys.iter().cloned());
        writer.write(&csv_line(&header)?).await.map_err(failed)?;
    }
    let mut rows: i64 = 0;
    let mut after: Option<Uuid> = None;
    let mut renewed = Instant::now();
    loop {
        let page = {
            let mut tx = cx.db().begin_in(workspace).await?;
            let page = super::list(&mut tx, workspace, selection, after, true, PAGE).await?;
            tx.commit().await?;
            page
        };
        let mut bytes = Vec::new();
        for person in &page {
            match format {
                Format::Csv => bytes.extend(csv_line(&csv_cells(person, keys))?),
                Format::Jsonl => {
                    serde_json::to_writer(&mut bytes, person)
                        .map_err(|error| JobError::Failed(error.to_string()))?;
                    bytes.push(b'\n');
                }
            }
        }
        writer.write(&bytes).await.map_err(failed)?;
        rows = rows.saturating_add(i64::try_from(page.len()).unwrap_or(0));
        if renewed.elapsed() >= RENEW {
            cx.heartbeat().await?;
            renewed = Instant::now();
        }
        match page.last() {
            Some(last) if i64::try_from(page.len()).unwrap_or(0) >= PAGE => {
                after = Some(last.id.uuid())
            }
            _ => return Ok(rows),
        }
    }
}

/// A person's CSV cells: the attributes, the groups joined by `;`, then each custom field.
fn csv_cells(person: &PersonObject, keys: &[String]) -> Vec<String> {
    let time = |at: Option<Timestamp>| at.map(|at| at.to_string()).unwrap_or_default();
    let mut cells = vec![
        person.id.to_string(),
        person.email.clone(),
        person.given_name.clone().unwrap_or_default(),
        person.family_name.clone().unwrap_or_default(),
        person.company.clone().unwrap_or_default(),
        person
            .group_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(";"),
        time(person.last_sent_at),
        time(person.replied_at),
        person.created_at.to_string(),
        person.updated_at.to_string(),
    ];
    for key in keys {
        cells.push(match person.fields.get(key) {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
        });
    }
    cells
}

/// One CSV record, quoted as needed, with its line end.
fn csv_line(cells: &[String]) -> Result<Vec<u8>, JobError> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer
        .write_record(cells)
        .map_err(|error| JobError::Failed(error.to_string()))?;
    writer
        .into_inner()
        .map_err(|error| JobError::Failed(error.to_string()))
}

/// Ends the export `failed` with `code` and `detail`, under the job's fence.
async fn fail(
    cx: &mut JobContext,
    id: Id<Export>,
    code: &str,
    detail: &str,
) -> Result<Outcome, JobError> {
    let workspace = cx.workspace();
    let mut chunk = cx.begin().await?;
    sqlx::query!(
        "UPDATE exports SET status = 'failed', last_error = jsonb_build_object('code', $3::text, 'detail', $4::text, 'at', now())
          WHERE workspace_id = $1 AND id = $2 AND status IN ('queued', 'running')",
        workspace.uuid(),
        id.uuid(),
        code,
        detail,
    )
    .execute(&mut **chunk.tx())
    .await?;
    cx.checkpoint(chunk, json!({ "failed": code })).await?;
    Ok(Outcome::Done)
}
