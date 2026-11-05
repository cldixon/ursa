//! Defining and dropping graphs: what `CREATE PROPERTY GRAPH` and
//! `DROP PROPERTY GRAPH` do once parsed.
//!
//! A definition is checked when it is created: its tables exist, the named
//! columns exist in them, the endpoints have one node id type (an integer of
//! any width, or a string), and the vertex key's type is compatible with it.
//! A table given as a file path is opened then (see the [module
//! docs](super)).
//!
//! The tables are read afresh by every query. [`check_ddl`] keeps them from
//! being dropped, or replaced by ones the definition no longer fits, by SQL
//! while a graph uses them.

use std::sync::Arc;

use datafusion::arrow::datatypes::{DataType, Schema};
use datafusion::catalog::{CatalogProvider, TableProvider};
use datafusion::common::utils::datafusion_strsim::levenshtein;
use datafusion::common::{ResolvedTableReference, SchemaReference, TableReference};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionState;
use datafusion::logical_expr::{DdlStatement, LogicalPlan};
use datafusion::prelude::SessionContext;

use super::schema::{graph_table, lookup_table, GraphSchema, EDGES, ID, NODES};
use super::{registry, FileTable, GraphDef, GraphRegistry};
use crate::file_table::open_file_table;
use crate::paths::looks_like_location;

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

    let mut files = Vec::new();
    let edges = open_table(&state, &def.edges.table, &mut files).await?;
    let vertices = match &def.vertices {
        Some(v) => Some(open_table(&state, &v.table, &mut files).await?),
        None => None,
    };
    validate(
        &def,
        &edges.schema(),
        vertices.as_ref().map(|v| v.schema()).as_deref(),
    )?;

    let name = def.name.clone();
    graphs.put_with_files(def, files);
    let catalog = default_catalog_inner(&state)?;
    let schema = match catalog.schema(&name) {
        Some(schema) => schema,
        None => {
            let schema = Arc::new(GraphSchema::new(name.clone(), ctx.state_weak_ref()));
            catalog.register_schema(&name, Arc::clone(&schema) as _)?;
            schema
        }
    };
    // Plan both tables once, so their columns are known to listings even if a
    // table under the graph later disappears.
    for table in [NODES, EDGES] {
        schema.table(table).await?;
    }
    Ok(())
}

/// The table `table` names: a table or view in the session, or else, when it
/// looks like a file path or URL, that file opened as a listing table (added
/// to `files`).
async fn open_table(
    state: &SessionState,
    table: &TableReference,
    files: &mut Vec<FileTable>,
) -> Result<Arc<dyn TableProvider>> {
    if let Some(provider) = session_table(state, table).await? {
        return Ok(provider);
    }
    if let TableReference::Bare { table: location } = table {
        if looks_like_location(location) {
            let provider = open_file_table(state, location).await?;
            files.push(FileTable {
                table: table.clone(),
                provider: Arc::clone(&provider),
            });
            return Ok(provider);
        }
    }
    find_table(state, table).await
}

/// The table or view `table` names in the session's catalogs, not opening
/// files.
async fn session_table(
    state: &SessionState,
    table: &TableReference,
) -> Result<Option<Arc<dyn TableProvider>>> {
    let Some(catalogs) = registry(state).catalogs().cloned() else {
        return lookup_table(state, table).await;
    };
    let resolved = resolve(state, table);
    match catalogs
        .catalog(&resolved.catalog)
        .and_then(|c| c.schema(&resolved.schema))
    {
        Some(schema) => schema.table(&resolved.table).await,
        None => Ok(None),
    }
}

/// The error for a graph that does not exist, with a suggestion.
pub fn unknown_graph(name: &str, graphs: &GraphRegistry) -> DataFusionError {
    let names = graphs.names();
    let hint = match closest_name(name, names.iter().map(String::as_str)) {
        Some(s) => format!("; did you mean '{s}'?"),
        None if names.is_empty() => {
            "; no graphs are defined (define one with CREATE PROPERTY GRAPH)".to_string()
        }
        None => format!(". The graphs are: {}", names.join(", ")),
    };
    DataFusionError::Plan(format!("graph '{name}' does not exist{hint}"))
}

/// Refuse a DDL statement, planned as `plan`, that would break a graph:
///
/// - `DROP TABLE` or `DROP VIEW` of a table a graph uses;
/// - `DROP SCHEMA` of a graph's schema, or of a schema holding a table a graph
///   uses;
/// - `CREATE OR REPLACE` of a table a graph uses, by one whose columns the
///   graph's definition no longer fits (see [`check_replace`]).
///
/// Every other plan passes.
pub async fn check_ddl(state: &SessionState, plan: &LogicalPlan) -> Result<()> {
    let LogicalPlan::Ddl(ddl) = plan else {
        return Ok(());
    };
    match ddl {
        DdlStatement::DropTable(drop) => check_drop(state, "table", &drop.name),
        DdlStatement::DropView(drop) => check_drop(state, "view", &drop.name),
        DdlStatement::DropCatalogSchema(drop) => check_drop_schema(state, &drop.name),
        DdlStatement::CreateView(create) if create.or_replace => {
            let schema = create.input.schema().as_arrow();
            check_replace(state, "view", &create.name, Some(schema)).await
        }
        DdlStatement::CreateMemoryTable(create) if create.or_replace => {
            let schema = create.input.schema().as_arrow();
            check_replace(state, "table", &create.name, Some(schema)).await
        }
        DdlStatement::CreateExternalTable(create) if create.or_replace => {
            // An empty schema is inferred from the files when the statement
            // runs, so it cannot be checked here.
            let schema = create.schema.as_arrow();
            let known = (!schema.fields().is_empty()).then_some(schema);
            check_replace(state, "table", &create.name, known).await
        }
        _ => Ok(()),
    }
}

