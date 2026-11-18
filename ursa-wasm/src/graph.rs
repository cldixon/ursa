//! A result as a graph, for the page's Graph view (`.graph`, drawn by
//! `ursa-viz`): each row an edge, from its `src` column to its `dst` column
//! (or the first two columns), the ids of any type mapped to dense node
//! indexes `0..n`, in the order they are first seen, so the same result always
//! gives the same graph.
//!
//! [`WebShell::graph`](crate::WebShell::graph) finds what to draw: the last
//! result, a query's, or a property graph's edge tables (and its vertex
//! tables' keys, so a node with no edges is drawn too).

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{new_null_array, Array, ArrayRef, Float32Array, RecordBatch, UInt32Array};
use arrow::compute::{cast, concat, interleave};
use arrow::datatypes::{DataType, Schema, SchemaRef};
use arrow::row::{RowConverter, SortField};
use arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::common::TableReference;
use datafusion::dataframe::DataFrame;
use datafusion::datasource::provider_as_source;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::LogicalPlanBuilder;
use datafusion::prelude::SessionContext;
use ursa_engine::graph::functions;
use ursa_engine::graph::schema::graph_table;
use ursa_engine::graph::{edges_plan, registry_of, GraphDef, PropertyGraph};

/// The most edges the view draws: about what a laptop's browser lays out and
/// draws, and 16 MB of node indexes to hand it.
pub(crate) const MAX_EDGES: usize = 2_000_000;
/// The most nodes, for a property graph whose vertex tables are large.
pub(crate) const MAX_NODES: usize = 4_000_000;

/// A graph to draw: `n` nodes, the edges `(src[i], dst[i])` between them,
/// and each node's id as the shell prints it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EdgeList {
    pub n: u32,
    pub src: Vec<u32>,
    pub dst: Vec<u32>,
    pub ids: Vec<String>,
    /// Rows left out because an endpoint is NULL.
    pub skipped: usize,
    /// What sizes the nodes (`SIZE BY`), if asked.
    pub size: Option<SizeBy>,
    /// What colours the nodes (`COLOR BY`), if asked.
    pub color: Option<ColorBy>,
}

fn plan_err<T>(message: impl Into<String>) -> Result<T> {
    Err(DataFusionError::Plan(message.into()))
}

/// The columns that hold each edge's endpoints: `src` and `dst` (in any
/// case), or else the first two.
pub(crate) fn endpoint_columns(schema: &Schema) -> Result<(usize, usize)> {
    let n = schema.fields().len();
    if n < 2 {
        return plan_err(format!(
            "the result has {n} column{}; a graph needs two, each edge's source and destination \
             (columns named src and dst, or else the first two)",
            if n == 1 { "" } else { "s" }
        ));
    }
    let find = |name: &str| {
        schema
            .fields()
            .iter()
            .position(|f| f.name().eq_ignore_ascii_case(name))
    };
    Ok(match (find("src"), find("dst")) {
        (Some(s), Some(d)) => (s, d),
        _ => (0, 1),
    })
}

/// The edges of a result (`schema`, `batches`), from its endpoint columns.
#[cfg(test)]
pub(crate) fn from_result(schema: &Schema, batches: &[RecordBatch]) -> Result<EdgeList> {
    Ok(from_result_indexed(schema, batches)?.0)
}

fn from_result_indexed(schema: &Schema, batches: &[RecordBatch]) -> Result<(EdgeList, NodeIndex)> {
    let (s, d) = endpoint_columns(schema)?;
    let edges: Vec<(ArrayRef, ArrayRef)> = batches
        .iter()
        .map(|b| (Arc::clone(b.column(s)), Arc::clone(b.column(d))))
        .collect();
    build_indexed(&[], &edges)
}

/// The graph with the nodes `vertices` (keys, in order) and the edges
/// `edges` (source and destination arrays of equal length). Nodes are
/// numbered as first seen: the vertices, then the edges' endpoints.
#[cfg(test)]
pub(crate) fn build(vertices: &[ArrayRef], edges: &[(ArrayRef, ArrayRef)]) -> Result<EdgeList> {
    Ok(build_indexed(vertices, edges)?.0)
}

