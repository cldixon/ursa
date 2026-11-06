//! The graph function plan node.
//!
//! [`GraphAlgorithmNode`] takes its edges as a child plan: the graph's edge
//! table, projected to the source and destination columns. Anything that
//! produces edge rows can sit under it (a scan, a view's filter, a join), and
//! DataFusion's optimizer works on that child as on any plan: projections and
//! filters reach the scan.
//!
//! A node whose calls take a `weight =>` has a second child: the same edge
//! table projected to the endpoints and one column per distinct weight
//! expression ([`WEIGHTED_SOURCE`], [`WEIGHTED_DESTINATION`],
//! [`weight_column`]). The first child stays exactly the plan an unweighted
//! call has, so its fingerprint, which keys the index cache, does not depend on
//! the weights: a weighted and an unweighted query over one graph share one
//! index. When the node runs with weights it reads only the second child (see
//! [`super::exec`]).
//!
//! A neighbour aggregation (`neighbors_agg`) reads one value per node. Its
//! node has one more child, last: the values, a plan of two columns, node ids
//! then values (see [`GraphAlgorithmNode::values`]). Like a traversal's node
//! ids, it runs when the node runs.
//!
//! The node's output is `id` (for per-node functions) plus one column per
//! [`Output`]: one per algorithm, two per layout (`x` and `y`), and a
//! statistic's columns, in one row and with no `id`.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::common::{DFSchema, DFSchemaRef};
use datafusion::error::Result;
use datafusion::logical_expr::{Expr, ExprSchemable, LogicalPlan, UserDefinedLogicalNodeCore};
use ursa_core::catalog::Call;

use super::functions::{display_call, plan};
use super::traversal::Traversal;

/// The source column of a node's weighted edge plan.
pub const WEIGHTED_SOURCE: &str = "__ursa_source";
/// The destination column of a node's weighted edge plan.
pub const WEIGHTED_DESTINATION: &str = "__ursa_destination";

/// The column of a node's weighted edge plan that holds weight expression `i`.
pub fn weight_column(i: usize) -> String {
    format!("__ursa_weight_{i}")
}

/// One output column of a graph node: output `field` of `call`, named `name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Output {
    /// The column's name.
    pub name: String,
    pub call: Call,
    /// Which of the call's outputs (a layout's `x` is 0 and `y` is 1).
    pub field: usize,
    /// The weight expression the call reads, as an index into the node's
    /// weights; `None` is unweighted.
    pub weight: Option<usize>,
}

/// Run graph functions over the edges its input produces.
#[derive(Debug, Clone)]
pub struct GraphAlgorithmNode {
    /// The graph's name, for plans and messages. The index is keyed by the
    /// edges, not by this.
    pub graph: String,
    /// The edge rows, projected to the endpoints.
    pub input: Arc<LogicalPlan>,
    /// The edge rows projected to the endpoints and the weights, when an
    /// output reads a weight (see the module docs).
    pub weighted: Option<Arc<LogicalPlan>>,
    /// The node values a neighbour aggregation reads: a plan of two columns,
    /// node ids then values.
    pub values: Option<Arc<LogicalPlan>>,
    /// The source and destination of each edge: column references into
    /// `input`.
    pub source: Expr,
    pub destination: Expr,
    /// The distinct weight expressions, as written, for `EXPLAIN`.
    pub weights: Vec<String>,
    /// What the node computes, one value column each.
    pub outputs: Vec<Output>,
    /// One row per node with an `id` column, or one row for the whole graph.
    pub per_node: bool,
    /// The node id type: the endpoints' type, with every integer type read as
    /// `Int64` and every string type as `Utf8`, as the Python library reads
    /// them.
    pub id_type: DataType,
    schema: DFSchemaRef,
}

