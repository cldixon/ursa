//! Triangle count — adjacency-intersection kernel.
//!
//! Treats edges as undirected. Builds a symmetric, deduplicated, *sorted*
//! adjacency (out ∪ in neighbours, self-loops removed), then for each node counts
//! triangles via sorted-list intersection — a triangle through `u` is exactly an
//! edge between two of `u`'s neighbours.
//!
//! `result[u]` is the number of triangles containing node `u` (so each triangle
//! contributes to three entries; the whole-graph triangle total is `sum / 3`).
//! Parallelised over vertices with Rayon. Duplicate `(src, dst)` rows and
//! self-loops do not inflate counts (they are deduped / dropped when building the
//! undirected adjacency).
//!
//! The count uses the GAP-canonical degree ordering (`tc.cc`): edges are
//! oriented toward the higher-degree endpoint and only those "upward" lists are
//! intersected, so each triangle is found once (see [`per_node_triangles`]).

use crate::parallel::*;

use super::linked::Linked;
use crate::topology::{EdgeMask, Topology, UndirectedCsr};

/// The undirected adjacency a triangle-family kernel runs over: the topology's
/// cached full view, or a per-subgraph masked one built for this call.
///
/// Exposed (with [`undirected_view`]) so a caller running *both* triangle-family
/// kernels over the same graph can resolve the view once and hand it to each,
/// rather than each kernel resolving its own. Under a mask that matters twice
/// over: the masked view is rebuilt per call, never cached on the topology.
pub enum UndirectedView<'a> {
    /// The topology's cached full undirected adjacency (no mask).
    Cached(&'a UndirectedCsr),
    /// A per-subgraph view, built for one mask and owned by this value.
    Masked(UndirectedCsr),
}

impl UndirectedView<'_> {
    #[inline]
    pub fn get(&self) -> &UndirectedCsr {
        match self {
            UndirectedView::Cached(adj) => adj,
            UndirectedView::Masked(adj) => adj,
        }
    }
}

/// Resolve the undirected adjacency for `mask` — the topology's cached view when
/// unmasked, a freshly built subgraph view otherwise.
pub fn undirected_view<'a>(topo: &'a Topology, mask: Option<&EdgeMask>) -> UndirectedView<'a> {
    match mask {
        None => UndirectedView::Cached(topo.undirected()),
        Some(m) => UndirectedView::Masked(topo.undirected_masked(m)),
    }
}

/// Per-node triangle count over a resolved undirected adjacency.
///
/// Public so a caller that also wants the clustering coefficient can compute the
/// (expensive) intersection pass once and pass the counts to
/// [`super::clustering_from_triangles`]; `triangle_count` is the one-shot form.
///
/// Uses the degree-ordered *forward* algorithm: each undirected edge is oriented
/// from its lower-ranked to its higher-ranked endpoint, ranked by `(degree, id)`,
/// and each triangle is found exactly once, as the intersection of two oriented
/// lists. Orienting toward the higher degree keeps every oriented list short
/// (at most `O(√m)`), so a hub's long adjacency is never re-scanned per neighbour,
/// which is what made the plain node-iterator quadratic in hub degree. Each found
/// triangle credits all three corners. The counts are integers, so the parallel
/// atomic adds give the same result in any order and on any thread count.
///
/// A node of degree under 2 is in no triangle and is no common neighbour of
/// two nodes, so it adds nothing to the count. When enough nodes have degree
/// under 2 (isolated nodes, most often, #326), the pass runs over the others
/// only, numbered in node order, and fills in 0 for the rest.
pub fn per_node_triangles(adj: &UndirectedCsr) -> Vec<u32> {
    let n = adj.offsets.len().saturating_sub(1);
    let linked = Linked::from_fn(n, |u| adj.degree(u) >= 2);
    triangles_over(adj, linked.worth_it().then_some(&linked))
}

