//! Frames as plan objects (#193): the tree a frame *is*, built step by step.
//!
//! A frame holds one [`Node`]: its last step, which holds the node before it
//! (and a graph node the edge frame it runs over, a join its right side). Each
//! frame method builds a new node on top of the one it is called on; nothing is
//! copied, so a pipeline shares its prefix with every frame derived from it.
//!
//! A node is validated as it is built ([`crate::infer`]): the step's shape, its
//! arguments and the columns it names are checked against the frame it is added
//! to, and an error is raised by the method that added the step. What only the
//! rows can tell (a null endpoint, a value out of range, a file that changed)
//! is still found by `collect()`.
//!
//! To run, a tree is lowered to the [`Plan`] IR ([`Lowering`]), with each frame
//! and seed set it reads numbered as a slot, and executed by [`crate::plan`]
//! through [`TreeInputs`], which reads each slot back from the node it names:
//!
//! - a source's rows: an in-memory table the host holds, or a scan;
//! - the caches of a frame ([`State`]): its topology index (which carries its
//!   weights, masks and symmetric views) and a scanned node table, shared by a
//!   frame and every derivation that keeps its rows, so a build through any of
//!   them serves all, including derivations made before the build;
//! - seed sets: a node frame (run here) or a value the host converts.
//!
//! The host ([`Host`]) is what stays with the frame's owner: its tables and the
//! endpoint arrays it converted, a scan's storage options, the seeded draw of a
//! `sample`, warnings, and the errors it met converting a step's arguments.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use arrow::array::{Array, ArrayRef, RecordBatch};
use arrow::datatypes::{DataType, SchemaRef};
use serde_json::Value;

use crate::expr::GraphVerb;
use crate::infer::Info;
use crate::plan::{
    pending, AggSpec, EdgeInput, Expr, GraphIndex, HostRows, Inputs, IntArg, NamedExpr, Plan,
    PlanError, PlanResult, Rows, ScanPath, SelectItem, Slot, SortKey, SubPlan,
};
use crate::scan::{CsvOptions, NodeTableCache};

/// An object the host owns (a table, an argument it could not convert, ...).
pub type HostHandle = Arc<dyn Any + Send + Sync>;

/// What a frame tree reads back from its host. Each method is called only when
/// the value is needed, in the order the executor needs it.
pub trait Host: Send + Sync {
    /// An in-memory table's batches (at least one, so an empty table keeps its
    /// schema).
    fn batches(&self, table: &HostHandle) -> PlanResult<Vec<RecordBatch>>;
    /// The `(src, dst)` batches an in-memory edge source builds its index from:
    /// its canonical endpoint arrays (`endpoints`), swapped when `reversed`.
    fn edge_batches(&self, endpoints: &HostHandle, reversed: bool) -> PlanResult<Vec<RecordBatch>>;
    /// A scan's storage options (or the error for options it can't pass).
    fn storage_options(&self, spec: &HostHandle) -> PlanResult<HashMap<String, String>>;
    /// A seed set given as a value (an iterable of ids): its ids, int64 or string.
    fn seeds(&self, value: &HostHandle) -> PlanResult<ArrayRef>;
    /// The positions (ascending) a `sample` with arguments `draw` keeps out of
    /// `rows` rows, or `None` for all of them.
    fn sample(&self, draw: &HostHandle, rows: usize) -> PlanResult<Option<Vec<usize>>>;
    /// Tell the user `on_null="drop"` removed `n` edge rows.
    fn warn_dropped(&self, n: usize) -> PlanResult<()>;
    /// A token naming an in-memory table, for the reshaped-topology cache key.
    fn identity(&self, table: &HostHandle) -> PlanResult<Option<String>>;
    /// The error the host met converting a step's arguments.
    fn error(&self, error: &HostHandle) -> PlanError;
}

// --- the tree -------------------------------------------------------------------

/// A frame's last step, holding the steps before it.
pub struct Node {
    pub(crate) op: Op,
    pub(crate) kind: Kind,
    pub(crate) state: Arc<State>,
    /// This frame's ids as a seed set, once it had to run to get them.
    seeds: OnceLock<ArrayRef>,
    /// The host's object for `seeds`, once it has one.
    pub seeds_host: OnceLock<HostHandle>,
    info: OnceLock<Info>,
}

/// Which frame type a node is, with its role columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Edges { src: String, dst: String },
    Nodes { id: String },
}

/// A node's step.
pub enum Op {
    /// An in-memory table: an edge frame's whole edge table with its canonical
    /// endpoint arrays (`endpoints`), or a node frame's attribute table.
    FromArrow {
        table: HostHandle,
        schema: SchemaRef,
        endpoints: Option<HostHandle>,
    },
    ScanEdges {
        path: Option<ScanPath>,
        on_null: String,
        spec: HostHandle,
        csv: CsvOptions,
    },
    ScanNodes {
        path: Option<ScanPath>,
        spec: HostHandle,
        csv: CsvOptions,
    },
    /// A relational step. `error` is the host's error for arguments it could not
    /// convert (the step's arguments are then left out).
    Step {
        input: Arc<Node>,
        step: Step,
        error: Option<HostHandle>,
    },
    Nodes {
        input: Arc<Node>,
    },
    Reverse {
        input: Arc<Node>,
    },
    Hop {
        edges: Arc<Node>,
        n: IntArg,
        direction: String,
        seeds: Seeds,
    },
    ShortestPath {
        edges: Arc<Node>,
        source: Seeds,
        target: Seeds,
        weight: Option<Value>,
        direction: String,
    },
    RandomWalk {
        edges: Arc<Node>,
        start: Seeds,
        steps: IntArg,
        walks_per_node: IntArg,
        seed: Option<IntArg>,
    },
    Describe {
        edges: Arc<Node>,
        full: bool,
        direction: String,
    },
}

/// A relational step's arguments.
pub enum Step {
    Filter(Expr),
    Select(Vec<SelectItem>),
    WithColumns(Vec<Column>),
    Rename(Vec<(String, String)>),
    Sort(Vec<SortKey>),
    Head(IntArg),
    /// `draw` is the host's own `(n, seed)`, which the rows are drawn with.
    Sample {
        n: IntArg,
        seed: Option<IntArg>,
        draw: HostHandle,
    },
    Distinct,
    Join {
        right: Arc<Node>,
        on: Vec<String>,
        how: String,
    },
    GroupByAgg {
        keys: Vec<String>,
        aggs: Vec<AggSpec>,
    },
}

/// One `with_columns` output.
pub struct Column {
    pub name: String,
    pub expr: ColumnExpr,
}

/// A `with_columns` expression: a graph verb and the edge frame it runs over,
/// or any other expression (its IR).
pub enum ColumnExpr {
    Scalar(Value),
    Graph {
        verb: Box<GraphVerb>,
        edges: Option<Arc<Node>>,
    },
}

/// A seed set: a node frame's ids, or a value the host converts.
#[derive(Clone)]
pub enum Seeds {
    Frame(Arc<Node>),
    Value(HostHandle),
}

