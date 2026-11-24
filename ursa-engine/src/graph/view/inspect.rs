//! What a graph's tables say of one node or one edge of a drawing: the
//! inspector's rows (design §4, #444).
//!
//! A drawing's node is a key of a vertex table, and its edge a row of an
//! edge table, when the drawing names a property graph. [`inspect`] reads
//! that row with one query by key:
//!
//! - **a node:** `SELECT * FROM members WHERE id = 33`, on the vertex table
//!   its label names. A drawing with one label `""` (`.graph NAME`, a plain
//!   result on a graph) reads the graph's vertex tables in turn, and gives
//!   the first that has the key.
//! - **an edge:** `SELECT * FROM ties WHERE src = 0 AND dst = 33`, the rows
//!   of the edge table between its two ends. Where several rows join them
//!   (parallel edges) and the drawing knows the edge's row number, that row
//!   alone is read (`LIMIT 1 OFFSET row`), when the table is read as one
//!   partition, as it is in the browser: there the scan's order is the row
//!   numbers'. Otherwise all the rows between the ends are given, and
//!   [`Inspected::note`] says so. An edge with no row number may be drawn
//!   against the way the table stores it (a path's steps are), so its rows
//!   are looked for both ways.
//!
//! The queries are built as plans, not as text, so a table that is a file
//! path, and any name, reads the same; [`Inspected::sql`] is the query as a
//! user would type it. Nothing here changes the session or its last result.
//!
//! **An edge's rows are found through the graph's index** (#489), where they
//! can be: scanning a million edges for two ends took 90 to 160 ms in the
//! browser. `induced_edges(G, nodes => [a, b])` reads the two nodes'
//! adjacency and gives the row number of each edge between them, and those
//! rows are then read by number ([`take`]): from a Parquet file with a row
//! selection, which skips the pages before them, and from any other table by
//! passing over the rows before. The rows read are checked against the two
//! ends, so they are the rows the query by the ends gives, in the same order.
//! The scan stays for what the index cannot say: a graph with several edge
//! tables, a table read as several partitions (its row numbers are then not
//! the scan's order), ends the index does not hold, or a check that fails.
//!
//! [`graph_neighbours`] is how many neighbours a node has in the whole graph,
//! for "9 drawn, 17 neighbours in karate".

use datafusion::arrow::array::{Array, AsArray, RecordBatch};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::{DataType, UInt64Type};
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::common::stats::Precision;
use datafusion::common::TableReference;
use datafusion::dataframe::DataFrame;
use datafusion::datasource::physical_plan::parquet::ParquetRowSelection;
use datafusion::datasource::physical_plan::{FileGroup, FileScanConfig, FileScanConfigBuilder};
use datafusion::datasource::provider_as_source;
use datafusion::datasource::source::DataSourceExec;
use datafusion::error::Result;
use datafusion::logical_expr::{Expr, LogicalPlanBuilder};
use datafusion::parquet::arrow::arrow_reader::RowSelection;
use datafusion::physical_plan::projection::ProjectionExec;
use datafusion::physical_plan::{
    replace_children_if_necessary, ExecutionPlan, ExecutionPlanProperties,
};
use datafusion::prelude::{ident, lit, SessionContext};
use futures::StreamExt;
use std::sync::Arc;
use ursa_viz::model::{GraphResult, KeyType};

use super::{plan_err, RunSql};
use crate::graph::schema::graph_table;
use crate::graph::{registry_of, PropertyGraph};

/// The most rows given for one edge: the edges between two nodes, when the
/// drawing cannot say which of them it drew.
pub const MAX_ROWS: usize = 20;

/// The most characters of one value; a longer one ends with `…`.
pub const MAX_VALUE_CHARS: usize = 4096;

/// What to inspect: a node or an edge of the drawing, by its index, with the
/// keys the asker shows for it. The keys are checked against the drawing, so
/// an asker whose picture is not the session's is told, not shown another
/// node's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inspect<'a> {
    Node {
        node: u32,
        key: &'a str,
    },
    Edge {
        edge: u32,
        src: &'a str,
        dst: &'a str,
    },
}

