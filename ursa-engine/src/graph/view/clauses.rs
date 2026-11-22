//! `.graph`'s text (design §3, #434):
//!
//! ```text
//! [SOURCE] [ON graph]
//!          [AS edges(src, dst [, weight]) | nodes(id) | path(nodes [, edges]) | hops(seed, id, distance)]
//!          [NODES (query)] [SIZE BY x] [COLOR BY y] [LAYOUT force|rings|layers]
//! ```
//!
//! [`parse`] splits it into the source and a [`Style`]; [`identifier`] says
//! whether a source is one name.

use datafusion::error::Result;
use ursa_viz::model::Layout;

use super::plan_err;

/// What `.graph` text asks for besides its source.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Style {
    /// `NODES (query)`: the query.
    pub nodes: Option<String>,
    pub size: Option<Name>,
    pub color: Option<Name>,
    /// `AS …`: what the result's rows are and the columns to read. It is
    /// rule 0: with it, nothing is inferred.
    pub shape: Option<As>,
    /// `ON graph`: the property graph the result's ids belong to.
    pub on: Option<Name>,
    /// `LAYOUT …`: the layout, whatever the rule chose.
    pub layout: Option<Layout>,
}

/// An `AS …` clause: the shape of a result's rows, and its columns by name.
#[derive(Debug, Clone, PartialEq)]
pub enum As {
    /// `AS edges(src, dst [, weight])`: a row is an edge.
    Edges {
        src: Name,
        dst: Name,
        weight: Option<Name>,
    },
    /// `AS nodes(id)`: a row is a node.
    Nodes { id: Name },
    /// `AS path(nodes [, edges])`: a row is a path, its nodes in a list
    /// column and its edges' rows in another.
    Path { nodes: Name, edges: Option<Name> },
    /// `AS hops(seed, id, distance)`: a row is a node reached from a seed.
    Hops {
        seed: Name,
        id: Name,
        distance: Name,
    },
}

