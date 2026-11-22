//! Parallel label propagation (`parallel = true`, #453), pinned.
//!
//! The parallel sweeps find different labels from the serial default, and
//! NetworkX has no equivalent to check them against, so they are pinned
//! three ways instead: the same labels on any thread count
//! (`determinism_threads.rs`); here, golden hashes of the labels on fixture
//! graphs, so any change to what it computes shows; and, on planted-partition
//! graphs, communities about as good as the serial run's (count and
//! modularity), so a change that keeps it deterministic but makes it worse
//! shows too. The serial run's labels are pinned here as well, so a change
//! meant to keep them (#453's serial wins) is checked to.

use ursa_core::algo::label_propagation;
use ursa_core::{EdgeMask, Topology};

/// Zachary's karate club (78 edges).
const KARATE: &[(u32, u32)] = &[
    (0, 1),
    (0, 2),
    (0, 3),
    (0, 4),
    (0, 5),
    (0, 6),
    (0, 7),
    (0, 8),
    (0, 10),
    (0, 11),
    (0, 12),
    (0, 13),
    (0, 17),
    (0, 19),
    (0, 21),
    (0, 31),
    (1, 2),
    (1, 3),
    (1, 7),
    (1, 13),
    (1, 17),
    (1, 19),
    (1, 21),
    (1, 30),
    (2, 3),
    (2, 7),
    (2, 8),
    (2, 9),
    (2, 13),
    (2, 27),
    (2, 28),
    (2, 32),
    (3, 7),
    (3, 12),
    (3, 13),
    (4, 6),
    (4, 10),
    (5, 6),
    (5, 10),
    (5, 16),
    (6, 16),
    (8, 30),
    (8, 32),
    (8, 33),
    (9, 33),
    (13, 33),
    (14, 32),
    (14, 33),
    (15, 32),
    (15, 33),
    (18, 32),
    (18, 33),
    (19, 33),
    (20, 32),
    (20, 33),
    (22, 32),
    (22, 33),
    (23, 25),
    (23, 27),
    (23, 29),
    (23, 32),
    (23, 33),
    (24, 25),
    (24, 27),
    (24, 31),
    (25, 31),
    (26, 29),
    (26, 33),
    (27, 33),
    (28, 31),
    (28, 33),
    (29, 32),
    (29, 33),
    (30, 32),
    (30, 33),
    (31, 32),
    (31, 33),
    (32, 33),
];

/// splitmix64.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[allow(dead_code)]
fn unit(state: &mut u64) -> f64 {
    (next(state) >> 11) as f64 / (1u64 << 53) as f64
}

/// An edge list over `n` nodes.
struct Edges {
    n: usize,
    src: Vec<u32>,
    dst: Vec<u32>,
}

impl Edges {
    fn topology(&self) -> Topology {
        Topology::build(self.n, self.src.clone(), self.dst.clone())
    }

    /// Modularity (γ = 1) of `comm` over the symmetrised edges.
    fn modularity(&self, w: Option<&[f64]>, comm: &[u32]) -> f64 {
        let k = comm.iter().max().map_or(0, |&c| c as usize + 1);
        let (mut tot, mut intra, mut m2) = (vec![0.0f64; k], 0.0f64, 0.0f64);
        for (i, (&s, &d)) in self.src.iter().zip(&self.dst).enumerate() {
            let wt = w.map_or(1.0, |w| w[i]);
            let (cs, cd) = (comm[s as usize] as usize, comm[d as usize] as usize);
            tot[cs] += wt;
            tot[cd] += wt;
            m2 += 2.0 * wt;
            if cs == cd {
                intra += 2.0 * wt;
            }
        }
        intra / m2 - tot.iter().map(|t| (t / m2) * (t / m2)).sum::<f64>()
    }
}

fn karate() -> Edges {
    let (src, dst) = KARATE.iter().copied().unzip();
    Edges { n: 34, src, dst }
}

/// A planted-partition graph: `n` nodes in consecutive communities of 16 to
/// 79 (the planted labels: each community's first node), and `m` edges, each
/// from a uniformly drawn source to a destination drawn from the whole graph
/// for `percent`% of the edges and from the source's community otherwise.
fn planted(n: usize, m: usize, percent: u64, seed: u64) -> (Edges, Vec<u32>) {
    let mut st = seed;
    let (mut start, mut size) = (vec![0u32; n], vec![0u32; n]);
    let mut lo = 0usize;
    while lo < n {
        let len = (16 + (next(&mut st) % 64) as usize).min(n - lo);
        for u in lo..lo + len {
            start[u] = lo as u32;
            size[u] = len as u32;
        }
        lo += len;
    }
    let (src, dst) = (0..m)
        .map(|_| {
            let s = (next(&mut st) % n as u64) as usize;
            let d = if next(&mut st) % 100 < percent {
                (next(&mut st) % n as u64) as u32
            } else {
                start[s] + (next(&mut st) % u64::from(size[s])) as u32
            };
            (s as u32, d)
        })
        .unzip();
    (Edges { n, src, dst }, start)
}