impl GraphAlgorithmNode {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        graph: String,
        input: Arc<LogicalPlan>,
        weighted: Option<Arc<LogicalPlan>>,
        values: Option<Arc<LogicalPlan>>,
        source: Expr,
        destination: Expr,
        weights: Vec<String>,
        outputs: Vec<Output>,
        per_node: bool,
    ) -> Result<Self> {
        let id_type = endpoint_id_type(&graph, &input, &source, &destination)?;
        if let Some(values) = &values {
            let columns = values.schema().fields().len();
            if columns != 2 {
                return Err(plan(format!(
                    "the node values of graph '{graph}' must be two columns, node ids then \
                     values; the query returns {columns}"
                )));
            }
        }
        let mut fields = Vec::new();
        if per_node {
            fields.push(Field::new("id", id_type.clone(), false));
        }
        for o in &outputs {
            let spec = o.call.spec().outputs[o.field];
            // A neighbour aggregation's type depends on its values' type.
            let ty = match (&o.call, &values) {
                (Call::NeighborAgg { agg, .. }, Some(values)) => {
                    super::kernels::neighbors_agg_type(*agg, values.schema().field(1).data_type())
                }
                _ => spec.ty.data_type(),
            };
            fields.push(Field::new(&o.name, ty, spec.nullable));
        }
        let schema = Arc::new(DFSchema::try_from(Schema::new(fields))?);
        Ok(Self {
            graph,
            input,
            weighted,
            values,
            source,
            destination,
            weights,
            outputs,
            per_node,
            id_type,
            schema,
        })
    }

    /// The positions of the source and destination columns in the input.
    pub fn endpoint_indices(&self) -> Result<(usize, usize)> {
        endpoint_positions(&self.graph, &self.input, &self.source, &self.destination)
    }

    /// The outputs as SQL would spell them, for `EXPLAIN`: `name: call`, or
    /// just the call when the column is named after its function.
    pub(crate) fn display_outputs(&self) -> String {
        let outputs: Vec<String> = self
            .outputs
            .iter()
            .map(|o| {
                let weight = o.weight.map(|i| self.weights[i].as_str());
                let call = display_call(&o.call, weight);
                let spec = o.call.spec();
                if spec.outputs.len() == 1 && o.name == spec.outputs[0].name {
                    call
                } else {
                    format!("{}: {call}", o.name)
                }
            })
            .collect();
        outputs.join(", ")
    }
}

/// The node id type of a graph whose edges `input` holds, with endpoint
/// columns `source` and `destination`: both integer or both string.
pub(crate) fn endpoint_id_type(
    graph: &str,
    input: &LogicalPlan,
    source: &Expr,
    destination: &Expr,
) -> Result<DataType> {
    let source_type = source.get_type(input.schema())?;
    let destination_type = destination.get_type(input.schema())?;
    match (id_type(&source_type), id_type(&destination_type)) {
        (Some(s), Some(d)) if s == d => Ok(s),
        (Some(_), Some(_)) => Err(plan(format!(
            "graph '{graph}': the source and destination columns must have one id type \
             (both integer or both string); got {source_type} and {destination_type}"
        ))),
        (None, _) | (_, None) => {
            let bad = if id_type(&source_type).is_none() {
                source_type
            } else {
                destination_type
            };
            Err(plan(format!(
                "graph '{graph}': node ids must be an integer or string column; got {bad}"
            )))
        }
    }
}

/// The positions of the endpoint columns `source` and `destination` in
/// `input`.
fn endpoint_positions(
    graph: &str,
    input: &LogicalPlan,
    source: &Expr,
    destination: &Expr,
) -> Result<(usize, usize)> {
    let index = |e: &Expr| -> Result<usize> {
        match e {
            Expr::Column(c) => input.schema().index_of_column(c),
            other => Err(plan(format!(
                "graph '{graph}': an edge endpoint must be a column; got {other}"
            ))),
        }
    };
    Ok((index(source)?, index(destination)?))
}

/// The node id type a column of `t` gives: integers as `Int64`, strings as
/// `Utf8`, or `None` if `t` cannot hold node ids. The rule every source shares
/// (`ursa_io::ids`), so a graph's ids are the Python library's.
pub(crate) fn id_type(t: &DataType) -> Option<DataType> {
    ursa_io::ids::canonical_id_type(t)
}

