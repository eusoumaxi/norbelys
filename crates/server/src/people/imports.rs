//! Imports: people brought in from a file, in bulk, by a job.
//!
//! # Accepting a file
//!
//! `POST /imports` takes either a CSV file as the request body (`Content-Type: text/csv`, at
//! most 16 MiB, its first record the header) or a JSON body with up to 1,000 `people`, and an
//! optional group every imported person joins. The request checks what can be checked at once
//! (the group exists; the header has an email column and no two columns mean the same thing),
//! writes the file to object storage, then commits the import row (`queued`) together with its
//! `people.import` job, and answers `202` with the import. Nothing is imported in the request. A
//! file whose row never committed (the request failed, or its process died, in between) is
//! abandoned: `retention.prune` deletes it once it is a day old.
//!
//! # The job
//!
//! `people.import` reads the file from object storage (the file is bounded, so it is read
//! whole) and works through it in chunks of 2,000 rows from the import's cursor: a byte offset
//! into a CSV file, an index into a JSON array. For each chunk it:
//!
//! 1. checks every row against the field definitions as they are now (`domain::import`), and
//!    writes the chunk's problems as one object of the full error report, keyed by the chunk's
//!    first position, so a chunk done again overwrites its own part;
//! 2. records the chunk in one transaction under the shared field lock, after reading the
//!    definitions again (if they changed since step 1, the chunk is checked again first): the
//!    valid rows are upserted in address order (an existing person is merged: given values
//!    overwrite, absent ones stay), joined to the import's group (never past 100 groups) and
//!    linked to the import; the counts, the first 100 problems and the cursor move forward;
//! 3. checkpoints, fenced by the job's lease, so the chunk's effects and its progress commit
//!    together or not at all.
//!
//! A row is `invalid` when it cannot describe a person, `skipped` when its address was already
//! imported by an earlier row of the same file (the first occurrence wins), `imported`
//! otherwise. When the file is done, the parts are joined into `errors.csv`, the import becomes
//! `completed` and `import.completed` is recorded for the customer's webhooks. A file that cannot
//! be read as a whole (no email column, not CSV text, not a JSON array) makes the import
//! `failed` with its `last_error`, and so does a missing file.
//!
//! A run may be repeated safely: a chunk is recorded once (its effects and the cursor commit
//! together), and everything it writes outside the database is a deterministic object a repeat
//! overwrites (a part deleted twice is already gone). Writing object storage is a call outside,
//! so the kind is `ExternalRetryable`, not `Idempotent` (which promises database writes only);
//! both are recovered the same way, by running again. A job that ended `failed` or `cancelled`
//! without the import knowing (its retries
//! were exhausted, a person cancelled it) shows through the import: an import whose job ended
//! that way reads as `failed`, with the job's error.

use std::collections::HashSet;
use std::time::Duration;

use bytes::Bytes;
use csv::{ByteRecord, ReaderBuilder, WriterBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{Error, GROUPS_MAX, fields};
use crate::db::{Database, Tx};
use crate::domain::ids::{Group, Id, Import, WorkspaceId};
use crate::domain::import::{self as rows, HeaderError, PersonRow, RowError};
use crate::domain::people::Definition;
use crate::domain::time::Timestamp;
use crate::jobs::http::LastError;
use crate::jobs::{self, Effect, Job, JobContext, JobError, JobId, Outcome, Queue};
use crate::storage::{Links, Storage, StorageError};
use crate::webhooks::EventType;
use crate::webhooks::outbox::{self, Event};

/// The largest CSV file an import takes.
pub const UPLOAD_MAX: usize = 16 << 20;
/// The most people a JSON import takes.
pub const PEOPLE_MAX: usize = 1_000;
/// Rows per chunk of the job.
const CHUNK_ROWS: usize = 2_000;
/// Problems an import shows itself; the rest are in its error report.
const ERRORS_SHOWN: usize = 100;
/// How long a link to the error report lives.
const REPORT_LINK: Duration = Duration::from_secs(15 * 60);

/// The format of an import's file: what an import read, a CSV file or the people of a JSON body.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[schema(as = ImportFormat)]
pub enum Format {
    /// A CSV file whose first record is the header.
    Csv,
    /// A JSON array of people.
    Json,
}

/// What `imports.source` holds: the file's format, size and key in object storage, never its
/// bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Source {
    format: Format,
    key: String,
    size: u64,
}

