//! The plan IR and its one entry point, [`execute_plan`].
//!
//! A [`Plan`] is a whole query as a tree: one variant per frame step, each holding
//! its input as a child plan, so a `join` has two children and a graph verb holds
//! the edge plan it runs over. The Python layer serializes a frame to this tree
//! (JSON, `deny_unknown_fields` on every variant) and calls [`execute_plan`];
//! nothing else in the engine is reached from Python for a `collect()`.
//!
//! ## What runs where (#189)
//!
//! This is the first step of moving planning into Rust (#190). [`execute_plan`]
//! owns the *dispatch*: it walks the tree, rejects the shapes that aren't
//! supported (the [`PlanError::NotImplemented`] guards, with the messages the
//! Python layer used to raise), and maps each supported shape onto the existing
//! executors in [`crate::query`]. What those executors do is unchanged.
//!
//! Some leaf work still belongs to the host (the Python layer) and is reached
//! through [`Inputs`], by slot:
//!
//! - in-memory Arrow tables and scans of plain sources, and the cached
//!   [`GraphIndex`] of each edge frame;
//! - subgraph-view edge masks and the edge columns a weight reads (both cached on
//!   the host's frames);
//! - seed id sets;
//! - the relational steps the engine's fixed-order tail can't run in the order
//!   written (the pyarrow tail, retired by #191).
//!
//! Inputs are resolved lazily, in the order the executor needs them, so a plan
//! that fails validation never builds an index or reads a file.
//!
//! ## The edge input of a graph node
//!
//! Every graph node (`with_columns` of a verb, `hop`, `shortest_path`,
//! `random_walk`, `describe`) names its edges as an [`EdgeInput`]: an edge
//! subplan plus the host frame it came from. [`EdgeInput::view`] splits it into
//! the interface #191 and #192 build on, an [`EdgeView`]:
//!
//! - `topology`: the subplan whose rows define the CSR. Today that is a source
//!   (under any `reverse`), or a traversal's own input. #191 widens it to any
//!   subplan (a `distinct`, `join`, `group_by`, ...), rebuilt from that subplan's
//!   output and cached by a fingerprint of it.
//! - a mask, derived in dense id space over the topology's edge rows: the
//!   `filter` predicates over those rows (a subgraph view, #114), ANDed, and the
//!   subgraph induced by a traversal's reached node set (#116). Today the host
//!   evaluates it ([`Inputs::edge_mask`]); #192 evaluates it here, as a bitset
//!   over CSR edge ids, with no round trip through user ids.

use std::any::Any;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use arrow::array::{ArrayRef, RecordBatch};
use datafusion::error::DataFusionError;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use ursa_core::{EdgeMask, IdMap, Topology};

use crate::expr::GraphVerb;
use crate::query::{
    execute_hop_query, execute_join_query, execute_node_query_cached, execute_path_query,
    execute_walk_query, NodeColumn,
};
use crate::stats::{describe, parse_stat_direction};
use crate::weight::WeightCache;

/// A numbered input the host resolves: a frame (its table, scan, index, mask) or
/// a seed set.
pub type Slot = usize;

/// A query plan: one node per frame step, each holding its input.
///
/// Relational nodes carry `step`, the host's handle for the frame step they came
/// from, so the host can run that step itself when the engine's fixed-order tail
/// can't (see [`Inputs::apply_steps`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Plan {
    /// `ur.scan_edges(path, src=, dst=)`.
    ScanEdges {
        frame: Slot,
        /// The path(s), when representable; the host holds the full scan spec
        /// (including storage options) until scans move here (#192).
        #[serde(default)]
        path: Option<ScanPath>,
        src: String,
        dst: String,
        on_null: String,
    },
    /// `ur.scan_nodes(path, id=)`.
    ScanNodes {
        frame: Slot,
        #[serde(default)]
        path: Option<ScanPath>,
        id: String,
    },
    /// An in-memory Arrow table: an edge frame (`src`/`dst`) or a node frame (`id`).
    FromArrow {
        frame: Slot,
        #[serde(default)]
        src: Option<String>,
        #[serde(default)]
        dst: Option<String>,
        #[serde(default)]
        id: Option<String>,
    },
    Filter {
        input: Box<Plan>,
        predicate: Expr,
        #[serde(default)]
        step: Option<Slot>,
    },
    Select {
        input: Box<Plan>,
        columns: Vec<String>,
        #[serde(default)]
        step: Option<Slot>,
    },
    WithColumns {
        input: Box<Plan>,
        columns: Vec<NamedExpr>,
    },
    Rename {
        input: Box<Plan>,
        /// `(old, new)` pairs, in the order given.
        mapping: Vec<(String, String)>,
        #[serde(default)]
        step: Option<Slot>,
    },
    Sort {
        input: Box<Plan>,
        by: Vec<SortKey>,
        #[serde(default)]
        step: Option<Slot>,
    },
    Head {
        input: Box<Plan>,
        n: usize,
        #[serde(default)]
        step: Option<Slot>,
    },
    Sample {
        input: Box<Plan>,
        n: usize,
        #[serde(default)]
        seed: Option<u64>,
        #[serde(default)]
        step: Option<Slot>,
    },
    Distinct {
        input: Box<Plan>,
        #[serde(default)]
        step: Option<Slot>,
    },
    Join {
        input: Box<Plan>,
        right: Box<Plan>,
        on: Vec<String>,
        how: String,
    },
    GroupByAgg {
        input: Box<Plan>,
        keys: Vec<String>,
        aggs: Vec<AggSpec>,
        #[serde(default)]
        step: Option<Slot>,
    },
    /// `edges.nodes()`: the node set of an edge plan.
    Nodes { input: Box<Plan> },
    /// `edges.reverse()`: swap the src/dst roles.
    Reverse { input: Box<Plan> },
    /// `ur.hop(edges, n, direction).from_(seeds)`.
    Hop {
        edges: EdgeInput,
        n: u32,
        direction: String,
        seeds: Slot,
    },
    /// `ur.shortest_path(edges, source, target, weight=, direction=)`. `source`
    /// and `target` are seed slots holding one id each.
    ShortestPath {
        edges: EdgeInput,
        source: Slot,
        target: Slot,
        #[serde(default)]
        weight: Option<Value>,
        direction: String,
    },
    /// `ur.random_walk(edges, start, steps, walks_per_node, seed)`.
    RandomWalk {
        edges: EdgeInput,
        start: Slot,
        steps: u32,
        walks_per_node: u32,
        #[serde(default)]
        seed: Option<u64>,
    },
    /// `ur.describe(edges, full, direction)`.
    Describe {
        edges: EdgeInput,
        full: bool,
        direction: String,
    },
}

/// A scan path: one path (a glob included) or a list of them.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ScanPath {
    One(String),
    Many(Vec<String>),
}

/// One `with_columns` output.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedExpr {
    pub name: String,
    pub expr: Expr,
}

/// One sort key.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SortKey {
    pub column: String,
    #[serde(default)]
    pub descending: bool,
}

/// One `group_by().agg()` aggregation: `fn` over `column`, named `output`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AggSpec {
    pub output: String,
    #[serde(rename = "fn")]
    pub func: String,
    pub column: String,
}

/// An expression in the grammar of [`crate::expr`]: a graph verb, typed, or any
/// other node, kept as its JSON and parsed where it is used (a filter predicate
/// after its role references resolve, a weight by the weight evaluator).
#[derive(Debug, Clone)]
pub enum Expr {
    Graph(Box<GraphVerb>),
    Scalar(Value),
}

impl<'de> Deserialize<'de> for Expr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        if kind(&v) == "graph" {
            GraphVerb::from_value(&v)
                .map(|g| Expr::Graph(Box::new(g)))
                .map_err(serde::de::Error::custom)
        } else {
            Ok(Expr::Scalar(v))
        }
    }
}

/// The `kind` tag of a JSON expression node (`""` when absent).
fn kind(v: &Value) -> &str {
    v.get("kind").and_then(Value::as_str).unwrap_or("")
}

/// The edges a graph node runs over: the edge plan as written, and the host frame
/// it came from (whose cached index and mask the host holds). See the module docs
/// and [`EdgeInput::view`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeInput {
    pub plan: Box<Plan>,
    pub frame: Slot,
}

