//! Growing a layout (#442): the acceptance test of the Graph Explorer's
//! click to expand. A node of degree 16 is expanded in a 500-node view; what
//! was drawn barely moves, pinned nodes do not move at all, the new nodes do
//! not overlap, and the same expansions give the same bits every time.

use ursa_core::algo::{Fa2Params, MultilevelParams, WarmParams};
use ursa_viz::scene::{bounds, BadGrowth, Graph, Growth, LiveLayout};

/// A 500-node view such as an exploration builds: 25 communities of 20, each
/// a ring with chords, joined in a ring of communities, with a few long ties.
fn view_edges() -> (Vec<u32>, Vec<u32>) {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move |m: u32| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 33) as u32) % m
    };
    let (mut src, mut dst) = (Vec::new(), Vec::new());
    for c in 0..25u32 {
        for i in 0..20u32 {
            let a = c * 20 + i;
            src.push(a);
            dst.push(c * 20 + (i + 1) % 20);
            src.push(a);
            dst.push(c * 20 + next(20));
        }
        src.push(c * 20);
        dst.push(((c + 1) % 25) * 20 + 10);
    }
    for _ in 0..15 {
        src.push(next(500));
        dst.push(next(500));
    }
    (src, dst)
}

/// The view after `hub` is expanded: `add` new neighbours after the `n`
/// nodes there are, a few tied to each other and to nodes already drawn.
fn expand(src: &mut Vec<u32>, dst: &mut Vec<u32>, n: u32, hub: u32, add: u32) {
    for j in 0..add {
        src.push(hub);
        dst.push(n + j);
    }
    for (a, b) in [(0, 1), (2, 9), (5, 11)] {
        src.push(n + a % add);
        dst.push(n + b % add);
    }
    // Two of the new nodes are neighbours of nodes near the hub.
    for (a, b) in [(3, hub + 1), (7, hub + 2)] {
        src.push(n + a % add);
        dst.push(b);
    }
}

fn bits(layout: &LiveLayout) -> (Vec<u32>, Vec<u32>) {
    let (x, y) = layout.positions();
    (
        x.iter().map(|v| v.to_bits()).collect(),
        y.iter().map(|v| v.to_bits()).collect(),
    )
}

fn run(layout: &mut LiveLayout, chunk: u32) {
    while !layout.is_done() {
        layout.step(chunk);
    }
}

/// The acceptance numbers of one expansion, in widths of the view before
/// it: the mean and greatest move of the old nodes that are not pinned, and
/// how close nodes are to their nearest other node (the least and the mean),
/// among the old nodes before and for the new nodes after.
struct Moved {
    mean: f32,
    worst: f32,
    old_nearest: (f32, f32),
    new_nearest: (f32, f32),
}

/// The least and the mean, over the nodes `of`, of the distance to the
/// nearest other node of `x`, `y`.
fn nearest(x: &[f32], y: &[f32], of: std::ops::Range<usize>) -> (f32, f32) {
    let (mut least, mut total) = (f32::MAX, 0.0f64);
    for i in of.clone() {
        let mut near = f32::MAX;
        for j in 0..x.len() {
            if i != j {
                near = near.min(((x[i] - x[j]).powi(2) + (y[i] - y[j]).powi(2)).sqrt());
            }
        }
        least = least.min(near);
        total += f64::from(near);
    }
    (least, (total / of.len() as f64) as f32)
}

