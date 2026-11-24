//! A result as a graph (design §3 and §6, #425): what `.graph` draws, as a
//! [`GraphResult`] any front end can show: the browser shell, and the CLI
//! and the notebook after it.
//!
//! [`of_command`] reads `.graph`'s text, `[SOURCE] [ON graph] [AS …]
//! [NODES (query)] [SIZE BY x] [COLOR BY y] [LAYOUT l] [POSITIONS (query)]`:
//!
//! 1. [`clauses`] splits the text into its source and its [`Style`].
//! 2. A [`Source`] is found: the last result, a query's, a table's rows, or a
//!    property graph.
//! 3. [`rules`] chooses the result → graph rule that draws it (design §3,
//!    #433), by what its columns are: their `ursa.*` metadata, or else their
//!    names and types. Rows are paths, a hop's rings, edges, a pattern's
//!    matches, or nodes; or they are not a graph, and the error says what
//!    would be. A property graph is its edge tables and its vertex tables'
//!    keys, so a node with no edges is drawn too. [`interpret`] is this step
//!    alone, on a result. `AS …` is rule 0 (#434): it names the shape and
//!    its columns, and nothing is inferred. `ON graph` names the graph the
//!    ids belong to, and `LAYOUT` the layout, whatever the rule chose.
//! 4. Ids become dense node indexes `0..n`, in the order they are first
//!    seen, so the same rows always give the same graph.
//! 5. `SIZE BY` and `COLOR BY` each become a node column, which the result's
//!    encodings name.
//! 6. `POSITIONS (query)` gives the result positions ([`positions`], #502):
//!    a front end draws it there and runs no layout.
//!
//! A drawing has at most `ursa.graph_max_edges` edges ([`MAX_EDGES`] by
//! default): more is refused, with a message that names the setting.
//!
//! The result's provenance says the SQL it came from, the rule that drew
//! it, the columns the rule read and what it left out ([`drawn_as`] is the
//! sentence a status line shows). Its highlights are the paths, the seeds
//! and each node's level, for the view to draw. [`GraphView::legend`] is the
//! legend of `COLOR BY`.
//!
//! [`Session`] is the explore session of a drawing (design §4, #440): the
//! commands that change it (`.expand`, `.hide`, `.show`, `.undo`, `.redo`),
//! each as SQL and a [`GraphDelta`] for the view. [`find`] is its search
//! (#445): the nodes that match a text or a condition, for the view to
//! select.
//!
//! This module runs SQL text (a source that is a query, `NODES (…)`, the
//! algorithms of `SIZE BY pagerank`), and the SQL front end is `ursa-sql`,
//! which depends on this crate. So the caller hands in how to run it, as a
//! [`RunSql`]: `ursa_sql::Sql` for Ursa's whole dialect, or [`DataFusionSql`]
//! for DataFusion's alone.

use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::array::{Array, AsArray, RecordBatch};
use datafusion::arrow::compute::{cast, concat_batches};
use datafusion::arrow::datatypes::{DataType, SchemaRef, UInt64Type};
use datafusion::common::TableReference;
use datafusion::dataframe::DataFrame;
use datafusion::datasource::provider_as_source;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::LogicalPlanBuilder;
use datafusion::prelude::SessionContext;
use ursa_viz::model::{
    Column as ModelColumn, ColumnRole, EdgeBatch, Encodings, GraphDelta, GraphResult, Highlights,
    KeyType, NodeBatch, NodeLabel, Op, Provenance, Role, Strings, Target, Values, NO_ROW, NULL,
};

use crate::graph::schema::graph_table;
use crate::graph::{edges_plan, registry_of, GraphDef, PropertyGraph};

mod build;
pub mod clauses;
pub mod find;
pub mod groups;
pub mod inspect;
mod positions;
pub mod rules;
pub mod session;
mod style;

#[cfg(test)]
mod tests;

use build::{build_within, EdgeList, NodeIndex};
pub use clauses::{identifier, parse, As, Name, Style, USAGE};
pub use groups::Grouping;
pub use rules::Rule;
use rules::{MatchEdge, Shape};
pub use session::{Command, Outcome, Session, Step};
use style::{
    color_by, degree_column, resolve, run_algorithms, size_by, vertex_column, ColorBy, Found,
    NodeValues,
};
/// A place per node: what [`GraphView::positions`] holds.
pub use ursa_viz::model::Positions;

/// The most edges a view draws unless `ursa.graph_max_edges` says otherwise
/// (#502): what a desktop browser draws within its frame budget, and 40 MB
/// of node indexes to hand it. A front end on a phone sets less.
pub const MAX_EDGES: usize = 5_000_000;
/// The most nodes, for a property graph whose vertex tables are large.
pub const MAX_NODES: usize = 4_000_000;
/// The most categories `COLOR BY` colours; the rest are drawn as "other". A
/// front end's palette has as many colours, then other's.
pub const MAX_CATEGORIES: usize = 8;

/// What the colour column is called when `SIZE BY` and `COLOR BY` name the
/// same thing: the name, then this. A result's node columns have distinct
/// names, and the size column has the name itself. [`Legend::by`] is the
/// name without it.
pub const COLOR_SUFFIX: &str = " (colour)";
/// What the size column is called when a column of the rule's (a hop's
/// `distance`, a pattern's property) already has `SIZE BY`'s name.
pub const SIZE_SUFFIX: &str = " (size)";

pub(crate) fn plan_err<T>(message: impl Into<String>) -> Result<T> {
    Err(DataFusionError::Plan(message.into()))
}

/// A result's schema and its rows.
pub type Rows = (SchemaRef, Vec<RecordBatch>);

/// How SQL text is run on a session: every statement, giving the rows of the
/// last.
#[async_trait]
pub trait RunSql: Send + Sync {
    async fn run(&self, ctx: &SessionContext, text: &str) -> Result<Rows>;
}

