//! The compressed layout gives the plain layout's results (#173).
//!
//! A compressed topology ([`Topology::compressed`]) sorts each node's
//! neighbours and stores them as varint-coded gaps. Every kernel reads it
//! through the same seam as a plain one, so:
//!
//! 1. **Bit for bit** against the plain layout with its segments sorted the
//!    same way ([`Topology::with_sorted_segments`]): every kernel, plain,
//!    weighted and masked, serial and parallel, with the edge ids built
//!    eagerly or late. This is what shows every decode path is right: the
//!    neighbours, their order and their edge rows are the same.
//! 2. **Against the default plain layout**, whose segments keep their rows
//!    in row order: every result that does not depend on the order of a
//!    node's neighbours is identical, bit for bit: components, distances,
//!    triangles, clustering, closeness, degree, Louvain and label
//!    propagation (serial and parallel, weighted and masked: both work on
//!    a graph of their own whose segments they sort), the induced edges,
//!    the weighted symmetric view and path costs. `hop` lists the nodes it
//!    reached in the order it reached them, so its (node, distance) pairs
//!    are compared in node order. Three kinds can differ, and are checked
//!    as such:
//!    - sums of floating-point values in neighbour order (PageRank and
//!      betweenness, plain, weighted and masked): within a relative 1e-9;
//!    - which of several shortest paths of one length is returned: the same
//!      length and ends, and a path of the graph;
//!    - a random walk's steps, which pick a neighbour by its place: checked
//!      against the sorted plain layout only (1.).

use std::sync::Arc;

use ursa_core::algo::{
    betweenness, betweenness_weighted, bfs_distances, closeness_directed, closeness_weighted,
    clustering_coefficient, connected_components_strong, connected_components_weak, degree,
    dijkstra_distances, induced_edges, k_hop_distances_masked, k_hop_reached_set,
    label_propagation, louvain, louvain_weighted, neighbor_aggregate, pagerank, pagerank_weighted,
    random_walk_masked, shortest_path, shortest_path_masked,
    shortest_path_weighted_with_cost_masked, symmetric_view_weighted, triangle_count, AggKind,
    PageRankParams,
};
use ursa_core::topology::{Direction, EdgeMask, NodeOrder, RowSource, Topology};

/// splitmix64.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `m` skewed random rows over `n` nodes, with self-loops and parallel rows;
/// sorted by source when `sorted`.
fn rows(n: u32, m: usize, seed: u64, sorted: bool) -> (Vec<u32>, Vec<u32>) {
    let mut st = seed;
    let pick = |st: &mut u64| {
        let x = (next(st) % n as u64) as f64 / n as f64;
        ((x * x) * n as f64) as u32
    };
    let mut pairs: Vec<(u32, u32)> = (0..m).map(|_| (pick(&mut st), pick(&mut st))).collect();
    if sorted {
        pairs.sort_by_key(|p| p.0);
    }
    pairs.into_iter().unzip()
}

/// How a topology is laid out.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    /// The default: plain, each segment in row order.
    Plain,
    /// Plain, each segment sorted as the compressed layout sorts it.
    Sorted,
    /// Compressed, its edge ids built with it.
    Compressed,
    /// Compressed, its edge ids built when first read, from the rows read
    /// back (as the engine builds a large index).
    CompressedLate,
}

fn build(n: u32, (s, d): &(Vec<u32>, Vec<u32>), order: NodeOrder, layout: Layout) -> Topology {
    match layout {
        Layout::Plain => Topology::build_ordered(n as usize, s.clone(), d.clone(), order),
        Layout::Sorted => {
            Topology::build_ordered(n as usize, s.clone(), d.clone(), order).with_sorted_segments()
        }
        Layout::Compressed => {
            Topology::build_ordered(n as usize, s.clone(), d.clone(), order).compressed(None)
        }
        Layout::CompressedLate => {
            let t =
                Topology::build_with_rows(n as usize, s.clone(), d.clone(), order, &mut |src| {
                    Some(Arc::new(src.to_vec()) as Arc<dyn RowSource>)
                });
            t.compressed(Some(Arc::new(d.clone())))
        }
    }
}