fn measure(before: (&[f32], &[f32]), layout: &LiveLayout, pinned: &[u32]) -> Moved {
    let (x0, y0) = before;
    let (x, y) = layout.positions();
    let (min_x, min_y, max_x, max_y) = bounds(x0, y0).unwrap();
    let width = (max_x - min_x).max(max_y - min_y);
    let (mut total, mut worst, mut count) = (0.0f64, 0.0f32, 0u32);
    for i in 0..x0.len() {
        let d = ((x[i] - x0[i]).powi(2) + (y[i] - y0[i]).powi(2)).sqrt();
        if pinned.contains(&(i as u32)) {
            assert_eq!(
                (x[i].to_bits(), y[i].to_bits()),
                (x0[i].to_bits(), y0[i].to_bits()),
                "pinned node {i} moved"
            );
        } else {
            total += f64::from(d);
            worst = worst.max(d);
            count += 1;
        }
    }
    let old = nearest(x0, y0, 0..x0.len());
    let new = nearest(x, y, x0.len()..x.len());
    Moved {
        mean: (total / f64::from(count)) as f32 / width,
        worst: worst / width,
        old_nearest: (old.0 / width, old.1 / width),
        new_nearest: (new.0 / width, new.1 / width),
    }
}

#[test]
fn expanding_a_node_keeps_the_view_where_it_was() {
    let (src, dst) = view_edges();
    let graph = Graph::new(500, src.clone(), dst.clone()).unwrap();
    for multilevel in [true, false] {
        let mut layout = if multilevel {
            let mut coarse_to_fine = LiveLayout::multilevel(&graph, MultilevelParams::default());
            run(&mut coarse_to_fine, 50);
            coarse_to_fine
        } else {
            // Flat is never done: it runs for as long as it is stepped.
            let mut flat = LiveLayout::new(&graph, Fa2Params::default());
            flat.step(300);
            flat
        };
        let before = (layout.positions().0.to_vec(), layout.positions().1.to_vec());

        // Node 240 has degree 16 once expanded by 12; pin four nodes, one
        // of them its neighbour.
        let hub = 240u32;
        let had = graph.degrees()[hub as usize];
        let add = 16 - had;
        assert!(add >= 8, "degree {had}");
        let (mut s, mut d) = (src.clone(), dst.clone());
        expand(&mut s, &mut d, 500, hub, add);
        let grown = Graph::new(500 + add as usize, s, d).unwrap();
        assert!(grown.degrees()[hub as usize] >= 16);
        let pinned = [241u32, 7, 133, 499];
        layout
            .grow(
                &grown,
                Growth {
                    pinned: &pinned,
                    ..Growth::default()
                },
            )
            .unwrap();
        assert_eq!(layout.total(), Some(60), "a short schedule");
        assert_eq!((layout.iterations(), layout.is_done()), (0, false));
        assert_eq!(layout.positions().0.len(), 500 + add as usize);
        run(&mut layout, 7);
        assert_eq!(layout.iterations(), 60);
        layout.step(10);
        assert_eq!(layout.iterations(), 60, "no more than the schedule");

        let m = measure((&before.0, &before.1), &layout, &pinned);
        assert!(
            m.mean < 0.05,
            "old nodes moved {:.2}% of the view's width on average (multilevel: {multilevel})",
            m.mean * 100.0
        );
        assert!(
            m.worst < 0.10,
            "an old node moved {:.2}% of the view's width",
            m.worst * 100.0
        );
        // The new nodes do not overlap: no two are on one spot, and they
        // are spaced as the drawing is, within a factor of four (the leaves
        // of one hub sit closer together than the average pair of nodes).
        // Whether two discs touch depends on the zoom, and the old
        // drawing's closest pairs touch at a zoom that shows it whole:
        // ForceAtlas2 has no notion of a node's size.
        assert!(
            m.new_nearest.1 > 0.25 * m.old_nearest.1,
            "new nodes are {} from their nearest node on average, old ones {}",
            m.new_nearest.1,
            m.old_nearest.1
        );
        assert!(
            m.new_nearest.0 > 0.25 * m.old_nearest.0,
            "a new node is {} from another, the closest old pair {}",
            m.new_nearest.0,
            m.old_nearest.0
        );
        // And the new nodes are by the node they came from.
        let (x, y) = layout.positions();
        let (min_x, _, max_x, _) = bounds(&before.0, &before.1).unwrap();
        for i in 500..x.len() {
            let h = hub as usize;
            let dist = ((x[i] - x[h]).powi(2) + (y[i] - y[h]).powi(2)).sqrt();
            assert!(dist < 0.15 * (max_x - min_x), "new node {i} is {dist} away");
        }
    }
}

