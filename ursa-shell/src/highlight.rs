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

/// `text` split into pieces, each with its kind. The pieces joined are `text`.
pub fn pieces<'a>(text: &'a str) -> Vec<(Kind, &'a str)> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('.') {
        let start = text.len() - trimmed.len();
        let end = trimmed
            .find(char::is_whitespace)
            .map_or(text.len(), |i| start + i);
        let mut out = Vec::new();
        if start > 0 {
            out.push((Kind::Plain, &text[..start]));
        }
        out.push((Kind::Command, &text[start..end]));
        if end < text.len() {
            out.push((Kind::Arguments, &text[end..]));
        }
        return out;
    }
    let bytes = text.as_bytes();
    let mut out: Vec<(Kind, &'a str)> = Vec::new();
    let mut i = 0;
    let mut plain_from = 0;
    let flush = |out: &mut Vec<(Kind, &'a str)>, from: usize, to: usize| {
        if from < to {
            out.push((Kind::Plain, &text[from..to]));
        }
    };
    while i < bytes.len() {
        let c = bytes[i];
        let (kind, end) = if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            (
                Kind::Comment,
                text[i..].find('\n').map_or(text.len(), |n| i + n),
            )
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            (
                Kind::Comment,
                text[i + 2..]
                    .find("*/")
                    .map_or(text.len(), |n| i + 2 + n + 2),
            )
        } else if c == b'\'' || c == b'"' {
            // To the closing quote (a doubled quote is part of the text), or
            // the end while it is still open.
            let mut j = i + 1;
            loop {
                match bytes.get(j) {
                    None => break,
                    Some(&q) if q == c => {
                        if bytes.get(j + 1) == Some(&c) {
                            j += 2;
                        } else {
                            j += 1;
                            break;
                        }
                    }
                    Some(_) => j += 1,
                }
            }
            // A quoted name is plain; a string literal is a string.
            (
                if c == b'\'' {
                    Kind::String
                } else {
                    Kind::Plain
                },
                j,
            )
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
                (Kind::Plain, j)
            } else {
                (Kind::Number, j)
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
            (kind, j)
        } else {
            // One character (it may be several bytes).
            let len = text[i..].chars().next().map_or(1, char::len_utf8);
            i += len;
            continue;
        };
        if kind == Kind::Plain {
            i = end;
            continue;
        }
        flush(&mut out, plain_from, i);
        out.push((kind, &text[i..end]));
        i = end;
        plain_from = end;
    }
    flush(&mut out, plain_from, text.len());
    out
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// `text` with ANSI colour.
pub fn ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for (kind, piece) in pieces(text) {
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
