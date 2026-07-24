//! k-hop reachability — the first frontier kernel.
//!
//! `ur.hop(edges, n).from_(seeds)` answers: from each seed, which nodes are
//! reachable within `k` hops? The result is edge-shaped — one `(seed, reached)`
//! pair per reached node — so it flows back out as an `EdgeFrame` whose `src` is
//! the seed and whose `dst` is the reached node.
//!
//! This is a plain breadth-first frontier expansion per seed: a `visited` bitmap
//! bounds each seed's search to `n_nodes`, and the frontier advances exactly `k`
//! levels. The seed itself is not emitted as its own reached node (0 hops); a
//! node reached from a seed is emitted once, at the first level it appears.
//! Direction-optimizing BFS (Beamer top-down/bottom-up) is deferred — correctness
//! first, per the spec's "naive placement first" stance.

use rayon::prelude::*;

use crate::topology::{Direction, Topology};

/// Per-worker reusable BFS scratch, so a parallel seed sweep allocates the
/// `n`-sized `visited` bitmap once per thread instead of once per seed.
struct Scratch {
    visited: Vec<bool>,
    frontier: Vec<u32>,
    next: Vec<u32>,
    touched: Vec<u32>,
}

impl Scratch {
    fn new(n: usize) -> Self {
        Scratch {
            visited: vec![false; n],
            frontier: Vec::new(),
            next: Vec::new(),
            touched: Vec::new(),
        }
    }
}

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
/// Each seed's BFS is independent, so the seed set is swept in parallel (rayon)
/// with a per-worker reusable [`Scratch`]. Per-seed outputs are concatenated in
/// seed order, so the result is byte-for-byte identical to a serial sweep.
pub fn k_hop(topo: &Topology, seeds: &[u32], k: u32, dir: Direction) -> (Vec<u32>, Vec<u32>) {
    let n = topo.n_nodes();
    if k == 0 || n == 0 {
        return (Vec::new(), Vec::new());
    }

    // One (src, dst) pair-list per seed, in seed order.
    let per_seed: Vec<(Vec<u32>, Vec<u32>)> = seeds
        .par_iter()
        .map_init(
            || Scratch::new(n),
            |scratch, &seed| bfs_seed(topo, seed, k, dir, scratch),
        )
        .collect();

    let total: usize = per_seed.iter().map(|(s, _)| s.len()).sum();
    let mut src_out = Vec::with_capacity(total);
    let mut dst_out = Vec::with_capacity(total);
    for (s, d) in per_seed {
        src_out.extend(s);
        dst_out.extend(d);
    }
    (src_out, dst_out)
}

/// BFS from a single seed to depth `k`, returning its `(seed, reached)` pairs.
/// Uses and restores `scratch` (only the touched nodes are reset) so it is reusable
/// across seeds handled by the same worker.
fn bfs_seed(
    topo: &Topology,
    seed: u32,
    k: u32,
    dir: Direction,
    scratch: &mut Scratch,
) -> (Vec<u32>, Vec<u32>) {
    let n = topo.n_nodes();
    let mut src_out: Vec<u32> = Vec::new();
    let mut dst_out: Vec<u32> = Vec::new();
    if (seed as usize) >= n {
        return (src_out, dst_out); // unknown seed
    }

    let Scratch {
        visited,
        frontier,
        next,
        touched,
    } = scratch;
    visited[seed as usize] = true;
    frontier.clear();
    frontier.push(seed);
    touched.clear();
    touched.push(seed);

    for _level in 0..k {
        next.clear();
        for &u in frontier.iter() {
            push_neighbors(topo, u, dir, |v| {
                if !visited[v as usize] {
                    visited[v as usize] = true;
                    touched.push(v);
                    next.push(v);
                    src_out.push(seed);
                    dst_out.push(v);
                }
            });
        }
        if next.is_empty() {
            break;
        }
        std::mem::swap(frontier, next);
    }

    // Restore visited for the next seed on this worker.
    for &t in touched.iter() {
        visited[t as usize] = false;
    }
    (src_out, dst_out)
}

/// Apply `f` to each neighbour of `u` in `dir` (both adjacencies for `Both`).
#[inline]
fn push_neighbors<F: FnMut(u32)>(topo: &Topology, u: u32, dir: Direction, mut f: F) {
    match dir {
        Direction::Out => {
            for &v in topo.out().neighbors(u) {
                f(v);
            }
        }
        Direction::In => {
            for &v in topo.incoming().neighbors(u) {
                f(v);
            }
        }
        Direction::Both => {
            for &v in topo.out().neighbors(u) {
                f(v);
            }
            for &v in topo.incoming().neighbors(u) {
                f(v);
            }
        }
    }
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
}
