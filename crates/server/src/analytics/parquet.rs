//! The archive's file format: a PostgreSQL table's rows as Parquet, and those files read back.
//!
//! # One encoding for every table
//!
//! The archive exports whole leaves of several tables whose columns change with the schema, so
//! the encoding is derived from the table's catalogue at export time rather than written per
//! table: every column becomes an optional Parquet column of one of a few kinds ([`Kind`]):
//! integers stay integers, `timestamptz` becomes microseconds since the Unix epoch marked as a
//! UTC timestamp, `date` becomes days since the epoch, `bytea` stays bytes, and everything else
//! (uuids, text, `jsonb`, arrays) is its PostgreSQL text form. The files therefore read in any
//! Parquet reader (DuckDB, the analytics role's engine, reads the timestamps and dates as such)
//! and can be written back to PostgreSQL by casting the text columns.
//!
//! The rows are read through a select list ([`select_list`]) that casts each column to its
//! kind's encoding on the server, so decoding a row needs no type knowledge beyond the kind.
//! Rows are buffered per column and written as row groups of [`ROW_GROUP`] rows, which bounds
//! the writer's memory whatever the table's size.
//!
//! Writing is synchronous: the writer encodes into a local file (or memory) that the archive
//! then streams to object storage; reading takes a local file for the same reason (a Parquet
//! reader seeks to the footer first).

use std::fs::File;
use std::io::Write;
use std::sync::Arc;

use parquet::basic::{Compression, LogicalType, Repetition, TimeUnit, Type as Physical};
use parquet::data_type::{BoolType, ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::errors::ParquetError;
use parquet::file::properties::WriterProperties;
use parquet::file::reader::{FileReader as _, SerializedFileReader};
use parquet::file::writer::SerializedFileWriter;
use parquet::record::Field;
use parquet::record::reader::RowIter;
use parquet::schema::types::Type;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row as _};

/// Rows per row group: large enough for good compression, small enough to bound memory.
pub const ROW_GROUP: usize = 50_000;

/// How a column is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The PostgreSQL text form (uuids, text, `jsonb`, arrays, anything else).
    Text,
    /// `smallint` and `integer`.
    Int32,
    /// `bigint`.
    Int64,
    /// `boolean`.
    Bool,
    /// `timestamptz` (and `timestamp`, read as UTC): microseconds since the Unix epoch.
    Timestamp,
    /// `date`: days since the Unix epoch.
    Date,
    /// `bytea`.
    Bytes,
}

impl Kind {
    /// The kind a column of PostgreSQL type `type_name` (`pg_type.typname`) is archived as.
    #[must_use]
    pub fn of(type_name: &str) -> Self {
        match type_name {
            "int2" | "int4" => Self::Int32,
            "int8" => Self::Int64,
            "bool" => Self::Bool,
            "timestamptz" | "timestamp" => Self::Timestamp,
            "date" => Self::Date,
            "bytea" => Self::Bytes,
            _ => Self::Text,
        }
    }
}

/// A column of an archived table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub kind: Kind,
}

/// One value of a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    Null,
    Text(String),
    Int(i64),
    Bool(bool),
    /// Microseconds since the Unix epoch, UTC.
    Timestamp(i64),
    /// Days since the Unix epoch.
    Date(i32),
    Bytes(Vec<u8>),
}

impl Cell {
    /// The value as JSON: text, numbers and booleans as such, a timestamp as RFC 3339 in UTC, a
    /// date as `YYYY-MM-DD`, bytes as their lowercase hex.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value;
        match self {
            Self::Null => Value::Null,
            Self::Text(text) => Value::String(text.clone()),
            Self::Int(number) => Value::from(*number),
            Self::Bool(flag) => Value::Bool(*flag),
            Self::Timestamp(_) | Self::Date(_) | Self::Bytes(_) => Value::String(self.to_text()),
        }
    }

    /// The value as one CSV cell: empty for null, otherwise as [`Cell::to_json`] without quotes.
    #[must_use]
    pub fn to_text(&self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Text(text) => text.clone(),
            Self::Int(number) => number.to_string(),
            Self::Bool(flag) => flag.to_string(),
            Self::Timestamp(micros) => jiff::Timestamp::from_microsecond(*micros)
                .map(|at| at.to_string())
                .unwrap_or_default(),
            Self::Date(days) => jiff::civil::date(1970, 1, 1)
                .checked_add(jiff::Span::new().days(*days))
                .map(|day| day.to_string())
                .unwrap_or_default(),
            Self::Bytes(bytes) => crate::crypto::hex(bytes),
        }
    }
}

