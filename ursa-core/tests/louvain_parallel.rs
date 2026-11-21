//! Parallel Louvain (`parallel = true`, #387), pinned.
//!
//! The parallel local moving finds different communities from the serial
//! default, and NetworkX has no equivalent to check it against, so it is
//! pinned three ways instead: the same labels on any thread count
//! (`determinism_threads.rs`); here, golden hashes of its labels on fixture
//! graphs, so any change to what it computes shows; and its modularity no
//! more than 2% below the serial run's on karate, planted-partition graphs and
//! R-MAT, so a change that keeps it deterministic but makes it worse shows too.

use ursa_core::algo::{louvain, louvain_weighted};
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

/// A planted-partition graph, as `examples/louvain_memory.rs` makes it: `n`
/// nodes in consecutive communities of 8 to 519, `m` edges with a source
/// skewed toward low ids, and `percent`% of destinations drawn from the whole
/// graph (the rest from the source's community).
fn planted(n: usize, m: usize, percent: u64, seed: u64) -> Edges {
    let mut st = seed;
    let (mut start, mut size) = (vec![0u32; n], vec![0u32; n]);
    let mut lo = 0usize;
    while lo < n {
        let len = (8 + (next(&mut st) % 512) as usize).min(n - lo);
        for u in lo..lo + len {
            start[u] = lo as u32;
            size[u] = len as u32;
        }
        lo += len;
    }
    let (src, dst) = (0..m)
        .map(|_| {
            let r = unit(&mut st);
            let s = ((n as f64 * r * r) as usize).min(n - 1);
            let d = if next(&mut st) % 100 < percent {
                (next(&mut st) % n as u64) as u32
            } else {
                start[s] + (next(&mut st) % u64::from(size[s])) as u32
            };
            (s as u32, d)
        })
        .unzip();
    Edges { n, src, dst }
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

/// Inexact per-edge weights, so every sum is order-sensitive.
fn weights(m: usize) -> Vec<f64> {
    (0..m)
        .map(|i| ((i % 89) as f64 + 1.0).sqrt() / 3.0)
        .collect()
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
        ("planted-30", planted(20_000, 160_000, 30, 1)),
        ("planted-60", planted(20_000, 160_000, 60, 2)),
        ("rmat", rmat(14, 200_000, 3)),
    ]
}

