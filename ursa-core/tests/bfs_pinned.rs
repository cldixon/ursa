//! Breadth-first searches, pinned (#452).
//!
//! The unweighted searches switch from top-down to bottom-up steps when the
//! frontier is large (direction-optimizing BFS). A node's distance does not
//! depend on the direction a level was searched in, but the order in which a
//! level's nodes are found does, so `hop`'s rows may come back in another
//! order. These tests pin what must not change: golden hashes, recorded from
//! the top-down kernels before the change, of
//!
//! - `bfs_distances` (one distance per node) from several sources;
//! - `k_hop_distances_masked`, as its rows **sorted** (an order-insensitive
//!   hash of the `(seed, reached, distance)` multiset), from a hub, node 0 and
//!   a seed list with a repeat and an unknown id, at depths 1, 2, 3 and
//!   unbounded;
//! - `k_hop_reached_masked` (one flag per node) from the same seed sets;
//! - `avg_path_length` and `diameter`, exact and sampled;
//!
//! on karate, R-MAT (two sizes), a disconnected graph with isolated nodes, a
//! sparse directed graph and a long path, in every direction, masked and not.
//! With `rayon` on, they are checked again on pools of 1 and 4 threads.

use ursa_core::algo::{bfs_distances, k_hop_distances_masked, k_hop_masked, k_hop_reached_masked};
use ursa_core::catalog::stats::{avg_path_length, diameter};
use ursa_core::{Direction, EdgeMask, Topology};

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

