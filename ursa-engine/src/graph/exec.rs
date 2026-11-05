//! Executing a graph algorithm node.
//!
//! [`GraphPlanner`] maps a [`GraphAlgorithmNode`] to a [`GraphAlgorithmExec`]
//! whose child is the planned edge plan. When it executes, it looks up the
//! index in the session's [`IndexCache`]; on a miss it drains its child,
//! partition by partition, and builds the index. Then it runs the kernels and
//! emits one `(id, value...)` batch.
//!
//! Row order matters to the index: dense node ids are given in first-seen
//! order, and they decide component labels and the order floating-point sums
//! run in. The child's partitions are read in order, the node asks for no
//! repartitioning of its input, and the session turns off file-scan work
//! stealing, so a file's edges arrive in the file's own order on any thread
//! count, as `ursa-io` reads them for the Python library.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::array::{Array, ArrayRef};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::{DataType, SchemaRef};
use datafusion::catalog::Session;
use datafusion::common::tree_node::TreeNodeRecursion;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::QueryPlanner;
use datafusion::execution::TaskContext;
use datafusion::logical_expr::physical_planning_context::PhysicalPlanningContext;
use datafusion::logical_expr::{LogicalPlan, UserDefinedLogicalNode};
use datafusion::physical_expr::{EquivalenceProperties, PhysicalExpr};
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, ExecutionPlanProperties, Partitioning,
    PlanProperties, SendableRecordBatchStream,
};
use datafusion::physical_planner::{DefaultPhysicalPlanner, ExtensionPlanner, PhysicalPlanner};
use futures::{FutureExt, StreamExt};
use ursa_core::IdError;

use super::functions::Algorithm;
use super::index::{file_identities, fingerprint, GraphIndex, IndexCache};
use super::kernels;
use super::node::GraphAlgorithmNode;

/// Plans Ursa's graph nodes.
#[derive(Debug)]
pub struct GraphPlanner;

#[async_trait]
impl ExtensionPlanner for GraphPlanner {
    async fn plan_extension(
        &self,
        _planner: &dyn PhysicalPlanner,
        node: &dyn UserDefinedLogicalNode,
        logical_inputs: &[&LogicalPlan],
        physical_inputs: &[Arc<dyn ExecutionPlan>],
        _session_state: &dyn Session,
        _planning_ctx: &PhysicalPlanningContext,
    ) -> Result<Option<Arc<dyn ExecutionPlan>>> {
        let Some(node) = node.as_any().downcast_ref::<GraphAlgorithmNode>() else {
            return Ok(None);
        };
        let (source, destination) = node.endpoint_indices()?;
        Ok(Some(Arc::new(GraphAlgorithmExec::new(
            node.graph.clone(),
            Arc::clone(&physical_inputs[0]),
            (source, destination),
            node.id_type.clone(),
            node.outputs.clone(),
            fingerprint(logical_inputs[0]),
            Arc::new(node.schema().as_arrow().clone()),
        ))))
    }
}

/// A query planner whose physical planner knows Ursa's graph nodes.
#[derive(Debug)]
pub struct UrsaQueryPlanner;

#[async_trait]
impl QueryPlanner for UrsaQueryPlanner {
    async fn create_physical_plan(
        &self,
        logical_plan: &LogicalPlan,
        session_state: &dyn Session,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        DefaultPhysicalPlanner::with_extension_planners(vec![Arc::new(GraphPlanner)])
            .create_physical_plan(logical_plan, session_state)
            .await
    }
}

/// Builds or fetches a graph index from its child's edge rows, and runs
/// graph algorithms over it.
#[derive(Debug)]
pub struct GraphAlgorithmExec {
    graph: String,
    input: Arc<dyn ExecutionPlan>,
    /// The source and destination columns of the input.
    endpoints: (usize, usize),
    id_type: DataType,
    outputs: Vec<Algorithm>,
    /// The edge plan's fingerprint, or `None` if it cannot be cached.
    fingerprint: Option<String>,
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
}

impl GraphAlgorithmExec {
    pub fn new(
        graph: String,
        input: Arc<dyn ExecutionPlan>,
        endpoints: (usize, usize),
        id_type: DataType,
        outputs: Vec<Algorithm>,
        fingerprint: Option<String>,
        schema: SchemaRef,
    ) -> Self {
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(Arc::clone(&schema)),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Final,
            Boundedness::Bounded,
        ));
        Self {
            graph,
            input,
            endpoints,
            id_type,
            outputs,
            fingerprint,
            schema,
            properties,
        }
    }
}

