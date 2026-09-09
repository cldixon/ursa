//! Force-directed layout — positions as a kernel output.
//!
//! The founding observation of the visualization work: a force-directed layout is
//! an iterative fixpoint over the CSR, the same computational shape as PageRank.
//! It belongs beside the other kernels rather than in a rendering library, and it
//! surfaces through the frame algebra like everything else — positions are just
//! columns, so they filter, join and `sink_parquet` like any other value.
//!
//! # Determinism
//!
//! Positions get cached to Parquet as a workflow, so reproducibility here is more
//! than cosmetic. Two implementation choices make the output bit-identical
//! regardless of thread count, and both are load-bearing rather than incidental:
//!
//! - **Gather, never scatter.** Attraction is accumulated *per node*, by walking
//!   that node's own incident edges. The natural formulation — iterate edges and
//!   push a force to both endpoints — is a scatter, and its result depends on the
//!   order threads happen to write in. Every other kernel in this crate gathers
//!   for the same reason.
//! - **A grid built by counting sort.** The spatial index is rebuilt per iteration
//!   from a deterministic bucketing, so the order repulsion is summed in is fixed.
//!
//! Cross-platform bit-identity is *not* claimed: the force law uses only `+ - * /`
//! and `sqrt`, which are IEEE-exact, but that guarantee would end the moment a
//! transcendental (a LinLog mode's `ln`) entered the loop.
//!
//! # What this is not, yet
//!
//! Repulsion is approximated with a **uniform grid**: near cells exactly,
//! far cells through their centre of mass. That is O(n) per iteration for graphs
//! whose density is roughly even, and it is the wrong structure for a graph that
//! is violently clustered — a uniform grid cannot subdivide where the nodes
//! actually are. ForceAtlas2 with a Barnes–Hut quadtree is the successor, and the
//! seam is `repulse`: it is the only function that needs replacing.

use crate::parallel::*;

use super::rng::DEFAULT_SEED;
use super::triangle::undirected_view;
use crate::topology::{EdgeMask, Topology, UndirectedCsr};

/// A 2-D position, in the arbitrary world units the layout converges to.
pub type Positions = (Vec<f32>, Vec<f32>);

/// Parameters for [`layout_fr`].
#[derive(Debug, Clone, Copy)]
pub struct LayoutParams {
    pub iterations: u32,
    /// Ideal edge length. Positions scale linearly with it, so it mostly decides
    /// the units the result comes out in.
    pub k: f32,
    /// Pulls the whole drawing toward the origin, which is what stops disconnected
    /// components drifting apart forever — repulsion alone would separate them
    /// without bound.
    pub gravity: f32,
    pub seed: Option<u64>,
}

impl Default for LayoutParams {
    fn default() -> Self {
        LayoutParams {
            iterations: 300,
            k: 1.0,
            gravity: 0.02,
            seed: None,
        }
    }
}

/// Deterministic initial placement on a phyllotactic spiral, jittered by `seed`.
///
/// A spiral rather than uniform random: it starts the nodes evenly spread with no
/// two coincident, which matters because two nodes at exactly the same point feel
/// infinite repulsion and shoot apart. The jitter keeps different seeds giving
/// genuinely different layouts.
fn seed_positions(n: usize, k: f32, seed: u64) -> Positions {
    let mut x = vec![0.0f32; n];
    let mut y = vec![0.0f32; n];
    // Golden angle: successive points land maximally out of phase, which is what
    // makes the spiral fill space evenly.
    let golden = std::f32::consts::PI * (3.0 - 5.0f32.sqrt());
    let radius = k * (n as f32).sqrt();
    let mut state = seed | 1;
    let mut jitter = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f32 / 16_777_216.0) - 0.5
    };
    for i in 0..n {
        let t = (i as f32 + 0.5) / n as f32;
        let r = radius * t.sqrt();
        let a = golden * i as f32;
        x[i] = r * a.cos() + jitter() * k * 0.1;
        y[i] = r * a.sin() + jitter() * k * 0.1;
    }
    (x, y)
}

/// A uniform bucketing of the current positions, rebuilt each iteration.
struct Grid {
    cols: usize,
    rows: usize,
    cell: f32,
    min_x: f32,
    min_y: f32,
    /// Prefix-summed cell offsets into `items`; length `cols * rows + 1`.
    starts: Vec<u32>,
    items: Vec<u32>,
    /// Per-cell centre of mass and population, for the far-field approximation.
    sum_x: Vec<f32>,
    sum_y: Vec<f32>,
    count: Vec<f32>,
}