/// The caches a frame shares with the derivations that keep its rows.
#[derive(Default)]
pub struct State {
    /// The topology index of an edge frame's rows (with its weight, mask and
    /// symmetric-view caches).
    pub index: IndexCell,
    /// A `scan_nodes` frame's scanned table.
    pub node_cache: OnceLock<Arc<NodeTableCache>>,
    /// The host's (weak) handle on its object for this state's index cell, so
    /// the host hands out one object per cell.
    pub host_cell: Mutex<Option<HostHandle>>,
    /// A lock the host hands out with the cell, for code that sets its index.
    pub host_lock: OnceLock<HostHandle>,
    /// The host's object for `node_cache`, once it has one.
    pub host_cache: OnceLock<HostHandle>,
}

/// A frame's index, built once: concurrent first collects wait for one build.
#[derive(Default)]
pub struct IndexCell {
    value: Mutex<Option<CellValue>>,
    /// Held while the index is built (never while waiting for the host's lock
    /// on `value`).
    build: Mutex<()>,
}

/// A built index, and the host's handle on it once the host has one.
#[derive(Clone)]
pub struct CellValue {
    pub index: GraphIndex,
    pub host: Option<HostHandle>,
}

impl IndexCell {
    /// The index, if built.
    pub fn get(&self) -> Option<GraphIndex> {
        self.lock().as_ref().map(|v| v.index.clone())
    }

    /// The cell's value, with the host's handle on it.
    pub fn value(&self) -> Option<CellValue> {
        self.lock().clone()
    }

    /// Replace the value (the host setting it).
    pub fn set_value(&self, value: Option<CellValue>) {
        // The old value (and the host's handle on it) is dropped after the lock.
        let old = std::mem::replace(&mut *self.lock(), value);
        drop(old);
    }

    /// Keep `index` unless one is already built; the cell's index either way.
    pub fn put(&self, index: GraphIndex) -> GraphIndex {
        let mut value = self.lock();
        match value.as_ref() {
            Some(v) => v.index.clone(),
            None => {
                *value = Some(CellValue {
                    index: index.clone(),
                    host: None,
                });
                index
            }
        }
    }

    /// Record the host's handle on the cell's current index (if it is `index`).
    pub fn set_host(&self, index: &GraphIndex, host: HostHandle) {
        let mut value = self.lock();
        if let Some(v) = value.as_mut() {
            if Arc::ptr_eq(&v.index.topo, &index.topo) && v.host.is_none() {
                v.host = Some(host);
            }
        }
    }