/// The columns of `table` (a table or a leaf) in their order, with their kinds.
///
/// # Errors
///
/// The table does not exist, or the database failed.
pub async fn columns(conn: &mut PgConnection, table: &str) -> Result<Vec<Column>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT a.attname::text AS "name!", t.typname::text AS "type_name!"
             FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
            WHERE a.attrelid = $1::text::regclass AND a.attnum > 0 AND NOT a.attisdropped
            ORDER BY a.attnum"#,
        table,
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Column {
            kind: Kind::of(&row.type_name),
            name: row.name,
        })
        .collect())
}

/// `name` as a quoted SQL identifier.
#[must_use]
pub fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The select list that reads `columns` in their kinds' encodings, for [`cells`].
#[must_use]
pub fn select_list(columns: &[Column]) -> String {
    columns
        .iter()
        .map(|column| {
            let name = quoted(&column.name);
            match column.kind {
                Kind::Text => format!("{name}::text"),
                Kind::Int32 => format!("{name}::int4"),
                Kind::Int64 | Kind::Bool | Kind::Bytes => name,
                Kind::Timestamp => format!("(extract(epoch FROM {name}) * 1000000)::int8"),
                Kind::Date => format!("({name} - date '1970-01-01')::int4"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Decodes a row read through [`select_list`].
///
/// # Errors
///
/// A value does not decode as its kind (the select list and the columns disagree).
pub fn cells(row: &PgRow, columns: &[Column]) -> Result<Vec<Cell>, sqlx::Error> {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            Ok(match column.kind {
                Kind::Text => row
                    .try_get::<Option<String>, _>(index)?
                    .map_or(Cell::Null, Cell::Text),
                Kind::Int32 => row
                    .try_get::<Option<i32>, _>(index)?
                    .map_or(Cell::Null, |number| Cell::Int(i64::from(number))),
                Kind::Int64 => row
                    .try_get::<Option<i64>, _>(index)?
                    .map_or(Cell::Null, Cell::Int),
                Kind::Bool => row
                    .try_get::<Option<bool>, _>(index)?
                    .map_or(Cell::Null, Cell::Bool),
                Kind::Timestamp => row
                    .try_get::<Option<i64>, _>(index)?
                    .map_or(Cell::Null, Cell::Timestamp),
                Kind::Date => row
                    .try_get::<Option<i32>, _>(index)?
                    .map_or(Cell::Null, Cell::Date),
                Kind::Bytes => row
                    .try_get::<Option<Vec<u8>>, _>(index)?
                    .map_or(Cell::Null, Cell::Bytes),
            })
        })
        .collect()
}

/// The Parquet schema of `columns`: every column optional, of its kind's physical and logical type.
fn schema(columns: &[Column]) -> Result<Type, ParquetError> {
    let fields = columns
        .iter()
        .map(|column| {
            let (physical, logical) = match column.kind {
                Kind::Text => (Physical::BYTE_ARRAY, Some(LogicalType::String)),
                Kind::Int32 => (Physical::INT32, None),
                Kind::Int64 => (Physical::INT64, None),
                Kind::Bool => (Physical::BOOLEAN, None),
                Kind::Timestamp => (
                    Physical::INT64,
                    Some(LogicalType::timestamp(true, TimeUnit::MICROS)),
                ),
                Kind::Date => (Physical::INT32, Some(LogicalType::Date)),
                Kind::Bytes => (Physical::BYTE_ARRAY, None),
            };
            Type::primitive_type_builder(&column.name, physical)
                .with_repetition(Repetition::OPTIONAL)
                .with_logical_type(logical)
                .build()
                .map(Arc::new)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Type::group_type_builder("row").with_fields(fields).build()
}

/// Writes rows as Parquet into `W`, a row group at a time.
pub struct Writer<W: Write + Send> {
    inner: SerializedFileWriter<W>,
    columns: Vec<Column>,
    /// The buffered row group, column by column.
    buffer: Vec<Vec<Cell>>,
    rows: i64,
}

impl<W: Write + Send> Writer<W> {
    /// A writer of `columns` into `sink`, compressed with Snappy.
    ///
    /// # Errors
    ///
    /// A column name is not valid in Parquet.
    pub fn new(sink: W, columns: Vec<Column>) -> Result<Self, ParquetError> {
        let properties = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .build();
        let inner =
            SerializedFileWriter::new(sink, Arc::new(schema(&columns)?), Arc::new(properties))?;
        let buffer = columns.iter().map(|_| Vec::new()).collect();
        Ok(Self {
            inner,
            columns,
            buffer,
            rows: 0,
        })
    }

    /// Adds one row, in the columns' order; writes a row group when [`ROW_GROUP`] rows are
    /// buffered.
    ///
    /// # Errors
    ///
    /// The row has the wrong number of cells, or writing the row group failed.
    pub fn push(&mut self, row: Vec<Cell>) -> Result<(), ParquetError> {
        if row.len() != self.columns.len() {
            return Err(ParquetError::General(format!(
                "a row of {} cells for {} columns",
                row.len(),
                self.columns.len()
            )));
        }
        for (column, cell) in self.buffer.iter_mut().zip(row) {
            column.push(cell);
        }
        self.rows = self.rows.saturating_add(1);
        if self
            .buffer
            .first()
            .is_some_and(|column| column.len() >= ROW_GROUP)
        {
            self.flush()?;
        }
        Ok(())
    }

    /// Writes the buffered rows as one row group.
    fn flush(&mut self) -> Result<(), ParquetError> {
        if self.buffer.first().is_none_or(Vec::is_empty) {
            return Ok(());
        }
        let mut group = self.inner.next_row_group()?;
        for (column, cells) in self.columns.iter().zip(&mut self.buffer) {
            let Some(mut writer) = group.next_column()? else {
                return Err(ParquetError::General(
                    "the schema has fewer columns than the table".to_owned(),
                ));
            };
            let levels: Vec<i16> = cells
                .iter()
                .map(|cell| i16::from(*cell != Cell::Null))
                .collect();
            let cells = std::mem::take(cells);
            match column.kind {
                Kind::Text | Kind::Bytes => {
                    let values: Vec<ByteArray> = cells
                        .into_iter()
                        .filter_map(|cell| match cell {
                            Cell::Text(text) => Some(ByteArray::from(text.into_bytes())),
                            Cell::Bytes(bytes) => Some(ByteArray::from(bytes)),
                            _ => None,
                        })
                        .collect();
                    writer
                        .typed::<ByteArrayType>()
                        .write_batch(&values, Some(&levels), None)?;
                }
                Kind::Int32 | Kind::Date => {
                    let values: Vec<i32> = cells
                        .into_iter()
                        .filter_map(|cell| match cell {
                            Cell::Int(number) => i32::try_from(number).ok(),
                            Cell::Date(days) => Some(days),
                            _ => None,
                        })
                        .collect();
                    writer
                        .typed::<Int32Type>()
                        .write_batch(&values, Some(&levels), None)?;
                }
                Kind::Int64 | Kind::Timestamp => {
                    let values: Vec<i64> = cells
                        .into_iter()
                        .filter_map(|cell| match cell {
                            Cell::Int(number) | Cell::Timestamp(number) => Some(number),
                            _ => None,
                        })
                        .collect();
                    writer
                        .typed::<Int64Type>()
                        .write_batch(&values, Some(&levels), None)?;
                }
                Kind::Bool => {
                    let values: Vec<bool> = cells
                        .into_iter()
                        .filter_map(|cell| match cell {
                            Cell::Bool(flag) => Some(flag),
                            _ => None,
                        })
                        .collect();
                    writer
                        .typed::<BoolType>()
                        .write_batch(&values, Some(&levels), None)?;
                }
            }
            writer.close()?;
        }
        group.close()?;
        Ok(())
    }

    /// Writes the last row group and the footer; returns the rows written.
    ///
    /// # Errors
    ///
    /// Writing failed.
    pub fn finish(mut self) -> Result<i64, ParquetError> {
        self.flush()?;
        self.inner.close()?;
        Ok(self.rows)
    }
}

/// The rows of a Parquet file, in order, as cells: optionally only those of one workspace (whose
/// `workspace_id` column holds that uuid in its text form).
pub struct Rows {
    names: Vec<String>,
    owner: Option<(usize, Cell)>,
    inner: RowIter<'static>,
}

impl Rows {
    /// Opens the Parquet file `file`; with `workspace`, its rows are only that workspace's.
    ///
    /// # Errors
    ///
    /// The file is not Parquet.
    pub fn open(file: File, workspace: Option<&str>) -> Result<Self, ParquetError> {
        let reader = SerializedFileReader::new(file)?;
        let names: Vec<String> = reader
            .metadata()
            .file_metadata()
            .schema_descr()
            .columns()
            .iter()
            .map(|column| column.name().to_owned())
            .collect();
        let owner = workspace.and_then(|workspace| {
            names
                .iter()
                .position(|name| name == "workspace_id")
                .map(|index| (index, Cell::Text(workspace.to_owned())))
        });
        Ok(Self {
            names,
            owner,
            inner: reader.into_iter(),
        })
    }

    /// The column names, in the cells' order.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }
}

impl Iterator for Rows {
    type Item = Result<Vec<Cell>, ParquetError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let row = match self.inner.next()? {
                Ok(row) => row,
                Err(error) => return Some(Err(error)),
            };
            let cells: Vec<Cell> = row
                .get_column_iter()
                .map(|(_, field)| match field {
                    Field::Null => Cell::Null,
                    Field::Bool(flag) => Cell::Bool(*flag),
                    Field::Byte(number) => Cell::Int(i64::from(*number)),
                    Field::Short(number) => Cell::Int(i64::from(*number)),
                    Field::Int(number) => Cell::Int(i64::from(*number)),
                    Field::Long(number) => Cell::Int(*number),
                    Field::Str(text) => Cell::Text(text.clone()),
                    Field::Bytes(bytes) => Cell::Bytes(bytes.data().to_vec()),
                    Field::Date(days) => Cell::Date(*days),
                    Field::TimestampMicros(micros) => Cell::Timestamp(*micros),
                    Field::TimestampMillis(millis) => Cell::Timestamp(millis.saturating_mul(1000)),
                    other => Cell::Text(other.to_string()),
                })
                .collect();
            match &self.owner {
                Some((index, owner)) if cells.get(*index) != Some(owner) => {}
                _ => return Some(Ok(cells)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cell, Column, Kind, Rows, Writer};

    /// Rows written with every kind (and nulls in each) read back as the same cells, in order,
    /// across several row groups, and the workspace filter keeps only that workspace's rows: the
    /// archive drops a leaf only after its export verified, so the encoding must round-trip
    /// exactly.
    #[test]
    fn rows_round_trip_through_parquet() {
        let columns = vec![
            Column {
                name: "workspace_id".to_owned(),
                kind: Kind::Text,
            },
            Column {
                name: "n".to_owned(),
                kind: Kind::Int32,
            },
            Column {
                name: "big".to_owned(),
                kind: Kind::Int64,
            },
            Column {
                name: "flag".to_owned(),
                kind: Kind::Bool,
            },
            Column {
                name: "at".to_owned(),
                kind: Kind::Timestamp,
            },
            Column {
                name: "day".to_owned(),
                kind: Kind::Date,
            },
            Column {
                name: "raw".to_owned(),
                kind: Kind::Bytes,
            },
        ];
        let row = |index: i64| {
            let workspace = if index % 2 == 0 { "a" } else { "b" };
            if index % 7 == 0 {
                return vec![
                    Cell::Text(workspace.to_owned()),
                    Cell::Null,
                    Cell::Null,
                    Cell::Null,
                    Cell::Null,
                    Cell::Null,
                    Cell::Null,
                ];
            }
            vec![
                Cell::Text(workspace.to_owned()),
                Cell::Int(index),
                Cell::Int(index * 1_000_000_000),
                Cell::Bool(index % 3 == 0),
                Cell::Timestamp(1_790_000_000_000_000 + index),
                Cell::Date(20_000 + i32::try_from(index).unwrap()),
                Cell::Bytes(vec![1, 2, u8::try_from(index % 256).unwrap()]),
            ]
        };
        let total = i64::try_from(super::ROW_GROUP).unwrap() + 10;
        let file = tempfile();
        let mut writer = Writer::new(file.try_clone().unwrap(), columns).unwrap();
        for index in 0..total {
            writer.push(row(index)).unwrap();
        }
        assert_eq!(writer.finish().unwrap(), total);

        let rows = Rows::open(file.try_clone().unwrap(), None).unwrap();
        assert_eq!(rows.names().len(), 7);
        let mut seen = 0;
        for cells in rows {
            assert_eq!(cells.unwrap(), row(seen));
            seen += 1;
        }
        assert_eq!(seen, total);
        let only_b: Vec<Vec<Cell>> = Rows::open(file, Some("b"))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(i64::try_from(only_b.len()).unwrap(), total / 2);
        assert!(
            only_b
                .iter()
                .all(|cells| cells.first() == Some(&Cell::Text("b".to_owned())))
        );
        assert_eq!(Cell::Date(0).to_text(), "1970-01-01");
        assert_eq!(Cell::Timestamp(0).to_text(), "1970-01-01T00:00:00Z");
    }

    /// An anonymous file in the system's temporary directory, removed when closed.
    fn tempfile() -> std::fs::File {
        let path = std::env::temp_dir().join(format!("parquet-{}.parquet", uuid::Uuid::now_v7()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        file
    }
}
