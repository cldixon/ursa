//! Writing results in the `-f` formats.
//!
//! The table format is DuckDB's box table, drawn by [`crate::table`] (the
//! browser shell draws the same): each column's type under its name, numbers
//! to the right, and at most [`DEFAULT_MAX_ROWS`] rows (`.maxrows N`,
//! `--max-rows N`; 0 shows every row) and as many columns as fit the
//! terminal. CSV, TSV, NDJSON, line and Arrow stream batch by batch, and write
//! every row; table, Markdown and JSON (an array) are written once the result
//! is complete.
//!
//! Nothing is written until the first row arrives, or until the result ends
//! with none: a statement that fails before its first row writes nothing to
//! stdout, not even its CSV header, and an empty result still writes its
//! header (as DuckDB does).
//!
//! Markdown and line are DuckDB's `.mode markdown` and `.mode line`:
//!
//! ```text
//! | name  | score |       name = alice
//! |-------|------:|      score = 0.25
//! | alice | 0.25  |
//! | bob   | NULL  |       name = bob
//!                        score = NULL
//! ```
//!
//! A Markdown table's header is centred over its column and a number column's
//! separator ends in `:` (right-aligned where it is rendered); a `|` in a value
//! is written `\|` and a line break `\n`, so every row stays one table row.
//! Line mode right-aligns the names (at least 5 wide) and puts a blank line
//! between rows. A null is `NULL` in both.

use std::io::Write;

use crate::table::{Rows, Style};
use arrow::array::Array;
use arrow::datatypes::{DataType, SchemaRef};
use arrow::error::ArrowError;
use arrow::ipc::writer::StreamWriter;
use arrow::json::writer::{JsonArray, LineDelimited};
use arrow::json::{Writer as JsonWriter, WriterBuilder as JsonWriterBuilder};
use arrow::record_batch::RecordBatch;
use arrow::util::display::FormatOptions;
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::StreamExt;
use unicode_width::UnicodeWidthStr;

pub use crate::table::DEFAULT_MAX_ROWS;

/// How results are written to stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum OutputFormat {
    /// ASCII table.
    #[cfg_attr(feature = "clap", value(aliases = ["duckbox", "box"]))]
    Table,
    /// Comma-separated values, with a header row.
    Csv,
    /// Tab-separated values, with a header row.
    Tsv,
    /// One JSON array of row objects.
    Json,
    /// One JSON object per line.
    #[cfg_attr(feature = "clap", value(alias = "jsonlines"))]
    Ndjson,
    /// A Markdown table (DuckDB's `.mode markdown`).
    Markdown,
    /// One `column = value` line per column, a blank line between rows
    /// (DuckDB's `.mode line`).
    #[cfg_attr(feature = "clap", value(alias = "lines"))]
    Line,
    /// An Arrow IPC stream.
    Arrow,
}

impl OutputFormat {
    /// Every format, in the order `--help` lists them.
    pub const ALL: [OutputFormat; 8] = [
        OutputFormat::Table,
        OutputFormat::Csv,
        OutputFormat::Tsv,
        OutputFormat::Json,
        OutputFormat::Ndjson,
        OutputFormat::Markdown,
        OutputFormat::Line,
        OutputFormat::Arrow,
    ];

    /// DuckDB's names for formats Ursa has under another name: `duckbox`
    /// (DuckDB's default) and `box` are the table, `jsonlines` is NDJSON,
    /// and `lines` is line.
    pub const ALIASES: [(&'static str, OutputFormat); 4] = [
        ("duckbox", OutputFormat::Table),
        ("box", OutputFormat::Table),
        ("jsonlines", OutputFormat::Ndjson),
        ("lines", OutputFormat::Line),
    ];

    /// The format's name, as `--format` and `.mode` take it.
    pub fn name(self) -> &'static str {
        match self {
            OutputFormat::Table => "table",
            OutputFormat::Csv => "csv",
            OutputFormat::Tsv => "tsv",
            OutputFormat::Json => "json",
            OutputFormat::Ndjson => "ndjson",
            OutputFormat::Markdown => "markdown",
            OutputFormat::Line => "line",
            OutputFormat::Arrow => "arrow",
        }
    }

