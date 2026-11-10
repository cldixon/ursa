//! Writing results in the `-f` formats.
//!
//! The table format is DuckDB's box table, drawn by [`ursa_shell::table`] (the
//! browser shell draws the same): each column's type under its name, numbers
//! to the right, and at most [`DEFAULT_MAX_ROWS`] rows (`.maxrows N`,
//! `--max-rows N`; 0 shows every row) and as many columns as fit the
//! terminal. CSV, TSV, NDJSON and Arrow stream batch by batch, and write every
//! row; table and JSON (an array) are written once the result is complete.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use arrow::csv::WriterBuilder as CsvWriterBuilder;
use arrow::datatypes::SchemaRef;
use arrow::error::ArrowError;
use arrow::ipc::writer::StreamWriter;
use arrow::json::{ArrayWriter, LineDelimitedWriter};
use arrow::record_batch::RecordBatch;
use arrow::util::display::FormatOptions;
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::StreamExt;
use ursa_shell::table::{Rows, Style};

pub use ursa_shell::table::DEFAULT_MAX_ROWS;

/// How results are written to stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
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
            // arrow-csv turns an I/O error into text (`CsvError("Broken
            // pipe")`), so the sink records a broken pipe itself, and it is
            // returned as one: `ursa --format csv … | head` then ends quietly.
            let mut sink = BrokenPipeAware::new(&mut *out);
            let mut writer = CsvWriterBuilder::new()
                .with_header(true)
                .with_delimiter(delimiter)
                .build(&mut sink);
            let mut wrote = false;
            let mut written = Ok(());
            while let Some(batch) = stream.next().await {
                written = writer.write(&batch?);
                if written.is_err() {
                    break;
                }
                wrote = true;
            }
            if written.is_ok() && !wrote {
                // An empty batch still writes the header row.
                written = writer.write(&RecordBatch::new_empty(schema));
            }
            drop(writer);
            if sink.broken {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into());
            }
            written?;
        }
        OutputFormat::Json => {
            let mut writer = ArrayWriter::new(&mut *out);
            while let Some(batch) = stream.next().await {
                writer.write(&batch?)?;
            }
            writer.finish()?;
            drop(writer);
            writeln!(out)?;
        }
        OutputFormat::Ndjson => {
            let mut writer = LineDelimitedWriter::new(&mut *out);
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

/// A writer that remembers whether a write failed with a broken pipe, for
/// writers (arrow-csv) that do not pass the `io::Error` through.
struct BrokenPipeAware<W> {
    inner: W,
    broken: bool,
}

impl<W: Write> BrokenPipeAware<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            broken: false,
        }
    }

    fn note<T>(&mut self, result: std::io::Result<T>) -> std::io::Result<T> {
        if let Err(e) = &result {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                self.broken = true;
            }
        }
        result
    }
}

impl<W: Write> Write for BrokenPipeAware<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let result = self.inner.write(buf);
        self.note(result)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let result = self.inner.flush();
        self.note(result)
    }
}

/// Set once SIGINT has cancelled a script (see `cancel_on_interrupt` in
/// `main.rs`).
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Stop [`Interruptible`] writers from writing anything more.
pub fn interrupt() {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// A writer that drops everything once [`interrupt`] is called, so a script
/// cancelled with Ctrl-C prints nothing more: not the rest of a result, and
/// not what is still buffered in front of this writer.
pub struct Interruptible<W>(pub W);

impl<W: Write> Write for Interruptible<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Ok(buf.len());
        }
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.0.flush()
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
