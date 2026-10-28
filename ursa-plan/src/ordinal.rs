//! Numbering rows inside a plan: [`EnumerateNode`] appends a `UInt64` column
//! counting its input's rows `0..`, in the order they arrive.
//!
//! A join numbers each input so its output can be put back in the order the join
//! guarantees (see [`crate::lower`]). Materialized rows are numbered directly
//! ([`crate::lower::attach_ordinal`]); a frame that is still a plan (a graph or
//! traversal leaf, and the order-keeping steps over it) is numbered by this node
//! instead, so it runs as part of the join's plan rather than being collected
//! first. Its input must emit its rows in order, as one partition: the node
//! requires a single partition, and the frame only uses it on one that is
//! `in_order`.

use std::fmt;
use std::sync::Arc;

use arrow::array::{RecordBatch, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::common::{DFSchema, DFSchemaRef};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::TaskContext;
use datafusion::logical_expr::{Expr, Extension, LogicalPlan, UserDefinedLogicalNodeCore};
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, Distribution, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::prelude::DataFrame;
use futures::StreamExt;

/// Append the ordinal column `name` to `df`, numbering its rows in the order the
/// plan emits them. `limit` caps the row count (an error past it).
pub fn enumerate(df: DataFrame, name: &str, limit: u64) -> Result<DataFrame> {
    let (state, input) = df.into_parts();
    let mut fields: Vec<Field> = input
        .schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect();
    fields.push(Field::new(name, DataType::UInt64, false));
    let arrow = Schema::new_with_metadata(fields, input.schema().metadata().clone());
    let node = EnumerateNode {
        input,
        limit,
        schema: Arc::new(DFSchema::try_from(arrow)?),
    };
    let plan = LogicalPlan::Extension(Extension {
        node: Arc::new(node),
    });
    Ok(DataFrame::new(state, plan))
}

/// The logical node of [`enumerate`]: its input's columns (unqualified), then the
/// ordinal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EnumerateNode {
    input: LogicalPlan,
    limit: u64,
    schema: DFSchemaRef,
}

impl PartialOrd for EnumerateNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        (&self.input, self.limit)
            .partial_cmp(&(&other.input, other.limit))
            // Never `Equal` unless `eq` holds.
            .filter(|o| o.is_ne() || self == other)
    }
}

impl UserDefinedLogicalNodeCore for EnumerateNode {
    fn name(&self) -> &str {
        "Enumerate"
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        vec![&self.input]
    }

    fn schema(&self) -> &DFSchemaRef {
        &self.schema
    }

    fn expressions(&self) -> Vec<Expr> {
        vec![]
    }

    fn fmt_for_explain(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Enumerate")
    }

    fn with_exprs_and_inputs(&self, _exprs: Vec<Expr>, inputs: Vec<LogicalPlan>) -> Result<Self> {
        let [input] = <[LogicalPlan; 1]>::try_from(inputs)
            .map_err(|_| DataFusionError::Internal("Enumerate takes one input".into()))?;
        Ok(EnumerateNode {
            input,
            limit: self.limit,
            schema: self.schema.clone(),
        })
    }
}

impl EnumerateNode {
    /// The node's physical operator over `input`.
    pub fn plan(&self, input: Arc<dyn ExecutionPlan>) -> Arc<dyn ExecutionPlan> {
        let schema: SchemaRef = Arc::new(self.schema.as_arrow().clone());
        Arc::new(EnumerateExec::new(input, schema, self.limit))
    }
}

/// The physical operator of [`EnumerateNode`]: one partition in, the same rows
/// out in the same order, each batch with its rows' positions appended.
#[derive(Debug)]
pub struct EnumerateExec {
    input: Arc<dyn ExecutionPlan>,
    schema: SchemaRef,
    limit: u64,
    properties: Arc<PlanProperties>,
}

impl EnumerateExec {
    fn new(input: Arc<dyn ExecutionPlan>, schema: SchemaRef, limit: u64) -> Self {
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        ));
        EnumerateExec {
            input,
            schema,
            limit,
            properties,
        }
    }
}

impl DisplayAs for EnumerateExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "EnumerateExec")
    }
}

impl ExecutionPlan for EnumerateExec {
    fn name(&self) -> &str {
        "EnumerateExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn required_input_distribution(&self) -> Vec<Distribution> {
        vec![Distribution::SinglePartition]
    }

    fn maintains_input_order(&self) -> Vec<bool> {
        vec![true]
    }

    fn benefits_from_input_partitioning(&self) -> Vec<bool> {
        vec![false]
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.input]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let [input] = <[Arc<dyn ExecutionPlan>; 1]>::try_from(children)
            .map_err(|_| DataFusionError::Internal("EnumerateExec takes one input".into()))?;
        Ok(Arc::new(EnumerateExec::new(
            input,
            self.schema.clone(),
            self.limit,
        )))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        if partition != 0 || self.input.properties().partitioning.partition_count() != 1 {
            return Err(DataFusionError::Internal(
                "Enumerate numbers a single ordered partition".into(),
            ));
        }
        let schema = self.schema.clone();
        let limit = self.limit;
        let mut next = 0u64;
        let stream = self.input.execute(0, context)?.map(move |batch| {
            let batch = batch?;
            let n = batch.num_rows() as u64;
            if next + n > limit {
                return Err(DataFusionError::NotImplemented(format!(
                    "a join input of more than {limit} rows"
                )));
            }
            let mut cols = batch.columns().to_vec();
            cols.push(Arc::new(UInt64Array::from_iter_values(next..next + n)));
            next += n;
            RecordBatch::try_new(schema.clone(), cols)
                .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
        });
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            self.schema.clone(),
            stream,
        )))
    }
}
