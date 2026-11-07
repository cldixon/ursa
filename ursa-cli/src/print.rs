//! Writing results in the `-f` formats.
//!
//! The table format is datafusion-cli's: Arrow's pretty printer (ASCII boxes,
//! values left-aligned), with the column headers still printed when there are
//! no rows. Adapted from datafusion-cli's `print_format.rs` (Apache-2.0).
//! CSV, TSV, NDJSON and Arrow stream batch by batch; table and JSON (an array)
//! are written once the result is complete.
//!
//! A table shows at most [`DEFAULT_MAX_ROWS`] rows (`.maxrows N`,
//! `--max-rows N`; 0 shows every row), as DuckDB's does: a longer result shows
//! its first half and its last half, three rows of `·` between them, and a
//! footer that counts the rows (`39,650,447 rows (40 shown)`). The other
//! formats write every row.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arrow::array::{ArrayRef, StringArray};
use arrow::compute::concat_batches;
use arrow::csv::WriterBuilder as CsvWriterBuilder;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::error::ArrowError;
use arrow::ipc::writer::StreamWriter;
use arrow::json::{ArrayWriter, LineDelimitedWriter};
use arrow::record_batch::RecordBatch;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use arrow::util::pretty::pretty_format_batches_with_options;
use datafusion::error::{DataFusionError, Result};
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

/// How many rows a table shows by default, as in DuckDB.
pub const DEFAULT_MAX_ROWS: usize = 40;

