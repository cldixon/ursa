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
//! The lists are stored by rank in that ordering, a long list is intersected
//! through a per-thread position table, and each node's credits are added once
//! per list, not once per triangle (#420).

use crate::parallel::*;

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
/// and each triangle is found exactly once, at its lowest-ranked corner `u`, as a
/// common entry of the oriented lists of `u` and of its middle corner `v`.
/// Orienting toward the higher degree keeps every oriented list short (at most
/// `O(√m)`), so a hub's long adjacency is never re-scanned per neighbour, which
/// is what made the plain node-iterator quadratic in hub degree. Each found
/// triangle credits all three corners. The counts are integers and any total
/// order of the nodes finds the same triangles, so the result is the same in any
/// order and on any thread count.
///
/// The nodes are numbered by rank and the lists hold ranks, sorted
/// ([`Forward`]), so the corners above `v` in `u`'s list are the entries after
/// `v`, and only those are read (#420). A node of degree under 2 is in no
/// triangle and is no common neighbour of two nodes, so it has no rank and no
/// list: the pass runs over the other nodes only, and its working memory does
/// not grow with the nodes that have no edge (#326).
pub fn per_node_triangles(adj: &UndirectedCsr) -> Vec<u32> {
    use std::sync::atomic::{AtomicU32, Ordering};

    let n = adj.offsets.len().saturating_sub(1);
    let fwd = Forward::build(adj);
    let n_ranked = fwd.order.len();
    // No list from here down is long enough to be marked, so the position
    // tables start at this rank.
    let base = (0..n_ranked)
        .find(|&u| fwd.list(u as u32).len() >= MARK_MIN)
        .unwrap_or(n_ranked);

    let counts: Vec<AtomicU32> = (0..n_ranked).map(|_| AtomicU32::new(0)).collect();
    // One scratch per block being counted, so at most one per thread.
    let pool = std::sync::Mutex::new(Vec::<Scratch>::new());
    (0..n_ranked.div_ceil(BLOCK)).into_par_iter().for_each(|b| {
        let mut scratch = pool.lock().unwrap().pop().unwrap_or_default();
        let Scratch { credit, pos } = &mut scratch;
        for u in b * BLOCK..((b + 1) * BLOCK).min(n_ranked) {
            let nu = fwd.list(u as u32);
            // The lowest corner of a triangle has the two others in its list.
            if nu.len() < 2 {
                continue;
            }
            // credit[i]: the triangles found here that have nu[i] as a corner.
            credit.clear();
            credit.resize(nu.len(), 0);
            let mark = nu.len() >= MARK_MIN;
            if mark {
                if pos.is_empty() {
                    pos.resize(n_ranked - base, 0);
                }
                for (i, &w) in nu.iter().enumerate() {
                    pos[w as usize - base] = i as u32 + 1;
                }
            }
            let last = nu[nu.len() - 1];
            let mut found_u = 0u32;
            for k in 0..nu.len() - 1 {
                // A common entry w of the lists of u and v closes the triangle
                // {u, v, w}. The list of v holds ranks above v only, so w is
                // among the entries of u's list after v.
                let (above, nv) = (&nu[k + 1..], fwd.list(nu[k]));
                let found_v = if mark && nv.len() < MARK_RATIO * above.len() {
                    let mut found = 0u32;
                    for &w in nv {
                        if w > last {
                            break;
                        }
                        let p = pos[w as usize - base];
                        if p != 0 {
                            let c = &mut credit[p as usize - 1];
                            *c = c.wrapping_add(1);
                            found += 1;
                        }
                    }
                    found
                } else {
                    intersect(above, nv, &mut credit[k + 1..])
                };
                credit[k] = credit[k].wrapping_add(found_v);
                found_u = found_u.wrapping_add(found_v);
            }
            if mark {
                for &w in nu {
                    pos[w as usize - base] = 0;
                }
            }
            // One add per corner, not one per triangle: the corners above u
            // are mostly hubs, which every thread would otherwise write to
            // all the time.
            if found_u > 0 {
                counts[u].fetch_add(found_u, Ordering::Relaxed);
                for (&w, &c) in nu.iter().zip(credit.iter()) {
                    if c > 0 {
                        counts[w as usize].fetch_add(c, Ordering::Relaxed);
                    }
                }
            }
        }
        pool.lock().unwrap().push(scratch);
    });

    let mut result = vec![0u32; n];
    for (&v, c) in fwd.order.iter().zip(counts) {
        result[v as usize] = c.into_inner();
    }
    result
}

