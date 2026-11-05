//! Executing a graph node.
//!
//! [`GraphPlanner`] maps a [`GraphAlgorithmNode`] to a [`GraphAlgorithmExec`]
//! whose children are the planned edge plans. When it executes, it looks up
//! the index in the session's [`IndexCache`]; on a miss it drains a child,
//! partition by partition, and builds the index. Then it runs the calls and
//! emits one batch.
//!
//! Row order matters to the index: dense node ids are given in first-seen
//! order, and they decide component labels and the order floating-point sums
//! run in. The child's partitions are read in order, the node asks for no
//! repartitioning of its input, and the session turns off file-scan work
//! stealing, so a file's edges arrive in the file's own order on any thread
//! count, as `ursa-io` reads them for the Python library.
//!
//! ## Weights
//!
//! Weights are looked up by edge row, so they must be in the order of the
//! rows the index was built from. A weighted node reads only its weighted
//! child (the endpoints and every weight expression, in one pass), and:
//!
//! - on an index-cache miss, builds the index from those same rows, so the
//!   weights line up by construction;
//! - on a hit, checks that the rows' endpoints are the index's, row for row
//!   (an edge plan over a join need not produce its rows in the same order
//!   twice). If they are not, it builds a private index from these rows rather
//!   than mis-assign a weight.
//!
//! Each distinct weight expression is evaluated once per node, cast to
//! `Float64`, and checked as `ursa-plan` checks weights (no nulls, no negative
//! values, with the same messages). Weights that line up with a cached index
//! are stored on it, keyed by the weighted plan's fingerprint and file
//! identities, so the next query with the same weights reads no edges at all.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::array::{new_empty_array, Array, ArrayRef, Float64Array};
use datafusion::arrow::compute::{cast, cast_with_options, concat, CastOptions};
use datafusion::arrow::datatypes::{DataType, SchemaRef};
use datafusion::catalog::Session;
use datafusion::common::config::ConfigOptions;
use datafusion::common::tree_node::{Transformed, TreeNode, TreeNodeRecursion};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::QueryPlanner;
use datafusion::execution::TaskContext;
use datafusion::logical_expr::physical_planning_context::PhysicalPlanningContext;
use datafusion::logical_expr::{LogicalPlan, UserDefinedLogicalNode};
use datafusion::physical_expr::{EquivalenceProperties, PhysicalExpr};
use datafusion::physical_optimizer::PhysicalOptimizerRule;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::repartition::RepartitionExec;
use datafusion::physical_plan::replace_children_if_necessary;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, ExecutionPlanProperties, Partitioning,
    PlanProperties, SendableRecordBatchStream,
};
use datafusion::physical_planner::{DefaultPhysicalPlanner, ExtensionPlanner, PhysicalPlanner};
use futures::{FutureExt, StreamExt};
use ursa_core::IdError;

use super::index::{file_identities, fingerprint, FileIdentity, GraphIndex, IndexCache, Weights};
use super::kernels;
use super::node::{
    weight_column, GraphAlgorithmNode, GraphTraversalNode, Output, WEIGHTED_DESTINATION,
    WEIGHTED_SOURCE,
};
use super::traversal::{self, Traversal, TraversalParam, TraversalParamType};

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
        if let Some(node) = node.as_any().downcast_ref::<GraphTraversalNode>() {
            return plan_traversal(node, logical_inputs, physical_inputs).map(Some);
        }
        let Some(node) = node.as_any().downcast_ref::<GraphAlgorithmNode>() else {
            return Ok(None);
        };
        let (source, destination) = node.endpoint_indices()?;
        let weighted = match (logical_inputs.get(1), physical_inputs.get(1)) {
            (Some(logical), Some(physical)) => {
                Some(weighted_input(logical, physical, node.weights.len())?)
            }
            _ => None,
        };
        Ok(Some(Arc::new(GraphAlgorithmExec::new(
            Shape {
                edges: Edges {
                    graph: node.graph.clone(),
                    endpoints: (source, destination),
                    id_type: node.id_type.clone(),
                    fingerprint: fingerprint(logical_inputs[0]),
                },
                outputs: node.outputs.clone(),
                per_node: node.per_node,
                display: node.display_outputs(),
                schema: Arc::new(node.schema().as_arrow().clone()),
            },
            Arc::clone(&physical_inputs[0]),
            weighted,
        ))))
    }
}

