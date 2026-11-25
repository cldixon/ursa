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
//! the mode allows.
//!
//! A pruned search keeps its states in a table it tests before anything else
//! per edge (one bit a state for `ANY SHORTEST`), and reads the adjacency
//! lists where they lie (#451). From several start nodes, a walker per thread
//! takes the next start, and the starts' rows are appended in start order: the
//! rows, their order and the limit's verdict are those of one start after
//! another (see [`run`]).
//!
//! ## The limit
//!
//! A search stops with an error once it holds more paths than
//! `ursa.max_paths` (default [`MAX_PATHS`]; 0 for no limit). What counts
//! depends on what the search can hold (#384, N50):
//!
//! - A pruned search with `ANY SHORTEST` or `SHORTEST k` holds at most `k`
//!   prefixes per (node, edges so far up to `min`), so its memory is bounded
//!   by the graph's nodes. It counts the paths it keeps (for `ANY SHORTEST`,
//!   one per pair of first and last node: the destinations), never fewer
//!   than `k` per node of the graph, so one start is never refused what
//!   `hop` would list. When that floor is above `ursa.max_paths`, the
//!   search says so in a notice ([`floor_notice`]).
//! - Every other search counts the prefixes it holds and the paths it keeps:
//!   a walk's prefixes can grow exponentially with its length.

use std::collections::HashSet;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use datafusion::arrow::array::{
    Array, ArrayBuilder, ArrayRef, Int64Array, Int64Builder, ListArray, UInt32Array,
};
use datafusion::arrow::buffer::OffsetBuffer;
use datafusion::arrow::compute::{interleave, take};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::{DFSchema, DFSchemaRef, ScalarValue};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{Expr, LogicalPlan, UserDefinedLogicalNodeCore};
use rustc_hash::FxHashMap;
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

/// The default `ursa.max_paths`: the most paths one search holds before it
/// stops with an error (see the module docs).
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

/// A dense state table may take this many bytes whatever the graph's size
/// (the 2^26 states of 8 bytes the table was limited to before #451)...
const DENSE_MIN_BYTES: usize = 512 << 20;
/// ...or this many per node of the graph, which holds that much per node
/// already: `ALL SHORTEST` with a `+` quantifier (two states per node) stays
/// dense at any size.
const DENSE_BYTES_PER_NODE: usize = 16;

/// The most bytes of state tables a search from many start nodes gives its
/// parallel walkers together; past it, fewer walkers run.
#[cfg(not(target_arch = "wasm32"))]
const PARALLEL_STATES_BYTES: usize = 1 << 30;

/// Per search state, (node, edges so far up to `min`), numbered
/// `node * width + c`: how many prefixes reached it, and the length of the
/// first. A pruned search keeps a prefix only if its state accepts it.
trait States: Send {
    /// Whether to keep a prefix of `len` edges that reaches state `i`,
    /// counting it if so.
    fn accept(&mut self, i: usize, len: u32, selector: Selector) -> bool;
    /// Count the start's prefix of no edges at state `i`.
    fn start(&mut self, i: usize);
    /// Forget every state reached.
    fn clear(&mut self);
}

/// The selector's test on a state (prefixes kept, length of the first),
/// counting the prefix if it passes.
#[inline(always)]
fn accept_pair(state: &mut (u32, u32), len: u32, selector: Selector) -> bool {
    let accept = match selector {
        Selector::All => true,
        Selector::Shortest(k) => state.0 < k,
        Selector::AllShortest => state.0 == 0 || state.1 == len,
    };
    if accept {
        if state.0 == 0 {
            state.1 = len;
        }
        state.0 += 1;
    }
    accept
}

/// No states: a search that is not pruned keeps every prefix its mode
/// allows.
struct NoStates;

impl States for NoStates {
    fn accept(&mut self, _: usize, _: u32, _: Selector) -> bool {
        true
    }
    fn start(&mut self, _: usize) {}
    fn clear(&mut self) {}
}

/// `ANY SHORTEST`'s states: one bit each, whether a prefix reached it (it
/// keeps the first, and the length is never asked). 2 bits a node for a `+`
/// quantifier, so it is dense at any size.
struct Bits {
    words: Vec<u64>,
    touched: Vec<usize>,
}

impl States for Bits {
    #[inline(always)]
    fn accept(&mut self, i: usize, _: u32, _: Selector) -> bool {
        let (word, bit) = (&mut self.words[i >> 6], 1u64 << (i & 63));
        if *word & bit != 0 {
            return false;
        }
        *word |= bit;
        self.touched.push(i);
        true
    }

    fn start(&mut self, i: usize) {
        self.accept(i, 0, Selector::Shortest(1));
    }

    fn clear(&mut self) {
        // Every bit set is in `touched`, so a word with one is cleared whole.
        if self.touched.len() > self.words.len() {
            self.words.fill(0);
        } else {
            for &i in &self.touched {
                self.words[i >> 6] = 0;
            }
        }
        self.touched.clear();
    }
}

/// The states as a table, for `SHORTEST k` and `ALL SHORTEST`.
struct Pairs {
    seen: Vec<(u32, u32)>,
    touched: Vec<usize>,
}

impl States for Pairs {
    #[inline(always)]
    fn accept(&mut self, i: usize, len: u32, selector: Selector) -> bool {
        let state = &mut self.seen[i];
        let first = state.0 == 0;
        let accept = accept_pair(state, len, selector);
        if accept && first {
            self.touched.push(i);
        }
        accept
    }