/// [`RunSql`] by `SessionContext::sql`: DataFusion's dialect and one
/// statement, without Ursa's own (`GRAPH_TABLE`, `FROM`-first queries,
/// `CREATE PROPERTY GRAPH`). A front end with `ursa-sql` uses `ursa_sql::Sql`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DataFusionSql;

#[async_trait]
impl RunSql for DataFusionSql {
    async fn run(&self, ctx: &SessionContext, text: &str) -> Result<Rows> {
        let frame = ctx.sql(text).await?;
        let schema = Arc::new(frame.schema().as_arrow().clone());
        Ok((schema, frame.collect().await?))
    }
}

/// `n` with thousands separators: 2,000,000.
pub(crate) fn thousands(n: usize) -> String {
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

/// The `SET` that lets a drawing of `edges` edges through: the limit
/// raised to the next million.
pub(crate) fn raise_max_edges(edges: usize) -> String {
    let limit = edges.div_ceil(1_000_000).max(1) * 1_000_000;
    format!("SET ursa.graph_max_edges = {limit}")
}

/// What `.graph` draws: a property graph, or rows.
pub enum Source {
    Graph(Arc<PropertyGraph>),
    Rows {
        schema: SchemaRef,
        batches: Vec<RecordBatch>,
        /// The SQL the rows are the result of, if known, for the provenance.
        sql: Option<String>,
    },
}

impl Source {
    /// What `.graph TEXT` names: the property graph TEXT names, a table's
    /// rows, or a query's result (its last statement's).
    pub async fn of_text(ctx: &SessionContext, sql: &dyn RunSql, text: &str) -> Result<Source> {
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
            return Ok(Source::Rows {
                schema,
                batches: frame.collect().await?,
                sql: Some(format!("SELECT * FROM {}", text.trim())),
            });
        }
        let (schema, batches) = sql.run(ctx, text).await?;
        Ok(Source::Rows {
            schema,
            batches,
            sql: Some(text.trim().to_string()),
        })
    }

    /// Rows whose SQL is not known: the shell's last result.
    pub fn of_rows((schema, batches): Rows) -> Source {
        Source::Rows {
            schema,
            batches,
            sql: None,
        }
    }
}

/// A graph to draw, and what was left out of it.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphView {
    /// The nodes, the edges, and the columns the rule and `SIZE BY` and
    /// `COLOR BY` made, which its encodings name. A property graph's nodes
    /// and a plain table's have the one label `""`, and their edges' rows
    /// are not known; a rule that reads metadata names the vertex and edge
    /// tables, and gives the rows it knows.
    pub result: GraphResult,
    /// Rows left out because an endpoint is NULL.
    pub skipped: usize,
    /// Where `POSITIONS (query)` puts each node (#502), one place per node
    /// of `result`: a front end draws them there and runs no layout. They
    /// are beside the result, not in it, because an explore session's deltas
    /// are of a result without positions: a view keeps where its nodes are,
    /// and moves them as the drawing grows. [`GraphView::placed`] is the
    /// result with them, for a front end that only shows it.
    pub positions: Option<Positions>,
    /// With `GROUP BY` (#499): which of the drawing's nodes each group of
    /// `result` has. They are not in the result until a group is opened,
    /// which an explore session does with this ([`Session::grouped`]).
    pub groups: Option<Grouping>,
}

impl GraphView {
    /// The result with `POSITIONS`'s positions in it (UGR's `nodes.x` and
    /// `nodes.y`): a drawing that opens drawn. The result as it is when
    /// there are none.
    pub fn placed(&self) -> GraphResult {
        let mut result = self.result.clone();
        if let Some(positions) = &self.positions {
            let op = result.set_positions_op(Some(positions.clone()));
            result
                .apply(&GraphDelta::new(vec![op]))
                .expect("a place per node");
        }
        result
    }

    /// The legend of `COLOR BY`, if it was asked.
    pub fn legend(&self) -> Option<Legend> {
        Legend::of(&self.result)
    }

    /// The legend of `GROUP BY`, if it was asked: `grouped by louvain: 12
    /// groups`.
    pub fn grouped_by(&self) -> Option<String> {
        let groups = self.groups.as_ref()?;
        groups::legend(&self.result, groups.by())
    }

    /// What the result was drawn as, for a status line (see [`drawn_as`]).
    pub fn drawn_as(&self) -> String {
        drawn_as(&self.result)
    }
}

/// What `result` was drawn as, by its provenance: the rule, the graph, the
/// columns read, and the rule's notes. `a path on karate (members, ties)`;
/// `nodes of karate (id, pagerank, community)`; `edges (src, dst)`; `the
/// graph karate`. A status line says `drawn as` and then this. Empty when
/// the result names no rule. (The page's `graph-data.js` says the same.)
pub fn drawn_as(result: &GraphResult) -> String {
    let provenance = result.provenance();
    let Some(rule) = provenance.rule.as_deref() else {
        return String::new();
    };
    let paths = result.highlights().paths.len();
    let (what, joins) = match rule {
        "graph" => ("the graph".to_string(), " "),
        "edges" => ("edges".to_string(), " of "),
        "path" if paths > 1 => (format!("{} paths", thousands(paths)), " on "),
        "path" => ("a path".to_string(), " on "),
        "rings" => ("rings".to_string(), " on "),
        "pattern" => ("pattern matches".to_string(), " on "),
        "nodes" => ("nodes".to_string(), " of "),
        groups::RULE => ("groups".to_string(), " of "),
        other => (other.to_string(), " on "),
    };
    let mut out = what;
    if let Some(graph) = result.graph() {
        out.push_str(joins);
        out.push_str(graph);
    }
    if !provenance.roles.is_empty() {
        let columns: Vec<&str> = provenance.roles.iter().map(|r| r.column.as_str()).collect();
        out.push_str(&format!(" ({})", columns.join(", ")));
    }
    for note in &provenance.notes {
        out.push_str(" · ");
        out.push_str(note);
    }
    out
}