/// What a kernel gave, and how it may differ between layouts whose segments
/// are in different orders.
#[derive(Debug, Clone)]
enum Got {
    /// The same whatever the order of a node's neighbours.
    Exact(String),
    /// Sums in neighbour order: the same within a relative 1e-9.
    Sums(Vec<f64>),
    /// A shortest path, one of several of the same length: the same length,
    /// and a path of the graph (`Direction`, and whether edges are masked).
    Path(Option<Vec<u32>>, Direction),
    /// A choice among ties in neighbour order: compared with the sorted
    /// plain layout only.
    Ordered(String),
}

fn sums(v: &[f64]) -> Got {
    Got::Sums(v.to_vec())
}

fn exact(x: impl std::fmt::Debug) -> Got {
    Got::Exact(format!("{x:?}"))
}

/// The (seed, node, distance) triples `hop` reached, in node order: the kernel
/// lists them in the order it reached them, which follows neighbour order.
fn reached((seeds, nodes, dist): (Vec<u32>, Vec<u32>, Vec<u32>)) -> Got {
    let mut pairs: Vec<(u32, u32, u32)> = seeds
        .into_iter()
        .zip(nodes)
        .zip(dist)
        .map(|((s, v), d)| (s, v, d))
        .collect();
    pairs.sort_unstable();
    exact(pairs)
}

fn ordered(x: impl std::fmt::Debug) -> Got {
    Got::Ordered(format!("{x:?}"))
}