/// Where an import is (`imports.status`): `queued` until its job starts, `processing` while it
/// reads rows, then `completed` (invalid rows are reported, not fatal) or `failed`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumIter,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ImportStatus {
    Queued,
    Processing,
    Completed,
    Failed,
}

/// An import as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ImportObject {
    pub id: Id<Import>,
    /// Where the import is. New values may be added.
    #[schema(value_type = ImportStatus)]
    pub status: String,
    /// What it read.
    #[schema(value_type = Format)]
    pub format: String,
    /// The group every imported person joins.
    #[schema(value_type = Option<String>)]
    pub group_id: Option<Id<Group>>,
    pub counts: Counts,
    /// The first 100 problems, one per invalid row, and a link to the full report.
    pub errors: Errors,
    /// The job doing the work.
    #[schema(value_type = Option<Id<crate::domain::ids::Job>>)]
    pub job_id: Option<JobId>,
    /// Why the import failed.
    pub last_error: Option<LastError>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub completed_at: Option<Timestamp>,
}

/// An import's rows so far.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = ImportCounts)]
pub struct Counts {
    /// Rows read.
    pub total: i64,
    /// Rows that created a person or merged into one.
    pub imported: i64,
    /// Rows whose address an earlier row of the file already imported.
    pub skipped: i64,
    /// Rows that could not describe a person.
    pub invalid: i64,
}

/// An import's problems: a first page and the report.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = ImportErrors)]
pub struct Errors {
    /// One problem per invalid row, the first 100.
    #[schema(max_items = 100)]
    pub data: Vec<RowProblem>,
    /// True when more rows are invalid than `data` shows.
    pub has_more: bool,
    /// A CSV of every problem of every invalid row (`row,field,problem`), once the import
    /// completed; the link works for 15 minutes from this read.
    pub url: Option<String>,
}

/// One problem of an import's row.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RowProblem {
    /// The row: in a CSV file, its position as a spreadsheet numbers it (the header is row 1);
    /// in a JSON import, the person's position in `people` (from 1).
    pub row: i64,
    /// The column or attribute at fault (`email`, `fields.industry`, …).
    pub field: String,
    pub problem: String,
}

/// The `people.import` job: works through one import (see the module).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeopleImport {
    pub import: Id<Import>,
}

impl Job for PeopleImport {
    const KIND: &'static str = "people.import";
    const QUEUE: Queue = Queue::Imports;
    const EFFECT: Effect = Effect::ExternalRetryable;

    fn unique_key(&self) -> Option<String> {
        Some(self.import.to_string())
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        run(self.import, cx).await
    }
}

fn source_key(workspace: WorkspaceId, id: Id<Import>, format: Format) -> String {
    let extension = match format {
        Format::Csv => "csv",
        Format::Json => "json",
    };
    format!(
        "imports/{}/{}/source.{extension}",
        workspace.uuid(),
        id.uuid()
    )
}

fn parts_prefix(workspace: WorkspaceId, id: Id<Import>) -> String {
    format!("imports/{}/{}/errors", workspace.uuid(), id.uuid())
}

fn part_key(workspace: WorkspaceId, id: Id<Import>, start: usize) -> String {
    format!("{}/{start:020}.csv", parts_prefix(workspace, id))
}

fn report_key(workspace: WorkspaceId, id: Id<Import>) -> String {
    format!("imports/{}/{}/errors.csv", workspace.uuid(), id.uuid())
}

// ───────────────────────────── reading the file ─────────────────────────────

/// Why a whole file cannot be imported.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileError {
    #[error(transparent)]
    Header(#[from] HeaderError),
    #[error("the file is empty: a CSV import needs a header")]
    Empty,
    #[error("the file is not CSV text: {0}")]
    Csv(String),
    #[error("the people are not a JSON array: {0}")]
    Json(String),
}

impl FileError {
    /// A short code for the import's `last_error`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Header(error) => error.code(),
            Self::Empty => "empty_file",
            Self::Csv(_) => "invalid_csv",
            Self::Json(_) => "invalid_json",
        }
    }
}

/// A CSV body without its UTF-8 byte order mark, which spreadsheets often write.
fn csv_body(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)
}

fn csv_reader(body: &[u8]) -> csv::Reader<&[u8]> {
    ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(body)
}

