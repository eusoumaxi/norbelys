//! Exports of history: a workspace's messages, attempts, delivery events and inbound messages,
//! including the periods the archive has already taken out of the database.
//!
//! # Rows and filters
//!
//! A history export holds the table's own rows, every column, in the table's column order:
//! as CSV (a header of the column names, each value as text, empty for null) or as JSON lines
//! (one object per row; numbers and booleans as such, instants as RFC 3339 in UTC, `jsonb` and
//! arrays as their text). The filters are those of the resource's list ([`HistoryFilters`]):
//! a range of the resource's instant (`created_at[gte]`, `created_at[lt]`: when a message was
//! created, an attempt claimed, an event recorded, an inbound message received) and equality
//! filters on its ids and states. A filter the resource does not have is refused.
//!
//! # Archived periods
//!
//! Messages, attempts and delivery events are partitioned by period, and a period past its
//! online window lives only as a Parquet object of the archive (`partition_leaves.archive_key`).
//! The export reads those objects first (the older periods, in period order; within a period
//! in the file's order), keeping the workspace's rows that pass the filters, then the rows still
//! online in key order, a page of [`PAGE`] per short transaction. A period whose rows are online
//! is never archived and the reverse, so no row is read twice; if the archive moves a period out
//! while the export runs, the archived set read at the start no longer matches the one at the
//! end, and the run fails to be retried rather than miss that period.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sqlx::AssertSqlSafe;
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

use super::{Format, Resource};
use crate::analytics::parquet::{self, Cell, Rows, quoted};
use crate::domain::ids::{Campaign, Connection, Id, Message, Person, Thread};
use crate::domain::time::Timestamp;
use crate::jobs::{JobContext, JobError};
use crate::storage::{Storage, Writer};

/// Rows read per online page.
pub const PAGE: i64 = 5_000;
/// How often a streaming run renews its lease.
const RENEW: Duration = Duration::from_secs(15);

/// The filters of a history export: those of the resource's list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryFilters {
    /// At or after this instant: when the message was created, the attempt claimed, the event
    /// recorded or the inbound message received.
    #[serde(
        default,
        rename = "created_at[gte]",
        skip_serializing_if = "Option::is_none"
    )]
    pub created_gte: Option<Timestamp>,
    /// Before this instant.
    #[serde(
        default,
        rename = "created_at[lt]",
        skip_serializing_if = "Option::is_none"
    )]
    pub created_lt: Option<Timestamp>,
    /// Messages of this campaign.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub campaign_id: Option<Id<Campaign>>,
    /// Messages, attempts or inbound messages of this connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub connection_id: Option<Id<Connection>>,
    /// Messages to, or inbound messages from, this person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub person_id: Option<Id<Person>>,
    /// Messages or inbound messages of this thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub thread_id: Option<Id<Thread>>,
    /// Attempts, events or inbound messages about this message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub message_id: Option<Id<Message>>,
    /// Messages in this state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Events of this kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Inbound messages of this classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<String>,
}

/// Where a history resource's rows are and how they are read.
struct Shape {
    table: &'static str,
    /// The instant `created_at[…]` filters on.
    time: &'static str,
    /// The key the online rows are paged by, with each column's type.
    key: &'static [(&'static str, &'static str)],
    /// The equality filters it has.
    filters: &'static [&'static str],
    /// Whether its old periods are in the archive.
    partitioned: bool,
}

fn shape(resource: Resource) -> Option<Shape> {
    match resource {
        Resource::People => None,
        Resource::Messages => Some(Shape {
            table: "messages",
            time: "created_at",
            key: &[("id", "uuid")],
            filters: &[
                "campaign_id",
                "connection_id",
                "person_id",
                "thread_id",
                "state",
            ],
            partitioned: true,
        }),
        Resource::Attempts => Some(Shape {
            table: "attempts",
            time: "claimed_at",
            key: &[("message_id", "uuid"), ("attempt_number", "int4")],
            filters: &["message_id", "connection_id"],
            partitioned: true,
        }),
        Resource::DeliveryEvents => Some(Shape {
            table: "delivery_events",
            time: "created_at",
            key: &[("id", "uuid")],
            filters: &["message_id", "kind"],
            partitioned: true,
        }),
        Resource::InboundMessages => Some(Shape {
            table: "inbound_messages",
            time: "received_at",
            key: &[("id", "uuid")],
            filters: &[
                "message_id",
                "connection_id",
                "person_id",
                "thread_id",
                "classification",
            ],
            partitioned: false,
        }),
    }
}

