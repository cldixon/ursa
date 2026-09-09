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
//! - **A spatial index that is a pure function of the positions.** The quadtree is
//!   rebuilt per iteration from a Morton sort, so the order repulsion is summed in
//!   is decided by the tree rather than by thread scheduling. [`super::quadtree`]
//!   explains why that build was chosen over the textbook insertion one.
//! - **Global reductions in index order.** ForceAtlas2's adaptive step needs two
//!   sums over all nodes; both are accumulated serially, because a parallel
//!   reduction would reintroduce exactly the order dependence the rest avoids.
//!
//! Cross-platform bit-identity is claimed **only for the non-LinLog force laws**.
//! `layout_fr` and `layout_fa2`'s default linear attraction use nothing but
//! `+ - * /` and `sqrt`, all IEEE-exact. LinLog mode introduces `ln`, whose
//! last-bit result is a libm implementation detail — so `lin_log: true` stays
//! reproducible on one machine and is not guaranteed to match across platforms.
//!
//! # Repulsion is a quadtree
//!
//! Both kernels approximate repulsion with a Barnes–Hut quadtree
//! ([`super::quadtree`]): a group of nodes far enough away is replaced by its
//! centre of mass, governed by `theta`. That is O(n log n) per iteration.
//!
//! It replaced a uniform grid, which was O(n²) — the grid scanned every cell for
//! every node, and cell count grows with the graph, so the "far field" saved a
//! constant factor and not the exponent. `examples/layout_scaling.rs` measures
//! this; the grid's growth factor was ~3.8× per doubling, which is 4× wearing a
//! disguise.

use crate::parallel::*;

use super::quadtree::{traversal_stack, unit_weights, QuadTree};
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
    /// Barnes–Hut opening angle. Smaller is more accurate and slower; `0.0` is
    /// exact all-pairs. `0.5` is the conventional default and what the accuracy
    /// test measures against brute force.
    pub theta: f32,
    pub seed: Option<u64>,
}

impl Default for LayoutParams {
    fn default() -> Self {
        LayoutParams {
            iterations: 300,
            k: 1.0,
            gravity: 0.02,
            theta: 0.5,
            seed: None,
        }
    }
}

/// Parameters for [`layout_fa2`] — ForceAtlas2, per Jacomy et al. (2014).
///
/// The differences from Fruchterman–Reingold are what make it the better default
/// for a real graph, and each is a separate knob here:
///
/// - **Repulsion is degree-weighted.** A hub repels proportionally to its degree,
///   which stops it being buried inside the neighbourhood it anchors. This is the
///   single biggest visual difference and it is not optional.
/// - **Attraction is linear** in distance (FR's is quadratic), so clusters stay
///   legible instead of collapsing to points.
/// - **The step size adapts**, per node, from the ratio of "swinging" to useful
///   motion — rather than FR's fixed cooling schedule, which has to be tuned to
///   the graph.
#[derive(Debug, Clone, Copy)]
pub struct Fa2Params {
    pub iterations: u32,
    /// Repulsion strength. Scales the drawing rather than changing its shape.
    pub k: f32,
    /// Pull toward the origin, keeping disconnected components in one picture.
    pub gravity: f32,
    /// Gravity proportional to distance rather than constant. Pulls sparse
    /// peripheries in hard, which tightens a drawing that would otherwise sprawl.
    pub strong_gravity: bool,
    /// LinLog mode: attraction becomes `ln(1 + d)`, which separates clusters more
    /// distinctly at the cost of the cross-platform determinism note above.
    pub lin_log: bool,
    /// Barnes–Hut opening angle; see [`LayoutParams::theta`].
    pub theta: f32,
    /// How much node "swinging" is tolerated before the global step size is cut.
    /// The paper's `tau`. Larger converges faster and shakes more.
    pub jitter_tolerance: f32,
    pub seed: Option<u64>,
}

