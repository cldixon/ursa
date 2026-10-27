//! The undirected view as a symmetric directed topology — the `direction="both"`
//! input for the directed kernels (`pagerank`, `betweenness`).
//!
//! Edges are stored directed. To run a directed kernel "as if undirected" the
//! graph is re-expressed as its **simple undirected view**: every unordered pair
//! `{u, v}` (`u != v`) joined by at least one row in either direction becomes one
//! undirected edge, emitted as the two directed rows `u -> v` and `v -> u`.
//! Self-loops are dropped, and parallel or reciprocal rows collapse to the one
//! pair. That is exactly the [`Topology::undirected`] adjacency the triangle
//! family already uses (and caches), turned back into a [`Topology`] so the
//! unchanged directed kernels can run over it. Dense node ids are preserved: node
//! `i` of the view is node `i` of the source topology, including isolated nodes.
//!
//! The unmasked unweighted view is cached on the topology
//! ([`Topology::symmetric`]); a masked or weighted view is built per call. Either
//! way it is taken straight from the sorted undirected CSR, with no counting sort,
//! and is its own transpose (see [`Topology::from_undirected`]).

use crate::parallel::*;

use super::triangle::{undirected_view, UndirectedView};
use crate::topology::{EdgeMask, Topology};

/// The unweighted symmetric view of `topo` (honoring `mask`): one `u -> v` and one
/// `v -> u` row per distinct unordered pair, self-loops dropped. Row order is by
/// source then target, so the result is deterministic.
///
/// Unmasked, prefer the cached [`Topology::symmetric`]; this copies it.
pub fn symmetric_view(topo: &Topology, mask: Option<&EdgeMask>) -> Topology {
    let n = topo.n_nodes();
    match undirected_view(topo, mask) {
        UndirectedView::Cached(adj) => {
            Topology::from_undirected(n, adj.offsets.clone(), adj.targets.clone())
        }
        UndirectedView::Masked(adj) => Topology::from_undirected(n, adj.offsets, adj.targets),
    }
}

