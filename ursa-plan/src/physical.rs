//! Physical execution: graph kernels as DataFusion `ExecutionPlan`s.
//!
//! A graph kernel is **a physical operator that happens to consult a side data
//! structure** (the [`ursa_core::Topology`]). DataFusion already contains
//! pipeline breakers (sort, hash aggregate) that fully materialize before
//! emitting; a node-valued graph algorithm is architecturally identical:
//! [`GraphAlgorithmExec`] is a leaf operator that runs its kernel over the CSR
//! and emits the result as a single Arrow `RecordBatch` into the downstream plan.
//!
//! ## Runtime traps respected here (spec §Runtime integration)
//!
//! 1. **Thread pools.** DataFusion executes on tokio (async, IO-oriented);
//!    kernels want Rayon (data-parallel compute). Running Rayon loops directly on
//!    tokio workers starves the runtime, so [`GraphAlgorithmExec::execute`]
//!    dispatches the compute via `spawn_blocking` and streams the batch back.
//! 2. **Determinism.** Stochastic kernels take a seed; that contract lives with
//!    the kernels. The two currently-wired kernels (degree, pagerank) and the two
//!    other executable ones (weak components, triangle count) are deterministic.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use arrow::datatypes::SchemaRef;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::TaskContext;
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use ursa_core::{IdMap, Topology};

use crate::logical::GraphAlgo;
use crate::result::{algorithm_batch, algorithm_schema, is_executable};

/// A leaf `ExecutionPlan` that runs one node-valued graph algorithm over a shared
/// topology and emits a `(id, value)` `RecordBatch`.
#[derive(Debug)]
pub struct GraphAlgorithmExec {
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    algo: GraphAlgo,
    schema: SchemaRef,
    properties: PlanProperties,
}

impl GraphAlgorithmExec {
    /// Build the operator, validating that `algo` has a wired kernel.
    pub fn try_new(topology: Arc<Topology>, ids: Arc<IdMap>, algo: GraphAlgo) -> Result<Self> {
        if !is_executable(&algo) {
            return Err(DataFusionError::NotImplemented(format!(
                "graph algorithm {algo:?} is not yet wired into the execution path"
            )));
        }
        let schema = algorithm_schema(&algo);
        let properties = PlanProperties::new(
            EquivalenceProperties::new(schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Final,
            Boundedness::Bounded,
        );
        Ok(GraphAlgorithmExec {
            topology,
            ids,
            algo,
            schema,
            properties,
        })
    }
}

impl DisplayAs for GraphAlgorithmExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "GraphAlgorithmExec: algo={:?}", self.algo)
    }
}

impl ExecutionPlan for GraphAlgorithmExec {
    fn name(&self) -> &str {
        "GraphAlgorithmExec"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn properties(&self) -> &PlanProperties {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        // Leaf: the topology is a side structure, not a child plan. (When
        // filtered-subgraph seeds arrive via `.from_(...)`, that becomes a child.)
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn execute(
        &self,
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let schema = self.schema.clone();
        let topo = self.topology.clone();
        let ids = self.ids.clone();
        let algo = self.algo.clone();

        // Compute off the tokio worker: the kernel is CPU-bound (and Rayon-parallel
        // inside), so it must not run on an async executor thread.
        let fut = async move {
            tokio::task::spawn_blocking(move || algorithm_batch(&topo, &ids, &algo))
                .await
                .map_err(|e| DataFusionError::Execution(format!("graph kernel panicked: {e}")))
        };

        let stream = RecordBatchStreamAdapter::new(schema, futures::stream::once(fut));
        Ok(Box::pin(stream))
    }
}

/// Synchronous convenience: run an algorithm to completion and return its batch.
///
/// Spins a small multi-thread runtime, executes the operator, and collects the
/// single output batch. This is the entry point the Python `collect()`
/// orchestration calls; inside a larger DataFusion plan the operator is driven by
/// the session's runtime instead.
pub fn run_algorithm(
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    algo: GraphAlgo,
) -> Result<arrow::record_batch::RecordBatch> {
    let exec = Arc::new(GraphAlgorithmExec::try_new(topology, ids, algo)?);
    let ctx = Arc::new(TaskContext::default());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| DataFusionError::Execution(format!("failed to build runtime: {e}")))?;

    runtime.block_on(async move {
        let stream = exec.execute(0, ctx)?;
        let batches = datafusion::physical_plan::common::collect(stream).await?;
        arrow::compute::concat_batches(&batches[0].schema(), &batches)
            .map_err(|e| DataFusionError::ArrowError(e, None))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logical::Direction;
    use crate::topology::build_topology;
    use arrow::array::{Float64Array, Int64Array};

    fn diamond() -> (Arc<Topology>, Arc<IdMap>) {
        let src = Int64Array::from(vec![0, 0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 2, 0]);
        build_topology(&src, &dst).unwrap()
    }

    #[test]
    fn rejects_unwired_algorithm() {
        let (topo, ids) = diamond();
        let err = GraphAlgorithmExec::try_new(topo, ids, GraphAlgo::Closeness);
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn execute_streams_a_result_batch() {
        let (topo, ids) = diamond();
        let exec = Arc::new(
            GraphAlgorithmExec::try_new(
                topo,
                ids,
                GraphAlgo::Degree {
                    direction: Direction::Out,
                },
            )
            .unwrap(),
        );
        let ctx = Arc::new(TaskContext::default());
        let stream = exec.execute(0, ctx).unwrap();
        let batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 3);
    }

    #[test]
    fn run_algorithm_returns_pagerank_batch() {
        let (topo, ids) = diamond();
        let batch = run_algorithm(
            topo,
            ids,
            GraphAlgo::PageRank {
                damping: 0.85,
                max_iter: 30,
                tol: 1e-6,
            },
        )
        .unwrap();
        let pr = batch
            .column(1)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        let sum: f64 = pr.values().iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
    }
}
