//! Defining and dropping graphs: what `CREATE PROPERTY GRAPH` and
//! `DROP PROPERTY GRAPH` do once parsed.
//!
//! A definition is checked when it is created: its tables exist, element
//! names are unique, every vertex table has a key (written, or inferred from a
//! primary key or the edges' `REFERENCES`, as DuckPGQ does), the named columns
//! and properties exist, each edge table's endpoints have one node id type (an
//! integer of any width, or a string), and each vertex key's type is
//! compatible with the endpoints that refer to it. A table given as a file
//! path is opened then (see the [module docs](super)).
//!
//! The tables are read afresh by every query. [`check_ddl`] keeps them from
//! being dropped, or replaced by ones the definition no longer fits, by SQL
//! while a graph uses them.

use std::sync::Arc;

use datafusion::arrow::datatypes::{DataType, Schema, SchemaRef};
use datafusion::catalog::{CatalogProvider, TableProvider};
use datafusion::common::utils::datafusion_strsim::levenshtein;
use datafusion::common::Constraint;
use datafusion::common::{ResolvedTableReference, SchemaReference, TableReference};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionState;
use datafusion::logical_expr::{DdlStatement, LogicalPlan};
use datafusion::prelude::SessionContext;

use super::schema::{graph_table, lookup_table, GraphSchema, EDGES, ID, NODES};
use super::{
    registry, EdgeTable, FileTable, GraphRegistry, Label, Properties, PropertyGraph, VertexTable,
};
use crate::file_table::open_file_table;
use crate::paths::looks_like_location;

/// The schema DataFusion reserves for `information_schema` views.
const INFORMATION_SCHEMA: &str = "information_schema";

/// A graph as `CREATE PROPERTY GRAPH` wrote it, its identifiers normalized,
/// before [`create_graph`] resolves its vertex keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSpec {
    pub name: String,
    pub vertex_tables: Vec<VertexSpec>,
    pub edge_tables: Vec<EdgeSpec>,
}

/// A vertex table as written: its `KEY` may be left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexSpec {
    pub name: String,
    pub table: TableReference,
    pub key: Option<String>,
    pub labels: Vec<Label>,
}

/// An edge table as written, with the key column each `REFERENCES` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeSpec {
    pub edge: EdgeTable,
    /// `SOURCE KEY (…) REFERENCES vertex (column)`: the column.
    pub source_column: Option<String>,
    /// `DESTINATION KEY (…) REFERENCES vertex (column)`: the column.
    pub destination_column: Option<String>,
}

/// A graph of one edge table and at most one vertex table, as a spec: what
/// every graph was before #295, and what tests build.
impl From<super::GraphDef> for GraphSpec {
    fn from(def: super::GraphDef) -> Self {
        GraphSpec {
            name: def.name,
            vertex_tables: def
                .vertices
                .into_iter()
                .map(|v| VertexSpec {
                    name: v.name,
                    table: v.table,
                    key: Some(v.key),
                    labels: v.labels,
                })
                .collect(),
            edge_tables: vec![EdgeSpec {
                edge: def.edges,
                source_column: None,
                destination_column: None,
            }],
        }
    }
}

