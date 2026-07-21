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
//! ## Status: skeleton
//!
//! The module structure and type surface are laid out; the DataFusion trait
//! implementations (`UserDefinedLogicalNodeCore`, `ExecutionPlan`, `OptimizerRule`)
//! are documented stubs. The first implementation task is [`physical`]: run one
//! kernel (`degree` or `pagerank`) as a real pipeline-breaking `ExecutionPlan`
//! that streams Arrow `RecordBatch`es, proving the operator contract end-to-end.

pub mod expr;
pub mod logical;
pub mod node;
pub mod physical;
pub mod planner;
pub mod query;
pub mod result;
pub mod scan;
pub mod session;
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
pub use session::ursa_session;
pub use stats::{avg_path_length, density, describe, diameter};
pub use topology::build_topology;
