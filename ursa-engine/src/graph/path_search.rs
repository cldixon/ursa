//! Searching paths on the graph index: SQL/PGQ's quantified edges
//! (`-[e]->{1,3}`, `-[e]->*`), path variables, selectors (`ANY SHORTEST`,
//! `ALL SHORTEST`, `SHORTEST k`) and path modes (`TRAIL`, `SIMPLE`,
//! `ACYCLIC`) (#297).
//!
//! A fixed-length pattern is planned as joins (see [`super::pattern`]). A
//! quantified edge is not: it is a [`PathSearchNode`], a relation of paths
//! that runs a breadth-first search on the edge table's graph index, the
//! index `hop` and `shortest_path` use, and joins to the nodes at its two
//! ends like any edge. Its rows are one per path:
//!
//! | column        | what                                                  |
//! |---------------|-------------------------------------------------------|
//! | `__start`     | the key of the path's first node                      |
//! | `__end`       | the key of its last node                              |
//! | `__length`    | its number of edges (`path_length(p)`)                |
//! | `__vertices`  | its nodes' keys, first to last (`vertices(p)`)        |
//! | `__edges`     | its edges' rows in the edge table, from 0 (`edges(p)`) |
//! | `__element_id`| nodes and edges alternating (`element_id(p)`)         |
//!
//! The last three are computed only when the query uses them.
//!
//! A fixed-length edge next to a bound node is searched too, as one edge
//! (`{1,1}`) from the nodes already reached (see [`super::pattern`]): each of
//! its rows is one edge, in edge-row order, so the search's rows are the
//! join's rows.
//!
//! ## Targets
//!
//! A search may be given its targets: the keys the last node can have, when
//! a condition on it alone says so (`b.id = 7`, `b.id IN (…)`). It keeps only
//! the paths that end at one, and a search with a selector stops as soon as
//! every target has all its paths: `ANY SHORTEST` from one node to another
//! looks no further than the target.
//!
//! ## Which paths
//!
//! The search runs from each start node: every node of the left end's
//! table that its conditions keep. A path of `n` edges is in the result when
//! `min <= n <= max` and the path mode allows it. Without a selector, every
//! such path is (a walk needs an upper bound, or it would be endless). With
//! one, per pair of first and last node:
//!
//! - `ANY SHORTEST` (and `ANY`): one shortest path;
//! - `SHORTEST k` (and `ANY k`): the `k` shortest paths;
//! - `ALL SHORTEST`: every path as short as the shortest.
//!
//! Among paths of one length, a path comes before another when its first
//! edge has a lower row in the edge table, or the same first edge and a
//! lower second, and so on: `ANY SHORTEST` keeps the first. The search finds
//! paths in that order, so the result does not depend on the run or the
//! thread count. (An undirected edge is followed either way; a self-loop
//! once.)
//!
//! ## How
//!
//! The search is breadth-first over (node, edges so far, up to `min`), so a
//! shortest path of at least `min` edges is found even when it has to come
//! back to a node it passed. Under `WALK`, a selector lets it stop extending
//! a state once it has its `k` paths (or, for `ALL SHORTEST`, the paths as
//! short as the first): any longer prefix makes a longer or later path. That
//! holds for the other modes too when the lower bound is 0 or 1, the
//! selector is `ANY SHORTEST` or `ALL SHORTEST` and the path is not an
//! undirected `TRAIL`: a shortest walk between two nodes repeats no node, and
//! one back to its start is a cycle. Otherwise the search lists every path
//! the mode allows, up to [`MAX_PATHS`].

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use datafusion::arrow::array::{
    Array, ArrayBuilder, ArrayRef, Int64Array, Int64Builder, ListArray, UInt32Array,
};
use datafusion::arrow::buffer::OffsetBuffer;
use datafusion::arrow::compute::{concat, take};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::{DFSchema, DFSchemaRef, ScalarValue};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{Expr, LogicalPlan, UserDefinedLogicalNodeCore};
use ursa_core::{IdMap, Topology};

use super::node::{check_vertices, endpoint_id_type};
use super::pattern::{Direction, Mode, Quantifier, Selector};

/// The path's first node.
pub const START: &str = "__start";
/// Its last node.
pub const END: &str = "__end";
/// Its number of edges.
pub const LENGTH: &str = "__length";
/// Its nodes, first to last.
pub const VERTICES: &str = "__vertices";
/// Its edges' rows in the edge table.
pub const EDGES: &str = "__edges";
/// Its nodes and edges, alternating.
pub const ELEMENTS: &str = "__element_id";

/// The path functions, and the column each reads.
pub const FUNCTIONS: &[(&str, &str)] = &[
    ("path_length", LENGTH),
    ("vertices", VERTICES),
    ("edges", EDGES),
    ("element_id", ELEMENTS),
];

/// The most paths one search lists before it stops with an error.
pub const MAX_PATHS: usize = 10_000_000;

/// Rows per output batch.
const BATCH_ROWS: usize = 8192;

/// What a search looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd)]
pub struct Search {
    pub direction: Direction,
    pub quantifier: Quantifier,
    pub selector: Selector,
    pub mode: Mode,
}

