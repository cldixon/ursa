//! Graphs in a session: their definitions and where they are kept.
//!
//! A graph is defined by `CREATE PROPERTY GRAPH` (SQL/PGQ) over tables or views
//! already in the session: one edge table with a source and a destination
//! column, and optionally one vertex table keyed by an id column. Without a
//! vertex table, the nodes are the endpoints of the edges.
//!
//! This module is the contract between the graph DDL (which creates and drops
//! [`GraphDef`]s) and everything that reads graphs: the `<graph>.nodes` and
//! `<graph>.edges` tables and the algorithm table functions. Definitions are
//! held per session in a [`GraphRegistry`], stored as a session config
//! extension so any code holding the session state can reach it.
//!
//! A definition names tables; it does not copy or pin their rows. Every query
//! plans the edge table afresh, so a graph over a view of a file sees the
//! file's current contents.
//!
//! An edge or vertex table may also be a file path or URL, as in `FROM`
//! (`EDGE TABLES ('edges.csv' SOURCE KEY (src) DESTINATION KEY (dst))`). It is
//! opened once, when the graph is created, as a listing table that the
//! registry keeps for that graph ([`GraphRegistry::file_table`]); it is not
//! registered under any name, so it never shows in a listing. Like a view over
//! the file, it fixes the file's columns at that point and reads the file's
//! current rows on every query.
//!
//! Tables a graph uses cannot be dropped, or replaced by ones the graph's
//! definition no longer fits, while the graph exists ([`check_drop`],
//! [`check_replace`]). If one disappears anyway (through the DataFusion API),
//! `<graph>.nodes` and `<graph>.edges` fail when queried, and session listings
//! keep working.
//!
//! Algorithms over a graph are table functions, `FROM pagerank(g)`, planned
//! as a [`GraphAlgorithmNode`] whose child is the edge table ([`functions`],
//! `relation`, `node`, `exec`), with the built index cached per session
//! ([`IndexCache`]).
//!
//! Traversals (`hop`, `shortest_path`, `random_walk`) are table functions
//! too, planned as a [`GraphTraversalNode`] over the same edge plan, with one
//! more child per node-id argument (a list, or a subquery that runs when the
//! node runs); see [`traversal`].

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

use datafusion::catalog::{CatalogProviderList, TableProvider};
use datafusion::common::TableReference;
use datafusion::execution::context::SessionState;
use datafusion::prelude::{SessionConfig, SessionContext};

mod define;
mod describe;
pub mod schema;

pub use define::{
    check_ddl, check_drop, check_drop_schema, check_replace, closest_name, create_graph,
    drop_graph, unknown_graph,
};
pub use describe::{describe_graph, show_graphs};
pub use schema::{edges_plan, edges_plan_from, nodes_plan, nodes_plan_from, GraphSchema};

mod catalog_table;
mod exec;
pub mod functions;
mod index;
mod kernels;
mod node;
mod relation;
mod session;
pub mod traversal;

pub use catalog_table::UrsaFunctions;
pub use exec::{
    GraphAlgorithmExec, GraphPlanner, GraphTraversalExec, KeepEdgeOrder, UrsaQueryPlanner,
};
pub use index::{index_builds, FileIdentity, GraphIndex, IndexCache, Weights};
pub use node::{GraphAlgorithmNode, GraphTraversalNode, Output};
pub(crate) use session::{edge_order_rule, query_planner, register_functions, with_algorithms};

/// One graph, as `CREATE PROPERTY GRAPH` defined it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphDef {
    /// The graph's name: also the name of the schema holding its `nodes` and
    /// `edges` tables.
    pub name: String,
    pub edges: EdgeTable,
    /// The vertex table, if the definition has `VERTEX TABLES`.
    pub vertices: Option<VertexTable>,
}

/// The edge table: each row is an edge from `source` to `destination`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeTable {
    pub table: TableReference,
    /// `SOURCE KEY (column)`.
    pub source: String,
    /// `DESTINATION KEY (column)`.
    pub destination: String,
}

/// The vertex table: each row is a node, keyed by `key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexTable {
    pub table: TableReference,
    /// `KEY (column)`.
    pub key: String,
}

