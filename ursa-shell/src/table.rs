//! Results as DuckDB draws them: a box around the table, each column's name
//! with its type under it, numbers to the right and everything else to the
//! left, and `NULL` for a null. The types are Arrow's, named as DataFusion's
//! `DESCRIBE` names them (see [`type_name`]).
//!
//! ```text
//! ┌───────┬─────────┐
//! │  id   │  club   │
//! │ Int64 │  Utf8   │
//! ├───────┼─────────┤
//! │     0 │ Mr. Hi  │
//! │    33 │ Officer │
//! └───────┴─────────┘
//! ```
//!
//! A result longer than `max_rows` shows its first and last rows with three
//! rows of `·` between them; a result wider than the screen shows its first
//! and last columns with a `…` column between them, after cutting long values
//! short. Either way a footer counts what there was:
//!
//! ```text
//!   39,650,447 rows (40 shown)                     17 columns (10 shown)
//! ```
//!
//! [`Rows`] collects a result batch by batch and keeps only the rows it will
//! show; [`Table::render`] draws them for a given width.

use std::collections::VecDeque;

use arrow::array::{Array, RecordBatch};
use arrow::datatypes::{DataType, SchemaRef};
use arrow::error::ArrowError;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// How many rows a table shows by default, as in DuckDB.
pub const DEFAULT_MAX_ROWS: usize = 40;

/// How wide a column may be when the table is too wide for the screen
/// (DuckDB's `max_col_width` default).
const NARROW_COLUMN: usize = 20;

/// The rows a table shows: every row, or, past `max_rows`, the first
/// `ceil(max_rows / 2)` and the last `floor(max_rows / 2)`, and how many rows
/// there were. The rows in between are counted and dropped as they arrive.
pub struct Rows {
    max_rows: usize,
    /// The first rows: up to `top()` of them, or every row with no cap.
    head: Vec<RecordBatch>,
    head_rows: usize,
    /// Rows after `head`: the last `bottom()` at least, older ones dropped.
    tail: VecDeque<RecordBatch>,
    tail_rows: usize,
    total: usize,
}

impl Rows {
    /// Keep at most `max_rows` rows (0: every row).
    pub fn new(max_rows: usize) -> Self {
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

    /// Add the next batch of the result.
    pub fn push(&mut self, batch: RecordBatch) {
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

    /// The table these rows make, each value formatted with `options`.
    pub fn finish(self, schema: &SchemaRef, options: &FormatOptions) -> Result<Table, ArrowError> {
        let elided = self.max_rows > 0 && self.total > self.max_rows;
        let mut rows = Vec::new();
        let mut cells = |batches: &mut dyn Iterator<Item = &RecordBatch>,
                         skip: usize|
         -> Result<(), ArrowError> {
            let mut skip = skip;
            for batch in batches {
                if skip >= batch.num_rows() {
                    skip -= batch.num_rows();
                    continue;
                }
                let formatters = batch
                    .columns()
                    .iter()
                    .map(|c| ArrayFormatter::try_new(c.as_ref(), options))
                    .collect::<Result<Vec<_>, _>>()?;
                for row in skip..batch.num_rows() {
                    rows.push(
                        batch
                            .columns()
                            .iter()
                            .zip(&formatters)
                            .map(|(column, f)| {
                                (!column.is_null(row)).then(|| f.value(row).to_string())
                            })
                            .collect(),
                    );
                }
                skip = 0;
            }
            Ok(())
        };
        cells(&mut self.head.iter(), 0)?;
        let gap = elided.then_some(self.head_rows);
        if elided {
            // The tail batches may hold more than the last `bottom()` rows.
            cells(&mut self.tail.iter(), self.tail_rows - self.bottom())?;
        } else {
            cells(&mut self.tail.iter(), 0)?;
        }
        Ok(Table::new(schema, rows, gap, self.total))
    }
}

/// A result ready to draw: its columns, the rows it shows, and where rows
/// were left out.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    columns: Vec<Column>,
    /// Each row's values, `None` for a null.
    rows: Vec<Vec<Option<String>>>,
    /// The rows were cut short before this row: the `·` rows go here.
    gap: Option<usize>,
    /// How many rows the result had.
    total: usize,
}

#[derive(Debug, Clone, PartialEq)]
struct Column {
    name: String,
    kind: String,
    numeric: bool,
}

/// How to draw a [`Table`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Style {
    /// The widest the table may be, in terminal columns; 0 for no limit.
    pub max_width: usize,
    /// Dim the types, nulls, `·` rows and footer with ANSI escapes.
    pub color: bool,
}