/// A column of an inspected row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedColumn {
    pub name: String,
    /// Whether its values are numbers: what `SIZE BY` can read.
    pub numeric: bool,
}

/// The rows the graph's tables have for a node or an edge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inspected {
    /// The table read: the vertex or edge table's element name. `None` when
    /// there is none to read ([`Self::note`] says why).
    pub table: Option<String>,
    /// The query, as a user would type it.
    pub sql: Option<String>,
    pub columns: Vec<InspectedColumn>,
    /// Each row's values as the shell prints them, `None` for NULL. One row
    /// for a node; for an edge, one, or every row between its ends.
    pub rows: Vec<Vec<Option<String>>>,
    /// An edge's row number in its table, where the drawing knows it.
    pub row: Option<u64>,
    /// What a reader should know: why there is no row, or that the rows are
    /// several edges'.
    pub note: Option<String>,
    /// Whether an edge's rows were found through the graph's index and read
    /// by their numbers, not by a scan of the edge table (see the
    /// [module](self)). The rows are the same either way.
    pub indexed: bool,
}

impl Inspected {
    fn said(note: impl Into<String>) -> Inspected {
        Inspected {
            note: Some(note.into()),
            ..Inspected::default()
        }
    }
}

/// `name` as SQL reads an identifier: bare when it is a plain lower-case
/// name, else quoted.
fn name(name: &str) -> String {
    let plain = name
        .chars()
        .enumerate()
        .all(|(i, c)| c == '_' || c.is_ascii_lowercase() || (i > 0 && c.is_ascii_digit()));
    if plain && !name.is_empty() {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// `table` as a query names it: its name, or a file path as a string.
fn table_name(ctx: &SessionContext, graph: &PropertyGraph, table: &TableReference) -> String {
    if registry_of(ctx).file_table(&graph.name, table).is_some() {
        format!("'{}'", table.table().replace('\'', "''"))
    } else {
        match table.schema() {
            Some(schema) => format!("{}.{}", name(schema), name(table.table())),
            None => name(table.table()),
        }
    }
}

/// `key` as a literal, to compare and to print.
fn literal(key: &str, ty: KeyType) -> (Expr, String) {
    match (ty, key.parse::<i64>()) {
        (KeyType::Int, Ok(n)) => (lit(n), key.to_string()),
        _ => (lit(key), format!("'{}'", key.replace('\'', "''"))),
    }
}

/// Every row of `table` that `filter` keeps, up to `limit`, after `skip`.
async fn read(
    ctx: &SessionContext,
    graph: &PropertyGraph,
    table: &TableReference,
    filter: Option<Expr>,
    skip: usize,
    limit: usize,
) -> Result<(Vec<InspectedColumn>, Vec<Vec<Option<String>>>)> {
    let frame = scan(ctx, graph, table).await?;
    let frame = match filter {
        Some(filter) => frame.filter(filter)?,
        None => frame,
    };
    let columns = columns_of(&frame);
    let batches = frame.limit(skip, Some(limit))?.collect().await?;
    Ok((columns, rows_of(&batches)?))
}

/// The columns of `frame`, as the inspector names them.
fn columns_of(frame: &DataFrame) -> Vec<InspectedColumn> {
    frame
        .schema()
        .fields()
        .iter()
        .map(|f| InspectedColumn {
            name: f.name().clone(),
            numeric: f.data_type().is_numeric(),
        })
        .collect()
}

/// The rows of `table` numbered `rows` (ascending, each once), in that
/// order: fewer when the table ends before one of them. `None` when the
/// table is read as several partitions: its rows then come in no one order,
/// and a row number finds nothing.
///
/// A Parquet file is read with a row selection ([`selected`]), which reads
/// the pages the rows are in. Any other table is read from its start, up to
/// the last of the rows.
async fn take(
    ctx: &SessionContext,
    graph: &PropertyGraph,
    table: &TableReference,
    rows: &[u64],
) -> Result<Option<(Vec<InspectedColumn>, Vec<Vec<Option<String>>>)>> {
    let frame = scan(ctx, graph, table).await?;
    let columns = columns_of(&frame);
    let plan = frame.create_physical_plan().await?;
    if plan.output_partitioning().partition_count() != 1 {
        return Ok(None);
    }
    let mut taken = Vec::with_capacity(rows.len());
    if let Some(plan) = selected(&plan, rows) {
        let mut stream = plan.execute(0, ctx.task_ctx())?;
        while let Some(batch) = stream.next().await {
            taken.push(batch?);
        }
    } else {
        let mut stream = plan.execute(0, ctx.task_ctx())?;
        let (mut wanted, mut passed) = (rows.iter().copied().peekable(), 0u64);
        while wanted.peek().is_some() {
            let Some(batch) = stream.next().await else {
                break;
            };
            let batch = batch?;
            let end = passed + batch.num_rows() as u64;
            while let Some(row) = wanted.next_if(|&row| row < end) {
                taken.push(batch.slice((row - passed) as usize, 1));
            }
            passed = end;
        }
    }
    Ok(Some((columns, rows_of(&taken)?)))
}

/// `plan` reading only its rows numbered `rows` (ascending), when it is a
/// scan of one whole Parquet file with a known number of rows, under nothing
/// but projections: the file's scan with a row selection, so the reader
/// passes over the row groups and pages before a row without decoding them.
fn selected(plan: &Arc<dyn ExecutionPlan>, rows: &[u64]) -> Option<Arc<dyn ExecutionPlan>> {
    if let Some(projection) = plan.downcast_ref::<ProjectionExec>() {
        let input = selected(projection.input(), rows)?;
        return replace_children_if_necessary(Arc::clone(plan), vec![input]).ok();
    }
    let config = plan
        .downcast_ref::<DataSourceExec>()?
        .data_source()
        .downcast_ref::<FileScanConfig>()?;
    let [group] = config.file_groups.as_slice() else {
        return None;
    };
    let [file] = group.files() else {
        return None;
    };
    if config.limit.is_some()
        || file.range.is_some()
        || config.file_source().file_type() != "parquet"
    {
        return None;
    }
    let Precision::Exact(total) = file.statistics.as_ref()?.num_rows else {
        return None;
    };
    if rows.last().is_none_or(|&last| last >= total as u64) {
        return None;
    }
    let selection = RowSelection::from_consecutive_ranges(
        rows.iter().map(|&row| row as usize..row as usize + 1),
        total,
    );
    let file = file
        .clone()
        .with_extension(ParquetRowSelection::new(selection));
    let config = FileScanConfigBuilder::from(config.clone())
        .with_file_groups(vec![FileGroup::new(vec![file])])
        .build();
    Some(DataSourceExec::from_data_source(config))
}

async fn scan(
    ctx: &SessionContext,
    graph: &PropertyGraph,
    table: &TableReference,
) -> Result<DataFrame> {
    let state = ctx.state();
    let provider = graph_table(&state, &graph.name, table).await?;
    let plan =
        LogicalPlanBuilder::scan(table.clone(), provider_as_source(provider), None)?.build()?;
    Ok(DataFrame::new(state, plan))
}

/// The batches' rows as text, as the shell prints a value.
fn rows_of(batches: &[RecordBatch]) -> Result<Vec<Vec<Option<String>>>> {
    let options = FormatOptions::default();
    let mut rows = Vec::new();
    for batch in batches {
        let shown = batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &options))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for i in 0..batch.num_rows() {
            let row = batch
                .columns()
                .iter()
                .zip(&shown)
                .map(|(column, shown)| {
                    if column.is_null(i) {
                        return None;
                    }
                    let mut text = shown.value(i).to_string();
                    if let Some((at, _)) = text.char_indices().nth(MAX_VALUE_CHARS) {
                        text.truncate(at);
                        text.push('…');
                    }
                    Some(text)
                })
                .collect();
            rows.push(row);
        }
    }
    Ok(rows)
}

