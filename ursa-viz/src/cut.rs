//! The aggregated view: a cut through the layout's hierarchy, chosen by
//! the camera (#498).
//!
//! Zoomed out, a million edges are a blot at any frame rate. Coarse-to-fine
//! ForceAtlas2 has already grouped the graph, level over level
//! (`ursa_core::algo::Fa2Multilevel`): every node of a level stands for a
//! few nodes of the level below, down to the graph's own. That is a forest
//! over the graph's nodes, and a **cut** through it is a set of its nodes
//! such that every node of the graph is under exactly one of them (an
//! antichain that covers the leaves). The view draws the cut in place of
//! the graph: a disc for each of its nodes, sized by mass, and the merged
//! edges between them, the heaviest first.
//!
//! Which cut ([`Hierarchy::cut`]): the coarsest one whose discs stand
//! `separation` pixels apart on screen. A group is opened into its
//! children only if they would: if their discs' centres are, by the root
//! of their mean square distance from the group's own (its **spread**),
//! at least `separation` pixels from it. Starting from the forest's roots,
//! the group in view with the largest spread on screen is opened, again
//! and again, until no group in view has that much or opening another
//! would pass `max_nodes`. So a group out of view is never opened (it
//! costs one disc however far the view zooms in), a group whose children
//! lie on top of one another stays one disc until the view is close
//! enough to tell them apart, and zooming in opens the groups in view
//! down to the graph's own nodes. When every node of the cut in view is a
//! node of the graph, or the groups in view hold at most `detail_nodes`
//! of them, the cut says so ([`Cut::detail`]): the view then draws the
//! graph itself, exactly as it does with no cut.
//!
//! A group's place is the centre of mass of the nodes it stands for, where
//! the layout has them now, and it is in view if the box around those
//! nodes meets the view ([`Hierarchy::place`]).
//!
//! **What the layout's hierarchy leaves out.** A node with no neighbour is
//! in no group, and nor is a component the hierarchy has merged into one
//! node: the layout drops them from the levels above and brings them back
//! on a ring around the drawing. A graph can have tens of thousands (the
//! scale-17 R-MAT graph has 54,000 nodes with no edge), and as roots of
//! the forest each would be a disc at any zoom. So [`Hierarchy::place`]
//! groups them by where they are: in Z order over their bounding box,
//! [`FAN`] neighbours to a group, and those groups again, until [`FAN`]
//! are left. These groups are opened as any other is.
//!
//! Everything here is a pure function of the hierarchy, the positions and
//! the camera, on one thread, in a fixed order: the same cut every time.
//! No GPU and no browser: the layout worker runs it, since it holds the
//! hierarchy.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use ursa_core::algo::Fa2Multilevel;
use ursa_core::topology::UndirectedCsr;

/// How far apart, in pixels, the discs of a cut stand, about: a group in
/// view whose children would be this far from its centre is opened.
pub const SEPARATION: f32 = 24.0;
/// The most discs a cut has.
pub const MAX_NODES: usize = 20_000;
/// The most merged edges a cut gives: the heaviest.
pub const MAX_EDGES: usize = 100_000;
/// With at most this many of the graph's nodes under the groups in view,
/// the graph itself is drawn.
pub const DETAIL_NODES: usize = 2_000;

/// How many of the nodes the layout's hierarchy left out make a group, and
/// how many such groups may be roots ([`Hierarchy::place`]).
pub const FAN: usize = 4;

/// The layout's hierarchy as a forest, and where its groups are now.
///
/// Its nodes are numbered level by level: the graph's own `0..n`, then
/// level 1's, and so on; after the last level's, the groups of what the
/// hierarchy left out. For the 131K-node, 1M-edge R-MAT graph it holds
/// nine megabytes: four bytes an edge, and the rest by node.
#[derive(Debug, Clone)]
pub struct Hierarchy {
    /// The graph's nodes: the forest's leaves.
    n: usize,
    /// The first node of each level, and after the last, the count of the
    /// layout's own nodes.
    base: Vec<u32>,
    /// The children of each group (a node of level 1 or above; group `g`
    /// is node `n + g`): `children[child_offsets[g]..child_offsets[g + 1]]`.
    child_offsets: Vec<u32>,
    children: Vec<u32>,
    /// The coarsest level's nodes, and what left the hierarchy below it
    /// (an isolated node, a component merged into one node), finest level
    /// last: the layout's own nodes with no parent.
    top: Vec<u32>,
    orphans: Vec<u32>,
    /// The nodes with no parent now: `top`, then the orphans or, where
    /// they are many, the groups [`Hierarchy::place`] made of them.
    roots: Vec<u32>,
    /// Each group's leaves, and their summed mass (`degree + 1` each).
    count: Vec<u32>,
    mass: Vec<f32>,
    /// Each group's centre of mass, how far from it its children's
    /// centres are (the root of their mean square distance, by mass), and
    /// the box around its leaves, `(min_x, min_y, max_x, max_y)`
    /// ([`Hierarchy::place`]).
    x: Vec<f32>,
    y: Vec<f32>,
    spread: Vec<f32>,
    bounds: Vec<[f32; 4]>,
    /// Each leaf's number in the forest's order, and each edge of the
    /// graph once, at its earlier end, as its later end's number: row
    /// `r`'s are `later[later_offsets[r]..later_offsets[r + 1]]`,
    /// ascending ([`Hierarchy::index_edges`]).
    rank: Vec<u32>,
    later_offsets: Vec<u32>,
    later: Vec<u32>,
}

