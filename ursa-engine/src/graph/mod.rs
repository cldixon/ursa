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
    drop_graph, unknown_graph, EdgeSpec, GraphSpec, VertexSpec,
};
pub use describe::{describe_graph, show_graphs};
pub use schema::{edges_plan, edges_plan_from, nodes_plan, nodes_plan_from, GraphSchema};

mod catalog_table;
mod exec;
pub mod functions;
mod index;
mod kernels;
mod node;
pub mod pattern;
mod relation;
mod session;
pub mod traversal;

pub use catalog_table::UrsaFunctions;
pub use exec::{
    GraphAlgorithmExec, GraphPlanner, GraphTraversalExec, KeepEdgeOrder, UrsaQueryPlanner,
};
pub use index::{index_builds, index_loads, FileIdentity, GraphIndex, IndexCache, Weights};
pub use node::{GraphAlgorithmNode, GraphTraversalNode, Output};
pub(crate) use session::{edge_order_rule, query_planner, register_functions, with_algorithms};

/// One graph, as `CREATE PROPERTY GRAPH` defined it: its vertex and edge
/// tables (SQL/PGQ's element tables), with their keys resolved (#295).
///
/// `MATCH` spans every table. An algorithm runs on one vertex table and one
/// edge table, a [`GraphDef`], as in DuckPGQ: see [`PropertyGraph::pair`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyGraph {
    /// The graph's name: also the name of the schema holding its `nodes` and
    /// `edges` tables (when it has one implied pair; see [`Self::pair`]).
    pub name: String,
    pub vertex_tables: Vec<VertexTable>,
    pub edge_tables: Vec<EdgeTable>,
}

/// A graph as an algorithm sees it: one edge table, and the vertex table its
/// endpoints refer to, if any. A whole graph before #295; now one pair of a
/// [`PropertyGraph`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphDef {
    /// The graph's name.
    pub name: String,
    pub edges: EdgeTable,
    /// The vertex table, if the pair has one.
    pub vertices: Option<VertexTable>,
}

/// An edge table: each row is an edge from `source` to `destination`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeTable {
    /// Its element name: the `AS` name, or else the table's name.
    pub name: String,
    pub table: TableReference,
    /// `SOURCE KEY (column)`.
    pub source: String,
    /// `DESTINATION KEY (column)`.
    pub destination: String,
    /// The vertex table (element name) `SOURCE … REFERENCES`, if any.
    pub source_vertex: Option<String>,
    /// The vertex table (element name) `DESTINATION … REFERENCES`, if any.
    pub destination_vertex: Option<String>,
    pub labels: Vec<Label>,
}

/// A vertex table: each row is a node, keyed by `key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexTable {
    /// Its element name: the `AS` name, or else the table's name.
    pub name: String,
    pub table: TableReference,
    /// `KEY (column)`, or the key inferred when `KEY` was left out.
    pub key: String,
    pub labels: Vec<Label>,
}

/// A label of an element table, and the properties it exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    pub properties: Properties,
}

/// The properties a label exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Properties {
    /// Every column but `except`: no properties clause, or `PROPERTIES ARE
    /// ALL COLUMNS [EXCEPT (…)]`.
    AllColumns { except: Vec<String> },
    /// `PROPERTIES (column [AS name], …)`: (column, property name) pairs.
    Columns(Vec<(String, String)>),
    /// `NO PROPERTIES`.
    None,
}

impl Label {
    /// The default label of an element table: its name, with every column.
    pub fn default_for(element: &str) -> Self {
        Label {
            name: element.to_string(),
            properties: Properties::AllColumns { except: Vec::new() },
        }
    }
}

/// An element table's labels, or its default label when none were written.
fn labels_or_default<'a>(labels: &'a [Label], name: &str) -> std::borrow::Cow<'a, [Label]> {
    if labels.is_empty() {
        std::borrow::Cow::Owned(vec![Label::default_for(name)])
    } else {
        std::borrow::Cow::Borrowed(labels)
    }
}

impl VertexTable {
    /// A vertex table named after its table, with its default label.
    pub fn new(table: impl Into<TableReference>, key: impl Into<String>) -> Self {
        let table = table.into();
        VertexTable {
            name: table.table().to_string(),
            table,
            key: key.into(),
            labels: Vec::new(),
        }
    }

