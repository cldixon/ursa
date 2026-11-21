//! Executing a graph node.
//!
//! [`GraphPlanner`] maps a [`GraphAlgorithmNode`] to a [`GraphAlgorithmExec`]
//! whose children are the planned edge plans. When it executes, it looks up
//! the index in the session's [`IndexCache`]; on a miss it reads a child,
//! its partitions decoded in parallel and handed on in partition order, and
//! builds the index as the rows arrive. Then it runs the calls and emits one
//! batch.
//!
//! Row order matters to the index: dense node ids are given in first-seen
//! order, and they decide component labels and the order floating-point sums
//! run in. The child's partitions are read in order, the node asks for no
//! repartitioning of its input, and the session turns off file-scan work
//! stealing, so a file's edges arrive in the file's own order on any thread
//! count, as `ursa-io` reads them for the Python library.
//!
//! ## A vertex table
//!
//! A graph with a vertex table has its keys as its nodes (#311). Its node has
//! one more child, the keys ([`GraphAlgorithmNode::vertices`]), which the
//! build reads first: each key takes the next dense id, in the table's order,
//! so a node on no edge is a node all the same. The edges are read after,
//! and an edge whose source or destination is not a key is dropped, as an
//! inner join of the edges with the vertex table drops it (DuckPGQ's CSR is
//! built that way). The index counts them ([`GraphIndex::n_dropped`]), for
//! `describe`, and keeps which rows it kept, so weights read row by row line
//! up with the kept edges and a path's edges are still the edge table's rows.
//! The keys' plan is part of the index's cache key: its fingerprint, and the
//! identity of the files it reads, so a changed vertex table builds afresh.
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
use datafusion::arrow::array::{new_empty_array, Array, ArrayRef, BooleanArray, Float64Array};
use datafusion::arrow::buffer::BooleanBuffer;
use datafusion::arrow::compute::SortOptions;
use datafusion::arrow::compute::{cast, cast_with_options, concat, CastOptions};
use datafusion::arrow::compute::{filter, filter_record_batch};
use datafusion::arrow::datatypes::{DataType, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::common::config::ConfigOptions;
use datafusion::common::tree_node::{Transformed, TreeNode, TreeNodeRecursion};
use datafusion::common::ScalarValue;
use datafusion::datasource::physical_plan::FileScanConfig;
use datafusion::datasource::source::DataSourceExec;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::QueryPlanner;
use datafusion::execution::TaskContext;
use datafusion::logical_expr::physical_planning_context::PhysicalPlanningContext;
use datafusion::logical_expr::{Expr, LogicalPlan, UserDefinedLogicalNode};
use datafusion::physical_expr::expressions::Column;
use datafusion::physical_expr::{EquivalenceProperties, PhysicalExpr};
use datafusion::physical_expr::{LexOrdering, PhysicalSortExpr};
use datafusion::physical_optimizer::PhysicalOptimizerRule;
use datafusion::physical_plan::aggregates::AggregateExec;
use datafusion::physical_plan::coalesce_partitions::CoalescePartitionsExec;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::filter::FilterExec;
use datafusion::physical_plan::metrics::{ExecutionPlanMetricsSet, MetricBuilder, MetricsSet};
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
use futures::{FutureExt, TryStreamExt};
use ursa_core::{EdgeInterner, IdError, Topology};

use super::in_order::{read_ahead, InOrder};
use super::index::{
    file_identities, fingerprint, portable_in, BuildInfo, BuildSize, FileIdentity, GraphIndex,
    IndexCache, IndexHeld, Weights,
};
use super::kernels;
use super::node::{
    weight_column, GraphAlgorithmNode, GraphTraversalNode, Output, WEIGHTED_DESTINATION,
    WEIGHTED_SOURCE,
};
use super::path_search::{self, PathColumns, PathSearchNode, Search};
use super::schema_metadata::{SchemaMetadataExec, SchemaMetadataNode};
use super::traversal::{self, Traversal, TraversalParam, TraversalParamType};
use crate::notices::{NoticeLevel, Notifier};

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
        session_state: &dyn Session,
        _planning_ctx: &PhysicalPlanningContext,
    ) -> Result<Option<Arc<dyn ExecutionPlan>>> {
        let unique = |graph: &str, source: &Expr, destination: &Expr| {
            key_names(session_state, graph, source, destination)
        };
        if let Some(node) = node.as_any().downcast_ref::<SchemaMetadataNode>() {
            return Ok(Some(Arc::new(SchemaMetadataExec::try_new(
                Arc::clone(&physical_inputs[0]),
                Arc::new(node.schema().as_arrow().clone()),
            )?)));
        }
        if let Some(node) = node.as_any().downcast_ref::<GraphTraversalNode>() {
            let unique = unique(&node.graph, &node.source, &node.destination);
            return plan_traversal(node, logical_inputs, physical_inputs, unique).map(Some);
        }
        if let Some(node) = node.as_any().downcast_ref::<PathSearchNode>() {
            let unique = unique(&node.graph, &node.source, &node.destination);
            let mut exec = plan_path_search(node, logical_inputs, physical_inputs, unique)?;
            exec.held = held_index(&exec.shape.edges, &exec.input, session_state).await;
            return Ok(Some(Arc::new(exec)));
        }
        let Some(node) = node.as_any().downcast_ref::<GraphAlgorithmNode>() else {
            return Ok(None);
        };
        let (source, destination) = node.endpoint_indices()?;
        let mut next = 1;
        let weighted = match (
            &node.weighted,
            logical_inputs.get(next),
            physical_inputs.get(next),
        ) {
            (Some(_), Some(logical), Some(physical)) => {
                next += 1;
                Some(weighted_input(logical, physical, node.weights.len())?)
            }
            _ => None,
        };
        let vertices = vertex_input(
            node.vertices.is_some(),
            logical_inputs,
            physical_inputs,
            next,
            unique(&node.graph, &node.source, &node.destination),
        )?;
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
                    vertices,
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

/// The vertex table whose keys are the nodes of `graph`'s edges from
/// `source` to `destination` (the endpoint columns of a graph node), when
/// they are one vertex table's: its keys must then identify one row each
/// (#411). `None` for the keys of two vertex tables (a `GRAPH_TABLE` search
/// over an edge table between two of them), which may share values.
fn key_names(
    session: &dyn Session,
    graph: &str,
    source: &Expr,
    destination: &Expr,
) -> Option<KeyNames> {
    let graph = session
        .config()
        .get_extension::<super::GraphRegistry>()?
        .get(graph)?;
    let (Expr::Column(source), Expr::Column(destination)) = (source, destination) else {
        return None;
    };
    let table = source.relation.as_ref()?;
    let edges = graph.edge_tables.iter().find(|e| {
        &e.table == table && e.source == source.name && e.destination == destination.name
    })?;
    let from = graph.endpoint_vertex(edges, true)?;
    let to = graph.endpoint_vertex(edges, false)?;
    (from.name == to.name).then(|| KeyNames {
        table: from.name.clone(),
        key: from.key.clone(),
    })
}

/// The vertex keys input at `at`, if the node has one; `unique` as
/// [`VertexInput::unique`].
fn vertex_input(
    has: bool,
    logical_inputs: &[&LogicalPlan],
    physical_inputs: &[Arc<dyn ExecutionPlan>],
    at: usize,
    unique: Option<KeyNames>,
) -> Result<Option<VertexInput>> {
    if !has {
        return Ok(None);
    }
    match (logical_inputs.get(at), physical_inputs.get(at)) {
        (Some(logical), Some(physical)) => Ok(Some(VertexInput {
            plan: Arc::clone(physical),
            fingerprint: fingerprint(logical),
            unique,
        })),
        _ => Err(DataFusionError::Internal(
            "a graph node's vertex keys were not planned".to_string(),
        )),
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
    unique: Option<KeyNames>,
) -> Result<Arc<dyn ExecutionPlan>> {
    let expected = 1
        + usize::from(node.weighted.is_some())
        + usize::from(node.vertices.is_some())
        + node.nodes.len();
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
    let vertices = vertex_input(
        node.vertices.is_some(),
        logical_inputs,
        physical_inputs,
        next,
        unique,
    )?;
    next += usize::from(vertices.is_some());
    Ok(Arc::new(GraphTraversalExec::new(
        TraversalShape {
            edges: Edges {
                graph: node.graph.clone(),
                endpoints: (source, destination),
                id_type: node.id_type.clone(),
                fingerprint: fingerprint(logical_inputs[0]),
                vertices,
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

/// Where the session keeps the index of the edges `input` reads, if it
/// does: then running the node reads none of its edge or vertex inputs.
async fn held_index(
    shape: &Edges,
    input: &Arc<dyn ExecutionPlan>,
    session: &dyn Session,
) -> Option<IndexHeld> {
    let context = session.task_ctx();
    let cache = context.session_config().get_extension::<IndexCache>()?;
    let key = index_key(shape, input, &context).await.ok()??;
    cache.holds(context.session_config(), &key)
}

fn plan_path_search(
    node: &PathSearchNode,
    logical_inputs: &[&LogicalPlan],
    physical_inputs: &[Arc<dyn ExecutionPlan>],
    unique: Option<KeyNames>,
) -> Result<PathSearchExec> {
    let expected = 2 + usize::from(node.vertices.is_some()) + usize::from(node.targets.is_some());
    if physical_inputs.len() != expected || logical_inputs.len() != expected {
        return Err(DataFusionError::Internal(format!(
            "PathSearch has {expected} inputs; planned {}",
            physical_inputs.len()
        )));
    }
    let vertices = vertex_input(
        node.vertices.is_some(),
        logical_inputs,
        physical_inputs,
        1,
        unique,
    )?;
    let starts = 1 + usize::from(vertices.is_some());
    Ok(PathSearchExec::new(
        PathShape {
            edges: Edges {
                graph: node.graph.clone(),
                endpoints: node.endpoint_indices()?,
                id_type: node.id_type.clone(),
                fingerprint: fingerprint(logical_inputs[0]),
                vertices,
            },
            search: node.search,
            columns: node.columns,
            known_only: node.known_only,
            schema: Arc::new(node.schema().as_arrow().clone()),
        },
        Arc::clone(&physical_inputs[0]),
        Arc::clone(&physical_inputs[starts]),
        node.targets
            .as_ref()
            .map(|_| Arc::clone(&physical_inputs[starts + 1])),
    ))
}

#[derive(Debug, Clone)]
struct PathShape {
    edges: Edges,
    search: Search,
    columns: PathColumns,
    known_only: bool,
    schema: SchemaRef,
}

/// Builds or fetches a graph index from its first child's edge rows, reads
/// the start nodes its next child produces (and the targets its last, if it
/// has them), and searches paths from them (see [`super::path_search`]).
/// `EXPLAIN ANALYZE` shows how many path prefixes it extended (`expanded`).
/// When the session held the index as the node was planned, `EXPLAIN` says
/// so (`index=stored, sources: 2 files, not read`, #351): once the plan is
/// optimized, its edge and vertex inputs, which do not run, are no longer
/// its children, so `EXPLAIN` does not draw them (#384, N48). They are kept,
/// optimized, to build the index from should the session no longer hold it
/// when the node runs.
#[derive(Debug)]
pub struct PathSearchExec {
    shape: PathShape,
    input: Arc<dyn ExecutionPlan>,
    starts: Arc<dyn ExecutionPlan>,
    targets: Option<Arc<dyn ExecutionPlan>>,
    /// Where the session kept the index when the node was planned.
    held: Option<IndexHeld>,
    /// Whether the edge and vertex inputs are left out of the children: set
    /// by [`CollapseHeldInputs`] when the index is `held`.
    collapsed: bool,
    properties: Arc<PlanProperties>,
    metrics: ExecutionPlanMetricsSet,
}

impl PathSearchExec {
    fn new(
        shape: PathShape,
        input: Arc<dyn ExecutionPlan>,
        starts: Arc<dyn ExecutionPlan>,
        targets: Option<Arc<dyn ExecutionPlan>>,
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
            starts,
            targets,
            held: None,
            collapsed: false,
            properties,
            metrics: ExecutionPlanMetricsSet::new(),
        }
    }

    /// This node with its edge and vertex inputs left out of its children.
    fn collapse(&self) -> Self {
        let mut exec = Self::new(
            self.shape.clone(),
            Arc::clone(&self.input),
            Arc::clone(&self.starts),
            self.targets.clone(),
        );
        exec.held = self.held;
        exec.collapsed = true;
        exec
    }

    /// What the index is built from, for `EXPLAIN`: `2 files`, `1 file and
    /// 1 other source`.
    fn sources(&self) -> String {
        let mut files = Vec::new();
        let mut others = 0usize;
        let plans =
            std::iter::once(&self.input).chain(self.shape.edges.vertices.as_ref().map(|v| &v.plan));
        for plan in plans {
            let _ =
                plan.apply(|node| {
                    match node
                        .downcast_ref::<DataSourceExec>()
                        .and_then(|exec| exec.data_source().downcast_ref::<FileScanConfig>())
                    {
                        Some(config) => files.extend(config.file_groups.iter().flat_map(|g| {
                            g.files().iter().map(|f| f.object_meta.location.clone())
                        })),
                        None if node.children().is_empty() => others += 1,
                        None => {}
                    }
                    Ok(TreeNodeRecursion::Continue)
                });
        }
        files.sort();
        files.dedup();
        let plural = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
        match (files.len(), others) {
            (0, 0) => "none".to_string(),
            (f, 0) => plural(f, "file"),
            (0, o) => plural(o, "source"),
            (f, o) => format!("{} and {}", plural(f, "file"), plural(o, "other source")),
        }
    }
}

impl DisplayAs for PathSearchExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "PathSearchExec: graph={}, search={}{}",
            self.shape.edges.graph,
            self.shape.search,
            if self.targets.is_some() {
                ", targets"
            } else {
                ""
            }
        )?;
        if let Some(held) = self.held {
            write!(f, ", index={held}, sources: {}, not read", self.sources())?;
        }
        Ok(())
    }
}

impl ExecutionPlan for PathSearchExec {
    fn name(&self) -> &str {
        "PathSearchExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        let mut children = Vec::new();
        if !self.collapsed {
            children.push(&self.input);
            children.extend(self.shape.edges.vertices.as_ref().map(|v| &v.plan));
        }
        children.push(&self.starts);
        children.extend(self.targets.as_ref());
        children
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

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
                "PathSearchExec has {expected} children; got {}",
                children.len()
            )));
        }
        let mut children = children.into_iter();
        let mut shape = self.shape.clone();
        let input = if self.collapsed {
            Arc::clone(&self.input)
        } else {
            let input = children.next().expect("checked above");
            shape.edges.vertices = shape.edges.vertices.map(|v| VertexInput {
                plan: children.next().expect("checked above"),
                ..v
            });
            input
        };
        let starts = children.next().expect("checked above");
        let targets = self.targets.as_ref().and_then(|_| children.next());
        let mut exec = Self::new(shape, input, starts, targets);
        exec.held = self.held;
        exec.collapsed = self.collapsed;
        Ok(Arc::new(exec))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        if partition != 0 {
            return Err(DataFusionError::Internal(format!(
                "PathSearchExec has one partition; asked for {partition}"
            )));
        }
        let shape = self.shape.clone();
        let input = Arc::clone(&self.input);
        let starts = Arc::clone(&self.starts);
        let targets = self.targets.clone();
        let schema = Arc::clone(&shape.schema);
        let expanded = MetricBuilder::new(&self.metrics).counter("expanded", partition);
        let max_paths = crate::options::max_paths(context.session_config());
        let run = async move {
            let index = unweighted_index(&shape.edges, input, &context).await?;
            let starts = search_keys(&starts, "start nodes", &shape, &context).await?;
            let targets = match &targets {
                Some(t) => Some(search_keys(t, "target nodes", &shape, &context).await?),
                None => None,
            };
            let out_schema = Arc::clone(&shape.schema);
            let (batches, n) = crate::run_blocking(move || {
                path_search::run(
                    &index.topology,
                    &index.ids,
                    &|e| index.edge_row(e),
                    &shape.search,
                    &starts,
                    targets.as_ref(),
                    shape.known_only,
                    shape.columns,
                    out_schema,
                    max_paths,
                )
            })
            .await
            .map_err(|e| DataFusionError::Execution(format!("a path search failed: {e}")))??;
            expanded.add(n);
            Ok::<_, DataFusionError>(futures::stream::iter(batches.into_iter().map(Ok)))
        };
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            schema,
            futures::stream::once(run).try_flatten(),
        )))
    }
}