/// What a cut is asked for: the camera, and the limits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CutParams {
    /// The part of the layout in view: `(min_x, min_y, max_x, max_y)`.
    pub view: (f32, f32, f32, f32),
    /// How many pixels a unit of the layout spans.
    pub pixels_per_unit: f32,
    /// [`SEPARATION`].
    pub separation: f32,
    /// [`MAX_NODES`].
    pub max_nodes: usize,
    /// [`MAX_EDGES`].
    pub max_edges: usize,
    /// [`DETAIL_NODES`].
    pub detail_nodes: usize,
}

impl CutParams {
    /// The cut for a camera at `center` with `pixels_per_unit`, on a
    /// canvas of `viewport` pixels, with the default limits.
    pub fn of_camera(center: (f32, f32), pixels_per_unit: f32, viewport: (f32, f32)) -> Self {
        let ppu = pixels_per_unit.max(1e-9);
        let (hw, hh) = (viewport.0 * 0.5 / ppu, viewport.1 * 0.5 / ppu);
        Self {
            view: (center.0 - hw, center.1 - hh, center.0 + hw, center.1 + hh),
            pixels_per_unit,
            separation: SEPARATION,
            max_nodes: MAX_NODES,
            max_edges: MAX_EDGES,
            detail_nodes: DETAIL_NODES,
        }
    }
}

/// A cut through the hierarchy ([`Hierarchy::cut`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Cut {
    /// Whether the graph itself is to be drawn: every node of the cut in
    /// view is one of the graph's, or the groups in view hold few enough.
    /// The edges are then not gathered (`src`, `dst` and `weight` are
    /// empty): the graph's own are drawn.
    pub detail: bool,
    /// The cut's nodes, as nodes of the hierarchy ([`Hierarchy`]'s
    /// numbering), ascending: one below [`Hierarchy::n_leaves`] is a node
    /// of the graph.
    pub nodes: Vec<u32>,
    /// Each node's place.
    pub x: Vec<f32>,
    pub y: Vec<f32>,
    /// Each node's mass: the summed `degree + 1` of the graph's nodes
    /// under it.
    pub mass: Vec<f32>,
    /// How many of the graph's nodes are under each: they sum to the
    /// graph's node count.
    pub members: Vec<u32>,
    /// For each node of the graph, the node of the cut it is under: an
    /// index into `nodes`.
    pub of: Vec<u32>,
    /// The merged edges, heaviest first (those as heavy, in the order of
    /// their earlier ends in the forest): edge `i` joins the cut's nodes
    /// `src[i] < dst[i]` and stands for `weight[i]` of the graph's edges.
    /// At most
    /// [`CutParams::max_edges`] of the `total_edges` the cut has.
    pub src: Vec<u32>,
    pub dst: Vec<u32>,
    pub weight: Vec<f32>,
    pub total_edges: usize,
    /// How many of the cut's nodes are in view, and how many of the
    /// graph's nodes are under those.
    pub in_view: usize,
    pub members_in_view: usize,
}

/// A group waiting to be opened: the largest on screen first, then the
/// lowest numbered.
#[derive(Debug, PartialEq)]
struct Open {
    size: f32,
    node: u32,
}

impl Eq for Open {}

impl Ord for Open {
    fn cmp(&self, other: &Self) -> Ordering {
        self.size
            .total_cmp(&other.size)
            .then(other.node.cmp(&self.node))
    }
}

impl PartialOrd for Open {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hierarchy {
    /// The hierarchy of `layout`, a layout of the graph with adjacency
    /// `adj`, its groups not yet placed ([`Hierarchy::place`]). `None` if
    /// the layout has no coarse level.
    pub fn of_layout(layout: &Fa2Multilevel, adj: &UndirectedCsr) -> Option<Self> {
        let levels = layout.coarse_levels();
        if levels == 0 {
            return None;
        }
        let members: Vec<(&[u32], &[u32])> = (1..=levels)
            .map(|l| layout.level_members(l))
            .collect::<Option<_>>()?;
        Some(Self::new(adj, &members))
    }

