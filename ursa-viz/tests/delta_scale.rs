//! A delta on the design's large view (#441): 100 new nodes and 300 new
//! edges on 131,072 nodes and 1,000,000 edges cost what the delta holds,
//! not what the graph does.
//!
//! The bytes are asserted always: at most 64 KB to the GPU. The time is
//! asserted only in a release build, where it means something: at most the
//! issue's 2 ms for everything outside the frame but the GPU writes
//! themselves (which the browser bench measures; see `BENCH.md`):
//!
//! ```sh
//! cargo test -p ursa-viz --no-default-features --release --test delta_scale -- --nocapture
//! ```

use std::time::{Duration, Instant};

use ursa_viz::live::LiveResult;
use ursa_viz::model::*;
use ursa_viz::scene::Theme;

const NODES: u32 = 131_072;
const EDGES: usize = 1_000_000;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u32) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) % u64::from(n)) as u32
    }
}

#[test]
fn a_small_delta_on_a_million_edges_costs_what_it_holds() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let src: Vec<u32> = (0..EDGES).map(|_| rng.below(NODES)).collect();
    let dst: Vec<u32> = (0..EDGES).map(|_| rng.below(NODES)).collect();
    let base = GraphDelta::new(vec![
        Op::AddNodeLabel(NodeLabel::new("node", KeyType::Int)),
        Op::AddEdgeLabel("edge".into()),
        Op::AddNodes(NodeBatch::of_label(0, 0..i64::from(NODES))),
        Op::AddEdges(EdgeBatch::of_label(0, src, dst)),
    ]);
    let result = GraphResult::build(None, Provenance::default(), &base).unwrap();
    let theme = Theme::light();
    let (mut live, mut drawn) = LiveResult::new(result, &theme);
    drawn.highlight.take_dirty();
    let radius = drawn.style.radius.clone();

    // An expansion: 100 new nodes off one hub, each with an edge to it and
    // two to nodes there are.
    let hub = 7;
    let new = |i: u32| NODES + i;
    let mut edges = (Vec::new(), Vec::new());
    for i in 0..100 {
        for other in [hub, rng.below(NODES), rng.below(NODES)] {
            edges.0.push(new(i));
            edges.1.push(other);
        }
    }
    let delta = GraphDelta::new(vec![
        Op::AddNodes(NodeBatch::of_label(
            0,
            i64::from(NODES)..i64::from(NODES) + 100,
        )),
        Op::AddEdges(EdgeBatch::of_label(0, edges.0, edges.1)),
    ]);
    let bytes = delta.to_bytes();
    let inverse = delta.invert();

    let mut times = Vec::new();
    let mut undo_times = Vec::new();
    let mut uploaded = 0;
    for _ in 0..20 {
        let start = Instant::now();
        // As `applyDelta` does: decode, apply, and gather what to write.
        let decoded = GraphDelta::from_bytes(&bytes).unwrap();
        let changes = live.apply(&decoded, &mut drawn.parts(&theme)).unwrap();
        let lines = live.line_indices(changes.new_edges_from..changes.edges.1);
        let flags: usize = drawn
            .highlight
            .take_dirty()
            .iter()
            .map(|r| 4 * r.len())
            .sum();
        times.push(start.elapsed());
        assert_eq!(changes.nodes, (NODES as usize, NODES as usize + 100));
        assert_eq!(changes.edges, (EDGES, EDGES + 300));
        assert_eq!(lines.len(), 600);
        uploaded = changes.upload_bytes() + flags as u64;
        assert!(drawn.x[NODES as usize..].iter().all(|v| v.is_finite()));

        let start = Instant::now();
        let back = live.apply(&inverse, &mut drawn.parts(&theme)).unwrap();
        undo_times.push(start.elapsed());
        assert_eq!(back.edges, (EDGES + 300, EDGES));
        assert!(drawn.highlight.take_dirty().is_empty());
    }
    // Undone, the radii are the first ones, bit for bit.
    assert!(drawn
        .style
        .radius
        .iter()
        .zip(&radius)
        .all(|(a, b)| a.to_bits() == b.to_bits()));

    // 100 nodes of 16 bytes and 4 of flags, 300 edges of 8, and the radii
    // of the older nodes that gained a neighbour, in runs.
    println!("a 100-node, 300-edge delta on 1M edges: {uploaded} bytes to the GPU");
    assert!(uploaded >= 100 * 20 + 300 * 8);
    assert!(uploaded <= 64 * 1024, "{uploaded} bytes, over 64 KB");

    let first = times[0];
    println!("the first, cold: {:.3} ms", first.as_secs_f64() * 1e3);
    times.sort();
    let (best, median) = (times[0], times[times.len() / 2]);
    println!(
        "decode, apply and gather: best {:.3} ms, median {:.3} ms (budget 2 ms)",
        best.as_secs_f64() * 1e3,
        median.as_secs_f64() * 1e3
    );
    undo_times.sort();
    let undo = undo_times[undo_times.len() / 2];
    println!("its inverse: median {:.3} ms", undo.as_secs_f64() * 1e3);
    if !cfg!(debug_assertions) {
        // The first has room made for it when the view is: it moves nothing.
        for (what, took) in [
            ("the first delta", first),
            ("a delta", median),
            ("its inverse", undo),
        ] {
            assert!(
                took <= Duration::from_millis(2),
                "{what} took {took:?}, over 2 ms"
            );
        }
    }
}