impl HistoryFilters {
    /// The equality filters given, as `(column, value)`, the value in PostgreSQL's text form.
    fn equalities(&self) -> Vec<(&'static str, String)> {
        let uuid = |id: Option<Uuid>| id.map(|id| id.to_string());
        [
            ("campaign_id", uuid(self.campaign_id.map(|id| id.uuid()))),
            (
                "connection_id",
                uuid(self.connection_id.map(|id| id.uuid())),
            ),
            ("person_id", uuid(self.person_id.map(|id| id.uuid()))),
            ("thread_id", uuid(self.thread_id.map(|id| id.uuid()))),
            ("message_id", uuid(self.message_id.map(|id| id.uuid()))),
            ("state", self.state.clone()),
            ("kind", self.kind.clone()),
            ("classification", self.classification.clone()),
        ]
        .into_iter()
        .filter_map(|(column, value)| value.map(|value| (column, value)))
        .collect()
    }
}

/// Checks that `filters` are filters of `resource`'s list; answers the first that is not.
///
/// # Errors
///
/// The name of a filter `resource` does not have.
pub fn check(resource: Resource, filters: &HistoryFilters) -> Result<(), &'static str> {
    let Some(shape) = shape(resource) else {
        return Ok(());
    };
    match filters
        .equalities()
        .into_iter()
        .find(|(column, _)| !shape.filters.contains(column))
    {
        Some((column, _)) => Err(column),
        None => Ok(()),
    }
}