/// `COLOR BY`'s legend: the categories coloured, the largest first, and the
/// nodes that are in none of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legend {
    /// What the nodes are coloured by, as typed.
    pub by: String,
    /// The categories coloured, at most [`MAX_CATEGORIES`], by colour index:
    /// their values as the shell prints them.
    pub labels: Vec<String>,
    /// How many nodes each has.
    pub counts: Vec<u32>,
    /// Nodes in the categories past the coloured ones.
    pub other: u32,
    /// Nodes with no value (NULL, or none given).
    pub missing: u32,
}

impl Legend {
    /// The legend of the column `result`'s encodings colour by: a dictionary
    /// column whose values are in order of rank, as [`of_command`] writes it.
    pub fn of(result: &GraphResult) -> Option<Legend> {
        let name = result.encodings().color_by.as_deref()?;
        let Values::Dict { values, codes } = &result.node_column(name)?.values else {
            return None;
        };
        let by = match name.strip_suffix(COLOR_SUFFIX) {
            Some(by) if result.node_column(by).is_some() => by,
            _ => name,
        };
        let top = values.len().min(MAX_CATEGORIES);
        let mut legend = Legend {
            by: by.to_string(),
            labels: values.iter().take(top).map(str::to_string).collect(),
            counts: vec![0; top],
            other: 0,
            missing: 0,
        };
        for &code in codes {
            match code {
                NULL => legend.missing += 1,
                c if (c as usize) < top => legend.counts[c as usize] += 1,
                _ => legend.other += 1,
            }
        }
        Some(legend)
    }
}

/// What `.graph TEXT` draws: `[SOURCE] [NODES (query)] [SIZE BY x]
/// [COLOR BY y]`. With no source, it is `last`: the shell's last result.
pub async fn of_command(
    ctx: &SessionContext,
    sql: &dyn RunSql,
    text: &str,
    last: Option<Rows>,
) -> Result<GraphView> {
    let (text, style) = parse(text)?;
    crate::probe::mark("start");
    let source = if !text.is_empty() {
        Source::of_text(ctx, sql, &text).await?
    } else {
        let Some(rows) = last else {
            return plan_err(
                "no result to draw yet: run a query whose rows are edges (columns src and dst, \
                 or else the first two), then .graph; or .graph NAME for a property graph",
            );
        };
        Source::of_rows(rows)
    };
    crate::probe::mark("source");
    draw(ctx, sql, source, &style).await
}

/// The graph `source` draws, styled as `style` says (`SIZE BY`, `COLOR BY`,
/// `NODES`).
pub async fn draw(
    ctx: &SessionContext,
    sql: &dyn RunSql,
    source: Source,
    style: &Style,
) -> Result<GraphView> {
    if let (Some(_), Some(layout)) = (&style.positions, style.layout) {
        return plan_err(format!(
            "POSITIONS (…) and LAYOUT {} both say where the nodes go; give one. POSITIONS \
             draws the nodes where its query puts them, and no layout runs",
            layout_name(layout)
        ));
    }
    let max_edges = crate::options::graph_max_edges(ctx.state().config());
    let (mut drawing, text) = match source {
        Source::Graph(g) => {
            // A property graph is drawn whole: there is nothing to read.
            if let Some(shape) = &style.shape {
                return plan_err(format!(
                    "{} says how to read a result's rows, and '{}' is a property graph, drawn \
                     whole. Leave the clause out (.graph {}), or draw a query's rows: .graph \
                     SELECT … FROM … {}",
                    shape.shown(),
                    g.name,
                    g.name,
                    shape.shown()
                ));
            }
            if let Some(on) = &style.on {
                return plan_err(format!(
                    "ON {} names the graph a result's ids belong to, and '{}' is a property graph \
                     already. Leave the clause out (.graph {}), or draw a query's rows: .graph \
                     SELECT … FROM … ON {}",
                    on.shown, g.name, g.name, on.shown
                ));
            }
            let (edges, index) = property_graph_indexed(ctx, &g, max_edges).await?;
            (Drawing::of_graph(edges, index, g), None)
        }
        Source::Rows {
            schema,
            batches,
            sql: text,
        } => (
            read_rows(ctx, sql, schema, batches, style, max_edges).await?,
            text,
        ),
    };
    if let Some(by) = &style.group {
        return group(ctx, sql, drawing, style, by, text).await;
    }
    crate::probe::mark("drawing");
    apply_style(ctx, sql, &mut drawing, style).await?;
    crate::probe::mark("style");
    if let Some(layout) = style.layout {
        set_layout(&mut drawing, layout)?;
    }
    let saved = match &style.positions {
        Some(query) => {
            let saved = positions::saved(ctx, sql, query, &drawing.edges, &drawing.index).await?;
            // The nodes are where the query puts them, whatever layout the
            // rule would have asked for.
            drawing.edges.layout = ursa_viz::model::Layout::Force;
            drawing.notes.extend(saved.notes);
            Some(saved.positions)
        }
        None => None,
    };
    let provenance = Provenance {
        sql: text,
        rule: Some(drawing.rule.name().to_string()),
        roles: drawing
            .roles
            .iter()
            .map(|(column, role)| ColumnRole::new(column, role))
            .collect(),
        notes: std::mem::take(&mut drawing.notes),
    };
    let graph = drawing.graph.as_ref().map(|g| g.name.clone());
    let mut view = to_view(drawing.edges, &drawing.index.key_type, graph, provenance)?;
    view.positions = saved;
    crate::probe::mark("to_view");
    Ok(view)
}

