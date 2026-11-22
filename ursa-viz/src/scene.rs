//! What the view shows, apart from how it is drawn: the graph, its live
//! layout, each node's size and colour, and the camera.
//!
//! None of this touches the GPU, so it is tested natively and is the same on
//! every platform. [`crate::render`] draws it.

use std::ops::Range;
use std::sync::{Arc, OnceLock};

#[cfg(test)]
use ursa_core::algo::fa2_seed_positions;
use ursa_core::algo::{
    fa2_grow, layout_layers, layout_rings, Fa2Multilevel, Fa2Params, Fa2State, LayersParams,
    MultilevelParams, RingsParams, WarmParams,
};
use ursa_core::topology::UndirectedCsr;

/// A graph to show: `n_nodes` nodes, and one edge per `(src[i], dst[i])`, as
/// dense node indexes below `n_nodes`.
///
/// Dense indexes keep the view independent of where the graph came from: the
/// SQL shell maps its result's `src`/`dst` ids to them, and the DataFrame API
/// already has them in its topology.
///
/// A graph is two things. What a frame needs is the node count and the
/// edge list, which is all [`Graph::new`] keeps: making one reads the edges
/// once, to check them. What a layout needs is the adjacency
/// ([`Graph::adjacency`]), which costs as much again as the edge list, and
/// a sort, so it is built when first asked for and not before. A view
/// whose layout runs on another thread never asks.
///
/// A graph grows and shrinks at its end ([`Graph::append`],
/// [`Graph::truncate`]) in time proportional to the change: its adjacency
/// is built again when next asked for.
#[derive(Debug)]
pub struct Graph {
    n_nodes: usize,
    /// Built on demand ([`Graph::adjacency`]), and again after a change.
    adjacency: OnceLock<Arc<UndirectedCsr>>,
    src: Vec<u32>,
    dst: Vec<u32>,
}

/// An edge that names a node index at or past the node count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadEdge {
    pub edge: usize,
    pub node: u32,
    pub n_nodes: usize,
}

impl std::fmt::Display for BadEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "edge {} names node {}, but the graph has {} nodes",
            self.edge, self.node, self.n_nodes
        )
    }
}

impl std::error::Error for BadEdge {}

impl Graph {
    pub fn new(n_nodes: usize, src: Vec<u32>, dst: Vec<u32>) -> Result<Self, BadEdge> {
        assert_eq!(
            src.len(),
            dst.len(),
            "src and dst must have one entry per edge"
        );
        check_edges(n_nodes, 0, &src, &dst)?;
        Ok(Self {
            n_nodes,
            adjacency: OnceLock::new(),
            src,
            dst,
        })
    }

    /// `new_nodes` more nodes, after the ones there are, and these edges
    /// after the ones there are. No index changes. On an error nothing does.
    pub fn append(&mut self, new_nodes: usize, src: &[u32], dst: &[u32]) -> Result<(), BadEdge> {
        assert_eq!(
            src.len(),
            dst.len(),
            "src and dst must have one entry per edge"
        );
        check_edges(self.n_nodes + new_nodes, self.src.len(), src, dst)?;
        if new_nodes > 0 || !src.is_empty() {
            self.n_nodes += new_nodes;
            self.src.extend_from_slice(src);
            self.dst.extend_from_slice(dst);
            self.adjacency = OnceLock::new();
        }
        Ok(())
    }

    /// Make room for `edges` more edges, so that appending up to as many
    /// moves none.
    pub fn reserve(&mut self, edges: usize) {
        self.src.reserve(edges);
        self.dst.reserve(edges);
    }

    /// Keep the first `n_nodes` nodes and the first `n_edges` edges, which
    /// must name only those nodes: what undoes an [`append`](Self::append).
    pub fn truncate(&mut self, n_nodes: usize, n_edges: usize) -> Result<(), BadEdge> {
        let (n_nodes, n_edges) = (n_nodes.min(self.n_nodes), n_edges.min(self.src.len()));
        if n_nodes < self.n_nodes {
            check_edges(n_nodes, 0, &self.src[..n_edges], &self.dst[..n_edges])?;
        }
        self.truncate_checked(n_nodes, n_edges);
        Ok(())
    }

    /// [`Graph::truncate`] for a caller that knows the edges that stay name
    /// only nodes that stay (a [`crate::model::GraphResult`] has checked):
    /// it does not read them.
    pub(crate) fn truncate_checked(&mut self, n_nodes: usize, n_edges: usize) {
        if n_nodes < self.n_nodes || n_edges < self.src.len() {
            self.n_nodes = n_nodes;
            self.src.truncate(n_edges);
            self.dst.truncate(n_edges);
            self.adjacency = OnceLock::new();
        }
    }

    pub fn n_nodes(&self) -> usize {
        self.n_nodes
    }

    pub fn src(&self) -> &[u32] {
        &self.src
    }

    pub fn dst(&self) -> &[u32] {
        &self.dst
    }

    pub fn n_edges(&self) -> usize {
        self.src.len()
    }

    /// The edges of `range` as one index list, `src, dst` per edge: a line
    /// list over the node positions.
    pub fn line_indices(&self, range: Range<usize>) -> Vec<u32> {
        let mut out = Vec::with_capacity(range.len() * 2);
        for (&s, &d) in self.src[range.clone()].iter().zip(&self.dst[range]) {
            out.push(s);
            out.push(d);
        }
        out
    }

    /// Each node's undirected degree (distinct neighbours, self-loops left
    /// out): the degree the adjacency gives, counted from the edge list
    /// without building it.
    pub fn degrees(&self) -> Vec<u32> {
        match self.adjacency.get() {
            Some(adj) => (0..self.n_nodes() as u32)
                .map(|u| adj.degree(u) as u32)
                .collect(),
            None => Pairs::new(self.n_nodes, &self.src, &self.dst).1,
        }
    }

    /// Whether the adjacency is built: a view that only draws never builds
    /// it.
    pub fn has_adjacency(&self) -> bool {
        self.adjacency.get().is_some()
    }

    /// The graph's undirected adjacency, for a layout: built when first
    /// asked for, and again after the graph changes ([`Graph::append`],
    /// [`Graph::truncate`]), when it is next asked for. It is built from
    /// the edge list alone ([`UndirectedCsr::from_edges`]), with no
    /// `Topology` beside it: a layout reads nothing else.
    pub fn adjacency(&self) -> &Arc<UndirectedCsr> {
        self.adjacency.get_or_init(|| {
            Arc::new(UndirectedCsr::from_edges(
                self.n_nodes,
                &self.src,
                &self.dst,
            ))
        })
    }
}