impl EdgeInput {
    /// Parse an edge input from its JSON (as [`Plan::from_json`]).
    pub fn from_json(json: &str) -> PlanResult<EdgeInput> {
        let mut de = serde_json::Deserializer::from_str(json);
        de.disable_recursion_limit();
        let edges =
            EdgeInput::deserialize(&mut de).map_err(|e| PlanError::Invalid(e.to_string()))?;
        de.end().map_err(|e| PlanError::Invalid(e.to_string()))?;
        Ok(edges)
    }
}

/// An [`EdgeInput`] split into the topology it runs over and the mask that
/// restricts it. See the module docs.
#[derive(Debug)]
pub struct EdgeView<'a> {
    /// The plan whose rows define the CSR.
    pub topology: &'a Plan,
    /// `filter` predicates over the topology's edge rows, ANDed (#114).
    pub predicates: Vec<&'a Expr>,
    /// A `hop` / `shortest_path` whose reached node set induces the subgraph (#116).
    pub induced_by: Option<&'a Plan>,
}

impl EdgeView<'_> {
    /// Whether the view restricts its topology at all.
    pub fn has_mask(&self) -> bool {
        !self.predicates.is_empty() || self.induced_by.is_some()
    }
}

impl EdgeInput {
    /// Split the edge plan into its [`EdgeView`], rejecting the edge plans a graph
    /// op can't run over yet: a step that reshapes the edge set (`distinct`,
    /// `sample`, `join`, `group_by`), or a relational step after a traversal.
    pub fn view(&self) -> PlanResult<EdgeView<'_>> {
        let chain = self.plan.chain();
        let ops: BTreeSet<&str> = chain.iter().map(|p| p.op()).collect();
        let traversal = chain
            .iter()
            .copied()
            .find(|p| matches!(p, Plan::Hop { .. } | Plan::ShortestPath { .. }));
        if traversal.is_some() {
            let tail: Vec<&str> = ops
                .iter()
                .copied()
                .filter(|op| !TRAVERSAL_OPS.contains(op))
                .collect();
            if !tail.is_empty() {
                return Err(PlanError::NotImplemented(format!(
                    "a graph op over a traversal result supports a bare hop/shortest_path \
                     for now; a relational step ({}) between the traversal and the graph op \
                     isn't wired yet — collect() the traversal and re-ingest via \
                     ur.from_arrow(...) to run further ops on it.",
                    tail.join(", ")
                )));
            }
        } else if ops
            .iter()
            .any(|op| ["distinct", "sample", "join", "group_by_agg"].contains(op))
        {
            return Err(PlanError::NotImplemented(
                "graph ops on a distinct/sample/join/group_by-derived edge frame aren't \
                 wired yet — unlike filter (a subgraph view over the parent topology), \
                 these reshape the edge set in a way an edge mask can't express. Run the op \
                 on the source frame, or collect and re-ingest the derived edges."
                    .into(),
            ));
        }
        let predicates = chain
            .iter()
            .filter_map(|p| match p {
                Plan::Filter { predicate, .. } => Some(predicate),
                _ => None,
            })
            .collect();
        // The topology: the traversal's own input, else the nearest `reverse` or
        // source below the property-only steps.
        let topology = match traversal {
            Some(Plan::Hop { edges, .. }) | Some(Plan::ShortestPath { edges, .. }) => {
                edges.plan.as_ref()
            }
            _ => chain
                .iter()
                .rev()
                .copied()
                .find(|p| p.input().is_none() || matches!(p, Plan::Reverse { .. }))
                .unwrap_or(self.plan.as_ref()),
        };
        Ok(EdgeView {
            topology,
            predicates,
            induced_by: traversal,
        })
    }

    /// The traversal this edge plan is the result of, if any.
    fn traversal(&self) -> Option<&Plan> {
        self.plan
            .chain()
            .into_iter()
            .find(|p| matches!(p, Plan::Hop { .. } | Plan::ShortestPath { .. }))
    }
}

const TRAVERSAL_OPS: [&str; 2] = ["hop", "shortest_path"];

/// The relational steps the engine's tail and the host both run (`_TAIL_OPS`).
const TAIL_OPS: [&str; 8] = [
    "filter",
    "sort",
    "head",
    "distinct",
    "sample",
    "rename",
    "select",
    "group_by_agg",
];

impl Plan {
    /// The step name, as the frame API spells it (and as error messages quote it).
    pub fn op(&self) -> &'static str {
        match self {
            Plan::ScanEdges { .. } => "scan_edges",
            Plan::ScanNodes { .. } => "scan_nodes",
            Plan::FromArrow { .. } => "from_arrow",
            Plan::Filter { .. } => "filter",
            Plan::Select { .. } => "select",
            Plan::WithColumns { .. } => "with_columns",
            Plan::Rename { .. } => "rename",
            Plan::Sort { .. } => "sort",
            Plan::Head { .. } => "head",
            Plan::Sample { .. } => "sample",
            Plan::Distinct { .. } => "distinct",
            Plan::Join { .. } => "join",
            Plan::GroupByAgg { .. } => "group_by_agg",
            Plan::Nodes { .. } => "nodes",
            Plan::Reverse { .. } => "reverse",
            Plan::Hop { .. } => "hop",
            Plan::ShortestPath { .. } => "shortest_path",
            Plan::RandomWalk { .. } => "random_walk",
            Plan::Describe { .. } => "describe",
        }
    }

    /// The node's primary input, `None` for a leaf (a source, or a graph node
    /// whose edges are an [`EdgeInput`] rather than an input).
    pub fn input(&self) -> Option<&Plan> {
        match self {
            Plan::Filter { input, .. }
            | Plan::Select { input, .. }
            | Plan::WithColumns { input, .. }
            | Plan::Rename { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Head { input, .. }
            | Plan::Sample { input, .. }
            | Plan::Distinct { input, .. }
            | Plan::Join { input, .. }
            | Plan::GroupByAgg { input, .. }
            | Plan::Nodes { input }
            | Plan::Reverse { input } => Some(input),
            _ => None,
        }
    }

    /// This plan's steps from its leaf up to itself, following primary inputs: a
    /// frame's steps in the order they were written.
    pub fn chain(&self) -> Vec<&Plan> {
        let mut chain = vec![self];
        while let Some(input) = chain.last().and_then(|p| p.input()) {
            chain.push(input);
        }
        chain.reverse();
        chain
    }

    /// The host's handle for the step a relational node came from.
    pub fn step(&self) -> Option<Slot> {
        match self {
            Plan::Filter { step, .. }
            | Plan::Select { step, .. }
            | Plan::Rename { step, .. }
            | Plan::Sort { step, .. }
            | Plan::Head { step, .. }
            | Plan::Sample { step, .. }
            | Plan::Distinct { step, .. }
            | Plan::GroupByAgg { step, .. } => *step,
            _ => None,
        }
    }

    /// Parse a plan from its JSON. Plans nest one level per step, so the parser's
    /// recursion limit is lifted (the serializer's own limit is lower).
    pub fn from_json(json: &str) -> PlanResult<Plan> {
        let mut de = serde_json::Deserializer::from_str(json);
        de.disable_recursion_limit();
        let plan = Plan::deserialize(&mut de).map_err(|e| PlanError::Invalid(e.to_string()))?;
        de.end().map_err(|e| PlanError::Invalid(e.to_string()))?;
        Ok(plan)
    }
}

/// A built graph index: the CSR topology, the id map, and the weight vectors
/// evaluated over its edge rows. The host caches one per edge frame.
#[derive(Clone)]
pub struct GraphIndex {
    pub topo: Arc<Topology>,
    pub ids: Arc<IdMap>,
    pub weights: Arc<WeightCache>,
}

impl GraphIndex {
    pub fn new(topo: Arc<Topology>, ids: Arc<IdMap>) -> Self {
        let weights = Arc::default();
        GraphIndex { topo, ids, weights }
    }
}

