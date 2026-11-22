//! A result as the view draws it, kept in step with its deltas (#441).
//!
//! A scene is a base [`GraphResult`] and the [`GraphDelta`]s applied to it.
//! [`LiveResult`] holds the result and applies a delta to what is drawn from
//! it: the graph, the positions, each node's radius, colour and flags. It
//! gives back the [`Changes`]: which slots the renderer has to write. None of
//! this touches the GPU; [`crate::view::View::apply`] does the writing.
//!
//! The cost of a delta is the delta's, not the graph's:
//!
//! - **Slots are append-only.** New nodes and edges take the slots after the
//!   last, and only those slots are written. Taking them off again (a step
//!   undone) is a shorter range, and writes nothing.
//! - **A removed node keeps its slot**, hidden by its flag
//!   ([`crate::highlight::flag::HIDDEN`]): four bytes. Its edges are left
//!   out by the renderer while any node is hidden.
//! - **A removed edge keeps its slot** as a line from its source to itself,
//!   which draws nothing: eight bytes.
//! - **Styles follow the result's encodings.** A node sized by degree is
//!   written again when an edge changes its degree; a column that sizes or
//!   colours the nodes restyles them when it is set, and only the nodes
//!   whose radius or colour came out different are written.
//!
//! Undo is the inverse delta ([`GraphDelta::invert`]) applied the same way:
//! everything here is a function of the result and the positions, so a delta
//! and its inverse leave what is drawn as it was, bit for bit.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use serde::Serialize;
use ursa_core::algo::Fa2Params;
use ursa_core::topology::Topology;

use crate::highlight::HighlightState;
use crate::model::{GraphDelta, GraphResult, ModelError, Op, Target, Values, NULL, REMOVED};
use crate::scene::{bounds, Graph, LiveLayout, NodeStyle, Theme, NO_CATEGORY};

/// The radii a column maps to ([`NodeStyle::sizes_from`]), in pixels.
pub const SIZE_BY_RADII: (f32, f32) = (3.0, 14.0);

/// How many slots a view of `len` starts with room for, beyond its own: a
/// sixty-fourth more and a few. Its first deltas then move nothing: not
/// what the result holds, nor what is drawn, nor the GPU's buffers
/// (`render::starting_capacity`).
pub fn room(len: usize) -> usize {
    len / 64 + 64
}

/// Changed slots closer than this are written as one run.
const MERGE_GAP: usize = 8;

/// The colour legend lists this many categories; the rest are "other".
const LEGEND_ENTRIES: usize = 12;

/// How many nodes are read to size up the drawing a new node is placed in.
const PLACE_SAMPLE: usize = 4096;

/// The golden angle: where each new node goes around its parent.
const GOLDEN_ANGLE: f32 = 2.399_963_1;

/// What a view draws, borrowed for a delta to change.
pub struct Parts<'a> {
    pub graph: &'a mut Graph,
    pub x: &'a mut Vec<f32>,
    pub y: &'a mut Vec<f32>,
    pub style: &'a mut NodeStyle,
    /// Each node's category, when the result colours by a column.
    pub categories: &'a mut Option<Vec<u32>>,
    pub highlight: &'a mut HighlightState,
    pub theme: &'a Theme,
}

/// What a view draws, owned: what [`LiveResult::new`] makes of a result.
#[derive(Debug)]
pub struct Drawn {
    pub graph: Graph,
    pub x: Vec<f32>,
    pub y: Vec<f32>,
    pub style: NodeStyle,
    pub categories: Option<Vec<u32>>,
    pub highlight: HighlightState,
}

impl Drawn {
    pub fn parts<'a>(&'a mut self, theme: &'a Theme) -> Parts<'a> {
        Parts {
            graph: &mut self.graph,
            x: &mut self.x,
            y: &mut self.y,
            style: &mut self.style,
            categories: &mut self.categories,
            highlight: &mut self.highlight,
            theme,
        }
    }
}

