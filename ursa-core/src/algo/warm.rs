//! Warm ForceAtlas2: a layout that grows without moving what is drawn.
//!
//! A graph view that expands a node adds a few neighbours to a drawing the
//! user has already read. Laying the grown graph out afresh would move
//! everything; so would stepping it from where it is, since every node would
//! give way to the new ones at once. A warm start does three things instead
//! ([`fa2_grow`]):
//!
//! 1. **Places the new nodes near where they belong** ([`place_new_nodes`]):
//!    on a golden-angle spiral around the node they came from, as the
//!    multilevel prolongation places a group's members around their parent
//!    ([`super::Fa2Multilevel`]), so no two start at one point.
//! 2. **Holds the old ones**: pinned nodes are fixed
//!    ([`Fa2State::set_fixed`]) and do not move by a bit, and so are the
//!    nodes the new ones came from and everything more than a couple of
//!    hops away; the old nodes in between get ten times the inertia
//!    ([`Fa2State::set_inertia`]), so they give way a little. All of them
//!    still repel and attract, which is what the new nodes settle against.
//!    [`WarmParams`] says why each is held, with the measurements.
//! 3. **Runs a short, fixed schedule** ([`warm_iterations`]): 60 iterations
//!    for a small addition.
//!
//! # Determinism
//!
//! As in [`super::layout_fa2`]. Placement is serial and in index order, and
//! uses nothing but `+ - * /` and `sqrt`; the iterations are [`Fa2State`]'s.
//! So the same graph grown by the same additions in the same order has the
//! same bits on any thread count, and (LinLog aside) on any platform.

use std::collections::VecDeque;

use super::layout::{Fa2State, Positions};
use super::multilevel::{degree_mass, ring_of, rotate, NONE};
use crate::topology::UndirectedCsr;

/// Parameters for [`fa2_grow`].
#[derive(Debug, Clone, Copy)]
pub struct WarmParams {
    /// Iterations to run; `None` is [`warm_iterations`]' schedule.
    pub iterations: Option<u32>,
    /// How many times harder than a new node an old, unpinned one is to
    /// move. 1 lets the old nodes move as freely as the new.
    pub inertia: f32,
    /// The placement spiral's radius for one new node, in mean edge lengths
    /// of the drawing; `n` nodes around one parent take `√n` times that.
    pub spread: f32,
    /// Fix the nodes the new ones came from (their parents, as
    /// [`place_new_nodes`] finds them) for this schedule. An expanded node
    /// gains mass with its new neighbours, ForceAtlas2's mass being
    /// `deg + 1`, so everything repels it harder than a moment ago and it
    /// sets off for the edge of the drawing: 18% of the view's width in 60
    /// iterations for a node of degree 30 that gains 16, in a 128-node
    /// drawing (`examples/fa2_multilevel.rs grow`). It is also the node the
    /// user has just clicked. Held, its new neighbours settle around it
    /// where it is.
    pub hold_parents: bool,
    /// How many hops from a new node an old node may be and still move:
    /// those further off are fixed for this schedule. 2 is the new nodes'
    /// neighbours and theirs, so the nodes around an expanded node, which
    /// are the ones that have to make room. `u32::MAX` is every node of a
    /// component that gained one.
    ///
    /// A finished layout is not at rest: ForceAtlas2 keeps moving the
    /// leaves of a drawing outwards long after its shape has settled, so
    /// 60 more iterations move nodes that have nothing to do with what was
    /// added, at any inertia (2% of the view's width on average and 11% at
    /// most for the 128-node R-MAT graph, with one node added;
    /// `examples/fa2_multilevel.rs grow`, `GROW_REACH`). The reach is what
    /// keeps the rest of the drawing where the user left it.
    pub reach: u32,
    /// With more old nodes than this, they are all fixed, not only the
    /// pinned ones. A fixed node costs an iteration nothing
    /// ([`Fa2State::set_fixed`]), so growing a large drawing costs what the
    /// new nodes do, not what the drawing does; and in a large drawing a
    /// handful of new nodes should move nothing anyway.
    pub freeze_above: usize,
}

impl Default for WarmParams {
    fn default() -> Self {
        WarmParams {
            iterations: None,
            inertia: 10.0,
            spread: 0.2,
            hold_parents: true,
            reach: 2,
            freeze_above: 5_000,
        }
    }
}