/// The pairs of nodes a graph's edges join, each pair once, however many
/// edges join it and whichever way they point: enough to count each node's
/// distinct neighbours and to ask whether two nodes are joined, in at most
/// one `u32` per edge and one per node. It is not an adjacency: a pair is
/// kept at its lower-indexed end only, so a node's list is not its
/// neighbours, and it costs three passes over the edge list to make, with
/// no sort and no copy of the edges. A page makes it a pass at a time
/// ([`Pairs::count`]).
#[derive(Debug)]
pub(crate) struct Pairs {
    /// Node `u`'s list, the higher-indexed nodes it is joined to, is
    /// `other[offsets[u]..offsets[u + 1]]`.
    offsets: Vec<u32>,
    other: Vec<u32>,
    /// The lists sorted so far ([`Pairs::joined`]); empty until one is.
    sorted: Vec<bool>,
}

impl Pairs {
    /// A list this short is read through; a longer one is sorted the first
    /// time it is asked about, and searched.
    const SHORT: usize = 32;

    /// The pairs of the graph of `n_nodes` nodes and the edges
    /// `(src[i], dst[i])`, which name only those nodes, and each node's
    /// distinct neighbours (self-loops left out).
    pub(crate) fn new(n_nodes: usize, src: &[u32], dst: &[u32]) -> (Self, Vec<u32>) {
        let mut counted = Self::count(n_nodes, src, dst);
        counted.fill(src, dst);
        counted.finish()
    }

    /// The first of the three passes [`Pairs::new`] makes, for a caller
    /// that gives its thread up between them: then [`Counted::fill`] and
    /// [`Counted::finish`], with the same edges.
    pub(crate) fn count(n_nodes: usize, src: &[u32], dst: &[u32]) -> Counted {
        // The lists' sizes, then their starts; filling them moves each
        // start to its list's end, which is the next list's start.
        let mut offsets = vec![0u32; n_nodes + 1];
        let mut pairs = 0usize;
        for (&s, &d) in src.iter().zip(dst) {
            if s != d {
                offsets[s.min(d) as usize + 1] += 1;
                pairs += 1;
            }
        }
        let mut total = 0;
        for slot in &mut offsets[1..] {
            let len = *slot;
            *slot = total;
            total += len;
        }
        Counted {
            offsets,
            other: vec![0u32; pairs],
        }
    }

    /// Give back the room the repeated pairs took: for pairs that are kept.
    pub(crate) fn shrink(&mut self) {
        self.other.shrink_to_fit();
    }

    /// Whether an edge joins `s` and `d`, two different nodes. A long list
    /// is sorted the first time it is asked about, once.
    pub(crate) fn joined(&mut self, s: u32, d: u32) -> bool {
        let n = self.offsets.len() - 1;
        let (lo, hi) = (s.min(d) as usize, s.max(d));
        if hi as usize >= n || s == d {
            return false;
        }
        let list = &mut self.other[self.offsets[lo] as usize..self.offsets[lo + 1] as usize];
        if list.len() <= Self::SHORT {
            return list.contains(&hi);
        }
        if self.sorted.is_empty() {
            self.sorted = vec![false; n];
        }
        if !self.sorted[lo] {
            list.sort_unstable();
            self.sorted[lo] = true;
        }
        list.binary_search(&hi).is_ok()
    }
}

/// [`Pairs`] being made: the lists sized ([`Pairs::count`]).
#[derive(Debug)]
pub(crate) struct Counted {
    offsets: Vec<u32>,
    other: Vec<u32>,
}

impl Counted {
    /// The second pass: every edge into its list.
    pub(crate) fn fill(&mut self, src: &[u32], dst: &[u32]) {
        let (offsets, other) = (&mut self.offsets, &mut self.other);
        for (&s, &d) in src.iter().zip(dst) {
            if s != d {
                let next = &mut offsets[s.min(d) as usize + 1];
                other[*next as usize] = s.max(d);
                *next += 1;
            }
        }
    }

    /// The third pass: the pairs, and each node's distinct neighbours.
    pub(crate) fn finish(self) -> (Pairs, Vec<u32>) {
        let Self {
            mut offsets,
            mut other,
        } = self;
        let n = offsets.len() - 1;
        // A pair is in one list only: keep and count it the first time it
        // is seen there, and drop its repeats. `seen[v]` is the last list
        // (plus one) that named `v`.
        let mut degrees = vec![0u32; n];
        let mut seen = vec![0u32; n];
        let (mut kept, mut start) = (0usize, 0usize);
        for u in 0..n {
            let end = offsets[u + 1] as usize;
            offsets[u] = kept as u32;
            let mark = u as u32 + 1;
            let mut degree = 0;
            for i in start..end {
                let v = other[i] as usize;
                if seen[v] != mark {
                    seen[v] = mark;
                    degree += 1;
                    degrees[v] += 1;
                    other[kept] = v as u32;
                    kept += 1;
                }
            }
            degrees[u] += degree;
            start = end;
        }
        offsets[n] = kept as u32;
        other.truncate(kept);
        let pairs = Pairs {
            offsets,
            other,
            sorted: Vec::new(),
        };
        (pairs, degrees)
    }
}

/// Every edge names a node below `n_nodes`; `first` is the first edge's index.
fn check_edges(n_nodes: usize, first: usize, src: &[u32], dst: &[u32]) -> Result<(), BadEdge> {
    for (i, (&s, &d)) in src.iter().zip(dst).enumerate() {
        for node in [s, d] {
            if node as usize >= n_nodes {
                return Err(BadEdge {
                    edge: first + i,
                    node,
                    n_nodes,
                });
            }
        }
    }
    Ok(())
}

/// ForceAtlas2, run a few iterations at a time so the view can draw it as it
/// converges: flat ([`LiveLayout::new`]), whose positions are `Fa2State`'s, so
/// a layout stepped here is the same as `layout_fa2` with as many iterations;
/// or coarse to fine ([`LiveLayout::multilevel`]), `Fa2Multilevel`'s, the same
/// as `layout_fa2_multilevel` once done.
///
/// A result whose nodes have a level needs no iterations at all:
/// [`LiveLayout::rings`] (a neighbourhood, by distance from its seeds) and
/// [`LiveLayout::layers`] (paths, by position along them) are **fixed**
/// layouts, done as soon as they are made.
///
/// Any of them can then **grow** ([`LiveLayout::grow`]): the graph gains
/// nodes and edges, the new nodes start beside the ones they came from, and
/// a short warm schedule settles them while what was drawn is held.
#[derive(Debug)]
pub struct LiveLayout {
    /// The graph's undirected adjacency: all a layout reads of it.
    adjacency: Arc<UndirectedCsr>,
    engine: Engine,
}