/// Role name → column name: what `ur.src()` / `ur.dst()` / `ur.id()` resolve to.
pub type Roles = [(&'static str, String)];

/// What [`execute_plan`] reads from its host, by slot. Every method is called at
/// most when the executor needs the value, in the order the executor needs it.
///
/// Row-valued results are batch lists of at least one batch, so an empty result
/// still carries its schema.
pub trait Inputs {
    /// The cached graph index of the edge frame `frame` (built on first use).
    fn index(&self, frame: Slot) -> PlanResult<GraphIndex>;

    /// The subgraph-view mask of `frame`, one entry per edge row of its index
    /// (`n_edges`), or `None` for the full graph.
    fn edge_mask(&self, frame: Slot, n_edges: usize) -> PlanResult<Option<Arc<EdgeMask>>>;

    /// The edge rows holding `columns`, aligned with the index's edge rows, for
    /// evaluating weights; `None` when every weight in `keys` (serialized weight
    /// expressions) is already cached on the frame's index.
    fn edge_weights(
        &self,
        frame: Slot,
        columns: &[String],
        keys: &[String],
    ) -> PlanResult<Option<Vec<RecordBatch>>>;

    /// The rows of the node source `frame`, reading at least `columns` (`None`:
    /// every column).
    fn node_table(&self, frame: Slot, columns: Option<&[String]>) -> PlanResult<Vec<RecordBatch>>;

    /// The rows of the edge source `frame`, reading at least `columns` (`None`:
    /// every column).
    fn edge_table(&self, frame: Slot, columns: Option<&[String]>) -> PlanResult<Vec<RecordBatch>>;

    /// A seed set: user ids (int64 or string).
    fn seeds(&self, slot: Slot) -> PlanResult<ArrayRef>;

    /// Run relational steps on `rows`, in order, with `roles` resolving role
    /// references. These are the steps the engine's tail can't run as written
    /// (and every step on a plain source), each identified by its `step` handle.
    fn apply_steps(
        &self,
        rows: Vec<RecordBatch>,
        steps: &[&Plan],
        roles: &Roles,
    ) -> PlanResult<Vec<RecordBatch>>;
}

/// An error from planning or execution.
#[derive(Debug)]
pub enum PlanError {
    /// An unsupported plan shape (Python's `NotImplementedError`).
    NotImplemented(String),
    /// A column the plan references doesn't exist.
    ColumnNotFound(String),
    /// An invalid argument (Python's `ValueError`).
    Value(String),
    /// A malformed plan.
    Invalid(String),
    /// An engine error from an executor.
    Engine(DataFusionError),
    /// An error the host raised while resolving an input.
    Host(HostError),
}

/// An error raised by the host, carried through unchanged.
pub struct HostError {
    pub message: String,
    /// Whether the host classes it as "not implemented".
    pub not_implemented: bool,
    /// The host's own error value, for the host to restore.
    pub payload: Box<dyn Any + Send + Sync>,
}

impl fmt::Debug for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostError")
            .field("message", &self.message)
            .field("not_implemented", &self.not_implemented)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::NotImplemented(m) | PlanError::ColumnNotFound(m) | PlanError::Value(m) => {
                f.write_str(m)
            }
            PlanError::Invalid(m) => write!(f, "invalid plan: {m}"),
            PlanError::Engine(e) => write!(f, "{e}"),
            PlanError::Host(e) => f.write_str(&e.message),
        }
    }
}

impl std::error::Error for PlanError {}

impl From<DataFusionError> for PlanError {
    fn from(e: DataFusionError) -> Self {
        PlanError::Engine(e)
    }
}

pub type PlanResult<T> = std::result::Result<T, PlanError>;

/// Execute a plan, reading its inputs from `inputs`. Returns at least one batch.
pub fn execute_plan(plan: &Plan, inputs: &dyn Inputs) -> PlanResult<Vec<RecordBatch>> {
    let chain = plan.chain();
    if is_node_frame(&chain) {
        collect_node_frame(&chain, inputs)
    } else {
        collect_edge_frame(&chain, inputs)
    }
}

/// The graph index an edge input runs over, after the checks a graph op over it
/// needs. For a traversal result that is its input's index (the traversal only
/// masks it), so its input is checked too unless the host builds that from a
/// single-path scan.
pub fn require_index(edges: &EdgeInput, inputs: &dyn Inputs) -> PlanResult<GraphIndex> {
    edges.view()?;
    if let Some(Plan::Hop { edges: parent, .. } | Plan::ShortestPath { edges: parent, .. }) =
        edges.traversal()
    {
        let single_path_scan = matches!(
            parent.plan.chain()[0],
            Plan::ScanEdges {
                path: Some(ScanPath::One(_)),
                ..
            }
        );
        if !single_path_scan {
            parent.view()?;
        }
    }
    inputs.index(edges.frame)
}

// --- frame shape -------------------------------------------------------------

fn is_node_frame(chain: &[&Plan]) -> bool {
    chain.iter().any(|p| matches!(p, Plan::Nodes { .. }))
        || matches!(
            chain[0],
            Plan::ScanNodes { .. }
                | Plan::FromArrow { id: Some(_), .. }
                | Plan::RandomWalk { .. }
                | Plan::Describe { .. }
        )
}

/// A node frame's id column.
fn node_id_col(chain: &[&Plan]) -> String {
    if chain.iter().any(|p| matches!(p, Plan::Nodes { .. })) {
        return "id".into();
    }
    match chain[0] {
        Plan::ScanNodes { id, .. } | Plan::FromArrow { id: Some(id), .. } => id.clone(),
        Plan::RandomWalk { .. } => "node".into(),
        _ => "__summary__".into(),
    }
}

/// An edge frame's `(src, dst)` columns.
fn edge_roles(chain: &[&Plan]) -> (String, String) {
    let (mut src, mut dst) = match chain[0] {
        Plan::ScanEdges { src, dst, .. }
        | Plan::FromArrow {
            src: Some(src),
            dst: Some(dst),
            ..
        } => (src.clone(), dst.clone()),
        _ => ("src".to_string(), "dst".to_string()),
    };
    for p in chain {
        if matches!(p, Plan::Reverse { .. }) {
            std::mem::swap(&mut src, &mut dst);
        }
    }
    (src, dst)
}

/// The host slot of a frame's source.
fn source_slot(chain: &[&Plan]) -> Option<Slot> {
    match chain[0] {
        Plan::ScanEdges { frame, .. }
        | Plan::ScanNodes { frame, .. }
        | Plan::FromArrow { frame, .. } => Some(*frame),
        _ => None,
    }
}

fn not_implemented<T>(msg: impl Into<String>) -> PlanResult<T> {
    Err(PlanError::NotImplemented(msg.into()))
}

fn is_tail(p: &Plan) -> bool {
    TAIL_OPS.contains(&p.op())
}

// --- node frames ---------------------------------------------------------------