const DIM: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

impl Table {
    /// A table of `rows` (already formatted), under the columns of `schema`.
    /// `gap` is the row the left-out rows were before, if any; `total` is
    /// how many rows the result had.
    pub fn new(
        schema: &SchemaRef,
        rows: Vec<Vec<Option<String>>>,
        gap: Option<usize>,
        total: usize,
    ) -> Self {
        let columns = schema
            .fields()
            .iter()
            .map(|f| Column {
                name: f.name().clone(),
                kind: type_name(f.data_type()),
                numeric: is_numeric(f.data_type()),
            })
            .collect();
        Self {
            columns,
            rows,
            gap,
            total,
        }
    }

    /// How many rows the result had.
    pub fn total_rows(&self) -> usize {
        self.total
    }

    /// The table as text, one line after another, each ending in `\n`.
    pub fn render(&self, style: Style) -> String {
        let n = self.columns.len();
        if n == 0 {
            return String::new();
        }
        let cells: Vec<Vec<Option<String>>> = self
            .rows
            .iter()
            .map(|row| row.iter().map(|v| v.as_deref().map(escape)).collect())
            .collect();
        let mut widths: Vec<usize> = (0..n)
            .map(|i| {
                let values = cells
                    .iter()
                    .map(|r| r[i].as_deref().map_or(4, UnicodeWidthStr::width));
                values
                    .chain([
                        self.columns[i].name.width(),
                        self.columns[i].kind.width(),
                        1,
                    ])
                    .max()
                    .unwrap_or(1)
            })
            .collect();
        let width = |widths: &[usize], shown: &[usize], elided: bool| {
            shown.iter().map(|&i| widths[i] + 3).sum::<usize>() + 1 + if elided { 4 } else { 0 }
        };

        // Too wide: cut long columns short, then leave out middle columns.
        let mut shown: Vec<usize> = (0..n).collect();
        let mut elided = false;
        if style.max_width > 0 && width(&widths, &shown, false) > style.max_width {
            for w in &mut widths {
                *w = (*w).min(NARROW_COLUMN);
            }
            if width(&widths, &shown, false) > style.max_width {
                // Keep columns from both ends, alternately, while they fit.
                let order = (0..n).map(|k| if k % 2 == 0 { k / 2 } else { n - 1 - k / 2 });
                let mut kept: Vec<usize> = Vec::new();
                for i in order {
                    let mut candidate = kept.clone();
                    candidate.push(i);
                    if !kept.is_empty() && width(&widths, &candidate, true) > style.max_width {
                        break;
                    }
                    kept = candidate;
                }
                kept.sort_unstable();
                elided = kept.len() < n;
                shown = kept;
                // One column still too wide: narrow it to fit.
                let over = width(&widths, &shown, elided).saturating_sub(style.max_width);
                if over > 0 {
                    let i = shown[0];
                    widths[i] = widths[i].saturating_sub(over).max(1);
                }
            }
        }
        // Where the `…` column goes: after the columns kept from the left.
        let left_count = shown
            .iter()
            .enumerate()
            .take_while(|&(k, &i)| k == i)
            .count();

        let dim = |s: String| {
            if style.color {
                format!("{DIM}{s}{RESET}")
            } else {
                s
            }
        };
        let mut out = String::new();
        let rule = |left: &str, mid: &str, right: &str| {
            let mut line = String::from(left);
            for (k, &i) in shown.iter().enumerate() {
                if k > 0 {
                    line.push_str(mid);
                }
                if elided && k == left_count {
                    line.push_str("───");
                    line.push_str(mid);
                }
                line.push_str(&"─".repeat(widths[i] + 2));
            }
            if elided && left_count == shown.len() {
                line.push_str(mid);
                line.push_str("───");
            }
            line.push_str(right);
            line.push('\n');
            line
        };
        let line = |parts: Vec<String>, filler: &str| {
            let mut s = String::from("│");
            for (k, part) in parts.into_iter().enumerate() {
                if elided && k == left_count {
                    s.push_str(&format!(" {filler} │"));
                }
                s.push(' ');
                s.push_str(&part);
                s.push_str(" │");
            }
            if elided && left_count == shown.len() {
                s.push_str(&format!(" {filler} │"));
            }
            s.push('\n');
            s
        };

        out.push_str(&rule("┌", "┬", "┐"));
        out.push_str(&line(
            shown
                .iter()
                .map(|&i| center(&fit(&self.columns[i].name, widths[i]), widths[i]))
                .collect(),
            "…",
        ));
        out.push_str(&line(
            shown
                .iter()
                .map(|&i| dim(center(&fit(&self.columns[i].kind, widths[i]), widths[i])))
                .collect(),
            "…",
        ));
        if !cells.is_empty() {
            out.push_str(&rule("├", "┼", "┤"));
        }
        let gap_rows = |out: &mut String| {
            for _ in 0..3 {
                out.push_str(&line(
                    shown.iter().map(|&i| dim(center("·", widths[i]))).collect(),
                    "·",
                ));
            }
        };
        for (r, row) in cells.iter().enumerate() {
            if self.gap == Some(r) {
                gap_rows(&mut out);
            }
            out.push_str(&line(
                shown
                    .iter()
                    .map(|&i| match &row[i] {
                        None => dim(pad("NULL", widths[i], self.columns[i].numeric)),
                        Some(v) => pad(&fit(v, widths[i]), widths[i], self.columns[i].numeric),
                    })
                    .collect(),
                "…",
            ));
        }
        if self.gap == Some(cells.len()) {
            gap_rows(&mut out);
        }
        out.push_str(&rule("└", "┴", "┘"));

        // The footer, when something was left out (or there was nothing).
        let shown_rows = cells.len();
        if self.gap.is_some() || elided || self.total == 0 {
            let rows = if self.total == shown_rows {
                plural(self.total, "row")
            } else {
                format!(
                    "{} ({} shown)",
                    plural(self.total, "row"),
                    thousands(shown_rows)
                )
            };
            let table_width = width(&widths, &shown, elided);
            // The columns, when some were left out or there is room to say.
            let columns = if elided {
                format!("{} ({} shown)", plural(n, "column"), shown.len())
            } else if 2 + rows.width() + 1 + plural(n, "column").width() <= table_width {
                plural(n, "column")
            } else {
                String::new()
            };
            let room = table_width
                .saturating_sub(2 + rows.width() + columns.width())
                .max(1);
            let footer = format!("  {rows}{}{columns}", " ".repeat(room));
            out.push_str(&dim(footer.trim_end().to_string()));
            out.push('\n');
        }
        out
    }
}