/// The header of a CSV body and the offset where its data begins.
///
/// # Errors
///
/// The body is empty or its header is not UTF-8 text.
pub fn csv_header(body: &[u8]) -> Result<(Vec<String>, usize), FileError> {
    let body = csv_body(body);
    let mut reader = csv_reader(body);
    let mut record = ByteRecord::new();
    if !reader
        .read_byte_record(&mut record)
        .map_err(|error| FileError::Csv(error.to_string()))?
    {
        return Err(FileError::Empty);
    }
    let names = record
        .iter()
        .map(|name| String::from_utf8(name.to_vec()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| FileError::Csv("the header is not UTF-8 text".to_owned()))?;
    let start = usize::try_from(reader.position().byte()).unwrap_or(body.len());
    Ok((names, start))
}

/// One row as read, before it is checked.
enum Raw {
    /// A CSV record's cells, or `None` when the record is not UTF-8 text.
    Cells(Option<Vec<String>>),
    /// An element of a JSON `people` array.
    Person(Value),
}

/// An import's file, read from a position.
enum File<'a> {
    Csv {
        header: Vec<String>,
        body: &'a [u8],
        next: usize,
    },
    Json {
        people: Vec<Value>,
        next: usize,
    },
}

impl<'a> File<'a> {
    /// Opens `bytes` at `cursor` (0 for the beginning).
    fn open(format: Format, bytes: &'a [u8], cursor: i64) -> Result<Self, FileError> {
        let cursor = usize::try_from(cursor).unwrap_or(0);
        match format {
            Format::Csv => {
                let (header, start) = csv_header(bytes)?;
                Ok(Self::Csv {
                    header,
                    body: csv_body(bytes),
                    next: cursor.max(start),
                })
            }
            Format::Json => Ok(Self::Json {
                people: serde_json::from_slice(bytes)
                    .map_err(|error| FileError::Json(error.to_string()))?,
                next: cursor,
            }),
        }
    }

    /// Where the next row starts.
    fn position(&self) -> usize {
        match self {
            Self::Csv { next, .. } | Self::Json { next, .. } => *next,
        }
    }

    /// Goes back to `position`, to read a chunk again.
    fn rewind(&mut self, position: usize) {
        match self {
            Self::Csv { next, .. } | Self::Json { next, .. } => *next = position,
        }
    }

    /// Up to `limit` rows from the current position, which moves past them.
    fn next_chunk(&mut self, limit: usize) -> Result<Vec<Raw>, FileError> {
        match self {
            Self::Csv { body, next, .. } => {
                let rest = body.get(*next..).unwrap_or_default();
                let mut reader = csv_reader(rest);
                let mut record = ByteRecord::new();
                let mut rows = Vec::new();
                while rows.len() < limit
                    && reader
                        .read_byte_record(&mut record)
                        .map_err(|error| FileError::Csv(error.to_string()))?
                {
                    let cells = record
                        .iter()
                        .map(|cell| String::from_utf8(cell.to_vec()).ok())
                        .collect::<Option<Vec<_>>>();
                    rows.push(Raw::Cells(cells));
                }
                *next += usize::try_from(reader.position().byte()).unwrap_or(rest.len());
                Ok(rows)
            }
            Self::Json { people, next } => {
                let end = next.saturating_add(limit).min(people.len());
                let rows = people
                    .get(*next..end)
                    .unwrap_or_default()
                    .iter()
                    .cloned()
                    .map(Raw::Person)
                    .collect();
                *next = end;
                Ok(rows)
            }
        }
    }

    /// The number the first data row has: a CSV file's header is row 1, a JSON array's first
    /// person is 1.
    fn first_row(&self) -> i64 {
        match self {
            Self::Csv { .. } => 2,
            Self::Json { .. } => 1,
        }
    }
}

/// A chunk's rows, checked.
struct Checked {
    /// Valid rows with their row numbers, in file order.
    people: Vec<(i64, PersonRow)>,
    /// Invalid rows with every problem of each.
    invalid: Vec<(i64, Vec<RowError>)>,
}

/// Checks a chunk's rows against `definitions`; `first` is the first row's number.
fn check(
    file: &File<'_>,
    raws: Vec<Raw>,
    first: i64,
    definitions: &[Definition],
) -> Result<Checked, FileError> {
    let columns = match file {
        File::Csv { header, .. } => Some(rows::map_header(header, definitions)?),
        File::Json { .. } => None,
    };
    let mut checked = Checked {
        people: Vec::new(),
        invalid: Vec::new(),
    };
    for (row, raw) in (first..).zip(raws) {
        let read = match (&raw, &columns) {
            (Raw::Cells(Some(cells)), Some(columns)) => rows::csv_row(columns, definitions, cells),
            (Raw::Cells(_), _) => Err(vec![RowError {
                field: String::new(),
                problem: "the row is not UTF-8 text".to_owned(),
            }]),
            (Raw::Person(person), _) => rows::json_row(definitions, person),
        };
        match read {
            Ok(person) => checked.people.push((row, person)),
            Err(problems) => checked.invalid.push((row, problems)),
        }
    }
    Ok(checked)
}