impl Grid {
    /// Counting sort, the same two-pass shape as the CSR build — and, like it,
    /// chosen partly because a fixed bucket order is what makes the summation
    /// below reproducible.
    fn build(x: &[f32], y: &[f32], k: f32) -> Grid {
        let n = x.len();
        let (mut min_x, mut min_y) = (f32::INFINITY, f32::INFINITY);
        let (mut max_x, mut max_y) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
        for i in 0..n {
            min_x = min_x.min(x[i]);
            max_x = max_x.max(x[i]);
            min_y = min_y.min(y[i]);
            max_y = max_y.max(y[i]);
        }
        // A cell about two ideal edge-lengths across: big enough that most pairs
        // fall in the far field, small enough that the near field stays local.
        let cell = (k * 2.0).max(1e-4);
        let cols = (((max_x - min_x) / cell).ceil() as usize + 1).clamp(1, 4096);
        let rows = (((max_y - min_y) / cell).ceil() as usize + 1).clamp(1, 4096);

        let cell_of = |i: usize| -> usize {
            let cx = (((x[i] - min_x) / cell) as usize).min(cols - 1);
            let cy = (((y[i] - min_y) / cell) as usize).min(rows - 1);
            cy * cols + cx
        };

        let mut starts = vec![0u32; cols * rows + 1];
        for i in 0..n {
            starts[cell_of(i) + 1] += 1;
        }
        for c in 0..cols * rows {
            starts[c + 1] += starts[c];
        }
        let mut cursor = starts[..cols * rows].to_vec();
        let mut items = vec![0u32; n];
        let mut sum_x = vec![0.0f32; cols * rows];
        let mut sum_y = vec![0.0f32; cols * rows];
        let mut count = vec![0.0f32; cols * rows];
        for i in 0..n {
            let c = cell_of(i);
            items[cursor[c] as usize] = i as u32;
            cursor[c] += 1;
            sum_x[c] += x[i];
            sum_y[c] += y[i];
            count[c] += 1.0;
        }
        Grid {
            cols,
            rows,
            cell,
            min_x,
            min_y,
            starts,
            items,
            sum_x,
            sum_y,
            count,
        }
    }

    #[inline]
    fn coords(&self, xi: f32, yi: f32) -> (usize, usize) {
        (
            (((xi - self.min_x) / self.cell) as usize).min(self.cols - 1),
            (((yi - self.min_y) / self.cell) as usize).min(self.rows - 1),
        )
    }
}

/// Repulsive force on node `i` from every other node.
///
/// Near cells (the 3×3 neighbourhood) contribute exactly, node by node; every
/// other occupied cell contributes once, through its centre of mass weighted by
/// its population. That is the Barnes–Hut bargain at a single fixed level of
/// detail — and the reason a quadtree eventually wins, since it can choose the
/// level per region instead of taking one for the whole graph.
///
/// The order of summation is fixed by the grid's layout, not by thread
/// scheduling, so the result is reproducible.
#[inline]
fn repulse(grid: &Grid, x: &[f32], y: &[f32], i: usize, k2: f32) -> (f32, f32) {
    let (xi, yi) = (x[i], y[i]);
    let (cx, cy) = grid.coords(xi, yi);
    let (mut fx, mut fy) = (0.0f32, 0.0f32);

    for gy in 0..grid.rows {
        for gx in 0..grid.cols {
            let c = gy * grid.cols + gx;
            if grid.count[c] == 0.0 {
                continue;
            }
            let near = gx.abs_diff(cx) <= 1 && gy.abs_diff(cy) <= 1;
            if near {
                for s in grid.starts[c]..grid.starts[c + 1] {
                    let j = grid.items[s as usize] as usize;
                    if j == i {
                        continue;
                    }
                    let (dx, dy) = (xi - x[j], yi - y[j]);
                    // Floored so two coincident nodes get a large but finite push
                    // rather than an infinity that poisons every later iteration.
                    let d2 = (dx * dx + dy * dy).max(1e-4);
                    let f = k2 / d2;
                    fx += dx * f;
                    fy += dy * f;
                }
            } else {
                let m = grid.count[c];
                let (dx, dy) = (xi - grid.sum_x[c] / m, yi - grid.sum_y[c] / m);
                let d2 = (dx * dx + dy * dy).max(1e-4);
                let f = k2 * m / d2;
                fx += dx * f;
                fy += dy * f;
            }
        }
    }
    (fx, fy)
}

