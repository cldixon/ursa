//! Coarse-to-fine ForceAtlas2: lay out a coarsened graph, then refine.
//!
//! Flat ForceAtlas2 moves each node by its local forces, so information
//! crosses the drawing one neighbourhood per iteration: a large graph needs
//! hundreds of iterations, and a grid can settle folded, because no local move
//! unfolds it. The multilevel scheme of sfdp and OpenOrd fixes both. The graph
//! is coarsened, level by level, by merging nodes, down to a few dozen nodes,
//! which converge in a few hundred near-free iterations into the drawing's
//! global shape. Each finer level then starts from the shape of the one above,
//! every node at its parent's place, and needs far fewer iterations to settle
//! its own detail.
//!
//! Why a few dozen and not the few thousand of a single coarse step: every
//! level settles structure about twice the size of the last, and a small
//! level costs next to nothing. Stopping at 2,000 nodes left the 10,000-node
//! grid folded (fold measure 0.58, against 0.98 stopping at 50) and the 16K
//! R-MAT graph's layout worse, for more time, since a 2,000-node level needs
//! hundreds of iterations of its own (`examples/fa2_multilevel.rs`).
//!
//! # Coarsening
//!
//! One level merges nodes in three passes, all in index order and serial, so
//! the hierarchy is a pure function of the graph:
//!
//! 1. **Heavy-edge matching.** Each node not yet merged pairs with the
//!    unmerged neighbour of greatest `w / (mᵢ·mⱼ)`: the edge weight `w` (the
//!    number of input edges it stands for) over the two nodes' ForceAtlas2
//!    masses. The normalisation prefers light nodes, so merged nodes grow
//!    evenly instead of a growing node swallowing its neighbourhood level after
//!    level. Ties go to the lowest index.
//! 2. **Hub absorption.** Matching stalls on stars and power-law hubs: a hub
//!    pairs with one leaf, and its other leaves have no unmerged neighbour
//!    left. A matching is maximal, so every node still unmerged after pass 1
//!    has only merged neighbours; it joins the group of the one it is most
//!    strongly tied to, by the same score. A star collapses to one node in one
//!    level. Every node with a neighbour is now in a group of two or more, so a
//!    level at least halves the connected part of the graph.
//! 3. **Isolated nodes** (and, at coarse levels, whole components merged into
//!    one node) leave the hierarchy. They feel only gravity and the repulsion
//!    of everything else, which in two dimensions is a 1/d force: a ring of
//!    them exerts none on what lies inside it, so the rest of the layout is
//!    the same without them. Merging them, in pairs, would carry clumps of
//!    `2ˡ` down to the input graph, which a few iterations cannot spread.
//!    They come back on a ring around the drawing ([`ring_of`]).
//!
//! A merged node's mass is the sum of its members' masses (`deg + 1` each, as
//! [`Fa2State`] weighs them), and an edge between two merged nodes weighs as
//! many input edges as it stands for: summed repulsion and summed linear
//! attraction, so a coarse layout has about the forces, and so the scale, of
//! the finer one it stands for. Coarsening stops at
//! [`MultilevelParams::coarsest_nodes`] nodes, or when a level would shrink the
//! graph by less than a fifth.
//!
//! # Prolongation
//!
//! When a level has had its iterations, each of its nodes hands its position
//! to its members, spread on a small phyllotactic spiral around it (radius
//! growing with the square root of the member count, scaled to the level's
//! mean edge length) so no two start coincident. The spiral's directions are built by
//! repeated rotation through the golden angle with constants written out, so
//! prolongation uses nothing but `+ - * /` and `sqrt`, as the rest of the
//! layout does.
//!
//! # Determinism
//!
//! As in [`super::layout`]: the hierarchy is built serially in index order,
//! the coarse adjacency is gathered per coarse node, every level is stepped by
//! [`Fa2State`], and the iteration schedule is fixed in advance. How the work
//! is split into [`Fa2Multilevel::step`] calls changes only when positions are
//! looked at, never what they are at the end.

use crate::parallel::*;

use super::layout::{center, seed_positions, Fa2Params, Fa2State, Positions};
use super::rng::DEFAULT_SEED;
use super::triangle::undirected_view;
use crate::topology::{EdgeMask, Topology, UndirectedCsr};

/// Parameters for [`Fa2Multilevel`] and [`layout_fa2_multilevel`].
///
/// The iteration schedule gives the input graph `finest_work / size`
/// iterations, `size` being its nodes plus adjacency entries (what an
/// iteration costs), clamped to `min_iterations..=max_iterations`; each level
/// up gets `level_growth` times as many as the one below, up to
/// `max_iterations`; the coarsest gets `coarsest_iterations`. So a small graph
/// refines for as long as flat ForceAtlas2 would run, and a large one spends
/// about the same time on its finest level as a mid-sized one does, its global
/// shape having been settled by the levels above.
#[derive(Debug, Clone, Copy)]
pub struct MultilevelParams {
    /// The force law, as for [`super::layout_fa2`]; `fa2.iterations` is not
    /// used, the schedule says how many each level gets.
    pub fa2: Fa2Params,
    /// Stop coarsening once a level has at most this many nodes.
    pub coarsest_nodes: usize,
    /// Iterations on the coarsest level, which sets the drawing's global
    /// shape. Cheap: the coarsest level is tiny.
    pub coarsest_iterations: u32,
    /// Work for the input graph, in adjacency entries plus nodes per
    /// iteration, times iterations.
    pub finest_work: f64,
    /// Bounds on the iterations of every level but the coarsest.
    pub min_iterations: u32,
    pub max_iterations: u32,
    /// How many times as many iterations a level gets as the one below it.
    pub level_growth: f64,
}

impl Default for MultilevelParams {
    fn default() -> Self {
        MultilevelParams {
            // theta 1.0 rather than 0.5: an iteration of the 1M-edge graph
            // takes 2.6 times less, and the layout is as good (measured by
            // `examples/fa2_multilevel.rs`), because the levels above have
            // already placed the far field the approximation blurs.
            fa2: Fa2Params {
                theta: 1.0,
                ..Fa2Params::default()
            },
            coarsest_nodes: 50,
            coarsest_iterations: 300,
            finest_work: 3.0e7,
            min_iterations: 10,
            max_iterations: 200,
            level_growth: 1.5,
        }
    }
}

impl MultilevelParams {
    /// Iterations per level, the input graph first, for levels of `sizes`
    /// (nodes plus adjacency entries each).
    fn schedule(&self, sizes: &[f64]) -> Vec<u32> {
        let (lo, hi) = (
            self.min_iterations,
            self.max_iterations.max(self.min_iterations),
        );
        let last = sizes.len() - 1;
        let finest = (self.finest_work / sizes[0].max(1.0)).round();
        let finest = finest.clamp(f64::from(lo), f64::from(hi));
        (0..sizes.len())
            .map(|l| {
                if l == last {
                    self.coarsest_iterations
                } else {
                    let it = (finest * self.level_growth.powi(l as i32)).round();
                    it.min(f64::from(hi)) as u32
                }
            })
            .collect()
    }
}

/// One coarsened level: its graph, and how its nodes group the level below.
#[derive(Debug)]
struct Coarse {
    adj: UndirectedCsr,
    /// Input edges each adjacency entry stands for.
    weights: Vec<f32>,
    /// Summed ForceAtlas2 mass of each node's members.
    mass: Vec<f32>,
    /// The members (nodes of the level below) of each node, as a CSR: node
    /// `c`'s are `members[member_offsets[c]..member_offsets[c + 1]]`, in index
    /// order.
    member_offsets: Vec<u32>,
    members: Vec<u32>,
    /// Nodes of the level below with no neighbour there, in index order:
    /// isolated nodes, and at coarse levels whole components merged into
    /// one node. They leave the hierarchy, and come back on a ring around
    /// the drawing when it is prolonged (see [`ring_of`]).
    dropped: Vec<u32>,
    /// Node count of the level below.
    fine_n: usize,
    /// This level's node count and adjacency entries: kept when the level
    /// itself is let go ([`Coarse::release`]).
    n: usize,
    entries: usize,
}

impl Coarse {
    fn n(&self) -> usize {
        self.n
    }

    /// Whether the weights are not held now: a lean hierarchy's
    /// ([`Fa2Multilevel::new_lean`]) are made again when the level is
    /// reached.
    fn lacks_weights(&self) -> bool {
        self.weights.len() != self.adj.targets.len()
    }

    /// The group of each node of the level below, [`NONE`] for a dropped
    /// one: what [`group_nodes`] gave, read back from the members.
    fn groups(&self) -> Vec<u32> {
        let mut group = vec![NONE; self.fine_n];
        for c in 0..self.n {
            for &u in self.members(c) {
                group[u as usize] = c as u32;
            }
        }
        group
    }

    /// Let go of everything but the counts and the members: the layout is
    /// past this level, and nothing lays it out again. The members stay
    /// (four bytes for each node of this level and of the one below): they
    /// are the hierarchy itself, which a cut through it reads after the
    /// layout is done ([`Fa2Multilevel::level_members`]).
    fn release(&mut self) {
        self.adj = UndirectedCsr {
            offsets: Vec::new(),
            targets: Vec::new(),
        };
        self.weights = Vec::new();
        self.mass = Vec::new();
        self.dropped = Vec::new();
    }

    fn members(&self, c: usize) -> &[u32] {
        &self.members[self.member_offsets[c] as usize..self.member_offsets[c + 1] as usize]
    }
}

pub(super) const NONE: u32 = u32::MAX;

/// Group the nodes of one level: `group[i]` per node, and the group count.
/// The three passes of the module docs.
fn group_nodes(adj: &UndirectedCsr, weights: Option<&[f32]>, mass: &[f32]) -> (Vec<u32>, usize) {
    let n = mass.len();
    let w = |e: usize| weights.map_or(1.0, |w| w[e]);
    let mut group = vec![NONE; n];
    let mut groups = 0u32;

    // The neighbour of `u` with the greatest normalised weight among those
    // `eligible`, ties to the lowest index (neighbours are sorted).
    let best = |u: usize, group: &[u32], eligible: &dyn Fn(u32) -> bool| -> Option<usize> {
        let base = adj.offsets[u] as usize;
        let mut best: Option<(usize, f32)> = None;
        for (e, &v) in adj.neighbors(u as u32).iter().enumerate() {
            let v = v as usize;
            if v == u || !eligible(group[v]) {
                continue;
            }
            let score = w(base + e) / (mass[u] * mass[v]);
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((v, score));
            }
        }
        best.map(|(v, _)| v)
    };

    // 1. Heavy-edge matching.
    for u in 0..n {
        if group[u] != NONE {
            continue;
        }
        if let Some(v) = best(u, &group, &|g| g == NONE) {
            group[u] = groups;
            group[v] = groups;
            groups += 1;
        }
    }
    // 2. Hub absorption: whatever is left with a neighbour joins its
    // strongest-tied neighbour's group. After a maximal matching every such
    // neighbour is grouped; the fallback is for safety only.
    for u in 0..n {
        if group[u] != NONE || adj.degree(u as u32) == 0 {
            continue;
        }
        match best(u, &group, &|g| g != NONE) {
            Some(v) => group[u] = group[v],
            None => {
                group[u] = groups;
                groups += 1;
            }
        }
    }
    // 3. Isolated nodes stay ungrouped: they leave the hierarchy here
    // (see `Coarse::dropped`).
    (group, groups as usize)
}

