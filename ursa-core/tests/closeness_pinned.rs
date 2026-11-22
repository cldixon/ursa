//! Closeness centrality, pinned (#422).
//!
//! Unweighted closeness runs 64 sources per breadth-first pass with one bit per
//! source. The integers it folds (each source's distance sum and reach count) are
//! the per-source search's, so the `f64` results must be the same bits. Two checks
//! hold that: golden hashes of the output, recorded from the one-search-per-source
//! kernel before the change, on karate, R-MAT, a disconnected graph with isolated
//! nodes, a sparse directed graph and a long path, in every direction, masked and
//! not; and a plain per-source search written here, compared bit for bit.
//! Weighted closeness (Dijkstra) is not batched; its hashes are pinned too.

use ursa_core::algo::{closeness, closeness_directed, closeness_weighted_directed};
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

/// Several components and isolated nodes, 301 nodes (not a multiple of 64): a
/// directed 50-cycle, a path of 70 linked both ways, a star of 40 pointing out,
/// a star of 40 pointing in, a reciprocal pair, a self-loop, and nodes with no
/// edge between and after them.
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

/// A sparse random directed graph: most searches reach part of the graph.
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

/// A directed path of `n` nodes: searches far deeper than 64 levels.
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

/// FNV-1a over the result's bits.
fn hash(xs: &[f64]) -> u64 {
    xs.iter().fold(0xCBF2_9CE4_8422_2325, |h, x| {
        (h ^ x.to_bits()).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

/// One breadth-first search per source, folded as `reachable / Σ dist` in
/// integers: the definition the kernel must keep, bit for bit.
fn reference(t: &Topology, mask: Option<&EdgeMask>, dir: Direction) -> Vec<f64> {
    let n = t.n_nodes();
    (0..n as u32)
        .map(|u| {
            let mut dist = vec![-1i64; n];
            dist[u as usize] = 0;
            let mut frontier = vec![u];
            let (mut sum, mut reach) = (0i64, 0i64);
            while !frontier.is_empty() {
                let mut next = Vec::new();
                for &v in &frontier {
                    let d = dist[v as usize] + 1;
                    t.for_each_neighbor(v, dir, mask, |w| {
                        if dist[w as usize] < 0 {
                            dist[w as usize] = d;
                            sum += d;
                            reach += 1;
                            next.push(w);
                        }
                    });
                }
                frontier = next;
            }
            if sum > 0 {
                reach as f64 / sum as f64
            } else {
                0.0
            }
        })
        .collect()
}

const DIRS: [Direction; 3] = [Direction::Out, Direction::In, Direction::Both];

/// The fixtures, each with a mask over its edges.
fn fixtures() -> Vec<(&'static str, Edges, EdgeMask)> {
    [
        ("karate", karate()),
        ("rmat", rmat(20_000, 11, 42)),
        ("disconnected", disconnected()),
        ("sparse_directed", sparse_directed(1000, 1300, 7)),
        ("path", path(200)),
    ]
    .into_iter()
    .map(|(name, e)| {
        let m = mask(e.src.len(), 99);
        (name, e, m)
    })
    .collect()
}

/// Compare `name -> hash` pairs with the pinned ones, printing them all first so
/// a deliberate change can be re-recorded.
fn check(got: &[(String, u64)], want: &[(&str, u64)], what: &str) {
    for (name, h) in got {
        println!("    (\"{name}\", 0x{h:016x}),");
    }
    assert_eq!(got.len(), want.len());
    for ((name, h), (want_name, want_h)) in got.iter().zip(want) {
        assert_eq!(name, want_name);
        assert_eq!(*h, *want_h, "{name}: {what} changed");
    }
}

#[test]
fn unweighted_closeness_matches_the_golden_hashes() {
    let mut got = Vec::new();
    for (name, e, m) in fixtures() {
        let t = e.topology();
        for dir in DIRS {
            for masked in [false, true] {
                let c = closeness_directed(&t, masked.then_some(&m), dir);
                got.push((format!("{name}/{dir:?}/{masked}"), hash(&c)));
            }
        }
    }
    check(&got, GOLDEN_UNWEIGHTED, "closeness");
}

#[test]
fn weighted_closeness_matches_the_golden_hashes() {
    let mut got = Vec::new();
    for (name, e, m) in fixtures() {
        let t = e.topology();
        let w: Vec<f64> = (0..e.src.len())
            .map(|i| 0.25 + (i % 11) as f64 * 0.75)
            .collect();
        for dir in DIRS {
            for masked in [false, true] {
                let c = closeness_weighted_directed(&t, &w, masked.then_some(&m), dir);
                got.push((format!("{name}/{dir:?}/{masked}"), hash(&c)));
            }
        }
    }
    check(&got, GOLDEN_WEIGHTED, "weighted closeness");
}

#[test]
fn unweighted_closeness_equals_a_per_source_search_bit_for_bit() {
    for (name, e, m) in fixtures() {
        let t = e.topology();
        for dir in DIRS {
            for mask in [None, Some(&m)] {
                let got = closeness_directed(&t, mask, dir);
                let want = reference(&t, mask, dir);
                assert_eq!(got.len(), want.len());
                for (u, (g, w)) in got.iter().zip(&want).enumerate() {
                    assert_eq!(
                        g.to_bits(),
                        w.to_bits(),
                        "{name} {dir:?} masked={} node {u}: {g} vs {w}",
                        mask.is_some()
                    );
                }
            }
        }
    }
}

#[test]
fn closeness_is_closeness_directed_out() {
    let t = rmat(20_000, 11, 42).topology();
    assert_eq!(
        hash(&closeness(&t, None)),
        hash(&closeness_directed(&t, None, Direction::Out))
    );
}

#[test]
fn tiny_graphs_and_batch_boundaries() {
    // No nodes; one isolated node; one self-loop.
    assert!(closeness(&Topology::build(0, vec![], vec![]), None).is_empty());
    assert_eq!(closeness(&Topology::build(1, vec![], vec![]), None), [0.0]);
    assert_eq!(
        closeness(&Topology::build(1, vec![0], vec![0]), None),
        [0.0]
    );
    // Directed cycles around a 64-node batch boundary: every node reaches the
    // others at distances 1..n-1, so all scores are the same bits.
    for n in [63u32, 64, 65, 129] {
        let src: Vec<u32> = (0..n).collect();
        let dst: Vec<u32> = (0..n).map(|i| (i + 1) % n).collect();
        let c = closeness(&Topology::build(n as usize, src, dst), None);
        let want = (n - 1) as f64 / ((n - 1) * n / 2) as f64;
        assert!(c.iter().all(|x| x.to_bits() == want.to_bits()), "n={n}");
    }
}

/// Recorded from the one-search-per-source kernel (main at f29e7a6), before #422.
const GOLDEN_UNWEIGHTED: &[(&str, u64)] = &[
    ("karate/Out/false", 0x68d9d1760cba1664),
    ("karate/Out/true", 0x63f734557b6cb2e1),
    ("karate/In/false", 0x8c3656fb9631af1a),
    ("karate/In/true", 0x203b6016b6547943),
    ("karate/Both/false", 0xc56ba3a5926848ac),
    ("karate/Both/true", 0x49b8abe93d9e05f4),
    ("rmat/Out/false", 0x30d2b64725b4d8db),
    ("rmat/Out/true", 0x74b926ec8081433e),
    ("rmat/In/false", 0xf1cce9cd2e174037),
    ("rmat/In/true", 0x6d579629b1472161),
    ("rmat/Both/false", 0x8c30cdb5a3cd2399),
    ("rmat/Both/true", 0x2d6fd54e6f268b26),
    ("disconnected/Out/false", 0x5e6a389c4eb5c7a9),
    ("disconnected/Out/true", 0x62f9e7023494364d),
    ("disconnected/In/false", 0x880a389c4eb5c7a9),
    ("disconnected/In/true", 0x03ed4b0eee0f59a4),
    ("disconnected/Both/false", 0x30d7c41a81b1012f),
    ("disconnected/Both/true", 0x182b0470b665c582),
    ("sparse_directed/Out/false", 0x8c664604fe1127fb),
    ("sparse_directed/Out/true", 0xfa5533233b964b14),
    ("sparse_directed/In/false", 0x102e9021d059969b),
    ("sparse_directed/In/true", 0x0d31da6a73b69ac9),
    ("sparse_directed/Both/false", 0x8447b9c0c94b3eca),
    ("sparse_directed/Both/true", 0x2e7f9834cb298758),
    ("path/Out/false", 0x6256e32b19d8b1d6),
    ("path/Out/true", 0x472f0c1869699c6f),
    ("path/In/false", 0x8b80353599f99cc8),
    ("path/In/true", 0x1ae2e0744355a991),
    ("path/Both/false", 0x87d3e13098e3890d),
    ("path/Both/true", 0x2c5cd5ac0055e7dc),
];

/// Recorded at the same commit; weighted closeness is not changed by #422.
const GOLDEN_WEIGHTED: &[(&str, u64)] = &[
    ("karate/Out/false", 0x86d66dd542700bc5),
    ("karate/Out/true", 0xda4f20c9a5e2bb2d),
    ("karate/In/false", 0x13a9fbea0f9feb12),
    ("karate/In/true", 0xb73f3ff6bfb5ffb9),
    ("karate/Both/false", 0x815556ce4fafacb8),
    ("karate/Both/true", 0xe5eb840912eb55b3),
    ("rmat/Out/false", 0x3f1d95827261336f),
    ("rmat/Out/true", 0x618dee2de56e9b1a),
    ("rmat/In/false", 0x3cf1e3e088567eca),
    ("rmat/In/true", 0x1a27f8f2708b3d41),
    ("rmat/Both/false", 0x818e1cda8dd7fdbd),
    ("rmat/Both/true", 0xc0a3eee5dcb459ca),
    ("disconnected/Out/false", 0x2b96bea1f0c01efd),
    ("disconnected/Out/true", 0xc5013187b2d896a1),
    ("disconnected/In/false", 0xdd9dcdafe7851726),
    ("disconnected/In/true", 0xebb798a7c528864d),
    ("disconnected/Both/false", 0xb2db830863bc1126),
    ("disconnected/Both/true", 0xc7b07ac0f100a6ea),
    ("sparse_directed/Out/false", 0x959b284007982903),
    ("sparse_directed/Out/true", 0x6dd3f0a4a46c2b04),
    ("sparse_directed/In/false", 0x3b3c26b289c65f0e),
    ("sparse_directed/In/true", 0xada1566bd1e73527),
    ("sparse_directed/Both/false", 0x9e398e3aad072ffa),
    ("sparse_directed/Both/true", 0x4468ecb4fbadf7c2),
    ("path/Out/false", 0x295720641d468350),
    ("path/Out/true", 0xbba73570f5756666),
    ("path/In/false", 0x14d7de0f9a12a1fd),
    ("path/In/true", 0x47d322c9d84063ae),
    ("path/Both/false", 0x24b918e4e29dc50e),
    ("path/Both/true", 0x8bd7ba299528a3d1),
];