/// `n word`, or `n words`, with thousands separated.
fn plural(n: usize, word: &str) -> String {
    format!("{} {word}{}", thousands(n), if n == 1 { "" } else { "s" })
}

/// `n` with a comma between each group of three digits.
pub fn thousands(n: usize) -> String {
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

/// A value on one line: control characters written as escapes.
fn escape(value: &str) -> String {
    if !value.chars().any(char::is_control) {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    for c in value.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `text` cut to `width` columns, ending in `…` if it was cut.
fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// `text` padded to `width` columns, to the right of it or (for numbers) the left.
fn pad(text: &str, width: usize, right: bool) -> String {
    let fill = " ".repeat(width.saturating_sub(text.width()));
    if right {
        format!("{fill}{text}")
    } else {
        format!("{text}{fill}")
    }
}

/// `text` in the middle of `width` columns (the odd space on the right).
fn center(text: &str, width: usize) -> String {
    let free = width.saturating_sub(text.width());
    format!(
        "{}{text}{}",
        " ".repeat(free / 2),
        " ".repeat(free - free / 2)
    )
}

fn is_numeric(t: &DataType) -> bool {
    match t {
        DataType::Dictionary(_, v) => is_numeric(v),
        t => t.is_numeric(),
    }
}

/// A type's name as the shell shows it: Arrow's own name for it, exactly as
/// `DataType` prints it (`Int64`, `Utf8`, `List(Int64)`). DataFusion
/// names types the same way in `DESCRIBE` and `arrow_typeof()`, so a table's
/// type row says what those say.
pub fn type_name(t: &DataType) -> String {
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow::array::{Float64Array, Int64Array, ListArray, StringArray};
    use arrow::datatypes::{Field, Int64Type, Schema};

    fn karate() -> (SchemaRef, RecordBatch) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("club", DataType::Utf8, true),
            Field::new("pagerank", DataType::Float64, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![33, 0, 32])),
                Arc::new(StringArray::from(vec![
                    Some("Officer"),
                    Some("Mr. Hi"),
                    None,
                ])),
                Arc::new(Float64Array::from(vec![Some(0.1009), Some(0.097), None])),
            ],
        )
        .unwrap();
        (schema, batch)
    }

    fn draw(rows: Rows, schema: &SchemaRef, max_width: usize) -> String {
        let table = rows.finish(schema, &FormatOptions::default()).unwrap();
        table.render(Style {
            max_width,
            color: false,
        })
    }

    #[test]
    fn a_table_has_types_under_names_and_numbers_on_the_right() {
        let (schema, batch) = karate();
        let mut rows = Rows::new(40);
        rows.push(batch);
        assert_eq!(
            draw(rows, &schema, 0),
            "\
┌───────┬─────────┬──────────┐
│  id   │  club   │ pagerank │
│ Int64 │  Utf8   │ Float64  │
├───────┼─────────┼──────────┤
│    33 │ Officer │   0.1009 │
│     0 │ Mr. Hi  │    0.097 │
│    32 │ NULL    │     NULL │
└───────┴─────────┴──────────┘
"
        );
    }

    #[test]
    fn a_long_result_shows_its_ends_and_counts_its_rows() {
        let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false)]));
        let mut rows = Rows::new(4);
        for chunk in (0..1000).collect::<Vec<i64>>().chunks(7) {
            rows.push(
                RecordBatch::try_new(
                    schema.clone(),
                    vec![Arc::new(Int64Array::from(chunk.to_vec()))],
                )
                .unwrap(),
            );
        }
        assert_eq!(
            draw(rows, &schema, 0),
            "\
┌───────┐
│   n   │
│ Int64 │
├───────┤
│     0 │
│     1 │
│   ·   │
│   ·   │
│   ·   │
│   998 │
│   999 │
└───────┘
  1,000 rows (4 shown)
"
        );
    }

    #[test]
    fn an_empty_result_says_so() {
        let (schema, batch) = karate();
        let mut rows = Rows::new(40);
        rows.push(batch.slice(0, 0));
        assert_eq!(
            draw(rows, &schema, 0),
            "\
┌───────┬──────┬──────────┐
│  id   │ club │ pagerank │
│ Int64 │ Utf8 │ Float64  │
└───────┴──────┴──────────┘
  0 rows          3 columns
"
        );
    }

    #[test]
    fn a_wide_result_cuts_long_values_then_leaves_out_middle_columns() {
        let names: Vec<String> = (0..8).map(|i| format!("column_{i}")).collect();
        let schema = Arc::new(Schema::new(
            names
                .iter()
                .map(|n| Field::new(n, DataType::Utf8, false))
                .collect::<Vec<_>>(),
        ));
        let long = "a value much longer than twenty columns";
        let batch = RecordBatch::try_new(
            schema.clone(),
            (0..8)
                .map(|_| Arc::new(StringArray::from(vec![long])) as _)
                .collect(),
        )
        .unwrap();
        let mut rows = Rows::new(40);
        rows.push(batch);
        let text = draw(rows, &schema, 80);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.iter().all(|l| l.width() <= 80), "{text}");
        assert!(lines[1].starts_with("│       column_0       │"), "{text}");
        assert!(lines[1].contains(" … "), "{text}");
        assert!(lines[1].ends_with("│       column_7       │"), "{text}");
        assert!(lines[4].contains("a value much longer…"), "{text}");
        assert!(
            lines.last().unwrap().ends_with("8 columns (3 shown)"),
            "{text}"
        );
    }

    #[test]
    fn types_have_arrows_names() {
        assert_eq!(type_name(&DataType::Utf8View), "Utf8View");
        assert_eq!(type_name(&DataType::UInt32), "UInt32");
        let list =
            ListArray::from_iter_primitive::<Int64Type, _, _>(vec![Some(vec![Some(16), Some(5)])]);
        // As DataFusion's arrow_typeof() names it.
        assert_eq!(type_name(list.data_type()), list.data_type().to_string());
    }

    #[test]
    fn a_value_stays_on_one_line() {
        assert_eq!(escape("a\nb\tc"), "a\\nb\\tc");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 4), "abc");
    }

    #[test]
    fn colour_dims_types_and_nulls() {
        let (schema, batch) = karate();
        let mut rows = Rows::new(40);
        rows.push(batch);
        let table = rows.finish(&schema, &FormatOptions::default()).unwrap();
        let text = table.render(Style {
            max_width: 0,
            color: true,
        });
        assert!(text.contains("\x1b[90mFloat64 \x1b[0m"), "{text}");
        assert!(text.contains("\x1b[90mNULL   \x1b[0m"), "{text}");
    }

    /// `n` rows of (x, s), with s null for odd x, in batches of `size`.
    fn numbered(n: i64, size: i64) -> (SchemaRef, Vec<RecordBatch>) {
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

    fn numbered_table(n: i64, size: i64, max_rows: usize) -> String {
        let (schema, batches) = numbered(n, size);
        let mut rows = Rows::new(max_rows);
        for batch in batches {
            rows.push(batch);
        }
        draw(rows, &schema, 0)
    }

    #[test]
    fn the_first_and_last_rows_whatever_the_batches() {
        let expected = "\
┌───────┬──────┐
│   x   │  s   │
│ Int64 │ Utf8 │
├───────┼──────┤
│     0 │ v0   │
│     1 │ NULL │
│     2 │ v2   │
│   ·   │  ·   │
│   ·   │  ·   │
│   ·   │  ·   │
│   998 │ v998 │
│   999 │ NULL │
└───────┴──────┘
  1,000 rows (5 shown)
";
        for size in [1, 2, 3, 7, 999, 1000] {
            assert_eq!(numbered_table(1000, size, 5), expected, "batches of {size}");
        }
    }

    #[test]
    fn up_to_the_cap_or_with_none_every_row() {
        for (n, max_rows) in [(5, 5), (4, 5), (50, 0)] {
            let out = numbered_table(n, 3, max_rows);
            assert_eq!(out.lines().count(), 5 + n as usize, "{out}");
            assert!(!out.contains('·') && !out.contains("shown"), "{out}");
        }
        assert!(numbered_table(6, 4, 5).contains("6 rows (5 shown)"));
        let one = numbered_table(3, 1, 1);
        assert!(
            one.contains("│     0 │ v0   │\n│   ·   │  ·   │\n"),
            "{one}"
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