/// The coarse graph of one level grouped by `group`.
fn contract(
    adj: &UndirectedCsr,
    weights: Option<&[f32]>,
    mass: &[f32],
    group: &[u32],
    nc: usize,
) -> Coarse {
    let n = mass.len();
    // Members per group, by a stable counting sort: index order within each.
    // Ungrouped (isolated) nodes are dropped, in index order.
    let mut member_offsets = vec![0u32; nc + 1];
    let mut dropped = Vec::new();
    for (i, &g) in group.iter().enumerate() {
        if g == NONE {
            dropped.push(i as u32);
        } else {
            member_offsets[g as usize + 1] += 1;
        }
    }
    for c in 0..nc {
        member_offsets[c + 1] += member_offsets[c];
    }
    let mut fill = member_offsets.clone();
    let mut members = vec![0u32; n - dropped.len()];
    for (i, &g) in group.iter().enumerate() {
        if g != NONE {
            members[fill[g as usize] as usize] = i as u32;
            fill[g as usize] += 1;
        }
    }

    let rows = coarse_rows(adj, weights, mass, group, &member_offsets, &members, None);
    Coarse {
        n: nc,
        entries: rows.targets.len(),
        adj: UndirectedCsr {
            offsets: rows.offsets,
            targets: rows.targets,
        },
        weights: rows.weights,
        mass: rows.mass,
        member_offsets,
        members,
        dropped,
        fine_n: n,
    }
}

/// The adjacency, weights and masses of a coarse level.
struct Rows {
    offsets: Vec<u64>,
    targets: Vec<u32>,
    weights: Vec<f32>,
    mass: Vec<f32>,
}

/// How many adjacency entries of the level below go into the rows gathered
/// before they are appended (a run of coarse nodes; at least one): the rows
/// of a run are held twice for a moment, the whole level never. Counted in
/// entries, not nodes: a dense level has few nodes and long rows.
const ROW_RUN: usize = 1 << 17;

/// The rows of the coarse level whose node `c` has the members
/// `members[member_offsets[c]..member_offsets[c + 1]]` of the level below
/// (`adj`, `weights`, `mass`, each node in `group[i]`).
///
/// Each coarse node gathers its own row: its members' edges, mapped to
/// groups, merged by target. Sums of edge counts are whole numbers, exact
/// in f32 well past any graph this lays out, and run in a fixed order
/// anyway (members in index order, a stable sort). The rows are gathered a
/// run of nodes at a time, in parallel within a run, and appended in order,
/// so the result does not depend on the run's length or the thread count.
///
/// `weights_only` is for a level made before, of that many entries: only
/// [`Rows::weights`] is filled, which is all a lean hierarchy let go of.
fn coarse_rows(
    adj: &UndirectedCsr,
    weights: Option<&[f32]>,
    mass: &[f32],
    group: &[u32],
    member_offsets: &[u32],
    members: &[u32],
    weights_only: Option<usize>,
) -> Rows {
    let nc = member_offsets.len() - 1;
    // No more entries than the level below has; or exactly as many as the
    // level had when it was first made.
    let most = weights_only.unwrap_or(adj.targets.len());
    let all = weights_only.is_none();
    let mut offsets = Vec::with_capacity(if all { nc + 1 } else { 0 });
    let mut targets: Vec<u32> = Vec::with_capacity(if all { most } else { 0 });
    let mut weights_out: Vec<f32> = Vec::with_capacity(most);
    let mut mass_out = Vec::with_capacity(if all { nc } else { 0 });
    if all {
        offsets.push(0u64);
    }
    let mut start = 0;
    while start < nc {
        let mut end = start;
        let mut entries = 0;
        while end < nc && (end == start || entries < ROW_RUN) {
            for &u in &members[member_offsets[end] as usize..member_offsets[end + 1] as usize] {
                entries += adj.degree(u);
            }
            end += 1;
        }
        let rows: Vec<(Vec<u32>, Vec<f32>, f32)> = (start..end)
            .into_par_iter()
            .map_init(
                || {
                    let words = nc.div_ceil(64);
                    (
                        vec![0.0f32; nc],
                        vec![0u64; words],
                        vec![0u64; words.div_ceil(64)],
                        Vec::<u32>::new(),
                    )
                },
                |(sum, met, any, found), c| {
                    // Each target's weights are added in the order its
                    // edges are met (members in index order, each one's
                    // neighbours in theirs), into a slot of its own: the
                    // sums a stable sort by target and a merge would give,
                    // without sorting the edges. `met` has a bit for each
                    // target met, `any` a bit for each of its words that has
                    // one, and `found` the targets as they were met.
                    let mut m = 0.0f32;
                    found.clear();
                    for &u in &members[member_offsets[c] as usize..member_offsets[c + 1] as usize] {
                        m += mass[u as usize];
                        let base = adj.offsets[u as usize] as usize;
                        for (e, &v) in adj.neighbors(u).iter().enumerate() {
                            // A grouped node's neighbours are all grouped: only
                            // isolated nodes are dropped.
                            let gv = group[v as usize] as usize;
                            if gv != c {
                                let w = weights.map_or(1.0, |w| w[base + e]);
                                let (word, bit) = (gv / 64, 1u64 << (gv % 64));
                                if met[word] & bit != 0 {
                                    sum[gv] += w;
                                } else {
                                    met[word] |= bit;
                                    any[word / 64] |= 1 << (word % 64);
                                    sum[gv] = w;
                                    found.push(gv as u32);
                                }
                            }
                        }
                    }
                    // The targets in ascending order. They are read off
                    // the bits, which costs the words the row touched and
                    // a scan of `any`; a row with too few targets for that
                    // scan is sorted. The order is the same either way.
                    let mut targets = Vec::with_capacity(found.len());
                    if found.len() * 8 >= any.len() {
                        for (top, words) in any.iter_mut().enumerate() {
                            let mut words = std::mem::take(words);
                            while words != 0 {
                                let word = top * 64 + words.trailing_zeros() as usize;
                                words &= words - 1;
                                let mut bits = std::mem::take(&mut met[word]);
                                while bits != 0 {
                                    targets.push((word * 64) as u32 + bits.trailing_zeros());
                                    bits &= bits - 1;
                                }
                            }
                        }
                    } else {
                        targets.extend_from_slice(found);
                        targets.sort_unstable();
                        for &t in &targets {
                            let word = t as usize / 64;
                            met[word] = 0;
                            any[word / 64] = 0;
                        }
                    }
                    let ws: Vec<f32> = targets.iter().map(|&t: &u32| sum[t as usize]).collect();
                    (targets, ws, m)
                },
            )
            .collect();
        for (t, w, m) in rows {
            weights_out.extend_from_slice(&w);
            if all {
                targets.extend_from_slice(&t);
                offsets.push(targets.len() as u64);
                mass_out.push(m);
            }
        }
        start = end;
    }
    targets.shrink_to_fit();
    weights_out.shrink_to_fit();
    Rows {
        offsets,
        targets,
        weights: weights_out,
        mass: mass_out,
    }
}

/// ForceAtlas2's mass for each node of `adj`: `deg + 1`, as [`Fa2State`]
/// gives it.
pub(super) fn degree_mass(adj: &UndirectedCsr) -> Vec<f32> {
    let n = adj.offsets.len().saturating_sub(1);
    (0..n).map(|i| 1.0 + adj.degree(i as u32) as f32).collect()
}

/// A lean hierarchy holds a level's weights only while that level is laid
/// out, if the level has more than this share of the input graph's
/// adjacency entries: a dense core that grouping does not thin. Levels that
/// shrink, as most graphs' do, keep theirs.
const LEAN_SHARE: f64 = 0.75;

/// The coarsened levels of `adj`, finest first: `coarsen(adj)[0]` groups the
/// nodes of `adj` itself. `lean` lets go of the weights of the levels that
/// are nearly as large as `adj`, each once the level above it is made of
/// them ([`Fa2Multilevel::new_lean`]).
fn coarsen(adj: &UndirectedCsr, coarsest_nodes: usize, lean: bool) -> Vec<Coarse> {
    let mut levels: Vec<Coarse> = Vec::new();
    let base_mass = degree_mass(adj);
    loop {
        let (a, w, m) = match levels.last() {
            None => (adj, None, &base_mass[..]),
            Some(l) => (&l.adj, Some(&l.weights[..]), &l.mass[..]),
        };
        let n = m.len();
        if n <= coarsest_nodes.max(1) {
            break;
        }
        let (group, nc) = group_nodes(a, w, m);
        if nc == 0 {
            break;
        }
        // Shrinking by less than a fifth is not worth a level.
        if nc * 5 > n * 4 {
            break;
        }
        let next = contract(a, w, m, &group, nc);
        // The level below `next` has given all it had to give until the
        // layout comes down to it. The coarsest keeps its weights: it is
        // laid out first.
        if lean {
            if let Some(below) = levels.last_mut() {
                if below.entries as f64 > LEAN_SHARE * adj.targets.len() as f64 {
                    below.weights = Vec::new();
                }
            }
        }
        levels.push(next);
    }
    levels
}

/// Golden-angle rotation, `cos` and `sin` of `π(3 − √5)`, written out so the
/// spiral is the same bits on every platform (no libm).
const GOLDEN_COS: f32 = -0.737_368_9;
const GOLDEN_SIN: f32 = 0.675_490_3;

/// Positions for the level below `coarse` from `(x, y)`, its own: each member
/// at its group's place, spread on a spiral of radius `spread · √members`.
fn prolong(coarse: &Coarse, x: &[f32], y: &[f32], spread: f32) -> Positions {
    let fine_n = coarse.fine_n;
    let mut fx = vec![0.0f32; fine_n];
    let mut fy = vec![0.0f32; fine_n];
    // A start direction per group, turning by the golden angle from one group
    // to the next, so pairs do not all split along one axis.
    let (mut cx, mut cy) = (1.0f32, 0.0f32);
    for c in 0..coarse.n() {
        let members = coarse.members(c);
        let s = members.len();
        if s == 1 {
            fx[members[0] as usize] = x[c];
            fy[members[0] as usize] = y[c];
        } else {
            let radius = spread * (s as f32).sqrt();
            let (mut dx, mut dy) = (cx, cy);
            for (k, &m) in members.iter().enumerate() {
                let r = radius * ((k as f32 + 0.5) / s as f32).sqrt();
                fx[m as usize] = x[c] + r * dx;
                fy[m as usize] = y[c] + r * dy;
                (dx, dy) = rotate(dx, dy);
            }
        }
        (cx, cy) = rotate(cx, cy);
    }
    // Dropped nodes go on a ring around the drawing, by the golden angle so
    // any run of them is spread evenly, and a little in and out of it (by a
    // hash of the index, unrelated to the angle) so they do not start in
    // single file.
    if !coarse.dropped.is_empty() {
        let (ox, oy, ring) = ring_of(x, y);
        let (mut dx, mut dy) = (1.0f32, 0.0f32);
        for &m in &coarse.dropped {
            let phase = (m.wrapping_mul(0x9E37_79B1) >> 8) as f32 / 16_777_216.0;
            let r = ring * (0.95 + 0.1 * phase);
            fx[m as usize] = ox + r * dx;
            fy[m as usize] = oy + r * dy;
            (dx, dy) = rotate(dx, dy);
        }
    }
    (fx, fy)
}

