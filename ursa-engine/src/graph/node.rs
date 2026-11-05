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
        source: Expr,
        destination: Expr,
        weights: Vec<String>,
        outputs: Vec<Output>,
        per_node: bool,
    ) -> Result<Self> {
        let source_type = source.get_type(input.schema())?;
        let destination_type = destination.get_type(input.schema())?;
        let id_type = match (id_type(&source_type), id_type(&destination_type)) {
            (Some(s), Some(d)) if s == d => s,
            (Some(_), Some(_)) => {
                return Err(plan(format!(
                    "graph '{graph}': the source and destination columns must have one id type \
                     (both integer or both string); got {source_type} and {destination_type}"
                )))
            }
            (None, _) | (_, None) => {
                let bad = if id_type(&source_type).is_none() {
                    source_type
                } else {
                    destination_type
                };
                return Err(plan(format!(
                    "graph '{graph}': node ids must be an integer or string column; got {bad}"
                )));
            }
        };
        let mut fields = Vec::new();
        if per_node {
            fields.push(Field::new("id", id_type.clone(), false));
        }
        for o in &outputs {
            let spec = o.call.spec().outputs[o.field];
            fields.push(Field::new(&o.name, spec.ty.data_type(), spec.nullable));
        }
        let schema = Arc::new(DFSchema::try_from(Schema::new(fields))?);
        Ok(Self {
            graph,
            input,
            weighted,
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
        let index = |e: &Expr| -> Result<usize> {
            match e {
                Expr::Column(c) => self.input.schema().index_of_column(c),
                other => Err(plan(format!(
                    "graph '{}': an edge endpoint must be a column; got {other}",
                    self.graph
                ))),
            }
        };
        Ok((index(&self.source)?, index(&self.destination)?))
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

/// The node id type a column of `t` gives: integers as `Int64`, strings as
/// `Utf8` (as `ursa-io` canonicalizes a scan's endpoints), or `None` if `t`
/// cannot hold node ids.
pub(crate) fn id_type(t: &DataType) -> Option<DataType> {
    use DataType::*;
    match t {
        Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64 => Some(Int64),
        Utf8 | LargeUtf8 | Utf8View => Some(Utf8),
        Dictionary(_, value) => id_type(value),
        _ => None,
    }
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
        self.weighted.partial_cmp(&other.weighted)
    }
}

impl UserDefinedLogicalNodeCore for GraphAlgorithmNode {
    fn name(&self) -> &str {
        "GraphAlgorithm"
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        let mut inputs = vec![self.input.as_ref()];
        inputs.extend(self.weighted.as_deref());
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
        let expected = 1 + usize::from(self.weighted.is_some());
        if inputs.len() != expected {
            return Err(plan(format!(
                "GraphAlgorithm takes {expected} inputs; got {}",
                inputs.len()
            )));
        }
        let mut inputs = inputs.into_iter().map(Arc::new);
        let input = inputs.next().expect("checked above");
        let weighted = inputs.next();
        Self::try_new(
            self.graph.clone(),
            input,
            weighted,
            source,
            destination,
            self.weights.clone(),
            self.outputs.clone(),
            self.per_node,
        )
    }

    /// Every output needs both endpoints and nothing else of the first input,
    /// and every column of the weighted input.
    fn necessary_children_exprs(&self, _output_columns: &[usize]) -> Option<Vec<Vec<usize>>> {
        let (source, destination) = self.endpoint_indices().ok()?;
        let mut needed = vec![source, destination];
        needed.sort_unstable();
        needed.dedup();
        let mut children = vec![needed];
        if let Some(weighted) = &self.weighted {
            children.push((0..weighted.schema().fields().len()).collect());
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