/// A list at least this long is marked in the thread's position table while its
/// node is the lowest corner, and the lists it meets are looked up in the table
/// instead of merged with it.
const MARK_MIN: usize = 32;

/// A list is looked up in the marks unless it is this many times longer than
/// what is left of the marked list; then the shorter is searched for in it.
const MARK_RATIO: usize = 4;

/// Of two sorted lists, one at least this many times longer than the other is
/// searched for the other's entries; closer lengths are merged.
const GALLOP_RATIO: usize = 8;

/// How many nodes, consecutive in rank, one task counts.
const BLOCK: usize = 256;

/// A thread's working memory for the counting pass.
#[derive(Default)]
struct Scratch {
    /// The triangles found at the current lowest corner, per entry of its list.
    credit: Vec<u32>,
    /// For rank `base + i`, at index `i`: its position in the marked list plus
    /// one, or 0. All 0 between nodes. Empty until a list is first marked:
    /// 4 bytes per ranked node from `base` up, per thread.
    pos: Vec<u32>,
}

/// The oriented adjacency in rank order.
///
/// The nodes of degree 2 or more, ordered by `(degree, id)`, are numbered by
/// their place in that order, their rank. Each has the list of its neighbours
/// of higher rank, as ranks, ascending.
struct Forward {
    /// The node of each rank.
    order: Vec<u32>,
    /// The list of rank `r` is `lists[offsets[r]..offsets[r + 1]]`.
    offsets: Vec<usize>,
    lists: Vec<u32>,
}

impl Forward {
    fn build(adj: &UndirectedCsr) -> Forward {
        let n = adj.offsets.len().saturating_sub(1);
        // A counting sort by degree, stable, so nodes of equal degree stay in
        // id order. `rank1[v]` is the rank of node `v` plus one, or 0 when it
        // has none.
        let mut start: Vec<usize> = Vec::new();
        for v in 0..n as u32 {
            let d = adj.degree(v);
            if d >= 2 {
                if start.len() <= d {
                    start.resize(d + 1, 0);
                }
                start[d] += 1;
            }
        }
        let mut n_ranked = 0;
        for s in &mut start {
            n_ranked += std::mem::replace(s, n_ranked);
        }
        let mut order = vec![0u32; n_ranked];
        let mut rank1 = vec![0u32; n];
        for v in 0..n as u32 {
            let d = adj.degree(v);
            if d >= 2 {
                order[start[d]] = v;
                start[d] += 1;
                rank1[v as usize] = start[d] as u32;
            }
        }
        drop(start);

        // The ranks above rank `r` among the neighbours of its node.
        let above = |r: u32| {
            adj.neighbors(order[r as usize])
                .iter()
                .map(|&v| rank1[v as usize])
                .filter(move |&r1| r1 > r + 1)
                .map(|r1| r1 - 1)
        };
        let lens: Vec<u32> = (0..n_ranked as u32)
            .into_par_iter()
            .map(|r| above(r).count() as u32)
            .collect();
        let mut offsets = vec![0usize; n_ranked + 1];
        for r in 0..n_ranked {
            offsets[r + 1] = offsets[r] + lens[r] as usize;
        }
        drop(lens);
        let mut lists = vec![0u32; offsets[n_ranked]];
        // Each rank fills and sorts its own segment `offsets[r]..offsets[r + 1]`;
        // the segments are disjoint, so the parallel writes never overlap. The
        // pointer crosses the thread boundary as a plain address, as in the CSR
        // build.
        let lists_addr = lists.as_mut_ptr() as usize;
        (0..n_ranked as u32).into_par_iter().for_each(|r| {
            let (lo, hi) = (offsets[r as usize], offsets[r as usize + 1]);
            // SAFETY: `lo..hi` is within `lists`, which outlives the loop, and
            // no other rank reads or writes this segment.
            let list = unsafe {
                std::slice::from_raw_parts_mut((lists_addr as *mut u32).add(lo), hi - lo)
            };
            // `above` yields exactly the `lens[r]` ranks counted before.
            for (slot, w) in list.iter_mut().zip(above(r)) {
                *slot = w;
            }
            list.sort_unstable();
        });
        Forward {
            order,
            offsets,
            lists,
        }
    }