/// The weighted symmetric view: as [`symmetric_view`], plus the per-row weight of
/// the returned topology. `weights[e]` is the weight of source row `e`. When
/// several rows join the same pair (parallel edges, or a `u -> v` matched by a
/// `v -> u`), the undirected edge takes the **minimum** of their weights — the
/// cheapest way across, which is what a shortest-path kernel would pick anyway.
/// Both directed rows of a pair carry that same weight.
///
/// Built flat, as [`Topology::undirected`] is: each node gets an upper-bound
/// segment of `out_deg + in_deg` slots, sorts and deduplicates its neighbours into
/// it (parallel, disjoint segments), and one serial pass slides the kept prefixes
/// left.
pub fn symmetric_view_weighted(
    topo: &Topology,
    weights: &[f64],
    mask: Option<&EdgeMask>,
) -> (Topology, Vec<f64>) {
    assert_eq!(
        weights.len(),
        topo.n_edges(),
        "weights length must equal the edge count"
    );
    let n = topo.n_nodes();
    let out = topo.out();
    // Build the transpose before the parallel loop rather than lazily inside it.
    let inc = topo.incoming();

    let mut bound = vec![0u64; n + 1];
    for u in 0..n {
        let d = out.degree(u as u32) as u64 + inc.degree(u as u32) as u64;
        bound[u + 1] = bound[u] + d;
    }
    let cap = bound[n] as usize;
    let mut targets = vec![0u32; cap];
    let mut wts = vec![0f64; cap];
    let (t_addr, w_addr) = (targets.as_mut_ptr() as usize, wts.as_mut_ptr() as usize);
    let lens: Vec<u32> = (0..n as u32)
        .into_par_iter()
        .map_init(Vec::new, |nb: &mut Vec<(u32, f64)>, u| {
            nb.clear();
            for adj in [out, inc] {
                for (&w, &e) in adj.neighbors(u).iter().zip(adj.edge_ids(u)) {
                    if w != u && mask.is_none_or(|m| m.keep(e)) {
                        nb.push((w, weights[e as usize]));
                    }
                }
            }
            // Sort by neighbour, then weight, so the first entry of each run is the
            // pair's minimum weight; keep only that one.
            nb.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
            nb.dedup_by_key(|p| p.0);
            let start = bound[u as usize] as usize;
            // SAFETY: segments `bound[u]..bound[u + 1]` are disjoint across nodes,
            // `nb` holds at most `out_deg + in_deg` entries so it fits in its
            // segment, and both buffers outlive the loop.
            unsafe {
                let t = (t_addr as *mut u32).add(start);
                let w = (w_addr as *mut f64).add(start);
                for (i, &(v, x)) in nb.iter().enumerate() {
                    *t.add(i) = v;
                    *w.add(i) = x;
                }
            }
            nb.len() as u32
        })
        .collect();

    // Compact: each segment's kept prefix moves left (never right), in order.
    let mut offsets = vec![0u64; n + 1];
    let mut write = 0usize;
    for u in 0..n {
        let (start, len) = (bound[u] as usize, lens[u] as usize);
        targets.copy_within(start..start + len, write);
        wts.copy_within(start..start + len, write);
        write += len;
        offsets[u + 1] = write as u64;
    }
    targets.truncate(write);
    targets.shrink_to_fit();
    wts.truncate(write);
    wts.shrink_to_fit();
    (Topology::from_undirected(n, offsets, targets), wts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(t: &Topology) -> Vec<(u32, u32)> {
        let mut r = Vec::new();
        for u in 0..t.n_nodes() as u32 {
            for &v in t.out().neighbors(u) {
                r.push((u, v));
            }
        }
        r.sort_unstable();
        r
    }

    /// Today's reference: the rows `u -> v` of the undirected CSR, re-sorted by a
    /// full `Topology::build`, and the weighted per-node `Vec` build.
    fn reference(t: &Topology, mask: Option<&EdgeMask>) -> Topology {
        let view = undirected_view(t, mask);
        let adj = view.get();
        let mut src = Vec::new();
        for u in 0..t.n_nodes() as u32 {
            src.extend(std::iter::repeat_n(u, adj.degree(u)));
        }
        Topology::build(t.n_nodes(), src, adj.targets.clone())
    }

    fn reference_weighted(
        t: &Topology,
        weights: &[f64],
        mask: Option<&EdgeMask>,
    ) -> (Topology, Vec<f64>) {
        let (mut src, mut dst, mut wts) = (Vec::new(), Vec::new(), Vec::new());
        for u in 0..t.n_nodes() as u32 {
            let mut nb: Vec<(u32, f64)> = Vec::new();
            for adj in [t.out(), t.incoming()] {
                for (&w, &e) in adj.neighbors(u).iter().zip(adj.edge_ids(u)) {
                    if w != u && mask.is_none_or(|m| m.keep(e)) {
                        nb.push((w, weights[e as usize]));
                    }
                }
            }
            nb.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
            nb.dedup_by_key(|p| p.0);
            for (v, w) in nb {
                src.push(u);
                dst.push(v);
                wts.push(w);
            }
        }
        (Topology::build(t.n_nodes(), src, dst), wts)
    }

    /// A random multigraph with self-loops, parallel and reciprocal rows, and
    /// isolated nodes; big enough for the parallel CSR builds.
    fn random_graph(n: u32, m: u32) -> (Topology, Vec<f64>, EdgeMask) {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut src = Vec::new();
        let mut dst = Vec::new();
        for _ in 0..m {
            src.push((next() % n as u64) as u32);
            dst.push((next() % n as u64) as u32);
        }
        let weights: Vec<f64> = (0..m).map(|_| (next() % 7) as f64 * 0.5).collect();
        let keep: Vec<bool> = (0..m).map(|_| next() % 3 != 0).collect();
        // Leave the top ids out so some nodes are isolated.
        (
            Topology::build(n as usize + 5, src, dst),
            weights,
            EdgeMask::from_bools(&keep),
        )
    }

    fn assert_same_csr(a: &crate::topology::Adjacency, b: &crate::topology::Adjacency) {
        assert_eq!(a.offsets, b.offsets);
        assert_eq!(a.targets, b.targets);
        assert_eq!(a.edge_ids, b.edge_ids);
    }

    #[test]
    fn direct_views_match_the_rebuilt_reference() {
        for (n, m) in [(1u32, 3u32), (40, 300), (3_000, 200_000)] {
            let (t, w, mask) = random_graph(n, m);
            for mask in [None, Some(&mask)] {
                let want = reference(&t, mask);
                let got = symmetric_view(&t, mask);
                assert_eq!(got.n_edges(), want.n_edges());
                assert_same_csr(got.out(), want.out());
                // The view is its own transpose: same offsets and targets as the
                // built one; each in-slot names the mirror row of a pair.
                let (gi, wi) = (got.incoming(), want.incoming());
                assert_eq!(gi.offsets, wi.offsets);
                assert_eq!(gi.targets, wi.targets);

                let (want, want_w) = reference_weighted(&t, &w, mask);
                let (got, got_w) = symmetric_view_weighted(&t, &w, mask);
                assert_same_csr(got.out(), want.out());
                assert_eq!(got_w, want_w);
                // Weights gathered through the transpose are the same either way.
                let (gi, wi) = (got.incoming(), want.incoming());
                assert_eq!(gi.targets, wi.targets);
                let gather = |ids: &[u32], ws: &[f64]| -> Vec<u64> {
                    ids.iter().map(|&e| ws[e as usize].to_bits()).collect()
                };
                assert_eq!(gather(&gi.edge_ids, &got_w), gather(&wi.edge_ids, &want_w));
            }
            // The cached view equals the per-call one.
            let cached = t.symmetric();
            assert!(std::ptr::eq(cached, t.symmetric()));
            assert_same_csr(cached.out(), reference(&t, None).out());
        }
    }

    #[test]
    fn collapses_reciprocal_parallel_and_self_loops() {
        // 0->1, 1->0 (reciprocal), 1->2 twice (parallel), 2->2 (self-loop), node 3
        // isolated only as a self-loop.
        let t = Topology::build(4, vec![0, 1, 1, 1, 2, 3], vec![1, 0, 2, 2, 2, 3]);
        let s = symmetric_view(&t, None);
        assert_eq!(s.n_nodes(), 4);
        assert_eq!(rows(&s), vec![(0, 1), (1, 0), (1, 2), (2, 1)]);
    }

    #[test]
    fn weighted_view_takes_the_minimum_weight_per_pair() {
        // 0->1 (5), 1->0 (2), 1->2 (3), 1->2 (7)
        let t = Topology::build(3, vec![0, 1, 1, 1], vec![1, 0, 2, 2]);
        let (s, w) = symmetric_view_weighted(&t, &[5.0, 2.0, 3.0, 7.0], None);
        let mut got: Vec<(u32, u32, f64)> = Vec::new();
        for u in 0..s.n_nodes() as u32 {
            for (&v, &e) in s.out().neighbors(u).iter().zip(s.out().edge_ids(u)) {
                got.push((u, v, w[e as usize]));
            }
        }
        got.sort_by_key(|a| (a.0, a.1));
        assert_eq!(
            got,
            vec![(0, 1, 2.0), (1, 0, 2.0), (1, 2, 3.0), (2, 1, 3.0)]
        );
    }

    #[test]
    fn mask_restricts_the_view() {
        use crate::topology::EdgeMask;
        let t = Topology::build(3, vec![0, 1], vec![1, 2]);
        let mask = EdgeMask::from_bools(&[true, false]);
        assert_eq!(rows(&symmetric_view(&t, Some(&mask))), vec![(0, 1), (1, 0)]);
    }
}