#[derive(Debug)]
enum Engine {
    Flat(Fa2State),
    // Boxed: one per view, and twice the size of the flat state.
    Multilevel(Box<Fa2Multilevel>),
    /// A layout that has grown, or was given its positions: flat
    /// ForceAtlas2 with what was drawn held, for `total` iterations.
    Warm {
        state: Fa2State,
        run: u32,
        total: u32,
    },
    /// Rings or layers: positions that were right when they were made, with
    /// nothing to run.
    Fixed {
        kind: LayoutKind,
        x: Vec<f32>,
        y: Vec<f32>,
    },
}

/// Which layout a [`LiveLayout`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    /// ForceAtlas2: flat, coarse to fine, or warm after growing.
    Force,
    /// Concentric rings by distance from the seeds ([`LiveLayout::rings`]).
    Rings,
    /// Layers left to right ([`LiveLayout::layers`]).
    Layers,
}

impl LayoutKind {
    /// `"force"`, `"rings"` or `"layers"`: the names `.graph … LAYOUT` takes.
    pub fn name(self) -> &'static str {
        match self {
            LayoutKind::Force => "force",
            LayoutKind::Rings => "rings",
            LayoutKind::Layers => "layers",
        }
    }
}

/// Levels that are not one per node of the graph ([`LiveLayout::rings`],
/// [`LiveLayout::layers`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadLevels {
    pub n_nodes: usize,
    pub got: usize,
}

impl std::fmt::Display for BadLevels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "expected one level per node ({}); got {}",
            self.n_nodes, self.got
        )
    }
}

impl std::error::Error for BadLevels {}

/// What a layout gains in [`LiveLayout::grow`], besides the grown graph.
#[derive(Debug, Clone, Copy, Default)]
pub struct Growth<'a> {
    /// The node each new node came from (the expanded one), one entry per
    /// new node in index order; `u32::MAX`, or no list at all, for "its
    /// lowest-indexed neighbour already drawn".
    pub parents: Option<&'a [u32]>,
    /// Every node pinned now, as node indexes: these do not move at all.
    pub pinned: &'a [u32],
    /// The schedule and how firmly the old nodes are held.
    pub params: WarmParams,
}

/// A [`LiveLayout::grow`] that does not fit the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadGrowth {
    /// The graph has fewer nodes than the layout: a layout grows, and
    /// hidden nodes keep their index.
    Shrunk { had: usize, got: usize },
    /// `parents` is not one entry per new node.
    Parents { new_nodes: usize, got: usize },
    /// A pinned index at or past the node count.
    Pinned { node: u32, n_nodes: usize },
    /// Positions that are not `x` then `y` for every node.
    Positions { n_nodes: usize, got: usize },
}

impl std::fmt::Display for BadGrowth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BadGrowth::Shrunk { had, got } => write!(
                f,
                "the layout has {had} nodes, but the grown graph has {got}"
            ),
            BadGrowth::Parents { new_nodes, got } => write!(
                f,
                "expected one parent per new node ({new_nodes}); got {got}"
            ),
            BadGrowth::Pinned { node, n_nodes } => {
                write!(f, "pinned node {node}, but the graph has {n_nodes} nodes")
            }
            BadGrowth::Positions { n_nodes, got } => write!(
                f,
                "expected x then y for {n_nodes} nodes ({} values); got {got}",
                n_nodes * 2
            ),
        }
    }
}

impl std::error::Error for BadGrowth {}

impl LiveLayout {
    pub fn new(graph: &Graph, params: Fa2Params) -> Self {
        Self::new_on(Arc::clone(graph.adjacency()), params)
    }

    /// [`LiveLayout::new`] for a caller that holds the graph's undirected
    /// adjacency and not its edge list: it is used as it is, and nothing is
    /// built or sorted. The layout is the same, bit for bit, as the one of
    /// a [`Graph`] with that adjacency. So are the other `_on` constructors
    /// and [`LiveLayout::grow_on`].
    pub fn new_on(adjacency: Arc<UndirectedCsr>, params: Fa2Params) -> Self {
        let state = Fa2State::new(&adjacency, params);
        Self {
            adjacency,
            engine: Engine::Flat(state),
        }
    }

    /// Coarse to fine: the graph's hierarchy is built here, and every node
    /// has a position from the start (its coarsest ancestor's, spread out).
    pub fn multilevel(graph: &Graph, params: MultilevelParams) -> Self {
        Self::multilevel_on(Arc::clone(graph.adjacency()), params)
    }

    /// [`LiveLayout::multilevel`], on an adjacency ([`LiveLayout::new_on`]).
    ///
    /// The hierarchy is a lean one (`Fa2Multilevel::new_lean`): the same
    /// positions at every step, holding less of a graph whose levels do not
    /// thin.
    pub fn multilevel_on(adjacency: Arc<UndirectedCsr>, params: MultilevelParams) -> Self {
        let state = Fa2Multilevel::new_lean(&adjacency, params);
        Self {
            adjacency,
            engine: Engine::Multilevel(Box::new(state)),
        }
    }

    /// A layout that starts from the positions `x`, `y` (one per node) and
    /// has nothing left to run: saved positions, another layout's (rings,
    /// layers), or the ones an undo restores. It is there to
    /// [`grow`](Self::grow) from.
    pub fn from_positions(
        graph: &Graph,
        params: Fa2Params,
        x: Vec<f32>,
        y: Vec<f32>,
    ) -> Result<Self, BadGrowth> {
        Self::from_positions_on(Arc::clone(graph.adjacency()), params, x, y)
    }

    /// [`LiveLayout::from_positions`], on an adjacency
    /// ([`LiveLayout::new_on`]).
    pub fn from_positions_on(
        adjacency: Arc<UndirectedCsr>,
        params: Fa2Params,
        x: Vec<f32>,
        y: Vec<f32>,
    ) -> Result<Self, BadGrowth> {
        let n = adjacency.offsets.len() - 1;
        if x.len() != n || y.len() != n {
            return Err(BadGrowth::Positions {
                n_nodes: n,
                got: x.len() + y.len(),
            });
        }
        let mass = degree_mass(&adjacency);
        Ok(Self {
            adjacency,
            engine: Engine::Warm {
                state: Fa2State::from_positions(params, x, y, mass),
                run: 0,
                total: 0,
            },
        })
    }