/// A chunk's problems as rows of the error report, without its header.
fn report_part(invalid: &[(i64, Vec<RowError>)]) -> Result<Vec<u8>, JobError> {
    let mut writer = WriterBuilder::new()
        .has_headers(false)
        .from_writer(Vec::new());
    for (row, problems) in invalid {
        for problem in problems {
            writer
                .write_record([row.to_string().as_str(), &problem.field, &problem.problem])
                .map_err(|error| JobError::Failed(error.to_string()))?;
        }
    }
    writer
        .into_inner()
        .map_err(|error| JobError::Failed(error.to_string()))
}

// ───────────────────────────── the API side ─────────────────────────────

/// What an import reads.
pub enum Input {
    /// A CSV file, as uploaded.
    Csv(Bytes),
    /// JSON people, each checked by the job.
    Json(Vec<Value>),
}

/// Accepts an import (see the module): checks the group and the header, stores the file,
/// commits the import with its job, and wakes the job's queue.
///
/// # Errors
///
/// [`Error::NotFound`] for an absent group, [`Error::Invalid`] for a file that cannot be
/// imported, [`Error::Storage`] when the file cannot be stored, or the database refused.
pub async fn create(
    db: &Database,
    links: Links<'_>,
    workspace: WorkspaceId,
    input: Input,
    group: Option<Id<Group>>,
) -> Result<ImportObject, Error> {
    let mut tx = db.begin_in(workspace).await?;
    if let Some(group) = group
        && super::groups::read(&mut tx, workspace, group)
            .await?
            .is_none()
    {
        return Err(Error::NotFound("group"));
    }
    let (format, bytes) = match input {
        Input::Csv(bytes) => {
            let definitions = fields::definitions(&mut tx, workspace).await?;
            csv_header(&bytes)
                .and_then(|(header, _)| Ok(rows::map_header(&header, &definitions)?))
                .map_err(|error| Error::Invalid(String::new(), error.to_string()))?;
            (Format::Csv, bytes)
        }
        Input::Json(people) => (
            Format::Json,
            Bytes::from(
                serde_json::to_vec(&people)
                    .map_err(|error| sqlx::Error::Encode(Box::new(error)))?,
            ),
        ),
    };
    tx.commit().await?;

    let id = Id::<Import>::new();
    let source = Source {
        format,
        key: source_key(workspace, id, format),
        size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    };
    links.storage.put(&source.key, bytes).await?;

    let mut tx = db.begin_in(workspace).await?;
    let job = jobs::enqueue(&mut tx, workspace, &PeopleImport { import: id }, None).await?;
    sqlx::query!(
        "INSERT INTO imports (workspace_id, id, source, group_id, job_id, status) VALUES ($1, $2, $3, $4, $5, 'queued')",
        workspace.uuid(),
        id.uuid(),
        serde_json::to_value(&source).map_err(|error| sqlx::Error::Encode(Box::new(error)))?,
        group.map(|group| group.uuid()),
        job.uuid(),
    )
    .execute(&mut *tx)
    .await
    .map_err(|error| match super::sqlstate(&error).as_deref() {
        Some("23503") => Error::NotFound("group"),
        _ => Error::Db(error),
    })?;
    let import = read(&mut tx, links, workspace, id)
        .await?
        .ok_or(Error::NotFound("import"))?;
    tx.commit().await?;
    jobs::wake(db, Queue::Imports).await;
    Ok(import)
}

/// An import's row with its job's state.
struct ImportRow {
    id: Id<Import>,
    source: Value,
    group_id: Option<Id<Group>>,
    job_id: Option<JobId>,
    status: String,
    total: i64,
    imported: i64,
    skipped: i64,
    invalid: i64,
    errors: Value,
    last_error: Option<Value>,
    created_at: Timestamp,
    updated_at: Timestamp,
    completed_at: Option<Timestamp>,
    job_state: Option<String>,
    job_error: Option<String>,
    job_updated_at: Option<Timestamp>,
}