/// A node frame's collect (`collect_node_frame`): `describe`, a random walk, or
/// a node query (graph columns over the node set or an attribute table, an
/// optional join, then the relational steps in the order written).
fn collect_node_frame(chain: &[&Plan], inputs: &dyn Inputs) -> PlanResult<Vec<RecordBatch>> {
    let id_col = node_id_col(chain);
    // describe() is a whole-graph summary, computed eagerly off the topology and
    // wrapped as a one-row frame (not a per-node algorithm).
    if let Some(Plan::Describe {
        edges,
        full,
        direction,
    }) = chain.iter().find(|p| matches!(p, Plan::Describe { .. }))
    {
        if chain.iter().any(|p| !matches!(p, Plan::Describe { .. })) {
            return not_implemented(
                "describe() output does not support a filter/sort/head tail yet; \
                 call describe() as the final step of the pipeline.",
            );
        }
        let index = require_index(edges, inputs)?;
        let dir = parse_stat_direction(direction)?;
        return Ok(vec![describe(&index.topo, *full, dir)?]);
    }

    if let Some(walk) = chain.iter().find(|p| matches!(p, Plan::RandomWalk { .. })) {
        return collect_random_walk(chain, walk, inputs);
    }

    let mut graph_exprs: Option<&[NamedExpr]> = None;
    // The relational steps, in the order written. A join splits them: the steps
    // written before it shape the left side, those after it apply to the joined rows.
    let mut steps: Vec<&Plan> = vec![];
    let mut pre_join: Vec<&Plan> = vec![];
    let mut join: Option<&Plan> = None;
    let mut pre_nodes: Vec<&str> = vec![]; // non-filter edge-plan steps before `nodes`
                                           // A `nodes()`-derived frame inherits the edge plan: its steps before the `nodes`
                                           // boundary are edge-plan steps (a `filter` there is a subgraph view, read as the
                                           // edge mask), not node-result operators.
    let mut past_nodes_boundary = !chain.iter().any(|p| matches!(p, Plan::Nodes { .. }));

    for &step in chain {
        let op = step.op();
        if matches!(
            op,
            "scan_edges"
                | "scan_nodes"
                | "nodes"
                | "from_arrow"
                | "reverse"
                | "hop"
                | "shortest_path"
        ) {
            if op == "nodes" {
                past_nodes_boundary = true;
            }
            continue; // source / metadata steps
        }
        if !past_nodes_boundary && (is_tail(step) || op == "join") {
            if op != "filter" {
                pre_nodes.push(op);
            }
            continue;
        }
        if let Plan::Join { how, .. } = step {
            if join.is_some() {
                return not_implemented("collect() supports a single join() for now.");
            }
            check_join_how(how)?;
            join = Some(step);
            pre_join = std::mem::take(&mut steps);
            continue;
        }
        if let Plan::WithColumns { columns, .. } = step {
            if graph_exprs.is_some() {
                return not_implemented(
                    "collect() supports a single with_columns step in a composed pipeline for now.",
                );
            }
            graph_exprs = Some(columns);
            check_with_columns_position(&steps, join.is_some(), columns, &id_col)?;
            continue;
        }
        if !is_tail(step) {
            return not_implemented(format!(
                "collect() does not yet support the '{op}' step in a composed pipeline."
            ));
        }
        steps.push(step);
    }

    if !pre_nodes.is_empty() {
        // A graph op over that derived edge frame raises the precise guard;
        // otherwise the node set would silently ignore the step.
        if let Some(exprs) = graph_exprs.filter(|g| !g.is_empty()) {
            single_edges(exprs)?.view()?;
        }
        let mut seen = HashSet::new();
        let ops: Vec<String> = pre_nodes
            .into_iter()
            .filter(|op| seen.insert(*op))
            .map(|op| format!("{op}()"))
            .collect();
        return not_implemented(format!(
            "{} on the edges before nodes() is not supported yet; collect the edge frame \
             and re-ingest it via ur.from_arrow(...) first.",
            ops.join("/")
        ));
    }

    if let Some(join) = join {
        // The left side is this frame with the steps written before the join; the
        // steps after it apply to its rows. ur.id() after the join is the real id.
        let left = collect_node_query(chain, &id_col, graph_exprs, &pre_join, inputs)?;
        return run_join(left, join, &steps, &[("id", id_col)], inputs);
    }
    collect_node_query(chain, &id_col, graph_exprs, &steps, inputs)
}

/// `with_columns` computes the graph columns on the frame's base rows, so a
/// relational step written before it would otherwise silently move after it. Only
/// a filter that doesn't read a computed column commutes with it.
fn check_with_columns_position(
    steps: &[&Plan],
    joined: bool,
    graph_exprs: &[NamedExpr],
    id_col: &str,
) -> PlanResult<()> {
    if joined {
        return not_implemented(
            "with_columns() after join() is not supported yet; compute the columns \
             first, then join().",
        );
    }
    let computed: HashSet<&str> = graph_exprs.iter().map(|e| e.name.as_str()).collect();
    for step in steps {
        let Plan::Filter { predicate, .. } = step else {
            let verb = match step.op() {
                "group_by_agg" => "group_by().agg",
                op => op,
            };
            return not_implemented(format!(
                "with_columns() after {verb}() is not supported; compute columns first, \
                 then {verb}()."
            ));
        };
        let clash: BTreeSet<String> = expr_columns(predicate, &[("id", id_col.to_string())])
            .into_iter()
            .filter(|c| computed.contains(c.as_str()))
            .collect();
        if !clash.is_empty() {
            return not_implemented(format!(
                "filter() before with_columns() references column(s) {} that \
                 with_columns() computes; filter after the with_columns().",
                py_list(clash.iter())
            ));
        }
    }
    Ok(())
}

/// The one edge input every graph expression of a `with_columns` shares.
fn single_edges(exprs: &[NamedExpr]) -> PlanResult<&EdgeInput> {
    let mut edges: Option<&EdgeInput> = None;
    for e in exprs {
        let Expr::Graph(verb) = &e.expr else {
            return not_implemented(NON_GRAPH_COLUMN);
        };
        let Some(this) = &verb.edges else {
            return not_implemented(NON_GRAPH_COLUMN);
        };
        match edges {
            None => edges = Some(this),
            Some(first) if first.frame != this.frame => {
                return not_implemented(
                    "collect() requires every algorithm in with_columns to run over the \
                     same edges frame for now.",
                )
            }
            Some(_) => {}
        }
    }
    edges.ok_or_else(|| PlanError::NotImplemented(NON_GRAPH_COLUMN.into()))
}

const NON_GRAPH_COLUMN: &str = "collect() supports with_columns of graph algorithms only; \
     non-graph expressions are not executable yet.";

/// The node query (`_collect_node_query`): a plain attribute table, or the graph
/// columns (attached to the attribute table, if any), then `steps` in order.
fn collect_node_query(
    chain: &[&Plan],
    id_col: &str,
    graph_exprs: Option<&[NamedExpr]>,
    steps: &[&Plan],
    inputs: &dyn Inputs,
) -> PlanResult<Vec<RecordBatch>> {
    let Some(graph_exprs) = graph_exprs.filter(|g| !g.is_empty()) else {
        return collect_plain_nodes(chain, id_col, steps, inputs);
    };
    let edges = single_edges(graph_exprs)?;
    let mut columns: Vec<NodeColumn> = Vec::with_capacity(graph_exprs.len());
    for e in graph_exprs {
        let Expr::Graph(verb) = &e.expr else {
            unreachable!("single_edges admits graph expressions only")
        };
        check_verb(verb)?;
        columns.push((e.name.clone(), verb.as_ref().clone()));
    }
    // Scan projection pushdown: the file must supply the join id, any column a
    // neighbour aggregation reads, and whatever the steps read before a select or
    // group_by narrows them. The engine's sample orders by every input column, so
    // narrowing before a sample would change its picks: that reads the whole file.
    let mut required: BTreeSet<String> = BTreeSet::from([id_col.to_string()]);
    for (_, verb) in &columns {
        if let Some(c) = verb.agg_column() {
            required.insert(c.to_string());
        }
    }
    let computed: HashSet<&str> = graph_exprs.iter().map(|e| e.name.as_str()).collect();
    let file_needed = projection(
        steps,
        required,
        &[("id", id_col.to_string())],
        &computed,
        true,
    );
    // An attribute table's columns join onto the algorithm outputs by id;
    // `edges.nodes()`-derived frames have none.
    let nodes = match node_source_slot(chain) {
        Some(slot) => Some(inputs.node_table(slot, file_needed.as_deref())?),
        None => None,
    };
    let nodes_id = nodes.as_ref().map(|_| id_col.to_string());
    // A weighted algorithm needs the edge columns its weight expression reads. The
    // host skips the read when the index already caches every one of these weights
    // (and, for a scan, builds the index from the same read).
    let mut weight_cols: BTreeSet<String> = BTreeSet::new();
    let mut weight_keys: Vec<String> = vec![];
    for (_, verb) in &columns {
        if let Some(w) = &verb.weight {
            weight_cols.extend(value_columns(w, &[]));
            weight_keys.push(w.to_string());
        }
    }
    let edge_attr = if weight_cols.is_empty() {
        None
    } else {
        prepare_weighted(edges, &weight_cols, &weight_keys, inputs)?
    };
    // The engine runs the leading steps its fixed-order tail executes identically;
    // any later step runs in written order on its output. ur.id() resolves to the
    // result's reserved `id` column.
    let (pushed, rest) = split_engine_tail(steps, true);
    let index = require_index(edges, inputs)?;
    let mask = edge_mask(edges, &index, inputs)?;
    let roles = [("id", "id".to_string())];
    let filters = filter_jsons(&pushed.filters, &roles)?;
    let (group_keys, aggs) = pushed.group_by.clone().unwrap_or_default();
    let result = execute_node_query_cached(
        index.topo,
        index.ids,
        &columns,
        &filters,
        pushed.sort.clone(),
        pushed.limit,
        nodes,
        nodes_id,
        edge_attr,
        pushed.distinct,
        pushed.sample,
        pushed.rename.clone(),
        group_keys,
        aggs,
        mask,
        Some(&index.weights),
    )?;
    finish(result, &pushed, &rest, &roles, inputs)
}

