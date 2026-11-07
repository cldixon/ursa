//! Positions in rewritten SQL, mapped back to the text the user wrote.
//!
//! The `GRAPH_TABLE` and FROM-first rewrites ([`crate::graph_table`],
//! [`crate::from_first`]) change a statement's text before sqlparser sees
//! it, so sqlparser's "at Line: L, Column: C" names a place in the rewritten
//! text. Each rewrite records what it replaced ([`Rewritten`]), and
//! [`SourceMap::user_message`] moves each location in an error message back
//! to the user's text: a place inside a rewritten span goes to the start of
//! what the user wrote there.

use datafusion::sql::sqlparser::tokenizer::Location;

use crate::parser::byte_offset;

/// One replacement: `output` (bytes of the rewritten text) holds what was
/// `input` (bytes of the text before the rewrite).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Edit {
    output: (usize, usize),
    input: (usize, usize),
}

/// Text built from another by replacing some spans, with a record of them.
#[derive(Debug, Default)]
pub(crate) struct Rewritten {
    pub text: String,
    edits: Vec<Edit>,
    /// Bytes of the input copied into `text` so far.
    copied: usize,
}

impl Rewritten {
    pub fn new(capacity: usize) -> Self {
        Self {
            text: String::with_capacity(capacity),
            ..Self::default()
        }
    }

    /// Copy `input[copied..start]`, then put `with` in place of
    /// `input[start..end]`.
    pub fn replace(&mut self, input: &str, start: usize, end: usize, with: &str) {
        self.text.push_str(&input[self.copied..start]);
        let output_start = self.text.len();
        self.text.push_str(with);
        self.edits.push(Edit {
            output: (output_start, self.text.len()),
            input: (start, end),
        });
        self.copied = end;
    }

    /// Copy the rest of `input`.
    pub fn finish(mut self, input: &str) -> Self {
        self.text.push_str(&input[self.copied..]);
        self.copied = input.len();
        self
    }

    /// The input's byte for the rewritten text's byte `at`.
    fn input_offset(&self, at: usize) -> usize {
        match self.edits.iter().rev().find(|e| e.output.0 <= at) {
            None => at,
            Some(e) if at < e.output.1 => e.input.0,
            Some(e) => at - e.output.1 + e.input.1,
        }
    }
}

/// The user's text and the rewrites made to it, in order.
#[derive(Debug, Default)]
pub struct SourceMap {
    original: String,
    /// Each rewrite, and the text it produced.
    steps: Vec<Rewritten>,
}

impl SourceMap {
    pub(crate) fn new(original: &str) -> Self {
        Self {
            original: original.to_string(),
            steps: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, step: Rewritten) {
        self.steps.push(step);
    }

    /// The user's text.
    pub(crate) fn original(&self) -> &str {
        &self.original
    }

    /// The text after every rewrite.
    pub(crate) fn text(&self) -> &str {
        self.steps
            .last()
            .map_or(self.original.as_str(), |s| s.text.as_str())
    }

    /// Whether any rewrite changed the text.
    fn changed(&self) -> bool {
        self.steps.iter().any(|s| !s.edits.is_empty())
    }

    /// The user's location for `at`, a location in the rewritten text.
    fn location(&self, at: Location) -> Option<Location> {
        let mut offset = byte_offset(self.text(), at)?;
        for step in self.steps.iter().rev() {
            offset = step.input_offset(offset);
        }
        Some(location_of(&self.original, offset))
    }

    /// `message` with every "Line: L, Column: C" moved to the user's text.
    pub fn user_message(&self, message: &str) -> String {
        const LINE: &str = "Line: ";
        const COLUMN: &str = ", Column: ";
        if !self.changed() {
            return message.to_string();
        }
        let mut out = String::with_capacity(message.len());
        let mut rest = message;
        while let Some(at) = rest.find(LINE) {
            let (before, after) = rest.split_at(at);
            out.push_str(before);
            let after_line = &after[LINE.len()..];
            let line_digits = digits(after_line);
            let after_digits = &after_line[line_digits..];
            let parsed = after_digits.strip_prefix(COLUMN).and_then(|c| {
                let column_digits = digits(c);
                let line = after_line[..line_digits].parse().ok()?;
                let column = c[..column_digits].parse().ok()?;
                Some((line, column, &c[column_digits..]))
            });
            match parsed.and_then(|(line, column, tail)| {
                Some((self.location(Location::new(line, column))?, tail))
            }) {
                Some((location, tail)) => {
                    out.push_str(&format!(
                        "{LINE}{}{COLUMN}{}",
                        location.line, location.column
                    ));
                    rest = tail;
                }
                None => {
                    out.push_str(LINE);
                    rest = after_line;
                }
            }
        }
        out.push_str(rest);
        out
    }
}

/// How many ASCII digits `s` starts with.
fn digits(s: &str) -> usize {
    s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len()
}

/// The 1-based line and column (in characters) of byte `offset` of `sql`.
fn location_of(sql: &str, offset: usize) -> Location {
    let offset = offset.min(sql.len());
    let before = &sql[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let column = sql[line_start..offset].chars().count() + 1;
    Location::new(line as u64, column as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_map_through_replacements() {
        let input = "ab\nFROM t WHERE x";
        let mut step = Rewritten::new(0);
        step.replace(input, 3, 3, "SELECT * ");
        let step = step.finish(input);
        assert_eq!(step.text, "ab\nSELECT * FROM t WHERE x");
        let mut map = SourceMap::new(input);
        map.push(step);
        // `WHERE` is at column 17 of line 2 of the rewritten text, 8 of the
        // user's; the inserted `*` maps to the `FROM` it was put before.
        assert_eq!(
            map.user_message("found: WHERE at Line: 2, Column: 17"),
            "found: WHERE at Line: 2, Column: 8"
        );
        assert_eq!(
            map.user_message("found: * at Line: 2, Column: 8 and Line: 1, Column: 2"),
            "found: * at Line: 2, Column: 1 and Line: 1, Column: 2"
        );
        assert_eq!(map.user_message("Line: x"), "Line: x");
    }

    #[test]
    fn two_rewrites_compose() {
        let input = "FROM GRAPH_TABLE (g MATCH (a)) x WHERE";
        let mut first = Rewritten::new(0);
        first.replace(input, 5, 30, "graph_table('g MATCH (a)')");
        let first = first.finish(input);
        let middle = first.text.clone();
        let mut second = Rewritten::new(0);
        second.replace(&middle, 0, 0, "SELECT * ");
        let second = second.finish(&middle);
        assert_eq!(
            second.text,
            "SELECT * FROM graph_table('g MATCH (a)') x WHERE"
        );
        let mut map = SourceMap::new(input);
        map.push(first);
        map.push(second);
        let at = second_column(&map, "WHERE");
        assert_eq!(
            map.user_message(&format!("at Line: 1, Column: {at}")),
            format!("at Line: 1, Column: {}", input.find("WHERE").unwrap() + 1)
        );
    }

    fn second_column(map: &SourceMap, word: &str) -> usize {
        map.text().find(word).unwrap() + 1
    }
}