    /// The index, built by `build` if no one has built it (once, under the
    /// build lock).
    fn get_or_build(
        &self,
        build: impl FnOnce() -> PlanResult<GraphIndex>,
    ) -> PlanResult<GraphIndex> {
        if let Some(i) = self.get() {
            return Ok(i);
        }
        let _building = self.build.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(i) = self.get() {
            return Ok(i);
        }
        let index = build()?;
        Ok(self.put(index))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<CellValue>> {
        self.value.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A node that stands in for a child a dropped node no longer holds.
fn placeholder() -> Arc<Node> {
    static LEAF: OnceLock<Arc<Node>> = OnceLock::new();
    LEAF.get_or_init(|| {
        Arc::new(Node {
            op: Op::FromArrow {
                table: Arc::new(()),
                schema: Arc::new(arrow::datatypes::Schema::empty()),
                endpoints: None,
            },
            kind: Kind::Nodes { id: String::new() },
            state: Arc::default(),
            seeds: OnceLock::new(),
            seeds_host: OnceLock::new(),
            info: OnceLock::new(),
        })
    })
    .clone()
}

/// Move a node's children (the frames its step reads, and those its validation
/// keeps) into `out`, leaving placeholders.
fn take_children(node: &mut Node, out: &mut Vec<Arc<Node>>) {
    let mut take = |child: &mut Arc<Node>| out.push(std::mem::replace(child, placeholder()));
    let seeds = |s: &mut Seeds, take: &mut dyn FnMut(&mut Arc<Node>)| {
        if let Seeds::Frame(n) = s {
            take(n);
        }
    };
    match &mut node.op {
        Op::FromArrow { .. } | Op::ScanEdges { .. } | Op::ScanNodes { .. } => {}
        Op::Step { input, step, .. } => {
            take(input);
            match step {
                Step::Join { right, .. } => take(right),
                Step::WithColumns(columns) => {
                    for c in columns {
                        if let ColumnExpr::Graph {
                            edges: Some(edges), ..
                        } = &mut c.expr
                        {
                            take(edges);
                        }
                    }
                }
                _ => {}
            }
        }
        Op::Nodes { input } | Op::Reverse { input } => take(input),
        Op::Hop {
            edges, seeds: s, ..
        } => {
            take(edges);
            seeds(s, &mut take);
        }
        Op::ShortestPath {
            edges,
            source,
            target,
            ..
        } => {
            take(edges);
            seeds(source, &mut take);
            seeds(target, &mut take);
        }
        Op::RandomWalk { edges, start, .. } => {
            take(edges);
            seeds(start, &mut take);
        }
        Op::Describe { edges, .. } => take(edges),
    }
    if let Some(info) = node.info.take() {
        out.extend(info.pending.into_iter().flatten());
        out.extend(info.chain.anchor);
    }
}

impl Drop for Node {
    /// Drop a chain one node at a time: a frame of many steps would otherwise
    /// drop them recursively, a stack frame per step.
    fn drop(&mut self) {
        let mut stack = vec![];
        take_children(self, &mut stack);
        while let Some(child) = stack.pop() {
            if let Ok(mut node) = Arc::try_unwrap(child) {
                take_children(&mut node, &mut stack);
            }
        }
    }
}

/// The steps of the `ops` that keep an edge frame's rows, so the frame keeps its
/// parent's index (a filter is a subgraph view of it).
fn keeps_index(step: &Step) -> bool {
    !matches!(
        step,
        Step::Sample { .. } | Step::Distinct | Step::Join { .. } | Step::GroupByAgg { .. }
    )
}

impl Node {
    /// Build a node and validate it ([`crate::infer`]); the error is the first
    /// one a collect of it would raise that the plan alone can tell.
    fn build(op: Op, kind: Kind, state: Arc<State>, host: &dyn Host) -> PlanResult<Arc<Node>> {
        let node = Arc::new(Node {
            op,
            kind,
            state,
            seeds: OnceLock::new(),
            seeds_host: OnceLock::new(),
            info: OnceLock::new(),
        });
        let info = crate::infer::validate(&node, host)?;
        let _ = node.info.set(info);
        Ok(node)
    }

    /// What validation knows of the frame.
    pub(crate) fn info(&self) -> &Info {
        static UNKNOWN: OnceLock<Info> = OnceLock::new();
        self.info
            .get()
            .unwrap_or_else(|| UNKNOWN.get_or_init(Info::default))
    }

    /// An in-memory edge frame over `table`, with its canonical endpoints.
    pub fn edge_table(
        table: HostHandle,
        schema: SchemaRef,
        endpoints: HostHandle,
        src: &str,
        dst: &str,
        host: &dyn Host,
    ) -> PlanResult<Arc<Node>> {
        let kind = Kind::Edges {
            src: src.into(),
            dst: dst.into(),
        };
        let op = Op::FromArrow {
            table,
            schema,
            endpoints: Some(endpoints),
        };
        Node::build(op, kind, Arc::default(), host)
    }

    /// An in-memory node (attribute) frame over `table`.
    pub fn node_table(
        table: HostHandle,
        schema: SchemaRef,
        id: &str,
        host: &dyn Host,
    ) -> PlanResult<Arc<Node>> {
        let op = Op::FromArrow {
            table,
            schema,
            endpoints: None,
        };
        Node::build(op, Kind::Nodes { id: id.into() }, Arc::default(), host)
    }

    /// `ur.scan_edges(path, src=, dst=, on_null=)`; `spec` holds the storage
    /// options, `csv` how a CSV file parses.
    pub fn scan_edges(
        path: Option<ScanPath>,
        src: &str,
        dst: &str,
        on_null: &str,
        spec: HostHandle,
        csv: CsvOptions,
        host: &dyn Host,
    ) -> PlanResult<Arc<Node>> {
        let kind = Kind::Edges {
            src: src.into(),
            dst: dst.into(),
        };
        let op = Op::ScanEdges {
            path,
            on_null: on_null.into(),
            spec,
            csv,
        };
        Node::build(op, kind, Arc::default(), host)
    }

    /// `ur.scan_nodes(path, id=)`.
    pub fn scan_nodes(
        path: Option<ScanPath>,
        id: &str,
        spec: HostHandle,
        csv: CsvOptions,
        host: &dyn Host,
    ) -> PlanResult<Arc<Node>> {
        let op = Op::ScanNodes { path, spec, csv };
        Node::build(op, Kind::Nodes { id: id.into() }, Arc::default(), host)
    }

    /// A relational step on this frame. An edge frame keeps its index through
    /// the steps that keep its rows; a node frame keeps its caches through all.
    pub fn step(
        self: &Arc<Node>,
        step: Step,
        error: Option<HostHandle>,
        host: &dyn Host,
    ) -> PlanResult<Arc<Node>> {
        let state = match self.kind {
            Kind::Edges { .. } if !keeps_index(&step) => Arc::default(),
            _ => self.state.clone(),
        };
        let op = Op::Step {
            input: self.clone(),
            step,
            error,
        };
        Node::build(op, self.kind.clone(), state, host)
    }

    /// `edges.nodes()`.
    pub fn nodes(self: &Arc<Node>, host: &dyn Host) -> PlanResult<Arc<Node>> {
        let op = Op::Nodes {
            input: self.clone(),
        };
        Node::build(op, Kind::Nodes { id: "id".into() }, Arc::default(), host)
    }

    /// `edges.reverse()`: the roles swapped, over a fresh index (the transpose).
    pub fn reverse(self: &Arc<Node>, host: &dyn Host) -> PlanResult<Arc<Node>> {
        let Kind::Edges { src, dst } = &self.kind else {
            return Err(PlanError::Invalid("reverse() takes an edge frame".into()));
        };
        let kind = Kind::Edges {
            src: dst.clone(),
            dst: src.clone(),
        };
        let op = Op::Reverse {
            input: self.clone(),
        };
        Node::build(op, kind, Arc::default(), host)
    }

    /// A traversal or graph summary over `edges`: a frame of its own.
    pub fn graph(op: Op, host: &dyn Host) -> PlanResult<Arc<Node>> {
        let kind = match &op {
            Op::Hop { .. } | Op::ShortestPath { .. } => Kind::Edges {
                src: "src".into(),
                dst: "dst".into(),
            },
            Op::RandomWalk { .. } => Kind::Nodes { id: "node".into() },
            Op::Describe { .. } => Kind::Nodes {
                id: "__summary__".into(),
            },
            _ => return Err(PlanError::Invalid("not a graph node".into())),
        };
        Node::build(op, kind, Arc::default(), host)
    }

    pub fn kind(&self) -> &Kind {
        &self.kind
    }

    pub fn op(&self) -> &Op {
        &self.op
    }

    pub fn state(&self) -> &Arc<State> {
        &self.state
    }

    /// The step before this one in the frame's own chain.
    pub fn input(&self) -> Option<&Arc<Node>> {
        match &self.op {
            Op::Step { input, .. } | Op::Nodes { input } | Op::Reverse { input } => Some(input),
            _ => None,
        }
    }

    /// The frame's steps from its first, in the order written.
    pub fn chain(&self) -> Vec<&Node> {
        let mut chain = vec![self];
        while let Some(input) = chain.last().and_then(|n| n.input()) {
            chain.push(input);
        }
        chain.reverse();
        chain
    }

    /// The step's name, as the frame API spells it.
    pub fn op_name(&self) -> &'static str {
        match &self.op {
            Op::FromArrow { .. } => "from_arrow",
            Op::ScanEdges { .. } => "scan_edges",
            Op::ScanNodes { .. } => "scan_nodes",
            Op::Step { step, .. } => match step {
                Step::Filter(_) => "filter",
                Step::Select(_) => "select",
                Step::WithColumns(_) => "with_columns",
                Step::Rename(_) => "rename",
                Step::Sort(_) => "sort",
                Step::Head(_) => "head",
                Step::Sample { .. } => "sample",
                Step::Distinct => "distinct",
                Step::Join { .. } => "join",
                Step::GroupByAgg { .. } => "group_by_agg",
            },
            Op::Nodes { .. } => "nodes",
            Op::Reverse { .. } => "reverse",
            Op::Hop { .. } => "hop",
            Op::ShortestPath { .. } => "shortest_path",
            Op::RandomWalk { .. } => "random_walk",
            Op::Describe { .. } => "describe",
        }
    }

    /// The source of this frame's own rows (not crossing `nodes()`), and how
    /// many `reverse` steps lie between it and this frame.
    fn source(&self) -> Option<(&Node, usize)> {
        let mut reversed = 0;
        let mut node = self;
        loop {
            match &node.op {
                Op::FromArrow { .. } | Op::ScanEdges { .. } | Op::ScanNodes { .. } => {
                    return Some((node, reversed))
                }
                Op::Reverse { input } => {
                    reversed += 1;
                    node = input;
                }
                Op::Step { input, .. } => node = input,
                _ => return None,
            }
        }
    }

    /// This frame's ids as a seed set, if it had to run to get them.
    pub fn cached_seeds(&self) -> Option<&ArrayRef> {
        self.seeds.get()
    }

    /// The host objects of this frame's source: its table, its canonical
    /// endpoint arrays, a scan's spec.
    pub fn source_handles(
        &self,
    ) -> (
        Option<&HostHandle>,
        Option<&HostHandle>,
        Option<&HostHandle>,
    ) {
        match self.source().map(|(s, _)| &s.op) {
            Some(Op::FromArrow {
                table, endpoints, ..
            }) => (Some(table), endpoints.as_ref(), None),
            Some(Op::ScanEdges { spec, .. } | Op::ScanNodes { spec, .. }) => {
                (None, None, Some(spec))
            }
            _ => (None, None, None),
        }
    }

    /// The in-memory table of this frame's source.
    fn table(&self) -> Option<&HostHandle> {
        match self.source()?.0 {
            Node {
                op: Op::FromArrow { table, .. },
                ..
            } => Some(table),
            _ => None,
        }
    }

    /// The edge roles of the frame (an edge frame's `src` / `dst`).
    fn roles(&self) -> Option<(&str, &str)> {
        match &self.kind {
            Kind::Edges { src, dst } => Some((src, dst)),
            Kind::Nodes { .. } => None,
        }
    }

    /// The id column of a node frame.
    pub fn id_col(&self) -> Option<&str> {
        match &self.kind {
            Kind::Nodes { id } => Some(id),
            Kind::Edges { .. } => None,
        }
    }
}

// --- lowering ------------------------------------------------------------------

/// What a slot of a lowered plan names.
enum SlotRef {
    Frame(Arc<Node>),
    Seeds(Seeds),
}

/// A tree lowered to the [`Plan`] IR: the plan, with the frames and seed sets
/// it reads numbered as slots, and the relational steps it holds by handle.
pub struct Lowering {
    slots: Vec<SlotRef>,
    frames: HashMap<*const Node, Slot>,
    steps: Vec<Arc<Node>>,
    /// Lowered for a shape check ([`check_shape`]): a node frame's steps whose
    /// check is their own are left out (see `infer::elides`), and a join's right
    /// side, which a shape check never reads, is a stub.
    shape: bool,
}

impl Lowering {
    fn new() -> Lowering {
        Lowering {
            slots: vec![],
            frames: HashMap::new(),
            steps: vec![],
            shape: false,
        }
    }

    /// Lower `node`'s frame.
    pub fn of(node: &Arc<Node>) -> (Plan, Lowering) {
        let mut l = Lowering::new();
        let plan = l.frame(node);
        (plan, l)
    }

    /// Lower an edge frame as the edge input of a graph node.
    pub fn edges_of(node: &Arc<Node>) -> (EdgeInput, Lowering) {
        let mut l = Lowering::new();
        let edges = l.edges(node);
        (edges, l)
    }

    fn frame_slot(&mut self, node: &Arc<Node>) -> Slot {
        let key = Arc::as_ptr(node);
        if let Some(s) = self.frames.get(&key) {
            return *s;
        }
        self.slots.push(SlotRef::Frame(node.clone()));
        let s = self.slots.len() - 1;
        self.frames.insert(key, s);
        s
    }

    fn seed_slot(&mut self, seeds: &Seeds) -> Slot {
        self.slots.push(SlotRef::Seeds(seeds.clone()));
        self.slots.len() - 1
    }

    fn edges(&mut self, node: &Arc<Node>) -> EdgeInput {
        let plan = self.frame(node);
        EdgeInput {
            plan: SubPlan::new(plan),
            frame: self.frame_slot(node),
        }
    }

    /// The plan of a frame: each step wraps the one before it. A source reads
    /// through the frame's slot; the steps of `edges.nodes()` up to `nodes`
    /// read through the edge frame's.
    fn frame(&mut self, top: &Arc<Node>) -> Plan {
        let slot = self.frame_slot(top);
        let mut chain: Vec<Arc<Node>> = vec![top.clone()];
        while let Some(input) = chain.last().and_then(|n| n.input()).cloned() {
            chain.push(if self.shape {
                crate::infer::shape_input(&input)
            } else {
                input
            });
        }
        chain.reverse();
        let mut step_slot = slot;
        if let Some(pos) = chain.iter().position(|n| matches!(n.op, Op::Nodes { .. })) {
            if pos > 0 {
                step_slot = self.frame_slot(&chain[pos - 1].clone());
            }
        }
        let mut plan: Option<Plan> = None;
        for node in &chain {
            plan = Some(self.node(node, plan, step_slot));
            if matches!(node.op, Op::Nodes { .. }) {
                step_slot = slot;
            }
        }
        plan.expect("a frame has a first step")
    }

    fn handle(&mut self, node: &Arc<Node>) -> Slot {
        self.steps.push(node.clone());
        self.steps.len() - 1
    }

    fn node(&mut self, node: &Arc<Node>, input: Option<Plan>, slot: Slot) -> Plan {
        let input = || SubPlan::new(input.expect("a step has an input"));
        match &node.op {
            Op::FromArrow { .. } => {
                let (src, dst, id) = match &node.kind {
                    Kind::Edges { src, dst } => (Some(src.clone()), Some(dst.clone()), None),
                    Kind::Nodes { id } => (None, None, Some(id.clone())),
                };
                Plan::FromArrow {
                    frame: slot,
                    src,
                    dst,
                    id,
                }
            }
            Op::ScanEdges {
                path, on_null, csv, ..
            } => {
                let (src, dst) = node.roles().unwrap_or(("src", "dst"));
                Plan::ScanEdges {
                    frame: slot,
                    path: path.clone(),
                    src: src.to_string(),
                    dst: dst.to_string(),
                    on_null: on_null.clone(),
                    csv: csv.clone(),
                }
            }
            Op::ScanNodes { path, csv, .. } => Plan::ScanNodes {
                frame: slot,
                path: path.clone(),
                id: node.id_col().unwrap_or("id").to_string(),
                csv: csv.clone(),
            },
            Op::Nodes { .. } => Plan::Nodes { input: input() },
            Op::Reverse { .. } => Plan::Reverse { input: input() },
            Op::Step { step, error, .. } => {
                let invalid = error.is_some();
                if let Step::WithColumns(columns) = step {
                    let columns = columns
                        .iter()
                        .map(|c| NamedExpr {
                            name: c.name.clone(),
                            expr: self.column_expr(&c.expr),
                        })
                        .collect();
                    return Plan::WithColumns {
                        input: input(),
                        columns,
                    };
                }
                if let Step::Join { right, on, how } = step {
                    let input = input();
                    let right = SubPlan::new(if self.shape {
                        Plan::Invalid {
                            step: self.handle(right),
                        }
                    } else {
                        self.frame(right)
                    });
                    let handle = self.handle(node);
                    return Plan::Join {
                        input,
                        right,
                        on: if invalid { vec![] } else { on.clone() },
                        how: how.clone(),
                        step: Some(handle),
                        invalid,
                    };
                }
                let step_handle = Some(self.handle(node));
                let input = input();
                if invalid {
                    return invalid_step(step, input, step_handle);
                }
                match step {
                    Step::Filter(p) => Plan::Filter {
                        input,
                        predicate: p.clone(),
                        step: step_handle,
                        invalid,
                    },
                    Step::Select(columns) => Plan::Select {
                        input,
                        columns: columns.clone(),
                        step: step_handle,
                        invalid,
                    },
                    Step::Rename(mapping) => Plan::Rename {
                        input,
                        mapping: mapping.clone(),
                        step: step_handle,
                        invalid,
                    },
                    Step::Sort(by) => Plan::Sort {
                        input,
                        by: by.clone(),
                        step: step_handle,
                        invalid,
                    },
                    Step::Head(n) => Plan::Head {
                        input,
                        n: n.clone(),
                        step: step_handle,
                        invalid,
                    },
                    Step::Sample { n, seed, .. } => Plan::Sample {
                        input,
                        n: n.clone(),
                        seed: seed.clone(),
                        step: step_handle,
                        invalid,
                    },
                    Step::Distinct => Plan::Distinct {
                        input,
                        step: step_handle,
                        invalid,
                    },
                    Step::GroupByAgg { keys, aggs } => Plan::GroupByAgg {
                        input,
                        keys: keys.clone(),
                        aggs: aggs.clone(),
                        step: step_handle,
                        invalid,
                    },
                    Step::WithColumns(_) | Step::Join { .. } => unreachable!("lowered above"),
                }
            }
            Op::Hop {
                edges,
                n,
                direction,
                seeds,
            } => {
                let edges = self.edges(edges);
                Plan::Hop {
                    edges,
                    n: n.clone(),
                    direction: direction.clone(),
                    seeds: self.seed_slot(seeds),
                }
            }
            Op::ShortestPath {
                edges,
                source,
                target,
                weight,
                direction,
            } => {
                let edges = self.edges(edges);
                Plan::ShortestPath {
                    edges,
                    source: self.seed_slot(source),
                    target: self.seed_slot(target),
                    weight: weight.clone(),
                    direction: direction.clone(),
                }
            }
            Op::RandomWalk {
                edges,
                start,
                steps,
                walks_per_node,
                seed,
            } => {
                let edges = self.edges(edges);
                Plan::RandomWalk {
                    edges,
                    start: self.seed_slot(start),
                    steps: steps.clone(),
                    walks_per_node: walks_per_node.clone(),
                    seed: seed.clone(),
                }
            }
            Op::Describe {
                edges,
                full,
                direction,
            } => Plan::Describe {
                edges: self.edges(edges),
                full: *full,
                direction: direction.clone(),
            },
        }
    }

    fn column_expr(&mut self, expr: &ColumnExpr) -> Expr {
        match expr {
            ColumnExpr::Scalar(v) => Expr::Scalar(v.clone()),
            ColumnExpr::Graph { verb, edges } => {
                let mut verb = verb.as_ref().clone();
                verb.edges = edges.as_ref().map(|e| self.edges(e));
                Expr::Graph(Box::new(verb))
            }
        }
    }

    /// The inputs of this lowering, read through `host`.
    pub fn inputs<'a>(&'a self, host: &'a dyn Host, mode: Mode) -> TreeInputs<'a> {
        TreeInputs {
            lowering: self,
            host,
            mode,
        }
    }
}

/// A relational step whose arguments the host could not convert: the step with
/// its arguments left out, raising the host's error where it is read.
fn invalid_step(step: &Step, input: SubPlan, handle: Option<Slot>) -> Plan {
    let (invalid, step_h) = (true, handle);
    match step {
        Step::Filter(_) => Plan::Filter {
            input,
            predicate: Expr::default(),
            step: step_h,
            invalid,
        },
        Step::Select(_) => Plan::Select {
            input,
            columns: vec![],
            step: step_h,
            invalid,
        },
        Step::Rename(_) => Plan::Rename {
            input,
            mapping: vec![],
            step: step_h,
            invalid,
        },
        Step::Sort(_) => Plan::Sort {
            input,
            by: vec![],
            step: step_h,
            invalid,
        },
        Step::Head(_) => Plan::Head {
            input,
            n: IntArg::default(),
            step: step_h,
            invalid,
        },
        Step::Sample { .. } => Plan::Sample {
            input,
            n: IntArg::default(),
            seed: None,
            step: step_h,
            invalid,
        },
        Step::Distinct => Plan::Distinct {
            input,
            step: step_h,
            invalid,
        },
        Step::GroupByAgg { .. } => Plan::GroupByAgg {
            input,
            keys: vec![],
            aggs: vec![],
            step: step_h,
            invalid,
        },
        Step::WithColumns(_) | Step::Join { .. } => unreachable!("lowered by the caller"),
    }
}

// --- inputs -------------------------------------------------------------------

/// How a lowered tree's inputs are served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The real inputs (a collect).
    Run,
    /// Empty tables and indexes ([`crate::dry`]: `schema()`, `explain()`).
    Dry,
    /// None at all: the plan's shape is checked up to the first input it needs
    /// ([`crate::plan::check_shape`]).
    Shape,
}

/// The [`Inputs`] of a lowered tree.
pub struct TreeInputs<'a> {
    lowering: &'a Lowering,
    host: &'a dyn Host,
    mode: Mode,
}