/// The host slot of a node source (`None` for a `nodes()`-derived frame).
fn node_source_slot(chain: &[&Plan]) -> Option<Slot> {
    match chain[0] {
        Plan::ScanNodes { frame, .. }
        | Plan::FromArrow {
            frame, id: Some(_), ..
        } => Some(*frame),
        _ => None,
    }
}

/// The checks `_algo_column` ran on each graph column before it reached the engine.
fn check_verb(verb: &GraphVerb) -> PlanResult<()> {
    let name = verb.verb.as_str();
    if !EXECUTABLE.contains(&name) {
        return not_implemented(format!(
            "the '{name}' kernel is not wired into the execution path yet (wired: {}).",
            EXECUTABLE.join(", ")
        ));
    }
    if name == "connected_components" {
        let mode = verb.mode.clone().unwrap_or_else(|| Value::from("weak"));
        if !matches!(mode.as_str(), Some("weak" | "strong")) {
            return not_implemented(format!(
                "connected_components(mode={}) is not supported; use mode='weak' or \
                 mode='strong'.",
                py_value(&mode)
            ));
        }
    }
    if name == "neighbors_agg" {
        if verb.from == Some(true) {
            return not_implemented(
                "neighbors(from_=...) is not supported yet; the aggregation resolves \
                 against the frame it runs in.",
            );
        }
        let is_agg = verb.agg.as_ref().is_some_and(|a| kind(a) == "agg");
        if !is_agg || verb.agg_column().is_none() {
            return not_implemented(
                "neighbors().agg() supports ur.col(<name>).<fn>() with fn in \
                 mean/sum/min/max/count/n_unique (mean/sum/min/max need a numeric \
                 attribute column; count/n_unique also accept strings).",
            );
        }
    }
    // A weight= on a verb that doesn't consume it would be silently ignored.
    let weighted = ["pagerank", "closeness", "betweenness", "louvain"];
    if verb.weight.is_some() && !weighted.contains(&name) {
        return not_implemented(format!("weight= is not supported for '{name}'."));
    }
    // Output dtype narrowing (#117). Positions are f32 at the source, so dtype has
    // no meaning on a layout; integer kernels have no f32 form.
    let dtype = verb.dtype.as_ref().filter(|d| !d.is_null());
    if MULTI_OUTPUT_VERBS.contains(&name) && dtype.is_some() {
        return not_implemented(format!(
            "'{name}' already emits f32 positions; dtype= has nothing to narrow."
        ));
    }
    if let Some(dtype) = dtype.filter(|d| d.as_str() != Some("f64")) {
        if dtype.as_str() != Some("f32") {
            return not_implemented(format!(
                "dtype must be 'f32' or 'f64'; got {}.",
                py_value(dtype)
            ));
        }
        if !FLOAT_VALUED_VERBS.contains(&name) {
            return not_implemented(format!(
                "dtype='f32' is only supported for float-valued kernels ({}); '{name}' \
                 emits integers.",
                FLOAT_VALUED_VERBS.join(", ")
            ));
        }
    }
    Ok(())
}

/// The graph verbs wired into execution, sorted.
const EXECUTABLE: [&str; 13] = [
    "betweenness",
    "closeness",
    "clustering_coefficient",
    "connected_components",
    "degree",
    "label_propagation",
    "layout_circle",
    "layout_fr",
    "layout_random",
    "louvain",
    "neighbors_agg",
    "pagerank",
    "triangle_count",
];

/// Kernels that emit more than one column from one run (#115).
const MULTI_OUTPUT_VERBS: [&str; 3] = ["layout_circle", "layout_fr", "layout_random"];

/// The kernels whose output is floating-point (the only ones `dtype="f32"` fits),
/// sorted.
const FLOAT_VALUED_VERBS: [&str; 5] = [
    "betweenness",
    "closeness",
    "clustering_coefficient",
    "neighbors_agg",
    "pagerank",
];

fn prepare_weighted(
    edges: &EdgeInput,
    columns: &BTreeSet<String>,
    keys: &[String],
    inputs: &dyn Inputs,
) -> PlanResult<Option<Vec<RecordBatch>>> {
    // Checked before the host reads anything: for a scan the read also builds the
    // index, which must never happen for an edge plan a graph op can't run over.
    edges.view()?;
    let columns: Vec<String> = columns.iter().cloned().collect();
    inputs.edge_weights(edges.frame, &columns, keys)
}

fn edge_mask(
    edges: &EdgeInput,
    index: &GraphIndex,
    inputs: &dyn Inputs,
) -> PlanResult<Option<Arc<EdgeMask>>> {
    if edges.view()?.has_mask() {
        inputs.edge_mask(edges.frame, index.topo.n_edges())
    } else {
        Ok(None)
    }
}

/// A plain node source and its steps, all run by the host (`_collect_plain_nodes`).
fn collect_plain_nodes(
    chain: &[&Plan],
    id_col: &str,
    steps: &[&Plan],
    inputs: &dyn Inputs,
) -> PlanResult<Vec<RecordBatch>> {
    let roles = [("id", id_col.to_string())];
    // A scan reads the id plus what the steps read before a select or group_by
    // narrows the columns.
    let needed = projection(
        steps,
        BTreeSet::from([id_col.to_string()]),
        &roles,
        &HashSet::new(),
        false,
    );
    let Some(slot) = node_source_slot(chain) else {
        return not_implemented(
            "collect() on this NodeFrame needs a source (scan_nodes / from_arrow(id=...)) \
             or a with_columns of graph algorithms; a bare edges.nodes() has no \
             materializable node set yet.",
        );
    };
    let table = inputs.node_table(slot, needed.as_deref())?;
    apply_steps(table, steps, &roles, inputs)
}

/// A random walk and its relational steps (`_collect_random_walk`).
fn collect_random_walk(
    chain: &[&Plan],
    walk: &Plan,
    inputs: &dyn Inputs,
) -> PlanResult<Vec<RecordBatch>> {
    let Plan::RandomWalk {
        edges,
        start,
        steps: n_steps,
        walks_per_node,
        seed,
    } = walk
    else {
        unreachable!("collect_random_walk takes a random_walk step")
    };
    let mut steps: Vec<&Plan> = vec![];
    for &s in chain {
        let op = s.op();
        if matches!(op, "random_walk" | "nodes" | "from_arrow" | "scan_nodes") {
            continue;
        }
        if !is_tail(s) {
            return not_implemented(format!(
                "collect() does not yet support the '{op}' step after a random_walk."
            ));
        }
        steps.push(s);
    }
    let index = require_index(edges, inputs)?;
    let starts = inputs.seeds(*start)?;
    let (pushed, rest) = split_engine_tail(&steps, false);
    // Walk output columns are (walk_id, step, node); no role applies.
    let filters = filter_jsons(&pushed.filters, &[])?;
    let result = execute_walk_query(
        index.topo,
        index.ids,
        starts.as_ref(),
        *n_steps,
        *walks_per_node,
        *seed,
        &filters,
        pushed.sort.clone(),
        pushed.limit,
        pushed.distinct,
        pushed.sample,
        pushed.rename.clone(),
    )?;
    finish(result, &pushed, &rest, &[], inputs)
}

// --- edge frames ---------------------------------------------------------------