#[test]
fn the_same_expansions_give_the_same_bits() {
    let run_all = |chunk: u32| {
        let (mut src, mut dst) = view_edges();
        let graph = Graph::new(500, src.clone(), dst.clone()).unwrap();
        let mut layout = LiveLayout::multilevel(&graph, MultilevelParams::default());
        run(&mut layout, 40);
        let mut n = 500u32;
        let mut seen = Vec::new();
        for (hub, add, pinned) in [
            (240u32, 12u32, &[7u32, 133][..]),
            (505, 30, &[7, 133, 240]),
            (12, 100, &[505]),
        ] {
            expand(&mut src, &mut dst, n, hub, add);
            n += add;
            let grown = Graph::new(n as usize, src.clone(), dst.clone()).unwrap();
            layout
                .grow(
                    &grown,
                    Growth {
                        pinned,
                        ..Growth::default()
                    },
                )
                .unwrap();
            run(&mut layout, chunk);
            seen.push(bits(&layout));
        }
        seen
    };
    let a = run_all(60);
    assert_eq!(a, run_all(60), "a second run differs");
    assert_eq!(a, run_all(7), "stepping in other chunks differs");
}

#[test]
fn a_layout_given_its_positions_grows_from_them() {
    let graph = Graph::new(3, vec![0, 1], vec![1, 2]).unwrap();
    let params = Fa2Params::default();
    let err = LiveLayout::from_positions(&graph, params, vec![0.0; 3], vec![0.0; 2]).unwrap_err();
    assert_eq!(
        err.to_string(),
        "expected x then y for 3 nodes (6 values); got 5"
    );
    let (x, y) = (vec![0.0, 5.0, 10.0], vec![0.0, 0.0, 0.0]);
    let mut layout = LiveLayout::from_positions(&graph, params, x.clone(), y.clone()).unwrap();
    assert!(layout.is_done());
    assert_eq!(layout.total(), Some(0));
    layout.step(5);
    assert_eq!(layout.positions(), (&x[..], &y[..]), "nothing to run");

    // Node 1 is expanded: the new nodes start around it, with or without
    // their parent named.
    let grown = Graph::new(5, vec![0, 1, 1, 1], vec![1, 2, 3, 4]).unwrap();
    for parents in [None, Some(&[1u32, u32::MAX][..])] {
        let mut layout = LiveLayout::from_positions(&graph, params, x.clone(), y.clone()).unwrap();
        layout
            .grow(
                &grown,
                Growth {
                    parents,
                    params: WarmParams {
                        iterations: Some(0),
                        ..WarmParams::default()
                    },
                    ..Growth::default()
                },
            )
            .unwrap();
        let (gx, gy) = layout.positions();
        assert_eq!((&gx[..3], &gy[..3]), (&x[..], &y[..]));
        for i in 3..5 {
            let d = ((gx[i] - 5.0).powi(2) + gy[i].powi(2)).sqrt();
            assert!(d > 0.1 && d < 2.5, "new node {i} starts {d} from its parent");
        }
    }

    // What does not fit is refused, and the layout is as it was.
    let mut layout = LiveLayout::from_positions(&graph, params, x.clone(), y.clone()).unwrap();
    let smaller = Graph::new(2, vec![0], vec![1]).unwrap();
    assert_eq!(
        layout.grow(&smaller, Growth::default()),
        Err(BadGrowth::Shrunk { had: 3, got: 2 })
    );
    let bad = Growth {
        parents: Some(&[1]),
        ..Growth::default()
    };
    assert_eq!(
        layout.grow(&grown, bad).unwrap_err().to_string(),
        "expected one parent per new node (2); got 1"
    );
    let bad = Growth {
        pinned: &[5],
        ..Growth::default()
    };
    assert_eq!(
        layout.grow(&grown, bad).unwrap_err().to_string(),
        "pinned node 5, but the graph has 5 nodes"
    );
    assert_eq!(layout.positions(), (&x[..], &y[..]));
}
