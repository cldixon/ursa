//! A locality relabel of the nodes round-trips (#172): every kernel's result,
//! read through the original ids, is the one an unrelabelled build gives.
//!
//! What must not change: component partitions, counts (degrees, triangles,
//! distances, reached sets), sampled sources, and the user ids an [`IdMap`]
//! gives each node. What may change: the bits of a float whose sum runs in
//! node order (PageRank, betweenness), checked within a tolerance here, and
//! the communities of the seeded community kernels, whose visit order follows
//! the numbering: checked to be deterministic and as good (modularity).

use ursa_core::algo::{
    betweenness, bfs_distances, closeness_directed, connected_components_strong,
    connected_components_weak, degree, k_hop_distances_masked, label_propagation, louvain,
    louvain_weighted, pagerank, triangle_count, PageRankParams,
};
use ursa_core::catalog::stats::{avg_path_length, diameter};
use ursa_core::{Direction, EdgeMask, IdMap, NodeOrder, Topology};

/// A skewed pseudo-random graph (endpoints drawn towards low ids, so the
/// in-degrees differ and the relabel moves nodes), plus some isolated nodes,
/// self-loops and parallel edges: `(n, src, dst)` in first-seen-like ids.
fn graph(n: u32, m: usize, seed: u64) -> (usize, Vec<u32>, Vec<u32>) {
    let mut state = seed;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u32
    };
    let mut skewed = move || {
        let a = next() % n;
        let b = next() % n;
        let c = next() % n;
        a.min(b).min(c) ^ 1
    };
    let (mut src, mut dst) = (Vec::with_capacity(m), Vec::with_capacity(m));
    for i in 0..m {
        let s = skewed() % (n - 5);
        let d = if i.is_multiple_of(97) {
            s
        } else {
            skewed() % (n - 5)
        };
        src.push(s);
        dst.push(d);
    }
    // The last five nodes are on no edge.
    (n as usize, src, dst)
}

/// The two builds of one edge list.
fn builds(n: usize, src: &[u32], dst: &[u32]) -> (Topology, Topology) {
    let plain = Topology::build(n, src.to_vec(), dst.to_vec());
    let relabelled = Topology::build_ordered(n, src.to_vec(), dst.to_vec(), NodeOrder::InDegree);
    assert!(
        relabelled.original_ids().is_some(),
        "the graph is relabelled"
    );
    (plain, relabelled)
}

/// Per-node values of `relabelled`, read in original-id order.
fn back<T: Copy>(relabelled: &Topology, values: &[T]) -> Vec<T> {
    (0..relabelled.n_nodes() as u32)
        .map(|o| values[relabelled.node_of_original(o) as usize])
        .collect()
}

fn close(a: &[f64], b: &[f64], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}");
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let tol = 1e-12 * x.abs().max(y.abs()).max(1e-300);
        assert!((x - y).abs() <= tol, "{what}: node {i}: {x} vs {y}");
    }
}

/// Modularity (resolution 1) of `labels` over the kept edges of `t`, read
/// as undirected (each row once, both ends).
fn modularity(t: &Topology, mask: Option<&EdgeMask>, labels: &[u32]) -> f64 {
    let out = t.out();
    let mut sums = std::collections::HashMap::new();
    let (mut intra, mut m2) = (0.0f64, 0.0f64);
    for u in 0..t.n_nodes() as u32 {
        for (&v, &e) in out.neighbors(u).iter().zip(out.edge_ids(u)) {
            if mask.is_some_and(|m| !m.keep(e)) {
                continue;
            }
            let (a, b) = (labels[u as usize], labels[v as usize]);
            *sums.entry(a).or_insert(0.0) += 1.0;
            *sums.entry(b).or_insert(0.0) += 1.0;
            if a == b {
                intra += 2.0;
            }
            m2 += 2.0;
        }
    }
    if m2 == 0.0 {
        return 0.0;
    }
    intra / m2
        - sums
            .values()
            .map(|s: &f64| (s / m2) * (s / m2))
            .sum::<f64>()
}

/// A community kernel `run` on the relabelled topology: the same on a second
/// run (it is seeded), a label per node, and (on 1,000 edges or more) a
/// modularity within 0.05 of the first-seen build's (#172: the visit order
/// follows the numbering, so the communities may differ, not their quality).
fn comparable(
    plain: &Topology,
    relabelled: &Topology,
    mask: Option<&EdgeMask>,
    run: impl Fn(&Topology) -> Vec<u32>,
    what: &str,
) {
    let (a, b) = (run(relabelled), run(relabelled));
    assert_eq!(a, b, "{what}: deterministic");
    assert_eq!(a.len(), relabelled.n_nodes(), "{what}");
    let (q_plain, q) = (
        modularity(plain, mask, &run(plain)),
        modularity(relabelled, mask, &a),
    );
    // On a graph of a few dozen edges one move is a large share of the
    // modularity; the tolerance is for graphs of some size.
    if relabelled.n_edges() >= 1000 {
        assert!(
            (q - q_plain).abs() <= 0.05,
            "{what}: modularity {q} vs {q_plain}"
        );
    }
}

