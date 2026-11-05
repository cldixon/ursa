//! `SHOW GRAPHS` and `DESCRIBE GRAPH`: what graphs a session has, and what
//! one of them is.

use std::sync::Arc;

use datafusion::arrow::array::{ArrayRef, RecordBatch, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::common::TableReference;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionState;

use super::define::unknown_graph;
use super::schema::{edges_plan, nodes_plan, EDGES, ID, NODES};
use super::{registry, GraphDef, GraphRegistry};

/// `SHOW GRAPHS`: one row per graph, sorted by name, with its definition.
///
/// | column         | value                                                |
/// | -------------- | ---------------------------------------------------- |
/// | `name`         | the graph's name                                     |
/// | `edge_table`   | the edge table (a file path in single quotes)        |
/// | `source`       | its `SOURCE KEY` column                              |
/// | `destination`  | its `DESTINATION KEY` column                         |
/// | `vertex_table` | the vertex table, or NULL without `VERTEX TABLES`    |
/// | `vertex_key`   | the vertex table's `KEY` column, or NULL without one |
pub fn show_graphs(state: &SessionState) -> Result<RecordBatch> {
    let graphs = registry(state);
    let all = graphs.all();
    let column = |value: &dyn Fn(&GraphDef) -> Option<String>| -> ArrayRef {
        Arc::new(all.iter().map(|g| value(g)).collect::<StringArray>())
    };
    let columns = vec![
        column(&|g| Some(g.name.clone())),
        column(&|g| Some(table_name(&graphs, g, &g.edges.table))),
        column(&|g| Some(g.edges.source.clone())),
        column(&|g| Some(g.edges.destination.clone())),
        column(&|g| {
            g.vertices
                .as_ref()
                .map(|v| table_name(&graphs, g, &v.table))
        }),
        column(&|g| g.vertices.as_ref().map(|v| v.key.clone())),
    ];
    let schema = Schema::new(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("edge_table", DataType::Utf8, false),
        Field::new("source", DataType::Utf8, false),
        Field::new("destination", DataType::Utf8, false),
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
pub async fn describe_graph(state: &SessionState, name: &str) -> Result<RecordBatch> {
    let graphs = registry(state);
    let def = graphs
        .get(name)
        .ok_or_else(|| unknown_graph(name, &graphs))?;
    let edge_table = table_name(&graphs, &def, &def.edges.table);
    let vertex_table = def
        .vertices
        .as_ref()
        .map(|v| (table_name(&graphs, &def, &v.table), &v.key));

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
fn table_name(graphs: &GraphRegistry, def: &GraphDef, table: &TableReference) -> String {
    if graphs.file_table(&def.name, table).is_some() {
        format!("'{}'", table.table().replace('\'', "''"))
    } else {
        table.to_string()
    }
}
