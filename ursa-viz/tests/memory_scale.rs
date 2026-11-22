//! What a view and a layout hold at the design's reference size (#501):
//! 46,318 nodes and 1,000,000 edges, as `.graph rmat` has.
//!
//! Every allocation of this test binary is counted, so the figures are the
//! bytes asked for: a WebAssembly memory holds these, the module's own few
//! megabytes, and what its allocator cannot reuse (`web/tests/graph-timing.ts`
//! asserts the memories themselves, in a browser). The budgets are a fifth
//! over what is measured, so the test catches a second copy of the edges
//! or an adjacency built for nothing, not an allocator's rounding.
//!
//! The graph is uniformly random, which grouping does not thin: its first
//! coarse levels are as large as it is, as R-MAT's are, and that is the
//! case a lean hierarchy is for.
//!
//! ```sh
//! cargo test -p ursa-viz --no-default-features --test memory_scale -- --nocapture
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

use ursa_core::algo::MultilevelParams;
use ursa_core::topology::{Topology, UndirectedCsr};
use ursa_viz::model::*;
use ursa_viz::scene::{LiveLayout, Theme};
use ursa_viz::LiveResult;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let live = LIVE.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(live, Relaxed);
}

// SAFETY: every call goes to the system allocator unchanged; the counters
// are only read.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        if new > layout.size() {
            grew(new - layout.size());
        } else {
            LIVE.fetch_sub(layout.size() - new, Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const NODES: u32 = 46_318;
const EDGES: usize = 1_000_000;

/// The most bytes held at once while `f` ran, beyond what was held before,
/// and what it gave.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = LIVE.load(Relaxed);
    PEAK.store(before, Relaxed);
    let out = f();
    (out, PEAK.load(Relaxed) - before)
}

fn edges() -> (Vec<u32>, Vec<u32>) {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 33) % NODES as u64) as u32
    };
    let src: Vec<u32> = (0..EDGES).map(|_| next()).collect();
    let dst: Vec<u32> = (0..EDGES).map(|_| next()).collect();
    (src, dst)
}

fn per_edge(bytes: usize) -> f64 {
    bytes as f64 / EDGES as f64
}

// One test: the counters are the whole process's.
#[test]
fn a_million_edges_are_held_once_by_a_view_and_without_weights_by_a_layout() {
    let (src, dst) = edges();
    let bytes = {
        let base = GraphDelta::new(vec![
            Op::AddNodeLabel(NodeLabel::new("", KeyType::Int)),
            Op::AddEdgeLabel(String::new()),
            Op::AddNodes(NodeBatch::of_label(
                0,
                (0..NODES as i64).map(|i| i * 7 + 1_000_000),
            )),
            Op::AddEdges(EdgeBatch::of_label(0, src.clone(), dst.clone())),
        ]);
        GraphResult::build(None, Provenance::default(), &base)
            .unwrap()
            .to_bytes()
    };

    // The page's view, without its renderer: the result, decoded, and what
    // is drawn of it. The edges are held once (8 bytes each), with the
    // pairs they join (at most 4), and no label or flag while all are zero.
    let (live, view) = peak_of(|| {
        let result = GraphResult::from_bytes(&bytes).unwrap();
        LiveResult::new(result, &Theme::light())
    });
    println!("a view of the result: {:.1} bytes an edge", per_edge(view));
    assert!(per_edge(view) <= 18.0, "{view} bytes");
    let result = live.0.result();
    assert!(result.edge_label().as_slice().is_none());
    assert!(result.edge_flags().as_slice().is_none());
    drop(live);
    drop(bytes);

    // The layout worker: the adjacency from the edge list, with no
    // topology beside it, and then the hierarchy.
    let n = NODES as usize;
    let (adjacency, built) = peak_of(|| Arc::new(UndirectedCsr::from_edges(n, &src, &dst)));
    println!("its adjacency: {:.1} bytes an edge", per_edge(built));
    // Two slots an edge, and an offset a node.
    assert!(per_edge(built) <= 8.5, "{built} bytes");
    let lean = |adjacency: &Arc<UndirectedCsr>| {
        LiveLayout::multilevel_on(Arc::clone(adjacency), MultilevelParams::default())
    };
    let (layout, hierarchy) = peak_of(|| lean(&adjacency));
    println!(
        "the layout's hierarchy, {} levels: {:.1} bytes an edge",
        layout.levels(),
        per_edge(hierarchy)
    );
    assert!(per_edge(hierarchy) <= HIERARCHY, "{hierarchy} bytes");

    // The adjacency handed over is the one a topology would build, so the
    // layouts are the same: the first positions here, every bit of the
    // schedule in `ursa-core`'s tests.
    let (x, y) = layout.positions();
    let bits = |v: &[f32]| v.iter().map(|f| f.to_bits()).collect::<Vec<_>>();
    let first = (bits(x), bits(y));
    drop(layout);
    let parts = UndirectedCsr::from_parts(n, adjacency.offsets.clone(), adjacency.targets.clone());
    let handed = lean(&Arc::new(parts.unwrap()));
    let (x, y) = handed.positions();
    assert_eq!((bits(x), bits(y)), first);
    drop(handed);
    // What lean costs in time: in a release build, the whole schedule with
    // every level's weights held, and lean.
    if !cfg!(debug_assertions) {
        use ursa_core::algo::Fa2Multilevel;
        let run = |lean: bool| {
            let start = std::time::Instant::now();
            let params = MultilevelParams::default();
            let mut layout = if lean {
                Fa2Multilevel::new_lean(&adjacency, params)
            } else {
                Fa2Multilevel::new(&adjacency, params)
            };
            let built = start.elapsed();
            layout.step(&adjacency, f64::INFINITY);
            let (x, _) = layout.positions();
            (built, start.elapsed(), bits(x))
        };
        let (full, lean) = (run(false), run(true));
        println!(
            "the whole layout: {:.2} s with every weight held ({:.0} ms to build), {:.2} s lean ({:.0} ms)",
            full.1.as_secs_f64(),
            full.0.as_secs_f64() * 1e3,
            lean.1.as_secs_f64(),
            lean.0.as_secs_f64() * 1e3,
        );
        assert_eq!(full.2, lean.2);
    }
    let topology = Topology::build(n, src, dst);
    assert_eq!(topology.undirected().targets, adjacency.targets);
    assert_eq!(topology.undirected().offsets, adjacency.offsets);
}

/// The hierarchy's budget, in bytes an edge at its peak. Measured: 52,
/// which is the targets of six levels as large as the graph (8 bytes an
/// edge each) and one level's weights; with every level's weights held it
/// is 100. R-MAT's levels thin sooner: 38.
const HIERARCHY: f64 = 62.0;
