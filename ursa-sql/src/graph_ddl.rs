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
use ursa_engine::graph::{self, EdgeSpec, EdgeTable, GraphSpec, Label, Properties, VertexSpec};

use crate::parser::{CreatePropertyGraph, DescribeGraph, DropPropertyGraph};
use crate::property_graph::{EndpointDef, LabelDef, PropertiesDef, References};

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

/// The definition `statement` makes, with its identifiers normalized. Vertex
/// keys left out are resolved by the engine, which can see the tables.
fn graph_def(statement: CreatePropertyGraph, normalize: bool) -> Result<GraphSpec> {
    let normalizer = IdentNormalizer::new(normalize);
    let name = graph_name(statement.name, normalize)?;
    let mut vertex_tables = Vec::new();
    for v in statement.vertices {
        let table = object_name_to_table_reference(v.table, normalize)?;
        let element = element_name(v.alias, &table, &normalizer);
        vertex_tables.push(VertexSpec {
            labels: labels(v.labels, &element, &normalizer),
            name: element,
            table,
            key: v.key.map(|k| normalizer.normalize(k)),
        });
    }
    let mut edge_tables = Vec::new();
    for e in statement.edges {
        let table = object_name_to_table_reference(e.table, normalize)?;
        let element = element_name(e.alias, &table, &normalizer);
        let (source, source_vertex, source_column) = endpoint(e.source, &vertex_tables, normalize)?;
        let (destination, destination_vertex, destination_column) =
            endpoint(e.destination, &vertex_tables, normalize)?;
        edge_tables.push(EdgeSpec {
            edge: EdgeTable {
                labels: labels(e.labels, &element, &normalizer),
                name: element,
                table,
                source,
                destination,
                source_vertex,
                destination_vertex,
            },
            source_column,
            destination_column,
        });
    }
    Ok(GraphSpec {
        name,
        vertex_tables,
        edge_tables,
    })
}

/// An element table's name: its `AS` name, or else its table's name.
fn element_name(
    alias: Option<Ident>,
    table: &TableReference,
    normalizer: &IdentNormalizer,
) -> String {
    match alias {
        Some(alias) => normalizer.normalize(alias),
        None => table.table().to_string(),
    }
}

/// The labels as written; the default label (`DEFAULT LABEL`, or a bare
/// `PROPERTIES` clause) is named after the element table.
fn labels(labels: Vec<LabelDef>, element: &str, normalizer: &IdentNormalizer) -> Vec<Label> {
    labels
        .into_iter()
        .map(|label| {
            let properties = match label.properties {
                PropertiesDef::Default => Properties::AllColumns { except: Vec::new() },
                PropertiesDef::AllColumns { except } => Properties::AllColumns {
                    except: except
                        .into_iter()
                        .map(|c| normalizer.normalize(c))
                        .collect(),
                },
                PropertiesDef::List(columns) => Properties::Columns(
                    columns
                        .into_iter()
                        .map(|(column, alias)| {
                            let column = normalizer.normalize(column);
                            let name =
                                alias.map_or_else(|| column.clone(), |a| normalizer.normalize(a));
                            (column, name)
                        })
                        .collect(),
                ),
                PropertiesDef::None => Properties::None,
            };
            Label {
                name: label
                    .name
                    .map_or_else(|| element.to_string(), |name| normalizer.normalize(name)),
                properties,
            }
        })
        .collect()
}

/// The endpoint column, the vertex table (element name) its `REFERENCES`
/// names, and the column it references.
fn endpoint(
    endpoint: EndpointDef,
    vertices: &[VertexSpec],
    normalize: bool,
) -> Result<(String, Option<String>, Option<String>)> {
    let normalizer = IdentNormalizer::new(normalize);
    let key = normalizer.normalize(endpoint.key);
    let Some(References { table, column }) = endpoint.references else {
        return Ok((key, None, None));
    };
    let referenced = object_name_to_table_reference(table, normalize)?;
    let column = normalizer.normalize(column);
    // A vertex table by its element name, else by its table's name.
    let vertex = vertices
        .iter()
        .find(|v| matches!(&referenced, TableReference::Bare { table } if table.as_ref() == v.name))
        .or_else(|| vertices.iter().find(|v| same_table(&referenced, &v.table)))
        .map(|v| v.name.clone())
        .unwrap_or_else(|| referenced.table().to_string());
    Ok((key, Some(vertex), Some(column)))
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
pub(crate) fn graph_name(name: ObjectName, normalize: bool) -> Result<String> {
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

pub(crate) fn normalizes(ctx: &SessionContext) -> bool {
    ctx.state()
        .config()
        .options()
        .sql_parser
        .enable_ident_normalization
}

/// The result of a DDL statement: no rows, no columns (as `CREATE VIEW`).
pub(crate) fn empty(ctx: &SessionContext) -> Result<DataFrame> {
    Ok(DataFrame::new(
        ctx.state(),
        LogicalPlanBuilder::empty(false).build()?,
    ))
}
