//! The model at the design's reference size (design §1): `.graph rmat`,
//! 46,318 nodes and 1,000,000 edges.
//!
//! The size is asserted always. The times are asserted only in a release
//! build, where they mean something, against budgets several times what is
//! measured (see `UGR.md`), so the test catches a change of complexity and
//! not a slow machine:
//!
//! ```sh
//! cargo test -p ursa-viz --no-default-features --release --test model_scale -- --nocapture
//! ```

use std::time::{Duration, Instant};

use ursa_viz::model::*;

const NODES: u32 = 46_318;
const EDGES: usize = 1_000_000;

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let out = f();
    (out, start.elapsed())
}

/// The fastest of five runs: what the work costs, without the machine's noise.
fn best<T>(mut f: impl FnMut() -> T) -> (T, Duration) {
    let mut best = timed(&mut f);
    for _ in 0..4 {
        let next = timed(&mut f);
        if next.1 < best.1 {
            best = next;
        }
    }
    best
}

/// A million edges among 46,318 integer-keyed nodes, by a fixed generator.
fn edge_list() -> GraphDelta {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 33) % NODES as u64) as u32
    };
    let src: Vec<u32> = (0..EDGES).map(|_| next()).collect();
    let dst: Vec<u32> = (0..EDGES).map(|_| next()).collect();
    GraphDelta::new(vec![
        Op::AddNodeLabel(NodeLabel::new("", KeyType::Int)),
        Op::AddEdgeLabel(String::new()),
        // Keys that are not the indexes, as a result's are not.
        Op::AddNodes(NodeBatch::of_label(
            0,
            (0..NODES as i64).map(|i| i * 7 + 1_000_000),
        )),
        Op::AddEdges(EdgeBatch::of_label(0, src, dst)),
    ])
}

#[test]
fn a_million_edges_cost_eight_bytes_each_and_milliseconds() {
    let release = !cfg!(debug_assertions);
    let budget = |what: &str, took: Duration, limit_ms: u64| {
        println!(
            "{what}: {:.2} ms (budget {limit_ms} ms)",
            took.as_secs_f64() * 1e3
        );
        if release {
            assert!(
                took <= Duration::from_millis(limit_ms),
                "{what} took {took:?}, over {limit_ms} ms"
            );
        }
    };

    let delta = edge_list();
    let (result, took) = timed(|| GraphResult::build(None, Provenance::default(), &delta).unwrap());
    budget("build 1M edges from a delta", took, 250);
    assert_eq!(
        (result.n_nodes(), result.n_edges()),
        (NODES as usize, EDGES)
    );

    // Size: the two endpoint arrays, the keys' text and offsets, and a
    // header of a kilobyte. No labels, flags or rows, which are all default.
    let (bytes, took) = best(|| result.to_bytes());
    budget("encode", took, 30);
    let edges = EDGES * 8;
    let keys = result.node_keys().parts().1.len() + (NODES as usize + 1) * 4;
    let overhead = bytes.len() - edges - keys;
    println!(
        "{} bytes: {edges} of edges, {keys} of keys, {overhead} of header and padding; {:.3} bytes an edge",
        bytes.len(),
        bytes.len() as f64 / EDGES as f64
    );
    assert!(overhead < 2048, "{overhead} bytes of header and padding");
    assert!(bytes.len() < EDGES * 8 + NODES as usize * 12 + 2048);

    let (back, took) = best(|| GraphResult::from_bytes(&bytes).unwrap());
    budget("decode and validate", took, 60);
    assert!(back == result);
    assert!(back.to_bytes() == bytes);

    // The same with every optional section: labels, rows, flags, a column a
    // side, positions. 8 more bytes an edge for the rows, 4 for the column.
    let mut full = result.clone();
    let every = GraphDelta::new(vec![
        // Made for the million edges; the edge added next gets a null.
        full.set_column_op(
            Target::Edges,
            Column::new("weight", None, Values::F32(vec![1.5; EDGES])),
        ),
        Op::AddEdgeLabel("second".into()),
        Op::AddEdges(EdgeBatch::of_label(1, vec![1], vec![2]).with_rows(vec![5])),
        full.set_column_op(
            Target::Nodes,
            Column::new(
                "community",
                Some(Role::Category),
                Values::U32((0..NODES).map(|i| i % 9).collect()),
            ),
        ),
        full.set_positions_op(Some(Positions {
            x: (0..NODES).map(|i| i as f32).collect(),
            y: (0..NODES).map(|i| -(i as f32)).collect(),
        })),
        Op::RemoveNodes(vec![3]),
    ]);
    full.apply(&every).unwrap();
    let (full_bytes, took) = best(|| full.to_bytes());
    budget("encode with every section", took, 100);
    println!("{} bytes with every section", full_bytes.len());
    assert!(
        full_bytes.len()
            < (EDGES + 1) * (8 + 2 + 8 + 1 + 4) + NODES as usize * (12 + 1 + 4 + 8) + 4096
    );
    let (back, took) = best(|| GraphResult::from_bytes(&full_bytes).unwrap());
    budget("decode and validate with every section", took, 100);
    assert!(back == full);

    // A step of exploring: 100 new nodes and 200 edges to them, applied and
    // undone. The design's budget for an undo is a frame.
    let step = GraphDelta::new(vec![
        Op::AddNodes(NodeBatch::of_label(0, (0..100i64).map(|i| -1 - i))),
        Op::AddEdges(EdgeBatch::of_label(
            0,
            (0..200).map(|i| NODES + i % 100).collect(),
            (0..200).map(|i| i * 37 % NODES).collect(),
        )),
    ]);
    let step_bytes = step.to_bytes();
    assert!(
        step_bytes.len() < 4096,
        "{} bytes for a step",
        step_bytes.len()
    );
    let mut stepped = result.clone();
    let ((), took) = timed(|| {
        stepped
            .apply(&GraphDelta::from_bytes(&step_bytes).unwrap())
            .unwrap()
    });
    budget("decode and apply a step of 100 nodes", took, 16);
    assert_eq!(stepped.n_edges(), EDGES + 200);
    let ((), took) = timed(|| stepped.apply(&step.invert()).unwrap());
    budget("undo it", took, 16);
    assert!(stepped == result);
    assert!(stepped.to_bytes() == bytes);
}
