//! `.find` (design §6 "Search and focus", #445): the nodes of the graph that
//! is drawn that match a text or a condition, for the view to select and fit.
//!
//! | command | what matches |
//! |---|---|
//! | `.find TEXT` | a node whose key starts with TEXT, then one whose label column (the vertex table's first text column that is not its key) contains TEXT; letter case is ignored |
//! | `.find WHERE condition` | a row of the vertex table the condition holds for, the condition being SQL on the table's columns |
//!
//! The matches that are shown in the view are the selection ([`Found::selected`]):
//! a key that is TEXT itself first, then the keys that start with it, then
//! the label matches, each in the table's key order (the view's own order
//! where no table is asked); at most [`MAX_SELECTED`], with the true count
//! beside them. The matches the view does not show (a hidden node
//! of the session, or a node of the graph that was never drawn) are counted
//! and the first [`MAX_LISTED`] named ([`Found::not_shown`]).
//!
//! `.find` reads the session ([`Session::result`]) and changes nothing: it is
//! no step of the exploration, so `.undo` does not see it. `--sql` at the end
//! prints its SQL and selects nothing.
//!
//! # Its SQL
//!
//! One statement for each vertex table of the drawing's property graph:
//!
//! ```sql
//! -- .find 33
//! SELECT CAST("id" AS VARCHAR) AS node, CASE … END AS rank FROM members
//! WHERE starts_with(lower(CAST("id" AS VARCHAR)), '33') OR strpos(lower("club"), '33') > 0
//! ORDER BY rank, "id"
//! -- .find WHERE club = 'Officer'
//! SELECT CAST("id" AS VARCHAR) AS node FROM members WHERE (club = 'Officer') ORDER BY "id"
//! ```
//!
//! A graph with no vertex table has its `<graph>.nodes` (the edges' ends,
//! `id`) in the table's place. Two cases run no SQL: a drawing that is of no
//! property graph, and `.find TEXT` on the whole of a graph with no vertex
//! table (`.graph NAME`), where every node is in the session already. Both
//! match the session's keys, and neither can take a `WHERE`.

use std::collections::HashSet;

use datafusion::arrow::array::{Array, AsArray};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::{DataType, Int64Type};
use datafusion::common::TableReference;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;
use ursa_viz::model::{GraphResult, Key};

use super::session::Session;
use super::{plan_err, thousands, RunSql};
use crate::graph::registry_of;
use crate::graph::schema::{ID, NODES};

/// The most nodes `.find` selects; it says how many matched.
pub const MAX_SELECTED: usize = 1_000;
/// The most matches outside the view that `.find` names; it counts the rest.
pub const MAX_LISTED: usize = 20;

const USAGE: &str = "usage: .find TEXT or .find WHERE condition";

/// What `.find` looks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// Keys that start with the text, then label columns that contain it.
    Text(String),
    /// A SQL condition on the vertex table's columns.
    Where(String),
}

/// A `.find` line, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Find {
    pub query: Query,
    /// `--sql`: print the SQL and select nothing.
    pub sql_only: bool,
}

impl Find {
    /// Read `line`. `Ok(None)` when it is not `.find`; an error when it is
    /// one with nothing to look for.
    pub fn parse(line: &str) -> Result<Option<Find>> {
        let text = line.trim().trim_end_matches(';').trim_end();
        let (name, rest) = match text.find(char::is_whitespace) {
            Some(at) => (&text[..at], text[at..].trim()),
            None => (text, ""),
        };
        if !name.eq_ignore_ascii_case(".find") {
            return Ok(None);
        }
        let (rest, sql_only) = match rest.strip_suffix("--sql") {
            Some(before) if before.is_empty() || before.ends_with(char::is_whitespace) => {
                (before.trim_end(), true)
            }
            _ => (rest, false),
        };
        let is_where = rest
            .get(..5)
            .is_some_and(|w| w.eq_ignore_ascii_case("where"))
            && rest[5..].chars().next().is_none_or(char::is_whitespace);
        let query = if is_where {
            let condition = rest[5..].trim();
            if condition.is_empty() {
                return plan_err(format!(
                    "WHERE takes a condition on the vertex table's columns: .find WHERE club = \
                     'Officer'; {USAGE}"
                ));
            }
            Query::Where(condition.to_string())
        } else {
            // 'a b' and "a b" are the text without the quotes.
            let unquoted = ['\'', '"'].iter().find_map(|&q| {
                let inner = rest.strip_prefix(q)?.strip_suffix(q)?;
                (rest.len() >= 2).then(|| inner.replace(&format!("{q}{q}"), &q.to_string()))
            });
            let text = unquoted.unwrap_or_else(|| rest.to_string());
            if text.is_empty() {
                return plan_err(USAGE);
            }
            Query::Text(text)
        };
        Ok(Some(Find { query, sql_only }))
    }
}

