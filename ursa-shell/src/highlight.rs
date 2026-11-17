//! Colour for what is typed at the prompt, with ANSI escapes: keywords bold,
//! strings yellow, comments grey and italic, dot-commands blue. The `ursa`
//! CLI colours its line editor with it and the browser shell its input, so
//! both look the same; what each colour looks like is the terminal's theme
//! (the browser shell sets its own).
//!
//! It reads the text with a small scanner rather than the SQL tokenizer, so a
//! statement being typed, with a quote still open, is coloured as far as it
//! goes.

/// The prompt's colour: orange (xterm's 256-colour 208).
pub const PROMPT: &str = "\x1b[1;38;5;208m";
/// Keywords.
pub const KEYWORD: &str = "\x1b[1m";
/// String literals.
pub const STRING: &str = "\x1b[33m";
/// Comments.
pub const COMMENT: &str = "\x1b[3;90m";
/// A dot-command's name.
pub const COMMAND: &str = "\x1b[1;34m";
/// A dot-command's arguments, and anything else set back.
pub const MUTED: &str = "\x1b[90m";
/// Errors.
pub const ERROR: &str = "\x1b[31m";
/// Back to the terminal's colours.
pub const RESET: &str = "\x1b[0m";

/// The words shown as keywords: SQL's common ones and SQL/PGQ's.
const KEYWORDS: &[&str] = &[
    "acyclic",
    "all",
    "analyze",
    "and",
    "any",
    "are",
    "as",
    "asc",
    "attach",
    "between",
    "by",
    "case",
    "cast",
    "columns",
    "copy",
    "create",
    "cross",
    "database",
    "default",
    "delete",
    "desc",
    "describe",
    "destination",
    "distinct",
    "drop",
    "edge",
    "else",
    "end",
    "except",
    "exists",
    "explain",
    "false",
    "filter",
    "first",
    "from",
    "full",
    "graph",
    "graph_table",
    "graphs",
    "group",
    "having",
    "if",
    "ilike",
    "in",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "label",
    "last",
    "left",
    "like",
    "limit",
    "match",
    "no",
    "not",
    "null",
    "nulls",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "over",
    "partition",
    "properties",
    "property",
    "range",
    "recursive",
    "references",
    "replace",
    "right",
    "rows",
    "select",
    "set",
    "shortest",
    "show",
    "simple",
    "source",
    "table",
    "tables",
    "then",
    "to",
    "trail",
    "true",
    "union",
    "update",
    "using",
    "values",
    "vertex",
    "view",
    "walk",
    "when",
    "where",
    "with",
];

/// What a piece of the text is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Keyword,
    String,
    Number,
    Comment,
    Command,
    Arguments,
}

/// Where text starts, after what was typed before it: a line is coloured
/// as part of the statement typed so far ([`crate::shell::Shell::highlight_state`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum State {
    /// A new statement: a line that starts with `.` is a dot-command.
    #[default]
    Start,
    /// In a statement, outside quotes and comments: a line that starts with
    /// `.` is SQL (`.5`).
    Statement,
    /// In a string literal; `escapes` in an `E'…'` string, where a backslash
    /// escapes the next character.
    String { escapes: bool },
    /// In a quoted name (`"…"`).
    QuotedName,
    /// In a dollar-quoted string, `$tag$…$tag$`, with its tag (`$$…$$` has
    /// an empty one).
    Dollar(String),
    /// In a block comment, this many deep (`/* /* */ */` nests, as
    /// sqlparser reads it).
    Comment(usize),
}

/// `text` split into pieces, each with its kind, as a new statement. The
/// pieces joined are `text`.
pub fn pieces(text: &str) -> Vec<(Kind, &str)> {
    pieces_from(text, &State::Start).0
}