impl fmt::Display for Search {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.selector {
            Selector::All => {}
            Selector::Shortest(1) => write!(f, "ANY SHORTEST ")?,
            Selector::Shortest(k) => write!(f, "SHORTEST {k} ")?,
            Selector::AllShortest => write!(f, "ALL SHORTEST ")?,
        }
        let mode = match self.mode {
            Mode::Walk => "WALK",
            Mode::Trail => "TRAIL",
            Mode::Simple => "SIMPLE",
            Mode::Acyclic => "ACYCLIC",
        };
        let arrow = match self.direction {
            Direction::Right => "-[]->",
            Direction::Left => "<-[]-",
            Direction::Any => "-[]-",
        };
        let max = self
            .quantifier
            .max
            .map(|m| m.to_string())
            .unwrap_or_default();
        write!(f, "{mode} {arrow}{{{},{max}}}", self.quantifier.min)
    }
}

/// Which path columns a node computes besides its ends and length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd)]
pub struct PathColumns {
    pub vertices: bool,
    pub edges: bool,
    pub elements: bool,
}

/// The schema of a search's rows, for node ids of `id_type`.
pub fn schema(id_type: &DataType, columns: PathColumns) -> Schema {
    let list = |t: DataType| DataType::List(Arc::new(Field::new_list_field(t, false)));
    let mut fields = vec![
        Field::new(START, id_type.clone(), false),
        Field::new(END, id_type.clone(), false),
        Field::new(LENGTH, DataType::Int64, false),
    ];
    if columns.vertices {
        fields.push(Field::new(VERTICES, list(id_type.clone()), false));
    }
    if columns.edges {
        fields.push(Field::new(EDGES, list(DataType::Int64), false));
    }
    if columns.elements {
        fields.push(Field::new(ELEMENTS, list(DataType::Int64), false));
    }
    Schema::new(fields)
}

/// A search on a graph's index, as a logical plan node. Its inputs:
///
/// 1. the edge table's rows, projected to the endpoints (the plan the index
///    is built from, as for a traversal);
/// 2. the start nodes: one column of keys, each once.
#[derive(Debug, Clone)]
pub struct PathSearchNode {
    pub graph: String,
    pub search: Search,
    pub columns: PathColumns,
    pub input: Arc<LogicalPlan>,
    /// The keys of the vertex tables the edges connect, one column, when they
    /// have them: the graph's nodes, as an algorithm's (#311).
    pub vertices: Option<Arc<LogicalPlan>>,
    pub starts: Arc<LogicalPlan>,
    /// The keys the paths may end at, one column, if they are known (see the
    /// module docs).
    pub targets: Option<Arc<LogicalPlan>>,
    /// Whether every node the starts can name is a node of the index (the
    /// start node's table is the one the index's nodes are the keys of): a
    /// start that is not one is no node, so it has no path, not even the
    /// path of no edges.
    pub known_only: bool,
    pub source: Expr,
    pub destination: Expr,
    pub id_type: DataType,
    schema: DFSchemaRef,
}

impl PathSearchNode {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        graph: String,
        search: Search,
        columns: PathColumns,
        input: Arc<LogicalPlan>,
        vertices: Option<Arc<LogicalPlan>>,
        starts: Arc<LogicalPlan>,
        targets: Option<Arc<LogicalPlan>>,
        known_only: bool,
        source: Expr,
        destination: Expr,
    ) -> Result<Self> {
        let id_type = endpoint_id_type(&graph, &input, &source, &destination)?;
        check_vertices(&graph, vertices.as_deref())?;
        if columns.elements && id_type != DataType::Int64 {
            return Err(DataFusionError::Plan(format!(
                "in GRAPH_TABLE: element_id(p) lists node keys and edge rows together, so it needs integer node keys; graph '{graph}' has {id_type} keys: use vertices(p) and edges(p)"
            )));
        }
        let schema = Arc::new(DFSchema::try_from(schema(&id_type, columns))?);
        Ok(Self {
            graph,
            search,
            columns,
            input,
            vertices,
            starts,
            targets,
            known_only,
            source,
            destination,
            id_type,
            schema,
        })
    }

    pub(crate) fn endpoint_indices(&self) -> Result<(usize, usize)> {
        let index = |e: &Expr| -> Result<usize> {
            match e {
                Expr::Column(c) => self.input.schema().index_of_column(c),
                other => Err(DataFusionError::Internal(format!(
                    "a path search's endpoint must be a column; got {other}"
                ))),
            }
        };
        Ok((index(&self.source)?, index(&self.destination)?))
    }
}

impl PartialEq for PathSearchNode {
    fn eq(&self, other: &Self) -> bool {
        self.graph == other.graph
            && self.search == other.search
            && self.columns == other.columns
            && self.input == other.input
            && self.vertices == other.vertices
            && self.starts == other.starts
            && self.targets == other.targets
            && self.known_only == other.known_only
            && self.source == other.source
            && self.destination == other.destination
    }
}

impl Eq for PathSearchNode {}

impl Hash for PathSearchNode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.graph.hash(state);
        self.search.hash(state);
        self.columns.hash(state);
        self.input.hash(state);
        self.vertices.hash(state);
        self.starts.hash(state);
        self.targets.hash(state);
        self.known_only.hash(state);
    }
}

impl PartialOrd for PathSearchNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        (&self.graph, &self.search, &self.columns)
            .partial_cmp(&(&other.graph, &other.search, &other.columns))
            .filter(|o| o.is_ne())
    }
}