/// The property graph `result` names, or why there is none to read.
fn graph_of(
    ctx: &SessionContext,
    result: &GraphResult,
) -> std::result::Result<std::sync::Arc<PropertyGraph>, String> {
    let Some(graph) = result.graph() else {
        return Err(
            "this drawing is not of a property graph, so there is no table row to show; draw \
             a graph's (.graph NAME) or a query's on one"
                .into(),
        );
    };
    registry_of(ctx)
        .get(graph)
        .ok_or_else(|| format!("the graph '{graph}' is gone, and its tables with it"))
}

/// The row of the graph's tables for one node or edge of `result`: see the
/// [module](self). A drawing with no table to read gives no rows and a
/// [`note`](Inspected::note), not an error; an index or a key that is not the
/// drawing's is an error.
pub async fn inspect(
    ctx: &SessionContext,
    sql: &dyn RunSql,
    result: &GraphResult,
    what: Inspect<'_>,
) -> Result<Inspected> {
    match what {
        Inspect::Node { node, key } => {
            let at = node as usize;
            if at >= result.n_nodes() || result.node_keys().get(at) != key {
                return plan_err(format!(
                    "the drawing has changed: its node {node} is not '{key}'; draw it again \
                     (.graph)"
                ));
            }
            inspect_node(ctx, result, at).await
        }
        Inspect::Edge { edge, src, dst } => {
            let at = edge as usize;
            let keys = result.node_keys();
            if at >= result.n_edges()
                || keys.get(result.src()[at] as usize) != src
                || keys.get(result.dst()[at] as usize) != dst
            {
                return plan_err(format!(
                    "the drawing has changed: its edge {edge} does not join '{src}' and \
                     '{dst}'; draw it again (.graph)"
                ));
            }
            inspect_edge(ctx, sql, result, at).await
        }
    }
}