/// Iterations for a warm start that adds `n_new` nodes to `n_old`: 60 for a
/// small addition (up to 100 nodes, or a tenth of the drawing), which is
/// what a new node needs to find its place among held ones; 200 for a larger
/// one, which rearranges the drawing and needs about what a level of the
/// multilevel layout does.
pub fn warm_iterations(n_old: usize, n_new: usize) -> u32 {
    if n_new == 0 {
        0
    } else if n_new <= (n_old / 10).max(100) {
        60
    } else {
        200
    }
}

/// Mean length of the edges among the first `x.len()` nodes of `adj`, over a
/// strided sample of them, summed in index order; `None` with no such edge.
fn mean_old_edge_length(adj: &UndirectedCsr, x: &[f32], y: &[f32]) -> Option<f32> {
    let n = x.len();
    let stride = n.div_ceil(4096).max(1);
    let (mut sum, mut count) = (0.0f64, 0u64);
    for i in (0..n).step_by(stride) {
        for &j in adj.neighbors(i as u32) {
            let j = j as usize;
            if j < n {
                let (dx, dy) = (x[i] - x[j], y[i] - y[j]);
                sum += f64::from((dx * dx + dy * dy).sqrt());
                count += 1;
            }
        }
    }
    (count > 0).then(|| (sum / count as f64) as f32)
}

/// The parent of each new node of `adj` (those from `n_old` on), as
/// [`place_new_nodes`] defines it; `NONE` for a node tied to nothing old.
fn resolve_parents(adj: &UndirectedCsr, n_old: usize, parents: Option<&[u32]>) -> Vec<u32> {
    let n_new = adj.offsets.len().saturating_sub(1) - n_old;
    if let Some(p) = parents {
        assert_eq!(p.len(), n_new, "one parent per new node");
    }
    let mut parent = vec![NONE; n_new];
    let mut queue = VecDeque::new();
    for j in 0..n_new {
        let given = parents.map_or(NONE, |p| p[j]);
        parent[j] = if (given as usize) < n_old {
            given
        } else {
            // Neighbours are sorted, so the first is the lowest.
            match adj.neighbors((n_old + j) as u32).first() {
                Some(&v) if (v as usize) < n_old => v,
                _ => NONE,
            }
        };
        if parent[j] != NONE {
            queue.push_back(j);
        }
    }
    // The rest inherit through new nodes, breadth first from those that
    // have one, in index order.
    while let Some(j) = queue.pop_front() {
        for &v in adj.neighbors((n_old + j) as u32) {
            if (v as usize) >= n_old && parent[v as usize - n_old] == NONE {
                parent[v as usize - n_old] = parent[j];
                queue.push_back(v as usize - n_old);
            }
        }
    }

    parent
}

