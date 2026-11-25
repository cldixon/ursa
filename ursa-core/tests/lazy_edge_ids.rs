//! Edge ids built late are the edge ids built eagerly (#170).
//!
//! An index can build its out-adjacency's edge ids with the CSR (the eager
//! build), know them to be the identity (rows grouped by source), or read the
//! rows' sources back and build them on first read (a [`RowSource`]: in
//! memory here, or spilled to a temporary file). The in-adjacency's are then
//! the transpose's, also built late. Every kernel must give the same bytes on
//! each, plain, weighted and masked; and an unweighted, unmasked kernel must
//! never build them.

use std::sync::Arc;

use ursa_core::algo::{
    betweenness, betweenness_weighted, bfs_distances, closeness_directed, closeness_weighted,
    clustering_coefficient, connected_components_strong, connected_components_weak, degree,
    dijkstra_distances, induced_edges, k_hop_distances_masked, label_propagation, louvain,
    louvain_weighted, neighbor_aggregate, pagerank, pagerank_weighted, random_walk_masked,
    symmetric_view_weighted, triangle_count, AggKind, PageRankParams,
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
    let mut pick = |st: &mut u64| {
        // Squaring a uniform skews towards low ids: a few hubs.
        let x = (next(st) % n as u64) as f64 / n as f64;
        ((x * x) * n as f64) as u32
    };
    let mut pairs: Vec<(u32, u32)> = (0..m).map(|_| (pick(&mut st), pick(&mut st))).collect();
    if sorted {
        pairs.sort_by_key(|p| p.0);
    }
    pairs.into_iter().unzip()
}

/// How the out-adjacency's edge ids are had.
#[derive(Clone, Copy, Debug)]
enum Ids {
    Eager,
    InMemory,
    #[cfg(unix)]
    Spilled,
}

fn build(n: u32, (s, d): &(Vec<u32>, Vec<u32>), order: NodeOrder, ids: Ids) -> Topology {
    let mut rows = |src: &[u32]| -> Option<Arc<dyn RowSource>> {
        match ids {
            Ids::Eager => None,
            Ids::InMemory => Some(Arc::new(src.to_vec())),
            #[cfg(unix)]
            Ids::Spilled => Some(Arc::new(
                ursa_core::topology::SpilledRows::write(src).expect("a temporary file"),
            )),
        }
    };
    Topology::build_with_rows(n as usize, s.clone(), d.clone(), order, &mut rows)
}

const LAZY: &[Ids] = &[
    Ids::InMemory,
    #[cfg(unix)]
    Ids::Spilled,
];

/// Every kernel that reads no edge row, on `t`: none of them may build the
/// edge ids.
fn plain(t: &Topology) -> Vec<String> {
    let params = PageRankParams {
        damping: 0.85,
        max_iter: 50,
        tol: 1e-12,
    };
    vec![
        format!("{:?}", pagerank(t, None, params)),
        format!("{:?}", connected_components_weak(t, None)),
        format!("{:?}", connected_components_strong(t, None)),
        format!("{:?}", bfs_distances(t, 0, Direction::Out)),
        format!("{:?}", bfs_distances(t, 0, Direction::Both)),
        format!("{:?}", k_hop_distances_masked(t, None, &[0, 3], 3, Direction::In)),
        format!("{:?}", louvain(t, None, 1.0, Some(1), false)),
        format!("{:?}", louvain(t, None, 1.0, Some(1), true)),
        format!("{:?}", label_propagation(t, None, 20, Some(1), false)),
        format!("{:?}", label_propagation(t, None, 20, Some(1), true)),
        format!("{:?}", triangle_count(t, None)),
        format!("{:?}", clustering_coefficient(t, None)),
        format!("{:?}", closeness_directed(t, None, Direction::Out)),
        format!("{:?}", betweenness(t, None, Some(0.2), Some(7))),
        format!("{:?}", degree(t, None, Direction::Both)),
        format!(
            "{:?}",
            neighbor_aggregate(
                t,
                &(0..t.n_nodes()).map(|v| Some(v as f64)).collect::<Vec<_>>(),
                None,
                Direction::Both,
                AggKind::Sum
            )
        ),
        format!("{:?}", pagerank(t.symmetric(), None, params)),
    ]
}

