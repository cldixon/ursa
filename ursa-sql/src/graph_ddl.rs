//! Running `CREATE PROPERTY GRAPH`, `DROP PROPERTY GRAPH`, `SHOW GRAPHS` and
//! `DESCRIBE GRAPH`.
//!
//! Identifiers are normalized as DataFusion normalizes table names (unquoted
//! names lower-cased, quoted names kept, unless
//! `datafusion.sql_parser.enable_ident_normalization` is off), then the engine
//! checks the definition against the session's tables and registers it.

use datafusion::common::TableReference;
use datafusion::dataframe::DataFrame;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::LogicalPlanBuilder;
use datafusion::prelude::SessionContext;
use datafusion::sql::planner::{object_name_to_table_reference, IdentNormalizer};
use datafusion::sql::sqlparser::ast::{Ident, ObjectName};
use ursa_engine::graph::{self, EdgeTable, GraphDef, VertexTable};

use crate::parser::{CreatePropertyGraph, DescribeGraph, DropPropertyGraph};
use crate::property_graph::{EndpointDef, References};

pub(crate) async fn create(
    ctx: &SessionContext,
    statement: CreatePropertyGraph,
) -> Result<DataFrame> {
    let normalize = normalizes(ctx);
    let or_replace = statement.or_replace;
    let if_not_exists = statement.if_not_exists;
    let def = graph_def(statement, normalize)?;
    graph::create_graph(ctx, def, or_replace, if_not_exists).await?;
    empty(ctx)
}

pub(crate) fn drop(ctx: &SessionContext, statement: DropPropertyGraph) -> Result<DataFrame> {
    let name = graph_name(statement.name, normalizes(ctx))?;
    graph::drop_graph(ctx, &name, statement.if_exists)?;
    empty(ctx)
}

pub(crate) fn show(ctx: &SessionContext) -> Result<DataFrame> {
    ctx.read_batch(graph::show_graphs(&ctx.state())?)
}

pub(crate) async fn describe(ctx: &SessionContext, statement: DescribeGraph) -> Result<DataFrame> {
    let name = graph_name(statement.name, normalizes(ctx))?;
    ctx.read_batch(graph::describe_graph(&ctx.state(), &name).await?)
}

/// The definition `statement` makes, with its identifiers normalized.
fn graph_def(statement: CreatePropertyGraph, normalize: bool) -> Result<GraphDef> {
    let normalizer = IdentNormalizer::new(normalize);
    let name = graph_name(statement.name, normalize)?;
    let vertices = match statement.vertices {
        Some(v) => Some(VertexTable {
            table: object_name_to_table_reference(v.table, normalize)?,
            key: normalizer.normalize(v.key),
        }),
        None => None,
    };
    let edge_table = object_name_to_table_reference(statement.edges.table, normalize)?;
    let source = endpoint(statement.edges.source, "SOURCE", &vertices, normalize)?;
    let destination = endpoint(
        statement.edges.destination,
        "DESTINATION",
        &vertices,
        normalize,
    )?;
    Ok(GraphDef {
        name,
        edges: EdgeTable {
            table: edge_table,
            source,
            destination,
        },
        vertices,
    })
}

/// The endpoint column, after checking that its `REFERENCES` (if any) names
/// the vertex table and its key.
fn endpoint(
    endpoint: EndpointDef,
    side: &str,
    vertices: &Option<VertexTable>,
    normalize: bool,
) -> Result<String> {
    let key = IdentNormalizer::new(normalize).normalize(endpoint.key);
    let Some(References { table, column }) = endpoint.references else {
        return Ok(key);
    };
    let Some(vertices) = vertices else {
        return Err(DataFusionError::Plan(format!(
            "{side} KEY ({key}) REFERENCES {table}, but the graph has no VERTEX TABLES; \
             add VERTEX TABLES ({table} KEY ({column})) or drop the REFERENCES clause"
        )));
    };
    let referenced = object_name_to_table_reference(table, normalize)?;
    let column = IdentNormalizer::new(normalize).normalize(column);
    if !same_table(&referenced, &vertices.table) || column != vertices.key {
        return Err(DataFusionError::Plan(format!(
            "{side} KEY ({key}) REFERENCES {referenced} ({column}), but the vertex table is \
             {} KEY ({}); REFERENCES must name the vertex table and its key",
            vertices.table, vertices.key
        )));
    }
    Ok(key)
}

/// Whether two references name the same table: equal, or one is the other
/// with a qualifier left out (`members` and `public.members`).
fn same_table(a: &TableReference, b: &TableReference) -> bool {
    if a == b {
        return true;
    }
    a.table() == b.table()
        && (a.schema().is_none() || b.schema().is_none() || a.schema() == b.schema())
        && (a.catalog().is_none() || b.catalog().is_none() || a.catalog() == b.catalog())
}

/// A graph's name: one identifier, since it is also a schema's name.
fn graph_name(name: ObjectName, normalize: bool) -> Result<String> {
    let parts: Vec<Ident> = name
        .0
        .iter()
        .map(|part| part.as_ident().cloned())
        .collect::<Option<_>>()
        .unwrap_or_default();
    match <[Ident; 1]>::try_from(parts) {
        Ok([ident]) => Ok(IdentNormalizer::new(normalize).normalize(ident)),
        Err(_) => Err(DataFusionError::Plan(format!(
            "a graph name is a single identifier, not '{name}': the graph's name is also the \
             name of the schema holding its nodes and edges tables"
        ))),
    }
}

fn normalizes(ctx: &SessionContext) -> bool {
    ctx.state()
        .config()
        .options()
        .sql_parser
        .enable_ident_normalization
}

/// The result of a DDL statement: no rows, no columns (as `CREATE VIEW`).
fn empty(ctx: &SessionContext) -> Result<DataFrame> {
    Ok(DataFrame::new(
        ctx.state(),
        LogicalPlanBuilder::empty(false).build()?,
    ))
}
