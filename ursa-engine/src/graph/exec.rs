//! Executing a graph node.
//!
//! [`GraphPlanner`] maps a [`GraphAlgorithmNode`] to a [`GraphAlgorithmExec`]
//! whose children are the planned edge plans. When it executes, it looks up
//! the index in the session's [`IndexCache`]; on a miss it reads a child,
//! partition by partition, and builds the index as the rows arrive. Then it runs the calls and
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
//! child (the endpoints and every weight expression, in one pass, evaluating
//! the weights a batch at a time as it goes), and:
//!
//! - on an index-cache miss, builds the index from those same rows, so the
//!   weights line up by construction;
//! - on a hit, checks that the rows' endpoints are the index's, row for row
//!   (an edge plan over a join need not produce its rows in the same order
//!   twice). If they are not, it reads the child again and builds a private
//!   index from those rows rather than mis-assign a weight.
//!
//! Each distinct weight expression is evaluated once per node, cast to
//! `Float64`, and checked as `ursa-plan` checks weights (no nulls, no negative
//! values, with the same messages). Weights that line up with a cached index
//! are stored on it, keyed by the weighted plan's fingerprint and file
//! identities, so the next query with the same weights reads no edges at all.
//! In a database, they are stored with the index on disk too, so a weighted
//! query over a stored index in a later process reads no edges either.

use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use datafusion::arrow::array::{new_empty_array, Array, ArrayRef, Float64Array};
use datafusion::arrow::compute::SortOptions;
use datafusion::arrow::compute::{cast, cast_with_options, concat, CastOptions};
use datafusion::arrow::datatypes::{DataType, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::common::config::ConfigOptions;
use datafusion::common::tree_node::{Transformed, TreeNode, TreeNodeRecursion};
use datafusion::common::ScalarValue;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::QueryPlanner;
use datafusion::execution::TaskContext;
use datafusion::logical_expr::physical_planning_context::PhysicalPlanningContext;
use datafusion::logical_expr::{LogicalPlan, UserDefinedLogicalNode};
use datafusion::physical_expr::expressions::Column;
use datafusion::physical_expr::{EquivalenceProperties, PhysicalExpr};
use datafusion::physical_expr::{LexOrdering, PhysicalSortExpr};
use datafusion::physical_optimizer::PhysicalOptimizerRule;
use datafusion::physical_plan::aggregates::AggregateExec;
use datafusion::physical_plan::coalesce_partitions::CoalescePartitionsExec;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::repartition::RepartitionExec;
use datafusion::physical_plan::replace_children_if_necessary;
use datafusion::physical_plan::sorts::sort::SortExec;
use datafusion::physical_plan::sorts::sort_preserving_merge::SortPreservingMergeExec;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::union::{InterleaveExec, UnionExec};
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, ExecutionPlanProperties, Partitioning,
    PlanProperties, SendableRecordBatchStream, StatisticsArgs, StatisticsContext,
};
use datafusion::physical_planner::{DefaultPhysicalPlanner, ExtensionPlanner, PhysicalPlanner};
use futures::{FutureExt, StreamExt};
use ursa_core::{EdgeInterner, IdError, Topology};

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
        let weighted = match (
            &node.weighted,
            logical_inputs.get(1),
            physical_inputs.get(1),
        ) {
            (Some(_), Some(logical), Some(physical)) => {
                Some(weighted_input(logical, physical, node.weights.len())?)
            }
            _ => None,
        };
        // A neighbour aggregation's node values are the last input.
        let values = node
            .values
            .as_ref()
            .and_then(|_| physical_inputs.last().cloned());
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
            values,
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
    /// A neighbour aggregation's node values: node ids, then values.
    values: Option<Arc<dyn ExecutionPlan>>,
    properties: Arc<PlanProperties>,
}