/// Every kernel, on `t`: `(name, result)`. Plain ones first (no edge row is
/// read), then the weighted and masked ones.
fn kernels(t: &Topology, plain_only: bool) -> Vec<(&'static str, Got)> {
    let params = PageRankParams {
        damping: 0.85,
        max_iter: 50,
        tol: 1e-12,
    };
    let n = t.n_nodes() as u32;
    let attr: Vec<Option<f64>> = (0..n).map(|v| Some(v as f64)).collect();
    let mut out = vec![
        ("pagerank", sums(&pagerank(t, None, params))),
        ("wcc", exact(connected_components_weak(t, None))),
        ("scc", exact(connected_components_strong(t, None))),
        ("bfs out", exact(bfs_distances(t, 0, Direction::Out))),
        ("bfs in", exact(bfs_distances(t, 1, Direction::In))),
        ("bfs both", exact(bfs_distances(t, 0, Direction::Both))),
        (
            "hop in",
            reached(k_hop_distances_masked(t, None, &[0, 3], 3, Direction::In)),
        ),
        (
            "hop both",
            exact(k_hop_reached_set(t, &[0, 3], 2, Direction::Both)),
        ),
        ("louvain", exact(louvain(t, None, 1.0, Some(1), false))),
        (
            "louvain parallel",
            exact(louvain(t, None, 1.0, Some(1), true)),
        ),
        (
            "label propagation",
            exact(label_propagation(t, None, 20, Some(1), false)),
        ),
        (
            "label propagation parallel",
            exact(label_propagation(t, None, 20, Some(1), true)),
        ),
        ("triangles", exact(triangle_count(t, None))),
        ("clustering", exact(clustering_coefficient(t, None))),
        (
            "closeness out",
            exact(closeness_directed(t, None, Direction::Out)),
        ),
        (
            "closeness both",
            exact(closeness_directed(t, None, Direction::Both)),
        ),
        (
            "betweenness",
            sums(&betweenness(t, None, Some(0.2), Some(7))),
        ),
        ("degree", exact(degree(t, None, Direction::Both))),
        (
            "neighbour sum",
            exact(neighbor_aggregate(
                t,
                &attr,
                None,
                Direction::Both,
                AggKind::Sum,
            )),
        ),
        (
            "shortest path out",
            Got::Path(shortest_path(t, 0, n - 1, Direction::Out), Direction::Out),
        ),
        (
            "shortest path both",
            Got::Path(
                shortest_path(t, n - 1, n / 2, Direction::Both),
                Direction::Both,
            ),
        ),
        (
            "symmetric pagerank",
            sums(&pagerank(t.symmetric(), None, params)),
        ),
    ];
    if plain_only {
        return out;
    }
    let m = t.n_edges();
    let w: Vec<f64> = (0..m).map(|e| (e % 13) as f64 * 0.25 + 0.5).collect();
    let keep: Vec<bool> = (0..m).map(|e| e % 5 != 2).collect();
    let mask = EdgeMask::from_bools(&keep);
    let mk = Some(&mask);
    let (wsym, sw) = symmetric_view_weighted(t, &w, mk);
    let induced: Vec<u32> = (0..n).step_by(3).collect();
    let nodes: Vec<bool> = (0..n).map(|v| v % 3 != 0).collect();
    out.extend([
        ("masked pagerank", sums(&pagerank(t, mk, params))),
        (
            "weighted pagerank",
            sums(&pagerank_weighted(t, &w, None, params)),
        ),
        (
            "masked weighted pagerank",
            sums(&pagerank_weighted(t, &w, mk, params)),
        ),
        ("masked wcc", exact(connected_components_weak(t, mk))),
        ("masked scc", exact(connected_components_strong(t, mk))),
        (
            "masked hop",
            reached(k_hop_distances_masked(t, mk, &[0, 3], 3, Direction::Both)),
        ),
        (
            "dijkstra",
            exact(dijkstra_distances(t, &w, 0, Direction::Both)),
        ),
        ("masked louvain", exact(louvain(t, mk, 1.0, Some(1), false))),
        (
            "weighted louvain",
            exact(louvain_weighted(t, &w, None, 1.0, Some(1), true)),
        ),
        (
            "masked label propagation",
            exact(label_propagation(t, mk, 20, Some(1), false)),
        ),
        ("masked triangles", exact(triangle_count(t, mk))),
        ("weighted closeness", exact(closeness_weighted(t, &w, mk))),
        (
            "masked betweenness",
            sums(&betweenness(t, mk, Some(0.2), Some(7))),
        ),
        (
            "weighted betweenness",
            sums(&betweenness_weighted(t, &w, mk, Some(0.2), Some(7))),
        ),
        ("masked degree", exact(degree(t, mk, Direction::Both))),
        (
            "masked neighbour max",
            exact(neighbor_aggregate(
                t,
                &attr,
                mk,
                Direction::In,
                AggKind::Max,
            )),
        ),
        (
            "masked random walk",
            ordered(random_walk_masked(t, mk, &[0, 1, 2], 10, 2, Some(3)).node),
        ),
        ("induced edges", exact(induced_edges(t, &induced))),
        (
            "induced mask",
            exact(EdgeMask::induced(t, &nodes, mk).words()),
        ),
        (
            "weighted symmetric view",
            exact((&wsym.out().offsets, &wsym.out().targets, &sw)),
        ),
        (
            "masked shortest path",
            Got::Path(
                shortest_path_masked(t, mk, n - 1, 0, Direction::Both),
                Direction::Both,
            ),
        ),
        (
            "weighted shortest path cost",
            exact(
                shortest_path_weighted_with_cost_masked(t, mk, &w, 0, n - 1, Direction::Both)
                    .map(|(_, cost)| cost.last().copied()),
            ),
        ),
    ]);
    out
}

/// Whether `path` is a path of `t` in `dir`.
fn is_path(t: &Topology, path: &[u32], dir: Direction) -> bool {
    path.windows(2).all(|w| {
        let mut found = false;
        t.for_each_neighbor(w[0], dir, None, |v| found |= v == w[1]);
        found
    })
}

/// Check `got` (compressed) against `want` (plain, in row order).
fn agrees(plain: &Topology, name: &str, got: &Got, want: &Got) -> Result<(), String> {
    match (got, want) {
        (Got::Exact(a), Got::Exact(b)) if a == b => Ok(()),
        (Got::Sums(a), Got::Sums(b)) => {
            if a.len() != b.len() {
                return Err(format!("{name}: lengths differ"));
            }
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                if (x - y).abs() > 1e-9 * x.abs().max(y.abs()).max(1e-300) {
                    return Err(format!("{name}: node {i}: {x} against {y}"));
                }
            }
            Ok(())
        }
        (Got::Path(a, dir), Got::Path(b, _)) => match (a, b) {
            (None, None) => Ok(()),
            (Some(a), Some(b))
                if a.len() == b.len()
                    && a.first() == b.first()
                    && a.last() == b.last()
                    && is_path(plain, a, *dir) =>
            {
                Ok(())
            }
            _ => Err(format!("{name}: {a:?} against {b:?}")),
        },
        (Got::Ordered(_), Got::Ordered(_)) => Ok(()),
        _ => Err(format!("{name}: {got:?} against {want:?}")),
    }
}

