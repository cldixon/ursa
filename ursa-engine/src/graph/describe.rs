//! `SHOW GRAPHS` and `DESCRIBE GRAPH`: what graphs a session has, and what
//! one of them is.

use std::sync::Arc;

use datafusion::arrow::array::{ArrayRef, RecordBatch, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::common::TableReference;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionState;

use super::define::unknown_graph;
use super::schema::{edges_plan, graph_table, nodes_plan, EDGES, ID, NODES};
use super::{registry, GraphRegistry, PropertyGraph};

/// `SHOW GRAPHS`: one row per edge table of each graph (one row for a graph
/// with no edge tables), sorted by graph name, with its definition.
///
/// | column         | value                                                     |
/// | -------------- | --------------------------------------------------------- |
/// | `name`         | the graph's name                                          |
/// | `edge_table`   | the edge table (a file path in single quotes), or NULL    |
/// | `source`       | its `SOURCE KEY` column                                   |
/// | `destination`  | its `DESTINATION KEY` column                              |
/// | `vertex_table` | the vertex table its endpoints refer to, or NULL          |
/// | `vertex_key`   | that vertex table's key column, or NULL                   |
///
/// A graph with one edge table and at most one vertex table, as every graph
/// before #295, has one row, as before.
pub fn show_graphs(state: &SessionState) -> Result<RecordBatch> {
    let graphs = registry(state);
    let mut rows: Vec<[Option<String>; 6]> = Vec::new();
    for g in graphs.all() {
        if g.edge_tables.is_empty() {
            for v in &g.vertex_tables {
                rows.push([
                    Some(g.name.clone()),
                    None,
                    None,
                    None,
                    Some(table_name(&graphs, &g, &v.table)),
                    Some(v.key.clone()),
                ]);
            }
            continue;
        }
        for e in &g.edge_tables {
            let vertex = g
                .endpoint_vertex(e, true)
                .or_else(|| g.endpoint_vertex(e, false));
            rows.push([
                Some(g.name.clone()),
                Some(table_name(&graphs, &g, &e.table)),
                Some(e.source.clone()),
                Some(e.destination.clone()),
                vertex.map(|v| table_name(&graphs, &g, &v.table)),
                vertex.map(|v| v.key.clone()),
            ]);
        }
    }
    let columns = (0..6)
        .map(|i| Arc::new(rows.iter().map(|r| r[i].clone()).collect::<StringArray>()) as ArrayRef)
        .collect();
    let schema = Schema::new(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("edge_table", DataType::Utf8, true),
        Field::new("source", DataType::Utf8, true),
        Field::new("destination", DataType::Utf8, true),
        Field::new("vertex_table", DataType::Utf8, true),
        Field::new("vertex_key", DataType::Utf8, true),
    ]);
    Ok(RecordBatch::try_new(Arc::new(schema), columns)?)
}

/// `DESCRIBE GRAPH name`: one row per column of `<name>.nodes`, then of
/// `<name>.edges`, saying what each is and where it comes from.
///
/// | column        | value                                                  |
/// | ------------- | ------------------------------------------------------ |
/// | `table_name`  | `nodes` or `edges`                                     |
/// | `column_name` | the column                                             |
/// | `data_type`   | its type (for `nodes.id`, the graph's node id type)    |
/// | `role`        | `node id`, `source` or `destination`; NULL for others  |
/// | `origin`      | the table column it comes from (`ties.src`)            |
///
/// `origin` of `nodes.id` is the vertex table's key, or without a vertex
/// table, the edge endpoints (`ties.src, ties.dst`). So the rows hold the whole
/// definition, and every row has the same shape.
///
/// A graph with several vertex or edge tables has no `nodes` and `edges`
/// tables: its rows are each element table's columns instead, `table_name`
/// being the element name and `role` one of `key`, `source` (`→ vertex`) and
/// `destination` (`→ vertex`).
pub async fn describe_graph(state: &SessionState, name: &str) -> Result<RecordBatch> {
    let graphs = registry(state);
    let graph = graphs
        .get(name)
        .ok_or_else(|| unknown_graph(name, &graphs))?;
    let Some(def) = graph.default_pair() else {
        return describe_elements(state, &graphs, &graph).await;
    };
    let edge_table = table_name(&graphs, &graph, &def.edges.table);
    let vertex_table = def
        .vertices
        .as_ref()
        .map(|v| (table_name(&graphs, &graph, &v.table), &v.key));

    let unreadable = |e: DataFusionError| {
        DataFusionError::Plan(format!(
            "cannot describe graph '{name}': {}",
            crate::user_message(&e)
        ))
    };
    let mut rows: Vec<[Option<String>; 5]> = Vec::new();
    let nodes = nodes_plan(state, &def).await.map_err(unreadable)?;
    for field in nodes.schema().fields() {
        let column = field.name();
        let (role, origin) = match (&vertex_table, column == ID) {
            (Some((table, key)), true) => (Some("node id"), format!("{table}.{key}")),
            (Some((table, _)), false) => (None, format!("{table}.{column}")),
            (None, _) => (
                Some("node id"),
                format!(
                    "{edge_table}.{}, {edge_table}.{}",
                    def.edges.source, def.edges.destination
                ),
            ),
        };
        rows.push(row(NODES, column, field.data_type(), role, origin));
    }
    let edges = edges_plan(state, &def).await.map_err(unreadable)?;
    for field in edges.schema().fields() {
        let column = field.name();
        let role = match (
            column == &def.edges.source,
            column == &def.edges.destination,
        ) {
            (true, true) => Some("source, destination"),
            (true, false) => Some("source"),
            (false, true) => Some("destination"),
            (false, false) => None,
        };
        rows.push(row(
            EDGES,
            column,
            field.data_type(),
            role,
            format!("{edge_table}.{column}"),
        ));
    }

    describe_batch(rows)
}