/// The ring dropped nodes start on: centre and radius, `RING` times the
/// root-mean-square distance of the level's nodes from their centroid,
/// summed in index order.
///
/// Not where ForceAtlas2 would settle them. A node with no neighbours feels
/// gravity `g` inwards and, in two dimensions, `k·M/r` outwards from the
/// connected mass `M` inside its radius `r`, so it settles at `r = k·M/g`:
/// 190,000 units for the 16K-node R-MAT graph, whose connected part is 600
/// across. Flat ForceAtlas2 never gets there either (its ring is at 3,000
/// after 300 iterations, 6,600 after 1,000, still drifting out); a ring just
/// outside the drawing keeps the connected part, the part worth looking at,
/// in view. The connected part does not feel the ring: a ring's 1/d forces
/// cancel inside it, which is also why dropping these nodes from the coarse
/// levels leaves the rest of the layout as it would have been.
pub(super) fn ring_of(x: &[f32], y: &[f32]) -> (f32, f32, f32) {
    let n = x.len().max(1) as f32;
    let (mut sx, mut sy) = (0.0f32, 0.0f32);
    for i in 0..x.len() {
        sx += x[i];
        sy += y[i];
    }
    let (ox, oy) = (sx / n, sy / n);
    let mut s2 = 0.0f32;
    for i in 0..x.len() {
        s2 += (x[i] - ox) * (x[i] - ox) + (y[i] - oy) * (y[i] - oy);
    }
    (ox, oy, RING * (s2 / n).sqrt().max(1.0))
}

/// The dropped nodes' ring radius, in root-mean-square radii of the rest.
const RING: f32 = 3.0;

/// `(x, y)` turned by the golden angle, renormalised so the error of repeated
/// turns does not build up.
#[inline]
pub(super) fn rotate(x: f32, y: f32) -> (f32, f32) {
    let (rx, ry) = (
        x * GOLDEN_COS - y * GOLDEN_SIN,
        x * GOLDEN_SIN + y * GOLDEN_COS,
    );
    let norm = (rx * rx + ry * ry).sqrt();
    (rx / norm, ry / norm)
}

/// Mean edge length of a level, over a strided sample of its nodes, summed
/// in index order. The unit the
/// prolongation spiral is scaled to.
fn mean_edge_length(adj: &UndirectedCsr, x: &[f32], y: &[f32]) -> f32 {
    let n = x.len();
    // At most about 4096 nodes' edges, evenly strided: a scale, not a
    // statistic, and the view prolongs every level after every batch.
    let stride = n.div_ceil(4096).max(1);
    let (mut sum, mut count) = (0.0f64, 0u64);
    for i in (0..n).step_by(stride) {
        for &j in adj.neighbors(i as u32) {
            let j = j as usize;
            let (dx, dy) = (x[i] - x[j], y[i] - y[j]);
            sum += f64::from((dx * dx + dy * dy).sqrt());
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64) as f32
    }
}

/// The share of a level's spiral radius, per unit of mean edge length.
const SPREAD: f32 = 0.2;

/// Coarse-to-fine ForceAtlas2, one budget at a time, for a view that draws the
/// layout as it converges.
///
/// [`Self::new`] coarsens the graph ([module docs](self)) and seeds the
/// coarsest level; [`Self::step`] spends a budget of work, measured in
/// iterations of the full graph, on as many iterations as it buys at the
/// current level (an iteration on a level of a tenth the size costs about a
/// tenth), moving to the next finer level when one is done.
/// [`Self::positions`] has a position for every input node from the start:
/// the current level's, prolonged down to the input graph.
///
/// The schedule is fixed by [`MultilevelParams`], so the final positions do
/// not depend on how the work was split into steps, nor on the thread count;
/// [`layout_fa2_multilevel`] gives the same bits in one call. A graph that is
/// already no larger than [`MultilevelParams::coarsest_nodes`] has no coarse
/// levels, and is exactly [`super::layout_fa2`] with `coarsest_iterations`
/// and the same [`Fa2Params`].
#[derive(Debug)]
pub struct Fa2Multilevel {
    params: MultilevelParams,
    /// `coarse[l]` is level `l + 1`; level 0 is the input graph.
    coarse: Vec<Coarse>,
    /// The level being laid out now.
    level: usize,
    state: Fa2State,
    /// Iterations to run at each level, input graph first.
    schedule: Vec<u32>,
    /// What one iteration at each level costs, in input-graph iterations.
    cost: Vec<f64>,
    /// Budget given and not yet spent.
    credit: f64,
    /// Work done so far, in input-graph iterations.
    work: f64,
    /// The positions of the input graph's nodes now.
    x: Vec<f32>,
    y: Vec<f32>,
    /// `x` and `y` are behind the level being laid out
    /// ([`Self::step_level`] leaves them so): [`Self::prolong`] brings
    /// them up to date.
    stale: bool,
}

impl Fa2Multilevel {
    /// The hierarchy of `adj`, and seeded positions for its coarsest level.
    pub fn new(adj: &UndirectedCsr, params: MultilevelParams) -> Self {
        Self::build(adj, params, false)
    }

    /// [`Self::new`], holding less: for a layout that shares a small memory
    /// with what it lays out (a browser's worker). Every position is the
    /// same, bit for bit, at every step.
    ///
    /// A graph with a dense core has levels that grouping does not thin:
    /// the 1M-edge R-MAT graph's first four each have 70 to 95% of its
    /// adjacency entries, and their weights are half of what the hierarchy
    /// holds. A lean hierarchy lets go of the weights of each such level
    /// (one with more than three quarters of the input's entries) once the
    /// level above it is built, and makes them again, from the input graph
    /// up, when the layout comes down to that level. That costs one more
    /// contraction for each level between, which is paid during the
    /// layout, and nothing for a graph whose levels shrink.
    pub fn new_lean(adj: &UndirectedCsr, params: MultilevelParams) -> Self {
        Self::build(adj, params, true)
    }

    fn build(adj: &UndirectedCsr, params: MultilevelParams, lean: bool) -> Self {
        let coarse = coarsen(adj, params.coarsest_nodes, lean);
        let levels = coarse.len();
        // An iteration's cost is about its nodes plus its adjacency entries.
        let sizes: Vec<f64> = (0..=levels)
            .map(|l| {
                if l == 0 {
                    (adj.offsets.len() - 1 + adj.targets.len()) as f64
                } else {
                    let c = &coarse[l - 1];
                    (c.n() + c.entries) as f64
                }
            })
            .collect();
        let schedule = params.schedule(&sizes);
        let full = sizes[0].max(1.0);
        let cost: Vec<f64> = sizes.iter().map(|s| s / full).collect();

        let state = if levels == 0 {
            Fa2State::new(adj, params.fa2)
        } else {
            let top = &coarse[levels - 1];
            let k = params.fa2.k.max(1e-4);
            let (x, y) = seed_positions(top.n(), k, params.fa2.seed.unwrap_or(DEFAULT_SEED));
            Fa2State::from_positions(params.fa2, x, y, top.mass.clone())
        };
        let mut this = Self {
            params,
            coarse,
            level: levels,
            state,
            schedule,
            cost,
            credit: 0.0,
            work: 0.0,
            x: Vec::new(),
            y: Vec::new(),
            stale: false,
        };
        this.sync();
        this
    }

    /// The parameters it was made with.
    pub fn params(&self) -> &MultilevelParams {
        &self.params
    }

    /// Levels above the input graph.
    pub fn coarse_levels(&self) -> usize {
        self.coarse.len()
    }

    /// Node count of each level, the input graph first.
    pub fn level_sizes(&self) -> Vec<usize> {
        let n0 = self.x.len();
        std::iter::once(n0)
            .chain(self.coarse.iter().map(Coarse::n))
            .collect()
    }

    /// Adjacency entries (twice the edges) of each level, the input graph
    /// first.
    pub fn level_entries(&self, adj: &UndirectedCsr) -> Vec<usize> {
        std::iter::once(adj.targets.len())
            .chain(self.coarse.iter().map(|c| c.entries))
            .collect()
    }

    /// The level being laid out now: 0 is the input graph.
    pub fn level(&self) -> usize {
        self.level
    }

    /// Iterations to run at each level, the input graph first.
    pub fn schedule(&self) -> &[u32] {
        &self.schedule
    }

    /// Work done so far, in iterations of the input graph.
    pub fn work_done(&self) -> f64 {
        self.work
    }

    /// The work of the whole schedule, in iterations of the input graph.
    pub fn total_work(&self) -> f64 {
        self.schedule
            .iter()
            .zip(&self.cost)
            .map(|(&n, &c)| f64::from(n) * c)
            .sum()
    }

    /// Whether the input graph has had its last iteration.
    pub fn is_done(&self) -> bool {
        self.level == 0 && self.state.iterations() >= self.schedule[0]
    }

    /// Spend `budget` (in iterations of the input graph; any amount, carried
    /// over between calls) on iterations, moving down a level whenever one is
    /// done. `adj` is the input graph's adjacency, the one given to
    /// [`Self::new`]. `f64::INFINITY` runs the schedule to the end.
    pub fn step(&mut self, adj: &UndirectedCsr, budget: f64) {
        self.credit += budget.max(0.0);
        while !self.is_done() {
            let remaining = self.schedule[self.level] - self.state.iterations();
            if remaining == 0 {
                self.descend(adj);
                continue;
            }
            let cost = self.cost[self.level];
            let affordable = (self.credit / cost).floor().min(f64::from(remaining));
            if affordable < 1.0 {
                break;
            }
            let iterations = affordable as u32;
            if self.level == 0 {
                self.state.step(adj, iterations);
            } else {
                self.ensure_weights(adj);
                let c = &self.coarse[self.level - 1];
                self.state
                    .step_weighted(&c.adj, Some(&c.weights), iterations);
            }
            let spent = f64::from(iterations) * cost;
            self.credit -= spent;
            self.work += spent;
        }
        if self.is_done() {
            self.credit = 0.0;
        }
        self.sync();
    }

    /// Prolong the current level's positions to the next finer level, and
    /// start laying that one out.
    fn descend(&mut self, adj: &UndirectedCsr) {
        let l = self.level;
        let c = &self.coarse[l - 1];
        let (x, y) = self.state.positions();
        let spread = SPREAD * mean_edge_length(&c.adj, x, y);
        let (fx, fy) = prolong(c, x, y, spread);
        let mass = if l == 1 {
            degree_mass(adj)
        } else {
            self.coarse[l - 2].mass.clone()
        };
        self.state = Fa2State::from_positions(self.params.fa2, fx, fy, mass);
        self.level = l - 1;
        // Nothing reads a level the layout has come down from: the levels
        // below are prolonged from the one being laid out.
        self.coarse[l - 1].release();
    }

