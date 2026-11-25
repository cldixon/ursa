//! `complete` and `highlight` (`design/console.md` §4.3), for the console's
//! editor. Both are cheap: they read the text, the catalog and the function
//! registry, and run nothing.
//!
//! Offsets are UTF-16 code units, as JavaScript strings (and CodeMirror)
//! count them: `cursor` in, `from`, `to` and each span's out.

use std::collections::{BTreeMap, BTreeSet};

use datafusion::prelude::SessionContext;
use serde_json::{json, Value};
use ursa_engine::graph;

use super::catalog::{self, TableInfo};
use crate::highlight::{self, Kind, State};

/// The byte offset of the UTF-16 offset `units` in `text` (its end, past
/// it; the next character, inside one).
fn byte_at(text: &str, units: usize) -> usize {
    let mut seen = 0;
    for (i, c) in text.char_indices() {
        if seen >= units {
            return i;
        }
        seen += c.len_utf16();
    }
    text.len()
}

/// The UTF-16 offset of the byte offset `byte` in `text`.
fn units_at(text: &str, byte: usize) -> usize {
    text[..byte].encode_utf16().count()
}

/// The name of a piece's kind on the wire.
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Plain => "plain",
        Kind::Keyword => "keyword",
        Kind::String => "string",
        Kind::Number => "number",
        Kind::Comment => "comment",
        Kind::Command => "command",
        Kind::Arguments => "arguments",
    }
}

/// `highlight {sql}`: `{spans: [{from, to, kind}]}`, the pieces of `sql` as
/// the shell colours them (`highlight::pieces`), but for plain text: `kind`
/// is `keyword`, `string`, `number`, `comment`, `command` or `arguments`.
pub(crate) fn highlight(sql: &str) -> Value {
    let mut spans = Vec::new();
    let (mut at, mut units) = (0usize, 0usize);
    for (kind, text) in highlight::pieces(sql) {
        let len = text.encode_utf16().count();
        if kind != Kind::Plain && !text.trim().is_empty() {
            spans.push(json!({"from": units, "to": units + len, "kind": kind_name(kind)}));
        }
        at += text.len();
        units += len;
    }
    debug_assert_eq!(at, sql.len());
    json!({ "spans": spans })
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A token of a statement, for finding its tables and their aliases.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Punct(char),
}

/// The tokens of `sql`'s plain and keyword pieces, with their byte offsets:
/// words (a quoted name unquoted) and punctuation. Strings, numbers and
/// comments are left out, a string as a word of its own (`FROM 'x.csv'`).
fn tokens(sql: &str) -> Vec<(usize, Token)> {
    let mut out = Vec::new();
    let mut at = 0;
    for (kind, text) in highlight::pieces(sql) {
        let start = at;
        at += text.len();
        match kind {
            Kind::Plain | Kind::Keyword => {}
            Kind::String => {
                out.push((start, Token::Word(text.to_string())));
                continue;
            }
            Kind::Number => {
                out.push((start, Token::Word(text.to_string())));
                continue;
            }
            _ => continue,
        }
        let mut chars = text.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == '"' {
                let mut name = String::new();
                while let Some((_, d)) = chars.next() {
                    if d == '"' {
                        if chars.peek().map(|p| p.1) == Some('"') {
                            chars.next();
                            name.push('"');
                            continue;
                        }
                        break;
                    }
                    name.push(d);
                }
                out.push((start + i, Token::Word(name)));
            } else if is_ident(c) {
                let mut word = String::from(c);
                while let Some(&(_, d)) = chars.peek() {
                    if !is_ident(d) {
                        break;
                    }
                    word.push(d);
                    chars.next();
                }
                out.push((start + i, Token::Word(word)));
            } else if !c.is_whitespace() {
                out.push((start + i, Token::Punct(c)));
            }
        }
    }
    out
}

fn is_keyword(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    highlight::KEYWORDS.contains(&lower.as_str())
}

/// The tables a statement reads (`FROM` and `JOIN`), by the names it may
/// qualify a column with: its alias and its own name, lower case, each to
/// the table's (possibly qualified) name as written.
fn table_aliases(tokens: &[Token]) -> BTreeMap<String, String> {
    let mut aliases = BTreeMap::new();
    let mut i = 0;
    while i < tokens.len() {
        let after_from = matches!(&tokens[i], Token::Word(w)
            if w.eq_ignore_ascii_case("from") || w.eq_ignore_ascii_case("join"));
        i += 1;
        if !after_from {
            continue;
        }
        // One or more `name [AS] [alias]`, separated by commas.
        while let Some(Token::Word(first)) = tokens.get(i) {
            let mut name = first.clone();
            i += 1;
            while let (Some(Token::Punct('.')), Some(Token::Word(part))) =
                (tokens.get(i), tokens.get(i + 1))
            {
                name = format!("{name}.{part}");
                i += 2;
            }
            if tokens.get(i) == Some(&Token::Punct('(')) {
                // A table function: not a table.
                break;
            }
            let last = name.rsplit('.').next().unwrap_or(&name).to_string();
            aliases.insert(last.to_lowercase(), name.clone());
            if let Some(Token::Word(w)) = tokens.get(i) {
                if w.eq_ignore_ascii_case("as") {
                    i += 1;
                }
            }
            if let Some(Token::Word(alias)) = tokens.get(i) {
                if !is_keyword(alias) {
                    aliases.insert(alias.to_lowercase(), name.clone());
                    i += 1;
                }
            }
            if tokens.get(i) == Some(&Token::Punct(',')) {
                i += 1;
                continue;
            }
            break;
        }
    }
    aliases
}