/// Define `spec` in `ctx`, as `CREATE [OR REPLACE] PROPERTY GRAPH [IF NOT EXISTS]`
/// does. Follows DataFusion's `CREATE TABLE`: an existing graph is an error,
/// unless `or_replace` (replace it) or `if_not_exists` (keep it, do nothing).
pub async fn create_graph(
    ctx: &SessionContext,
    spec: impl Into<GraphSpec>,
    or_replace: bool,
    if_not_exists: bool,
) -> Result<()> {
    let spec = spec.into();
    if or_replace && if_not_exists {
        return plan_err("'IF NOT EXISTS' cannot coexist with 'REPLACE'");
    }
    let state = ctx.state();
    let graphs = registry(&state);
    if graphs.get(&spec.name).is_some() {
        if if_not_exists {
            return Ok(());
        }
        if !or_replace {
            return plan_err(format!("graph '{}' already exists", spec.name));
        }
    } else if spec.name == INFORMATION_SCHEMA
        || default_catalog(&state)?.schema(&spec.name).is_some()
    {
        return plan_err(format!(
            "cannot name a graph '{}': a schema of that name already exists, and a graph's \
             name is the name of the schema holding its nodes and edges tables",
            spec.name
        ));
    }
    check_element_names(&spec)?;

    let mut files = Vec::new();
    let mut vertex_providers = Vec::new();
    for v in &spec.vertex_tables {
        vertex_providers.push(open_table(&state, &v.table, &mut files).await?);
    }
    let mut edge_schemas = Vec::new();
    for e in &spec.edge_tables {
        edge_schemas.push(
            open_table(&state, &e.edge.table, &mut files)
                .await?
                .schema(),
        );
    }
    let def = resolve_keys(&spec, &vertex_providers)?;
    let mut schemas: Vec<(TableReference, SchemaRef)> = Vec::new();
    for (v, provider) in def.vertex_tables.iter().zip(&vertex_providers) {
        schemas.push((v.table.clone(), provider.schema()));
    }
    for (e, schema) in def.edge_tables.iter().zip(edge_schemas) {
        schemas.push((e.table.clone(), schema));
    }
    validate(&def, &|t| {
        schemas
            .iter()
            .find(|(table, _)| table == t)
            .map(|(_, s)| Arc::clone(s))
            .expect("every table of the graph was opened")
    })?;

    let name = def.name.clone();
    let has_tables = def.default_pair().is_some();
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
    if has_tables {
        for table in [NODES, EDGES] {
            schema.table(table).await?;
        }
    }
    Ok(())
}

/// Element names (an `AS` name, or else the table's name) are unique across
/// a graph's vertex and edge tables, as SQL/PGQ requires; `REFERENCES` names
/// a vertex table by its element name.
fn check_element_names(spec: &GraphSpec) -> Result<()> {
    let names: Vec<&str> = spec
        .vertex_tables
        .iter()
        .map(|v| v.name.as_str())
        .chain(spec.edge_tables.iter().map(|e| e.edge.name.as_str()))
        .collect();
    for (i, name) in names.iter().enumerate() {
        if names[..i].iter().any(|n| n.eq_ignore_ascii_case(name)) {
            return plan_err(format!(
                "graph '{}' has two element tables named '{name}'; give one a different name \
                 with AS",
                spec.name
            ));
        }
    }
    for e in &spec.edge_tables {
        for (side, key, vertex, column) in endpoints(e) {
            let (Some(vertex), Some(column)) = (vertex, column) else {
                continue;
            };
            if spec.vertex_tables.iter().any(|v| &v.name == vertex) {
                continue;
            }
            return plan_err(match spec.vertex_tables.as_slice() {
                [] => format!(
                    "{side} KEY ({key}) REFERENCES {vertex}, but the graph has no VERTEX \
                     TABLES; add VERTEX TABLES ({vertex} KEY ({column})) or drop the \
                     REFERENCES clause"
                ),
                [only] => format!(
                    "{side} KEY ({key}) REFERENCES {vertex} ({column}), but the vertex table is \
                     {} KEY ({}); REFERENCES must name the vertex table and its key",
                    only.table,
                    only.key.as_deref().unwrap_or("…")
                ),
                several => {
                    let names: Vec<&str> = several.iter().map(|v| v.name.as_str()).collect();
                    format!(
                        "{side} KEY ({key}) of edge table '{}' REFERENCES {vertex}, which is not \
                         a vertex table of the graph; its vertex tables are {}",
                        e.edge.name,
                        names.join(", ")
                    )
                }
            });
        }
        if spec.vertex_tables.len() > 1
            && (e.edge.source_vertex.is_none() || e.edge.destination_vertex.is_none())
        {
            let side = if e.edge.source_vertex.is_none() {
                "SOURCE"
            } else {
                "DESTINATION"
            };
            return plan_err(format!(
                "the {side} of edge table '{}' needs REFERENCES: graph '{}' has several vertex \
                 tables, so say which one it refers to",
                e.edge.name, spec.name
            ));
        }
    }
    Ok(())
}

