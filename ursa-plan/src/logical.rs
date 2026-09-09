//! Shared plan-layer enums: traversal [`Direction`] and the node-valued
//! algorithm descriptor [`GraphAlgo`].
//!
//! The custom logical plan *nodes* themselves (with their full
//! `UserDefinedLogicalNodeCore` implementations) live in [`crate::node`]; this
//! module holds only the small parameter types they share, kept here so logical
//! nodes don't leak `ursa_core` types into the plan surface.
//!
//! The **v0.1 planner ambition** (decided): the nodes execute with *naive*
//! placement — in written order, correctness over cleverness — with the optimizer
//! rules landing incrementally in v0.1.x/v0.2. The public API is identical either
//! way.

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
        // false = weak (undirected union-find); true = strong (Tarjan SCC).
        strong: bool,
    },
    Degree {
        direction: Direction,
    },
    TriangleCount,
    ClusteringCoefficient,
    Betweenness {
        sample: Option<f64>,
        seed: Option<u64>,
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
    /// Force-directed layout. The first kernel that emits **two** columns from one
    /// invocation — an output column selects x or y through its `field`, and both
    /// share a memo entry so the simulation runs once.
    Layout {
        kind: LayoutKind,
        iterations: u32,
        k: f64,
        gravity: f64,
        /// Barnes-Hut opening angle. Ignored by `Random` and `Circle`, which have
        /// no repulsion to approximate.
        theta: f64,
        /// ForceAtlas2 only: gravity proportional to distance rather than constant.
        strong_gravity: bool,
        /// ForceAtlas2 only: `ln(1 + d)` attraction instead of linear.
        lin_log: bool,
        /// ForceAtlas2 only: the paper's swing tolerance, `tau`.
        jitter_tolerance: f64,
        seed: Option<u64>,
    },
}

/// Which layout to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    /// ForceAtlas2 with a Barnes-Hut quadtree — degree-weighted repulsion and an
    /// adaptive step. The default worth reaching for on a real graph.
    Fa2,
    /// Fruchterman–Reingold, also Barnes-Hut accelerated. The simpler force model:
    /// quadratic attraction, unweighted repulsion, a fixed cooling schedule.
    Fr,
    /// A deterministic spread — the trivial layout, and what the others seed from.
    Random,
    /// Evenly spaced on a circle, in dense index order.
    Circle,
}

// The concrete logical nodes (`GraphAlgorithmNode`, `HopNode`, `ShortestPathNode`,
// `RandomWalkNode`) and their `UserDefinedLogicalNodeCore` impls live in
// `crate::node`; this module intentionally holds only the shared enums above.