impl UserDefinedLogicalNodeCore for PathSearchNode {
    fn name(&self) -> &str {
        "PathSearch"
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        let mut inputs = vec![self.input.as_ref()];
        inputs.extend(self.vertices.as_deref());
        inputs.push(&self.starts);
        inputs.extend(self.targets.as_deref());
        inputs
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
            "PathSearch: graph={}, search={}{}",
            self.graph,
            self.search,
            if self.targets.is_some() {
                ", targets"
            } else {
                ""
            }
        )
    }

    fn with_exprs_and_inputs(&self, _exprs: Vec<Expr>, inputs: Vec<LogicalPlan>) -> Result<Self> {
        let expected =
            2 + usize::from(self.vertices.is_some()) + usize::from(self.targets.is_some());
        if inputs.len() != expected {
            return Err(DataFusionError::Internal(format!(
                "PathSearch has {expected} inputs; got {}",
                inputs.len()
            )));
        }
        let mut inputs = inputs.into_iter().map(Arc::new);
        let input = inputs.next().expect("checked above");
        let vertices = self.vertices.as_ref().and_then(|_| inputs.next());
        let starts = inputs.next().expect("checked above");
        let targets = self.targets.as_ref().and_then(|_| inputs.next());
        Ok(Self {
            input,
            vertices,
            starts,
            targets,
            ..self.clone()
        })
    }

    /// Every input is read whole: the endpoints, the vertex key, the one
    /// start column and the one target column.
    fn necessary_children_exprs(&self, _output_columns: &[usize]) -> Option<Vec<Vec<usize>>> {
        let (source, destination) = self.endpoint_indices().ok()?;
        let mut children = vec![vec![source, destination]];
        if self.vertices.is_some() {
            children.push(vec![0]);
        }
        children.push(vec![0]);
        if self.targets.is_some() {
            children.push(vec![0]);
        }
        Some(children)
    }

    /// A filter on the paths cannot move into the edges or the starts.
    fn prevent_predicate_push_down_columns(&self) -> std::collections::HashSet<String> {
        self.schema
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    }
}

/// One path prefix the search holds: its last node, the entry it extends,
/// the edge that does, and its length.
#[derive(Debug, Clone, Copy)]
struct Entry {
    node: u32,
    parent: u32,
    edge: u32,
    len: u32,
}

const ROOT: u32 = u32::MAX;

/// Per search state, (node, edges so far up to `min`): how many prefixes
/// reached it, and the length of the first.
enum States {
    Dense {
        seen: Vec<(u32, u32)>,
        touched: Vec<usize>,
        width: usize,
    },
    Sparse(HashMap<(u32, u32), (u32, u32)>),
}

impl States {
    fn new(n_nodes: usize, min: u32) -> Self {
        let width = min as usize + 1;
        match n_nodes.checked_mul(width) {
            Some(n) if n <= 1 << 26 => States::Dense {
                seen: vec![(0, 0); n],
                touched: Vec::new(),
                width,
            },
            _ => States::Sparse(HashMap::new()),
        }
    }

    fn get(&mut self, node: u32, c: u32) -> &mut (u32, u32) {
        match self {
            States::Dense {
                seen,
                touched,
                width,
            } => {
                let i = node as usize * *width + c as usize;
                if seen[i].0 == 0 {
                    touched.push(i);
                }
                &mut seen[i]
            }
            States::Sparse(map) => map.entry((node, c)).or_insert((0, 0)),
        }
    }

    fn clear(&mut self) {
        match self {
            States::Dense { seen, touched, .. } => {
                for i in touched.drain(..) {
                    seen[i] = (0, 0);
                }
            }
            States::Sparse(map) => map.clear(),
        }
    }
}

/// The edges a step can take from `u`, in edge-row order, as (next node,
/// edge row).
fn steps(topology: &Topology, direction: Direction, u: u32, out: &mut Vec<(u32, u32)>) {
    out.clear();
    fn pairs(a: &ursa_core::Adjacency, u: u32) -> impl Iterator<Item = (u32, u32)> + '_ {
        a.neighbors(u)
            .iter()
            .copied()
            .zip(a.edge_ids(u).iter().copied())
    }
    match direction {
        Direction::Right => out.extend(pairs(topology.out(), u)),
        Direction::Left => out.extend(pairs(topology.incoming(), u)),
        Direction::Any => {
            let forward: Vec<(u32, u32)> = pairs(topology.out(), u).collect();
            // A self-loop is in both lists: it is taken once, forward.
            let backward: Vec<(u32, u32)> = pairs(topology.incoming(), u)
                .filter(|&(w, _)| w != u)
                .collect();
            let (mut i, mut j) = (0, 0);
            while i < forward.len() || j < backward.len() {
                let take_forward =
                    j == backward.len() || (i < forward.len() && forward[i].1 < backward[j].1);
                if take_forward {
                    out.push(forward[i]);
                    i += 1;
                } else {
                    out.push(backward[j]);
                    j += 1;
                }
            }
        }
    }
}

/// Whether a prefix ending at entry `at` may take the edge `edge` to `w`
/// under `mode`.
fn allowed(entries: &[Entry], at: u32, w: u32, edge: u32, mode: Mode, start: u32) -> bool {
    let chain = || {
        let mut i = at;
        std::iter::from_fn(move || {
            if i == ROOT {
                return None;
            }
            let e = entries[i as usize];
            i = e.parent;
            Some(e)
        })
    };
    match mode {
        Mode::Walk => true,
        Mode::Trail => chain().all(|e| e.parent == ROOT || e.edge != edge),
        Mode::Acyclic => chain().all(|e| e.node != w),
        Mode::Simple => {
            let last = entries[at as usize];
            if last.len > 0 && last.node == start {
                return false; // closed: it cannot go on
            }
            w == start && chain().all(|e| e.node != w || e.parent == ROOT)
                || chain().all(|e| e.node != w)
        }
    }
}

