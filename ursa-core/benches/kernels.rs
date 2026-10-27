//! Criterion micro-benchmarks for the ursa-core kernels (#178, #21).
//!
//! These time the Rust kernels directly — no DataFusion plan, no Python — so a
//! kernel regression shows up here before it is buried in the cross-library
//! harness's end-to-end numbers (`benchmarks/`). Only the public ursa-core API is
//! used.
//!
//! Graphs come from a seeded xorshift stream, so every run and every machine sees
//! the same edges with no RNG dependency. Two families:
//!
//! - `er`   — Erdős–Rényi `G(n, m)`: uniform endpoints, flat degree distribution.
//! - `rmat` — R-MAT (Chakrabarti et al., Graph500 quadrant probabilities): skewed,
//!   power-law-ish degrees with hubs — the shape that exposes per-node imbalance.
//!
//! Sizes are named by edge count. `10k` stays under the parallel CSR-build
//! threshold (`2^16` edges, see `topology.rs`), so `build/*/10k` times the serial
//! path; `100k` and `1M` take the parallel one. Quadratic-ish kernels (closeness,
//! Louvain — #125) run only at the small sizes to keep a full run CI-sized.
//!
//! Compare two commits with criterion baselines (see benchmarks/README.md):
//!
//! ```text
//! cargo bench -p ursa-core -- --save-baseline main
//! cargo bench -p ursa-core -- --baseline main
//! ```

use std::time::Duration;

use arrow::array::Int64Array;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, SamplingMode};
use ursa_core::algo::{
    betweenness, bfs_distances, closeness, clustering_coefficient, connected_components_strong,
    connected_components_weak, degree, k_hop, label_propagation, louvain, neighbor_aggregate,
    pagerank, pagerank_weighted, random_walk, shortest_path, triangle_count, AggKind,
    PageRankParams,
};
use ursa_core::{Direction, IdMap, Topology};

// ---------------------------------------------------------------------------
// Synthetic graphs.
// ---------------------------------------------------------------------------