/// [`build`], and the index from node ids to node indexes, to join node
/// values on.
fn build_indexed(
    vertices: &[ArrayRef],
    edges: &[(ArrayRef, ArrayRef)],
) -> Result<(EdgeList, NodeIndex)> {
    let rows: usize = edges.iter().map(|(s, _)| s.len()).sum();
    if rows == 0 {
        return plan_err("the result has no rows: there are no edges to draw");
    }
    if rows > MAX_EDGES {
        return plan_err(format!(
            "the result has {} edges; the graph view draws at most {}. Draw part of it, \
             with a WHERE or a LIMIT: .graph SELECT src, dst FROM … LIMIT 100000",
            thousands(rows),
            thousands(MAX_EDGES)
        ));
    }
    let keys: usize = vertices.iter().map(|v| v.len()).sum();
    if keys > MAX_NODES {
        return plan_err(format!(
            "the graph has {} nodes; the graph view draws at most {}",
            thousands(keys),
            thousands(MAX_NODES)
        ));
    }
    // One key type for every id, so 7 in one column is 7 in the other: the
    // columns' own when they agree, else their text.
    let mut types = vertices
        .iter()
        .chain(edges.iter().flat_map(|(s, d)| [s, d]))
        .map(|a| a.data_type());
    let first = types.next().cloned().unwrap_or(DataType::Null);
    let key_type = if types.all(|t| t == &first) {
        first
    } else {
        DataType::Utf8
    };
    let converter = RowConverter::new(vec![SortField::new(key_type.clone())]).map_err(|_| {
        DataFusionError::Plan(format!("node ids of type {key_type} cannot be drawn"))
    })?;
    let mut nodes = Nodes {
        index: HashMap::new(),
        ids: Vec::new(),
    };
    let options = FormatOptions::default();
    let prepare = |array: &ArrayRef| -> Result<ArrayRef> {
        Ok(if array.data_type() == &key_type {
            Arc::clone(array)
        } else {
            cast(array, &key_type)?
        })
    };

    for keys in vertices {
        let keys = prepare(keys)?;
        let rows = converter.convert_columns(&[Arc::clone(&keys)])?;
        let shown = ArrayFormatter::try_new(keys.as_ref(), &options)?;
        for i in 0..keys.len() {
            if keys.is_valid(i) {
                nodes.intern(rows.row(i).as_ref(), || shown.value(i).to_string());
            }
        }
    }
    let mut out = EdgeList {
        n: 0,
        src: Vec::with_capacity(rows),
        dst: Vec::with_capacity(rows),
        ids: Vec::new(),
        skipped: 0,
        size: None,
        color: None,
    };
    for (s, d) in edges {
        let (s, d) = (prepare(s)?, prepare(d)?);
        let (s_rows, d_rows) = (
            converter.convert_columns(&[Arc::clone(&s)])?,
            converter.convert_columns(&[Arc::clone(&d)])?,
        );
        let (s_shown, d_shown) = (
            ArrayFormatter::try_new(s.as_ref(), &options)?,
            ArrayFormatter::try_new(d.as_ref(), &options)?,
        );
        for i in 0..s.len() {
            if s.is_null(i) || d.is_null(i) {
                out.skipped += 1;
                continue;
            }
            out.src
                .push(nodes.intern(s_rows.row(i).as_ref(), || s_shown.value(i).to_string()));
            out.dst
                .push(nodes.intern(d_rows.row(i).as_ref(), || d_shown.value(i).to_string()));
        }
    }
    if out.src.is_empty() {
        return plan_err(format!(
            "every row has a NULL source or destination: there are no edges to draw ({} rows)",
            thousands(rows)
        ));
    }
    out.n = nodes.ids.len() as u32;
    out.ids = nodes.ids;
    Ok((
        out,
        NodeIndex {
            key_type,
            index: nodes.index,
        },
    ))
}

/// Node indexes by id, as [`build`] numbered them: what node values are
/// joined on.
pub(crate) struct NodeIndex {
    key_type: DataType,
    index: HashMap<Vec<u8>, u32>,
}

impl NodeIndex {
    /// The node each of `ids` is, if it is one.
    fn lookup(&self, ids: &ArrayRef) -> Result<Vec<Option<u32>>> {
        let ids = if ids.data_type() == &self.key_type {
            Arc::clone(ids)
        } else {
            cast(ids, &self.key_type).map_err(|_| {
                DataFusionError::Plan(format!(
                    "node ids of type {} cannot be matched to the graph's, of type {}",
                    ids.data_type(),
                    self.key_type
                ))
            })?
        };
        let converter = RowConverter::new(vec![SortField::new(self.key_type.clone())])?;
        let rows = converter.convert_columns(&[Arc::clone(&ids)])?;
        Ok((0..ids.len())
            .map(|i| {
                if ids.is_null(i) {
                    None
                } else {
                    self.index.get(rows.row(i).as_ref()).copied()
                }
            })
            .collect())
    }
}

/// Node indexes by id (an id's row-format bytes), numbered as first seen.
struct Nodes {
    index: HashMap<Vec<u8>, u32>,
    ids: Vec<String>,
}

impl Nodes {
    fn intern(&mut self, key: &[u8], shown: impl FnOnce() -> String) -> u32 {
        if let Some(&i) = self.index.get(key) {
            return i;
        }
        let i = self.ids.len() as u32;
        self.index.insert(key.to_vec(), i);
        self.ids.push(shown());
        i
    }
}