    fn start(&mut self, i: usize) {
        if self.seen[i].0 == 0 {
            self.touched.push(i);
        }
        self.seen[i] = (1, 0);
    }

    fn clear(&mut self) {
        for i in self.touched.drain(..) {
            self.seen[i] = (0, 0);
        }
    }
}

/// The states as a map, when a table would be too large.
struct Sparse(FxHashMap<usize, (u32, u32)>);

impl States for Sparse {
    #[inline]
    fn accept(&mut self, i: usize, len: u32, selector: Selector) -> bool {
        accept_pair(self.0.entry(i).or_insert((0, 0)), len, selector)
    }

    fn start(&mut self, i: usize) {
        self.0.insert(i, (1, 0));
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

/// Which state table a search uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    None,
    Bits,
    Pairs,
    Sparse,
}

impl Table {
    /// The table for a search over `n_nodes` nodes with `width` states each.
    fn choose(pruned: bool, selector: Selector, n_nodes: usize, width: usize) -> Self {
        if !pruned {
            return Table::None;
        }
        let budget = DENSE_MIN_BYTES.max(n_nodes.saturating_mul(DENSE_BYTES_PER_NODE));
        let states = n_nodes.saturating_mul(width);
        if selector == Selector::Shortest(1) && states / 8 <= budget {
            Table::Bits
        } else if states.saturating_mul(8) <= budget {
            Table::Pairs
        } else {
            Table::Sparse
        }
    }

    /// The bytes one walker's table takes once the search has touched it
    /// all, if they are known.
    fn bytes(self, states: usize) -> Option<usize> {
        match self {
            Table::None => Some(0),
            Table::Bits => Some(states.div_ceil(64) * 8),
            Table::Pairs => Some(states * 8),
            Table::Sparse => None,
        }
    }
}

/// The edges a step can take from `u`, in edge-row order, as (next node,
/// edge row): the adjacency lists read as they are, and for an undirected
/// step both merged by edge row as they are read. A self-loop is in both
/// lists: it is taken once, forward.
struct Undirected<'a> {
    forward: (&'a [u32], &'a [u32]),
    backward: (&'a [u32], &'a [u32]),
    u: u32,
    i: usize,
    j: usize,
}