impl TreeInputs<'_> {
    fn frame(&self, slot: Slot) -> PlanResult<&Arc<Node>> {
        match self.lowering.slots.get(slot) {
            Some(SlotRef::Frame(n)) => Ok(n),
            _ => Err(PlanError::Invalid(format!("slot {slot} is not a frame"))),
        }
    }

    fn step_node(&self, handle: Slot) -> PlanResult<&Arc<Node>> {
        self.lowering
            .steps
            .get(handle)
            .ok_or_else(|| PlanError::Invalid(format!("no step {handle}")))
    }

    fn shape(&self) -> PlanResult<()> {
        if self.mode == Mode::Shape {
            return Err(pending());
        }
        Ok(())
    }
}

impl Inputs for TreeInputs<'_> {
    fn index(&self, frame: Slot) -> PlanResult<GraphIndex> {
        self.shape()?;
        let node = self.frame(frame)?;
        if self.mode == Mode::Dry {
            return dry_index(node, self.host);
        }
        node.state
            .index
            .get_or_build(|| build_source_index(node, self.host))
    }

    fn built_index(&self, frame: Slot) -> PlanResult<Option<GraphIndex>> {
        self.shape()?;
        if self.mode == Mode::Dry {
            return Ok(None);
        }
        Ok(self.frame(frame)?.state.index.get())
    }

    fn put_index(&self, frame: Slot, index: GraphIndex) -> PlanResult<GraphIndex> {
        self.shape()?;
        if self.mode == Mode::Dry {
            return Ok(index);
        }
        Ok(self.frame(frame)?.state.index.put(index))
    }

    fn table(&self, frame: Slot) -> PlanResult<Option<Rows>> {
        self.shape()?;
        let node = self.frame(frame)?;
        let Some((
            Node {
                op: Op::FromArrow { table, schema, .. },
                ..
            },
            _,
        )) = node.source()
        else {
            return Ok(None);
        };
        if self.mode == Mode::Dry {
            return Ok(Some(Rows::Batches(vec![RecordBatch::new_empty(
                schema.clone(),
            )])));
        }
        Ok(Some(Rows::Host(Box::new(table.clone()))))
    }

    fn storage_options(&self, frame: Slot) -> PlanResult<HashMap<String, String>> {
        self.shape()?;
        let node = self.frame(frame)?;
        match node.source() {
            Some((
                Node {
                    op: Op::ScanEdges { spec, .. } | Op::ScanNodes { spec, .. },
                    ..
                },
                _,
            )) => self.host.storage_options(spec),
            _ => Ok(HashMap::new()),
        }
    }

    fn node_cache(&self, frame: Slot) -> PlanResult<Arc<NodeTableCache>> {
        self.shape()?;
        if self.mode == Mode::Dry {
            return Ok(Arc::default());
        }
        let node = self.frame(frame)?;
        Ok(node.state.node_cache.get_or_init(Arc::default).clone())
    }

    fn seeds(&self, slot: Slot) -> PlanResult<ArrayRef> {
        self.shape()?;
        if self.mode == Mode::Dry {
            return Ok(arrow::array::new_empty_array(&DataType::Int64));
        }
        match self.lowering.slots.get(slot) {
            Some(SlotRef::Seeds(seeds)) => resolve_seeds(seeds, self.host),
            _ => Err(PlanError::Invalid(format!("slot {slot} is not a seed set"))),
        }
    }

    fn sample(&self, step: Slot, rows: usize) -> PlanResult<Option<Vec<usize>>> {
        self.shape()?;
        if self.mode == Mode::Dry {
            return Ok(None);
        }
        match &self.step_node(step)?.op {
            Op::Step {
                step: Step::Sample { draw, .. },
                ..
            } => self.host.sample(draw, rows),
            _ => Err(PlanError::Invalid(format!("step {step} is not a sample"))),
        }
    }

    fn batches(&self, rows: HostRows) -> PlanResult<Vec<RecordBatch>> {
        self.shape()?;
        let table = rows
            .downcast::<HostHandle>()
            .map_err(|_| PlanError::Invalid("host rows of another host".into()))?;
        self.host.batches(&table)
    }

    fn step_error(&self, step: Slot) -> PlanError {
        match self.step_node(step).map(|n| &n.op) {
            Ok(Op::Step { error: Some(e), .. }) => self.host.error(e),
            // A join's right side in a shape check (never read there).
            _ if self.mode == Mode::Shape => pending(),
            _ => PlanError::Invalid(format!("step {step} has no stored error")),
        }
    }

    fn warn_dropped(&self, n: usize) -> PlanResult<()> {
        self.shape()?;
        if self.mode == Mode::Dry {
            return Ok(());
        }
        self.host.warn_dropped(n)
    }

    fn identity(&self, frame: Slot) -> PlanResult<Option<String>> {
        // A dry run keys its topologies as a collect would, so the subplans a
        // collect shares one index between share one too.
        self.shape()?;
        match self.frame(frame)?.table() {
            Some(table) => self.host.identity(table),
            None => Ok(None),
        }
    }
}