/// `m` R-MAT edges (Graph500 quadrants) over `2^scale` nodes, some of them
/// isolated.
fn rmat(scale: u32, m: usize, seed: u64) -> Edges {
    let mut st = seed;
    let (a, ab, abc) = (0.57, 0.76, 0.95);
    let (src, dst) = (0..m)
        .map(|_| {
            let (mut s, mut d) = (0u32, 0u32);
            for _ in 0..scale {
                let r = unit(&mut st);
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

/// FNV-1a over the labels.
fn hash(labels: &[u32]) -> u64 {
    labels.iter().fold(0xcbf2_9ce4_8422_2325, |h, &c| {
        (h ^ u64::from(c)).wrapping_mul(0x100_0000_01b3)
    })
}

/// The fixtures, by name.
fn fixtures() -> Vec<(&'static str, Edges)> {
    vec![
        ("karate", karate()),
        ("planted-20", planted(20_000, 320_000, 20, 1).0),
        ("planted-50", planted(20_000, 320_000, 50, 2).0),
        ("rmat", rmat(14, 200_000, 3)),
        ("rmat-sparse", rmat(15, 60_000, 4)),
    ]
}

/// The masked form of `g`: every third edge dropped.
fn masked(g: &Edges) -> (EdgeMask, Edges) {
    let keep: Vec<bool> = (0..g.src.len()).map(|e| e % 3 != 1).collect();
    let kept = |xs: &[u32]| -> Vec<u32> {
        xs.iter()
            .zip(&keep)
            .filter(|p| *p.1)
            .map(|p| *p.0)
            .collect()
    };
    let edges = Edges {
        n: g.n,
        src: kept(&g.src),
        dst: kept(&g.dst),
    };
    (EdgeMask::from_bools(&keep), edges)
}

/// The number of distinct labels.
fn count(labels: &[u32]) -> usize {
    let mut l = labels.to_vec();
    l.sort_unstable();
    l.dedup();
    l.len()
}

/// Labels renumbered `0..k` in order of first appearance (the modularity
/// wants dense ids).
fn relabel(labels: &[u32]) -> Vec<u32> {
    let mut map = std::collections::HashMap::new();
    labels
        .iter()
        .map(|&l| {
            let k = map.len() as u32;
            *map.entry(l).or_insert(k)
        })
        .collect()
}

/// (graph, masked, seed, communities, labels hash) for each fixture, masked
/// (every third edge dropped) and not, over two seeds.
fn fingerprints(parallel: bool) -> Vec<(&'static str, bool, u64, usize, u64)> {
    let mut got = Vec::new();
    for (name, g) in fixtures() {
        let t = g.topology();
        let (mask, _) = masked(&g);
        for m in [false, true] {
            for seed in [1u64, 9] {
                let l = label_propagation(&t, m.then_some(&mask), 20, Some(seed), parallel);
                got.push((name, m, seed, count(&l), hash(&l)));
            }
        }
    }
    got
}

/// Golden results of the parallel run. Recorded when the parallel sweeps were
/// written (#453): a change to what they compute must update these on purpose.
#[test]
fn parallel_results_are_pinned() {
    // (graph, masked, seed, communities, labels hash)
    let want: &[(&str, bool, u64, usize, u64)] = &[
        ("karate", false, 1, 2, 1069913290551278413),
        ("karate", false, 9, 2, 16715955134096545543),
        ("karate", true, 1, 2, 14432428937527601869),
        ("karate", true, 9, 1, 13055558309906261613),
        ("planted-20", false, 1, 424, 7689058777160285714),
        ("planted-20", false, 9, 422, 17236411537054247244),
        ("planted-20", true, 1, 423, 13372451433932517704),
        ("planted-20", true, 9, 425, 4577522715992516605),
        ("planted-50", false, 1, 428, 2108003267928182555),
        ("planted-50", false, 9, 429, 4608074012775489905),
        ("planted-50", true, 1, 403, 16868152339392386454),
        ("planted-50", true, 9, 415, 8863370580763100739),
        ("rmat", false, 1, 4383, 2152593210752904684),
        ("rmat", false, 9, 4383, 239648304363423808),
        ("rmat", true, 1, 5369, 16797221299090695663),
        ("rmat", true, 9, 5369, 3120062473193924229),
        ("rmat-sparse", false, 1, 19199, 5027610512249620792),
        ("rmat-sparse", false, 9, 19197, 17944691955450659524),
        ("rmat-sparse", true, 1, 21178, 3009914891124616042),
        ("rmat-sparse", true, 9, 21180, 15727620303182265496),
    ];
    assert_eq!(fingerprints(true), want);
}

/// Golden results of the serial (default) run, recorded before #453's serial
/// changes, which keep its labels.
#[test]
fn serial_results_are_pinned() {
    // (graph, masked, seed, communities, labels hash)
    let want: &[(&str, bool, u64, usize, u64)] = &[
        ("karate", false, 1, 2, 1069913290551278413),
        ("karate", false, 9, 2, 16715955134096545543),
        ("karate", true, 1, 2, 14432428937527601869),
        ("karate", true, 9, 1, 13055558309906261613),
        ("planted-20", false, 1, 423, 2313167652501196012),
        ("planted-20", false, 9, 421, 14970827380572124828),
        ("planted-20", true, 1, 429, 6583801001451854440),
        ("planted-20", true, 9, 427, 2414154427010168692),
        ("planted-50", false, 1, 424, 7519445942503956305),
        ("planted-50", false, 9, 425, 16489211729825715394),
        ("planted-50", true, 1, 386, 15100382504789444517),
        ("planted-50", true, 9, 399, 13070978185400345936),
        ("rmat", false, 1, 4383, 2152593210752904684),
        ("rmat", false, 9, 4383, 239648304363423808),
        ("rmat", true, 1, 5369, 16797221299090695663),
        ("rmat", true, 9, 5369, 3120062473193924229),
        ("rmat-sparse", false, 1, 19199, 5027610512249620792),
        ("rmat-sparse", false, 9, 19197, 17944691955450659524),
        ("rmat-sparse", true, 1, 21179, 1929039923938828638),
        ("rmat-sparse", true, 9, 21180, 2422842425255484294),
    ];
    assert_eq!(fingerprints(false), want);
}

/// On planted-partition graphs the parallel run finds the planted
/// communities about as well as the serial one: on average over eight seeds,
/// a modularity no more than 1% below the serial run's and a community count
/// within 2% of it, unmasked and masked. Both are close to the planted
/// partition's own.
#[test]
fn parallel_finds_the_planted_communities() {
    for (percent, seed) in [(20u64, 1u64), (50, 2)] {
        let (g, truth) = planted(20_000, 320_000, percent, seed);
        let t = g.topology();
        let (mask, me) = masked(&g);
        let planted_q = g.modularity(None, &relabel(&truth));
        for m in [false, true] {
            let (edges, mask) = if m { (&me, Some(&mask)) } else { (&g, None) };
            let mean = |parallel| {
                let (mut q, mut k) = (0.0f64, 0.0f64);
                for seed in 1..=8u64 {
                    let l = label_propagation(&t, mask, 20, Some(seed), parallel);
                    q += edges.modularity(None, &relabel(&l));
                    k += count(&l) as f64;
                }
                (q / 8.0, k / 8.0)
            };
            let ((sq, sk), (pq, pk)) = (mean(false), mean(true));
            println!(
                "planted {percent}%, masked {m}: planted {} communities, modularity \
                 {planted_q:.5}; serial {sk} / {sq:.5}; parallel {pk} / {pq:.5}",
                count(&truth)
            );
            assert!(
                pq >= sq * 0.99,
                "planted {percent}%, masked {m}: parallel modularity {pq} is more than 1% \
                 below serial {sq}"
            );
            assert!(
                (pk - sk).abs() <= sk * 0.02,
                "planted {percent}%, masked {m}: parallel finds {pk} communities, serial {sk}"
            );
        }
    }
}

/// Neighbours of one run that would trade labels for ever settle instead
/// (the guard): on disjoint pairs and 4-cycles, many of them in one run, every
/// piece ends with one label, and more sweeps change nothing.
#[test]
fn bipartite_pieces_settle() {
    // 6,000 pairs and 3,000 4-cycles: 24,000 nodes, runs of 375, so about one
    // piece in 64 has its nodes in one run.
    let (mut src, mut dst) = (Vec::new(), Vec::new());
    for p in 0..6_000u32 {
        src.push(2 * p);
        dst.push(2 * p + 1);
    }
    for c in 0..3_000u32 {
        let b = 12_000 + 4 * c;
        for i in 0..4 {
            src.push(b + i);
            dst.push(b + (i + 1) % 4);
        }
    }
    let t = Topology::build(24_000, src, dst);
    for seed in [1u64, 2, 3] {
        let l = label_propagation(&t, None, 20, Some(seed), true);
        for p in 0..6_000usize {
            assert_eq!(l[2 * p], l[2 * p + 1], "pair {p}, seed {seed}");
        }
        for c in 0..3_000usize {
            let b = 12_000 + 4 * c;
            assert!(
                l[b..b + 4].iter().all(|&x| x == l[b]),
                "4-cycle {c}, seed {seed}: {:?}",
                &l[b..b + 4]
            );
        }
        assert_eq!(l, label_propagation(&t, None, 50, Some(seed), true));
    }
}

/// On a graph of at most 64 nodes every run holds one node, so the parallel
/// run is the serial one.
#[test]
fn small_graphs_are_the_serial_run() {
    let t = karate().topology();
    for seed in 1..=8u64 {
        assert_eq!(
            label_propagation(&t, None, 20, Some(seed), true),
            label_propagation(&t, None, 20, Some(seed), false)
        );
    }
}

/// On a larger graph the parallel run is not the serial one.
#[test]
fn parallel_differs_from_serial() {
    let t = planted(20_000, 320_000, 20, 1).0.topology();
    assert_ne!(
        label_propagation(&t, None, 20, Some(1), true),
        label_propagation(&t, None, 20, Some(1), false)
    );
}