/// `text` split into pieces, each with its kind, starting in `state`; and
/// the state at its end. The pieces joined are `text`.
pub fn pieces_from<'a>(text: &'a str, state: &State) -> (Vec<(Kind, &'a str)>, State) {
    let bytes = text.as_bytes();
    let mut out = Pieces {
        text,
        pieces: Vec::new(),
        plain_from: 0,
    };
    // First, the end of a string, name or comment the text before left open.
    let mut i = 0;
    let open = match state {
        State::Start | State::Statement => None,
        State::String { escapes } => Some(close_quote(bytes, 0, b'\'', *escapes).map_or_else(
            || (Kind::String, text.len(), Some(state.clone())),
            |end| (Kind::String, end, None),
        )),
        State::QuotedName => Some(close_quote(bytes, 0, b'"', false).map_or_else(
            || (Kind::Plain, text.len(), Some(State::QuotedName)),
            |end| (Kind::Plain, end, None),
        )),
        State::Dollar(tag) => Some(match close_dollar(text, 0, tag) {
            Some(end) => (Kind::String, end, None),
            None => (Kind::String, text.len(), Some(state.clone())),
        }),
        State::Comment(depth) => Some(match close_comment(bytes, 0, *depth) {
            Ok(end) => (Kind::Comment, end, None),
            Err(depth) => (Kind::Comment, text.len(), Some(State::Comment(depth))),
        }),
    };
    if let Some((kind, end, still_open)) = open {
        out.push(kind, 0, end);
        if let Some(state) = still_open {
            return (out.finish(), state);
        }
        i = end;
    }
    // A statement has begun: a line that starts with `.` is not a command
    // until it ends with a `;`.
    let mut started = !matches!(state, State::Start);

    while i < bytes.len() {
        let c = bytes[i];
        let line_start = i == 0 || bytes[i - 1] == b'\n';
        if line_start && !started {
            let indent = text[i..].len() - text[i..].trim_start_matches([' ', '\t']).len();
            if bytes.get(i + indent) == Some(&b'.') {
                let line_end = text[i..].find('\n').map_or(text.len(), |n| i + n);
                let name_end = text[i + indent..line_end]
                    .find(char::is_whitespace)
                    .map_or(line_end, |n| i + indent + n);
                out.push(Kind::Plain, i, i + indent);
                out.push(Kind::Command, i + indent, name_end);
                out.push(Kind::Arguments, name_end, line_end);
                i = line_end;
                continue;
            }
        }
        let (kind, end, still_open) = if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            let end = text[i..].find('\n').map_or(text.len(), |n| i + n);
            (Kind::Comment, end, None)
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            match close_comment(bytes, i + 2, 1) {
                Ok(end) => (Kind::Comment, end, None),
                Err(depth) => (Kind::Comment, text.len(), Some(State::Comment(depth))),
            }
        } else if c == b'\'' || (matches!(c, b'E' | b'e') && bytes.get(i + 1) == Some(&b'\'')) {
            // A string, to its closing quote (a doubled quote is part of
            // it, and in `E'…'` a backslash escapes), or the end.
            let escapes = c != b'\'';
            let from = if escapes { i + 2 } else { i + 1 };
            match close_quote(bytes, from, b'\'', escapes) {
                Some(end) => (Kind::String, end, None),
                None => (Kind::String, text.len(), Some(State::String { escapes })),
            }
        } else if c == b'"' {
            // A quoted name is plain.
            match close_quote(bytes, i + 1, b'"', false) {
                Some(end) => (Kind::Plain, end, None),
                None => (Kind::Plain, text.len(), Some(State::QuotedName)),
            }
        } else if c == b'$' {
            // `$tag$…$tag$` is a string; `$1` a placeholder.
            let tag_end = i
                + 1
                + text[i + 1..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(text.len() - i - 1);
            if bytes.get(tag_end) == Some(&b'$') {
                let tag = &text[i + 1..tag_end];
                match close_dollar(text, tag_end + 1, tag) {
                    Some(end) => (Kind::String, end, None),
                    None => (
                        Kind::String,
                        text.len(),
                        Some(State::Dollar(tag.to_string())),
                    ),
                }
            } else {
                (Kind::Plain, tag_end, None)
            }
        } else if c.is_ascii_digit() && (i == 0 || !is_word(bytes[i - 1])) {
            let mut j = i;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'.') {
                j += 1;
            }
            if j < bytes.len() && is_word(bytes[j]) {
                // Part of a name (`2nd_column`): plain.
                while j < bytes.len() && is_word(bytes[j]) {
                    j += 1;
                }
                (Kind::Plain, j, None)
            } else {
                (Kind::Number, j, None)
            }
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let mut j = i;
            while j < bytes.len() && is_word(bytes[j]) {
                j += 1;
            }
            let word = text[i..j].to_ascii_lowercase();
            let kind = if KEYWORDS.binary_search(&word.as_str()).is_ok() {
                Kind::Keyword
            } else {
                Kind::Plain
            };
            (kind, j, None)
        } else {
            // One character (it may be several bytes).
            let end = i + text[i..].chars().next().map_or(1, char::len_utf8);
            (Kind::Plain, end, None)
        };
        if kind != Kind::Comment && !c.is_ascii_whitespace() {
            started = c != b';';
        }
        out.push(kind, i, end);
        if let Some(state) = still_open {
            return (out.finish(), state);
        }
        i = end;
    }
    let state = if started {
        State::Statement
    } else {
        State::Start
    };
    (out.finish(), state)
}