/// The weighted edge plan `physical` (planned from `logical`), with
/// `n_weights` weight columns.
fn weighted_input(
    logical: &LogicalPlan,
    physical: &Arc<dyn ExecutionPlan>,
    n_weights: usize,
) -> Result<WeightedInput> {
    let schema = logical.schema();
    let column = |name: &str| schema.index_of_column_by_name(None, name);
    let missing =
        |name: &str| DataFusionError::Internal(format!("the weighted edge plan has no {name}"));
    let weights = (0..n_weights)
        .map(|i| {
            let name = weight_column(i);
            column(&name).ok_or_else(|| missing(&name))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(WeightedInput {
        plan: Arc::clone(physical),
        source: column(WEIGHTED_SOURCE).ok_or_else(|| missing(WEIGHTED_SOURCE))?,
        destination: column(WEIGHTED_DESTINATION).ok_or_else(|| missing(WEIGHTED_DESTINATION))?,
        weights,
        fingerprint: fingerprint(logical),
    })
}

/// The exec of a traversal node.
fn plan_traversal(
    node: &GraphTraversalNode,
    logical_inputs: &[&LogicalPlan],
    physical_inputs: &[Arc<dyn ExecutionPlan>],
) -> Result<Arc<dyn ExecutionPlan>> {
    let expected = 1 + usize::from(node.weighted.is_some()) + node.nodes.len();
    if logical_inputs.len() != expected || physical_inputs.len() != expected {
        return Err(DataFusionError::Internal(format!(
            "GraphTraversal has {expected} inputs; planned {}",
            physical_inputs.len()
        )));
    }
    let (source, destination) = node.endpoint_indices()?;
    let mut next = 1;
    let weighted = match node.weighted {
        Some(_) => {
            next += 1;
            Some(weighted_input(logical_inputs[1], &physical_inputs[1], 1)?)
        }
        None => None,
    };
    Ok(Arc::new(GraphTraversalExec::new(
        TraversalShape {
            edges: Edges {
                graph: node.graph.clone(),
                endpoints: (source, destination),
                id_type: node.id_type.clone(),
                fingerprint: fingerprint(logical_inputs[0]),
            },
            traversal: node.traversal,
            display: node.display_call(),
            schema: Arc::new(node.schema().as_arrow().clone()),
        },
        Arc::clone(&physical_inputs[0]),
        weighted,
        physical_inputs[next..].to_vec(),
    )))
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

/// A graph node's edge input: where its endpoints are, and how its index is
/// cached.
#[derive(Debug, Clone)]
struct Edges {
    graph: String,
    /// The source and destination columns of the first input.
    endpoints: (usize, usize),
    id_type: DataType,
    /// The edge plan's fingerprint, or `None` if it cannot be cached.
    fingerprint: Option<String>,
}

/// What a [`GraphAlgorithmExec`] computes, besides its children.
#[derive(Debug, Clone)]
struct Shape {
    edges: Edges,
    outputs: Vec<Output>,
    per_node: bool,
    /// The outputs as `EXPLAIN` shows them.
    display: String,
    schema: SchemaRef,
}

/// The weighted edge plan, and where its columns are.
#[derive(Debug, Clone)]
struct WeightedInput {
    plan: Arc<dyn ExecutionPlan>,
    source: usize,
    destination: usize,
    /// One column per weight expression, in the node's order.
    weights: Vec<usize>,
    /// The weighted plan's fingerprint, or `None` if it cannot be cached.
    fingerprint: Option<String>,
}

/// Builds or fetches a graph index from its child's edge rows, and runs
/// graph functions over it.
#[derive(Debug)]
pub struct GraphAlgorithmExec {
    shape: Shape,
    input: Arc<dyn ExecutionPlan>,
    weighted: Option<WeightedInput>,
    properties: Arc<PlanProperties>,
}

impl GraphAlgorithmExec {
    fn new(shape: Shape, input: Arc<dyn ExecutionPlan>, weighted: Option<WeightedInput>) -> Self {
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(Arc::clone(&shape.schema)),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Final,
            Boundedness::Bounded,
        ));
        Self {
            shape,
            input,
            weighted,
            properties,
        }
    }
}

impl DisplayAs for GraphAlgorithmExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "GraphAlgorithmExec: graph={}, outputs=[{}]",
            self.shape.edges.graph, self.shape.display
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
        let mut children = vec![&self.input];
        children.extend(self.weighted.as_ref().map(|w| &w.plan));
        children
    }

    /// The edges are read partition by partition in order; splitting them
    /// further would only scramble that order.
    fn benefits_from_input_partitioning(&self) -> Vec<bool> {
        vec![false; self.children().len()]
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
        let expected = self.children().len();
        if children.len() != expected {
            return Err(DataFusionError::Internal(format!(
                "GraphAlgorithmExec has {expected} children; got {}",
                children.len()
            )));
        }
        let mut children = children.into_iter();
        let input = children.next().expect("checked above");
        let weighted = self.weighted.clone().map(|w| WeightedInput {
            plan: children.next().expect("checked above"),
            ..w
        });
        Ok(Arc::new(Self::new(self.shape.clone(), input, weighted)))
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
        let shape = self.shape.clone();
        let input = Arc::clone(&self.input);
        let weighted = self.weighted.clone();
        let schema = Arc::clone(&shape.schema);
        let run = async move {
            let (index, weights) = match weighted {
                None => (unweighted_index(&shape.edges, input, &context).await?, None),
                Some(w) => {
                    let (index, weights) = weighted_index(&shape.edges, input, w, &context).await?;
                    (index, Some(weights))
                }
            };
            // The kernels are CPU-bound and parallel (rayon): keep them off
            // the async workers.
            tokio::task::spawn_blocking(move || {
                let weights = weights.as_deref().map_or(&[][..], Vec::as_slice);
                kernels::batch(
                    &index,
                    &shape.outputs,
                    weights,
                    shape.per_node,
                    shape.schema,
                )
            })
            .await
            .map_err(|e| DataFusionError::Execution(format!("a graph kernel failed: {e}")))?
        };
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            schema,
            futures::stream::once(run),
        )))
    }
}

