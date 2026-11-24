//! The explore session (design §4, #440): the engine's side of exploring the
//! graph that is drawn. A [`Session`] knows what is shown and turns each
//! command into SQL and a [`GraphDelta`] for the view to apply.
//!
//! A session holds:
//!
//! - **the graph as it stands**, a [`GraphResult`]: the base that `.graph`
//!   drew with every step so far applied. It carries the base property
//!   graph's name, the `(label, key)` → index map ([`GraphResult::node`]) and
//!   the shown set (a hidden node keeps its slot, with a flag);
//! - **the history**, a list of [`Step`]s (the command, its SQL, its delta,
//!   and through [`Step::inverse`] the delta that undoes it), and how many
//!   of them are applied.
//!
//! The commands ([`Command`]), each one line of text:
//!
//! | command | what it does | its SQL |
//! |---|---|---|
//! | `.expand ID [in\|out\|both] [LIMIT n]` | adds the node's neighbours that are not shown, and the graph's edges among every node | `hop`, then `induced_edges` |
//! | `.expand group:N` | in a drawing grouped by `GROUP BY`: opens group N, adding its members and the graph's edges among the members of the open groups | `induced_edges` |
//! | `.collapse group:N` | closes group N: its members are hidden | none |
//! | `.hide ID…` | hides nodes | none |
//! | `.show ID…` or `.show all` | shows hidden nodes again | none |
//! | `.undo`, `.redo` | steps the history | none |
//! | `.graph history` | lists the steps | none |
//!
//! `--sql` on a command prints its SQL and changes nothing: no step, no
//! delta. For `.expand` it prints both statements as a step would run them,
//! and to do so it runs the first, a query, whose rows are the nodes the
//! second names.
//!
//! # Expanding
//!
//! `.expand 33` runs two statements ([`Session::run`] gives them back in the
//! step):
//!
//! 1. The neighbours to add: `hop` one step from the node (`both`
//!    directions unless the command says `in` or `out`), without the nodes
//!    that are shown, ordered by their degree in the graph (both directions,
//!    highest first) and then by key, and cut at the limit
//!    ([`EXPAND_LIMIT`], or `LIMIT n` up to [`MAX_EXPAND`]). The order is
//!    total, so the same command always adds the same nodes, in the same
//!    order. Expanding again adds the next ones.
//! 2. The edges: `induced_edges` over every node of the session and the new
//!    ones. An edge the session already has is left out: by its label and
//!    its row in the edge table where both are known, else by the two nodes
//!    it joins and its place among their parallel edges.
//!
//! The delta restores the neighbours that were hidden, appends the new nodes
//! (in the order of statement 1) and then the new edges (in the order of
//! statement 2, which is the edge table's). Hidden nodes are part of
//! statement 2, so their edges to newer nodes are in the session when
//! `.show` brings them back, with no query.
//!
//! # Groups
//!
//! A drawing made with `GROUP BY` ([`super::groups`], #499) has a node for
//! each group and none of the groups' members; a session on it
//! ([`Session::grouped`]) knows which nodes each group has. `.expand
//! group:3` opens the group keyed 3, in one statement: `induced_edges` over
//! its members and the members of the groups already open. The delta adds
//! the members (in the drawing's node order, each with its group's value in
//! the column the result is grouped by and its place in the group's disc in `group x` and
//! `group y`: [`super::groups::disc_layout`] of the group's members and the
//! edges among them) and then the edges the session does not have. The
//! group's own node stays, with the edges to the other groups: a view
//! draws the members in its disc. `.collapse group:3` hides the members,
//! and opening the group again shows them, with no query. `.undo` of the
//! step that opened a group takes its members and their edges away again.
//! In such a session `.expand` opens groups only.
//!
//! # Undo
//!
//! Every op of a delta carries what its inverse needs, so `.undo` applies
//! [`GraphDelta::invert`] of the last step and runs no SQL, and `.redo`
//! applies the step's delta again. A new step drops the steps that were
//! undone.

use std::collections::{HashMap, HashSet};

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::array::{Array, ArrayRef, AsArray, Int64Array, StringArray};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::datatypes::{DataType, Int64Type, UInt64Type};
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;
use ursa_viz::model::{
    BatchValues, EdgeBatch, GraphDelta, GraphResult, Key, KeyType, NodeBatch, Op, Provenance,
    Strings, Values, NO_ROW,
};

use super::groups::{self, Grouping};
use super::{plan_err, thousands, RunSql};
use crate::graph::provenance as meta;

/// How many neighbours `.expand` adds unless it says `LIMIT n`.
pub const EXPAND_LIMIT: usize = 100;
/// The most `LIMIT n` may ask for.
pub const MAX_EXPAND: usize = 5_000;

/// Which of a node's edges `.expand` follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
    Both,
}

impl Direction {
    /// As `hop`'s `direction` takes it.
    pub fn name(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
            Direction::Both => "both",
        }
    }
}

/// An explore command, read from its line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Expand {
        id: String,
        direction: Direction,
        limit: usize,
    },
    /// `.collapse group:N`: the group to close.
    Collapse(String),
    Hide(Vec<String>),
    /// The ids to show; none for `.show all`.
    Show(Vec<String>),
    Undo,
    Redo,
    History,
}

/// A command and whether it asked for its SQL alone (`--sql`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub command: Command,
    pub sql_only: bool,
    /// The line as the history shows it: trimmed, with no `;`.
    pub text: String,
}

/// The words of a command: split at spaces, a quoted word (`'a b'`, `"a b"`)
/// kept whole without its quotes, a doubled quote inside one being one.
fn words(text: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut word = String::new();
        if c == '\'' || c == '"' {
            chars.next();
            loop {
                match chars.next() {
                    None => return plan_err(format!("a quote ({c}) is not closed in: {text}")),
                    Some(x) if x == c => {
                        if chars.peek() == Some(&c) {
                            chars.next();
                            word.push(c);
                        } else {
                            break;
                        }
                    }
                    Some(x) => word.push(x),
                }
            }
        } else {
            while let Some(&x) = chars.peek() {
                if x.is_whitespace() {
                    break;
                }
                word.push(x);
                chars.next();
            }
        }
        out.push(word);
    }
    Ok(out)
}