impl Default for Fa2Params {
    fn default() -> Self {
        Fa2Params {
            iterations: 300,
            k: 1.0,
            gravity: 1.0,
            strong_gravity: false,
            lin_log: false,
            theta: 0.5,
            jitter_tolerance: 1.0,
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
    // Mass is population here: FR's repulsion does not care about degree, so a
    // cell's pull is proportional to how many nodes it holds.
    let mass = unit_weights(n);
    // Start displacing by a tenth of the drawing's extent and cool linearly to
    // nothing, so early iterations rearrange freely and late ones only settle.
    let t0 = k * (n as f32).sqrt() * 0.1;

    for it in 0..params.iterations {
        let tree = QuadTree::build(&x, &y, &mass);
        let temp = t0 * (1.0 - it as f32 / params.iterations as f32).max(0.0);

        let step: Vec<(f32, f32)> = (0..n)
            .into_par_iter()
            .map_init(traversal_stack, |stack, i| {
                // Inverse-square repulsion, `m` being either one node's weight or
                // a whole cell's aggregate — the tree decides which, the law does
                // not need to know.
                let (mut fx, mut fy) =
                    tree.accumulate(&x, &y, &mass, i, params.theta, stack, |dx, dy, m| {
                        // Floored so two coincident nodes get a large but finite
                        // push rather than an infinity that poisons every later
                        // iteration.
                        let d2 = (dx * dx + dy * dy).max(1e-4);
                        let f = k2 * m / d2;
                        (dx * f, dy * f)
                    });

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

/// ForceAtlas2 layout: `(x, y)` per node, dense-indexed.
///
/// The default force-directed layout for a graph you actually want to look at.
/// See [`Fa2Params`] for what it does differently from [`layout_fr`]; the short
/// version is that hubs repel by degree and the step size tunes itself.
///
/// A `mask` restricts attraction to the kept edges, exactly as in [`layout_fr`] —
/// note that it also changes the *degrees*, and therefore the repulsion, because
/// in a subgraph a node's degree is its degree in that subgraph.
pub fn layout_fa2(topo: &Topology, mask: Option<&EdgeMask>, params: Fa2Params) -> Positions {
    let n = topo.n_nodes();
    if n == 0 {
        return (Vec::new(), Vec::new());
    }
    let k = params.k.max(1e-4);
    let (mut x, mut y) = seed_positions(n, k, params.seed.unwrap_or(DEFAULT_SEED));
    if n == 1 {
        return (x, y);
    }

    let view = undirected_view(topo, mask);
    let adj: &UndirectedCsr = view.get();

    // The mass that makes ForceAtlas2 what it is: `deg + 1`, so a hub pushes
    // proportionally to what it anchors and an isolated node still pushes a
    // little. The `+ 1` is not cosmetic — at mass 0 a degree-0 node would neither
    // repel nor be repelled, and would sit wherever it was seeded, on top of
    // whatever else is there.
    let mass: Vec<f32> = (0..n).map(|i| 1.0 + adj.degree(i as u32) as f32).collect();

    // Previous-iteration force per node, for the swing/traction measure below.
    let mut prev: Vec<(f32, f32)> = vec![(0.0, 0.0); n];
    // The paper's global speed, carried across iterations and adjusted by at most
    // 50% per step so one bad iteration cannot destabilise the run.
    let mut speed = 1.0f32;

    for _ in 0..params.iterations {
        let tree = QuadTree::build(&x, &y, &mass);

        let forces: Vec<(f32, f32)> = (0..n)
            .into_par_iter()
            .map_init(traversal_stack, |stack, i| {
                let mi = mass[i];
                // Repulsion: k · mᵢ · mⱼ / d, as a vector k·mᵢ·mⱼ·d⃗/d². Note the
                // 1/d magnitude, not FR's 1/d² — ForceAtlas2's repulsion decays
                // more slowly, which is what spreads a large graph out instead of
                // packing it into a disc.
                let (mut fx, mut fy) =
                    tree.accumulate(&x, &y, &mass, i, params.theta, stack, |dx, dy, mj| {
                        let d2 = (dx * dx + dy * dy).max(1e-4);
                        let f = k * mi * mj / d2;
                        (dx * f, dy * f)
                    });

                // Attraction, gathered — same reason as everywhere else in this
                // crate. Linear in distance by default: the force vector is just
                // the displacement, which is why there is no division here.
                let (xi, yi) = (x[i], y[i]);
                for &j in adj.neighbors(i as u32) {
                    let j = j as usize;
                    let (dx, dy) = (xi - x[j], yi - y[j]);
                    if params.lin_log {
                        let d = (dx * dx + dy * dy).sqrt().max(1e-4);
                        // ln(1+d) magnitude along the unit vector. The one
                        // transcendental in this file, and the reason LinLog is
                        // excluded from the cross-platform determinism claim.
                        let f = (1.0 + d).ln() / d;
                        fx -= dx * f;
                        fy -= dy * f;
                    } else {
                        fx -= dx;
                        fy -= dy;
                    }
                }

                // Gravity: constant magnitude toward the origin, scaled by mass so
                // a hub is not dragged around by its periphery. Strong gravity
                // drops the 1/d and pulls proportionally to distance instead.
                let dist = (xi * xi + yi * yi).sqrt().max(1e-4);
                let g = if params.strong_gravity {
                    params.gravity * mi
                } else {
                    params.gravity * mi / dist
                };
                fx -= xi * g;
                fy -= yi * g;

                (fx, fy)
            })
            .collect();

        // The adaptive step. "Swing" is how much a node's force reversed since the
        // last iteration — a node that keeps changing its mind is oscillating, not
        // converging — and "traction" is how much of the force is doing useful
        // work. Their ratio sets the global step size.
        //
        // Both sums run serially in index order. This is the one place a parallel
        // reduction would be tempting and it is exactly where it would break
        // reproducibility, since the result feeds back into every node's step.
        let (mut swing_total, mut traction_total) = (0.0f32, 0.0f32);
        for i in 0..n {
            let (fx, fy) = forces[i];
            let (px, py) = prev[i];
            let swing = ((fx - px).powi(2) + (fy - py).powi(2)).sqrt();
            let traction = (((fx + px) * 0.5).powi(2) + ((fy + py) * 0.5).powi(2)).sqrt();
            swing_total += mass[i] * swing;
            traction_total += mass[i] * traction;
        }

        let target = if swing_total > 0.0 {
            params.jitter_tolerance * traction_total / swing_total
        } else {
            // Nothing swung, which happens on iteration one (prev is zero) and at
            // convergence. Keep the current speed rather than dividing by zero.
            speed
        };
        // Rise by at most 50% per iteration. Falling is unconstrained: an
        // unstable layout should be able to slam the brakes immediately.
        speed = if target > speed * 1.5 {
            speed * 1.5
        } else {
            target.max(1e-6)
        };

        for i in 0..n {
            let (fx, fy) = forces[i];
            let swing = ((fx - prev[i].0).powi(2) + (fy - prev[i].1).powi(2)).sqrt();
            // Per-node damping: a node that is swinging takes a smaller step than
            // one moving steadily, even though both see the same global speed.
            let factor = speed / (1.0 + speed * swing.sqrt());
            // Cap the displacement so an outlier force cannot fling a node across
            // the drawing, which FR handles with its cooling schedule instead.
            let mag = (fx * fx + fy * fy).sqrt().max(1e-6);
            let capped = (factor * mag).min(10.0 * k) / mag;
            x[i] += fx * capped;
            y[i] += fy * capped;
        }

        prev = forces;
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

    // --- ForceAtlas2 -------------------------------------------------------

    #[test]
    fn fa2_handles_empty_and_singleton_graphs() {
        let empty = Topology::build(0, vec![], vec![]);
        assert_eq!(layout_fa2(&empty, None, Fa2Params::default()).0.len(), 0);
        let one = Topology::build(1, vec![], vec![]);
        let p = layout_fa2(&one, None, Fa2Params::default());
        assert_eq!(p.0.len(), 1);
        assert!(p.0[0].is_finite() && p.1[0].is_finite());
    }

    #[test]
    fn fa2_positions_are_all_finite() {
        // The adaptive step divides by a swing measure and by a force magnitude;
        // either can be zero at convergence, and one NaN silently voids the
        // whole drawing.
        for params in [
            Fa2Params::default(),
            Fa2Params {
                lin_log: true,
                ..Fa2Params::default()
            },
            Fa2Params {
                strong_gravity: true,
                ..Fa2Params::default()
            },
            Fa2Params {
                gravity: 0.0,
                ..Fa2Params::default()
            },
        ] {
            let p = layout_fa2(&barbell(), None, params);
            assert!(
                p.0.iter().chain(p.1.iter()).all(|v| v.is_finite()),
                "non-finite position under {params:?}"
            );
        }
    }

    #[test]
    fn fa2_puts_connected_nodes_closer_than_unconnected_ones() {
        let p = layout_fa2(&barbell(), None, Fa2Params::default());
        assert!(
            dist(&p, 0, 1) < dist(&p, 0, 5),
            "adjacent {} should be nearer than distant {}",
            dist(&p, 0, 1),
            dist(&p, 0, 5)
        );
    }

    #[test]
    fn fa2_reproduces_a_layout_from_a_seed() {
        let params = Fa2Params {
            seed: Some(11),
            ..Fa2Params::default()
        };
        let a = layout_fa2(&barbell(), None, params);
        let b = layout_fa2(&barbell(), None, params);
        assert_eq!(a.0, b.0);
        assert_eq!(a.1, b.1);
    }

    #[test]
    fn fa2_pushes_hubs_further_out_than_fr_does() {
        // The defining behavioural difference, and the reason ForceAtlas2 exists.
        // A star: one hub, many leaves. Degree-weighted repulsion gives the hub a
        // mass of `deg+1` and so a wide berth; FR treats it as one more node and
        // lets the leaves crowd it.
        //
        // Measured as the hub's mean distance to its leaves, relative to the
        // drawing's own scale — otherwise this only compares the two kernels'
        // arbitrary units.
        let n = 40usize;
        let src: Vec<u32> = (1..n as u32).collect();
        let dst: Vec<u32> = vec![0; n - 1];
        let topo = Topology::build(n, src, dst);

        let spread = |p: &Positions| -> f32 {
            let mean_hub: f32 = (1..n).map(|i| dist(p, 0, i)).sum::<f32>() / (n - 1) as f32;
            // The drawing's radius, as the normaliser.
            let radius = (1..n)
                .map(|i| (p.0[i] * p.0[i] + p.1[i] * p.1[i]).sqrt())
                .fold(0.0f32, f32::max)
                .max(1e-6);
            mean_hub / radius
        };

        let fa2 = layout_fa2(
            &topo,
            None,
            Fa2Params {
                seed: Some(1),
                ..Fa2Params::default()
            },
        );
        let fr = layout_fr(
            &topo,
            None,
            LayoutParams {
                seed: Some(1),
                ..LayoutParams::default()
            },
        );
        assert!(
            spread(&fa2) > spread(&fr),
            "fa2 relative hub distance {} should exceed fr's {}",
            spread(&fa2),
            spread(&fr)
        );
    }

    #[test]
    fn fa2_strong_gravity_tightens_the_drawing() {
        let params = Fa2Params {
            seed: Some(2),
            ..Fa2Params::default()
        };
        let loose = layout_fa2(&barbell(), None, params);
        let tight = layout_fa2(
            &barbell(),
            None,
            Fa2Params {
                strong_gravity: true,
                ..params
            },
        );
        let extent = |p: &Positions| {
            p.0.iter()
                .zip(p.1.iter())
                .map(|(a, b)| (a * a + b * b).sqrt())
                .fold(0.0f32, f32::max)
        };
        assert!(
            extent(&tight) < extent(&loose),
            "strong gravity extent {} should be under {}",
            extent(&tight),
            extent(&loose)
        );
    }

    #[test]
    fn fa2_lin_log_changes_the_drawing() {
        // Not a quality claim — just that the flag is wired through to the force
        // law rather than accepted and dropped.
        let params = Fa2Params {
            seed: Some(4),
            ..Fa2Params::default()
        };
        let linear = layout_fa2(&barbell(), None, params);
        let log = layout_fa2(
            &barbell(),
            None,
            Fa2Params {
                lin_log: true,
                ..params
            },
        );
        assert_ne!(linear.0, log.0);
    }

    #[test]
    fn fa2_honours_an_edge_mask() {
        let topo = barbell();
        let full = layout_fa2(&topo, None, Fa2Params::default());
        let mask = EdgeMask::from_bools(&[true, true, true, false, true, true, true]);
        let cut = layout_fa2(&topo, Some(&mask), Fa2Params::default());
        assert_ne!(full.0, cut.0);
        assert!(cut.0.iter().chain(cut.1.iter()).all(|v| v.is_finite()));
    }

    #[test]
    fn fa2_result_is_centred() {
        let p = layout_fa2(&barbell(), None, Fa2Params::default());
        let mx: f32 = p.0.iter().sum::<f32>() / 6.0;
        assert!(mx.abs() < 1e-3, "x mean {mx}");
    }

    #[test]
    fn theta_changes_the_result_but_not_its_shape() {
        // theta trades accuracy for speed, so exact positions must differ — but
        // the layout must still be a layout at the loose end, or the default is
        // buying speed with nonsense.
        let topo = barbell();
        let exact = layout_fa2(
            &topo,
            None,
            Fa2Params {
                theta: 0.0,
                seed: Some(5),
                ..Fa2Params::default()
            },
        );
        let approx = layout_fa2(
            &topo,
            None,
            Fa2Params {
                theta: 1.2,
                seed: Some(5),
                ..Fa2Params::default()
            },
        );
        assert_ne!(exact.0, approx.0);
        assert!(dist(&approx, 0, 1) < dist(&approx, 0, 5));
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