/// A match the view does not show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotShown {
    /// The node's key, as the shell prints it.
    pub key: String,
    /// Whether it is a hidden node of the session, which `.show` brings
    /// back; otherwise it is a node of the graph that was never drawn.
    pub hidden: bool,
}

/// What `.find` found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Found {
    /// What to print. No newline at the end.
    pub text: String,
    /// The matches the view shows, as its node indexes, the best first: at
    /// most [`MAX_SELECTED`]. The view selects them and fits them.
    pub selected: Vec<u32>,
    /// How many matches the view shows: more than `selected` past the cap.
    pub matches: usize,
    /// The first [`MAX_LISTED`] matches the view does not show.
    pub not_shown: Vec<NotShown>,
    /// How many matches the view does not show.
    pub not_shown_total: usize,
    /// The statements run (or, for `--sql`, that would be).
    pub sql: Vec<String>,
}

/// A table `.find` reads: a vertex table, or a graph's `nodes`.
struct Table {
    /// As SQL names it.
    name: String,
    /// The vertex table's element name: the label of its nodes.
    label: Option<String>,
    key: String,
    /// The label column: the first text column that is not the key.
    text: Option<String>,
}

impl Table {
    /// What the statement calls the key as text: not the key's own name,
    /// which `ORDER BY` must still mean the column by (10 after 9).
    fn out(&self) -> &'static str {
        if self.key.eq_ignore_ascii_case("node") {
            "node_key"
        } else {
            "node"
        }
    }
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn is_text(ty: &DataType) -> bool {
    match ty {
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => true,
        DataType::Dictionary(_, values) => is_text(values),
        _ => false,
    }
}

/// The tables of the drawing's graph: its vertex tables, or its `nodes`.
/// None when the drawing is of no graph the session has.
async fn tables(ctx: &SessionContext, r: &GraphResult) -> Result<Option<Vec<Table>>> {
    let Some(graph) = r.graph().and_then(|name| registry_of(ctx).get(name)) else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for v in &graph.vertex_tables {
        let schema = ctx
            .table(v.table.clone())
            .await?
            .schema()
            .as_arrow()
            .clone();
        let text = schema
            .fields()
            .iter()
            .find(|f| f.name() != &v.key && is_text(f.data_type()))
            .map(|f| f.name().clone());
        out.push(Table {
            name: v.table.to_quoted_string(),
            label: Some(v.name.clone()),
            key: v.key.clone(),
            text,
        });
    }
    if out.is_empty() && graph.default_pair().is_some() {
        out.push(Table {
            name: TableReference::partial(graph.name.as_str(), NODES).to_quoted_string(),
            label: None,
            key: ID.to_string(),
            text: None,
        });
    }
    Ok(Some(out))
}

/// The matches as they are gathered: each node of the session once.
struct Matches<'a> {
    r: &'a GraphResult,
    seen: Vec<bool>,
    /// Keys outside the session that matched already.
    outside: HashSet<String>,
    found: Found,
}

impl Matches<'_> {
    /// Session node `i` matched.
    fn node(&mut self, i: u32) {
        if std::mem::replace(&mut self.seen[i as usize], true) {
            return;
        }
        if self.r.node_is_removed(i as usize) {
            self.miss(self.r.node_keys().get(i as usize).to_string(), true);
        } else {
            self.found.matches += 1;
            if self.found.selected.len() < MAX_SELECTED {
                self.found.selected.push(i);
            }
        }
    }

    fn miss(&mut self, key: String, hidden: bool) {
        self.found.not_shown_total += 1;
        if self.found.not_shown.len() < MAX_LISTED {
            self.found.not_shown.push(NotShown { key, hidden });
        }
    }

    /// A row of `table` matched: the node with this key, in the session or
    /// not.
    fn row(&mut self, table: &Table, key: &str) {
        let labels = self.r.node_labels();
        let of = |l: usize| {
            let key = Key::parse(labels[l].key, key).ok()?;
            self.r.node(l as u16, &key)
        };
        // The table's own label first; a drawing may name its nodes' label
        // otherwise (or not at all), so then any.
        let own = table
            .label
            .as_ref()
            .and_then(|name| labels.iter().position(|l| &l.name == name));
        let node = own.and_then(of).or_else(|| (0..labels.len()).find_map(of));
        match node {
            Some(i) => self.node(i),
            None => {
                if self.outside.insert(key.to_string()) {
                    self.miss(key.to_string(), false);
                }
            }
        }
    }
}