impl Command {
    /// The names of the explore commands, as typed: `.graph` is one only as
    /// `.graph history`.
    pub const NAMES: [&'static str; 6] =
        [".expand", ".collapse", ".hide", ".show", ".undo", ".redo"];

    /// Read `line`. `Ok(None)` when it is not an explore command (`.graph
    /// karate`, `.tables`); an error when it is one that cannot be read.
    pub fn parse(line: &str) -> Result<Option<Parsed>> {
        let text = line.trim().trim_end_matches(';').trim_end();
        let (name, rest) = match text.find(char::is_whitespace) {
            Some(at) => (&text[..at], text[at..].trim()),
            None => (text, ""),
        };
        let name = name.to_ascii_lowercase();
        if name == ".graph" {
            if !rest
                .split_whitespace()
                .next()
                .is_some_and(|w| w.eq_ignore_ascii_case("history"))
            {
                return Ok(None);
            }
        } else if !Self::NAMES.contains(&name.as_str()) {
            return Ok(None);
        }
        let mut args = words(rest)?;
        let before = args.len();
        args.retain(|a| a != "--sql");
        let sql_only = args.len() != before;
        let command = match name.as_str() {
            ".expand" => {
                const USAGE: &str = "usage: .expand ID [in|out|both] [LIMIT n]";
                let mut args = args.into_iter();
                let Some(id) = args.next() else {
                    return plan_err(USAGE);
                };
                let mut direction = None;
                let mut limit = None;
                while let Some(word) = args.next() {
                    match word.to_ascii_lowercase().as_str() {
                        "in" if direction.is_none() => direction = Some(Direction::In),
                        "out" if direction.is_none() => direction = Some(Direction::Out),
                        "both" if direction.is_none() => direction = Some(Direction::Both),
                        "limit" if limit.is_none() => {
                            let n = args.next().and_then(|n| n.parse::<usize>().ok());
                            match n {
                                Some(n) if (1..=MAX_EXPAND).contains(&n) => limit = Some(n),
                                _ => {
                                    return plan_err(format!(
                                        "LIMIT takes a number of neighbours from 1 to {}; {USAGE}",
                                        thousands(MAX_EXPAND)
                                    ))
                                }
                            }
                        }
                        _ => {
                            return plan_err(format!(
                                "'{word}' is not part of .expand (one node at a time); {USAGE}"
                            ))
                        }
                    }
                }
                Command::Expand {
                    id,
                    direction: direction.unwrap_or(Direction::Both),
                    limit: limit.unwrap_or(EXPAND_LIMIT),
                }
            }
            ".collapse" => {
                if args.len() != 1 {
                    return plan_err("usage: .collapse group:N");
                }
                Command::Collapse(args.remove(0))
            }
            ".hide" => {
                if args.is_empty() {
                    return plan_err("usage: .hide ID…");
                }
                Command::Hide(args)
            }
            ".show" => {
                if args.is_empty() {
                    return plan_err("usage: .show ID… or .show all");
                }
                if args.len() == 1 && args[0].eq_ignore_ascii_case("all") {
                    Command::Show(Vec::new())
                } else {
                    Command::Show(args)
                }
            }
            ".undo" | ".redo" => {
                if !args.is_empty() {
                    return plan_err(format!("usage: {name}"));
                }
                if name == ".undo" {
                    Command::Undo
                } else {
                    Command::Redo
                }
            }
            _ => {
                if args.len() != 1 {
                    return plan_err("usage: .graph history");
                }
                Command::History
            }
        };
        Ok(Some(Parsed {
            command,
            sql_only,
            text: text.to_string(),
        }))
    }
}

/// One step of an exploration.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// The command, as typed.
    pub command: String,
    /// The statements it ran, in order; none for a step that reads nothing
    /// (`.hide`, `.show`).
    pub sql: Vec<String>,
    /// What it changed.
    pub delta: GraphDelta,
    /// How many more nodes and edges were shown after it (fewer, when
    /// negative).
    pub nodes: i64,
    pub edges: i64,
}

impl Step {
    /// The delta that undoes the step.
    pub fn inverse(&self) -> GraphDelta {
        self.delta.invert()
    }

    /// `+16 nodes, +41 edges`.
    pub fn summary(&self) -> String {
        changed(self.nodes, self.edges)
    }
}

/// `+16 nodes, +41 edges`; `-1 node, -3 edges`.
fn changed(nodes: i64, edges: i64) -> String {
    let part = |n: i64, one: &str| {
        let sign = if n < 0 { '-' } else { '+' };
        let s = if n.unsigned_abs() == 1 { "" } else { "s" };
        format!("{sign}{} {one}{s}", thousands(n.unsigned_abs() as usize))
    };
    format!("{}, {}", part(nodes, "node"), part(edges, "edge"))
}

/// What a command did.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// What to print: one line for a command that changes the graph (what
    /// changed, or why nothing did), the SQL for `--sql`, the steps for
    /// `.graph history`. No newline at the end.
    pub text: String,
    /// The change for the view to apply, if there is one.
    pub delta: Option<GraphDelta>,
}

impl Outcome {
    fn said(text: impl Into<String>) -> Outcome {
        Outcome {
            text: text.into(),
            delta: None,
        }
    }
}

/// Two nodes, whichever way an edge joins them. A drawing's edge with no row
/// may be drawn against the way the graph stores it (a path's steps are).
fn pair(a: u32, b: u32) -> (u32, u32) {
    (a.min(b), a.max(b))
}

/// The session's edges, by what makes one the same edge: its label and row
/// where the engine knew the row, else the two nodes it joins (and how many
/// such edges join them).
#[derive(Debug, Default)]
struct EdgeIndex {
    rows: HashSet<(u16, u64)>,
    pairs: HashMap<(u32, u32), u32>,
}