    /// Concentric rings, for a neighbourhood (a `hop` result):
    /// `distance[v]` is node `v`'s distance from a seed, 0 for a seed and
    /// [`ursa_core::algo::NO_LEVEL`] for a node that has none. One seed is
    /// at the centre with distance `d` on the circle of radius `d`; several
    /// seeds get a ring system each, side by side. The order on each ring
    /// shortens the graph's edges. It is `ursa_core::algo::layout_rings`,
    /// and it is done at once: [`Self::is_done`] from the start, with no
    /// iterations to run.
    pub fn rings(graph: &Graph, distance: &[u32], params: RingsParams) -> Result<Self, BadLevels> {
        Self::rings_on(Arc::clone(graph.adjacency()), distance, params)
    }

    /// [`LiveLayout::rings`], on an adjacency ([`LiveLayout::new_on`]).
    pub fn rings_on(
        adjacency: Arc<UndirectedCsr>,
        distance: &[u32],
        params: RingsParams,
    ) -> Result<Self, BadLevels> {
        Self::fixed(adjacency, distance, LayoutKind::Rings, |adj| {
            layout_rings(adj, distance, params)
        })
    }

    /// Layers left to right, for paths: `layer[v]` is node `v`'s position
    /// along its path (or its distance from the nearest of several seeds),
    /// [`ursa_core::algo::NO_LEVEL`] for a node that has none. Layer `l` is
    /// a vertical line at `x = l`, and one path alone is a straight line. It
    /// is `ursa_core::algo::layout_layers`, done at once like
    /// [`Self::rings`].
    pub fn layers(graph: &Graph, layer: &[u32], params: LayersParams) -> Result<Self, BadLevels> {
        Self::layers_on(Arc::clone(graph.adjacency()), layer, params)
    }

    /// [`LiveLayout::layers`], on an adjacency ([`LiveLayout::new_on`]).
    pub fn layers_on(
        adjacency: Arc<UndirectedCsr>,
        layer: &[u32],
        params: LayersParams,
    ) -> Result<Self, BadLevels> {
        Self::fixed(adjacency, layer, LayoutKind::Layers, |adj| {
            layout_layers(adj, layer, params)
        })
    }

    fn fixed(
        adjacency: Arc<UndirectedCsr>,
        levels: &[u32],
        kind: LayoutKind,
        place: impl FnOnce(&UndirectedCsr) -> (Vec<f32>, Vec<f32>),
    ) -> Result<Self, BadLevels> {
        let n = adjacency.offsets.len() - 1;
        if levels.len() != n {
            return Err(BadLevels {
                n_nodes: n,
                got: levels.len(),
            });
        }
        let (x, y) = place(&adjacency);
        Ok(Self {
            adjacency,
            engine: Engine::Fixed { kind, x, y },
        })
    }

    /// Which layout this is. A fixed layout that has grown is a force
    /// layout from then on.
    pub fn kind(&self) -> LayoutKind {
        match &self.engine {
            Engine::Fixed { kind, .. } => *kind,
            _ => LayoutKind::Force,
        }
    }

    /// Grow the layout to `graph`: the graph it had, with the same node
    /// indexes, plus new nodes after them and any new edges.
    ///
    /// Nothing that is drawn jumps. Each new node starts on a golden-angle
    /// spiral around its parent ([`Growth::parents`]), as the multilevel
    /// layout places a group's members. Then flat ForceAtlas2 runs warm
    /// from the positions the layout has now. [`Growth::pinned`] nodes are
    /// fixed and do not move by a bit; so are the new nodes' parents, and
    /// the old nodes more than two hops from a new one. The old nodes
    /// nearer than that have ten times a new node's inertia, so they make
    /// room without rearranging. A drawing of more than
    /// [`WarmParams::freeze_above`] nodes is held whole, so growing it costs
    /// what the new nodes do. [`WarmParams`] has each of these as a knob.
    ///
    /// The schedule is short: 60 iterations for a small addition.
    /// [`Self::total`] says how many, [`Self::step`] runs them and
    /// [`Self::is_done`] says when they are done. A graph that gained no
    /// node has none to run, unless [`WarmParams::iterations`] says so.
    ///
    /// The same layout grown by the same additions in the same order has the
    /// same bits, on any thread count (`ursa_core::algo::fa2_grow`). A
    /// coarse-to-fine layout can be grown before its schedule is done; it
    /// grows from the positions it has reached, and the rest of that
    /// schedule is dropped. Rings and layers grow from their positions too,
    /// as a force layout (coarse to fine's force law): the new nodes settle
    /// beside their parents, not on a ring or a layer.
    pub fn grow(&mut self, graph: &Graph, growth: Growth<'_>) -> Result<(), BadGrowth> {
        // Checked before the adjacency is built: a graph that does not fit
        // costs nothing.
        self.check_growth(graph.n_nodes(), &growth)?;
        self.grow_on(Arc::clone(graph.adjacency()), growth)
    }

    /// [`LiveLayout::grow`], to the graph with this adjacency
    /// ([`LiveLayout::new_on`]).
    pub fn grow_on(
        &mut self,
        adjacency: Arc<UndirectedCsr>,
        growth: Growth<'_>,
    ) -> Result<(), BadGrowth> {
        let had = self.positions().0.len();
        self.check_growth(adjacency.offsets.len() - 1, &growth)?;
        let placeholder = Engine::Warm {
            state: Fa2State::from_positions(Fa2Params::default(), vec![], vec![], vec![]),
            run: 0,
            total: 0,
        };
        let mut state = match std::mem::replace(&mut self.engine, placeholder) {
            Engine::Flat(s) | Engine::Warm { state: s, .. } => s,
            Engine::Multilevel(m) => {
                let (x, y) = m.positions();
                // The masses are the grown graph's, set below.
                Fa2State::from_positions(m.params().fa2, x.to_vec(), y.to_vec(), vec![1.0; had])
            }
            Engine::Fixed { x, y, .. } => {
                Fa2State::from_positions(MultilevelParams::default().fa2, x, y, vec![1.0; had])
            }
        };
        self.adjacency = adjacency;
        let total = fa2_grow(
            &mut state,
            &self.adjacency,
            growth.parents,
            growth.pinned,
            growth.params,
        );
        self.engine = Engine::Warm {
            state,
            run: 0,
            total,
        };
        Ok(())
    }

    /// Whether a graph of `n` nodes is one this layout can grow to with
    /// `growth`.
    fn check_growth(&self, n: usize, growth: &Growth<'_>) -> Result<(), BadGrowth> {
        let had = self.positions().0.len();
        if n < had {
            return Err(BadGrowth::Shrunk { had, got: n });
        }
        if let Some(p) = growth.parents {
            if p.len() != n - had {
                return Err(BadGrowth::Parents {
                    new_nodes: n - had,
                    got: p.len(),
                });
            }
        }
        if let Some(&node) = growth.pinned.iter().find(|&&p| p as usize >= n) {
            return Err(BadGrowth::Pinned { node, n_nodes: n });
        }
        Ok(())
    }