/// `n` with thousands separators: 2,000,000.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What `.graph NAME` names, if NAME is one identifier: unquoted, it is
/// lower-cased as SQL lower-cases names; quoted, it is kept.
pub(crate) fn identifier(text: &str) -> Option<String> {
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

/// The edges of `graph`: every edge table's source and destination columns,
/// and every vertex table's keys, so nodes with no edges are drawn too.
async fn property_graph_indexed(
    ctx: &SessionContext,
    graph: &PropertyGraph,
) -> Result<(EdgeList, NodeIndex)> {
    let state = ctx.state();
    let mut vertices = Vec::new();
    for v in &graph.vertex_tables {
        let batches = scan_columns(ctx, graph, &v.table, &[v.key.as_str()]).await?;
        vertices.extend(batches.iter().map(|b| Arc::clone(b.column(0))));
    }
    let mut edges = Vec::new();
    for e in &graph.edge_tables {
        let def = GraphDef {
            name: graph.name.clone(),
            edges: e.clone(),
            vertices: None,
        };
        let plan = edges_plan(&state, &def).await?;
        let batches = DataFrame::new(state.clone(), plan)
            .select_columns(&[e.source.as_str(), e.destination.as_str()])?
            .collect()
            .await?;
        edges.extend(
            batches
                .iter()
                .map(|b| (Arc::clone(b.column(0)), Arc::clone(b.column(1)))),
        );
    }
    if graph.edge_tables.is_empty() {
        return plan_err(format!(
            "graph '{}' has no edge tables: there are no edges to draw",
            graph.name
        ));
    }
    build_indexed(&vertices, &edges)
}

/// The columns `columns` of one of `graph`'s tables, every row.
async fn scan_columns(
    ctx: &SessionContext,
    graph: &PropertyGraph,
    table: &TableReference,
    columns: &[&str],
) -> Result<Vec<RecordBatch>> {
    let state = ctx.state();
    let provider = graph_table(&state, &graph.name, table).await?;
    let plan =
        LogicalPlanBuilder::scan(table.clone(), provider_as_source(provider), None)?.build()?;
    DataFrame::new(state.clone(), plan)
        .select_columns(columns)?
        .collect()
        .await
}

/// A query's result (its last statement's): its schema and rows.
async fn run_query(ctx: &SessionContext, text: &str) -> Result<(SchemaRef, Vec<RecordBatch>)> {
    let statements = ursa_sql::parse_sql_with_text_for(ctx, text)?;
    let mut last = None;
    for (statement, sql) in statements {
        last = Some(ursa_sql::execute_with_text(ctx, statement, &sql).await?);
    }
    let Some(frame) = last else {
        return plan_err("no query to draw; .graph SELECT src, dst FROM …");
    };
    let schema = Arc::new(frame.schema().as_arrow().clone());
    Ok((schema, frame.collect().await?))
}

/// What `.graph` draws: a property graph, or rows that are edges.
pub(crate) enum Source {
    Graph(Arc<PropertyGraph>),
    Rows(SchemaRef, Vec<RecordBatch>),
}

impl Source {
    /// What `.graph TEXT` names: the property graph TEXT names, a table's
    /// rows, or a query's result (its last statement's).
    pub(crate) async fn of_text(ctx: &SessionContext, text: &str) -> Result<Source> {
        if let Some(name) = identifier(text) {
            if let Some(graph) = registry_of(ctx).get(&name) {
                return Ok(Source::Graph(graph));
            }
            let table = TableReference::bare(name.as_str());
            if !ctx.table_exist(table.clone())? {
                return plan_err(format!(
                    "no graph or table named '{name}'; .graphs lists the graphs and .tables the \
                     tables, or give a query: .graph SELECT src, dst FROM …"
                ));
            }
            let frame = ctx.table(table).await?;
            let schema = Arc::new(frame.schema().as_arrow().clone());
            return Ok(Source::Rows(schema, frame.collect().await?));
        }
        let (schema, batches) = run_query(ctx, text).await?;
        Ok(Source::Rows(schema, batches))
    }
}

/// The graph `source` draws, styled as `style` says (`SIZE BY`, `COLOR BY`,
/// `NODES`).
pub(crate) async fn draw(ctx: &SessionContext, source: Source, style: &Style) -> Result<EdgeList> {
    let (mut edges, index) = match &source {
        Source::Graph(g) => property_graph_indexed(ctx, g).await?,
        Source::Rows(schema, batches) => from_result_indexed(schema, batches)?,
    };
    if style.size.is_none() && style.color.is_none() {
        if style.nodes.is_some() {
            return plan_err(
                "NODES (…) gives values for SIZE BY or COLOR BY to use; add one, as in \
                 .graph … NODES (SELECT id, score FROM …) SIZE BY score",
            );
        }
        return Ok(edges);
    }
    let graph = match &source {
        Source::Graph(g) => Some(g.as_ref()),
        Source::Rows(..) => None,
    };
    let nodes = match &style.nodes {
        Some(query) => Some(NodeValues::of_query(ctx, query).await?),
        None => None,
    };
    // Where each name's values come from; the algorithms run together.
    let mut found = Vec::new();
    for (clause, name) in [("SIZE BY", &style.size), ("COLOR BY", &style.color)] {
        if let Some(name) = name {
            found.push((
                clause,
                name,
                resolve(ctx, graph, nodes.as_ref(), clause, name).await?,
            ));
        }
    }
    let mut algorithms: Vec<&'static str> = Vec::new();
    for (_, _, f) in &found {
        if let Found::Algorithm(name) = f {
            if !algorithms.contains(name) {
                algorithms.push(name);
            }
        }
    }
    let computed = match (graph, algorithms.is_empty()) {
        (Some(g), false) => Some(run_algorithms(ctx, g, &algorithms).await?),
        _ => None,
    };
    for (clause, name, f) in found {
        let column = match f {
            Found::Nodes(i) => nodes.as_ref().expect("NODES was given").column(i),
            Found::Vertex => vertex_column(ctx, graph.expect("a graph"), &name.ident).await?,
            Found::Algorithm(algorithm) => computed.as_ref().expect("ran").column_named(algorithm),
            Found::Degree => degree_column(&edges),
        };
        let values = column.per_node(&index, edges.n as usize)?;
        if clause == "SIZE BY" {
            edges.size = Some(size_by(&name.shown, &values)?);
        } else {
            edges.color = Some(color_by(&name.shown, &values)?);
        }
    }
    Ok(edges)
}

// ---------------------------------------------------------------- styles
//
// `.graph [SOURCE] [NODES (query)] [SIZE BY x] [COLOR BY y]`: what sizes and
// colours the nodes. x and y name, in turn: a column of NODES's query; a
// column of the property graph's vertex tables; `degree`; or a per-node
// algorithm, run on the graph with defaults through `algorithms(G, …)`. A
// source that is rows (a query, a table, the last result) has no vertex
// tables and no graph to run an algorithm on: NODES and degree only.

/// The most categories `COLOR BY` colours; the rest are drawn as "other",
/// at index [`OTHER`]. The page's palette has as many colours, then other's.
pub(crate) const MAX_CATEGORIES: usize = 8;
/// The colour index of a node in no top category, or with no value.
pub(crate) const OTHER: u32 = MAX_CATEGORIES as u32;

/// `SIZE BY x`: one value per node (NaN for a node with none), which the page
/// maps to radii.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SizeBy {
    /// What it is by, as typed.
    pub by: String,
    pub values: Vec<f32>,
    /// Nodes with no value.
    pub missing: usize,
}