impl EdgeIndex {
    fn of(result: &GraphResult) -> EdgeIndex {
        let mut index = EdgeIndex::default();
        let (src, dst, label) = (result.src(), result.dst(), result.edge_label());
        for i in 0..result.n_edges() {
            index.add(src[i], dst[i], label[i], result.edge_row(i));
        }
        index
    }

    fn add(&mut self, src: u32, dst: u32, label: u16, row: Option<u64>) {
        match row {
            Some(row) => {
                self.rows.insert((label, row));
            }
            None => *self.pairs.entry(pair(src, dst)).or_insert(0) += 1,
        }
    }

    fn remove(&mut self, src: u32, dst: u32, label: u16, row: Option<u64>) {
        match row {
            Some(row) => {
                self.rows.remove(&(label, row));
            }
            None => {
                let pair = pair(src, dst);
                if let Some(count) = self.pairs.get_mut(&pair) {
                    *count -= 1;
                    if *count == 0 {
                        self.pairs.remove(&pair);
                    }
                }
            }
        }
    }

    /// Keep up with a delta the session applied.
    fn track(&mut self, delta: &GraphDelta) {
        let each = |batch: &EdgeBatch, f: &mut dyn FnMut(u32, u32, u16, Option<u64>)| {
            for i in 0..batch.len() {
                let row = batch
                    .rows
                    .as_ref()
                    .map(|rows| rows[i])
                    .filter(|&r| r != NO_ROW);
                f(batch.src[i], batch.dst[i], batch.labels[i], row);
            }
        };
        for op in &delta.ops {
            match op {
                Op::AddEdges(batch) => each(batch, &mut |s, d, l, r| self.add(s, d, l, r)),
                Op::PopEdges(batch) => each(batch, &mut |s, d, l, r| self.remove(s, d, l, r)),
                _ => {}
            }
        }
    }
}

/// A column of node ids, as integers where it is one of integers.
enum Ids {
    Int(Int64Array),
    Text(StringArray),
}

impl Ids {
    fn of(array: &ArrayRef) -> Result<Ids> {
        let integers = matches!(
            array.data_type(),
            DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
        );
        if integers {
            let cast = cast(array, &DataType::Int64)?;
            Ok(Ids::Int(cast.as_primitive::<Int64Type>().clone()))
        } else {
            let cast = cast(array, &DataType::Utf8)?;
            Ok(Ids::Text(cast.as_string::<i32>().clone()))
        }
    }

    /// Row `i`'s id as a key of type `ty`, if it is one (and not NULL).
    fn key(&self, i: usize, ty: KeyType) -> Option<Key> {
        match (self, ty) {
            (Ids::Int(a), _) if a.is_null(i) => None,
            (Ids::Text(a), _) if a.is_null(i) => None,
            (Ids::Int(a), KeyType::Int) => Some(Key::Int(a.value(i))),
            (Ids::Int(a), KeyType::Text) => Some(Key::Text(a.value(i).to_string())),
            (Ids::Text(a), ty) => Key::parse(ty, a.value(i)).ok(),
        }
    }
}

/// `key` as a SQL literal.
fn literal(out: &mut String, key: &str, ty: KeyType) {
    match ty {
        KeyType::Int => out.push_str(key),
        KeyType::Text => {
            out.push('\'');
            out.push_str(&key.replace('\'', "''"));
            out.push('\'');
        }
    }
}

fn model(e: ursa_viz::model::ModelError) -> DataFusionError {
    DataFusionError::Plan(format!("the graph cannot be changed: {e}"))
}

/// The explore session of one drawing. See the [module](self).
#[derive(Debug)]
pub struct Session {
    result: GraphResult,
    steps: Vec<Step>,
    /// How many of `steps` are applied; the rest were undone.
    done: usize,
    /// Made by the first `.expand`, and kept up after it.
    edges: Option<EdgeIndex>,
    /// Each node's degree in the property graph (both directions, every
    /// edge counted), by slot, for the slots asked about so far
    /// ([`Session::not_shown`]); [`NO_DEGREE`] for a node the graph has no
    /// row for.
    degrees: Vec<u32>,
    /// Of a drawing made with `GROUP BY`: which nodes each group has.
    groups: Option<Grouping>,
}

/// The degree of a node the graph's `degree` has no row for.
const NO_DEGREE: u32 = u32::MAX;
/// Up to this many nodes' degrees are asked for by key; more, and the whole
/// graph's are read.
const DEGREES_BY_KEY: usize = 1_000;

impl Session {
    /// A session on what `.graph` drew.
    pub fn new(base: GraphResult) -> Session {
        Session {
            result: base,
            steps: Vec::new(),
            done: 0,
            edges: None,
            degrees: Vec::new(),
            groups: None,
        }
    }

    /// A session on what `.graph … GROUP BY` drew: `base` is the grouped
    /// result and `groups` its groups' members (`GraphView::groups`), which
    /// `.expand group:N` opens.
    pub fn grouped(base: GraphResult, groups: Grouping) -> Session {
        Session {
            groups: Some(groups),
            ..Session::new(base)
        }
    }

    /// The groups of a session on a grouped drawing.
    pub fn grouping(&self) -> Option<&Grouping> {
        self.groups.as_ref()
    }

    /// The graph as it stands: the base with every applied step.
    pub fn result(&self) -> &GraphResult {
        &self.result
    }

    /// Every step, the undone ones included: `steps()[..applied()]` are in
    /// the graph.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// How many steps are applied.
    pub fn applied(&self) -> usize {
        self.done
    }

    /// How many nodes and edges are shown.
    pub fn shown(&self) -> (usize, usize) {
        let r = &self.result;
        let nodes = r.node_flags().iter().filter(|&&f| f == 0).count();
        let edges = (0..r.n_edges()).filter(|&i| r.edge_is_shown(i)).count();
        (nodes, edges)
    }

