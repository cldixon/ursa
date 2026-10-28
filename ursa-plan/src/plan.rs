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
//! [`execute_plan`] owns the *dispatch* (#190): it walks the tree, rejects the
//! shapes that aren't supported (the [`PlanError::NotImplemented`] guards, with
//! the messages the Python layer used to raise), and builds each frame's leaf
//! from [`crate::query`]: a source's rows, a graph or traversal node.
//!
//! Every relational step is then lowered onto that leaf, in the order written,
//! as stock DataFusion operators ([`crate::lower`], #191): `filter`, `select`,
//! `with_columns`, `rename`, `sort`, `head`, `distinct`, `sample` and
//! `group_by().agg()`, with the order guarantees implemented once there. A step
//! means the same wherever it runs; its semantics and errors are the ones the
//! plain frame always had. A `join` builds its right side the same way, as a
//! frame of its own, and joins the two in one plan ([`crate::lower::join`]).
//!
//! Since #192 it also owns the leaf work around them, with one expression
//! evaluator ([`crate::expr`] lowered to DataFusion, [`crate::eval`] outside a
//! plan):
//!
//! - plain sources: a scan reads the columns the steps need and takes its
//!   leading filters into the read (Parquet row-group pruning); every `filter`
//!   step runs here, wherever it sits;
//! - subgraph-view masks, built in dense id space and cached on the
//!   [`GraphIndex`] ([`MaskCache`]);
//! - the edge columns a weight reads, and the index a weighted scan builds;
//! - a scanned node table, through the frame's [`NodeTableCache`].
//!
//! The host (the Python layer) keeps what belongs to its frames, reached through
//! [`Inputs`] by slot: each frame's source (its in-memory table, or a scan's
//! storage options), its cached index and node-table cache, seed id sets, and the
//! rows a `sample` keeps (drawn with the host's seeded generator).
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
//!   (under any `reverse`). #191 widens it to any subplan (a `distinct`, `join`,
//!   `group_by`, ...), rebuilt from that subplan's output and cached by a
//!   fingerprint of it.
//! - a mask, derived in dense id space over the topology's edge rows: the
//!   `filter` predicates written above the topology (a subgraph view, #114),
//!   ANDed, and for a traversal result the subgraph its reached node set induces
//!   (#116). The view is recursive: a traversal's input is a view of its own, so
//!   the traversal follows only that view's edges and the induced subgraph stays
//!   within it, and each filter is applied exactly once.

use std::any::Any;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use arrow::array::{Array, ArrayRef, RecordBatch};
use datafusion::error::DataFusionError;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use ursa_core::{EdgeMask, IdMap, Topology};

use arrow::datatypes::DataType;
use datafusion::logical_expr::Expr as DfExpr;

use crate::expr::GraphVerb;
use crate::lower::{col_named, Frame, How};
use crate::query::{
    attach_columns, graph_leaf, hop_leaf, node_columns, parse_direction, path_leaf, resolve_dense,
    walk_leaf, NodeColumn,
};
use crate::scan::{scan_edges_batch, scan_edges_rows, NodeTableCache};
use crate::stats::{describe, parse_stat_direction};
use crate::weight::{edge_weights, WeightCache};

/// A numbered input the host resolves: a frame (its table, scan, index, mask) or
/// a seed set.
pub type Slot = usize;

