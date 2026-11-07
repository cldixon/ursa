//! Measure how the layout kernels actually scale, so the complexity claim in
//! `algo/layout.rs` is checked against a clock rather than against an intention.
//!
//! This example exists because the claim was wrong once. The original uniform-grid
//! repulsion was documented as O(n) and measured at ~3.8× per doubling, which is
//! quadratic wearing a disguise (#142). The number to watch is the rightmost
//! column: **~2 is linear, ~2.2 is n log n, ~4 is quadratic.**
//!
//! Run with: cargo run --release -p ursa-core --example layout_scaling

use std::time::Instant;
use ursa_core::algo::{layout_fa2, layout_fr, Fa2Params, LayoutParams};
use ursa_core::topology::Topology;

/// A ring with random chords: connected, cheap to build, and roughly even in
/// density, which is the case the uniform grid is supposed to be good at.
fn ring_with_chords(n: usize, chords: usize) -> Topology {
    let mut src = Vec::with_capacity(n + chords);
    let mut dst = Vec::with_capacity(n + chords);
    for i in 0..n {
        src.push(i as u32);
        dst.push(((i + 1) % n) as u32);
    }
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..chords {
        src.push((next() % n as u64) as u32);
        dst.push((next() % n as u64) as u32);
    }
    Topology::build(n, src, dst)
}

/// One kernel over a doubling ladder, reporting the growth factor per doubling.
fn ladder(name: &str, sizes: &[usize], iterations: u32, run: impl Fn(&Topology, u32)) {
    println!("\n{name}  ({iterations} iterations)");
    println!(
        "{:>9}  {:>10}  {:>10}  {:>9}",
        "nodes", "ms", "ms/iter", "growth"
    );
    let mut prev: Option<f64> = None;
    for &n in sizes {
        let topo = ring_with_chords(n, n / 2);
        let t = Instant::now();
        run(&topo, iterations);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let factor = prev
            .map(|pms| format!("{:.2}x", ms / pms))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{n:>9}  {ms:>10.1}  {:>10.2}  {factor:>9}",
            ms / iterations as f64
        );
        prev = Some(ms);
    }
}

fn main() {
    let sizes = [
        1_000usize, 2_000, 4_000, 8_000, 16_000, 32_000, 64_000, 128_000,
    ];
    let iterations = 20;

    ladder("layout_fr (Barnes-Hut)", &sizes, iterations, |topo, it| {
        let p = layout_fr(
            topo,
            None,
            LayoutParams {
                iterations: it,
                ..LayoutParams::default()
            },
        );
        std::hint::black_box(&p);
    });

    ladder("layout_fa2", &sizes, iterations, |topo, it| {
        let p = layout_fa2(
            topo,
            None,
            Fa2Params {
                iterations: it,
                ..Fa2Params::default()
            },
        );
        std::hint::black_box(&p);
    });

    // Where the time actually goes, both halves measured serially so the ratio is
    // apples to apples. The build is *inherently* serial today — a radix sort, a
    // top-down subdivision, a centre-of-mass pass — while the traversal is what
    // the thread pool scales. If the build were the larger half, adding cores
    // would buy almost nothing and it would be the thing to parallelize next.
    {
        use ursa_core::algo::quadtree::{traversal_stack, unit_weights, QuadTree};
        let n = 1_000_000usize;
        // Positions on a spiral, the same shape the layout seeds from.
        let (mut px, mut py) = (vec![0.0f32; n], vec![0.0f32; n]);
        let golden = std::f32::consts::PI * (3.0 - 5.0f32.sqrt());
        for i in 0..n {
            let r = (n as f32).sqrt() * ((i as f32 + 0.5) / n as f32).sqrt();
            px[i] = r * (golden * i as f32).cos();
            py[i] = r * (golden * i as f32).sin();
        }
        let w = unit_weights(n);
        let t = Instant::now();
        let tree = QuadTree::build(&px, &py, &w);
        let build_ms = t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        let mut stack = traversal_stack();
        let mut acc = (0.0f32, 0.0f32);
        for i in 0..n {
            let f = tree.accumulate(&px, &py, &w, i, 0.5, &mut stack, |dx, dy, m| {
                let d2 = (dx * dx + dy * dy).max(1e-4);
                let f = m / d2;
                (dx * f, dy * f)
            });
            acc.0 += f.0;
            acc.1 += f.1;
        }
        let traverse_ms = t.elapsed().as_secs_f64() * 1000.0;
        std::hint::black_box(acc);
        println!(
            "\none iteration at {n} nodes, single-threaded: build {build_ms:.0} ms (serial, \
             does not scale with cores) + traverse {traverse_ms:.0} ms (parallelizable), \
             {} cells",
            tree.len()
        );
    }

    // The headline claim from docs/VIZ_VISION.md, timed rather than asserted.
    let n = 1_000_000;
    let iterations = 300;
    println!("\nlayout_fa2, {n} nodes, {iterations} iterations (the VIZ_VISION claim)");
    let topo = ring_with_chords(n, n / 2);
    let t = Instant::now();
    let p = layout_fa2(
        &topo,
        None,
        Fa2Params {
            iterations,
            ..Fa2Params::default()
        },
    );
    std::hint::black_box(&p);
    println!("  {:.1}s", t.elapsed().as_secs_f64());
}