    /// The node `id` names: of the first label that has the key.
    /// For each node slot, how many of the node's edges in the property
    /// graph lead to something that is not shown: its degree in the graph
    /// (both directions, every edge counted, as `degree` counts) less its
    /// shown edges here. It is what a view's `+N` badge says, and 0 for a
    /// hidden node and for one the graph does not have. Empty when the
    /// drawing is not of a property graph.
    ///
    /// The graph's degrees are read once for each node (one `degree` query
    /// for the nodes not asked about before: by key for up to
    /// [`DEGREES_BY_KEY`] of them, else the whole graph's), and the shown
    /// edges are counted each time.
    pub async fn not_shown(&mut self, ctx: &SessionContext, sql: &dyn RunSql) -> Result<Vec<u32>> {
        // A group is no node of the graph: it has no degree there.
        if self.groups.is_some() {
            return Ok(Vec::new());
        }
        let Some(graph) = self.result.graph() else {
            return Ok(Vec::new());
        };
        let n = self.result.n_nodes();
        self.degrees.truncate(n);
        let known = self.degrees.len();
        if known < n {
            let graph = format!("\"{}\"", graph.replace('"', "\"\""));
            let mut query = format!("SELECT id, degree FROM degree({graph}, direction => 'both')");
            let one_type = (known..n).all(|i| self.key_type(i) == self.key_type(known));
            if n - known <= DEGREES_BY_KEY && one_type {
                query.push_str(" WHERE id IN (");
                for i in known..n {
                    if i > known {
                        query.push_str(", ");
                    }
                    literal(&mut query, self.result.node_keys().get(i), self.key_type(i));
                }
                query.push(')');
            }
            let (_, batches) = sql.run(ctx, &query).await?;
            let r = &self.result;
            let mut degrees = vec![NO_DEGREE; n - known];
            for batch in &batches {
                let ids = Ids::of(batch.column(0))?;
                let degree = cast(batch.column(1), &DataType::UInt64)?;
                let degree = degree.as_primitive::<UInt64Type>();
                for i in 0..batch.num_rows() {
                    let found = r.node_labels().iter().zip(0u16..).find_map(|(l, at)| {
                        let key = ids.key(i, l.key)?;
                        r.node(at, &key)
                    });
                    if let Some(at) = found.filter(|&at| at as usize >= known) {
                        degrees[at as usize - known] =
                            degree.value(i).min(u64::from(u32::MAX - 1)) as u32;
                    }
                }
            }
            self.degrees.extend(degrees);
        }
        let r = &self.result;
        let mut shown = vec![0u32; n];
        let (src, dst) = (r.src(), r.dst());
        for e in 0..r.n_edges() {
            if r.edge_is_shown(e) {
                shown[src[e] as usize] += 1;
                shown[dst[e] as usize] += 1;
            }
        }
        Ok((0..n)
            .map(|i| match self.degrees[i] {
                NO_DEGREE => 0,
                _ if r.node_is_removed(i) => 0,
                degree => degree.saturating_sub(shown[i]),
            })
            .collect())
    }

    fn find(&self, id: &str) -> Option<u32> {
        let r = &self.result;
        // In a grouped drawing `group:3` is a group, and any other id a
        // member of an open one.
        let skip = if self.groups.is_some() {
            if let Some(group) = group_id(id) {
                return group.and_then(|g| r.node(0, &Key::Int(g)));
            }
            1
        } else {
            0
        };
        r.node_labels()
            .iter()
            .zip(0u16..)
            .skip(skip)
            .find_map(|(label, l)| {
                let key = Key::parse(label.key, id).ok()?;
                r.node(l, &key)
            })
    }

    fn resolve(&self, id: &str) -> Result<u32> {
        match self.find(id) {
            Some(node) => Ok(node),
            None => plan_err(format!(
                "no node '{id}' in the graph that is drawn; a node is named by its key, as the \
                 graph view shows it"
            )),
        }
    }

    /// The key type of node `i`'s label.
    fn key_type(&self, i: usize) -> KeyType {
        self.result.node_labels()[self.result.node_label()[i] as usize].key
    }

    /// The keys of the session's nodes as a SQL list's items: the shown
    /// ones, or all of them.
    fn keys(&self, hidden_too: bool) -> String {
        let r = &self.result;
        let mut out = String::with_capacity(r.n_nodes() * 8);
        for i in 0..r.n_nodes() {
            if !hidden_too && r.node_is_removed(i) {
                continue;
            }
            if !out.is_empty() {
                out.push_str(", ");
            }
            literal(&mut out, r.node_keys().get(i), self.key_type(i));
        }
        out
    }

    /// Apply `delta` as a new step, dropping the steps that were undone.
    fn step(&mut self, command: &str, sql: Vec<String>, delta: GraphDelta) -> Result<Outcome> {
        let before = self.shown();
        self.result.apply(&delta).map_err(model)?;
        self.degrees.truncate(self.result.n_nodes());
        if let Some(edges) = &mut self.edges {
            edges.track(&delta);
        }
        let after = self.shown();
        let step = Step {
            command: command.to_string(),
            sql,
            delta: delta.clone(),
            nodes: after.0 as i64 - before.0 as i64,
            edges: after.1 as i64 - before.1 as i64,
        };
        let text = step.summary();
        self.steps.truncate(self.done);
        self.steps.push(step);
        self.done += 1;
        Ok(Outcome {
            text,
            delta: Some(delta),
        })
    }

    /// Run one command: `.expand`, `.hide`, `.show`, `.undo`, `.redo` or
    /// `.graph history`, as [`Command::parse`] reads it. `sql` runs the
    /// statements of `.expand` on `ctx`.
    ///
    /// An error (a line that is not a command, an id that is not a node, SQL
    /// that failed) changes nothing.
    pub async fn run(
        &mut self,
        ctx: &SessionContext,
        sql: &dyn RunSql,
        line: &str,
    ) -> Result<Outcome> {
        let Some(parsed) = Command::parse(line)? else {
            return plan_err(format!(
                "'{}' is not an explore command; they are .expand, .collapse, .hide, .show, \
                 .undo, .redo and .graph history",
                line.trim()
            ));
        };
        let Parsed {
            command,
            sql_only,
            text,
        } = parsed;
        match command {
            Command::Expand {
                id,
                direction,
                limit,
            } => {
                if self.groups.is_some() {
                    return self.open(ctx, sql, &text, &id, sql_only).await;
                }
                self.expand(ctx, sql, &text, &id, direction, limit, sql_only)
                    .await
            }
            _ if sql_only => {
                let name = text.split_whitespace().next().unwrap_or_default();
                Ok(Outcome::said(format!(
                    "-- {name} runs no SQL: it changes what is shown of the graph"
                )))
            }
            Command::Collapse(id) => self.collapse(&text, &id),
            Command::Hide(ids) => self.set_hidden(&text, &ids, true),
            Command::Show(ids) => self.set_hidden(&text, &ids, false),
            Command::Undo => self.undo(),
            Command::Redo => self.redo(),
            Command::History => Ok(Outcome::said(self.history())),
        }
    }