    /// Run `iterations` more iterations. Coarse to fine, that is work worth
    /// as many iterations of the whole graph, spent on as many iterations of
    /// the current level as it buys. A grown layout runs no more than its
    /// warm schedule has left.
    pub fn step(&mut self, iterations: u32) {
        let adj = &*self.adjacency;
        match &mut self.engine {
            Engine::Flat(s) => s.step(adj, iterations),
            Engine::Multilevel(s) => s.step(adj, f64::from(iterations)),
            Engine::Warm { state, run, total } => {
                let iterations = iterations.min(*total - *run);
                state.step(adj, iterations);
                *run += iterations;
            }
            Engine::Fixed { .. } => {}
        }
    }

    /// Iterations run so far: for coarse to fine, the work done, in
    /// iterations of the whole graph.
    pub fn iterations(&self) -> u32 {
        match &self.engine {
            Engine::Flat(s) => s.iterations(),
            Engine::Multilevel(s) => s.work_done().round() as u32,
            Engine::Warm { run, .. } => *run,
            Engine::Fixed { .. } => 0,
        }
    }

    /// The work of the whole schedule, in iterations of the whole graph, for
    /// coarse to fine; `None` for flat, which runs for as long as it is
    /// stepped; 0 for rings and layers.
    pub fn total(&self) -> Option<u32> {
        match &self.engine {
            Engine::Flat(_) => None,
            Engine::Multilevel(s) => Some(s.total_work().round() as u32),
            Engine::Warm { total, .. } => Some(*total),
            Engine::Fixed { .. } => Some(0),
        }
    }

    /// Whether the layout has run its schedule: never, for flat; from the
    /// start, for rings and layers.
    pub fn is_done(&self) -> bool {
        match &self.engine {
            Engine::Flat(_) => false,
            Engine::Multilevel(s) => s.is_done(),
            Engine::Warm { run, total, .. } => run >= total,
            Engine::Fixed { .. } => true,
        }
    }

    /// The level being laid out: 0 is the graph itself (always, for flat).
    pub fn level(&self) -> usize {
        match &self.engine {
            Engine::Flat(_) | Engine::Warm { .. } | Engine::Fixed { .. } => 0,
            Engine::Multilevel(s) => s.level(),
        }
    }

    /// How many levels there are to lay out: the coarse levels and the
    /// graph itself (1, for flat).
    pub fn levels(&self) -> usize {
        match &self.engine {
            Engine::Flat(_) | Engine::Warm { .. } | Engine::Fixed { .. } => 1,
            Engine::Multilevel(s) => s.coarse_levels() + 1,
        }
    }

    /// The positions now: `x` and `y`, one per node.
    pub fn positions(&self) -> (&[f32], &[f32]) {
        match &self.engine {
            Engine::Flat(s) | Engine::Warm { state: s, .. } => s.positions(),
            Engine::Multilevel(s) => s.positions(),
            Engine::Fixed { x, y, .. } => (x, y),
        }
    }
}

/// ForceAtlas2's mass for each node: its undirected degree plus one.
fn degree_mass(adj: &UndirectedCsr) -> Vec<f32> {
    (0..adj.offsets.len() as u32 - 1)
        .map(|u| 1.0 + adj.degree(u) as f32)
        .collect()
}

/// The smallest box holding every position: `(min_x, min_y, max_x, max_y)`,
/// or `None` for no nodes (or none finite).
pub fn bounds(x: &[f32], y: &[f32]) -> Option<(f32, f32, f32, f32)> {
    let mut b: Option<(f32, f32, f32, f32)> = None;
    for (&px, &py) in x.iter().zip(y) {
        if !(px.is_finite() && py.is_finite()) {
            continue;
        }
        b = Some(match b {
            None => (px, py, px, py),
            Some((a, c, d, e)) => (a.min(px), c.min(py), d.max(px), e.max(py)),
        });
    }
    b
}

/// A 2-D camera: the world point at the centre of the canvas, and how many
/// pixels a world unit spans.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub center: (f32, f32),
    pub pixels_per_unit: f32,
    /// The canvas size, in pixels.
    pub viewport: (f32, f32),
}

impl Camera {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            center: (0.0, 0.0),
            pixels_per_unit: 1.0,
            viewport: (width.max(1.0), height.max(1.0)),
        }
    }

    /// Show the whole box `(min_x, min_y, max_x, max_y)`, with a margin.
    pub fn fit(&mut self, (x0, y0, x1, y1): (f32, f32, f32, f32)) {
        self.center = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
        let (w, h) = ((x1 - x0).max(1e-6), (y1 - y0).max(1e-6));
        let margin = 0.92;
        self.pixels_per_unit = (self.viewport.0 / w).min(self.viewport.1 / h) * margin;
    }

    /// Move the view by a drag of `(dx, dy)` pixels (y down, as on screen).
    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.center.0 -= dx / self.pixels_per_unit;
        self.center.1 += dy / self.pixels_per_unit;
    }

    /// Zoom by `factor` (above 1 is in) around the pixel `(px, py)`, which
    /// stays over the same world point.
    pub fn zoom(&mut self, factor: f32, px: f32, py: f32) {
        let before = self.to_world(px, py);
        self.pixels_per_unit = (self.pixels_per_unit * factor).clamp(1e-6, 1e9);
        let after = self.to_world(px, py);
        self.center.0 += before.0 - after.0;
        self.center.1 += before.1 - after.1;
    }

    /// The world point under the pixel `(px, py)` (origin top left).
    pub fn to_world(&self, px: f32, py: f32) -> (f32, f32) {
        (
            self.center.0 + (px - self.viewport.0 * 0.5) / self.pixels_per_unit,
            self.center.1 - (py - self.viewport.1 * 0.5) / self.pixels_per_unit,
        )
    }

    /// The pixel over the world point `(x, y)`.
    pub fn to_pixel(&self, x: f32, y: f32) -> (f32, f32) {
        (
            (x - self.center.0) * self.pixels_per_unit + self.viewport.0 * 0.5,
            self.viewport.1 * 0.5 - (y - self.center.1) * self.pixels_per_unit,
        )
    }
}

/// How each node is drawn: a radius in pixels and a colour (`0xRRGGBBAA`).
#[derive(Debug, Clone, PartialEq)]
pub struct NodeStyle {
    pub radius: Vec<f32>,
    pub color: Vec<u32>,
}