    /// The forest over the nodes of the graph with adjacency `adj`, whose
    /// level `l + 1` groups level `l`'s nodes as `levels[l]` says:
    /// `(offsets, members)`, a CSR of the members of each node (level 0
    /// is the graph's nodes, the leaves). A leaf's mass is its degree
    /// plus one, as the layout has it.
    ///
    /// # Panics
    ///
    /// If a level names a node the level below does not have, or names
    /// one twice.
    pub fn new(adj: &UndirectedCsr, levels: &[(&[u32], &[u32])]) -> Self {
        let n = adj.offsets.len() - 1;
        let leaf_mass = |v: u32| 1.0 + adj.degree(v) as f32;
        let mut base = vec![0u32, n as u32];
        for (offsets, _) in levels {
            let last = *base.last().unwrap();
            base.push(last + (offsets.len() - 1) as u32);
        }
        let total = *base.last().unwrap() as usize;
        let groups = total - n;
        let mut child_offsets = Vec::with_capacity(groups + 1);
        let mut children = Vec::with_capacity(levels.iter().map(|(_, m)| m.len()).sum());
        let mut has_parent = vec![false; total];
        child_offsets.push(0);
        for (l, (offsets, members)) in levels.iter().enumerate() {
            let below = base[l];
            let below_n = base[l + 1] - below;
            for c in 0..offsets.len() - 1 {
                for &m in &members[offsets[c] as usize..offsets[c + 1] as usize] {
                    assert!(m < below_n, "level {} has no node {m}", l);
                    let child = below + m;
                    assert!(
                        !std::mem::replace(&mut has_parent[child as usize], true),
                        "node {m} of level {l} is in two groups"
                    );
                    children.push(child);
                }
                child_offsets.push(children.len() as u32);
            }
        }
        // The coarsest level, then what left the hierarchy below it.
        let levels = base.len() - 1;
        let top: Vec<u32> = (base[levels - 1]..base[levels]).collect();
        let mut orphans = Vec::new();
        for l in (0..levels - 1).rev() {
            orphans.extend((base[l]..base[l + 1]).filter(|&v| !has_parent[v as usize]));
        }
        drop(has_parent);
        let roots = top.iter().chain(&orphans).copied().collect();
        let mut this = Self {
            n,
            base,
            child_offsets,
            children,
            top,
            orphans,
            roots,
            count: vec![0; groups],
            mass: vec![0.0; groups],
            x: vec![0.0; groups],
            y: vec![0.0; groups],
            spread: vec![0.0; groups],
            bounds: vec![[0.0; 4]; groups],
            rank: Vec::new(),
            later_offsets: Vec::new(),
            later: Vec::new(),
        };
        // A group's children are numbered below it: one pass, ascending.
        for g in 0..groups {
            let (mut count, mut mass) = (0u32, 0.0f32);
            for &c in this.children_of(g) {
                if (c as usize) < n {
                    count += 1;
                    mass += leaf_mass(c);
                } else {
                    count += this.count[c as usize - n];
                    mass += this.mass[c as usize - n];
                }
            }
            this.count[g] = count;
            this.mass[g] = mass;
        }
        this.index_edges(adj);
        this
    }

    fn children_of(&self, group: usize) -> &[u32] {
        &self.children[self.child_offsets[group] as usize..self.child_offsets[group + 1] as usize]
    }

    /// How many nodes the graph has: the forest's leaves, its nodes
    /// `0..n_leaves()`.
    pub fn n_leaves(&self) -> usize {
        self.n
    }

    /// How many nodes the forest has, the leaves among them.
    pub fn n_nodes(&self) -> usize {
        self.n + self.count.len()
    }

    /// How many of them are the layout's own: the rest are the groups of
    /// what its hierarchy left out, made by [`Hierarchy::place`].
    pub fn n_layout_nodes(&self) -> usize {
        *self.base.last().unwrap() as usize
    }

    /// How many levels there are, the graph itself included.
    pub fn levels(&self) -> usize {
        self.base.len() - 1
    }

    /// The level of a node of the layout's hierarchy: 0 for one of the
    /// graph's. `None` for a group of what the hierarchy left out.
    pub fn level_of(&self, node: u32) -> Option<usize> {
        ((node as usize) < self.n_layout_nodes())
            .then(|| self.base.partition_point(|&b| b <= node) - 1)
    }

    /// The nodes with no parent.
    pub fn roots(&self) -> &[u32] {
        &self.roots
    }

    /// The children of a node of the forest: none, for one of the graph's.
    pub fn children(&self, node: u32) -> &[u32] {
        match (node as usize).checked_sub(self.n) {
            Some(g) => self.children_of(g),
            None => &[],
        }
    }

    /// How many of the graph's nodes are under `node`.
    pub fn members(&self, node: u32) -> u32 {
        match (node as usize).checked_sub(self.n) {
            Some(g) => self.count[g],
            None => 1,
        }
    }

    /// The bytes this holds.
    pub fn bytes(&self) -> usize {
        let ids = self.base.len()
            + self.child_offsets.len()
            + self.children.len()
            + self.top.len()
            + self.orphans.len()
            + self.roots.len();
        let index = self.rank.len() + self.later_offsets.len() + self.later.len();
        4 * (ids + index) + 36 * self.count.len()
    }