/// What a delta changed in what is drawn: the slots to write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// How many node slots there were, and are.
    pub nodes: (usize, usize),
    /// How many edge slots there were, and are.
    pub edges: (usize, usize),
    /// The node slots from here on are new: write their positions, radii,
    /// colours and flags.
    pub new_nodes_from: usize,
    /// The edge slots from here on are new: write them.
    pub new_edges_from: usize,
    /// Older nodes whose radius or colour changed, as runs.
    pub restyled: Vec<Range<usize>>,
    /// Whether any of their colours did: if not, only radii are written.
    pub recolored: bool,
    /// Older edges that were removed or restored, as runs.
    pub edge_runs: Vec<Range<usize>>,
    /// Every node's position changed (a step's positions were restored).
    pub positions: bool,
    /// The flags or the highlighted edges may have changed.
    pub highlights: bool,
}

impl Changes {
    /// Whether nothing drawn changed.
    pub fn is_empty(&self) -> bool {
        self.nodes.0 == self.nodes.1
            && self.edges.0 == self.edges.1
            && self.new_nodes_from == self.nodes.1
            && self.new_edges_from == self.edges.1
            && self.restyled.is_empty()
            && self.edge_runs.is_empty()
            && !self.positions
            && !self.highlights
    }

    /// The bytes the renderer writes for these changes, apart from the
    /// flags (four bytes for each node whose flags changed) and the
    /// highlight layer's own instances: 16 for each new node, 4 for each
    /// restyled one (8 if any was recoloured), 8 for each edge written, and
    /// 8 a node when every position changed.
    pub fn upload_bytes(&self) -> u64 {
        let len = |runs: &[Range<usize>]| runs.iter().map(|r| r.len() as u64).sum::<u64>();
        let new_nodes = (self.nodes.1 - self.new_nodes_from) as u64;
        let new_edges = (self.edges.1 - self.new_edges_from) as u64;
        // With every position written, a new node's is not written twice.
        let nodes = if self.positions {
            8 * self.nodes.1 as u64 + 8 * new_nodes
        } else {
            16 * new_nodes
        };
        let style = if self.recolored { 8 } else { 4 };
        nodes + style * len(&self.restyled) + 8 * (new_edges + len(&self.edge_runs))
    }
}

/// Each node's undirected degree (distinct neighbours, self-loops left
/// out), kept as edges come and go at the end of the graph, without
/// building its adjacency again.
#[derive(Debug)]
struct Degrees {
    /// The adjacency of the graph as it was when this was made.
    base: Arc<Topology>,
    /// How many of the graph's edges `base` has: the first ones.
    base_edges: usize,
    degree: Vec<u32>,
    /// How many edges after `base`'s join each pair of nodes, `(low, high)`.
    added: HashMap<(u32, u32), u32>,
}

impl Degrees {
    fn new(graph: &Graph) -> Self {
        Self {
            base: Arc::clone(graph.topology()),
            base_edges: graph.n_edges(),
            degree: graph.degrees(),
            added: HashMap::new(),
        }
    }

    /// Whether `base` has an edge between `s` and `d`.
    fn in_base(&self, s: u32, d: u32) -> bool {
        let n = self.base.n_nodes() as u32;
        if s >= n || d >= n {
            return false;
        }
        let adj = self.base.undirected();
        let (a, b) = if adj.degree(s) <= adj.degree(d) {
            (s, d)
        } else {
            (d, s)
        };
        adj.neighbors(a).binary_search(&b).is_ok()
    }

    /// An edge after `base`'s comes (`on`) or goes. Whether its ends'
    /// degrees changed.
    fn edge(&mut self, s: u32, d: u32, on: bool) -> bool {
        if s == d {
            return false;
        }
        let pair = (s.min(d), s.max(d));
        let known = self.in_base(s, d);
        let count = self.added.entry(pair).or_insert(0);
        let changed = if on {
            *count += 1;
            *count == 1 && !known
        } else {
            *count -= 1;
            *count == 0 && !known
        };
        if *count == 0 {
            self.added.remove(&pair);
        }
        if changed {
            for v in [s, d] {
                let degree = &mut self.degree[v as usize];
                *degree = if on { *degree + 1 } else { *degree - 1 };
            }
        }
        changed
    }
}

/// A result, and what it takes to keep a drawing of it in step.
#[derive(Debug)]
pub struct LiveResult {
    result: GraphResult,
    degrees: Degrees,
    /// The range of the column that sizes the nodes, if one does.
    size_range: Option<(f32, f32)>,
    /// How many nodes and edges are removed, kept as deltas remove and
    /// restore them.
    hidden_nodes: usize,
    removed_edges: usize,
    counts: OnceLock<Counts>,
}