/// Collects a search's rows into batches.
struct Rows<'a> {
    starts: &'a ArrayRef,
    columns: PathColumns,
    schema: SchemaRef,
    start_rows: Vec<u32>,
    ends: Vec<u32>,
    lengths: Vec<i64>,
    vertex_offsets: Vec<i32>,
    vertex_values: Vec<u32>,
    edge_offsets: Vec<i32>,
    edge_values: Vec<i64>,
    batches: Vec<RecordBatch>,
    /// The starts, then every node of the index, as user ids: what the
    /// node references index.
    values: ArrayRef,
    /// The edge table's row of an edge of the index.
    edge_row: &'a dyn Fn(u32) -> u64,
}

impl<'a> Rows<'a> {
    fn new(
        ids: &'a IdMap,
        starts: &'a ArrayRef,
        columns: PathColumns,
        schema: SchemaRef,
        edge_row: &'a dyn Fn(u32) -> u64,
    ) -> Result<Self> {
        let values = if ids.is_empty() {
            Arc::clone(starts)
        } else {
            concat(&[starts.as_ref(), ids.user_id_array().as_ref()])?
        };
        Ok(Self {
            starts,
            columns,
            schema,
            start_rows: Vec::new(),
            ends: Vec::new(),
            lengths: Vec::new(),
            vertex_offsets: vec![0],
            vertex_values: Vec::new(),
            edge_offsets: vec![0],
            edge_values: Vec::new(),
            batches: Vec::new(),
            values,
            edge_row,
        })
    }

    /// The index into `values` of dense node `d`.
    fn dense(&self, d: u32) -> u32 {
        (self.starts.len() as u32) + d
    }

    /// A path from start row `start`: its nodes (as `values` indexes) and
    /// edges, first to last.
    fn push(&mut self, start: u32, nodes: &[u32], edges: &[u32]) -> Result<()> {
        self.start_rows.push(start);
        self.ends.push(*nodes.last().expect("a path has a node"));
        self.lengths.push(edges.len() as i64);
        if self.columns.vertices || self.columns.elements {
            self.vertex_values.extend_from_slice(nodes);
            self.vertex_offsets.push(self.vertex_values.len() as i32);
        }
        if self.columns.edges || self.columns.elements {
            let edge_row = self.edge_row;
            self.edge_values
                .extend(edges.iter().map(|&e| edge_row(e) as i64));
            self.edge_offsets.push(self.edge_values.len() as i32);
        }
        if self.start_rows.len() >= BATCH_ROWS {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.start_rows.is_empty() {
            return Ok(());
        }
        let start = take(
            self.starts.as_ref(),
            &UInt32Array::from(std::mem::take(&mut self.start_rows)),
            None,
        )?;
        let end = take(
            self.values.as_ref(),
            &UInt32Array::from(std::mem::take(&mut self.ends)),
            None,
        )?;
        let mut columns: Vec<ArrayRef> = vec![
            start,
            end,
            Arc::new(Int64Array::from(std::mem::take(&mut self.lengths))),
        ];
        let vertex_offsets = std::mem::replace(&mut self.vertex_offsets, vec![0]);
        let vertex_values = take(
            self.values.as_ref(),
            &UInt32Array::from(std::mem::take(&mut self.vertex_values)),
            None,
        )?;
        let edge_offsets = std::mem::replace(&mut self.edge_offsets, vec![0]);
        let edge_values = std::mem::take(&mut self.edge_values);
        let field = |i: usize| match self.schema.field(i).data_type() {
            DataType::List(f) => Arc::clone(f),
            _ => unreachable!("a list column"),
        };
        let mut i = 3;
        if self.columns.vertices {
            columns.push(Arc::new(ListArray::try_new(
                field(i),
                OffsetBuffer::new(vertex_offsets.clone().into()),
                Arc::clone(&vertex_values),
                None,
            )?));
            i += 1;
        }
        if self.columns.edges {
            columns.push(Arc::new(ListArray::try_new(
                field(i),
                OffsetBuffer::new(edge_offsets.clone().into()),
                Arc::new(Int64Array::from(edge_values.clone())),
                None,
            )?));
            i += 1;
        }
        if self.columns.elements {
            let keys = vertex_values
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or_else(|| DataFusionError::Internal("element_id needs Int64 keys".into()))?;
            let mut values = Int64Builder::with_capacity(keys.len() + edge_values.len());
            let mut offsets = vec![0i32];
            for p in 0..vertex_offsets.len() - 1 {
                let (v0, v1) = (vertex_offsets[p] as usize, vertex_offsets[p + 1] as usize);
                let e0 = edge_offsets[p] as usize;
                for k in v0..v1 {
                    values.append_value(keys.value(k));
                    if k + 1 < v1 {
                        values.append_value(edge_values[e0 + (k - v0)]);
                    }
                }
                offsets.push(values.len() as i32);
            }
            columns.push(Arc::new(ListArray::try_new(
                field(i),
                OffsetBuffer::new(offsets.into()),
                Arc::new(values.finish()),
                None,
            )?));
        }
        self.batches
            .push(RecordBatch::try_new(Arc::clone(&self.schema), columns)?);
        Ok(())
    }
}

/// The targets of a search: the dense nodes its paths may end at, each with
/// a slot, and the target keys that are no node of the index.
struct Goals {
    slots: HashMap<u32, u32>,
    unknown: HashSet<ScalarValue>,
}

impl Goals {
    fn new(ids: &IdMap, targets: &ArrayRef) -> Result<Self> {
        let dense = ids
            .dense_from_array(targets.as_ref())
            .map_err(|e| DataFusionError::Execution(format!("in GRAPH_TABLE: {e}")))?;
        let mut slots = HashMap::new();
        let mut unknown = HashSet::new();
        for (row, d) in dense.iter().enumerate() {
            if targets.is_null(row) {
                continue;
            }
            match d {
                Some(d) => {
                    let next = slots.len() as u32;
                    slots.entry(*d).or_insert(next);
                }
                None => {
                    unknown.insert(ScalarValue::try_from_array(targets.as_ref(), row)?);
                }
            }
        }
        Ok(Self { slots, unknown })
    }
}

/// What one search is doing, apart from its rows.
struct Walker<'a> {
    topology: &'a Topology,
    search: &'a Search,
    restricted: bool,
    pruned: bool,
    states: States,
    entries: Vec<Entry>,
    next: Vec<(u32, u32)>,
    /// Paths listed so far.
    total: usize,
    /// Prefixes extended so far.
    expanded: usize,
    /// The targets' slots, when a pruned search has targets: their paths are
    /// known once pushed (see [`Reach`]).
    slots: Option<&'a HashMap<u32, u32>>,
    reach: Reach,
}

