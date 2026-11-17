//! Writing results in the `-f` formats.
//!
//! The table format is DuckDB's box table, drawn by [`crate::table`] (the
//! browser shell draws the same): each column's type under its name, numbers
//! to the right, and at most [`DEFAULT_MAX_ROWS`] rows (`.maxrows N`,
//! `--max-rows N`; 0 shows every row) and as many columns as fit the
//! terminal. CSV, TSV, NDJSON and Arrow stream batch by batch, and write every
//! row; table and JSON (an array) are written once the result is complete.

use std::io::Write;

use crate::table::{Rows, Style};
use arrow::array::Array;
use arrow::datatypes::SchemaRef;
use arrow::error::ArrowError;
use arrow::ipc::writer::StreamWriter;
use arrow::json::writer::{JsonArray, LineDelimited};
use arrow::json::{Writer as JsonWriter, WriterBuilder as JsonWriterBuilder};
use arrow::record_batch::RecordBatch;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::StreamExt;

pub use crate::table::DEFAULT_MAX_ROWS;

/// How results are written to stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum OutputFormat {
    /// ASCII table.
    Table,
    /// Comma-separated values, with a header row.
    Csv,
    /// Tab-separated values, with a header row.
    Tsv,
    /// One JSON array of row objects.
    Json,
    /// One JSON object per line.
    Ndjson,
    /// An Arrow IPC stream.
    Arrow,
}

impl OutputFormat {
    /// Every format, in the order `--help` lists them.
    pub const ALL: [OutputFormat; 6] = [
        OutputFormat::Table,
        OutputFormat::Csv,
        OutputFormat::Tsv,
        OutputFormat::Json,
        OutputFormat::Ndjson,
        OutputFormat::Arrow,
    ];

    /// The format's name, as `--format` and `.mode` take it.
    pub fn name(self) -> &'static str {
        match self {
            OutputFormat::Table => "table",
            OutputFormat::Csv => "csv",
            OutputFormat::Tsv => "tsv",
            OutputFormat::Json => "json",
            OutputFormat::Ndjson => "ndjson",
            OutputFormat::Arrow => "arrow",
        }
    }

    /// The format named `name`, in any case.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|f| f.name().eq_ignore_ascii_case(name))
    }
}

/// How a table is drawn: how many rows it shows (0: every row), and how.
#[derive(Debug, Clone, Copy)]
pub struct TableStyle {
    pub max_rows: usize,
    pub style: Style,
}

/// Write every batch of `stream` to `out` as `format`, a table as `table`
/// says; see the module's docs.
pub async fn write_stream<W: Write>(
    format: OutputFormat,
    mut stream: SendableRecordBatchStream,
    out: &mut W,
    options: &FormatOptions<'_>,
    table: TableStyle,
) -> Result<()> {
    let schema = stream.schema();
    match format {
        OutputFormat::Table => {
            let mut rows = Rows::new(table.max_rows);
            while let Some(batch) = stream.next().await {
                rows.push(batch?);
            }
            write!(
                out,
                "{}",
                rows.finish(&schema, options)?.render(table.style)
            )?;
        }
        OutputFormat::Csv | OutputFormat::Tsv => {
            let delimiter = if format == OutputFormat::Csv {
                b','
            } else {
                b'\t'
            };
            let mut writer = DelimitedWriter::new(&mut *out, delimiter, &schema)?;
            while let Some(batch) = stream.next().await {
                writer.write(&batch?)?;
            }
        }
        OutputFormat::Json => {
            let mut writer = json_array_writer(&mut *out);
            while let Some(batch) = stream.next().await {
                writer.write(&batch?)?;
            }
            writer.finish()?;
            drop(writer);
            writeln!(out)?;
        }
        OutputFormat::Ndjson => {
            let mut writer = JsonWriterBuilder::new()
                .with_explicit_nulls(true)
                .build::<_, LineDelimited>(&mut *out);
            while let Some(batch) = stream.next().await {
                writer.write(&batch?)?;
            }
            writer.finish()?;
        }
        OutputFormat::Arrow => {
            let mut writer = StreamWriter::try_new(&mut *out, &schema)?;
            while let Some(batch) = stream.next().await {
                writer.write(&batch?)?;
            }
            writer.finish()?;
        }
    }
    out.flush()?;
    Ok(())
}

