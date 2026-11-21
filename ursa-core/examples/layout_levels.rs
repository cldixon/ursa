//! How long rings and layers take (`layout_rings`, `layout_layers`), on
//! R-MAT graphs of 1K to 1M edges with every node levelled by its distance
//! from node 0: the largest neighbourhood each graph has.
//!
//! ```sh
//! cargo run -p ursa-core --release --example layout_levels [1k,10k,100k,1m]
//! ```
//!
//! Build with `--no-default-features` for the serial crate the browser
//! runs; the layouts are serial either way. For each graph it prints the
//! nodes on each level, the time of each layout (the median of five runs,
//! the adjacency already built), and the mean edge length before and after
//! the barycentre sweeps, in units of the ring or layer spacing.
use std::time::Instant;

use ursa_core::algo::{layout_layers, layout_rings, LayersParams, RingsParams, NO_LEVEL};
use ursa_core::topology::{Topology, UndirectedCsr};

/// xorshift64 → [0, 1), as `ursa-viz/demo/graphs.js` has it.
fn rng(seed: u64) -> impl FnMut() -> f64 {
    let mut s = seed;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn rmat(scale: u32, edges: usize) -> Topology {
    let mut next = rng(42);
    let (mut src, mut dst) = (Vec::with_capacity(edges), Vec::with_capacity(edges));
    for _ in 0..edges {
        let (mut u, mut v) = (0u32, 0u32);
        for bit in (0..scale).rev() {
            let r = next();
            if r < 0.57 {
            } else if r < 0.76 {
                v |= 1 << bit;
            } else if r < 0.95 {
                u |= 1 << bit;
            } else {
                u |= 1 << bit;
                v |= 1 << bit;
            }
        }
        src.push(u);
        dst.push(v);
    }
    Topology::build(1 << scale, src, dst)
}

fn distances(adj: &UndirectedCsr, seed: u32) -> Vec<u32> {
    let mut d = vec![NO_LEVEL; adj.offsets.len() - 1];
    d[seed as usize] = 0;
    let mut frontier = vec![seed];
    let mut step = 0;
    while !frontier.is_empty() {
        step += 1;
        let mut next = Vec::new();
        for &v in &frontier {
            for &u in adj.neighbors(v) {
                if d[u as usize] == NO_LEVEL {
                    d[u as usize] = step;
                    next.push(u);
                }
            }
        }
        frontier = next;
    }
    d
}

fn mean_edge(adj: &UndirectedCsr, (x, y): &(Vec<f32>, Vec<f32>)) -> f64 {
    let (mut sum, mut count) = (0.0f64, 0u64);
    for v in 0..x.len() {
        for &u in adj.neighbors(v as u32) {
            sum += f64::from((x[v] - x[u as usize]).hypot(y[v] - y[u as usize]));
            count += 1;
        }
    }
    sum / count.max(1) as f64
}

fn median_ms(mut run: impl FnMut()) -> f64 {
    let mut ms: Vec<f64> = (0..5)
        .map(|_| {
            let t = Instant::now();
            run();
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    ms[2]
}

fn main() {
    let sizes = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1k,10k,100k,1m".into());
    for size in sizes.split(',') {
        let (scale, edges) = match size {
            "1k" => (8, 1_000),
            "10k" => (11, 10_000),
            "100k" => (14, 100_000),
            "1m" => (17, 1_000_000),
            other => panic!("no size called {other}: 1k, 10k, 100k or 1m"),
        };
        let t = rmat(scale, edges);
        let adj = t.undirected();
        let d = distances(adj, 0);
        let mut per_level: Vec<usize> = Vec::new();
        let mut unreached = 0usize;
        for &l in &d {
            if l == NO_LEVEL {
                unreached += 1;
            } else {
                if per_level.len() <= l as usize {
                    per_level.resize(l as usize + 1, 0);
                }
                per_level[l as usize] += 1;
            }
        }
        println!(
            "rmat {size}: {} nodes, {} undirected edges; per level {per_level:?}, {unreached} unreached",
            d.len(),
            adj.targets.len() / 2
        );
        let rings = RingsParams::default();
        let layers = LayersParams::default();
        let ms = median_ms(|| {
            std::hint::black_box(layout_rings(adj, &d, rings));
        });
        let unswept = mean_edge(
            adj,
            &layout_rings(adj, &d, RingsParams { sweeps: 0, ..rings }),
        );
        let swept = mean_edge(adj, &layout_rings(adj, &d, rings));
        println!("  rings   {ms:8.2} ms   mean edge {unswept:.2} -> {swept:.2}");
        let ms = median_ms(|| {
            std::hint::black_box(layout_layers(adj, &d, layers));
        });
        let unswept = mean_edge(
            adj,
            &layout_layers(
                adj,
                &d,
                LayersParams {
                    sweeps: 0,
                    ..layers
                },
            ),
        );
        let swept = mean_edge(adj, &layout_layers(adj, &d, layers));
        println!("  layers  {ms:8.2} ms   mean edge {unswept:.2} -> {swept:.2}");
    }
}
