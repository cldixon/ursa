//! The graph algorithm plan node.
//!
//! [`GraphAlgorithmNode`] takes its edges as a child plan: the graph's edge
//! table, projected to the source and destination columns. Anything that
//! produces edge rows can sit under it (a scan, a view's filter, a join), and
//! DataFusion's optimizer works on that child as on any plan: projections and
//! filters reach the scan. The node's output is `id` plus one column per
//! algorithm it computes.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::common::{DFSchema, DFSchemaRef};
use datafusion::error::Result;
use datafusion::logical_expr::{Expr, ExprSchemable, LogicalPlan, UserDefinedLogicalNodeCore};

use super::functions::{plan, Algorithm};

/// Run graph algorithms over the edges its input produces.
#[derive(Debug, Clone)]
pub struct GraphAlgorithmNode {
    /// The graph's name, for plans and messages. The index is keyed by the
    /// edges, not by this.
    pub graph: String,
    /// The edge rows.
    pub input: Arc<LogicalPlan>,
    /// The source and destination of each edge: column references into
    /// `input`.
    pub source: Expr,
    pub destination: Expr,
    /// What the node computes, one value column each.
    pub outputs: Vec<Algorithm>,
    /// The node id type: the endpoints' type, with every integer type read as
    /// `Int64` and every string type as `Utf8`, as the Python library reads
    /// them.
    pub id_type: DataType,
    schema: DFSchemaRef,
}

impl GraphAlgorithmNode {
    pub fn try_new(
        graph: String,
        input: Arc<LogicalPlan>,
        source: Expr,
        destination: Expr,
        outputs: Vec<Algorithm>,
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
        let mut fields = vec![Field::new("id", id_type.clone(), false)];
        fields.extend(
            outputs
                .iter()
                .map(|a| Field::new(a.name(), a.output_type(), false)),
        );
        let schema = Arc::new(DFSchema::try_from(Schema::new(fields))?);
        Ok(Self {
            graph,
            input,
            source,
            destination,
            outputs,
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

// The schema is derived from the other fields, so equality, hashing and order
// leave it out.
impl PartialEq for GraphAlgorithmNode {
    fn eq(&self, other: &Self) -> bool {
        self.graph == other.graph
            && self.outputs == other.outputs
            && self.source == other.source
            && self.destination == other.destination
            && self.input == other.input
    }
}

impl Eq for GraphAlgorithmNode {}

impl Hash for GraphAlgorithmNode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.graph.hash(state);
        self.outputs.hash(state);
        self.source.hash(state);
        self.destination.hash(state);
        self.input.hash(state);
    }
}

impl PartialOrd for GraphAlgorithmNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (&self.graph, &self.outputs).cmp(&(&other.graph, &other.outputs)) {
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
        self.input.partial_cmp(&other.input)
    }
}

impl UserDefinedLogicalNodeCore for GraphAlgorithmNode {
    fn name(&self) -> &str {
        "GraphAlgorithm"
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        vec![&self.input]
    }

    fn schema(&self) -> &DFSchemaRef {
        &self.schema
    }

    /// The endpoint columns, so DataFusion keeps them in the input.
    fn expressions(&self) -> Vec<Expr> {
        vec![self.source.clone(), self.destination.clone()]
    }

    fn fmt_for_explain(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let outputs: Vec<String> = self.outputs.iter().map(|a| a.to_string()).collect();
        write!(
            f,
            "GraphAlgorithm: graph={}, edges=({}, {}), outputs=[{}]",
            self.graph,
            self.source,
            self.destination,
            outputs.join(", ")
        )
    }

    fn with_exprs_and_inputs(&self, exprs: Vec<Expr>, inputs: Vec<LogicalPlan>) -> Result<Self> {
        let [source, destination]: [Expr; 2] = exprs
            .try_into()
            .map_err(|_| plan("GraphAlgorithm takes two expressions".into()))?;
        let [input]: [LogicalPlan; 1] = inputs
            .try_into()
            .map_err(|_| plan("GraphAlgorithm takes one input".into()))?;
        Self::try_new(
            self.graph.clone(),
            Arc::new(input),
            source,
            destination,
            self.outputs.clone(),
        )
    }

    /// Every output needs both endpoints, and nothing else of the input.
    fn necessary_children_exprs(&self, _output_columns: &[usize]) -> Option<Vec<Vec<usize>>> {
        let (source, destination) = self.endpoint_indices().ok()?;
        let mut needed = vec![source, destination];
        needed.sort_unstable();
        needed.dedup();
        Some(vec![needed])
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
