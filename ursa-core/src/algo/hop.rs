//! k-hop reachability — the first frontier kernel.
//!
//! `ur.hop(edges, n).from_(seeds)` answers: from each seed, which nodes are
//! reachable within `k` hops? The result is edge-shaped — one `(seed, reached)`
//! pair per reached node — so it flows back out as an `EdgeFrame` whose `src` is
//! the seed and whose `dst` is the reached node.
//!
//! This is a breadth-first frontier expansion per seed: a `visited` flag per
//! node bounds each seed's search to `n_nodes`, and the frontier advances at
//! most `k` levels. The seed itself is not emitted as its own reached node (0
//! hops); a node reached from a seed is emitted once, at the first level it
//! appears. Large levels are expanded bottom-up (direction-optimizing BFS, see
//! the `frontier` module, #452): the same nodes at the same distances, but a
//! level's nodes are not in the order a top-down search finds them.

use crate::parallel::*;

use super::frontier::Frontier;
use crate::topology::{Direction, EdgeMask, Topology};

/// For each seed, gather the nodes reachable within `k` hops (`k >= 1`).
///
/// Returns two parallel dense-index vectors `(seeds_out, reached)` of equal
/// length: `reached[i]` is reachable from `seeds_out[i]` within `k` hops. Within
/// a single seed's search each reached node appears once; a **repeated** seed in
/// `seeds` runs its BFS again and emits the same pairs again (multiplicity-is-
/// rows — dedup with `.distinct()` if unwanted). Unknown/out-of-range seeds and
/// `k == 0` contribute nothing. `Both` walks out- and in-neighbours together (an
/// undirected hop).
///
/// **Row order.** The rows of each seed are together, the seeds in the order
/// given, and within a seed the nearer levels first. The order of the nodes
/// within one level is not defined: a small level is in the order the search
/// finds them, a large one (expanded bottom-up) in node order. It depends only
/// on the graph, the mask and the arguments, never on the thread count.
///
/// Each seed's BFS is independent, so the seed set is swept in parallel (rayon)
/// with a per-worker reusable search state; one seed alone splits its large
/// levels across threads instead. Per-seed outputs are concatenated in seed
/// order, so the result is byte-for-byte identical to a serial sweep.
pub fn k_hop(topo: &Topology, seeds: &[u32], k: u32, dir: Direction) -> (Vec<u32>, Vec<u32>) {
    k_hop_masked(topo, None, seeds, k, dir)
}

/// [`k_hop`] over the subgraph view `mask` (`None`: the whole graph): only kept
/// edges are followed.
pub fn k_hop_masked(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    seeds: &[u32],
    k: u32,
    dir: Direction,
) -> (Vec<u32>, Vec<u32>) {
    let (seeds, reached, _) = k_hop_distances_masked(topo, mask, seeds, k, dir);
    (seeds, reached)
}

/// [`k_hop_masked`], with each reached node's distance from its seed: the
/// number of steps (`1..=k`) at which the BFS first reached it. The three
/// vectors have equal length and the same order as [`k_hop_masked`]'s.
pub fn k_hop_distances_masked(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    seeds: &[u32],
    k: u32,
    dir: Direction,
) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let n = topo.n_nodes();
    if k == 0 || n == 0 {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    // Resolve the transpose before the parallel loop (see `neighbor_aggregate`).
    if dir != Direction::Out {
        topo.incoming();
    }
    // One (reached, distance) list per seed, in seed order.
    let per_seed: Vec<(u32, Vec<u32>, Vec<u32>)> = if let [seed] = *seeds {
        // One search: its bottom-up levels may use every thread.
        let (reached, distance) =
            bfs_seed(topo, mask, seed, k, dir, &mut Frontier::new(n).parallel());
        vec![(seed, reached, distance)]
    } else {
        seeds
            .par_iter()
            .map_init(
                || Frontier::new(n),
                |search, &seed| {
                    let (reached, distance) = bfs_seed(topo, mask, seed, k, dir, search);
                    (seed, reached, distance)
                },
            )
            .collect()
    };

    let total: usize = per_seed.iter().map(|(_, r, _)| r.len()).sum();
    let mut src_out = Vec::with_capacity(total);
    let mut dst_out = Vec::with_capacity(total);
    let mut distance_out = Vec::with_capacity(total);
    for (seed, reached, distance) in per_seed {
        src_out.extend(std::iter::repeat_n(seed, reached.len()));
        dst_out.extend(reached);
        distance_out.extend(distance);
    }
    (src_out, dst_out, distance_out)
}