impl ImportRow {
    async fn object(self, links: Links<'_>, workspace: WorkspaceId) -> Result<ImportObject, Error> {
        let format = serde_json::from_value::<Source>(self.source)
            .map(|source| <&'static str>::from(source.format).to_owned())
            .unwrap_or_default();
        // A job that ended without the import knowing ends the import too.
        let abandoned = matches!(self.status.as_str(), "queued" | "processing")
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
        } else {
            (
                self.status,
                self.last_error
                    .and_then(|error| serde_json::from_value::<LastError>(error).ok()),
            )
        };
        let shown: Vec<RowProblem> = serde_json::from_value(self.errors).unwrap_or_default();
        let url = if status == "completed" && self.invalid > 0 {
            Some(
                links
                    .url(&report_key(workspace, self.id), REPORT_LINK)
                    .await?
                    .to_string(),
            )
        } else {
            None
        };
        Ok(ImportObject {
            id: self.id,
            status,
            format,
            group_id: self.group_id,
            counts: Counts {
                total: self.total,
                imported: self.imported,
                skipped: self.skipped,
                invalid: self.invalid,
            },
            errors: Errors {
                has_more: self.invalid > i64::try_from(shown.len()).unwrap_or(i64::MAX),
                data: shown,
                url,
            },
            job_id: self.job_id,
            last_error,
            created_at: self.created_at,
            updated_at: self.updated_at,
            completed_at: self.completed_at,
        })
    }
}

/// Reads one import of `workspace`.
///
/// # Errors
///
/// The database is unavailable, or the report's link cannot be signed.
pub async fn read(
    tx: &mut Tx,
    links: Links<'_>,
    workspace: WorkspaceId,
    id: Id<Import>,
) -> Result<Option<ImportObject>, Error> {
    let row = sqlx::query_as!(
        ImportRow,
        r#"SELECT i.id AS "id!: Id<Import>", i.source AS "source!", i.group_id AS "group_id: Id<Group>", i.job_id AS "job_id: JobId",
                  i.status AS "status!", i.total AS "total!", i.imported AS "imported!", i.skipped AS "skipped!",
                  i.invalid AS "invalid!", i.errors AS "errors!", i.last_error,
                  i.created_at AS "created_at!: Timestamp", i.updated_at AS "updated_at!: Timestamp",
                  i.completed_at AS "completed_at: Timestamp",
                  j.state AS "job_state?", j.last_error AS "job_error?", j.updated_at AS "job_updated_at?: Timestamp"
             FROM imports i LEFT JOIN jobs j ON j.workspace_id = i.workspace_id AND j.id = i.job_id
            WHERE i.workspace_id = $1 AND i.id = $2"#,
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?;
    match row {
        Some(row) => Ok(Some(row.object(links, workspace).await?)),
        None => Ok(None),
    }
}

/// One page of `workspace`'s imports in id order after `cursor`, optionally of one `status`
/// (as stored). Fetches `limit` rows.
///
/// # Errors
///
/// The database is unavailable, or a report's link cannot be signed.
pub async fn list(
    tx: &mut Tx,
    links: Links<'_>,
    workspace: WorkspaceId,
    status: Option<&str>,
    cursor: Option<Uuid>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<ImportObject>, Error> {
    // Two statements, one per direction, so each walks the primary key in its own order.
    let rows = if ascending {
        sqlx::query_as!(
            ImportRow,
            r#"SELECT i.id AS "id!: Id<Import>", i.source AS "source!", i.group_id AS "group_id: Id<Group>", i.job_id AS "job_id: JobId",
                      i.status AS "status!", i.total AS "total!", i.imported AS "imported!", i.skipped AS "skipped!",
                      i.invalid AS "invalid!", i.errors AS "errors!", i.last_error,
                      i.created_at AS "created_at!: Timestamp", i.updated_at AS "updated_at!: Timestamp",
                      i.completed_at AS "completed_at: Timestamp",
                      j.state AS "job_state?", j.last_error AS "job_error?", j.updated_at AS "job_updated_at?: Timestamp"
                 FROM imports i LEFT JOIN jobs j ON j.workspace_id = i.workspace_id AND j.id = i.job_id
                WHERE i.workspace_id = $1 AND ($2::text IS NULL OR i.status = $2) AND ($3::uuid IS NULL OR i.id > $3)
                ORDER BY i.id LIMIT $4"#,
            workspace.uuid(),
            status,
            cursor,
            limit,
        )
        .fetch_all(&mut **tx)
        .await?
    } else {
        sqlx::query_as!(
            ImportRow,
            r#"SELECT i.id AS "id!: Id<Import>", i.source AS "source!", i.group_id AS "group_id: Id<Group>", i.job_id AS "job_id: JobId",
                      i.status AS "status!", i.total AS "total!", i.imported AS "imported!", i.skipped AS "skipped!",
                      i.invalid AS "invalid!", i.errors AS "errors!", i.last_error,
                      i.created_at AS "created_at!: Timestamp", i.updated_at AS "updated_at!: Timestamp",
                      i.completed_at AS "completed_at: Timestamp",
                      j.state AS "job_state?", j.last_error AS "job_error?", j.updated_at AS "job_updated_at?: Timestamp"
                 FROM imports i LEFT JOIN jobs j ON j.workspace_id = i.workspace_id AND j.id = i.job_id
                WHERE i.workspace_id = $1 AND ($2::text IS NULL OR i.status = $2) AND ($3::uuid IS NULL OR i.id < $3)
                ORDER BY i.id DESC LIMIT $4"#,
            workspace.uuid(),
            status,
            cursor,
            limit,
        )
        .fetch_all(&mut **tx)
        .await?
    };
    let mut imports = Vec::with_capacity(rows.len());
    for row in rows {
        imports.push(row.object(links, workspace).await?);
    }
    Ok(imports)
}

/// Counts `workspace`'s imports, optionally of one `status`, up to `cap + 1`.
///
/// # Errors
///
/// The database is unavailable.
pub async fn count(
    tx: &mut Tx,
    workspace: WorkspaceId,
    status: Option<&str>,
    cap: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
               SELECT 1 FROM imports WHERE workspace_id = $1 AND ($2::text IS NULL OR status = $2) LIMIT $3) counted"#,
        workspace.uuid(),
        status,
        cap.saturating_add(1),
    )
    .fetch_one(&mut **tx)
    .await
}

