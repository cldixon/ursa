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
use ursa_core::{Direction, IdMap, Topology};

use crate::result::{hop_schema, path_schema, query_schema, OutputColumn};

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

/// The custom logical node for a `hop` traversal.
///
/// Like [`GraphAlgorithmNode`] it is a *leaf* — the topology is a side data
/// structure — but where the algorithm node emits a per-node `(id, values...)`
/// frame, `HopNode` emits an **edge** frame `(src, dst)` of reached pairs. The
/// seed set is carried as baked-in dense indices (`.from_(...)`), the smallest
/// faithful form of a seed-input traversal node: it stays a leaf `ExecutionPlan`
/// while still being a first-class citizen of the plan, so filter/sort/limit
/// compose after it. `crate::planner` lowers it to `crate::physical::HopExec`.
#[derive(Clone)]
pub struct HopNode {
    pub topology: Arc<Topology>,
    pub ids: Arc<IdMap>,
    pub seeds: Arc<Vec<u32>>,
    pub n: u32,
    pub direction: Direction,
    schema: DFSchemaRef,
}

impl HopNode {
    pub fn new(
        topology: Arc<Topology>,
        ids: Arc<IdMap>,
        seeds: Vec<u32>,
        n: u32,
        direction: Direction,
    ) -> Self {
        let schema = Arc::new(
            DFSchema::try_from(hop_schema().as_ref().clone())
                .expect("hop schema converts to a DFSchema"),
        );
        HopNode {
            topology,
            ids,
            seeds: Arc::new(seeds),
            n,
            direction,
            schema,
        }
    }
}

impl PartialEq for HopNode {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.topology, &other.topology)
            && self.seeds == other.seeds
            && self.n == other.n
            && self.direction == other.direction
    }
}

impl Eq for HopNode {}

impl PartialOrd for HopNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        // Order by the scalar params only (topology identity is not orderable).
        (self.n, self.seeds.len()).partial_cmp(&(other.n, other.seeds.len()))
    }
}

impl Hash for HopNode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.topology) as usize).hash(state);
        self.n.hash(state);
        self.seeds.hash(state);
    }
}

impl fmt::Debug for HopNode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.fmt_for_explain(f)
    }
}

impl UserDefinedLogicalNodeCore for HopNode {
    fn name(&self) -> &str {
        "Hop"
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
        write!(
            f,
            "Hop: n={}, direction={:?}, seeds={}",
            self.n,
            self.direction,
            self.seeds.len()
        )
    }

    fn with_exprs_and_inputs(&self, _exprs: Vec<Expr>, _inputs: Vec<LogicalPlan>) -> Result<Self> {
        Ok(self.clone())
    }
}

/// The custom logical node for a `shortest_path` traversal.
///
/// A sibling of [`HopNode`]: another leaf node with a baked-in source/target
/// (dense indices) that emits an **edge** frame — here `(src, dst, hop)`, the
/// edges of the single source→target path in order. `weighted` is carried for the
/// future weighted SSSP; v0.1 is unweighted BFS. Lowered to
/// `crate::physical::ShortestPathExec`.
#[derive(Clone)]
pub struct ShortestPathNode {
    pub topology: Arc<Topology>,
    pub ids: Arc<IdMap>,
    pub source: u32,
    pub target: u32,
    pub direction: Direction,
    pub weighted: bool,
    schema: DFSchemaRef,
}

impl ShortestPathNode {
    pub fn new(
        topology: Arc<Topology>,
        ids: Arc<IdMap>,
        source: u32,
        target: u32,
        direction: Direction,
        weighted: bool,
    ) -> Self {
        let schema = Arc::new(
            DFSchema::try_from(path_schema().as_ref().clone())
                .expect("path schema converts to a DFSchema"),
        );
        ShortestPathNode {
            topology,
            ids,
            source,
            target,
            direction,
            weighted,
            schema,
        }
    }
}

impl PartialEq for ShortestPathNode {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.topology, &other.topology)
            && self.source == other.source
            && self.target == other.target
            && self.direction == other.direction
            && self.weighted == other.weighted
    }
}

impl Eq for ShortestPathNode {}

impl PartialOrd for ShortestPathNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        (self.source, self.target).partial_cmp(&(other.source, other.target))
    }
}

impl Hash for ShortestPathNode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.topology) as usize).hash(state);
        self.source.hash(state);
        self.target.hash(state);
    }
}

impl fmt::Debug for ShortestPathNode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.fmt_for_explain(f)
    }
}

impl UserDefinedLogicalNodeCore for ShortestPathNode {
    fn name(&self) -> &str {
        "ShortestPath"
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
        write!(
            f,
            "ShortestPath: source={}, target={}, direction={:?}",
            self.source, self.target, self.direction
        )
    }

    fn with_exprs_and_inputs(&self, _exprs: Vec<Expr>, _inputs: Vec<LogicalPlan>) -> Result<Self> {
        Ok(self.clone())
    }
}