/// The fused in-degree build is the counting sort over the renamed endpoints.
#[test]
fn the_fused_build_is_the_renamed_build() {
    // The last is past 4,194,304 edges, where the in-degrees are sampled.
    for (seed, n, m) in [
        (7, 500, 4000),
        (8, 20_000, 300_000),
        (9, 200_000, 4_500_000),
    ] {
        let (n, src, dst) = graph(n, m, seed);
        let fused = Topology::build_ordered(n, src.clone(), dst.clone(), NodeOrder::InDegree);
        let original = fused.original_ids().unwrap().to_vec();
        // Serial (a build on a pool's worker is) and parallel alike.
        #[cfg(feature = "rayon")]
        {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build();
            let serial = pool.unwrap().install(|| {
                Topology::build_ordered(n, src.clone(), dst.clone(), NodeOrder::InDegree)
            });
            assert_eq!(serial.original_ids(), Some(&original[..]));
            assert_eq!(serial.out().targets, fused.out().targets);
        }
        let reference = Topology::build_relabelled(n, src, dst, original);
        assert_eq!(fused.out().offsets, reference.out().offsets);
        assert_eq!(fused.out().targets, reference.out().targets);
        assert_eq!(fused.out().edge_ids, reference.out().edge_ids);
    }
}

#[test]
fn the_relabel_is_a_permutation_by_in_degree() {
    let (n, src, dst) = graph(500, 4000, 1);
    let (plain, relabelled) = builds(n, &src, &dst);
    let original = relabelled.original_ids().unwrap();
    let mut sorted = original.to_vec();
    sorted.sort_unstable();
    assert_eq!(sorted, (0..n as u32).collect::<Vec<_>>());
    let indeg = degree(&plain, None, Direction::In);
    for w in original.windows(2) {
        let (a, b) = (indeg[w[0] as usize], indeg[w[1] as usize]);
        assert!(
            a > b || (a == b && w[0] < w[1]),
            "in-degree order, ties by id"
        );
    }
    for v in 0..n as u32 {
        assert_eq!(relabelled.node_of_original(relabelled.original_id(v)), v);
    }
    // Rebuilt in the original numbering, it is the unrelabelled build.
    let again = relabelled.in_original_order().unwrap();
    assert_eq!(again.out().offsets, plain.out().offsets);
    assert_eq!(again.out().targets, plain.out().targets);
    assert_eq!(again.out().edge_ids, plain.out().edge_ids);
    assert!(plain.in_original_order().is_none());
    // With its in-adjacency built, that is renumbered too: the transpose.
    relabelled.incoming();
    let again = relabelled.in_original_order().unwrap();
    let inc = again.incoming_if_built().expect("renumbered, not rebuilt");
    assert_eq!(inc.offsets, plain.incoming().offsets);
    assert_eq!(inc.targets, plain.incoming().targets);
    assert_eq!(inc.edge_ids, plain.incoming().edge_ids);
    // The edge rows are the same edges.
    let out = relabelled.out();
    for u in 0..n as u32 {
        for (&v, &e) in out.neighbors(u).iter().zip(out.edge_ids(u)) {
            let e = e as usize;
            assert_eq!(relabelled.original_id(u), src[e]);
            assert_eq!(relabelled.original_id(v), dst[e]);
        }
    }
}

