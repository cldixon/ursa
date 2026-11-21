//! Determinism across rayon thread-pool sizes (#76).
//!
//! The #40 work made the parallel kernels' outputs independent of execution order
//! (fixed-order f64 reductions, order-independent Louvain tie-breaks, seeded
//! sampled betweenness). The rest of the suite checks a *single* thread count,
//! which cannot catch a non-associative parallel reduction — that only diverges as
//! the pool size varies. Here each determinism-sensitive kernel runs on one fixed
//! graph across several `num_threads` values and every result must be bit-identical
//! (exact `==`, not approximate) to the single-threaded run. This locks the #40
//! guarantees against future parallelization work silently reintroducing order
//! dependence.
//!
//! Gated on the `rayon` feature: without it there is no thread pool to vary, and the
//! kernels run through the serial fallback (`ursa-core::parallel`) — whose whole point
//! is to produce the *same* bytes. The per-kernel unit tests, which assert exact
//! expected values, cover that serial path when the suite is run with
//! `--no-default-features`; this cross-pool-size test only has meaning with `rayon` on.
#![cfg(feature = "rayon")]

use rayon::ThreadPoolBuilder;
use ursa_core::algo::{
    betweenness, betweenness_weighted, closeness_weighted, connected_components_strong,
    connected_components_weak, label_propagation, layout_fa2, layout_fa2_multilevel, layout_fr,
    louvain, louvain_weighted, pagerank, pagerank_weighted, symmetric_view_weighted, Fa2Multilevel,
    Fa2Params, LayoutParams, MultilevelParams, PageRankParams,
};
use ursa_core::algo::{
    bfs_distances, fa2_grow, layout_layers, layout_rings, Fa2State, LayersParams, RingsParams,
    WarmParams, NO_LEVEL,
};
use ursa_core::topology::Direction;
use ursa_core::Topology;

/// A deterministic pseudo-random directed graph via a hand-rolled LCG, so the test
/// pulls in no RNG dependency and the graph is identical on every run and platform.
fn seeded_graph(n: usize, m: usize) -> Topology {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // High bits have the best distribution for an LCG.
        (state >> 33) as u32
    };
    let mut src = Vec::with_capacity(m);
    let mut dst = Vec::with_capacity(m);
    for _ in 0..m {
        src.push(next() % n as u32);
        dst.push(next() % n as u32);
    }
    Topology::build(n, src, dst)
}

/// Run `f` on a fresh rayon pool pinned to exactly `threads` workers.
fn on_pool<T: Send>(threads: usize, f: impl FnOnce() -> T + Send) -> T {
    ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("thread pool builds")
        .install(f)
}

/// Thread counts to cross-check. 1 is the reference; the rest must match it. 3 is
/// deliberately non-power-of-two so an uneven work split can't hide a bug.
const THREADS: [usize; 4] = [2, 3, 4, 8];

#[test]
fn pagerank_is_thread_count_independent() {
    let t = seeded_graph(400, 4000);
    let params = PageRankParams {
        damping: 0.85,
        max_iter: 100,
        tol: 1e-12,
    };
    let reference = on_pool(1, || pagerank(&t, None, params));
    for &threads in &THREADS {
        assert_eq!(
            on_pool(threads, || pagerank(&t, None, params)),
            reference,
            "pagerank diverged at {threads} threads"
        );
    }
}

#[test]
fn symmetric_views_are_thread_count_independent() {
    // The `direction="both"` views, each built on the pool under test (a fresh
    // topology per pool, so the lazy undirected CSR is built there too), and
    // PageRank over them.
    let params = PageRankParams {
        damping: 0.85,
        max_iter: 100,
        tol: 1e-12,
    };
    let run = || {
        let t = seeded_graph(2000, 100_000);
        let w: Vec<f64> = (0..t.n_edges()).map(|e| (e % 7) as f64 + 0.5).collect();
        let sym = t.symmetric();
        let (wsym, sw) = symmetric_view_weighted(&t, &w, None);
        (
            sym.out().targets.clone(),
            wsym.out().targets.clone(),
            sw.clone(),
            pagerank(sym, None, params),
            pagerank_weighted(&wsym, &sw, None, params),
        )
    };
    let reference = on_pool(1, run);
    for &threads in &THREADS {
        assert!(
            on_pool(threads, run) == reference,
            "symmetric views diverged at {threads} threads"
        );
    }
}