    /// Make the weights of the level being laid out, if a lean hierarchy
    /// let go of them: contract again from the nearest level below that has
    /// its own (the input graph's are all one), a level at a time, keeping
    /// only the last. They are the weights the hierarchy was built with:
    /// the same rows, gathered the same way.
    fn ensure_weights(&mut self, adj: &UndirectedCsr) {
        let l = self.level;
        if l == 0 || !self.coarse[l - 1].lacks_weights() {
            return;
        }
        // Level `from` has weights, or is the input graph.
        let mut from = l - 1;
        while from > 0 && self.coarse[from - 1].lacks_weights() {
            from -= 1;
        }
        let base_mass = if from == 0 {
            degree_mass(adj)
        } else {
            Vec::new()
        };
        for i in from + 1..=l {
            let (below, level) = self.coarse.split_at_mut(i - 1);
            let level = &mut level[0];
            let (a, w, m) = match below.last() {
                None => (adj, None, &base_mass[..]),
                Some(b) => (&b.adj, Some(&b.weights[..]), &b.mass[..]),
            };
            let group = level.groups();
            let rows = coarse_rows(
                a,
                w,
                m,
                &group,
                &level.member_offsets,
                &level.members,
                Some(level.entries),
            );
            drop(group);
            debug_assert_eq!(rows.weights.len(), level.entries);
            level.weights = rows.weights;
            // The level between gave what it was made again for.
            if i - 1 > from {
                self.coarse[i - 2].weights = Vec::new();
            }
        }
    }

    /// Bring the input graph's positions up to date: the current level's,
    /// prolonged level by level, as [`Self::descend`] will.
    fn sync(&mut self) {
        self.stale = false;
        let (x, y) = self.state.positions();
        if self.level == 0 {
            self.x.clear();
            self.x.extend_from_slice(x);
            self.y.clear();
            self.y.extend_from_slice(y);
            return;
        }
        let (mut x, mut y) = (x.to_vec(), y.to_vec());
        for l in (1..=self.level).rev() {
            let c = &self.coarse[l - 1];
            let spread = SPREAD * mean_edge_length(&c.adj, &x, &y);
            (x, y) = prolong(c, &x, &y, spread);
        }
        self.x = x;
        self.y = y;
    }

    /// A position for every input node now, uncentered: as of the last
    /// [`Self::step`] or [`Self::prolong`] ([`Self::step_level`] moves a
    /// coarse level without bringing these along).
    pub fn positions(&self) -> (&[f32], &[f32]) {
        (&self.x, &self.y)
    }

    /// The positions, centered on the origin, as [`layout_fa2_multilevel`]
    /// returns them.
    pub fn into_positions(mut self) -> Positions {
        self.prolong();
        let (mut x, mut y) = (self.x, self.y);
        center(&mut x, &mut y);
        (x, y)
    }
}

/// The heaviest merged edges of one level of the hierarchy
/// ([`Fa2Multilevel::level_edges`]): edge `i` joins the level's nodes
/// `src[i] < dst[i]` and stands for `weight[i]` edges of the input graph.
/// Heaviest first, then by `src`, then by `dst`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LevelEdges {
    pub src: Vec<u32>,
    pub dst: Vec<u32>,
    pub weight: Vec<f32>,
    /// How many merged edges the level has: these are the heaviest of them.
    pub total: usize,
}

/// # The level being laid out, as a graph of its own
///
/// For a view that draws the layout a level at a time, as an aggregated
/// graph (a disc for each node of the level, sized by its mass, and its
/// heaviest merged edges), and not as the input graph with every node at
/// its group's place. [`Self::step_level`] is [`Self::step`] one level at a
/// time; between two calls the level's nodes, masses, merged edges and the
/// groups its nodes will split into can be read. None of it changes the
/// layout: the positions are the same, bit for bit, however it is stepped
/// and whatever is read.
impl Fa2Multilevel {
    /// Whether the level being laid out has had all its iterations, and is
    /// not the input graph: the next [`Self::step_level`] comes down from
    /// it, and then the level is let go. Read what is wanted of it first.
    pub fn level_finished(&self) -> bool {
        self.level > 0 && self.state.iterations() >= self.schedule[self.level]
    }

    /// [`Self::step`], within one level: comes down a level first if the
    /// one being laid out is finished ([`Self::level_finished`]), then
    /// spends the budget on iterations of the level it is at, and stops
    /// there when that level is finished. So a caller sees every level, at
    /// its last iteration, before the hierarchy lets it go. What is left
    /// of the budget when a level finishes is dropped, not carried over
    /// (within a level it is, as [`Self::step`] carries it): the next call
    /// runs no longer than its own budget buys, though the levels above
    /// were cheap.
    ///
    /// While a coarse level is laid out, the input graph's positions
    /// ([`Self::positions`]) are not brought up to date: prolonging every
    /// level after every step is work a caller that draws the level itself
    /// ([`Self::level_positions`]) has no use for. [`Self::prolong`] does
    /// it when asked.
    pub fn step_level(&mut self, adj: &UndirectedCsr, budget: f64) {
        self.credit += budget.max(0.0);
        if self.level_finished() {
            self.descend(adj);
        }
        while !self.is_done() {
            let remaining = self.schedule[self.level] - self.state.iterations();
            let cost = self.cost[self.level];
            let affordable = (self.credit / cost).floor().min(f64::from(remaining));
            if affordable < 1.0 {
                break;
            }
            let iterations = affordable as u32;
            if self.level == 0 {
                self.state.step(adj, iterations);
            } else {
                self.ensure_weights(adj);
                let c = &self.coarse[self.level - 1];
                self.state
                    .step_weighted(&c.adj, Some(&c.weights), iterations);
            }
            let spent = f64::from(iterations) * cost;
            self.credit -= spent;
            self.work += spent;
        }
        if self.is_done() || self.level_finished() {
            self.credit = 0.0;
        }
        if self.level == 0 {
            self.sync();
        } else {
            self.stale = true;
        }
    }

    /// Bring the input graph's positions ([`Self::positions`]) up to date
    /// with the level being laid out, after [`Self::step_level`].
    pub fn prolong(&mut self) {
        if self.stale {
            self.sync();
        }
    }

    /// The positions of the nodes of the level being laid out
    /// ([`Self::level`]), uncentered: at level 0, the input graph's.
    pub fn level_positions(&self) -> (&[f32], &[f32]) {
        self.state.positions()
    }

    /// How many nodes the level being laid out has.
    pub fn level_nodes(&self) -> usize {
        self.state.positions().0.len()
    }

    /// The mass of each node of the level being laid out: the summed
    /// ForceAtlas2 masses (`degree + 1`) of the input nodes it stands for.
    /// `adj` is the input graph's adjacency.
    pub fn level_mass(&self, adj: &UndirectedCsr) -> Vec<f32> {
        match self.level {
            0 => degree_mass(adj),
            l => self.coarse[l - 1].mass.clone(),
        }
    }

    /// The nodes of level `level - 1` that each node of level `level`
    /// stands for (`1..=coarse_levels()`; level 0 is the input graph), as
    /// a CSR: node `c`'s are `members[offsets[c]..offsets[c + 1]]`, in
    /// index order. A node of the level below that is in no node's
    /// members left the hierarchy there (an isolated node, or a whole
    /// component merged into one node). `None` for a level there is not.
    ///
    /// Held for as long as the layout is, whichever level it is laying
    /// out: the hierarchy is a pure function of the graph, and this is
    /// all of it that outlasts the layout.
    pub fn level_members(&self, level: usize) -> Option<(&[u32], &[u32])> {
        let c = self.coarse.get(level.checked_sub(1)?)?;
        Some((&c.member_offsets, &c.members))
    }

    /// For each node of the level below the one being laid out, the node
    /// of this level it is part of: where it will start from when the
    /// layout comes down. `u32::MAX` for a node that has no group (an
    /// isolated node, or at a coarse level a whole component merged into
    /// one node: they start on a ring around the drawing). `None` at level
    /// 0, which has no level below.
    pub fn parents_below(&self) -> Option<Vec<u32>> {
        (self.level > 0).then(|| self.coarse[self.level - 1].groups())
    }

    /// The merged edges of the level being laid out, the heaviest `most` of
    /// them ([`LevelEdges`]). `adj` is the input graph's adjacency. At
    /// level 0 there are none to give: the input graph's edges are the
    /// caller's already, each of weight 1 (`total` says how many).
    ///
    /// A lean hierarchy ([`Self::new_lean`]) makes the level's weights
    /// again here if it let go of them, as the level's first iteration
    /// would have. Beyond the edges returned this holds a quarter of a
    /// megabyte while it runs, whatever the level's size: the heaviest are
    /// found by counting, not by sorting them all.
    pub fn level_edges(&mut self, adj: &UndirectedCsr, most: usize) -> LevelEdges {
        if self.level == 0 {
            return LevelEdges {
                total: adj.targets.len() / 2,
                ..LevelEdges::default()
            };
        }
        self.ensure_weights(adj);
        let c = &self.coarse[self.level - 1];
        let total = c.entries / 2;
        let take = most.min(total);
        // Each edge once, from its lower end: `(node, entry)`.
        let entries = || {
            (0..c.n as u32).flat_map(move |u| {
                let base = c.adj.offsets[u as usize] as usize;
                c.adj
                    .neighbors(u)
                    .iter()
                    .enumerate()
                    .filter(move |&(_, &v)| v > u)
                    .map(move |(e, &v)| (u, v, c.weights[base + e]))
            })
        };
        // The weight the last edge taken has, as far as a bucket tells
        // weights apart (they are whole numbers), and how many heavier
        // edges there are than that.
        const BUCKETS: usize = 1 << 16;
        let bucket = |w: f32| (w.max(0.0) as usize).min(BUCKETS - 1);
        let (mut least, mut above) = (0, 0);
        if take < total {
            let mut count = vec![0u32; BUCKETS];
            for (_, _, w) in entries() {
                count[bucket(w)] += 1;
            }
            least = BUCKETS - 1;
            while above + count[least] as usize <= take && least > 0 {
                above += count[least] as usize;
                least -= 1;
            }
        }
        // Every heavier edge, and the first in index order of those as
        // heavy as the last.
        let mut ties = take - above.min(take);
        let mut picked: Vec<(u32, u32, f32)> = Vec::with_capacity(take);
        if take > 0 {
            for (u, v, w) in entries() {
                let b = bucket(w);
                if take == total || b > least {
                    picked.push((u, v, w));
                } else if b == least && ties > 0 {
                    picked.push((u, v, w));
                    ties -= 1;
                }
                if picked.len() == take {
                    break;
                }
            }
        }
        picked.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
        LevelEdges {
            src: picked.iter().map(|e| e.0).collect(),
            dst: picked.iter().map(|e| e.1).collect(),
            weight: picked.iter().map(|e| e.2).collect(),
            total,
        }
    }
}