/// What identifies a node besides its plans and expressions.
type Key<'a> = (&'a String, &'a Vec<Output>, &'a Vec<String>, bool);

impl GraphAlgorithmNode {
    fn key(&self) -> Key<'_> {
        (&self.graph, &self.outputs, &self.weights, self.per_node)
    }
}

// The schema is derived from the other fields, so equality, hashing and order
// leave it out.
impl PartialEq for GraphAlgorithmNode {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
            && self.source == other.source
            && self.destination == other.destination
            && self.input == other.input
            && self.weighted == other.weighted
            && self.values == other.values
    }
}

impl Eq for GraphAlgorithmNode {}

impl Hash for GraphAlgorithmNode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state);
        self.source.hash(state);
        self.destination.hash(state);
        self.input.hash(state);
        self.weighted.hash(state);
        self.values.hash(state);
    }
}

impl PartialOrd for GraphAlgorithmNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match self.key().cmp(&other.key()) {
            Ordering::Equal => {}
            unequal => return Some(unequal),
        }
        match self.source.partial_cmp(&other.source)? {
            Ordering::Equal => {}
            unequal => return Some(unequal),
        }
        match self.destination.partial_cmp(&other.destination)? {
            Ordering::Equal => {}
            unequal => return Some(unequal),
        }
        match self.input.partial_cmp(&other.input)? {
            Ordering::Equal => {}
            unequal => return Some(unequal),
        }
        match self.weighted.partial_cmp(&other.weighted)? {
            Ordering::Equal => {}
            unequal => return Some(unequal),
        }
        self.values.partial_cmp(&other.values)
    }
}

impl UserDefinedLogicalNodeCore for GraphAlgorithmNode {
    fn name(&self) -> &str {
        "GraphAlgorithm"
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        let mut inputs = vec![self.input.as_ref()];
        inputs.extend(self.weighted.as_deref());
        inputs.extend(self.values.as_deref());
        inputs
    }

    fn schema(&self) -> &DFSchemaRef {
        &self.schema
    }

    /// The endpoint columns, so DataFusion keeps them in the input.
    fn expressions(&self) -> Vec<Expr> {
        vec![self.source.clone(), self.destination.clone()]
    }

    fn fmt_for_explain(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "GraphAlgorithm: graph={}, edges=({}, {}), outputs=[{}]",
            self.graph,
            self.source,
            self.destination,
            self.display_outputs()
        )
    }

    fn with_exprs_and_inputs(&self, exprs: Vec<Expr>, inputs: Vec<LogicalPlan>) -> Result<Self> {
        let [source, destination]: [Expr; 2] = exprs
            .try_into()
            .map_err(|_| plan("GraphAlgorithm takes two expressions".into()))?;
        let expected =
            1 + usize::from(self.weighted.is_some()) + usize::from(self.values.is_some());
        if inputs.len() != expected {
            return Err(plan(format!(
                "GraphAlgorithm takes {expected} inputs; got {}",
                inputs.len()
            )));
        }
        let mut inputs = inputs.into_iter().map(Arc::new);
        let input = inputs.next().expect("checked above");
        let weighted = self.weighted.as_ref().and_then(|_| inputs.next());
        let values = self.values.as_ref().and_then(|_| inputs.next());
        Self::try_new(
            self.graph.clone(),
            input,
            weighted,
            values,
            source,
            destination,
            self.weights.clone(),
            self.outputs.clone(),
            self.per_node,
        )
    }

    /// Every output needs both endpoints and nothing else of the first input,
    /// every column of the weighted input, and both columns of the values.
    fn necessary_children_exprs(&self, _output_columns: &[usize]) -> Option<Vec<Vec<usize>>> {
        let (source, destination) = self.endpoint_indices().ok()?;
        let mut needed = vec![source, destination];
        needed.sort_unstable();
        needed.dedup();
        let mut children = vec![needed];
        if let Some(weighted) = &self.weighted {
            children.push((0..weighted.schema().fields().len()).collect());
        }
        if self.values.is_some() {
            children.push(vec![0, 1]);
        }
        Some(children)
    }

    /// Filters on the output are about nodes, not edges: none moves below.
    fn prevent_predicate_push_down_columns(&self) -> std::collections::HashSet<String> {
        self.schema
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    }
}