/// The table `name` (`t`, `s.t` or `c.s.t`, as written) names, among
/// `tables`: one in the default schema for a bare name.
fn find_table<'a>(
    tables: &'a [TableInfo],
    name: &str,
    default: (&str, &str),
) -> Option<&'a TableInfo> {
    let parts: Vec<&str> = name.split('.').collect();
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    tables.iter().find(|t| match parts.as_slice() {
        [table] => eq(&t.name, table) && t.catalog == default.0 && t.schema == default.1,
        [schema, table] => eq(&t.name, table) && eq(&t.schema, schema) && t.catalog == default.0,
        [catalog, schema, table] => {
            eq(&t.name, table) && eq(&t.schema, schema) && eq(&t.catalog, catalog)
        }
        _ => false,
    })
}

/// One completion.
struct Item {
    label: String,
    kind: &'static str,
    detail: String,
}

impl Item {
    fn new(label: impl Into<String>, kind: &'static str, detail: impl Into<String>) -> Item {
        Item {
            label: label.into(),
            kind,
            detail: detail.into(),
        }
    }
}

fn column_items(table: &TableInfo, items: &mut Vec<Item>) {
    for (name, ty, _) in &table.columns {
        items.push(Item::new(name, "column", format!("{ty} · {}", table.name)));
    }
}

/// The current statement's range around `cur`: from after the `;` before
/// it to its next `;`, outside strings and comments.
fn statement_range(sql: &str, cur: usize) -> (usize, usize) {
    let (mut start, mut end, mut at) = (0, sql.len(), 0);
    for (kind, text) in highlight::pieces(sql) {
        if kind == Kind::Plain {
            for (i, c) in text.char_indices() {
                if c == ';' {
                    let pos = at + i;
                    if pos < cur {
                        start = pos + 1;
                    } else if end == sql.len() {
                        end = pos;
                    }
                }
            }
        }
        at += text.len();
    }
    (start, end.max(start))
}