/// The graph with every vertex key resolved: `KEY (column)` as written, else
/// the table's primary key, else the column the edges' `REFERENCES` name (as
/// DuckPGQ infers it). A `REFERENCES` column must be the key.
fn resolve_keys(spec: &GraphSpec, providers: &[Arc<dyn TableProvider>]) -> Result<PropertyGraph> {
    let mut vertex_tables = Vec::new();
    for (v, provider) in spec.vertex_tables.iter().zip(providers) {
        // (side, endpoint column, referenced column) of each endpoint that
        // REFERENCES this vertex table.
        let referenced: Vec<(&str, &str, &str)> = spec
            .edge_tables
            .iter()
            .flat_map(endpoints)
            .filter_map(|(side, key, vertex, column)| match (vertex, column) {
                (Some(vertex), Some(column)) if vertex == &v.name => {
                    Some((side, key, column.as_str()))
                }
                _ => None,
            })
            .collect();
        let key = match &v.key {
            Some(key) => key.clone(),
            None => match primary_key(provider.as_ref()) {
                Some(key) => key,
                None => match referenced.first() {
                    Some((_, _, column)) => column.to_string(),
                    None => {
                        return plan_err(format!(
                            "vertex table '{}' needs a key: write KEY (column), declare a \
                             primary key, or have an edge table REFERENCES it",
                            v.name
                        ))
                    }
                },
            },
        };
        if let Some((side, endpoint, column)) = referenced.iter().find(|(_, _, c)| *c != key) {
            return plan_err(format!(
                "{side} KEY ({endpoint}) REFERENCES {} ({column}), but the vertex table is {} \
                 KEY ({key}); REFERENCES must name the vertex table and its key",
                v.name, v.table
            ));
        }
        vertex_tables.push(VertexTable {
            name: v.name.clone(),
            table: v.table.clone(),
            key,
            labels: v.labels.clone(),
        });
    }
    Ok(PropertyGraph {
        name: spec.name.clone(),
        vertex_tables,
        edge_tables: spec.edge_tables.iter().map(|e| e.edge.clone()).collect(),
    })
}

