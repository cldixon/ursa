//! Defining and dropping graphs: what `CREATE PROPERTY GRAPH` and
//! `DROP PROPERTY GRAPH` do once parsed.
//!
//! A definition is checked when it is created: its tables exist, the named
//! columns exist in them, the endpoints have one node id type (an integer of
//! any width, or a string), and the vertex key's type is compatible with it.
//! The tables are read afresh by every query, so a later change to them (a
//! dropped view, say) shows up as an error when the graph is next queried.

use std::sync::Arc;

use datafusion::arrow::datatypes::{DataType, Schema};
use datafusion::catalog::{CatalogProvider, TableProvider};
use datafusion::common::utils::datafusion_strsim::levenshtein;
use datafusion::common::TableReference;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionState;
use datafusion::prelude::SessionContext;

use super::schema::{lookup_table, GraphSchema, ID};
use super::{registry, GraphDef};

/// The schema DataFusion reserves for `information_schema` views.
const INFORMATION_SCHEMA: &str = "information_schema";

/// Define `def` in `ctx`, as `CREATE [OR REPLACE] PROPERTY GRAPH [IF NOT EXISTS]`
/// does. Follows DataFusion's `CREATE TABLE`: an existing graph is an error,
/// unless `or_replace` (replace it) or `if_not_exists` (keep it, do nothing).
pub async fn create_graph(
    ctx: &SessionContext,
    def: GraphDef,
    or_replace: bool,
    if_not_exists: bool,
) -> Result<()> {
    if or_replace && if_not_exists {
        return plan_err("'IF NOT EXISTS' cannot coexist with 'REPLACE'");
    }
    let state = ctx.state();
    let graphs = registry(&state);
    if graphs.get(&def.name).is_some() {
        if if_not_exists {
            return Ok(());
        }
        if !or_replace {
            return plan_err(format!("graph '{}' already exists", def.name));
        }
    } else if def.name == INFORMATION_SCHEMA || default_catalog(&state)?.schema(&def.name).is_some()
    {
        return plan_err(format!(
            "cannot name a graph '{}': a schema of that name already exists, and a graph's \
             name is the name of the schema holding its nodes and edges tables",
            def.name
        ));
    }

    validate(&state, &def).await?;

    let name = def.name.clone();
    graphs.put(def);
    let catalog = default_catalog_inner(&state)?;
    if catalog.schema(&name).is_none() {
        catalog.register_schema(
            &name,
            Arc::new(GraphSchema::new(name.clone(), ctx.state_weak_ref())),
        )?;
    }
    Ok(())
}

/// Drop the graph called `name` and its schema, as `DROP PROPERTY GRAPH
/// [IF EXISTS]` does.
pub fn drop_graph(ctx: &SessionContext, name: &str, if_exists: bool) -> Result<()> {
    let state = ctx.state();
    let graphs = registry(&state);
    if graphs.remove(name).is_none() {
        if if_exists {
            return Ok(());
        }
        let names = graphs.names();
        return plan_err(format!(
            "graph '{name}' does not exist{}",
            did_you_mean(name, names.iter().map(String::as_str))
        ));
    }
    let catalog = default_catalog_inner(&state)?;
    if catalog
        .schema(name)
        .is_some_and(|schema| schema.is::<GraphSchema>())
    {
        catalog.deregister_schema(name, true)?;
    }
    Ok(())
}

/// The closest of `candidates` to `name` by edit distance, if any is close
/// enough to be a likely typo: for "did you mean" suggestions.
pub fn closest_name<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let limit = (name.chars().count() / 3).max(2);
    candidates
        .into_iter()
        .map(|c| (levenshtein(name, c), c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, c)| (*d, *c))
        .map(|(_, c)| c)
}

/// "; did you mean 'x'?" for the closest candidate, or nothing.
fn did_you_mean<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> String {
    closest_name(name, candidates)
        .map(|c| format!("; did you mean '{c}'?"))
        .unwrap_or_default()
}