    /// `.hide` (`hide`) or `.show`; no ids is every hidden node.
    fn set_hidden(&mut self, command: &str, ids: &[String], hide: bool) -> Result<Outcome> {
        let r = &self.result;
        let mut nodes: Vec<u32> = if ids.is_empty() {
            (0..r.n_nodes() as u32).collect()
        } else {
            let mut nodes = Vec::with_capacity(ids.len());
            for id in ids {
                nodes.push(self.resolve(id)?);
            }
            nodes
        };
        nodes.sort_unstable();
        nodes.dedup();
        nodes.retain(|&i| r.node_is_removed(i as usize) != hide);
        if nodes.is_empty() {
            let (state, named) = if hide {
                ("hidden", ids.join(", "))
            } else if ids.is_empty() {
                return Ok(Outcome::said("Nothing changed: no node is hidden"));
            } else {
                ("shown", ids.join(", "))
            };
            let verb = if ids.len() == 1 { "is" } else { "are" };
            return Ok(Outcome::said(format!(
                "Nothing changed: {named} {verb} already {state}"
            )));
        }
        let op = if hide {
            Op::RemoveNodes(nodes)
        } else {
            Op::RestoreNodes(nodes)
        };
        self.step(command, Vec::new(), GraphDelta::new(vec![op]))
    }

    fn undo(&mut self) -> Result<Outcome> {
        if self.done == 0 {
            return Ok(Outcome::said("Nothing to undo"));
        }
        let step = &self.steps[self.done - 1];
        let delta = step.inverse();
        let text = format!(
            "Undid {}: {}",
            step.command,
            changed(-step.nodes, -step.edges)
        );
        self.result.apply(&delta).map_err(model)?;
        self.degrees.truncate(self.result.n_nodes());
        if let Some(edges) = &mut self.edges {
            edges.track(&delta);
        }
        self.done -= 1;
        Ok(Outcome {
            text,
            delta: Some(delta),
        })
    }

    fn redo(&mut self) -> Result<Outcome> {
        let Some(step) = self.steps.get(self.done) else {
            return Ok(Outcome::said("Nothing to redo"));
        };
        let delta = step.delta.clone();
        let text = format!("Redid {}: {}", step.command, step.summary());
        self.result.apply(&delta).map_err(model)?;
        self.degrees.truncate(self.result.n_nodes());
        if let Some(edges) = &mut self.edges {
            edges.track(&delta);
        }
        self.done += 1;
        Ok(Outcome {
            text,
            delta: Some(delta),
        })
    }

    /// `.graph history`: what was drawn, then a line a step.
    fn history(&self) -> String {
        let r = &self.result;
        let (nodes, edges) = self.shown();
        let count = |n: usize, one: &str| {
            let s = if n == 1 { "" } else { "s" };
            format!("{} {one}{s}", thousands(n))
        };
        let mut out = match r.provenance().sql.as_deref() {
            Some(sql) => format!("Explored from: {}", sql.replace('\n', " ")),
            None => match r.graph() {
                Some(graph) => format!("Explored from: the graph {graph}"),
                None => "Explored from: a result".to_string(),
            },
        };
        if self.steps.is_empty() {
            out.push_str("\nNo steps yet: .expand ID adds a node's neighbours");
        }
        let width = self.steps.len().to_string().len();
        let longest = self
            .steps
            .iter()
            .map(|s| s.command.chars().count())
            .max()
            .unwrap_or(0);
        for (i, step) in self.steps.iter().enumerate() {
            let pad = " ".repeat(longest - step.command.chars().count());
            let undone = if i < self.done { "" } else { "  (undone)" };
            out.push_str(&format!(
                "\n{:>width$}  {}{pad}  {}{undone}",
                i + 1,
                step.command,
                step.summary()
            ));
        }
        out.push_str(&format!(
            "\nShown: {}, {}",
            count(nodes, "node"),
            count(edges, "edge")
        ));
        let hidden = r.n_nodes() - nodes;
        if hidden > 0 {
            out.push_str(&format!(" ({} hidden)", thousands(hidden)));
        }
        out
    }

