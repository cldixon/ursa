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
//! rows of `·` between them; a result wider than the screen cuts its longest
//! values short, as little as it can, and if that is not enough shows its
//! first and last columns with a `…` column between them. Either way a
//! footer counts what there was:
//!
//! ```text
//!   39,650,447 rows (40 shown)                     17 columns (10 shown)
//! ```
//!
//! [`Rows`] collects a result batch by batch and keeps only the rows it will
//! show; [`Table::render`] draws them for a given width.

use std::borrow::Cow;
use std::collections::VecDeque;

use arrow::array::{Array, RecordBatch};
use arrow::datatypes::{DataType, SchemaRef};
use arrow::error::ArrowError;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// How many rows a table shows by default, as in DuckDB.
pub const DEFAULT_MAX_ROWS: usize = 40;

/// How wide a column is when choosing which columns a table too wide for the
/// screen shows (DuckDB's `max_col_width` default), and the most a column's
/// floor may be (see [`MIN_COLUMN`]).
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
                // `logical_nulls`, not `is_null`: a column of type Null (an
                // untyped `SELECT NULL`) has no null buffer, yet is all null.
                let nulls: Vec<_> = batch.columns().iter().map(|c| c.logical_nulls()).collect();
                for row in skip..batch.num_rows() {
                    rows.push(
                        nulls
                            .iter()
                            .zip(&formatters)
                            .map(|(nulls, f)| {
                                let null = nulls.as_ref().is_some_and(|n| n.is_null(row));
                                (!null).then(|| f.value(row).to_string())
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
                name: escape(f.name()).into_owned(),
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
        // Values are escaped only when they need it.
        let cells: Vec<Vec<Option<Cow<str>>>> = self
            .rows
            .iter()
            .map(|row| row.iter().map(|v| v.as_deref().map(escape)).collect())
            .collect();
        // Each column's width with nothing cut: its widest value, name or type.
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
        let (shown, left_count, elided) = if style.max_width > 0
            && table_width(&widths, &(0..n).collect::<Vec<_>>(), false) > style.max_width
        {
            fit_width(&self.columns, &mut widths, style.max_width)
        } else {
            ((0..n).collect(), n, false)
        };

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
            let table_width = table_width(&widths, &shown, elided);
            // The columns, at the table's right edge when they fit there with
            // two spaces to spare; on a line of their own when some were left
            // out and they do not; otherwise not at all.
            let columns = if elided {
                format!("{} ({} shown)", plural(n, "column"), shown.len())
            } else {
                plural(n, "column")
            };
            let footer = if 2 + rows.width() + 2 + columns.width() <= table_width {
                let room = table_width - 2 - rows.width() - columns.width();
                format!("  {rows}{}{columns}", " ".repeat(room))
            } else if elided {
                format!("  {rows}\n  {columns}")
            } else {
                format!("  {rows}")
            };
            for line in footer.lines() {
                out.push_str(&dim(line.to_string()));
                out.push('\n');
            }
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

/// A value on one line, its characters shown as they are: control
/// characters, and the invisible ones that change how text around them is
/// drawn (bidirectional overrides, zero-width spaces and joiners, line and
/// paragraph separators, byte order marks), written as escapes. A value that
/// needs none is borrowed.
fn escape(value: &str) -> Cow<'_, str> {
    if !value.chars().any(needs_escape) {
        return Cow::Borrowed(value);
    }
    let mut out = String::with_capacity(value.len() + 2);
    for c in value.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if needs_escape(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Whether `c` is written as an escape: a control character, or a format
/// character or separator that is invisible but moves or hides the text
/// around it. The zero-width joiner and non-joiner (U+200D, U+200C) are not:
/// they are part of ordinary text, in emoji sequences and in several scripts.
fn needs_escape(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}'
                | '\u{180e}'
                | '\u{200b}'
                | '\u{200e}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
        )
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

/// How wide a table is with columns `shown` at `widths`, and a `…` column
/// if `elided`.
fn table_width(widths: &[usize], shown: &[usize], elided: bool) -> usize {
    shown.iter().map(|&i| widths[i] + 3).sum::<usize>() + 1 + if elided { 4 } else { 0 }
}

/// The narrowest a column is cut when every column can still be shown: its
/// name and type (between [`MIN_COLUMN`] and [`NARROW_COLUMN`] wide), or less
/// if nothing in it is that wide.
const MIN_COLUMN: usize = 10;

/// Fit a table too wide for `max_width`: narrow the widest columns, only as
/// much as needed and no narrower than their names (see [`MIN_COLUMN`]); if
/// that is not enough, leave out middle columns, keeping as many from both
/// ends as fit (see [`from_both_ends`]), then narrow those as needed.
/// Returns the columns shown, how many of them are from the left (the `…`
/// column comes after them), and whether any were left out. `widths` holds
/// each column's width with nothing cut, and is set to the widths to draw.
fn fit_width(
    columns: &[Column],
    widths: &mut [usize],
    max_width: usize,
) -> (Vec<usize>, usize, bool) {
    let n = columns.len();
    let all: Vec<usize> = (0..n).collect();
    let floor = |i: usize| {
        let header = columns[i].name.width().max(columns[i].kind.width());
        widths[i].min(header.clamp(MIN_COLUMN, NARROW_COLUMN))
    };
    let floors: Vec<usize> = all.iter().map(|&i| floor(i)).collect();
    if table_width(&floors, &all, false) <= max_width {
        let room = max_width - table_width(&vec![0; n], &all, false);
        narrow(widths, &all, &floors, room);
        return (all, n, false);
    }

    // As many columns as fit: from both ends, at NARROW_COLUMN wide or at
    // their floors, whichever shows more (at NARROW_COLUMN if the same).
    let narrowed: Vec<usize> = widths.iter().map(|&w| w.min(NARROW_COLUMN)).collect();
    let (wide, wide_left) = from_both_ends(&narrowed, max_width);
    let (narrow_ones, narrow_left) = from_both_ends(&floors, max_width);
    let (shown, left_count) = if narrow_ones.len() > wide.len() {
        (narrow_ones, narrow_left)
    } else {
        (wide, wide_left)
    };
    let elided = shown.len() < n;
    // Then as wide as there is room for: no narrower than their floors
    // unless a column alone is too wide.
    let room = max_width.saturating_sub(table_width(&vec![0; n], &shown, elided));
    narrow(widths, &shown, &floors, room);
    narrow(widths, &shown, &vec![1; n], room);
    (shown, left_count, elided)
}

/// The columns that fit in `max_width` at `widths`, with a `…` column for
/// the rest: taken from both ends, alternately, the first column always. A
/// column that does not fit closes its end (the columns past it would make a
/// second gap) and the other end goes on. Returns them in order, and how
/// many are from the left.
fn from_both_ends(widths: &[usize], max_width: usize) -> (Vec<usize>, usize) {
    let n = widths.len();
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let (mut next_left, mut next_right) = (0, n);
    let (mut left_open, mut right_open) = (true, true);
    let mut used = table_width(widths, &[], true);
    let mut from_left = true;
    while next_left < next_right && (left_open || right_open) {
        if !(if from_left { left_open } else { right_open }) {
            from_left = !from_left;
            continue;
        }
        let i = if from_left { next_left } else { next_right - 1 };
        let first = left.is_empty() && right.is_empty();
        if first || used + widths[i] + 3 <= max_width {
            used += widths[i] + 3;
            if from_left {
                left.push(i);
                next_left += 1;
            } else {
                right.push(i);
                next_right -= 1;
            }
        } else if from_left {
            left_open = false;
        } else {
            right_open = false;
        }
        from_left = !from_left;
    }
    let left_count = left.len();
    left.extend(right.into_iter().rev());
    (left, left_count)
}

/// Narrow the widest of the columns `shown`, none below its floor, until
/// their widths add up to `room` at most (or all are at their floors). Each
/// is cut to the same level, as little as that allows; the leftmost of those
/// cut take what is left over.
fn narrow(widths: &mut [usize], shown: &[usize], floors: &[usize], room: usize) {
    let total: usize = shown.iter().map(|&i| widths[i]).sum();
    if total <= room {
        return;
    }
    let over = total - room;
    let at = |i: usize, level: usize| widths[i].min(level).max(floors[i].min(widths[i]));
    let cut = |level: usize| {
        shown
            .iter()
            .map(|&i| widths[i] - at(i, level))
            .sum::<usize>()
    };
    if cut(0) <= over {
        let floored: Vec<usize> = shown.iter().map(|&i| at(i, 0)).collect();
        for (&i, w) in shown.iter().zip(floored) {
            widths[i] = w;
        }
        return;
    }
    // The highest level that cuts enough.
    let (mut low, mut high) = (0, shown.iter().map(|&i| widths[i]).max().unwrap_or(0));
    while low < high {
        let mid = (low + high).div_ceil(2);
        if cut(mid) >= over {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let level = low;
    let mut spare = cut(level) - over;
    let narrowed: Vec<(usize, usize)> = shown
        .iter()
        .map(|&i| {
            let w = at(i, level);
            if spare > 0 && w == level && widths[i] > level {
                spare -= 1;
                (i, level + 1)
            } else {
                (i, w)
            }
        })
        .collect();
    for (i, w) in narrowed {
        widths[i] = w;
    }
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

    use arrow::array::{
        DictionaryArray, Float64Array, Int32Array, Int64Array, ListArray, NullArray, StringArray,
    };
    use arrow::datatypes::{Field, Int32Type, Int64Type, Schema};

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
        // As many columns as fit, from both ends, each cut to the same width.
        assert!(
            lines[1].starts_with("│   column_0   │   column_1   │"),
            "{text}"
        );
        assert!(lines[1].contains(" … "), "{text}");
        assert!(
            lines[1].ends_with("│   column_6   │   column_7   │"),
            "{text}"
        );
        assert!(lines[4].starts_with("│ a value muc… │"), "{text}");
        assert!(
            lines.last().unwrap().ends_with("8 columns (5 shown)"),
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

    /// A table of one row, from `(name, type, value)` (`None`: null).
    fn one_row(columns: &[(&str, DataType, Option<&str>)]) -> Table {
        let schema = Arc::new(Schema::new(
            columns
                .iter()
                .map(|(name, t, _)| Field::new(*name, t.clone(), true))
                .collect::<Vec<_>>(),
        ));
        let row = columns
            .iter()
            .map(|(_, _, v)| v.map(String::from))
            .collect();
        Table::new(&schema, vec![row], None, 1)
    }

    fn at_width(table: &Table, max_width: usize) -> String {
        table.render(Style {
            max_width,
            color: false,
        })
    }

    #[test]
    fn a_null_in_a_null_column_shows_null() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("n", DataType::Null, true),
            Field::new("i", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(NullArray::new(1)),
                Arc::new(Int64Array::from(vec![1])),
            ],
        )
        .unwrap();
        let mut rows = Rows::new(40);
        rows.push(batch);
        assert_eq!(
            draw(rows, &schema, 0),
            "\
┌──────┬───────┐
│  n   │   i   │
│ Null │ Int64 │
├──────┼───────┤
│ NULL │     1 │
└──────┴───────┘
"
        );
    }

    #[test]
    fn a_null_dictionary_value_shows_null() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "d",
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            true,
        )]));
        let values = StringArray::from(vec![Some("a"), None]);
        let keys = Int32Array::from(vec![0, 1]);
        let dictionary = DictionaryArray::<Int32Type>::try_new(keys, Arc::new(values)).unwrap();
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(dictionary)]).unwrap();
        let mut rows = Rows::new(40);
        rows.push(batch);
        let text = draw(rows, &schema, 0);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[4].starts_with("│ a ") && lines[5].starts_with("│ NULL "),
            "{text}"
        );
    }

    #[test]
    fn a_column_too_wide_to_show_is_skipped_and_the_other_end_goes_on() {
        // `SELECT 1 a, 'abcdefghijklmnopqrstuvwxyz' b, 3 c, 4 d, 5 e, 6 f` in 40.
        let mut columns = vec![("a", DataType::Int64, Some("1"))];
        columns.push(("b", DataType::Utf8, Some("abcdefghijklmnopqrstuvwxyz")));
        for (name, value) in [("c", "3"), ("d", "4"), ("e", "5"), ("f", "6")] {
            columns.push((name, DataType::Int64, Some(value)));
        }
        let table = one_row(&columns);
        assert_eq!(
            at_width(&table, 40),
            "\
┌───────┬───┬───────┬───────┬───────┐
│   a   │ … │   d   │   e   │   f   │
│ Int64 │ … │ Int64 │ Int64 │ Int64 │
├───────┼───┼───────┼───────┼───────┤
│     1 │ … │     4 │     5 │     6 │
└───────┴───┴───────┴───────┴───────┘
  1 row           6 columns (4 shown)
"
        );
        // At the right end too: the left end goes on.
        let b = columns.remove(1);
        columns.insert(4, b);
        let names: Vec<_> = columns.iter().map(|c| c.0).collect();
        assert_eq!(names, ["a", "c", "d", "e", "b", "f"]);
        let text = at_width(&one_row(&columns), 40);
        assert!(
            text.lines().nth(1) == Some("│   a   │   c   │   d   │ … │   f   │"),
            "{text}"
        );
    }

    #[test]
    fn a_table_a_little_too_wide_is_narrowed_only_as_needed() {
        // `SELECT repeat('x', 60) AS s, 1 AS n` in 70: s loses two characters.
        let long = "x".repeat(60);
        let table = one_row(&[
            ("s", DataType::Utf8, Some(long.as_str())),
            ("n", DataType::Int64, Some("1")),
        ]);
        let text = at_width(&table, 70);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.iter().all(|l| l.width() == 70), "{text}");
        assert_eq!(lines[4], format!("│ {}… │     1 │", "x".repeat(57)));

        // Both columns shown, narrowed, rather than one left out.
        let table = one_row(&[
            ("a", DataType::Int64, Some("1")),
            ("b", DataType::Utf8, Some("a long value that is long")),
        ]);
        assert_eq!(
            at_width(&table, 25),
            "\
┌───────┬───────────────┐
│   a   │       b       │
│ Int64 │     Utf8      │
├───────┼───────────────┤
│     1 │ a long value… │
└───────┴───────────────┘
"
        );
    }

    #[test]
    fn columns_are_narrowed_no_further_than_their_names_before_some_are_left_out() {
        let long = "y".repeat(30);
        let columns: Vec<(&str, DataType, Option<&str>)> = ["first", "second_name", "third"]
            .into_iter()
            .map(|name| (name, DataType::Utf8, Some(long.as_str())))
            .collect();
        let table = one_row(&columns);
        // 10, 11 and 10 wide fit in 41: all three, cut to their names.
        let text = at_width(&table, 41);
        assert!(
            text.lines().nth(1) == Some("│   first    │ second_name │   third    │"),
            "{text}"
        );
        // Not in 40: the middle one is left out, the others as wide as fits.
        let text = at_width(&table, 40);
        assert!(text.lines().all(|l| l.width() <= 40), "{text}");
        assert!(text.lines().nth(1).unwrap().contains(" … "), "{text}");
        assert!(text.ends_with("3 columns (2 shown)\n"), "{text}");
    }

    #[test]
    fn the_footer_fits_under_the_table() {
        let long = "z".repeat(30);
        let columns: Vec<(&str, DataType, Option<&str>)> = ["p", "q", "r"]
            .into_iter()
            .map(|name| (name, DataType::Utf8, Some(long.as_str())))
            .collect();
        let table = one_row(&columns);
        // Too narrow for the rows and the columns on one line: two lines.
        let text = at_width(&table, 25);
        assert!(
            text.ends_with("┘\n  1 row\n  3 columns (1 shown)\n"),
            "{text}"
        );
        assert!(text.lines().all(|l| l.width() <= 25), "{text}");
        // Wide enough: one line, the columns at the table's right edge.
        let text = at_width(&table, 38);
        let last = text.lines().last().unwrap();
        assert_eq!(last.width(), text.lines().next().unwrap().width(), "{text}");
        assert!(last.starts_with("  1 row  ") && last.ends_with("3 columns (2 shown)"));

        // A count of columns that is not needed is left out rather than
        // crowded in.
        let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false)]));
        let empty = Table::new(&schema, vec![], None, 0);
        assert_eq!(
            at_width(&empty, 0),
            "┌───────┐\n│   n   │\n│ Int64 │\n└───────┘\n  0 rows\n"
        );
    }

    #[test]
    fn invisible_characters_are_escaped() {
        assert_eq!(
            escape("a\u{202e}b\u{200b}c\u{feff}"),
            "a\\u{202e}b\\u{200b}c\\u{feff}"
        );
        // Joiners are ordinary text: an emoji family stays one family.
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        assert_eq!(escape(family), family);
        assert_eq!(escape("a\u{200c}b"), "a\u{200c}b");
        assert_eq!(
            escape("\u{2028}\u{2029}\u{2066}\u{2069}\u{200f}"),
            "\\u{2028}\\u{2029}\\u{2066}\\u{2069}\\u{200f}"
        );
        // Text that needs no escapes is not copied.
        assert!(matches!(escape("plain, ünïcode 熊"), Cow::Borrowed(_)));
        // Names too.
        let table = one_row(&[("x\u{202e}y", DataType::Utf8, Some("\u{2067}v"))]);
        let text = at_width(&table, 0);
        assert!(
            text.contains("x\\u{202e}y") && text.contains("\\u{2067}v"),
            "{text}"
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
