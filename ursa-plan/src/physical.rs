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
use ursa_core::{Direction, EdgeMask, IdMap, Topology};

use crate::result::{
    hop_batch, hop_schema, path_batch, path_schema, query_batch, query_schema, walk_batch,
    walk_schema, OutputColumn,
};

/// A leaf `ExecutionPlan` that runs a graph query's algorithms and emits a
/// single `(id, values...)` `RecordBatch`.
#[derive(Debug)]
pub struct GraphAlgorithmExec {
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    columns: Arc<Vec<OutputColumn>>,
    /// Optional subgraph view (#114), forwarded to the kernels via `query_batch`.
    mask: Option<Arc<EdgeMask>>,
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
}

impl GraphAlgorithmExec {
    pub fn new(
        topology: Arc<Topology>,
        ids: Arc<IdMap>,
        columns: Arc<Vec<OutputColumn>>,
        mask: Option<Arc<EdgeMask>>,
    ) -> Self {
        let schema = query_schema(&columns, ids.user_type());
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Final,
            Boundedness::Bounded,
        ));
        GraphAlgorithmExec {
            topology,
            ids,
            columns,
            mask,
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

    fn properties(&self) -> &Arc<PlanProperties> {
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
        let mask = self.mask.clone();

        // CPU-bound (Rayon-parallel inside) — keep it off the tokio worker.
        let fut = async move {
            tokio::task::spawn_blocking(move || query_batch(&topo, &ids, &columns, mask.as_deref()))
                .await
                .map_err(|e| DataFusionError::Execution(format!("graph kernel panicked: {e}")))?
        };

        let stream = RecordBatchStreamAdapter::new(schema, futures::stream::once(fut));
        Ok(Box::pin(stream))
    }
}

/// A leaf `ExecutionPlan` that runs a `hop` traversal and emits a single
/// `(src, dst)` edge `RecordBatch` of reached pairs. Produced from a
/// [`crate::node::HopNode`] by [`crate::planner::GraphExtensionPlanner`].
#[derive(Debug)]
pub struct HopExec {
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    seeds: Arc<Vec<u32>>,
    n: u32,
    direction: Direction,
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
}

impl HopExec {
    pub fn new(
        topology: Arc<Topology>,
        ids: Arc<IdMap>,
        seeds: Arc<Vec<u32>>,
        n: u32,
        direction: Direction,
    ) -> Self {
        let schema = hop_schema(ids.user_type());
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Final,
            Boundedness::Bounded,
        ));
        HopExec {
            topology,
            ids,
            seeds,
            n,
            direction,
            schema,
            properties,
        }
    }
}

impl DisplayAs for HopExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "HopExec: n={}, direction={:?}, seeds={}",
            self.n,
            self.direction,
            self.seeds.len()
        )
    }
}

impl ExecutionPlan for HopExec {
    fn name(&self) -> &str {
        "HopExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
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
        let seeds = self.seeds.clone();
        let (n, direction) = (self.n, self.direction);

        // CPU-bound frontier walk — keep it off the tokio worker.
        let fut = async move {
            tokio::task::spawn_blocking(move || hop_batch(&topo, &ids, &seeds, n, direction))
                .await
                .map_err(|e| DataFusionError::Execution(format!("hop kernel panicked: {e}")))?
        };

        let stream = RecordBatchStreamAdapter::new(schema, futures::stream::once(fut));
        Ok(Box::pin(stream))
    }
}

/// A leaf `ExecutionPlan` that runs a `shortest_path` traversal and emits a single
/// `(src, dst, hop, cost)` edge `RecordBatch` of the path edges in order. Produced from a
/// [`crate::node::ShortestPathNode`] by [`crate::planner::GraphExtensionPlanner`].
#[derive(Debug)]
pub struct ShortestPathExec {
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    source: u32,
    target: u32,
    direction: Direction,
    weights: Option<Arc<Vec<f64>>>,
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
}

impl ShortestPathExec {
    pub fn new(
        topology: Arc<Topology>,
        ids: Arc<IdMap>,
        source: u32,
        target: u32,
        direction: Direction,
        weights: Option<Arc<Vec<f64>>>,
    ) -> Self {
        let schema = path_schema(ids.user_type());
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Final,
            Boundedness::Bounded,
        ));
        ShortestPathExec {
            topology,
            ids,
            source,
            target,
            direction,
            weights,
            schema,
            properties,
        }
    }
}

