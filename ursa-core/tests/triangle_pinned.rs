//! Per-node triangle counts, pinned (#420).
//!
//! The counts of `triangle_count` on a varied set of graphs: hubs, cliques,
//! long and short neighbour lists side by side, self-loops, parallel and
//! reciprocal edges, vertices with no edge, and edge masks. Each is checked
//! twice: against a count made here from the edge list by a different method
//! (the common neighbours of each edge's ends), and against a hash of the
//! counts recorded from the kernel as it was before #420. So a change to the
//! kernel that alters any node's count, on any of these graphs, fails here.

use ursa_core::algo::{clustering_coefficient, triangle_count};
use ursa_core::{EdgeMask, Topology};

/// splitmix64.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn below(state: &mut u64, bound: u32) -> u32 {
    (next(state) % u64::from(bound)) as u32
}

/// FNV-1a over the counts.
fn hash(counts: &[u32]) -> u64 {
    counts.iter().fold(0xCBF2_9CE4_8422_2325, |h, &c| {
        (h ^ u64::from(c)).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

/// The per-node triangle counts of the kept edges, read as undirected, from
/// the edge list: a triangle on the edge {u, v} is a common neighbour of u and
/// v, and a node is on two edges of each of its triangles.
fn expected(n: usize, src: &[u32], dst: &[u32], keep: Option<&[bool]>) -> Vec<u32> {
    let mut adj = vec![Vec::new(); n];
    for (e, (&u, &v)) in src.iter().zip(dst).enumerate() {
        if u != v && keep.is_none_or(|k| k[e]) {
            adj[u as usize].push(v);
            adj[v as usize].push(u);
        }
    }
    for list in &mut adj {
        list.sort_unstable();
        list.dedup();
    }
    let mut seen = vec![false; n];
    (0..n)
        .map(|u| {
            for &v in &adj[u] {
                seen[v as usize] = true;
            }
            let twice: usize = adj[u]
                .iter()
                .map(|&v| {
                    adj[v as usize]
                        .iter()
                        .filter(|&&w| seen[w as usize])
                        .count()
                })
                .sum();
            for &v in &adj[u] {
                seen[v as usize] = false;
            }
            (twice / 2) as u32
        })
        .collect()
}

/// A graph, an optional mask, and what its counts must be.
struct Case {
    name: &'static str,
    n: usize,
    edges: Vec<(u32, u32)>,
    keep: Option<Vec<bool>>,
    /// The triangles of the graph, and the hash of the per-node counts.
    pinned: (u64, u64),
}

impl Case {
    fn new(name: &'static str, n: usize, edges: &[(u32, u32)], pinned: (u64, u64)) -> Case {
        Case {
            name,
            n,
            edges: edges.to_vec(),
            keep: None,
            pinned,
        }
    }

    /// The graph keeping only the edges `keep` says (by edge number and ends).
    fn masked(
        name: &'static str,
        n: usize,
        edges: &[(u32, u32)],
        keep: impl Fn(usize, u32, u32) -> bool,
        pinned: (u64, u64),
    ) -> Case {
        let keep = edges
            .iter()
            .enumerate()
            .map(|(e, &(u, v))| keep(e, u, v))
            .collect();
        Case {
            keep: Some(keep),
            ..Case::new(name, n, edges, pinned)
        }
    }
}

/// `m` random edges over `n` nodes, the smaller of two draws at one end (so
/// low ids are hubs), with self-loops and repeated and reciprocal edges.
fn skewed(seed: u64, n: u32, m: usize) -> Vec<(u32, u32)> {
    let mut st = seed;
    (0..m)
        .map(|_| {
            let a = below(&mut st, n).min(below(&mut st, n));
            let b = below(&mut st, n);
            if next(&mut st) & 1 == 0 {
                (a, b)
            } else {
                (b, a)
            }
        })
        .collect()
}

/// `m` R-MAT edges over `2^scale` nodes (Graph500 quadrants).
fn rmat(seed: u64, scale: u32, m: usize) -> Vec<(u32, u32)> {
    let mut st = seed;
    (0..m)
        .map(|_| {
            let (mut s, mut d) = (0u32, 0u32);
            for _ in 0..scale {
                let r = (next(&mut st) >> 11) as f64 / (1u64 << 53) as f64;
                s = s << 1 | u32::from(r >= 0.76);
                d = d << 1 | u32::from((0.57..0.76).contains(&r) || r >= 0.95);
            }
            (s, d)
        })
        .collect()
}

/// A clique on the first `k` nodes; then `leaves` nodes each tied to a few of
/// the clique's nodes and sometimes to the node before it; and one node tied
/// to every other. Long lists meet short ones, and many nodes share a degree.
fn clique_and_leaves(seed: u64, k: u32, leaves: u32) -> (usize, Vec<(u32, u32)>) {
    let mut st = seed;
    let mut edges = Vec::new();
    for a in 0..k {
        for b in a + 1..k {
            // One direction for some pairs, the other or both for the rest.
            match (a + b) % 3 {
                0 => edges.push((a, b)),
                1 => edges.push((b, a)),
                _ => edges.extend([(a, b), (b, a)]),
            }
        }
    }
    for leaf in k..k + leaves {
        for _ in 0..1 + below(&mut st, 4) {
            edges.push((leaf, below(&mut st, k)));
        }
        if leaf > k && below(&mut st, 3) == 0 {
            edges.push((leaf - 1, leaf));
        }
    }
    let hub = k + leaves;
    edges.extend((0..hub).map(|v| (hub, v)));
    edges.push((hub, hub));
    (hub as usize + 1, edges)
}

/// The edges with every node id multiplied by `step`: the same graph among
/// `step - 1` times as many nodes with no edge.
fn spread(edges: &[(u32, u32)], step: u32) -> Vec<(u32, u32)> {
    edges.iter().map(|&(u, v)| (u * step, v * step)).collect()
}

fn cases() -> Vec<Case> {
    let (n_clique, clique) = clique_and_leaves(3, 90, 2500);
    let hub = n_clique as u32 - 1;
    let dense = skewed(11, 300, 20_000);
    let wide = skewed(12, 4000, 60_000);
    let power = rmat(13, 12, 60_000);
    vec![
        Case::new("skewed 300", 300, &dense, (220_209, 0x6cbf_b66f_1f39_80ee)),
        Case::masked(
            "skewed 300, two edges in three",
            300,
            &dense,
            |e, _, _| e % 3 != 0,
            (83_607, 0x8de8_6472_0b50_a7be),
        ),
        Case::new("skewed 4000", 4000, &wide, (5637, 0xe0da_7a33_cc0f_04c4)),
        Case::masked(
            "skewed 4000, the edges among the first 500 nodes",
            4000,
            &wide,
            |_, u, v| u < 500 && v < 500,
            (50, 0xb1a5_fa83_607e_fecd),
        ),
        Case::new(
            "skewed 4000 among 36000 nodes with no edge",
            40_000,
            &spread(&wide, 10),
            (5637, 0x5d7d_36db_43a2_9008),
        ),
        Case::new("R-MAT 4096", 4096, &power, (413_480, 0x4996_ce0e_b509_ed0b)),
        Case::masked(
            "R-MAT 4096, one edge in four",
            4096,
            &power,
            |e, _, _| e % 4 == 1,
            (31_582, 0xe1dc_55fe_93d7_e505),
        ),
        Case::new(
            "R-MAT 4096 among 8192 nodes with no edge",
            3 * 4096,
            &spread(&power, 3),
            (413_480, 0x2c5e_413f_fde6_6913),
        ),
        Case::new(
            "clique and leaves",
            n_clique,
            &clique,
            (134_561, 0xa83c_1e6c_4c5e_16a0),
        ),
        Case::masked(
            "clique and leaves, without the node tied to all",
            n_clique,
            &clique,
            |_, u, _| u != hub,
            (123_573, 0xab7b_a5a4_aaae_5b08),
        ),
    ]
}

#[test]
fn per_node_counts_are_the_pinned_ones() {
    for case in cases() {
        let Case {
            name,
            n,
            edges,
            keep,
            pinned,
        } = case;
        let (src, dst): (Vec<u32>, Vec<u32>) = edges.into_iter().unzip();
        let expect = expected(n, &src, &dst, keep.as_deref());
        let t = Topology::build(n, src, dst);
        let mask = keep.as_deref().map(EdgeMask::from_bools);
        let counts = triangle_count(&t, mask.as_ref());
        assert_eq!(counts, expect, "{name}");
        let total = counts.iter().map(|&c| u64::from(c)).sum::<u64>() / 3;
        assert_eq!(
            (total, hash(&counts)),
            pinned,
            "{name}: (triangles, hash of the counts) is ({total}, {:#018x})",
            hash(&counts)
        );
        // The clustering coefficient runs the same pass: a node's coefficient
        // is positive exactly when the node is in a triangle.
        let cc = clustering_coefficient(&t, mask.as_ref());
        assert!(
            counts.iter().zip(&cc).all(|(&c, &x)| (c > 0) == (x > 0.0)),
            "{name}: clustering"
        );
    }
}

/// Small graphs whose counts can be read off.
#[test]
fn small_graphs_count_as_written() {
    let count = |n: usize, edges: &[(u32, u32)]| {
        let (src, dst) = edges.iter().copied().unzip();
        triangle_count(&Topology::build(n, src, dst), None)
    };
    // Two triangles sharing the edge 1-2, a tail to 4, node 5 with no edge and
    // node 6 with a self-loop only.
    let bowtie = [(0, 1), (1, 2), (2, 0), (1, 3), (3, 2), (3, 4), (6, 6)];
    assert_eq!(count(7, &bowtie), [1, 2, 2, 1, 0, 0, 0]);
    // The same with every edge twice, reversed, and a self-loop on each node.
    let noisy: Vec<(u32, u32)> = bowtie
        .iter()
        .flat_map(|&(u, v)| [(u, v), (v, u), (u, v), (u, u)])
        .collect();
    assert_eq!(count(7, &noisy), [1, 2, 2, 1, 0, 0, 0]);
    // A directed 3-cycle and a transitive triple are both one triangle.
    assert_eq!(count(3, &[(0, 1), (1, 2), (2, 0)]), [1, 1, 1]);
    assert_eq!(count(3, &[(0, 1), (1, 2), (0, 2)]), [1, 1, 1]);
    // K5 less the edge 3-4: nodes 0..3 are in 5 triangles, 3 and 4 in 3.
    let k5: Vec<(u32, u32)> = (0..5u32)
        .flat_map(|a| (a + 1..5).map(move |b| (b, a)))
        .filter(|&e| e != (4, 3))
        .collect();
    assert_eq!(count(5, &k5), [5, 5, 5, 3, 3]);
    // A wheel: the hub 0 and a 6-cycle around it.
    let wheel: Vec<(u32, u32)> = (1..=6u32).flat_map(|i| [(0, i), (i, i % 6 + 1)]).collect();
    assert_eq!(count(7, &wheel), [6, 2, 2, 2, 2, 2, 2]);
}