async fn inspect_node(
    ctx: &SessionContext,
    result: &GraphResult,
    node: usize,
) -> Result<Inspected> {
    let graph = match graph_of(ctx, result) {
        Ok(graph) => graph,
        Err(note) => return Ok(Inspected::said(note)),
    };
    if super::groups::is_group(result, node) {
        return Ok(Inspected::said(format!(
            "a group of {}'s nodes, not a row of its tables: its values are above, and .expand \
             group:{} opens it",
            graph.name,
            result.node_keys().get(node)
        )));
    }
    let label = &result.node_labels()[result.node_label()[node] as usize];
    let key = result.node_keys().get(node);
    // The label's vertex table; with no label, each of the graph's in turn.
    let tables: Vec<_> = graph
        .vertex_tables
        .iter()
        .filter(|v| label.name.is_empty() || v.name == label.name)
        .collect();
    if tables.is_empty() {
        return Ok(Inspected::said(if graph.vertex_tables.is_empty() {
            format!(
                "the graph {} has no vertex tables: a node is its key and its edges",
                graph.name
            )
        } else {
            format!(
                "the graph {} has no vertex table '{}'",
                graph.name, label.name
            )
        }));
    }
    let mut first = None;
    for v in tables {
        let (value, shown) = literal(key, label.key);
        let (columns, rows) =
            read(ctx, &graph, &v.table, Some(ident(&v.key).eq(value)), 0, 1).await?;
        let found = Inspected {
            table: Some(v.name.clone()),
            sql: Some(format!(
                "SELECT * FROM {} WHERE {} = {shown}",
                table_name(ctx, &graph, &v.table),
                name(&v.key)
            )),
            columns,
            rows,
            row: None,
            note: None,
            indexed: false,
        };
        if !found.rows.is_empty() {
            return Ok(found);
        }
        first.get_or_insert(found);
    }
    let mut none = first.expect("there is a table");
    none.note = Some(format!(
        "no row of {} has the key {key}: the node is named only by an edge",
        none.table.as_deref().unwrap_or_default()
    ));
    Ok(none)
}

