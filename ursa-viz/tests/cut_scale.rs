//! The aggregated view at the design's reference size (#498): the 1M-edge
//! R-MAT graph (scale 17: 131,072 node slots, as `ursa-viz/demo/graphs.js`
//! and `ursa-core/examples/fa2_multilevel.rs` make it), laid out to the
//! end, then cut for a camera fitted to it and for cameras zoomed in.
//!
//! It asserts what #498 accepts: at the fitted zoom at most 20,000 discs
//! and 100,000 merged edges; the groups' members sum to the node count;
//! zooming into a region reaches the graph's own nodes; the same cut every
//! time. And it prints what a cut takes, which it asserts in release
//! builds only (30 ms; a debug build is ten times slower).
//!
//! The layout takes a few seconds in release and minutes in debug, so the
//! test is ignored by default:
//!
//! ```sh
//! cargo test -p ursa-viz --no-default-features --release --test cut_scale -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Instant;

use ursa_core::algo::MultilevelParams;
use ursa_core::topology::UndirectedCsr;
use ursa_viz::cut::{CutParams, MAX_EDGES, MAX_NODES};
use ursa_viz::scene::{bounds, Camera, LiveLayout};

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

fn rmat_edges(scale: u32, edges: usize) -> (Vec<u32>, Vec<u32>) {
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
    (src, dst)
}

fn median(mut ms: Vec<f64>) -> f64 {
    ms.sort_by(f64::total_cmp);
    ms[ms.len() / 2]
}

#[test]
#[ignore = "lays out a million edges: run in release"]
fn a_million_edges_are_cut_within_a_frame_or_two() {
    let n = 1usize << 17;
    let (src, dst) = rmat_edges(17, 1_000_000);
    let adjacency = Arc::new(UndirectedCsr::from_edges(n, &src, &dst));
    let mut layout = LiveLayout::multilevel_on(adjacency, MultilevelParams::default());
    assert!(!layout.has_cut(), "not before the layout reaches the graph");
    let t = Instant::now();
    while !layout.is_done() {
        layout.step_level(1000);
    }
    println!("laid out in {:.1} s", t.elapsed().as_secs_f64());
    assert!(layout.has_cut());

    // A 1280 x 800 canvas, fitted as the view fits it.
    let mut camera = Camera::new(1280.0, 800.0);
    let (x, y) = layout.positions();
    camera.fit(bounds(x, y).unwrap());
    let of = |c: &Camera| CutParams::of_camera(c.center, c.pixels_per_unit, c.viewport);

    let t = Instant::now();
    let fitted = layout.cut(&of(&camera)).unwrap();
    println!(
        "the first cut, with its forest ({} KB held): {:.1} ms",
        layout.cut_bytes() / 1024,
        t.elapsed().as_secs_f64() * 1e3
    );
    println!(
        "fitted: {} discs ({} in view), {} merged edges of {}",
        fitted.nodes.len(),
        fitted.in_view,
        fitted.src.len(),
        fitted.total_edges
    );
    assert!(!fitted.detail);
    assert!(fitted.nodes.len() <= MAX_NODES);
    assert!(fitted.src.len() <= MAX_EDGES);
    assert!(fitted.nodes.len() >= 100, "a readable picture, not a dot");
    assert_eq!(fitted.members.iter().map(|&m| m as usize).sum::<usize>(), n);
    assert_eq!(fitted.of.len(), n);
    assert_eq!(layout.cut(&of(&camera)).unwrap(), fitted, "deterministic");

    // Zooming in on the middle of the drawing, a step at a time: more
    // of what is in view is opened, until it is the graph's own nodes.
    let mut times = Vec::new();
    let mut reached = None;
    for step in 0..40 {
        let params = of(&camera);
        let t = Instant::now();
        let cut = layout.cut(&params).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1e3;
        times.push(ms);
        println!(
            "zoom x{:<8.1} {:>6} discs, {:>6} in view of {:>6} nodes, {:>6} edges of {:>6}{} {ms:.1} ms",
            1.5f64.powi(step),
            cut.nodes.len(),
            cut.in_view,
            cut.members_in_view,
            cut.src.len(),
            cut.total_edges,
            if cut.detail { " detail" } else { "" },
        );
        assert!(cut.nodes.len() <= MAX_NODES && cut.src.len() <= MAX_EDGES);
        assert_eq!(cut.members.iter().map(|&m| m as usize).sum::<usize>(), n);
        if cut.detail {
            reached = Some(step);
            break;
        }
        camera.zoom(1.5, 640.0, 400.0);
    }
    assert!(
        reached.is_some(),
        "zooming in reaches the graph's own nodes"
    );

    let worst = times.iter().copied().fold(0.0, f64::max);
    println!("a cut: median {:.1} ms, worst {worst:.1} ms", median(times));
    if !cfg!(debug_assertions) {
        assert!(worst <= 30.0, "a cut took {worst:.1} ms");
    }
}