impl GraphAlgorithmExec {
    fn new(
        shape: Shape,
        input: Arc<dyn ExecutionPlan>,
        weighted: Option<WeightedInput>,
        values: Option<Arc<dyn ExecutionPlan>>,
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
            values,
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
        children.extend(self.values.as_ref());
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
        let values = self
            .values
            .as_ref()
            .map(|_| children.next().expect("checked above"));
        Ok(Arc::new(Self::new(
            self.shape.clone(),
            input,
            weighted,
            values,
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
        let shape = self.shape.clone();
        let input = Arc::clone(&self.input);
        let weighted = self.weighted.clone();
        let values_plan = self.values.clone();
        let schema = Arc::clone(&shape.schema);
        let run = async move {
            let (index, weights) = match weighted {
                None => (unweighted_index(&shape.edges, input, &context).await?, None),
                Some(w) => {
                    let (index, weights) = weighted_index(&shape.edges, input, w, &context).await?;
                    (index, Some(weights))
                }
            };
            let values = match values_plan {
                Some(plan) => {
                    let batches = drain(&plan, &[0, 1], &context).await?;
                    Some(kernels::node_values(&index, &shape.edges.graph, batches)?)
                }
                None => None,
            };
            // The kernels are CPU-bound and parallel (rayon): keep them off
            // the async workers (inline in the browser, which has none).
            crate::run_blocking(move || {
                let weights = weights.as_deref().map_or(&[][..], Vec::as_slice);
                kernels::batch(
                    &index,
                    &shape.outputs,
                    weights,
                    values.as_deref(),
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
            crate::run_blocking(move || {
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
            // No float, boolean or date, and a string must be exactly an
            // integer on a graph with integer ids (#271).
            if let Some(row) = traversal::first_refused_node_id(&b[0], &id_type) {
                let got = sql_text(&b[0], row)
                    .unwrap_or_else(|| format!("a value of type {}", b[0].data_type()));
                return Err(DataFusionError::Execution(traversal::not_node_ids(
                    function, param, graph, &id_type, &got,
                )));
            }
            cast_with_options(&b[0], &id_type, &options).map_err(|e| {
                let got = first_uncast(&b[0], &id_type).unwrap_or_else(|| {
                    crate::user_message(&DataFusionError::ArrowError(Box::new(e), None))
                });
                DataFusionError::Execution(traversal::not_node_ids(
                    function, param, graph, &id_type, &got,
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

/// The first value of `values` that does not cast to `id_type`, as SQL writes
/// it (`'Valjean'`, `2.5`), for a message.
fn first_uncast(values: &ArrayRef, id_type: &DataType) -> Option<String> {
    let lenient = cast(values, id_type).ok()?;
    let row = (0..values.len()).find(|&i| values.is_valid(i) && lenient.is_null(i))?;
    sql_text(values, row)
}

/// Row `row` of `values` as SQL writes it (`'Valjean'`, `2.5`).
fn sql_text(values: &ArrayRef, row: usize) -> Option<String> {
    let mut value = ScalarValue::try_from_array(values, row).ok()?;
    while let ScalarValue::Dictionary(_, inner) = value {
        value = *inner;
    }
    Some(match value.try_as_str() {
        Some(Some(s)) => format!("'{}'", s.replace('\'', "''")),
        _ => value.to_string(),
    })
}

/// Keeps a graph node's edge rows in the order the plan produces them, and
/// gives them a fixed order where the plan has none.
///
/// DataFusion's `EnforceDistribution` puts a round-robin repartition under an
/// operator that benefits from parallel input, such as the `FilterExec` of a
/// view with a `WHERE` (a subgraph) or the `ProjectionExec` that computes a
/// weight expression. Draining its partitions in order would then read the
/// batches out of file order, changing dense ids (and so labels and float
/// summation order) from what `ursa-io` gives the Python library. This rule
/// runs after the default rules and removes every round-robin repartition
/// below a graph node: a round-robin repartition only adds parallelism, so
/// removing it never changes a result.
///
/// What remains may still not preserve order: a join, an aggregate, a hash
/// repartition, a merge of partitions or a `UNION` produces its rows in an
/// order that can change between runs and thread counts (#263). Over such a
/// plan, the rule sorts the edge rows by `(source, destination)`, and a
/// weighted node's rows by their weights after that, so that rows that tie
/// are identical in every column the node reads. A traversal's node-id
/// inputs over such a plan are sorted by value. Order-preserving plans
/// (scans, views over files, filters, projections) are left as they are,
/// in file order. The sort is a `SortExec` under the node, so `EXPLAIN`
/// shows it.
#[derive(Debug)]
pub struct KeepEdgeOrder;

impl PhysicalOptimizerRule for KeepEdgeOrder {
    fn optimize(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        _config: &ConfigOptions,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        plan.transform_up(|node| {
            // The columns each child is sorted by, if its order is not fixed.
            let keys: Vec<Vec<usize>> =
                if let Some(exec) = node.downcast_ref::<GraphAlgorithmExec>() {
                    let mut keys = vec![endpoint_keys(exec.shape.edges.endpoints)];
                    keys.extend(exec.weighted.as_ref().map(weighted_keys));
                    // A neighbour aggregation's values give one value per
                    // node whatever their order: no sort.
                    keys.extend(exec.values.as_ref().map(|_| vec![]));
                    keys
                } else if let Some(exec) = node.downcast_ref::<GraphTraversalExec>() {
                    let mut keys = vec![endpoint_keys(exec.shape.edges.endpoints)];
                    keys.extend(exec.weighted.as_ref().map(weighted_keys));
                    keys.extend(exec.nodes.iter().map(|_| vec![0]));
                    keys
                } else {
                    return Ok(Transformed::no(node));
                };
            let children = node
                .children()
                .into_iter()
                .zip(keys)
                .map(|(child, keys)| {
                    let child = without_round_robin(Arc::clone(child))?;
                    if preserves_order(&child) {
                        Ok(child)
                    } else {
                        sorted(child, &keys)
                    }
                })
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

/// The sort key of an edge input: its source, then its destination.
fn endpoint_keys((source, destination): (usize, usize)) -> Vec<usize> {
    vec![source, destination]
}

/// The sort key of a weighted edge input: its endpoints, then every weight,
/// so rows that tie are the same in every column the node reads.
fn weighted_keys(weighted: &WeightedInput) -> Vec<usize> {
    let mut keys = endpoint_keys((weighted.source, weighted.destination));
    keys.extend(&weighted.weights);
    keys
}

/// Whether reading `plan`'s partitions in order gives its rows in an order
/// that does not depend on the run or the thread count: true unless it has
/// a join, an aggregate, a repartition, a merge of partitions or a union. A
/// nested graph node emits its rows in a fixed order, whatever its inputs.
fn preserves_order(plan: &Arc<dyn ExecutionPlan>) -> bool {
    let mut preserves = true;
    let _ = plan.apply(|node| {
        if node.downcast_ref::<GraphAlgorithmExec>().is_some()
            || node.downcast_ref::<GraphTraversalExec>().is_some()
        {
            return Ok(TreeNodeRecursion::Jump);
        }
        let reorders = node.downcast_ref::<RepartitionExec>().is_some()
            || node.downcast_ref::<CoalescePartitionsExec>().is_some()
            || node.downcast_ref::<AggregateExec>().is_some()
            || node.downcast_ref::<UnionExec>().is_some()
            || node.downcast_ref::<InterleaveExec>().is_some()
            || node.name().contains("Join");
        if reorders {
            preserves = false;
            return Ok(TreeNodeRecursion::Stop);
        }
        Ok(TreeNodeRecursion::Continue)
    });
    preserves
}

/// `plan`'s rows sorted by the columns `keys`, ascending with nulls last,
/// as one partition: each partition is sorted, then they are merged.
fn sorted(plan: Arc<dyn ExecutionPlan>, keys: &[usize]) -> Result<Arc<dyn ExecutionPlan>> {
    let schema = plan.schema();
    let exprs = keys.iter().map(|&i| {
        PhysicalSortExpr::new(
            Arc::new(Column::new(schema.field(i).name(), i)) as Arc<dyn PhysicalExpr>,
            SortOptions {
                descending: false,
                nulls_first: false,
            },
        )
    });
    let Some(ordering) = LexOrdering::new(exprs) else {
        return Ok(plan);
    };
    let partitions = plan.output_partitioning().partition_count();
    let sort =
        Arc::new(SortExec::new(ordering.clone(), plan).with_preserve_partitioning(partitions > 1));
    Ok(if partitions > 1 {
        Arc::new(SortPreservingMergeExec::new(ordering, sort))
    } else {
        sort
    })
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
            let mut no_weights = WeightReader::new(&[], 0);
            build_streamed(&shape, &input, shape.endpoints, &context, &mut no_weights).await
        }
    };
    match context.session_config().get_extension::<IndexCache>() {
        Some(cache) => {
            let key = index_key(shape, &input, context).await?;
            cache
                .get_or_build_stored(context.session_config(), key, build.boxed())
                .await
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
    let n_weights = weighted.weights.len();
    // Weights already evaluated over the cached index's rows: nothing to read.
    if let (Some(cache), Some(key), Some(weights_key)) = (&cache, &key, &weights_key) {
        if let Some(index) = cache.get(key) {
            if let Some(weights) = known_weights(&index, weights_key, n_weights).await {
                return Ok((index, weights));
            }
        }
    }

    let (index, weights, shared) = match &cache {
        Some(cache) => {
            // Weights read by this node's own build, if the cache ran it.
            let slot = Arc::new(Mutex::new(None));
            let build = {
                let (shape, weighted) = (shape.clone(), weighted.clone());
                let (context, slot) = (Arc::clone(context), Arc::clone(&slot));
                async move {
                    let (index, weights) = build_weighted(&shape, &weighted, &context).await?;
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(weights);
                    Ok(index)
                }
            };
            let index = cache
                .get_or_build_stored(context.session_config(), key, build.boxed())
                .await?;
            let built_here = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            // A stored index may have these weights stored with it.
            if built_here.is_none() {
                if let Some(weights_key) = &weights_key {
                    if let Some(weights) = known_weights(&index, weights_key, n_weights).await {
                        return Ok((index, weights));
                    }
                }
            }
            match built_here {
                Some(weights) => (index, weights, true),
                None => {
                    let mut weights =
                        WeightReader::new(&weighted.weights, index.topology.n_edges());
                    if read_aligned(&index, &weighted, &shape.id_type, context, &mut weights)
                        .await?
                    {
                        (index, weights, true)
                    } else {
                        // The cached index's rows are in another order: these
                        // weights belong to an index of these rows.
                        cache.record_build();
                        let (private, weights) = build_weighted(shape, &weighted, context).await?;
                        (Arc::new(private), weights, false)
                    }
                }
            }
        }
        None => {
            let (index, weights) = build_weighted(shape, &weighted, context).await?;
            (Arc::new(index), weights, false)
        }
    };

    let weights: Weights = Arc::new(weights.finish()?.into_iter().map(Arc::new).collect());
    if let (true, Some(weights_key)) = (shared, weights_key) {
        index.store_weights(weights_key, Arc::clone(&weights));
    }
    Ok((index, weights))
}

/// The weights `index` holds under `key` (with `n_weights` columns), in
/// memory or stored with it on disk.
async fn known_weights(index: &Arc<GraphIndex>, key: &str, n_weights: usize) -> Option<Weights> {
    if let Some(weights) = index.weights(key) {
        return Some(weights);
    }
    if !index.has_stored_weights(key) {
        return None;
    }
    let (index, key) = (Arc::clone(index), key.to_string());
    // Reading a file is blocking work.
    crate::run_blocking(move || index.stored_weights(&key, n_weights))
        .await
        .ok()
        .flatten()
}

/// Build the index of the weighted rows, reading their weights in the same
/// pass, so they line up by construction.
async fn build_weighted(
    shape: &Edges,
    weighted: &WeightedInput,
    context: &Arc<TaskContext>,
) -> Result<(GraphIndex, WeightReader)> {
    let mut weights = WeightReader::new(&weighted.weights, row_hint(&weighted.plan));
    let endpoints = (weighted.source, weighted.destination);
    let index = build_streamed(shape, &weighted.plan, endpoints, context, &mut weights).await?;
    Ok((index, weights))
}

/// Read the weighted rows once, reading their weights, and say whether
/// their endpoints are the rows `index` was built from, row for row. Stops
/// at the first row that is not.
async fn read_aligned(
    index: &GraphIndex,
    weighted: &WeightedInput,
    id_type: &DataType,
    context: &Arc<TaskContext>,
    weights: &mut WeightReader,
) -> Result<bool> {
    let mut rows = RowCheck::new(index);
    let plan = &weighted.plan;
    for partition in 0..plan.output_partitioning().partition_count() {
        let mut stream = plan.execute(partition, Arc::clone(context))?;
        while let Some(batch) = stream.next().await {
            let batch = batch?;
            if batch.num_rows() == 0 {
                continue;
            }
            let src = cast(batch.column(weighted.source), id_type)?;
            let dst = cast(batch.column(weighted.destination), id_type)?;
            if !rows.check(&src, &dst)? {
                return Ok(false);
            }
            weights.push(&batch);
        }
    }
    Ok(rows.complete())
}

/// The weight columns of a weighted node's rows, evaluated a batch at a
/// time as the rows are read. The first bad weight is reported once every
/// row has been read, after any error in building the index from those
/// rows, as when the weights were evaluated after the build.
struct WeightReader {
    columns: Vec<usize>,
    values: Vec<Vec<f64>>,
    error: Option<DataFusionError>,
}

impl WeightReader {
    /// A reader of the columns `columns`, sized for `rows` rows.
    fn new(columns: &[usize], rows: usize) -> Self {
        Self {
            columns: columns.to_vec(),
            values: columns.iter().map(|_| Vec::with_capacity(rows)).collect(),
            error: None,
        }
    }

    fn push(&mut self, batch: &RecordBatch) {
        if self.error.is_some() {
            return;
        }
        for (&c, values) in self.columns.iter().zip(&mut self.values) {
            if let Err(e) = extend_weights(batch.column(c), values) {
                self.error = Some(e);
                return;
            }
        }
    }

    /// One `f64` per row read, per column, or the first bad weight.
    fn finish(self) -> Result<Vec<Vec<f64>>> {
        match self.error {
            Some(e) => Err(e),
            None => Ok(self.values),
        }
    }
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

/// Rows the index build interns at a time. A scan's batches are 8,192 rows,
/// and `EdgeInterner` resolves known ids in parallel only in larger chunks,
/// so the build gathers batches up to this many rows first: what `ursa-io`'s
/// scan does for the Python library (its `SCAN_BATCH_ROWS`). A chunk is
/// 2 MiB of `Int64` endpoints.
const INTERN_ROWS: usize = 1 << 17;

/// Endpoint chunks gathered up to [`INTERN_ROWS`] rows, in row order.
#[derive(Default)]
struct Gather {
    src: Vec<ArrayRef>,
    dst: Vec<ArrayRef>,
    rows: usize,
}

impl Gather {
    /// Add a chunk; once [`INTERN_ROWS`] rows are gathered, take them.
    fn push(&mut self, src: ArrayRef, dst: ArrayRef) -> Result<Option<(ArrayRef, ArrayRef)>> {
        self.rows += src.len();
        self.src.push(src);
        self.dst.push(dst);
        if self.rows >= INTERN_ROWS {
            self.take()
        } else {
            Ok(None)
        }
    }

    /// The rows gathered so far, as one chunk, if there are any.
    fn take(&mut self) -> Result<Option<(ArrayRef, ArrayRef)>> {
        self.rows = 0;
        let (src, dst) = (std::mem::take(&mut self.src), std::mem::take(&mut self.dst));
        Ok(match src.len() {
            0 => None,
            1 => src.into_iter().zip(dst).next(),
            _ => {
                let join = |arrays: &[ArrayRef]| {
                    let refs: Vec<&dyn Array> = arrays.iter().map(|a| a.as_ref()).collect();
                    concat(&refs)
                };
                Some((join(&src)?, join(&dst)?))
            }
        })
    }
}

/// The index of everything `interner` was given; an empty graph of
/// `id_type` ids if it was given no edges.
fn finish_index(interner: EdgeInterner, id_type: &DataType) -> GraphIndex {
    if interner.is_empty() {
        let (topology, ids) = ursa_io::topology::empty_topology(id_type);
        return GraphIndex::new(topology, ids);
    }
    let (ids, source, destination) = interner.finish();
    let topology = Topology::build(ids.len(), source, destination);
    GraphIndex::new(Arc::new(topology), Arc::new(ids))
}

/// The number of rows `plan` will produce, if its statistics say: sizes the
/// interner, as `ursa-io`'s scan sizes it from the Parquet metadata.
fn row_hint(plan: &Arc<dyn ExecutionPlan>) -> usize {
    StatisticsContext::new()
        .compute(plan.as_ref(), &StatisticsArgs::new())
        .ok()
        .and_then(|s| s.num_rows.get_value().copied())
        .unwrap_or(0)
}

/// What the reader hands the interning thread.
enum Chunk {
    Edges(ArrayRef, ArrayRef),
    /// Every row has been read: build the index.
    End,
}

/// Build the index of the edge rows `input` produces, interning each chunk
/// as it is read rather than collecting every endpoint array first, so the
/// Arrow input never accumulates: the build holds the dense endpoints and a
/// chunk or two in flight, as `ursa-io`'s scan does. Reading (here) and
/// interning (on a blocking thread) overlap. The rows are interned in the
/// order [`drain`] would collect them, so the index is the one
/// `ursa_io::build_topology_batches` gives over the collected rows. A
/// weighted node's weights are read from the same rows (`weights`).
async fn build_streamed(
    shape: &Edges,
    input: &Arc<dyn ExecutionPlan>,
    (source, destination): (usize, usize),
    context: &Arc<TaskContext>,
    weights: &mut WeightReader,
) -> Result<GraphIndex> {
    let (graph, id_type) = (shape.graph.clone(), shape.id_type.clone());
    let hint = row_hint(input);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Chunk>(2);

    // Native: the interner runs on a blocking thread, so reading and interning
    // overlap on two threads. It is spawned here, before the reader starts
    // (`run_blocking` would start it only when awaited, after the reader).
    #[cfg(not(target_arch = "wasm32"))]
    let interning = tokio::task::spawn_blocking(move || -> Result<Option<GraphIndex>> {
        let mut interner = EdgeInterner::with_capacity(hint);
        while let Some(chunk) = rx.blocking_recv() {
            match chunk {
                Chunk::Edges(src, dst) => interner
                    .push(src.as_ref(), dst.as_ref())
                    .map_err(|e| build_error(&graph, e))?,
                Chunk::End => return Ok(Some(finish_index(interner, &id_type))),
            }
        }
        // The reader stopped before the end (its error is the one reported).
        Ok(None)
    });
    // The browser (wasm32) has one thread: the interner is a future polled
    // alongside the reader, taking each chunk as it is sent. The channel's
    // bound still keeps at most two chunks in flight, and the chunks arrive in
    // the same order, so the index is the same (#278).
    #[cfg(target_arch = "wasm32")]
    let interning = async move {
        let mut interner = EdgeInterner::with_capacity(hint);
        while let Some(chunk) = rx.recv().await {
            match chunk {
                Chunk::Edges(src, dst) => interner
                    .push(src.as_ref(), dst.as_ref())
                    .map_err(|e| build_error(&graph, e))?,
                Chunk::End => return Ok(Some(finish_index(interner, &id_type))),
            }
        }
        Ok::<_, DataFusionError>(None)
    };

    let read = async {
        // The reader owns the sender, so the interner sees the channel close
        // when the reader stops, on an error as at the end.
        let tx = tx;
        let mut gather = Gather::default();
        for partition in 0..input.output_partitioning().partition_count() {
            let mut stream = input.execute(partition, Arc::clone(context))?;
            while let Some(batch) = stream.next().await {
                let batch = batch?;
                if batch.num_rows() == 0 {
                    continue;
                }
                let src = cast(batch.column(source), &shape.id_type)?;
                let dst = cast(batch.column(destination), &shape.id_type)?;
                weights.push(&batch);
                drop(batch);
                if let Some((src, dst)) = gather.push(src, dst)? {
                    if tx.send(Chunk::Edges(src, dst)).await.is_err() {
                        // The interner stopped on an error: it is reported below.
                        return Ok(());
                    }
                }
            }
        }
        if let Some((src, dst)) = gather.take()? {
            if tx.send(Chunk::Edges(src, dst)).await.is_err() {
                return Ok(());
            }
        }
        let _ = tx.send(Chunk::End).await;
        Ok::<_, DataFusionError>(())
    };
    #[cfg(not(target_arch = "wasm32"))]
    let (read, built) = {
        let read = read.await;
        let built = interning.await.map_err(|e| {
            DataFusionError::Execution(format!("building graph '{}' failed: {e}", shape.graph))
        })?;
        (read, built)
    };
    #[cfg(target_arch = "wasm32")]
    let (read, built) = futures::join!(read, interning);
    read?;
    built?.ok_or_else(|| {
        DataFusionError::Internal(format!(
            "building graph '{}' stopped before the last edge",
            shape.graph
        ))
    })
}

/// Checks, a chunk at a time, that rows are the edge rows an index was
/// built from, in order.
struct RowCheck<'a> {
    index: &'a GraphIndex,
    /// Each row's endpoints as dense ids, from the CSR and its row
    /// permutation.
    source: Vec<u32>,
    destination: Vec<u32>,
    /// Rows checked so far.
    start: usize,
}

impl<'a> RowCheck<'a> {
    fn new(index: &'a GraphIndex) -> Self {
        let topology = &index.topology;
        let n = topology.n_edges();
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
        Self {
            index,
            source,
            destination,
            start: 0,
        }
    }

    /// Whether the next rows are `src` → `dst`. Each chunk's ids are
    /// compared on their own, so no column of every row's ids is made.
    fn check(&mut self, src: &ArrayRef, dst: &ArrayRef) -> Result<bool> {
        let rows = self.start..self.start + src.len();
        if rows.end > self.source.len() {
            return Ok(false);
        }
        self.start = rows.end;
        let ids = &self.index.ids;
        let user_type = ids.user_type();
        Ok(
            ids.gather_user(&self.source[rows.clone()]).as_ref() == cast(src, &user_type)?.as_ref()
                && ids.gather_user(&self.destination[rows]).as_ref()
                    == cast(dst, &user_type)?.as_ref(),
        )
    }

    /// Whether every row of the index has been checked.
    fn complete(&self) -> bool {
        self.start == self.source.len()
    }
}

/// Whether `endpoints` are the edge rows `index` was built from, row for row.
#[cfg(test)]
fn aligned(index: &GraphIndex, endpoints: &[(ArrayRef, ArrayRef)]) -> Result<bool> {
    let mut rows = RowCheck::new(index);
    for (src, dst) in endpoints {
        if !rows.check(src, dst)? {
            return Ok(false);
        }
    }
    Ok(rows.complete())
}

/// A weight expression's values, one `f64` per edge row, checked as
/// `ursa-plan`'s `evaluate_weight` checks them, with its messages.
#[cfg(test)]
fn edge_weights(columns: &[ArrayRef]) -> Result<Vec<f64>> {
    let mut out = Vec::with_capacity(columns.iter().map(|c| c.len()).sum());
    for column in columns {
        extend_weights(column, &mut out)?;
    }
    Ok(out)
}

/// Append one chunk of a weight expression's values to `out`, checked as
/// `ursa-plan`'s `evaluate_weight` checks them, with its messages.
fn extend_weights(column: &ArrayRef, out: &mut Vec<f64>) -> Result<()> {
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
    Ok(())
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