/// An edge table's two endpoints: (side, column, referenced vertex, referenced
/// column).
fn endpoints(e: &EdgeSpec) -> [(&'static str, &str, Option<&String>, Option<&String>); 2] {
    [
        (
            "SOURCE",
            e.edge.source.as_str(),
            e.edge.source_vertex.as_ref(),
            e.source_column.as_ref(),
        ),
        (
            "DESTINATION",
            e.edge.destination.as_str(),
            e.edge.destination_vertex.as_ref(),
            e.destination_column.as_ref(),
        ),
    ]
}

/// The table's declared single-column primary key, if it has one.
fn primary_key(provider: &dyn TableProvider) -> Option<String> {
    let schema = provider.schema();
    provider.constraints()?.iter().find_map(|c| match c {
        Constraint::PrimaryKey(columns) if columns.len() == 1 => {
            Some(schema.field(columns[0]).name().clone())
        }
        _ => None,
    })
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
        let mut schemas: Vec<(TableReference, SchemaRef)> = Vec::new();
        for t in def.tables() {
            let schema =
                if graphs.file_table(&def.name, t).is_none() && resolve(state, t) == replaced {
                    Arc::new(schema.clone())
                } else {
                    graph_table(state, &def.name, t).await?.schema()
                };
            schemas.push((t.clone(), schema));
        }
        let schema_of = |t: &TableReference| {
            schemas
                .iter()
                .find(|(table, _)| table == t)
                .map(|(_, s)| Arc::clone(s))
                .expect("every table of the graph was looked up")
        };
        if let Err(e) = validate(def, &schema_of) {
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
) -> Vec<Arc<PropertyGraph>> {
    let graphs = registry(state);
    graphs
        .all()
        .into_iter()
        .filter(|def| {
            def.tables()
                .any(|t| graphs.file_table(&def.name, t).is_none() && uses(&resolve(state, t)))
        })
        .collect()
}

/// "graph 'g' uses it" or "graphs 'g', 'h' use it".
fn used_by(graphs: &[Arc<PropertyGraph>]) -> String {
    let names: Vec<String> = graphs.iter().map(|g| format!("'{}'", g.name)).collect();
    if names.len() == 1 {
        format!("graph {} uses it", names[0])
    } else {
        format!("graphs {} use it", names.join(", "))
    }
}

/// "property graph g" or "property graphs g, h".
fn drop_statements(graphs: &[Arc<PropertyGraph>]) -> String {
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

/// Check `def` against its tables' columns (`schema_of`): every named column
/// and property exists, each edge table's endpoints have one node id type,
/// and each vertex key is a node id type compatible with the endpoints that
/// refer to it.
fn validate(def: &PropertyGraph, schema_of: &dyn Fn(&TableReference) -> SchemaRef) -> Result<()> {
    // Edge tables first, then what is left of the vertex tables: the order a
    // one-pair graph's checks have always run in, so its errors are unchanged.
    for e in &def.edge_tables {
        let edges = schema_of(&e.table);
        let table = &e.table;
        let source = column_type(&edges, table, &e.source, "SOURCE KEY")?;
        let destination = column_type(&edges, table, &e.destination, "DESTINATION KEY")?;
        if source != destination {
            return plan_err(format!(
                "the source and destination columns of edge table '{table}' must have the same \
                 type, but {} is {source} and {} is {destination}",
                e.source, e.destination
            ));
        }
        let Some(family) = IdFamily::of(&source) else {
            return plan_err(format!(
                "the endpoints of edge table '{table}' ({} and {}) are {source}; node ids must \
                 be integers or strings",
                e.source, e.destination
            ));
        };
        for v in [def.endpoint_vertex(e, true), def.endpoint_vertex(e, false)]
            .into_iter()
            .flatten()
        {
            let key = column_type(&schema_of(&v.table), &v.table, &v.key, "KEY")?;
            match IdFamily::of(&key) {
                None => return not_an_id_type(v, &key),
                Some(f) if f != family => {
                    return plan_err(format!(
                        "the key of vertex table '{}' ({}) is {key}, but the endpoints of edge \
                         table '{table}' are {source}; both must be integers or both strings",
                        v.table, v.key
                    ))
                }
                Some(_) => {}
            }
        }
        check_properties(&e.labels, &edges, table)?;
    }
    for v in &def.vertex_tables {
        let vertices = schema_of(&v.table);
        let key = column_type(&vertices, &v.table, &v.key, "KEY")?;
        if IdFamily::of(&key).is_none() {
            return not_an_id_type(v, &key);
        }
        if v.key != ID && vertices.field_with_name(ID).is_ok() {
            return plan_err(format!(
                "vertex table '{}' has a column named '{ID}' that is not its key ({}); \
                 {}.nodes names the key '{ID}', so rename that column (in a view) first",
                v.table, v.key, def.name
            ));
        }
        check_properties(&v.labels, &vertices, &v.table)?;
    }
    Ok(())
}

fn not_an_id_type<T>(v: &VertexTable, key: &DataType) -> Result<T> {
    plan_err(format!(
        "the key of vertex table '{}' ({}) is {key}; node ids must be integers or strings",
        v.table, v.key
    ))
}

/// Every column a label's properties name exists in the table.
fn check_properties(labels: &[Label], schema: &Schema, table: &TableReference) -> Result<()> {
    for label in labels {
        let columns: Vec<&String> = match &label.properties {
            Properties::AllColumns { except } => except.iter().collect(),
            Properties::Columns(columns) => columns.iter().map(|(c, _)| c).collect(),
            Properties::None => Vec::new(),
        };
        for column in columns {
            column_type(
                schema,
                table,
                column,
                &format!("label {} property", label.name),
            )?;
        }
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