/// How far a pruned search with targets has reached them, counting the
/// prefixes it pushed. A pruned search lists every prefix it pushes to a
/// target (at `min` edges or more), in the order it pushed them, so once each
/// target has its `k` (`SHORTEST k`), or its first (`ALL SHORTEST`), no
/// prefix needs extending any more: for `ALL SHORTEST`, none at the
/// longest of those lengths or beyond.
#[derive(Default)]
struct Reach {
    /// Per slot, prefixes pushed.
    pushed: Vec<u32>,
    touched: Vec<u32>,
    /// Targets without all their paths yet.
    unfilled: usize,
    /// The longest length at which a target was first reached.
    longest: u32,
}

impl Reach {
    fn reset(&mut self, n: usize) {
        if self.pushed.len() != n {
            self.pushed = vec![0; n];
        }
        for slot in self.touched.drain(..) {
            self.pushed[slot as usize] = 0;
        }
        self.unfilled = n;
        self.longest = 0;
    }

    /// Whether a prefix of `len` edges still needs extending.
    fn extends(&self, len: u32, selector: Selector) -> bool {
        self.unfilled > 0 || (selector == Selector::AllShortest && len < self.longest)
    }
}

impl Walker<'_> {
    /// Push the prefixes that extend entry `at` by one edge.
    fn extend(&mut self, at: u32, start: u32) -> Result<()> {
        let entry = self.entries[at as usize];
        let min = self.search.quantifier.min;
        self.expanded += 1;
        steps(
            self.topology,
            self.search.direction,
            entry.node,
            &mut self.next,
        );
        for &(w, edge) in self.next.iter() {
            if self.restricted && !allowed(&self.entries, at, w, edge, self.search.mode, start) {
                continue;
            }
            let len = entry.len + 1;
            if self.pruned {
                let state = self.states.get(w, len.min(min));
                let accept = match self.search.selector {
                    Selector::All => true,
                    Selector::Shortest(k) => state.0 < k,
                    Selector::AllShortest => state.0 == 0 || state.1 == len,
                };
                if !accept {
                    continue;
                }
                if state.0 == 0 {
                    state.1 = len;
                }
                state.0 += 1;
            }
            self.entries.push(Entry {
                node: w,
                parent: at,
                edge,
                len,
            });
            // A pruned search lists this prefix: a target may have its paths.
            let listed = len >= min && !(self.search.mode == Mode::Acyclic && w == start);
            if let (true, true, Some(&slot)) =
                (self.pruned, listed, self.slots.and_then(|s| s.get(&w)))
            {
                let reach = &mut self.reach;
                let pushed = &mut reach.pushed[slot as usize];
                if *pushed == 0 {
                    reach.touched.push(slot);
                    reach.longest = reach.longest.max(len);
                }
                *pushed += 1;
                let filled = match self.search.selector {
                    Selector::All => false,
                    Selector::Shortest(k) => *pushed == k,
                    Selector::AllShortest => *pushed == 1,
                };
                if filled {
                    reach.unfilled -= 1;
                }
            }
            if self.entries.len() + self.total > MAX_PATHS {
                return Err(DataFusionError::Execution(format!(
                    "in GRAPH_TABLE: the path search reached {MAX_PATHS} paths; give the quantified edge a lower upper bound, or keep fewer paths with ANY SHORTEST"
                )));
            }
        }
        Ok(())
    }
}