/// Write every batch of `stream` to `out` as `format`. A table shows at most
/// `max_rows` rows (0: every row); see the module's docs.
pub async fn write_stream<W: Write>(
    format: OutputFormat,
    mut stream: SendableRecordBatchStream,
    out: &mut W,
    options: &FormatOptions<'_>,
    max_rows: usize,
) -> Result<()> {
    let schema = stream.schema();
    match format {
        OutputFormat::Table => {
            let mut rows = TableRows::new(max_rows);
            while let Some(batch) = stream.next().await {
                rows.push(batch?);
            }
            writeln!(out, "{}", rows.render(schema, options)?)?;
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

/// The rows a table shows: every row, or, past `max_rows`, the first
/// `ceil(max_rows / 2)` and the last `floor(max_rows / 2)`, and how many rows
/// there were. The rows in between are counted and dropped as they arrive.
struct TableRows {
    max_rows: usize,
    /// The first rows: up to `top()` of them, or every row with no cap.
    head: Vec<RecordBatch>,
    head_rows: usize,
    /// Rows after `head`: the last `bottom()` at least, older ones dropped.
    tail: VecDeque<RecordBatch>,
    tail_rows: usize,
    total: usize,
}

impl TableRows {
    fn new(max_rows: usize) -> Self {
        Self {
            max_rows,
            head: Vec::new(),
            head_rows: 0,
            tail: VecDeque::new(),
            tail_rows: 0,
            total: 0,
        }
    }

    fn top(&self) -> usize {
        self.max_rows.div_ceil(2)
    }

    fn bottom(&self) -> usize {
        self.max_rows / 2
    }

    fn push(&mut self, batch: RecordBatch) {
        let n = batch.num_rows();
        if n == 0 {
            return;
        }
        self.total += n;
        let room = if self.max_rows == 0 {
            n
        } else {
            self.top() - self.head_rows
        };
        if room > 0 {
            let taken = room.min(n);
            self.head.push(batch.slice(0, taken));
            self.head_rows += taken;
            if taken < n {
                self.push_tail(batch.slice(taken, n - taken));
            }
        } else {
            self.push_tail(batch);
        }
    }

    fn push_tail(&mut self, batch: RecordBatch) {
        self.tail_rows += batch.num_rows();
        self.tail.push_back(batch);
        // Keep only the batches the last `bottom()` rows are in.
        while let Some(front) = self.tail.front() {
            if self.tail_rows - front.num_rows() < self.bottom() {
                break;
            }
            self.tail_rows -= front.num_rows();
            self.tail.pop_front();
        }
    }

    /// The table, with a footer when rows were left out.
    fn render(self, schema: SchemaRef, options: &FormatOptions<'_>) -> Result<String> {
        if self.max_rows == 0 || self.total <= self.max_rows {
            let mut batches = self.head;
            batches.extend(self.tail);
            if batches.is_empty() {
                batches.push(RecordBatch::new_empty(schema));
            }
            return Ok(pretty_format_batches_with_options(&batches, options)?.to_string());
        }
        let head = concat_batches(&schema, &self.head)?;
        let tail = concat_batches(&schema, &self.tail)?;
        let tail = tail.slice(tail.num_rows() - self.bottom(), self.bottom());
        // Each value as the table would show it, so the left-out rows can be
        // three rows of `·`.
        let mut columns: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
        for i in 0..schema.fields().len() {
            let mut values: Vec<String> = Vec::with_capacity(self.max_rows + 3);
            let formatter = ArrayFormatter::try_new(head.column(i).as_ref(), options)?;
            values.extend((0..head.num_rows()).map(|row| formatter.value(row).to_string()));
            values.extend(std::iter::repeat_n("·".to_string(), 3));
            let formatter = ArrayFormatter::try_new(tail.column(i).as_ref(), options)?;
            values.extend((0..tail.num_rows()).map(|row| formatter.value(row).to_string()));
            columns.push(Arc::new(StringArray::from(values)));
        }
        let fields: Vec<Field> = schema
            .fields()
            .iter()
            .map(|f| Field::new(f.name(), DataType::Utf8, true))
            .collect();
        let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)?;
        let table = pretty_format_batches_with_options(&[batch], options)?;
        Ok(format!(
            "{table}\n{} rows ({} shown)",
            thousands(self.total),
            thousands(head.num_rows() + tail.num_rows())
        ))
    }
}

/// `n` with a comma between each group of three digits.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
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

#[cfg(test)]
mod tests {
    use arrow::array::Int64Array;

    use super::*;

    /// `n` rows of (x, s), with s null for odd x, in batches of `size`.
    fn batches(n: i64, size: i64) -> (SchemaRef, Vec<RecordBatch>) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("x", DataType::Int64, false),
            Field::new("s", DataType::Utf8, true),
        ]));
        let mut out = Vec::new();
        let mut start = 0;
        while start < n {
            let end = (start + size).min(n);
            let x: Vec<i64> = (start..end).collect();
            let s: Vec<Option<String>> = x
                .iter()
                .map(|x| (x % 2 == 0).then(|| format!("v{x}")))
                .collect();
            out.push(
                RecordBatch::try_new(
                    schema.clone(),
                    vec![
                        Arc::new(Int64Array::from(x)),
                        Arc::new(StringArray::from(s)),
                    ],
                )
                .unwrap(),
            );
            start = end;
        }
        (schema, out)
    }

    fn render(n: i64, size: i64, max_rows: usize) -> String {
        let (schema, batches) = batches(n, size);
        let mut rows = TableRows::new(max_rows);
        for batch in batches {
            rows.push(batch);
        }
        let options = FormatOptions::default().with_null("NULL");
        rows.render(schema, &options).unwrap()
    }

    #[test]
    fn the_first_and_last_rows_whatever_the_batches() {
        let expected = "\
+-----+------+
| x   | s    |
+-----+------+
| 0   | v0   |
| 1   | NULL |
| 2   | v2   |
| ·   | ·    |
| ·   | ·    |
| ·   | ·    |
| 998 | v998 |
| 999 | NULL |
+-----+------+
1,000 rows (5 shown)";
        for size in [1, 2, 3, 7, 999, 1000] {
            assert_eq!(render(1000, size, 5), expected, "batches of {size}");
        }
    }

    #[test]
    fn up_to_the_cap_or_with_none_every_row() {
        for (n, max_rows) in [(5, 5), (4, 5), (0, 5), (50, 0)] {
            let out = render(n, 3, max_rows);
            assert_eq!(out.lines().count(), 3 + n as usize + 1, "{out}");
            assert!(!out.contains('·') && !out.contains("shown"), "{out}");
        }
        // One row over.
        let out = render(6, 4, 5);
        assert!(out.ends_with("6 rows (5 shown)"), "{out}");
        let out = render(3, 1, 1);
        assert!(
            out.contains("| 0 | v0 |\n| · | ·  |\n| · | ·  |\n| · | ·  |\n+"),
            "{out}"
        );
    }

    #[test]
    fn thousands_separators() {
        for (n, shown) in [
            (0, "0"),
            (999, "999"),
            (1000, "1,000"),
            (100_000, "100,000"),
            (39_650_447, "39,650,447"),
        ] {
            assert_eq!(thousands(n), shown);
        }
    }
}