/// Refuse to drop `table` (a `kind`: "table" or "view") while a graph uses it.
pub fn check_drop(state: &SessionState, kind: &str, table: &TableReference) -> Result<()> {
    let dropped = resolve(state, table);
    let users = graphs_using(state, |t| t == &dropped);
    if users.is_empty() {
        return Ok(());
    }
    plan_err(format!(
        "cannot drop {kind} '{table}': {}; drop {} first",
        used_by(&users),
        drop_statements(&users)
    ))
}

/// Refuse to drop the schema `schema` while it is a graph's, or holds a table
/// a graph uses.
pub fn check_drop_schema(state: &SessionState, schema: &SchemaReference) -> Result<()> {
    let options = &state.config_options().catalog;
    let (catalog, name) = match schema {
        SchemaReference::Bare { schema } => (options.default_catalog.as_str(), schema.as_ref()),
        SchemaReference::Full { schema, catalog } => (catalog.as_ref(), schema.as_ref()),
    };
    if catalog == options.default_catalog && registry(state).get(name).is_some() {
        return plan_err(format!(
            "cannot drop schema '{name}': it holds the tables of graph '{name}'; drop the \
             graph with DROP PROPERTY GRAPH {name}"
        ));
    }
    let users = graphs_using(state, |t| {
        t.catalog.as_ref() == catalog && t.schema.as_ref() == name
    });
    if users.is_empty() {
        return Ok(());
    }
    plan_err(format!(
        "cannot drop schema '{name}': {} a table in it; drop {} first",
        if users.len() == 1 {
            format!("graph '{}' uses", users[0].name)
        } else {
            used_by(&users).replace(" use it", " use")
        },
        drop_statements(&users)
    ))
}

/// Refuse to replace `table` (a `kind`) by one with the columns `schema`
/// while a graph uses it and its definition does not fit those columns.
/// `None`: the new columns are not known yet, so the replacement is refused
/// whenever a graph uses the table.
pub async fn check_replace(
    state: &SessionState,
    kind: &str,
    table: &TableReference,
    schema: Option<&Schema>,
) -> Result<()> {
    let replaced = resolve(state, table);
    let users = graphs_using(state, |t| t == &replaced);
    if users.is_empty() {
        return Ok(());
    }
    let Some(schema) = schema else {
        return plan_err(format!(
            "cannot replace {kind} '{table}' by one whose columns are not known yet: {}; \
             drop {} first",
            used_by(&users),
            drop_statements(&users)
        ));
    };
    let graphs = registry(state);
    for def in &users {
        let mut schemas = Vec::new();
        for t in std::iter::once(&def.edges.table).chain(def.vertices.as_ref().map(|v| &v.table)) {
            let schema =
                if graphs.file_table(&def.name, t).is_none() && resolve(state, t) == replaced {
                    Arc::new(schema.clone())
                } else {
                    graph_table(state, def, t).await?.schema()
                };
            schemas.push(schema);
        }
        if let Err(e) = validate(def, &schemas[0], schemas.get(1).map(|s| s.as_ref())) {
            return plan_err(format!(
                "cannot replace {kind} '{table}': graph '{}' uses it, and the new {kind} does \
                 not fit its definition ({}). Keep the columns the graph uses, or drop \
                 property graph {} first",
                def.name,
                crate::user_message(&e),
                def.name
            ));
        }
    }
    Ok(())
}

/// The graphs with a session table (not a file path) for which `uses` holds.
fn graphs_using(
    state: &SessionState,
    uses: impl Fn(&ResolvedTableReference) -> bool,
) -> Vec<Arc<GraphDef>> {
    let graphs = registry(state);
    graphs
        .all()
        .into_iter()
        .filter(|def| {
            std::iter::once(&def.edges.table)
                .chain(def.vertices.as_ref().map(|v| &v.table))
                .any(|t| graphs.file_table(&def.name, t).is_none() && uses(&resolve(state, t)))
        })
        .collect()
}

/// "graph 'g' uses it" or "graphs 'g', 'h' use it".
fn used_by(graphs: &[Arc<GraphDef>]) -> String {
    let names: Vec<String> = graphs.iter().map(|g| format!("'{}'", g.name)).collect();
    if names.len() == 1 {
        format!("graph {} uses it", names[0])
    } else {
        format!("graphs {} use it", names.join(", "))
    }
}

/// "property graph g" or "property graphs g, h".
fn drop_statements(graphs: &[Arc<GraphDef>]) -> String {
    let names: Vec<&str> = graphs.iter().map(|g| g.name.as_str()).collect();
    if names.len() == 1 {
        format!("property graph {}", names[0])
    } else {
        format!("property graphs {}", names.join(", "))
    }
}

/// `table` with the session's default catalog and schema filled in.
fn resolve(state: &SessionState, table: &TableReference) -> ResolvedTableReference {
    let options = &state.config_options().catalog;
    table
        .clone()
        .resolve(&options.default_catalog, &options.default_schema)
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
        return Err(unknown_graph(name, &graphs));
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

/// Check that `def`'s columns exist in its tables, whose columns are `edges`
/// and `vertices`, and have node id types.
fn validate(def: &GraphDef, edges: &Schema, vertices: Option<&Schema>) -> Result<()> {
    let table = &def.edges.table;
    let source = column_type(edges, table, &def.edges.source, "SOURCE KEY")?;
    let destination = column_type(edges, table, &def.edges.destination, "DESTINATION KEY")?;
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

    let (Some(v), Some(vertices)) = (&def.vertices, vertices) else {
        return Ok(());
    };
    let key = column_type(vertices, &v.table, &v.key, "KEY")?;
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