/// `GROUP BY by` on `drawing` ([`groups`], #499): its nodes grouped by a
/// column of `NODES (query)`, of the result's own rows or of the graph's
/// vertex tables, by degree, or by an algorithm, found as `COLOR BY` finds
/// its values.
async fn group(
    ctx: &SessionContext,
    sql: &dyn RunSql,
    mut drawing: Drawing,
    style: &Style,
    by: &Name,
    text: Option<String>,
) -> Result<GraphView> {
    let shown = &by.shown;
    for (clause, given) in [
        ("SIZE BY", style.size.as_ref().map(|n| n.shown.clone())),
        ("COLOR BY", style.color.as_ref().map(|n| n.shown.clone())),
        (
            "LAYOUT",
            style.layout.map(|layout| layout_name(layout).to_string()),
        ),
        (
            "POSITIONS",
            style.positions.as_ref().map(|_| "(…)".to_string()),
        ),
    ] {
        if let Some(given) = given {
            let why = match clause {
                "SIZE BY" => "a group's disc is sized by how many members it has",
                "COLOR BY" => "a group's disc takes its group's colour",
                _ => "the groups are laid out by the edges between them",
            };
            return plan_err(format!(
                "GROUP BY {shown} draws a disc for each group, and {clause} {given} is of the \
                 nodes: {why}. Leave {clause} out, or GROUP BY"
            ));
        }
    }
    let graph = drawing.graph.clone();
    let graph = graph.as_deref();
    let nodes = match &style.nodes {
        Some(query) => Some(NodeValues::of_query(ctx, sql, query).await?),
        None => None,
    };
    let own = drawing.own.as_ref();
    let found = resolve(ctx, graph, nodes.as_ref(), own, "GROUP BY", by).await?;
    let column = match found {
        Found::Nodes(i) => nodes.as_ref().expect("NODES was given").column(i),
        Found::Own(i) => own.expect("the result's rows").column(i),
        Found::Vertex => vertex_column(ctx, graph.expect("a graph"), &by.ident).await?,
        Found::Algorithm(algorithm) => {
            let graph = graph.expect("an algorithm has a graph");
            run_algorithms(ctx, sql, graph, &[algorithm])
                .await?
                .column_named(algorithm)
        }
        Found::Degree => degree_column(&drawing.edges),
    };
    let values = column.per_node(&drawing.index, drawing.edges.n as usize)?;
    let provenance = Provenance {
        sql: text,
        rule: None,
        roles: drawing
            .roles
            .iter()
            .map(|(column, role)| ColumnRole::new(column, role))
            .collect(),
        notes: std::mem::take(&mut drawing.notes),
    };
    groups::grouped(
        drawing.edges,
        &drawing.index.key_type,
        graph.map(|g| g.name.clone()),
        provenance,
        shown,
        &values,
    )
}

fn layout_name(layout: ursa_viz::model::Layout) -> &'static str {
    use ursa_viz::model::Layout;
    match layout {
        Layout::Force => "force",
        Layout::Rings => "rings",
        Layout::Layers => "layers",
    }
}

/// What the result → graph rules (design §3, [`rules`]) make of a result:
/// its rows as a graph, drawn by the first rule that matches, with that
/// rule's defaults. This is `.graph` after a query, with no clauses.
///
/// Rules 2 and 5 read the graph's edges among the result's nodes
/// (`induced_edges`), from the graph the columns say they are from
/// (`ursa.graph`), or else the session's only property graph.
pub async fn interpret(
    ctx: &SessionContext,
    sql: &dyn RunSql,
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
) -> Result<GraphView> {
    interpret_with(ctx, sql, schema, batches, &Style::default()).await
}

/// [`interpret`] with `.graph`'s clauses (`style`): `AS …` in place of the
/// rules (rule 0), `ON graph` for the graph the ids belong to, `LAYOUT`,
/// and `SIZE BY`, `COLOR BY` and `NODES`.
pub async fn interpret_with(
    ctx: &SessionContext,
    sql: &dyn RunSql,
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    style: &Style,
) -> Result<GraphView> {
    let source = Source::of_rows((schema, batches));
    draw(ctx, sql, source, style).await
}

/// `LAYOUT`: the layout of `drawing`, whatever its rule chose. Rings and
/// layers place each node by its level (a hop's distance, a path's step), so
/// a drawing without one cannot have them.
fn set_layout(drawing: &mut Drawing, layout: ursa_viz::model::Layout) -> Result<()> {
    use ursa_viz::model::Layout;
    let name = layout_name(layout);
    if layout != Layout::Force && drawing.edges.level.is_none() {
        let what = match drawing.rule {
            Rule::Graph => "a property graph",
            Rule::Edges => "edges",
            Rule::Matches => "pattern matches",
            Rule::Nodes => "nodes",
            Rule::Path | Rule::Rings => "this drawing",
        };
        return plan_err(format!(
            "LAYOUT {name} places each node by its distance from a seed or its step along a path, \
             and {what} {} neither. Draw a hop result or paths (.graph SELECT * FROM hop(g, seeds \
             => [1], steps => 2) LAYOUT {name}), say which columns are one (AS hops(seed, id, \
             distance)), or use LAYOUT force",
            if what == "this drawing" || what == "a property graph" {
                "has"
            } else {
                "have"
            }
        ));
    }
    drawing.edges.layout = layout;
    Ok(())
}

/// A drawing before its style: the nodes and edges a rule read, and what
/// `SIZE BY` and `COLOR BY` may use.
struct Drawing {
    edges: EdgeList,
    index: NodeIndex,
    rule: Rule,
    /// The graph the nodes belong to, if known: what an algorithm runs on.
    graph: Option<Arc<PropertyGraph>>,
    /// The result's own rows, when each is a node (rule 5).
    own: Option<NodeValues>,
    /// The rule's defaults for `SIZE BY` and `COLOR BY`.
    size: Option<Name>,
    color: Option<Name>,
    roles: Vec<(String, String)>,
    notes: Vec<String>,
}

impl Drawing {
    fn of_graph(edges: EdgeList, index: NodeIndex, graph: Arc<PropertyGraph>) -> Drawing {
        Drawing {
            edges,
            index,
            rule: Rule::Graph,
            graph: Some(graph),
            own: None,
            size: None,
            color: None,
            roles: Vec::new(),
            notes: Vec::new(),
        }
    }
}