/// The same, bit for bit.
fn same(got: &Got, want: &Got) -> bool {
    match (got, want) {
        (Got::Exact(a), Got::Exact(b)) | (Got::Ordered(a), Got::Ordered(b)) => a == b,
        (Got::Sums(a), Got::Sums(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
        }
        (Got::Path(a, _), Got::Path(b, _)) => a == b,
        _ => false,
    }
}

/// A named graph: its node count, rows and node order.
type Graph = (String, u32, (Vec<u32>, Vec<u32>), NodeOrder);

/// Unsorted and sorted rows, on both sides of the parallel thresholds, in
/// both node orders.
fn graphs() -> Vec<Graph> {
    let mut out = Vec::new();
    for (sorted, m) in [(false, 70_000), (true, 70_000), (false, 3_000)] {
        for order in [NodeOrder::AsGiven, NodeOrder::InDegree] {
            let n = 1_500;
            let name = format!("sorted={sorted} m={m} {order:?}");
            out.push((name, n, rows(n, m, 0x173 + m as u64, sorted), order));
        }
    }
    out
}

/// Run `f` on a pool of `threads` threads (1: every kernel's serial path).
fn on_threads<R: Send>(threads: usize, f: impl FnOnce() -> R + Send) -> R {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("a thread pool")
        .install(f)
}

#[test]
fn every_kernel_on_the_compressed_layout_is_the_sorted_plain_layout_bit_for_bit() {
    for threads in [1, 4] {
        on_threads(threads, || {
            for (name, n, rows, order) in graphs() {
                let sorted = kernels(&build(n, &rows, order, Layout::Sorted), false);
                for layout in [Layout::Compressed, Layout::CompressedLate] {
                    let t = build(n, &rows, order, layout);
                    assert!(t.is_compressed());
                    let got = kernels(&t, false);
                    for ((kernel, a), (_, b)) in got.iter().zip(&sorted) {
                        assert!(
                            same(a, b),
                            "{name} {layout:?} on {threads} threads: {kernel}: {a:?} against {b:?}"
                        );
                    }
                }
            }
        });
    }
}

#[test]
fn every_kernel_on_the_compressed_layout_agrees_with_the_plain_layout() {
    for threads in [1, 4] {
        on_threads(threads, || {
            for (name, n, rows, order) in graphs() {
                let plain = build(n, &rows, order, Layout::Plain);
                let want = kernels(&plain, false);
                let t = build(n, &rows, order, Layout::CompressedLate);
                for ((kernel, a), (_, b)) in kernels(&t, false).iter().zip(&want) {
                    if let Err(e) = agrees(&plain, kernel, a, b) {
                        panic!("{name} on {threads} threads: {e}");
                    }
                }
            }
        });
    }
}

#[test]
fn plain_kernels_on_the_compressed_layout_build_no_edge_ids() {
    for (name, n, rows, order) in graphs() {
        let t = build(n, &rows, order, Layout::CompressedLate);
        let _ = kernels(&t, true);
        assert!(
            !t.out().edge_ids.is_built() && !t.incoming().edge_ids.is_built(),
            "{name}: a kernel that reads no edge row built the edge ids"
        );
    }
}

#[test]
fn late_edge_ids_follow_the_sorted_order() {
    for (name, n, rows, order) in graphs() {
        let sorted = build(n, &rows, order, Layout::Sorted);
        let eager = build(n, &rows, order, Layout::Compressed);
        let late = build(n, &rows, order, Layout::CompressedLate);
        let ids = |t: &Topology| {
            (
                t.out().edge_ids.detached(),
                t.incoming().edge_ids.detached(),
            )
        };
        assert_eq!(ids(&late), ids(&sorted), "{name}");
        assert_eq!(ids(&eager), ids(&sorted), "{name}");
        assert!(!late.out().edge_ids.is_built() && !late.incoming().edge_ids.is_built());
        // Each segment's rows by (target, row), its targets ascending.
        let out = sorted.out();
        let plain = build(n, &rows, order, Layout::Plain);
        let original = |v: u32| plain.original_id(v);
        let ids = &out.edge_ids;
        for u in 0..n {
            let (lo, hi) = (
                out.offsets[u as usize] as usize,
                out.offsets[u as usize + 1] as usize,
            );
            let seg = &out.targets[lo..hi];
            assert!(seg.windows(2).all(|w| w[0] <= w[1]), "{name}");
            for k in lo..hi {
                let row = ids[k] as usize;
                assert_eq!(rows.0[row], original(u), "{name}");
                assert_eq!(rows.1[row], original(out.targets[k]), "{name}");
                if k > lo && out.targets[k] == out.targets[k - 1] {
                    assert!(ids[k] > ids[k - 1], "{name}");
                }
            }
        }
        // The in-adjacency's, by (source, row).
        let inc = sorted.incoming();
        for v in 0..n {
            let (lo, hi) = (
                inc.offsets[v as usize] as usize,
                inc.offsets[v as usize + 1] as usize,
            );
            for k in lo..hi {
                let row = inc.edge_ids[k] as usize;
                assert_eq!(rows.1[row], original(v), "{name}");
                assert_eq!(rows.0[row], original(inc.targets[k]), "{name}");
                if k > lo {
                    let prev = (inc.targets[k - 1], inc.edge_ids[k - 1]);
                    assert!(prev < (inc.targets[k], inc.edge_ids[k]), "{name}");
                }
            }
        }
    }
}

#[test]
fn a_compressed_topology_decodes_to_the_sorted_one_and_takes_less_room() {
    for (name, n, rows, order) in graphs() {
        let sorted = build(n, &rows, order, Layout::Sorted);
        let t = build(n, &rows, order, Layout::CompressedLate);
        let back = t.out().unpacked();
        assert_eq!(back.offsets, sorted.out().offsets, "{name}");
        assert_eq!(back.targets, sorted.out().targets, "{name}");
        let back = t.incoming().unpacked();
        assert_eq!(back.targets, sorted.incoming().targets, "{name}");
        let packed = t.out().packed.as_ref().unwrap();
        assert!(packed.bytes.len() < 4 * t.n_edges(), "{name}");
        // Its parts check out as a stored topology's would.
        let again = Topology::from_parts(n as usize, t.out().clone(), Some(t.incoming().clone()));
        assert!(again.is_ok(), "{name}: {:?}", again.err());
    }
}

#[test]
fn malformed_compressed_parts_are_an_error() {
    // Two nodes; node 0 has `degree` slots whose segment is `bytes`.
    let adj = |degree: u64, bytes: Vec<u8>| ursa_core::Adjacency {
        offsets: vec![0, degree, degree].into(),
        targets: Vec::<u32>::new().into(),
        edge_ids: (0..degree as u32).collect::<Vec<u32>>().into(),
        packed: Some(ursa_core::Packed {
            byte_offsets: vec![0, bytes.len() as u64, bytes.len() as u64].into(),
            bytes: bytes.into(),
        }),
    };
    assert!(Topology::from_parts(2, adj(2, vec![0, 1]), None).is_ok());
    for (degree, bytes) in [
        (1, vec![5]),                                  // a target past the last node
        (2, vec![1]),                                  // fewer values than slots
        (1, vec![0, 1]),                               // more
        (1, vec![0x81]),                               // a gap cut short
        (1, vec![0x80, 0x80, 0x80, 0x80, 0x80, 0x00]), // a gap of six bytes
    ] {
        assert!(
            Topology::from_parts(2, adj(degree, bytes.clone()), None).is_err(),
            "{bytes:?}"
        );
    }
    // Byte offsets that do not span the bytes.
    let mut bad = adj(2, vec![0, 1]);
    bad.packed.as_mut().unwrap().byte_offsets = vec![0, 1, 1].into();
    assert!(Topology::from_checksummed_parts(2, bad).is_err());
}
