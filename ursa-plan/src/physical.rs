//! Physical execution: graph kernels as DataFusion `ExecutionPlan`s.
//!
//! A graph kernel is **a physical operator that happens to consult a side data
//! structure** (the [`ursa_core::Topology`]). DataFusion already contains
//! pipeline breakers (sort, hash aggregate) that fully materialize before
//! emitting; an iterative graph algorithm is architecturally identical:
//! `PageRankExec` blocks, runs its fixpoint over the CSR, and emits results as
//! Arrow `RecordBatch`es into the downstream plan.
//!
//! ## Two runtime traps this layer must respect (spec §Runtime integration)
//!
//! 1. **Thread pools.** DataFusion executes on tokio (async, IO-oriented);
//!    kernels want Rayon (data-parallel compute). Running Rayon loops directly on
//!    tokio workers starves the runtime. Graph `ExecutionPlan`s MUST dispatch the
//!    compute via `spawn_blocking` (or a dedicated compute pool) and stream the
//!    result batches back. This is a deadlock-in-month-two bug if forgotten.
//! 2. **Determinism.** Stochastic kernels take a seed; document per-seed vs
//!    per-(seed, thread-count) determinism at this boundary.
//!
//! ## First implementation task
//!
//! Implement `DegreeExec` (or `PageRankExec`) as a real `ExecutionPlan`:
//! build/borrow the `Arc<Topology>`, call the `ursa_core` kernel inside
//! `spawn_blocking`, gather dense results back to user ids via the frame's
//! `IdMap`, and emit a single `RecordBatch`. That proves the operator contract
//! for every kernel that follows.

use std::sync::Arc;

use ursa_core::Topology;

/// Skeleton handle for a node-valued algorithm execution. Real version implements
/// `datafusion::physical_plan::ExecutionPlan`.
pub struct GraphAlgorithmExec {
    pub topology: Arc<Topology>,
    // properties (schema, partitioning, output ordering) attach when the
    // ExecutionPlan trait is implemented.
}

impl GraphAlgorithmExec {
    pub fn new(topology: Arc<Topology>) -> Self {
        GraphAlgorithmExec { topology }
    }
}

// TODO(v0.1): impl ExecutionPlan for GraphAlgorithmExec + friends, dispatching
// ursa-core kernels via spawn_blocking and emitting Arrow batches.