/// `n` nodes, in words.
fn nodes(n: usize) -> String {
    format!("{} node{}", thousands(n), if n == 1 { "" } else { "s" })
}

impl Found {
    /// What `.find` says of these matches.
    fn say(&mut self, what: &str) {
        let mut out = match (self.matches, self.selected.len()) {
            (0, _) if self.not_shown_total == 0 => format!("No node matches {what}"),
            (0, _) => format!("No node in the view matches {what}"),
            (all, some) if some < all => format!(
                "Selected the first {} of {} that match {what}",
                thousands(some),
                nodes(all)
            ),
            (all, _) => format!("Selected {}", nodes(all)),
        };
        if self.not_shown_total > 0 {
            let named: Vec<String> = self
                .not_shown
                .iter()
                .map(|n| match n.hidden {
                    true => format!("{} (hidden)", n.key),
                    false => n.key.clone(),
                })
                .collect();
            let more = self.not_shown_total - named.len();
            out.push_str(&format!("\nNot shown: {}", named.join(", ")));
            if more > 0 {
                out.push_str(&format!(" and {} more", thousands(more)));
            }
            if self.not_shown.iter().any(|n| n.hidden) {
                out.push_str("\n.show ID shows a hidden node again");
            }
        }
        self.text = out;
    }
}

/// Run a `.find` line ([`Find::parse`]) on `session`: `sql` runs its
/// statements on `ctx`. Nothing changes: the caller's view selects
/// [`Found::selected`].
///
/// An error is a line that is not `.find`, a `WHERE` on a drawing with no
/// table to ask, or a condition that no vertex table can answer.
pub async fn find(
    session: &Session,
    ctx: &SessionContext,
    sql: &dyn RunSql,
    line: &str,
) -> Result<Found> {
    let Some(Find { query, sql_only }) = Find::parse(line)? else {
        return plan_err(format!("'{}' is not .find; {USAGE}", line.trim()));
    };
    let r = session.result();
    let tables = tables(ctx, r).await?;
    let mut matches = Matches {
        r,
        seen: vec![false; r.n_nodes()],
        outside: HashSet::new(),
        found: Found::default(),
    };
    let what;
    match &query {
        Query::Text(text) => {
            what = format!("'{text}'");
            let needle = text.to_lowercase();
            let key_of = |t: &Table| format!("lower(CAST({} AS VARCHAR))", ident(&t.key));
            // The whole of a graph with no vertex table: the session has
            // every node, and its keys are all there is to match.
            let whole = r.provenance().rule.as_deref() == Some("graph");
            let asked: Vec<&Table> = tables
                .iter()
                .flatten()
                .filter(|t| !(whole && t.label.is_none()))
                .collect();
            let statements: Vec<String> = asked
                .iter()
                .map(|t| {
                    let (key, lit) = (key_of(t), string(&needle));
                    let prefix = format!("starts_with({key}, {lit})");
                    let (label, rank) = match &t.text {
                        Some(c) => (
                            format!(" OR strpos(lower({}), {lit}) > 0", ident(c)),
                            format!(
                                "CASE WHEN {key} = {lit} THEN 0 WHEN {prefix} THEN 1 ELSE 2 END"
                            ),
                        ),
                        None => (
                            String::new(),
                            format!("CASE WHEN {key} = {lit} THEN 0 ELSE 1 END"),
                        ),
                    };
                    format!(
                        "SELECT CAST({k} AS VARCHAR) AS {}, {rank} AS rank FROM {} \
                         WHERE {prefix}{label} ORDER BY rank, {k}",
                        t.out(),
                        t.name,
                        k = ident(&t.key),
                    )
                })
                .collect();
            if sql_only {
                return Ok(said_sql(
                    statements,
                    ".find TEXT here matches the keys drawn",
                ));
            }
            // By rank: the key itself, keys it starts, then label columns.
            let mut ranked: [Vec<(usize, String)>; 3] = Default::default();
            for (at, statement) in statements.iter().enumerate() {
                let (_, batches) = sql.run(ctx, statement).await?;
                for b in &batches {
                    let ids = cast(b.column(0), &DataType::Utf8)?;
                    let ids = ids.as_string::<i32>();
                    let rank = cast(b.column(1), &DataType::Int64)?;
                    let rank = rank.as_primitive::<Int64Type>();
                    for i in (0..b.num_rows()).filter(|&i| ids.is_valid(i)) {
                        let rank = rank.value(i).clamp(0, 2) as usize;
                        ranked[rank].push((at, ids.value(i).to_string()));
                    }
                }
            }
            // The session's own keys, where no table was asked (or a node
            // is of none of them).
            let mut exact = Vec::new();
            let mut starts = Vec::new();
            for (i, key) in r.node_keys().iter().enumerate() {
                // Keys are mostly numbers: lower-case only what has letters.
                let hit = if key.is_ascii() {
                    key.len() >= needle.len()
                        && key.as_bytes()[..needle.len()].eq_ignore_ascii_case(needle.as_bytes())
                } else {
                    key.to_lowercase().starts_with(&needle)
                };
                if hit {
                    let whole_key = key.len() == needle.len() || key.to_lowercase() == needle;
                    if whole_key { &mut exact } else { &mut starts }.push(i as u32);
                }
            }
            for (rank, own) in [(0, &exact), (1, &starts), (2, &Vec::new())] {
                for (at, key) in &ranked[rank] {
                    matches.row(asked[*at], key);
                }
                for &i in own {
                    matches.node(i);
                }
            }
            matches.found.sql = statements;
        }
        Query::Where(condition) => {
            what = condition.clone();
            let Some(tables) = &tables else {
                return plan_err(
                    "this drawing is not of a property graph, so there is no vertex table for a \
                     condition; .find TEXT matches the keys drawn, or draw a graph's nodes \
                     first: .graph NAME",
                );
            };
            if tables.is_empty() {
                return plan_err(
                    "the graph that is drawn has several edge tables and no vertex table, so \
                     there are no columns for a condition; .find TEXT matches the keys drawn",
                );
            }
            let statements: Vec<String> = tables
                .iter()
                .map(|t| {
                    format!(
                        "SELECT CAST({k} AS VARCHAR) AS {} FROM {} WHERE ({condition}) ORDER BY {k}",
                        t.out(),
                        t.name,
                        k = ident(&t.key),
                    )
                })
                .collect();
            if sql_only {
                return Ok(said_sql(statements, ""));
            }
            // A condition is on one table's columns: with several vertex
            // tables, those it cannot be asked of are left out.
            let mut failed: Option<DataFusionError> = None;
            let mut ran = Vec::new();
            for (table, statement) in tables.iter().zip(statements) {
                match sql.run(ctx, &statement).await {
                    Ok((_, batches)) => {
                        for b in &batches {
                            let ids = cast(b.column(0), &DataType::Utf8)?;
                            let ids = ids.as_string::<i32>();
                            for i in (0..b.num_rows()).filter(|&i| ids.is_valid(i)) {
                                matches.row(table, ids.value(i));
                            }
                        }
                        ran.push(statement);
                    }
                    Err(e) => failed = failed.or(Some(e)),
                }
            }
            match failed {
                Some(e) if ran.is_empty() => return Err(e),
                _ => matches.found.sql = ran,
            }
        }
    }
    let mut found = matches.found;
    found.say(&what);
    Ok(found)
}

