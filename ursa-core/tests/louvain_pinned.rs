//! Louvain, pinned (#268).
//!
//! The default (serial) Louvain must give the same labels, bit for bit, after
//! any change made only for speed: the same communities and the same label
//! values, for every seed, on any thread count, with or without `rayon`.
//! These golden hashes were recorded from the kernel before #268's changes
//! (main at d9496af), on karate, R-MAT over 2^11 and 2^15 node slots (hubs,
//! parallel edges, self-loops and nodes with no edge), a graph of several
//! components with isolated nodes, and R-MAT with inexact weights; unweighted
//! and weighted, masked and not, at resolutions 1 and 0.5, unseeded and for
//! several seeds. The opt-in parallel Louvain (#387) shares the working graph
//! and the aggregation, so its labels are pinned too. With `rayon` on, every
//! hash is checked again on pools of 1, 2, 4 and 8 threads.

use ursa_core::algo::{louvain, louvain_weighted};
use ursa_core::{EdgeMask, NodeOrder, Topology};

/// Zachary's karate club (78 edges, each listed once from the lower id).
#[rustfmt::skip]
const KARATE: &[(u32, u32)] = &[
    (0, 1), (0, 2), (0, 3), (0, 4), (0, 5), (0, 6), (0, 7), (0, 8), (0, 10), (0, 11),
    (0, 12), (0, 13), (0, 17), (0, 19), (0, 21), (0, 31), (1, 2), (1, 3), (1, 7),
    (1, 13), (1, 17), (1, 19), (1, 21), (1, 30), (2, 3), (2, 7), (2, 8), (2, 9),
    (2, 13), (2, 27), (2, 28), (2, 32), (3, 7), (3, 12), (3, 13), (4, 6), (4, 10),
    (5, 6), (5, 10), (5, 16), (6, 16), (8, 30), (8, 32), (8, 33), (9, 33), (13, 33),
    (14, 32), (14, 33), (15, 32), (15, 33), (18, 32), (18, 33), (19, 33), (20, 32),
    (20, 33), (22, 32), (22, 33), (23, 25), (23, 27), (23, 29), (23, 32), (23, 33),
    (24, 25), (24, 27), (24, 31), (25, 31), (26, 29), (26, 33), (27, 33), (28, 31),
    (28, 33), (29, 32), (29, 33), (30, 32), (30, 33), (31, 32), (31, 33), (32, 33),
];

/// splitmix64.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

struct Edges {
    n: usize,
    src: Vec<u32>,
    dst: Vec<u32>,
}

fn karate() -> Edges {
    assert_eq!(KARATE.len(), 78);
    let (src, dst) = KARATE.iter().copied().unzip();
    Edges { n: 34, src, dst }
}

/// `m` R-MAT edges (Graph500 quadrants) over `2^scale` node slots: hubs,
/// self-loops, parallel edges and nodes with no edge.
fn rmat(m: usize, scale: u32, seed: u64) -> Edges {
    let mut st = seed;
    let (a, ab, abc) = (0.57, 0.76, 0.95);
    let (src, dst) = (0..m)
        .map(|_| {
            let (mut s, mut d) = (0u32, 0u32);
            for _ in 0..scale {
                let r = (next(&mut st) >> 11) as f64 / (1u64 << 53) as f64;
                s = s << 1 | u32::from(r >= ab);
                d = d << 1 | u32::from((r >= a && r < ab) || r >= abc);
            }
            (s, d)
        })
        .unzip();
    Edges {
        n: 1 << scale,
        src,
        dst,
    }
}

/// Several components and isolated nodes, 400 nodes: ten 6-cliques joined in
/// a ring, a path, a star, a reciprocal pair, a lone self-loop, and nodes
/// with no edge between them.
fn disconnected() -> Edges {
    let (mut src, mut dst) = (Vec::new(), Vec::new());
    let mut edge = |a: u32, b: u32| {
        src.push(a);
        dst.push(b);
    };
    for c in 0..10u32 {
        let base = c * 12;
        for i in 0..6 {
            for j in i + 1..6 {
                edge(base + i, base + j);
            }
        }
        edge(base + 5, (base + 12) % 120);
    }
    for i in 150..199 {
        edge(i, i + 1);
    }
    for i in 221..260 {
        edge(220, i);
    }
    edge(300, 301);
    edge(301, 300);
    edge(320, 320);
    Edges { n: 400, src, dst }
}

