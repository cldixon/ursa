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

use arrow::array::{Array, ArrayRef, RecordBatch};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Schema};
use arrow::row::{RowConverter, SortField};
use arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::common::TableReference;
use datafusion::dataframe::DataFrame;
use datafusion::datasource::provider_as_source;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::LogicalPlanBuilder;
use datafusion::prelude::SessionContext;
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
pub(crate) fn from_result(schema: &Schema, batches: &[RecordBatch]) -> Result<EdgeList> {
    let (s, d) = endpoint_columns(schema)?;
    let edges: Vec<(ArrayRef, ArrayRef)> = batches
        .iter()
        .map(|b| (Arc::clone(b.column(s)), Arc::clone(b.column(d))))
        .collect();
    build(&[], &edges)
}

/// The graph with the nodes `vertices` (keys, in order) and the edges
/// `edges` (source and destination arrays of equal length). Nodes are
/// numbered as first seen: the vertices, then the edges' endpoints.
pub(crate) fn build(vertices: &[ArrayRef], edges: &[(ArrayRef, ArrayRef)]) -> Result<EdgeList> {
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
    Ok(out)
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
pub(crate) async fn from_property_graph(
    ctx: &SessionContext,
    graph: &PropertyGraph,
) -> Result<EdgeList> {
    let state = ctx.state();
    let mut vertices = Vec::new();
    for v in &graph.vertex_tables {
        let table = graph_table(&state, &graph.name, &v.table).await?;
        let plan =
            LogicalPlanBuilder::scan(v.table.clone(), provider_as_source(table), None)?.build()?;
        let batches = DataFrame::new(state.clone(), plan)
            .select_columns(&[v.key.as_str()])?
            .collect()
            .await?;
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
    build(&vertices, &edges)
}

/// What `.graph TEXT` draws: the property graph TEXT names, a table's rows,
/// or a query's result (its last statement's).
pub(crate) async fn from_text(ctx: &SessionContext, text: &str) -> Result<EdgeList> {
    if let Some(name) = identifier(text) {
        if let Some(graph) = registry_of(ctx).get(&name) {
            return from_property_graph(ctx, &graph).await;
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
        return from_result(&schema, &frame.collect().await?);
    }
    let statements = ursa_sql::parse_sql_with_text_for(ctx, text)?;
    let mut last = None;
    for (statement, sql) in statements {
        last = Some(ursa_sql::execute_with_text(ctx, statement, &sql).await?);
    }
    let Some(frame) = last else {
        return plan_err("no query to draw; .graph SELECT src, dst FROM …");
    };
    let schema = Arc::new(frame.schema().as_arrow().clone());
    let batches = frame.collect().await?;
    from_result(&schema, &batches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, StringArray};
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
