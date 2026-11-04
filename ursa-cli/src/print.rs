//! Writing results in the `-f` formats.
//!
//! The table format is datafusion-cli's: Arrow's pretty printer (ASCII boxes,
//! values left-aligned), with the column headers still printed when there are
//! no rows. Adapted from datafusion-cli's `print_format.rs` (Apache-2.0).
//! CSV, TSV, NDJSON and Arrow stream batch by batch; table and JSON (an array)
//! are written once the result is complete.

use std::io::Write;

use arrow::csv::WriterBuilder as CsvWriterBuilder;
use arrow::datatypes::SchemaRef;
use arrow::ipc::writer::StreamWriter;
use arrow::json::{ArrayWriter, LineDelimitedWriter};
use arrow::record_batch::RecordBatch;
use arrow::util::display::FormatOptions;
use arrow::util::pretty::pretty_format_batches_with_options;
use datafusion::error::Result;
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::StreamExt;

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

/// Write every batch of `stream` to `out` as `format`.
pub async fn write_stream<W: Write>(
    format: OutputFormat,
    mut stream: SendableRecordBatchStream,
    out: &mut W,
    options: &FormatOptions<'_>,
) -> Result<()> {
    let schema = stream.schema();
    match format {
        OutputFormat::Table => {
            let mut batches = Vec::new();
            while let Some(batch) = stream.next().await {
                let batch = batch?;
                if batch.num_rows() > 0 {
                    batches.push(batch);
                }
            }
            if batches.is_empty() {
                batches.push(RecordBatch::new_empty(schema));
            }
            writeln!(
                out,
                "{}",
                pretty_format_batches_with_options(&batches, options)?
            )?;
        }
        OutputFormat::Csv | OutputFormat::Tsv => {
            let delimiter = if format == OutputFormat::Csv {
                b','
            } else {
                b'\t'
            };
            let mut writer = CsvWriterBuilder::new()
                .with_header(true)
                .with_delimiter(delimiter)
                .build(&mut *out);
            let mut wrote = false;
            while let Some(batch) = stream.next().await {
                writer.write(&batch?)?;
                wrote = true;
            }
            if !wrote {
                // An empty batch still writes the header row.
                writer.write(&RecordBatch::new_empty(schema))?;
            }
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

/// Whether a result has anything to print. DDL and `SET` return no columns.
pub fn has_columns(schema: &SchemaRef) -> bool {
    !schema.fields().is_empty()
}