#[test]
fn counts_and_partitions_are_the_same() {
    for (seed, n, m) in [(1, 500, 4000), (2, 3000, 70_000), (3, 40, 30)] {
        let (n, src, dst) = graph(n, m, seed);
        let (plain, relabelled) = builds(n, &src, &dst);
        for dir in [Direction::Out, Direction::In, Direction::Both] {
            assert_eq!(
                degree(&plain, None, dir),
                back(&relabelled, &degree(&relabelled, None, dir))
            );
        }
        assert_eq!(
            triangle_count(&plain, None),
            back(&relabelled, &triangle_count(&relabelled, None))
        );
        // Canonical component labels: the same values, not only the same
        // partition.
        assert_eq!(
            connected_components_weak(&plain, None),
            back(&relabelled, &connected_components_weak(&relabelled, None)),
            "weak components"
        );
        assert_eq!(
            connected_components_strong(&plain, None),
            back(&relabelled, &connected_components_strong(&relabelled, None)),
            "strong components"
        );
        // The seeded community kernels follow the node numbering (#172):
        // other communities, as good.
        for parallel in [false, true] {
            comparable(
                &plain,
                &relabelled,
                None,
                |t| louvain(t, None, 1.0, Some(3), parallel),
                &format!("louvain (parallel: {parallel})"),
            );
            comparable(
                &plain,
                &relabelled,
                None,
                |t| label_propagation(t, None, 20, Some(3), parallel),
                &format!("label propagation (parallel: {parallel})"),
            );
        }
        let weights: Vec<f64> = (0..m).map(|i| 0.5 + (i % 7) as f64).collect();
        comparable(
            &plain,
            &relabelled,
            None,
            |t| louvain_weighted(t, &weights, None, 1.0, None, false),
            "weighted louvain",
        );
        // Distances, reached sets and sampled statistics.
        let hub = 0u32;
        for dir in [Direction::Out, Direction::Both] {
            assert_eq!(
                bfs_distances(&plain, hub, dir),
                back(
                    &relabelled,
                    &bfs_distances(&relabelled, relabelled.node_of_original(hub), dir)
                )
            );
            let mut a: Vec<(u32, u32)> = {
                let (_, r, d) = k_hop_distances_masked(&plain, None, &[hub], 2, dir);
                r.into_iter().zip(d).collect()
            };
            let mut b: Vec<(u32, u32)> = {
                let seed = relabelled.node_of_original(hub);
                let (_, r, d) = k_hop_distances_masked(&relabelled, None, &[seed], 2, dir);
                r.into_iter()
                    .map(|v| relabelled.original_id(v))
                    .zip(d)
                    .collect()
            };
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(a, b, "hop");
            assert_eq!(
                avg_path_length(&plain, None, Some(0.1), dir),
                avg_path_length(&relabelled, None, Some(0.1), dir)
            );
            assert_eq!(
                diameter(&plain, None, true, dir),
                diameter(&relabelled, None, true, dir)
            );
        }
        // Floats summed in node order: the same within a tolerance.
        close(
            &pagerank(&plain, None, PageRankParams::default()),
            &back(
                &relabelled,
                &pagerank(&relabelled, None, PageRankParams::default()),
            ),
            "pagerank",
        );
        close(
            &betweenness(&plain, None, Some(0.2), Some(5)),
            &back(
                &relabelled,
                &betweenness(&relabelled, None, Some(0.2), Some(5)),
            ),
            "sampled betweenness",
        );
        close(
            &closeness_directed(&plain, None, Direction::Out),
            &back(
                &relabelled,
                &closeness_directed(&relabelled, None, Direction::Out),
            ),
            "closeness",
        );
    }
}

#[test]
fn a_subgraph_mask_applies_unchanged() {
    let (n, src, dst) = graph(800, 9000, 4);
    let (plain, relabelled) = builds(n, &src, &dst);
    let keep: Vec<bool> = (0..src.len())
        .map(|e: usize| !e.is_multiple_of(3))
        .collect();
    let mask = EdgeMask::from_bools(&keep);
    assert_eq!(
        connected_components_weak(&plain, Some(&mask)),
        back(
            &relabelled,
            &connected_components_weak(&relabelled, Some(&mask))
        )
    );
    comparable(
        &plain,
        &relabelled,
        Some(&mask),
        |t| louvain(t, Some(&mask), 1.0, Some(1), false),
        "masked louvain",
    );
    comparable(
        &plain,
        &relabelled,
        Some(&mask),
        |t| label_propagation(t, Some(&mask), 10, Some(1), true),
        "masked label propagation",
    );
}

#[test]
fn the_id_map_follows_the_relabel() {
    let (n, src, dst) = graph(300, 2000, 5);
    let (_, relabelled) = builds(n, &src, &dst);
    let ids = IdMap::from_ids((0..n as i64).map(|i| 1000 + 7 * i));
    let reordered = ids.reordered(relabelled.original_ids().unwrap());
    let before = ids.user_id_array();
    let after = reordered.user_id_array();
    let (before, after) = (
        before
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap(),
        after
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap(),
    );
    for v in 0..n as u32 {
        assert_eq!(
            after.value(v as usize),
            before.value(relabelled.original_id(v) as usize)
        );
    }
    // A lookup finds each user id's new node.
    let found = reordered.dense_from_array(before).unwrap();
    for (o, d) in found.into_iter().enumerate() {
        assert_eq!(d, Some(relabelled.node_of_original(o as u32)));
    }
}

#[test]
fn a_stored_order_is_checked() {
    let (n, src, dst) = graph(50, 200, 6);
    let (plain, relabelled) = builds(n, &src, &dst);
    let parts = || Topology::from_parts(n, relabelled.out().clone(), None).unwrap();
    let original = relabelled.original_ids().unwrap().to_vec();
    let back_again = parts().with_original_ids(original.clone().into()).unwrap();
    assert_eq!(back_again.original_ids(), Some(&original[..]));
    let mut twice = original.clone();
    twice[1] = twice[0];
    assert!(parts().with_original_ids(twice.into()).is_err());
    assert!(parts()
        .with_original_ids(original[1..].to_vec().into())
        .is_err());
    // The identity is no relabel.
    let identity = Topology::from_parts(n, plain.out().clone(), None)
        .unwrap()
        .with_original_ids((0..n as u32).collect::<Vec<_>>().into())
        .unwrap();
    assert!(identity.original_ids().is_none());
}