/// An edge frame's collect (`collect_edge_frame`): a plain edge frame, or a
/// `hop` / `shortest_path` result, plus its relational steps in the order written.
fn collect_edge_frame(chain: &[&Plan], inputs: &dyn Inputs) -> PlanResult<Vec<RecordBatch>> {
    let (src_col, dst_col) = edge_roles(chain);
    let mut traversal: Option<&Plan> = None;
    let mut steps: Vec<&Plan> = vec![];
    let mut pre_join: Vec<&Plan> = vec![];
    let mut join: Option<&Plan> = None;

    for &step in chain {
        let op = step.op();
        match step {
            Plan::Hop { .. } | Plan::ShortestPath { .. } => traversal = Some(step),
            Plan::ScanEdges { .. } | Plan::FromArrow { .. } | Plan::Nodes { .. } => {}
            Plan::Join { how, .. } => {
                if join.is_some() {
                    return not_implemented("collect() supports a single join() for now.");
                }
                check_join_how(how)?;
                join = Some(step);
                pre_join = std::mem::take(&mut steps);
            }
            Plan::Reverse { .. } => {
                // After a traversal its semantics are undesigned.
                if traversal.is_some() {
                    return not_implemented(
                        "reverse() after a traversal (hop/shortest_path) is not supported; \
                         reverse the edges before the traversal.",
                    );
                }
            }
            _ if is_tail(step) => steps.push(step),
            _ => {
                return not_implemented(format!(
                    "collect() does not yet support the '{op}' step on an edge frame."
                ))
            }
        }
    }

    let roles = [("src", src_col), ("dst", dst_col)];
    if let Some(join) = join {
        if traversal.is_some() {
            return not_implemented(
                "join() on a traversal result (hop/shortest_path) is not supported yet; \
                 join is available on plain edge/node frames.",
            );
        }
        let left = collect_plain_edges(chain, &pre_join, &roles, inputs)?;
        return run_join(left, join, &steps, &roles, inputs);
    }

    let Some(traversal) = traversal else {
        return collect_plain_edges(chain, &steps, &roles, inputs);
    };

    let (Plan::Hop { edges, .. } | Plan::ShortestPath { edges, .. }) = traversal else {
        unreachable!("a traversal is a hop or a shortest_path")
    };
    // A traversal over a traversal result has no topology to run on: the inner
    // result's edges are synthetic (seed, reached) pairs, not the parent graph.
    if edges.traversal().is_some() {
        return not_implemented(
            "chaining a traversal (hop/shortest_path) off a traversal result isn't \
             supported — the inner result's edges are synthetic reached-pairs, not the \
             parent graph. Collect it and re-ingest via ur.from_arrow(...) to traverse \
             further.",
        );
    }

    // A traversal result carries reserved `src`/`dst` columns.
    let roles = [("src", "src".to_string()), ("dst", "dst".to_string())];
    let (pushed, rest) = split_engine_tail(&steps, false);
    let filters = filter_jsons(&pushed.filters, &roles)?;
    let result = match traversal {
        Plan::Hop {
            edges,
            n,
            direction,
            seeds,
        } => {
            let index = require_index(edges, inputs)?;
            let seeds = inputs.seeds(*seeds)?;
            execute_hop_query(
                index.topo,
                index.ids,
                seeds.as_ref(),
                *n,
                direction,
                &filters,
                pushed.sort.clone(),
                pushed.limit,
                pushed.distinct,
                pushed.sample,
                pushed.rename.clone(),
            )?
        }
        Plan::ShortestPath {
            edges,
            source,
            target,
            weight,
            direction,
        } => {
            // A weight= expression over edge columns selects weighted Dijkstra.
            // Its columns are read first: for a scan with no index yet, that read
            // builds the index too.
            let mut edge_attr = None;
            let weight_json = weight.as_ref().map(Value::to_string);
            if let Some(w) = weight {
                let cols: BTreeSet<String> = value_columns(w, &[]);
                if !cols.is_empty() {
                    edge_attr = prepare_weighted(edges, &cols, &[], inputs)?;
                }
            }
            let index = require_index(edges, inputs)?;
            let source = inputs.seeds(*source)?;
            let target = inputs.seeds(*target)?;
            execute_path_query(
                index.topo,
                index.ids,
                source.as_ref(),
                target.as_ref(),
                direction,
                weight_json.as_deref(),
                edge_attr,
                &filters,
                pushed.sort.clone(),
                pushed.limit,
                pushed.distinct,
                pushed.sample,
                pushed.rename.clone(),
            )?
        }
        _ => unreachable!("a traversal is a hop or a shortest_path"),
    };
    finish(result, &pushed, &rest, &roles, inputs)
}

/// A plain edge source and its steps, all run by the host (`_collect_plain_edges`).
fn collect_plain_edges(
    chain: &[&Plan],
    steps: &[&Plan],
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<Vec<RecordBatch>> {
    // The endpoints plus what the steps read before a select or group_by narrows
    // the columns.
    let required: BTreeSet<String> = roles.iter().map(|(_, c)| c.clone()).collect();
    let needed = projection(steps, required, roles, &HashSet::new(), false);
    let Some(slot) = source_slot(chain) else {
        return not_implemented(
            "collect() needs edges from ur.from_arrow(...), ur.from_polars(...), or \
             ur.scan_edges(<.parquet|.csv>); this frame has no resolvable source.",
        );
    };
    let table = inputs.edge_table(slot, needed.as_deref())?;
    apply_steps(table, steps, roles, inputs)
}

// --- join --------------------------------------------------------------------

fn check_join_how(how: &str) -> PlanResult<()> {
    if matches!(how, "inner" | "left") {
        return Ok(());
    }
    not_implemented(format!(
        "join how={} is not supported yet (use ['inner', 'left']); 'right'/'outer' are \
         a follow-up.",
        py_str(how)
    ))
}

/// Materialize the right side, check the keys and column collisions, and run the
/// join, then `steps` (written after it) in order (`_run_join`).
fn run_join(
    left: Vec<RecordBatch>,
    join: &Plan,
    steps: &[&Plan],
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<Vec<RecordBatch>> {
    let Plan::Join { right, on, how, .. } = join else {
        unreachable!("run_join takes a join step")
    };
    let right = execute_plan(right, inputs).map_err(|e| match e {
        PlanError::NotImplemented(msg)
        | PlanError::Host(HostError {
            message: msg,
            not_implemented: true,
            ..
        }) => PlanError::NotImplemented(format!(
            "join(other): could not materialize the right-hand frame ({msg})."
        )),
        e => e,
    })?;
    let names = |b: &[RecordBatch]| -> Vec<String> {
        b[0].schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    };
    let (left_cols, right_cols) = (names(&left), names(&right));
    for k in on {
        if !left_cols.contains(k) {
            return Err(PlanError::ColumnNotFound(format!(
                "join(on=): key {} not in the left frame {}.",
                py_str(k),
                py_list(left_cols.iter())
            )));
        }
        if !right_cols.contains(k) {
            return Err(PlanError::ColumnNotFound(format!(
                "join(on=): key {} not in the right frame {}.",
                py_str(k),
                py_list(right_cols.iter())
            )));
        }
    }
    // A same-named non-key column on both sides would collide in the output.
    let right_set: HashSet<&String> = right_cols.iter().filter(|c| !on.contains(c)).collect();
    let collide: BTreeSet<&String> = left_cols
        .iter()
        .filter(|c| !on.contains(c) && right_set.contains(c))
        .collect();
    if !collide.is_empty() {
        return Err(PlanError::Value(format!(
            "join produces duplicate column name(s) {}; rename() one side before joining.",
            py_list(collide.into_iter())
        )));
    }
    let (pushed, rest) = split_engine_tail(steps, false);
    let filters = filter_jsons(&pushed.filters, roles)?;
    let result = execute_join_query(
        left,
        right,
        on.clone(),
        how.clone(),
        &filters,
        pushed.sort.clone(),
        pushed.limit,
        pushed.distinct,
        pushed.sample,
        pushed.rename.clone(),
    )?;
    finish(result, &pushed, &rest, roles, inputs)
}

// --- the engine tail and the host's steps ------------------------------------

/// The slots of the engine's fixed-order tail (`apply_tail` in [`crate::query`]),
/// plus the `select` applied to its output.
#[derive(Default)]
struct EngineTail<'a> {
    filters: Vec<&'a Value>,
    group_by: Option<(Vec<String>, Vec<String>)>,
    distinct: bool,
    sample: Option<(usize, Option<u64>)>,
    sort: Option<Vec<(String, bool)>>,
    limit: Option<usize>,
    rename: Vec<(String, String)>,
    select: Option<&'a Plan>,
}

/// The engine's tail order after its filters, then the select on its output.
const ENGINE_ORDER: [&str; 7] = [
    "group_by_agg",
    "distinct",
    "sample",
    "sort",
    "head",
    "rename",
    "select",
];

/// Split written-order `steps` into the longest prefix the engine's fixed-order
/// tail (`filters → group_by → distinct → sample → sort → limit → rename`, then
/// `select`) runs exactly as written, and the rest, which the host runs in order
/// on the engine's output.
///
/// Each slot fills at most once, in engine order. A `filter` may still join the
/// filters after a `sort` or `distinct` (it commutes with both) but not after a
/// step that changes the rows or the column names. `group_by = false` for the
/// executors without a group slot. A rename whose targets are also sources (a swap
/// or chain) stays out: the engine relabels pair by pair, the host all at once.
fn split_engine_tail<'a>(steps: &[&'a Plan], group_by: bool) -> (EngineTail<'a>, Vec<&'a Plan>) {
    let mut out = EngineTail::default();
    let mut filled: HashSet<&str> = HashSet::new();
    let mut pos = 0; // the earliest ENGINE_ORDER slot still fillable
    for (i, &step) in steps.iter().enumerate() {
        if let Plan::Filter { predicate, .. } = step {
            if filled.iter().all(|k| matches!(*k, "distinct" | "sort")) {
                let Expr::Scalar(v) = predicate else {
                    return (out, steps[i..].to_vec());
                };
                out.filters.push(v);
                continue;
            }
            return (out, steps[i..].to_vec());
        }
        let op = step.op();
        let k = ENGINE_ORDER.iter().position(|o| *o == op).unwrap_or(0);
        let swap = match step {
            Plan::Rename { mapping, .. } => {
                let targets: HashSet<&String> = mapping.iter().map(|(_, new)| new).collect();
                mapping.iter().any(|(old, _)| targets.contains(old))
            }
            _ => false,
        };
        if k < pos || (op == "group_by_agg" && !group_by) || swap {
            return (out, steps[i..].to_vec());
        }
        match step {
            Plan::GroupByAgg { keys, aggs, .. } => {
                out.group_by = Some((keys.clone(), aggs.iter().map(agg_json).collect()))
            }
            Plan::Distinct { .. } => out.distinct = true,
            Plan::Sample { n, seed, .. } => out.sample = Some((*n, *seed)),
            Plan::Sort { by, .. } => {
                out.sort = Some(
                    by.iter()
                        .map(|k| (k.column.clone(), k.descending))
                        .collect(),
                )
            }
            Plan::Head { n, .. } => out.limit = Some(*n),
            Plan::Rename { mapping, .. } => out.rename = mapping.clone(),
            _ => out.select = Some(step), // select
        }
        filled.insert(op);
        pos = k + 1;
    }
    (out, vec![])
}