    /// `.expand`: see the [module](self).
    #[allow(clippy::too_many_arguments)]
    async fn expand(
        &mut self,
        ctx: &SessionContext,
        sql: &dyn RunSql,
        command: &str,
        id: &str,
        direction: Direction,
        limit: usize,
        sql_only: bool,
    ) -> Result<Outcome> {
        let node = self.resolve(id)? as usize;
        let Some(graph) = self.result.graph() else {
            return plan_err(
                "this drawing is not of a property graph, so there are no neighbours to add; \
                 draw a graph's nodes first: .graph NAME, or .graph after a query on one \
                 (hop, a path, an algorithm)",
            );
        };
        if self.result.node_is_removed(node) {
            return plan_err(format!("node {id} is hidden; .show {id} first"));
        }
        let graph = format!("\"{}\"", graph.replace('"', "\"\""));
        let mut seed = String::new();
        literal(
            &mut seed,
            self.result.node_keys().get(node),
            self.key_type(node),
        );

        // 1. The neighbours that are not shown, the highest degree first.
        let neighbours = format!(
            "SELECT h.id, count(*) OVER () AS neighbours \
             FROM hop({graph}, seeds => [{seed}], steps => 1, direction => '{}') AS h \
             JOIN degree({graph}, direction => 'both') AS d ON d.id = h.id \
             WHERE h.id NOT IN ({}) \
             ORDER BY d.degree DESC, h.id LIMIT {limit}",
            direction.name(),
            self.keys(false),
        );
        let (schema, batches) = sql.run(ctx, &neighbours).await?;
        let named = schema.field(0).metadata().get(meta::LABEL);
        let r = &self.result;
        // The label of a new node: the one the result says, or the node's.
        let label = named
            .and_then(|name| r.node_labels().iter().position(|l| &l.name == name))
            .map_or(r.node_label()[node], |l| l as u16);
        let key_type = r.node_labels()[label as usize].key;
        let mut restored: Vec<u32> = Vec::new();
        let mut added = NodeBatch::new();
        let mut new: HashMap<Key, u32> = HashMap::new();
        let mut not_shown = 0usize;
        for batch in &batches {
            let ids = Ids::of(batch.column(0))?;
            let total = cast(batch.column(1), &DataType::UInt64)?;
            let total = total.as_primitive::<UInt64Type>();
            for i in 0..batch.num_rows() {
                not_shown = not_shown.max(total.value(i) as usize);
                let found = r.node_labels().iter().zip(0u16..).find_map(|(l, at)| {
                    let key = ids.key(i, l.key)?;
                    r.node(at, &key)
                });
                match found {
                    Some(at) if r.node_is_removed(at as usize) => restored.push(at),
                    Some(_) => {}
                    None => {
                        let Some(key) = ids.key(i, key_type) else {
                            continue;
                        };
                        let at = (r.n_nodes() + added.len()) as u32;
                        if new.insert(key.clone(), at).is_none() {
                            added.push(label, &key);
                        }
                    }
                }
            }
        }
        restored.sort_unstable();
        restored.dedup();

        // 2. The graph's edges among every node of the session and the new.
        let mut all = self.keys(true);
        for key in added.keys.iter() {
            if !all.is_empty() {
                all.push_str(", ");
            }
            literal(&mut all, key, key_type);
        }
        let among = format!("SELECT src, dst, edge FROM induced_edges({graph}, nodes => [{all}])");
        drop(all);
        if sql_only {
            return Ok(Outcome::said(format!("{neighbours};\n{among};")));
        }
        let (schema, batches) = sql.run(ctx, &among).await?;
        let edges = self.edges.get_or_insert_with(|| EdgeIndex::of(r));
        let node_of = |ids: &Ids, i: usize| -> Option<u32> {
            let found = r.node_labels().iter().zip(0u16..).find_map(|(l, at)| {
                let key = ids.key(i, l.key)?;
                r.node(at, &key)
            });
            found.or_else(|| new.get(&ids.key(i, key_type)?).copied())
        };
        let (batch, new_label) = new_edges(edges, r, &schema, &batches, node_of, |_, _| {})?;

        let mut ops = Vec::new();
        if !restored.is_empty() {
            ops.push(Op::RestoreNodes(restored));
        }
        let new_nodes = added.len();
        if !added.is_empty() {
            ops.push(Op::AddNodes(added));
        }
        if !batch.is_empty() {
            if let Some(name) = new_label {
                ops.push(Op::AddEdgeLabel(name));
            }
            ops.push(Op::AddEdges(batch));
        }
        if ops.is_empty() {
            let whose = match direction {
                Direction::Both => "neighbour",
                Direction::In => "in-neighbour",
                Direction::Out => "out-neighbour",
            };
            return Ok(Outcome::said(format!(
                "Nothing changed: every {whose} of {id} is shown already, with the edges among \
                 them"
            )));
        }
        let statements = vec![neighbours, among];
        let delta = GraphDelta::new(ops).with_provenance(Provenance {
            sql: Some(statements.join(";\n")),
            ..Provenance::default()
        });
        let mut outcome = self.step(command, statements, delta)?;
        let brought = new_nodes + outcome_restored(&outcome);
        if not_shown > brought {
            let more = not_shown - brought;
            let are = if more == 1 {
                "neighbour of {id} is"
            } else {
                "neighbours of {id} are"
            };
            outcome.text.push_str(&format!(
                " ({} more {} not shown: .expand {id} again adds the next)",
                thousands(more),
                are.replace("{id}", id)
            ));
        }
        Ok(outcome)
    }

    /// The group `id` names in a grouped session (`group:3`), as its key
    /// and its node.
    fn group(&self, command: &str, id: &str) -> Result<usize> {
        let Some(groups) = &self.groups else {
            return plan_err(format!(
                "{command} closes a group of a grouped drawing, and this one is not grouped; \
                 draw one with .graph NAME GROUP BY column"
            ));
        };
        match group_id(id) {
            Some(Some(g)) if (g as usize) < groups.len() => Ok(g as usize),
            Some(_) => plan_err(format!(
                "no group '{id}' in the graph that is drawn; its groups are group:0 to group:{}",
                groups.len() - 1
            )),
            None => plan_err(format!(
                "this drawing is grouped by {}, and {command} takes one of its groups, as \
                 {command} group:0 (group:0 to group:{}); a group's key is in its tooltip",
                groups.by(),
                groups.len() - 1
            )),
        }
    }

    /// Each node's group in a grouped session, by slot: a group's own, and
    /// a member's.
    fn group_codes(&self) -> &[u32] {
        let r = &self.result;
        let column = r.encodings().group_by.as_deref();
        match column
            .and_then(|name| r.node_column(name))
            .map(|c| &c.values)
        {
            Some(Values::Dict { codes, .. }) => codes,
            _ => &[],
        }
    }