/// Build the index of an edge frame's source: its scan, or its in-memory
/// endpoints (swapped under an odd number of `reverse` steps).
fn build_source_index(node: &Node, host: &dyn Host) -> PlanResult<GraphIndex> {
    let Some((source, reversed)) = node.source() else {
        return Err(PlanError::NotImplemented(
            crate::plan::NO_EDGE_SOURCE.to_string(),
        ));
    };
    let (src, dst) = node.roles().unwrap_or(("src", "dst"));
    match &source.op {
        Op::ScanEdges {
            path: Some(path),
            on_null,
            spec,
            csv,
        } => {
            let opts = host.storage_options(spec)?;
            let source = crate::scan::ScanSource::new(path.paths(), &opts, csv);
            let scanned = crate::scan::scan_edges_topology(source, src, dst, on_null == "drop")
                .map_err(|e| match e {
                    crate::ScanBuildError::Scan(e) => PlanError::Engine(e),
                    crate::ScanBuildError::Build(e) => PlanError::Value(e.to_string()),
                })?;
            crate::plan::count_topology_build();
            if scanned.dropped > 0 {
                host.warn_dropped(scanned.dropped)?;
            }
            Ok(GraphIndex::new(scanned.topo, scanned.ids))
        }
        Op::ScanEdges { .. } => Err(PlanError::NotImplemented(
            crate::plan::MULTI_PATH_EDGES.to_string(),
        )),
        Op::FromArrow {
            endpoints: Some(endpoints),
            ..
        } => {
            let batches = host.edge_batches(endpoints, reversed % 2 == 1)?;
            let pairs: Vec<(&dyn Array, &dyn Array)> = batches
                .iter()
                .map(|b| (b.column(0).as_ref(), b.column(1).as_ref()))
                .collect();
            let (topo, ids) = crate::topology::build_topology_batches(&pairs)
                .map_err(|e| PlanError::Value(e.to_string()))?;
            crate::plan::count_topology_build();
            Ok(GraphIndex::new(topo, ids))
        }
        _ => Err(PlanError::NotImplemented(
            crate::plan::NO_EDGE_SOURCE.to_string(),
        )),
    }
}