/// BFS from a single seed to depth `k`, returning the nodes it reaches and the
/// level (1-based) at which each was first reached. Uses and resets `search`
/// so it is reusable across seeds handled by the same worker.
fn bfs_seed(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    seed: u32,
    k: u32,
    dir: Direction,
    search: &mut Frontier,
) -> (Vec<u32>, Vec<u32>) {
    let mut dst_out: Vec<u32> = Vec::new();
    let mut distance_out: Vec<u32> = Vec::new();
    if (seed as usize) >= topo.n_nodes() {
        return (dst_out, distance_out); // unknown seed
    }
    search.start(topo, dir, &[seed]);
    for level in 1..=k {
        if !search.advance(topo, dir, mask) {
            break;
        }
        let reached = search.level();
        dst_out.extend_from_slice(reached);
        distance_out.extend(std::iter::repeat_n(level, reached.len()));
    }
    search.reset();
    (dst_out, distance_out)
}

/// The set of nodes reachable within `k` hops from **any** seed, including the
/// seeds themselves — a multi-source BFS over the whole seed set at once.
///
/// Returned as dense node indices in ascending order (a set: a node reachable from
/// several seeds appears once). This backs "graph op over a traversal result"
/// (#116): the reached set induces a subgraph of the parent CSR (keep an edge iff
/// both endpoints are reached), which the node-valued kernels then run over via the
/// #114 edge mask. The seeds are included because the reached *region* contains its
/// origin. `k == 0` yields exactly the (known) seeds; unknown/out-of-range seeds
/// contribute nothing. `Both` walks out- and in-neighbours together.
pub fn k_hop_reached_set(topo: &Topology, seeds: &[u32], k: u32, dir: Direction) -> Vec<u32> {
    let visited = k_hop_reached_masked(topo, None, seeds, k, dir);
    // Ascending dense order — a deterministic set independent of BFS visit order.
    (0..visited.len() as u32)
        .filter(|&v| visited[v as usize])
        .collect()
}

/// The reached region of [`k_hop_reached_set`] over the subgraph view `mask`, as a
/// per-node flag (`reached[v]`), the form an induced edge mask is built from
/// ([`EdgeMask::induced`]).
pub fn k_hop_reached_masked(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    seeds: &[u32],
    k: u32,
    dir: Direction,
) -> Vec<bool> {
    if dir != Direction::Out {
        topo.incoming();
    }
    let mut search = Frontier::once(topo.n_nodes());
    search.start(topo, dir, seeds);
    for _level in 0..k {
        if !search.advance(topo, dir, mask) {
            break;
        }
    }
    search.into_visited()
}

#[cfg(test)]
mod tests {
    use super::*;

    // path 0 -> 1 -> 2 -> 3, plus a branch 1 -> 4
    fn path() -> Topology {
        Topology::build(5, vec![0, 1, 2, 1], vec![1, 2, 3, 4])
    }

    fn pairs(src: &[u32], dst: &[u32]) -> Vec<(u32, u32)> {
        let mut p: Vec<(u32, u32)> = src.iter().copied().zip(dst.iter().copied()).collect();
        p.sort_unstable();
        p
    }

    #[test]
    fn one_hop_out() {
        let t = path();
        let (s, d) = k_hop(&t, &[0], 1, Direction::Out);
        assert_eq!(pairs(&s, &d), vec![(0, 1)]);
    }

    #[test]
    fn two_hops_out_reaches_level_two() {
        let t = path();
        let (s, d) = k_hop(&t, &[0], 2, Direction::Out);
        // from 0: level1 = {1}, level2 = {2, 4}
        assert_eq!(pairs(&s, &d), vec![(0, 1), (0, 2), (0, 4)]);
    }