/// [`describe_graph`] for a graph with several element tables.
async fn describe_elements(
    state: &SessionState,
    graphs: &GraphRegistry,
    graph: &PropertyGraph,
) -> Result<RecordBatch> {
    let unreadable = |e: DataFusionError| {
        DataFusionError::Plan(format!(
            "cannot describe graph '{}': {}",
            graph.name,
            crate::user_message(&e)
        ))
    };
    let mut rows: Vec<[Option<String>; 5]> = Vec::new();
    for v in &graph.vertex_tables {
        let schema = graph_table(state, &graph.name, &v.table)
            .await
            .map_err(unreadable)?
            .schema();
        let table = table_name(graphs, graph, &v.table);
        for field in schema.fields() {
            let role = (field.name() == &v.key).then_some("key");
            rows.push(row(
                &v.name,
                field.name(),
                field.data_type(),
                role,
                format!("{table}.{}", field.name()),
            ));
        }
    }
    for e in &graph.edge_tables {
        let schema = graph_table(state, &graph.name, &e.table)
            .await
            .map_err(unreadable)?
            .schema();
        let table = table_name(graphs, graph, &e.table);
        let target = |source: bool| {
            graph
                .endpoint_vertex(e, source)
                .map(|v| format!(" \u{2192} {}", v.name))
                .unwrap_or_default()
        };
        let (source, destination) = (
            format!("source{}", target(true)),
            format!("destination{}", target(false)),
        );
        for field in schema.fields() {
            let column = field.name();
            let both = format!("{source}, {destination}");
            let role = match (column == &e.source, column == &e.destination) {
                (true, true) => Some(both.as_str()),
                (true, false) => Some(source.as_str()),
                (false, true) => Some(destination.as_str()),
                (false, false) => None,
            };
            rows.push(row(
                &e.name,
                column,
                field.data_type(),
                role,
                format!("{table}.{column}"),
            ));
        }
    }
    describe_batch(rows)
}

fn describe_batch(rows: Vec<[Option<String>; 5]>) -> Result<RecordBatch> {
    let columns = (0..5)
        .map(|i| Arc::new(rows.iter().map(|r| r[i].clone()).collect::<StringArray>()) as ArrayRef)
        .collect();
    let schema = Schema::new(vec![
        Field::new("table_name", DataType::Utf8, false),
        Field::new("column_name", DataType::Utf8, false),
        Field::new("data_type", DataType::Utf8, false),
        Field::new("role", DataType::Utf8, true),
        Field::new("origin", DataType::Utf8, false),
    ]);
    Ok(RecordBatch::try_new(Arc::new(schema), columns)?)
}

fn row(
    table: &str,
    column: &str,
    data_type: &DataType,
    role: Option<&str>,
    origin: String,
) -> [Option<String>; 5] {
    [
        Some(table.to_string()),
        Some(column.to_string()),
        Some(data_type.to_string()),
        role.map(str::to_string),
        Some(origin),
    ]
}

/// `table` as the definition names it; a file path in single quotes, as it is
/// written in SQL.
fn table_name(graphs: &GraphRegistry, graph: &PropertyGraph, table: &TableReference) -> String {
    if graphs.file_table(&graph.name, table).is_some() {
        format!("'{}'", table.table().replace('\'', "''"))
    } else {
        table.to_string()
    }
}