impl As {
    /// The clause as it is written, for a message.
    pub fn shown(&self) -> String {
        let join = |names: &[Option<&Name>]| {
            names
                .iter()
                .flatten()
                .map(|n| n.shown.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self {
            As::Edges { src, dst, weight } => {
                format!(
                    "AS edges({})",
                    join(&[Some(src), Some(dst), weight.as_ref()])
                )
            }
            As::Nodes { id } => format!("AS nodes({})", id.shown),
            As::Path { nodes, edges } => {
                format!("AS path({})", join(&[Some(nodes), edges.as_ref()]))
            }
            As::Hops { seed, id, distance } => {
                format!("AS hops({})", join(&[Some(seed), Some(id), Some(distance)]))
            }
        }
    }
}

/// A name in a clause (`SIZE BY x`, `AS nodes(x)`, `ON g`): as SQL reads it (lower-cased unless
/// quoted), and as typed.
#[derive(Debug, Clone, PartialEq)]
pub struct Name {
    pub ident: String,
    pub shown: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok {
    Word,
    Quoted,
    Str,
    Open,
    Close,
    Other,
}

/// `text` as tokens (kind, start, end), SQL's strings, quoted names and
/// comments kept whole, so a word in them is never read as a clause.
fn tokens(text: &str) -> Vec<(Tok, usize, usize)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        let kind = if c == b'\'' || c == b'"' {
            // To the closing quote; a doubled quote is one quote.
            i += 1;
            loop {
                if i >= b.len() {
                    break;
                }
                if b[i] == c {
                    if b.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            if c == b'"' {
                Tok::Quoted
            } else {
                Tok::Str
            }
        } else if c == b'(' {
            i += 1;
            Tok::Open
        } else if c == b')' {
            i += 1;
            Tok::Close
        } else if c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80 {
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80)
            {
                i += 1;
            }
            Tok::Word
        } else {
            i += 1;
            Tok::Other
        };
        out.push((kind, start, i));
    }
    out
}

/// `.graph`'s clauses, as a message that goes wrong shows them.
pub const USAGE: &str = ".graph [SOURCE] [ON graph] [AS edges(src, dst [, weight]) | nodes(id) | \
                         path(nodes [, edges]) | hops(seed, id, distance)] [NODES (SELECT id, … \
                         FROM …)] [SIZE BY x] [COLOR BY y] [LAYOUT force|rings|layers]";

/// The four forms of `AS …`, as a message shows them.
const FORMS: &str =
    "AS edges(src, dst [, weight]), AS nodes(id), AS path(nodes [, edges]) or AS hops(seed, id, \
     distance)";

/// The name a token is, if it is one: a word, lower-cased as SQL lower-cases
/// names, or a quoted name, kept.
fn name_of(text: &str, tok: Option<&(Tok, usize, usize)>) -> Option<Name> {
    match tok {
        Some(&(Tok::Word, s, e)) => Some(Name {
            ident: text[s..e].to_lowercase(),
            shown: text[s..e].to_string(),
        }),
        Some(&(Tok::Quoted, s, e)) if e - s > 2 && text[s..e].ends_with('"') => Some(Name {
            ident: text[s + 1..e - 1].replace("\"\"", "\""),
            shown: text[s..e].to_string(),
        }),
        _ => None,
    }
}

/// The layout `word` names.
fn layout_of(word: &str) -> Option<Layout> {
    match word {
        "FORCE" => Some(Layout::Force),
        "RINGS" => Some(Layout::Rings),
        "LAYERS" => Some(Layout::Layers),
        _ => None,
    }
}

/// Split `.graph`'s text into its source (empty for the last result) and
/// the clauses after it: `ON graph`, `AS shape(columns)`, `NODES (query)`,
/// `SIZE BY x`, `COLOR BY y` and `LAYOUT l`, in any order, each at most
/// once. The clauses start at the first of these outside parentheses,
/// strings and comments:
///
/// - `SIZE BY`, `COLOR BY` (or `COLOUR BY`), `NODES (`;
/// - `AS edges(`, `AS nodes(`, `AS path(` or `AS hops(`. (SQL's own
///   `FROM t AS edges(a, b)`, a table alias with column names, is read as the
///   clause: give such an alias another name.)
/// - `LAYOUT force`, `LAYOUT rings` or `LAYOUT layers`;
/// - `ON name` followed by nothing or by another clause, unless it is the
///   `ON` of a `JOIN`.
///
/// Text that begins with `AS`, `ON` or `LAYOUT` is clauses, with no source.
pub fn parse(text: &str) -> Result<(String, Style)> {
    let text = text.trim().trim_end_matches(';').trim_end();
    let toks = tokens(text);
    let word = |i: usize| -> Option<String> {
        toks.get(i)
            .filter(|t| t.0 == Tok::Word)
            .map(|t| text[t.1..t.2].to_ascii_uppercase())
    };
    let kind = |i: usize| toks.get(i).map(|t| t.0);
    // Whether a clause starts at token `i` of `n`. `ON name` is one only
    // before the end or another clause.
    fn starts(
        i: usize,
        word: &dyn Fn(usize) -> Option<String>,
        kind: &dyn Fn(usize) -> Option<Tok>,
        n: usize,
    ) -> bool {
        match word(i).as_deref() {
            Some("SIZE" | "COLOR" | "COLOUR") => word(i + 1).as_deref() == Some("BY"),
            Some("NODES") => kind(i + 1) == Some(Tok::Open),
            Some("AS") => {
                i == 0
                    || (matches!(
                        word(i + 1).as_deref(),
                        Some("EDGES" | "NODES" | "PATH" | "HOPS")
                    ) && kind(i + 2) == Some(Tok::Open))
            }
            Some("LAYOUT") => i == 0 || word(i + 1).as_deref().and_then(layout_of).is_some(),
            Some("ON") => {
                i == 0
                    || (matches!(kind(i + 1), Some(Tok::Word | Tok::Quoted))
                        && (i + 2 >= n || starts(i + 2, word, kind, n)))
            }
            _ => false,
        }
    }
    let is_clause = |i: usize| starts(i, &word, &kind, toks.len());
    let mut depth = 0i32;
    let mut first = None;
    // JOINs still waiting for their ON or USING: that ON is SQL's.
    let mut joins = 0u32;
    for (i, t) in toks.iter().enumerate() {
        match t.0 {
            Tok::Open => depth += 1,
            Tok::Close => depth -= 1,
            Tok::Word if depth == 0 => {
                let w = word(i).unwrap_or_default();
                match w.as_str() {
                    "JOIN"
                        if !matches!(
                            i.checked_sub(1).and_then(&word).as_deref(),
                            Some("CROSS" | "NATURAL")
                        ) =>
                    {
                        joins += 1
                    }
                    "ON" | "USING" if joins > 0 => joins -= 1,
                    _ if is_clause(i) => {
                        first = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let Some(mut i) = first else {
        return Ok((text.to_string(), Style::default()));
    };
    let source = text[..toks[i].1].trim().to_string();
    let mut style = Style::default();
    // The token that closes the parenthesis opened at token `open`.
    let closing = |open: usize| -> Option<usize> {
        let mut depth = 0;
        for (j, t) in toks.iter().enumerate().skip(open) {
            match t.0 {
                Tok::Open => depth += 1,
                Tok::Close => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
        }
        None
    };
    while i < toks.len() {
        let t = toks[i];
        let shown = &text[t.1..t.2];
        let keyword = word(i).unwrap_or_default();
        match keyword.as_str() {
            "NODES" if kind(i + 1) == Some(Tok::Open) => {
                if style.nodes.is_some() {
                    return plan_err(format!("NODES is given twice; give it once: {USAGE}"));
                }
                let open = toks[i + 1];
                let Some(close) = closing(i + 1) else {
                    return plan_err("NODES ( is not closed: NODES (SELECT id, … FROM …)");
                };
                let query = text[open.2..toks[close].1].trim();
                if query.is_empty() {
                    return plan_err("NODES () is empty: NODES (SELECT id, … FROM …)");
                }
                style.nodes = Some(query.to_string());
                i = close + 1;
            }
            "SIZE" | "COLOR" | "COLOUR" if word(i + 1).as_deref() == Some("BY") => {
                let clause = if keyword == "SIZE" {
                    "SIZE BY"
                } else {
                    "COLOR BY"
                };
                let Some(name) = name_of(text, toks.get(i + 2)) else {
                    let what = if keyword == "SIZE" {
                        "a numeric column or an algorithm, as SIZE BY pagerank"
                    } else {
                        "a column of categories or an algorithm, as COLOR BY louvain"
                    };
                    return plan_err(format!("{clause} needs {what}"));
                };
                let slot = if keyword == "SIZE" {
                    &mut style.size
                } else {
                    &mut style.color
                };
                if slot.is_some() {
                    return plan_err(format!("{clause} is given twice; give it once: {USAGE}"));
                }
                *slot = Some(name);
                i += 3;
            }
            "AS" => {
                if style.shape.is_some() {
                    return plan_err(format!(
                        "AS is given twice; a result is drawn one way, so give one of {FORMS}"
                    ));
                }
                let Some(form) = word(i + 1) else {
                    return plan_err(format!(
                        "AS needs what the result's rows are and the columns to read: {FORMS}"
                    ));
                };
                let form_shown = &text[toks[i + 1].1..toks[i + 1].2];
                let usage = match form.as_str() {
                    "EDGES" => "AS edges(src, dst [, weight])",
                    "NODES" => "AS nodes(id)",
                    "PATH" => "AS path(nodes [, edges])",
                    "HOPS" => "AS hops(seed, id, distance)",
                    // What was probably meant.
                    "EDGE" => {
                        return plan_err(format!(
                            "AS {form_shown} is not a form; write AS edges(src, dst [, weight]). \
                             The forms are {FORMS}"
                        ))
                    }
                    "NODE" => {
                        return plan_err(format!(
                            "AS {form_shown} is not a form; write AS nodes(id). The forms are \
                             {FORMS}"
                        ))
                    }
                    "PATHS" => {
                        return plan_err(format!(
                            "AS {form_shown} is not a form; write AS path(nodes [, edges]). The \
                             forms are {FORMS}"
                        ))
                    }
                    "HOP" | "RINGS" => {
                        return plan_err(format!(
                            "AS {form_shown} is not a form; write AS hops(seed, id, distance). \
                             The forms are {FORMS}"
                        ))
                    }
                    _ => {
                        return plan_err(format!(
                            "AS {form_shown} is not a form; write one of {FORMS}"
                        ))
                    }
                };
                if kind(i + 2) != Some(Tok::Open) {
                    return plan_err(format!(
                        "AS {form_shown} needs its columns in parentheses: {usage}"
                    ));
                }
                let Some(close) = closing(i + 2) else {
                    return plan_err(format!("AS {form_shown}( is not closed: {usage}"));
                };
                // Names, with a comma between each two.
                let mut names = Vec::new();
                let mut j = i + 3;
                while j < close {
                    let Some(name) = name_of(text, toks.get(j)) else {
                        return plan_err(format!(
                            "AS {form_shown}(…) takes the result's column names, with commas \
                             between them, and '{}' is not one: {usage}",
                            &text[toks[j].1..toks[j].2]
                        ));
                    };
                    names.push(name);
                    j += 1;
                    if j < close {
                        let comma = toks[j].0 == Tok::Other && &text[toks[j].1..toks[j].2] == ",";
                        if !comma || j + 1 == close {
                            return plan_err(format!(
                                "AS {form_shown}(…) takes the result's column names, with \
                                 commas between them: {usage}"
                            ));
                        }
                        j += 1;
                    }
                }
                let given = names.len();
                let mut names = names.into_iter();
                let mut next = || names.next().expect("counted");
                let wrong = |takes: &str| -> Result<(String, Style)> {
                    plan_err(format!(
                        "AS {form_shown}(…) takes {takes}, and was given {given}: {usage}"
                    ))
                };
                style.shape = Some(match (form.as_str(), given) {
                    ("EDGES", 2 | 3) => As::Edges {
                        src: next(),
                        dst: next(),
                        weight: names.next(),
                    },
                    ("EDGES", _) => {
                        return wrong("the columns of an edge's two ends, and then its weight's")
                    }
                    ("NODES", 1) => As::Nodes { id: next() },
                    ("NODES", _) => return wrong("one column, the node's id"),
                    ("PATH", 1 | 2) => As::Path {
                        nodes: next(),
                        edges: names.next(),
                    },
                    ("PATH", _) => {
                        return wrong(
                            "the column of each path's nodes, and then the column of its edges",
                        )
                    }
                    ("HOPS", 3) => As::Hops {
                        seed: next(),
                        id: next(),
                        distance: next(),
                    },
                    _ => {
                        return wrong("three columns: the seed, the node reached and its distance")
                    }
                });
                i = close + 1;
            }
            "ON" => {
                if style.on.is_some() {
                    return plan_err(format!(
                        "ON is given twice; a result's ids belong to one graph, so give it once: \
                         {USAGE}"
                    ));
                }
                let Some(name) = name_of(text, toks.get(i + 1)) else {
                    return plan_err(
                        "ON needs the name of the property graph the result's ids belong to, as \
                         ON karate; .graphs lists the graphs",
                    );
                };
                style.on = Some(name);
                i += 2;
            }
            "LAYOUT" => {
                if style.layout.is_some() {
                    return plan_err(format!("LAYOUT is given twice; give it once: {USAGE}"));
                }
                let Some(layout) = word(i + 1).as_deref().and_then(layout_of) else {
                    let given = match toks.get(i + 1) {
                        Some(t) => format!(", and '{}' is none of them", &text[t.1..t.2]),
                        None => String::new(),
                    };
                    return plan_err(format!(
                        "LAYOUT needs force, rings or layers{given}: LAYOUT force lets the edges \
                         pull the nodes into place, LAYOUT rings puts a hop result's nodes on \
                         circles by distance, and LAYOUT layers puts them in rows"
                    ));
                };
                style.layout = Some(layout);
                i += 2;
            }
            _ => {
                return plan_err(format!(
                    "unexpected '{shown}' after the graph's source; the clauses are ON graph, \
                     AS …, NODES (…), SIZE BY x, COLOR BY y and LAYOUT l: {USAGE}"
                ));
            }
        }
    }
    Ok((source, style))
}

/// What `.graph NAME` names, if NAME is one identifier: unquoted, it is
/// lower-cased as SQL lower-cases names; quoted, it is kept.
pub fn identifier(text: &str) -> Option<String> {
    let text = text.trim();
    if let Some(quoted) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        if quoted.is_empty() || quoted.replace("\"\"", "").contains('"') {
            return None;
        }
        return Some(quoted.replace("\"\"", "\""));
    }
    let mut chars = text.chars();
    let first = chars.next()?;
    if !(first.is_alphabetic() || first == '_')
        || !chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
    {
        return None;
    }
    Some(text.to_lowercase())
}