/// A JSON writer that writes `null` for a null, as DuckDB does, so every row
/// object has every column, in column order.
pub fn json_array_writer<W: Write>(out: W) -> JsonWriter<W, JsonArray> {
    JsonWriterBuilder::new()
        .with_explicit_nulls(true)
        .build::<_, JsonArray>(out)
}

/// A CSV or TSV writer (`delimiter` `,` or tab), with a header row, written
/// as DuckDB writes them:
///
/// - a null is an empty field and an empty string is `""`, so the two can be
///   told apart;
/// - a list, struct or map is its text form (`[1, 2]`, `{a: 1}`), with
///   `NULL` for a null inside it, quoted where CSV needs it;
/// - a field is quoted when it holds the delimiter, a quote or a line break,
///   and a quote in it is doubled.
pub struct DelimitedWriter<W> {
    out: W,
    delimiter: u8,
    line: Vec<u8>,
}

impl<W: Write> DelimitedWriter<W> {
    /// A writer to `out`, which writes the header row of `schema` now.
    pub fn new(out: W, delimiter: u8, schema: &SchemaRef) -> std::result::Result<Self, ArrowError> {
        let mut writer = Self {
            out,
            delimiter,
            line: Vec::with_capacity(1024),
        };
        for (i, field) in schema.fields().iter().enumerate() {
            writer.field(i, Some(field.name()));
        }
        writer.end_line();
        writer.out.write_all(&writer.line)?;
        writer.line.clear();
        Ok(writer)
    }

    /// Write every row of `batch`.
    pub fn write(&mut self, batch: &RecordBatch) -> std::result::Result<(), ArrowError> {
        let options = FormatOptions::default().with_null("NULL");
        let formatters = batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &options))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // `logical_nulls`, not `is_null`: a column of type Null has no null
        // buffer, yet every value in it is null.
        let nulls: Vec<_> = batch.columns().iter().map(|c| c.logical_nulls()).collect();
        let mut text = String::new();
        for row in 0..batch.num_rows() {
            for (i, formatter) in formatters.iter().enumerate() {
                if nulls[i].as_ref().is_some_and(|n| n.is_null(row)) {
                    self.field(i, None);
                } else {
                    text.clear();
                    formatter.value(row).write(&mut text)?;
                    self.field(i, Some(&text));
                }
            }
            self.end_line();
            if self.line.len() >= 64 * 1024 {
                self.out.write_all(&self.line)?;
                self.line.clear();
            }
        }
        self.out.write_all(&self.line)?;
        self.line.clear();
        Ok(())
    }

    /// Add the `i`th field of a line to the buffer: `None` for a null.
    fn field(&mut self, i: usize, value: Option<&str>) {
        if i > 0 {
            self.line.push(self.delimiter);
        }
        let Some(value) = value else { return };
        let quote = value.is_empty()
            || value
                .bytes()
                .any(|b| b == self.delimiter || b == b'"' || b == b'\n' || b == b'\r');
        if quote {
            self.line.push(b'"');
            for b in value.bytes() {
                if b == b'"' {
                    self.line.push(b'"');
                }
                self.line.push(b);
            }
            self.line.push(b'"');
        } else {
            self.line.extend_from_slice(value.as_bytes());
        }
    }

    fn end_line(&mut self) {
        self.line.push(b'\n');
    }
}

/// Whether a result has anything to print. DDL and `SET` return no columns.
pub fn has_columns(schema: &SchemaRef) -> bool {
    !schema.fields().is_empty()
}

/// Whether `err` is a write to a reader that has gone (`ursa … | head`).
pub fn is_broken_pipe(err: &DataFusionError) -> bool {
    match err {
        DataFusionError::IoError(e) => e.kind() == std::io::ErrorKind::BrokenPipe,
        DataFusionError::ArrowError(e, _) => {
            matches!(e.as_ref(), ArrowError::IoError(_, e) if e.kind() == std::io::ErrorKind::BrokenPipe)
        }
        _ => false,
    }
}