/// Golden results of the parallel run: the community count, the modularity
/// bits and a hash of the labels, for each fixture, unweighted and weighted
/// (inexact weights), masked (every third edge dropped) and not, over two
/// seeds. Recorded when the parallel local moving was written (#387): a
/// change to what it computes must update these on purpose.
#[test]
fn parallel_results_are_pinned() {
    let mut got = Vec::new();
    for (name, g) in fixtures() {
        let t = g.topology();
        let w = weights(g.src.len());
        let keep: Vec<bool> = (0..g.src.len()).map(|e| e % 3 != 1).collect();
        let mask = EdgeMask::from_bools(&keep);
        let kept = |xs: &[u32]| -> Vec<u32> {
            xs.iter()
                .zip(&keep)
                .filter(|p| *p.1)
                .map(|p| *p.0)
                .collect()
        };
        let masked_edges = Edges {
            n: g.n,
            src: kept(&g.src),
            dst: kept(&g.dst),
        };
        let masked_w: Vec<f64> = w.iter().zip(&keep).filter(|p| *p.1).map(|p| *p.0).collect();
        for masked in [false, true] {
            let (edges, ww) = if masked {
                (&masked_edges, &masked_w)
            } else {
                (&g, &w)
            };
            let mask = masked.then_some(&mask);
            for weighted in [false, true] {
                for seed in [1u64, 9] {
                    let comm = if weighted {
                        louvain_weighted(&t, &w, mask, 1.0, Some(seed), true)
                    } else {
                        louvain(&t, mask, 1.0, Some(seed), true)
                    };
                    let q = edges.modularity(weighted.then_some(ww.as_slice()), &comm);
                    let k = comm.iter().max().unwrap() + 1;
                    got.push((name, masked, weighted, seed, k, q.to_bits(), hash(&comm)));
                }
            }
        }
    }
    // (graph, masked, weighted, seed, communities, modularity bits, labels hash)
    let want: &[(&str, bool, bool, u64, u32, u64, u64)] = &[
        (
            "karate",
            false,
            false,
            1,
            4,
            4601233877280367620,
            15833618299619503390,
        ),
        (
            "karate",
            false,
            false,
            9,
            4,
            4601158373144603026,
            9259789879169192460,
        ),
        (
            "karate",
            false,
            true,
            1,
            4,
            4601319371234413124,
            15833618299619503390,
        ),
        (
            "karate",
            false,
            true,
            9,
            4,
            4601101317956232647,
            10673980715093358010,
        ),
        (
            "karate",
            true,
            false,
            1,
            5,
            4602155841996983326,
            15712114714833906453,
        ),
        (
            "karate",
            true,
            false,
            9,
            4,
            4602015937274831284,
            3328103253203055033,
        ),
        (
            "karate",
            true,
            true,
            1,
            5,
            4602112388262714122,
            9661242175239975558,
        ),
        (
            "karate",
            true,
            true,
            9,
            4,
            4601880581271528865,
            3328103253203055033,
        ),
        (
            "planted-30",
            false,
            false,
            1,
            30,
            4603979118361740221,
            11415310627754275080,
        ),
        (
            "planted-30",
            false,
            false,
            9,
            35,
            4603925324531930519,
            419595541786071181,
        ),
        (
            "planted-30",
            false,
            true,
            1,
            54,
            4604119281616824282,
            8432813196548986901,
        ),
        (
            "planted-30",
            false,
            true,
            9,
            55,
            4604118543190766510,
            5266969884641761054,
        ),
        (
            "planted-30",
            true,
            false,
            1,
            38,
            4603569485549703104,
            11598445971526107967,
        ),
        (
            "planted-30",
            true,
            false,
            9,
            36,
            4603530899097299743,
            2216580593566412623,
        ),
        (
            "planted-30",
            true,
            true,
            1,
            51,
            4603838007379682329,
            7374230995834396785,
        ),
        (
            "planted-30",
            true,
            true,
            9,
            53,
            4603850528956644748,
            7317972594396966570,
        ),
        (
            "planted-60",
            false,
            false,
            1,
            12,
            4599765039114385905,
            11925694537662240254,
        ),
        (
            "planted-60",
            false,
            false,
            9,
            12,
            4599702214465348790,
            2207004009669584029,
        ),
        (
            "planted-60",
            false,
            true,
            1,
            20,
            4599749277008880608,
            219054619498606171,
        ),
        (
            "planted-60",
            false,
            true,
            9,
            20,
            4599836905110190256,
            12344450470954781634,
        ),
        (
            "planted-60",
            true,
            false,
            1,
            19,
            4600080304437328730,
            14313016845038190696,
        ),
        (
            "planted-60",
            true,
            false,
            9,
            23,
            4600104383512581616,
            9089779634255409970,
        ),
        (
            "planted-60",
            true,
            true,
            1,
            28,
            4600503306701307854,
            14464632321550900787,
        ),
        (
            "planted-60",
            true,
            true,
            9,
            30,
            4600518818890042826,
            5154126177691183353,
        ),
        (
            "rmat",
            false,
            false,
            1,
            4402,
            4591905631298041034,
            9829802860531633074,
        ),
        (
            "rmat",
            false,
            false,
            9,
            4399,
            4592089876982433196,
            5329254112810266108,
        ),
        (
            "rmat",
            false,
            true,
            1,
            4402,
            4592474068964011756,
            3741389034872217055,
        ),
        (
            "rmat",
            false,
            true,
            9,
            4401,
            4592566665487697755,
            15272640205107020764,
        ),
        (
            "rmat",
            true,
            false,
            1,
            5390,
            4593710823536122152,
            352037768419806036,
        ),
        (
            "rmat",
            true,
            false,
            9,
            5387,
            4593800162169872708,
            9633146114730832530,
        ),
        (
            "rmat",
            true,
            true,
            1,
            5392,
            4594077903854617282,
            1131135738897446032,
        ),
        (
            "rmat",
            true,
            true,
            9,
            5389,
            4594152198452949566,
            15606156901948169016,
        ),
    ];
    assert_eq!(got, want);
}

/// The parallel run's modularity is within 2% of the serial run's (or above
/// it), unweighted and weighted, on average over eight seeds: on a graph as
/// small as karate, one seed's result varies by more than that in either
/// mode.
#[test]
fn parallel_modularity_is_close_to_serial() {
    for (name, g) in fixtures() {
        let t = g.topology();
        let w = weights(g.src.len());
        for weighted in [false, true] {
            let ws = weighted.then_some(w.as_slice());
            let mean = |parallel| {
                (1..=8u64)
                    .map(|seed| {
                        let comm = match ws {
                            Some(w) => louvain_weighted(&t, w, None, 1.0, Some(seed), parallel),
                            None => louvain(&t, None, 1.0, Some(seed), parallel),
                        };
                        g.modularity(ws, &comm)
                    })
                    .sum::<f64>()
                    / 8.0
            };
            let (serial, parallel) = (mean(false), mean(true));
            println!("{name}, weighted {weighted}: serial {serial:.5}, parallel {parallel:.5}");
            assert!(
                parallel >= serial * 0.98,
                "{name}, weighted {weighted}: parallel modularity {parallel} is more than 2% \
                 below serial {serial}"
            );
        }
    }
}

/// The parallel run is not the serial one, and the flag is honoured through
/// both entry points.
#[test]
fn parallel_differs_from_serial() {
    let g = planted(20_000, 160_000, 30, 1);
    let t = g.topology();
    assert_ne!(
        louvain(&t, None, 1.0, Some(1), true),
        louvain(&t, None, 1.0, Some(1), false)
    );
    let w = weights(g.src.len());
    assert_ne!(
        louvain_weighted(&t, &w, None, 1.0, Some(1), true),
        louvain_weighted(&t, &w, None, 1.0, Some(1), false)
    );
}