#[test]
fn betweenness_full_is_thread_count_independent() {
    let t = seeded_graph(300, 2500);
    let reference = on_pool(1, || betweenness(&t, None, None, None));
    for &threads in &THREADS {
        assert_eq!(
            on_pool(threads, || betweenness(&t, None, None, None)),
            reference,
            "full betweenness diverged at {threads} threads"
        );
    }
}

#[test]
fn betweenness_sampled_is_thread_count_independent() {
    // Sampled + seeded: the #54 guarantee is that a fixed seed pins the sampled
    // source set and the result regardless of how many threads process it.
    let t = seeded_graph(400, 4000);
    let reference = on_pool(1, || betweenness(&t, None, Some(0.25), Some(42)));
    for &threads in &THREADS {
        assert_eq!(
            on_pool(threads, || betweenness(&t, None, Some(0.25), Some(42))),
            reference,
            "sampled betweenness diverged at {threads} threads"
        );
    }
}

#[test]
fn betweenness_small_samples_are_thread_count_independent() {
    // Few sources: fewer 64-source chunks than threads, so the multi-threaded runs
    // take the per-source schedule (sources of one chunk in parallel, folded in
    // source order) while the single-threaded reference takes the per-chunk one —
    // they must still agree exactly. 0.1 is one chunk, 0.4 is three; the dense
    // graph exercises the parallel block fold, the sparse one the reached-set fold.
    for (n, m) in [(400, 4000), (2000, 1500)] {
        let t = seeded_graph(n, m);
        let w: Vec<f64> = (0..m).map(|e| 1.0 + (e % 7) as f64 * 0.5).collect();
        for frac in [0.1, 0.4] {
            let reference = on_pool(1, || betweenness(&t, None, Some(frac), Some(42)));
            let reference_w = on_pool(1, || {
                betweenness_weighted(&t, &w, None, Some(frac), Some(42))
            });
            for &threads in &THREADS {
                assert_eq!(
                    on_pool(threads, || betweenness(&t, None, Some(frac), Some(42))),
                    reference,
                    "sampled betweenness (n={n}, {frac}) diverged at {threads} threads"
                );
                assert_eq!(
                    on_pool(threads, || {
                        betweenness_weighted(&t, &w, None, Some(frac), Some(42))
                    }),
                    reference_w,
                    "weighted betweenness (n={n}, {frac}) diverged at {threads} threads"
                );
            }
        }
    }
}

#[test]
fn closeness_weighted_is_thread_count_independent() {
    // Weighted closeness sums f64 distances; the reached-set fold must give the
    // same bytes however sources are spread over workers.
    for (n, m) in [(400, 4000), (3000, 2500)] {
        let t = seeded_graph(n, m);
        let w: Vec<f64> = (0..m).map(|e| 0.25 + (e % 11) as f64 * 0.75).collect();
        let reference = on_pool(1, || closeness_weighted(&t, &w, None));
        for &threads in &THREADS {
            assert_eq!(
                on_pool(threads, || closeness_weighted(&t, &w, None)),
                reference,
                "weighted closeness (n={n}) diverged at {threads} threads"
            );
        }
    }
}

#[test]
fn louvain_is_thread_count_independent() {
    let t = seeded_graph(400, 4000);
    let reference = on_pool(1, || louvain(&t, None, 1.0, Some(7)));
    for &threads in &THREADS {
        assert_eq!(
            on_pool(threads, || louvain(&t, None, 1.0, Some(7))),
            reference,
            "louvain diverged at {threads} threads"
        );
    }
}

#[test]
fn louvain_weighted_large_is_thread_count_independent() {
    // Large enough (> 2^16 edges) for the working graph to be built in parallel
    // chunks, with inexact weights so any change in summation order would show.
    let t = seeded_graph(20_000, 100_000);
    let w: Vec<f64> = (0..t.n_edges())
        .map(|i| ((i % 89) as f64 + 1.0).sqrt() / 7.0)
        .collect();
    let reference = on_pool(1, || louvain_weighted(&t, &w, None, 1.0, Some(7)));
    for &threads in &THREADS {
        assert_eq!(
            on_pool(threads, || louvain_weighted(&t, &w, None, 1.0, Some(7))),
            reference,
            "weighted louvain diverged at {threads} threads"
        );
    }
}

#[test]
fn label_propagation_is_thread_count_independent() {
    let t = seeded_graph(400, 4000);
    let reference = on_pool(1, || label_propagation(&t, None, 20, Some(7)));
    for &threads in &THREADS {
        assert_eq!(
            on_pool(threads, || label_propagation(&t, None, 20, Some(7))),
            reference,
            "label propagation diverged at {threads} threads"
        );
    }
}

