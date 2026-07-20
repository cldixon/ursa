//! Physical execution: the graph query operator.
//!
//! [`GraphAlgorithmExec`] is a leaf `ExecutionPlan` that runs a query's
//! node-valued algorithms over a shared topology and emits one aligned
//! `(id, values...)` `RecordBatch`. It is produced from a
//! [`crate::node::GraphAlgorithmNode`] by [`crate::planner::GraphExtensionPlanner`]
//! during physical planning, so a graph query is a first-class citizen of the
//! DataFusion plan rather than something orchestrated from outside it.
//!
//! ## Runtime trap respected here (spec §Runtime integration)
//!
//! DataFusion executes on tokio (async, IO-oriented); the kernels want Rayon
//! (data-parallel compute). Running Rayon loops on tokio workers starves the
//! runtime, so [`GraphAlgorithmExec::execute`] dispatches the compute via
//! `spawn_blocking` and streams the batch back.

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

use crate::result::{query_batch, query_schema, OutputColumn};

/// A leaf `ExecutionPlan` that runs a graph query's algorithms and emits a
/// single `(id, values...)` `RecordBatch`.
#[derive(Debug)]
pub struct GraphAlgorithmExec {
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    columns: Arc<Vec<OutputColumn>>,
    schema: SchemaRef,
    properties: PlanProperties,
}

impl GraphAlgorithmExec {
    pub fn new(topology: Arc<Topology>, ids: Arc<IdMap>, columns: Arc<Vec<OutputColumn>>) -> Self {
        let schema = query_schema(&columns);
        let properties = PlanProperties::new(
            EquivalenceProperties::new(schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Final,
            Boundedness::Bounded,
        );
        GraphAlgorithmExec {
            topology,
            ids,
            columns,
            schema,
            properties,
        }
    }
}

impl DisplayAs for GraphAlgorithmExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        let names: Vec<&str> = self.columns.iter().map(|c| c.name()).collect();
        write!(f, "GraphAlgorithmExec: columns=[{}]", names.join(", "))
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
        let columns = self.columns.clone();

        // CPU-bound (Rayon-parallel inside) — keep it off the tokio worker.
        let fut = async move {
            tokio::task::spawn_blocking(move || query_batch(&topo, &ids, &columns))
                .await
                .map_err(|e| DataFusionError::Execution(format!("graph kernel panicked: {e}")))
        };

        let stream = RecordBatchStreamAdapter::new(schema, futures::stream::once(fut));
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logical::{Direction, GraphAlgo};
    use crate::topology::build_topology;
    use arrow::array::Int64Array;

    #[tokio::test]
    async fn execute_streams_the_query_batch() {
        let src = Int64Array::from(vec![0, 0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 2, 0]);
        let (topo, ids) = build_topology(&src, &dst).unwrap();
        let columns = Arc::new(vec![OutputColumn::Algo {
            name: "deg".to_string(),
            algo: GraphAlgo::Degree {
                direction: Direction::Out,
            },
        }]);
        let exec = Arc::new(GraphAlgorithmExec::new(topo, ids, columns));
        let ctx = Arc::new(TaskContext::default());
        let stream = exec.execute(0, ctx).unwrap();
        let batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 3);
        assert_eq!(batches[0].num_columns(), 2);
    }
}