/// The engine's JSON for one aggregation: `alias(agg(col))`.
fn agg_json(a: &AggSpec) -> String {
    serde_json::json!({
        "kind": "alias",
        "name": a.output,
        "operand": {"kind": "agg", "fn": a.func, "operand": {"kind": "col", "name": a.column}},
    })
    .to_string()
}

/// Complete an engine result: the pushed `select`, then the remaining steps, in
/// order, on the host (`_finish`).
fn finish(
    result: Vec<RecordBatch>,
    pushed: &EngineTail<'_>,
    rest: &[&Plan],
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<Vec<RecordBatch>> {
    if pushed.select.is_none() && rest.is_empty() {
        return Ok(result);
    }
    let steps: Vec<&Plan> = pushed
        .select
        .into_iter()
        .chain(rest.iter().copied())
        .collect();
    inputs.apply_steps(result, &steps, roles)
}

fn apply_steps(
    rows: Vec<RecordBatch>,
    steps: &[&Plan],
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<Vec<RecordBatch>> {
    if steps.is_empty() {
        return Ok(rows);
    }
    if let Some(step) = steps.iter().find(|s| s.step().is_none()) {
        return Err(PlanError::Invalid(format!(
            "the '{}' step has no host handle, so its host can't run it",
            step.op()
        )));
    }
    inputs.apply_steps(rows, steps, roles)
}

/// The columns a scan must read for `steps` (projection pushdown), or `None` to
/// read them all (`_projection`).
///
/// Walks the steps in written order, tracking renames back to source names, up to
/// the first `select` or `group_by`, the first step that narrows the columns. A
/// `distinct` before that reads every column, and so does a `sample` when
/// `sample_reads_all` (the engine's sample orders by full row content). Without a
/// narrowing step every column reaches the output. `required` is always read;
/// `computed` columns come from the graph op, not the file.
fn projection(
    steps: &[&Plan],
    required: BTreeSet<String>,
    roles: &Roles,
    computed: &HashSet<&str>,
    sample_reads_all: bool,
) -> Option<Vec<String>> {
    // current name -> source column (None: no source column)
    let mut source: HashMap<String, Option<String>> = HashMap::new();
    let mut needed = required;
    let src = |source: &HashMap<String, Option<String>>, names: Vec<String>| -> Vec<String> {
        names
            .into_iter()
            .filter_map(|n| source.get(&n).cloned().unwrap_or(Some(n)))
            .collect()
    };
    let done = |needed: BTreeSet<String>| -> Option<Vec<String>> {
        Some(
            needed
                .into_iter()
                .filter(|c| !computed.contains(c.as_str()))
                .collect(),
        )
    };
    for step in steps {
        match step {
            Plan::Filter { predicate, .. } => {
                let cols = expr_columns(predicate, roles).into_iter().collect();
                needed.extend(src(&source, cols));
            }
            Plan::Sort { by, .. } => {
                let cols = by.iter().map(|k| k.column.clone()).collect();
                needed.extend(src(&source, cols));
            }
            Plan::Rename { mapping, .. } => {
                // A rename reads its sources (an unknown one must still error).
                let olds: BTreeSet<String> = mapping.iter().map(|(o, _)| o.clone()).collect();
                needed.extend(src(&source, olds.into_iter().collect()));
                let relabelled: Vec<(String, Option<String>)> = mapping
                    .iter()
                    .map(|(old, new)| {
                        let s = source.get(old).cloned().unwrap_or(Some(old.clone()));
                        (new.clone(), s)
                    })
                    .collect();
                for (old, _) in mapping {
                    source.insert(old.clone(), None); // the old names are gone
                }
                source.extend(relabelled);
            }
            Plan::Select { columns, .. } => {
                needed.extend(src(&source, columns.clone()));
                return done(needed);
            }
            Plan::GroupByAgg { keys, aggs, .. } => {
                let mut cols: BTreeSet<String> = keys.iter().cloned().collect();
                cols.extend(aggs.iter().map(|a| a.column.clone()));
                needed.extend(src(&source, cols.into_iter().collect()));
                return done(needed);
            }
            Plan::Distinct { .. } => return None,
            Plan::Sample { .. } if sample_reads_all => return None,
            _ => {} // head / (position-based) sample read no column
        }
    }
    None
}

// --- expressions -------------------------------------------------------------

/// The operators valid at the top of a filter predicate.
const PREDICATE_OPS: [&str; 8] = [">", "<", ">=", "<=", "==", "!=", "&", "|"];

/// Serialize the engine filters: each must be a boolean predicate at its top
/// node, with its role references resolved to `roles` (`_filter_json`).
fn filter_jsons(predicates: &[&Value], roles: &Roles) -> PlanResult<Vec<String>> {
    predicates
        .iter()
        .map(|p| {
            require_predicate(p)?;
            Ok(resolve_roles(p, roles)?.to_string())
        })
        .collect()
}

fn require_predicate(p: &Value) -> PlanResult<()> {
    let op = p.get("op").and_then(Value::as_str).unwrap_or("");
    let ok = match kind(p) {
        "binary" => PREDICATE_OPS.contains(&op),
        "unary" => op == "~",
        _ => false,
    };
    if ok {
        return Ok(());
    }
    not_implemented(
        "filter() expects a boolean predicate — a comparison (>, >=, <, <=, ==, !=) or a \
         boolean combination (&, |, ~), e.g. .filter(ur.col('deg') > 2) or \
         .filter((ur.col('a') > 1) & (ur.col('b') == 'x')).",
    )
}

/// An expression with `ur.src()` / `ur.dst()` / `ur.id()` resolved to the columns
/// `roles` names; a role not in `roles` is an error.
fn resolve_roles(v: &Value, roles: &Roles) -> PlanResult<Value> {
    let k = kind(v);
    let mut out = v.clone();
    match k {
        "binary" => {
            for side in ["left", "right"] {
                if let Some(e) = v.get(side) {
                    out[side] = resolve_roles(e, roles)?;
                }
            }
        }
        "unary" | "agg" | "alias" => {
            if let Some(e) = v.get("operand") {
                out["operand"] = resolve_roles(e, roles)?;
            }
        }
        "src" | "dst" | "id" => {
            let Some((_, column)) = roles.iter().find(|(r, _)| *r == k) else {
                let mut names: Vec<&str> = roles.iter().map(|(r, _)| *r).collect();
                names.sort_unstable();
                return not_implemented(format!(
                    "ur.{k}() is not available in this context (resolvable roles here: {}).",
                    py_list(names.into_iter())
                ));
            };
            return Ok(serde_json::json!({"kind": "col", "name": column}));
        }
        _ => {}
    }
    Ok(out)
}

/// The columns an expression reads, role references resolved through `roles`.
fn expr_columns(e: &Expr, roles: &Roles) -> BTreeSet<String> {
    match e {
        Expr::Scalar(v) => value_columns(v, roles),
        Expr::Graph(_) => BTreeSet::new(),
    }
}

fn value_columns(v: &Value, roles: &Roles) -> BTreeSet<String> {
    let k = kind(v);
    let mut out = BTreeSet::new();
    match k {
        "col" => {
            if let Some(name) = v.get("name").and_then(Value::as_str) {
                out.insert(name.to_string());
            }
        }
        "binary" => {
            for side in ["left", "right"] {
                if let Some(e) = v.get(side) {
                    out.extend(value_columns(e, roles));
                }
            }
        }
        "unary" | "agg" | "alias" => {
            if let Some(e) = v.get("operand") {
                out.extend(value_columns(e, roles));
            }
        }
        "src" | "dst" | "id" => {
            if let Some((_, column)) = roles.iter().find(|(r, _)| *r == k) {
                out.insert(column.clone());
            }
        }
        _ => {}
    }
    out
}

// --- Python-compatible message formatting --------------------------------------
//
// The guards keep the messages the Python layer raised, which quote values with
// Python's `repr`.

/// `repr(s)` for a Python `str`.
fn py_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `repr(list_of_str)`.
fn py_list<S: AsRef<str>>(items: impl Iterator<Item = S>) -> String {
    let parts: Vec<String> = items.map(|s| py_str(s.as_ref())).collect();
    format!("[{}]", parts.join(", "))
}

/// `repr(value)` for a JSON-representable Python value.
fn py_value(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => py_str(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(_) => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_reprs() {
        assert_eq!(py_str("a"), "'a'");
        assert_eq!(py_str("it's"), "\"it's\"");
        assert_eq!(py_str("a'\"b"), "'a\\'\"b'");
        assert_eq!(py_list(["dst", "src"].iter()), "['dst', 'src']");
        assert_eq!(py_list(Vec::<String>::new().iter()), "[]");
        assert_eq!(py_value(&serde_json::json!(null)), "None");
        assert_eq!(py_value(&serde_json::json!("x")), "'x'");
    }

    fn plan(json: serde_json::Value) -> Plan {
        Plan::from_json(&json.to_string()).unwrap()
    }

    fn src() -> serde_json::Value {
        serde_json::json!({"op": "from_arrow", "frame": 0, "id": "id"})
    }

    fn steps_of(p: &Plan) -> Vec<&Plan> {
        p.chain().into_iter().skip(1).collect()
    }

    fn col_pred(c: &str) -> serde_json::Value {
        serde_json::json!({"kind": "binary", "op": ">",
            "left": {"kind": "col", "name": c}, "right": {"kind": "lit", "value": 1}})
    }

    #[test]
    fn engine_split_pushes_the_common_tail() {
        let p = plan(serde_json::json!({"op": "head", "n": 5, "input":
            {"op": "filter", "predicate": col_pred("b"), "input":
            {"op": "sort", "by": [{"column": "v", "descending": true}], "input":
            {"op": "filter", "predicate": col_pred("a"), "input": src()}}}}));
        let steps = steps_of(&p);
        let (pushed, rest) = split_engine_tail(&steps, true);
        assert!(rest.is_empty());
        assert_eq!(pushed.filters.len(), 2);
        assert_eq!(pushed.sort, Some(vec![("v".to_string(), true)]));
        assert_eq!(pushed.limit, Some(5));
    }

    #[test]
    fn engine_split_leaves_out_of_order_steps_to_the_remainder() {
        let head = |input| serde_json::json!({"op": "head", "n": 3, "input": input});
        let sort = |c: &str, input| serde_json::json!({"op": "sort", "by": [{"column": c}], "input": input});
        let filter =
            |input| serde_json::json!({"op": "filter", "predicate": col_pred("v"), "input": input});
        let rename = |pairs: serde_json::Value, input| serde_json::json!({"op": "rename", "mapping": pairs, "input": input});
        let select = |input| serde_json::json!({"op": "select", "columns": ["c"], "input": input});
        let distinct = |input| serde_json::json!({"op": "distinct", "input": input});
        let group = |input| serde_json::json!({"op": "group_by_agg", "keys": ["c"], "aggs": [], "input": input});
        // (plan, steps pushed)
        let cases = [
            (sort("v", head(src())), 1),
            (filter(head(src())), 1),
            (sort("w", rename(serde_json::json!([["v", "w"]]), src())), 1),
            (distinct(select(src())), 1),
            (sort("c", sort("v", src())), 1),
            (
                rename(serde_json::json!([["a", "b"], ["b", "a"]]), src()),
                0,
            ),
            (filter(group(src())), 1),
        ];
        for (json, n_pushed) in cases {
            let p = plan(json);
            let steps = steps_of(&p);
            let (_, rest) = split_engine_tail(&steps, true);
            assert_eq!(rest.len(), steps.len() - n_pushed, "{p:?}");
        }
    }

    #[test]
    fn projection_reads_every_sort_key_through_a_rename() {
        // rename(c -> k).sort([k, v]).head(3).select(id): the scan reads c, id, v.
        let p = plan(
            serde_json::json!({"op": "select", "columns": ["id"], "input":
            {"op": "head", "n": 3, "input":
            {"op": "sort", "by": [{"column": "k", "descending": true}, {"column": "v"}], "input":
            {"op": "rename", "mapping": [["c", "k"]], "input": src()}}}}),
        );
        let steps = steps_of(&p);
        let got = projection(
            &steps,
            BTreeSet::from(["id".to_string()]),
            &[("id", "id".to_string())],
            &HashSet::new(),
            false,
        );
        assert_eq!(got, Some(vec!["c".into(), "id".into(), "v".into()]));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err =
            Plan::from_json(r#"{"op": "from_arrow", "frame": 0, "id": "id", "x": 1}"#).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
        let err = Plan::from_json(
            r#"{"op": "with_columns", "input": {"op": "from_arrow", "frame": 0, "id": "id"},
                "columns": [{"name": "d", "expr": {"kind": "graph", "verb": "degree", "bogus": 1}}]}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn deep_plans_parse() {
        let mut json = src();
        for _ in 0..500 {
            json = serde_json::json!({"op": "head", "n": 1, "input": json});
        }
        assert_eq!(plan(json).chain().len(), 501);
    }

    #[test]
    fn edge_view_splits_topology_and_mask() {
        let edges = serde_json::json!({"op": "from_arrow", "frame": 0, "src": "s", "dst": "d"});
        let input: EdgeInput = serde_json::from_value(serde_json::json!({"frame": 1, "plan":
            {"op": "filter", "predicate": col_pred("w"), "input": {"op": "reverse", "input": edges}}}))
        .unwrap();
        let view = input.view().unwrap();
        assert_eq!(view.topology.op(), "reverse");
        assert_eq!(view.predicates.len(), 1);
        assert!(view.induced_by.is_none() && view.has_mask());

        let hop: EdgeInput = serde_json::from_value(serde_json::json!({"frame": 2, "plan":
            {"op": "hop", "n": 1, "direction": "out", "seeds": 0,
             "edges": {"frame": 0, "plan": edges}}}))
        .unwrap();
        let view = hop.view().unwrap();
        assert_eq!(view.topology.op(), "from_arrow");
        assert_eq!(view.induced_by.map(Plan::op), Some("hop"));

        let reshaped: EdgeInput = serde_json::from_value(serde_json::json!({"frame": 3, "plan":
            {"op": "distinct", "input": edges}}))
        .unwrap();
        assert!(matches!(reshaped.view(), Err(PlanError::NotImplemented(_))));
    }
}