/// The graph a result's nodes belong to: the one `ON graph` names (`on`),
/// else the one its columns name, or else (`or_the_only`) the session's only
/// property graph.
fn graph_of(
    ctx: &SessionContext,
    reading: &rules::Reading,
    on: Option<&Name>,
    or_the_only: bool,
    notes: &mut Vec<String>,
) -> Result<Option<Arc<PropertyGraph>>> {
    let registry = registry_of(ctx);
    if let Some(on) = on {
        let Some(found) = registry.get(&on.ident) else {
            let mut names: Vec<String> = registry.all().iter().map(|g| g.name.clone()).collect();
            names.sort();
            return plan_err(format!(
                "ON {}: there is no property graph named '{}'. {}",
                on.shown,
                on.ident,
                if names.is_empty() {
                    "This session has none: make one with CREATE PROPERTY GRAPH, or leave the \
                     clause out"
                        .to_string()
                } else {
                    format!("The graphs are {}; name one of them", names.join(", "))
                }
            ));
        };
        if let Some(theirs) = reading.graph.as_deref().filter(|g| *g != found.name) {
            notes.push(format!(
                "read on {} as ON says; the columns are from {theirs}",
                found.name
            ));
        }
        return Ok(Some(found));
    }
    Ok(graph_of_columns(&registry, reading, or_the_only, notes))
}

fn graph_of_columns(
    registry: &crate::graph::GraphRegistry,
    reading: &rules::Reading,
    or_the_only: bool,
    notes: &mut Vec<String>,
) -> Option<Arc<PropertyGraph>> {
    if let Some(name) = &reading.graph {
        let found = registry.get(name);
        if found.is_none() {
            notes.push(format!("the graph '{name}' is not in this session"));
        }
        return found;
    }
    let mut all = registry.all();
    match (or_the_only, all.len()) {
        (true, 1) => all.pop(),
        _ => None,
    }
}

/// The drawing of a result's rows, by the rule that matches them.
async fn read_rows(
    ctx: &SessionContext,
    sql: &dyn RunSql,
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    style: &Style,
    max_edges: usize,
) -> Result<Drawing> {
    // Rule 0: what `AS …` says; else the first rule that matches.
    let reading = match &style.shape {
        Some(shape) => rules::read_as(&schema, shape, style.on.is_some())?,
        None => rules::read(&schema)?,
    };
    let mut notes = reading.notes.clone();
    let name = |column: usize| schema.field(column).name().as_str();
    // One batch, for the rules that read rows by position.
    let whole = || concat_batches(&schema, &batches);
    let label_names = |keys: &[usize]| -> (Vec<String>, Vec<u16>) {
        let mut labels: Vec<String> = Vec::new();
        let of_key = keys
            .iter()
            .map(|&k| {
                let label = reading.label_of(k).unwrap_or("");
                let at = labels.iter().position(|l| l == label).unwrap_or_else(|| {
                    labels.push(label.to_string());
                    labels.len() - 1
                });
                at as u16
            })
            .collect();
        (labels, of_key)
    };
    let mut own = None;
    let (mut size, mut color) = (None, None);
    let mut graph = graph_of(
        ctx,
        &reading,
        style.on.as_ref(),
        matches!(reading.shape, Shape::Hop { .. } | Shape::Nodes { .. }),
        &mut notes,
    )?;
    // The session's only graph is a guess until its edges are read: ids
    // that are not its own are of no graph.
    let guessed = reading.graph.is_none() && style.on.is_none();
    // The label of each key column, for the nodes' properties.
    let mut key_labels: Vec<(usize, u16)> = Vec::new();
    let (mut edges, index) = match &reading.shape {
        Shape::Edges { src, dst } => {
            let (labels, of_key) = label_names(&[*src, *dst]);
            if labels.len() > 1 {
                // Two vertex tables: a node is its table and its key.
                let batch = whole()?;
                key_labels = vec![(*src, of_key[0]), (*dst, of_key[1])];
                let edge = MatchEdge {
                    from: 0,
                    to: 1,
                    directed: true,
                    label: reading.edge_label.clone().unwrap_or_default(),
                };
                build::matches_of(&batch, &key_labels, &labels, &[edge], max_edges)?
            } else {
                let (mut edges, index) = build::edges_of(&batches, *src, *dst, max_edges)?;
                if reading.marked {
                    edges.labels = labels;
                    edges.edge_labels = reading.edge_label.iter().cloned().collect();
                }
                if !reading.attributes.is_empty() {
                    let batch = whole()?;
                    let (s, d) = (batch.column(*src), batch.column(*dst));
                    let kept: Vec<usize> = (0..batch.num_rows())
                        .filter(|&i| s.is_valid(i) && d.is_valid(i))
                        .collect();
                    for &a in &reading.attributes {
                        let role = meta_role(&schema, a);
                        edges.edge_columns.push(build::edge_attribute(
                            name(a),
                            role,
                            batch.column(a),
                            &kept,
                        )?);
                    }
                }
                (edges, index)
            }
        }
        Shape::Path {
            nodes,
            edges,
            elements,
        } => {
            let (mut drawn, index) =
                build::paths_of(&whole()?, *nodes, *edges, *elements, &reading, max_edges)?;
            let anchor = nodes.or(*elements).expect("a path column");
            if reading.label_of(anchor).is_some() {
                drawn.labels = label_names(&[anchor]).0;
            }
            (drawn, index)
        }
        Shape::Steps { src, dst, step } => {
            let batch = whole()?;
            let (mut edges, index, kept) = build::steps_of(&batch, *src, *dst, *step)?;
            let (labels, _) = label_names(&[*src, *dst]);
            if labels.len() == 1 {
                edges.labels = labels;
            }
            for &a in &reading.attributes {
                let role = meta_role(&schema, a);
                edges.edge_columns.push(build::edge_attribute(
                    name(a),
                    role,
                    batch.column(a),
                    &kept,
                )?);
            }
            (edges, index)
        }
        Shape::Hop { seed, id, distance } => {
            let batch = whole()?;
            let (mut edges, index, rows) = build::hop_of(&batch, *seed, *id, *distance)?;
            if reading.label_of(*id).is_some() {
                edges.labels = label_names(&[*id]).0;
            }
            let read = match &graph {
                Some(g) => induced(ctx, sql, g, &mut edges, &index, max_edges).await,
                None => Err("there is no property graph to read the edges from".to_string()),
            };
            if let Err(why) = read {
                if guessed {
                    graph = None;
                }
                // The rows' own edges: each seed's to the nodes one step away.
                for i in 0..rows.reached.len() {
                    if rows.distance[i] == 1 && rows.seeds[i] != NULL && rows.reached[i] != NULL {
                        edges.src.push(rows.seeds[i]);
                        edges.dst.push(rows.reached[i]);
                    }
                }
                notes.push(format!(
                    "{why}: only each seed's edges to the nodes one step away are drawn"
                ));
            }
            (edges, index)
        }
        Shape::Matches { keys, edges } => {
            let (labels, of_key) = label_names(keys);
            key_labels = keys.iter().copied().zip(of_key).collect();
            build::matches_of(&whole()?, &key_labels, &labels, edges, max_edges)?
        }
        Shape::Nodes { id } => {
            let batch = whole()?;
            let (mut edges, index) = build::nodes_of(&batch, *id)?;
            if reading.label_of(*id).is_some() {
                edges.labels = label_names(&[*id]).0;
            }
            let read = match &graph {
                Some(g) => induced(ctx, sql, g, &mut edges, &index, max_edges).await,
                None => Err("there is no property graph to read the edges from".to_string()),
            };
            if let Err(why) = read {
                if guessed {
                    graph = None;
                }
                notes.push(format!("{why}: the nodes are drawn without edges"));
            }
            let named = |column: usize| Name {
                ident: name(column).to_string(),
                shown: name(column).to_string(),
            };
            size = reading.size.map(named);
            color = reading.color.map(named);
            own = Some(NodeValues::of_rows(
                Arc::clone(&schema),
                batches.clone(),
                *id,
            ));
            (edges, index)
        }
    };
    if key_labels.is_empty() {
        if let Shape::Edges { src, dst } = &reading.shape {
            key_labels = vec![(*src, 0), (*dst, 0)];
        }
    }
    // The properties of the pattern's variables, on their nodes.
    if !reading.properties.is_empty() {
        let batch = whole()?;
        let several = edges.labels.len() > 1;
        for &(column, key) in &reading.properties {
            let label = key_labels
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, l)| *l)
                .filter(|_| several);
            let values = style::Column::of_keys(
                Arc::clone(batch.column(key)),
                Arc::clone(batch.column(column)),
                label,
            )
            .per_node(&index, edges.n as usize)?;
            let taken = |n: &str| edges.node_columns.iter().any(|c| c.name == n);
            if !taken(name(column)) {
                edges
                    .node_columns
                    .push(style::property_column(name(column), &values)?);
            }
        }
    }
    Ok(Drawing {
        edges,
        index,
        rule: reading.shape.rule(),
        graph,
        own,
        size,
        color,
        roles: reading.roles,
        notes,
    })
}