/// Run `search` from each of `starts` (keys of the graph's id type, each
/// once) on the index `topology`/`ids`; the rows of [`schema`], and how many
/// path prefixes the search extended. `edge_row` gives the edge table's row
/// of an edge of the index (see [`super::GraphIndex::edge_row`]). With
/// `targets`, only the paths that end at one are kept, and a search with a
/// selector stops once every target has its paths (see the module docs).
/// With `known_only`, a start that is no node of the index has no path.
#[allow(clippy::too_many_arguments)]
pub fn run(
    topology: &Topology,
    ids: &IdMap,
    edge_row: &dyn Fn(u32) -> u64,
    search: &Search,
    starts: &ArrayRef,
    targets: Option<&ArrayRef>,
    known_only: bool,
    columns: PathColumns,
    schema: SchemaRef,
) -> Result<(Vec<RecordBatch>, usize)> {
    let dense = ids
        .dense_from_array(starts.as_ref())
        .map_err(|e| DataFusionError::Execution(format!("in GRAPH_TABLE: {e}")))?;
    let goals = targets.map(|t| Goals::new(ids, t)).transpose()?;
    let mut rows = Rows::new(ids, starts, columns, schema, edge_row)?;
    let Quantifier { min, max } = search.quantifier;
    let restricted = search.mode != Mode::Walk;
    let pruned = !restricted
        || (matches!(
            search.selector,
            Selector::Shortest(1) | Selector::AllShortest
        ) && min <= 1
            && !(search.mode == Mode::Trail && search.direction == Direction::Any));
    let mut walker = Walker {
        topology,
        search,
        restricted,
        pruned,
        states: States::new(topology.n_nodes(), min),
        entries: Vec::new(),
        next: Vec::new(),
        total: 0,
        expanded: 0,
        slots: goals.as_ref().map(|g| &g.slots),
        reach: Reach::default(),
    };
    // Per last node, for a search that lists every path: (paths kept, length of the first).
    let mut targets: HashMap<u32, (u32, u32)> = HashMap::new();
    // Per target slot, for this start: (paths kept, length of the first).
    let mut found: Vec<(u32, u32)> = vec![(0, 0); goals.as_ref().map_or(0, |g| g.slots.len())];
    let mut touched: Vec<u32> = Vec::new();
    let (mut nodes, mut edges) = (Vec::new(), Vec::new());
    for (row, start) in dense.iter().enumerate() {
        if starts.is_null(row) {
            continue;
        }
        let Some(start) = *start else {
            // A node with no edges: only the path of no edges.
            let ends_at_a_target = match &goals {
                None => true,
                Some(g) => g
                    .unknown
                    .contains(&ScalarValue::try_from_array(starts.as_ref(), row)?),
            };
            if min == 0 && !known_only && ends_at_a_target {
                rows.push(row as u32, &[row as u32], &[])?;
            }
            continue;
        };
        let row = row as u32;
        if goals.as_ref().is_some_and(|g| g.slots.is_empty()) {
            continue; // no target is a node: no path ends at one
        }
        for slot in touched.drain(..) {
            found[slot as usize] = (0, 0);
        }
        walker.reach.reset(found.len());
        // Targets not yet done, and (ALL SHORTEST) the length past which
        // none has another path once every one is found.
        let mut remaining = found.len();
        let mut longest = 0u32;
        let mut stop_after: Option<u32> = None;
        walker.states.clear();
        targets.clear();
        walker.entries.clear();
        walker.entries.push(Entry {
            node: start,
            parent: ROOT,
            edge: 0,
            len: 0,
        });
        *walker.states.get(start, 0) = (1, 0);
        let mut head = 0;
        'search: while head < walker.entries.len() {
            let at = head as u32;
            let entry = walker.entries[head];
            head += 1;
            if stop_after.is_some_and(|len| entry.len > len) {
                break;
            }
            let c = entry.len.min(min);
            // The target slot of the entry's node, when the search has targets;
            // with targets, a node that is none of them ends no path.
            //
            // Two plain values rather than one `Option<Option<u32>>`: rustc
            // 1.97 at `opt-level` "s" and "z" (the browser engine's build)
            // matched `Some(Some(_))` on a `None` here, and indexed `found`
            // out of bounds. The browser parity test runs GRAPH_TABLE at "z".
            let (slot, ends_here) = match &goals {
                None => (None, true),
                Some(g) => {
                    let slot = g.slots.get(&entry.node).copied();
                    (slot, slot.is_some())
                }
            };
            if c == min && ends_here {
                let keep = if pruned {
                    true
                } else {
                    let t = targets.entry(entry.node).or_insert((0, entry.len));
                    let keep = match search.selector {
                        Selector::All => true,
                        Selector::Shortest(k) => t.0 < k,
                        Selector::AllShortest => t.1 == entry.len,
                    };
                    if keep {
                        t.0 += 1;
                    }
                    keep
                };
                // ACYCLIC: a path back to its start is a cycle.
                let closed = search.mode == Mode::Acyclic && entry.len > 0 && entry.node == start;
                if keep && !closed {
                    nodes.clear();
                    edges.clear();
                    let mut i = at;
                    while i != ROOT {
                        let e = walker.entries[i as usize];
                        nodes.push(rows.dense(e.node));
                        if e.parent != ROOT {
                            edges.push(e.edge);
                        }
                        i = e.parent;
                    }
                    nodes.reverse();
                    edges.reverse();
                    rows.push(row, &nodes, &edges)?;
                    walker.total += 1;
                    if let Some(slot) = slot {
                        let f = &mut found[slot as usize];
                        if f.0 == 0 {
                            touched.push(slot);
                            f.1 = entry.len;
                        }
                        f.0 += 1;
                        let done = match search.selector {
                            Selector::All => false,
                            Selector::Shortest(k) => f.0 == k,
                            Selector::AllShortest => f.0 == 1,
                        };
                        if done {
                            remaining -= 1;
                            longest = longest.max(f.1);
                            if remaining == 0 {
                                match search.selector {
                                    Selector::AllShortest => stop_after = Some(longest),
                                    _ => break 'search,
                                }
                            }
                        }
                    }
                }
            }
            if max.is_some_and(|max| entry.len >= max) {
                continue;
            }
            if pruned && goals.is_some() && !walker.reach.extends(entry.len, search.selector) {
                continue; // every target has its paths among those pushed
            }
            walker.extend(at, start)?;
        }
    }
    rows.flush()?;
    Ok((rows.batches, walker.expanded))
}