/// Run a traversal (`hop`, `shortest_path`, `random_walk`) over the edges its
/// first input produces.
///
/// Its inputs, in order:
///
/// 1. the edge rows projected to the endpoints, exactly as a
///    [`GraphAlgorithmNode`]'s first input, so the two share one index;
/// 2. for a weighted `shortest_path`, the weighted edge plan, as a
///    [`GraphAlgorithmNode`]'s second input (with one weight column);
/// 3. one plan per node-id parameter (`seeds`; `source` and `target`;
///    `start`), each of one column: a `VALUES` list for a literal, or the
///    planned subquery. They run when the node runs, never while the query is
///    planned, and their values are cast to the graph's id type then.
///
/// The output is the traversal's own rows (see [`super::traversal`]).
#[derive(Debug, Clone)]
pub struct GraphTraversalNode {
    /// The graph's name, for plans and messages.
    pub graph: String,
    pub traversal: Traversal,
    /// The edge rows, projected to the endpoints.
    pub input: Arc<LogicalPlan>,
    /// The edge rows projected to the endpoints and the weight, for a
    /// weighted shortest path.
    pub weighted: Option<Arc<LogicalPlan>>,
    /// The weight expression, as written, for `EXPLAIN`.
    pub weight: Option<String>,
    /// One plan per node-id parameter, in the spec's order.
    pub nodes: Vec<Arc<LogicalPlan>>,
    /// Each node-id argument as written, for `EXPLAIN`.
    pub written: Vec<String>,
    /// The source and destination of each edge: column references into
    /// `input`.
    pub source: Expr,
    pub destination: Expr,
    /// The node id type (see [`GraphAlgorithmNode::id_type`]).
    pub id_type: DataType,
    schema: DFSchemaRef,
}

impl GraphTraversalNode {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        graph: String,
        traversal: Traversal,
        input: Arc<LogicalPlan>,
        weighted: Option<Arc<LogicalPlan>>,
        weight: Option<String>,
        nodes: Vec<Arc<LogicalPlan>>,
        written: Vec<String>,
        source: Expr,
        destination: Expr,
    ) -> Result<Self> {
        let id_type = endpoint_id_type(&graph, &input, &source, &destination)?;
        let spec = traversal.spec();
        let expected = spec.node_params().count();
        if nodes.len() != expected || written.len() != expected {
            return Err(plan(format!(
                "{}() takes {expected} node-id inputs; got {}",
                spec.name,
                nodes.len()
            )));
        }
        if weighted.is_some() != weight.is_some() {
            return Err(plan(format!(
                "{}(): a weighted plan needs its weight expression",
                spec.name
            )));
        }
        for (p, n) in spec.node_params().zip(&nodes) {
            let columns = n.schema().fields().len();
            if columns != 1 {
                return Err(plan(format!(
                    "{}({} =>) must be one column of node ids; the query returns {columns} \
                     columns",
                    spec.name, p.name
                )));
            }
        }
        let schema = Arc::new(DFSchema::try_from(spec.schema(&id_type))?);
        Ok(Self {
            graph,
            traversal,
            input,
            weighted,
            weight,
            nodes,
            written,
            source,
            destination,
            id_type,
            schema,
        })
    }

    /// The positions of the source and destination columns in the input.
    pub fn endpoint_indices(&self) -> Result<(usize, usize)> {
        endpoint_positions(&self.graph, &self.input, &self.source, &self.destination)
    }

    /// The call as SQL would spell it, for `EXPLAIN`.
    pub(crate) fn display_call(&self) -> String {
        self.traversal
            .display(&self.written, self.weight.as_deref())
    }
}

/// What identifies a traversal node besides its plans and expressions.
type TraversalKey<'a> = (
    &'a String,
    &'a Traversal,
    &'a Option<String>,
    &'a Vec<String>,
);