/// Fruchterman–Reingold layout: `(x, y)` per node, dense-indexed.
///
/// A `mask` restricts the attractive forces to the kept edges, so a subgraph view
/// lays out as the subgraph it is — repulsion still involves every node, because
/// a node with no kept edges is isolated in the drawing, not absent from it.
pub fn layout_fr(topo: &Topology, mask: Option<&EdgeMask>, params: LayoutParams) -> Positions {
    let n = topo.n_nodes();
    if n == 0 {
        return (Vec::new(), Vec::new());
    }
    let k = params.k.max(1e-4);
    let (mut x, mut y) = seed_positions(n, k, params.seed.unwrap_or(DEFAULT_SEED));
    if n == 1 {
        return (x, y);
    }

    // The undirected view: an edge pulls its endpoints together regardless of
    // which way it points, and a layout that honoured direction would draw two
    // different pictures of the same graph depending on ingest order.
    let view = undirected_view(topo, mask);
    let adj: &UndirectedCsr = view.get();

    let k2 = k * k;
    // Start displacing by a tenth of the drawing's extent and cool linearly to
    // nothing, so early iterations rearrange freely and late ones only settle.
    let t0 = k * (n as f32).sqrt() * 0.1;

    for it in 0..params.iterations {
        let grid = Grid::build(&x, &y, k);
        let temp = t0 * (1.0 - it as f32 / params.iterations as f32).max(0.0);

        let step: Vec<(f32, f32)> = (0..n)
            .into_par_iter()
            .map(|i| {
                let (mut fx, mut fy) = repulse(&grid, &x, &y, i, k2);

                // Attraction, gathered: node i walks its own neighbours. The
                // scatter form (iterate edges, push to both ends) is what makes a
                // parallel layout non-reproducible.
                let (xi, yi) = (x[i], y[i]);
                for &j in adj.neighbors(i as u32) {
                    let j = j as usize;
                    let (dx, dy) = (xi - x[j], yi - y[j]);
                    let d = (dx * dx + dy * dy).sqrt().max(1e-4);
                    let f = d * d / k;
                    fx -= dx / d * f;
                    fy -= dy / d * f;
                }

                // Gravity toward the origin, so disconnected components stay in
                // the same picture instead of repelling to infinity.
                fx -= xi * params.gravity * k;
                fy -= yi * params.gravity * k;

                // Clamp the step to the temperature: without it a large force
                // early on throws a node across the drawing and the layout never
                // recovers.
                let mag = (fx * fx + fy * fy).sqrt().max(1e-6);
                let scale = mag.min(temp) / mag;
                (fx * scale, fy * scale)
            })
            .collect();

        for i in 0..n {
            x[i] += step[i].0;
            y[i] += step[i].1;
        }
    }

    center(&mut x, &mut y);
    (x, y)
}

/// Deterministic scatter — the trivial layout, and the seeding step for others.
pub fn layout_random(topo: &Topology, params: LayoutParams) -> Positions {
    let n = topo.n_nodes();
    if n == 0 {
        return (Vec::new(), Vec::new());
    }
    let (mut x, mut y) = seed_positions(n, params.k.max(1e-4), params.seed.unwrap_or(DEFAULT_SEED));
    center(&mut x, &mut y);
    (x, y)
}

/// Nodes evenly spaced on a circle, in dense index order.
///
/// Useful as a baseline that is obviously not force-directed: if a figure looks
/// the same under this and under `layout_fr`, the force layout did not run.
pub fn layout_circle(topo: &Topology, params: LayoutParams) -> Positions {
    let n = topo.n_nodes();
    if n == 0 {
        return (Vec::new(), Vec::new());
    }
    let k = params.k.max(1e-4);
    let radius = k * (n as f32).max(1.0) / (2.0 * std::f32::consts::PI);
    let mut x = vec![0.0f32; n];
    let mut y = vec![0.0f32; n];
    for i in 0..n {
        let a = 2.0 * std::f32::consts::PI * i as f32 / n as f32;
        x[i] = radius * a.cos();
        y[i] = radius * a.sin();
    }
    (x, y)
}