/// `complete {sql, cursor}`: `{from, to, items: [{label, kind, detail}]}`,
/// the completions for the word at `cursor`, which replace `from..to`.
/// `kind` is `table`, `view`, `external`, `schema`, `column`, `function`,
/// `graph`, `keyword` or `command`. After `t.`, the columns of the table
/// `t` names (or aliases), or the tables of the schema `t`; else tables,
/// the columns of the statement's tables, functions, graphs and keywords;
/// at the start of a line, dot-commands.
pub(crate) async fn complete(
    ctx: &SessionContext,
    commands: Vec<(String, String)>,
    sql: &str,
    cursor: usize,
) -> Value {
    let cur = byte_at(sql, cursor);
    let before = &sql[..cur];
    let none = |at: usize| {
        let at = units_at(sql, at);
        json!({"from": at, "to": at, "items": []})
    };
    // In a string, a quoted name or a comment: nothing.
    let state = highlight::state_after(before, &State::Start);
    if !matches!(state, State::Start | State::Statement) {
        return none(cur);
    }
    let pieces = highlight::pieces(before);
    if let Some((kind, text)) = pieces.last() {
        match kind {
            Kind::Comment => return none(cur),
            Kind::Arguments => return none(cur),
            Kind::Command => {
                let typed = text.trim_start();
                let from = cur - typed.len();
                let items: Vec<Value> = commands
                    .iter()
                    .filter(|(name, _)| name.starts_with(typed))
                    .map(|(name, help)| json!({"label": name, "kind": "command", "detail": help}))
                    .collect();
                return json!({"from": units_at(sql, from), "to": units_at(sql, cur), "items": items});
            }
            _ => {}
        }
    }

    // The word at the cursor, and what qualifies it (`s.t.`).
    let word_start = before
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_ident(c))
        .last()
        .map_or(cur, |(i, _)| i);
    let word_end = cur
        + sql[cur..]
            .char_indices()
            .find(|&(_, c)| !is_ident(c))
            .map_or(sql.len() - cur, |(i, _)| i);
    let word = &sql[word_start..cur];
    let mut qualifier = Vec::new();
    let mut at = word_start;
    while sql[..at].ends_with('.') {
        let head = &sql[..at - 1];
        let part_start = if let Some(quoted) = head.strip_suffix('"') {
            match quoted.rfind('"') {
                Some(q) => q,
                None => break,
            }
        } else {
            head.char_indices()
                .rev()
                .take_while(|&(_, c)| is_ident(c))
                .last()
                .map_or(head.len(), |(i, _)| i)
        };
        if part_start == head.len() {
            break;
        }
        qualifier.insert(0, head[part_start..].trim_matches('"').to_string());
        at = part_start;
    }

    let state = ctx.state();
    let options = &state.config().options().catalog;
    let default = (
        options.default_catalog.as_str(),
        options.default_schema.as_str(),
    );
    let tables = catalog::tables(ctx).await;
    let (stmt_start, stmt_end) = statement_range(sql, cur);
    let toks: Vec<Token> = tokens(&sql[stmt_start..stmt_end])
        .into_iter()
        .map(|(_, t)| t)
        .collect();
    let aliases = table_aliases(&toks);
    let registry = graph::registry_of(ctx);
    let graph_names = registry.names();

    let mut items = Vec::new();
    if !qualifier.is_empty() {
        let q = qualifier.join(".");
        let lower = q.to_lowercase();
        // A table or its alias: its columns.
        let named = aliases.get(&lower).cloned().unwrap_or_else(|| q.clone());
        if let Some(table) = find_table(&tables, &named, default) {
            column_items(table, &mut items);
        }
        // A schema: its tables. A database: its schemas.
        for t in &tables {
            let in_schema = match qualifier.as_slice() {
                [s] => t.catalog == default.0 && t.schema.eq_ignore_ascii_case(s),
                [c, s] => t.catalog.eq_ignore_ascii_case(c) && t.schema.eq_ignore_ascii_case(s),
                _ => false,
            };
            if in_schema {
                items.push(Item::new(
                    &t.name,
                    t.kind,
                    format!("{} {}", t.kind, t.schema),
                ));
            }
        }
        if let [c] = qualifier.as_slice() {
            let schemas: BTreeSet<&str> = tables
                .iter()
                .filter(|t| t.catalog.eq_ignore_ascii_case(c))
                .map(|t| t.schema.as_str())
                .collect();
            for s in schemas {
                items.push(Item::new(s, "schema", format!("schema of {c}")));
            }
            // A graph: its `nodes` and `edges`.
            if let Some(g) = graph_names.iter().find(|g| g.eq_ignore_ascii_case(c)) {
                if registry.get(g).is_some_and(|g| g.default_pair().is_some()) {
                    for t in [graph::schema::NODES, graph::schema::EDGES] {
                        items.push(Item::new(t, "view", format!("{t} of graph {g}")));
                    }
                }
            }
        }
    } else {
        // The statement's tables' columns first: what is most likely meant.
        let mut seen = BTreeSet::new();
        for name in aliases.values() {
            if let Some(table) = find_table(&tables, name, default) {
                if seen.insert((&table.catalog, &table.schema, &table.name)) {
                    column_items(table, &mut items);
                }
            }
        }
        for t in &tables {
            let label = if (t.catalog.as_str(), t.schema.as_str()) == default {
                t.name.clone()
            } else if t.catalog == default.0 {
                format!("{}.{}", t.schema, t.name)
            } else {
                format!("{}.{}.{}", t.catalog, t.schema, t.name)
            };
            items.push(Item::new(
                label,
                t.kind,
                format!("{} · {} columns", t.kind, t.columns.len()),
            ));
        }
        for g in &graph_names {
            items.push(Item::new(g, "graph", "property graph"));
        }
        let graph_functions: BTreeSet<&str> = graph::functions::names().collect();
        let mut functions: BTreeMap<String, &'static str> = BTreeMap::new();
        for name in state.scalar_functions().keys() {
            functions.insert(name.clone(), "scalar function");
        }
        for name in state.aggregate_functions().keys() {
            functions.insert(name.clone(), "aggregate function");
        }
        for name in state.window_functions().keys() {
            functions.entry(name.clone()).or_insert("window function");
        }
        for name in state.table_functions().keys() {
            functions.insert(name.clone(), "table function");
        }
        for name in &graph_functions {
            functions.insert(name.to_string(), "graph function");
        }
        for (name, detail) in functions {
            items.push(Item::new(name, "function", detail));
        }
        for k in highlight::KEYWORDS {
            items.push(Item::new(k.to_ascii_uppercase(), "keyword", "keyword"));
        }
    }

    let typed = word.to_lowercase();
    let mut labels = BTreeSet::new();
    let items: Vec<Value> = items
        .into_iter()
        .filter(|i| i.label.to_lowercase().starts_with(&typed))
        .filter(|i| labels.insert((i.label.clone(), i.kind)))
        .map(|i| json!({"label": i.label, "kind": i.kind, "detail": i.detail}))
        .collect();
    json!({
        "from": units_at(sql, word_start),
        "to": units_at(sql, word_end),
        "items": items,
    })
}
