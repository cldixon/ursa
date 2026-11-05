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
//! Algorithms over a graph are table functions, `FROM pagerank(g)`, planned
//! as a [`GraphAlgorithmNode`] whose child is the edge table ([`functions`],
//! `relation`, `node`, `exec`), with the built index cached per session
//! ([`IndexCache`]).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

mod exec;
pub mod functions;
mod index;
mod kernels;
mod node;
mod relation;
mod session;

pub use exec::{GraphAlgorithmExec, GraphPlanner, UrsaQueryPlanner};
pub use functions::Algorithm;
pub use index::{index_builds, FileIdentity, GraphIndex, IndexCache};
pub use node::GraphAlgorithmNode;
pub(crate) use session::{query_planner, register_functions, with_algorithms};

use datafusion::common::TableReference;
use datafusion::execution::context::SessionState;
use datafusion::prelude::{SessionConfig, SessionContext};

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

/// The graphs defined in one session, by name. Names are compared as given
/// (the DDL normalizes identifiers the way DataFusion does for tables).
#[derive(Debug, Default)]
pub struct GraphRegistry {
    graphs: RwLock<BTreeMap<String, Arc<GraphDef>>>,
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
        self.graphs
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(def.name.clone(), Arc::new(def))
    }

    /// Remove the graph called `name`; returns it if it existed.
    pub fn remove(&self, name: &str) -> Option<Arc<GraphDef>> {
        self.graphs
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
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