/// Where to start the nodes a graph has gained.
///
/// `adj` is the grown graph; its first `x.len()` nodes are the ones already
/// drawn, at `x`, `y`, and the rest are new. Returns a position for each new
/// node, in index order.
///
/// Each new node is placed around its **parent**: `parents[j]` for new node
/// `x.len() + j` if that names an old node, else (`parents` is `None`, the
/// entry is `u32::MAX`, or it is not an old node) its lowest-indexed old
/// neighbour, else the parent of the nearest new node that has one, by hops
/// through new nodes. The children of one parent go on a phyllotactic spiral
/// around it, in index order, of radius `spread · √children` mean edge
/// lengths, each parent's spiral starting a golden angle on from the last
/// one's. A new node tied to nothing old goes on a ring around the drawing,
/// as the multilevel layout places isolated nodes.
pub fn place_new_nodes(
    adj: &UndirectedCsr,
    x: &[f32],
    y: &[f32],
    parents: Option<&[u32]>,
    spread: f32,
) -> Positions {
    let n = adj.offsets.len().saturating_sub(1);
    let n_old = x.len();
    assert_eq!(n_old, y.len(), "one y per x");
    assert!(n_old <= n, "the grown graph has every old node");
    let n_new = n - n_old;

    let parent = resolve_parents(adj, n_old, parents);

    // Children grouped by parent, in index order within each (a stable sort).
    let mut order: Vec<u32> = (0..n_new as u32)
        .filter(|&j| parent[j as usize] != NONE)
        .collect();
    order.sort_by_key(|&j| parent[j as usize]);

    let unit = spread * mean_old_edge_length(adj, x, y).unwrap_or(1.0);
    let mut nx = vec![0.0f32; n_new];
    let mut ny = vec![0.0f32; n_new];
    let (mut cx, mut cy) = (1.0f32, 0.0f32);
    let mut at = 0;
    while at < order.len() {
        let p = parent[order[at] as usize];
        let s = order[at..]
            .iter()
            .take_while(|&&j| parent[j as usize] == p)
            .count();
        let radius = unit * (s as f32).sqrt();
        let (mut dx, mut dy) = (cx, cy);
        for (k, &j) in order[at..at + s].iter().enumerate() {
            // Never at the parent itself: the first child is half a step out.
            let r = radius * ((k as f32 + 0.5) / s as f32).sqrt();
            nx[j as usize] = x[p as usize] + r * dx;
            ny[j as usize] = y[p as usize] + r * dy;
            (dx, dy) = rotate(dx, dy);
        }
        (cx, cy) = rotate(cx, cy);
        at += s;
    }

    // Tied to nothing old: a ring around the drawing, as `multilevel`'s
    // prolongation places the nodes it dropped.
    if order.len() < n_new {
        let (ox, oy, ring) = ring_of(x, y);
        let (mut dx, mut dy) = (1.0f32, 0.0f32);
        for j in 0..n_new {
            if parent[j] != NONE {
                continue;
            }
            let m = (n_old + j) as u32;
            let phase = (m.wrapping_mul(0x9E37_79B1) >> 8) as f32 / 16_777_216.0;
            let r = ring * (0.95 + 0.1 * phase);
            nx[j] = ox + r * dx;
            ny[j] = oy + r * dy;
            (dx, dy) = rotate(dx, dy);
        }
    }
    (nx, ny)
}