/// What [`LiveResult::info`] counts, until the next delta.
#[derive(Debug, Clone)]
struct Counts {
    shown_edges: usize,
    /// Shown nodes per category, most first; then those in no category.
    categories: Vec<(u32, usize)>,
    uncategorized: usize,
}

/// What a view shows, for a status line and a legend.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    /// The nodes and edges shown: not removed, and for an edge, neither end
    /// removed.
    pub nodes: usize,
    pub edges: usize,
    /// Nodes removed (hidden); they keep their slots.
    pub hidden_nodes: usize,
    /// Edges not shown: removed, or at a removed node.
    pub hidden_edges: usize,
    /// Every slot, shown or not: the next node's and the next edge's index.
    pub node_slots: usize,
    pub edge_slots: usize,
    /// What sizes the nodes: a column and its range, or their degree.
    pub size: SizeLegend,
    /// What colours the nodes, if a column does.
    pub color: Option<ColorLegend>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SizeLegend {
    /// The column, or `None` for the nodes' degree.
    pub column: Option<String>,
    /// The column's least and greatest finite values; `None` with no column
    /// or no finite value.
    pub range: Option<(f32, f32)>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColorLegend {
    pub column: String,
    /// The categories with the most shown nodes, most first.
    pub entries: Vec<LegendEntry>,
    /// Shown nodes in the categories not listed.
    pub other: usize,
    /// Shown nodes with no value in the column.
    pub none: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegendEntry {
    pub category: u32,
    /// The category's text, or its number.
    pub label: String,
    /// `#rrggbb`, in the theme asked for.
    pub color: String,
    /// How many shown nodes have it.
    pub count: usize,
}

/// Row `i` of `values` as a category.
fn category(values: &Values, i: usize) -> u32 {
    match values {
        Values::U32(v) => v[i],
        Values::Dict { codes, .. } => codes[i],
        Values::F32(v) if v[i].is_finite() && v[i] >= 0.0 => v[i] as u32,
        Values::F32(_) => NO_CATEGORY,
    }
}

/// Row `i` of `values` as a number to size by; NaN for a null.
fn number(values: &Values, i: usize) -> f32 {
    match values {
        Values::F32(v) => v[i],
        Values::U32(v) if v[i] != NULL => v[i] as f32,
        _ => f32::NAN,
    }
}

/// Sorted indexes as runs, joining those less than [`MERGE_GAP`] apart.
fn runs(sorted: impl IntoIterator<Item = usize>) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for i in sorted {
        match out.last_mut() {
            Some(run) if i < run.end => {}
            Some(run) if i - run.end <= MERGE_GAP => run.end = i + 1,
            _ => out.push(i..i + 1),
        }
    }
    out
}

impl LiveResult {
    /// `result`, and what a view draws of it: its graph, each node where
    /// the result puts it (or where a layout would start it), sized and
    /// coloured as the result's encodings say (by degree, in the theme's
    /// node colour, without), with its highlights and its removed nodes
    /// hidden.
    pub fn new(mut result: GraphResult, theme: &Theme) -> (Self, Drawn) {
        let (n, m) = (result.n_nodes(), result.n_edges());
        let (more_nodes, more_edges) = (room(n), room(m));
        result.reserve(more_nodes, more_edges);
        let mut graph = Graph::new(n, result.src().to_vec(), result.dst().to_vec())
            .expect("a result's edges name its nodes");
        graph.reserve(more_edges);
        let (x, y) = match result.positions() {
            Some(p) => (p.x.clone(), p.y.clone()),
            None => {
                let start = LiveLayout::new(&graph, Fa2Params::default());
                let (x, y) = start.positions();
                (x.to_vec(), y.to_vec())
            }
        };
        let mut highlight = HighlightState::new(n);
        highlight
            .set_result(&result)
            .expect("the highlight has the result's nodes");
        let removed = |flags: &[u8]| flags.iter().filter(|&&f| f & REMOVED != 0).count();
        let mut live = Self {
            degrees: Degrees::new(&graph),
            hidden_nodes: removed(result.node_flags()),
            removed_edges: removed(result.edge_flags()),
            result,
            size_range: None,
            counts: OnceLock::new(),
        };
        live.size_range = live.scan_size_range();
        let mut drawn = Drawn {
            graph,
            x,
            y,
            style: NodeStyle {
                radius: vec![0.0; n],
                color: vec![0; n],
            },
            categories: None,
            highlight,
        };
        drawn.categories = live.color_column().map(|_| vec![NO_CATEGORY; n]);
        for i in 0..n {
            live.style_node(i, &mut drawn.parts(theme));
        }
        live.degrees.degree.reserve(more_nodes);
        drawn.x.reserve(more_nodes);
        drawn.y.reserve(more_nodes);
        drawn.style.radius.reserve(more_nodes);
        drawn.style.color.reserve(more_nodes);
        if let Some(categories) = &mut drawn.categories {
            categories.reserve(more_nodes);
        }
        drawn.highlight.reserve(more_nodes);
        (live, drawn)
    }

    pub fn result(&self) -> &GraphResult {
        &self.result
    }

    fn size_column(&self) -> Option<&Values> {
        let name = self.result.encodings().size_by.as_deref()?;
        self.result.node_column(name).map(|c| &c.values)
    }

    fn color_column(&self) -> Option<&Values> {
        let name = self.result.encodings().color_by.as_deref()?;
        self.result.node_column(name).map(|c| &c.values)
    }

    fn scan_size_range(&self) -> Option<(f32, f32)> {
        let values = self.size_column()?;
        Some(NodeStyle::size_range(
            (0..values.len()).map(|i| number(values, i)),
        ))
    }

    /// Set node `i`'s radius, colour and category from the result. Whether
    /// its radius or colour changed, and whether its colour did.
    fn style_node(&self, i: usize, parts: &mut Parts<'_>) -> (bool, bool) {
        let radius = match (self.size_column(), self.size_range) {
            (Some(values), Some(range)) => {
                NodeStyle::size_from(number(values, i), range, SIZE_BY_RADII.0, SIZE_BY_RADII.1)
            }
            _ => NodeStyle::degree_radius(self.degrees.degree[i]),
        };
        let color = match (self.color_column(), parts.categories.as_mut()) {
            (Some(values), Some(categories)) => {
                categories[i] = category(values, i);
                parts.theme.color(categories[i])
            }
            _ => parts.theme.node,
        };
        let resized = parts.style.radius[i].to_bits() != radius.to_bits();
        let recolored = parts.style.color[i] != color;
        parts.style.radius[i] = radius;
        parts.style.color[i] = color;
        (resized || recolored, recolored)
    }

    /// The edges of `range` as the renderer draws them: `src, dst` each, or
    /// `src, src` for a removed edge, which draws nothing.
    pub fn line_indices(&self, range: Range<usize>) -> Vec<u32> {
        let (src, dst, flags) = (
            self.result.src(),
            self.result.dst(),
            self.result.edge_flags(),
        );
        let mut out = Vec::with_capacity(2 * range.len());
        for e in range {
            out.push(src[e]);
            out.push(if flags[e] & REMOVED != 0 {
                src[e]
            } else {
                dst[e]
            });
        }
        out
    }

    /// Apply `delta` to the result and to what is drawn of it, or, if the
    /// delta does not fit the result, change nothing and say why.
    ///
    /// A new node is drawn where the delta puts it. One the delta gives no
    /// position starts beside the node a new edge joins it to, on a
    /// golden-angle spiral, until a layout places it
    /// ([`LiveLayout::grow`]). `SetPositions` moves every node: that is how
    /// an undo restores the positions of its step.
    pub fn apply(
        &mut self,
        delta: &GraphDelta,
        parts: &mut Parts<'_>,
    ) -> Result<Changes, ModelError> {
        let (n0, m0) = (self.result.n_nodes(), self.result.n_edges());
        debug_assert_eq!(parts.x.len(), n0);
        self.result.apply(delta)?;
        self.counts = OnceLock::new();

        // Walk the ops as the result applied them. `n` and `m` follow the
        // slot counts; the least they reach is where the new slots start.
        let (mut n, mut m) = (n0, m0);
        let (mut low_n, mut low_m) = (n0, m0);
        let mut changes = Changes::default();
        let mut restyle_all = false;
        let mut rebuild_degrees = false;
        // Older nodes whose degree changed, and older edges whose flag did.
        let mut touched: Vec<usize> = Vec::new();
        let mut flagged: Vec<usize> = Vec::new();
        let mut unplaced = false;
        for op in &delta.ops {
            match op {
                Op::AddNodes(batch) => {
                    n += batch.len();
                    match &batch.positions {
                        Some(p) => {
                            parts.x.extend_from_slice(&p.x);
                            parts.y.extend_from_slice(&p.y);
                        }
                        None => {
                            parts.x.resize(n, f32::NAN);
                            parts.y.resize(n, f32::NAN);
                            unplaced = true;
                        }
                    }
                    self.degrees.degree.resize(n, 0);
                }
                Op::PopNodes(batch) => {
                    n -= batch.len();
                    low_n = low_n.min(n);
                    parts.x.truncate(n);
                    parts.y.truncate(n);
                    self.degrees.degree.truncate(n);
                }
                Op::AddEdges(batch) => {
                    m += batch.len();
                    for (&s, &d) in batch.src.iter().zip(&batch.dst) {
                        if self.degrees.edge(s, d, true) {
                            touched.extend([s as usize, d as usize]);
                        }
                    }
                }
                Op::PopEdges(batch) => {
                    m -= batch.len();
                    low_m = low_m.min(m);
                    if m < self.degrees.base_edges {
                        // The adjacency the degrees are counted from has
                        // these edges: count again, from the graph.
                        rebuild_degrees = true;
                    }
                    if !rebuild_degrees {
                        for (&s, &d) in batch.src.iter().zip(&batch.dst) {
                            if self.degrees.edge(s, d, false) {
                                touched.extend([s as usize, d as usize]);
                            }
                        }
                    }
                }
                Op::RemoveNodes(nodes) => {
                    self.hidden_nodes += nodes.len();
                    changes.highlights = true;
                }
                Op::RestoreNodes(nodes) => {
                    self.hidden_nodes -= nodes.len();
                    changes.highlights = true;
                }
                Op::SetHighlights { .. } => changes.highlights = true,
                Op::RemoveEdges(edges) | Op::RestoreEdges(edges) => {
                    if matches!(op, Op::RemoveEdges(_)) {
                        self.removed_edges += edges.len();
                    } else {
                        self.removed_edges -= edges.len();
                    }
                    flagged.extend(edges.iter().map(|&e| e as usize));
                    // A removed edge of a path is not highlighted.
                    changes.highlights |= !self.result.highlights().paths.is_empty();
                }
                Op::SetColumn {
                    target, old, new, ..
                } => {
                    let name = old.as_ref().or(new.as_ref()).map(|c| c.name.as_str());
                    let encodings = self.result.encodings();
                    restyle_all |= *target == Target::Nodes
                        && (name == encodings.size_by.as_deref()
                            || name == encodings.color_by.as_deref());
                }
                Op::SetEncodings { .. } => restyle_all = true,
                Op::SetPositions { new, .. } => {
                    if let Some(p) = new {
                        parts.x.clone_from(&p.x);
                        parts.y.clone_from(&p.y);
                        changes.positions = true;
                    }
                }
                Op::AddNodeLabel(_)
                | Op::PopNodeLabel(_)
                | Op::AddEdgeLabel(_)
                | Op::PopEdgeLabel(_) => {}
            }
        }
        let (n1, m1) = (self.result.n_nodes(), self.result.n_edges());
        debug_assert_eq!((n, m), (n1, m1));
        changes.nodes = (n0, n1);
        changes.edges = (m0, m1);
        changes.new_nodes_from = low_n;
        changes.new_edges_from = low_m;

        // The graph: the slots that stayed, then the new ones.
        parts.graph.truncate_checked(low_n, low_m);
        let (src, dst) = (self.result.src(), self.result.dst());
        parts
            .graph
            .append(n1 - low_n, &src[low_m..], &dst[low_m..])
            .expect("a result's edges name its nodes");
        if rebuild_degrees {
            self.degrees = Degrees::new(parts.graph);
            restyle_all = true;
        }
        if unplaced {
            self.place(low_n, low_m, parts);
        }

        // The flags: new nodes unflagged (a slot that was taken off and
        // filled again too), and the result's highlights and removed nodes
        // if they changed, or if a flagged node went.
        changes.highlights |= parts.highlight.resize(low_n);
        parts.highlight.resize(n1);
        if changes.highlights {
            parts
                .highlight
                .set_result(&self.result)
                .expect("the highlight has the result's nodes");
        }

        // The styles. A column's range decides every radius it gives.
        let range = match (self.size_range, n1 >= n0 && low_n == n0 && !restyle_all) {
            // Nodes were only added: their values widen the range, or not.
            (Some(range), true) => self.size_column().map(|values| {
                let new = NodeStyle::size_range((n0..n1).map(|i| number(values, i)));
                (range.0.min(new.0), range.1.max(new.1))
            }),
            _ => self.scan_size_range(),
        };
        let same = |a: Option<(f32, f32)>, b: Option<(f32, f32)>| match (a, b) {
            (Some(a), Some(b)) => a.0.to_bits() == b.0.to_bits() && a.1.to_bits() == b.1.to_bits(),
            (None, None) => true,
            _ => false,
        };
        restyle_all |= !same(range, self.size_range);
        self.size_range = range;
        parts.style.radius.resize(n1, 0.0);
        parts.style.color.resize(n1, 0);
        match (self.color_column().is_some(), parts.categories.as_mut()) {
            (true, Some(categories)) => categories.resize(n1, NO_CATEGORY),
            (true, None) => *parts.categories = Some(vec![NO_CATEGORY; n1]),
            (false, _) => *parts.categories = None,
        }
        for i in low_n..n1 {
            self.style_node(i, parts);
        }
        let again: Vec<usize> = if restyle_all {
            (0..low_n).collect()
        } else if self.size_range.is_none() {
            // Sized by degree: the ends of the edges that changed one.
            touched.retain(|&i| i < low_n);
            touched.sort_unstable();
            touched.dedup();
            touched
        } else {
            Vec::new()
        };
        let mut restyled = Vec::new();
        for i in again {
            let (changed, recolored) = self.style_node(i, parts);
            if changed {
                restyled.push(i);
            }
            changes.recolored |= recolored;
        }
        changes.restyled = runs(restyled);

        flagged.retain(|&e| e < low_m);
        flagged.sort_unstable();
        flagged.dedup();
        changes.edge_runs = runs(flagged);
        Ok(changes)
    }

    /// Give the new nodes without a position one: beside the placed node a
    /// new edge joins each to, the `k`-th around a node `k` golden angles
    /// round and a little farther out; or, with no such edge, around the
    /// middle of the drawing.
    fn place(&self, new_nodes_from: usize, new_edges_from: usize, parts: &mut Parts<'_>) {
        let (x, y) = (&mut *parts.x, &mut *parts.y);
        let n = x.len();
        // The drawing's extent, without the nodes to place: of a few
        // thousand of its nodes, evenly taken, so that it costs the same
        // for any graph.
        let stride = new_nodes_from.div_ceil(PLACE_SAMPLE).max(1);
        let sample = |v: &[f32]| -> Vec<f32> {
            v[..new_nodes_from]
                .iter()
                .step_by(stride)
                .copied()
                .collect()
        };
        let placed = bounds(&sample(x), &sample(y));
        let (centre, step) = match placed {
            Some((x0, y0, x1, y1)) => {
                let area = (x1 - x0).max(1e-3) * (y1 - y0).max(1e-3);
                (
                    ((x0 + x1) / 2.0, (y0 + y1) / 2.0),
                    0.5 * (area / new_nodes_from as f32).sqrt(),
                )
            }
            None => ((0.0, 0.0), 1.0),
        };
        let mut children: HashMap<usize, u32> = HashMap::new();
        let mut beside = |x: &mut [f32], y: &mut [f32], i: usize, parent: Option<usize>| {
            let (px, py) = parent.map_or(centre, |p| (x[p], y[p]));
            let k = children.entry(parent.unwrap_or(usize::MAX)).or_insert(0);
            let (angle, r) = (*k as f32 * GOLDEN_ANGLE, step * ((*k + 1) as f32).sqrt());
            *k += 1;
            x[i] = px + r * angle.cos();
            y[i] = py + r * angle.sin();
        };
        let (src, dst) = (
            &self.result.src()[new_edges_from..],
            &self.result.dst()[new_edges_from..],
        );
        // A node placed in one pass can be the parent in the next: a chain
        // of new nodes takes a pass a link.
        loop {
            let mut placed_any = false;
            for (&s, &d) in src.iter().zip(dst) {
                let (s, d) = (s as usize, d as usize);
                let todo = |i: usize| i >= new_nodes_from && x[i].is_nan();
                match (todo(s), todo(d)) {
                    (true, false) => beside(x, y, s, Some(d)),
                    (false, true) => beside(x, y, d, Some(s)),
                    _ => continue,
                }
                placed_any = true;
            }
            if !placed_any {
                break;
            }
        }
        for i in new_nodes_from..n {
            if x[i].is_nan() {
                beside(x, y, i, None);
            }
        }
    }

    fn counts(&self) -> &Counts {
        self.counts.get_or_init(|| {
            let result = &self.result;
            // An edge is shown when neither it nor either end is removed:
            // with no node removed, there is nothing to read.
            let shown_edges = if self.hidden_nodes == 0 {
                result.n_edges() - self.removed_edges
            } else {
                (0..result.n_edges())
                    .filter(|&e| result.edge_is_shown(e))
                    .count()
            };
            let mut by_category: HashMap<u32, usize> = HashMap::new();
            let mut uncategorized = 0;
            if let Some(values) = self.color_column() {
                // A text column's categories are its codes: counted in place.
                let mut by_code = match values {
                    Values::Dict { values, .. } => vec![0usize; values.len()],
                    _ => Vec::new(),
                };
                let flags = result.node_flags();
                for (i, &flag) in flags.iter().enumerate() {
                    if flag & REMOVED != 0 {
                        continue;
                    }
                    match category(values, i) {
                        NO_CATEGORY => uncategorized += 1,
                        c if (c as usize) < by_code.len() => by_code[c as usize] += 1,
                        c => *by_category.entry(c).or_insert(0) += 1,
                    }
                }
                let counted = by_code.iter().enumerate().filter(|c| *c.1 > 0);
                by_category.extend(counted.map(|(c, &count)| (c as u32, count)));
            }
            let mut categories: Vec<(u32, usize)> = by_category.into_iter().collect();
            categories.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            Counts {
                shown_edges,
                categories,
                uncategorized,
            }
        })
    }

    /// What is shown now, for a status line and a legend, with the legend's
    /// colours as `theme` draws them.
    ///
    /// The counts of nodes cost nothing. The shown edges are counted, in
    /// time proportional to the edges, only while a node is removed (its
    /// edges are not shown); the legend's categories, in time proportional
    /// to the nodes, only when a column colours them. Both on the first
    /// call after a delta, and kept until the next.
    pub fn info(&self, theme: &Theme) -> Info {
        let counts = self.counts();
        let result = &self.result;
        let encodings = result.encodings();
        let color = self.color_column().map(|values| {
            let label = |c: u32| match values {
                Values::Dict { values, .. } => values.get(c as usize).to_string(),
                _ => c.to_string(),
            };
            let listed = counts.categories.len().min(LEGEND_ENTRIES);
            ColorLegend {
                column: encodings.color_by.clone().unwrap_or_default(),
                entries: counts.categories[..listed]
                    .iter()
                    .map(|&(c, count)| LegendEntry {
                        category: c,
                        label: label(c),
                        color: format!("#{:06x}", theme.color(c) >> 8),
                        count,
                    })
                    .collect(),
                other: counts.categories[listed..].iter().map(|c| c.1).sum(),
                none: counts.uncategorized,
            }
        });
        Info {
            nodes: result.n_nodes() - self.hidden_nodes,
            edges: counts.shown_edges,
            hidden_nodes: self.hidden_nodes,
            hidden_edges: result.n_edges() - counts.shown_edges,
            node_slots: result.n_nodes(),
            edge_slots: result.n_edges(),
            size: SizeLegend {
                column: encodings.size_by.clone(),
                range: self.size_range.filter(|r| r.0 <= r.1),
            },
            color,
        }
    }
}

#[cfg(test)]
mod tests;