/// What a [`GraphTraversalExec`] computes, besides its children.
#[derive(Debug, Clone)]
struct TraversalShape {
    edges: Edges,
    traversal: Traversal,
    /// The call as `EXPLAIN` shows it.
    display: String,
    schema: SchemaRef,
}

/// Builds or fetches a graph index from its first child's edge rows, reads
/// the node ids its other children produce, and runs a traversal.
#[derive(Debug)]
pub struct GraphTraversalExec {
    shape: TraversalShape,
    input: Arc<dyn ExecutionPlan>,
    weighted: Option<WeightedInput>,
    /// One plan per node-id parameter, in the spec's order.
    nodes: Vec<Arc<dyn ExecutionPlan>>,
    properties: Arc<PlanProperties>,
}

impl GraphTraversalExec {
    fn new(
        shape: TraversalShape,
        input: Arc<dyn ExecutionPlan>,
        weighted: Option<WeightedInput>,
        nodes: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Self {
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(Arc::clone(&shape.schema)),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Final,
            Boundedness::Bounded,
        ));
        Self {
            shape,
            input,
            weighted,
            nodes,
            properties,
        }
    }
}

impl DisplayAs for GraphTraversalExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "GraphTraversalExec: graph={}, call={}",
            self.shape.edges.graph, self.shape.display
        )
    }
}

impl ExecutionPlan for GraphTraversalExec {
    fn name(&self) -> &str {
        "GraphTraversalExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        let mut children = vec![&self.input];
        children.extend(self.weighted.as_ref().map(|w| &w.plan));
        children.extend(self.nodes.iter());
        children
    }