/// The model's role of column `column`, if its metadata says one the model
/// has.
fn meta_role(schema: &SchemaRef, column: usize) -> Option<Role> {
    use crate::graph::provenance as meta;
    match schema.field(column).metadata().get(meta::ROLE)?.as_str() {
        meta::ROLE_METRIC => Some(Role::Metric),
        meta::ROLE_CATEGORY => Some(Role::Category),
        meta::ROLE_PROPERTY => Some(Role::Property),
        meta::ROLE_DISTANCE => Some(Role::Distance),
        _ => None,
    }
}

/// Add `graph`'s edges among the nodes of `edges` (`induced_edges`), each
/// with its row in the edge table. `Err` says why they could not be read.
async fn induced(
    ctx: &SessionContext,
    sql: &dyn RunSql,
    graph: &PropertyGraph,
    edges: &mut EdgeList,
    index: &NodeIndex,
    max_edges: usize,
) -> std::result::Result<(), String> {
    use crate::graph::provenance as meta;
    let quoted = !index.key_type.is_integer();
    let mut list = String::with_capacity(edges.ids.len() * 8);
    for (i, id) in edges.ids.iter().enumerate() {
        if i > 0 {
            list.push(',');
        }
        if quoted {
            list.push('\'');
            list.push_str(&id.replace('\'', "''"));
            list.push('\'');
        } else {
            list.push_str(id);
        }
    }
    let query = format!(
        "SELECT src, dst, edge FROM induced_edges(\"{}\", nodes => [{list}]) LIMIT {}",
        graph.name.replace('"', "\"\""),
        max_edges.saturating_add(1)
    );
    drop(list);
    let (schema, batches) = sql.run(ctx, &query).await.map_err(|e| {
        format!(
            "the edges of '{}' could not be read ({})",
            graph.name,
            crate::user_message(&e)
        )
    })?;
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    if rows > max_edges {
        return Err(format!(
            "there are more than {} edges among the nodes, the most the graph view draws \
             (ursa.graph_max_edges)",
            thousands(max_edges)
        ));
    }
    let fail = |e: DataFusionError| crate::user_message(&e);
    let mut edge_rows = Vec::with_capacity(rows);
    for b in &batches {
        let (s, d) = (
            index.lookup(b.column(0)).map_err(fail)?,
            index.lookup(b.column(1)).map_err(fail)?,
        );
        let row = cast(b.column(2), &DataType::UInt64).map_err(|e| e.to_string())?;
        let row = row.as_primitive::<UInt64Type>();
        for i in 0..b.num_rows() {
            if let (Some(s), Some(d)) = (s[i], d[i]) {
                edges.src.push(s);
                edges.dst.push(d);
                edge_rows.push(if row.is_valid(i) {
                    row.value(i)
                } else {
                    NO_ROW
                });
            }
        }
    }
    if edge_rows.iter().any(|&r| r != NO_ROW) {
        edges.edge_rows = Some(edge_rows);
    }
    if let Some(label) = schema.field(2).metadata().get(meta::LABEL) {
        edges.edge_labels = vec![label.clone()];
    }
    Ok(())
}

