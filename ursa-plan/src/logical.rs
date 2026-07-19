//! Custom logical plan nodes for graph operations.
//!
//! Each graph verb becomes a `UserDefinedLogicalNodeCore` so it participates in
//! DataFusion's logical plan, optimization, and display. This module defines the
//! node *data* (params); the trait implementations that make them real logical
//! nodes are the next implementation step (they are verbose and version-coupled,
//! so the skeleton keeps them as plain types with a documented TODO).
//!
//! The **v0.1 planner ambition** (decided): ship these nodes with *naive*
//! placement first — execute in written order, correctness over cleverness — and
//! land the optimizer rules incrementally in v0.1.x/v0.2. The public API is
//! identical either way.

/// Traversal direction, mirrored from [`ursa_core::Direction`] at this layer so
/// logical nodes don't leak the core type into the plan surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Both,
}

impl From<Direction> for ursa_core::Direction {
    fn from(d: Direction) -> Self {
        match d {
            Direction::Out => ursa_core::Direction::Out,
            Direction::In => ursa_core::Direction::In,
            Direction::Both => ursa_core::Direction::Both,
        }
    }
}

/// A node-valued graph algorithm and its parameters. Produced by both spellings
/// of each kernel: the `with_columns` expression form and the standalone
/// NodeFrame form lower to the *same* `GraphAlgorithmNode { algo, .. }`.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphAlgo {
    PageRank {
        damping: f64,
        max_iter: u32,
        tol: f64,
    },
    ConnectedComponents {
        // "weak" for v0.1; "strong" later.
        strong: bool,
    },
    Degree {
        direction: Direction,
    },
    TriangleCount,
    ClusteringCoefficient,
    Betweenness {
        sample: Option<f64>,
    },
    Closeness,
    LabelPropagation {
        max_iter: u32,
        seed: Option<u64>,
    },
    Louvain {
        resolution: f64,
        seed: Option<u64>,
    },
}

/// `HopNode` — one/`n`-hop expansion; lowers to a frontier `ExecutionPlan`.
/// EdgeFrame -> EdgeFrame, so traversal composes.
#[derive(Debug, Clone)]
pub struct HopNode {
    pub n: u32,
    pub direction: Direction,
    // seed restriction (`.from_(...)`) is a child plan feeding this node.
}

/// `NeighborAggNode` — neighbour aggregation, fused into a segmented reduction
/// over CSR by the optimizer (never materializes neighbour lists).
#[derive(Debug, Clone)]
pub struct NeighborAggNode {
    pub direction: Direction,
    // the aggregate expression is a child DataFusion expr resolved against the
    // ambient frame (the attribute-resolution rule).
}

/// `GraphAlgorithmNode` — a node-valued algorithm over the topology.
#[derive(Debug, Clone)]
pub struct GraphAlgorithmNode {
    pub algo: GraphAlgo,
    // weight is an optional child expr over edge columns, evaluated to an Arrow
    // array before the kernel runs (gathered via Topology::edge_ids).
}

/// `ShortestPathNode` — single-pair (v0.1) shortest path; EdgeFrame with `hop`
/// (and, weighted, cost) columns.
#[derive(Debug, Clone)]
pub struct ShortestPathNode {
    pub source: i64,
    pub target: i64,
    pub direction: Direction,
    pub weighted: bool,
}

/// `RandomWalkNode` — `(walk_id, step, node)` frame; feeds node2vec-style pipelines.
#[derive(Debug, Clone)]
pub struct RandomWalkNode {
    pub steps: u32,
    pub walks_per_node: u32,
    pub seed: Option<u64>,
}

// TODO(v0.1): impl `UserDefinedLogicalNodeCore` for each node above
// (name/inputs/schema/expressions/fmt_for_explain/with_exprs_and_inputs), then
// register `ExecutionPlan` builders in `crate::physical`.