/// The one column of keys `plan` gives (a search's `what`: its start or
/// target nodes), cast to the graph's node id type.
async fn search_keys(
    plan: &Arc<dyn ExecutionPlan>,
    what: &str,
    shape: &PathShape,
    context: &Arc<TaskContext>,
) -> Result<ArrayRef> {
    let batches = drain(plan, &[0], context).await?;
    let id_type = &shape.edges.id_type;
    let mut columns = Vec::with_capacity(batches.len());
    for batch in &batches {
        columns.push(cast(batch[0].as_ref(), id_type).map_err(|_| {
            DataFusionError::Plan(format!(
                "in GRAPH_TABLE: the {what}' keys ({}) cannot be read as graph '{}''s node ids ({id_type})",
                batch[0].data_type(),
                shape.edges.graph
            ))
        })?);
    }
    Ok(if columns.is_empty() {
        new_empty_array(id_type)
    } else {
        let refs: Vec<&dyn Array> = columns.iter().map(|c| c.as_ref()).collect();
        concat(&refs)?
    })
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
    /// The graph's vertex keys, if it has a vertex table.
    vertices: Option<VertexInput>,
}

/// A graph node's vertex keys: a plan of one column (see the module docs).
#[derive(Debug, Clone)]
struct VertexInput {
    plan: Arc<dyn ExecutionPlan>,
    /// The keys' plan's fingerprint, or `None` if it cannot be cached.
    fingerprint: Option<String>,
    /// The vertex table the keys are of, when they are one table's: a build
    /// then refuses a key that repeats or is NULL (#411; see
    /// [`super::vertex_keys`]).
    unique: Option<KeyNames>,
}