    /// `.expand group:N` in a grouped session: see the [module](self).
    async fn open(
        &mut self,
        ctx: &SessionContext,
        sql: &dyn RunSql,
        command: &str,
        id: &str,
        sql_only: bool,
    ) -> Result<Outcome> {
        let g = self.group(".expand", id)?;
        let groups = self.groups.as_ref().expect("a grouped session");
        let r = &self.result;
        if r.node_is_removed(g) {
            return plan_err(format!("group {g} is hidden; .show group:{g} first"));
        }
        let members = groups.members(g);
        if members.len() > groups::MAX_OPEN {
            return plan_err(format!(
                "group {g} has {} members, and a group of more than {} is not opened in place; \
                 draw it on its own: .graph SELECT … WHERE … on its members",
                thousands(members.len()),
                thousands(groups::MAX_OPEN)
            ));
        }
        // The group's members: the node each is, here already or new, and
        // its place among them.
        let n_groups = groups.len();
        let mut restored: Vec<u32> = Vec::new();
        let mut added = NodeBatch::new();
        let mut new: HashMap<(u16, Key), u32> = HashMap::new();
        let mut place: HashMap<u32, u32> = HashMap::with_capacity(members.len());
        let mut added_at: Vec<u32> = Vec::new();
        for (local, &v) in members.iter().enumerate() {
            let label = groups.label(v);
            let Ok(key) = Key::parse(r.node_labels()[label as usize].key, groups.key(v)) else {
                continue;
            };
            let slot = match r.node(label, &key) {
                Some(at) => {
                    if r.node_is_removed(at as usize) {
                        restored.push(at);
                    }
                    at
                }
                None => {
                    let at = (r.n_nodes() + added.len()) as u32;
                    added.push(label, &key);
                    added_at.push(local as u32);
                    new.insert((label, key), at);
                    at
                }
            };
            place.insert(slot, local as u32);
        }
        if restored.is_empty() && added.is_empty() {
            return Ok(Outcome::said(format!(
                "Nothing changed: group {g} is open already; .collapse group:{g} closes it"
            )));
        }
        if added.is_empty() {
            // Closed by .collapse: its members and their edges are here.
            if sql_only {
                return Ok(Outcome::said(format!(
                    "-- .expand group:{g} runs no SQL: the group's members are in the session"
                )));
            }
            return self.step(
                command,
                Vec::new(),
                GraphDelta::new(vec![Op::RestoreNodes(restored)]),
            );
        }
        let Some(graph) = r.graph() else {
            return plan_err(
                "opening a group reads the edges among its members from a property graph, and \
                 this drawing is of rows that name none; group a graph (.graph NAME GROUP BY \
                 column), or say which graph the ids are of (ON graph)",
            );
        };
        let graph = format!("\"{}\"", graph.replace('"', "\"\""));

        // The graph's edges among the members of every open group and this
        // one's.
        let mut all = String::with_capacity((r.n_nodes() - n_groups + added.len()) * 8);
        for i in n_groups..r.n_nodes() {
            if !all.is_empty() {
                all.push_str(", ");
            }
            literal(&mut all, r.node_keys().get(i), self.key_type(i));
        }
        for (key, &label) in added.keys.iter().zip(&added.labels) {
            if !all.is_empty() {
                all.push_str(", ");
            }
            literal(&mut all, key, r.node_labels()[label as usize].key);
        }
        let max_edges = crate::options::graph_max_edges(ctx.state().config());
        let among = format!(
            "SELECT src, dst, edge FROM induced_edges({graph}, nodes => [{all}]) LIMIT {}",
            max_edges.saturating_add(1)
        );
        drop(all);
        if sql_only {
            return Ok(Outcome::said(format!("{among};")));
        }
        let (schema, batches) = sql.run(ctx, &among).await?;
        let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        if rows > max_edges {
            return plan_err(format!(
                "there are more than {} edges among the members of the open groups, the most \
                 the graph view draws; close a group (.collapse group:N), or raise the limit: {}",
                thousands(max_edges),
                super::raise_max_edges(rows)
            ));
        }
        let edges = self.edges.get_or_insert_with(|| EdgeIndex::of(r));
        // A member's id is a key of a vertex table, never of `group`.
        let node_of = |ids: &Ids, i: usize| -> Option<u32> {
            r.node_labels()
                .iter()
                .zip(0u16..)
                .skip(1)
                .find_map(|(l, at)| {
                    let key = ids.key(i, l.key)?;
                    r.node(at, &key).or_else(|| new.get(&(at, key)).copied())
                })
        };
        // The edges among this group's members, for their places in its disc.
        let (mut inner_src, mut inner_dst) = (Vec::new(), Vec::new());
        let (batch, new_label) = new_edges(edges, r, &schema, &batches, node_of, |s, d| {
            if let (Some(&a), Some(&b)) = (place.get(&s), place.get(&d)) {
                inner_src.push(a);
                inner_dst.push(b);
            }
        })?;
        drop(batches);
        let (x, y) = groups::disc_layout(members.len(), inner_src, inner_dst);
        let count = added.len();
        let added = added
            .with_column(
                r.encodings().group_by.clone().unwrap_or_default(),
                BatchValues::Dict {
                    new_values: Strings::new(),
                    codes: vec![g as u32; count],
                },
            )
            .with_column(
                groups::X,
                BatchValues::F32(added_at.iter().map(|&i| x[i as usize]).collect()),
            )
            .with_column(
                groups::Y,
                BatchValues::F32(added_at.iter().map(|&i| y[i as usize]).collect()),
            );

        let mut ops = Vec::new();
        if !restored.is_empty() {
            ops.push(Op::RestoreNodes(restored));
        }
        ops.push(Op::AddNodes(added));
        if !batch.is_empty() {
            if let Some(name) = new_label {
                ops.push(Op::AddEdgeLabel(name));
            }
            ops.push(Op::AddEdges(batch));
        }
        let statements = vec![among];
        let delta = GraphDelta::new(ops).with_provenance(Provenance {
            sql: Some(statements.join(";\n")),
            ..Provenance::default()
        });
        self.step(command, statements, delta)
    }