    /// The ranks above rank `r` that it has an edge to, ascending.
    #[inline]
    fn list(&self, r: u32) -> &[u32] {
        &self.lists[self.offsets[r as usize]..self.offsets[r as usize + 1]]
    }
}

/// How many values are in both `a` and `b`, each ascending and distinct; adds 1
/// to `credit[i]` for each `a[i]` that is.
#[inline]
fn intersect(a: &[u32], b: &[u32], credit: &mut [u32]) -> u32 {
    let mut found = 0u32;
    let mut hit = |i: usize| {
        credit[i] = credit[i].wrapping_add(1);
        found += 1;
    };
    let (mut i, mut j) = (0usize, 0usize);
    if a.len() * GALLOP_RATIO <= b.len() {
        while i < a.len() {
            j = lower_bound(b, j, a[i]);
            if j == b.len() {
                break;
            }
            if b[j] == a[i] {
                hit(i);
                j += 1;
            }
            i += 1;
        }
    } else if b.len() * GALLOP_RATIO <= a.len() {
        while j < b.len() {
            i = lower_bound(a, i, b[j]);
            if i == a.len() {
                break;
            }
            if a[i] == b[j] {
                hit(i);
                i += 1;
            }
            j += 1;
        }
    } else {
        while i < a.len() && j < b.len() {
            match a[i].cmp(&b[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    hit(i);
                    i += 1;
                    j += 1;
                }
            }
        }
    }
    found
}