/// The empty index a dry run uses for an edge frame's source, with the id type
/// its index would have.
fn dry_index(node: &Node, host: &dyn Host) -> PlanResult<GraphIndex> {
    let id_type = match node.source() {
        Some((
            Node {
                op: Op::FromArrow { schema, .. },
                kind,
                ..
            },
            _,
        )) => match kind {
            Kind::Edges { src, .. } => schema
                .field_with_name(src)
                .ok()
                .and_then(|f| crate::infer::canonical_id_type(f.data_type()))
                .unwrap_or(DataType::Int64),
            Kind::Nodes { .. } => DataType::Int64,
        },
        // A scan reads its schema (a dry run reads no rows).
        Some((
            Node {
                op: Op::ScanEdges { .. },
                ..
            },
            _,
        )) => return build_source_index(node, host),
        _ => DataType::Int64,
    };
    let (topo, ids) = crate::topology::empty_topology(&id_type);
    Ok(GraphIndex::new(topo, ids))
}

// --- seeds ----------------------------------------------------------------------

/// The node ids of a seed set (int64 or string).
fn resolve_seeds(seeds: &Seeds, host: &dyn Host) -> PlanResult<ArrayRef> {
    match seeds {
        Seeds::Value(v) => host.seeds(v),
        Seeds::Frame(node) => frame_seeds(node, host),
    }
}

/// The steps that keep a node frame's id column as its source has it, so the
/// ids are read without running the frame.
const SEED_PASSTHROUGH: [&str; 3] = ["from_arrow", "scan_nodes", "nodes"];

const SEED_FRAME: &str = "the seed NodeFrame could not be materialized. Seeding a traversal / \
     walk from a derived node frame (filtered, or with computed columns) needs that frame to be \
     collectable; pass an explicit id iterable, a plain scan_nodes/from_arrow(id=...) frame, or \
     precompute the ids.";

