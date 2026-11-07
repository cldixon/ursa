//! Per-iteration cost of layout_fa2 at the browser view's target size: an
//! R-MAT power-law graph (Graph500 parameters) of 2^17 nodes and 1M edges.
//! Build serial to match the browser: --no-default-features.
use std::time::Instant;
use ursa_core::algo::{layout_fa2, Fa2Params};
use ursa_core::topology::Topology;

fn rmat(scale: u32, edges: usize, seed: u64) -> Topology {
    let n = 1usize << scale;
    let mut state = seed;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let (a, b, c) = (0.57, 0.19, 0.19);
    let (mut src, mut dst) = (Vec::with_capacity(edges), Vec::with_capacity(edges));
    for _ in 0..edges {
        let (mut u, mut v) = (0usize, 0usize);
        for bit in (0..scale).rev() {
            let r = next();
            let (du, dv) = if r < a {
                (0, 0)
            } else if r < a + b {
                (0, 1)
            } else if r < a + b + c {
                (1, 0)
            } else {
                (1, 1)
            };
            u |= du << bit;
            v |= dv << bit;
        }
        src.push(u as u32);
        dst.push(v as u32);
    }
    Topology::build(n, src, dst)
}

fn main() {
    let scale: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(17);
    let edges: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);
    let t = rmat(scale, edges, 42);
    println!("nodes {} edges {}", t.n_nodes(), t.n_edges());
    for iters in [1u32, 10] {
        let s = Instant::now();
        let _ = layout_fa2(
            &t,
            None,
            Fa2Params {
                iterations: iters,
                ..Fa2Params::default()
            },
        );
        let ms = s.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{iters:>3} iterations: {ms:>8.1} ms  ({:.1} ms/iteration)",
            ms / iters as f64
        );
    }
}