    /// `.collapse group:N`: hide the members of group N that are shown.
    fn collapse(&mut self, command: &str, id: &str) -> Result<Outcome> {
        let g = self.group(".collapse", id)?;
        let groups = self.groups.as_ref().expect("a grouped session");
        let r = &self.result;
        let codes = self.group_codes();
        let nodes: Vec<u32> = (groups.len()..r.n_nodes())
            .filter(|&i| codes.get(i) == Some(&(g as u32)) && !r.node_is_removed(i))
            .map(|i| i as u32)
            .collect();
        if nodes.is_empty() {
            return Ok(Outcome::said(format!(
                "Nothing changed: group {g} is not open; .expand group:{g} opens it"
            )));
        }
        self.step(
            command,
            Vec::new(),
            GraphDelta::new(vec![Op::RemoveNodes(nodes)]),
        )
    }
}

/// `id` as a group's: `Some(Some(3))` for `group:3`, `Some(None)` for
/// `group:` and something that is no number, `None` for any other id.
fn group_id(id: &str) -> Option<Option<i64>> {
    let (label, key) = id.split_once(':')?;
    if !label.eq_ignore_ascii_case(groups::GROUP) {
        return None;
    }
    Some(key.parse::<i64>().ok().filter(|&g| g >= 0))
}

/// The rows of an `induced_edges` result (`src`, `dst`, `edge`) that the
/// session does not have (`edges` is its index, of `r`'s), as a batch to
/// add, and the name of its edge label if the result has none of that name
/// yet (the batch's edges are then of the label after `r`'s last).
/// `node_of` says which node an id is, if any; `seen` is told each row's
/// two nodes, whether or not the session has the edge.
fn new_edges(
    edges: &EdgeIndex,
    r: &GraphResult,
    schema: &SchemaRef,
    batches: &[RecordBatch],
    node_of: impl Fn(&Ids, usize) -> Option<u32>,
    mut seen: impl FnMut(u32, u32),
) -> Result<(EdgeBatch, Option<String>)> {
    let edge_label = schema.field(2).metadata().get(meta::LABEL);
    let at = edge_label.and_then(|name| r.edge_labels().iter().position(|l| l == name));
    let new_label = match (edge_label, at) {
        (Some(name), None) => Some(name.clone()),
        _ => None,
    };
    let e_label = match (&new_label, at) {
        (Some(_), _) => r.edge_labels().len() as u16,
        (None, at) => at.unwrap_or(0) as u16,
    };
    let mut batch = EdgeBatch::new();
    let mut rows = Vec::new();
    // How many of each pair's edges with no row this pass has matched.
    let mut matched: HashMap<(u32, u32), u32> = HashMap::new();
    for b in batches {
        let (s, d) = (Ids::of(b.column(0))?, Ids::of(b.column(1))?);
        let row = cast(b.column(2), &DataType::UInt64)?;
        let row = row.as_primitive::<UInt64Type>();
        for i in 0..b.num_rows() {
            let (Some(s), Some(d)) = (node_of(&s, i), node_of(&d, i)) else {
                continue;
            };
            seen(s, d);
            let row = row.is_valid(i).then(|| row.value(i));
            if row.is_some_and(|row| edges.rows.contains(&(e_label, row))) {
                continue;
            }
            if let Some(&have) = edges.pairs.get(&pair(s, d)) {
                let used = matched.entry(pair(s, d)).or_insert(0);
                if *used < have {
                    *used += 1;
                    continue;
                }
            }
            batch.src.push(s);
            batch.dst.push(d);
            batch.labels.push(e_label);
            rows.push(row.unwrap_or(NO_ROW));
        }
    }
    batch.rows = Some(rows);
    Ok((batch, new_label))
}

/// How many nodes an expansion's delta restored.
fn outcome_restored(outcome: &Outcome) -> usize {
    outcome.delta.as_ref().map_or(0, |delta| {
        delta
            .ops
            .iter()
            .map(|op| match op {
                Op::RestoreNodes(nodes) => nodes.len(),
                _ => 0,
            })
            .sum()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(line: &str) -> Command {
        Command::parse(line).unwrap().unwrap().command
    }

    #[test]
    fn commands_are_read_from_their_lines() {
        assert_eq!(
            parsed(".expand 33"),
            Command::Expand {
                id: "33".into(),
                direction: Direction::Both,
                limit: EXPAND_LIMIT
            }
        );
        assert_eq!(
            parsed("  .EXPAND 'a b' In limit 7 ; "),
            Command::Expand {
                id: "a b".into(),
                direction: Direction::In,
                limit: 7
            }
        );
        assert_eq!(
            parsed(".hide 1 \"it's\" 'o''neil'"),
            Command::Hide(vec!["1".into(), "it's".into(), "o'neil".into()])
        );
        assert_eq!(parsed(".show ALL"), Command::Show(Vec::new()));
        assert_eq!(
            parsed(".show all 3"),
            Command::Show(vec!["all".into(), "3".into()])
        );
        assert_eq!(parsed(".undo;"), Command::Undo);
        assert_eq!(parsed(".redo"), Command::Redo);
        assert_eq!(parsed(".graph History"), Command::History);
        let sql = Command::parse(".expand 5 --sql out").unwrap().unwrap();
        assert!(sql.sql_only);
        assert_eq!(sql.text, ".expand 5 --sql out");
        assert_eq!(
            sql.command,
            Command::Expand {
                id: "5".into(),
                direction: Direction::Out,
                limit: EXPAND_LIMIT
            }
        );
    }

    #[test]
    fn other_lines_are_not_explore_commands() {
        for line in [
            ".graph",
            ".graph karate",
            ".graph SELECT history FROM t",
            ".tables",
            "SELECT 1",
            "",
        ] {
            assert_eq!(Command::parse(line).unwrap(), None, "{line}");
        }
        for line in [
            ".expand",
            ".expand 1 2",
            ".expand 1 in out",
            ".expand 1 LIMIT",
            ".expand 1 LIMIT x",
            ".hide",
            ".hide 'open",
            ".redo 2",
        ] {
            assert!(Command::parse(line).is_err(), "{line}");
        }
    }

    #[test]
    fn changes_are_signed_and_counted() {
        assert_eq!(changed(16, 41), "+16 nodes, +41 edges");
        assert_eq!(changed(-1, -1), "-1 node, -1 edge");
        assert_eq!(changed(0, 1200), "+0 nodes, +1,200 edges");
    }
}
