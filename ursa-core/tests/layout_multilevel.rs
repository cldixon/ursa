//! Coarse-to-fine ForceAtlas2 against flat, on the graph view's test graphs
//! (`ursa-viz/demo/graphs.js` makes the same ones): it must not separate
//! clusters worse than flat does, and must unfold a grid that flat leaves
//! folded. `examples/fa2_multilevel.rs` measures the same on the full-size
//! graphs, with timings.
use ursa_core::algo::{layout_fa2, layout_fa2_multilevel, Fa2Params, MultilevelParams, Positions};
use ursa_core::Topology;

/// xorshift64 → [0, 1), as `graphs.js` has it.
fn rng(seed: u64) -> impl FnMut() -> f64 {
    let mut s = seed;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// `k` clusters of `size` nodes, six edges out of each node inside its
/// cluster, and one in ten nodes with an edge anywhere.
fn clusters(k: u32, size: u32) -> (Topology, Vec<u32>) {
    let mut next = rng(7);
    let (mut src, mut dst) = (Vec::new(), Vec::new());
    let n = k * size;
    for c in 0..k {
        for i in 0..size {
            let a = c * size + i;
            for _ in 0..6 {
                src.push(a);
                dst.push(c * size + (next() * size as f64) as u32);
            }
            if next() < 0.1 {
                src.push(a);
                dst.push((next() * n as f64) as u32);
            }
        }
    }
    (
        Topology::build(n as usize, src, dst),
        (0..n).map(|i| i / size).collect(),
    )
}

fn grid(w: u32, h: u32) -> Topology {
    let (mut src, mut dst) = (Vec::new(), Vec::new());
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if x + 1 < w {
                src.push(i);
                dst.push(i + 1);
            }
            if y + 1 < h {
                src.push(i);
                dst.push(i + w);
            }
        }
    }
    Topology::build((w * h) as usize, src, dst)
}

fn dist(p: &Positions, i: usize, j: usize) -> f64 {
    f64::from(((p.0[i] - p.0[j]).powi(2) + (p.1[i] - p.1[j]).powi(2)).sqrt())
}

/// Mean distance within a cluster over mean distance between clusters, on
/// the pairs the browser harness samples. Lower is better.
fn cluster_ratio(p: &Positions, community: &[u32]) -> f64 {
    let n = community.len();
    let (mut is, mut inn, mut os, mut on) = (0.0, 0u64, 0.0, 0u64);
    for i in (0..n).step_by(7) {
        for j in (i + 3..n).step_by(11) {
            if community[i] == community[j] {
                is += dist(p, i, j);
                inn += 1;
            } else {
                os += dist(p, i, j);
                on += 1;
            }
        }
    }
    (is / inn as f64) / (os / on as f64)
}

/// The grid's fold measure: Pearson correlation of layout distance and grid
/// distance over sampled pairs. A sheet laid out flat is near 1, whatever
/// its rotation or scale; a folded one brings far-apart nodes together.
fn grid_r(p: &Positions, w: usize) -> f64 {
    let n = p.0.len();
    let mut next = rng(5);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for _ in 0..20_000 {
        let (i, j) = ((next() * n as f64) as usize, (next() * n as f64) as usize);
        let (gx, gy) = (
            (i % w) as f64 - (j % w) as f64,
            (i / w) as f64 - (j / w) as f64,
        );
        a.push((gx * gx + gy * gy).sqrt());
        b.push(dist(p, i, j));
    }
    let m = a.len() as f64;
    let (ma, mb) = (a.iter().sum::<f64>() / m, b.iter().sum::<f64>() / m);
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for k in 0..a.len() {
        sab += (a[k] - ma) * (b[k] - mb);
        saa += (a[k] - ma).powi(2);
        sbb += (b[k] - mb).powi(2);
    }
    sab / (saa * sbb).sqrt()
}

#[test]
fn clusters_separate_as_well_as_flat() {
    // The demo's clustered graph: 8 clusters of 500.
    let (t, community) = clusters(8, 500);
    let flat = layout_fa2(&t, None, Fa2Params::default());
    let ml = layout_fa2_multilevel(&t, None, MultilevelParams::default());
    let (rf, rm) = (
        cluster_ratio(&flat, &community),
        cluster_ratio(&ml, &community),
    );
    eprintln!("cluster ratio: flat (300 iterations) {rf:.4}, multilevel {rm:.4}");
    // Both measure 0.0725 (flat 0.07253, multilevel 0.07262): the same
    // separation, from two different local minima.
    assert!(rm < 0.08, "multilevel cluster ratio {rm}");
    assert!(rm < rf * 1.05, "multilevel {rm} against flat {rf}");
}

#[test]
fn a_grid_unfolds_where_flat_leaves_it_folded() {
    let (w, h) = (60, 60);
    let t = grid(w, h);
    let flat = layout_fa2(&t, None, Fa2Params::default());
    let ml = layout_fa2_multilevel(&t, None, MultilevelParams::default());
    let (rf, rm) = (grid_r(&flat, w as usize), grid_r(&ml, w as usize));
    eprintln!("grid r: flat (300 iterations) {rf:.3}, multilevel {rm:.3}");
    assert!(rm > 0.9, "multilevel leaves the grid folded: r {rm}");
    assert!(
        rf < 0.8,
        "flat unfolded the grid (r {rf}); the test no longer shows a difference"
    );
}