    /// Put every group at the centre of mass of the leaves under it, the
    /// graph's nodes being at `x`, `y` (one per node each), with the box
    /// around them and its children's spread about it. `adj` is the
    /// graph's adjacency, as [`Hierarchy::new`] had it.
    ///
    /// What the layout's hierarchy left out is grouped again here, by
    /// where it is now (the module docs): the groups made by the last
    /// call are let go.
    pub fn place(&mut self, adj: &UndirectedCsr, x: &[f32], y: &[f32]) {
        assert_eq!((x.len(), y.len()), (self.n, self.n), "one place per node");
        assert_eq!(adj.offsets.len() - 1, self.n, "the adjacency of the graph");
        let leaf_mass = |v: u32| 1.0 + adj.degree(v) as f32;
        let n = self.n;
        // The layout's own groups first: the others are made of them.
        let own = self.n_layout_nodes() - n;
        self.child_offsets.truncate(own + 1);
        self.children.truncate(self.child_offsets[own] as usize);
        for v in [&mut self.x, &mut self.y, &mut self.spread, &mut self.mass] {
            v.truncate(own);
        }
        self.count.truncate(own);
        self.bounds.truncate(own);
        self.place_from(0, x, y, &leaf_mass);

        self.roots.clear();
        self.roots.extend(&self.top);
        if self.orphans.len() <= FAN {
            self.roots.extend(&self.orphans);
            return;
        }
        // In Z order over their box: neighbours in the order are
        // neighbours on the canvas, mostly.
        let at = |this: &Self, v: u32| match (v as usize).checked_sub(n) {
            Some(g) => (this.x[g], this.y[g]),
            None => (x[v as usize], y[v as usize]),
        };
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &v in &self.orphans {
            let (px, py) = at(self, v);
            if px.is_finite() && py.is_finite() {
                (x0, y0, x1, y1) = (x0.min(px), y0.min(py), x1.max(px), y1.max(py));
            }
        }
        let cell = |p: f32, lo: f32, hi: f32| {
            let t = if hi > lo { (p - lo) / (hi - lo) } else { 0.0 };
            // NaN, and anything outside, to the first or last cell.
            (t.clamp(0.0, 1.0) * 65535.0) as u32
        };
        let spread = |v: u32| {
            let mut v = v & 0xffff;
            v = (v | (v << 8)) & 0x00ff_00ff;
            v = (v | (v << 4)) & 0x0f0f_0f0f;
            v = (v | (v << 2)) & 0x3333_3333;
            (v | (v << 1)) & 0x5555_5555
        };
        let mut keyed: Vec<u64> = (self.orphans.iter())
            .map(|&v| {
                let (px, py) = at(self, v);
                let z = spread(cell(px, x0, x1)) | (spread(cell(py, y0, y1)) << 1);
                (u64::from(z) << 32) | u64::from(v)
            })
            .collect();
        keyed.sort_unstable();
        let mut row: Vec<u32> = keyed.iter().map(|&k| k as u32).collect();
        drop(keyed);
        while row.len() > FAN {
            let first = self.count.len();
            let mut above = Vec::with_capacity(row.len().div_ceil(FAN));
            for chunk in row.chunks(FAN) {
                // One left over stays as it is.
                if let [single] = chunk {
                    above.push(*single);
                    continue;
                }
                let g = self.count.len();
                self.children.extend_from_slice(chunk);
                self.child_offsets.push(self.children.len() as u32);
                let (mut count, mut mass) = (0u32, 0.0f32);
                for &c in chunk {
                    match (c as usize).checked_sub(n) {
                        Some(h) => {
                            count += self.count[h];
                            mass += self.mass[h];
                        }
                        None => {
                            count += 1;
                            mass += leaf_mass(c);
                        }
                    }
                }
                self.count.push(count);
                self.mass.push(mass);
                self.x.push(0.0);
                self.y.push(0.0);
                self.spread.push(0.0);
                self.bounds.push([0.0; 4]);
                above.push((n + g) as u32);
            }
            self.place_from(first, x, y, &leaf_mass);
            row = above;
        }
        self.roots.extend(&row);
    }

    /// Place the groups from `first` on, ascending: a group's children
    /// are numbered below it.
    fn place_from(&mut self, first: usize, x: &[f32], y: &[f32], leaf_mass: &impl Fn(u32) -> f32) {
        let n = self.n;
        for g in first..self.count.len() {
            let at = |c: u32| match (c as usize).checked_sub(n) {
                Some(h) => (self.x[h], self.y[h], self.bounds[h], self.mass[h]),
                None => {
                    let (px, py) = (x[c as usize], y[c as usize]);
                    (px, py, [px, py, px, py], leaf_mass(c))
                }
            };
            let (mut sx, mut sy, mut sm) = (0.0f64, 0.0f64, 0.0f64);
            let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
            for &c in self.children_of(g) {
                let (cx, cy, cb, m) = at(c);
                sx += f64::from(cx) * f64::from(m);
                sy += f64::from(cy) * f64::from(m);
                sm += f64::from(m);
                b = [
                    b[0].min(cb[0]),
                    b[1].min(cb[1]),
                    b[2].max(cb[2]),
                    b[3].max(cb[3]),
                ];
            }
            let (gx, gy) = if sm > 0.0 {
                ((sx / sm) as f32, (sy / sm) as f32)
            } else {
                (0.0, 0.0)
            };
            let mut square = 0.0f64;
            for &c in self.children_of(g) {
                let (cx, cy, _, m) = at(c);
                let (dx, dy) = (f64::from(cx - gx), f64::from(cy - gy));
                square += f64::from(m) * (dx * dx + dy * dy);
            }
            self.x[g] = gx;
            self.y[g] = gy;
            self.spread[g] = if sm > 0.0 {
                (square / sm).sqrt() as f32
            } else {
                0.0
            };
            self.bounds[g] = b;
        }
    }