/// xorshift64* — tiny, seeded, and plenty for synthetic edge lists.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        // A zero state is a fixed point of xorshift; mix the seed away from it.
        XorShift(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n` (multiply-shift; the bias is irrelevant here).
    fn below(&mut self, n: u32) -> u32 {
        (((self.next_u64() >> 32) * n as u64) >> 32) as u32
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

/// A synthetic edge list in dense `u32` space.
#[derive(Clone)]
struct Edges {
    n: usize,
    src: Vec<u32>,
    dst: Vec<u32>,
}

impl Edges {
    fn topology(&self) -> Topology {
        Topology::build(self.n, self.src.clone(), self.dst.clone())
    }
}

/// Erdős–Rényi `G(n, m)`: `m` directed edges with independent uniform endpoints.
fn erdos_renyi(n: usize, m: usize, seed: u64) -> Edges {
    let mut rng = XorShift::new(seed);
    let (mut src, mut dst) = (Vec::with_capacity(m), Vec::with_capacity(m));
    for _ in 0..m {
        src.push(rng.below(n as u32));
        dst.push(rng.below(n as u32));
    }
    Edges { n, src, dst }
}

/// R-MAT on `2^scale` nodes with the Graph500 quadrant probabilities
/// `(a, b, c) = (0.57, 0.19, 0.19)`: each edge descends `scale` levels of the
/// adjacency matrix, picking a quadrant per level.
fn rmat(scale: u32, m: usize, seed: u64) -> Edges {
    const A: f64 = 0.57;
    const B: f64 = 0.19;
    const C: f64 = 0.19;
    let mut rng = XorShift::new(seed);
    let (mut src, mut dst) = (Vec::with_capacity(m), Vec::with_capacity(m));
    for _ in 0..m {
        let (mut u, mut v) = (0u32, 0u32);
        for _ in 0..scale {
            let r = rng.unit();
            let (bu, bv) = if r < A {
                (0, 0)
            } else if r < A + B {
                (0, 1)
            } else if r < A + B + C {
                (1, 0)
            } else {
                (1, 1)
            };
            u = (u << 1) | bu;
            v = (v << 1) | bv;
        }
        src.push(u);
        dst.push(v);
    }
    Edges {
        n: 1 << scale,
        src,
        dst,
    }
}

/// One positive weight per edge, seeded.
fn weights(m: usize, seed: u64) -> Vec<f64> {
    let mut rng = XorShift::new(seed);
    (0..m).map(|_| 0.1 + rng.unit()).collect()
}

/// Average out-degree 10 at every size.
fn er_10k() -> Edges {
    erdos_renyi(1_000, 10_000, 1)
}
fn er_100k() -> Edges {
    erdos_renyi(10_000, 100_000, 2)
}
fn er_1m() -> Edges {
    erdos_renyi(100_000, 1_000_000, 3)
}
/// ~8 edges per node.
fn rmat_100k() -> Edges {
    rmat(14, 100_000, 4)
}
fn rmat_1m() -> Edges {
    rmat(17, 1_000_000, 5)
}

/// A built topology with its lazy views (transpose, undirected CSR) forced, so a
/// kernel bench times the kernel and not a one-off cache fill.
fn warm(edges: &Edges) -> Topology {
    let t = edges.topology();
    t.incoming();
    t.undirected();
    t
}

// ---------------------------------------------------------------------------
// Benchmarks.
// ---------------------------------------------------------------------------

/// Run-wide defaults: short warm-up and measurement windows and the minimum
/// sample count, so the whole suite fits a CI runner. Override per run with
/// criterion's CLI (`--measurement-time`, `--sample-size`, …). Groups whose
/// iterations take milliseconds or more also switch to flat sampling, which runs
/// `sample_size` equal samples instead of linear sampling's `1 + 2 + … + 10`
/// iterations — the difference between seconds and minutes for a 2 s kernel.
fn config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
        .sample_size(10)
}

/// CSR build, then the two lazy views built from it: the transpose
/// (`incoming`) and the sorted/deduplicated undirected CSR. The views are cached
/// on the topology, so each sample builds a fresh one outside the timed region.
fn bench_topology(c: &mut Criterion) {
    let graphs = [
        ("er/10k", er_10k()),
        ("er/100k", er_100k()),
        ("er/1M", er_1m()),
        ("rmat/1M", rmat_1m()),
    ];
    let mut g = c.benchmark_group("topology");
    g.sampling_mode(SamplingMode::Flat);
    for (name, e) in &graphs {
        g.bench_function(BenchmarkId::new("build", name), |b| {
            b.iter_batched(
                || (e.src.clone(), e.dst.clone()),
                |(s, d)| Topology::build(e.n, s, d),
                BatchSize::LargeInput,
            )
        });
        g.bench_function(BenchmarkId::new("transpose", name), |b| {
            b.iter_batched(
                || e.topology(),
                |t| {
                    t.incoming();
                    t
                },
                BatchSize::LargeInput,
            )
        });
        g.bench_function(BenchmarkId::new("undirected", name), |b| {
            b.iter_batched(
                || e.topology(),
                |t| {
                    t.undirected();
                    t
                },
                BatchSize::LargeInput,
            )
        });
    }
    g.finish();
}

/// `i64` id interning (`IdMap::from_edge_arrays`, the `EdgeInterner` path):
/// `compact` ids are already `0..n`; `sparse` ids are the same graph scattered
/// over the full `i64` range by an odd multiplier (a bijection), so the hash map
/// sees real-looking keys.
fn bench_interning(c: &mut Criterion) {
    let mut g = c.benchmark_group("intern_i64");
    g.sampling_mode(SamplingMode::Flat);
    for (name, e) in [("100k", er_100k()), ("1M", er_1m())] {
        let compact = |v: &[u32]| Int64Array::from_iter_values(v.iter().map(|&x| x as i64));
        let sparse = |v: &[u32]| {
            Int64Array::from_iter_values(
                v.iter()
                    .map(|&x| (x as i64 + 1).wrapping_mul(0x5851_F42D_4C95_7F2D)),
            )
        };
        for (kind, src, dst) in [
            ("compact", compact(&e.src), compact(&e.dst)),
            ("sparse", sparse(&e.src), sparse(&e.dst)),
        ] {
            g.bench_function(BenchmarkId::new(kind, name), |b| {
                b.iter(|| IdMap::from_edge_arrays(&src, &dst).unwrap())
            });
        }
    }
    g.finish();
}

/// The linear-ish kernels, run at every size on both families.
fn bench_linear(c: &mut Criterion) {
    let graphs = [
        ("er/100k", er_100k()),
        ("er/1M", er_1m()),
        ("rmat/100k", rmat_100k()),
        ("rmat/1M", rmat_1m()),
    ];
    let mut g = c.benchmark_group("kernel");
    g.sampling_mode(SamplingMode::Flat);
    for (name, e) in &graphs {
        let t = warm(e);
        let w = weights(t.n_edges(), 7);
        let pr = PageRankParams::default();

        g.bench_function(BenchmarkId::new("degree_both", name), |b| {
            b.iter(|| degree(&t, None, Direction::Both))
        });
        g.bench_function(BenchmarkId::new("pagerank", name), |b| {
            b.iter(|| pagerank(&t, None, pr))
        });
        g.bench_function(BenchmarkId::new("pagerank_weighted", name), |b| {
            b.iter(|| pagerank_weighted(&t, &w, None, pr))
        });
        g.bench_function(BenchmarkId::new("components_weak", name), |b| {
            b.iter(|| connected_components_weak(&t, None))
        });
        g.bench_function(BenchmarkId::new("components_strong", name), |b| {
            b.iter(|| connected_components_strong(&t, None))
        });
        g.bench_function(BenchmarkId::new("triangle_count", name), |b| {
            b.iter(|| triangle_count(&t, None))
        });
        g.bench_function(BenchmarkId::new("clustering", name), |b| {
            b.iter(|| clustering_coefficient(&t, None))
        });
        g.bench_function(BenchmarkId::new("label_propagation", name), |b| {
            b.iter(|| label_propagation(&t, None, 10, Some(42)))
        });
        // ~100 sampled Brandes sources at every size.
        let frac = 100.0 / t.n_nodes() as f64;
        g.bench_function(BenchmarkId::new("betweenness_sampled", name), |b| {
            b.iter(|| betweenness(&t, None, Some(frac), Some(42)))
        });
    }
    g.finish();
}

/// Single-source traversals and walks: a small frontier workload per call.
fn bench_traversal(c: &mut Criterion) {
    let graphs = [("er/1M", er_1m()), ("rmat/1M", rmat_1m())];
    let mut g = c.benchmark_group("traversal");
    for (name, e) in &graphs {
        let t = warm(e);
        let n = t.n_nodes() as u32;
        let seeds: Vec<u32> = (0..64).map(|i| i * (n / 64)).collect();
        let attr: Vec<Option<f64>> = (0..n).map(|i| (i % 7 != 0).then_some(i as f64)).collect();

        g.bench_function(BenchmarkId::new("bfs_distances", name), |b| {
            b.iter(|| bfs_distances(&t, 0, Direction::Out))
        });
        g.bench_function(BenchmarkId::new("shortest_path", name), |b| {
            b.iter(|| shortest_path(&t, 0, n - 1, Direction::Both))
        });
        g.bench_function(BenchmarkId::new("k_hop_2", name), |b| {
            b.iter(|| k_hop(&t, &seeds, 2, Direction::Out))
        });
        g.bench_function(BenchmarkId::new("random_walk", name), |b| {
            b.iter(|| random_walk(&t, &seeds, 20, 10, Some(42)))
        });
        g.bench_function(BenchmarkId::new("neighbor_mean", name), |b| {
            b.iter(|| neighbor_aggregate(&t, &attr, None, Direction::Both, AggKind::Mean))
        });
    }
    g.finish();
}

/// Kernels too slow for the large sizes: exact closeness is `O(n·m)`, and
/// Louvain is the known slow path (#125).
fn bench_small(c: &mut Criterion) {
    let graphs = [("er/10k", er_10k()), ("er/100k", er_100k())];
    let mut g = c.benchmark_group("small");
    g.sampling_mode(SamplingMode::Flat);
    for (name, e) in &graphs {
        let t = warm(e);
        if t.n_edges() <= 10_000 {
            g.bench_function(BenchmarkId::new("closeness", name), |b| {
                b.iter(|| closeness(&t, None))
            });
        }
        g.bench_function(BenchmarkId::new("louvain", name), |b| {
            b.iter(|| louvain(&t, None, 1.0, Some(42)))
        });
    }
    g.finish();
}

criterion_group! {
    name = benches;
    config = config();
    targets = bench_topology, bench_interning, bench_linear, bench_traversal, bench_small
}
criterion_main!(benches);