impl Fa2Multilevel {
    /// The order to draw the edges `(src[e], dst[e])` in, most important
    /// first, so that any prefix of it is a backbone of the drawing and not
    /// a corner of it: `order[i]` is the edge drawn `i`-th. A renderer that
    /// has to draw fewer edges than there are (while the view moves) draws
    /// the first of these.
    ///
    /// The key, in order:
    ///
    /// 1. **The level at which the edge's two ends merge**, coarsest first:
    ///    an edge whose ends are still apart at the coarsest level joins two
    ///    of the drawing's far groups; one whose ends merge at level 1 is
    ///    local fill.
    /// 2. **One edge of each merged edge per round.** Just below the level
    ///    where its ends merge, the edge is one of the input edges a merged
    ///    edge stands for (its weight). Each merged edge gives its first
    ///    input edge before any gives its second, and so on, so a short
    ///    prefix shows every tie between groups, not all of the heaviest
    ///    one. (An edge whose ends merge at the first level stands for
    ///    itself: the local fill is in hash order.)
    /// 3. **Weight** within a round, heaviest first.
    /// 4. **A hash** of the edge's index.
    ///
    /// The edges are those of the graph the hierarchy was built from, in any
    /// order and with any duplicates; an edge that names no node of it, or
    /// joins a node to itself, comes last. A pure function of the hierarchy
    /// and the edge list: the same on any thread count. With no coarse
    /// level there is no hierarchy to rank by: see [`edge_order_by_degree`].
    ///
    /// # Panics
    ///
    /// If `src` and `dst` differ in length.
    pub fn edge_order(&self, adj: &UndirectedCsr, src: &[u32], dst: &[u32]) -> Vec<u32> {
        assert_eq!(src.len(), dst.len(), "one src and one dst per edge");
        let mut read = |start: usize, s: &mut [u32], d: &mut [u32]| {
            s.copy_from_slice(&src[start..start + s.len()]);
            d.copy_from_slice(&dst[start..start + d.len()]);
        };
        let mut keys = vec![0u64; src.len()];
        self.edge_order_keys(adj, src.len(), &mut read, &mut keys);
        edges_of_keys(keys)
    }

    /// [`Self::edge_order`] for a caller that holds neither the edges nor
    /// the order's keys in memory of its own (a browser's worker, whose
    /// memory is small and whose edges are the page's): the `edges` edges
    /// are read a run at a time (`read(start, src, dst)` fills the two
    /// runs, of equal length, with the edges from `start` on), and each
    /// edge's sort key is left in `keys`, which is read and written a run
    /// at a time too ([`OrderKeys`]). The caller sorts the keys, ascending,
    /// and [`edge_of_key`] of each is then the order. Here it holds two
    /// runs of edges, one of keys, and two bytes for each adjacency entry
    /// of the largest level.
    ///
    /// `adj` is the input graph's adjacency, the one given to
    /// [`Self::new`]. A lean hierarchy ([`Self::new_lean`]) does not hold
    /// the weights of its dense levels: they are counted again here, from
    /// `adj`, a level at a time, and the order is the same. Once the layout
    /// has come down a level the hierarchy above it is let go, and the
    /// order is [`edge_order_by_degree`]'s.
    pub fn edge_order_keys(
        &self,
        adj: &UndirectedCsr,
        edges: usize,
        read: &mut ReadEdges<'_>,
        keys: &mut dyn OrderKeys,
    ) {
        let levels = self.coarse.len();
        if self.level < levels {
            return edge_order_by_degree_keys(adj, edges, read, keys);
        }
        let n0 = self.x.len();
        // Each level's nodes to their groups in the level above.
        let parents: Vec<Vec<u32>> = self.coarse.iter().map(Coarse::groups).collect();
        // First, for each edge, the level its ends merge at (0: it joins
        // nothing) in bits 32 to 37 and, at the level below that, the
        // merged edge it is part of (its adjacency entry there) below; and
        // how many edges there are to rank at each level.
        let level_of = |key: u64| (key >> 32) as usize & 63;
        let ranked = |key: u64, l: usize| level_of(key) == l + 1 && key as u32 != NONE;
        let mut to_rank = [0usize; 64];
        let run = ORDER_RUN.min(edges);
        let (mut src, mut dst) = (vec![0u32; run], vec![0u32; run]);
        let mut chunk = vec![0u64; run];
        let mut start = 0;
        while start < edges {
            let len = run.min(edges - start);
            read(start, &mut src[..len], &mut dst[..len]);
            for i in 0..len {
                chunk[i] = 0;
                let (mut a, mut b) = (src[i], dst[i]);
                if a as usize >= n0 || b as usize >= n0 || a == b {
                    continue;
                }
                // `l` is the level `a` and `b` are nodes of.
                let mut l = 0;
                while l < levels {
                    let (pa, pb) = (parents[l][a as usize], parents[l][b as usize]);
                    if pa == NONE || pb == NONE || pa == pb {
                        break;
                    }
                    (a, b) = (pa, pb);
                    l += 1;
                }
                // Apart at level `l`, together (or gone) at `l + 1`.
                let mut entry = NONE;
                if l > 0 {
                    let level = &self.coarse[l - 1].adj;
                    let (lo, hi) = (a.min(b), a.max(b));
                    let base = level.offsets[lo as usize] as usize;
                    if let Ok(at) = level.neighbors(lo).binary_search(&hi) {
                        entry = (base + at) as u32;
                    }
                }
                let level = (l + 1).min(63);
                chunk[i] = (level as u64) << 32 | u64::from(entry);
                if entry != NONE {
                    to_rank[level] += 1;
                }
            }
            keys.write(start, &chunk[..len]);
            start += len;
        }
        drop((src, dst));
        // The round and the weight join them (bits 38 to 51, and 52 up), a
        // level at a time, in one buffer of two bytes an entry that every
        // level uses in turn: first as the count of the edges seen so far
        // of each merged edge, then, for a level whose weights a lean
        // hierarchy let go of, as those weights counted again.
        let most = self.coarse.iter().map(|c| c.entries).max().unwrap_or(0);
        let mut scratch = vec![0u16; most];
        // Every run of the keys, through `change`, and back.
        let mut each_run = |keys: &mut dyn OrderKeys, change: &mut dyn FnMut(&mut [u64])| {
            let mut start = 0;
            while start < edges {
                let len = run.min(edges - start);
                keys.read(start, &mut chunk[..len]);
                change(&mut chunk[..len]);
                keys.write(start, &chunk[..len]);
                start += len;
            }
        };
        // Each input node's group at the level in hand (`NONE` once it has
        // left the hierarchy), for the weights counted again.
        let mut ancestor: Vec<u32> = (0..n0 as u32).collect();
        for l in 1..=levels.min(62) {
            let coarse = &self.coarse[l - 1];
            for a in &mut ancestor {
                if *a != NONE {
                    *a = parents[l - 1][*a as usize];
                }
            }
            // No edge's ends merge just above this level: nothing to rank.
            if to_rank[l + 1] == 0 {
                continue;
            }
            let seen = &mut scratch[..coarse.entries];
            seen.fill(0);
            let lean = coarse.lacks_weights();
            // Its round: how many edges before it, in index order, are
            // part of the same merged edge. And its merged edge's weight,
            // as far as a key tells them apart: with the round, where the
            // level holds its weights.
            each_run(keys, &mut |chunk| {
                for key in chunk.iter_mut().filter(|key| ranked(**key, l)) {
                    let entry = *key as u32 as usize;
                    let count = &mut seen[entry];
                    *key |= u64::from(*count).min(KEY_ROUNDS) << 38;
                    *count = count.saturating_add(1);
                    if !lean {
                        let weight = (coarse.weights[entry].max(0.0) as u64).min(KEY_WEIGHTS);
                        *key |= weight << 52;
                    }
                }
            });
            if lean {
                self.entry_counts(adj, &ancestor, l, seen);
                each_run(keys, &mut |chunk| {
                    for key in chunk.iter_mut().filter(|key| ranked(**key, l)) {
                        *key |= u64::from(seen[*key as u32 as usize]) << 52;
                    }
                });
            }
        }
        drop(parents);
        // And with those, its key. An edge whose ends merge at the first
        // level stands for itself, and one that joins nothing comes last.
        let mut e = 0;
        each_run(keys, &mut |chunk| {
            for key in chunk.iter_mut() {
                let (level, entry) = (level_of(*key), *key as u32);
                *key = if level <= 1 || entry == NONE {
                    order_key(level, 0, 1, e)
                } else {
                    order_key(level, (*key >> 38) & KEY_ROUNDS, *key >> 52, e)
                };
                e += 1;
            }
        });
    }

    /// The weight of each adjacency entry of level `l`, into `counts` (one
    /// an entry), as far as an edge's key tells weights apart
    /// ([`KEY_WEIGHTS`]): how many of the input graph's entries it stands
    /// for, counted from `adj` (`ancestor[v]` is input node `v`'s group at
    /// level `l`, or [`NONE`]). A weight is a sum of ones, so this is the
    /// weight the level was built with, capped; it is for a level whose
    /// weights a lean hierarchy let go of, and takes two bytes an entry
    /// where they took four.
    fn entry_counts(&self, adj: &UndirectedCsr, ancestor: &[u32], l: usize, counts: &mut [u16]) {
        let level = &self.coarse[l - 1].adj;
        counts.fill(0);
        for (u, &a) in ancestor.iter().enumerate() {
            if a == NONE {
                continue;
            }
            let base = level.offsets[a as usize] as usize;
            let row = level.neighbors(a);
            for &v in adj.neighbors(u as u32) {
                let b = ancestor[v as usize];
                if b == NONE || b == a {
                    continue;
                }
                if let Ok(at) = row.binary_search(&b) {
                    let count = &mut counts[base + at];
                    *count = (*count + 1).min(KEY_WEIGHTS as u16);
                }
            }
        }
    }
}

/// How many edges, and how many keys, an order reads and writes at a time.
const ORDER_RUN: usize = 1 << 15;

/// The greatest weight an edge's key tells apart, and the greatest round:
/// heavier and later ones tie.
const KEY_WEIGHTS: u64 = (1 << 12) - 1;
const KEY_ROUNDS: u64 = (1 << 14) - 1;

/// What gives an edge order its edges, a run at a time: `read(start, src,
/// dst)` fills the two runs, of equal length, with the ends of the edges
/// from `start` on.
pub type ReadEdges<'a> = dyn FnMut(usize, &mut [u32], &mut [u32]) + 'a;

/// Where an edge order's keys are kept while it is made, one `u64` an edge:
/// a slice, or memory that is not this program's (a browser worker's
/// `BigUint64Array`, outside its small WebAssembly memory), read and
/// written a run at a time. See [`Fa2Multilevel::edge_order_keys`].
pub trait OrderKeys {
    /// Copy the keys from `start` on into `out`, all of it.
    fn read(&mut self, start: usize, out: &mut [u64]);
    /// Set the keys from `start` on to `keys`.
    fn write(&mut self, start: usize, keys: &[u64]);
}