    /// The cut for `params`, the graph's nodes being at `x`, `y` (as
    /// [`Hierarchy::place`] last had them) and its adjacency `adj`. See
    /// the module docs for which cut that is.
    pub fn cut(&self, adj: &UndirectedCsr, x: &[f32], y: &[f32], params: &CutParams) -> Cut {
        let n = self.n;
        assert_eq!((x.len(), y.len()), (n, n), "one place per node");
        assert_eq!(adj.offsets.len() - 1, n, "the adjacency of the graph");
        let (x0, y0, x1, y1) = params.view;
        let ppu = params.pixels_per_unit;
        let at = |v: u32| match (v as usize).checked_sub(n) {
            Some(g) => (self.x[g], self.y[g]),
            None => (x[v as usize], y[v as usize]),
        };
        // Whether the box around the leaves of `v` meets the view.
        let in_view = |v: u32| {
            let [bx0, by0, bx1, by1] = match (v as usize).checked_sub(n) {
                Some(g) => self.bounds[g],
                None => {
                    let (px, py) = (x[v as usize], y[v as usize]);
                    [px, py, px, py]
                }
            };
            bx0 <= x1 && bx1 >= x0 && by0 <= y1 && by1 >= y0
        };
        // A group in view whose children would stand apart on screen.
        let open = |v: u32| {
            let g = (v as usize).checked_sub(n)?;
            let size = self.spread[g] * ppu;
            (size >= params.separation && in_view(v)).then_some(Open { size, node: v })
        };

        // The largest group in view first, until none is large enough or
        // the cut is as many nodes as it may be.
        let mut kept: Vec<u32> = Vec::with_capacity(self.roots.len());
        let mut heap = BinaryHeap::new();
        for &r in &self.roots {
            match open(r) {
                Some(o) => heap.push(o),
                None => kept.push(r),
            }
        }
        let mut size = self.roots.len();
        while let Some(Open { node, .. }) = heap.pop() {
            let children = self.children(node);
            if size - 1 + children.len() > params.max_nodes {
                kept.push(node);
                continue;
            }
            size += children.len() - 1;
            for &c in children {
                match open(c) {
                    Some(o) => heap.push(o),
                    None => kept.push(c),
                }
            }
        }
        let mut nodes = kept;
        nodes.sort_unstable();

        // Each leaf's node of the cut.
        let mut of = vec![u32::MAX; n];
        let mut stack = Vec::new();
        for (i, &v) in nodes.iter().enumerate() {
            stack.push(v);
            while let Some(u) = stack.pop() {
                if (u as usize) < n {
                    debug_assert_eq!(of[u as usize], u32::MAX, "a cut covers a node once");
                    of[u as usize] = i as u32;
                } else {
                    stack.extend(self.children(u));
                }
            }
        }
        debug_assert!(!of.contains(&u32::MAX), "a cut covers every node");

        let (mut cx, mut cy) = (Vec::with_capacity(nodes.len()), Vec::new());
        let (mut mass, mut members) = (Vec::with_capacity(nodes.len()), Vec::new());
        let (mut seen, mut seen_members, mut all_leaves) = (0usize, 0usize, true);
        for (i, &v) in nodes.iter().enumerate() {
            let (px, py) = at(v);
            cx.push(px);
            cy.push(py);
            members.push(self.members(v));
            mass.push(match (v as usize).checked_sub(n) {
                Some(g) => self.mass[g],
                None => 1.0 + adj.degree(v) as f32,
            });
            if in_view(v) {
                seen += 1;
                seen_members += members[i] as usize;
                all_leaves &= (v as usize) < n;
            }
        }
        let detail = all_leaves || seen_members <= params.detail_nodes;
        let mut cut = Cut {
            detail,
            nodes,
            x: cx,
            y: cy,
            mass,
            members,
            of,
            src: Vec::new(),
            dst: Vec::new(),
            weight: Vec::new(),
            total_edges: 0,
            in_view: seen,
            members_in_view: seen_members,
        };
        if !detail {
            self.merged_edges(&mut cut, params.max_edges);
        }
        cut
    }
}

/// A merged edge: its weight, when it was found, and its ends.
type Merged = (f32, u32, u32, u32);

/// Heaviest first, then in the order found: a total order.
fn heavier(a: &Merged, b: &Merged) -> Ordering {
    b.0.total_cmp(&a.0).then(a.1.cmp(&b.1))
}

impl Hierarchy {
    /// Number the leaves in the forest's order (a walk from each of the
    /// layout's own roots, a group's leaves together), and keep each
    /// edge of `adj` once, at its earlier end, as the later end's number:
    /// what [`Hierarchy::merged_edges`] reads. Four bytes an edge and
    /// eight a node.
    fn index_edges(&mut self, adj: &UndirectedCsr) {
        let n = self.n;
        let mut order = Vec::with_capacity(n);
        let mut stack: Vec<u32> = Vec::new();
        for &root in self.top.iter().chain(&self.orphans) {
            stack.push(root);
            while let Some(u) = stack.pop() {
                if (u as usize) < n {
                    order.push(u);
                } else {
                    stack.extend(self.children(u).iter().rev());
                }
            }
        }
        debug_assert_eq!(order.len(), n, "every node is under one root");
        let mut rank = vec![0u32; n];
        for (r, &u) in order.iter().enumerate() {
            rank[u as usize] = r as u32;
        }
        let mut offsets = Vec::with_capacity(n + 1);
        let mut later = Vec::with_capacity(adj.targets.len() / 2);
        offsets.push(0);
        for (r, &u) in order.iter().enumerate() {
            let start = later.len();
            later.extend(
                (adj.neighbors(u).iter())
                    .map(|&v| rank[v as usize])
                    .filter(|&q| q as usize > r),
            );
            later[start..].sort_unstable();
            offsets.push(later.len() as u32);
        }
        later.shrink_to_fit();
        self.rank = rank;
        self.later_offsets = offsets;
        self.later = later;
    }