/// The archived leaves of `table` whose period may hold rows in the range, as
/// `(leaf, object key)`, oldest first.
async fn archived(
    cx: &JobContext,
    shape: &Shape,
    filters: &HistoryFilters,
) -> Result<Vec<(String, String)>, JobError> {
    if !shape.partitioned {
        return Ok(Vec::new());
    }
    // An attempt is claimed after its message was created, so the period of its message (its
    // partition key) can start well before `created_at[gte]`: attempts prune by the upper end only.
    let from = if shape.table == "attempts" {
        None
    } else {
        filters.created_gte
    };
    let mut tx = cx.db().begin().await?;
    let leaves = sqlx::query!(
        r#"SELECT name, archive_key AS "archive_key!" FROM partition_leaves
            WHERE parent = $1 AND archive_key IS NOT NULL
              AND ($2::timestamptz IS NULL OR upper > $2) AND ($3::timestamptz IS NULL OR lower < $3)
            ORDER BY lower"#,
        shape.table,
        from as _,
        filters.created_lt as _,
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(leaves
        .into_iter()
        .map(|leaf| (leaf.name, leaf.archive_key))
        .collect())
}

fn failed(error: impl std::fmt::Display) -> JobError {
    JobError::Failed(error.to_string())
}

/// The bytes of one row in `format`.
fn line(format: Format, columns: &[String], cells: &[Cell]) -> Result<Vec<u8>, JobError> {
    match format {
        Format::Csv => super::csv_line(&cells.iter().map(Cell::to_text).collect::<Vec<_>>()),
        Format::Jsonl => {
            let object: serde_json::Map<String, serde_json::Value> = columns
                .iter()
                .cloned()
                .zip(cells.iter().map(Cell::to_json))
                .collect();
            let mut bytes = serde_json::to_vec(&object).map_err(failed)?;
            bytes.push(b'\n');
            Ok(bytes)
        }
    }
}

/// Streams every row of `resource` in `workspace` that passes `filters` into `writer` (see the
/// module); returns how many were written.
///
/// # Errors
///
/// The database, the object store or an archived file failed; or the archive moved a period
/// while the export ran (retried).
pub(super) async fn stream(
    cx: &mut JobContext,
    storage: &Storage,
    writer: &mut Writer,
    resource: Resource,
    filters: &HistoryFilters,
    format: Format,
) -> Result<i64, JobError> {
    let Some(shape) = shape(resource) else {
        return Err(failed("people are not history"));
    };
    let workspace = cx.workspace();
    let columns = {
        let mut conn = cx.db().pool().acquire().await?;
        parquet::columns(&mut conn, shape.table).await?
    };
    let names: Vec<String> = columns.iter().map(|column| column.name.clone()).collect();
    if format == Format::Csv {
        writer
            .write(&super::csv_line(&names)?)
            .await
            .map_err(failed)?;
    }
    let equalities = filters.equalities();
    let mut rows: i64 = 0;
    let mut renewed = Instant::now();

    // The archived periods.
    let leaves = archived(cx, &shape, filters).await?;
    for (leaf, key) in &leaves {
        let path =
            std::env::temp_dir().join(format!("norbelys-export-{leaf}-{}.parquet", Uuid::now_v7()));
        let result = async {
            download(storage, key, &path).await?;
            let file = std::fs::File::open(&path).map_err(failed)?;
            let archived = Rows::open(file, Some(&workspace.uuid().to_string())).map_err(failed)?;
            let positions = Positions::of(archived.names(), &names, &shape, &equalities)?;
            let mut bytes = Vec::new();
            for cells in archived {
                let cells = cells.map_err(failed)?;
                if !positions.keeps(&cells, filters) {
                    continue;
                }
                bytes.extend(line(format, &names, &positions.project(cells))?);
                rows = rows.saturating_add(1);
                if bytes.len() >= 1 << 20 {
                    writer.write(&bytes).await.map_err(failed)?;
                    bytes.clear();
                }
                if renewed.elapsed() >= RENEW {
                    cx.heartbeat().await?;
                    renewed = Instant::now();
                }
            }
            writer.write(&bytes).await.map_err(failed)?;
            Ok::<(), JobError>(())
        }
        .await;
        let _ = std::fs::remove_file(&path);
        result?;
    }

    // The rows still online, a page per short transaction.
    let mut clauses = vec!["workspace_id = $1".to_owned()];
    let mut next = 2;
    if filters.created_gte.is_some() {
        clauses.push(format!("{} >= ${next}", quoted(shape.time)));
        next += 1;
    }
    if filters.created_lt.is_some() {
        clauses.push(format!("{} < ${next}", quoted(shape.time)));
        next += 1;
    }
    for (column, _) in &equalities {
        clauses.push(format!("{}::text = ${next}", quoted(column)));
        next += 1;
    }
    let key_columns: Vec<String> = shape.key.iter().map(|(column, _)| quoted(column)).collect();
    let key_params: Vec<String> = shape
        .key
        .iter()
        .enumerate()
        .map(|(index, (_, kind))| format!("${}::{kind}", next + index))
        .collect();
    let select = parquet::select_list(&columns);
    let first = format!(
        "SELECT {select} FROM {table} WHERE {filters} ORDER BY {key} LIMIT {PAGE}",
        table = quoted(shape.table),
        filters = clauses.join(" AND "),
        key = key_columns.join(", "),
    );
    let after = format!(
        "SELECT {select} FROM {table} WHERE {filters} AND ({key}) > ({params}) ORDER BY {key} LIMIT {PAGE}",
        table = quoted(shape.table),
        filters = clauses.join(" AND "),
        key = key_columns.join(", "),
        params = key_params.join(", "),
    );
    let key_positions: Vec<usize> = shape
        .key
        .iter()
        .filter_map(|(column, _)| names.iter().position(|name| name == column))
        .collect();
    let mut cursor: Option<Vec<String>> = None;
    loop {
        let page = {
            let mut tx = cx.db().begin_in(workspace).await?;
            let statement = if cursor.is_some() { &after } else { &first };
            let mut query = sqlx::query(AssertSqlSafe(statement.clone())).bind(workspace.uuid());
            if let Some(at) = filters.created_gte {
                query = query.bind(at);
            }
            if let Some(at) = filters.created_lt {
                query = query.bind(at);
            }
            for (_, value) in &equalities {
                query = query.bind(value.clone());
            }
            for value in cursor.iter().flatten() {
                query = query.bind(value.clone());
            }
            let page = query.fetch_all(&mut *tx).await?;
            tx.commit().await?;
            page
        };
        let mut bytes = Vec::new();
        let mut last = None;
        for row in &page {
            let cells = parquet::cells(row, &columns)?;
            bytes.extend(line(format, &names, &cells)?);
            last = Some(cells);
        }
        writer.write(&bytes).await.map_err(failed)?;
        rows = rows.saturating_add(i64::try_from(page.len()).unwrap_or(0));
        if renewed.elapsed() >= RENEW {
            cx.heartbeat().await?;
            renewed = Instant::now();
        }
        match last {
            Some(cells) if i64::try_from(page.len()).unwrap_or(0) >= PAGE => {
                cursor = Some(
                    key_positions
                        .iter()
                        .map(|index| cells.get(*index).map(Cell::to_text).unwrap_or_default())
                        .collect(),
                );
            }
            _ => break,
        }
    }

    if archived(cx, &shape, filters).await? != leaves {
        return Err(failed(
            "the archive moved a period out while the export ran; it runs again",
        ));
    }
    Ok(rows)
}

/// Where, in an archived file's columns, the filters look.
struct Positions {
    /// For each current column, its place in the file (a column added since is absent).
    project: Vec<Option<usize>>,
    time: Option<usize>,
    equalities: Vec<(usize, Cell)>,
}

impl Positions {
    fn of(
        names: &[String],
        current: &[String],
        shape: &Shape,
        equalities: &[(&'static str, String)],
    ) -> Result<Self, JobError> {
        let position = |column: &str| {
            names
                .iter()
                .position(|name| name == column)
                .ok_or_else(|| failed(format!("an archived {} file has no {column}", shape.table)))
        };
        Ok(Self {
            project: current
                .iter()
                .map(|column| names.iter().position(|name| name == column))
                .collect(),
            time: Some(position(shape.time)?),
            equalities: equalities
                .iter()
                .map(|(column, value)| Ok((position(column)?, Cell::Text(value.clone()))))
                .collect::<Result<_, JobError>>()?,
        })
    }

    /// An archived row in the current columns' order, null where the file has no such column.
    fn project(&self, cells: Vec<Cell>) -> Vec<Cell> {
        self.project
            .iter()
            .map(|index| {
                index
                    .and_then(|index| cells.get(index).cloned())
                    .unwrap_or(Cell::Null)
            })
            .collect()
    }

    /// Whether an archived row passes the filters.
    fn keeps(&self, cells: &[Cell], filters: &HistoryFilters) -> bool {
        let at = self.time.and_then(|index| match cells.get(index) {
            Some(Cell::Timestamp(micros)) => Some(*micros),
            _ => None,
        });
        let micros = |bound: Option<Timestamp>| bound.map(|bound| bound.0.as_microsecond());
        let after_start =
            micros(filters.created_gte).is_none_or(|gte| at.is_some_and(|at| at >= gte));
        let before_end = micros(filters.created_lt).is_none_or(|lt| at.is_some_and(|at| at < lt));
        after_start
            && before_end
            && self
                .equalities
                .iter()
                .all(|(index, value)| cells.get(*index) == Some(value))
    }
}

/// Copies the stored object at `key` to the local file `path`.
async fn download(storage: &Storage, key: &str, path: &std::path::Path) -> Result<(), JobError> {
    use futures_util::StreamExt as _;
    let (_, body) = storage.body(key).await.map_err(failed)?;
    let mut file = tokio::fs::File::create(path).await.map_err(failed)?;
    let mut stream = body.into_data_stream();
    while let Some(bytes) = stream.next().await {
        file.write_all(&bytes.map_err(failed)?)
            .await
            .map_err(failed)?;
    }
    file.flush().await.map_err(failed)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::{HistoryFilters, check, shape};
    use crate::people::exports::Resource;

    /// Every resource but people has a history shape whose filters are columns it can be checked
    /// against, and a filter of another resource is refused by name: an export with a filter it
    /// silently ignored would hold more than the customer asked for.
    #[test]
    fn each_history_resource_takes_only_its_own_filters() {
        for resource in Resource::iter() {
            let Some(shape) = shape(resource) else {
                assert_eq!(resource, Resource::People);
                continue;
            };
            assert!(!shape.key.is_empty() && !shape.filters.is_empty());
        }
        let state = HistoryFilters {
            state: Some("sent".to_owned()),
            ..HistoryFilters::default()
        };
        assert_eq!(check(Resource::Messages, &state), Ok(()));
        assert_eq!(check(Resource::Attempts, &state), Err("state"));
        assert_eq!(check(Resource::People, &state), Ok(()));
    }
}