/// Layout, across thread counts.
///
/// The kernels where determinism took deliberate design rather than falling out of
/// the existing shape. Three things had to be arranged for it, and each one is a
/// property this test would catch the loss of:
///
/// 1. **Attraction gathers.** The natural force formulation scatters (iterate
///    edges, push to both endpoints), and a scatter's result depends on the order
///    threads happen to write in.
/// 2. **The quadtree is a pure function of the positions.** Built from a Morton
///    sort rather than by insertion, so its shape — and therefore the order
///    repulsion is summed in — cannot depend on scheduling.
/// 3. **ForceAtlas2's global reductions run serially**, in index order. They feed
///    back into every node's step size, so a parallel reduction there would make
///    the *whole* layout thread-dependent through one shared scalar.
///
/// Exact equality, like every other kernel here — approximate equality would pass
/// while the property silently rotted.
#[test]
fn layout_fr_is_thread_count_independent() {
    let topo = seeded_graph(400, 1600);
    let params = LayoutParams {
        // Fewer iterations than the default: enough that any order dependence has
        // compounded well past f32 rounding, few enough to keep the test quick.
        iterations: 40,
        seed: Some(99),
        ..LayoutParams::default()
    };
    let baseline = on_pool(1, || layout_fr(&topo, None, params));
    for threads in [2, 3, 4, 8] {
        let got = on_pool(threads, || layout_fr(&topo, None, params));
        assert_eq!(got.0, baseline.0, "x diverged on {threads} threads");
        assert_eq!(got.1, baseline.1, "y diverged on {threads} threads");
    }
}

/// Rings and layers (#436) are serial, so the pool cannot reach them; this
/// holds them to that if either is ever parallelised. Levels are distances
/// from two seeds, with the unreached nodes left without one.
#[test]
fn layout_rings_and_layers_are_thread_count_independent() {
    let topo = seeded_graph(2000, 5000);
    let (a, b) = (
        bfs_distances(&topo, 0, Direction::Both),
        bfs_distances(&topo, 1000, Direction::Both),
    );
    let level: Vec<u32> = a
        .iter()
        .zip(&b)
        .map(|(&a, &b)| match (a, b) {
            (-1, -1) => NO_LEVEL,
            (-1, d) | (d, -1) => d as u32,
            (a, b) => a.min(b) as u32,
        })
        .collect();
    let adj = topo.undirected();
    let run = || {
        (
            layout_rings(adj, &level, RingsParams::default()),
            layout_layers(adj, &level, LayersParams::default()),
        )
    };
    let bits = |v: &[f32]| -> Vec<u32> { v.iter().map(|f| f.to_bits()).collect() };
    let baseline = on_pool(1, run);
    assert!(baseline.0 .0.iter().all(|v| v.is_finite()));
    for threads in [2, 3, 4, 8] {
        let got = on_pool(threads, run);
        for (name, got, want) in [
            ("rings x", &got.0 .0, &baseline.0 .0),
            ("rings y", &got.0 .1, &baseline.0 .1),
            ("layers x", &got.1 .0, &baseline.1 .0),
            ("layers y", &got.1 .1, &baseline.1 .1),
        ] {
            assert_eq!(
                bits(got),
                bits(want),
                "{name} diverged on {threads} threads"
            );
        }
    }
}

#[test]
fn layout_fa2_is_thread_count_independent() {
    let topo = seeded_graph(400, 1600);
    let params = Fa2Params {
        iterations: 40,
        seed: Some(99),
        ..Fa2Params::default()
    };
    let baseline = on_pool(1, || layout_fa2(&topo, None, params));
    for threads in [2, 3, 4, 8] {
        let got = on_pool(threads, || layout_fa2(&topo, None, params));
        assert_eq!(got.0, baseline.0, "x diverged on {threads} threads");
        assert_eq!(got.1, baseline.1, "y diverged on {threads} threads");
    }
}

/// LinLog mode, across thread counts.
///
/// Worth its own case because `ln` is the one transcendental in the force laws.
/// It is *not* claimed to be bit-identical across platforms — libm's last bit is
/// an implementation detail — but on one machine it must still be independent of
/// the pool size, since that is a property of the summation order rather than of
/// the function being summed.
#[test]
fn layout_fa2_lin_log_is_thread_count_independent() {
    let topo = seeded_graph(300, 1200);
    let params = Fa2Params {
        iterations: 30,
        lin_log: true,
        seed: Some(3),
        ..Fa2Params::default()
    };
    let baseline = on_pool(1, || layout_fa2(&topo, None, params));
    for threads in [2, 4, 8] {
        let got = on_pool(threads, || layout_fa2(&topo, None, params));
        assert_eq!(got.0, baseline.0, "x diverged on {threads} threads");
        assert_eq!(got.1, baseline.1, "y diverged on {threads} threads");
    }
}