/// Check that `def`'s tables and columns exist and have node id types.
async fn validate(state: &SessionState, def: &GraphDef) -> Result<()> {
    let edges = find_table(state, &def.edges.table).await?.schema();
    let table = &def.edges.table;
    let source = column_type(&edges, table, &def.edges.source, "SOURCE KEY")?;
    let destination = column_type(&edges, table, &def.edges.destination, "DESTINATION KEY")?;
    if source != destination {
        return plan_err(format!(
            "the source and destination columns of edge table '{table}' must have the same \
             type, but {} is {source} and {} is {destination}",
            def.edges.source, def.edges.destination
        ));
    }
    let Some(family) = IdFamily::of(&source) else {
        return plan_err(format!(
            "the endpoints of edge table '{table}' ({} and {}) are {source}; node ids must \
             be integers or strings",
            def.edges.source, def.edges.destination
        ));
    };

    let Some(v) = &def.vertices else {
        return Ok(());
    };
    let vertices = find_table(state, &v.table).await?.schema();
    let key = column_type(&vertices, &v.table, &v.key, "KEY")?;
    match IdFamily::of(&key) {
        None => {
            return plan_err(format!(
                "the key of vertex table '{}' ({}) is {key}; node ids must be integers or \
                 strings",
                v.table, v.key
            ))
        }
        Some(f) if f != family => {
            return plan_err(format!(
                "the key of vertex table '{}' ({}) is {key}, but the edge endpoints are \
                 {source}; both must be integers or both strings",
                v.table, v.key
            ))
        }
        Some(_) => {}
    }
    if v.key != ID && vertices.field_with_name(ID).is_ok() {
        return plan_err(format!(
            "vertex table '{}' has a column named '{ID}' that is not its key ({}); \
             {}.nodes names the key '{ID}', so rename that column (in a view) first",
            v.table, v.key, def.name
        ));
    }
    Ok(())
}

/// Node ids are integers of any width, or strings.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum IdFamily {
    Integer,
    String,
}

impl IdFamily {
    fn of(data_type: &DataType) -> Option<Self> {
        match data_type {
            t if t.is_integer() => Some(Self::Integer),
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Some(Self::String),
            DataType::Dictionary(_, value) => match Self::of(value) {
                Some(Self::String) => Some(Self::String),
                _ => None,
            },
            _ => None,
        }
    }
}

/// The table `table` names, or an error with a suggestion.
async fn find_table(
    state: &SessionState,
    table: &TableReference,
) -> Result<Arc<dyn TableProvider>> {
    match lookup_table(state, table).await? {
        Some(provider) => Ok(provider),
        None => {
            let names = table_names(state).await;
            plan_err(format!(
                "table '{table}' not found{}",
                did_you_mean(&table.to_string(), names.iter().map(String::as_str))
            ))
        }
    }
}

/// Every table in the session, named as a query in the default schema would
/// name it.
async fn table_names(state: &SessionState) -> Vec<String> {
    let options = &state.config_options().catalog;
    let catalogs = state.catalog_list();
    let mut names = Vec::new();
    for catalog_name in catalogs.catalog_names() {
        let Some(catalog) = catalogs.catalog(&catalog_name) else {
            continue;
        };
        for schema_name in catalog.schema_names() {
            if schema_name == INFORMATION_SCHEMA {
                continue;
            }
            let Some(schema) = catalog.schema(&schema_name) else {
                continue;
            };
            for table in schema.table_names() {
                let reference = if catalog_name != options.default_catalog {
                    TableReference::full(catalog_name.as_str(), schema_name.as_str(), table)
                } else if schema_name != options.default_schema {
                    TableReference::partial(schema_name.as_str(), table)
                } else {
                    TableReference::bare(table)
                };
                names.push(reference.to_string());
            }
        }
    }
    names
}

fn column_type(
    schema: &Schema,
    table: &TableReference,
    column: &str,
    clause: &str,
) -> Result<DataType> {
    match schema.field_with_name(column) {
        Ok(field) => Ok(field.data_type().clone()),
        Err(_) => {
            let columns: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
            let hint = match closest_name(column, columns.iter().copied()) {
                Some(c) => format!("did you mean '{c}'?"),
                None => format!("its columns are {}", columns.join(", ")),
            };
            plan_err(format!(
                "{clause} column '{column}' not found in table '{table}'; {hint}"
            ))
        }
    }
}

/// The default catalog, as queries see it.
fn default_catalog(state: &SessionState) -> Result<Arc<dyn CatalogProvider>> {
    let name = &state.config_options().catalog.default_catalog;
    state
        .catalog_list()
        .catalog(name)
        .ok_or_else(|| DataFusionError::Plan(format!("the default catalog '{name}' is missing")))
}

/// The default catalog without the file-table wrapper (which cannot
/// deregister a schema), when the session was built by `new_session`.
fn default_catalog_inner(state: &SessionState) -> Result<Arc<dyn CatalogProvider>> {
    let name = &state.config_options().catalog.default_catalog;
    match registry(state).catalogs() {
        Some(catalogs) => catalogs.catalog(name).ok_or_else(|| {
            DataFusionError::Plan(format!("the default catalog '{name}' is missing"))
        }),
        None => default_catalog(state),
    }
}

fn plan_err<T>(message: impl Into<String>) -> Result<T> {
    Err(DataFusionError::Plan(message.into()))
}