#[cfg(test)]
mod tests {
    use datafusion::arrow::array::{AsArray, Int64Array};
    use datafusion::arrow::datatypes::Int64Type;

    use super::*;

    /// The index of edges `(src, dst)`, in row order.
    fn index(edges: &[(i64, i64)]) -> (Topology, IdMap) {
        let src: ArrayRef = Arc::new(Int64Array::from(
            edges.iter().map(|e| e.0).collect::<Vec<_>>(),
        ));
        let dst: ArrayRef = Arc::new(Int64Array::from(
            edges.iter().map(|e| e.1).collect::<Vec<_>>(),
        ));
        let mut interner = ursa_core::EdgeInterner::with_capacity(edges.len());
        interner.push(src.as_ref(), dst.as_ref()).unwrap();
        let (ids, s, d) = interner.finish();
        (Topology::build(ids.len(), s, d), ids)
    }

    fn search(
        direction: Direction,
        min: u32,
        max: Option<u32>,
        selector: Selector,
        mode: Mode,
    ) -> Search {
        Search {
            direction,
            quantifier: Quantifier { min, max },
            selector,
            mode,
        }
    }

    /// Each path as (vertices, edges).
    /// A path: its vertices and its edges.
    type Path = (Vec<i64>, Vec<i64>);

    fn paths(edges: &[(i64, i64)], starts: &[i64], s: Search) -> Vec<Path> {
        targeted(edges, starts, None, s).0
    }

    /// Each path as (vertices, edges), searched toward `targets`, and how
    /// many prefixes the search extended.
    fn targeted(
        edges: &[(i64, i64)],
        starts: &[i64],
        targets: Option<&[i64]>,
        s: Search,
    ) -> (Vec<Path>, usize) {
        let (topology, ids) = index(edges);
        let targets: Option<ArrayRef> =
            targets.map(|t| Arc::new(Int64Array::from(t.to_vec())) as ArrayRef);
        let columns = PathColumns {
            vertices: true,
            edges: true,
            elements: true,
        };
        let schema = Arc::new(super::schema(&DataType::Int64, columns));
        let starts: ArrayRef = Arc::new(Int64Array::from(starts.to_vec()));
        let (batches, expanded) = run(
            &topology,
            &ids,
            &|e| u64::from(e),
            &s,
            &starts,
            targets.as_ref(),
            false,
            columns,
            schema,
        )
        .unwrap();
        let mut out = Vec::new();
        for b in batches {
            let v = b.column(3).as_list::<i32>();
            let e = b.column(4).as_list::<i32>();
            let el = b.column(5).as_list::<i32>();
            for r in 0..b.num_rows() {
                let vs: Vec<i64> = v.value(r).as_primitive::<Int64Type>().values().to_vec();
                let es: Vec<i64> = e.value(r).as_primitive::<Int64Type>().values().to_vec();
                let els: Vec<i64> = el.value(r).as_primitive::<Int64Type>().values().to_vec();
                assert_eq!(els.len(), vs.len() + es.len());
                assert_eq!(
                    b.column(2).as_primitive::<Int64Type>().value(r),
                    es.len() as i64
                );
                out.push((vs, es));
            }
        }
        (out, expanded)
    }

    /// A search toward targets keeps the paths a search without them finds
    /// to those targets, in the same order, extending fewer prefixes.
    #[test]
    fn targets_keep_the_same_paths_and_stop_early() {
        // A chain 0 → 1 → … → 9 with a fan out of every node, and shortcuts.
        let mut edges: Vec<(i64, i64)> = (0..9).map(|i| (i, i + 1)).collect();
        for i in 0..10 {
            edges.extend((0..4).map(|j| (i, 100 + 10 * i + j)));
        }
        edges.extend([(0, 5), (2, 7), (5, 5), (7, 2), (3, 1)]);
        let graphs: [&[(i64, i64)]; 2] = [KNOW, &edges];
        let mut stopped_early = false;
        for edges in graphs {
            for direction in [Direction::Right, Direction::Left, Direction::Any] {
                for selector in [
                    Selector::All,
                    Selector::Shortest(1),
                    Selector::Shortest(2),
                    Selector::AllShortest,
                ] {
                    for mode in [Mode::Walk, Mode::Trail, Mode::Simple, Mode::Acyclic] {
                        for (min, max) in [(0, Some(3)), (1, None), (2, Some(4)), (1, Some(1))] {
                            if selector == Selector::All && mode == Mode::Walk && max.is_none() {
                                continue;
                            }
                            let s = search(direction, min, max, selector, mode);
                            for (starts, targets) in [
                                (&[0][..], &[3][..]),
                                (&[0, 4], &[0, 3, 9]),
                                (&[1, 2], &[2, 77]),
                                (&[3], &[7, 5, 1]),
                            ] {
                                let (all, n_all) = targeted(edges, starts, None, s);
                                let kept: Vec<_> = all
                                    .into_iter()
                                    .filter(|(v, _)| targets.contains(v.last().unwrap()))
                                    .collect();
                                let (found, n) = targeted(edges, starts, Some(targets), s);
                                assert_eq!(found, kept, "{s} from {starts:?} to {targets:?}");
                                assert!(n <= n_all, "{s}");
                                stopped_early |= n < n_all;
                            }
                        }
                    }
                }
            }
        }
        assert!(stopped_early);
    }

    // DuckPGQ's test graph: Daniel 0, Tavneet 1, Gabor 2, Peter 3, David 4.
    const KNOW: &[(i64, i64)] = &[
        (0, 1),
        (0, 2),
        (0, 3),
        (3, 0),
        (1, 2),
        (1, 3),
        (2, 3),
        (4, 3),
    ];