/// A vertex table and its key, as a refusal of its keys names them.
#[derive(Debug, Clone)]
struct KeyNames {
    table: String,
    key: String,
}

/// The node set `interner` was given is fixed: refuse it if `unique` names
/// its vertex table and a key repeats or is NULL.
fn close_keys(
    interner: &mut EdgeInterner,
    unique: Option<&KeyNames>,
    graph: &str,
    id_type: &DataType,
) -> Result<()> {
    if let Some(names) = unique {
        let rows = interner.key_rows();
        if !rows.is_unique() {
            return Err(super::vertex_keys::key_error(
                graph,
                &names.table,
                &names.key,
                rows,
                *id_type == DataType::Utf8,
            ));
        }
    }
    interner.close_nodes();
    Ok(())
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
        children.extend(self.shape.edges.vertices.as_ref().map(|v| &v.plan));
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
        let mut shape = self.shape.clone();
        shape.edges.vertices = shape.edges.vertices.map(|v| VertexInput {
            plan: children.next().expect("checked above"),
            ..v
        });
        let values = self
            .values
            .as_ref()
            .map(|_| children.next().expect("checked above"));
        Ok(Arc::new(Self::new(shape, input, weighted, values)))
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
            let (index, weights) = verified_index(&shape.edges, input, weighted, &context).await?;
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
                    values.as_ref(),
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

/// The index (and the weight columns, for a weighted node) for an algorithm
/// that reads the whole graph, with the checksums of a memory-mapped stored
/// index checked first (`crate::database::mapped`): the arrays are about to
/// be read whole anyway, and a result is never computed from damaged ones.
/// A damaged stored index is dropped and built again from its sources, with
/// a notice.
async fn verified_index(
    edges: &Edges,
    input: Arc<dyn ExecutionPlan>,
    weighted: Option<WeightedInput>,
    context: &Arc<TaskContext>,
) -> Result<(Arc<GraphIndex>, Option<Weights>)> {
    let mut damaged = None;
    loop {
        let (index, weights) = match weighted.clone() {
            None => (
                unweighted_index(edges, Arc::clone(&input), context).await?,
                None,
            ),
            Some(w) => {
                let (index, weights) =
                    weighted_index(edges, Arc::clone(&input), w, context).await?;
                (index, Some(weights))
            }
        };
        let checked = Arc::clone(&index);
        let verified = crate::run_blocking(move || checked.verify_stored())
            .await
            .map_err(|e| DataFusionError::Execution(format!("checking an index failed: {e}")))?;
        match (verified, damaged) {
            (Ok(()), _) => return Ok((index, weights)),
            (Err(why), None) => {
                if let Some(cache) = context.session_config().get_extension::<IndexCache>() {
                    cache.record_damaged_load();
                }
                Notifier::of(context.session_config()).say(NoticeLevel::On, || {
                    format!(
                        "the stored index for {} is damaged ({why}); building it again",
                        edges.graph
                    )
                });
                damaged = Some(why);
            }
            // Built from its sources: not stored, so never damaged.
            (Err(why), Some(_)) => {
                return Err(DataFusionError::Execution(format!(
                    "the index for {} is damaged ({why})",
                    edges.graph
                )))
            }
        }
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
        children.extend(self.shape.edges.vertices.as_ref().map(|v| &v.plan));
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
        let mut shape = self.shape.clone();
        shape.edges.vertices = shape.edges.vertices.map(|v| VertexInput {
            plan: children.next().expect("checked above"),
            ..v
        });
        Ok(Arc::new(Self::new(
            shape,
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

/// Leaves the edge and vertex inputs of a path search over a held index out
/// of its children, so `EXPLAIN` does not draw inputs that do not run
/// (#384, N48). It runs last, once every other rule has optimized them: they
/// still build the index should the session no longer hold it when the node
/// runs.
#[derive(Debug)]
pub struct CollapseHeldInputs;

impl PhysicalOptimizerRule for CollapseHeldInputs {
    fn optimize(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        _config: &ConfigOptions,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        plan.transform_up(|node| match node.downcast_ref::<PathSearchExec>() {
            Some(exec) if exec.held.is_some() && !exec.collapsed => Ok(Transformed::yes(Arc::new(
                exec.collapse(),
            )
                as Arc<dyn ExecutionPlan>)),
            _ => Ok(Transformed::no(node)),
        })
        .map(|t| t.data)
    }

    fn name(&self) -> &str {
        "collapse_held_inputs"
    }

    fn schema_check(&self) -> bool {
        true
    }
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
/// inputs over such a plan are sorted by value, and so are a graph's vertex
/// keys, which set the order of its nodes. Order-preserving plans
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
                    keys.extend(exec.shape.edges.vertices.as_ref().map(|_| vec![0]));
                    // A neighbour aggregation's values give one value per
                    // node whatever their order: no sort.
                    keys.extend(exec.values.as_ref().map(|_| vec![]));
                    keys
                } else if let Some(exec) = node.downcast_ref::<GraphTraversalExec>() {
                    let mut keys = vec![endpoint_keys(exec.shape.edges.endpoints)];
                    keys.extend(exec.weighted.as_ref().map(weighted_keys));
                    keys.extend(exec.shape.edges.vertices.as_ref().map(|_| vec![0]));
                    keys.extend(exec.nodes.iter().map(|_| vec![0]));
                    keys
                } else if let Some(exec) = node.downcast_ref::<PathSearchExec>() {
                    if exec.collapsed {
                        return Ok(Transformed::no(node)); // its inputs are done
                    }
                    let mut keys = vec![endpoint_keys(exec.shape.edges.endpoints)];
                    keys.extend(exec.shape.edges.vertices.as_ref().map(|_| vec![0]));
                    keys.push(vec![0]);
                    // The targets are a set: their order does not matter.
                    keys.extend(exec.targets.as_ref().map(|_| vec![]));
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
            || node.downcast_ref::<PathSearchExec>().is_some()
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

/// The index cache key for the edges `input` reads, if they can be cached:
/// with a vertex table, the keys' plan and files too, so a change to the
/// vertex table builds the index afresh.
async fn index_key(
    shape: &Edges,
    input: &Arc<dyn ExecutionPlan>,
    context: &TaskContext,
) -> Result<Option<(String, Vec<FileIdentity>)>> {
    let Some(edges) = &shape.fingerprint else {
        return Ok(None);
    };
    let mut files = file_identities(input, context).await?;
    let fingerprint = match &shape.vertices {
        None => edges.clone(),
        Some(VertexInput {
            fingerprint: None, ..
        }) => return Ok(None),
        Some(VertexInput {
            plan,
            fingerprint: Some(vertices),
            unique,
        }) => {
            files.extend(file_identities(plan, context).await?);
            files.sort();
            files.dedup();
            // An index whose build refused a key that repeats or is NULL
            // (#411) says so in its key: one stored before builds checked
            // has another key and is built afresh, and a stored index with
            // this key shows its vertex table's keys are one row each
            // (see `vertex_keys`).
            match unique {
                Some(_) => format!("{edges}{}{vertices}", super::vertex_keys::UNIQUE_KEYS),
                None => format!("{edges}\n-- vertex keys --\n{vertices}"),
            }
        }
    };
    Ok(Some((fingerprint, files)))
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
                .get_or_build_for(
                    context.session_config(),
                    key,
                    &build_info(shape, &input),
                    build.boxed(),
                )
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
            "{}\n{:?}",
            portable_in(f, context.session_config()),
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
                .get_or_build_for(
                    context.session_config(),
                    key,
                    &build_info(shape, &weighted.plan),
                    build.boxed(),
                )
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
                    if read_aligned(&index, &weighted, shape, context, &mut weights).await? {
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
    shape: &Edges,
    context: &Arc<TaskContext>,
    weights: &mut WeightReader,
) -> Result<bool> {
    let id_type = &shape.id_type;
    let mut rows = RowCheck::new(index);
    let mut batches = InOrder::new(&weighted.plan, context);
    while let Some(batch) = batches.next().await {
        let mut batch = batch?;
        let mut src = cast(batch.column(weighted.source), id_type)?;
        let mut dst = cast(batch.column(weighted.destination), id_type)?;
        // With a vertex table, the index holds only the edges between
        // keys: skip the others, as the build did.
        if shape.vertices.is_some() {
            let keep = between_nodes(index, &src, &dst)?;
            if keep.true_count() < keep.len() {
                batch = filter_record_batch(&batch, &keep)?;
                src = filter(&src, &keep)?;
                dst = filter(&dst, &keep)?;
            }
        }
        if batch.num_rows() == 0 {
            continue;
        }
        if !rows.check(&src, &dst)? {
            return Ok(false);
        }
        weights.push(&batch);
    }
    Ok(rows.complete())
}

/// Which edges `src` -> `dst` (of the index's id type) join two nodes of
/// `index`.
fn between_nodes(index: &GraphIndex, src: &ArrayRef, dst: &ArrayRef) -> Result<BooleanArray> {
    let lookup = |a: &ArrayRef| {
        index
            .ids
            .dense_from_array(a.as_ref())
            .map_err(|e| DataFusionError::Execution(e.to_string()))
    };
    let (s, d) = (lookup(src)?, lookup(dst)?);
    Ok(s.iter()
        .zip(&d)
        .map(|(s, d)| Some(s.is_some() && d.is_some()))
        .collect())
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

    /// Keep only the rows `kept` marks, one bit per row read.
    fn keep(&mut self, kept: &BooleanBuffer) {
        for values in &mut self.values {
            let mut bits = kept.iter();
            values.retain(|_| bits.next().unwrap_or(false));
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

/// Every batch of `input`, partition by partition in order (decoded in
/// parallel, see [`InOrder`]), as the columns
/// `columns` of each non-empty batch.
async fn drain(
    input: &Arc<dyn ExecutionPlan>,
    columns: &[usize],
    context: &Arc<TaskContext>,
) -> Result<Vec<Vec<ArrayRef>>> {
    let mut out = Vec::new();
    let mut batches = InOrder::new(input, context);
    while let Some(batch) = batches.next().await {
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
/// `id_type` ids if it was given no node and no edge.
fn finish_index(mut interner: EdgeInterner, id_type: &DataType) -> GraphIndex {
    let (n_dropped, kept) = (interner.n_dropped(), interner.kept_rows());
    let (ids, source, destination) = interner.finish();
    if ids.is_empty() {
        let (topology, ids) = ursa_io::topology::empty_topology(id_type);
        return GraphIndex::new(topology, ids).with_dropped(n_dropped, kept);
    }
    let topology = Topology::build(ids.len(), source, destination);
    GraphIndex::new(Arc::new(topology), Arc::new(ids)).with_dropped(n_dropped, kept)
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

/// What a build over `input` is for, as its notices say: the graph, and how
/// much it reads, as far as `input`'s statistics know (#351). An exact count
/// is the edge rows. A filter the statistics cannot settle (one no file or
/// row-group statistics prune) leaves only a guess at the edges, DataFusion's
/// default selectivity, which can be off a hundredfold either way: then the
/// notice gives the rows the scans read instead, which the statistics do
/// know. Otherwise the estimate is of edge rows.
fn build_info(shape: &Edges, input: &Arc<dyn ExecutionPlan>) -> BuildInfo {
    let rows = plan_rows(input);
    let size = match rows {
        Some((n, true)) => Some(BuildSize::Edges(n, true)),
        _ if filters(input) => scanned_rows(input).map(BuildSize::Reads),
        Some((n, false)) => Some(BuildSize::Edges(n, false)),
        None => None,
    };
    BuildInfo {
        graph: Some(shape.graph.clone()),
        size,
    }
}

/// The rows `plan`'s statistics say it gives, and whether that is exact.
fn plan_rows(plan: &Arc<dyn ExecutionPlan>) -> Option<(u64, bool)> {
    StatisticsContext::new()
        .compute(plan.as_ref(), &StatisticsArgs::new())
        .ok()
        .and_then(|s| {
            let exact = s.num_rows.is_exact().unwrap_or(false);
            s.num_rows.get_value().map(|n| (*n as u64, exact))
        })
}

/// Whether `plan` filters rows: a filter node, or a predicate pushed into a
/// file scan.
fn filters(plan: &Arc<dyn ExecutionPlan>) -> bool {
    let mut found = false;
    let _ = plan.apply(|node| {
        let scan_predicate = node
            .downcast_ref::<DataSourceExec>()
            .and_then(|exec| exec.data_source().downcast_ref::<FileScanConfig>())
            .is_some_and(|config| config.file_source().filter().is_some());
        if scan_predicate || node.downcast_ref::<FilterExec>().is_some() {
            found = true;
            return Ok(TreeNodeRecursion::Stop);
        }
        Ok(TreeNodeRecursion::Continue)
    });
    found
}

/// The rows the scans under `plan` read, if every scan's statistics say:
/// the rows of its leaves.
fn scanned_rows(plan: &Arc<dyn ExecutionPlan>) -> Option<u64> {
    let mut total = Some(0u64);
    let _ = plan.apply(|node| {
        if node.children().is_empty() {
            let leaf = plan_rows(node).map(|(n, _)| n);
            total = total.zip(leaf).map(|(a, b)| a + b);
        }
        Ok(TreeNodeRecursion::Continue)
    });
    total.filter(|n| *n > 0)
}

/// What the reader hands an interning thread.
enum Chunk {
    /// Vertex keys, before any edge.
    Nodes(ArrayRef),
    /// Every vertex key has been read: the node set is fixed.
    NodesDone,
    Edges(ArrayRef, ArrayRef),
    /// Every row has been read: build the index.
    End,
}

/// Hand one chunk to `interner`, which takes the keys and then the edges in
/// turn (the browser's build, see [`build_streamed`]); the index once every
/// row has been read.
#[cfg(target_arch = "wasm32")]
fn take_chunk(
    interner: &mut EdgeInterner,
    chunk: Chunk,
    graph: &str,
    id_type: &DataType,
    unique: Option<&KeyNames>,
) -> Result<Option<GraphIndex>> {
    match chunk {
        Chunk::Nodes(keys) => interner
            .push_nodes(keys.as_ref())
            .map_err(|e| build_error(graph, e))?,
        Chunk::NodesDone => close_keys(interner, unique, graph, id_type)?,
        Chunk::Edges(src, dst) => interner
            .push(src.as_ref(), dst.as_ref())
            .map_err(|e| build_error(graph, e))?,
        Chunk::End => {
            let interner = std::mem::replace(interner, EdgeInterner::with_capacity(0));
            return Ok(Some(finish_index(interner, id_type)));
        }
    }
    Ok(None)
}

/// Send the vertex keys `batches` gives, cast to `id_type`, to `tx`, then
/// [`Chunk::NodesDone`]. Stops without error if the receiver is gone (it
/// stopped on an error of its own).
async fn send_keys(
    mut batches: InOrder,
    id_type: &DataType,
    tx: &tokio::sync::mpsc::Sender<Chunk>,
) -> Result<()> {
    while let Some(batch) = batches.next().await {
        let batch = batch?;
        if batch.num_rows() == 0 {
            continue;
        }
        let keys = cast(batch.column(0), id_type)?;
        if tx.send(Chunk::Nodes(keys)).await.is_err() {
            return Ok(());
        }
    }
    let _ = tx.send(Chunk::NodesDone).await;
    Ok(())
}

/// Send the endpoints of the edge rows `batches` gives to `tx`, gathered
/// into chunks of [`INTERN_ROWS`] rows, then [`Chunk::End`]; and read each
/// batch's weights into `weights`. Stops without error if the receiver is
/// gone (it stopped on an error of its own).
async fn send_edges(
    mut batches: InOrder,
    shape: &Edges,
    (source, destination): (usize, usize),
    weights: &mut WeightReader,
    tx: &tokio::sync::mpsc::Sender<Chunk>,
) -> Result<()> {
    let mut gather = Gather::default();
    while let Some(batch) = batches.next().await {
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
                return Ok(());
            }
        }
    }
    if let Some((src, dst)) = gather.take()? {
        if tx.send(Chunk::Edges(src, dst)).await.is_err() {
            return Ok(());
        }
    }
    let _ = tx.send(Chunk::End).await;
    Ok(())
}

/// Intern the vertex keys `rx` brings, on a thread of their own, and once the
/// last is in, hand the interner, its node set fixed, to the edges' thread
/// (`done`; see [`build_streamed`]).
#[cfg(not(target_arch = "wasm32"))]
fn intern_keys(
    mut rx: tokio::sync::mpsc::Receiver<Chunk>,
    done: tokio::sync::oneshot::Sender<EdgeInterner>,
    hint: usize,
    graph: &str,
    id_type: &DataType,
    unique: Option<&KeyNames>,
) -> Result<()> {
    let mut interner = EdgeInterner::with_capacity(hint);
    while let Some(chunk) = rx.blocking_recv() {
        match chunk {
            Chunk::Nodes(keys) => interner
                .push_nodes(keys.as_ref())
                .map_err(|e| build_error(graph, e))?,
            Chunk::NodesDone => {
                close_keys(&mut interner, unique, graph, id_type)?;
                // The edges' thread is gone only if it stopped on an error.
                let _ = done.send(interner);
                return Ok(());
            }
            Chunk::Edges(..) | Chunk::End => {
                return Err(DataFusionError::Internal(format!(
                    "building graph '{graph}': an edge chunk reached the vertex keys"
                )))
            }
        }
    }
    // The key read stopped before the end: its error is the one reported.
    Ok(())
}

/// The edges' side of a build with a vertex table (see [`build_streamed`]):
/// edges interned provisionally until the node set is fixed, then into it.
#[cfg(not(target_arch = "wasm32"))]
struct EdgeSide {
    /// The interner the edges go into once the node set is fixed; from the
    /// start without a vertex table.
    interner: Option<EdgeInterner>,
    /// The edges interned before then ([`EdgeInterner::provisional`]).
    provisional: Option<EdgeInterner>,
    /// Where the keys' interner comes from, until it has.
    nodes: Option<tokio::sync::oneshot::Receiver<EdgeInterner>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl EdgeSide {
    fn new(nodes: Option<tokio::sync::oneshot::Receiver<EdgeInterner>>, hint: usize) -> Self {
        match nodes {
            None => Self {
                interner: Some(EdgeInterner::with_capacity(hint)),
                provisional: None,
                nodes: None,
            },
            Some(nodes) => Self {
                interner: None,
                provisional: Some(EdgeInterner::provisional(hint)),
                nodes: Some(nodes),
            },
        }
    }

    /// Take the node set in if it is fixed (waiting for it, with `wait`),
    /// renumbering the edges interned so far into it. `false` if it never
    /// will be: the keys' read or interning stopped on an error.
    fn take_nodes(&mut self, wait: bool) -> std::result::Result<bool, IdError> {
        use tokio::sync::oneshot::error::TryRecvError;
        let Some(mut rx) = self.nodes.take() else {
            return Ok(true);
        };
        let mut nodes = match wait {
            true => match rx.blocking_recv() {
                Ok(nodes) => nodes,
                Err(_) => return Ok(false),
            },
            false => match rx.try_recv() {
                Ok(nodes) => nodes,
                Err(TryRecvError::Empty) => {
                    self.nodes = Some(rx);
                    return Ok(true);
                }
                Err(TryRecvError::Closed) => return Ok(false),
            },
        };
        if let Some(provisional) = self.provisional.take() {
            nodes.adopt(provisional)?;
        }
        self.interner = Some(nodes);
        Ok(true)
    }

    fn push(&mut self, src: &dyn Array, dst: &dyn Array) -> std::result::Result<(), IdError> {
        match (&mut self.interner, &mut self.provisional) {
            (Some(interner), _) => interner.push(src, dst),
            (None, Some(provisional)) => provisional.push(src, dst),
            (None, None) => unreachable!("edges interned before the node set, or into it"),
        }
    }
}

/// Intern the edges `rx` brings, on a thread of their own (see
/// [`build_streamed`]); the index once every row has been read, `None` if a
/// read stopped first (its error is the one reported).
#[cfg(not(target_arch = "wasm32"))]
fn intern_edges(
    mut rx: tokio::sync::mpsc::Receiver<Chunk>,
    nodes: Option<tokio::sync::oneshot::Receiver<EdgeInterner>>,
    hint: usize,
    graph: &str,
    id_type: &DataType,
) -> Result<Option<GraphIndex>> {
    let error = |e| build_error(graph, e);
    let mut side = EdgeSide::new(nodes, hint);
    while let Some(chunk) = rx.blocking_recv() {
        if !side.take_nodes(false).map_err(error)? {
            return Ok(None);
        }
        match chunk {
            Chunk::Edges(src, dst) => side.push(src.as_ref(), dst.as_ref()).map_err(error)?,
            Chunk::End => {
                if !side.take_nodes(true).map_err(error)? {
                    return Ok(None);
                }
                let interner = side.interner.take().expect("the node set is taken in");
                return Ok(Some(finish_index(interner, id_type)));
            }
            Chunk::Nodes(_) | Chunk::NodesDone => {
                return Err(DataFusionError::Internal(format!(
                    "building graph '{graph}': a vertex key chunk reached the edges"
                )))
            }
        }
    }
    Ok(None)
}

/// Build the index of the edge rows `input` produces, interning each chunk
/// as it is read rather than collecting every endpoint array first, so the
/// Arrow input does not accumulate: the build holds the dense endpoints, a
/// chunk or two in flight, and what the partitions decoded ahead of their
/// turn hold, within the session's read-ahead ([`read_ahead`]). The
/// partitions decode in parallel and are interned in partition order
/// ([`InOrder`]), and reading (here) and interning (on a blocking thread)
/// overlap. The rows are interned in the order [`drain`] would collect them,
/// so the index is the one `ursa_io::build_topology_batches` gives over the
/// collected rows. A weighted node's weights are read from the same rows
/// (`weights`), and with a vertex table kept only for the edges kept (see
/// the module docs).
///
/// A vertex table's keys number the nodes, in the order they are read, and
/// an edge between two keys is kept. The keys used to be read and interned
/// first, then the edges; now (#385) both are read and interned at once, on
/// two threads: the keys into their interner, the edges into a provisional
/// one ([`EdgeInterner::provisional`]) until the last key is in. Then the
/// edges interned so far are renumbered into the keys' numbering
/// ([`EdgeInterner::adopt`]), and the rest go straight into it. The index is
/// exactly the one interning the keys first gives, wherever that switch
/// falls. The two reads share the read-ahead: a quarter for the keys, the
/// rest for the edges. In the browser (wasm32), with one thread, the keys
/// are still read first.
async fn build_streamed(
    shape: &Edges,
    input: &Arc<dyn ExecutionPlan>,
    endpoints: (usize, usize),
    context: &Arc<TaskContext>,
    weights: &mut WeightReader,
) -> Result<GraphIndex> {
    let partitions = input.output_partitioning().partition_count();
    let read_ahead = read_ahead(context.session_config(), partitions);
    build_streamed_with(shape, input, endpoints, context, weights, read_ahead).await
}

/// [`build_streamed`], reading the partitions in parallel within
/// `read_ahead` bytes (see [`InOrder`]), or one after another with `None`:
/// the index is the same either way.
async fn build_streamed_with(
    shape: &Edges,
    input: &Arc<dyn ExecutionPlan>,
    endpoints: (usize, usize),
    context: &Arc<TaskContext>,
    weights: &mut WeightReader,
    read_ahead: Option<usize>,
) -> Result<GraphIndex> {
    let hint = row_hint(input);
    let (key_budget, edge_budget) = match (&shape.vertices, read_ahead) {
        (Some(_), Some(bytes)) => (Some(bytes / 4), Some(bytes - bytes / 4)),
        _ => (read_ahead, read_ahead),
    };
    let edges = InOrder::with_budget(input, context, edge_budget);

    // Native: the keys are read on a task of their own and interned on a
    // thread of their own, while the edges are read here and interned on a
    // third (see the docs above). The threads are spawned before the reads
    // start (`run_blocking` would start one only when awaited).
    #[cfg(not(target_arch = "wasm32"))]
    let (read, built) = {
        use datafusion::common::runtime::SpawnedTask;
        let failed = |e: tokio::task::JoinError| {
            DataFusionError::Execution(format!("building graph '{}' failed: {e}", shape.graph))
        };
        let (tx, rx) = tokio::sync::mpsc::channel::<Chunk>(2);
        let mut keys = None;
        let mut nodes = None;
        if let Some(vertices) = &shape.vertices {
            let (keys_tx, keys_rx) = tokio::sync::mpsc::channel::<Chunk>(2);
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            nodes = Some(done_rx);
            let (graph, id_type) = (shape.graph.clone(), shape.id_type.clone());
            let unique = vertices.unique.clone();
            let interning = tokio::task::spawn_blocking(move || {
                intern_keys(keys_rx, done_tx, hint, &graph, &id_type, unique.as_ref())
            });
            let batches = InOrder::with_budget(&vertices.plan, context, key_budget);
            let id_type = shape.id_type.clone();
            let reading =
                SpawnedTask::spawn(async move { send_keys(batches, &id_type, &keys_tx).await });
            keys = Some((reading, interning));
        }
        let (graph, id_type) = (shape.graph.clone(), shape.id_type.clone());
        let interning =
            tokio::task::spawn_blocking(move || intern_edges(rx, nodes, hint, &graph, &id_type));
        // The reader owns the sender, so the interner sees the channel close
        // when the reader stops, on an error as at the end.
        let edges_read = send_edges(edges, shape, endpoints, weights, &tx).await;
        drop(tx);
        // A key's error comes first, as when the keys were read first.
        let keys_done = match keys {
            Some((reading, interning)) => {
                let read = reading.join().await.map_err(failed)?;
                let interned = interning.await.map_err(failed)?;
                read.and(interned)
            }
            None => Ok(()),
        };
        let built = interning.await.map_err(failed)?;
        (keys_done.and(edges_read), built)
    };

    // The browser (wasm32) has one thread: the interner is a future polled
    // alongside the reader, taking each chunk as it is sent, the keys first.
    // The channel's bound still keeps at most two chunks in flight, and the
    // chunks arrive in the same order, so the index is the same (#278).
    #[cfg(target_arch = "wasm32")]
    let (read, built) = {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Chunk>(2);
        let (graph, id_type) = (shape.graph.clone(), shape.id_type.clone());
        let unique = shape.vertices.as_ref().and_then(|v| v.unique.clone());
        let interning = async move {
            let mut interner = EdgeInterner::with_capacity(hint);
            while let Some(chunk) = rx.recv().await {
                if let Some(index) =
                    take_chunk(&mut interner, chunk, &graph, &id_type, unique.as_ref())?
                {
                    return Ok(Some(index));
                }
            }
            Ok::<_, DataFusionError>(None)
        };
        let read = async {
            let tx = tx;
            if let Some(vertices) = &shape.vertices {
                let batches = InOrder::with_budget(&vertices.plan, context, key_budget);
                send_keys(batches, &shape.id_type, &tx).await?;
            }
            send_edges(edges, shape, endpoints, weights, &tx).await
        };
        futures::join!(read, interning)
    };
    read?;
    let index = built?.ok_or_else(|| {
        DataFusionError::Internal(format!(
            "building graph '{}' stopped before the last edge",
            shape.graph
        ))
    })?;
    if let Some(kept) = index.kept_rows() {
        weights.keep(kept);
    }
    Ok(index)
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
    use crate::graph::in_order::MAX_READ_AHEAD;
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

    /// The bytes a database stores `index` as.
    fn stored_bytes(index: &GraphIndex) -> Vec<u8> {
        let key = crate::database::IndexKey::new("edges".into(), Vec::new());
        let encoded =
            crate::database::index_file::Encoded::new(&key, &index.topology, &index.ids).unwrap();
        let mut bytes = Vec::new();
        encoded.write_to(&mut bytes).unwrap();
        bytes
    }

    /// A session over a Parquet file of many row groups, split into several
    /// partitions, and the plan of its edges behind a filter no statistics
    /// prune.
    async fn filtered_edges() -> (tempfile::TempDir, Arc<dyn ExecutionPlan>, Arc<TaskContext>) {
        use datafusion::prelude::SessionConfig;
        let dir = tempfile::tempdir().unwrap();
        let mut config = SessionConfig::new().with_target_partitions(8);
        config.options_mut().optimizer.repartition_file_min_size = 1;
        let ctx = crate::new_session_with_config(config);
        let file = dir.path().join("edges.parquet");
        let sql = [
            "SET datafusion.execution.parquet.max_row_group_size = 4096".to_string(),
            // Ids scattered over the rows, so first-seen order numbers them
            // differently from sorted order.
            format!(
                "COPY (SELECT (value * 7919) % 50021 AS s, (value * 104729 + 13) % 50021 AS d \
                 FROM generate_series(0, 299999)) TO '{}'",
                file.display()
            ),
        ];
        for sql in sql {
            ctx.sql(&sql).await.unwrap().collect().await.unwrap();
        }
        let plan = ctx
            .sql(&format!(
                "SELECT s, d FROM '{}' WHERE s % 7 <> 3",
                file.display()
            ))
            .await
            .unwrap()
            .create_physical_plan()
            .await
            .unwrap();
        (dir, plan, ctx.task_ctx())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_index_built_from_partitions_read_in_parallel_is_the_serial_one() {
        let (_dir, plan, context) = filtered_edges().await;
        let partitions = plan.output_partitioning().partition_count();
        assert!(partitions > 1, "{partitions} partition(s)");
        let shape = Edges {
            graph: "g".into(),
            endpoints: (0, 1),
            id_type: DataType::Int64,
            fingerprint: None,
            vertices: None,
        };
        let build = |read_ahead| {
            let (shape, plan, context) = (shape.clone(), Arc::clone(&plan), Arc::clone(&context));
            async move {
                let mut no_weights = WeightReader::new(&[], 0);
                build_streamed_with(&shape, &plan, (0, 1), &context, &mut no_weights, read_ahead)
                    .await
                    .unwrap()
            }
        };
        let serial = build(None).await;
        assert!(serial.topology.n_edges() > 200_000);
        // The serial build is the one over the rows collected in order.
        let rows = drain(&plan, &[0, 1], &context).await.unwrap();
        let rows: Vec<(&dyn Array, &dyn Array)> = rows
            .iter()
            .map(|c| (c[0].as_ref(), c[1].as_ref()))
            .collect();
        let (topology, ids) = ursa_io::build_topology_batches(&rows).unwrap();
        let collected = GraphIndex::new(topology, ids);
        let expected = stored_bytes(&serial);
        assert_eq!(stored_bytes(&collected), expected);
        // In parallel, with room to read ahead and with none: the same node
        // numbering, the same CSR, the same stored bytes.
        for read_ahead in [Some(MAX_READ_AHEAD), Some(1)] {
            let parallel = build(read_ahead).await;
            assert_eq!(
                parallel.ids.user_id_array().as_ref(),
                serial.ids.user_id_array().as_ref(),
                "node numbering, read ahead {read_ahead:?}"
            );
            assert_eq!(
                stored_bytes(&parallel),
                expected,
                "read ahead {read_ahead:?}"
            );
        }
    }

    /// The plan of a vertex table of `keys` keys written as `file` (by its
    /// extension: CSV, gzipped CSV or Parquet of many row groups), read in
    /// several partitions where the format splits.
    async fn vertex_keys(dir: &std::path::Path, file: &str) -> Arc<dyn ExecutionPlan> {
        use datafusion::prelude::SessionConfig;
        let mut config = SessionConfig::new().with_target_partitions(8);
        config.options_mut().optimizer.repartition_file_min_size = 1;
        let ctx = crate::new_session_with_config(config);
        let path = dir.join(file);
        // Keys scattered over a range wider than the edges' ids, so some keys
        // are on no edge and some edges have an endpoint that is no key.
        let keys = "SELECT (value * 31337) % 60013 AS id FROM generate_series(0, 45000)";
        let copy = match file {
            f if f.ends_with(".csv.gz") => format!(
                "COPY ({keys}) TO '{}' STORED AS CSV OPTIONS ('format.compression' 'gzip')",
                path.display()
            ),
            f if f.ends_with(".csv") => {
                format!("COPY ({keys}) TO '{}' STORED AS CSV", path.display())
            }
            _ => format!("COPY ({keys}) TO '{}'", path.display()),
        };
        for sql in [
            "SET datafusion.execution.parquet.max_row_group_size = 2048".to_string(),
            copy,
        ] {
            ctx.sql(&sql).await.unwrap().collect().await.unwrap();
        }
        ctx.sql(&format!("SELECT id FROM '{}'", path.display()))
            .await
            .unwrap()
            .create_physical_plan()
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn keys_and_edges_interned_at_once_build_the_index_keys_first_did() {
        let (dir, plan, context) = filtered_edges().await;
        for file in ["keys.csv", "keys.csv.gz", "keys.parquet"] {
            let vertices = vertex_keys(dir.path(), file).await;
            let shape = Edges {
                graph: "g".into(),
                endpoints: (0, 1),
                id_type: DataType::Int64,
                fingerprint: None,
                vertices: Some(VertexInput {
                    plan: vertices,
                    fingerprint: None,
                    unique: None,
                }),
            };
            let build = |read_ahead| {
                let (shape, plan, context) =
                    (shape.clone(), Arc::clone(&plan), Arc::clone(&context));
                async move {
                    let mut no_weights = WeightReader::new(&[], 0);
                    build_streamed_with(
                        &shape,
                        &plan,
                        (0, 1),
                        &context,
                        &mut no_weights,
                        read_ahead,
                    )
                    .await
                    .unwrap()
                }
            };
            // The index as the build made it before #385: every key interned,
            // then every edge, in the order a serial read gives them.
            let mut interner = EdgeInterner::with_capacity(0);
            for keys in drain(&shape.vertices.as_ref().unwrap().plan, &[0], &context)
                .await
                .unwrap()
            {
                let keys = cast(&keys[0], &DataType::Int64).unwrap();
                interner.push_nodes(keys.as_ref()).unwrap();
            }
            interner.close_nodes();
            for edges in drain(&plan, &[0, 1], &context).await.unwrap() {
                interner.push(edges[0].as_ref(), edges[1].as_ref()).unwrap();
            }
            let before = finish_index(interner, &DataType::Int64);
            assert!(before.ids.len() > 45_000, "{file}: {}", before.ids.len());
            assert!(before.n_dropped() > 0, "{file}");
            assert!(before.topology.n_edges() > 100_000, "{file}");
            let expected = stored_bytes(&before);
            // The keys and the edges read and interned at once: serially, in
            // parallel with room to read ahead, with little, and with none.
            // Wherever the edges switch from provisional interning to the
            // keys' numbering, the same node numbering, the same edges kept
            // and the same stored bytes.
            for _ in 0..3 {
                for read_ahead in [None, Some(MAX_READ_AHEAD), Some(1 << 20), Some(1), Some(0)] {
                    let built = build(read_ahead).await;
                    assert_eq!(
                        built.ids.user_id_array().as_ref(),
                        before.ids.user_id_array().as_ref(),
                        "{file}: node numbering, read ahead {read_ahead:?}"
                    );
                    assert_eq!(
                        (built.n_dropped(), built.kept_rows()),
                        (before.n_dropped(), before.kept_rows()),
                        "{file}: edges kept, read ahead {read_ahead:?}"
                    );
                    assert_eq!(
                        stored_bytes(&built),
                        expected,
                        "{file}: read ahead {read_ahead:?}"
                    );
                }
            }
        }
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