/// Every kernel that reads edge rows (weights, a mask, the rows themselves).
fn rowed(t: &Topology) -> Vec<String> {
    let params = PageRankParams {
        damping: 0.85,
        max_iter: 50,
        tol: 1e-12,
    };
    let m = t.n_edges();
    let w: Vec<f64> = (0..m).map(|e| (e % 13) as f64 * 0.25 + 0.5).collect();
    let keep: Vec<bool> = (0..m).map(|e| e % 5 != 2).collect();
    let mask = EdgeMask::from_bools(&keep);
    let mk = Some(&mask);
    let (wsym, sw) = symmetric_view_weighted(t, &w, mk);
    let induced: Vec<u32> = (0..t.n_nodes() as u32).step_by(3).collect();
    let nodes: Vec<bool> = (0..t.n_nodes()).map(|v| v % 3 != 0).collect();
    vec![
        format!("{:?}", pagerank(t, mk, params)),
        format!("{:?}", pagerank_weighted(t, &w, None, params)),
        format!("{:?}", pagerank_weighted(t, &w, mk, params)),
        format!("{:?}", connected_components_weak(t, mk)),
        format!("{:?}", connected_components_strong(t, mk)),
        format!("{:?}", k_hop_distances_masked(t, mk, &[0, 3], 3, Direction::Both)),
        format!("{:?}", dijkstra_distances(t, &w, 0, Direction::Both)),
        format!("{:?}", louvain(t, mk, 1.0, Some(1), false)),
        format!("{:?}", louvain_weighted(t, &w, None, 1.0, Some(1), true)),
        format!("{:?}", label_propagation(t, mk, 20, Some(1), false)),
        format!("{:?}", triangle_count(t, mk)),
        format!("{:?}", closeness_weighted(t, &w, mk)),
        format!("{:?}", betweenness(t, mk, Some(0.2), Some(7))),
        format!("{:?}", betweenness_weighted(t, &w, mk, Some(0.2), Some(7))),
        format!("{:?}", degree(t, mk, Direction::Both)),
        format!(
            "{:?}",
            neighbor_aggregate(
                t,
                &(0..t.n_nodes()).map(|v| Some(v as f64)).collect::<Vec<_>>(),
                mk,
                Direction::In,
                AggKind::Max
            )
        ),
        format!(
            "{:?}",
            random_walk_masked(t, mk, &[0, 1, 2], 10, 2, Some(3)).node
        ),
        format!("{:?}", induced_edges(t, &induced)),
        format!("{:?}", EdgeMask::induced(t, &nodes, mk).words()),
        format!("{:?} {:?}", wsym.out().targets, sw),
    ]
}

/// The graphs: unsorted rows (the ids must be read back) and sorted ones
/// (the identity), over enough rows for the parallel builds, in both node
/// orders.
fn graphs() -> Vec<(String, u32, (Vec<u32>, Vec<u32>), NodeOrder)> {
    let mut out = Vec::new();
    for (sorted, m) in [(false, 70_000), (true, 70_000), (false, 3_000)] {
        for order in [NodeOrder::AsGiven, NodeOrder::InDegree] {
            let n = 1_500;
            let name = format!("sorted={sorted} m={m} {order:?}");
            out.push((name, n, rows(n, m, 0x170 + m as u64, sorted), order));
        }
    }
    out
}

#[test]
fn late_ids_are_the_eager_ids() {
    for (name, n, rows, order) in graphs() {
        let eager = build(n, &rows, order, Ids::Eager);
        let eager_in = eager.incoming().edge_ids.detached();
        for &ids in LAZY {
            let lazy = build(n, &rows, order, ids);
            assert_eq!(lazy.out().targets, eager.out().targets, "{name} {ids:?}");
            assert_eq!(lazy.out().offsets, eager.out().offsets, "{name} {ids:?}");
            // The in-adjacency before the out-adjacency's ids are built ...
            assert_eq!(lazy.incoming().targets, eager.incoming().targets);
            assert!(!lazy.out().edge_ids.is_built(), "{name} {ids:?}");
            assert_eq!(lazy.incoming().edge_ids.detached(), eager_in, "{name}");
            assert!(!lazy.incoming().edge_ids.is_built(), "{name} {ids:?}");
            // ... and the ids themselves.
            assert_eq!(&lazy.out().edge_ids[..], &eager.out().edge_ids[..], "{name}");
            assert_eq!(&lazy.incoming().edge_ids[..], &eager_in[..], "{name}");
            // An in-adjacency transposed after the out-ids are held.
            let late = build(n, &rows, order, ids);
            let _ = late.out().edge_ids.get();
            assert_eq!(&late.incoming().edge_ids[..], &eager_in[..], "{name}");
        }
    }
}

#[test]
fn every_kernel_agrees_and_plain_kernels_build_no_ids() {
    for (name, n, rows, order) in graphs() {
        let eager = build(n, &rows, order, Ids::Eager);
        let (want_plain, want_rowed) = (plain(&eager), rowed(&eager));
        for &ids in LAZY {
            let lazy = build(n, &rows, order, ids);
            let got = plain(&lazy);
            assert!(
                !lazy.out().edge_ids.is_built() && !lazy.incoming().edge_ids.is_built(),
                "{name} {ids:?}: a kernel that reads no edge row built the edge ids"
            );
            assert_eq!(got, want_plain, "{name} {ids:?}");
            assert_eq!(rowed(&lazy), want_rowed, "{name} {ids:?}");
        }
    }
}

#[test]
fn sorted_rows_have_identity_ids_without_a_source() {
    let rows = rows(1_000, 50_000, 3, true);
    let mut asked = false;
    let t = Topology::build_with_rows(
        1_000,
        rows.0.clone(),
        rows.1.clone(),
        NodeOrder::AsGiven,
        &mut |_| {
            asked = true;
            None
        },
    );
    assert!(!asked, "grouped rows need no source");
    assert!(t.out().edge_ids.is_identity() && !t.out().edge_ids.is_built());
    assert!(t.out().edge_ids.iter().enumerate().all(|(k, &e)| k as u32 == e));
    // The default build of sorted rows is the same.
    let plain = Topology::build(1_000, rows.0, rows.1);
    assert!(plain.out().edge_ids.is_identity());
    assert_eq!(plain.out().edge_ids, t.out().edge_ids);
}