/// Pieces of `text` as they are found; plain text between them is one piece.
struct Pieces<'a> {
    text: &'a str,
    pieces: Vec<(Kind, &'a str)>,
    /// Where the plain text not yet added starts.
    plain_from: usize,
}

impl<'a> Pieces<'a> {
    /// The text from `from` to `to` is of `kind`.
    fn push(&mut self, kind: Kind, from: usize, to: usize) {
        if kind == Kind::Plain || from == to {
            return;
        }
        self.flush(from);
        self.pieces.push((kind, &self.text[from..to]));
        self.plain_from = to;
    }

    fn flush(&mut self, to: usize) {
        if self.plain_from < to {
            self.pieces
                .push((Kind::Plain, &self.text[self.plain_from..to]));
        }
        self.plain_from = to;
    }

    fn finish(mut self) -> Vec<(Kind, &'a str)> {
        self.flush(self.text.len());
        self.pieces
    }
}

/// The end of a quoted string or name whose text starts at `from`: just past
/// the closing `quote` (a doubled quote is part of the text; with `escapes`,
/// so is a quote after a backslash), or `None` if it is still open.
fn close_quote(bytes: &[u8], from: usize, quote: u8, escapes: bool) -> Option<usize> {
    let mut j = from;
    loop {
        match bytes.get(j) {
            None => return None,
            Some(b'\\') if escapes => j += 2,
            Some(&q) if q == quote => {
                if bytes.get(j + 1) == Some(&quote) {
                    j += 2;
                } else {
                    return Some(j + 1);
                }
            }
            Some(_) => j += 1,
        }
    }
}

/// The end of a block comment `depth` deep whose text starts at `from`: just
/// past its last `*/`, or the depth it is still open at the end.
fn close_comment(bytes: &[u8], from: usize, mut depth: usize) -> Result<usize, usize> {
    let mut j = from;
    while j < bytes.len() {
        if bytes[j] == b'/' && bytes.get(j + 1) == Some(&b'*') {
            depth += 1;
            j += 2;
        } else if bytes[j] == b'*' && bytes.get(j + 1) == Some(&b'/') {
            depth -= 1;
            j += 2;
            if depth == 0 {
                return Ok(j);
            }
        } else {
            j += 1;
        }
    }
    Err(depth)
}

/// The end of a dollar-quoted string with `tag` whose text starts at `from`:
/// just past the closing `$tag$`, or `None` if it is still open.
fn close_dollar(text: &str, from: usize, tag: &str) -> Option<usize> {
    let close = format!("${tag}$");
    text.get(from..)?
        .find(&close)
        .map(|n| from + n + close.len())
}