/// Recentre on the origin, so a layout's position in space is not an accident of
/// where it happened to drift.
fn center(x: &mut [f32], y: &mut [f32]) {
    let n = x.len() as f32;
    if n == 0.0 {
        return;
    }
    // Summed in index order, single-threaded: a parallel reduction here would
    // reintroduce the order dependence the rest of the kernel avoids.
    let (mut sx, mut sy) = (0.0f32, 0.0f32);
    for i in 0..x.len() {
        sx += x[i];
        sy += y[i];
    }
    let (mx, my) = (sx / n, sy / n);
    for i in 0..x.len() {
        x[i] -= mx;
        y[i] -= my;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two triangles joined by a bridge: 0-1-2 and 3-4-5, with 2-3 between them.
    fn barbell() -> Topology {
        Topology::build(6, vec![0, 1, 2, 2, 3, 4, 5], vec![1, 2, 0, 3, 4, 5, 3])
    }

    fn dist(p: &Positions, a: usize, b: usize) -> f32 {
        let dx = p.0[a] - p.0[b];
        let dy = p.1[a] - p.1[b];
        (dx * dx + dy * dy).sqrt()
    }

    #[test]
    fn empty_and_singleton_graphs_are_handled() {
        let empty = Topology::build(0, vec![], vec![]);
        assert_eq!(layout_fr(&empty, None, LayoutParams::default()).0.len(), 0);
        let one = Topology::build(1, vec![], vec![]);
        let p = layout_fr(&one, None, LayoutParams::default());
        assert_eq!(p.0.len(), 1);
        assert!(p.0[0].is_finite() && p.1[0].is_finite());
    }

    #[test]
    fn every_position_is_finite() {
        // The failure mode that matters: one NaN propagates through repulsion and
        // silently turns the whole drawing into nothing.
        let p = layout_fr(&barbell(), None, LayoutParams::default());
        for i in 0..6 {
            assert!(p.0[i].is_finite(), "x[{i}] not finite");
            assert!(p.1[i].is_finite(), "y[{i}] not finite");
        }
    }

    #[test]
    fn connected_nodes_end_closer_than_unconnected_ones() {
        // The one property that makes a layout a layout. Nodes 0 and 5 are in
        // different triangles, three hops apart; 0 and 1 share an edge.
        let p = layout_fr(&barbell(), None, LayoutParams::default());
        assert!(
            dist(&p, 0, 1) < dist(&p, 0, 5),
            "adjacent {} should be nearer than distant {}",
            dist(&p, 0, 1),
            dist(&p, 0, 5)
        );
    }

    #[test]
    fn the_result_is_centred() {
        let p = layout_fr(&barbell(), None, LayoutParams::default());
        let mx: f32 = p.0.iter().sum::<f32>() / 6.0;
        let my: f32 = p.1.iter().sum::<f32>() / 6.0;
        assert!(mx.abs() < 1e-3, "x mean {mx}");
        assert!(my.abs() < 1e-3, "y mean {my}");
    }

    #[test]
    fn a_seed_reproduces_a_layout_exactly() {
        let params = LayoutParams {
            seed: Some(7),
            ..LayoutParams::default()
        };
        let a = layout_fr(&barbell(), None, params);
        let b = layout_fr(&barbell(), None, params);
        assert_eq!(a.0, b.0);
        assert_eq!(a.1, b.1);
    }

    #[test]
    fn different_seeds_give_different_layouts() {
        let a = layout_fr(
            &barbell(),
            None,
            LayoutParams {
                seed: Some(1),
                ..LayoutParams::default()
            },
        );
        let b = layout_fr(
            &barbell(),
            None,
            LayoutParams {
                seed: Some(2),
                ..LayoutParams::default()
            },
        );
        assert_ne!(a.0, b.0);
    }

    #[test]
    fn a_mask_changes_the_drawing() {
        // Dropping the bridge (edge row 3, 2->3) leaves two components, which
        // gravity holds in frame but which no longer pull on each other.
        let topo = barbell();
        let full = layout_fr(&topo, None, LayoutParams::default());
        let mask = EdgeMask::from_bools(&[true, true, true, false, true, true, true]);
        let cut = layout_fr(&topo, Some(&mask), LayoutParams::default());
        assert_ne!(full.0, cut.0);
        for i in 0..6 {
            assert!(cut.0[i].is_finite() && cut.1[i].is_finite());
        }
    }

    #[test]
    fn coincident_nodes_do_not_produce_infinities() {
        // Two nodes at the same point feel a division by zero without the floor in
        // `repulse`, and one infinity turns every later iteration into NaN.
        let topo = Topology::build(2, vec![0], vec![1]);
        let p = layout_fr(
            &topo,
            None,
            LayoutParams {
                iterations: 50,
                ..LayoutParams::default()
            },
        );
        assert!(p.0.iter().all(|v| v.is_finite()));
        assert!(p.1.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn circle_places_every_node_at_one_radius() {
        let topo = barbell();
        let p = layout_circle(&topo, LayoutParams::default());
        let r0 = (p.0[0] * p.0[0] + p.1[0] * p.1[0]).sqrt();
        for i in 1..6 {
            let r = (p.0[i] * p.0[i] + p.1[i] * p.1[i]).sqrt();
            assert!((r - r0).abs() < 1e-4, "node {i} radius {r} != {r0}");
        }
    }

    #[test]
    fn random_is_seeded_and_spread() {
        let topo = Topology::build(50, vec![], vec![]);
        let a = layout_random(
            &topo,
            LayoutParams {
                seed: Some(3),
                ..LayoutParams::default()
            },
        );
        let b = layout_random(
            &topo,
            LayoutParams {
                seed: Some(3),
                ..LayoutParams::default()
            },
        );
        assert_eq!(a.0, b.0);
        // No two nodes coincident: the spiral exists so the force kernel never
        // starts from a division by zero.
        for i in 0..50 {
            for j in i + 1..50 {
                assert!(dist(&a, i, j) > 1e-6, "{i} and {j} coincide");
            }
        }
    }
}