/// Inexact per-edge weights, so the order of every float sum shows.
fn weights(m: usize, seed: u64) -> Vec<f64> {
    let mut st = seed;
    (0..m)
        .map(|_| ((next(&mut st) % 97) as f64 + 1.0).sqrt() / 3.0)
        .collect()
}

/// Every edge row kept unless the seeded draw drops it (about a third).
fn mask(m: usize, seed: u64) -> EdgeMask {
    let mut st = seed;
    let keep: Vec<bool> = (0..m).map(|_| !next(&mut st).is_multiple_of(3)).collect();
    EdgeMask::from_bools(&keep)
}

/// FNV-1a over a stream of 64-bit words.
fn fnv(words: impl IntoIterator<Item = u64>) -> u64 {
    words.into_iter().fold(0xCBF2_9CE4_8422_2325, |h, x| {
        (h ^ x).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

fn labels_hash(v: &[u32]) -> u64 {
    fnv(std::iter::once(v.len() as u64).chain(v.iter().map(|&c| u64::from(c))))
}

const SEEDS: [Option<u64>; 4] = [None, Some(1), Some(7), Some(42)];

/// The fixtures: a name, the edges, and whether they also run weighted.
fn fixtures() -> Vec<(&'static str, Edges, bool)> {
    vec![
        ("karate", karate(), true),
        ("rmat11", rmat(30_000, 11, 3), true),
        ("rmat15", rmat(240_000, 15, 5), false),
        ("disconnected", disconnected(), true),
        ("rmat13", rmat(120_000, 13, 11), true),
    ]
}

/// Every pinned result, as `name -> hash`, in a fixed order.
fn compute() -> Vec<(String, u64)> {
    let mut got = Vec::new();
    for (name, e, weighted) in fixtures() {
        let t = Topology::build(e.n, e.src.clone(), e.dst.clone());
        let w = weights(e.src.len(), 17);
        let m = mask(e.src.len(), 99);
        for masked in [false, true] {
            let mk = masked.then_some(&m);
            for weigh in [false, true] {
                if weigh && !weighted {
                    continue;
                }
                for res in [1.0, 0.5] {
                    for parallel in [false, true] {
                        for seed in SEEDS {
                            let labels = if weigh {
                                louvain_weighted(&t, &w, mk, res, seed, parallel)
                            } else {
                                louvain(&t, mk, res, seed, parallel)
                            };
                            got.push((
                                format!(
                                    "{name}/masked={masked}/weighted={weigh}/res={res}/\
                                     parallel={parallel}/seed={seed:?}"
                                ),
                                labels_hash(&labels),
                            ));
                        }
                    }
                }
            }
        }
    }
    got
}

fn digest(got: &[(String, u64)]) -> (usize, u64) {
    (
        got.len(),
        fnv(got
            .iter()
            .flat_map(|(name, h)| name.bytes().map(u64::from).chain(std::iter::once(*h)))),
    )
}

fn check(got: &[(String, u64)]) {
    let d = digest(got);
    if d != GOLDEN {
        for (name, h) in got {
            println!("{name} 0x{h:016x}");
        }
    }
    println!("digest: ({}, 0x{:016x})", d.0, d.1);
    assert_eq!(d, GOLDEN, "a Louvain result changed");
}

/// Recorded from the kernel before #268 (main at d9496af).
const GOLDEN: (usize, u64) = (288, 0xaa8644d2259bd838);

#[test]
fn louvain_results_match_the_golden_hashes() {
    check(&compute());
}

/// A planted-partition graph, as `examples/rmat_kernel.rs --planted` makes
/// it: `edges / 16` nodes in consecutive communities of 16 to 79 nodes, and
/// `percent`% of the edges drawn between communities.
fn planted(edges: usize, percent: u64) -> Edges {
    let n = (edges / 16).max(16);
    let mut st = 7u64;
    let mut start = vec![0u32; n];
    let mut size = vec![0u32; n];
    let mut lo = 0usize;
    while lo < n {
        let len = (16 + (next(&mut st) % 64) as usize).min(n - lo);
        for u in lo..lo + len {
            start[u] = lo as u32;
            size[u] = len as u32;
        }
        lo += len;
    }
    let (src, dst) = (0..edges)
        .map(|_| {
            let s = (next(&mut st) % n as u64) as usize;
            let d = if (next(&mut st) % 100) < percent {
                (next(&mut st) % n as u64) as u32
            } else {
                start[s] + (next(&mut st) % u64::from(size[s])) as u32
            };
            (s as u32, d)
        })
        .unzip();
    Edges { n, src, dst }
}

/// Larger graphs, past the sizes at which Louvain's setup, renumbering and
/// shuffles go parallel: R-MAT, 1M edges over 2^18 node slots (most of them
/// isolated), and a planted partition of 137,500 nodes with 2.2M edges,
/// serial and parallel Louvain, seed 1. The hashes are those
/// `examples/rmat_kernel.rs --kernel louvain` printed for the same graphs
/// before #268 (main at d9496af): FNV-1a over the labels.
fn compute_large() -> Vec<(&'static str, bool, u64)> {
    let mut got = Vec::new();
    for (name, e) in [
        ("rmat18", rmat(1_000_000, 18, 42)),
        ("planted", planted(2_200_000, 30)),
    ] {
        let t = Topology::build(e.n, e.src.clone(), e.dst.clone());
        for parallel in [false, true] {
            let labels = louvain(&t, None, 1.0, Some(1), parallel);
            got.push((name, parallel, fnv(labels.iter().map(|&c| u64::from(c)))));
        }
        // The same graph relabelled by in-degree, as the SQL engine builds
        // an index this size (#172): other communities for the seed, since
        // the visit order follows the numbering, pinned here in their own
        // right. Read in original-id order.
        let t = Topology::build_ordered(e.n, e.src, e.dst, NodeOrder::InDegree);
        let relabelled = match name {
            "rmat18" => "rmat18, relabelled",
            _ => "planted, relabelled",
        };
        for parallel in [false, true] {
            let labels = louvain(&t, None, 1.0, Some(1), parallel);
            let by_original =
                (0..t.n_nodes() as u32).map(|o| labels[t.node_of_original(o) as usize]);
            got.push((relabelled, parallel, fnv(by_original.map(u64::from))));
        }
    }
    got
}

const GOLDEN_LARGE: &[(&str, bool, u64)] = &[
    ("rmat18", false, 0xaa57b292dfc64d40),
    ("rmat18", true, 0x3131f1aa5d828be8),
    ("rmat18, relabelled", false, 0xc139ad82384c0df4),
    ("rmat18, relabelled", true, 0x784f9d393bd85ac9),
    ("planted", false, 0x8f5802282bb165a4),
    ("planted", true, 0x78172d275afe1cec),
    ("planted, relabelled", false, 0x37cdae96a09737aa),
    ("planted, relabelled", true, 0x7946274668d4673a),
];

#[test]
fn large_louvain_results_match_the_golden_hashes() {
    assert_eq!(compute_large(), GOLDEN_LARGE);
}

#[cfg(feature = "rayon")]
#[test]
fn large_louvain_results_match_the_golden_hashes_on_1_2_4_and_8_threads() {
    for threads in [1, 2, 4, 8] {
        let got = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(compute_large);
        assert_eq!(got, GOLDEN_LARGE, "{threads} threads");
    }
}

#[cfg(feature = "rayon")]
#[test]
fn louvain_results_match_the_golden_hashes_on_1_2_4_and_8_threads() {
    for threads in [1, 2, 4, 8] {
        let got = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(compute);
        check(&got);
    }
}
