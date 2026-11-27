//! Level-by-level breadth-first search that switches direction on large
//! frontiers (#452): the shared engine of `bfs_distances`, `k_hop` and its
//! reached set, and the path statistics' per-source scan.
//!
//! While the frontier is small, a level is expanded **top-down**: each frontier
//! node's neighbours are read, and the unvisited ones join the next level.
//! When the frontier's edges are a large share of the edges not yet explored,
//! a level is expanded **bottom-up** instead: each unvisited node reads its
//! neighbours in the reverse direction and stops at the first one in the
//! frontier (Beamer, Asanović and Patterson, *Direction-Optimizing Breadth-First
//! Search*, SC 2012). On a graph with a small diameter, the two or three middle
//! levels hold most of the nodes; there a top-down step reads nearly every edge
//! and finds most of their ends visited already, while a bottom-up step stops
//! early for most nodes.
//!
//! **What the search finds does not depend on the direction**: a node joins
//! level `d` exactly when it is unvisited and has a neighbour at level `d - 1`,
//! whichever side reads the edge. The **order** of a level does: top-down finds
//! nodes in the order the frontier reaches them, bottom-up in node order. The
//! choice of direction depends only on the graph, the mask and the search, so
//! the order is the same on any number of threads. A caller that returns rows
//! in level order (`k_hop`) therefore returns the same rows, each with the same
//! distance, possibly in another order.
//!
//! A bottom-up step in direction `Out` reads the in-adjacency. It is used only
//! when that is already built or stored (see [`Topology::incoming_is_cheap`]):
//! the search never builds a transpose just to go bottom-up.

use crate::parallel::*;
use crate::topology::{Adjacency, Direction, EdgeMask, Topology};

/// Go bottom-up when the frontier's edges exceed `1 / ALPHA` of the edges not
/// yet explored (Beamer et al.'s `alpha`).
const ALPHA: u64 = 14;
/// Go back top-down when the frontier holds fewer than `n / BETA` nodes
/// (Beamer et al.'s `beta`).
const BETA: usize = 24;
/// A bottom-up step reads every node's visited flag, so go bottom-up only when
/// the frontier has at least `n / SCAN` edges: a small search in a large graph
/// with few edges (many isolated nodes, a small component) stays top-down.
const SCAN: u64 = 16;
/// The fewest nodes for which a bottom-up step is split across threads.
const PARALLEL_MIN_NODES: usize = 1 << 16;

/// How a search picks each level's direction. Only the tests force one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Policy {
    /// Beamer et al.'s rule (the default).
    Auto,
    /// Always top-down: the kernels before #452.
    #[cfg_attr(not(test), allow(dead_code))]
    TopDown,
    /// Always bottom-up after the seeds (reading the in-adjacency even in
    /// direction `Out`).
    #[cfg_attr(not(test), allow(dead_code))]
    BottomUp,
}

/// One breadth-first search's state, reusable across searches over the same
/// graph (see [`Frontier::reset`]).
pub(crate) struct Frontier {
    /// `visited[v]`: `v` is a seed or has been reached.
    visited: Vec<bool>,
    /// The current level.
    frontier: Vec<u32>,
    next: Vec<u32>,
    /// Every node visited by this search, so a reset costs what it reached
    /// (kept only if `track`).
    touched: Vec<u32>,
    /// Whether `touched` is kept: the state is reused for many searches.
    track: bool,
    /// The current level as a bitmap, during a bottom-up step only; all zero
    /// otherwise. Allocated on the first bottom-up step.
    bits: Vec<u64>,
    /// Whether the last step was bottom-up.
    bottom_up: bool,
    /// The edges (adjacency slots in the search's direction) not yet read from
    /// a frontier.
    unexplored: u64,
    /// Whether a bottom-up step on a large graph may run on several threads.
    parallel: bool,
    policy: Policy,
}

impl Frontier {
    /// State for many searches, one after another, over a graph of `n` nodes,
    /// each one among many running in parallel: a bottom-up step runs on the
    /// calling thread.
    pub(crate) fn new(n: usize) -> Self {
        Self::make(n, false, true)
    }

