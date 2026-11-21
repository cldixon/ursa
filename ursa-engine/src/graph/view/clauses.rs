//! `.graph`'s text: `[SOURCE] [NODES (query)] [SIZE BY x] [COLOR BY y]`.
//! [`parse`] splits it into the source and a [`Style`]; [`identifier`] says
//! whether a source is one name.

use datafusion::error::Result;

use super::plan_err;

/// What `.graph` text asks for besides its source.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Style {
    /// `NODES (query)`: the query.
    pub nodes: Option<String>,
    pub size: Option<Name>,
    pub color: Option<Name>,
}

/// A name after `SIZE BY` or `COLOR BY`: as SQL reads it (lower-cased unless
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

/// Split `.graph`'s text into its source (empty for the last result) and
/// the clauses after it: `NODES (query)`, `SIZE BY x`, `COLOR BY y`, in any
/// order, each at most once. A clause starts at the first `SIZE BY`, `COLOR
/// BY` (or `COLOUR BY`) or `NODES (` outside parentheses, strings and
/// comments.
pub fn parse(text: &str) -> Result<(String, Style)> {
    let text = text.trim().trim_end_matches(';').trim_end();
    let toks = tokens(text);
    let word = |i: usize| -> Option<String> {
        toks.get(i)
            .filter(|t| t.0 == Tok::Word)
            .map(|t| text[t.1..t.2].to_ascii_uppercase())
    };
    let is_clause = |i: usize| match word(i).as_deref() {
        Some("SIZE" | "COLOR" | "COLOUR") => word(i + 1).as_deref() == Some("BY"),
        Some("NODES") => toks.get(i + 1).map(|t| t.0) == Some(Tok::Open),
        _ => false,
    };
    let mut depth = 0i32;
    let mut first = None;
    for (i, t) in toks.iter().enumerate() {
        match t.0 {
            Tok::Open => depth += 1,
            Tok::Close => depth -= 1,
            _ if depth == 0 && is_clause(i) => {
                first = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(mut i) = first else {
        return Ok((text.to_string(), Style::default()));
    };
    let source = text[..toks[i].1].trim().to_string();
    let mut style = Style::default();
    const USAGE: &str = ".graph [SOURCE] [NODES (SELECT id, … FROM …)] [SIZE BY x] [COLOR BY y]";
    while i < toks.len() {
        let t = toks[i];
        let shown = &text[t.1..t.2];
        let keyword = word(i).unwrap_or_default();
        match keyword.as_str() {
            "NODES" if toks.get(i + 1).map(|t| t.0) == Some(Tok::Open) => {
                if style.nodes.is_some() {
                    return plan_err(format!("NODES is given twice; give it once: {USAGE}"));
                }
                let open = toks[i + 1];
                let mut depth = 0;
                let mut close = None;
                for (j, t) in toks.iter().enumerate().skip(i + 1) {
                    match t.0 {
                        Tok::Open => depth += 1,
                        Tok::Close => {
                            depth -= 1;
                            if depth == 0 {
                                close = Some(j);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let Some(close) = close else {
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
                let name = match toks.get(i + 2) {
                    Some(&(Tok::Word, s, e)) => Name {
                        ident: text[s..e].to_lowercase(),
                        shown: text[s..e].to_string(),
                    },
                    Some(&(Tok::Quoted, s, e)) if e - s > 2 && text[s..e].ends_with('"') => Name {
                        ident: text[s + 1..e - 1].replace("\"\"", "\""),
                        shown: text[s..e].to_string(),
                    },
                    _ => {
                        let what = if keyword == "SIZE" {
                            "a numeric column or an algorithm, as SIZE BY pagerank"
                        } else {
                            "a column of categories or an algorithm, as COLOR BY louvain"
                        };
                        return plan_err(format!("{clause} needs {what}"));
                    }
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
            _ => {
                return plan_err(format!(
                    "unexpected '{shown}' after the graph's source; the clauses are NODES (…), \
                     SIZE BY x and COLOR BY y: {USAGE}"
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