/// A table a graph opened from a file path or URL when it was created.
#[derive(Debug, Clone)]
pub struct FileTable {
    /// The table reference the definition names: the path, as written.
    pub table: TableReference,
    pub provider: Arc<dyn TableProvider>,
}

/// The graphs defined in one session, by name. Names are compared as given
/// (the DDL normalizes identifiers the way DataFusion does for tables).
#[derive(Debug, Default)]
pub struct GraphRegistry {
    graphs: RwLock<BTreeMap<String, Arc<GraphDef>>>,
    /// Per graph, the tables it opened from file paths (see the module docs).
    files: RwLock<BTreeMap<String, Vec<FileTable>>>,
    /// The session's catalogs before the file-table wrapper: graph schemas are
    /// registered and dropped there (the wrapper cannot drop a schema).
    catalogs: OnceLock<Arc<dyn CatalogProviderList>>,
}

impl GraphRegistry {
    /// The graph called `name`.
    pub fn get(&self, name: &str) -> Option<Arc<GraphDef>> {
        self.graphs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// Every graph, sorted by name.
    pub fn all(&self) -> Vec<Arc<GraphDef>> {
        self.graphs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Add or replace `def`; returns the definition it replaced.
    pub fn put(&self, def: GraphDef) -> Option<Arc<GraphDef>> {
        self.put_with_files(def, Vec::new())
    }

    /// Add or replace `def`, with the tables it opened from file paths.
    pub fn put_with_files(&self, def: GraphDef, files: Vec<FileTable>) -> Option<Arc<GraphDef>> {
        let mut graphs = self.graphs.write().unwrap_or_else(|e| e.into_inner());
        let mut all_files = self.files.write().unwrap_or_else(|e| e.into_inner());
        if files.is_empty() {
            all_files.remove(&def.name);
        } else {
            all_files.insert(def.name.clone(), files);
        }
        graphs.insert(def.name.clone(), Arc::new(def))
    }

    /// Remove the graph called `name`; returns it if it existed.
    pub fn remove(&self, name: &str) -> Option<Arc<GraphDef>> {
        let mut graphs = self.graphs.write().unwrap_or_else(|e| e.into_inner());
        self.files
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        graphs.remove(name)
    }

    /// The table graph `graph` opened from the file path `table`, if `table`
    /// is one of its file tables.
    pub fn file_table(
        &self,
        graph: &str,
        table: &TableReference,
    ) -> Option<Arc<dyn TableProvider>> {
        self.files
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(graph)?
            .iter()
            .find(|f| &f.table == table)
            .map(|f| Arc::clone(&f.provider))
    }

    /// Record the session's unwrapped catalog list (done by `new_session`).
    pub(crate) fn set_catalogs(&self, catalogs: Arc<dyn CatalogProviderList>) {
        let _ = self.catalogs.set(catalogs);
    }

    pub(crate) fn catalogs(&self) -> Option<&Arc<dyn CatalogProviderList>> {
        self.catalogs.get()
    }

    /// The names of all graphs, sorted: for "did you mean" suggestions.
    pub fn names(&self) -> Vec<String> {
        self.graphs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

/// Install an empty registry on `config` (done by `new_session`).
pub(crate) fn with_registry(config: SessionConfig) -> SessionConfig {
    config.with_extension(Arc::new(GraphRegistry::default()))
}

/// The session's graph registry.
pub fn registry(state: &SessionState) -> Arc<GraphRegistry> {
    state
        .config()
        .get_extension::<GraphRegistry>()
        .expect("an Ursa session has a graph registry (built by ursa_engine::new_session)")
}

/// The graph registry of `ctx`.
pub fn registry_of(ctx: &SessionContext) -> Arc<GraphRegistry> {
    registry(&ctx.state())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_has_an_empty_registry() {
        let ctx = crate::new_session();
        let graphs = registry_of(&ctx);
        assert!(graphs.all().is_empty());
        graphs.put(GraphDef {
            name: "g".into(),
            edges: EdgeTable {
                table: TableReference::bare("e"),
                source: "a".into(),
                destination: "b".into(),
            },
            vertices: None,
        });
        // The registry is shared by every handle on the session.
        assert_eq!(registry_of(&ctx).names(), vec!["g".to_string()]);
    }
}