impl Edges {
    fn topology(&self) -> Topology {
        Topology::build(self.n, self.src.clone(), self.dst.clone())
    }
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

/// Several components and isolated nodes, 301 nodes: a directed 50-cycle, a
/// path of 70 linked both ways, a star of 40 pointing out, a star of 40
/// pointing in, a reciprocal pair, a self-loop, and nodes with no edge.
fn disconnected() -> Edges {
    let (mut src, mut dst) = (Vec::new(), Vec::new());
    let mut edge = |a: u32, b: u32| {
        src.push(a);
        dst.push(b);
    };
    for i in 0..50 {
        edge(i, (i + 1) % 50);
    }
    for i in 60..129 {
        edge(i, i + 1);
        edge(i + 1, i);
    }
    for i in 141..180 {
        edge(140, i);
    }
    for i in 191..230 {
        edge(i, 190);
    }
    edge(240, 241);
    edge(241, 240);
    edge(250, 250);
    Edges { n: 301, src, dst }
}

/// A random directed graph: most searches reach part of the graph.
fn sparse_directed(n: usize, m: usize, seed: u64) -> Edges {
    let mut st = seed;
    let (src, dst) = (0..m)
        .map(|_| {
            (
                (next(&mut st) % n as u64) as u32,
                (next(&mut st) % n as u64) as u32,
            )
        })
        .unzip();
    Edges { n, src, dst }
}

/// A directed path of `n` nodes: one node per level, many levels.
fn path(n: u32) -> Edges {
    Edges {
        n: n as usize,
        src: (0..n - 1).collect(),
        dst: (1..n).collect(),
    }
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

const DIRS: [Direction; 3] = [Direction::Out, Direction::In, Direction::Both];

/// Depths for `hop`: 1, 2, 3 and far past every fixture's depth.
const DEPTHS: [u32; 4] = [1, 2, 3, 1000];

/// The fixtures, each with a mask over its edges, and whether the path
/// statistics run exact (every source) or sampled on it.
fn fixtures() -> Vec<(&'static str, Edges, EdgeMask, bool)> {
    [
        ("karate", karate(), true),
        ("rmat", rmat(20_000, 11, 42), true),
        ("rmat_large", rmat(240_000, 15, 5), false),
        ("disconnected", disconnected(), true),
        ("sparse_directed", sparse_directed(1000, 1300, 7), true),
        ("path", path(200), true),
    ]
    .into_iter()
    .map(|(name, e, exact)| {
        let m = mask(e.src.len(), 99);
        (name, e, m, exact)
    })
    .collect()
}

/// The node with the most out-edges (the lowest such id): a hub.
fn hub(e: &Edges) -> u32 {
    let mut deg = vec![0u32; e.n];
    for &s in &e.src {
        deg[s as usize] += 1;
    }
    let max = *deg.iter().max().unwrap();
    deg.iter().position(|&d| d == max).unwrap() as u32
}

/// The seed sets: the hub alone, node 0 alone, and several seeds with a
/// repeat, an unknown id and the last node.
fn seed_sets(e: &Edges) -> Vec<(&'static str, Vec<u32>)> {
    let h = hub(e);
    let last = e.n as u32 - 1;
    vec![
        ("hub", vec![h]),
        ("zero", vec![0]),
        (
            "many",
            vec![h, 0, 5, 5, e.n as u32 + 7, last, 17 % e.n as u32],
        ),
    ]
}

/// `hop`'s rows, sorted: an order-insensitive hash of the row multiset.
fn hop_hash(rows: (Vec<u32>, Vec<u32>, Vec<u32>)) -> u64 {
    let (s, r, d) = rows;
    assert!(s.len() == r.len() && r.len() == d.len());
    let mut rows: Vec<(u32, u32, u32)> = (0..s.len()).map(|i| (s[i], r[i], d[i])).collect();
    rows.sort_unstable();
    let len = rows.len() as u64;
    fnv(std::iter::once(len).chain(
        rows.into_iter()
            .map(|(s, r, d)| (u64::from(s) << 32 | u64::from(r)) ^ (u64::from(d) << 56)),
    ))
}

fn bools_hash(v: &[bool]) -> u64 {
    fnv(std::iter::once(v.len() as u64).chain(v.iter().map(|&b| u64::from(b))))
}

fn dist_hash(v: &[i32]) -> u64 {
    fnv(std::iter::once(v.len() as u64).chain(v.iter().map(|&d| d as i64 as u64)))
}

/// Every pinned result, as `name -> hash`, in a fixed order.
fn compute() -> Vec<(String, u64)> {
    let mut got = Vec::new();
    for (name, e, m, exact) in fixtures() {
        let t = e.topology();
        // With the in-adjacency built, direction `out` may go bottom-up too
        // (it does not build one just for that).
        t.incoming();
        let sources = [0, hub(&e), e.n as u32 / 2, e.n as u32 - 1];
        for dir in DIRS {
            for (i, &s) in sources.iter().enumerate() {
                got.push((
                    format!("{name}/bfs_distances/{dir:?}/src{i}"),
                    dist_hash(&bfs_distances(&t, s, dir)),
                ));
            }
            for masked in [false, true] {
                let mk = masked.then_some(&m);
                for (set, seeds) in seed_sets(&e) {
                    for k in DEPTHS {
                        let rows = k_hop_distances_masked(&t, mk, &seeds, k, dir);
                        // `k_hop_masked` is the same rows without the distance.
                        let (s2, r2) = k_hop_masked(&t, mk, &seeds, k, dir);
                        assert_eq!((&rows.0, &rows.1), (&s2, &r2));
                        got.push((
                            format!("{name}/hop/{dir:?}/{masked}/{set}/{k}"),
                            hop_hash(rows),
                        ));
                    }
                    for k in [0, 1, 2, 1000] {
                        got.push((
                            format!("{name}/reached/{dir:?}/{masked}/{set}/{k}"),
                            bools_hash(&k_hop_reached_masked(&t, mk, &seeds, k, dir)),
                        ));
                    }
                }
                let sample = (!exact).then_some(0.02);
                let apl = avg_path_length(&t, mk, sample, dir);
                let dia = diameter(&t, mk, !exact, dir);
                got.push((
                    format!("{name}/stats/{dir:?}/{masked}"),
                    fnv([apl.to_bits(), dia as u64]),
                ));
            }
        }
    }
    got
}

/// One hash over every pinned result, and the count, so the golden table stays
/// short; a mismatch prints every per-result hash to find the one that moved.
fn digest(got: &[(String, u64)]) -> (usize, u64) {
    (got.len(), fnv(got.iter().map(|(_, h)| *h)))
}

fn check(got: &[(String, u64)]) {
    let d = digest(got);
    if d != GOLDEN {
        for (name, h) in got {
            println!("{name} 0x{h:016x}");
        }
    }
    println!("digest: ({}, 0x{:016x})", d.0, d.1);
    assert_eq!(d, GOLDEN, "a BFS result changed");
}

/// Recorded from the top-down kernels (main at 7bcb19c), before #452.
const GOLDEN: (usize, u64) = (972, 0x8b8f1d78776b75d6);

#[test]
fn bfs_results_match_the_golden_hashes() {
    check(&compute());
}

#[cfg(feature = "rayon")]
#[test]
fn bfs_results_match_the_golden_hashes_on_1_and_4_threads() {
    for threads in [1, 4] {
        let got = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(compute);
        check(&got);
    }
}

/// `hop`'s rows in the order they come back (not sorted), on one fixture.
#[cfg(feature = "rayon")]
fn hop_rows_in_order() -> Vec<(Vec<u32>, Vec<u32>, Vec<u32>)> {
    let e = rmat(240_000, 15, 5);
    let t = e.topology();
    t.incoming();
    let m = mask(e.src.len(), 99);
    let mut out = Vec::new();
    for dir in DIRS {
        for mk in [None, Some(&m)] {
            for (_, seeds) in seed_sets(&e) {
                for k in DEPTHS {
                    out.push(k_hop_distances_masked(&t, mk, &seeds, k, dir));
                }
            }
        }
    }
    out
}

/// The order of `hop`'s rows is not defined, but it does not depend on the
/// number of threads.
#[cfg(feature = "rayon")]
#[test]
fn hop_row_order_does_not_depend_on_the_thread_count() {
    let on = |threads| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(hop_rows_in_order)
    };
    let one = on(1);
    for threads in [2, 3, 4] {
        assert!(on(threads) == one, "hop rows differ on {threads} threads");
    }
}