async fn inspect_edge(
    ctx: &SessionContext,
    run: &dyn RunSql,
    result: &GraphResult,
    edge: usize,
) -> Result<Inspected> {
    let graph = match graph_of(ctx, result) {
        Ok(graph) => graph,
        Err(note) => return Ok(Inspected::said(note)),
    };
    if super::groups::is_group(result, result.src()[edge] as usize) {
        return Ok(Inspected::said(format!(
            "the edges of {} between two groups, counted: not one row of its tables",
            graph.name
        )));
    }
    let label = &result.edge_labels()[result.edge_label()[edge] as usize];
    let row = result.edge_row(edge);
    let end = |node: u32| {
        let at = node as usize;
        let ty = result.node_labels()[result.node_label()[at] as usize].key;
        (
            result.node_keys().get(at),
            literal(result.node_keys().get(at), ty),
        )
    };
    let (s_key, (s, s_shown)) = end(result.src()[edge]);
    let (d_key, (d, d_shown)) = end(result.dst()[edge]);
    let tables: Vec<_> = graph
        .edge_tables
        .iter()
        .filter(|e| label.is_empty() || &e.name == label)
        .collect();
    if tables.is_empty() {
        return Ok(Inspected::said(format!(
            "the graph {} has no edge table '{label}'",
            graph.name
        )));
    }
    let mut first = None;
    for e in tables {
        let (source, destination) = (ident(&e.source), ident(&e.destination));
        let table = table_name(ctx, &graph, &e.table);
        let (from, to) = (name(&e.source), name(&e.destination));
        let mut filter = source
            .clone()
            .eq(s.clone())
            .and(destination.clone().eq(d.clone()));
        let mut sql =
            format!("SELECT * FROM {table} WHERE {from} = {s_shown} AND {to} = {d_shown}");
        // With no row number, the drawing may have it the other way round.
        let either_way = row.is_none() && s_shown != d_shown;
        if either_way {
            filter = filter.or(source.eq(d.clone()).and(destination.eq(s.clone())));
            sql = format!(
                "SELECT * FROM {table} WHERE ({from} = {s_shown} AND {to} = {d_shown}) OR \
                 ({from} = {d_shown} AND {to} = {s_shown})"
            );
        }
        // Through the index, where it finds them; else by a scan.
        let ends = Ends {
            literals: [&s_shown, &d_shown],
            keys: [s_key, d_key],
            either_way,
        };
        let (indexed, (columns, rows)) = match between(ctx, run, &graph, e, &ends).await {
            Some(found) => (true, found),
            None => (
                false,
                read(ctx, &graph, &e.table, Some(filter), 0, MAX_ROWS + 1).await?,
            ),
        };
        let mut found = Inspected {
            table: Some(e.name.clone()),
            sql: Some(sql),
            columns,
            rows,
            row,
            note: None,
            indexed,
        };
        if found.rows.len() > 1 {
            // Parallel edges: the one row, where its number finds it.
            let one = match row {
                Some(row) => take(ctx, &graph, &e.table, &[row])
                    .await?
                    .filter(|(_, rows)| !rows.is_empty())
                    .map(|(_, rows)| (row, rows)),
                None => None,
            };
            match one {
                Some((row, rows)) => {
                    found.rows = rows;
                    found.sql = Some(format!("SELECT * FROM {table} LIMIT 1 OFFSET {row}"));
                }
                None => {
                    let more = found.rows.len() > MAX_ROWS;
                    found.rows.truncate(MAX_ROWS);
                    found.note = Some(format!(
                        "{}{} rows of {} join these two nodes; the drawing does not say which \
                         this edge is",
                        if more { "more than " } else { "" },
                        found.rows.len(),
                        e.name
                    ));
                }
            }
        }
        if !found.rows.is_empty() {
            return Ok(found);
        }
        first.get_or_insert(found);
    }
    let mut none = first.expect("there is a table");
    none.note = Some(format!(
        "no row of {} joins these two nodes: the edge is not the graph's (a query made it)",
        none.table.as_deref().unwrap_or_default()
    ));
    Ok(none)
}

/// The two ends of an edge, to find its rows by.
struct Ends<'a> {
    /// The ends as SQL literals, source first.
    literals: [&'a str; 2],
    /// The ends as the drawing's keys: how a value of the table prints.
    keys: [&'a str; 2],
    /// Whether a row from the second end to the first is the edge's too.
    either_way: bool,
}

impl Ends<'_> {
    /// Whether a row from `src` to `dst` (as they print) joins the ends.
    fn joins(&self, src: Option<&str>, dst: Option<&str>) -> bool {
        let [s, d] = self.keys.map(Some);
        (src, dst) == (s, d) || (self.either_way && (src, dst) == (d, s))
    }
}