/// The default node colour: near-black, opaque.
pub const NODE_COLOR: u32 = 0x2a_2a_2a_ff;

impl NodeStyle {
    /// Every node the same: radius `r` pixels, colour [`NODE_COLOR`].
    pub fn uniform(n: usize, r: f32) -> Self {
        Self {
            radius: vec![r; n],
            color: vec![NODE_COLOR; n],
        }
    }

    /// Radius from degree, so hubs read as hubs: `1.5 + sqrt(degree)` pixels,
    /// capped at 12.
    pub fn by_degree(degrees: &[u32]) -> Self {
        Self {
            radius: degrees.iter().map(|&d| Self::degree_radius(d)).collect(),
            color: vec![NODE_COLOR; degrees.len()],
        }
    }

    /// The radius [`NodeStyle::by_degree`] gives a node of degree `d`.
    pub fn degree_radius(d: u32) -> f32 {
        (1.5 + (d as f32).sqrt()).min(12.0)
    }

    /// Radii from a numeric column (`pagerank`, say): its range mapped to
    /// `min_r..max_r` pixels by square root, so area tracks value. Values that
    /// are not finite get `min_r`.
    pub fn sizes_from(values: &[f32], min_r: f32, max_r: f32) -> Vec<f32> {
        let range = Self::size_range(values.iter().copied());
        values
            .iter()
            .map(|&v| Self::size_from(v, range, min_r, max_r))
            .collect()
    }

    /// The least and the greatest of the finite `values`: the range
    /// [`NodeStyle::sizes_from`] maps to radii. With nothing finite, it is
    /// `(inf, -inf)`.
    pub fn size_range(values: impl IntoIterator<Item = f32>) -> (f32, f32) {
        let finite = values.into_iter().filter(|v| v.is_finite());
        finite.fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), v| {
            (a.min(v), b.max(v))
        })
    }

    /// The radius [`NodeStyle::sizes_from`] gives the value `v` of a column
    /// whose range is `(lo, hi)`.
    pub fn size_from(v: f32, (lo, hi): (f32, f32), min_r: f32, max_r: f32) -> f32 {
        // No spread (or nothing finite): every node the same size.
        if !v.is_finite() || hi <= lo || !hi.is_finite() {
            return min_r;
        }
        let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0).sqrt();
        min_r + t * (max_r - min_r)
    }

    /// Colours from categories (`louvain`'s community, say): category `c` gets
    /// the `c`-th colour of a small fixed palette, repeating past its end.
    pub fn colors_from(categories: &[u32]) -> Vec<u32> {
        categories
            .iter()
            .map(|&c| PALETTE[c as usize % PALETTE.len()])
            .collect()
    }
}

/// A categorical palette (`0xRRGGBBAA`) for a light background: eight hues,
/// blue, orange, aqua, yellow, magenta, green, violet and red, told apart
/// with colour-vision deficiencies too (adjacent pairs at least 9 ΔE OKLab
/// apart under protanopia, deuteranopia and tritanopia simulation).
pub const PALETTE: [u32; 8] = [
    0x2a78d6ff, 0xeb6834ff, 0x1baf7aff, 0xeda100ff, 0xe87ba4ff, 0x008300ff, 0x6250d6ff, 0xe34948ff,
];

/// [`PALETTE`]'s hues, in the same order, stepped for a dark background:
/// each at least 3:1 against near-black.
pub const DARK_PALETTE: [u32; 8] = [
    0x3987e5ff, 0xd95926ff, 0x199e70ff, 0xc98500ff, 0xd55181ff, 0x0ca30cff, 0x9085e9ff, 0xe66767ff,
];

/// The category of a node that has none (a null in the column it is
/// coloured by): drawn in the theme's node colour.
pub const NO_CATEGORY: u32 = u32::MAX;

/// The colours a view draws with, each `0xRRGGBBAA`: the background, the
/// edges' ink (their opacity is [`crate::render::Renderer::edge_alpha`]
/// times this alpha), the nodes' colour when they are not coloured by
/// category, and the categories' colours (category `c` is
/// `palette[c % palette.len()]`). Then the highlight layer's
/// ([`crate::highlight`]): highlighted edges, halos and the rings of a
/// route's ends; the rings of selected and hovered nodes; and the rings of
/// pinned ones. The built-in themes' are at least 3:1 against their
/// background ([`contrast`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub background: u32,
    pub edge: u32,
    pub node: u32,
    pub palette: Vec<u32>,
    pub highlight: u32,
    pub selection: u32,
    pub pin: u32,
}

impl Theme {
    /// Ink on white: the default.
    pub fn light() -> Self {
        Self {
            background: 0xffffffff,
            edge: 0x595966ff,
            node: NODE_COLOR,
            palette: PALETTE.to_vec(),
            highlight: 0xd9480fff,
            selection: 0x0b63ceff,
            pin: 0x7a3fc0ff,
        }
    }

    /// Light on near-black.
    pub fn dark() -> Self {
        Self {
            background: 0x0b0b0aff,
            edge: 0xb4b8c8ff,
            node: 0xe8e4dcff,
            palette: DARK_PALETTE.to_vec(),
            highlight: 0xffa94dff,
            selection: 0x6cb2ffff,
            pin: 0xc9a3ffff,
        }
    }

    /// Category colours with this theme's palette.
    pub fn colors(&self, categories: &[u32]) -> Vec<u32> {
        categories.iter().map(|&c| self.color(c)).collect()
    }

    /// Category `c`'s colour: `palette[c % palette.len()]`, or the node
    /// colour for [`NO_CATEGORY`] (and with an empty palette).
    pub fn color(&self, c: u32) -> u32 {
        if c == NO_CATEGORY || self.palette.is_empty() {
            return self.node;
        }
        self.palette[c as usize % self.palette.len()]
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::light()
    }
}

/// A CSS hex colour, `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` (the `#` is
/// optional), as `0xRRGGBBAA`.
pub fn parse_color(text: &str) -> Option<u32> {
    let hex = text.trim();
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let long: String = match hex.len() {
        3 | 4 => hex.chars().flat_map(|c| [c, c]).collect(),
        6 | 8 => hex.to_string(),
        _ => return None,
    };
    let v = u32::from_str_radix(&long, 16).ok()?;
    Some(if long.len() == 6 { (v << 8) | 0xff } else { v })
}

/// `0xRRGGBBAA` as red, green, blue and alpha from 0 to 1.
pub fn rgba(color: u32) -> [f32; 4] {
    color.to_be_bytes().map(|b| f32::from(b) / 255.0)
}