    /// The format named `name`, or one of its DuckDB names
    /// ([`Self::ALIASES`]), in any case.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|f| f.name().eq_ignore_ascii_case(name))
            .or_else(|| {
                Self::ALIASES
                    .into_iter()
                    .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
                    .map(|(_, f)| f)
            })
    }
}

/// How a table is drawn: how many rows it shows (0: every row), and how;
/// and whether CSV and TSV start with a header row (`.headers on|off`).
#[derive(Debug, Clone, Copy)]
pub struct TableStyle {
    pub max_rows: usize,
    pub style: Style,
    pub headers: bool,
}

/// The first batch of `stream` that has rows, or `None` when it ends with
/// none. Batches with no rows before it are dropped. An error before the
/// first row is returned before anything is written.
async fn first_rows(stream: &mut SendableRecordBatchStream) -> Result<Option<RecordBatch>> {
    while let Some(batch) = stream.next().await {
        let batch = batch?;
        if batch.num_rows() > 0 {
            return Ok(Some(batch));
        }
    }
    Ok(None)
}

/// Write every batch of `stream` to `out` as `format`, a table as `table`
/// says; see the module's docs. Nothing is written before the first row
/// arrives or the stream ends, so a statement that fails before its first
/// row writes nothing, not even a header.
pub async fn write_stream<W: Write>(
    format: OutputFormat,
    mut stream: SendableRecordBatchStream,
    out: &mut W,
    options: &FormatOptions<'_>,
    table: TableStyle,
) -> Result<()> {
    let schema = stream.schema();
    let first = first_rows(&mut stream).await?;
    // The first batch, then the rest of the stream.
    let mut stream = futures::stream::iter(first.map(Ok)).chain(stream);
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
            let mut writer =
                DelimitedWriter::with_header(&mut *out, delimiter, &schema, table.headers)?;
            while let Some(batch) = stream.next().await {
                writer.write(&batch?)?;
            }
        }
        OutputFormat::Markdown => {
            let mut batches = Vec::new();
            while let Some(batch) = stream.next().await {
                batches.push(batch?);
            }
            write!(out, "{}", markdown(&schema, &batches, options)?)?;
        }
        OutputFormat::Line => {
            let mut writer = LineWriter::new(&schema);
            while let Some(batch) = stream.next().await {
                writer.write(&mut *out, &batch?, options)?;
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
///   `NULL` for a null inside it and a string inside it in single quotes
///   where it needs them (`['a, b', c]`, see [`crate::text`]), quoted where
///   CSV needs it;
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
        Self::with_header(out, delimiter, schema, true)
    }

    /// A writer to `out`, which writes the header row of `schema` now if
    /// `header` (`.headers on`).
    pub fn with_header(
        out: W,
        delimiter: u8,
        schema: &SchemaRef,
        header: bool,
    ) -> std::result::Result<Self, ArrowError> {
        let mut writer = Self {
            out,
            delimiter,
            line: Vec::with_capacity(1024),
        };
        if header {
            for (i, field) in schema.fields().iter().enumerate() {
                writer.field(i, Some(field.name()));
            }
            writer.end_line();
            writer.out.write_all(&writer.line)?;
            writer.line.clear();
        }
        Ok(writer)
    }

    /// Write every row of `batch`.
    pub fn write(&mut self, batch: &RecordBatch) -> std::result::Result<(), ArrowError> {
        let options = FormatOptions::default().with_null("NULL");
        let formatters = batch
            .columns()
            .iter()
            .map(|c| crate::text::formatter(c.as_ref(), &options))
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

/// The text of each value of `batch`, column by column: `NULL` for a null,
/// nested values as [`crate::text`] writes them, and control characters as
/// escapes ([`crate::table::visible`]).
fn texts(
    batch: &RecordBatch,
    options: &FormatOptions<'_>,
) -> std::result::Result<Vec<Vec<String>>, ArrowError> {
    let mut columns = Vec::with_capacity(batch.num_columns());
    for column in batch.columns() {
        let formatter = crate::text::formatter(column.as_ref(), options)?;
        // `logical_nulls`, not `is_null`: a column of type Null has no null
        // buffer, yet every value in it is null.
        let nulls = column.logical_nulls();
        let mut values = Vec::with_capacity(batch.num_rows());
        let mut text = String::new();
        for row in 0..batch.num_rows() {
            if nulls.as_ref().is_some_and(|n| n.is_null(row)) {
                values.push("NULL".to_string());
            } else {
                text.clear();
                formatter.value(row).write(&mut text)?;
                values.push(crate::table::visible(&text).into_owned());
            }
        }
        columns.push(values);
    }
    Ok(columns)
}

/// Whether a Markdown table right-aligns a column of type `t`: DuckDB's
/// `.mode markdown` does so for integers, floats and decimals.
fn is_number(t: &DataType) -> bool {
    match t {
        DataType::Dictionary(_, values) => is_number(values),
        t => t.is_numeric(),
    }
}

/// A value as a Markdown table cell: a `|` would end the cell, and a line
/// break the row.
fn markdown_cell(text: &str) -> String {
    text.replace('|', "\\|")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

/// `batches` as a Markdown table, as DuckDB's `.mode markdown` writes one:
///
/// ```text
/// | name  | score |
/// |-------|------:|
/// | alice | 0.25  |
/// ```
///
/// Each column is as wide as its widest value or name; a name is centred, a
/// value is written from the left, and a number column's separator ends in
/// `:`. A result with no rows is its header and separator.
pub fn markdown(
    schema: &SchemaRef,
    batches: &[RecordBatch],
    options: &FormatOptions<'_>,
) -> std::result::Result<String, ArrowError> {
    let names: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| markdown_cell(&crate::table::visible(f.name())))
        .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for batch in batches {
        let columns = texts(batch, options)?;
        for row in 0..batch.num_rows() {
            rows.push(columns.iter().map(|c| markdown_cell(&c[row])).collect());
        }
    }
    let widths: Vec<usize> = (0..names.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].width())
                .chain([names[i].width()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    out.push('|');
    for (name, &w) in names.iter().zip(&widths) {
        let pad = w - name.width();
        out.push_str(&format!(
            " {}{name}{} |",
            " ".repeat(pad / 2),
            " ".repeat(pad - pad / 2)
        ));
    }
    out.push('\n');
    out.push('|');
    for (field, &w) in schema.fields().iter().zip(&widths) {
        if is_number(field.data_type()) {
            out.push_str(&format!("{}:|", "-".repeat(w + 1)));
        } else {
            out.push_str(&format!("{}|", "-".repeat(w + 2)));
        }
    }
    out.push('\n');
    for row in &rows {
        out.push('|');
        for (value, &w) in row.iter().zip(&widths) {
            out.push_str(&format!(" {value}{} |", " ".repeat(w - value.width())));
        }
        out.push('\n');
    }
    Ok(out)
}

/// Writes rows as DuckDB's `.mode line` does: one `name = value` line per
/// column, the names right-aligned to the longest (at least 5 wide), and a
/// blank line between rows.
pub struct LineWriter {
    names: Vec<String>,
    width: usize,
    first: bool,
}

impl LineWriter {
    pub fn new(schema: &SchemaRef) -> Self {
        let names: Vec<String> = schema
            .fields()
            .iter()
            .map(|f| crate::table::visible(f.name()).into_owned())
            .collect();
        let width = names.iter().map(|n| n.width()).max().unwrap_or(0).max(5);
        Self {
            names,
            width,
            first: true,
        }
    }

    /// Write every row of `batch` to `out`.
    pub fn write<W: Write>(
        &mut self,
        out: &mut W,
        batch: &RecordBatch,
        options: &FormatOptions<'_>,
    ) -> std::result::Result<(), ArrowError> {
        let columns = texts(batch, options)?;
        let mut text = String::new();
        for row in 0..batch.num_rows() {
            if !self.first {
                text.push('\n');
            }
            self.first = false;
            for (name, column) in self.names.iter().zip(&columns) {
                let pad = self.width - name.width();
                text.push_str(&format!("{}{name} = {}\n", " ".repeat(pad), column[row]));
            }
        }
        out.write_all(text.as_bytes())?;
        Ok(())
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