/// [`per_node_triangles`], over the nodes of `linked` only when given.
fn triangles_over(adj: &UndirectedCsr, linked: Option<&Linked>) -> Vec<u32> {
    use std::sync::atomic::{AtomicU32, Ordering};

    let n = linked.map_or(adj.offsets.len().saturating_sub(1), Linked::len);
    // The node of compact node `u`, the compact node of node `v`, and whether
    // node `v` is one of the compact nodes.
    let node = |u: u32| linked.map_or(u, |l| l.nodes()[u as usize]);
    let at = |v: u32| linked.map_or(v, |l| l.compact(v));
    let kept = |v: u32| linked.is_none_or(|l| l.contains(v));
    let ranks_above = |u: u32, v: u32| {
        let (du, dv) = (adj.degree(u), adj.degree(v));
        dv > du || (dv == du && v > u)
    };

    // The oriented adjacency: each node's higher-ranked neighbours, still sorted by
    // id (a filtered sorted list stays sorted), as a flat CSR. Its entries are
    // node ids; its rows are compact.
    let fwd_deg: Vec<u32> = (0..n as u32)
        .into_par_iter()
        .map(|u| {
            let u = node(u);
            adj.neighbors(u)
                .iter()
                .filter(|&&v| kept(v) && ranks_above(u, v))
                .count() as u32
        })
        .collect();
    let mut fwd_off = vec![0usize; n + 1];
    for u in 0..n {
        fwd_off[u + 1] = fwd_off[u] + fwd_deg[u] as usize;
    }
    drop(fwd_deg);
    let mut fwd = vec![0u32; fwd_off[n]];
    // Each node fills its own segment `fwd_off[u]..fwd_off[u + 1]`; the segments
    // are disjoint, so the parallel writes never overlap. The pointer crosses the
    // thread boundary as a plain address, as in the CSR build.
    let fwd_addr = fwd.as_mut_ptr() as usize;
    (0..n as u32).into_par_iter().for_each(|u| {
        let base = fwd_off[u as usize];
        let uo = node(u);
        let ups = adj
            .neighbors(uo)
            .iter()
            .filter(|&&v| kept(v) && ranks_above(uo, v));
        for (k, &v) in ups.enumerate() {
            // SAFETY: `base + k < fwd_off[u + 1]` (the segment holds exactly the
            // `fwd_deg[u]` neighbours this filter yields), no other node writes this
            // segment, and `fwd` outlives the loop.
            unsafe { *(fwd_addr as *mut u32).add(base + k) = v };
        }
    });
    // The higher-ranked neighbours of node `v`.
    let fwd_of = |v: u32| {
        let c = at(v) as usize;
        &fwd[fwd_off[c]..fwd_off[c + 1]]
    };

    let counts: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    (0..n as u32).into_par_iter().for_each(|u| {
        let nu = &fwd[fwd_off[u as usize]..fwd_off[u as usize + 1]];
        let mut found_u = 0u32;
        for &v in nu {
            // Common higher-ranked neighbours w of u and v close triangle {u, v, w}.
            let (a, b) = (nu, fwd_of(v));
            let (mut i, mut j, mut found_v) = (0usize, 0usize, 0u32);
            while i < a.len() && j < b.len() {
                match a[i].cmp(&b[j]) {
                    std::cmp::Ordering::Less => i += 1,
                    std::cmp::Ordering::Greater => j += 1,
                    std::cmp::Ordering::Equal => {
                        counts[at(a[i]) as usize].fetch_add(1, Ordering::Relaxed);
                        found_v += 1;
                        i += 1;
                        j += 1;
                    }
                }
            }
            if found_v > 0 {
                counts[at(v) as usize].fetch_add(found_v, Ordering::Relaxed);
                found_u += found_v;
            }
        }
        if found_u > 0 {
            counts[u as usize].fetch_add(found_u, Ordering::Relaxed);
        }
    });
    let counts: Vec<u32> = counts.into_iter().map(AtomicU32::into_inner).collect();
    match linked {
        None => counts,
        Some(l) => l.expand(&counts, |_| 0),
    }
}