    /// The edges are read in order (see [`GraphAlgorithmExec`]), and the node
    /// ids in the order their query gives them, which sets the order of the
    /// rows.
    fn benefits_from_input_partitioning(&self) -> Vec<bool> {
        vec![false; self.children().len()]
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
        let expected = self.children().len();
        if children.len() != expected {
            return Err(DataFusionError::Internal(format!(
                "GraphTraversalExec has {expected} children; got {}",
                children.len()
            )));
        }
        let mut children = children.into_iter();
        let input = children.next().expect("checked above");
        let weighted = self.weighted.clone().map(|w| WeightedInput {
            plan: children.next().expect("checked above"),
            ..w
        });
        Ok(Arc::new(Self::new(
            self.shape.clone(),
            input,
            weighted,
            children.collect(),
        )))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        if partition != 0 {
            return Err(DataFusionError::Internal(format!(
                "GraphTraversalExec has one partition; asked for {partition}"
            )));
        }
        let shape = self.shape.clone();
        let input = Arc::clone(&self.input);
        let weighted = self.weighted.clone();
        let node_plans = self.nodes.clone();
        let schema = Arc::clone(&shape.schema);
        let run = async move {
            let (index, weights) = match weighted {
                None => (unweighted_index(&shape.edges, input, &context).await?, None),
                Some(w) => {
                    let (index, weights) = weighted_index(&shape.edges, input, w, &context).await?;
                    (index, Some(weights))
                }
            };
            let spec = shape.traversal.spec();
            let mut nodes = Vec::with_capacity(node_plans.len());
            for (param, plan) in spec.node_params().zip(&node_plans) {
                let batches = drain(plan, &[0], &context).await?;
                nodes.push(node_ids(&shape, param, &index, batches)?);
            }
            tokio::task::spawn_blocking(move || {
                let weights = weights.as_ref().map(|w| w[0].as_slice());
                traversal::batch(
                    &index.topology,
                    &index.ids,
                    &shape.traversal,
                    &nodes,
                    weights,
                    shape.schema,
                )
            })
            .await
            .map_err(|e| DataFusionError::Execution(format!("a graph kernel failed: {e}")))?
        };
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            schema,
            futures::stream::once(run),
        )))
    }
}

