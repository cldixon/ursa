//! The `catalog` operation and the `catalog` event (`design/console.md`
//! §4.3–§4.4): what the session has defined, read from the engine's catalog
//! and graph registry without running a query.
//!
//! The event is found by comparing a fingerprint of what `catalog` would
//! answer before and after a request that may change it (`feed`, `query`,
//! `start`, `sources.attach`), not by reading the statements: `CREATE`,
//! `DROP`, `ALTER`, `ATTACH`, `CREATE PROPERTY GRAPH`, `.open`, a `.read`
//! script and a dataset load are all caught alike, and a statement that
//! failed halfway, or a request that was cancelled, still is.

use std::collections::BTreeSet;
use std::sync::Arc;

use datafusion::catalog::TableProvider;
use datafusion::datasource::listing::ListingTable;
use datafusion::datasource::TableType;
use datafusion::prelude::SessionContext;
use serde_json::{json, Value};
use ursa_engine::graph;

/// One table or view, as `catalog` lists it.
#[derive(Debug, Clone)]
pub(crate) struct TableInfo {
    pub catalog: String,
    pub schema: String,
    pub name: String,
    /// `table`, `view` or `external` (a table of files).
    pub kind: &'static str,
    /// `(name, type, nullable)`.
    pub columns: Vec<(String, String, bool)>,
}

/// What a table provider is, for `catalog`'s `kind`.
fn kind_of(provider: &dyn TableProvider) -> &'static str {
    if provider.is::<ListingTable>() {
        return "external";
    }
    match provider.table_type() {
        TableType::View => "view",
        TableType::Base | TableType::Temporary => "table",
    }
}

/// The schemas that are graphs' (`<graph>.nodes`, `<graph>.edges`): listed
/// under `graphs`, not as a database's schemas.
fn graph_schemas(ctx: &SessionContext) -> BTreeSet<String> {
    let registry = graph::registry_of(ctx);
    let mut names: BTreeSet<String> = registry.names().into_iter().collect();
    names.extend(registry.all_unavailable().into_iter().map(|g| g.name));
    names
}

/// Every table and view of every catalog and schema, sorted by catalog,
/// schema and name: `information_schema` and the graphs' own schemas left
/// out. A table that cannot be read is listed with no columns.
pub(crate) async fn tables(ctx: &SessionContext) -> Vec<TableInfo> {
    let default_catalog = ctx
        .state()
        .config()
        .options()
        .catalog
        .default_catalog
        .clone();
    let graphs = graph_schemas(ctx);
    let mut out = Vec::new();
    let mut catalogs = ctx.catalog_names();
    catalogs.sort();
    for catalog_name in catalogs {
        let Some(catalog) = ctx.catalog(&catalog_name) else {
            continue;
        };
        let mut schemas = catalog.schema_names();
        schemas.sort();
        for schema_name in schemas {
            if schema_name == "information_schema"
                || (catalog_name == default_catalog && graphs.contains(&schema_name))
            {
                continue;
            }
            let Some(schema) = catalog.schema(&schema_name) else {
                continue;
            };
            let mut names = schema.table_names();
            names.sort();
            for name in names {
                let provider: Option<Arc<dyn TableProvider>> =
                    schema.table(&name).await.ok().flatten();
                let (kind, columns) = match provider {
                    Some(p) => {
                        let columns = p
                            .schema()
                            .fields()
                            .iter()
                            .map(|f| (f.name().clone(), f.data_type().to_string(), f.is_nullable()))
                            .collect();
                        (kind_of(p.as_ref()), columns)
                    }
                    None => ("table", Vec::new()),
                };
                out.push(TableInfo {
                    catalog: catalog_name.clone(),
                    schema: schema_name.clone(),
                    name,
                    kind,
                    columns,
                });
            }
        }
    }
    out
}

/// The session's property graphs, sorted by name, as `catalog` lists them:
/// `{name, vertices: [{name, table, key}], edges: [{name, table, source,
/// destination}]}`, and for a graph of the database that could not be
/// defined, no tables and its `error`.
pub(crate) fn graphs(ctx: &SessionContext) -> Vec<Value> {
    let registry = graph::registry_of(ctx);
    let mut out: Vec<(String, Value)> = registry
        .all()
        .iter()
        .map(|g| {
            let vertices: Vec<Value> = g
                .vertex_tables
                .iter()
                .map(|v| json!({"name": v.name, "table": v.table.to_string(), "key": v.key}))
                .collect();
            let edges: Vec<Value> = g
                .edge_tables
                .iter()
                .map(|e| {
                    json!({
                        "name": e.name,
                        "table": e.table.to_string(),
                        "source": e.source,
                        "destination": e.destination,
                    })
                })
                .collect();
            (
                g.name.clone(),
                json!({"name": g.name, "vertices": vertices, "edges": edges}),
            )
        })
        .collect();
    for g in registry.all_unavailable() {
        if registry.get(&g.name).is_none() {
            let value = json!({"name": g.name, "vertices": [], "edges": [], "error": g.message});
            out.push((g.name, value));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.into_iter().map(|(_, v)| v).collect()
}

/// `catalog`'s result: `{databases: [{name, schemas: [{name, tables: [{name,
/// kind, columns: [{name, type, nullable}]}]}]}], graphs}`. A database is one
/// of the session's catalogs; a schema with no tables is listed too.
pub(crate) async fn catalog(ctx: &SessionContext) -> Value {
    let tables = tables(ctx).await;
    let graph_schemas = graph_schemas(ctx);
    let default_catalog = ctx
        .state()
        .config()
        .options()
        .catalog
        .default_catalog
        .clone();
    let mut databases = Vec::new();
    let mut catalogs = ctx.catalog_names();
    catalogs.sort();
    for catalog_name in catalogs {
        let Some(catalog) = ctx.catalog(&catalog_name) else {
            continue;
        };
        let mut schema_names = catalog.schema_names();
        schema_names.sort();
        let schemas: Vec<Value> = schema_names
            .into_iter()
            .filter(|s| {
                s != "information_schema"
                    && !(catalog_name == default_catalog && graph_schemas.contains(s))
            })
            .map(|schema_name| {
                let tables: Vec<Value> = tables
                    .iter()
                    .filter(|t| t.catalog == catalog_name && t.schema == schema_name)
                    .map(|t| {
                        let columns: Vec<Value> = t
                            .columns
                            .iter()
                            .map(|(name, ty, nullable)| {
                                json!({"name": name, "type": ty, "nullable": nullable})
                            })
                            .collect();
                        json!({"name": t.name, "kind": t.kind, "columns": columns})
                    })
                    .collect();
                json!({"name": schema_name, "tables": tables})
            })
            .collect();
        databases.push(json!({"name": catalog_name, "schemas": schemas}));
    }
    json!({"databases": databases, "graphs": graphs(ctx)})
}

/// A fingerprint of [`catalog`]'s answer: it changes when the answer does.
/// FNV-1a over its JSON, stable across builds.
pub(crate) async fn fingerprint(ctx: &SessionContext) -> u64 {
    let text = catalog(ctx).await.to_string();
    text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}