    #[test]
    fn distances_are_the_level_each_node_was_reached_at() {
        let t = path();
        let (s, d, hops) = k_hop_distances_masked(&t, None, &[0, 2], 10, Direction::Out);
        let mut rows: Vec<(u32, u32, u32)> = (0..s.len()).map(|i| (s[i], d[i], hops[i])).collect();
        rows.sort_unstable();
        assert_eq!(
            rows,
            vec![(0, 1, 1), (0, 2, 2), (0, 3, 3), (0, 4, 2), (2, 3, 1)]
        );
        // The same pairs, in the same order, as k_hop.
        let (s2, d2) = k_hop(&t, &[0, 2], 10, Direction::Out);
        assert_eq!((s, d), (s2, d2));
    }

    #[test]
    fn each_reached_node_once() {
        let t = path();
        let (s, d) = k_hop(&t, &[0], 10, Direction::Out);
        // full reach from 0: 1,2,3,4 — each exactly once, seed not included
        assert_eq!(pairs(&s, &d), vec![(0, 1), (0, 2), (0, 3), (0, 4)]);
    }

    #[test]
    fn in_direction_walks_backwards() {
        let t = path();
        let (s, d) = k_hop(&t, &[3], 2, Direction::In);
        // in-neighbours backward from 3: level1 = {2}, level2 = {1}
        assert_eq!(pairs(&s, &d), vec![(3, 1), (3, 2)]);
    }

    #[test]
    fn multiple_seeds_are_independent() {
        let t = path();
        let (s, d) = k_hop(&t, &[0, 2], 1, Direction::Out);
        assert_eq!(pairs(&s, &d), vec![(0, 1), (2, 3)]);
    }

    #[test]
    fn unknown_seed_and_zero_k_are_empty() {
        let t = path();
        let (s, d) = k_hop(&t, &[99], 3, Direction::Out);
        assert!(s.is_empty() && d.is_empty());
        let (s, d) = k_hop(&t, &[0], 0, Direction::Out);
        assert!(s.is_empty() && d.is_empty());
    }

    #[test]
    fn reached_set_includes_seed_and_all_within_k() {
        let t = path();
        // from 0, depth 2: seed 0 + level1 {1} + level2 {2,4}
        assert_eq!(
            k_hop_reached_set(&t, &[0], 2, Direction::Out),
            vec![0, 1, 2, 4]
        );
    }

    #[test]
    fn reached_set_k_zero_is_just_the_seeds() {
        let t = path();
        assert_eq!(
            k_hop_reached_set(&t, &[2, 0], 0, Direction::Out),
            vec![0, 2]
        );
    }

    #[test]
    fn reached_set_unions_multiple_seeds_without_duplicates() {
        let t = path();
        // 0 reaches {0,1}; 2 reaches {2,3} at depth 1 -> union {0,1,2,3}, each once.
        assert_eq!(
            k_hop_reached_set(&t, &[0, 2], 1, Direction::Out),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn reached_set_drops_unknown_seeds() {
        let t = path();
        assert_eq!(
            k_hop_reached_set(&t, &[99], 3, Direction::Out),
            Vec::<u32>::new()
        );
        // a known seed alongside an unknown one still expands from the known one.
        assert_eq!(
            k_hop_reached_set(&t, &[3, 99], 1, Direction::In),
            vec![2, 3]
        );
    }

    #[test]
    fn masked_hops_follow_only_kept_edges() {
        // path(): rows 0:0->1, 1:1->2, 2:2->3, 3:1->4. Dropping row 1 cuts 1->2.
        let t = path();
        let m = EdgeMask::from_bools(&[true, false, true, true]);
        let (s, d) = k_hop_masked(&t, Some(&m), &[0], 3, Direction::Out);
        assert_eq!(pairs(&s, &d), vec![(0, 1), (0, 4)]);
        let reached = k_hop_reached_masked(&t, Some(&m), &[0], 3, Direction::Out);
        assert_eq!(reached, vec![true, true, false, false, true]);
    }
}