impl GraphTraversalNode {
    fn key(&self) -> TraversalKey<'_> {
        (&self.graph, &self.traversal, &self.weight, &self.written)
    }
}

// The schema and id type are derived from the other fields, so equality,
// hashing and order leave them out.
impl PartialEq for GraphTraversalNode {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
            && self.source == other.source
            && self.destination == other.destination
            && self.input == other.input
            && self.weighted == other.weighted
            && self.nodes == other.nodes
    }
}

impl Eq for GraphTraversalNode {}

impl Hash for GraphTraversalNode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state);
        self.source.hash(state);
        self.destination.hash(state);
        self.input.hash(state);
        self.weighted.hash(state);
        self.nodes.hash(state);
    }
}

impl PartialOrd for GraphTraversalNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match self.key().cmp(&other.key()) {
            Ordering::Equal => {}
            unequal => return Some(unequal),
        }
        match self.source.partial_cmp(&other.source)? {
            Ordering::Equal => {}
            unequal => return Some(unequal),
        }
        match self.destination.partial_cmp(&other.destination)? {
            Ordering::Equal => {}
            unequal => return Some(unequal),
        }
        match self.input.partial_cmp(&other.input)? {
            Ordering::Equal => {}
            unequal => return Some(unequal),
        }
        match self.weighted.partial_cmp(&other.weighted)? {
            Ordering::Equal => {}
            unequal => return Some(unequal),
        }
        self.nodes.partial_cmp(&other.nodes)
    }
}

impl UserDefinedLogicalNodeCore for GraphTraversalNode {
    fn name(&self) -> &str {
        "GraphTraversal"
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        let mut inputs = vec![self.input.as_ref()];
        inputs.extend(self.weighted.as_deref());
        inputs.extend(self.nodes.iter().map(Arc::as_ref));
        inputs
    }

    fn schema(&self) -> &DFSchemaRef {
        &self.schema
    }

    /// The endpoint columns, so DataFusion keeps them in the input.
    fn expressions(&self) -> Vec<Expr> {
        vec![self.source.clone(), self.destination.clone()]
    }

    fn fmt_for_explain(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "GraphTraversal: graph={}, edges=({}, {}), call={}",
            self.graph,
            self.source,
            self.destination,
            self.display_call()
        )
    }

    fn with_exprs_and_inputs(&self, exprs: Vec<Expr>, inputs: Vec<LogicalPlan>) -> Result<Self> {
        let [source, destination]: [Expr; 2] = exprs
            .try_into()
            .map_err(|_| plan("GraphTraversal takes two expressions".into()))?;
        let weighted = usize::from(self.weighted.is_some());
        let expected = 1 + weighted + self.nodes.len();
        if inputs.len() != expected {
            return Err(plan(format!(
                "GraphTraversal takes {expected} inputs; got {}",
                inputs.len()
            )));
        }
        let mut inputs = inputs.into_iter().map(Arc::new);
        let input = inputs.next().expect("checked above");
        let weighted = (weighted == 1).then(|| inputs.next().expect("checked above"));
        Self::try_new(
            self.graph.clone(),
            self.traversal,
            input,
            weighted,
            self.weight.clone(),
            inputs.collect(),
            self.written.clone(),
            source,
            destination,
        )
    }

    /// Both endpoints of the first input, every column of the weighted input,
    /// and the one column of each node-id input.
    fn necessary_children_exprs(&self, _output_columns: &[usize]) -> Option<Vec<Vec<usize>>> {
        let (source, destination) = self.endpoint_indices().ok()?;
        let mut needed = vec![source, destination];
        needed.sort_unstable();
        needed.dedup();
        let mut children = vec![needed];
        if let Some(weighted) = &self.weighted {
            children.push((0..weighted.schema().fields().len()).collect());
        }
        children.extend(self.nodes.iter().map(|_| vec![0]));
        Some(children)
    }

    /// Filters on the output are about the traversal's rows, not the edges or
    /// the node ids: none moves below.
    fn prevent_predicate_push_down_columns(&self) -> std::collections::HashSet<String> {
        self.schema
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    }
}