/// Per-node triangle count, dense-indexed. A subgraph `mask` restricts triangles
/// to kept edges — computed over the masked undirected view (built per subgraph;
/// no CSR/id rebuild).
pub fn triangle_count(topo: &Topology, mask: Option<&EdgeMask>) -> Vec<u32> {
    if topo.n_nodes() == 0 {
        return Vec::new();
    }
    per_node_triangles(undirected_view(topo, mask).get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_brute_force_on_random_multigraphs() {
        // Skewed random graphs with hubs, self-loops and parallel/reciprocal edges:
        // the degree-ordered count must equal checking every node triple.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |bound: u32| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as u32
        };
        for &(n, m) in &[(12u32, 40usize), (30, 200), (60, 300)] {
            let (mut src, mut dst) = (Vec::new(), Vec::new());
            for _ in 0..m {
                let a = next(n).min(next(n)); // skew toward low ids -> hubs
                src.push(a);
                dst.push(next(n));
            }
            let t = Topology::build(n as usize, src, dst);
            let adj = t.undirected();
            let linked = |a: u32, b: u32| adj.neighbors(a).binary_search(&b).is_ok();
            let mut expect = vec![0u32; n as usize];
            for a in 0..n {
                for b in a + 1..n {
                    for c in b + 1..n {
                        if linked(a, b) && linked(b, c) && linked(a, c) {
                            for x in [a, b, c] {
                                expect[x as usize] += 1;
                            }
                        }
                    }
                }
            }
            assert_eq!(triangle_count(&t, None), expect, "n={n} m={m}");
        }
    }

    /// The compact pass (#326) gives the full pass's counts.
    #[test]
    fn compact_counts_are_the_full_counts() {
        use crate::algo::linked::testing::{sparse, CASES};
        for &(seed, n, m) in CASES {
            let (t, mask) = sparse(seed, n, m.max(3) * 3);
            for view in [undirected_view(&t, None), undirected_view(&t, Some(&mask))] {
                let adj = view.get();
                let linked = Linked::from_fn(n, |u| adj.degree(u) >= 2);
                let full = triangles_over(adj, None);
                assert_eq!(triangles_over(adj, Some(&linked)), full, "graph {seed}");
                assert!(n < 1000 || full.iter().any(|&c| c > 0));
            }
        }
    }

    #[test]
    fn single_triangle_counts_once_per_node() {
        // 0->1, 0->2, 1->2, 2->0  (undirected: the triangle 0-1-2)
        let t = Topology::build(3, vec![0, 0, 1, 2], vec![1, 2, 2, 0]);
        assert_eq!(triangle_count(&t, None), vec![1, 1, 1]);
    }

    #[test]
    fn path_has_no_triangles() {
        // 0->1->2 : no triangle
        let t = Topology::build(3, vec![0, 1], vec![1, 2]);
        assert_eq!(triangle_count(&t, None), vec![0, 0, 0]);
    }

    #[test]
    fn k4_gives_three_per_node() {
        // complete graph on 4 nodes: every node is in C(3,2) = 3 triangles
        let src = vec![0, 0, 0, 1, 1, 2];
        let dst = vec![1, 2, 3, 2, 3, 3];
        let t = Topology::build(4, src, dst);
        assert_eq!(triangle_count(&t, None), vec![3, 3, 3, 3]);
    }

    #[test]
    fn ignores_self_loops_and_parallel_edges() {
        // triangle 0-1-2 plus a self-loop on 0 and a duplicated 0->1 edge
        let src = vec![0, 0, 1, 2, 0, 0];
        let dst = vec![1, 2, 2, 0, 0, 1];
        let t = Topology::build(3, src, dst);
        assert_eq!(triangle_count(&t, None), vec![1, 1, 1]);
    }
}
