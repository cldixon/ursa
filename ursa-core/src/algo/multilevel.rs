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

    /// Let go of everything but the counts: the layout is past this level,
    /// and nothing reads it again.
    fn release(&mut self) {
        self.adj = UndirectedCsr {
            offsets: Vec::new(),
            targets: Vec::new(),
        };
        self.weights = Vec::new();
        self.mass = Vec::new();
        self.member_offsets = Vec::new();
        self.members = Vec::new();
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
            .map_init(Vec::<(u32, f32)>::new, |scratch, c| {
                scratch.clear();
                let mut m = 0.0f32;
                for &u in &members[member_offsets[c] as usize..member_offsets[c + 1] as usize] {
                    m += mass[u as usize];
                    let base = adj.offsets[u as usize] as usize;
                    for (e, &v) in adj.neighbors(u).iter().enumerate() {
                        // A grouped node's neighbours are all grouped: only
                        // isolated nodes are dropped.
                        let gv = group[v as usize];
                        if gv as usize != c {
                            scratch.push((gv, weights.map_or(1.0, |w| w[base + e])));
                        }
                    }
                }
                scratch.sort_by_key(|&(t, _)| t);
                let mut targets: Vec<u32> = Vec::new();
                let mut ws: Vec<f32> = Vec::new();
                for &(t, w) in scratch.iter() {
                    if targets.last() == Some(&t) {
                        *ws.last_mut().unwrap() += w;
                    } else {
                        targets.push(t);
                        ws.push(w);
                    }
                }
                (targets, ws, m)
            })
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

    /// A position for every input node now, uncentered.
    pub fn positions(&self) -> (&[f32], &[f32]) {
        (&self.x, &self.y)
    }

    /// The positions, centered on the origin, as [`layout_fa2_multilevel`]
    /// returns them.
    pub fn into_positions(self) -> Positions {
        let (mut x, mut y) = (self.x, self.y);
        center(&mut x, &mut y);
        (x, y)
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