/// Coarse-to-fine ForceAtlas2, across thread counts: the hierarchy (built
/// serially in index order, its coarse rows gathered per coarse node), every
/// level's ForceAtlas2, and the prolongation between them. Stepped in uneven
/// budgets too, as the graph view steps it, which must end on the same bits.
#[test]
fn layout_fa2_multilevel_is_thread_count_independent() {
    // Sparse enough to leave isolated nodes, which leave the hierarchy, and
    // big enough for several levels.
    let topo = seeded_graph(3000, 4000);
    let params = MultilevelParams {
        coarsest_nodes: 40,
        coarsest_iterations: 60,
        max_iterations: 20,
        fa2: Fa2Params {
            seed: Some(11),
            ..MultilevelParams::default().fa2
        },
        ..MultilevelParams::default()
    };
    let run = || {
        let one = layout_fa2_multilevel(&topo, None, params);
        let adj = topo.undirected();
        let mut ml = Fa2Multilevel::new(adj, params);
        assert!(ml.coarse_levels() >= 3, "levels {:?}", ml.level_sizes());
        let mut budgets = [0.5, 3.0, 0.05, 9.0].iter().cycle();
        while !ml.is_done() {
            ml.step(adj, *budgets.next().unwrap());
        }
        (one, ml.into_positions())
    };
    let (baseline, stepped) = on_pool(1, run);
    assert_eq!(baseline, stepped, "stepped differs from one call");
    for threads in [2, 3, 4, 8] {
        let (got, got_stepped) = on_pool(threads, run);
        assert_eq!(got.0, baseline.0, "x diverged on {threads} threads");
        assert_eq!(got.1, baseline.1, "y diverged on {threads} threads");
        assert_eq!(
            got_stepped, baseline,
            "stepped diverged on {threads} threads"
        );
    }
}

/// The edges of `seeded_graph(n, m)`, to build a graph that grows from it.
fn seeded_edges(n: usize, m: usize) -> (Vec<u32>, Vec<u32>) {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u32
    };
    let (mut src, mut dst) = (Vec::with_capacity(m), Vec::with_capacity(m));
    for _ in 0..m {
        src.push(next() % n as u32);
        dst.push(next() % n as u32);
    }
    (src, dst)
}

#[test]
fn fa2_masks_are_thread_count_independent() {
    // Fixed nodes and mixed inertia on the plain state: the masks change
    // which forces are computed and how the speed's sums are weighted, and
    // neither may depend on the pool.
    let topo = seeded_graph(1500, 3000);
    let adj = topo.undirected();
    let run = || {
        let mut state = Fa2State::new(
            adj,
            Fa2Params {
                seed: Some(5),
                ..Fa2Params::default()
            },
        );
        state.step(adj, 10);
        state.set_fixed(Some((0..1500).map(|i| i % 7 == 0).collect()));
        state.set_inertia(Some(
            (0..1500).map(|i| 1.0 + (i % 4) as f32 * 3.0).collect(),
        ));
        state.step(adj, 40);
        state.into_positions()
    };
    let baseline = on_pool(1, run);
    assert!(baseline.0.iter().chain(&baseline.1).all(|v| v.is_finite()));
    for threads in THREADS {
        let got = on_pool(threads, run);
        assert_eq!(got.0, baseline.0, "x diverged on {threads} threads");
        assert_eq!(got.1, baseline.1, "y diverged on {threads} threads");
    }
}