impl OrderKeys for Vec<u64> {
    fn read(&mut self, start: usize, out: &mut [u64]) {
        out.copy_from_slice(&self[start..start + out.len()]);
    }

    fn write(&mut self, start: usize, keys: &[u64]) {
        self[start..start + keys.len()].copy_from_slice(keys);
    }
}

/// The edge a sort key is of ([`Fa2Multilevel::edge_order_keys`],
/// [`edge_order_by_degree_keys`]): with the keys sorted ascending, the
/// order.
pub fn edge_of_key(key: u64) -> u32 {
    unmix32(key as u32)
}

/// The order of the edges with these keys.
fn edges_of_keys(mut keys: Vec<u64>) -> Vec<u32> {
    keys.sort_unstable();
    keys.into_iter().map(edge_of_key).collect()
}

/// The sort key of edge `e`: `level` (up to 63) highest first, then `round`
/// lowest first, then `weight` heaviest first, in its high half; and in its
/// low half a hash of `e` that `e` can be read back from. Rounds past
/// 16,383 and weights past 4,095 tie.
fn order_key(level: usize, round: u64, weight: u64, e: usize) -> u64 {
    let level = 63 - (level as u64).min(63);
    let round = round.min(KEY_ROUNDS);
    let weight = KEY_WEIGHTS - weight.min(KEY_WEIGHTS);
    (level << 26 | round << 12 | weight) << 32 | u64::from(mix32(e as u32))
}

/// MurmurHash3's 32-bit finalizer: a permutation of the `u32`s that sends
/// neighbouring indexes far apart, the same on every platform.
fn mix32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 13;
    h = h.wrapping_mul(0xC2B2_AE35);
    h ^ (h >> 16)
}

/// [`mix32`]'s inverse.
fn unmix32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x7ED1_B41D);
    h ^= (h >> 13) ^ (h >> 26);
    h = h.wrapping_mul(0xA5CB_9243);
    h ^ (h >> 16)
}

/// [`Fa2Multilevel::edge_order`] for a graph with no hierarchy (a flat
/// layout, rings, layers, or a graph too small to coarsen): the edges
/// `(src[e], dst[e])` by the larger degree of their two ends in `adj`,
/// highest first, then by a hash of the index. A node is drawn larger the
/// more neighbours it has, so the edges at the largest nodes come first. An
/// edge that names no node of `adj` comes last.
///
/// # Panics
///
/// If `src` and `dst` differ in length.
pub fn edge_order_by_degree(adj: &UndirectedCsr, src: &[u32], dst: &[u32]) -> Vec<u32> {
    assert_eq!(src.len(), dst.len(), "one src and one dst per edge");
    let mut read = |start: usize, s: &mut [u32], d: &mut [u32]| {
        s.copy_from_slice(&src[start..start + s.len()]);
        d.copy_from_slice(&dst[start..start + d.len()]);
    };
    let mut keys = vec![0u64; src.len()];
    edge_order_by_degree_keys(adj, src.len(), &mut read, &mut keys);
    edges_of_keys(keys)
}

/// [`edge_order_by_degree`], with the edges read a run at a time and each
/// edge's sort key left in `keys`, as [`Fa2Multilevel::edge_order_keys`]
/// does.
pub fn edge_order_by_degree_keys(
    adj: &UndirectedCsr,
    edges: usize,
    read: &mut ReadEdges<'_>,
    keys: &mut dyn OrderKeys,
) {
    let n = adj.offsets.len().saturating_sub(1);
    let run = ORDER_RUN.min(edges);
    let (mut src, mut dst) = (vec![0u32; run], vec![0u32; run]);
    let mut chunk = vec![0u64; run];
    let mut start = 0;
    while start < edges {
        let len = run.min(edges - start);
        read(start, &mut src[..len], &mut dst[..len]);
        for i in 0..len {
            let (a, b) = (src[i], dst[i]);
            let degree = if (a as usize) < n && (b as usize) < n {
                1 + adj.degree(a).max(adj.degree(b)) as u64
            } else {
                0
            };
            let rank = u64::from(u32::MAX) - degree.min(u64::from(u32::MAX));
            chunk[i] = rank << 32 | u64::from(mix32((start + i) as u32));
        }
        keys.write(start, &chunk[..len]);
        start += len;
    }
}

