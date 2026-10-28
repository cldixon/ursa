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
//! the messages the Python layer used to raise), and maps each supported shape
//! onto the executors in [`crate::query`].
//!
//! Since #192 it also owns the leaf work around them, with one expression
//! evaluator ([`crate::expr`] lowered to DataFusion, [`crate::eval`] outside an
//! engine query):
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
//! storage options), its cached index and node-table cache, seed id sets, and
//! the relational steps other than `filter` that the engine's fixed-order tail
//! can't run in written order (the pyarrow tail, retired by #191).
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

use crate::expr::GraphVerb;
use crate::query::{
    execute_hop_query, execute_join_query, execute_node_query_cached, execute_path_query,
    execute_walk_query, parse_direction, resolve_dense, NodeColumn,
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
/// from, so the host can run that step itself when the engine's fixed-order tail
/// can't (see [`Inputs::apply_steps`]). A step whose arguments the host could not
/// parse is sent with `invalid` set and its arguments left out: the executor
/// raises the host's error ([`Inputs::step_error`]) where it would have read the
/// step, so a guard that fires earlier still wins.
///
/// Integer arguments are [`IntArg`]s, typed only where the engine reads them: the
/// host's own evaluator takes the original step and whatever it accepts.
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
        columns: Vec<String>,
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
/// [`NodeTableCache`], seeds, and the relational steps the engine's tail can't
/// run in written order (the pyarrow tail, retired by #191). Everything else
/// (scans, masks, weights, filters) runs here.
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

    /// Run relational steps on `rows`, in order, with `roles` resolving role
    /// references. These are the non-`filter` steps the engine's tail can't run as
    /// written (and those on a plain source), each identified by its `step`
    /// handle.
    fn apply_steps(&self, rows: Rows, steps: &[&Plan], roles: &Roles) -> PlanResult<Rows>;

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

pub type PlanResult<T> = std::result::Result<T, PlanError>;

/// Execute a plan, reading its inputs from `inputs`. Returns at least one batch.
pub fn execute_plan(plan: &Plan, inputs: &dyn Inputs) -> PlanResult<Vec<RecordBatch>> {
    execute(plan, inputs)?.into_batches(inputs)
}

/// [`execute_plan`], leaving a result the host produced with the host.
pub fn execute(plan: &Plan, inputs: &dyn Inputs) -> PlanResult<Rows> {
    if let Plan::Invalid { step } = plan {
        return Err(inputs.step_error(*step));
    }
    let chain = plan.chain();
    if is_node_frame(&chain) {
        collect_node_frame(&chain, inputs)
    } else {
        collect_edge_frame(&chain, inputs)
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

/// A node frame's collect (`collect_node_frame`): `describe`, a random walk, or
/// a node query (graph columns over the node set or an attribute table, an
/// optional join, then the relational steps in the order written).
fn collect_node_frame(chain: &[&Plan], inputs: &dyn Inputs) -> PlanResult<Rows> {
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
        return Ok(Rows::Batches(vec![describe(&index.topo, *full, dir)?]));
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
            check_step(step, inputs)?;
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
        check_step(step, inputs)?;
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
) -> PlanResult<Rows> {
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
        Some(slot) => {
            Some(node_rows(chain, slot, file_needed.as_deref(), inputs)?.into_batches(inputs)?)
        }
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
    // The engine runs the leading steps its fixed-order tail executes identically;
    // any later step runs in written order on its output. ur.id() resolves to the
    // result's reserved `id` column.
    let (pushed, rest) = split_engine_tail(steps, true)?;
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
    let mut needed: BTreeSet<String> = BTreeSet::from([view.roles.0.clone(), view.roles.1.clone()]);
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

/// A plain node source and its steps, all run by the host (`_collect_plain_nodes`).
fn collect_plain_nodes(
    chain: &[&Plan],
    id_col: &str,
    steps: &[&Plan],
    inputs: &dyn Inputs,
) -> PlanResult<Rows> {
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
    let table = node_rows(chain, slot, needed.as_deref(), inputs)?;
    apply_steps(table, steps, &roles, inputs)
}

/// A random walk and its relational steps (`_collect_random_walk`).
fn collect_random_walk(chain: &[&Plan], walk: &Plan, inputs: &dyn Inputs) -> PlanResult<Rows> {
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
        check_step(s, inputs)?;
        steps.push(s);
    }
    let index = require_index(edges, inputs)?;
    let starts = inputs.seeds(*start)?;
    // A walk over a subgraph view follows only its edges (#192).
    let mask = edge_mask(edges, &index, inputs)?;
    let (pushed, rest) = split_engine_tail(&steps, false)?;
    // Walk output columns are (walk_id, step, node); no role applies.
    let filters = filter_jsons(&pushed.filters, &[])?;
    let result = execute_walk_query(
        index.topo,
        index.ids,
        mask,
        starts.as_ref(),
        n_steps.u32("steps")?,
        walks_per_node.u32("walks_per_node")?,
        seed_arg(seed, "seed")?,
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
fn collect_edge_frame(chain: &[&Plan], inputs: &dyn Inputs) -> PlanResult<Rows> {
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
                check_step(step, inputs)?;
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
    let (pushed, rest) = split_engine_tail(&steps, false)?;
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
            // A traversal over a subgraph view follows only its edges (#192).
            let mask = edge_mask(edges, &index, inputs)?;
            execute_hop_query(
                index.topo,
                index.ids,
                mask,
                seeds.as_ref(),
                n.u32("n")?,
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
            execute_path_query(
                index.topo,
                index.ids,
                mask,
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

/// A plain edge source and its steps. A scan reads the columns the steps need and
/// takes its leading filters with it (#192); the other steps run in order on its
/// rows.
fn collect_plain_edges(
    chain: &[&Plan],
    steps: &[&Plan],
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<Rows> {
    // The endpoints plus what the steps read before a select or group_by narrows
    // the columns.
    let required: BTreeSet<String> = roles.iter().map(|(_, c)| c.clone()).collect();
    let needed = projection(steps, required, roles, &HashSet::new(), false);
    let Some(slot) = source_slot(chain) else {
        return not_implemented(NO_EDGE_SOURCE);
    };
    let Plan::ScanEdges {
        path,
        src,
        dst,
        on_null,
        ..
    } = chain[0]
    else {
        let Some(table) = inputs.table(slot)? else {
            return not_implemented(NO_EDGE_SOURCE);
        };
        return apply_steps(table, steps, roles, inputs);
    };
    let path = single_path(path, MULTI_PATH_EDGES)?;
    let opts = inputs.storage_options(slot)?;
    let drop_nulls = on_null == "drop";
    // The leading filters go into the scan, unless `on_null="drop"` must count
    // the null rows of the whole file first.
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
    apply_steps(
        Rows::Batches(rows.batches),
        &steps[n_pushed..],
        roles,
        inputs,
    )
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
    left: Rows,
    join: &Plan,
    steps: &[&Plan],
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<Rows> {
    let Plan::Join { right, on, how, .. } = join else {
        unreachable!("run_join takes a join step")
    };
    let left = left.into_batches(inputs)?;
    let right = execute_plan(right, inputs).map_err(|e| match e {
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
    let (pushed, rest) = split_engine_tail(steps, false)?;
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
fn split_engine_tail<'a>(
    steps: &[&'a Plan],
    group_by: bool,
) -> PlanResult<(EngineTail<'a>, Vec<&'a Plan>)> {
    let mut out = EngineTail::default();
    let mut filled: HashSet<&str> = HashSet::new();
    let mut pos = 0; // the earliest ENGINE_ORDER slot still fillable
    for (i, &step) in steps.iter().enumerate() {
        if let Plan::Filter { predicate, .. } = step {
            if filled.iter().all(|k| matches!(*k, "distinct" | "sort")) {
                let Expr::Scalar(v) = predicate else {
                    return Ok((out, steps[i..].to_vec()));
                };
                out.filters.push(v);
                continue;
            }
            return Ok((out, steps[i..].to_vec()));
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
            return Ok((out, steps[i..].to_vec()));
        }
        match step {
            Plan::GroupByAgg { keys, aggs, .. } => {
                out.group_by = Some((keys.clone(), aggs.iter().map(agg_json).collect()))
            }
            Plan::Distinct { .. } => out.distinct = true,
            Plan::Sample { n, seed, .. } => {
                out.sample = Some((n.usize("sample")?, seed_arg(seed, "sample")?))
            }
            Plan::Sort { by, .. } => {
                out.sort = Some(
                    by.iter()
                        .map(|k| (k.column.clone(), k.descending))
                        .collect(),
                )
            }
            Plan::Head { n, .. } => out.limit = Some(n.usize("limit")?),
            Plan::Rename { mapping, .. } => out.rename = mapping.clone(),
            _ => out.select = Some(step), // select
        }
        filled.insert(op);
        pos = k + 1;
    }
    Ok((out, vec![]))
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
) -> PlanResult<Rows> {
    if pushed.select.is_none() && rest.is_empty() {
        return Ok(Rows::Batches(result));
    }
    let steps: Vec<&Plan> = pushed
        .select
        .into_iter()
        .chain(rest.iter().copied())
        .collect();
    apply_steps(Rows::Batches(result), &steps, roles, inputs)
}

/// Run `steps` on `rows` in order: each run of `filter`s here, every other step on
/// the host (its pyarrow tail, until #191).
fn apply_steps(
    rows: Rows,
    steps: &[&Plan],
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<Rows> {
    let mut rows = rows;
    let mut rest = steps;
    while let Some(first) = rest.first() {
        let is_filter = matches!(first, Plan::Filter { .. });
        let n = rest
            .iter()
            .take_while(|s| matches!(s, Plan::Filter { .. }) == is_filter)
            .count();
        let (run, tail) = rest.split_at(n);
        rows = if is_filter {
            let mut batches = rows.into_batches(inputs)?;
            for step in run {
                let Plan::Filter { predicate, .. } = step else {
                    unreachable!("a run of filters")
                };
                let compiled = host_predicate(predicate, roles, &batches[0].schema())?;
                batches = crate::eval::filter_batches(batches, &[compiled])?;
            }
            Rows::Batches(batches)
        } else {
            if let Some(step) = run.iter().find(|s| s.step().is_none()) {
                return Err(PlanError::Invalid(format!(
                    "the '{}' step has no host handle, so its host can't run it",
                    step.op()
                )));
            }
            inputs.apply_steps(rows, run, roles)?
        };
        rest = tail;
    }
    Ok(rows)
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
        Err(PlanError::Value(format!(
            "filter references unknown column '{name}'."
        )))
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
            check_literals(p, "filter")?;
            Ok(resolve_roles(p, roles)?.to_string())
        })
        .collect()
}

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
        let (pushed, rest) = split_engine_tail(&steps, true).unwrap();
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
            let (_, rest) = split_engine_tail(&steps, true).unwrap();
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