/// `--sql`'s answer: the statements, or why there are none.
fn said_sql(statements: Vec<String>, none: &str) -> Found {
    let text = if statements.is_empty() {
        format!("-- .find runs no SQL: {none}")
    } else {
        statements
            .iter()
            .map(|s| format!("{s};"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Found {
        text,
        sql: statements,
        ..Found::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(line: &str) -> Find {
        Find::parse(line).unwrap().unwrap()
    }

    #[test]
    fn find_lines_are_read() {
        assert_eq!(
            parsed(".find 33"),
            Find {
                query: Query::Text("33".into()),
                sql_only: false
            }
        );
        assert_eq!(
            parsed("  .FIND  Mr Hi ;").query,
            Query::Text("Mr Hi".into())
        );
        assert_eq!(
            parsed(".find 'O''Neil'").query,
            Query::Text("O'Neil".into())
        );
        assert_eq!(
            parsed(".find WHERE club = 'Officer';").query,
            Query::Where("club = 'Officer'".into())
        );
        assert_eq!(
            parsed(".find where id > 3 --sql"),
            Find {
                query: Query::Where("id > 3".into()),
                sql_only: true
            }
        );
        // A text that starts with "where" is a text.
        assert_eq!(
            parsed(".find wherever").query,
            Query::Text("wherever".into())
        );
        assert_eq!(parsed(".find 3 --sql").query, Query::Text("3".into()));
    }

    #[test]
    fn other_lines_are_not_find() {
        for line in [".finder", ".expand 3", "SELECT 1", ".graph karate", ""] {
            assert_eq!(Find::parse(line).unwrap(), None, "{line}");
        }
        for line in [".find", ".find ;", ".find WHERE", ".find --sql"] {
            let e = Find::parse(line).unwrap_err().to_string();
            assert!(e.contains("usage: .find TEXT"), "{line}: {e}");
        }
    }
}