/// `SIZE BY` and `COLOR BY` on `drawing`: what `style` asks, or else the
/// rule's defaults.
async fn apply_style(
    ctx: &SessionContext,
    sql: &dyn RunSql,
    drawing: &mut Drawing,
    style: &Style,
) -> Result<()> {
    if style.size.is_none() && style.color.is_none() && style.nodes.is_some() {
        return plan_err(
            "NODES (…) gives values for SIZE BY or COLOR BY to use; add one, as in \
             .graph … NODES (SELECT id, score FROM …) SIZE BY score",
        );
    }
    let graph = drawing.graph.clone();
    let graph = graph.as_deref();
    let nodes = match &style.nodes {
        Some(query) => Some(NodeValues::of_query(ctx, sql, query).await?),
        None => None,
    };
    let own = drawing.own.as_ref();
    // Where each name's values come from; the algorithms run together. A
    // default that cannot be drawn is left out; what was asked for is an
    // error.
    let mut found = Vec::new();
    for (clause, asked, default) in [
        ("SIZE BY", &style.size, &drawing.size),
        ("COLOR BY", &style.color, &drawing.color),
    ] {
        match (asked, default) {
            (Some(name), _) => found.push((
                clause,
                name.clone(),
                resolve(ctx, graph, nodes.as_ref(), own, clause, name).await?,
                true,
            )),
            (None, Some(name)) => {
                if let Ok(f) = resolve(ctx, None, None, own, clause, name).await {
                    found.push((clause, name.clone(), f, false));
                }
            }
            (None, None) => {}
        }
    }
    let mut algorithms: Vec<&'static str> = Vec::new();
    for (_, _, f, _) in &found {
        if let Found::Algorithm(name) = f {
            if !algorithms.contains(name) {
                algorithms.push(name);
            }
        }
    }
    let computed = match (graph, algorithms.is_empty()) {
        (Some(g), false) => Some(run_algorithms(ctx, sql, g, &algorithms).await?),
        _ => None,
    };
    let edges = &mut drawing.edges;
    for (clause, name, f, asked) in found {
        let column = match f {
            Found::Nodes(i) => nodes.as_ref().expect("NODES was given").column(i),
            Found::Own(i) => own.expect("the result's rows").column(i),
            Found::Vertex => vertex_column(ctx, graph.expect("a graph"), &name.ident).await?,
            Found::Algorithm(algorithm) => computed.as_ref().expect("ran").column_named(algorithm),
            Found::Degree => degree_column(edges),
        };
        let values = column.per_node(&drawing.index, edges.n as usize)?;
        if clause == "SIZE BY" {
            match size_by(&name.shown, &values) {
                Ok(size) => edges.size = Some(size),
                Err(e) if asked => return Err(e),
                Err(_) => {}
            }
        } else {
            match color_by(&name.shown, &values) {
                Ok(color) => edges.color = Some(color),
                Err(e) if asked => return Err(e),
                Err(_) => {}
            }
        }
    }
    if edges.color.is_none() {
        edges.color = default_color(edges);
    }
    Ok(())
}

/// What colours the nodes when nothing else does: a hop's distances, or the
/// vertex tables when there are several.
fn default_color(edges: &EdgeList) -> Option<ColorBy> {
    // Each node's group, and the groups' names, in their own order (not by
    // size: distance 0, then 1, then 2).
    let (by, names, codes): (&str, Vec<String>, Vec<u32>) = if edges.labels.len() > 1 {
        (
            LABEL,
            edges.labels.clone(),
            edges.node_label.iter().map(|&l| u32::from(l)).collect(),
        )
    } else if edges.level.as_deref() == Some(build::DISTANCE) {
        let column = edges
            .node_columns
            .iter()
            .find(|c| c.name == build::DISTANCE)?;
        let Values::U32(level) = &column.values else {
            return None;
        };
        let furthest = level.iter().filter(|&&d| d != NULL).max()?;
        (
            build::DISTANCE,
            (0..=*furthest).map(|d| d.to_string()).collect(),
            level.clone(),
        )
    } else {
        return None;
    };
    let mut counts = vec![0u32; names.len()];
    let mut missing = 0;
    for &c in &codes {
        match counts.get_mut(c as usize) {
            Some(n) => *n += 1,
            None => missing += 1,
        }
    }
    Some(ColorBy {
        by: by.to_string(),
        values: names,
        counts,
        codes,
        missing,
    })
}

/// What the nodes are coloured by when they are of several vertex tables.
pub const LABEL: &str = "label";

/// The edges of `graph`: every edge table's source and destination columns,
/// and every vertex table's keys, so nodes with no edges are drawn too.
async fn property_graph_indexed(
    ctx: &SessionContext,
    graph: &PropertyGraph,
    max_edges: usize,
) -> Result<(EdgeList, NodeIndex)> {
    if let Some(drawn) = from_index(ctx, graph, max_edges).await {
        return Ok(drawn);
    }
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
    build_within(&vertices, &edges, max_edges)
}