/// Coarse-to-fine ForceAtlas2 in one call: [`Fa2Multilevel`] run to the end of
/// its schedule. A `mask` restricts the edges as for [`super::layout_fa2`].
pub fn layout_fa2_multilevel(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    params: MultilevelParams,
) -> Positions {
    let n = topo.n_nodes();
    if n == 0 {
        return (Vec::new(), Vec::new());
    }
    let view = undirected_view(topo, mask);
    let adj: &UndirectedCsr = view.get();
    let mut state = Fa2Multilevel::new(adj, params);
    state.step(adj, f64::INFINITY);
    state.into_positions()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::layout_fa2;

    fn star(leaves: u32) -> Topology {
        let src = vec![0; leaves as usize];
        let dst = (1..=leaves).collect();
        Topology::build(leaves as usize + 1, src, dst)
    }

    fn grid(w: u32, h: u32) -> Topology {
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
        Topology::build((w * h) as usize, src, dst)
    }

    #[test]
    fn a_star_collapses_in_one_level() {
        // Matching alone would pair the hub with one leaf and stall; the
        // leaves join the hub's group instead.
        let t = star(500);
        let adj = t.undirected();
        let mass = degree_mass(adj);
        let (group, nc) = group_nodes(adj, None, &mass);
        assert_eq!(nc, 1);
        assert!(group.iter().all(|&g| g == 0));
    }

    #[test]
    fn isolated_nodes_leave_the_hierarchy_and_mass_is_kept() {
        // A path of 100 and 50 isolated nodes.
        let t = Topology::build(150, (0..99).collect(), (1..100).collect());
        let adj = t.undirected();
        let levels = coarsen(adj, 10, false);
        assert!(!levels.is_empty());
        assert_eq!(levels[0].dropped, (100..150).collect::<Vec<u32>>());
        assert_eq!(levels[0].fine_n, 150);
        // Summed masses: the path's `deg + 1` all the way up.
        let path_mass: f32 = degree_mass(adj)[..100].iter().sum();
        for l in &levels {
            assert_eq!(l.mass.iter().sum::<f32>(), path_mass);
            // Each edge weight counts input edges: the coarse graph of a path
            // keeps its 99 edges, less the ones merged inside groups.
            assert!(l.weights.iter().sum::<f32>() <= 2.0 * 99.0);
        }
        // Each level at least halves the path.
        for w in levels.windows(2) {
            assert!(w[1].n() * 2 <= w[0].n() + 1);
        }
    }

    #[test]
    fn every_node_has_a_place_from_the_start_and_at_the_end() {
        let t = Topology::build(300, (0..199).collect(), (1..200).collect());
        let adj = t.undirected();
        let params = MultilevelParams {
            coarsest_nodes: 10,
            ..MultilevelParams::default()
        };
        let mut ml = Fa2Multilevel::new(adj, params);
        assert!(ml.coarse_levels() > 0);
        let check = |ml: &Fa2Multilevel| {
            let (x, y) = ml.positions();
            assert_eq!(x.len(), 300);
            assert!(x.iter().chain(y).all(|v| v.is_finite()));
        };
        check(&ml);
        ml.step(adj, f64::INFINITY);
        assert!(ml.is_done());
        check(&ml);
    }

    #[test]
    fn how_the_work_is_split_does_not_change_the_result() {
        let t = grid(30, 30);
        let adj = t.undirected();
        let params = MultilevelParams {
            coarsest_nodes: 20,
            ..MultilevelParams::default()
        };
        let mut a = Fa2Multilevel::new(adj, params);
        a.step(adj, f64::INFINITY);
        let mut b = Fa2Multilevel::new(adj, params);
        let mut budgets = [0.3, 1.0, 7.0, 0.01, 25.0].iter().cycle();
        while !b.is_done() {
            b.step(adj, *budgets.next().unwrap());
        }
        assert_eq!(a.positions(), b.positions());
        assert_eq!(a.into_positions(), layout_fa2_multilevel(&t, None, params));
    }

    /// A random graph of `n` nodes and `m` edges (some repeated, some
    /// loops), the same every time.
    fn random_edges(n: u64, m: usize) -> (Vec<u32>, Vec<u32>) {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        (0..m).map(|_| (next() as u32, next() as u32)).unzip()
    }

    #[test]
    fn a_level_at_a_time_gives_the_same_bits_and_shows_every_level() {
        // Dense enough that a lean hierarchy lets go of weights, with a few
        // isolated nodes (the last 40) to be dropped.
        let (src, dst) = random_edges(2960, 45_000);
        let t = Topology::build(3000, src, dst);
        let adj = t.undirected();
        let params = MultilevelParams {
            coarsest_nodes: 20,
            ..MultilevelParams::default()
        };
        let want = layout_fa2_multilevel(&t, None, params);
        for lean in [false, true] {
            let mut whole = Fa2Multilevel::build(adj, params, lean);
            let mut ml = Fa2Multilevel::build(adj, params, lean);
            let levels = ml.coarse_levels();
            assert!(levels >= 3);
            let sizes = ml.level_sizes();
            let entries = ml.level_entries(adj);
            let mut seen = Vec::new();
            let mut budgets = [0.3, 1.0, 7.0, 0.01, 25.0].iter().cycle();
            // The level above's masses and positions, and this level's
            // nodes' groups in it.
            let mut above: Option<(Vec<f32>, Vec<u32>)> = None;
            while !ml.is_done() {
                let before = ml.level();
                let parents = ml.level_finished().then(|| ml.parents_below().unwrap());
                let budget = *budgets.next().unwrap();
                ml.step_level(adj, budget);
                // Never more than one level down a call, and only from a
                // finished one.
                assert!(before - ml.level() <= 1);
                assert_eq!(before != ml.level(), parents.is_some());
                let l = ml.level();
                let (x, y) = ml.level_positions();
                assert_eq!((x.len(), y.len()), (sizes[l], sizes[l]));
                assert_eq!(ml.level_nodes(), sizes[l]);
                if seen.last() != Some(&l) {
                    seen.push(l);
                    let mass = ml.level_mass(adj);
                    assert_eq!(mass.len(), sizes[l]);
                    if let (Some(parents), Some((mass_above, _))) = (&parents, &above) {
                        // A node's mass is its members', and every node
                        // with a neighbour has a group.
                        let mut sum = vec![0.0f32; mass_above.len()];
                        for (i, &p) in parents.iter().enumerate() {
                            if p != NONE {
                                sum[p as usize] += mass[i];
                            }
                        }
                        assert_eq!(&sum, mass_above);
                    }
                    let all = ml.level_edges(adj, usize::MAX);
                    assert_eq!(all.total, entries[l] / 2);
                    if l == 0 {
                        assert!(all.src.is_empty());
                    } else {
                        assert_eq!(all.src.len(), all.total);
                        // Every merged edge once, lower end first, heaviest
                        // first; and together they stand for the input
                        // edges whose ends are apart here.
                        for i in 0..all.total {
                            assert!(all.src[i] < all.dst[i] && (all.dst[i] as usize) < sizes[l]);
                            if i > 0 {
                                let (a, b) = (i - 1, i);
                                let key = |i: usize| (all.src[i], all.dst[i]);
                                assert!(
                                    all.weight[a] > all.weight[b]
                                        || (all.weight[a] == all.weight[b] && key(a) < key(b))
                                );
                            }
                        }
                        // The heaviest few are the first of them all.
                        for most in [0, 1, 7, all.total / 2, all.total - 1, all.total] {
                            let some = ml.level_edges(adj, most);
                            assert_eq!(some.total, all.total);
                            assert_eq!(some.src, &all.src[..most], "level {l}, most {most}");
                            assert_eq!(some.dst, &all.dst[..most]);
                            assert_eq!(some.weight, &all.weight[..most]);
                        }
                    }
                    above = Some((mass, Vec::new()));
                }
            }
            // Every level, coarsest first.
            assert_eq!(seen, (0..=levels).rev().collect::<Vec<_>>());
            whole.step(adj, f64::INFINITY);
            assert_eq!(ml.positions(), whole.positions());
            assert_eq!(ml.work_done().round(), whole.work_done().round());
            assert_eq!(ml.into_positions(), want);
        }
    }

    #[test]
    fn the_members_of_every_level_outlast_the_layout() {
        // The hierarchy itself is kept when a level is let go (#498): what
        // each node of a level stands for in the level below, the same
        // before the layout starts and after it ends, lean or not.
        let t = grid(30, 30);
        let adj = t.undirected();
        let params = MultilevelParams {
            coarsest_nodes: 20,
            ..MultilevelParams::default()
        };
        for lean in [false, true] {
            let mut ml = Fa2Multilevel::build(adj, params, lean);
            let levels = ml.coarse_levels();
            assert!(levels >= 3);
            let sizes = ml.level_sizes();
            let members = |ml: &Fa2Multilevel| -> Vec<(Vec<u32>, Vec<u32>)> {
                (1..=levels)
                    .map(|l| {
                        let (offsets, members) = ml.level_members(l).unwrap();
                        (offsets.to_vec(), members.to_vec())
                    })
                    .collect()
            };
            let before = members(&ml);
            for (l, (offsets, of)) in before.iter().enumerate() {
                // A CSR over the level's nodes, of nodes of the level
                // below, each in one node's members at most.
                assert_eq!(offsets.len(), sizes[l + 1] + 1);
                assert_eq!(*offsets.last().unwrap() as usize, of.len());
                let mut seen = vec![false; sizes[l]];
                for &m in of {
                    assert!(!std::mem::replace(&mut seen[m as usize], true));
                }
                // They are the parents the layout prolongs by.
                let group = ml.coarse[l].groups();
                for c in 0..sizes[l + 1] {
                    for &m in &of[offsets[c] as usize..offsets[c + 1] as usize] {
                        assert_eq!(group[m as usize], c as u32);
                    }
                }
            }
            assert!(ml.level_members(0).is_none() && ml.level_members(levels + 1).is_none());
            ml.step(adj, f64::INFINITY);
            assert!(ml.is_done());
            assert_eq!(members(&ml), before);
            // And nothing else of the levels the layout came down from.
            assert!(ml
                .coarse
                .iter()
                .all(|c| c.adj.targets.is_empty() && c.weights.is_empty() && c.mass.is_empty()));
        }
    }

    #[test]
    fn a_level_at_a_time_prolongs_only_when_asked() {
        let t = grid(30, 30);
        let adj = t.undirected();
        let params = MultilevelParams {
            coarsest_nodes: 20,
            ..MultilevelParams::default()
        };
        let mut whole = Fa2Multilevel::new(adj, params);
        let mut ml = Fa2Multilevel::new(adj, params);
        // The coarsest level, to its last iteration and no further.
        ml.step_level(adj, f64::INFINITY);
        assert!(ml.level_finished());
        assert_eq!(ml.level(), ml.coarse_levels());
        // As far as a whole step with exactly that work gets.
        whole.step(adj, ml.work_done());
        assert_eq!(whole.work_done(), ml.work_done());
        assert_ne!(ml.positions(), whole.positions());
        ml.prolong();
        assert_eq!(ml.positions(), whole.positions());
        // The next call comes a level down, and what the last one had left
        // over is not spent there.
        let work = ml.work_done();
        ml.step_level(adj, 0.0);
        assert_eq!(ml.level(), ml.coarse_levels() - 1);
        assert_eq!(ml.work_done(), work);
        assert!(!ml.level_finished());
        // An edge order asked for after a level was read is the order
        // asked for before.
        let (src, dst) = random_edges(900, 2000);
        let (src, dst) = (&src[..], &dst[..]);
        let mut fresh = Fa2Multilevel::new_lean(adj, params);
        let order = fresh.edge_order(adj, src, dst);
        fresh.step_level(adj, f64::INFINITY);
        let _ = fresh.level_edges(adj, 100);
        let _ = fresh.parents_below();
        assert_eq!(fresh.edge_order(adj, src, dst), order);
    }

    #[test]
    fn a_lean_hierarchy_gives_the_same_bits_at_every_step() {
        // A random graph of mean degree 30: grouping halves its nodes and
        // merges few of its edges, so its first levels are as large as it is.
        let n = 3000u64;
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let (src, dst): (Vec<u32>, Vec<u32>) =
            (0..45_000).map(|_| (next() as u32, next() as u32)).unzip();
        let t = Topology::build(n as usize, src, dst);
        let adj = t.undirected();
        let params = MultilevelParams {
            coarsest_nodes: 20,
            ..MultilevelParams::default()
        };
        let mut full = Fa2Multilevel::new(adj, params);
        let mut lean = Fa2Multilevel::new_lean(adj, params);
        let lacking = |ml: &Fa2Multilevel| ml.coarse.iter().filter(|c| c.lacks_weights()).count();
        assert_eq!(lacking(&full), 0);
        // Two levels in a row, so that one is made again from the other.
        assert!(lacking(&lean) >= 2, "{:?}", lean.level_entries(adj));
        assert!(lean.coarse[0].lacks_weights() && lean.coarse[1].lacks_weights());
        assert_eq!(full.level_sizes(), lean.level_sizes());
        assert_eq!(full.level_entries(adj), lean.level_entries(adj));
        assert_eq!(full.schedule(), lean.schedule());
        let bits = |ml: &Fa2Multilevel| {
            let (x, y) = ml.positions();
            let bits = |v: &[f32]| v.iter().map(|f| f.to_bits()).collect::<Vec<_>>();
            (bits(x), bits(y))
        };
        assert_eq!(bits(&full), bits(&lean));
        let mut budgets = [0.7, 3.0, 0.05, 11.0].iter().cycle();
        let sizes = full.level_sizes();
        while !full.is_done() {
            let budget = *budgets.next().unwrap();
            full.step(adj, budget);
            lean.step(adj, budget);
            assert_eq!(full.level(), lean.level());
            assert_eq!(bits(&full), bits(&lean), "level {}", full.level());
            // The counts outlive the levels the layout has come down from.
            assert_eq!(full.level_sizes(), sizes);
            assert_eq!(lean.level_entries(adj), full.level_entries(adj));
        }
        assert!(lean.is_done());
        // And both are the one-call layout.
        assert_eq!(
            lean.into_positions(),
            layout_fa2_multilevel(&t, None, params)
        );
    }

    #[test]
    fn a_graph_too_small_to_coarsen_is_layout_fa2() {
        let t = grid(6, 6);
        let params = MultilevelParams::default();
        let ml = layout_fa2_multilevel(&t, None, params);
        let flat = layout_fa2(
            &t,
            None,
            Fa2Params {
                iterations: params.coarsest_iterations,
                ..params.fa2
            },
        );
        assert_eq!(ml, flat);
    }

    #[test]
    fn weights_of_one_are_the_unweighted_step() {
        // `step_weighted` with every weight 1 is the same bits as `step`.
        let t = grid(12, 12);
        let adj = t.undirected();
        let p = Fa2Params::default();
        let mut a = Fa2State::new(adj, p);
        a.step(adj, 20);
        let mut b = Fa2State::new(adj, p);
        let ones = vec![1.0f32; adj.targets.len()];
        b.step_weighted(adj, Some(&ones), 20);
        assert_eq!(a.positions(), b.positions());
    }

    /// Two 6-cliques joined by 3 edges.
    fn barbell() -> (Topology, Vec<u32>, Vec<u32>) {
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for base in [0u32, 6] {
            for i in 0..6 {
                for j in i + 1..6 {
                    src.push(base + i);
                    dst.push(base + j);
                }
            }
        }
        for i in 0..3 {
            src.push(i);
            dst.push(6 + i);
        }
        let topo = Topology::build(12, src.clone(), dst.clone());
        (topo, src, dst)
    }

    fn is_permutation(order: &[u32], m: usize) -> bool {
        let mut seen = vec![false; m];
        order.len() == m
            && order
                .iter()
                .all(|&e| (e as usize) < m && !std::mem::replace(&mut seen[e as usize], true))
    }

    /// By the definition, slowly: the level the ends of `(a, b)` merge at,
    /// and the two groups they are in just below it.
    fn merge_of(state: &Fa2Multilevel, mut a: u32, mut b: u32) -> (usize, (u32, u32)) {
        for (l, c) in state.coarse.iter().enumerate() {
            let group = |u: u32| (0..c.n()).find(|&g| c.members(g).contains(&u));
            match (group(a), group(b)) {
                (Some(x), Some(y)) if x != y => (a, b) = (x as u32, y as u32),
                _ => return (l + 1, (a.min(b), a.max(b))),
            }
        }
        (state.coarse.len() + 1, (a.min(b), a.max(b)))
    }

    #[test]
    fn the_edge_order_is_by_merge_level_then_a_round_of_each_merged_edge() {
        // A grid with a few long chords, twice over (so merged edges stand
        // for several input edges from the first level up).
        let (w, h) = (12u32, 9u32);
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for _ in 0..2 {
            for y in 0..h {
                for x in 0..w {
                    let i = y * w + x;
                    if x + 1 < w {
                        src.push(i);
                        dst.push(i + 1);
                    }
                    if y + 1 < h {
                        src.push(i + w);
                        dst.push(i);
                    }
                }
            }
        }
        for i in 0..6 {
            src.push(i);
            dst.push(w * h - 1 - 7 * i);
        }
        let topo = Topology::build((w * h) as usize, src.clone(), dst.clone());
        let params = MultilevelParams {
            coarsest_nodes: 4,
            ..Default::default()
        };
        let adj = topo.undirected();
        let state = Fa2Multilevel::new(adj, params);
        assert!(state.coarse_levels() >= 3, "{:?}", state.level_sizes());
        let order = state.edge_order(adj, &src, &dst);
        assert!(is_permutation(&order, src.len()));
        assert_eq!(order, state.edge_order(adj, &src, &dst));

        // Each edge's level, merged edge, and round: how many edges of a
        // lower index share its merged edge.
        let merges: Vec<_> = (0..src.len())
            .map(|e| merge_of(&state, src[e], dst[e]))
            .collect();
        // (An edge whose ends merge at the first level stands for itself.)
        let round = |e: usize| match merges[e].0 {
            1 => 0,
            _ => merges[..e].iter().filter(|m| **m == merges[e]).count(),
        };
        let key: Vec<(usize, usize)> = order
            .iter()
            .map(|&e| (merges[e as usize].0, round(e as usize)))
            .collect();
        // Coarsest level first; within a level, round by round.
        assert!(
            key.windows(2)
                .all(|w| w[0].0 > w[1].0 || (w[0].0 == w[1].0 && w[0].1 <= w[1].1)),
            "{key:?}"
        );
        // The first edges' ends are apart at the coarsest level.
        assert_eq!(key[0], (state.coarse_levels() + 1, 0));
        // A round has each of its merged edges once, the heaviest first.
        // (A merged edge's weight counts each pair of input nodes once.)
        let weight = |m: &(usize, (u32, u32))| {
            let mut pairs: Vec<(u32, u32)> = (0..src.len())
                .filter(|&e| merges[e] == *m)
                .map(|e| (src[e].min(dst[e]), src[e].max(dst[e])))
                .collect();
            pairs.sort_unstable();
            pairs.dedup();
            pairs.len()
        };
        let mut rounds = 0;
        let mut at = 0;
        while at < order.len() {
            let len = key[at..].iter().take_while(|k| **k == key[at]).count();
            if key[at].0 >= 2 {
                let of: Vec<_> = order[at..at + len]
                    .iter()
                    .map(|&e| merges[e as usize])
                    .collect();
                let mut distinct = of.clone();
                distinct.sort_unstable();
                distinct.dedup();
                assert_eq!(distinct.len(), of.len(), "{:?}", key[at]);
                let weights: Vec<usize> = of.iter().map(weight).collect();
                assert!(weights.windows(2).all(|w| w[0] >= w[1]), "{weights:?}");
                rounds += usize::from(len >= 2);
            }
            at += len;
        }
        assert!(rounds >= 4, "{rounds}");
    }

    #[test]
    fn a_lean_hierarchy_gives_the_same_edge_order() {
        // The dense random graph of the lean layout's test, its edges in
        // both directions and some twice, so that merged edges have weights
        // and rounds; and more than 4,095 edges on one pair, past what a key
        // tells apart.
        let n = 3000u64;
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let (mut src, mut dst): (Vec<u32>, Vec<u32>) =
            (0..45_000).map(|_| (next() as u32, next() as u32)).unzip();
        for e in 0..9_000 {
            src.push(dst[5 * e]);
            dst.push(src[5 * e]);
        }
        src.extend([7; 5000]);
        dst.extend([11; 5000]);
        let t = Topology::build(n as usize, src.clone(), dst.clone());
        let adj = t.undirected();
        // A self-loop, and an edge off the end of the graph.
        src.extend([5, n as u32 + 3]);
        dst.extend([5, 0]);
        let params = MultilevelParams {
            coarsest_nodes: 20,
            ..MultilevelParams::default()
        };
        let mut full = Fa2Multilevel::new(adj, params);
        let mut lean = Fa2Multilevel::new_lean(adj, params);
        assert!(lean.coarse[0].lacks_weights() && lean.coarse[1].lacks_weights());
        // The weights counted again are the ones the level was built with.
        let mut ancestor: Vec<u32> = (0..n as u32).collect();
        for l in 1..=full.coarse.len() {
            let parent = lean.coarse[l - 1].groups();
            for a in ancestor.iter_mut().filter(|a| **a != NONE) {
                *a = parent[*a as usize];
            }
            let mut counts = vec![9u16; lean.coarse[l - 1].entries];
            lean.entry_counts(adj, &ancestor, l, &mut counts);
            let built: Vec<u16> = full.coarse[l - 1]
                .weights
                .iter()
                .map(|w| w.min(KEY_WEIGHTS as f32) as u16)
                .collect();
            assert_eq!(counts, built, "level {l}");
        }
        let order = full.edge_order(adj, &src, &dst);
        assert!(is_permutation(&order, src.len()));
        assert_eq!(lean.edge_order(adj, &src, &dst), order);
        // Read a run at a time, its keys sorted by the caller: the same.
        let mut read = |start: usize, s: &mut [u32], d: &mut [u32]| {
            s.copy_from_slice(&src[start..start + s.len()]);
            d.copy_from_slice(&dst[start..start + d.len()]);
        };
        let mut keys = vec![7u64; src.len()];
        lean.edge_order_keys(adj, src.len(), &mut read, &mut keys);
        keys.sort_unstable();
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
        let sorted: Vec<u32> = keys.into_iter().map(edge_of_key).collect();
        assert_eq!(sorted, order);
        // Taking the order changes nothing the layout reads.
        full.step(adj, 40.0);
        lean.step(adj, 40.0);
        assert_eq!(full.positions(), lean.positions());
        // Once the layout is down a level the hierarchy above is gone: both
        // give the order by degree, and neither reads what was let go.
        full.step(adj, f64::INFINITY);
        lean.step(adj, f64::INFINITY);
        let by_degree = edge_order_by_degree(adj, &src, &dst);
        assert_eq!(full.edge_order(adj, &src, &dst), by_degree);
        assert_eq!(lean.edge_order(adj, &src, &dst), by_degree);
    }

    #[test]
    fn the_hash_of_an_edge_index_can_be_read_back() {
        for e in (0..1_000_000u32).chain([u32::MAX, u32::MAX - 1, 1 << 31]) {
            assert_eq!(unmix32(mix32(e)), e);
        }
        // Not the index's order.
        assert!(mix32(1) > mix32(2) || mix32(2) > mix32(3));
    }

    #[test]
    fn edges_that_join_nothing_come_last() {
        let (topo, mut src, mut dst) = barbell();
        let params = MultilevelParams {
            coarsest_nodes: 2,
            ..Default::default()
        };
        let adj = topo.undirected();
        let state = Fa2Multilevel::new(adj, params);
        // A self-loop and an edge off the end of the graph.
        src.extend([4, 99]);
        dst.extend([4, 0]);
        let m = src.len();
        let order = state.edge_order(adj, &src, &dst);
        assert!(is_permutation(&order, m));
        let mut last = order[m - 2..].to_vec();
        last.sort_unstable();
        assert_eq!(last, [m as u32 - 2, m as u32 - 1]);
        assert!(state.edge_order(adj, &[], &[]).is_empty());
    }

    #[test]
    fn without_a_hierarchy_the_edges_at_the_largest_nodes_come_first() {
        // A star of 5 and a path: the star's edges, then the path's.
        let src = vec![10, 11, 0, 0, 0, 0, 0, 12];
        let dst = vec![11, 12, 1, 2, 3, 4, 5, 13];
        let topo = Topology::build(14, src.clone(), dst.clone());
        let order = edge_order_by_degree(topo.undirected(), &src, &dst);
        assert!(is_permutation(&order, 8));
        let mut first = order[..5].to_vec();
        first.sort_unstable();
        assert_eq!(first, [2, 3, 4, 5, 6]);
        assert_eq!(order, edge_order_by_degree(topo.undirected(), &src, &dst));
    }

    /// A coarse level's rows, by sorting every edge: what [`coarse_rows`]
    /// gives without sorting any (#525).
    fn rows_by_sorting(
        adj: &UndirectedCsr,
        weights: Option<&[f32]>,
        group: &[u32],
        nc: usize,
    ) -> (Vec<u64>, Vec<u32>, Vec<f32>) {
        let mut edges: Vec<Vec<(u32, f32)>> = vec![Vec::new(); nc];
        for u in 0..group.len() {
            let base = adj.offsets[u] as usize;
            for (e, &v) in adj.neighbors(u as u32).iter().enumerate() {
                let (gu, gv) = (group[u], group[v as usize]);
                if gu != NONE && gu != gv {
                    edges[gu as usize].push((gv, weights.map_or(1.0, |w| w[base + e])));
                }
            }
        }
        let (mut offsets, mut targets, mut sums) = (vec![0u64], Vec::new(), Vec::new());
        for row in &mut edges {
            // Stable: a target's weights are added in the order they came.
            row.sort_by_key(|&(t, _)| t);
            for &(t, w) in row.iter() {
                if targets.len() as u64 > *offsets.last().unwrap() && targets.last() == Some(&t) {
                    *sums.last_mut().unwrap() += w;
                } else {
                    targets.push(t);
                    sums.push(w);
                }
            }
            offsets.push(targets.len() as u64);
        }
        (offsets, targets, sums)
    }

    #[test]
    fn a_coarse_levels_rows_are_the_sorted_ones_whichever_way_they_are_ordered() {
        // A dense graph, whose rows are read off the bitmap; and a ring of
        // 600,000 nodes with chords, whose coarse nodes have a few targets
        // each among 300,000: those rows are sorted (their targets are
        // fewer than an eighth of the summary's 74 words).
        let dense = random_edges(300, 9_000);
        let mut ring: (Vec<u32>, Vec<u32>) =
            (0..600_000u32).map(|i| (i, (i + 1) % 600_000)).unzip();
        let chords = random_edges(600_000, 50_000);
        ring.0.extend(chords.0);
        ring.1.extend(chords.1);
        for (n, (src, dst), sorted) in [(300usize, dense, false), (600_000, ring, true)] {
            let adj = UndirectedCsr::from_edges(n, &src, &dst);
            let mut levels = 0;
            let (mut a, mut w, mut m) = (adj, None::<Vec<f32>>, None::<Vec<f32>>);
            loop {
                let mass = m.clone().unwrap_or_else(|| degree_mass(&a));
                let (group, nc) = group_nodes(&a, w.as_deref(), &mass);
                if nc < 2 {
                    break;
                }
                if levels == 0 {
                    let few = nc.div_ceil(64).div_ceil(64) > 8 * 3;
                    assert_eq!(few, sorted, "{nc} coarse nodes");
                }
                let next = contract(&a, w.as_deref(), &mass, &group, nc);
                let (offsets, targets, sums) = rows_by_sorting(&a, w.as_deref(), &group, nc);
                assert_eq!(next.adj.offsets, offsets);
                assert_eq!(next.adj.targets, targets);
                assert_eq!(next.weights, sums);
                levels += 1;
                if levels == 4 {
                    break;
                }
                (a, w, m) = (next.adj, Some(next.weights), Some(next.mass));
            }
            assert!(levels >= 2, "{levels} levels of {n} nodes");
        }
    }

    #[test]
    fn the_empty_graph_and_one_node_are_handled() {
        let p = MultilevelParams::default();
        assert_eq!(
            layout_fa2_multilevel(&Topology::build(0, vec![], vec![]), None, p)
                .0
                .len(),
            0
        );
        let one = layout_fa2_multilevel(&Topology::build(1, vec![], vec![]), None, p);
        assert!(one.0[0].is_finite());
        // Only isolated nodes: nothing to coarsen.
        let iso = layout_fa2_multilevel(&Topology::build(5000, vec![], vec![]), None, p);
        assert!(iso.0.iter().all(|v| v.is_finite()));
    }
}