impl Iterator for Undirected<'_> {
    type Item = (u32, u32);

    #[inline]
    fn next(&mut self) -> Option<(u32, u32)> {
        let (bw, be) = self.backward;
        while self.j < bw.len() && bw[self.j] == self.u {
            self.j += 1;
        }
        let (fw, fe) = self.forward;
        let take_forward = self.j == bw.len() || (self.i < fw.len() && fe[self.i] < be[self.j]);
        if take_forward {
            let i = self.i;
            if i == fw.len() {
                return None;
            }
            self.i += 1;
            Some((fw[i], fe[i]))
        } else {
            let j = self.j;
            self.j += 1;
            Some((bw[j], be[j]))
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

/// The paths one start node found, in the order found, and what its search
/// counted for the limit.
#[derive(Default)]
struct Found {
    /// The start's row.
    row: u32,
    /// Per path, its last node (an index into [`Rows`]' values) and length.
    ends: Vec<u32>,
    lengths: Vec<i64>,
    /// The paths' nodes, first to last, one after another, when asked.
    vertices: Vec<u32>,
    /// The paths' edges' rows in the edge table, when asked.
    edges: Vec<i64>,
    /// Prefixes extended.
    expanded: usize,
    /// Paths that count toward the limit (the path of no edges of a start
    /// that is no node does not).
    kept: usize,
    /// The most prefixes and paths held at once, where the search checked
    /// (see [`Walker::extend`]); `None` if it never did.
    peak: Option<usize>,
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
    /// Every node of the index, as user ids. A node is referred to by its
    /// index in the starts, then these (see [`Rows::gather`]).
    ids: Option<ArrayRef>,
}

impl<'a> Rows<'a> {
    fn new(ids: &'a IdMap, starts: &'a ArrayRef, columns: PathColumns, schema: SchemaRef) -> Self {
        Self {
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
            // The ids array is the index's own (borrowed from a stored
            // index's file): it is read only where a path ends or passes.
            ids: (!ids.is_empty()).then(|| ids.user_id_array()),
        }
    }

    /// Append one start's paths.
    fn append(&mut self, found: &Found) -> Result<()> {
        let vertices = self.columns.vertices || self.columns.elements;
        let edges = self.columns.edges || self.columns.elements;
        let (mut v, mut e) = (0, 0);
        for (&end, &len) in found.ends.iter().zip(&found.lengths) {
            let len = len as usize;
            self.start_rows.push(found.row);
            self.ends.push(end);
            self.lengths.push(len as i64);
            if vertices {
                self.vertex_values
                    .extend_from_slice(&found.vertices[v..v + len + 1]);
                v += len + 1;
                self.vertex_offsets.push(self.vertex_values.len() as i32);
            }
            if edges {
                self.edge_values.extend_from_slice(&found.edges[e..e + len]);
                e += len;
                self.edge_offsets.push(self.edge_values.len() as i32);
            }
            if self.start_rows.len() >= BATCH_ROWS {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// The user ids of `nodes`: indexes into the starts, then into the
    /// index's ids after them.
    fn gather(&self, nodes: Vec<u32>) -> Result<ArrayRef> {
        let n = self.starts.len() as u32;
        let Some(ids) = &self.ids else {
            return Ok(take(self.starts.as_ref(), &UInt32Array::from(nodes), None)?);
        };
        if nodes.iter().all(|&i| i >= n) {
            let nodes: Vec<u32> = nodes.into_iter().map(|i| i - n).collect();
            return Ok(take(ids.as_ref(), &UInt32Array::from(nodes), None)?);
        }
        let at: Vec<(usize, usize)> = nodes
            .into_iter()
            .map(|i| match i.checked_sub(n) {
                Some(d) => (1, d as usize),
                None => (0, i as usize),
            })
            .collect();
        Ok(interleave(&[self.starts.as_ref(), ids.as_ref()], &at)?)
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
        let ends = std::mem::take(&mut self.ends);
        let end = self.gather(ends)?;
        let mut columns: Vec<ArrayRef> = vec![
            start,
            end,
            Arc::new(Int64Array::from(std::mem::take(&mut self.lengths))),
        ];
        let vertex_offsets = std::mem::replace(&mut self.vertex_offsets, vec![0]);
        let vertex_values = std::mem::take(&mut self.vertex_values);
        let vertex_values = self.gather(vertex_values)?;
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
    slots: FxHashMap<u32, u32>,
    unknown: HashSet<ScalarValue>,
}

impl Goals {
    fn new(ids: &IdMap, targets: &ArrayRef) -> Result<Self> {
        let dense = ids
            .dense_from_array(targets.as_ref())
            .map_err(|e| DataFusionError::Execution(format!("in GRAPH_TABLE: {e}")))?;
        let mut slots = FxHashMap::default();
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

/// What every walker of one search shares.
struct Plan<'a> {
    topology: &'a Topology,
    edge_row: &'a (dyn Fn(u32) -> u64 + Sync),
    search: &'a Search,
    starts: &'a ArrayRef,
    /// The starts' dense nodes.
    dense: &'a [Option<u32>],
    goals: Option<&'a Goals>,
    known_only: bool,
    columns: PathColumns,
    restricted: bool,
    pruned: bool,
    /// States per node: `min + 1`.
    width: usize,
    /// Whether the search's memory is bounded by the graph's nodes (see the
    /// module docs): it counts only the paths it keeps.
    bounded: bool,
    /// The most prefixes and paths the search may hold (`ursa.max_paths`),
    /// unless it is `bounded`.
    cap: Option<usize>,
    /// The most paths a `bounded` search may keep.
    kept_limit: Option<usize>,
    /// Paths a `bounded` search has kept so far, from every start (each
    /// walker adds its own now and then, and at the end of a start).
    kept_all: AtomicUsize,
    /// Set when a search from many start nodes has failed: the others stop.
    stop: AtomicBool,
}

/// A `bounded` walker adds the paths it kept to [`Plan::kept_all`] after
/// this many.
const KEPT_BATCH: usize = 4096;

/// What one search is doing, from one start node after another.
struct Walker<'a, T> {
    plan: &'a Plan<'a>,
    states: T,
    entries: Vec<Entry>,
    /// The paths of the current start.
    found: Found,
    /// Its paths kept so far, and those not yet added to `kept_all`.
    kept: usize,
    unshared: usize,
    /// Paths kept by the starts before it (as far as known: see
    /// [`drive_parallel`]).
    before: usize,
    /// Per last node, for a search that lists every path: (paths kept,
    /// length of the first).
    targets: FxHashMap<u32, (u32, u32)>,
    /// Per target slot, for this start: (paths kept, length of the first).
    slot_paths: Vec<(u32, u32)>,
    touched: Vec<u32>,
    reach: Reach,
    /// Scratch for one path's nodes and edges.
    nodes: Vec<u32>,
    edges: Vec<u32>,
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

impl<'a, T: States> Walker<'a, T> {
    fn new(plan: &'a Plan<'a>, states: T) -> Self {
        let slots = plan.goals.map_or(0, |g| g.slots.len());
        Self {
            plan,
            states,
            entries: Vec::new(),
            found: Found::default(),
            kept: 0,
            unshared: 0,
            before: 0,
            targets: FxHashMap::default(),
            slot_paths: vec![(0, 0); slots],
            touched: Vec::new(),
            reach: Reach::default(),
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// Push the prefixes that extend entry `at` by one edge.
    #[inline]
    fn extend(&mut self, at: u32, start: u32) -> Result<()> {
        let topology = self.plan.topology;
        let u = self.entries[at as usize].node;
        self.found.expanded += 1;
        match self.plan.search.direction {
            Direction::Right => {
                let a = topology.out();
                let steps = a.neighbors(u).iter().copied();
                self.extend_with(at, start, steps.zip(a.edge_ids(u).iter().copied()))
            }
            Direction::Left => {
                let a = topology.incoming();
                let steps = a.neighbors(u).iter().copied();
                self.extend_with(at, start, steps.zip(a.edge_ids(u).iter().copied()))
            }
            Direction::Any => {
                let (out, inc) = (topology.out(), topology.incoming());
                let steps = Undirected {
                    forward: (out.neighbors(u), out.edge_ids(u)),
                    backward: (inc.neighbors(u), inc.edge_ids(u)),
                    u,
                    i: 0,
                    j: 0,
                };
                self.extend_with(at, start, steps)
            }
        }
    }

    /// [`Self::extend`] over the edges `steps` (next node, edge row), in
    /// edge-row order.
    #[inline(always)]
    fn extend_with(
        &mut self,
        at: u32,
        start: u32,
        steps: impl Iterator<Item = (u32, u32)>,
    ) -> Result<()> {
        let plan = self.plan;
        let search = plan.search;
        let min = search.quantifier.min;
        let len = self.entries[at as usize].len + 1;
        let c = len.min(min) as usize;
        let slots = plan.goals.filter(|_| plan.pruned).map(|g| &g.slots);
        let cap = plan.cap.filter(|_| !plan.bounded);
        for (w, edge) in steps {
            if plan.restricted && !allowed(&self.entries, at, w, edge, search.mode, start) {
                continue;
            }
            // The cheap test first: most edges reach a state already full.
            if plan.pruned
                && !self
                    .states
                    .accept(w as usize * plan.width + c, len, search.selector)
            {
                continue;
            }
            self.entries.push(Entry {
                node: w,
                parent: at,
                edge,
                len,
            });
            if let Some(slots) = slots {
                // A pruned search lists this prefix: a target may have its paths.
                let listed = len >= min && !(search.mode == Mode::Acyclic && w == start);
                if let (true, Some(&slot)) = (listed, slots.get(&w)) {
                    let reach = &mut self.reach;
                    let pushed = &mut reach.pushed[slot as usize];
                    if *pushed == 0 {
                        reach.touched.push(slot);
                        reach.longest = reach.longest.max(len);
                    }
                    *pushed += 1;
                    let filled = match search.selector {
                        Selector::All => false,
                        Selector::Shortest(k) => *pushed == k,
                        Selector::AllShortest => *pushed == 1,
                    };
                    if filled {
                        reach.unfilled -= 1;
                    }
                }
            }
            if let Some(cap) = cap {
                let held = self.entries.len() + self.kept;
                self.found.peak = Some(self.found.peak.map_or(held, |p| p.max(held)));
                if held + self.before > cap {
                    return Err(too_many_paths(cap, search, false));
                }
            }
        }
        Ok(())
    }

    /// Add the paths kept and not yet shared to [`Plan::kept_all`]; an error
    /// once every start's together are over the limit.
    fn share_kept(&mut self) -> Result<()> {
        let plan = self.plan;
        let Some(limit) = plan.kept_limit else {
            return Ok(());
        };
        let all = plan.kept_all.fetch_add(self.unshared, Ordering::Relaxed) + self.unshared;
        self.unshared = 0;
        if all > limit {
            return Err(too_many_paths(limit, plan.search, true));
        }
        Ok(())
    }

    /// The paths from the start at `row`, given that the starts before it
    /// kept `before` paths (or at least that many).
    fn search(&mut self, row: usize, before: usize) -> Result<Found> {
        let plan = self.plan;
        let search = plan.search;
        let min = search.quantifier.min;
        let max = search.quantifier.max;
        let mut found = std::mem::take(&mut self.found);
        found.row = row as u32;
        found.ends.clear();
        found.lengths.clear();
        found.vertices.clear();
        found.edges.clear();
        found.expanded = 0;
        found.kept = 0;
        found.peak = None;
        self.found = found;
        if plan.starts.is_null(row) {
            return Ok(std::mem::take(&mut self.found));
        }
        let vertices = plan.columns.vertices || plan.columns.elements;
        let edges = plan.columns.edges || plan.columns.elements;
        let offset = plan.starts.len() as u32;
        let Some(start) = plan.dense[row] else {
            // A node with no edges: only the path of no edges.
            let ends_at_a_target = match plan.goals {
                None => true,
                Some(g) => g
                    .unknown
                    .contains(&ScalarValue::try_from_array(plan.starts.as_ref(), row)?),
            };
            if min == 0 && !plan.known_only && ends_at_a_target {
                let f = &mut self.found;
                f.ends.push(row as u32);
                f.lengths.push(0);
                if vertices {
                    f.vertices.push(row as u32);
                }
            }
            return Ok(std::mem::take(&mut self.found));
        };
        if plan.goals.is_some_and(|g| g.slots.is_empty()) {
            return Ok(std::mem::take(&mut self.found)); // no target is a node: no path ends at one
        }
        self.kept = 0;
        self.unshared = 0;
        self.before = before;
        for slot in self.touched.drain(..) {
            self.slot_paths[slot as usize] = (0, 0);
        }
        self.reach.reset(self.slot_paths.len());
        // Targets not yet done, and (ALL SHORTEST) the length past which
        // none has another path once every one is found.
        let mut remaining = self.slot_paths.len();
        let mut longest = 0u32;
        let mut stop_after: Option<u32> = None;
        self.states.clear();
        self.targets.clear();
        self.entries.clear();
        self.entries.push(Entry {
            node: start,
            parent: ROOT,
            edge: 0,
            len: 0,
        });
        self.states.start(start as usize * plan.width);
        let mut head = 0;
        'search: while head < self.entries.len() {
            if head % 1024 == 1023 && plan.stop.load(Ordering::Relaxed) {
                return Err(DataFusionError::Execution("stopped".into()));
            }
            let at = head as u32;
            let entry = self.entries[head];
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
            // matched `Some(Some(_))` on a `None` here, and indexed out of
            // bounds. The browser parity test runs GRAPH_TABLE at "z".
            let (slot, ends_here) = match plan.goals {
                None => (None, true),
                Some(g) => {
                    let slot = g.slots.get(&entry.node).copied();
                    (slot, slot.is_some())
                }
            };
            if c == min && ends_here {
                let keep = if plan.pruned {
                    true
                } else {
                    let t = self.targets.entry(entry.node).or_insert((0, entry.len));
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
                    self.nodes.clear();
                    self.edges.clear();
                    let mut i = at;
                    while i != ROOT {
                        let e = self.entries[i as usize];
                        self.nodes.push(offset + e.node);
                        if e.parent != ROOT {
                            self.edges.push(e.edge);
                        }
                        i = e.parent;
                    }
                    let f = &mut self.found;
                    f.ends.push(offset + entry.node);
                    f.lengths.push(i64::from(entry.len));
                    if vertices {
                        f.vertices.extend(self.nodes.iter().rev());
                    }
                    if edges {
                        let edge_row = plan.edge_row;
                        f.edges
                            .extend(self.edges.iter().rev().map(|&e| edge_row(e) as i64));
                    }
                    f.kept += 1;
                    self.kept += 1;
                    self.unshared += 1;
                    if self.unshared == KEPT_BATCH {
                        self.share_kept()?;
                    }
                    if let Some(slot) = slot {
                        let f = &mut self.slot_paths[slot as usize];
                        if f.0 == 0 {
                            self.touched.push(slot);
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
            if plan.pruned
                && plan.goals.is_some()
                && !self.reach.extends(entry.len, search.selector)
            {
                continue; // every target has its paths among those pushed
            }
            self.extend(at, start)?;
        }
        self.share_kept()?;
        Ok(std::mem::take(&mut self.found))
    }
}

/// The error of a search that reached `limit` paths, with the remedy that
/// fits it: a search with a selector already keeps the fewest paths, so it
/// is pointed to `hop` (a `bounded` one says it keeps one path per pair of
/// nodes, or `k`); one without a selector to `ANY SHORTEST`. Either may
/// raise `ursa.max_paths`.
fn too_many_paths(limit: usize, search: &Search, bounded: bool) -> DataFusionError {
    const RAISE: &str = "or SET ursa.max_paths to a higher limit (0 for none)";
    const HOP: &str = "count the nodes at each distance with hop(…) \
                       (SELECT distance, count(*) FROM hop(g, seeds => […], steps => …) GROUP BY 1)";
    let remedy = match search.selector {
        Selector::All => format!(
            "give the quantified edge a lower upper bound, keep fewer paths with ANY SHORTEST, \
             {RAISE}"
        ),
        Selector::Shortest(1) if bounded => {
            format!("ANY SHORTEST keeps one path per pair of nodes already; {HOP}, {RAISE}")
        }
        Selector::Shortest(k) if bounded => {
            format!("SHORTEST {k} keeps {k} paths per pair of nodes already; {HOP}, {RAISE}")
        }
        _ => format!("give the quantified edge a lower upper bound, {HOP}, {RAISE}"),
    };
    DataFusionError::Execution(format!(
        "in GRAPH_TABLE: the path search reached {limit} paths, the limit ursa.max_paths sets; \
         {remedy}"
    ))
}

/// The paths kept by the starts committed so far, and the prefixes they
/// extended.
#[derive(Default)]
struct Tally {
    kept: usize,
    expanded: usize,
}

impl Tally {
    /// Append one start's paths, the next in start order, after checking
    /// the limit as a search from one start after another would have:
    /// `found` was searched knowing only that the starts before it kept at
    /// least as many paths as they did.
    fn commit(&mut self, plan: &Plan, rows: &mut Rows, found: &Found) -> Result<()> {
        if let (Some(cap), Some(peak)) = (plan.cap.filter(|_| !plan.bounded), found.peak) {
            if peak + self.kept > cap {
                return Err(too_many_paths(cap, plan.search, false));
            }
        }
        rows.append(found)?;
        self.kept += found.kept;
        self.expanded += found.expanded;
        Ok(())
    }
}

/// Whether `search` is pruned: it keeps a bounded number of prefixes per
/// state (see the module docs).
fn is_pruned(search: &Search) -> bool {
    search.mode == Mode::Walk
        || (matches!(
            search.selector,
            Selector::Shortest(1) | Selector::AllShortest
        ) && search.quantifier.min <= 1
            && !(search.mode == Mode::Trail && search.direction == Direction::Any))
}

/// The notice that `ursa.max_paths` (`max_paths`) is below the floor an
/// `ANY SHORTEST` or `SHORTEST k` search keeps to, `k` paths per node of a
/// graph of `n_nodes` (see the module docs), so the search uses the floor
/// instead; `None` when the setting applies as it is. It changes no result.
pub fn floor_notice(search: &Search, n_nodes: usize, max_paths: Option<usize>) -> Option<String> {
    let (Some(cap), Selector::Shortest(k)) = (max_paths, search.selector) else {
        return None;
    };
    let floor = n_nodes.saturating_mul(k as usize);
    if !is_pruned(search) || cap >= floor {
        return None;
    }
    let selector = if k == 1 {
        "ANY SHORTEST".to_string()
    } else {
        format!("SHORTEST {k}")
    };
    let count = if k == 1 {
        "the node count".to_string()
    } else {
        format!("{k} times the node count")
    };
    Some(format!(
        "ursa.max_paths {cap} is below {count}; {selector} keeps up to {floor} paths"
    ))
}

/// Run `search` from each of `starts` (keys of the graph's id type, each
/// once) on the index `topology`/`ids`; the rows of [`schema`], and how many
/// path prefixes the search extended. `edge_row` gives the edge table's row
/// of an edge of the index (see [`super::GraphIndex::edge_row`]). With
/// `targets`, only the paths that end at one are kept, and a search with a
/// selector stops once every target has its paths (see the module docs).
/// With `known_only`, a start that is no node of the index has no path.
/// `max_paths` is `ursa.max_paths` (`None` for no limit; see the module
/// docs).
///
/// Many start nodes are searched in parallel, each by a walker of its own,
/// and their rows appended in start order: the rows, their order and when
/// the limit stops the search are those of one start after another.
#[allow(clippy::too_many_arguments)]
pub fn run(
    topology: &Topology,
    ids: &IdMap,
    edge_row: &(dyn Fn(u32) -> u64 + Sync),
    search: &Search,
    starts: &ArrayRef,
    targets: Option<&ArrayRef>,
    known_only: bool,
    columns: PathColumns,
    schema: SchemaRef,
    max_paths: Option<usize>,
) -> Result<(Vec<RecordBatch>, usize)> {
    let dense = ids
        .dense_from_array(starts.as_ref())
        .map_err(|e| DataFusionError::Execution(format!("in GRAPH_TABLE: {e}")))?;
    let goals = targets.map(|t| Goals::new(ids, t)).transpose()?;
    let mut rows = Rows::new(ids, starts, columns, schema);
    let Quantifier { min, .. } = search.quantifier;
    let restricted = search.mode != Mode::Walk;
    let pruned = is_pruned(search);
    // At most `k` prefixes per state: memory bounded by the nodes, and at
    // most `k` paths per node from one start.
    let bounded = pruned && matches!(search.selector, Selector::Shortest(_));
    let kept_limit = match (bounded, max_paths, search.selector) {
        (true, Some(cap), Selector::Shortest(k)) => {
            Some(cap.max(topology.n_nodes().saturating_mul(k as usize)))
        }
        _ => None,
    };
    if search.direction != Direction::Right && dense.iter().any(Option::is_some) {
        // Built here, outside the walkers, if it is not yet.
        topology.incoming();
    }
    let n_nodes = topology.n_nodes();
    let width = min as usize + 1;
    let plan = Plan {
        topology,
        edge_row,
        search,
        starts,
        dense: &dense,
        goals: goals.as_ref(),
        known_only,
        columns,
        restricted,
        pruned,
        width,
        bounded,
        cap: max_paths,
        kept_limit,
        kept_all: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
    };
    let states = n_nodes * width;
    let table = Table::choose(pruned, search.selector, n_nodes, width);
    let walkers = walkers(&plan, table.bytes(states));
    let mut tally = Tally::default();
    match table {
        Table::None => drive(&plan, &mut rows, &mut tally, walkers, || NoStates)?,
        Table::Bits => drive(&plan, &mut rows, &mut tally, walkers, || Bits {
            words: vec![0; states.div_ceil(64)],
            touched: Vec::new(),
        })?,
        Table::Pairs => drive(&plan, &mut rows, &mut tally, walkers, || Pairs {
            seen: vec![(0, 0); states],
            touched: Vec::new(),
        })?,
        Table::Sparse => drive(&plan, &mut rows, &mut tally, walkers, || {
            Sparse(FxHashMap::default())
        })?,
    }
    rows.flush()?;
    Ok((rows.batches, tally.expanded))
}

/// How many walkers search from the starts at once: one per thread, one per
/// start at most, and as many as their state tables fit in
/// [`PARALLEL_STATES_BYTES`] (one for a sparse table, whose size is not
/// known).
fn walkers(plan: &Plan, table_bytes: Option<usize>) -> usize {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (plan, table_bytes);
        1
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let starts = plan.dense.iter().filter(|d| d.is_some()).count();
        let fit = match table_bytes {
            Some(0) => usize::MAX,
            Some(b) => (PARALLEL_STATES_BYTES / b).max(1),
            None => 1,
        };
        rayon::current_num_threads().min(starts).min(fit).max(1)
    }
}

/// Search from every start, with `walkers` walkers, each with the state
/// table `states` makes, and append the paths to `rows` in start order.
fn drive<T: States>(
    plan: &Plan,
    rows: &mut Rows,
    tally: &mut Tally,
    walkers: usize,
    states: impl Fn() -> T + Sync,
) -> Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    if walkers > 1 {
        return drive_parallel(plan, rows, tally, walkers, states);
    }
    let _ = walkers;
    let mut walker = Walker::new(plan, states());
    for row in 0..plan.dense.len() {
        let found = walker.search(row, tally.kept)?;
        tally.commit(plan, rows, &found)?;
        walker.found = found; // its buffers, for the next start
    }
    Ok(())
}

/// [`drive`] on rayon's threads. Each walker takes the next start, searches
/// it, and leaves its paths for the start-order commit; whoever finishes the
/// start next in order commits it and any after it that are done. A walker
/// runs at most `4 × walkers` starts ahead of the commit, so a slow start
/// holds back only that many finished ones.
///
/// A walker searches a start knowing the paths kept by the starts committed
/// when it took it, at most as many as the starts before it kept. So when
/// it reaches the limit, one start after another would have reached it too,
/// by then or before, with the same error; and the commit checks each start
/// against the exact count before it, as one after another would.
#[cfg(not(target_arch = "wasm32"))]
fn drive_parallel<T: States>(
    plan: &Plan,
    rows: &mut Rows,
    tally: &mut Tally,
    walkers: usize,
    states: impl Fn() -> T + Sync,
) -> Result<()> {
    use std::collections::BTreeMap;
    use std::sync::{Condvar, Mutex, PoisonError};

    struct Shared<'r, 'a> {
        rows: &'r mut Rows<'a>,
        tally: &'r mut Tally,
        /// The next start to take, and the next to commit.
        next: usize,
        commit: usize,
        done: BTreeMap<usize, Found>,
        error: Option<DataFusionError>,
    }
    let n = plan.dense.len();
    let window = 4 * walkers;
    let shared = Mutex::new(Shared {
        rows,
        tally,
        next: 0,
        commit: 0,
        done: BTreeMap::new(),
        error: None,
    });
    let moved = Condvar::new();
    let lock = || shared.lock().unwrap_or_else(PoisonError::into_inner);
    rayon::scope(|scope| {
        for _ in 0..walkers {
            scope.spawn(|_| {
                let mut walker = Walker::new(plan, states());
                loop {
                    let (row, before) = {
                        let mut s = lock();
                        loop {
                            if s.error.is_some() || s.next == n {
                                return;
                            }
                            if s.next < s.commit + window {
                                break;
                            }
                            s = moved.wait(s).unwrap_or_else(PoisonError::into_inner);
                        }
                        s.next += 1;
                        (s.next - 1, s.tally.kept)
                    };
                    let found = walker.search(row, before);
                    let mut s = lock();
                    match found {
                        Err(e) => {
                            if s.error.is_none() && !plan.stop.load(Ordering::Relaxed) {
                                s.error = Some(e);
                            }
                            plan.stop.store(true, Ordering::Relaxed);
                        }
                        Ok(found) => {
                            s.done.insert(row, found);
                            let s = &mut *s;
                            while s.error.is_none() {
                                let Some(found) = s.done.remove(&s.commit) else {
                                    break;
                                };
                                match s.tally.commit(plan, s.rows, &found) {
                                    Ok(()) => s.commit += 1,
                                    Err(e) => {
                                        s.error = Some(e);
                                        plan.stop.store(true, Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                    }
                    drop(s);
                    moved.notify_all();
                }
            });
        }
    });
    let shared = shared.into_inner().unwrap_or_else(PoisonError::into_inner);
    match shared.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
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
            Some(MAX_PATHS),
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

    /// FNV-1a, 64 bits: a hash that is the same on every platform and run.
    struct Fnv(u64);

    impl Fnv {
        fn new() -> Self {
            Fnv(0xcbf2_9ce4_8422_2325)
        }

        fn bytes(&mut self, b: &[u8]) {
            for &x in b {
                self.0 ^= u64::from(x);
                self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
            }
        }

        fn int(&mut self, v: i64) {
            self.bytes(&v.to_le_bytes());
        }
    }

    /// splitmix64.
    fn mix(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `m` R-MAT edges over `2^scale` node slots (the Graph500 quadrants),
    /// with node keys spread out (`key * 3 + 1`) so they are not the dense
    /// numbers.
    fn rmat(m: usize, scale: u32, seed: u64) -> Vec<(i64, i64)> {
        let mut st = seed;
        let (a, ab, abc) = (0.57, 0.76, 0.95);
        (0..m)
            .map(|_| {
                let (mut s, mut d) = (0i64, 0i64);
                for _ in 0..scale {
                    let r = (mix(&mut st) >> 11) as f64 / (1u64 << 53) as f64;
                    s = s << 1 | i64::from(r >= ab);
                    d = d << 1 | i64::from((r >= a && r < ab) || r >= abc);
                }
                (s * 3 + 1, d * 3 + 1)
            })
            .collect()
    }

    /// `m` uniform edges among `n` nodes, with self-loops and parallel edges.
    fn uniform(m: usize, n: u64, seed: u64) -> Vec<(i64, i64)> {
        let mut st = seed;
        (0..m)
            .map(|_| ((mix(&mut st) % n) as i64, (mix(&mut st) % n) as i64))
            .collect()
    }

    /// One run of `s`, as a hash of every row in order (start, end, length,
    /// vertices, edges, element ids) and how many prefixes it extended, or of
    /// its error.
    fn pinned(
        topology: &Topology,
        ids: &IdMap,
        s: &Search,
        starts: &ArrayRef,
        targets: Option<&ArrayRef>,
        max_paths: Option<usize>,
        hash: &mut Fnv,
    ) -> usize {
        let columns = PathColumns {
            vertices: true,
            edges: true,
            elements: true,
        };
        let schema = Arc::new(super::schema(&DataType::Int64, columns));
        let edge_row = |e: u32| u64::from(e) * 2 + 5;
        let result = run(
            topology, ids, &edge_row, s, starts, targets, false, columns, schema, max_paths,
        );
        match result {
            Err(e) => {
                hash.bytes(b"error");
                hash.bytes(e.to_string().as_bytes());
                0
            }
            Ok((batches, expanded)) => {
                let mut n = 0;
                for b in &batches {
                    for c in 0..3 {
                        let col = b.column(c).as_primitive::<Int64Type>();
                        hash.int(col.len() as i64);
                        col.values().iter().for_each(|&v| hash.int(v));
                    }
                    for c in 3..6 {
                        let list = b.column(c).as_list::<i32>();
                        list.offsets().iter().for_each(|&o| hash.int(i64::from(o)));
                        let values = list.values().as_primitive::<Int64Type>();
                        values.values().iter().for_each(|&v| hash.int(v));
                    }
                    n += b.num_rows();
                }
                hash.int(expanded as i64);
                n
            }
        }
    }

    /// The three state tables keep the same prefixes: the map, used past
    /// the dense tables' memory budget, which no graph in these tests
    /// reaches, and the bits of `ANY SHORTEST`, against the table.
    #[test]
    fn state_tables_agree() {
        let mut st = 5u64;
        for selector in [
            Selector::All,
            Selector::Shortest(1),
            Selector::Shortest(3),
            Selector::AllShortest,
        ] {
            let n = 300;
            let mut pairs = Pairs {
                seen: vec![(0, 0); n],
                touched: Vec::new(),
            };
            let mut sparse = Sparse(FxHashMap::default());
            let mut bits = Bits {
                words: vec![0; n.div_ceil(64)],
                touched: Vec::new(),
            };
            for round in 0..20 {
                pairs.clear();
                sparse.clear();
                bits.clear();
                let first = (mix(&mut st) % n as u64) as usize;
                pairs.start(first);
                sparse.start(first);
                bits.start(first);
                // Rounds that reach a few states, and rounds that reach most.
                let steps = if round % 2 == 0 { 40 } else { 3000 };
                for _ in 0..steps {
                    let i = (mix(&mut st) % n as u64) as usize;
                    let len = 1 + (mix(&mut st) % 4) as u32;
                    let kept = pairs.accept(i, len, selector);
                    assert_eq!(sparse.accept(i, len, selector), kept, "{selector:?}");
                    if selector == Selector::Shortest(1) {
                        assert_eq!(bits.accept(i, len, selector), kept);
                    }
                }
            }
        }
    }

    /// The rows of path searches over a matrix of graphs, directions,
    /// selectors, modes, bounds, start nodes, targets and limits, pinned as
    /// one hash per graph: recorded before the search was made faster
    /// (#451), so a change that moves a row, changes a path or changes when
    /// `ursa.max_paths` refuses a search fails here. The same on one thread
    /// and on four.
    #[test]
    fn rows_and_their_order_are_pinned() {
        let mut chain: Vec<(i64, i64)> = (0..9).map(|i| (i, i + 1)).collect();
        for i in 0..10 {
            chain.extend((0..4).map(|j| (i, 100 + 10 * i + j)));
        }
        chain.extend([(0, 5), (2, 7), (5, 5), (7, 2), (3, 1)]);
        let graphs: Vec<(&str, Vec<(i64, i64)>)> = vec![
            ("know", KNOW.to_vec()),
            ("chain", chain),
            ("uniform", uniform(140, 40, 7)),
            ("rmat", rmat(600, 7, 11)),
        ];
        let quantifiers: &[(u32, Option<u32>)] = &[
            (0, Some(0)),
            (0, Some(2)),
            (1, Some(1)),
            (1, Some(3)),
            (1, None),
            (2, Some(3)),
            (3, None),
        ];
        let expected: &[(&str, u64, usize)] = &[
            ("know", 12579871203238691482, 71739),
            ("chain", 1410182832036601776, 234240),
            ("uniform", 5017031796771702360, 385893),
            ("rmat", 9397029938903403620, 276899),
        ];
        let mut got = Vec::new();
        for threads in [1, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            for (name, edges) in &graphs {
                let (topology, ids) = index(edges);
                let mut keys: Vec<i64> = edges.iter().flat_map(|&(s, d)| [s, d]).collect();
                keys.sort();
                keys.dedup();
                let all: ArrayRef = Arc::new(Int64Array::from(keys.clone()));
                let one: ArrayRef = Arc::new(Int64Array::from(vec![keys[0]]));
                // A few, out of key order, with a null and a key that is no node.
                let some: ArrayRef = Arc::new(Int64Array::from(vec![
                    Some(keys[keys.len() / 2]),
                    None,
                    Some(-77),
                    Some(keys[0]),
                    Some(keys[keys.len() - 1]),
                    Some(keys[1]),
                ]));
                let targets: ArrayRef = Arc::new(Int64Array::from(vec![
                    keys[keys.len() / 3],
                    keys[0],
                    -5,
                    keys[keys.len() - 2],
                ]));
                let mut hash = Fnv::new();
                let mut rows = 0;
                for direction in [Direction::Right, Direction::Left, Direction::Any] {
                    for selector in [
                        Selector::All,
                        Selector::Shortest(1),
                        Selector::Shortest(3),
                        Selector::AllShortest,
                    ] {
                        for mode in [Mode::Walk, Mode::Trail, Mode::Simple, Mode::Acyclic] {
                            for &(min, max) in quantifiers {
                                if selector == Selector::All && mode == Mode::Walk && max.is_none()
                                {
                                    continue;
                                }
                                let s = search(direction, min, max, selector, mode);
                                for starts in [&one, &some, &all] {
                                    for t in [None, Some(&targets)] {
                                        for cap in [Some(300), Some(3_000)] {
                                            hash.bytes(s.to_string().as_bytes());
                                            rows += pool.install(|| {
                                                pinned(
                                                    &topology, &ids, &s, starts, t, cap, &mut hash,
                                                )
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                got.push((*name, hash.0, rows));
            }
        }
        let (one, four) = got.split_at(graphs.len());
        assert_eq!(one, four, "one thread and four differ");
        assert_eq!(one, expected, "the pinned rows changed");
    }
}