/// The rows of the edge table `e` between `ends`, found through the graph's
/// index: the first `MAX_ROWS + 1` of them in the table's order, which is
/// what the scan by the two ends gives. `None` when the index cannot say, and
/// the scan must: the graph has several edge tables (an index is of one),
/// the index has no edge between the ends, the table is read as several
/// partitions, or a row read by its number does not join the ends.
async fn between(
    ctx: &SessionContext,
    run: &dyn RunSql,
    graph: &PropertyGraph,
    e: &crate::graph::EdgeTable,
    ends: &Ends<'_>,
) -> Option<(Vec<InspectedColumn>, Vec<Vec<Option<String>>>)> {
    if graph.pair(None, None).ok()?.edges.name != e.name {
        return None;
    }
    let [s, d] = ends.literals;
    let (_, batches) = run
        .run(
            ctx,
            &format!(
                "SELECT src, dst, edge FROM induced_edges(\"{}\", nodes => [{s}, {d}])",
                graph.name.replace('"', "\"\"")
            ),
        )
        .await
        .ok()?;
    // The edges among the two nodes, by row: those between them (not a loop
    // at one end), the way the drawing has them.
    let mut rows = Vec::new();
    for batch in &batches {
        let number = cast(batch.column(2), &DataType::UInt64).ok()?;
        let number = number.as_primitive::<UInt64Type>();
        let shown = rows_of(std::slice::from_ref(&batch.project(&[0, 1]).ok()?)).ok()?;
        for (i, ends_of) in shown.iter().enumerate() {
            if number.is_valid(i) && ends.joins(ends_of[0].as_deref(), ends_of[1].as_deref()) {
                rows.push(number.value(i));
            }
        }
    }
    rows.sort_unstable();
    rows.dedup();
    rows.truncate(MAX_ROWS + 1);
    if rows.is_empty() {
        return None;
    }
    let (columns, found) = take(ctx, graph, &e.table, &rows).await.ok()??;
    // The rows read are the rows asked for: each joins the two ends.
    let at = |name: &str| columns.iter().position(|c| c.name == name);
    let (src, dst) = (at(&e.source)?, at(&e.destination)?);
    let all = found.len() == rows.len()
        && found
            .iter()
            .all(|row| ends.joins(row[src].as_deref(), row[dst].as_deref()));
    all.then_some((columns, found))
}

/// How many neighbours node `node` has in the whole graph `result` names,
/// whichever way the edges run: `SELECT count(*) FROM hop(G, seeds => [key],
/// steps => 1, direction => 'both')`, run by `sql`. It reads the node's edges
/// from the graph's index, not every edge, so it costs what the node's degree
/// does. On a graph with no parallel edges and no loops it is the node's
/// degree. `None` when the drawing names no graph.
pub async fn graph_neighbours(
    ctx: &SessionContext,
    sql: &dyn RunSql,
    result: &GraphResult,
    node: u32,
) -> Result<Option<u64>> {
    let at = node as usize;
    let Some(graph) = result.graph() else {
        return Ok(None);
    };
    if at >= result.n_nodes() {
        return plan_err(format!("the drawing has no node {node}"));
    }
    // A group is no node of the graph.
    if super::groups::is_group(result, at) {
        return Ok(None);
    }
    let ty = result.node_labels()[result.node_label()[at] as usize].key;
    let (_, key) = literal(result.node_keys().get(at), ty);
    let (_, batches) = sql
        .run(
            ctx,
            &format!(
                "SELECT count(*) FROM hop(\"{}\", seeds => [{key}], steps => 1, direction => 'both')",
                graph.replace('"', "\"\"")
            ),
        )
        .await?;
    for batch in &batches {
        if batch.num_rows() > 0 {
            let count = cast(batch.column(0), &DataType::UInt64)?;
            let count = count.as_primitive::<UInt64Type>();
            return Ok(count.is_valid(0).then(|| count.value(0)));
        }
    }
    Ok(None)
}