/// Grow a layout: make `state` lay out `adj`, a graph with every node it had
/// (same indexes, positions kept) and new ones after them, and return how
/// many iterations to [`Fa2State::step`] it with `adj`.
///
/// The new nodes are placed by [`place_new_nodes`] (`parents` as there).
/// Every node's mass becomes `deg + 1` in `adj`. The nodes named by `pinned`
/// (indexes into `adj`, in any order) are fixed; the other old nodes get
/// [`WarmParams::inertia`], or are fixed too past
/// [`WarmParams::freeze_above`]; the new nodes are free. Also fixed: the new
/// nodes' parents ([`WarmParams::hold_parents`]), a node with no edge, and
/// an old node more than [`WarmParams::reach`] hops from every new one. The
/// masks replace
/// whatever the state had, so a node unpinned since the last call is free
/// again, and a node new in the last call is old in this one.
///
/// Adding no nodes is allowed: the graph may have gained edges only, or
/// only the pins may have changed.
pub fn fa2_grow(
    state: &mut Fa2State,
    adj: &UndirectedCsr,
    parents: Option<&[u32]>,
    pinned: &[u32],
    params: WarmParams,
) -> u32 {
    let n = adj.offsets.len().saturating_sub(1);
    let (x, y) = state.positions();
    let n_old = x.len();
    assert!(n_old <= n, "the grown graph has every old node");
    let (nx, ny) = place_new_nodes(adj, x, y, parents, params.spread);
    state.extend(&nx, &ny, &vec![1.0; n - n_old]);
    state.set_mass(degree_mass(adj));

    let freeze = n_old > params.freeze_above;
    // Held as well, whatever the size:
    // - a node with no edge. Nothing pulls it, so ForceAtlas2 only drives
    //   it outwards, for ever (`multilevel`'s `ring_of` has the sum).
    // - an old node more than `reach` hops from every new node, which
    //   includes every component that gained none (`WarmParams::reach`).
    // Breadth first from the new nodes, so `hops` is the least distance.
    let mut hops = vec![if n == n_old { 0 } else { u32::MAX }; n];
    let mut queue: VecDeque<u32> = (n_old as u32..n as u32).collect();
    for &u in &queue {
        hops[u as usize] = 0;
    }
    while let Some(u) = queue.pop_front() {
        let next = hops[u as usize] + 1;
        if next > params.reach {
            continue;
        }
        for &v in adj.neighbors(u) {
            if hops[v as usize] == u32::MAX {
                hops[v as usize] = next;
                queue.push_back(v);
            }
        }
    }
    let touched = |i: usize| hops[i] != u32::MAX;
    let mut fixed: Vec<bool> = (0..n)
        .map(|i| (i < n_old && (freeze || !touched(i))) || adj.degree(i as u32) == 0)
        .collect();
    for &p in pinned {
        assert!((p as usize) < n, "pinned node {p} of {n}");
        fixed[p as usize] = true;
    }
    if params.hold_parents {
        for p in resolve_parents(adj, n_old, parents) {
            if p != NONE {
                fixed[p as usize] = true;
            }
        }
    }
    state.set_fixed(fixed.contains(&true).then_some(fixed));
    if params.inertia == 1.0 {
        state.set_inertia(None);
    } else {
        let mut inertia = vec![1.0f32; n];
        inertia[..n_old].fill(params.inertia);
        state.set_inertia(Some(inertia));
    }
    params
        .iterations
        .unwrap_or_else(|| warm_iterations(n_old, n - n_old))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::{Fa2Params, MultilevelParams};
    use crate::topology::Topology;

    /// A seeded pseudo-random graph: `n` nodes on a path (so it is connected)
    /// and `extra` more edges.
    fn edges(n: u32, extra: usize) -> (Vec<u32>, Vec<u32>) {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        let mut src: Vec<u32> = (0..n - 1).collect();
        let mut dst: Vec<u32> = (1..n).collect();
        for _ in 0..extra {
            src.push(next() % n);
            dst.push(next() % n);
        }
        (src, dst)
    }

    fn bits(v: &[f32]) -> Vec<u32> {
        v.iter().map(|f| f.to_bits()).collect()
    }

    /// A 500-node view, laid out, and the same graph after node `hub` is
    /// expanded: 16 new neighbours, some tied to each other and to old nodes.
    fn expanded(hub: u32) -> (Fa2State, Topology, Topology) {
        let n = 500u32;
        let (mut src, mut dst) = edges(n, 600);
        let old = Topology::build(n as usize, src.clone(), dst.clone());
        let mut state = Fa2State::new(old.undirected(), Fa2Params::default());
        state.step(old.undirected(), 300);
        for j in 0..16 {
            src.push(hub);
            dst.push(n + j);
        }
        for (a, b) in [(n, n + 1), (n + 2, n + 9), (n + 5, 17), (n + 11, 230)] {
            src.push(a);
            dst.push(b);
        }
        let grown = Topology::build(n as usize + 16, src, dst);
        (state, old, grown)
    }

    fn width(x: &[f32]) -> f32 {
        let (lo, hi) = x
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        hi - lo
    }

    #[test]
    fn fixed_nodes_do_not_move_by_a_bit_and_the_rest_do() {
        let (src, dst) = edges(200, 300);
        let topo = Topology::build(200, src, dst);
        let adj = topo.undirected();
        for theta in [0.5, 0.0] {
            let mut state = Fa2State::new(
                adj,
                Fa2Params {
                    theta,
                    ..Fa2Params::default()
                },
            );
            // Far from settled, so the forces on the fixed nodes are large.
            state.step(adj, 5);
            let fixed: Vec<bool> = (0..200).map(|i| i % 3 == 0).collect();
            state.set_fixed(Some(fixed.clone()));
            let (x0, y0) = (bits(state.positions().0), bits(state.positions().1));
            state.step(adj, 40);
            let (x1, y1) = (bits(state.positions().0), bits(state.positions().1));
            for i in 0..200 {
                if fixed[i] {
                    assert_eq!((x0[i], y0[i]), (x1[i], y1[i]), "fixed node {i} moved");
                } else {
                    assert_ne!((x0[i], y0[i]), (x1[i], y1[i]), "free node {i} stayed");
                }
            }
            assert_eq!(state.fixed(), Some(&fixed[..]));
        }
    }

    #[test]
    fn a_fixed_node_still_pushes_and_pulls() {
        // Two unconnected nodes: the free one is repelled by the fixed one.
        let topo = Topology::build(2, vec![], vec![]);
        let adj = topo.undirected();
        let params = Fa2Params {
            gravity: 0.0,
            ..Fa2Params::default()
        };
        let mut state =
            Fa2State::from_positions(params, vec![0.0, 1.0], vec![0.0, 0.0], vec![1.0, 1.0]);
        state.set_fixed(Some(vec![true, false]));
        state.step(adj, 10);
        let (x, y) = state.positions();
        assert_eq!((x[0].to_bits(), y[0].to_bits()), (0, 0));
        assert!(x[1] > 1.5, "repelled to {}", x[1]);

        // Two connected ones far apart: the free one is pulled in.
        let topo = Topology::build(2, vec![0], vec![1]);
        let adj = topo.undirected();
        let mut state =
            Fa2State::from_positions(params, vec![0.0, 50.0], vec![0.0, 0.0], vec![2.0, 2.0]);
        state.set_fixed(Some(vec![true, false]));
        state.step(adj, 30);
        let (x, _) = state.positions();
        assert_eq!(x[0].to_bits(), 0);
        assert!(x[1] < 10.0, "pulled to {}", x[1]);
    }

    #[test]
    fn masks_that_hold_nothing_change_nothing() {
        let (src, dst) = edges(300, 500);
        let topo = Topology::build(300, src, dst);
        let adj = topo.undirected();
        let mut plain = Fa2State::new(adj, Fa2Params::default());
        let mut masked = plain.clone();
        masked.set_fixed(Some(vec![false; 300]));
        masked.set_inertia(Some(vec![1.0; 300]));
        plain.step(adj, 50);
        masked.step(adj, 50);
        assert_eq!(bits(plain.positions().0), bits(masked.positions().0));
        assert_eq!(bits(plain.positions().1), bits(masked.positions().1));
    }

    #[test]
    fn inertia_slows_a_node_down() {
        let (src, dst) = edges(100, 150);
        let topo = Topology::build(100, src, dst);
        let adj = topo.undirected();
        let start = Fa2State::new(adj, Fa2Params::default());
        let mut light = start.clone();
        let mut heavy = start.clone();
        let mut inertia = vec![1.0f32; 100];
        inertia[..50].fill(10.0);
        heavy.set_inertia(Some(inertia));
        light.step(adj, 1);
        heavy.step(adj, 1);
        let moved = |s: &Fa2State, i: usize| {
            let (x, y) = s.positions();
            let (x0, y0) = start.positions();
            ((x[i] - x0[i]).powi(2) + (y[i] - y0[i]).powi(2)).sqrt()
        };
        for i in 0..50 {
            let (l, h) = (moved(&light, i), moved(&heavy, i));
            assert!(h > 0.0 && h < l * 0.2, "node {i}: {h} against {l}");
        }
    }

    #[test]
    fn extend_keeps_the_old_positions_and_the_masks() {
        let topo = Topology::build(3, vec![0, 1], vec![1, 2]);
        let mut state = Fa2State::new(topo.undirected(), Fa2Params::default());
        state.step(topo.undirected(), 10);
        state.set_fixed(Some(vec![true, false, false]));
        state.set_inertia(Some(vec![4.0; 3]));
        let (x0, y0) = (state.positions().0.to_vec(), state.positions().1.to_vec());
        state.extend(&[7.0, 8.0], &[-1.0, -2.0], &[1.0, 3.0]);
        let (x, y) = state.positions();
        assert_eq!(bits(&x[..3]), bits(&x0));
        assert_eq!(bits(&y[..3]), bits(&y0));
        assert_eq!((&x[3..], &y[3..]), (&[7.0, 8.0][..], &[-1.0, -2.0][..]));
        assert_eq!(state.fixed(), Some(&[true, false, false, false, false][..]));
        assert_eq!(state.inertia(), Some(&[4.0, 4.0, 4.0, 1.0, 1.0][..]));
        assert_eq!(state.mass()[3..], [1.0, 3.0]);
        // It steps the grown graph.
        let grown = Topology::build(5, vec![0, 1, 2, 3], vec![1, 2, 3, 4]);
        state.step(grown.undirected(), 5);
        assert_eq!(state.positions().0[0].to_bits(), x0[0].to_bits());
        assert!(state.positions().0.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn new_nodes_start_around_their_parents() {
        // Old: a path 0-1-2-3, on a line 10 apart. New: 4, 5, 6 on node 1;
        // 7 on node 3 and 0 (lowest wins); 8 on 7 only (inherits 0); 9 alone.
        let src = vec![0, 1, 2, 1, 1, 1, 7, 7, 8];
        let dst = vec![1, 2, 3, 4, 5, 6, 3, 0, 7];
        let topo = Topology::build(10, src, dst);
        let adj = topo.undirected();
        let (x, y) = (vec![0.0, 10.0, 20.0, 30.0], vec![0.0; 4]);
        let (nx, ny) = place_new_nodes(adj, &x, &y, None, 0.2);
        assert_eq!(nx.len(), 6);
        let from = |j: usize, p: usize| ((nx[j] - x[p]).powi(2) + (ny[j] - y[p]).powi(2)).sqrt();
        // Mean edge length 10, spread 0.2: one child within 2, three within 2√3.
        for j in 0..3 {
            assert!(from(j, 1) > 0.5 && from(j, 1) <= 2.0 * 3.0f32.sqrt() + 1e-4);
        }
        assert!(from(3, 0) > 0.5 && from(3, 0) <= 2.0 * 2.0f32.sqrt() + 1e-4);
        assert!(from(4, 0) > 0.5 && from(4, 0) <= 2.0 * 2.0f32.sqrt() + 1e-4);
        // Nothing coincides.
        for a in 0..6 {
            for b in a + 1..6 {
                let d = ((nx[a] - nx[b]).powi(2) + (ny[a] - ny[b]).powi(2)).sqrt();
                assert!(d > 0.5, "new nodes {a} and {b} are {d} apart");
            }
        }
        // The lone one is outside the drawing.
        assert!(from(5, 1) > 20.0, "ring at {}", from(5, 1));

        // A given parent wins over the neighbours; one that is not an old
        // node is inferred.
        let given = [3, NONE, 99, NONE, NONE, 2];
        let (gx, gy) = place_new_nodes(adj, &x, &y, Some(&given), 0.2);
        assert!(((gx[0] - 30.0).powi(2) + gy[0].powi(2)).sqrt() <= 2.0 + 1e-4);
        assert!(((gx[2] - 10.0).powi(2) + gy[2].powi(2)).sqrt() <= 2.0 * 2.0f32.sqrt() + 1e-4);
        assert!(((gx[5] - 20.0).powi(2) + gy[5].powi(2)).sqrt() <= 2.0 + 1e-4);
        // And the same call gives the same bits.
        let again = place_new_nodes(adj, &x, &y, Some(&given), 0.2);
        assert_eq!((bits(&gx), bits(&gy)), (bits(&again.0), bits(&again.1)));
    }

    #[test]
    fn the_schedule_is_short_for_a_small_addition() {
        assert_eq!(warm_iterations(500, 0), 0);
        assert_eq!(warm_iterations(500, 16), 60);
        assert_eq!(warm_iterations(10, 100), 60);
        assert_eq!(warm_iterations(100_000, 5_000), 60);
        assert_eq!(warm_iterations(500, 101), 200);
    }

    /// The issue's acceptance, at the kernel: expand a node by 16 in a
    /// 500-node view.
    #[test]
    fn expanding_a_node_moves_little_and_pinned_nodes_not_at_all() {
        let hub = 42u32;
        let (mut state, _old, grown) = expanded(hub);
        let adj = grown.undirected();
        let (x0, y0) = (state.positions().0.to_vec(), state.positions().1.to_vec());
        let view = width(&x0).max(width(&y0));
        // The expanded node is held without being pinned.
        let pinned = [3u32, 77, 499, hub];
        let iterations = fa2_grow(&mut state, adj, None, &pinned[..3], WarmParams::default());
        assert_eq!(iterations, 60);
        assert_eq!(state.positions().0.len(), 516);
        state.step(adj, iterations);
        let (x, y) = state.positions();
        assert!(x.iter().chain(y).all(|v| v.is_finite()));

        for &p in &pinned {
            let p = p as usize;
            assert_eq!(
                (x[p].to_bits(), y[p].to_bits()),
                (x0[p].to_bits(), y0[p].to_bits()),
                "pinned node {p} moved"
            );
        }
        let mut total = 0.0f64;
        let mut worst = 0.0f32;
        for i in 0..500 {
            let d = ((x[i] - x0[i]).powi(2) + (y[i] - y0[i]).powi(2)).sqrt();
            total += f64::from(d);
            worst = worst.max(d);
        }
        let mean = (total / 496.0) as f32;
        assert!(mean > 0.0, "unpinned old nodes give way a little");
        assert!(
            mean < 0.05 * view,
            "old nodes moved {mean} on average, in a view {view} wide"
        );
        assert!(
            worst < 0.25 * view,
            "an old node moved {worst}, in a view {view} wide"
        );

        // The new nodes are near the node they came from, and apart from
        // each other and from everything else.
        let h = hub as usize;
        let mut nearest = f32::MAX;
        for i in 500..516 {
            let d = ((x[i] - x[h]).powi(2) + (y[i] - y[h]).powi(2)).sqrt();
            assert!(d < 0.25 * view, "new node {i} is {d} from its parent");
            for j in 0..516 {
                if j != i {
                    let d = ((x[i] - x[j]).powi(2) + (y[i] - y[j]).powi(2)).sqrt();
                    nearest = nearest.min(d);
                }
            }
        }
        assert!(
            nearest > 0.002 * view,
            "a new node is {nearest} from another, in a view {view} wide"
        );
    }

    #[test]
    fn the_expanded_node_can_be_left_free() {
        let hub = 42usize;
        let (mut state, _old, grown) = expanded(hub as u32);
        let adj = grown.undirected();
        let (x0, y0) = (state.positions().0[hub], state.positions().1[hub]);
        let params = WarmParams {
            hold_parents: false,
            reach: u32::MAX,
            ..WarmParams::default()
        };
        let n = fa2_grow(&mut state, adj, None, &[], params);
        assert_eq!(state.fixed(), None, "connected, unpinned: nothing is fixed");
        state.step(adj, n);
        let (x, y) = state.positions();
        assert_ne!((x[hub], y[hub]), (x0, y0));
    }

    #[test]
    fn a_component_that_gained_nothing_is_held() {
        // A triangle and, apart from it, a pair and a lone node; the
        // triangle's node 0 gains two neighbours.
        let old = Topology::build(6, vec![0, 1, 2, 3], vec![1, 2, 0, 4]);
        let mut state = Fa2State::new(old.undirected(), Fa2Params::default());
        state.step(old.undirected(), 30);
        let before = (bits(state.positions().0), bits(state.positions().1));
        let grown = Topology::build(8, vec![0, 1, 2, 3, 0, 0], vec![1, 2, 0, 4, 6, 7]);
        let params = WarmParams {
            hold_parents: false,
            ..WarmParams::default()
        };
        let n = fa2_grow(&mut state, grown.undirected(), None, &[], params);
        assert_eq!(
            state.fixed(),
            Some(&[false, false, false, true, true, true, false, false][..])
        );
        state.step(grown.undirected(), n);
        let after = (bits(state.positions().0), bits(state.positions().1));
        for i in 0..6 {
            let same = (before.0[i], before.1[i]) == (after.0[i], after.1[i]);
            assert_eq!(same, i >= 3, "node {i}");
        }
        // Gaining edges only holds nothing but the lone node.
        let wired = Topology::build(8, vec![0, 1, 2, 3, 0, 0, 3], vec![1, 2, 0, 4, 6, 7, 2]);
        let n = fa2_grow(&mut state, wired.undirected(), None, &[], params);
        assert_eq!(n, 0, "no new node, no schedule: the caller says how long");
        assert_eq!(
            state.fixed(),
            Some(&[false, false, false, false, false, true, false, false][..])
        );
    }

    #[test]
    fn only_the_nodes_near_the_new_ones_are_free() {
        // A path 0-1-2-3-4-5; node 0 gains node 6.
        let path = |n: u32| ((0..n - 1).collect::<Vec<_>>(), (1..n).collect::<Vec<_>>());
        let (src, dst) = path(6);
        let old = Topology::build(6, src.clone(), dst.clone());
        let start = {
            let mut state = Fa2State::new(old.undirected(), Fa2Params::default());
            state.step(old.undirected(), 30);
            state
        };
        let (mut src, mut dst) = (src, dst);
        src.push(0);
        dst.push(6);
        let grown = Topology::build(7, src, dst);
        let fixed_for = |reach: u32, hold_parents: bool| {
            let mut state = start.clone();
            let params = WarmParams {
                reach,
                hold_parents,
                ..WarmParams::default()
            };
            fa2_grow(&mut state, grown.undirected(), None, &[], params);
            state.fixed().map(<[bool]>::to_vec)
        };
        // Two hops by default: the parent and its neighbour. The parent is
        // held all the same.
        assert_eq!(
            fixed_for(2, true),
            Some(vec![true, false, true, true, true, true, false])
        );
        assert_eq!(
            fixed_for(1, false),
            Some(vec![false, true, true, true, true, true, false])
        );
        assert_eq!(
            fixed_for(0, false),
            Some(vec![true, true, true, true, true, true, false])
        );
        assert_eq!(fixed_for(u32::MAX, false), None);
        // The old nodes that are free have the inertia; the new one has none.
        let mut state = start.clone();
        fa2_grow(
            &mut state,
            grown.undirected(),
            None,
            &[],
            WarmParams::default(),
        );
        assert_eq!(
            state.inertia(),
            Some(&[10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 1.0][..])
        );
    }

    #[test]
    fn growing_is_the_same_however_it_is_stepped() {
        let run = |chunks: &[u32]| {
            let (mut state, _old, grown) = expanded(7);
            let adj = grown.undirected();
            let n = fa2_grow(&mut state, adj, None, &[1, 2], WarmParams::default());
            assert_eq!(chunks.iter().sum::<u32>(), n);
            for &c in chunks {
                state.step(adj, c);
            }
            (bits(state.positions().0), bits(state.positions().1))
        };
        assert_eq!(run(&[60]), run(&[1, 9, 20, 30]));
    }

    #[test]
    fn a_large_drawing_is_frozen_and_only_the_new_nodes_move() {
        let (mut state, _old, grown) = expanded(42);
        let adj = grown.undirected();
        let (x0, y0) = (bits(state.positions().0), bits(state.positions().1));
        let params = WarmParams {
            freeze_above: 100,
            ..WarmParams::default()
        };
        let n = fa2_grow(&mut state, adj, None, &[], params);
        let start = (state.positions().0.to_vec(), state.positions().1.to_vec());
        state.step(adj, n);
        let (x, y) = state.positions();
        assert_eq!(bits(&x[..500]), x0);
        assert_eq!(bits(&y[..500]), y0);
        for i in 500..516 {
            assert_ne!(
                (x[i], y[i]),
                (start.0[i], start.1[i]),
                "new node {i} stayed"
            );
        }
    }

    #[test]
    fn a_multilevel_layout_grows_from_where_it_ended() {
        let (src, dst) = edges(2000, 3000);
        let topo = Topology::build(2000, src.clone(), dst.clone());
        let params = MultilevelParams::default();
        let mut ml = crate::algo::Fa2Multilevel::new(topo.undirected(), params);
        ml.step(topo.undirected(), f64::INFINITY);
        let (x0, y0) = (ml.positions().0.to_vec(), ml.positions().1.to_vec());
        let mut state = Fa2State::from_positions(
            params.fa2,
            x0.clone(),
            y0.clone(),
            degree_mass(topo.undirected()),
        );
        let (mut src, mut dst) = (src, dst);
        for j in 0..30 {
            src.push(100);
            dst.push(2000 + j);
        }
        let grown = Topology::build(2030, src, dst);
        let n = fa2_grow(
            &mut state,
            grown.undirected(),
            None,
            &[],
            WarmParams::default(),
        );
        state.step(grown.undirected(), n);
        let (x, y) = state.positions();
        let view = width(&x0).max(width(&y0));
        let mean = (0..2000)
            .map(|i| f64::from(((x[i] - x0[i]).powi(2) + (y[i] - y0[i]).powi(2)).sqrt()))
            .sum::<f64>()
            / 2000.0;
        assert!((mean as f32) < 0.05 * view, "moved {mean} in {view}");
    }
}