/// A node frame's ids as a seed set: read from an unmodified attribute table or
/// node scan, else from running the frame (once; frames don't change).
fn frame_seeds(node: &Arc<Node>, host: &dyn Host) -> PlanResult<ArrayRef> {
    let plain = node
        .chain()
        .iter()
        .all(|n| SEED_PASSTHROUGH.contains(&n.op_name()));
    let source = node.source().map(|(s, _)| &s.op);
    let id_col = node.id_col().unwrap_or("id").to_string();
    let batches = match source {
        Some(Op::FromArrow { table, .. }) if plain => host.batches(table)?,
        Some(Op::ScanNodes { .. }) if plain => {
            let select = node.step(
                Step::Select(vec![SelectItem::Name(id_col.clone())]),
                None,
                host,
            )?;
            execute(&select, host)?.into_batches_with(host)?
        }
        _ => {
            if let Some(ids) = node.seeds.get() {
                return Ok(ids.clone());
            }
            match execute(node, host).and_then(|rows| rows.into_batches_with(host)) {
                Ok(b) => b,
                Err(e) if is_not_implemented(&e) => {
                    return Err(PlanError::Wrapped {
                        message: format!("{SEED_FRAME} (underlying: {e})"),
                        cause: Box::new(e),
                    })
                }
                Err(e) => return Err(e),
            }
        }
    };
    let schema = batches[0].schema();
    let name = if schema.index_of(&id_col).is_ok() {
        id_col
    } else {
        "id".to_string()
    };
    let i = schema
        .index_of(&name)
        .map_err(|_| PlanError::Value(format!("Field \"{name}\" does not exist in schema")))?;
    let columns: Vec<&dyn Array> = batches.iter().map(|b| b.column(i).as_ref()).collect();
    let column = arrow::compute::concat(&columns)?;
    let ids = canonical_seed_ids(column)?;
    if !plain {
        let _ = node.seeds.set(ids.clone());
    }
    Ok(ids)
}

/// Whether an error is Python's `NotImplementedError`.
fn is_not_implemented(e: &PlanError) -> bool {
    matches!(
        e,
        PlanError::NotImplemented(_)
            | PlanError::Wrapped { .. }
            | PlanError::Host(crate::plan::HostError {
                not_implemented: true,
                ..
            })
    )
}

/// A seed id column as a seed set: any integer type as int64, strings as they
/// are, a dictionary decoded.
fn canonical_seed_ids(a: ArrayRef) -> PlanResult<ArrayRef> {
    match a.data_type() {
        t if t.is_integer() => Ok(arrow::compute::cast_with_options(
            &a,
            &DataType::Int64,
            &arrow::compute::CastOptions {
                safe: false,
                ..Default::default()
            },
        )?),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Ok(a),
        DataType::Dictionary(_, value)
            if value.is_integer()
                || matches!(
                    value.as_ref(),
                    DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
                ) =>
        {
            let decoded = arrow::compute::cast(&a, value)?;
            canonical_seed_ids(decoded)
        }
        other => Err(PlanError::Type(format!(
            "node ids must be an integer or string column; got {}",
            crate::compare::pa_name(other)
        ))),
    }
}

// --- entry points ---------------------------------------------------------------

impl Rows {
    /// The rows as batches, converting host rows through `host`.
    fn into_batches_with(self, host: &dyn Host) -> PlanResult<Vec<RecordBatch>> {
        match self {
            Rows::Batches(b) => Ok(b),
            Rows::Host(h) => {
                let table = h
                    .downcast::<HostHandle>()
                    .map_err(|_| PlanError::Invalid("host rows of another host".into()))?;
                host.batches(&table)
            }
        }
    }
}

/// Run a frame: its rows, left with the host when the host holds them (a plain
/// in-memory frame).
pub fn execute(node: &Arc<Node>, host: &dyn Host) -> PlanResult<Rows> {
    let (plan, lowering) = Lowering::of(node);
    crate::stack::for_steps(lowering.steps.len(), || {
        crate::plan::execute(&plan, &lowering.inputs(host, Mode::Run))
    })
}

/// The host handle of rows [`execute`] left with the host.
pub fn host_rows(rows: HostRows) -> Option<HostHandle> {
    rows.downcast::<HostHandle>().ok().map(|h| *h)
}

/// The schema `collect()` returns, from a dry run.
pub fn schema(node: &Arc<Node>, host: &dyn Host) -> PlanResult<SchemaRef> {
    let (plan, lowering) = Lowering::of(node);
    crate::stack::for_steps(lowering.steps.len(), || {
        Ok(crate::plan::dry_run(&plan, &lowering.inputs(host, Mode::Dry))?.0)
    })
}

/// The graph index of an edge frame, after the checks a graph op over it needs
/// (for the eager whole-graph stats).
pub fn graph_index(node: &Arc<Node>, host: &dyn Host) -> PlanResult<GraphIndex> {
    let (edges, lowering) = Lowering::edges_of(node);
    crate::stack::for_steps(lowering.steps.len(), || {
        crate::plan::require_index(&edges, &lowering.inputs(host, Mode::Run))
    })
}

/// `explain()`: the frame's plan tree, then the DataFusion plans a collect runs
/// (from a dry run), optimized, and with `physical` their physical plans.
pub fn explain(node: &Arc<Node>, host: &dyn Host, physical: bool) -> String {
    let (plan, lowering) = Lowering::of(node);
    crate::stack::for_steps(lowering.steps.len(), || {
        explain_plan(node, &plan, &lowering, host, physical)
    })
}

fn explain_plan(
    node: &Arc<Node>,
    plan: &Plan,
    lowering: &Lowering,
    host: &dyn Host,
    physical: bool,
) -> String {
    let mut out = String::new();
    out.push_str(match node.kind {
        Kind::Edges { .. } => "EdgeFrame",
        Kind::Nodes { .. } => "NodeFrame",
    });
    out.push_str(" plan:\n");
    crate::explain::tree(plan, 1, &mut out);
    let inputs = lowering.inputs(host, Mode::Dry);
    match crate::plan::dry_run(plan, &inputs) {
        Ok((schema, plans)) => {
            crate::explain::schema(&schema, &mut out);
            crate::explain::datafusion(&plans, physical, &mut out);
        }
        Err(e) => {
            out.push_str(&format!("\nDataFusion plan: not available ({e})\n"));
        }
    }
    out
}