/// A query plan: one node per frame step, each holding its input.
///
/// Relational nodes carry `step`, the host's handle for the frame step they came
/// from: a `sample` draws its rows through it ([`Inputs::sample`]). A step whose
/// arguments the host could not parse is sent with `invalid` set and its
/// arguments left out: the executor raises the host's error
/// ([`Inputs::step_error`]) where it would have read the step, so a guard that
/// fires earlier still wins.
///
/// Integer arguments are [`IntArg`]s, typed only where the engine reads them.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Plan {
    /// `ur.scan_edges(path, src=, dst=)`.
    ScanEdges {
        frame: Slot,
        /// The path(s), when representable (anything else is not a single path).
        /// Storage options stay with the host ([`Inputs::storage_options`]).
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
        #[serde(default)]
        predicate: Expr,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    Select {
        input: Box<Plan>,
        #[serde(default)]
        columns: Vec<SelectItem>,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    WithColumns {
        input: Box<Plan>,
        columns: Vec<NamedExpr>,
    },
    Rename {
        input: Box<Plan>,
        /// `(old, new)` pairs, in the order given.
        #[serde(default)]
        mapping: Vec<(String, String)>,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    Sort {
        input: Box<Plan>,
        #[serde(default)]
        by: Vec<SortKey>,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    Head {
        input: Box<Plan>,
        #[serde(default)]
        n: IntArg,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    Sample {
        input: Box<Plan>,
        #[serde(default)]
        n: IntArg,
        #[serde(default)]
        seed: Option<IntArg>,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    Distinct {
        input: Box<Plan>,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    Join {
        input: Box<Plan>,
        right: Box<Plan>,
        #[serde(default)]
        on: Vec<String>,
        how: String,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    GroupByAgg {
        input: Box<Plan>,
        #[serde(default)]
        keys: Vec<String>,
        #[serde(default)]
        aggs: Vec<AggSpec>,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    /// `edges.nodes()`: the node set of an edge plan.
    Nodes { input: Box<Plan> },
    /// `edges.reverse()`: swap the src/dst roles.
    Reverse { input: Box<Plan> },
    /// `ur.hop(edges, n, direction).from_(seeds)`.
    Hop {
        edges: EdgeInput,
        n: IntArg,
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
        steps: IntArg,
        walks_per_node: IntArg,
        #[serde(default)]
        seed: Option<IntArg>,
    },
    /// `ur.describe(edges, full, direction)`.
    Describe {
        edges: EdgeInput,
        full: bool,
        direction: String,
    },
    /// A frame the host could not serialize (a join's right side). Executing it
    /// raises the host's error for `step`, at the point the frame would have run.
    Invalid { step: Slot },
}

/// A scan path: one path (a glob included) or a list of them.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ScanPath {
    One(String),
    Many(Vec<String>),
}

/// An integer argument, as given. The engine converts it where it reads it, with
/// the errors a Python integer conversion gives (`OverflowError`, `TypeError`).
/// A host value with no JSON form is sent as `{"type": <type name>}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct IntArg(pub Value);

impl IntArg {
    /// The value as an integer, or the `TypeError` for a non-integer.
    fn int(&self, name: &str) -> PlanResult<Option<i128>> {
        let ty = match &self.0 {
            Value::Bool(b) => return Ok(Some(*b as i128)),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    return Ok(Some(i as i128));
                }
                if let Some(u) = n.as_u64() {
                    return Ok(Some(u as i128));
                }
                // An integer past 64 bits arrives as an integral float.
                let f = n.as_f64().unwrap_or(0.0);
                if f.fract() == 0.0 && f.abs() >= 9.2e18 {
                    return Ok(None);
                }
                "float"
            }
            Value::String(_) => "str",
            Value::Null => "NoneType",
            Value::Array(_) => "list",
            Value::Object(o) => o.get("type").and_then(Value::as_str).unwrap_or("object"),
        };
        Err(PlanError::Type(format!(
            "argument '{name}': '{ty}' object cannot be interpreted as an integer"
        )))
    }

    /// As a `u64`/`usize` argument.
    pub fn u64(&self, name: &str) -> PlanResult<u64> {
        match self.int(name)? {
            Some(i) if i < 0 => Err(PlanError::Overflow(
                "can't convert negative int to unsigned".into(),
            )),
            Some(i) if i <= u64::MAX as i128 => Ok(i as u64),
            _ => Err(PlanError::Overflow("int too big to convert".into())),
        }
    }

    pub fn usize(&self, name: &str) -> PlanResult<usize> {
        self.u64(name).map(|v| v as usize)
    }

    /// As a `u32` argument.
    pub fn u32(&self, name: &str) -> PlanResult<u32> {
        match self.int(name)? {
            Some(i) if (0..=u32::MAX as i128).contains(&i) => Ok(i as u32),
            _ => Err(PlanError::Overflow(
                "out of range integral type conversion attempted".into(),
            )),
        }
    }
}

/// An optional seed argument.
fn seed_arg(seed: &Option<IntArg>, name: &str) -> PlanResult<Option<u64>> {
    match seed {
        Some(IntArg(Value::Null)) | None => Ok(None),
        Some(s) => s.u64(name).map(Some),
    }
}

/// One `with_columns` output.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedExpr {
    pub name: String,
    pub expr: Expr,
}

/// One `select` item: a column name, or an expression (its JSON).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SelectItem {
    Name(String),
    Expr(Value),
}

/// One sort key: a column, or an expression (its JSON).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SortKey {
    #[serde(default)]
    pub column: Option<String>,
    #[serde(default)]
    pub expr: Option<Value>,
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
///
/// A graph node with no `verb` (the stub a graph verb serializes to where it
/// isn't a column, e.g. used as a filter predicate) stays a scalar, so the
/// context it is used in rejects it with its own error.
#[derive(Debug, Clone)]
pub enum Expr {
    Graph(Box<GraphVerb>),
    Scalar(Value),
}

impl Default for Expr {
    fn default() -> Self {
        Expr::Scalar(Value::Null)
    }
}

impl<'de> Deserialize<'de> for Expr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        // Only the verb-less stub stays scalar: a malformed graph verb (an
        // unknown parameter, say) is still a hard error.
        if kind(&v) == "graph" && v.get("verb").is_some() {
            return GraphVerb::from_value(&v)
                .map(|g| Expr::Graph(Box::new(g)))
                .map_err(serde::de::Error::custom);
        }
        Ok(Expr::Scalar(v))
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
///
/// The view is recursive: a traversal result's mask is the subgraph its reached
/// node set induces *within the view the traversal ran over*, so
/// `ur.hop(e.filter(a), 2)` follows only the edges `a` keeps, and a graph op over
/// that result keeps them too (#192). Each view takes only the filters written
/// above its topology; the filters of a traversal's input belong to that input's
/// view and are applied once, through it. When #191 lets the topology be any
/// subplan (`filter(a).distinct().filter(b)`), `a` stays with the topology's own
/// rows and the mask takes `b` alone.
#[derive(Debug)]
pub struct EdgeView<'a> {
    /// The host slot of the frame whose rows and index the view masks: the frame
    /// itself, or a traversal result's input frame.
    pub frame: Slot,
    /// Whether that frame has rows of its own (a source under property-only
    /// steps), not a traversal result's synthetic pairs.
    pub sourced: bool,
    /// The plan whose rows define the CSR: a source, under any `reverse`.
    pub topology: &'a Plan,
    /// The `src` / `dst` columns of the topology's rows, as the frame names them.
    pub roles: (String, String),
    /// `filter` predicates over the topology's edge rows, ANDed (#114).
    pub predicates: Vec<&'a Expr>,
    /// A `hop` / `shortest_path` whose reached node set induces the subgraph
    /// (#116), and the view it ran over.
    pub induced_by: Option<Induced<'a>>,
}

/// The traversal behind a traversal-result view.
#[derive(Debug)]
pub struct Induced<'a> {
    /// The `hop` / `shortest_path` node.
    pub traversal: &'a Plan,
    /// The edges it ran over.
    pub edges: &'a EdgeInput,
    /// Their view: its mask restricts the traversal and the induced subgraph.
    pub input: Box<EdgeView<'a>>,
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
        if let Some(t @ (Plan::Hop { edges, .. } | Plan::ShortestPath { edges, .. })) = traversal {
            // The traversal's input is a view of its own (filters included).
            let input = edges.view()?;
            return Ok(EdgeView {
                frame: edges.frame,
                sourced: edges.traversal().is_none(),
                topology: input.topology,
                roles: input.roles.clone(),
                predicates: vec![],
                induced_by: Some(Induced {
                    traversal: t,
                    edges,
                    input: Box::new(input),
                }),
            });
        }
        // The topology: the nearest `reverse` or source below the property-only
        // steps. A `reverse` keeps the rows (and their order), so a filter written
        // below it still filters the same rows.
        let topology = chain
            .iter()
            .rev()
            .copied()
            .find(|p| p.input().is_none() || matches!(p, Plan::Reverse { .. }))
            .unwrap_or(self.plan.as_ref());
        let predicates = chain
            .iter()
            .filter_map(|p| match p {
                Plan::Filter { predicate, .. } => Some(predicate),
                _ => None,
            })
            .collect();
        Ok(EdgeView {
            frame: self.frame,
            sourced: true,
            topology,
            roles: edge_roles(&chain),
            predicates,
            induced_by: None,
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

/// The relational steps [`lower_steps`] lowers.
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
            Plan::Invalid { .. } => "invalid",
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
            | Plan::Join { step, .. }
            | Plan::GroupByAgg { step, .. } => *step,
            Plan::Invalid { step } => Some(*step),
            _ => None,
        }
    }

    /// Whether the host could not parse this step's arguments.
    pub fn invalid(&self) -> bool {
        match self {
            Plan::Filter { invalid, .. }
            | Plan::Select { invalid, .. }
            | Plan::Rename { invalid, .. }
            | Plan::Sort { invalid, .. }
            | Plan::Head { invalid, .. }
            | Plan::Sample { invalid, .. }
            | Plan::Distinct { invalid, .. }
            | Plan::Join { invalid, .. }
            | Plan::GroupByAgg { invalid, .. } => *invalid,
            Plan::Invalid { .. } => true,
            _ => false,
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

/// A built graph index: the CSR topology, the id map, and what is derived from
/// its edge rows and cached with it: the weight vectors evaluated over them, and
/// the subgraph-view masks over them. The host keeps one per edge frame, so both
/// caches live exactly as long as the rows they were computed from.
#[derive(Clone)]
pub struct GraphIndex {
    pub topo: Arc<Topology>,
    pub ids: Arc<IdMap>,
    pub weights: Arc<WeightCache>,
    pub masks: Arc<MaskCache>,
}

impl GraphIndex {
    pub fn new(topo: Arc<Topology>, ids: Arc<IdMap>) -> Self {
        GraphIndex {
            topo,
            ids,
            weights: Arc::default(),
            masks: Arc::default(),
        }
    }
}

/// How many subgraph-view masks a [`MaskCache`] keeps (1 bit per edge each:
/// 1.25 MB at 10M edges), least recently used evicted.
const MASK_CACHE_CAPACITY: usize = 16;

/// One cached mask, filled once by whoever computes it first.
type MaskSlot = Arc<Mutex<Option<Arc<EdgeMask>>>>;

/// Subgraph-view masks memoized across queries (#181, moved here by #192), keyed
/// by a fingerprint of the view: its filters with roles resolved, and for a
/// traversal result the traversal, its resolved seeds, and the view it ran over.
/// Held on a [`GraphIndex`], so a mask is only ever served for the index it was
/// computed over.
#[derive(Default)]
pub struct MaskCache {
    /// Most recently used last. Each slot is locked while its mask is computed,
    /// so concurrent queries over one view compute it once.
    slots: Mutex<Vec<(String, MaskSlot)>>,
    builds: AtomicUsize,
}

impl MaskCache {
    /// The mask for `key`, computed by `build` on a miss.
    pub fn get_or_build(
        &self,
        key: &str,
        build: impl FnOnce() -> PlanResult<EdgeMask>,
    ) -> PlanResult<Arc<EdgeMask>> {
        let slot = {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            let slot = match slots.iter().position(|(k, _)| k == key) {
                Some(i) => slots.remove(i).1,
                None => Arc::default(),
            };
            if slots.len() >= MASK_CACHE_CAPACITY {
                slots.remove(0);
            }
            slots.push((key.to_string(), slot.clone()));
            slot
        };
        let mut mask = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(m) = mask.as_ref() {
            return Ok(m.clone());
        }
        let m = Arc::new(build()?);
        self.builds.fetch_add(1, Ordering::Relaxed);
        *mask = Some(m.clone());
        Ok(m)
    }

    /// Masks this cache has computed (test/observability hook).
    pub fn builds(&self) -> usize {
        self.builds.load(Ordering::Relaxed)
    }
}

/// Topology builds so far, wherever they happen (test/observability hook for the
/// index-preservation contract).
static TOPOLOGY_BUILDS: AtomicUsize = AtomicUsize::new(0);

/// Count one topology build.
pub fn count_topology_build() {
    TOPOLOGY_BUILDS.fetch_add(1, Ordering::Relaxed);
}

/// Topology builds so far.
pub fn topology_builds() -> usize {
    TOPOLOGY_BUILDS.load(Ordering::Relaxed)
}

/// Role name → column name: what `ur.src()` / `ur.dst()` / `ur.id()` resolve to.
pub type Roles = [(&'static str, String)];

/// What [`execute_plan`] reads from its host, by slot. Every method is called at
/// most when the executor needs the value, in the order the executor needs it.
///
/// The host keeps what belongs to its frames: their sources, their built
/// [`GraphIndex`] (and with it the weight and mask caches), a node scan's
/// [`NodeTableCache`], seeds, and the draw of a `sample`. Everything else
/// (scans, masks, weights, every relational step) runs here.
///
/// Batch lists are at least one batch long, so an empty result still carries its
/// schema. Rows the host produces may stay host-side ([`Rows::Host`]) until the
/// engine needs them as batches ([`Inputs::batches`]), so a result the host
/// finishes never crosses into Rust and back.
pub trait Inputs {
    /// The cached graph index of the edge frame `frame` (built on first use).
    fn index(&self, frame: Slot) -> PlanResult<GraphIndex>;

    /// The index of `frame` if it is already built (never builds one).
    fn built_index(&self, frame: Slot) -> PlanResult<Option<GraphIndex>>;

    /// Keep `index` as the index of `frame` unless one was built meanwhile, and
    /// return the frame's index.
    fn put_index(&self, frame: Slot, index: GraphIndex) -> PlanResult<GraphIndex>;

    /// The in-memory table of a frame: an edge frame's whole edge table, or a node
    /// frame's attribute table; `None` for a frame with none.
    fn table(&self, frame: Slot) -> PlanResult<Option<Rows>>;

    /// The storage options of a scan source (the host raises the error for
    /// options it can't pass, such as `store=`).
    fn storage_options(&self, frame: Slot) -> PlanResult<HashMap<String, String>>;

    /// The scanned-table cache of a `scan_nodes` frame.
    fn node_cache(&self, frame: Slot) -> PlanResult<Arc<NodeTableCache>>;

    /// A seed set: user ids (int64 or string).
    fn seeds(&self, slot: Slot) -> PlanResult<ArrayRef>;

    /// The rows a `sample` step keeps out of `rows` rows: their positions,
    /// ascending, or `None` for all of them. The host draws them with its own
    /// seeded generator from the step's `n` and `seed` (behind its `step` handle).
    fn sample(&self, step: Slot, rows: usize) -> PlanResult<Option<Vec<usize>>>;

    /// Host rows as batches.
    fn batches(&self, rows: HostRows) -> PlanResult<Vec<RecordBatch>>;

    /// The error the host met parsing the step behind `step` (see [`Plan`]).
    fn step_error(&self, step: Slot) -> PlanError;

    /// Tell the user `on_null="drop"` removed `n` edge rows (`n > 0`).
    fn warn_dropped(&self, n: usize) -> PlanResult<()>;
}

/// Rows held by the host, opaque to the executor.
pub type HostRows = Box<dyn Any + Send + Sync>;

/// A row set: engine batches, or rows the host holds (see [`Inputs`]).
pub enum Rows {
    Batches(Vec<RecordBatch>),
    Host(HostRows),
}

impl Rows {
    /// The rows as batches, converting host rows through `inputs`.
    pub fn into_batches(self, inputs: &dyn Inputs) -> PlanResult<Vec<RecordBatch>> {
        match self {
            Rows::Batches(b) => Ok(b),
            Rows::Host(h) => inputs.batches(h),
        }
    }
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
    /// An argument of the wrong type (Python's `TypeError`).
    Type(String),
    /// An integer argument out of range (Python's `OverflowError`).
    Overflow(String),
    /// A [`PlanError::NotImplemented`] raised because of `cause`.
    Wrapped {
        message: String,
        cause: Box<PlanError>,
    },
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
            PlanError::NotImplemented(m)
            | PlanError::ColumnNotFound(m)
            | PlanError::Value(m)
            | PlanError::Type(m)
            | PlanError::Overflow(m)
            | PlanError::Wrapped { message: m, .. } => f.write_str(m),
            PlanError::Invalid(m) => write!(f, "invalid plan: {m}"),
            PlanError::Engine(e) => write!(f, "{e}"),
            PlanError::Host(e) => f.write_str(&e.message),
        }
    }
}

impl std::error::Error for PlanError {}

impl From<DataFusionError> for PlanError {
    fn from(e: DataFusionError) -> Self {
        // A timezone-aware/naive comparison keeps the `ValueError` pyarrow raised.
        if let DataFusionError::External(inner) = e.find_root() {
            if let Some(m) = inner.downcast_ref::<crate::eval::TimezoneMismatch>() {
                return PlanError::Value(m.0.clone());
            }
        }
        PlanError::Engine(e)
    }
}

impl From<arrow::error::ArrowError> for PlanError {
    fn from(e: arrow::error::ArrowError) -> Self {
        PlanError::Engine(DataFusionError::ArrowError(Box::new(e), None))
    }
}

pub type PlanResult<T> = std::result::Result<T, PlanError>;

/// Execute a plan, reading its inputs from `inputs`. Returns at least one batch.
pub fn execute_plan(plan: &Plan, inputs: &dyn Inputs) -> PlanResult<Vec<RecordBatch>> {
    execute(plan, inputs)?.into_batches(inputs)
}

/// [`execute_plan`], leaving a result the host produced with the host.
pub fn execute(plan: &Plan, inputs: &dyn Inputs) -> PlanResult<Rows> {
    match build(plan, inputs)? {
        Built::Rows(rows) => Ok(rows),
        Built::Frame(frame) => Ok(Rows::Batches((*frame).collect()?)),
    }
}

/// A plan, built: a source's rows as they are (no step to run), or a frame to
/// lower further steps onto and collect.
enum Built {
    Rows(Rows),
    Frame(Box<Frame>),
}

impl Built {
    fn frame(frame: Frame) -> Built {
        Built::Frame(Box::new(frame))
    }
}

/// Build a plan's frame: run its guards, build its leaf, and lower its steps,
/// without collecting it.
fn build(plan: &Plan, inputs: &dyn Inputs) -> PlanResult<Built> {
    if let Plan::Invalid { step } = plan {
        return Err(inputs.step_error(*step));
    }
    let chain = plan.chain();
    if is_node_frame(&chain) {
        node_frame(&chain, inputs)
    } else {
        edge_frame(&chain, inputs)
    }
}

/// A plan as a [`Frame`], for a step that takes a whole frame (a join's right
/// side): built like any frame, its steps lowered, and not collected.
fn frame_of(plan: &Plan, inputs: &dyn Inputs) -> PlanResult<Frame> {
    match build(plan, inputs)? {
        Built::Frame(frame) => Ok(*frame),
        Built::Rows(rows) => Ok(Frame::from_batches(
            crate::lower::session(),
            rows.into_batches(inputs)?,
        )?),
    }
}

/// The graph index an edge input runs over, after the checks a graph op over it
/// needs. For a traversal result that is its input's index (the traversal only
/// masks it), so its input is checked too.
pub fn require_index(edges: &EdgeInput, inputs: &dyn Inputs) -> PlanResult<GraphIndex> {
    // The view checks a traversal's input too (it is a view of its own), and
    // names the frame whose index a traversal result runs over: its input's.
    let view = edges.view()?;
    inputs.index(view.frame)
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

/// A node frame: `describe` or a random walk with the steps written after it, or
/// a node source (an attribute table, or `edges.nodes()`) and its steps in the
/// order written: graph columns (`with_columns`), joins, and the relational
/// steps, lowered onto one frame.
fn node_frame(chain: &[&Plan], inputs: &dyn Inputs) -> PlanResult<Built> {
    let id_col = node_id_col(chain);
    // describe() is a whole-graph summary, computed eagerly off the topology and
    // wrapped as a one-row frame; steps written after it run on that row.
    if let Some(Plan::Describe {
        edges,
        full,
        direction,
    }) = chain.first()
    {
        let steps = leaf_steps(chain, "describe()", inputs)?;
        let index = require_index(edges, inputs)?;
        let dir = parse_stat_direction(direction)?;
        let summary = describe(&index.topo, *full, dir)?;
        if steps.is_empty() {
            return Ok(Built::Rows(Rows::Batches(vec![summary])));
        }
        let frame = Frame::from_batches(crate::lower::session(), vec![summary])?;
        return Ok(Built::frame(lower_node_steps(
            frame,
            &steps,
            &[],
            None,
            inputs,
        )?));
    }

    if let Some(walk) = chain.iter().find(|p| matches!(p, Plan::RandomWalk { .. })) {
        return random_walk_frame(chain, walk, inputs);
    }

    // The steps after the `nodes()` boundary, in the order written. A
    // `nodes()`-derived frame inherits the edge plan: its steps before the
    // boundary are edge-plan steps (a `filter` there is a subgraph view, read as
    // the edge mask), not node-result operators.
    let mut steps: Vec<&Plan> = vec![];
    let mut pre_nodes: Vec<&str> = vec![]; // non-filter edge-plan steps before `nodes`
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
        match step {
            Plan::Join { how, .. } => check_join_how(how)?,
            Plan::WithColumns { columns, .. } => {
                for e in columns {
                    if let Expr::Graph(verb) = &e.expr {
                        check_verb(verb)?;
                    }
                }
            }
            _ if !is_tail(step) => {
                return not_implemented(format!(
                    "collect() does not yet support the '{op}' step in a composed pipeline."
                ));
            }
            _ => {}
        }
        check_step(step, inputs)?;
        steps.push(step);
    }
    let first_columns = steps.iter().find_map(|s| match s {
        Plan::WithColumns { columns, .. } => Some(columns.as_slice()),
        _ => None,
    });

    if !pre_nodes.is_empty() {
        // A graph op over that derived edge frame raises the precise guard;
        // otherwise the node set would silently ignore the step.
        if let Some(exprs) = first_columns.filter(|g| !g.is_empty()) {
            single_edges(graph_columns(exprs))?.view()?;
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

    let roles = [("id", id_col.clone())];
    let Some(slot) = node_source_slot(chain) else {
        // `edges.nodes()`: no rows until the first with_columns computes them.
        if first_columns.is_none() {
            return not_implemented(
                "collect() on this NodeFrame needs a source (scan_nodes / from_arrow(id=...)) \
                 or a with_columns of graph algorithms; a bare edges.nodes() has no \
                 materializable node set yet.",
            );
        }
        let first = steps
            .iter()
            .position(|s| matches!(s, Plan::WithColumns { .. }))
            .expect("a with_columns step");
        check_before_node_set(&steps[..first], steps[first])?;
        let Plan::WithColumns { columns, .. } = steps[first] else {
            unreachable!("found above")
        };
        let frame = node_set_frame(columns, inputs)?;
        let frame = lower_node_steps(frame, &steps[..first], &roles, None, inputs)?;
        let frame = lower_node_steps(frame, &steps[first + 1..], &roles, None, inputs)?;
        return Ok(Built::frame(frame));
    };
    // A scan reads the id, what a neighbour aggregation reads, and whatever the
    // steps read before a select or group_by narrows the columns.
    let mut required: BTreeSet<String> = BTreeSet::from([id_col.clone()]);
    for s in &steps {
        if let Plan::WithColumns { columns, .. } = s {
            for e in columns {
                if let Expr::Graph(verb) = &e.expr {
                    if let Some(c) = verb.agg_column() {
                        required.insert(c.to_string());
                    }
                }
            }
        }
    }
    let needed = projection(&steps, required, &roles);
    let table = node_rows(chain, slot, needed.as_deref(), inputs)?;
    if steps.is_empty() {
        return Ok(Built::Rows(table));
    }
    let table = table.into_batches(inputs)?;
    let frame = Frame::from_batches(crate::lower::session(), table.clone())?;
    let frame = lower_node_steps(frame, &steps, &roles, Some(&table), inputs)?;
    Ok(Built::frame(frame))
}

/// The steps written after a leaf (`describe`, a random walk): relational steps
/// and joins.
fn leaf_steps<'a>(
    chain: &[&'a Plan],
    leaf: &str,
    inputs: &dyn Inputs,
) -> PlanResult<Vec<&'a Plan>> {
    let mut steps = vec![];
    for &s in &chain[1..] {
        if matches!(s, Plan::Nodes { .. }) {
            continue;
        }
        match s {
            Plan::Join { how, .. } => check_join_how(how)?,
            _ if is_tail(s) => {}
            _ => {
                return not_implemented(format!(
                    "collect() does not yet support the '{}' step after {leaf}.",
                    s.op()
                ))
            }
        }
        check_step(s, inputs)?;
        steps.push(s);
    }
    Ok(steps)
}

/// `edges.nodes()` has no rows before its first `with_columns` computes them, so
/// only a filter that commutes with it may be written first: one that reads no
/// column it computes.
fn check_before_node_set(before: &[&Plan], with_columns: &Plan) -> PlanResult<()> {
    let Plan::WithColumns { columns, .. } = with_columns else {
        unreachable!("a with_columns step")
    };
    let computed: HashSet<&str> = columns.iter().map(|e| e.name.as_str()).collect();
    for step in before {
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
        let clash: BTreeSet<String> = expr_columns(predicate, &[("id", "id".to_string())])
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

/// The graph verbs of a `with_columns`.
fn graph_columns(exprs: &[NamedExpr]) -> Vec<(&str, &GraphVerb)> {
    exprs
        .iter()
        .filter_map(|e| match &e.expr {
            Expr::Graph(v) => Some((e.name.as_str(), v.as_ref())),
            Expr::Scalar(_) => None,
        })
        .collect()
}

/// The one edge input every graph verb of a `with_columns` shares.
fn single_edges<'a>(verbs: Vec<(&str, &'a GraphVerb)>) -> PlanResult<&'a EdgeInput> {
    let mut edges: Option<&EdgeInput> = None;
    for (_, verb) in verbs {
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

const NON_GRAPH_COLUMN: &str = "with_columns() takes ursa expressions: ur.col(...), \
     ur.lit(...), arithmetic and comparisons over them, or graph verbs; got a value of \
     another kind.";

/// The graph columns of a `with_columns`, computed: the index and mask their
/// edges run over, and the columns, built. `nodes` is the frame's attribute
/// table (what a neighbour aggregation reads), if it has one.
struct GraphColumns {
    index: GraphIndex,
    mask: Option<Arc<EdgeMask>>,
    columns: Vec<crate::result::OutputColumn>,
}

fn graph_columns_for(
    exprs: &[NamedExpr],
    nodes: Option<(&[RecordBatch], &str)>,
    inputs: &dyn Inputs,
) -> PlanResult<Option<GraphColumns>> {
    let verbs = graph_columns(exprs);
    if verbs.is_empty() {
        return Ok(None);
    }
    let edges = single_edges(verbs.clone())?;
    // A weighted algorithm needs the edge columns its weight expression reads.
    // The read is skipped when the index already caches every one of these
    // weights (and, for a scan, builds the index too).
    let mut weight_cols: BTreeSet<String> = BTreeSet::new();
    let mut weight_keys: Vec<String> = vec![];
    for (_, verb) in &verbs {
        if let Some(w) = &verb.weight {
            check_literals(w, "weight")?;
            weight_cols.extend(value_columns(w, &[]));
            weight_keys.push(w.to_string());
        }
    }
    let edge_attr = if weight_cols.is_empty() {
        None
    } else {
        prepare_weighted(edges, &weight_cols, &weight_keys, inputs)?
    };
    let index = require_index(edges, inputs)?;
    let mask = edge_mask(edges, &index, inputs)?;
    let specs: Vec<NodeColumn> = verbs
        .iter()
        .map(|(name, verb)| (name.to_string(), (*verb).clone()))
        .collect();
    let columns = node_columns(
        &index.topo,
        &index.ids,
        &specs,
        nodes.map(|(b, _)| b),
        nodes.map(|(_, id)| id),
        edge_attr.as_deref(),
        Some(&index.weights),
    )?;
    Ok(Some(GraphColumns {
        index,
        mask,
        columns,
    }))
}

/// The frame of `edges.nodes().with_columns(...)`: the graph's node set, in node
/// order, with the graph columns, then the `with_columns`' other expressions.
fn node_set_frame(exprs: &[NamedExpr], inputs: &dyn Inputs) -> PlanResult<Frame> {
    let Some(g) = graph_columns_for(exprs, None, inputs)? else {
        return not_implemented(NON_GRAPH_COLUMN);
    };
    let ctx = crate::lower::session();
    let leaf = graph_leaf(&ctx, g.index.topo, g.index.ids, g.columns, g.mask);
    let mut frame = Frame::from_ordered(ctx, leaf);
    let before: Vec<String> = vec!["id".to_string()];
    scalar_columns(&mut frame, exprs, &before, &[("id", "id".to_string())])?;
    Ok(frame)
}

/// Lower the steps of a frame, in order: relational steps, `with_columns` (graph
/// columns attached to the rows by id) and joins. `table` is the frame's
/// attribute table, which a neighbour aggregation reads.
fn lower_node_steps(
    mut frame: Frame,
    steps: &[&Plan],
    roles: &Roles,
    table: Option<&[RecordBatch]>,
    inputs: &dyn Inputs,
) -> PlanResult<Frame> {
    // The id column graph columns attach by (a node frame's `id` role).
    let id_col = roles.first().map_or("id", |(_, c)| c.as_str());
    let mut rest = steps;
    while let Some(i) = rest
        .iter()
        .position(|s| matches!(s, Plan::WithColumns { .. } | Plan::Join { .. }))
    {
        lower_steps(&mut frame, &rest[..i], roles, inputs)?;
        match rest[i] {
            Plan::WithColumns { columns, .. } => {
                with_columns(&mut frame, columns, id_col, table, roles, inputs)?
            }
            join => frame = run_join(frame, join, inputs)?,
        }
        rest = &rest[i + 1..];
    }
    lower_steps(&mut frame, rest, roles, inputs)?;
    Ok(frame)
}

/// `with_columns` on a frame with rows: the graph columns attached by the id
/// column `id_col`, then the other expressions, evaluated over the frame's
/// columns. A column that already exists is replaced in place; new columns are
/// appended in the order written.
fn with_columns(
    frame: &mut Frame,
    exprs: &[NamedExpr],
    id_col: &str,
    table: Option<&[RecordBatch]>,
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<()> {
    let before = frame.names();
    let nodes = table.map(|t| (t, id_col));
    if let Some(g) = graph_columns_for(exprs, nodes, inputs)? {
        if !frame.has_column(id_col) {
            return Err(PlanError::ColumnNotFound(format!(
                "with_columns() attaches graph columns by the node id column {}, which an \
                 earlier step removed or renamed.",
                py_str(id_col)
            )));
        }
        let ctx = frame.ctx().clone();
        let rows = frame.ordered_batches()?;
        let attached = attach_columns(
            &ctx,
            g.index.topo,
            g.index.ids,
            g.columns,
            g.mask,
            rows,
            id_col,
        )?;
        frame.set_batches(attached)?;
    }
    scalar_columns(frame, exprs, &before, roles)
}

/// The non-graph expressions of a `with_columns`, evaluated over the frame (its
/// columns `before` the graph columns were attached), and the column order:
/// `before`, with replaced columns in place, then each new column as written.
fn scalar_columns(
    frame: &mut Frame,
    exprs: &[NamedExpr],
    before: &[String],
    roles: &Roles,
) -> PlanResult<()> {
    if exprs.iter().all(|e| matches!(e.expr, Expr::Graph(_))) {
        return Ok(());
    }
    let mut lowered: HashMap<&str, DfExpr> = HashMap::new();
    for e in exprs {
        if let Expr::Scalar(v) = &e.expr {
            lowered.insert(&e.name, scalar_expr(frame, v, roles, "with_columns()")?);
        }
    }
    let mut out: Vec<DfExpr> = before
        .iter()
        .map(|c| match lowered.remove(c.as_str()) {
            Some(e) => e.alias(c),
            None => col_named(c),
        })
        .collect();
    for e in exprs {
        if before.contains(&e.name) {
            continue;
        }
        out.push(match lowered.remove(e.name.as_str()) {
            Some(x) => x.alias(&e.name),
            None => col_named(&e.name),
        });
    }
    frame.project(out)?;
    Ok(())
}

/// A scalar expression over the frame's columns (`what` names the step, for its
/// errors): its role references resolved, its columns checked, lowered with
/// comparisons resolved as a filter's are.
fn scalar_expr(frame: &Frame, v: &Value, roles: &Roles, what: &str) -> PlanResult<DfExpr> {
    if !matches!(
        kind(v),
        "col" | "lit" | "binary" | "unary" | "src" | "dst" | "id" | "alias"
    ) {
        return not_implemented(NON_GRAPH_COLUMN);
    }
    check_literals(v, what)?;
    let v = resolve_roles(v, roles)?;
    let missing: Vec<String> = value_columns(&v, &[])
        .into_iter()
        .filter(|c| !frame.has_column(c))
        .collect();
    if let Some(c) = missing.first() {
        return Err(unknown_column(what, c, &frame.names()));
    }
    let e = crate::expr::lower(&crate::expr::parse_ursa_expr(&v)?)?;
    Ok(crate::compare::resolve_comparisons(e, frame.df_schema())?)
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

/// The edge rows a weight reads: `columns` of the edges' rows, aligned with their
/// index's edge rows, or `None` when the index already caches every weight in
/// `keys` (#179). For a scan with no index yet the same read builds the index.
fn prepare_weighted(
    edges: &EdgeInput,
    columns: &BTreeSet<String>,
    keys: &[String],
    inputs: &dyn Inputs,
) -> PlanResult<Option<Vec<RecordBatch>>> {
    // Checked before anything is read: for a scan the read also builds the index,
    // which must never happen for an edge plan a graph op can't run over.
    let view = edges.view()?;
    let frame = view.frame;
    if !keys.is_empty() {
        if let Some(index) = inputs.built_index(frame)? {
            if keys.iter().all(|k| index.weights.get(k).is_some()) {
                return Ok(None);
            }
        }
    }
    const NO_SOURCE: &str = "a weighted algorithm needs an in-memory or scan_edges source so \
         the weight columns can be read.";
    let columns: Vec<String> = columns.iter().cloned().collect();
    let chain = view.topology.chain();
    match chain[0] {
        _ if !view.sourced => not_implemented(NO_SOURCE),
        Plan::FromArrow { .. } => {
            let Some(table) = inputs.table(frame)? else {
                return not_implemented(NO_SOURCE);
            };
            let table = table.into_batches(inputs)?;
            let schema = table[0].schema();
            let missing: Vec<&String> = columns
                .iter()
                .filter(|c| schema.index_of(c).is_err())
                .collect();
            if !missing.is_empty() {
                return Err(PlanError::ColumnNotFound(format!(
                    "weight expression references unknown edge column(s): {}",
                    py_list(missing.into_iter())
                )));
            }
            let indices: Vec<usize> = columns
                .iter()
                .map(|c| schema.index_of(c).expect("checked above"))
                .collect();
            let batches = table
                .iter()
                .map(|b| b.project(&indices))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| PlanError::Engine(DataFusionError::ArrowError(Box::new(e), None)))?;
            Ok(Some(batches))
        }
        Plan::ScanEdges { path, on_null, .. } => {
            let path = single_path(
                path,
                "weighted algorithms over a multi-path scan_edges source are not supported yet.",
            )?;
            let opts = inputs.storage_options(frame)?;
            // The frame's own roles: a reversed scan reads its endpoints swapped,
            // as its index was built.
            let (src, dst) = &view.roles;
            let mut batches = scan_edges_batch(path, src, dst, &opts, &columns)?;
            if on_null == "drop" {
                let dropped = drop_null_endpoints(&mut batches)?;
                if dropped > 0 {
                    inputs.warn_dropped(dropped)?;
                }
            }
            // One scan, src/dst first: with no index yet, these rows build it, so
            // the weights line up with its edge rows by construction.
            if inputs.built_index(frame)?.is_none() {
                let pairs: Vec<(&dyn Array, &dyn Array)> = batches
                    .iter()
                    .map(|b| (b.column(0).as_ref(), b.column(1).as_ref()))
                    .collect();
                let (topo, ids) = crate::topology::build_topology_batches(&pairs)
                    .map_err(|e| PlanError::Value(e.to_string()))?;
                count_topology_build();
                inputs.put_index(frame, GraphIndex::new(topo, ids))?;
            }
            Ok(Some(batches))
        }
        _ => not_implemented(NO_SOURCE),
    }
}

/// Remove the rows of `batches` (src/dst first) with a null endpoint; the number
/// removed.
fn drop_null_endpoints(batches: &mut [RecordBatch]) -> PlanResult<usize> {
    use arrow::compute::kernels::boolean::{and, is_not_null};
    let arrow = |e| PlanError::Engine(DataFusionError::ArrowError(Box::new(e), None));
    let mut dropped = 0;
    for b in batches.iter_mut() {
        let keep = and(
            &is_not_null(b.column(0)).map_err(arrow)?,
            &is_not_null(b.column(1)).map_err(arrow)?,
        )
        .map_err(arrow)?;
        let kept = arrow::compute::filter_record_batch(b, &keep).map_err(arrow)?;
        dropped += b.num_rows() - kept.num_rows();
        *b = kept;
    }
    Ok(dropped)
}

/// A scan's one path, or the error for a list of them (`msg`).
fn single_path<'a>(path: &'a Option<ScanPath>, msg: &str) -> PlanResult<&'a str> {
    match path {
        Some(ScanPath::One(p)) => Ok(p),
        _ => not_implemented(msg),
    }
}

const MULTI_PATH_EDGES: &str = "collect() over a scan_edges source supports a single string \
     path (glob included) for now, not a list of paths.";

const MULTI_PATH_NODES: &str = "collect() over a scan_nodes source supports a single string \
     path (glob included) for now, not a list of paths.";

/// The subgraph-view mask of `edges` over `index`, or `None` for the whole graph.
fn edge_mask(
    edges: &EdgeInput,
    index: &GraphIndex,
    inputs: &dyn Inputs,
) -> PlanResult<Option<Arc<EdgeMask>>> {
    view_mask(&edges.view()?, index, inputs, &mut HashMap::new())
}

// --- subgraph-view masks (#114, #116, #192) -----------------------------------

/// Seed sets already read for a view, by slot (a fingerprint reads them first).
type SeedSets = HashMap<Slot, ArrayRef>;

fn seed_set(slot: Slot, seeds: &mut SeedSets, inputs: &dyn Inputs) -> PlanResult<ArrayRef> {
    if let Some(a) = seeds.get(&slot) {
        return Ok(a.clone());
    }
    let a = inputs.seeds(slot)?;
    seeds.insert(slot, a.clone());
    Ok(a)
}

/// A view's mask over `index`: one bit per edge row, served from the index's
/// [`MaskCache`] under the view's fingerprint, computed on a miss.
fn view_mask(
    view: &EdgeView<'_>,
    index: &GraphIndex,
    inputs: &dyn Inputs,
    seeds: &mut SeedSets,
) -> PlanResult<Option<Arc<EdgeMask>>> {
    if !view.has_mask() {
        return Ok(None);
    }
    let key = view_key(view, inputs, seeds)?;
    index
        .masks
        .get_or_build(&key, || build_mask(view, index, inputs, seeds))
        .map(Some)
}

/// The fingerprint of a view's mask: what it is a function of, given the rows of
/// its index.
fn view_key(view: &EdgeView<'_>, inputs: &dyn Inputs, seeds: &mut SeedSets) -> PlanResult<String> {
    let roles = view_roles(view);
    let mut key = String::new();
    for p in &view.predicates {
        let resolved = match p {
            Expr::Scalar(v) => resolve_roles(v, &roles).unwrap_or_else(|_| v.clone()),
            Expr::Graph(_) => Value::from("graph"),
        };
        key.push_str(&format!("filter {resolved};"));
    }
    if let Some(Induced {
        traversal, input, ..
    }) = &view.induced_by
    {
        let inner = view_key(input, inputs, seeds)?;
        match traversal {
            Plan::Hop {
                n,
                direction,
                seeds: slot,
                ..
            } => {
                let set = seed_set(*slot, seeds, inputs)?;
                key.push_str(&format!(
                    "hop n={} direction={direction} seeds={} over [{inner}];",
                    n.0,
                    array_digest(set.as_ref())
                ));
            }
            Plan::ShortestPath {
                source,
                target,
                weight,
                direction,
                ..
            } => {
                let (s, t) = (
                    seed_set(*source, seeds, inputs)?,
                    seed_set(*target, seeds, inputs)?,
                );
                key.push_str(&format!(
                    "shortest_path direction={direction} source={} target={} weight={} over [{inner}];",
                    array_digest(s.as_ref()),
                    array_digest(t.as_ref()),
                    weight.as_ref().map(Value::to_string).unwrap_or_default()
                ));
            }
            _ => unreachable!("a traversal is a hop or a shortest_path"),
        }
    }
    Ok(key)
}

/// A content digest of an id array (int64 or string), for a fingerprint.
fn array_digest(a: &dyn Array) -> String {
    use std::hash::{Hash, Hasher};
    let strings = arrow::compute::cast(a, &arrow::datatypes::DataType::Utf8).ok();
    let mut parts = [
        std::collections::hash_map::DefaultHasher::new(),
        std::collections::hash_map::DefaultHasher::new(),
    ];
    for (salt, h) in parts.iter_mut().enumerate() {
        salt.hash(h);
        a.data_type().to_string().hash(h);
        a.len().hash(h);
        match strings
            .as_ref()
            .map(|s| s.as_any().downcast_ref::<arrow::array::StringArray>())
        {
            Some(Some(s)) => s.iter().for_each(|v| v.hash(h)),
            _ => format!("{a:?}").hash(h),
        }
    }
    format!("{:016x}{:016x}", parts[0].finish(), parts[1].finish())
}

/// The role map of a view's rows.
fn view_roles(view: &EdgeView<'_>) -> [(&'static str, String); 2] {
    [("src", view.roles.0.clone()), ("dst", view.roles.1.clone())]
}

/// Compute a view's mask (uncached).
fn build_mask(
    view: &EdgeView<'_>,
    index: &GraphIndex,
    inputs: &dyn Inputs,
    seeds: &mut SeedSets,
) -> PlanResult<EdgeMask> {
    let mut mask: Option<EdgeMask> = None;
    if let Some(Induced {
        traversal,
        edges,
        input,
    }) = &view.induced_by
    {
        // The reached node set, found over the input's own view, induces the
        // subgraph: an edge is kept iff both its endpoints are reached (#116).
        let within = view_mask(input, index, inputs, seeds)?;
        let reached = reached_nodes(traversal, edges, index, within.as_deref(), inputs, seeds)?;
        mask = Some(EdgeMask::induced(&index.topo, &reached, within.as_deref()));
    }
    if !view.predicates.is_empty() {
        let m = predicate_mask(view, index.topo.n_edges(), inputs)?;
        mask = Some(match mask {
            Some(t) => t.intersect(&m),
            None => m,
        });
    }
    Ok(mask.expect("a view with a mask has a traversal or a filter"))
}

/// The nodes a traversal reaches over `within` (one flag per dense node): a hop's
/// region, seeds included, or the nodes on a shortest path.
fn reached_nodes(
    traversal: &Plan,
    edges: &EdgeInput,
    index: &GraphIndex,
    within: Option<&EdgeMask>,
    inputs: &dyn Inputs,
    seeds: &mut SeedSets,
) -> PlanResult<Vec<bool>> {
    let (topo, ids) = (&index.topo, &index.ids);
    let mut reached = vec![false; topo.n_nodes()];
    match traversal {
        Plan::Hop {
            n,
            direction,
            seeds: slot,
            ..
        } => {
            let set = seed_set(*slot, seeds, inputs)?;
            let dense: Vec<u32> = resolve_dense(ids, set.as_ref())?
                .into_iter()
                .flatten()
                .collect();
            let n = n.u32("n")?;
            let dir: ursa_core::Direction = parse_direction(direction)?.into();
            reached = ursa_core::algo::k_hop_reached_masked(topo, within, &dense, n, dir);
        }
        Plan::ShortestPath {
            source,
            target,
            weight,
            direction,
            ..
        } => {
            let s = seed_set(*source, seeds, inputs)?;
            let t = seed_set(*target, seeds, inputs)?;
            let weights = match weight {
                None => None,
                Some(w) => {
                    check_literals(w, "weight")?;
                    let cols = value_columns(w, &[]);
                    let attr = if cols.is_empty() {
                        None
                    } else {
                        prepare_weighted(edges, &cols, &[], inputs)?
                    };
                    Some(
                        attr.map(|a| (w.to_string(), a))
                            .unwrap_or((w.to_string(), vec![])),
                    )
                }
            };
            let dir: ursa_core::Direction = parse_direction(direction)?.into();
            let s = resolve_dense(ids, s.as_ref())?.into_iter().next().flatten();
            let t = resolve_dense(ids, t.as_ref())?.into_iter().next().flatten();
            let path = match (s, t) {
                (Some(s), Some(t)) => match weights {
                    None => ursa_core::algo::shortest_path_masked(topo, within, s, t, dir),
                    Some((json, attr)) => {
                        let attr = (!attr.is_empty()).then_some(attr);
                        let w = edge_weights(
                            attr.as_deref(),
                            &json,
                            topo.n_edges(),
                            "weighted shortest_path",
                        )?;
                        ursa_core::algo::shortest_path_weighted_with_cost_masked(
                            topo, within, &w, s, t, dir,
                        )
                        .map(|(p, _)| p)
                    }
                },
                _ => None,
            };
            for v in path.unwrap_or_default() {
                reached[v as usize] = true;
            }
        }
        _ => unreachable!("a traversal is a hop or a shortest_path"),
    }
    Ok(reached)
}

/// The mask of a view's filters over its topology's edge rows (#114): each
/// predicate evaluated over those rows, in index order, ANDed.
fn predicate_mask(
    view: &EdgeView<'_>,
    n_edges: usize,
    inputs: &dyn Inputs,
) -> PlanResult<EdgeMask> {
    let roles = view_roles(view);
    // Only what the predicates read: the index build already checked the
    // endpoints, and a scan reads them anyway to drop null-endpoint rows.
    let mut needed: BTreeSet<String> = BTreeSet::new();
    for p in &view.predicates {
        needed.extend(expr_columns(p, &roles));
    }
    let needed: Vec<String> = needed.into_iter().collect();
    let chain = view.topology.chain();
    let rows = edge_rows(chain[0], view.frame, Some(&needed), inputs)?;
    debug_assert!(view.sourced, "a filter view is over a source's rows");
    let schema = rows[0].schema();
    let mut compiled = Vec::with_capacity(view.predicates.len());
    for p in &view.predicates {
        compiled.push(host_predicate(p, &roles, &schema)?);
    }
    let flags = crate::eval::keep_flags(&rows, &compiled)?;
    if flags.len() != n_edges {
        return Err(PlanError::Value(format!(
            "edge_mask length ({}) does not match the edge count ({n_edges})",
            flags.len()
        )));
    }
    Ok(EdgeMask::from_boolean_array(&flags))
}

/// The rows of an edge source (`base`, a frame's first step) in index order:
/// the in-memory table, or a scan reading at least `columns` (`None`: all).
fn edge_rows(
    base: &Plan,
    frame: Slot,
    columns: Option<&[String]>,
    inputs: &dyn Inputs,
) -> PlanResult<Vec<RecordBatch>> {
    match base {
        Plan::FromArrow { .. } => match inputs.table(frame)? {
            Some(rows) => rows.into_batches(inputs),
            None => not_implemented(NO_EDGE_SOURCE),
        },
        Plan::ScanEdges {
            path,
            src,
            dst,
            on_null,
            ..
        } => {
            let path = single_path(path, MULTI_PATH_EDGES)?;
            let opts = inputs.storage_options(frame)?;
            let rows = scan_edges_rows(
                path,
                src,
                dst,
                &opts,
                columns.unwrap_or(&[]),
                &[],
                on_null == "drop",
            )?;
            if rows.dropped > 0 {
                inputs.warn_dropped(rows.dropped)?;
            }
            Ok(rows.batches)
        }
        _ => not_implemented(NO_EDGE_SOURCE),
    }
}

const NO_EDGE_SOURCE: &str = "collect() needs edges from ur.from_arrow(...), ur.from_polars(...), \
     or ur.scan_edges(<.parquet|.csv>); this frame has no resolvable source.";

/// The rows of a node source frame: its in-memory table, or its scan (through the
/// frame's [`NodeTableCache`]) reading at least `columns` (`None`: all).
fn node_rows(
    chain: &[&Plan],
    frame: Slot,
    columns: Option<&[String]>,
    inputs: &dyn Inputs,
) -> PlanResult<Rows> {
    match chain[0] {
        Plan::ScanNodes { path, id, .. } => {
            let path = single_path(path, MULTI_PATH_NODES)?;
            let opts = inputs.storage_options(frame)?;
            let cache = inputs.node_cache(frame)?;
            Ok(Rows::Batches(cache.read(path, id, &opts, columns)?))
        }
        _ => inputs
            .table(frame)?
            .ok_or_else(|| PlanError::NotImplemented(NO_EDGE_SOURCE.into())),
    }
}

/// A random walk and the steps written after it.
fn random_walk_frame(chain: &[&Plan], walk: &Plan, inputs: &dyn Inputs) -> PlanResult<Built> {
    let Plan::RandomWalk {
        edges,
        start,
        steps: n_steps,
        walks_per_node,
        seed,
    } = walk
    else {
        unreachable!("random_walk_frame takes a random_walk step")
    };
    let mut steps: Vec<&Plan> = vec![];
    for &s in chain {
        let op = s.op();
        if matches!(op, "random_walk" | "nodes" | "from_arrow" | "scan_nodes") {
            continue;
        }
        match s {
            Plan::Join { how, .. } => check_join_how(how)?,
            _ if is_tail(s) => {}
            _ => {
                return not_implemented(format!(
                    "collect() does not yet support the '{op}' step after a random_walk."
                ))
            }
        }
        check_step(s, inputs)?;
        steps.push(s);
    }
    let index = require_index(edges, inputs)?;
    let starts = inputs.seeds(*start)?;
    // A walk over a subgraph view follows only its edges (#192).
    let mask = edge_mask(edges, &index, inputs)?;
    let ctx = crate::lower::session();
    let leaf = walk_leaf(
        &ctx,
        index.topo,
        index.ids,
        mask,
        starts.as_ref(),
        n_steps.u32("steps")?,
        walks_per_node.u32("walks_per_node")?,
        seed_arg(seed, "seed")?,
    )?;
    let frame = Frame::from_ordered(ctx, leaf);
    // Walk output columns are (walk_id, step, node); no role applies.
    Ok(Built::frame(lower_node_steps(
        frame,
        &steps,
        &[],
        None,
        inputs,
    )?))
}

// --- edge frames ---------------------------------------------------------------

/// An edge frame: a plain edge frame, or a `hop` / `shortest_path` result, then
/// its steps in the order written (joins among them).
fn edge_frame(chain: &[&Plan], inputs: &dyn Inputs) -> PlanResult<Built> {
    let (src_col, dst_col) = edge_roles(chain);
    let mut traversal: Option<&Plan> = None;
    let mut steps: Vec<&Plan> = vec![];

    for &step in chain {
        let op = step.op();
        match step {
            Plan::Hop { .. } | Plan::ShortestPath { .. } => traversal = Some(step),
            Plan::ScanEdges { .. } | Plan::FromArrow { .. } | Plan::Nodes { .. } => {}
            Plan::Join { how, .. } => {
                check_join_how(how)?;
                check_step(step, inputs)?;
                steps.push(step);
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
            _ if is_tail(step) => {
                check_step(step, inputs)?;
                steps.push(step);
            }
            _ => {
                return not_implemented(format!(
                    "collect() does not yet support the '{op}' step on an edge frame."
                ))
            }
        }
    }

    let Some(traversal) = traversal else {
        let roles = [("src", src_col), ("dst", dst_col)];
        return plain_edges_frame(chain, &steps, &roles, inputs);
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
    let ctx = crate::lower::session();
    let frame = match traversal {
        Plan::Hop {
            edges,
            n,
            direction,
            seeds,
        } => {
            let index = require_index(edges, inputs)?;
            let seeds = inputs.seeds(*seeds)?;
            // A traversal over a subgraph view follows only its edges (#192).
            let mask = edge_mask(edges, &index, inputs)?;
            let leaf = hop_leaf(
                &ctx,
                index.topo,
                index.ids,
                mask,
                seeds.as_ref(),
                n.u32("n")?,
                direction,
            )?;
            Frame::from_ordered(ctx, leaf)
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
            if let Some(w) = weight {
                check_literals(w, "weight")?;
            }
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
            let mask = edge_mask(edges, &index, inputs)?;
            path_leaf(
                &ctx,
                index.topo,
                index.ids,
                mask,
                source.as_ref(),
                target.as_ref(),
                direction,
                weight_json.as_deref(),
                edge_attr,
            )?
        }
        _ => unreachable!("a traversal is a hop or a shortest_path"),
    };
    Ok(Built::frame(lower_node_steps(
        frame, &steps, &roles, None, inputs,
    )?))
}

/// A plain edge source and its steps. A scan reads the columns the steps need and
/// takes its leading filters with it (#192); the other steps are lowered on its
/// rows, in order.
fn plain_edges_frame(
    chain: &[&Plan],
    steps: &[&Plan],
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<Built> {
    // The endpoints plus what the steps read before a select or group_by narrows
    // the columns.
    let required: BTreeSet<String> = roles.iter().map(|(_, c)| c.clone()).collect();
    let needed = projection(steps, required, roles);
    let Some(slot) = source_slot(chain) else {
        return not_implemented(NO_EDGE_SOURCE);
    };
    let (rows, steps) = match chain[0] {
        Plan::ScanEdges {
            path,
            src,
            dst,
            on_null,
            ..
        } => {
            let path = single_path(path, MULTI_PATH_EDGES)?;
            let opts = inputs.storage_options(slot)?;
            let drop_nulls = on_null == "drop";
            // The leading filters go into the scan, unless `on_null="drop"` must
            // count the null rows of the whole file first.
            let mut pushed: Vec<Value> = vec![];
            if !drop_nulls {
                for step in steps {
                    match pushable(step, roles) {
                        Some(v) => pushed.push(v),
                        None => break,
                    }
                }
            }
            let (rows, n_pushed) = crate::scan::scan_edges_rows_pushing(
                path,
                src,
                dst,
                &opts,
                needed.as_deref().unwrap_or(&[]),
                &pushed,
                drop_nulls,
            )?;
            if rows.dropped > 0 {
                inputs.warn_dropped(rows.dropped)?;
            }
            (Rows::Batches(rows.batches), &steps[n_pushed..])
        }
        _ => match inputs.table(slot)? {
            Some(table) => (table, steps),
            None => return not_implemented(NO_EDGE_SOURCE),
        },
    };
    if steps.is_empty() {
        return Ok(Built::Rows(rows));
    }
    let frame = Frame::from_batches(crate::lower::session(), rows.into_batches(inputs)?)?;
    let frame = lower_node_steps(frame, steps, roles, None, inputs)?;
    Ok(Built::frame(frame))
}

/// A step a scan can take into its read: a well-formed `filter`, roles resolved.
/// The scan pushes it only if it reads columns the scan reads.
fn pushable(step: &Plan, roles: &Roles) -> Option<Value> {
    let Plan::Filter {
        predicate: Expr::Scalar(v),
        invalid: false,
        ..
    } = step
    else {
        return None;
    };
    require_predicate(v).ok()?;
    host_check(v, roles, None).ok()?;
    resolve_roles(v, roles).ok()
}

// --- join --------------------------------------------------------------------

fn check_join_how(how: &str) -> PlanResult<()> {
    if How::parse(how).is_some() {
        return Ok(());
    }
    not_implemented(format!(
        "join how={} is not supported (use ['anti', 'full', 'inner', 'left', 'outer', \
         'right', 'semi']).",
        py_str(how)
    ))
}

/// Join `left` with the join step's right side: build the right side as a frame
/// of its own, check the keys and column collisions, and join the two in one
/// plan ([`crate::lower::join`]).
fn run_join(left: Frame, join: &Plan, inputs: &dyn Inputs) -> PlanResult<Frame> {
    let Plan::Join { right, on, how, .. } = join else {
        unreachable!("run_join takes a join step")
    };
    let how = How::parse(how).expect("checked by check_join_how");
    let right = frame_of(right, inputs).map_err(|e| match e {
        PlanError::NotImplemented(_)
        | PlanError::Wrapped { .. }
        | PlanError::Host(HostError {
            not_implemented: true,
            ..
        }) => PlanError::Wrapped {
            message: format!("join(other): could not materialize the right-hand frame ({e})."),
            cause: Box::new(e),
        },
        e => e,
    })?;
    let (left_cols, right_cols) = (left.names(), right.names());
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
    // A same-named non-key column on both sides would collide in the output
    // (`semi` and `anti` keep the left columns only).
    let right_set: HashSet<&String> = right_cols.iter().filter(|c| !on.contains(c)).collect();
    let collide: BTreeSet<&String> = left_cols
        .iter()
        .filter(|c| how.keeps_right() && !on.contains(c) && right_set.contains(c))
        .collect();
    if !collide.is_empty() {
        return Err(PlanError::Value(format!(
            "join produces duplicate column name(s) {}; rename() one side before joining.",
            py_list(collide.into_iter())
        )));
    }
    Ok(crate::lower::join(left, right, on, how)?)
}

// --- relational steps --------------------------------------------------------

/// Lower relational `steps` onto `frame`, in the order written (#191). `roles`
/// resolves `ur.src()` / `ur.dst()` / `ur.id()` in their expressions.
fn lower_steps(
    frame: &mut Frame,
    steps: &[&Plan],
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<()> {
    let mut i = 0;
    while i < steps.len() {
        let step = steps[i];
        if step.step().is_some() && step.invalid() {
            return Err(inputs.step_error(step.step().expect("checked")));
        }
        match step {
            Plan::Filter { predicate, .. } => {
                let compiled = filter_expr(predicate, roles, frame)?;
                frame.filter(compiled)?;
            }
            Plan::Select { columns, .. } => select(frame, columns, roles)?,
            Plan::Rename { mapping, .. } => {
                if let Some((old, _)) = mapping.iter().find(|(old, _)| !frame.has_column(old)) {
                    return Err(unknown_column("rename()", old, &frame.names()));
                }
                let renamed: Vec<String> = frame
                    .names()
                    .into_iter()
                    .map(|n| {
                        mapping
                            .iter()
                            .find(|(old, _)| *old == n)
                            .map_or(n, |(_, new)| new.clone())
                    })
                    .collect();
                check_unique("rename()", renamed.iter())?;
                if !mapping.is_empty() {
                    frame.rename(mapping)?;
                }
            }
            Plan::Sort { by, .. } => {
                let keys = sort_keys(frame, by, roles)?;
                // A head written right after the sort is its top-k.
                let limit = match steps.get(i + 1) {
                    Some(Plan::Head {
                        n, invalid: false, ..
                    }) => {
                        i += 1;
                        Some(head_n(n)?)
                    }
                    _ => None,
                };
                if !keys.is_empty() {
                    frame.sort(keys, limit)?;
                } else if let Some(n) = limit {
                    frame.head(n)?;
                }
            }
            Plan::Head { n, .. } => frame.head(head_n(n)?)?,
            Plan::Distinct { .. } => frame.distinct()?,
            Plan::Sample { .. } => {
                let handle = step
                    .step()
                    .ok_or_else(|| PlanError::Invalid("a sample step has no host handle".into()))?;
                frame.sample(|rows| inputs.sample(handle, rows))?;
            }
            Plan::GroupByAgg { keys, aggs, .. } => group_by(frame, keys, aggs)?,
            other => {
                return Err(PlanError::Invalid(format!(
                    "the '{}' step is not a relational step",
                    other.op()
                )))
            }
        }
        i += 1;
    }
    Ok(())
}

/// A `head` count: a negative one keeps no rows, as a slice of that length did.
fn head_n(n: &IntArg) -> PlanResult<usize> {
    Ok(match n.int("n")? {
        Some(i) if i < 0 => 0,
        Some(i) => usize::try_from(i).unwrap_or(usize::MAX),
        None => usize::MAX,
    })
}

/// A `filter` predicate over the frame: the checks and errors filters have
/// always had ([`host_check`]), then lowered with comparisons resolved.
fn filter_expr(predicate: &Expr, roles: &Roles, frame: &Frame) -> PlanResult<DfExpr> {
    let Expr::Scalar(v) = predicate else {
        return require_predicate(&Value::Null).map(|_| unreachable!());
    };
    require_predicate(v)?;
    host_check(v, roles, Some(&frame.schema()))?;
    Ok(crate::eval::logical(
        &resolve_roles(v, roles)?,
        frame.df_schema(),
    )?)
}

/// `select`: column names and expressions, in order. An expression's column is
/// named by its alias, or else by its first column (as in Polars).
fn select(frame: &mut Frame, items: &[SelectItem], roles: &Roles) -> PlanResult<()> {
    let missing: Vec<&String> = items
        .iter()
        .filter_map(|c| match c {
            SelectItem::Name(n) if !frame.has_column(n) => Some(n),
            _ => None,
        })
        .collect();
    if let Some(c) = missing.first() {
        return Err(unknown_column("select()", c, &frame.names()));
    }
    let mut exprs = Vec::with_capacity(items.len());
    for item in items {
        exprs.push(match item {
            SelectItem::Name(n) => col_named(n),
            SelectItem::Expr(v) => {
                let e = scalar_expr(frame, v, roles, "select()")?;
                let name = expr_name(&resolve_roles(v, roles)?);
                match e {
                    DfExpr::Alias(_) => e,
                    e => e.alias(name),
                }
            }
        });
    }
    check_unique(
        "select()",
        exprs.iter().map(|e| match e {
            DfExpr::Alias(a) => &a.name,
            DfExpr::Column(c) => &c.name,
            _ => unreachable!("select items are columns or aliases"),
        }),
    )?;
    frame.project(exprs)?;
    Ok(())
}

/// A step's output keeps one column per name.
fn check_unique<'a>(step: &str, names: impl Iterator<Item = &'a String>) -> PlanResult<()> {
    let mut seen = BTreeSet::new();
    for n in names {
        if !seen.insert(n) {
            return Err(PlanError::Value(format!(
                "{step} would produce duplicate column {}.",
                py_str(n)
            )));
        }
    }
    Ok(())
}

/// The output name of an unaliased expression: its first column's, else
/// `literal`.
fn expr_name(v: &Value) -> String {
    match kind(v) {
        "col" => v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "alias" => v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "binary" => {
            let l = v.get("left").unwrap_or(&Value::Null);
            if value_columns(l, &[]).is_empty() {
                expr_name(v.get("right").unwrap_or(&Value::Null))
            } else {
                expr_name(l)
            }
        }
        "unary" | "agg" => expr_name(v.get("operand").unwrap_or(&Value::Null)),
        _ => "literal".to_string(),
    }
}

/// The keys of a `sort`, checked against the frame: a column (which must exist)
/// or an expression.
fn sort_keys(frame: &Frame, by: &[SortKey], roles: &Roles) -> PlanResult<Vec<(DfExpr, bool)>> {
    for k in by {
        if let Some(c) = &k.column {
            if !frame.has_column(c) {
                return Err(unknown_column("sort()", c, &frame.names()));
            }
        }
    }
    by.iter()
        .map(|k| {
            let e = match (&k.column, &k.expr) {
                (Some(c), _) => col_named(c),
                (None, Some(v)) => scalar_expr(frame, v, roles, "sort()")?,
                (None, None) => {
                    return Err(PlanError::Invalid(
                        "a sort key needs a column or an expr".into(),
                    ))
                }
            };
            Ok((e, k.descending))
        })
        .collect()
}

/// `group_by(keys).agg(aggs)`, its columns checked as they always were.
fn group_by(frame: &mut Frame, keys: &[String], aggs: &[AggSpec]) -> PlanResult<()> {
    use datafusion::functions_aggregate::expr_fn::{avg, count, count_distinct, max, min, sum};
    for k in keys {
        if !frame.has_column(k) {
            return Err(unknown_column("group_by()", k, &frame.names()));
        }
    }
    for a in aggs {
        if !frame.has_column(&a.column) {
            return Err(unknown_column("agg()", &a.column, &frame.names()));
        }
    }
    use crate::agg::{float_agg, has_nulls, FloatAgg};
    // The rows, in order: a float sum's NaN rule depends on whether its column
    // holds a null.
    let rows = frame.ordered_batches()?;
    let row_schema = rows[0].schema();
    let schema = frame.schema();
    let mut exprs = Vec::with_capacity(aggs.len());
    let mut ordered = false;
    for a in aggs {
        // A dictionary (categorical) operand aggregates its values.
        let mut operand = col_named(&a.column);
        let mut dt = schema.field_with_name(&a.column)?.data_type().clone();
        if let DataType::Dictionary(_, value) = dt {
            dt = value.as_ref().clone();
            operand = cast_to(operand, dt.clone());
        }
        // Booleans and integers average in f64, folded in input order like
        // pyarrow's; a boolean sum counts the trues.
        let numeric = dt.is_integer() || dt == DataType::Boolean;
        if numeric && a.func == "mean" {
            operand = cast_to(operand, DataType::Float64);
            dt = DataType::Float64;
        } else if dt == DataType::Boolean && a.func == "sum" {
            operand = cast_to(operand, DataType::UInt64);
        }
        if dt.is_floating() {
            let kind = match a.func.as_str() {
                "sum" => Some(FloatAgg::Sum),
                "mean" => Some(FloatAgg::Mean),
                "min" => Some(FloatAgg::Min),
                "max" => Some(FloatAgg::Max),
                _ => None,
            };
            if let Some(kind) = kind {
                let nulls = has_nulls(&rows, row_schema.index_of(&a.column)?);
                exprs.push(float_agg(kind, &dt, nulls, operand).alias(&a.output));
                ordered = true;
                continue;
            }
        }
        let e = match a.func.as_str() {
            "mean" => avg(operand),
            "sum" => sum(operand),
            "min" => min(operand),
            "max" => max(operand),
            "count" => count(operand),
            "n_unique" => count_distinct(operand),
            other => {
                return not_implemented(format!(
                    "aggregation {} is not supported (use count, max, mean, min, n_unique, sum).",
                    py_str(other)
                ))
            }
        };
        exprs.push(e.alias(&a.output));
    }
    frame.group_by(keys.iter().map(|k| col_named(k)).collect(), exprs, ordered)?;
    Ok(())
}

fn cast_to(e: DfExpr, dt: DataType) -> DfExpr {
    DfExpr::Cast(datafusion::logical_expr::Cast::new(Box::new(e), dt))
}

/// Compile a filter predicate that runs outside an engine query against `schema`,
/// with the checks and errors the host's evaluator had (`_eval_pyarrow`).
fn host_predicate(
    predicate: &Expr,
    roles: &Roles,
    schema: &arrow::datatypes::SchemaRef,
) -> PlanResult<crate::eval::Compiled> {
    let Expr::Scalar(v) = predicate else {
        return require_predicate(&Value::Null).map(|_| unreachable!());
    };
    require_predicate(v)?;
    host_check(v, roles, Some(schema))?;
    Ok(crate::eval::compile(&resolve_roles(v, roles)?, schema)?)
}

/// The binary operators the host's evaluator supported.
const HOST_BINARY_OPS: [&str; 12] = [
    "+", "-", "*", "/", ">", ">=", "<", "<=", "==", "!=", "&", "|",
];

/// Walk a predicate the way the host's evaluator did, raising its errors in its
/// order: an unknown column (checked against `schema`, when given), a role not
/// available here, an operator or node it doesn't evaluate.
fn host_check(
    v: &Value,
    roles: &Roles,
    schema: Option<&arrow::datatypes::SchemaRef>,
) -> PlanResult<()> {
    let has = |name: &str| schema.is_none_or(|s| s.index_of(name).is_ok());
    let unknown = |name: &str| {
        let names: Vec<String> = schema
            .map(|s| s.fields().iter().map(|f| f.name().clone()).collect())
            .unwrap_or_default();
        Err(unknown_column("filter()", name, &names))
    };
    let k = kind(v);
    match k {
        "col" => {
            let name = v.get("name").and_then(Value::as_str).unwrap_or("");
            if !has(name) {
                return unknown(name);
            }
            Ok(())
        }
        "lit" => check_literals(v, "filter"),
        "src" | "dst" | "id" => {
            let Some((_, column)) = roles.iter().find(|(r, _)| *r == k) else {
                // The same message as an engine filter's.
                return resolve_roles(v, roles).map(|_| ());
            };
            if !has(column) {
                return unknown(column);
            }
            Ok(())
        }
        "binary" => {
            let op = v.get("op").and_then(Value::as_str).unwrap_or("");
            if !HOST_BINARY_OPS.contains(&op) {
                return not_implemented(format!(
                    "filter operator {} is not supported.",
                    py_str(op)
                ));
            }
            for side in ["left", "right"] {
                host_check(v.get(side).unwrap_or(&Value::Null), roles, schema)?;
            }
            Ok(())
        }
        "unary" => {
            let op = v.get("op").and_then(Value::as_str).unwrap_or("");
            if op != "~" {
                return not_implemented(format!(
                    "unary filter operator {} is not supported.",
                    py_str(op)
                ));
            }
            host_check(v.get("operand").unwrap_or(&Value::Null), roles, schema)
        }
        other => not_implemented(format!(
            "filter expression node {} is not supported (aggregations belong in group_by).",
            py_str(other)
        )),
    }
}

/// The columns a scan must read for `steps` (projection pushdown), or `None` to
/// read them all.
///
/// Walks the steps in written order, tracking renames back to source names and
/// the columns `with_columns` computes (which no file holds), up to the first
/// `select` or `group_by`, the first step that narrows the columns. A `distinct`
/// before that reads every column. Without a narrowing step every column reaches
/// the output. `required` is always read.
fn projection(steps: &[&Plan], required: BTreeSet<String>, roles: &Roles) -> Option<Vec<String>> {
    // current name -> source column (None: no source column)
    let mut source: HashMap<String, Option<String>> = HashMap::new();
    let mut needed = required;
    let src = |source: &HashMap<String, Option<String>>, names: Vec<String>| -> Vec<String> {
        names
            .into_iter()
            .filter_map(|n| source.get(&n).cloned().unwrap_or(Some(n)))
            .collect()
    };
    let scalar = |v: &Value| -> Vec<String> { value_columns(v, roles).into_iter().collect() };
    for step in steps {
        match step {
            Plan::Filter { predicate, .. } => {
                let cols = expr_columns(predicate, roles).into_iter().collect();
                needed.extend(src(&source, cols));
            }
            Plan::Sort { by, .. } => {
                for k in by {
                    let cols = match (&k.column, &k.expr) {
                        (Some(c), _) => vec![c.clone()],
                        (None, Some(v)) => scalar(v),
                        (None, None) => vec![],
                    };
                    needed.extend(src(&source, cols));
                }
            }
            Plan::WithColumns { columns, .. } => {
                for e in columns {
                    if let Expr::Scalar(v) = &e.expr {
                        needed.extend(src(&source, scalar(v)));
                    }
                }
                for e in columns {
                    source.insert(e.name.clone(), None); // computed, not read
                }
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
                for c in columns {
                    let cols = match c {
                        SelectItem::Name(n) => vec![n.clone()],
                        SelectItem::Expr(v) => scalar(v),
                    };
                    needed.extend(src(&source, cols));
                }
                return Some(needed.into_iter().collect());
            }
            Plan::GroupByAgg { keys, aggs, .. } => {
                let mut cols: BTreeSet<String> = keys.iter().cloned().collect();
                cols.extend(aggs.iter().map(|a| a.column.clone()));
                needed.extend(src(&source, cols.into_iter().collect()));
                return Some(needed.into_iter().collect());
            }
            Plan::Distinct { .. } | Plan::Join { .. } => return None,
            _ => {} // head / (position-based) sample read no column
        }
    }
    None
}

// --- expressions -------------------------------------------------------------

/// The operators valid at the top of a filter predicate.
const PREDICATE_OPS: [&str; 8] = [">", "<", ">=", "<=", "==", "!=", "&", "|"];

/// A step the host could not parse raises the host's error where the step is
/// read, as it did when the host parsed each step on reaching it.
fn check_step(step: &Plan, inputs: &dyn Inputs) -> PlanResult<()> {
    match step.step() {
        Some(handle) if step.invalid() => Err(inputs.step_error(handle)),
        _ => Ok(()),
    }
}

/// Reject a literal the host could not put in JSON (`{"kind": "lit",
/// "unrepresentable": <type>}`), with the error serializing it used to raise:
/// a `TypeError` for a value of another type, an engine JSON error for a
/// non-finite float. The host's own evaluator still reads the original value.
fn check_literals(v: &Value, what: &str) -> PlanResult<()> {
    if kind(v) == "lit" {
        if let Some(ty) = v.get("unrepresentable").and_then(Value::as_str) {
            if ty == "float" {
                return Err(PlanError::Engine(DataFusionError::Execution(format!(
                    "invalid {what} expression JSON: NaN and infinity have no JSON form"
                ))));
            }
            if let Some(reason) = v.get("reason").and_then(Value::as_str) {
                return Err(PlanError::Type(reason.to_string()));
            }
            return Err(PlanError::Type(format!(
                "Object of type {ty} is not JSON serializable"
            )));
        }
    }
    for key in ["left", "right", "operand"] {
        if let Some(e) = v.get(key) {
            check_literals(e, what)?;
        }
    }
    Ok(())
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

/// The error for a step that names a column the frame doesn't have (Python's
/// `ColumnNotFoundError`, which is also a `ValueError`), with the columns it has.
fn unknown_column(step: &str, name: &str, available: &[String]) -> PlanError {
    PlanError::ColumnNotFound(format!(
        "{step} references unknown column {}; the frame has {}.",
        py_str(name),
        py_list(available.iter())
    ))
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
    fn integer_means_fold_in_order_for_any_partition_count() {
        use crate::lower::{session, Frame, TEST_PARTITIONS};
        use arrow::array::{Float64Array, Int64Array};
        use arrow::datatypes::{Field, Schema};
        // Large values: their f64 sum depends on the order they are added in.
        let n = 50_000usize;
        let k: Vec<i64> = (0..n).map(|i| (i * 7 % 3) as i64).collect();
        let v: Vec<i64> = (0..n)
            .map(|i| (1i64 << 58) + (i as i64).wrapping_mul(0x9E37_79B9_7F4A) % (3i64 << 58))
            .collect();
        let mut sums = [0.0f64; 3];
        let mut counts = [0usize; 3];
        for (g, x) in k.iter().zip(&v) {
            sums[*g as usize] += *x as f64;
            counts[*g as usize] += 1;
        }
        let want: Vec<u64> = (0..3)
            .map(|g| (sums[g] / counts[g] as f64).to_bits())
            .collect();
        let schema = Arc::new(Schema::new(vec![
            Field::new("k", DataType::Int64, false),
            Field::new("v", DataType::Int64, false),
        ]));
        let batches: Vec<RecordBatch> = (0..n)
            .step_by(4096)
            .map(|s| {
                let e = (s + 4096).min(n);
                RecordBatch::try_new(
                    schema.clone(),
                    vec![
                        Arc::new(Int64Array::from(k[s..e].to_vec())),
                        Arc::new(Int64Array::from(v[s..e].to_vec())),
                    ],
                )
                .unwrap()
            })
            .collect();
        let mean = AggSpec {
            output: "m".into(),
            func: "mean".into(),
            column: "v".into(),
        };
        for p in [1, 3, 8] {
            TEST_PARTITIONS.with(|c| c.set(Some(p)));
            let mut f = Frame::from_batches(session(), batches.clone()).unwrap();
            group_by(&mut f, &["k".to_string()], std::slice::from_ref(&mean)).unwrap();
            let out = f.collect().unwrap();
            TEST_PARTITIONS.with(|c| c.set(None));
            let mut got = vec![0u64; 3];
            for b in &out {
                let ks = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
                let ms = b.column(1).as_any().downcast_ref::<Float64Array>().unwrap();
                for r in 0..b.num_rows() {
                    got[ks.value(r) as usize] = ms.value(r).to_bits();
                }
            }
            assert_eq!(got, want, "p={p}");
        }
    }

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
        );
        assert_eq!(got, Some(vec!["c".into(), "id".into(), "v".into()]));
    }

    #[test]
    fn int_args_convert_like_python_integers() {
        let arg = |v: serde_json::Value| IntArg(v);
        assert_eq!(arg(serde_json::json!(5)).u64("n").unwrap(), 5);
        assert_eq!(arg(serde_json::json!(true)).u32("n").unwrap(), 1);
        let msg = |e: PlanError| e.to_string();
        assert_eq!(
            msg(arg(serde_json::json!(-1)).u64("n").unwrap_err()),
            "can't convert negative int to unsigned"
        );
        assert_eq!(
            msg(arg(serde_json::json!(1e21)).u64("n").unwrap_err()),
            "int too big to convert"
        );
        assert_eq!(
            msg(arg(serde_json::json!(-1)).u32("n").unwrap_err()),
            "out of range integral type conversion attempted"
        );
        assert!(matches!(
            arg(serde_json::json!("x")).u64("seed"),
            Err(PlanError::Type(m)) if m == "argument 'seed': 'str' object cannot be interpreted as an integer"
        ));
        assert!(matches!(
            arg(serde_json::json!({"type": "Decimal"})).u64("seed"),
            Err(PlanError::Type(m)) if m.contains("'Decimal' object")
        ));
    }

    #[test]
    fn unparsed_graph_nodes_stay_scalar() {
        // The stub a graph verb serializes to as a predicate is not a GraphVerb;
        // it parses, so the filter can reject it with its own error.
        let p = plan(
            serde_json::json!({"op": "filter", "predicate": {"kind": "graph"},
            "input": src()}),
        );
        let Plan::Filter { predicate, .. } = &p else {
            unreachable!()
        };
        assert!(matches!(predicate, Expr::Scalar(_)));
        assert!(matches!(
            require_predicate(&serde_json::json!({"kind": "graph"})),
            Err(PlanError::NotImplemented(_))
        ));
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
        assert_eq!(view.induced_by.map(|i| i.traversal.op()), Some("hop"));

        let reshaped: EdgeInput = serde_json::from_value(serde_json::json!({"frame": 3, "plan":
            {"op": "distinct", "input": edges}}))
        .unwrap();
        assert!(matches!(reshaped.view(), Err(PlanError::NotImplemented(_))));
    }
}