    /// State for one search over a graph of `n` nodes, whose bottom-up steps
    /// may split their scan across threads. It is not [`reset`](Self::reset)
    /// for another search: it does not keep the list of nodes reached.
    pub(crate) fn once(n: usize) -> Self {
        Self::make(n, true, false)
    }

    /// Let this state's bottom-up steps split their scan across threads: it
    /// is not one of many searches running in parallel.
    pub(crate) fn parallel(mut self) -> Self {
        self.parallel = true;
        self
    }

    fn make(n: usize, parallel: bool, track: bool) -> Self {
        Frontier {
            visited: vec![false; n],
            frontier: Vec::new(),
            next: Vec::new(),
            touched: Vec::new(),
            track,
            bits: Vec::new(),
            bottom_up: false,
            unexplored: 0,
            parallel,
            policy: Policy::Auto,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    /// Start a search from `seeds` (level 0). Out-of-range seeds and repeats
    /// are dropped. The state must be fresh or [`reset`](Self::reset).
    pub(crate) fn start(&mut self, topo: &Topology, dir: Direction, seeds: &[u32]) {
        let n = self.visited.len();
        for &s in seeds {
            if (s as usize) < n && !self.visited[s as usize] {
                self.visited[s as usize] = true;
                self.frontier.push(s);
                if self.track {
                    self.touched.push(s);
                }
            }
        }
        let m = topo.n_edges() as u64;
        self.unexplored = if dir == Direction::Both { 2 * m } else { m };
        self.bottom_up = false;
    }

    /// The current level: the seeds after [`start`](Self::start), then the
    /// nodes the last [`advance`](Self::advance) reached.
    pub(crate) fn level(&self) -> &[u32] {
        &self.frontier
    }

    /// Expand one level: the current level becomes the unvisited nodes with a
    /// neighbour (in `dir`, over kept edges) in it. Returns whether it is
    /// non-empty.
    pub(crate) fn advance(
        &mut self,
        topo: &Topology,
        dir: Direction,
        mask: Option<&EdgeMask>,
    ) -> bool {
        if self.frontier.is_empty() {
            return false;
        }
        let up = self.choose(topo, dir);
        self.bottom_up = up;
        self.next.clear();
        if up {
            self.bottom_up_step(topo, dir, mask);
        } else {
            self.top_down_step(topo, dir, mask);
        }
        if self.track {
            self.touched.extend_from_slice(&self.next);
        }
        std::mem::swap(&mut self.frontier, &mut self.next);
        !self.frontier.is_empty()
    }

    /// Clear the search so the state can start another. Costs what the search
    /// reached, or one pass over the flags if that is less (or the state does
    /// not track what it reached).
    pub(crate) fn reset(&mut self) {
        debug_assert!(self.track, "a one-search state is not reset");
        if !self.track || self.touched.len() > self.visited.len() / 8 {
            self.visited.fill(false);
        } else {
            for &v in &self.touched {
                self.visited[v as usize] = false;
            }
        }
        self.touched.clear();
        self.frontier.clear();
        self.next.clear();
    }

    /// The visited flags: the seeds and every node reached.
    pub(crate) fn into_visited(self) -> Vec<bool> {
        self.visited
    }

    /// Whether the next step goes bottom-up, and the bookkeeping for the rule.
    fn choose(&mut self, topo: &Topology, dir: Direction) -> bool {
        let n = self.visited.len();
        let front_edges: u64 = {
            let degree = |adj: &Adjacency| -> u64 {
                self.frontier
                    .iter()
                    .map(|&u| u64::from(adj.degree(u)))
                    .sum()
            };
            match dir {
                Direction::Out => degree(topo.out()),
                Direction::In => degree(topo.incoming()),
                Direction::Both => degree(topo.out()) + degree(topo.incoming()),
            }
        };
        let up = match self.policy {
            Policy::TopDown => false,
            Policy::BottomUp => true,
            Policy::Auto if self.bottom_up => self.frontier.len() >= n / BETA,
            Policy::Auto => {
                front_edges > self.unexplored / ALPHA
                    && front_edges >= n as u64 / SCAN
                    && (dir != Direction::Out || topo.incoming_is_cheap())
            }
        };
        self.unexplored = self.unexplored.saturating_sub(front_edges);
        up
    }

    fn top_down_step(&mut self, topo: &Topology, dir: Direction, mask: Option<&EdgeMask>) {
        let Frontier {
            visited,
            frontier,
            next,
            ..
        } = self;
        for &u in frontier.iter() {
            topo.for_each_neighbor(u, dir, mask, |v| {
                if !visited[v as usize] {
                    visited[v as usize] = true;
                    next.push(v);
                }
            });
        }
    }

    fn bottom_up_step(&mut self, topo: &Topology, dir: Direction, mask: Option<&EdgeMask>) {
        let n = self.visited.len();
        if self.bits.is_empty() {
            self.bits = vec![0u64; n.div_ceil(64)];
        }
        for &u in &self.frontier {
            self.bits[u as usize >> 6] |= 1 << (u & 63);
        }
        // A node joins if an edge leads to it from the frontier: read its
        // edges the other way.
        let (first, second): (&Adjacency, Option<&Adjacency>) = match dir {
            Direction::Out => (topo.incoming(), None),
            Direction::In => (topo.out(), None),
            Direction::Both => (topo.out(), Some(topo.incoming())),
        };
        let bits = &self.bits;
        let joins = |v: u32| {
            reaches(first, v, mask, bits) || second.is_some_and(|adj| reaches(adj, v, mask, bits))
        };
        let threads = current_num_threads();
        if self.parallel && threads > 1 && n >= PARALLEL_MIN_NODES {
            // Each chunk writes only its own flags; the chunks' finds are
            // joined in chunk order, so the level is in node order as on one
            // thread.
            let chunk = (n / (threads * 8)).max(4096);
            let parts: Vec<Vec<u32>> = self
                .visited
                .par_chunks_mut(chunk)
                .enumerate()
                .map(|(i, flags)| {
                    let base = (i * chunk) as u32;
                    let mut found = Vec::new();
                    for (j, seen) in flags.iter_mut().enumerate() {
                        let v = base + j as u32;
                        if !*seen && joins(v) {
                            *seen = true;
                            found.push(v);
                        }
                    }
                    found
                })
                .collect();
            for part in parts {
                self.next.extend_from_slice(&part);
            }
        } else {
            for (v, seen) in self.visited.iter_mut().enumerate() {
                if !*seen && joins(v as u32) {
                    *seen = true;
                    self.next.push(v as u32);
                }
            }
        }
        for &u in &self.frontier {
            self.bits[u as usize >> 6] = 0;
        }
    }
}

/// Whether one of `v`'s neighbours in `adj`, over a kept edge, is set in
/// `bits`. Stops at the first.
#[inline]
fn reaches(adj: &Adjacency, v: u32, mask: Option<&EdgeMask>, bits: &[u64]) -> bool {
    let set = |u: u32| bits[u as usize >> 6] >> (u & 63) & 1 == 1;
    match mask {
        None => adj.any_neighbor(v, set),
        Some(m) => {
            let ids = adj.edge_ids(v);
            let mut k = 0;
            adj.any_neighbor(v, |u| {
                let e = ids[k];
                k += 1;
                set(u) && m.keep(e)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// splitmix64.
    fn next(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn random(n: usize, m: usize, seed: u64) -> (Topology, EdgeMask) {
        let mut st = seed;
        let (src, dst): (Vec<u32>, Vec<u32>) = (0..m)
            .map(|_| {
                (
                    (next(&mut st) % n as u64) as u32,
                    (next(&mut st) % n as u64) as u32,
                )
            })
            .unzip();
        let keep: Vec<bool> = (0..m).map(|_| !next(&mut st).is_multiple_of(3)).collect();
        (Topology::build(n, src, dst), EdgeMask::from_bools(&keep))
    }

    /// Each level, sorted, from `seeds` to depth `k`.
    fn levels(
        t: &Topology,
        mask: Option<&EdgeMask>,
        seeds: &[u32],
        k: u32,
        dir: Direction,
        policy: Policy,
        parallel: bool,
    ) -> Vec<Vec<u32>> {
        let mut f = Frontier::make(t.n_nodes(), parallel, !parallel).with_policy(policy);
        f.start(t, dir, seeds);
        let mut out = vec![{
            let mut l = f.level().to_vec();
            l.sort_unstable();
            l
        }];
        for _ in 0..k {
            if !f.advance(t, dir, mask) {
                break;
            }
            let mut l = f.level().to_vec();
            l.sort_unstable();
            out.push(l);
        }
        out
    }

    #[test]
    fn every_policy_finds_the_same_levels() {
        // Sparse with many isolated nodes, and dense; sizes on either side of
        // the parallel threshold.
        for (n, m) in [(300, 200), (300, 3000), (70_000, 60_000), (70_000, 500_000)] {
            let (t, mask) = random(n, m, n as u64 ^ m as u64);
            for dir in [Direction::Out, Direction::In, Direction::Both] {
                for mk in [None, Some(&mask)] {
                    for seeds in [vec![0u32], vec![3, 3, 1, n as u32 + 5]] {
                        let want = levels(&t, mk, &seeds, 1000, dir, Policy::TopDown, false);
                        for (policy, parallel) in [
                            (Policy::BottomUp, false),
                            (Policy::BottomUp, true),
                            (Policy::Auto, false),
                            (Policy::Auto, true),
                        ] {
                            let got = levels(&t, mk, &seeds, 1000, dir, policy, parallel);
                            assert_eq!(got, want, "n={n} m={m} {dir:?} {policy:?} {parallel}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_bottom_up_level_is_in_node_order_and_reset_clears_it() {
        // 0 -> {5, 3, 4}: top-down finds them in edge order, bottom-up in id order.
        let t = Topology::build(6, vec![0, 0, 0], vec![5, 3, 4]);
        let mut f = Frontier::new(6).with_policy(Policy::BottomUp);
        f.start(&t, Direction::Out, &[0]);
        assert!(f.advance(&t, Direction::Out, None));
        assert_eq!(f.level(), [3, 4, 5]);
        assert!(!f.advance(&t, Direction::Out, None));
        f.reset();
        assert!(f.bits.iter().all(|&w| w == 0));
        let mut f2 = Frontier::new(6).with_policy(Policy::TopDown);
        f2.start(&t, Direction::Out, &[0]);
        f2.advance(&t, Direction::Out, None);
        assert_eq!(f2.level(), [5, 3, 4]);
        // Reused after a reset, the state searches again from scratch.
        f.start(&t, Direction::In, &[4]);
        assert!(f.advance(&t, Direction::In, None));
        assert_eq!(f.level(), [0]);
        assert!(f.into_visited() == [true, false, false, false, true, false]);
    }

    #[test]
    fn out_stays_top_down_without_a_cheap_in_adjacency() {
        // A star: the rule would go bottom-up at once, but direction `Out` on
        // a graph whose transpose is not built must not build one.
        let n = 2000u32;
        let t = Topology::build(n as usize, vec![0; 1999], (1..n).collect());
        let mut f = Frontier::new(n as usize);
        f.start(&t, Direction::Out, &[0]);
        f.advance(&t, Direction::Out, None);
        assert!(!f.bottom_up);
        assert!(t.incoming_if_built().is_none());
        // Once it is built, the same search goes bottom-up.
        t.incoming();
        let mut f = Frontier::new(n as usize);
        f.start(&t, Direction::Out, &[0]);
        f.advance(&t, Direction::Out, None);
        assert!(f.bottom_up);
        assert_eq!(f.level().len(), 1999);
    }
}