/// The WCAG contrast ratio of two colours (`0xRRGGBBAA`, alpha ignored):
/// from 1, the same, to 21, black on white.
pub fn contrast(a: u32, b: u32) -> f32 {
    let luminance = |c: u32| {
        let [r, g, b, _] = rgba(c).map(|v| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> Graph {
        Graph::new(4, vec![0, 1, 2, 3], vec![1, 2, 3, 0]).unwrap()
    }

    /// A skewed multigraph: hubs at the low indexes, parallel edges, both
    /// directions and self-loops.
    fn skewed(n: u32, m: usize) -> Graph {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut below = |k: u32| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) % u64::from(k)) as u32
        };
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for _ in 0..m {
            // The lesser of two draws: low indexes are named most.
            src.push(below(n).min(below(n)));
            dst.push(below(n).min(below(n)).min(below(n)));
        }
        Graph::new(n as usize, src, dst).unwrap()
    }

    #[test]
    fn a_graph_builds_no_adjacency_until_a_layout_asks() {
        let graph = skewed(300, 4000);
        assert!(!graph.has_adjacency());
        // Degrees and line indices come from the edge list.
        let degrees = graph.degrees();
        assert_eq!(graph.line_indices(0..4000).len(), 8000);
        assert!(!graph.has_adjacency());
        // The seeded positions are a layout's, and need no layout.
        let (x, y) = fa2_seed_positions(graph.n_nodes(), &Fa2Params::default());
        assert!(!graph.has_adjacency());

        // A layout asks; its adjacency gives the same degrees, and it
        // starts from the same positions, bit for bit.
        let layout = LiveLayout::new(&graph, Fa2Params::default());
        assert!(graph.has_adjacency());
        let adj = graph.adjacency();
        let exact: Vec<u32> = (0..300).map(|u| adj.degree(u) as u32).collect();
        assert_eq!(degrees, exact);
        assert_eq!(graph.degrees(), exact);
        let bits = |v: &[f32]| v.iter().map(|f| f.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&x), bits(layout.positions().0));
        assert_eq!(bits(&y), bits(layout.positions().1));

        // A change drops the adjacency, until it is next asked for.
        let mut graph = graph;
        graph.append(1, &[300], &[0]).unwrap();
        assert!(!graph.has_adjacency());
        assert_eq!(graph.degrees()[300], 1);
        assert!(!graph.has_adjacency());
    }

    #[test]
    fn pairs_know_who_is_joined_and_count_distinct_neighbours() {
        for graph in [
            skewed(300, 4000),
            skewed(7, 60),
            Graph::new(3, vec![1, 1], vec![1, 1]).unwrap(),
            Graph::new(0, vec![], vec![]).unwrap(),
        ] {
            let n = graph.n_nodes() as u32;
            let (mut pairs, degrees) = Pairs::new(graph.n_nodes(), graph.src(), graph.dst());
            let adj = graph.adjacency();
            let exact: Vec<u32> = (0..n).map(|u| adj.degree(u) as u32).collect();
            assert_eq!(degrees, exact);
            // Each pair is kept once.
            assert_eq!(2 * pairs.other.len() as u32, exact.iter().sum::<u32>());
            // Asked twice: a long list is sorted by the first answer.
            for _ in 0..2 {
                for s in 0..n {
                    for d in 0..n {
                        let joined = s != d && adj.neighbors(s).binary_search(&d).is_ok();
                        assert_eq!(pairs.joined(s, d), joined, "{s} and {d}");
                    }
                }
            }
            // Past the nodes there are, nothing is joined.
            assert!(!pairs.joined(0, n) && !pairs.joined(n + 3, 0));
        }
        // The hubs' lists are long enough to be sorted and searched.
        let hubs = skewed(300, 4000);
        let (pairs, _) = Pairs::new(hubs.n_nodes(), hubs.src(), hubs.dst());
        assert!(pairs.offsets[1] as usize > Pairs::SHORT);
    }

    #[test]
    fn a_graph_grows_and_shrinks_at_its_end() {
        let mut graph = Graph::new(3, vec![0, 1], vec![1, 2]).unwrap();
        assert_eq!(graph.degrees(), [1, 2, 1]);
        // Two more nodes, and three more edges.
        graph.append(2, &[2, 3, 0], &[3, 4, 4]).unwrap();
        assert_eq!((graph.n_nodes(), graph.n_edges()), (5, 5));
        assert_eq!(graph.line_indices(2..5), [2, 3, 3, 4, 0, 4]);
        // The adjacency is the grown graph's, when next asked for.
        assert_eq!(graph.degrees(), [2, 2, 2, 2, 2]);
        assert_eq!(graph.adjacency().offsets.len() - 1, 5);

        // An edge that names no node changes nothing, and says which.
        let bad = graph.append(1, &[5, 0], &[0, 6]).unwrap_err();
        assert_eq!((bad.edge, bad.node, bad.n_nodes), (6, 6, 6));
        assert_eq!((graph.n_nodes(), graph.n_edges()), (5, 5));
        // Nor does taking off a node an edge that stays still names.
        assert!(graph.truncate(3, 3).is_err());
        assert_eq!((graph.n_nodes(), graph.n_edges()), (5, 5));

        graph.truncate(3, 2).unwrap();
        assert_eq!((graph.n_nodes(), graph.n_edges()), (3, 2));
        assert_eq!(graph.degrees(), [1, 2, 1]);
    }

    #[test]
    fn a_graph_checks_its_edges() {
        let err = Graph::new(2, vec![0, 1], vec![1, 2]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "edge 1 names node 2, but the graph has 2 nodes"
        );
        let g = square();
        assert_eq!((g.n_nodes(), g.n_edges()), (4, 4));
        assert_eq!(g.line_indices(0..4), vec![0, 1, 1, 2, 2, 3, 3, 0]);
        assert_eq!(g.degrees(), vec![2, 2, 2, 2]);
    }

    #[test]
    fn a_live_layout_is_layout_fa2() {
        let g = square();
        let params = Fa2Params::default();
        let mut live = LiveLayout::new(&g, params);
        live.step(3);
        live.step(7);
        assert_eq!(live.iterations(), 10);
        let mut state = Fa2State::new(g.adjacency(), params);
        state.step(g.adjacency(), 10);
        let (x, y) = live.positions();
        let (ex, ey) = state.positions();
        assert_eq!((x, y), (ex, ey));
    }

    #[test]
    fn rings_and_layers_are_done_at_once_and_grow() {
        // A seed, two neighbours and one node two hops out.
        let g = Graph::new(4, vec![0, 0, 1], vec![1, 2, 3]).unwrap();
        let level = [0, 1, 1, 2];
        let rings = LiveLayout::rings(&g, &level, RingsParams::default()).unwrap();
        assert_eq!(rings.kind(), LayoutKind::Rings);
        assert!(rings.is_done());
        assert_eq!((rings.iterations(), rings.total()), (0, Some(0)));
        assert_eq!((rings.level(), rings.levels()), (0, 1));
        let (ex, ey) = layout_rings(g.adjacency(), &level, RingsParams::default());
        assert_eq!(rings.positions(), (&ex[..], &ey[..]));
        let (x, y) = rings.positions();
        for v in 0..4 {
            assert!((x[v].hypot(y[v]) - level[v] as f32).abs() < 1e-5);
        }

        let mut layers = LiveLayout::layers(&g, &level, LayersParams::default()).unwrap();
        assert_eq!(layers.kind().name(), "layers");
        assert!(layers.is_done());
        let before = (layers.positions().0.to_vec(), layers.positions().1.to_vec());
        assert_eq!(before.0, vec![-1.0, 0.0, 0.0, 1.0]);
        // Stepping a fixed layout does nothing.
        layers.step(50);
        assert_eq!(layers.positions(), (&before.0[..], &before.1[..]));

        // One level too few.
        let err = LiveLayout::rings(&g, &level[..3], RingsParams::default()).unwrap_err();
        assert_eq!(err.to_string(), "expected one level per node (4); got 3");

        // A fixed layout grows as any other: node 3 gains a neighbour, which
        // starts beside it, and the layout is a force layout from then on.
        let grown = Graph::new(5, vec![0, 0, 1, 3], vec![1, 2, 3, 4]).unwrap();
        layers
            .grow(&grown, Growth::default())
            .expect("a fixed layout grows");
        assert_eq!(layers.kind(), LayoutKind::Force);
        assert!(!layers.is_done());
        while !layers.is_done() {
            layers.step(10);
        }
        let (x, y) = layers.positions();
        assert_eq!(x.len(), 5);
        assert!(x.iter().chain(y).all(|v| v.is_finite()));
    }

    #[test]
    fn a_multilevel_live_layout_is_layout_fa2_multilevel() {
        // A grid big enough to coarsen, stepped in uneven budgets: the end is
        // the one-shot layout, uncentered.
        let (w, h) = (30u32, 30u32);
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if x + 1 < w {
                    src.push(i);
                    dst.push(i + 1);
                }
                if y + 1 < h {
                    src.push(i);
                    dst.push(i + w);
                }
            }
        }
        let g = Graph::new((w * h) as usize, src, dst).unwrap();
        let params = MultilevelParams::default();
        let mut live = LiveLayout::multilevel(&g, params);
        assert!(live.level() > 0, "a 900-node grid is coarsened");
        assert_eq!(live.levels(), live.level() + 1, "it starts at the coarsest");
        assert_eq!(
            live.positions().0.len(),
            900,
            "every node has a place at once"
        );
        let total = live.total().unwrap();
        while !live.is_done() {
            live.step(7);
        }
        assert!(live.iterations().abs_diff(total) <= 1);
        let mut state = Fa2Multilevel::new(g.adjacency(), params);
        state.step(g.adjacency(), f64::INFINITY);
        assert_eq!(live.positions(), state.positions());
    }

    #[test]
    fn the_camera_fits_pans_and_zooms_around_the_cursor() {
        let mut c = Camera::new(200.0, 100.0);
        c.fit((-10.0, -5.0, 10.0, 5.0));
        assert_eq!(c.center, (0.0, 0.0));
        assert!((c.pixels_per_unit - 9.2).abs() < 1e-4);
        let (px, py) = c.to_pixel(10.0, 5.0);
        assert!(px < 200.0 && py > 0.0, "the corner is inside the canvas");

        // The world point under the cursor stays under it.
        let before = c.to_world(150.0, 20.0);
        c.zoom(2.5, 150.0, 20.0);
        let after = c.to_world(150.0, 20.0);
        assert!((before.0 - after.0).abs() < 1e-4 && (before.1 - after.1).abs() < 1e-4);

        // Dragging right moves the world right with the cursor.
        let p = c.to_pixel(1.0, 1.0);
        c.pan(30.0, -10.0);
        let q = c.to_pixel(1.0, 1.0);
        assert!((q.0 - p.0 - 30.0).abs() < 1e-3 && (q.1 - p.1 + 10.0).abs() < 1e-3);
    }

    #[test]
    fn themes_parse_css_colours_and_colour_categories() {
        assert_eq!(parse_color("#0b0b0a"), Some(0x0b0b0aff));
        assert_eq!(parse_color("ffffff80"), Some(0xffffff80));
        assert_eq!(parse_color(" #fa0 "), Some(0xffaa00ff));
        assert_eq!(parse_color("#fa08"), Some(0xffaa0088));
        for bad in ["", "#", "#12345", "#ggg", "rgb(1, 2, 3)", "#+1234"] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
        assert_eq!(rgba(0xff000080), [1.0, 0.0, 0.0, 128.0 / 255.0]);
        let dark = Theme::dark();
        assert_eq!(dark.colors(&[0, 9]), vec![DARK_PALETTE[0], DARK_PALETTE[1]]);
        let plain = Theme {
            palette: vec![],
            ..Theme::light()
        };
        assert_eq!(plain.colors(&[3]), vec![NODE_COLOR]);
        assert_eq!(Theme::default(), Theme::light());
    }

    #[test]
    fn bounds_skip_non_finite_positions() {
        assert_eq!(bounds(&[], &[]), None);
        assert_eq!(
            bounds(&[1.0, f32::NAN, -2.0], &[0.5, 9.0, 3.0]),
            Some((-2.0, 0.5, 1.0, 3.0))
        );
    }

    #[test]
    fn styles_map_columns_to_sizes_and_colours() {
        assert_eq!(
            NodeStyle::sizes_from(&[0.0, 1.0, 0.25, f32::NAN], 2.0, 10.0),
            vec![2.0, 10.0, 6.0, 2.0]
        );
        assert_eq!(
            NodeStyle::sizes_from(&[3.0, 3.0], 2.0, 10.0),
            vec![2.0, 2.0]
        );
        assert_eq!(
            NodeStyle::colors_from(&[0, 1, 8]),
            vec![PALETTE[0], PALETTE[1], PALETTE[0]]
        );
        let s = NodeStyle::by_degree(&[0, 4, 1000]);
        assert_eq!(s.radius, vec![1.5, 3.5, 12.0]);
    }
}