/// `COLOR BY y`: each node's colour index, `0..MAX_CATEGORIES` for the
/// largest categories, most nodes first, or [`OTHER`]; and the legend.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColorBy {
    pub by: String,
    pub index: Vec<u32>,
    /// The categories coloured, by colour index: their values as the shell
    /// prints them, and how many nodes each has.
    pub labels: Vec<String>,
    pub counts: Vec<u32>,
    /// Nodes in the categories past the first [`MAX_CATEGORIES`].
    pub other: u32,
    /// Nodes with no value (NULL, or none given).
    pub missing: u32,
}

/// What `.graph` text asks for besides its source.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Style {
    /// `NODES (query)`: the query.
    pub nodes: Option<String>,
    pub size: Option<Name>,
    pub color: Option<Name>,
}

/// A name after `SIZE BY` or `COLOR BY`: as SQL reads it (lower-cased unless
/// quoted), and as typed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Name {
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
pub(crate) fn parse(text: &str) -> Result<(String, Style)> {
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

/// Where a name's values come from.
enum Found {
    /// A column of NODES's query, by index.
    Nodes(usize),
    /// A column of the property graph's vertex tables.
    Vertex,
    /// A per-node algorithm, run on the graph.
    Algorithm(&'static str),
    /// Each node's degree in the drawing.
    Degree,
}

/// The per-node algorithms `algorithms(G, …)` runs: every one that reads no
/// node values.
fn per_node_algorithm(name: &str) -> Option<&'static str> {
    functions::lookup(name)
        .filter(|s| functions::is_per_node(s) && !s.reads_attribute())
        .map(|s| s.name)
}

fn per_node_algorithms() -> Vec<&'static str> {
    functions::specs()
        .filter(|s| functions::is_per_node(s) && !s.reads_attribute())
        .map(|s| s.name)
        .collect()
}

/// `names[i]` that is `ident`: exactly, or else in any case.
fn find_name<'a>(names: impl Iterator<Item = &'a str> + Clone, ident: &str) -> Option<usize> {
    names
        .clone()
        .position(|n| n == ident)
        .or_else(|| names.clone().position(|n| n.eq_ignore_ascii_case(ident)))
}