/// The ids a node-id input produced, as dense indexes of `index` (`None` for a
/// null or an id not in the graph), cast to the graph's id type first.
fn node_ids(
    shape: &TraversalShape,
    param: &TraversalParam,
    index: &GraphIndex,
    batches: Vec<Vec<ArrayRef>>,
) -> Result<Vec<Option<u32>>> {
    let (function, name, graph) = (shape.traversal.name(), param.name, &shape.edges.graph);
    let id_type = index.ids.user_type();
    let options = CastOptions {
        safe: false,
        ..CastOptions::default()
    };
    let arrays = batches
        .iter()
        .map(|b| {
            cast_with_options(&b[0], &id_type, &options).map_err(|e| {
                DataFusionError::Execution(format!(
                    "{function}({name} =>): the values must be node ids of graph '{graph}', \
                     which are {}: {}",
                    id_kind(&id_type),
                    crate::user_message(&DataFusionError::ArrowError(Box::new(e), None))
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let refs: Vec<&dyn Array> = arrays.iter().map(|a| a.as_ref()).collect();
    let ids = match refs.as_slice() {
        [] => new_empty_array(&id_type),
        _ => concat(&refs)?,
    };
    if param.ty == TraversalParamType::Node && ids.len() > 1 {
        return Err(DataFusionError::Execution(format!(
            "{function}({name} =>) must be one node id; its query returned {} rows",
            ids.len()
        )));
    }
    index
        .ids
        .dense_from_array(ids.as_ref())
        .map_err(|e| DataFusionError::Execution(format!("{function}({name} =>): {e}")))
}

/// How a message names an id type.
fn id_kind(t: &DataType) -> &'static str {
    match t {
        DataType::Int64 => "integers",
        _ => "strings",
    }
}

/// Keeps a graph node's edge rows in the order the plan produces them.
///
/// DataFusion's `EnforceDistribution` puts a round-robin repartition under an
/// operator that benefits from parallel input, such as the `FilterExec` of a
/// view with a `WHERE` (a subgraph) or the `ProjectionExec` that computes a
/// weight expression. Draining its partitions in order would then read the
/// batches out of file order, changing dense ids (and so labels and float
/// summation order) from what `ursa-io` gives the Python library. This rule
/// runs after the default rules and removes every round-robin repartition
/// below a [`GraphAlgorithmExec`]: a round-robin repartition only adds
/// parallelism, so removing it never changes a result. Hash repartitions (a
/// join's) are needed for correctness and stay.
#[derive(Debug)]
pub struct KeepEdgeOrder;

impl PhysicalOptimizerRule for KeepEdgeOrder {
    fn optimize(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        _config: &ConfigOptions,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        plan.transform_up(|node| {
            let is_graph = node.downcast_ref::<GraphAlgorithmExec>().is_some()
                || node.downcast_ref::<GraphTraversalExec>().is_some();
            if !is_graph {
                return Ok(Transformed::no(node));
            }
            let children = node
                .children()
                .into_iter()
                .map(|child| without_round_robin(Arc::clone(child)))
                .collect::<Result<Vec<_>>>()?;
            Ok(Transformed::yes(replace_children_if_necessary(
                Arc::clone(&node),
                children,
            )?))
        })
        .map(|t| t.data)
    }

    fn name(&self) -> &str {
        "keep_edge_order"
    }

    fn schema_check(&self) -> bool {
        true
    }
}

/// `plan` with its round-robin repartitions removed.
fn without_round_robin(plan: Arc<dyn ExecutionPlan>) -> Result<Arc<dyn ExecutionPlan>> {
    plan.transform_up(|node| {
        if let Some(repartition) = node.downcast_ref::<RepartitionExec>() {
            if matches!(repartition.partitioning(), Partitioning::RoundRobinBatch(_)) {
                return Ok(Transformed::yes(Arc::clone(repartition.input())));
            }
        }
        Ok(Transformed::no(node))
    })
    .map(|t| t.data)
}

/// The index cache key for the edges `input` reads, if they can be cached.
async fn index_key(
    shape: &Edges,
    input: &Arc<dyn ExecutionPlan>,
    context: &TaskContext,
) -> Result<Option<(String, Vec<FileIdentity>)>> {
    Ok(match &shape.fingerprint {
        Some(f) => Some((f.clone(), file_identities(input, context).await?)),
        None => None,
    })
}

/// The index, cached or built from the endpoint-only input.
async fn unweighted_index(
    shape: &Edges,
    input: Arc<dyn ExecutionPlan>,
    context: &Arc<TaskContext>,
) -> Result<Arc<GraphIndex>> {
    let build = {
        let (shape, input, context) = (shape.clone(), Arc::clone(&input), Arc::clone(context));
        async move {
            let (source, destination) = shape.endpoints;
            let batches = drain(&input, &[source, destination], &context).await?;
            let endpoints = endpoint_columns(batches, &shape.id_type)?;
            build_index(shape.graph, shape.id_type, Arc::new(endpoints)).await
        }
    };
    match context.session_config().get_extension::<IndexCache>() {
        Some(cache) => {
            let key = index_key(shape, &input, context).await?;
            cache.get_or_build(key, build.boxed()).await
        }
        None => build.await.map(Arc::new),
    }
}

/// The index and the weight columns, for a node that reads weights (see the
/// module docs).
async fn weighted_index(
    shape: &Edges,
    input: Arc<dyn ExecutionPlan>,
    weighted: WeightedInput,
    context: &Arc<TaskContext>,
) -> Result<(Arc<GraphIndex>, Weights)> {
    let cache = context.session_config().get_extension::<IndexCache>();
    let key = match &cache {
        Some(_) => index_key(shape, &input, context).await?,
        None => None,
    };
    let weights_key = match &weighted.fingerprint {
        Some(f) => Some(format!(
            "{f}\n{:?}",
            file_identities(&weighted.plan, context).await?
        )),
        None => None,
    };
    // Weights already evaluated over the cached index's rows: nothing to read.
    if let (Some(cache), Some(key), Some(weights_key)) = (&cache, &key, &weights_key) {
        if let Some(index) = cache.get(key) {
            if let Some(weights) = index.weights(weights_key) {
                return Ok((index, weights));
            }
        }
    }

    let mut columns = vec![weighted.source, weighted.destination];
    columns.extend(&weighted.weights);
    let batches = drain(&weighted.plan, &columns, context).await?;
    let mut weight_columns: Vec<Vec<ArrayRef>> = vec![Vec::new(); weighted.weights.len()];
    let mut endpoints = Vec::with_capacity(batches.len());
    for batch in batches {
        let mut batch = batch.into_iter();
        let (source, destination) = (batch.next(), batch.next());
        endpoints.push(vec![
            source.expect("drained with the endpoints"),
            destination.expect("drained with the endpoints"),
        ]);
        for (column, array) in weight_columns.iter_mut().zip(batch) {
            column.push(array);
        }
    }
    let endpoints = Arc::new(endpoint_columns(endpoints, &shape.id_type)?);

    let build = |endpoints: Arc<Vec<(ArrayRef, ArrayRef)>>| {
        build_index(shape.graph.clone(), shape.id_type.clone(), endpoints)
    };
    let built_here = Arc::new(AtomicBool::new(false));
    let (index, shared) = match &cache {
        Some(cache) => {
            let flag = Arc::clone(&built_here);
            let rows = Arc::clone(&endpoints);
            let index = cache
                .get_or_build(
                    key,
                    async move {
                        flag.store(true, Ordering::Relaxed);
                        build(rows).await
                    }
                    .boxed(),
                )
                .await?;
            if built_here.load(Ordering::Relaxed) || aligned(&index, &endpoints)? {
                (index, true)
            } else {
                // The cached index's rows are in another order: these weights
                // belong to an index of these rows.
                cache.record_build();
                (Arc::new(build(Arc::clone(&endpoints)).await?), false)
            }
        }
        None => (Arc::new(build(Arc::clone(&endpoints)).await?), false),
    };

    let weights: Weights = Arc::new(
        weight_columns
            .iter()
            .map(|columns| edge_weights(columns).map(Arc::new))
            .collect::<Result<_>>()?,
    );
    if let (true, Some(weights_key)) = (shared, weights_key) {
        index.store_weights(weights_key, Arc::clone(&weights));
    }
    Ok((index, weights))
}

/// Every batch of `input`, partition by partition in order, as the columns
/// `columns` of each non-empty batch.
async fn drain(
    input: &Arc<dyn ExecutionPlan>,
    columns: &[usize],
    context: &Arc<TaskContext>,
) -> Result<Vec<Vec<ArrayRef>>> {
    let mut out = Vec::new();
    let partitions = input.output_partitioning().partition_count();
    for partition in 0..partitions {
        let mut stream = input.execute(partition, Arc::clone(context))?;
        while let Some(batch) = stream.next().await {
            let batch = batch?;
            if batch.num_rows() == 0 {
                continue;
            }
            out.push(
                columns
                    .iter()
                    .map(|&c| Arc::clone(batch.column(c)))
                    .collect(),
            );
        }
    }
    Ok(out)
}

/// Each batch's endpoints, cast to the node id type.
fn endpoint_columns(
    batches: Vec<Vec<ArrayRef>>,
    id_type: &DataType,
) -> Result<Vec<(ArrayRef, ArrayRef)>> {
    batches
        .into_iter()
        .map(|batch| Ok((cast(&batch[0], id_type)?, cast(&batch[1], id_type)?)))
        .collect()
}

/// Build the index of `endpoints`, off the async workers.
async fn build_index(
    graph: String,
    id_type: DataType,
    endpoints: Arc<Vec<(ArrayRef, ArrayRef)>>,
) -> Result<GraphIndex> {
    let name = graph.clone();
    tokio::task::spawn_blocking(move || {
        let endpoints = endpoints.as_slice();
        if endpoints.is_empty() {
            let (topology, ids) = ursa_io::topology::empty_topology(&id_type);
            return Ok(GraphIndex::new(topology, ids));
        }
        let chunks: Vec<(&dyn Array, &dyn Array)> = endpoints
            .iter()
            .map(|(s, d)| (s.as_ref(), d.as_ref()))
            .collect();
        let (topology, ids) =
            ursa_io::build_topology_batches(&chunks).map_err(|e| build_error(&graph, e))?;
        Ok(GraphIndex::new(topology, ids))
    })
    .await
    .map_err(|e| DataFusionError::Execution(format!("building graph '{name}' failed: {e}")))?
}

/// Whether `endpoints` are the edge rows `index` was built from, row for row.
fn aligned(index: &GraphIndex, endpoints: &[(ArrayRef, ArrayRef)]) -> Result<bool> {
    let topology = &index.topology;
    let n = topology.n_edges();
    if endpoints.iter().map(|(s, _)| s.len()).sum::<usize>() != n {
        return Ok(false);
    }
    if n == 0 {
        return Ok(true);
    }
    // Each row's endpoints as dense ids, from the CSR and its row permutation.
    let out = topology.out();
    let (mut source, mut destination) = (vec![0u32; n], vec![0u32; n]);
    for u in 0..topology.n_nodes() {
        let slots = out.offsets[u] as usize..out.offsets[u + 1] as usize;
        for slot in slots {
            let row = out.edge_ids[slot] as usize;
            source[row] = u as u32;
            destination[row] = out.targets[slot];
        }
    }
    let user_type = index.ids.user_type();
    let given = |pick: fn(&(ArrayRef, ArrayRef)) -> &ArrayRef| -> Result<ArrayRef> {
        let arrays = endpoints
            .iter()
            .map(|e| cast(pick(e), &user_type))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let refs: Vec<&dyn Array> = arrays.iter().map(|a| a.as_ref()).collect();
        Ok(concat(&refs)?)
    };
    Ok(
        index.ids.gather_user(&source).as_ref() == given(|e| &e.0)?.as_ref()
            && index.ids.gather_user(&destination).as_ref() == given(|e| &e.1)?.as_ref(),
    )
}

/// A weight expression's values, one `f64` per edge row, checked as
/// `ursa-plan`'s `evaluate_weight` checks them, with its messages.
fn edge_weights(columns: &[ArrayRef]) -> Result<Vec<f64>> {
    let mut out = Vec::with_capacity(columns.iter().map(|c| c.len()).sum());
    for column in columns {
        let column = cast(column, &DataType::Float64)?;
        let column = column
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("cast to Float64 yields Float64Array");
        if column.null_count() > 0 {
            return Err(DataFusionError::Execution(
                "weight expression produced a null value; weights must be non-null".into(),
            ));
        }
        let values = column.values();
        if let Some(&w) = values.iter().find(|&&w| w < 0.0) {
            return Err(DataFusionError::Execution(format!(
                "weight expression produced a negative value ({w}); weights must be \
                 non-negative"
            )));
        }
        out.extend_from_slice(values);
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::{Int64Array, StringArray};

    fn index(src: &dyn Array, dst: &dyn Array) -> GraphIndex {
        let (topology, ids) = ursa_io::build_topology_batches(&[(src, dst)]).unwrap();
        GraphIndex::new(topology, ids)
    }

    fn ints(v: &[i64]) -> ArrayRef {
        Arc::new(Int64Array::from(v.to_vec()))
    }

    #[test]
    fn weights_line_up_only_with_the_rows_the_index_was_built_from() {
        let (src, dst) = (ints(&[10, 20, 10, 30]), ints(&[20, 30, 30, 10]));
        let built = index(src.as_ref(), dst.as_ref());
        // The same rows, in two batches: aligned.
        let split = vec![
            (src.slice(0, 1), dst.slice(0, 1)),
            (src.slice(1, 3), dst.slice(1, 3)),
        ];
        assert!(aligned(&built, &split).unwrap());
        // The same edge set in another order: not.
        let swapped = vec![(ints(&[20, 10, 10, 30]), ints(&[30, 20, 30, 10]))];
        assert!(!aligned(&built, &swapped).unwrap());
        // Fewer rows: not.
        assert!(!aligned(&built, &split[1..]).unwrap());

        let names = |v: &[&str]| Arc::new(StringArray::from(v.to_vec())) as ArrayRef;
        let (s, d) = (names(&["a", "b"]), names(&["b", "c"]));
        let built = index(s.as_ref(), d.as_ref());
        assert!(aligned(&built, &[(Arc::clone(&s), Arc::clone(&d))]).unwrap());
        assert!(!aligned(&built, &[(d, s)]).unwrap());
    }

    #[test]
    fn weights_are_checked_as_ursa_plan_checks_them() {
        let ok = edge_weights(&[ints(&[1, 2]), ints(&[0])]).unwrap();
        assert_eq!(ok, vec![1.0, 2.0, 0.0]);
        let negative = edge_weights(&[ints(&[1, -1])]).unwrap_err();
        assert_eq!(
            crate::user_message(&negative),
            "weight expression produced a negative value (-1); weights must be non-negative"
        );
        let null: ArrayRef = Arc::new(Int64Array::from(vec![Some(1), None]));
        assert_eq!(
            crate::user_message(&edge_weights(&[null]).unwrap_err()),
            "weight expression produced a null value; weights must be non-null"
        );
    }
}