    /// Its labels, the default one if none were written.
    pub fn labels(&self) -> std::borrow::Cow<'_, [Label]> {
        labels_or_default(&self.labels, &self.name)
    }

    /// Whether `label` names this table: its element name or one of its labels.
    pub fn answers_to(&self, label: &str) -> bool {
        self.name.eq_ignore_ascii_case(label)
            || self
                .labels
                .iter()
                .any(|l| l.name.eq_ignore_ascii_case(label))
    }
}

impl EdgeTable {
    /// An edge table named after its table, with its default label and no
    /// `REFERENCES`.
    pub fn new(
        table: impl Into<TableReference>,
        source: impl Into<String>,
        destination: impl Into<String>,
    ) -> Self {
        let table = table.into();
        EdgeTable {
            name: table.table().to_string(),
            table,
            source: source.into(),
            destination: destination.into(),
            source_vertex: None,
            destination_vertex: None,
            labels: Vec::new(),
        }
    }

    /// Its labels, the default one if none were written.
    pub fn labels(&self) -> std::borrow::Cow<'_, [Label]> {
        labels_or_default(&self.labels, &self.name)
    }

    /// Whether `label` names this table: its element name or one of its labels.
    pub fn answers_to(&self, label: &str) -> bool {
        self.name.eq_ignore_ascii_case(label)
            || self
                .labels
                .iter()
                .any(|l| l.name.eq_ignore_ascii_case(label))
    }
}

/// A graph of one pair: what every graph was before #295.
impl From<GraphDef> for PropertyGraph {
    fn from(def: GraphDef) -> Self {
        PropertyGraph {
            name: def.name,
            vertex_tables: def.vertices.into_iter().collect(),
            edge_tables: vec![def.edges],
        }
    }
}

impl PropertyGraph {
    /// Every table the graph reads, vertex tables first.
    pub fn tables(&self) -> impl Iterator<Item = &TableReference> {
        self.vertex_tables
            .iter()
            .map(|v| &v.table)
            .chain(self.edge_tables.iter().map(|e| &e.table))
    }