// ───────────────────────────── the job ─────────────────────────────

/// What the job needs of its import.
struct Pending {
    status: String,
    source: Source,
    cursor: i64,
    total: i64,
}

fn failed(error: StorageError) -> JobError {
    JobError::Failed(error.to_string())
}

async fn run(id: Id<Import>, cx: &mut JobContext) -> Result<Outcome, JobError> {
    let workspace = cx.workspace();
    let storage = cx.env::<Storage>()?.clone();
    let pending = {
        let mut tx = cx.db().begin_in(workspace).await?;
        let row = sqlx::query!(
            "SELECT status, source, cursor, total FROM imports WHERE workspace_id = $1 AND id = $2",
            workspace.uuid(),
            id.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        match row {
            None => return Ok(Outcome::Done),
            Some(row) => Pending {
                status: row.status,
                source: serde_json::from_value(row.source)
                    .map_err(|error| JobError::InvalidPayload(error.to_string()))?,
                cursor: row.cursor,
                total: row.total,
            },
        }
    };
    if matches!(pending.status.as_str(), "completed" | "failed") {
        return Ok(Outcome::Done);
    }
    let bytes = match storage.get(&pending.source.key).await {
        Ok(bytes) => bytes,
        Err(StorageError::NotFound(_)) => {
            return fail(
                cx,
                id,
                "file_missing",
                "The import's file is not in object storage.",
            )
            .await;
        }
        Err(error) => return Err(failed(error)),
    };
    let mut file = match File::open(pending.source.format, &bytes, pending.cursor) {
        Ok(file) => file,
        Err(error) => return fail(cx, id, error.code(), &error.to_string()).await,
    };
    let mut total = pending.total;
    loop {
        if cx.should_yield() {
            return Ok(Outcome::Yield {
                after: Duration::ZERO,
            });
        }
        let start = file.position();
        let raws = match file.next_chunk(CHUNK_ROWS) {
            Ok(raws) => raws,
            Err(error) => return fail(cx, id, error.code(), &error.to_string()).await,
        };
        if raws.is_empty() {
            return complete(cx, &storage, id).await;
        }
        let read = i64::try_from(raws.len()).unwrap_or(i64::MAX);
        let first = total.saturating_add(file.first_row());

        // 1. Check the rows as the definitions are now, and write the chunk's problems.
        let definitions = {
            let mut tx = cx.db().begin_in(workspace).await?;
            let definitions = fields::definitions(&mut tx, workspace).await?;
            tx.commit().await?;
            definitions
        };
        let checked = match check(&file, raws, first, &definitions) {
            Ok(checked) => checked,
            Err(error) => return fail(cx, id, error.code(), &error.to_string()).await,
        };
        let part = part_key(workspace, id, start);
        if checked.invalid.is_empty() {
            storage.delete(&part).await.map_err(failed)?;
        } else {
            storage
                .put(&part, Bytes::from(report_part(&checked.invalid)?))
                .await
                .map_err(failed)?;
        }

        // 2. Record the chunk under the field lock, unless the definitions changed meanwhile.
        let mut chunk = cx.begin().await?;
        fields::lock_shared(chunk.tx(), workspace).await?;
        if fields::definitions(chunk.tx(), workspace).await? != definitions {
            drop(chunk);
            file.rewind(start);
            continue;
        }
        let cursor = i64::try_from(file.position()).unwrap_or(i64::MAX);
        let counts = record(chunk.tx(), workspace, id, &checked, read, cursor).await?;
        total = total.saturating_add(read);

        // 3. Checkpoint: the chunk's effects and the job's progress commit together.
        cx.checkpoint(
            chunk,
            json!({ "cursor": cursor, "total": total, "imported": counts.imported, "skipped": counts.skipped, "invalid": counts.invalid }),
        )
        .await?;
    }
}

/// Records a checked chunk in the chunk's transaction (see the module); returns the import's
/// counts after it.
async fn record(
    tx: &mut Tx,
    workspace: WorkspaceId,
    id: Id<Import>,
    checked: &Checked,
    read: i64,
    cursor: i64,
) -> Result<Counts, JobError> {
    let import = sqlx::query!(
        "SELECT group_id, errors FROM imports WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_one(&mut **tx)
    .await?;

    // The first occurrence of an address wins: later rows of the file, in this chunk or an
    // earlier one, are skipped.
    let keys: Vec<String> = checked
        .people
        .iter()
        .map(|(_, person)| person.email.key())
        .collect();
    let mut taken: HashSet<String> = sqlx::query_scalar!(
        "SELECT p.email_key FROM import_people i JOIN people p ON p.workspace_id = i.workspace_id AND p.id = i.person_id
          WHERE i.workspace_id = $1 AND i.import_id = $2 AND p.email_key = ANY($3)",
        workspace.uuid(),
        id.uuid(),
        &keys,
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect();
    let mut fresh: Vec<(String, &PersonRow)> = Vec::with_capacity(keys.len());
    for (key, (_, person)) in keys.into_iter().zip(&checked.people) {
        if taken.insert(key.clone()) {
            fresh.push((key, person));
        }
    }
    let skipped = i64::try_from(checked.people.len() - fresh.len()).unwrap_or(0);
    // Address order: two imports upserting the same people lock them in the same order.
    fresh.sort_by(|(left, _), (right, _)| left.cmp(right));

    let emails: Vec<&str> = fresh
        .iter()
        .map(|(_, person)| person.email.as_str())
        .collect();
    let given: Vec<Option<String>> = fresh
        .iter()
        .map(|(_, person)| person.given_name.clone())
        .collect();
    let family: Vec<Option<String>> = fresh
        .iter()
        .map(|(_, person)| person.family_name.clone())
        .collect();
    let company: Vec<Option<String>> = fresh
        .iter()
        .map(|(_, person)| person.company.clone())
        .collect();
    let values: Vec<Value> = fresh
        .iter()
        .map(|(_, person)| Value::Object(person.fields.clone()))
        .collect();
    // `DO UPDATE` updates every person who existed already, even when the row brings nothing
    // new, and that update moves the person's version (`updated_at`) in the transaction that
    // adds the memberships below. A client that read the person's `group_ids` before this chunk
    // and replaces them with `If-Match` is then refused (`412`) instead of erasing the import's
    // membership. Narrowing the update (`WHERE … IS DISTINCT FROM …`) would need another write
    // of the people who gain a membership.
    let people: Vec<Uuid> = sqlx::query_scalar!(
        "INSERT INTO people (workspace_id, email, given_name, family_name, company, custom_fields)
         SELECT $1, r.email, r.given_name, r.family_name, r.company, r.custom_fields
           FROM unnest($2::text[], $3::text[], $4::text[], $5::text[], $6::jsonb[])
                AS r(email, given_name, family_name, company, custom_fields)
         ON CONFLICT (workspace_id, email_key) DO UPDATE
            SET given_name = coalesce(EXCLUDED.given_name, people.given_name),
                family_name = coalesce(EXCLUDED.family_name, people.family_name),
                company = coalesce(EXCLUDED.company, people.company),
                custom_fields = people.custom_fields || EXCLUDED.custom_fields
         RETURNING id",
        workspace.uuid(),
        &emails as _,
        &given as _,
        &family as _,
        &company as _,
        &values,
    )
    .fetch_all(&mut **tx)
    .await?;
    if let Some(group) = import.group_id {
        let most = i64::try_from(GROUPS_MAX).unwrap_or(i64::MAX);
        sqlx::query!(
            "INSERT INTO group_people (workspace_id, group_id, person_id)
             SELECT $1, $2, p FROM unnest($3::uuid[]) AS p
              WHERE (SELECT count(*) FROM group_people m WHERE m.workspace_id = $1 AND m.person_id = p) < $4
                AND EXISTS (SELECT 1 FROM groups WHERE workspace_id = $1 AND id = $2 AND deleted_at IS NULL)
             ON CONFLICT DO NOTHING",
            workspace.uuid(),
            group,
            &people,
            most,
        )
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query!(
        "INSERT INTO import_people (workspace_id, import_id, person_id)
         SELECT $1, $2, p FROM unnest($3::uuid[]) AS p ON CONFLICT DO NOTHING",
        workspace.uuid(),
        id.uuid(),
        &people,
    )
    .execute(&mut **tx)
    .await?;

    let mut shown: Vec<Value> = import.errors.as_array().cloned().unwrap_or_default();
    for (row, problems) in &checked.invalid {
        if shown.len() >= ERRORS_SHOWN {
            break;
        }
        if let Some(problem) = problems.first() {
            shown.push(json!({ "row": row, "field": problem.field, "problem": problem.problem }));
        }
    }
    let imported = i64::try_from(people.len()).unwrap_or(0);
    let invalid = i64::try_from(checked.invalid.len()).unwrap_or(0);
    let counts = sqlx::query!(
        "UPDATE imports SET status = 'processing', cursor = $3, total = total + $4, imported = imported + $5,
                skipped = skipped + $6, invalid = invalid + $7, errors = $8
          WHERE workspace_id = $1 AND id = $2
         RETURNING total, imported, skipped, invalid",
        workspace.uuid(),
        id.uuid(),
        cursor,
        read,
        imported,
        skipped,
        invalid,
        Value::Array(shown),
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(Counts {
        total: counts.total,
        imported: counts.imported,
        skipped: counts.skipped,
        invalid: counts.invalid,
    })
}

/// Ends the import `completed`: joins the chunks' problems into the error report, then records
/// the completion and `import.completed`.
async fn complete(
    cx: &mut JobContext,
    storage: &Storage,
    id: Id<Import>,
) -> Result<Outcome, JobError> {
    let workspace = cx.workspace();
    let parts = storage
        .list(&parts_prefix(workspace, id))
        .await
        .map_err(failed)?;
    if !parts.is_empty() {
        let mut report = b"row,field,problem\n".to_vec();
        for key in parts {
            report.extend_from_slice(&storage.get(&key).await.map_err(failed)?);
        }
        storage
            .put(&report_key(workspace, id), Bytes::from(report))
            .await
            .map_err(failed)?;
    }
    let mut chunk = cx.begin().await?;
    let completed = sqlx::query!(
        "UPDATE imports SET status = 'completed', completed_at = now()
          WHERE workspace_id = $1 AND id = $2 AND status IN ('queued', 'processing')
         RETURNING total, imported, skipped, invalid",
        workspace.uuid(),
        id.uuid(),
    )
    .fetch_optional(&mut **chunk.tx())
    .await?;
    if let Some(counts) = completed {
        outbox::record(
            chunk.tx(),
            workspace,
            Event {
                kind: EventType::ImportCompleted,
                subject_type: "import",
                subject_id: id.uuid(),
                data: json!({ "import_id": id, "total": counts.total, "imported": counts.imported,
                              "skipped": counts.skipped, "invalid": counts.invalid }),
            },
        )
        .await?;
    }
    cx.checkpoint(chunk, json!({ "completed": true })).await?;
    Ok(Outcome::Done)
}

/// Ends the import `failed` with `code` and `detail`, under the job's fence.
async fn fail(
    cx: &mut JobContext,
    id: Id<Import>,
    code: &str,
    detail: &str,
) -> Result<Outcome, JobError> {
    let workspace = cx.workspace();
    let mut chunk = cx.begin().await?;
    sqlx::query!(
        "UPDATE imports SET status = 'failed', last_error = jsonb_build_object('code', $3::text, 'detail', $4::text, 'at', now())
          WHERE workspace_id = $1 AND id = $2 AND status IN ('queued', 'processing')",
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