#[test]
fn fa2_grow_is_thread_count_independent() {
    // A layout grown three times: neighbours of one node with pins, then of
    // another with the first lot now old and everything free to move, then
    // with the whole drawing frozen. The same sequence must give the same bits on every pool, and
    // twice on the same one.
    let n0 = 1200usize;
    let (src0, dst0) = seeded_edges(n0, 2400);
    let mut graphs = vec![Topology::build(n0, src0.clone(), dst0.clone())];
    let (mut src, mut dst, mut n) = (src0, dst0, n0 as u32);
    for (hub, add) in [(17u32, 16u32), (900, 40), (1210, 150)] {
        for j in 0..add {
            src.push(hub);
            dst.push(n + j);
            if j % 5 == 0 {
                // Ties among the new nodes, and back into the drawing.
                src.push(n + j);
                dst.push(n + (j + 1) % add);
                src.push(n + j);
                dst.push((j * 37) % n0 as u32);
            }
        }
        n += add;
        graphs.push(Topology::build(n as usize, src.clone(), dst.clone()));
    }
    let steps: [(&[u32], WarmParams); 3] = [
        (&[3, 17, 500], WarmParams::default()),
        (
            &[17, 1201],
            // The whole component free, the expanded node too: the path
            // with inertia and no tree of fixed nodes to lean on.
            WarmParams {
                reach: u32::MAX,
                hold_parents: false,
                ..WarmParams::default()
            },
        ),
        (
            &[],
            WarmParams {
                freeze_above: 1000,
                ..WarmParams::default()
            },
        ),
    ];
    let run = || {
        let adj = graphs[0].undirected();
        let mut state = Fa2State::new(adj, Fa2Params::default());
        state.step(adj, 80);
        let mut scheduled = Vec::new();
        for (g, (pinned, params)) in graphs[1..].iter().zip(&steps) {
            let adj = g.undirected();
            let iterations = fa2_grow(&mut state, adj, None, pinned, *params);
            scheduled.push(iterations);
            // Uneven chunks, as a view steps it.
            let mut left = iterations;
            for chunk in [7u32, 1, 30].iter().cycle() {
                if left == 0 {
                    break;
                }
                let c = (*chunk).min(left);
                state.step(adj, c);
                left -= c;
            }
        }
        let (x, y) = state.positions();
        (x.to_vec(), y.to_vec(), scheduled)
    };
    let baseline = on_pool(1, run);
    assert_eq!(baseline.2, [60, 60, 200]);
    assert_eq!(baseline.0.len(), 1200 + 16 + 40 + 150);
    assert!(baseline.0.iter().chain(&baseline.1).all(|v| v.is_finite()));
    assert_eq!(on_pool(1, run), baseline, "a second run differs");
    for threads in THREADS {
        let got = on_pool(threads, run);
        assert_eq!(got.0, baseline.0, "x diverged on {threads} threads");
        assert_eq!(got.1, baseline.1, "y diverged on {threads} threads");
    }
}

#[test]
fn connected_components_are_thread_count_independent() {
    // Above the kernels' parallel threshold (65,536 edges): at one thread they
    // run serially, which defines the labels, and in parallel at the others
    // (#383). Sparse enough for many weak and strong components.
    for (n, m) in [(100_000, 70_000), (40_000, 120_000)] {
        let t = seeded_graph(n, m);
        let run = || {
            (
                connected_components_weak(&t, None),
                connected_components_strong(&t, None),
            )
        };
        let reference = on_pool(1, run);
        for &threads in &THREADS {
            let (weak, strong) = on_pool(threads, run);
            assert_eq!(
                weak, reference.0,
                "weak components diverged at {threads} threads"
            );
            assert_eq!(
                strong, reference.1,
                "strong components diverged at {threads} threads"
            );
        }
    }
}

/// The catalog's `layout_fa2` (what SQL's `layout_fa2(g, ...)` runs), coarse
/// to fine and flat, with its named parameters, across thread counts.
#[test]
fn catalog_layout_fa2_is_thread_count_independent() {
    use arrow::array::{Array, Float32Array};
    use ursa_core::catalog::{self, Arg, Evaluator, Inputs};

    let topo = seeded_graph(2000, 5000);
    let calls: [Vec<(&str, Arg)>; 3] = [
        vec![],
        vec![("iterations", Arg::Int(30)), ("seed", Arg::Int(4))],
        vec![
            ("multilevel", Arg::Bool(false)),
            ("iterations", Arg::Int(40)),
            ("strong_gravity", Arg::Bool(true)),
            ("theta", Arg::Float(0.8)),
        ],
    ];
    let spec = catalog::lookup("layout_fa2").unwrap();
    for args in calls {
        let call = spec.resolve(args.clone()).unwrap().call;
        let run = || {
            let columns = Evaluator::new(&topo, None)
                .evaluate(&call, Inputs::default())
                .unwrap();
            columns
                .iter()
                .map(|c| {
                    let c = c.as_any().downcast_ref::<Float32Array>().unwrap();
                    c.values().iter().map(|v| v.to_bits()).collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        let baseline = on_pool(1, run);
        assert_eq!(baseline.len(), 2);
        assert_eq!(baseline[0].len(), topo.n_nodes());
        for threads in [2, 3, 8] {
            assert_eq!(
                on_pool(threads, run),
                baseline,
                "{args:?} on {threads} threads"
            );
        }
    }
}