impl DisplayAs for ShortestPathExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "ShortestPathExec: source={}, target={}, direction={:?}",
            self.source, self.target, self.direction
        )
    }
}

impl ExecutionPlan for ShortestPathExec {
    fn name(&self) -> &str {
        "ShortestPathExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
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
        let weights = self.weights.clone();
        let (source, target, direction) = (self.source, self.target, self.direction);

        // CPU-bound frontier walk — keep it off the tokio worker.
        let fut = async move {
            tokio::task::spawn_blocking(move || {
                path_batch(
                    &topo,
                    &ids,
                    source,
                    target,
                    direction,
                    weights.as_deref().map(Vec::as_slice),
                )
            })
            .await
            .map_err(|e| {
                DataFusionError::Execution(format!("shortest_path kernel panicked: {e}"))
            })?
        };

        let stream = RecordBatchStreamAdapter::new(schema, futures::stream::once(fut));
        Ok(Box::pin(stream))
    }
}

/// A leaf `ExecutionPlan` that runs a `random_walk` and emits a single
/// `(walk_id, step, node)` node `RecordBatch`. Produced from a
/// [`crate::node::RandomWalkNode`] by [`crate::planner::GraphExtensionPlanner`].
#[derive(Debug)]
pub struct RandomWalkExec {
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    starts: Arc<Vec<u32>>,
    steps: u32,
    walks_per_node: u32,
    seed: Option<u64>,
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
}

impl RandomWalkExec {
    pub fn new(
        topology: Arc<Topology>,
        ids: Arc<IdMap>,
        starts: Arc<Vec<u32>>,
        steps: u32,
        walks_per_node: u32,
        seed: Option<u64>,
    ) -> Self {
        let schema = walk_schema(ids.user_type());
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Final,
            Boundedness::Bounded,
        ));
        RandomWalkExec {
            topology,
            ids,
            starts,
            steps,
            walks_per_node,
            seed,
            schema,
            properties,
        }
    }
}

impl DisplayAs for RandomWalkExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "RandomWalkExec: steps={}, walks_per_node={}, starts={}, seed={:?}",
            self.steps,
            self.walks_per_node,
            self.starts.len(),
            self.seed
        )
    }
}

impl ExecutionPlan for RandomWalkExec {
    fn name(&self) -> &str {
        "RandomWalkExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
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
        let starts = self.starts.clone();
        let (steps, walks_per_node, seed) = (self.steps, self.walks_per_node, self.seed);

        // CPU-bound stochastic walk — keep it off the tokio worker.
        let fut = async move {
            tokio::task::spawn_blocking(move || {
                walk_batch(&topo, &ids, &starts, steps, walks_per_node, seed)
            })
            .await
            .map_err(|e| DataFusionError::Execution(format!("random_walk kernel panicked: {e}")))?
        };

        let stream = RecordBatchStreamAdapter::new(schema, futures::stream::once(fut));
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logical::{Direction as PlanDirection, GraphAlgo};
    use crate::result::OutputDtype;
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
                direction: PlanDirection::Out,
            },
            weights: None,
            dtype: OutputDtype::F64,
        }]);
        let exec = Arc::new(GraphAlgorithmExec::new(topo, ids, columns, None));
        let ctx = Arc::new(TaskContext::default());
        let stream = exec.execute(0, ctx).unwrap();
        let batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 3);
        assert_eq!(batches[0].num_columns(), 2);
    }

    #[tokio::test]
    async fn hop_exec_streams_reached_edges() {
        // path 0 -> 1 -> 2 -> 3
        let src = Int64Array::from(vec![0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 3]);
        let (topo, ids) = build_topology(&src, &dst).unwrap();
        // seed dense index of user id 0
        let seed = ids.dense_from_array(&Int64Array::from(vec![0])).unwrap()[0].unwrap();
        let exec = Arc::new(HopExec::new(
            topo,
            ids,
            Arc::new(vec![seed]),
            2,
            Direction::Out,
        ));
        let ctx = Arc::new(TaskContext::default());
        let stream = exec.execute(0, ctx).unwrap();
        let batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();
        assert_eq!(batches.len(), 1);
        // from 0 within 2 hops: reaches 1 and 2 -> 2 rows, (src, dst)
        assert_eq!(batches[0].num_columns(), 2);
        assert_eq!(batches[0].num_rows(), 2);
    }
}
