//! `SIZE BY x` and `COLOR BY y`: where a name's values come from, one value
//! per node, and how they are drawn.
//!
//! x and y name, in turn: a column of `NODES (query)`; a column of the
//! property graph's vertex tables; `degree`; or a per-node algorithm, run on
//! the graph with defaults through `algorithms(G, …)`. A source that is rows
//! (a query, a table, the last result) has no vertex tables and no graph to
//! run an algorithm on: NODES and degree only.

use std::collections::HashMap;
use std::sync::Arc;

use datafusion::arrow::array::{
    new_null_array, Array, ArrayRef, Float32Array, RecordBatch, UInt32Array,
};
use datafusion::arrow::compute::{cast, concat, interleave};
use datafusion::arrow::datatypes::{DataType, SchemaRef};
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::common::TableReference;
use datafusion::error::Result;
use datafusion::prelude::SessionContext;

use super::build::{EdgeList, NodeIndex};
use super::clauses::Name;
use super::{plan_err, scan_columns, thousands, RunSql, MAX_CATEGORIES};
use crate::graph::functions;
use crate::graph::schema::graph_table;
use crate::graph::PropertyGraph;
use ursa_viz::model::NULL;

/// The colour index of a node in no top category, or with no value.
#[cfg(test)]
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

/// `COLOR BY y`: each node's category, the categories ranked by how many
/// nodes they have. The first [`MAX_CATEGORIES`] are coloured; the rest are
/// drawn as "other".
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColorBy {
    pub by: String,
    /// Every category's value, as the shell prints it: the largest first,
    /// equal ones as first seen.
    pub values: Vec<String>,
    /// How many nodes each category has.
    pub counts: Vec<u32>,
    /// Each node's category, as an index into `values`, or [`NULL`] for a
    /// node with no value.
    pub codes: Vec<u32>,
    /// Nodes with no value (NULL, or none given).
    pub missing: u32,
}

#[cfg(test)]
impl ColorBy {
    /// The categories coloured, by colour index.
    pub(crate) fn labels(&self) -> &[String] {
        &self.values[..self.values.len().min(MAX_CATEGORIES)]
    }

    /// How many nodes each coloured category has.
    pub(crate) fn top_counts(&self) -> &[u32] {
        &self.counts[..self.counts.len().min(MAX_CATEGORIES)]
    }

    /// Nodes in the categories past the coloured ones.
    pub(crate) fn other(&self) -> u32 {
        self.counts.iter().skip(MAX_CATEGORIES).sum()
    }

    /// Each node's colour index: its category's, or [`OTHER`].
    pub(crate) fn index(&self) -> Vec<u32> {
        self.codes.iter().map(|&c| c.min(OTHER)).collect()
    }
}

/// Where a name's values come from.
pub(crate) enum Found {
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
pub(crate) fn per_node_algorithm(name: &str) -> Option<&'static str> {
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
pub(crate) async fn resolve(
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
pub(crate) async fn vertex_column(
    ctx: &SessionContext,
    graph: &PropertyGraph,
    ident: &str,
) -> Result<Column> {
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
pub(crate) fn degree_column(edges: &EdgeList) -> Column {
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
pub(crate) async fn run_algorithms(
    ctx: &SessionContext,
    sql: &dyn RunSql,
    graph: &PropertyGraph,
    algorithms: &[&'static str],
) -> Result<NodeValues> {
    let calls: Vec<String> = algorithms
        .iter()
        .map(|a| format!("{a} => {}", default_call(a)))
        .collect();
    let query = format!(
        "SELECT * FROM algorithms(\"{}\", {})",
        graph.name.replace('"', "\"\""),
        calls.join(", ")
    );
    NodeValues::of_query(ctx, sql, &query).await
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
pub(crate) struct NodeValues {
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    id: usize,
}

impl NodeValues {
    pub(crate) async fn of_query(
        ctx: &SessionContext,
        sql: &dyn RunSql,
        query: &str,
    ) -> Result<NodeValues> {
        let (schema, batches) = sql.run(ctx, query).await?;
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

    pub(crate) fn column_named(&self, name: &str) -> Column {
        self.column(self.find(name).expect("the column asked for"))
    }

    fn value_names(&self) -> Vec<String> {
        let fields = self.schema.fields().iter().enumerate();
        fields
            .filter(|(i, _)| *i != self.id)
            .map(|(_, f)| f.name().clone())
            .collect()
    }

    pub(crate) fn column(&self, i: usize) -> Column {
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
pub(crate) struct Column {
    ids: Option<Vec<ArrayRef>>,
    values: Vec<ArrayRef>,
}

impl Column {
    /// One value per node, in node order: NULL for a node with none. An id
    /// given twice keeps its first value.
    pub(crate) fn per_node(&self, index: &NodeIndex, n: usize) -> Result<ArrayRef> {
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
pub(crate) fn size_by(by: &str, values: &ArrayRef) -> Result<SizeBy> {
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
pub(crate) fn color_by(by: &str, values: &ArrayRef) -> Result<ColorBy> {
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
    let mut rank = vec![0u32; distinct];
    for (r, &c) in order.iter().enumerate() {
        rank[c] = r as u32;
    }
    Ok(ColorBy {
        by: by.to_string(),
        values: order.iter().map(|&c| categories[c].0.clone()).collect(),
        counts: order.iter().map(|&c| categories[c].1).collect(),
        codes: of_node
            .iter()
            .map(|c| c.map_or(NULL, |c| rank[c]))
            .collect(),
        missing: values.null_count() as u32,
    })
}