/// Where `name` (after `clause`) takes its values from: NODES's columns,
/// then the graph's vertex columns, then degree, then an algorithm.
async fn resolve(
    ctx: &SessionContext,
    graph: Option<&PropertyGraph>,
    nodes: Option<&NodeValues>,
    clause: &str,
    name: &Name,
) -> Result<Found> {
    if let Some(i) = nodes.and_then(|n| n.find(&name.ident)) {
        return Ok(Found::Nodes(i));
    }
    let mut columns = Vec::new();
    if let Some(g) = graph {
        for (_, _, names) in vertex_columns(ctx, g).await? {
            if find_name(names.iter().map(String::as_str), &name.ident).is_some() {
                return Ok(Found::Vertex);
            }
            columns.extend(names);
        }
    }
    if name.ident == "degree" {
        return Ok(Found::Degree);
    }
    let algorithm = per_node_algorithm(&name.ident);
    let shown = &name.shown;
    let nodes_columns = nodes
        .map(|n| format!("; NODES (…) has {}", n.value_names().join(", ")))
        .unwrap_or_default();
    match (graph, algorithm) {
        (Some(_), Some(algorithm)) => Ok(Found::Algorithm(algorithm)),
        (Some(g), None) => {
            columns.sort();
            columns.dedup();
            plan_err(format!(
                "{clause} {shown}: graph '{}' has no vertex column or algorithm named '{}'. Its \
                 vertex columns are {}; the algorithms are degree, {}{nodes_columns}",
                g.name,
                name.ident,
                if columns.is_empty() {
                    "none (it has no vertex tables)".to_string()
                } else {
                    columns.join(", ")
                },
                per_node_algorithms()
                    .into_iter()
                    .filter(|a| *a != "degree")
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
        (None, Some(algorithm)) => plan_err(format!(
            "{clause} {shown} runs {}() on a property graph, and this draws rows, which have none \
             to run it on: draw the graph by name (.graph NAME {clause} {shown}), or give the \
             values: NODES (SELECT id, … AS {shown} FROM …) {clause} {shown}",
            algorithm
        )),
        (None, None) => plan_err(format!(
            "{clause} {shown}: no column named '{}'. Rows drawn as edges are sized or coloured \
             by degree, or by a column of NODES (SELECT id, … FROM …){nodes_columns}",
            name.ident
        )),
    }
}

/// Each vertex table of `graph`: its name, its key, and its columns.
async fn vertex_columns(
    ctx: &SessionContext,
    graph: &PropertyGraph,
) -> Result<Vec<(TableReference, String, Vec<String>)>> {
    let state = ctx.state();
    let mut out = Vec::new();
    for v in &graph.vertex_tables {
        let provider = graph_table(&state, &graph.name, &v.table).await?;
        let names = provider
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect();
        out.push((v.table.clone(), v.key.clone(), names));
    }
    Ok(out)
}

/// The vertex column `ident`, from every vertex table that has it.
async fn vertex_column(ctx: &SessionContext, graph: &PropertyGraph, ident: &str) -> Result<Column> {
    let mut column = Column {
        ids: Some(Vec::new()),
        values: Vec::new(),
    };
    for (table, key, names) in vertex_columns(ctx, graph).await? {
        let Some(i) = find_name(names.iter().map(String::as_str), ident) else {
            continue;
        };
        // The key itself (COLOR BY id) is scanned once.
        let columns = if names[i] == key {
            vec![key.as_str()]
        } else {
            vec![key.as_str(), names[i].as_str()]
        };
        for b in scan_columns(ctx, graph, &table, &columns).await? {
            let ids = column.ids.as_mut().expect("ids");
            ids.push(Arc::clone(b.column(0)));
            column.values.push(Arc::clone(b.column(columns.len() - 1)));
        }
    }
    Ok(column)
}

/// Each node's degree as drawn: its edges' ends (a loop's two).
fn degree_column(edges: &EdgeList) -> Column {
    let mut degree = vec![0u32; edges.n as usize];
    for (&s, &d) in edges.src.iter().zip(&edges.dst) {
        degree[s as usize] += 1;
        degree[d as usize] += 1;
    }
    Column {
        ids: None,
        values: vec![Arc::new(UInt32Array::from(degree))],
    }
}

/// `algorithms(graph, …)` with each of `specs` at the view's defaults: read
/// as undirected (`direction => 'both'`) where it can be, as the view draws
/// the graph, and seeded (`seed => 1`) so a drawing is the same each time.
async fn run_algorithms(
    ctx: &SessionContext,
    graph: &PropertyGraph,
    algorithms: &[&'static str],
) -> Result<NodeValues> {
    let calls: Vec<String> = algorithms
        .iter()
        .map(|a| format!("{a} => {}", default_call(a)))
        .collect();
    let sql = format!(
        "SELECT * FROM algorithms(\"{}\", {})",
        graph.name.replace('"', "\"\""),
        calls.join(", ")
    );
    NodeValues::of_query(ctx, &sql).await
}

/// A call to the algorithm `name` with the view's defaults.
fn default_call(name: &str) -> String {
    let spec = functions::lookup(name).expect("a catalog function");
    let mut args = Vec::new();
    if spec.directions().contains(&"both") {
        args.push("direction => 'both'");
    }
    if spec.param("seed").is_some() {
        args.push("seed => 1");
    }
    format!("{}({})", spec.name, args.join(", "))
}

/// Rows of node values: an id column (`id`, or else the first) and others.
struct NodeValues {
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    id: usize,
}

impl NodeValues {
    async fn of_query(ctx: &SessionContext, query: &str) -> Result<NodeValues> {
        let (schema, batches) = run_query(ctx, query).await?;
        if schema.fields().len() < 2 {
            return plan_err(format!(
                "NODES (…) needs two columns or more, each node's id and its values; got {}",
                schema.fields().len()
            ));
        }
        let id = find_name(schema.fields().iter().map(|f| f.name().as_str()), "id").unwrap_or(0);
        Ok(NodeValues {
            schema,
            batches,
            id,
        })
    }

    /// The column called `ident`, other than the ids.
    fn find(&self, ident: &str) -> Option<usize> {
        let names = self.schema.fields().iter().map(|f| f.name().as_str());
        find_name(names, ident).filter(|&i| i != self.id)
    }

    fn column_named(&self, name: &str) -> Column {
        self.column(self.find(name).expect("the column asked for"))
    }

    fn value_names(&self) -> Vec<String> {
        let fields = self.schema.fields().iter().enumerate();
        fields
            .filter(|(i, _)| *i != self.id)
            .map(|(_, f)| f.name().clone())
            .collect()
    }

    fn column(&self, i: usize) -> Column {
        Column {
            ids: Some(
                self.batches
                    .iter()
                    .map(|b| Arc::clone(b.column(self.id)))
                    .collect(),
            ),
            values: self
                .batches
                .iter()
                .map(|b| Arc::clone(b.column(i)))
                .collect(),
        }
    }
}

/// Values with the node ids they belong to, or (no ids) one per node.
struct Column {
    ids: Option<Vec<ArrayRef>>,
    values: Vec<ArrayRef>,
}

impl Column {
    /// One value per node, in node order: NULL for a node with none. An id
    /// given twice keeps its first value.
    fn per_node(&self, index: &NodeIndex, n: usize) -> Result<ArrayRef> {
        let data_type = self
            .values
            .first()
            .map(|v| v.data_type().clone())
            .unwrap_or(DataType::Null);
        let mut sources: Vec<ArrayRef> = Vec::with_capacity(self.values.len() + 1);
        for v in &self.values {
            sources.push(if v.data_type() == &data_type {
                Arc::clone(v)
            } else {
                cast(v, &data_type)?
            });
        }
        let Some(ids) = &self.ids else {
            return Ok(concat(
                &sources.iter().map(|a| a.as_ref()).collect::<Vec<_>>(),
            )?);
        };
        let missing = sources.len();
        sources.push(new_null_array(&data_type, 1));
        let mut at = vec![(missing, 0usize); n];
        let mut given = vec![false; n];
        for (b, ids) in ids.iter().enumerate() {
            for (row, node) in index.lookup(ids)?.into_iter().enumerate() {
                if let Some(node) = node {
                    let node = node as usize;
                    if !given[node] {
                        given[node] = true;
                        at[node] = (b, row);
                    }
                }
            }
        }
        let refs: Vec<&dyn Array> = sources.iter().map(|a| a.as_ref()).collect();
        Ok(interleave(&refs, &at)?)
    }
}

/// `SIZE BY`: the values as numbers.
fn size_by(by: &str, values: &ArrayRef) -> Result<SizeBy> {
    let t = values.data_type();
    if !t.is_numeric() {
        let t = match t {
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "text".to_string(),
            other => other.to_string(),
        };
        return plan_err(format!(
            "SIZE BY {by} needs numbers, and {by} is {t}; colour nodes by it instead: COLOR BY {by}"
        ));
    }
    let numbers = cast(values, &DataType::Float32)?;
    let numbers = numbers
        .as_any()
        .downcast_ref::<Float32Array>()
        .expect("cast to Float32");
    if numbers.null_count() == numbers.len() {
        return plan_err(format!(
            "SIZE BY {by}: no node has a value (are the ids the graph's?)"
        ));
    }
    Ok(SizeBy {
        by: by.to_string(),
        values: numbers.iter().map(|v| v.unwrap_or(f32::NAN)).collect(),
        missing: numbers.null_count(),
    })
}

/// `COLOR BY`: the values as categories, the largest coloured.
fn color_by(by: &str, values: &ArrayRef) -> Result<ColorBy> {
    let options = FormatOptions::default();
    let shown = ArrayFormatter::try_new(values.as_ref(), &options)?;
    // Each category: its label, its count, and where it was first seen.
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut categories: Vec<(String, u32)> = Vec::new();
    let mut of_node = Vec::with_capacity(values.len());
    for i in 0..values.len() {
        if values.is_null(i) {
            of_node.push(None);
            continue;
        }
        let label = shown.value(i).to_string();
        let c = *seen.entry(label.clone()).or_insert_with(|| {
            categories.push((label, 0));
            categories.len() - 1
        });
        categories[c].1 += 1;
        of_node.push(Some(c));
    }
    let with_values = values.len() - values.null_count();
    if with_values == 0 {
        return plan_err(format!(
            "COLOR BY {by}: no node has a value (are the ids the graph's?)"
        ));
    }
    let distinct = categories.len();
    let t = values.data_type();
    let fractional = matches!(
        t,
        DataType::Float16
            | DataType::Float32
            | DataType::Float64
            | DataType::Decimal32(..)
            | DataType::Decimal64(..)
            | DataType::Decimal128(..)
            | DataType::Decimal256(..)
    );
    if distinct > MAX_CATEGORIES && (fractional || 2 * distinct > with_values) {
        let advice = if fractional {
            format!(" A measure is better shown by size: SIZE BY {by}")
        } else if t.is_numeric() {
            format!(
                " Colour by a column with fewer values, or size nodes by this one: SIZE BY {by}"
            )
        } else {
            " Colour by a column with fewer values.".to_string()
        };
        return plan_err(format!(
            "COLOR BY {by}: {} distinct values over {} nodes are too many categories to tell \
             apart by colour.{advice}",
            thousands(distinct),
            thousands(with_values),
        ));
    }
    // The largest first; equal ones as first seen.
    let mut order: Vec<usize> = (0..distinct).collect();
    order.sort_by(|&a, &b| categories[b].1.cmp(&categories[a].1).then(a.cmp(&b)));
    let mut rank = vec![OTHER; distinct];
    for (r, &c) in order.iter().take(MAX_CATEGORIES).enumerate() {
        rank[c] = r as u32;
    }
    let top = &order[..distinct.min(MAX_CATEGORIES)];
    Ok(ColorBy {
        by: by.to_string(),
        index: of_node
            .iter()
            .map(|c| c.map_or(OTHER, |c| rank[c]))
            .collect(),
        labels: top.iter().map(|&c| categories[c].0.clone()).collect(),
        counts: top.iter().map(|&c| categories[c].1).collect(),
        other: order[top.len()..].iter().map(|&c| categories[c].1).sum(),
        missing: values.null_count() as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, Int64Array, StringArray};
    use arrow::datatypes::Field;

    fn batch(columns: Vec<(&str, ArrayRef)>) -> RecordBatch {
        RecordBatch::try_from_iter(columns).unwrap()
    }

    #[test]
    fn ids_of_any_type_become_dense_indexes_in_first_seen_order() {
        let b = batch(vec![
            (
                "a",
                Arc::new(StringArray::from(vec!["x", "y", "z"])) as ArrayRef,
            ),
            (
                "b",
                Arc::new(StringArray::from(vec!["y", "z", "x"])) as ArrayRef,
            ),
        ]);
        let g = from_result(&b.schema(), std::slice::from_ref(&b)).unwrap();
        assert_eq!(g.n, 3);
        assert_eq!(g.src, vec![0, 1, 2]);
        assert_eq!(g.dst, vec![1, 2, 0]);
        assert_eq!(g.ids, vec!["x", "y", "z"]);
        // The same result, the same graph.
        assert_eq!(from_result(&b.schema(), &[b]).unwrap(), g);
    }

    #[test]
    fn src_and_dst_columns_are_used_wherever_they_are() {
        let b = batch(vec![
            ("weight", Arc::new(Int64Array::from(vec![5, 6])) as ArrayRef),
            ("DST", Arc::new(Int64Array::from(vec![30, 10])) as ArrayRef),
            ("Src", Arc::new(Int64Array::from(vec![10, 20])) as ArrayRef),
        ]);
        let g = from_result(&b.schema(), &[b]).unwrap();
        assert_eq!(g.ids, vec!["10", "30", "20"]);
        assert_eq!((g.src, g.dst), (vec![0, 2], vec![1, 0]));
    }

    #[test]
    fn nulls_are_skipped_and_mixed_types_compare_as_text() {
        let b = batch(vec![
            (
                "src",
                Arc::new(Int64Array::from(vec![Some(1), None, Some(2)])) as ArrayRef,
            ),
            (
                "dst",
                Arc::new(StringArray::from(vec!["2", "3", "1"])) as ArrayRef,
            ),
        ]);
        let g = from_result(&b.schema(), &[b]).unwrap();
        assert_eq!(g.skipped, 1);
        assert_eq!(g.n, 2);
        assert_eq!((g.src, g.dst), (vec![0, 1], vec![1, 0]));
    }

    #[test]
    fn what_cannot_be_drawn_says_why() {
        let one = Schema::new(vec![Field::new("x", DataType::Int64, true)]);
        let err = from_result(&one, &[]).unwrap_err().to_string();
        assert!(err.contains("the result has 1 column;"), "{err}");
        let two = Arc::new(Schema::new(vec![
            Field::new("src", DataType::Int64, true),
            Field::new("dst", DataType::Int64, true),
        ]));
        let err = from_result(&two, &[]).unwrap_err().to_string();
        assert!(err.contains("no rows"), "{err}");
        let nulls = RecordBatch::try_new(
            Arc::clone(&two),
            vec![
                Arc::new(Int64Array::from(vec![None::<i64>])),
                Arc::new(Int64Array::from(vec![Some(1)])),
            ],
        )
        .unwrap();
        let err = from_result(&two, &[nulls]).unwrap_err().to_string();
        assert!(err.contains("NULL"), "{err}");
        let big = Arc::new(Int64Array::from_iter_values(0..(MAX_EDGES as i64 + 1))) as ArrayRef;
        let err = build(&[], &[(Arc::clone(&big), big)])
            .unwrap_err()
            .to_string();
        assert!(err.contains("2,000,001 edges"), "{err}");
        assert!(err.contains("at most 2,000,000"), "{err}");
    }

    #[test]
    fn vertex_keys_come_first_so_lone_nodes_are_drawn() {
        let keys = Arc::new(Int64Array::from(vec![3, 2, 1, 9])) as ArrayRef;
        let s = Arc::new(Int64Array::from(vec![1, 2])) as ArrayRef;
        let d = Arc::new(Int64Array::from(vec![2, 3])) as ArrayRef;
        let g = build(&[keys], &[(s, d)]).unwrap();
        assert_eq!(g.n, 4);
        assert_eq!(g.ids, vec!["3", "2", "1", "9"]);
        assert_eq!((g.src, g.dst), (vec![2, 1], vec![1, 0]));
    }

    fn parsed(text: &str) -> (String, Style) {
        parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    fn name(ident: &str, shown: &str) -> Option<Name> {
        Some(Name {
            ident: ident.into(),
            shown: shown.into(),
        })
    }

    #[test]
    fn clauses_follow_the_source_in_any_order() {
        assert_eq!(parsed("karate"), ("karate".into(), Style::default()));
        assert_eq!(parsed(""), ("".into(), Style::default()));
        let (source, style) = parsed("karate SIZE BY PageRank color by louvain;");
        assert_eq!(source, "karate");
        assert_eq!(style.size, name("pagerank", "PageRank"));
        assert_eq!(style.color, name("louvain", "louvain"));
        assert_eq!(style.nodes, None);
        // The last result, a quoted name, COLOUR, NODES first.
        let (source, style) =
            parsed("NODES (SELECT id, (x + 1) AS \"Big X\" FROM t) COLOUR BY \"Big X\"");
        assert_eq!(source, "");
        assert_eq!(
            style.nodes.as_deref(),
            Some("SELECT id, (x + 1) AS \"Big X\" FROM t")
        );
        assert_eq!(style.color, name("Big X", "\"Big X\""));
        // A query keeps its own BYs, and words in strings, comments and
        // parentheses are its own.
        let q = "SELECT src, dst FROM e WHERE note <> 'SIZE BY x' -- COLOR BY y\n\
                 AND src IN (SELECT a FROM nodes (1)) GROUP BY src, dst ORDER BY src";
        assert_eq!(parsed(q), (q.into(), Style::default()));
        let (source, style) = parsed(&format!("{q} SIZE BY degree"));
        assert_eq!(source, q);
        assert_eq!(style.size, name("degree", "degree"));
    }

    #[test]
    fn a_clause_that_does_not_fit_says_why() {
        for (text, says) in [
            (
                "karate SIZE BY",
                "SIZE BY needs a numeric column or an algorithm",
            ),
            (
                "karate COLOR BY 'x'",
                "COLOR BY needs a column of categories",
            ),
            ("karate SIZE BY a SIZE BY b", "SIZE BY is given twice"),
            (
                "karate NODES (SELECT 1) NODES (SELECT 2)",
                "NODES is given twice",
            ),
            ("karate NODES (SELECT 1", "NODES ( is not closed"),
            ("karate NODES ( )", "NODES () is empty"),
            (
                "karate SIZE BY pagerank WITH care",
                "unexpected 'WITH' after the graph's source",
            ),
        ] {
            let err = parse(text).unwrap_err().to_string();
            assert!(err.contains(says), "{text}: {err}");
        }
    }

    #[test]
    fn color_by_ranks_categories_and_folds_the_rest_into_other() {
        // Nine categories, the largest first; the ninth is "other".
        let mut values: Vec<Option<&str>> = Vec::new();
        for (c, n) in ["a", "b", "c", "d", "e", "f", "g", "h", "i"]
            .iter()
            .zip([1, 9, 8, 7, 6, 5, 4, 3, 2])
        {
            values.extend(std::iter::repeat_n(Some(*c), n));
        }
        values.push(None);
        let array = Arc::new(StringArray::from(values)) as ArrayRef;
        let c = color_by("letter", &array).unwrap();
        assert_eq!(c.labels, vec!["b", "c", "d", "e", "f", "g", "h", "i"]);
        assert_eq!(c.counts, vec![9, 8, 7, 6, 5, 4, 3, 2]);
        assert_eq!((c.other, c.missing), (1, 1));
        assert_eq!(c.index[0], OTHER, "a, the smallest, is other");
        assert_eq!(c.index[1], 0, "b, the largest, is first");
        assert_eq!(
            *c.index.last().unwrap(),
            OTHER,
            "no value is drawn as other"
        );
        // Too many: numbers that are a measure, or a value per node.
        let floats = Arc::new(Float64Array::from_iter_values((0..20).map(f64::from))) as ArrayRef;
        let err = color_by("score", &floats).unwrap_err().to_string();
        assert!(
            err.contains("20 distinct values over 20 nodes") && err.contains("SIZE BY score"),
            "{err}"
        );
        let names = Arc::new(StringArray::from_iter_values(
            (0..20).map(|i| format!("n{i}")),
        )) as ArrayRef;
        let err = color_by("name", &names).unwrap_err().to_string();
        assert!(
            err.contains("too many categories") && err.contains("fewer values"),
            "{err}"
        );
        // A few distinct floats are categories.
        let few = Arc::new(Float64Array::from(vec![0.5, 1.5, 0.5])) as ArrayRef;
        assert_eq!(color_by("half", &few).unwrap().labels, vec!["0.5", "1.5"]);
    }

    #[test]
    fn size_by_takes_numbers() {
        let ints = Arc::new(Int64Array::from(vec![Some(3), None, Some(1)])) as ArrayRef;
        let s = size_by("n", &ints).unwrap();
        assert_eq!(&s.values[..1], &[3.0]);
        assert!(s.values[1].is_nan());
        assert_eq!(s.missing, 1);
        let text = Arc::new(StringArray::from(vec!["a"])) as ArrayRef;
        let err = size_by("club", &text).unwrap_err().to_string();
        assert!(
            err.contains("SIZE BY club needs numbers, and club is text")
                && err.contains("COLOR BY club"),
            "{err}"
        );
        let none = Arc::new(Int64Array::from(vec![None::<i64>])) as ArrayRef;
        assert!(size_by("n", &none)
            .unwrap_err()
            .to_string()
            .contains("no node has a value"));
    }

    #[test]
    fn a_name_is_one_identifier() {
        assert_eq!(identifier("karate"), Some("karate".into()));
        assert_eq!(identifier(" Karate "), Some("karate".into()));
        assert_eq!(identifier("\"Big Graph\""), Some("Big Graph".into()));
        assert_eq!(identifier("SELECT 1"), None);
        assert_eq!(identifier("1abc"), None);
        assert_eq!(identifier(""), None);
    }
}
