//! # ursa-plan
//!
//! Where Ursa's graph operations become first-class citizens of a DataFusion
//! query plan. This crate is the **single seam** between Ursa's Polars-shaped
//! dialect and DataFusion: it owns the custom logical plan nodes, the physical
//! `ExecutionPlan`s that call [`ursa_core`] kernels, the optimizer rules, and the
//! scan/session/`object_store` plumbing.
//!
//! Keeping this seam in one crate is a deliberate architectural choice. The cost
//! of choosing DataFusion over the Polars crates is *not* the graph kernels
//! (those are engine-agnostic, in `ursa-core`) — it is that we own a
//! Polars-shaped expression frontend that lowers to DataFusion `Expr`. Confining
//! that lowering here (see [`expr`]) keeps it a bounded surface rather than a
//! concern smeared across the codebase.
//!
//! ## Status
//!
//! The custom logical nodes ([`node`]), their pipeline-breaking `ExecutionPlan`s
//! ([`physical`]), the extension planner ([`planner`]), and the query builders
//! ([`query`]) are implemented: every graph verb — algorithms (weighted and
//! unweighted), neighbour aggregation, and the hop/shortest_path/random_walk
//! traversals — runs end to end, streaming Arrow `RecordBatch`es. The graph
//! `OptimizerRule`s (filter-before-traversal, segmented-reduction fusion) are the
//! remaining planned work; execution is correct without them.

pub mod expr;
pub mod logical;
pub mod node;
pub mod physical;
pub mod planner;
pub mod query;
pub mod result;
pub mod runtime;
pub mod scan;
pub mod stats;
pub mod topology;
pub mod weight;

pub use logical::{Direction, GraphAlgo};
pub use node::GraphAlgorithmNode;
pub use physical::GraphAlgorithmExec;
pub use planner::graph_session;
pub use query::{
    execute_hop_query, execute_node_query, execute_path_query, execute_walk_query, Comparison,
};
pub use scan::{scan_edges_batch, scan_nodes_batch};
pub use stats::{avg_path_length, density, describe, diameter};
pub use topology::{build_topology, build_topology_batches};
