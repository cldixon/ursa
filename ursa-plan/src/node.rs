//! The custom logical plan node for a graph query.
//!
//! [`GraphAlgorithmNode`] is a `UserDefinedLogicalNode`: it carries the shared
//! `Arc<Topology>` + `IdMap` and the query's output columns (`[(name, algo)]`),
//! and reports a `(id, values...)` schema. It is a *leaf* node — the topology is
//! a side data structure, not a child plan — so a graph query becomes a first-
//! class citizen of one DataFusion `LogicalPlan`. `crate::planner` lowers it to
//! the physical [`crate::physical::GraphAlgorithmExec`].

use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use datafusion::common::{DFSchema, DFSchemaRef, Result};
use datafusion::logical_expr::{Expr, LogicalPlan, UserDefinedLogicalNodeCore};
use ursa_core::{IdMap, Topology};

use crate::result::{query_schema, OutputColumn};

#[derive(Clone)]
pub struct GraphAlgorithmNode {
    pub topology: Arc<Topology>,
    pub ids: Arc<IdMap>,
    pub columns: Arc<Vec<OutputColumn>>,
    schema: DFSchemaRef,
}

impl GraphAlgorithmNode {
    pub fn new(topology: Arc<Topology>, ids: Arc<IdMap>, columns: Vec<OutputColumn>) -> Self {
        let arrow_schema = query_schema(&columns);
        let schema = Arc::new(
            DFSchema::try_from(arrow_schema.as_ref().clone())
                .expect("query schema converts to a DFSchema"),
        );
        GraphAlgorithmNode {
            topology,
            ids,
            columns: Arc::new(columns),
            schema,
        }
    }
}

// Topology / IdMap have no value equality; identity (Arc pointer) is the right
// notion here, and the columns carry the query's meaning.
impl PartialEq for GraphAlgorithmNode {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.topology, &other.topology)
            && Arc::ptr_eq(&self.ids, &other.ids)
            && self.columns == other.columns
    }
}

impl Eq for GraphAlgorithmNode {}

impl PartialOrd for GraphAlgorithmNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        // Order by column names only (topology identity is not orderable).
        let a: Vec<&str> = self.columns.iter().map(|c| c.name()).collect();
        let b: Vec<&str> = other.columns.iter().map(|c| c.name()).collect();
        a.partial_cmp(&b)
    }
}

impl Hash for GraphAlgorithmNode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.topology) as usize).hash(state);
        for col in self.columns.iter() {
            col.name().hash(state);
        }
    }
}

impl fmt::Debug for GraphAlgorithmNode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.fmt_for_explain(f)
    }
}

impl UserDefinedLogicalNodeCore for GraphAlgorithmNode {
    fn name(&self) -> &str {
        "GraphAlgorithm"
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        vec![]
    }

    fn schema(&self) -> &DFSchemaRef {
        &self.schema
    }

    fn expressions(&self) -> Vec<Expr> {
        vec![]
    }

    fn fmt_for_explain(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let names: Vec<&str> = self.columns.iter().map(|c| c.name()).collect();
        write!(f, "GraphAlgorithm: columns=[{}]", names.join(", "))
    }

    fn with_exprs_and_inputs(&self, _exprs: Vec<Expr>, _inputs: Vec<LogicalPlan>) -> Result<Self> {
        Ok(self.clone())
    }
}