    /// The vertex table called (or labelled) `label`.
    pub fn vertex_table(&self, label: &str) -> Option<&VertexTable> {
        self.vertex_tables
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(label))
            .or_else(|| self.vertex_tables.iter().find(|v| v.answers_to(label)))
    }

    /// The edge table called (or labelled) `label`.
    pub fn edge_table(&self, label: &str) -> Option<&EdgeTable> {
        self.edge_tables
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(label))
            .or_else(|| self.edge_tables.iter().find(|e| e.answers_to(label)))
    }

    /// The vertex table an endpoint of `edge` refers to: its `REFERENCES`, or
    /// without one, the graph's only vertex table (`None` if it has none).
    pub fn endpoint_vertex(&self, edge: &EdgeTable, source: bool) -> Option<&VertexTable> {
        let named = if source {
            &edge.source_vertex
        } else {
            &edge.destination_vertex
        };
        match named {
            Some(name) => self.vertex_tables.iter().find(|v| &v.name == name),
            None if self.vertex_tables.len() == 1 => self.vertex_tables.first(),
            None => None,
        }
    }

    /// The pair an algorithm runs on: an edge table, and the vertex table both
    /// its endpoints refer to (if any), as DuckPGQ's `pagerank(g, vertex,
    /// edge)`. With no labels the pair is implied when the graph has one edge
    /// table: every graph before #295 has one.
    pub fn pair(
        &self,
        vertex: Option<&str>,
        edge: Option<&str>,
    ) -> datafusion::error::Result<GraphDef> {
        use datafusion::error::DataFusionError;
        let plan_err = |m: String| Err(DataFusionError::Plan(m));
        let edges = match edge {
            Some(label) => match self.edge_table(label) {
                Some(e) => e,
                None => {
                    let names: Vec<&str> =
                        self.edge_tables.iter().map(|e| e.name.as_str()).collect();
                    return plan_err(format!(
                        "graph '{}' has no edge table '{label}'; its edge tables are {}",
                        self.name,
                        if names.is_empty() {
                            "none".to_string()
                        } else {
                            names.join(", ")
                        }
                    ));
                }
            },
            None => match self.edge_tables.as_slice() {
                [only] => only,
                [] => {
                    return plan_err(format!(
                        "graph '{}' has no edge tables, so there is nothing for an algorithm to \
                         run on",
                        self.name
                    ))
                }
                several => {
                    let names: Vec<&str> = several.iter().map(|e| e.name.as_str()).collect();
                    return plan_err(format!(
                        "graph '{}' has {} edge tables ({}); name the vertex and edge table to \
                         run on, as in pagerank({}, <vertex table>, <edge table>)",
                        self.name,
                        several.len(),
                        names.join(", "),
                        self.name
                    ));
                }
            },
        };
        let source = self.endpoint_vertex(edges, true);
        let destination = self.endpoint_vertex(edges, false);
        if let (Some(s), Some(d)) = (source, destination) {
            if s.name != d.name {
                return plan_err(format!(
                    "edge table '{}' of graph '{}' goes from vertex table '{}' to '{}'; an \
                     algorithm runs on edges between one vertex table",
                    edges.name, self.name, s.name, d.name
                ));
            }
        }
        let vertices = source.or(destination);
        if let Some(label) = vertex {
            let Some(asked) = self.vertex_table(label) else {
                let names: Vec<&str> = self.vertex_tables.iter().map(|v| v.name.as_str()).collect();
                return plan_err(format!(
                    "graph '{}' has no vertex table '{label}'; its vertex tables are {}",
                    self.name,
                    if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(", ")
                    }
                ));
            };
            if vertices.is_some_and(|v| v.name != asked.name) {
                return plan_err(format!(
                    "edge table '{}' of graph '{}' connects vertex table '{}', not '{}'",
                    edges.name,
                    self.name,
                    vertices.map(|v| v.name.as_str()).unwrap_or_default(),
                    asked.name
                ));
            }
            return Ok(GraphDef {
                name: self.name.clone(),
                edges: edges.clone(),
                vertices: Some(asked.clone()),
            });
        }
        Ok(GraphDef {
            name: self.name.clone(),
            edges: edges.clone(),
            vertices: vertices.cloned(),
        })
    }

    /// The implied pair, when there is one: the graph's `nodes` and `edges`
    /// tables are this pair's.
    pub fn default_pair(&self) -> Option<GraphDef> {
        if self.vertex_tables.len() > 1 {
            return None;
        }
        self.pair(None, None).ok()
    }
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
    graphs: RwLock<BTreeMap<String, Arc<PropertyGraph>>>,
    /// Per graph, the tables it opened from file paths (see the module docs).
    files: RwLock<BTreeMap<String, Vec<FileTable>>>,
    /// The session's catalogs before the file-table wrapper: graph schemas are
    /// registered and dropped there (the wrapper cannot drop a schema).
    catalogs: OnceLock<Arc<dyn CatalogProviderList>>,
}

impl GraphRegistry {
    /// The graph called `name`.
    pub fn get(&self, name: &str) -> Option<Arc<PropertyGraph>> {
        self.graphs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// Every graph, sorted by name.
    pub fn all(&self) -> Vec<Arc<PropertyGraph>> {
        self.graphs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Add or replace `def`; returns the definition it replaced.
    pub fn put(&self, def: impl Into<PropertyGraph>) -> Option<Arc<PropertyGraph>> {
        self.put_with_files(def.into(), Vec::new())
    }

    /// Add or replace `def`, with the tables it opened from file paths.
    pub fn put_with_files(
        &self,
        def: PropertyGraph,
        files: Vec<FileTable>,
    ) -> Option<Arc<PropertyGraph>> {
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
    pub fn remove(&self, name: &str) -> Option<Arc<PropertyGraph>> {
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
        graphs.put(PropertyGraph {
            name: "g".into(),
            vertex_tables: Vec::new(),
            edge_tables: vec![EdgeTable::new(TableReference::bare("e"), "a", "b")],
        });
        // The registry is shared by every handle on the session.
        assert_eq!(registry_of(&ctx).names(), vec!["g".to_string()]);
    }
}