/// What a slot of a lowered plan reads, for a fixture to bind it: a frame's
/// source table (if it has one), or a seed value (if it is not a frame).
pub enum SlotSource<'a> {
    Frame(Option<&'a HostHandle>),
    Seeds(Option<&'a HostHandle>),
}

/// A frame lowered to the plan IR as JSON, and what each slot reads (the plan
/// conformance fixtures are written from it).
pub fn lowered_json(node: &Arc<Node>, mut each: impl FnMut(SlotSource<'_>)) -> String {
    let (plan, lowering) = Lowering::of(node);
    for slot in &lowering.slots {
        each(match slot {
            SlotRef::Frame(n) => SlotSource::Frame(n.table()),
            SlotRef::Seeds(Seeds::Value(v)) => SlotSource::Seeds(Some(v)),
            SlotRef::Seeds(Seeds::Frame(_)) => SlotSource::Seeds(None),
        });
    }
    plan.to_json()
}

/// Check a lowered tree's shape without its inputs ([`crate::plan::check_shape`]).
pub(crate) fn check_shape(node: &Arc<Node>, host: &dyn Host) -> PlanResult<()> {
    let mut lowering = Lowering::new();
    lowering.shape = true;
    let plan = lowering.frame(node);
    crate::plan::check_shape(&plan, &lowering.inputs(host, Mode::Shape))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, Int64Array};
    use arrow::datatypes::{Field, Schema};
    use serde_json::json;

    /// A host whose tables are batch lists, its endpoints too.
    struct TestHost;

    impl Host for TestHost {
        fn batches(&self, table: &HostHandle) -> PlanResult<Vec<RecordBatch>> {
            Ok(table.downcast_ref::<Vec<RecordBatch>>().unwrap().clone())
        }
        fn edge_batches(&self, e: &HostHandle, reversed: bool) -> PlanResult<Vec<RecordBatch>> {
            let b = e.downcast_ref::<Vec<RecordBatch>>().unwrap();
            let order = if reversed { [1, 0] } else { [0, 1] };
            Ok(b.iter().map(|b| b.project(&order).unwrap()).collect())
        }
        fn storage_options(&self, _: &HostHandle) -> PlanResult<HashMap<String, String>> {
            Ok(HashMap::new())
        }
        fn seeds(&self, v: &HostHandle) -> PlanResult<ArrayRef> {
            Ok(v.downcast_ref::<ArrayRef>().unwrap().clone())
        }
        fn sample(&self, _: &HostHandle, _: usize) -> PlanResult<Option<Vec<usize>>> {
            Ok(None)
        }
        fn warn_dropped(&self, _: usize) -> PlanResult<()> {
            Ok(())
        }
        fn identity(&self, _: &HostHandle) -> PlanResult<Option<String>> {
            Ok(None)
        }
        fn error(&self, e: &HostHandle) -> PlanError {
            PlanError::Value(e.downcast_ref::<String>().unwrap().clone())
        }
    }

    fn edges() -> Arc<Node> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("s", DataType::Int64, false),
            Field::new("d", DataType::Int64, false),
            Field::new("w", DataType::Float64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![0, 0, 1, 2])),
                Arc::new(Int64Array::from(vec![1, 2, 2, 0])),
                Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0, 4.0])),
            ],
        )
        .unwrap();
        let table: HostHandle = Arc::new(vec![batch.clone()]);
        let ends: HostHandle = Arc::new(vec![batch.project(&[0, 1]).unwrap()]);
        Node::edge_table(table, schema, ends, "s", "d", &TestHost).unwrap()
    }

    fn filter(node: &Arc<Node>, v: Value) -> PlanResult<Arc<Node>> {
        node.step(Step::Filter(Expr::Scalar(v)), None, &TestHost)
    }

    fn gt(c: &str, x: i64) -> Value {
        json!({"kind": "binary", "op": ">", "left": {"kind": "col", "name": c},
               "right": {"kind": "lit", "value": x}})
    }

    fn degree(e: &Arc<Node>) -> Column {
        let verb: GraphVerb = serde_json::from_value(json!({"verb": "degree"})).unwrap();
        Column {
            name: "deg".into(),
            expr: ColumnExpr::Graph {
                verb: Box::new(verb),
                edges: Some(e.clone()),
            },
        }
    }

    fn rows(node: &Arc<Node>) -> Vec<RecordBatch> {
        execute(node, &TestHost)
            .unwrap()
            .into_batches_with(&TestHost)
            .unwrap()
    }

    #[test]
    fn an_unknown_column_raises_where_the_step_is_built() {
        let e = edges();
        let err = filter(&e, gt("nope", 1)).err().expect("an unknown column");
        assert_eq!(
            err.to_string(),
            "filter() references unknown column 'nope'; the frame has ['s', 'd', 'w']."
        );
        assert!(filter(&e, gt("w", 1)).is_ok());
    }

    #[test]
    fn a_deferred_argument_error_raises_where_the_step_is_built() {
        let e = edges();
        let err: HostHandle = Arc::new("sort() needs at least one column".to_string());
        let got = e.step(Step::Sort(vec![]), Some(err), &TestHost).err();
        assert!(matches!(got, Some(PlanError::Value(m)) if m.contains("sort()")));
    }

    #[test]
    fn schema_is_the_collected_schema() {
        let e = edges();
        let nodes = e.nodes(&TestHost).unwrap();
        let g = nodes
            .step(Step::WithColumns(vec![degree(&e)]), None, &TestHost)
            .unwrap();
        let g = filter(&g, gt("deg", 0)).unwrap();
        assert_eq!(schema(&g, &TestHost).unwrap(), rows(&g)[0].schema());
        assert_eq!(schema(&e, &TestHost).unwrap(), rows(&e)[0].schema());
        let text = explain(&g, &TestHost, true);
        assert!(text.contains("with_columns(deg=degree"), "{text}");
        assert!(text.contains("DataFusion physical plan 1 of"), "{text}");
    }

    #[test]
    fn a_bare_node_set_builds_and_a_with_columns_makes_it_collect() {
        let e = edges();
        let nodes = e.nodes(&TestHost).unwrap();
        let pending = filter(&nodes, gt("id", 0)).unwrap();
        assert!(execute(&pending, &TestHost).is_err());
        let g = pending
            .step(Step::WithColumns(vec![degree(&e)]), None, &TestHost)
            .unwrap();
        assert_eq!(rows(&g).iter().map(|b| b.num_rows()).sum::<usize>(), 2);
        // The pending filter is checked against the node set's columns.
        let bad = filter(&nodes, gt("nope", 0)).unwrap();
        let err = bad
            .step(Step::WithColumns(vec![degree(&e)]), None, &TestHost)
            .err()
            .expect("an unknown column");
        assert!(err.to_string().contains("the frame has ['id', 'deg']"));
    }

    #[test]
    fn steps_that_keep_the_rows_share_the_index() {
        let e = edges();
        let viewed = filter(&e, gt("w", 1)).unwrap();
        let reshaped = e.step(Step::Distinct, None, &TestHost).unwrap();
        assert!(Arc::ptr_eq(e.state(), viewed.state()));
        assert!(!Arc::ptr_eq(e.state(), reshaped.state()));
        // An index built through the parent serves a derivation made before it.
        let g = e
            .nodes(&TestHost)
            .unwrap()
            .step(Step::WithColumns(vec![degree(&e)]), None, &TestHost)
            .unwrap();
        rows(&g);
        let built = e.state().index.get().expect("built");
        let got = viewed.state().index.get().expect("shared");
        assert!(Arc::ptr_eq(&built.topo, &got.topo));
    }
}