/// The state at the end of `text`, starting in `state`.
pub fn state_after(text: &str, state: &State) -> State {
    pieces_from(text, state).1
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// `text` with ANSI colour, as a new statement.
pub fn ansi(text: &str) -> String {
    ansi_from(text, &State::Start)
}

/// `text` with ANSI colour, starting in `state` (the state at the end of
/// the statement typed before it).
pub fn ansi_from(text: &str, state: &State) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for (kind, piece) in pieces_from(text, state).0 {
        let color = match kind {
            Kind::Plain | Kind::Number => None,
            Kind::Keyword => Some(KEYWORD),
            Kind::String => Some(STRING),
            Kind::Comment => Some(COMMENT),
            Kind::Command => Some(COMMAND),
            Kind::Arguments => Some(MUTED),
        };
        match color {
            Some(color) => {
                out.push_str(color);
                out.push_str(piece);
                out.push_str(RESET);
            }
            None => out.push_str(piece),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keywords_are_sorted_for_binary_search() {
        let mut sorted = KEYWORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, KEYWORDS);
    }

    #[test]
    fn sql_is_split_by_kind() {
        let text = "SELECT id, 'it''s' AS s -- note\nFROM t WHERE x = 1.5 AND \"Name\" = 2nd";
        let pieces = pieces(text);
        assert_eq!(pieces.iter().map(|(_, p)| *p).collect::<String>(), text);
        let of = |kind| {
            pieces
                .iter()
                .filter(|(k, _)| *k == kind)
                .map(|(_, p)| *p)
                .collect::<Vec<_>>()
        };
        assert_eq!(of(Kind::Keyword), ["SELECT", "AS", "FROM", "WHERE", "AND"]);
        assert_eq!(of(Kind::String), ["'it''s'"]);
        assert_eq!(of(Kind::Comment), ["-- note"]);
        assert_eq!(of(Kind::Number), ["1.5"]);
    }

    #[test]
    fn an_open_quote_runs_to_the_end() {
        let pieces = pieces("SELECT 'abc");
        assert_eq!(pieces.last(), Some(&(Kind::String, "'abc")));
    }

    #[test]
    fn a_dot_command_and_its_arguments() {
        assert_eq!(
            pieces(".timer on"),
            [(Kind::Command, ".timer"), (Kind::Arguments, " on")]
        );
        assert_eq!(ansi(".help"), format!("{COMMAND}.help{RESET}"));
    }

    /// The pieces of `text` from `state`, by kind, after checking that
    /// they join back to `text`.
    fn kinds(text: &str, state: &State) -> Vec<(Kind, String)> {
        let (pieces, _) = pieces_from(text, state);
        assert_eq!(pieces.iter().map(|(_, p)| *p).collect::<String>(), text);
        pieces
            .into_iter()
            .filter(|(k, _)| *k != Kind::Plain)
            .map(|(k, p)| (k, p.to_string()))
            .collect()
    }

    #[test]
    fn a_line_goes_on_from_the_lines_before() {
        // A string left open goes on to its closing quote.
        let open = state_after("SELECT 'it''s a", &State::Start);
        assert_eq!(open, State::String { escapes: false });
        assert_eq!(
            kinds("long string' AS s", &open),
            [
                (Kind::String, "long string'".into()),
                (Kind::Keyword, "AS".into())
            ]
        );
        // A comment too, and a quoted name, and an escape string.
        let open = state_after("SELECT 1 /* a\n", &State::Start);
        assert_eq!(open, State::Comment(1));
        assert_eq!(
            kinds("note */ FROM t", &open),
            [
                (Kind::Comment, "note */".into()),
                (Kind::Keyword, "FROM".into())
            ]
        );
        assert_eq!(state_after("SELECT \"a", &State::Start), State::QuotedName);
        assert_eq!(
            kinds("from\" FROM t", &State::QuotedName),
            [(Kind::Keyword, "FROM".into())]
        );
        let open = state_after("SELECT E'a\\'", &State::Start);
        assert_eq!(open, State::String { escapes: true });
        assert_eq!(kinds("\\' b'", &open), [(Kind::String, "\\' b'".into())]);
        // Still open at the end of the line: all of it.
        assert_eq!(kinds("no end", &open), [(Kind::String, "no end".into())]);
        assert_eq!(state_after("no end", &open), open);
    }

    #[test]
    fn a_line_in_a_statement_that_starts_with_a_dot_is_sql() {
        assert_eq!(
            kinds(".5 AS half", &State::Statement),
            [(Kind::Number, "5".into()), (Kind::Keyword, "AS".into())]
        );
        assert_eq!(state_after("SELECT", &State::Start), State::Statement);
        assert_eq!(
            state_after("SELECT 1; -- done", &State::Start),
            State::Start
        );
        // After a statement ends, a line of its own is a command again.
        assert_eq!(
            kinds("SELECT 1;\n.mode csv\nSELECT\n.5", &State::Start),
            [
                (Kind::Keyword, "SELECT".into()),
                (Kind::Number, "1".into()),
                (Kind::Command, ".mode".into()),
                (Kind::Arguments, " csv".into()),
                (Kind::Keyword, "SELECT".into()),
                (Kind::Number, "5".into()),
            ]
        );
    }

    #[test]
    fn escape_strings() {
        assert_eq!(
            kinds(r"SELECT E'it\'s', e'\\' AS s, 'a\' AS t", &State::Start),
            [
                (Kind::Keyword, "SELECT".into()),
                (Kind::String, r"E'it\'s'".into()),
                (Kind::String, r"e'\\'".into()),
                (Kind::Keyword, "AS".into()),
                // Not an escape string: the backslash is a character.
                (Kind::String, r"'a\'".into()),
                (Kind::Keyword, "AS".into()),
            ]
        );
        // `E` in a word does not start one.
        assert_eq!(
            kinds("SELECT note'x'", &State::Start)[1],
            (Kind::String, "'x'".into())
        );
    }

    #[test]
    fn dollar_quoted_strings() {
        assert_eq!(
            kinds("SELECT $$it's$$, $tag$a $$ b$tag$ AS s, $1", &State::Start),
            [
                (Kind::Keyword, "SELECT".into()),
                (Kind::String, "$$it's$$".into()),
                (Kind::String, "$tag$a $$ b$tag$".into()),
                (Kind::Keyword, "AS".into()),
            ]
        );
        let open = state_after("SELECT $q$ 'a", &State::Start);
        assert_eq!(open, State::Dollar("q".into()));
        assert_eq!(
            kinds("b$q$ AS s", &open),
            [(Kind::String, "b$q$".into()), (Kind::Keyword, "AS".into())]
        );
        // A `$` in a name is part of it.
        assert_eq!(kinds("SELECT a$b$c", &State::Start).len(), 1);
    }

    #[test]
    fn block_comments_nest() {
        assert_eq!(
            kinds("/* a /* b */ still */ SELECT", &State::Start),
            [
                (Kind::Comment, "/* a /* b */ still */".into()),
                (Kind::Keyword, "SELECT".into())
            ]
        );
        assert_eq!(state_after("/* /* */", &State::Start), State::Comment(1));
        assert_eq!(
            kinds("*/ */ SELECT", &State::Comment(2)),
            [
                (Kind::Comment, "*/ */".into()),
                (Kind::Keyword, "SELECT".into())
            ]
        );
    }

    #[test]
    fn colouring_never_changes_the_text() {
        let states = [
            State::Start,
            State::Statement,
            State::String { escapes: false },
            State::String { escapes: true },
            State::QuotedName,
            State::Dollar(String::new()),
            State::Dollar("t".into()),
            State::Comment(1),
            State::Comment(3),
        ];
        let texts = [
            "",
            ".",
            "  .help x\n.q",
            "SELECT 'a''b' \"c\"\"d\" E'\\\\' $$ $ $t$ */ /* -- \n 1.5e3 x2 熊 ' é",
            "\\",
            "E",
            "e'",
            "$",
            "$a",
            "$$",
            "'",
            "*/",
            "/*",
            "/",
            "-",
            "\n\n.x\n",
        ];
        for state in &states {
            for text in texts {
                let (pieces, _) = pieces_from(text, state);
                assert_eq!(pieces.iter().map(|(_, p)| *p).collect::<String>(), text);
                assert!(
                    pieces.iter().all(|(_, p)| !p.is_empty()),
                    "{text:?} {state:?}"
                );
                let ansi = ansi_from(text, state);
                let stripped = [KEYWORD, STRING, COMMENT, COMMAND, MUTED, RESET]
                    .iter()
                    .fold(ansi, |a, code| a.replace(code, ""));
                assert_eq!(stripped, text, "{state:?}");
            }
        }
    }

    #[test]
    fn graph_patterns() {
        let of = |text| {
            pieces(text)
                .into_iter()
                .filter(|(k, _)| *k == Kind::Keyword)
                .map(|(_, p)| p)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            of("FROM GRAPH_TABLE (g MATCH p = ANY SHORTEST (a)-[e]->*(b) COLUMNS (a.id))"),
            ["FROM", "GRAPH_TABLE", "MATCH", "ANY", "SHORTEST", "COLUMNS"]
        );
    }
}