impl DisplayAs for GraphAlgorithmExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        let outputs: Vec<String> = self.outputs.iter().map(|a| a.to_string()).collect();
        write!(
            f,
            "GraphAlgorithmExec: graph={}, outputs=[{}]",
            self.graph,
            outputs.join(", ")
        )
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
        vec![&self.input]
    }

    /// The edges are read partition by partition in order; splitting them
    /// further would only scramble that order.
    fn benefits_from_input_partitioning(&self) -> Vec<bool> {
        vec![false]
    }

    fn apply_expressions(
        &self,
        _f: &mut dyn FnMut(&Arc<dyn PhysicalExpr>) -> Result<TreeNodeRecursion>,
    ) -> Result<TreeNodeRecursion> {
        Ok(TreeNodeRecursion::Continue)
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let [input]: [Arc<dyn ExecutionPlan>; 1] = children
            .try_into()
            .map_err(|_| DataFusionError::Internal("GraphAlgorithmExec has one child".into()))?;
        Ok(Arc::new(Self::new(
            self.graph.clone(),
            input,
            self.endpoints,
            self.id_type.clone(),
            self.outputs.clone(),
            self.fingerprint.clone(),
            Arc::clone(&self.schema),
        )))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        if partition != 0 {
            return Err(DataFusionError::Internal(format!(
                "GraphAlgorithmExec has one partition; asked for {partition}"
            )));
        }
        let schema = Arc::clone(&self.schema);
        let edges = EdgeSource {
            graph: self.graph.clone(),
            input: Arc::clone(&self.input),
            endpoints: self.endpoints,
            id_type: self.id_type.clone(),
        };
        let fingerprint = self.fingerprint.clone();
        let outputs = self.outputs.clone();
        let out_schema = Arc::clone(&schema);
        let run = async move {
            let cache = context.session_config().get_extension::<IndexCache>();
            let index = match cache {
                Some(cache) => {
                    let key = match fingerprint {
                        Some(f) => Some((f, file_identities(&edges.input, &context).await?)),
                        None => None,
                    };
                    cache
                        .get_or_build(key, edges.build(Arc::clone(&context)).boxed())
                        .await?
                }
                None => Arc::new(edges.build(Arc::clone(&context)).await?),
            };
            // The kernels are CPU-bound and parallel (rayon): keep them off
            // the async workers.
            tokio::task::spawn_blocking(move || kernels::batch(&index, &outputs, out_schema))
                .await
                .map_err(|e| DataFusionError::Execution(format!("a graph kernel failed: {e}")))?
        };
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            schema,
            futures::stream::once(run),
        )))
    }
}

/// Where a graph's edges come from: the node's child plan.
struct EdgeSource {
    graph: String,
    input: Arc<dyn ExecutionPlan>,
    endpoints: (usize, usize),
    id_type: DataType,
}

impl EdgeSource {
    /// Drain the child, partition by partition in order, and build the index.
    async fn build(self, context: Arc<TaskContext>) -> Result<GraphIndex> {
        let mut columns: Vec<(ArrayRef, ArrayRef)> = Vec::new();
        let partitions = self.input.output_partitioning().partition_count();
        for partition in 0..partitions {
            let mut stream = self.input.execute(partition, Arc::clone(&context))?;
            while let Some(batch) = stream.next().await {
                let batch = batch?;
                if batch.num_rows() == 0 {
                    continue;
                }
                let source = cast(batch.column(self.endpoints.0), &self.id_type)?;
                let destination = cast(batch.column(self.endpoints.1), &self.id_type)?;
                columns.push((source, destination));
            }
        }
        let graph = self.graph;
        let name = graph.clone();
        let id_type = self.id_type;
        tokio::task::spawn_blocking(move || {
            if columns.is_empty() {
                let (topology, ids) = ursa_io::topology::empty_topology(&id_type);
                return Ok(GraphIndex { topology, ids });
            }
            let chunks: Vec<(&dyn Array, &dyn Array)> = columns
                .iter()
                .map(|(s, d)| (s.as_ref(), d.as_ref()))
                .collect();
            let (topology, ids) =
                ursa_io::build_topology_batches(&chunks).map_err(|e| build_error(&graph, e))?;
            Ok(GraphIndex { topology, ids })
        })
        .await
        .map_err(|e| DataFusionError::Execution(format!("building graph '{name}' failed: {e}")))?
    }
}

fn build_error(graph: &str, e: IdError) -> DataFusionError {
    DataFusionError::Execution(match e {
        IdError::Null => format!(
            "graph '{graph}': an edge has a null source or destination; filter those rows out \
             of the edge table (for example, with a view that keeps rows WHERE both are NOT NULL)"
        ),
        other => format!("graph '{graph}': {other}"),
    })
}