    /// The edges between the nodes of `cut`, each weighted by the edges
    /// of the graph it stands for, the heaviest `most` of them into
    /// `cut`.
    ///
    /// The leaves under a node of the layout's hierarchy are numbered
    /// together ([`Hierarchy::index_edges`]), so the cut's nodes are runs
    /// of the leaves in that order, and an edge between two of them is
    /// kept at a leaf of the earlier run and leads past its end. One pass
    /// over the runs reads those edges alone, each once (a row is sorted:
    /// from its far end back to the run's), and counts them in a slot per
    /// node of the cut. Of the merged edges found so far only the
    /// heaviest `2 * most` are held.
    fn merged_edges(&self, cut: &mut Cut, most: usize) {
        let n = self.n;
        let groups = cut.nodes.len();
        // Each leaf's node of the cut, in the forest's order.
        let mut of = vec![0u32; n];
        for (u, &i) in cut.of.iter().enumerate() {
            of[self.rank[u] as usize] = i;
        }
        let mut slot = vec![0u32; groups];
        let mut touched: Vec<u32> = Vec::new();
        let mut best: Vec<Merged> = Vec::new();
        // Once `most` edges are held, the lightest of them: an edge found
        // later is among the heaviest only if it is heavier, since of two
        // as heavy the one found first comes first.
        let mut least = 0.0f32;
        let mut total = 0usize;
        let mut start = 0;
        while start < n {
            let i = of[start];
            let mut end = start + 1;
            while end < n && of[end] == i {
                end += 1;
            }
            for r in start..end {
                let row =
                    &self.later[self.later_offsets[r] as usize..self.later_offsets[r + 1] as usize];
                for &q in row.iter().rev() {
                    if (q as usize) < end {
                        break;
                    }
                    let j = of[q as usize];
                    // (Another run of the same node: what the hierarchy
                    // left out, grouped by place, has runs apart.)
                    if j != i {
                        let count = &mut slot[j as usize];
                        if *count == 0 {
                            touched.push(j);
                        }
                        *count += 1;
                    }
                }
            }
            for &j in &touched {
                let w = std::mem::take(&mut slot[j as usize]) as f32;
                if w > least {
                    best.push((w, total as u32, i.min(j), i.max(j)));
                }
                total += 1;
            }
            touched.clear();
            if best.len() >= 2 * most.max(1) {
                if most == 0 {
                    best.clear();
                    least = f32::INFINITY;
                } else {
                    best.select_nth_unstable_by(most - 1, heavier);
                    best.truncate(most);
                    least = best[most - 1].0;
                }
            }
            start = end;
        }
        best.sort_unstable_by(heavier);
        best.truncate(most);
        cut.total_edges = total;
        cut.src = best.iter().map(|e| e.2).collect();
        cut.dst = best.iter().map(|e| e.3).collect();
        cut.weight = best.iter().map(|e| e.0).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ursa_core::algo::MultilevelParams;

    fn adjacency(n: u32, src: &[u32], dst: &[u32]) -> UndirectedCsr {
        UndirectedCsr::from_edges(n as usize, src, dst)
    }

    /// A graph of `clusters` rings of `size` nodes, each ring joined to
    /// the next by one edge, and its layout, run to the end.
    fn clustered(clusters: u32, size: u32) -> (UndirectedCsr, Fa2Multilevel) {
        let n = clusters * size;
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for c in 0..clusters {
            for i in 0..size {
                src.push(c * size + i);
                dst.push(c * size + (i + 1) % size);
                // Chords, so a ring is a clump and not a line.
                src.push(c * size + i);
                dst.push(c * size + (i + 5) % size);
            }
            src.push(c * size);
            dst.push(((c + 1) % clusters) * size);
        }
        let adj = adjacency(n, &src, &dst);
        let mut layout = Fa2Multilevel::new_lean(&adj, MultilevelParams::default());
        layout.step(&adj, f64::INFINITY);
        (adj, layout)
    }

    fn placed(adj: &UndirectedCsr, layout: &Fa2Multilevel) -> Hierarchy {
        let mut h = Hierarchy::of_layout(layout, adj).unwrap();
        let (x, y) = layout.positions();
        h.place(adj, x, y);
        h
    }

    /// The whole drawing in view, at `ppu` pixels a unit.
    fn whole(ppu: f32) -> CutParams {
        CutParams {
            view: (-1e9, -1e9, 1e9, 1e9),
            pixels_per_unit: ppu,
            separation: SEPARATION,
            max_nodes: MAX_NODES,
            max_edges: MAX_EDGES,
            detail_nodes: 0,
        }
    }

    /// Every leaf is under exactly one node of the cut, and no node of
    /// the cut is under another.
    fn assert_antichain_cover(h: &Hierarchy, cut: &Cut) {
        let n = h.n_leaves();
        let mut under = vec![0u32; n];
        let mut in_cut = vec![false; h.n_nodes()];
        for &v in &cut.nodes {
            assert!(!std::mem::replace(&mut in_cut[v as usize], true));
        }
        for (i, &v) in cut.nodes.iter().enumerate() {
            let mut stack = vec![v];
            let mut leaves = 0;
            while let Some(u) = stack.pop() {
                // Nothing under a node of the cut is in the cut.
                assert!(u == v || !in_cut[u as usize], "{u} is under {v}");
                if (u as usize) < n {
                    under[u as usize] += 1;
                    assert_eq!(cut.of[u as usize], i as u32);
                    leaves += 1;
                }
                stack.extend(h.children(u));
            }
            assert_eq!(leaves, cut.members[i]);
        }
        assert!(under.iter().all(|&c| c == 1), "every node once");
        assert_eq!(cut.members.iter().map(|&m| m as usize).sum::<usize>(), n);
        assert!(cut.nodes.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn the_forest_is_the_layout_s_hierarchy() {
        let (adj, layout) = clustered(40, 30);
        let h = placed(&adj, &layout);
        assert_eq!(h.n_leaves(), 1200);
        assert_eq!(h.levels(), layout.coarse_levels() + 1);
        let sizes = layout.level_sizes();
        assert_eq!(h.n_nodes(), sizes.iter().sum::<usize>());
        // Every root's leaves, together, are the graph's nodes.
        let leaves: u32 = h.roots().iter().map(|&r| h.members(r)).sum();
        assert_eq!(leaves, 1200);
        // The coarsest level is all roots.
        let top = *sizes.last().unwrap();
        assert!(h.roots()[..top]
            .iter()
            .all(|&r| h.level_of(r) == Some(h.levels() - 1)));
        assert_eq!(h.level_of(0), Some(0));
        assert_eq!(h.level_of(1199), Some(0));
        assert_eq!(h.level_of(1200), Some(1));
        assert_eq!(h.n_layout_nodes(), h.n_nodes());
        // A group's box is the box around its leaves, and holds its centre.
        let (x, y) = layout.positions();
        for v in 1200..h.n_nodes() as u32 {
            let g = v as usize - 1200;
            let mut stack = vec![v];
            let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
            while let Some(u) = stack.pop() {
                if u < 1200 {
                    let (px, py) = (x[u as usize], y[u as usize]);
                    b = [b[0].min(px), b[1].min(py), b[2].max(px), b[3].max(py)];
                }
                stack.extend(h.children(u));
            }
            assert_eq!(h.bounds[g], b);
            assert!((b[0]..=b[2]).contains(&h.x[g]) && (b[1]..=b[3]).contains(&h.y[g]));
            assert!(h.spread[g] <= (b[2] - b[0]).hypot(b[3] - b[1]));
        }
    }

    #[test]
    fn a_cut_is_an_antichain_that_covers_every_node() {
        let (adj, layout) = clustered(40, 30);
        let h = placed(&adj, &layout);
        let (x, y) = layout.positions();
        let mut sizes = Vec::new();
        // From far out to far in: the roots, then finer, then the graph.
        for ppu in [1e-6, 0.01, 0.03, 0.1, 0.3, 1.0, 3.0, 10.0, 1e6] {
            let cut = h.cut(&adj, x, y, &whole(ppu));
            assert_antichain_cover(&h, &cut);
            sizes.push(cut.nodes.len());
            // The merged edges are the graph's, counted: between two
            // different nodes of the cut, heaviest first.
            if !cut.detail {
                let mut count = std::collections::BTreeMap::new();
                for u in 0..1200u32 {
                    for &v in adj.neighbors(u) {
                        let (a, b) = (cut.of[u as usize], cut.of[v as usize]);
                        if a < b {
                            *count.entry((a, b)).or_insert(0.0f32) += 1.0;
                        }
                    }
                }
                assert_eq!(cut.total_edges, count.len());
                assert_eq!(cut.src.len(), count.len());
                for i in 0..cut.src.len() {
                    assert_eq!(count[&(cut.src[i], cut.dst[i])], cut.weight[i]);
                }
                assert!(cut.weight.windows(2).all(|w| w[0] >= w[1]));
            }
        }
        assert_eq!(sizes[0], h.roots().len());
        assert!(sizes.windows(2).all(|w| w[0] <= w[1]), "{sizes:?}");
        assert_eq!(*sizes.last().unwrap(), 1200);
        // Fully zoomed in, the cut is the graph: every node its own.
        let cut = h.cut(&adj, x, y, &whole(1e6));
        assert!(cut.detail);
        assert_eq!(cut.nodes, (0..1200).collect::<Vec<u32>>());
        assert_eq!(cut.of, cut.nodes);
        assert_eq!((&cut.x[..], &cut.y[..]), (x, y));
        assert!(cut.src.is_empty());
        // Zoomed out it is not.
        assert!(!h.cut(&adj, x, y, &whole(1e-6)).detail);
    }

    #[test]
    fn a_cut_is_the_same_every_time() {
        let (adj, layout) = clustered(40, 30);
        let (adj2, layout2) = clustered(40, 30);
        let (h, h2) = (placed(&adj, &layout), placed(&adj2, &layout2));
        let (x, y) = layout.positions();
        for ppu in [0.01, 0.1, 1.0] {
            let a = h.cut(&adj, x, y, &whole(ppu));
            assert_eq!(a, h.cut(&adj, x, y, &whole(ppu)));
            assert_eq!(a, h2.cut(&adj2, x, y, &whole(ppu)));
        }
    }

    #[test]
    fn only_the_groups_in_view_are_opened() {
        let (adj, layout) = clustered(40, 30);
        let h = placed(&adj, &layout);
        let (x, y) = layout.positions();
        // A small window on node 0, zoomed far in.
        let (cx, cy) = (x[0], y[0]);
        let mut params = whole(1e4);
        params.view = (cx - 1.0, cy - 1.0, cx + 1.0, cy + 1.0);
        let cut = h.cut(&adj, x, y, &params);
        assert_antichain_cover(&h, &cut);
        // Node 0 is drawn as itself; far groups are still groups.
        assert_eq!(cut.nodes[cut.of[0] as usize], 0);
        assert!(cut.nodes.len() < 600, "{}", cut.nodes.len());
        assert!(cut.nodes.iter().any(|&v| v >= 1200));
        // Everything in view is a node of the graph: the graph is drawn.
        assert!(cut.detail && cut.in_view >= 1);
        assert_eq!(cut.in_view, cut.members_in_view);
        // With few enough nodes under the groups in view, too.
        let mut loose = whole(1e-6);
        loose.detail_nodes = 1200;
        assert!(h.cut(&adj, x, y, &loose).detail);
        loose.detail_nodes = 1199;
        assert!(!h.cut(&adj, x, y, &loose).detail);
    }

    #[test]
    fn a_cut_keeps_to_its_limits() {
        let (adj, layout) = clustered(40, 30);
        let h = placed(&adj, &layout);
        let (x, y) = layout.positions();
        let mut params = whole(1e6);
        params.max_nodes = 100;
        params.max_edges = 7;
        let cut = h.cut(&adj, x, y, &params);
        assert_antichain_cover(&h, &cut);
        assert!(cut.nodes.len() <= 100, "{}", cut.nodes.len());
        assert!(cut.nodes.len() > h.roots().len());
        assert!(!cut.detail);
        assert_eq!(cut.src.len(), 7);
        assert!(cut.total_edges > 7);
        // The seven are the heaviest of all of them.
        params.max_edges = usize::MAX / 4;
        let all = h.cut(&adj, x, y, &params);
        assert_eq!(all.src.len(), all.total_edges);
        assert_eq!(
            (&cut.src[..], &cut.dst[..], &cut.weight[..]),
            (&all.src[..7], &all.dst[..7], &all.weight[..7])
        );
    }

    #[test]
    fn nodes_that_left_the_hierarchy_are_roots() {
        // Two triangles and two isolated nodes: no coarse level joins
        // them, and every node is under some root.
        let adj = adjacency(8, &[0, 1, 2, 3, 4, 5], &[1, 2, 0, 4, 5, 3]);
        let levels: [(&[u32], &[u32]); 2] = [
            (&[0, 3, 6], &[0, 1, 2, 3, 4, 5]),
            // The level above has one node, of the first triangle alone.
            (&[0, 1], &[0]),
        ];
        let mut h = Hierarchy::new(&adj, &levels);
        assert_eq!(h.roots(), &[10, 9, 6, 7]);
        assert_eq!((h.members(10), h.members(9), h.members(6)), (3, 3, 1));
        let x = [0.0, 2.0, 1.0, 10.0, 12.0, 11.0, 50.0, 60.0];
        let y = [0.0, 0.0, 2.0, 0.0, 0.0, 2.0, 0.0, 0.0];
        h.place(&adj, &x, &y);
        let cut = h.cut(&adj, &x, &y, &whole(1e-3));
        assert_antichain_cover(&h, &cut);
        assert_eq!(cut.nodes, vec![6, 7, 9, 10]);
        assert_eq!(cut.members, vec![1, 1, 3, 3]);
        assert_eq!(cut.mass, vec![1.0, 1.0, 9.0, 9.0]);
        // The centre of an even triangle.
        assert!((cut.x[3] - 1.0).abs() < 1e-6 && (cut.y[3] - 2.0 / 3.0).abs() < 1e-6);
        assert_eq!(cut.total_edges, 0);
    }

    #[test]
    fn many_nodes_with_no_group_are_grouped_by_place() {
        // A triangle under one group, and an 8 x 8 grid of nodes with no
        // edge, 100 apart: 64 nodes the hierarchy left out.
        const N: u32 = 67;
        let adj = adjacency(N, &[0, 1, 2], &[1, 2, 0]);
        let levels: [(&[u32], &[u32]); 1] = [(&[0, 3], &[0, 1, 2])];
        let mut h = Hierarchy::new(&adj, &levels);
        assert_eq!(h.roots().len(), 65);
        let (mut x, mut y) = (vec![0.0, 2.0, 1.0], vec![0.0, 0.0, 2.0]);
        for i in 0..64 {
            // Not in the grid's order: by a stride that visits each once.
            let j = (i * 37) % 64;
            x.push(1000.0 + 100.0 * (j % 8) as f32);
            y.push(1000.0 + 100.0 * (j / 8) as f32);
        }
        h.place(&adj, &x, &y);
        // The triangle's group, and at most FAN groups of the rest.
        assert_eq!(h.roots()[0], N);
        assert_eq!(h.roots().len(), 1 + FAN);
        assert_eq!(h.roots().iter().map(|&r| h.members(r)).sum::<u32>(), N);
        assert!(h.n_nodes() > h.n_layout_nodes());
        assert_eq!(h.level_of(h.n_nodes() as u32 - 1), None);
        // The first groups are of neighbours: the four nodes of a cell
        // of the grid two by two, each 71 from its middle.
        let first = h.n_layout_nodes() - N as usize;
        for g in first..first + 16 {
            assert_eq!(h.children_of(g).len(), FAN);
            let [x0, y0, x1, y1] = h.bounds[g];
            assert_eq!((x1 - x0, y1 - y0), (100.0, 100.0));
            assert!((h.spread[g] - 5000f32.sqrt()).abs() < 1e-3);
        }
        // Zoomed out they are a few discs; zoomed in, every node.
        let far = h.cut(&adj, &x, &y, &whole(1e-4));
        assert_antichain_cover(&h, &far);
        assert_eq!(far.nodes.len(), h.roots().len());
        let near = h.cut(&adj, &x, &y, &whole(1e4));
        assert_antichain_cover(&h, &near);
        assert_eq!(near.nodes.len(), N as usize);
        // Placed again, the same groups, and no more of them.
        let nodes = h.n_nodes();
        let again = {
            h.place(&adj, &x, &y);
            h.cut(&adj, &x, &y, &whole(1e-4))
        };
        assert_eq!((h.n_nodes(), &again), (nodes, &far));
        // Moved, they are grouped where they now are.
        x[3..].reverse();
        h.place(&adj, &x, &y);
        assert_antichain_cover(&h, &h.cut(&adj, &x, &y, &whole(1e-2)));
    }
}