    #[test]
    fn any_shortest_takes_the_first_edges_and_can_return_to_the_start() {
        let found = paths(
            KNOW,
            &[4],
            search(
                Direction::Right,
                1,
                Some(3),
                Selector::Shortest(1),
                Mode::Walk,
            ),
        );
        assert_eq!(
            found,
            vec![
                (vec![4, 3], vec![7]),
                (vec![4, 3, 0], vec![7, 3]),
                (vec![4, 3, 0, 1], vec![7, 3, 0]),
                (vec![4, 3, 0, 2], vec![7, 3, 1]),
            ]
        );
        // From 0, a shortest walk of one edge or more back to 0 is 0→3→0.
        let found = paths(
            KNOW,
            &[0],
            search(
                Direction::Right,
                1,
                Some(3),
                Selector::Shortest(1),
                Mode::Walk,
            ),
        );
        assert!(found.contains(&(vec![0, 3, 0], vec![2, 3])));
        // ACYCLIC drops it.
        let found = paths(
            KNOW,
            &[0],
            search(
                Direction::Right,
                1,
                Some(3),
                Selector::Shortest(1),
                Mode::Acyclic,
            ),
        );
        assert!(found.iter().all(|(v, _)| v.last() != Some(&0)));
    }

    #[test]
    fn walks_trails_and_counts() {
        // Every walk of 1 or 2 edges from Peter: 3→0, then 0→1, 0→2, 0→3.
        let found = paths(
            KNOW,
            &[3],
            search(Direction::Right, 1, Some(2), Selector::All, Mode::Walk),
        );
        assert_eq!(found.len(), 4);
        // Without a bound under ACYCLIC: every simple path, finitely many.
        let acyclic = paths(
            KNOW,
            &[3],
            search(Direction::Right, 1, None, Selector::All, Mode::Acyclic),
        );
        assert!(acyclic.iter().all(|(v, _)| {
            let mut s = v.clone();
            s.sort();
            s.dedup();
            s.len() == v.len()
        }));
        // 3→0, 3→0→1, 3→0→2 and 3→0→1→2.
        assert_eq!(acyclic.len(), 4);
        // SHORTEST 2 from 0 to 3: 0→3 (edge 2), then 0→1→3 (0, 5) before 0→2→3 (1, 6).
        let found = paths(
            KNOW,
            &[0],
            search(Direction::Right, 1, None, Selector::Shortest(2), Mode::Walk),
        );
        let to3: Vec<_> = found.iter().filter(|(v, _)| v.last() == Some(&3)).collect();
        assert_eq!(
            to3,
            vec![&(vec![0, 3], vec![2]), &(vec![0, 1, 3], vec![0, 5])]
        );
        // ALL SHORTEST from 0 to 3 is the one edge; to 2: 0→2 alone.
        let found = paths(
            KNOW,
            &[1],
            search(Direction::Right, 1, None, Selector::AllShortest, Mode::Walk),
        );
        let to0: Vec<_> = found.iter().filter(|(v, _)| v.last() == Some(&0)).collect();
        assert_eq!(to0, vec![&(vec![1, 3, 0], vec![5, 3])]);
        let found = paths(
            KNOW,
            &[4],
            search(
                Direction::Right,
                3,
                Some(3),
                Selector::AllShortest,
                Mode::Walk,
            ),
        );
        let to2: Vec<_> = found.iter().filter(|(v, _)| v.last() == Some(&2)).collect();
        assert_eq!(to2, vec![&(vec![4, 3, 0, 2], vec![7, 3, 1])]);
    }

    #[test]
    fn zero_length_undirected_and_trails() {
        // A start with no edges has its path of no edges.
        let found = paths(
            KNOW,
            &[9, 0],
            search(Direction::Right, 0, Some(0), Selector::All, Mode::Walk),
        );
        assert_eq!(found, vec![(vec![9], vec![]), (vec![0], vec![])]);
        // Undirected, a self-loop is followed once.
        let found = paths(
            &[(1, 1), (1, 2)],
            &[1],
            search(Direction::Any, 1, Some(1), Selector::All, Mode::Walk),
        );
        assert_eq!(found, vec![(vec![1, 1], vec![0]), (vec![1, 2], vec![1])]);
        // An undirected trail back to the start cannot reuse its edge: 1-2-1
        // takes the two parallel edges.
        let found = paths(
            &[(1, 2), (2, 1)],
            &[1],
            search(Direction::Any, 1, Some(2), Selector::All, Mode::Trail),
        );
        assert_eq!(
            found,
            vec![
                (vec![1, 2], vec![0]),
                (vec![1, 2], vec![1]),
                (vec![1, 2, 1], vec![0, 1]),
                (vec![1, 2, 1], vec![1, 0]),
            ]
        );
        // Left: against the edges.
        let found = paths(
            KNOW,
            &[3],
            search(Direction::Left, 1, Some(1), Selector::All, Mode::Walk),
        );
        assert_eq!(
            found,
            vec![
                (vec![3, 0], vec![2]),
                (vec![3, 1], vec![5]),
                (vec![3, 2], vec![6]),
                (vec![3, 4], vec![7])
            ]
        );
        // SIMPLE: back to the start is allowed, once.
        let found = paths(
            KNOW,
            &[0],
            search(Direction::Right, 2, Some(2), Selector::All, Mode::Simple),
        );
        assert!(found.contains(&(vec![0, 3, 0], vec![2, 3])));
    }
}
