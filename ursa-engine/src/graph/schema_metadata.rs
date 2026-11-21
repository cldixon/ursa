//! A plan node that changes nothing but its schema's metadata.
//!
//! `GRAPH_TABLE` says which pattern it matched in its schema's metadata
//! (`ursa.pattern`, see [`super::provenance`]). DataFusion carries a schema's
//! metadata upwards from the plan's inputs (through projections, filters,
//! sorts, limits, joins and aggregates), but nothing in it sets one. So the
//! match's plan is wrapped in a [`SchemaMetadataNode`]: its rows and columns
//! are its input's, and its schema has the entries added.
//!
//! The node is no barrier to the optimizer: filters, limits and projections
//! pass through it to its input. Its physical plan, [`SchemaMetadataExec`],
//! relabels each batch (the arrays are shared, not copied).

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::Arc;

use datafusion::arrow::datatypes::SchemaRef;
use datafusion::common::tree_node::TreeNodeRecursion;
use datafusion::common::{DFSchema, DFSchemaRef};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::logical_expr::{Expr, LogicalPlan, UserDefinedLogicalNodeCore};
use datafusion::physical_expr::PhysicalExpr;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, ExecutionPlanProperties, PlanProperties,
};
use futures::StreamExt;

/// Its input, with `metadata` added to the schema's own metadata.
#[derive(Debug, Clone)]
pub struct SchemaMetadataNode {
    pub input: Arc<LogicalPlan>,
    /// The entries added, in key order.
    pub metadata: BTreeMap<String, String>,
    schema: DFSchemaRef,
}

impl SchemaMetadataNode {
    pub fn try_new(input: Arc<LogicalPlan>, metadata: BTreeMap<String, String>) -> Result<Self> {
        let from = input.schema();
        let mut all = from.metadata().clone();
        all.extend(metadata.iter().map(|(k, v)| (k.clone(), v.clone())));
        let fields = from
            .iter()
            .map(|(qualifier, field)| (qualifier.cloned(), Arc::clone(field)))
            .collect();
        let schema = DFSchema::new_with_metadata(fields, all)?
            .with_functional_dependencies(from.functional_dependencies().clone())?;
        Ok(Self {
            input,
            metadata,
            schema: Arc::new(schema),
        })
    }
}

// The schema is derived from the other fields.
impl PartialEq for SchemaMetadataNode {
    fn eq(&self, other: &Self) -> bool {
        self.metadata == other.metadata && self.input == other.input
    }
}

impl Eq for SchemaMetadataNode {}

impl std::hash::Hash for SchemaMetadataNode {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.metadata.hash(state);
        self.input.hash(state);
    }
}

impl PartialOrd for SchemaMetadataNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match self.metadata.cmp(&other.metadata) {
            Ordering::Equal => self.input.partial_cmp(&other.input),
            unequal => Some(unequal),
        }
    }
}

impl UserDefinedLogicalNodeCore for SchemaMetadataNode {
    fn name(&self) -> &str {
        "SchemaMetadata"
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        vec![self.input.as_ref()]
    }

    fn schema(&self) -> &DFSchemaRef {
        &self.schema
    }

    fn expressions(&self) -> Vec<Expr> {
        Vec::new()
    }

    fn fmt_for_explain(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let keys: Vec<&str> = self.metadata.keys().map(String::as_str).collect();
        write!(f, "SchemaMetadata: {}", keys.join(", "))
    }

    fn with_exprs_and_inputs(&self, exprs: Vec<Expr>, inputs: Vec<LogicalPlan>) -> Result<Self> {
        let ([], [input]): ([Expr; 0], [LogicalPlan; 1]) = (
            exprs
                .try_into()
                .map_err(|_| internal("SchemaMetadata takes no expressions"))?,
            inputs
                .try_into()
                .map_err(|_| internal("SchemaMetadata takes one input"))?,
        );
        Self::try_new(Arc::new(input), self.metadata.clone())
    }

    /// The columns asked of the node are the columns it asks of its input.
    fn necessary_children_exprs(&self, output_columns: &[usize]) -> Option<Vec<Vec<usize>>> {
        Some(vec![output_columns.to_vec()])
    }

    /// Every filter may move below: the rows are the input's.
    fn prevent_predicate_push_down_columns(&self) -> HashSet<String> {
        HashSet::new()
    }

    fn supports_limit_pushdown(&self) -> bool {
        true
    }
}

fn internal(message: &str) -> DataFusionError {
    DataFusionError::Internal(message.to_string())
}

/// Its input's batches, under a schema with more metadata (see
/// [`SchemaMetadataNode`]).
#[derive(Debug)]
pub struct SchemaMetadataExec {
    input: Arc<dyn ExecutionPlan>,
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
}

impl SchemaMetadataExec {
    /// `input` under `schema`, which must be `input`'s schema but for its
    /// metadata.
    pub fn try_new(input: Arc<dyn ExecutionPlan>, schema: SchemaRef) -> Result<Self> {
        if !schema.contains(&input.schema()) {
            return Err(DataFusionError::Internal(format!(
                "SchemaMetadataExec: the schema must be its input's but for metadata; got {schema:?} over {:?}",
                input.schema()
            )));
        }
        let equivalences = input
            .equivalence_properties()
            .clone()
            .with_new_schema(Arc::clone(&schema))?;
        let properties = Arc::new(PlanProperties::new(
            equivalences,
            input.output_partitioning().clone(),
            input.pipeline_behavior(),
            input.boundedness(),
        ));
        Ok(Self {
            input,
            schema,
            properties,
        })
    }
}

impl DisplayAs for SchemaMetadataExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        let mut keys: Vec<&str> = self.schema.metadata().keys().map(String::as_str).collect();
        keys.sort_unstable();
        write!(f, "SchemaMetadataExec: {}", keys.join(", "))
    }
}

impl ExecutionPlan for SchemaMetadataExec {
    fn name(&self) -> &str {
        "SchemaMetadataExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.input]
    }

    fn maintains_input_order(&self) -> Vec<bool> {
        vec![true]
    }

    /// One batch out per batch in: nothing to gain from more partitions.
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
            .map_err(|_| internal("SchemaMetadataExec has one child"))?;
        Ok(Arc::new(Self::try_new(input, Arc::clone(&self.schema))?))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let schema = Arc::clone(&self.schema);
        let relabel = Arc::clone(&schema);
        let batches = self.input.execute(partition, context)?.map(move |batch| {
            batch?
                .with_schema(Arc::clone(&relabel))
                .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
        });
        Ok(Box::pin(RecordBatchStreamAdapter::new(schema, batches)))
    }
}