/// [`property_graph_indexed`] from the session's index of `graph` (the
/// topology and the id map its algorithms run over, `crate::graph::index`)
/// and not from its rows (#525): with the index cached, as it is once any
/// algorithm has run, nothing is scanned and no id is hashed. The drawing
/// is the one the rows give, node for node and edge for edge.
///
/// For a graph that is one edge table and no vertex table, whose endpoint
/// columns have one type: its index numbers the nodes as they are first
/// seen in the rows, source before destination, and its edges by their
/// rows, as [`build_within`] does. `None` for any other graph, and
/// whenever the index cannot say what the rows would (it dropped a row, an
/// endpoint is NULL, the graph is beyond `max_edges`, or it could not be
/// planned or built): the rows are then read as ever, and say why.
async fn from_index(
    ctx: &SessionContext,
    graph: &PropertyGraph,
    max_edges: usize,
) -> Option<(EdgeList, NodeIndex)> {
    let [e] = &graph.edge_tables[..] else {
        return None;
    };
    if !graph.vertex_tables.is_empty() {
        return None;
    }
    let state = ctx.state();
    let def = GraphDef {
        name: graph.name.clone(),
        edges: e.clone(),
        vertices: None,
    };
    // The type the rows' ids are read as: the endpoint columns' own.
    let plan = edges_plan(&state, &def).await.ok()?;
    let schema = plan.schema();
    let type_of = |column: &str| {
        let i = schema.index_of_column_by_name(None, column)?;
        Some(schema.field(i).data_type().clone())
    };
    let key_type = type_of(&e.source)?;
    if type_of(&e.destination)? != key_type || !build::index_key(&key_type) {
        return None;
    }
    // Any algorithm over the graph has its index under it.
    let query = format!(
        "SELECT id FROM degree(\"{}\")",
        graph.name.replace('"', "\"\"")
    );
    crate::probe::mark("fi:edges_plan");
    let physical = ctx
        .sql(&query)
        .await
        .ok()?
        .create_physical_plan()
        .await
        .ok()?;
    crate::probe::mark("fi:planned");
    let index = crate::graph::index_under(&physical, &ctx.task_ctx())
        .await
        .ok()??;
    crate::probe::mark("fi:index");
    let edges = index.topology.n_edges();
    if index.n_dropped() > 0 || edges == 0 || edges > max_edges {
        return None;
    }
    build::of_index(&index.topology, &index.ids.user_id_array(), key_type).ok()?
}

/// The columns `columns` of one of `graph`'s tables, every row.
pub(crate) async fn scan_columns(
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

/// The type of the keys of a label whose ids were read as `key_type` and
/// print as `ids`: integers, where every one is an `i64` in decimal.
fn key_type_of(key_type: &DataType, ids: &[String]) -> KeyType {
    let fits = match key_type {
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32 => true,
        // Past i64::MAX a key is text.
        DataType::UInt64 => ids.iter().all(|id| id.parse::<i64>().is_ok()),
        _ => false,
    };
    if fits {
        KeyType::Int
    } else {
        KeyType::Text
    }
}

/// `edges` as a [`GraphResult`]: its node and edge labels (one of each,
/// `""`, when the rule names none), the node columns of the rule and of
/// `SIZE BY` and `COLOR BY`, the highlights, and the encodings.
fn to_view(
    edges: EdgeList,
    key_type: &DataType,
    graph: Option<String>,
    provenance: Provenance,
) -> Result<GraphView> {
    let model = |e: ursa_viz::model::ModelError| {
        DataFusionError::Plan(format!("the graph cannot be drawn: {e}"))
    };
    let EdgeList {
        n,
        src,
        dst,
        ids,
        skipped,
        size,
        color,
        mut labels,
        node_label,
        mut edge_labels,
        edge_label,
        edge_rows,
        node_columns,
        edge_columns,
        paths,
        seeds,
        level,
        layout,
    } = edges;
    if labels.is_empty() {
        labels.push(String::new());
    }
    if edge_labels.is_empty() {
        edge_labels.push(String::new());
    }
    let key = key_type_of(key_type, &ids);
    let nodes = NodeBatch {
        labels: if node_label.is_empty() {
            vec![0; n as usize]
        } else {
            node_label
        },
        keys: ids.iter().collect::<Strings>(),
        ..NodeBatch::default()
    };
    drop(ids);
    let mut batch = EdgeBatch::of_label(0, src, dst);
    if !edge_label.is_empty() {
        batch.labels = edge_label;
    }
    batch.rows = edge_rows;
    let mut ops: Vec<Op> = labels
        .into_iter()
        .map(|name| Op::AddNodeLabel(NodeLabel::new(name, key)))
        .collect();
    ops.extend(edge_labels.into_iter().map(Op::AddEdgeLabel));
    ops.push(Op::AddNodes(nodes));
    ops.push(Op::AddEdges(batch));
    let base = GraphDelta::new(ops);
    let mut result = GraphResult::build(graph, provenance, &base).map_err(model)?;
    drop(base);

    let mut encodings = Encodings {
        layout,
        ..Encodings::default()
    };
    let mut columns: Vec<(Target, ModelColumn)> = Vec::new();
    columns.extend(node_columns.into_iter().map(|c| (Target::Nodes, c)));
    columns.extend(edge_columns.into_iter().map(|c| (Target::Edges, c)));
    // A column of the rule's keeps its name; SIZE BY's and COLOR BY's give way.
    let taken = |columns: &[(Target, ModelColumn)], name: &str| {
        columns
            .iter()
            .any(|(t, c)| *t == Target::Nodes && c.name == name)
    };
    if let Some(size) = size {
        let name = if taken(&columns, &size.by) {
            format!("{}{SIZE_SUFFIX}", size.by)
        } else {
            size.by
        };
        encodings.size_by = Some(name.clone());
        columns.push((
            Target::Nodes,
            ModelColumn::new(name, Some(Role::Metric), Values::F32(size.values)),
        ));
    }
    if let Some(color) = color {
        let name = if taken(&columns, &color.by) {
            format!("{}{COLOR_SUFFIX}", color.by)
        } else {
            color.by
        };
        encodings.color_by = Some(name.clone());
        columns.push((
            Target::Nodes,
            ModelColumn::new(
                name,
                Some(Role::Category),
                Values::Dict {
                    values: color.values.iter().collect(),
                    codes: color.codes,
                },
            ),
        ));
    }
    for (target, column) in columns {
        let op = result.set_column_op(target, column);
        result.apply(&GraphDelta::new(vec![op])).map_err(model)?;
    }
    let highlights = Highlights {
        paths,
        seeds,
        distance: level,
    };
    if !highlights.is_empty() {
        let op = result.set_highlights_op(highlights);
        result.apply(&GraphDelta::new(vec![op])).map_err(model)?;
    }
    if encodings != Encodings::default() {
        let op = result.set_encodings_op(encodings);
        result.apply(&GraphDelta::new(vec![op])).map_err(model)?;
    }
    Ok(GraphView {
        result,
        skipped,
        positions: None,
        groups: None,
    })
}