/// The first index at or after `from` whose value in the ascending `list` is at
/// least `x` (`list.len()` when none is): doubling steps from `from`, then a
/// binary search of the last step, so the cost grows with the log of the
/// distance moved, not of the list.
#[inline]
fn lower_bound(list: &[u32], from: usize, x: u32) -> usize {
    let (mut lo, mut hi, mut step) = (from, from, 1usize);
    // Every value before `lo` is below `x`.
    while hi < list.len() && list[hi] < x {
        lo = hi + 1;
        hi += step;
        step *= 2;
    }
    let hi = hi.min(list.len());
    lo + list[lo..hi].partition_point(|&y| y < x)
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

    /// The kernel as it was before #420: lists of the higher-ranked neighbours
    /// sorted by node id, a merge of two whole lists per edge, and one credit
    /// per corner per triangle.
    fn reference(adj: &UndirectedCsr) -> Vec<u32> {
        let n = adj.offsets.len().saturating_sub(1);
        let ranks_above = |u: u32, v: u32| {
            let (du, dv) = (adj.degree(u), adj.degree(v));
            dv > du || (dv == du && v > u)
        };
        let fwd: Vec<Vec<u32>> = (0..n as u32)
            .map(|u| {
                let ups = adj.neighbors(u).iter().copied();
                ups.filter(|&v| ranks_above(u, v)).collect()
            })
            .collect();
        let mut counts = vec![0u32; n];
        for u in 0..n {
            for &v in &fwd[u] {
                let (a, b) = (&fwd[u], &fwd[v as usize]);
                let (mut i, mut j) = (0usize, 0usize);
                while i < a.len() && j < b.len() {
                    match a[i].cmp(&b[j]) {
                        std::cmp::Ordering::Less => i += 1,
                        std::cmp::Ordering::Greater => j += 1,
                        std::cmp::Ordering::Equal => {
                            for x in [u, v as usize, a[i] as usize] {
                                counts[x] += 1;
                            }
                            i += 1;
                            j += 1;
                        }
                    }
                }
            }
        }
        counts
    }

    /// The counts are those of the kernel before #420, on graphs where most
    /// nodes have no edge (#326) and under a mask.
    #[test]
    fn counts_are_the_reference_counts() {
        use crate::algo::linked::testing::{sparse, CASES};
        for &(seed, n, m) in CASES {
            let (t, mask) = sparse(seed, n, m.max(3) * 3);
            for view in [undirected_view(&t, None), undirected_view(&t, Some(&mask))] {
                let adj = view.get();
                let full = reference(adj);
                assert_eq!(per_node_triangles(adj), full, "graph {seed}");
                assert!(n < 1000 || full.iter().any(|&c| c > 0));
            }
        }
    }

    /// Lists of very different lengths meet (a clique whose nodes also have
    /// many leaves), so the marks, the search of the longer list and the merge
    /// are all taken, and the counts are still the reference's.
    #[test]
    fn long_and_short_lists_count_as_the_reference() {
        let k = 3 * MARK_MIN as u32;
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        let mut edge = |a: u32, b: u32| {
            src.push(a);
            dst.push(b);
        };
        for a in 0..k {
            for b in a + 1..k {
                // A third of the pairs are left out, so the lists differ.
                if (a * 7 + b * 3) % 5 > 1 || b == a + 1 {
                    edge(a, b);
                }
            }
        }
        // Leaves on two or three clique nodes each; node `a` of the clique
        // gets more the higher `a` is, so degrees spread widely.
        let mut next = k;
        for a in 0..k {
            for i in 0..a * 4 {
                edge(next, a);
                edge(next, (a + 1 + i) % k);
                if i % 2 == 0 {
                    edge(next, (a + 2 + 3 * i) % k);
                }
                next += 1;
            }
        }
        let t = Topology::build(next as usize, src, dst);
        let adj = t.undirected();
        let fwd = Forward::build(adj);
        let lens: Vec<usize> = (0..fwd.order.len() as u32)
            .map(|r| fwd.list(r).len())
            .collect();
        assert!(lens.iter().any(|&l| l >= MARK_MIN));
        assert!(lens
            .iter()
            .any(|&l| (2..MARK_MIN / GALLOP_RATIO).contains(&l)));
        let counts = per_node_triangles(adj);
        assert_eq!(counts, reference(adj));
        assert!(counts.iter().filter(|&&c| c > 0).count() > k as usize);
    }

    #[test]
    fn forward_lists_are_the_higher_ranked_neighbours() {
        use crate::algo::linked::testing::{sparse, CASES};
        for &(seed, n, m) in CASES {
            let (t, _) = sparse(seed, n, m.max(3) * 3);
            let adj = t.undirected();
            let fwd = Forward::build(adj);
            let key = |v: u32| (adj.degree(v), v);
            assert!(fwd.order.windows(2).all(|w| key(w[0]) < key(w[1])));
            let ranked = (0..n as u32).filter(|&v| adj.degree(v) >= 2).count();
            assert_eq!(fwd.order.len(), ranked, "graph {seed}");
            for (r, &u) in fwd.order.iter().enumerate() {
                let list = fwd.list(r as u32);
                assert!(list.windows(2).all(|w| w[0] < w[1]));
                let mut nodes: Vec<u32> = list.iter().map(|&w| fwd.order[w as usize]).collect();
                nodes.sort_unstable();
                let ups = adj.neighbors(u).iter().copied();
                let expect: Vec<u32> = ups
                    .filter(|&v| adj.degree(v) >= 2 && key(v) > key(u))
                    .collect();
                assert_eq!(nodes, expect, "graph {seed}, node {u}");
            }
        }
    }

    #[test]
    fn intersect_credits_the_common_entries() {
        // Evens against multiples of three, at lengths that take each branch.
        for (na, nb) in [(40u32, 40u32), (4, 400), (400, 4), (0, 9), (9, 0), (1, 1)] {
            let a: Vec<u32> = (0..na).map(|i| 2 * i).collect();
            let b: Vec<u32> = (0..nb).map(|i| 3 * i).collect();
            let mut credit = vec![0u32; a.len()];
            let found = intersect(&a, &b, &mut credit);
            let expect: Vec<u32> = a
                .iter()
                .map(|x| u32::from(b.binary_search(x).is_ok()))
                .collect();
            assert_eq!(credit, expect, "{na} and {nb}");
            assert_eq!(found, expect.iter().sum::<u32>());
        }
        let list = [1, 3, 5, 7, 9, 11];
        for from in 0..=list.len() {
            for x in 0..13 {
                let expect = from + list[from..].iter().filter(|&&y| y < x).count();
                assert_eq!(lower_bound(&list, from, x), expect);
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
