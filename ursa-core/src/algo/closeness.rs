//! Closeness centrality — a BFS from every node, folding finite distances.
//!
//! Unweighted (hop count): 64 sources per breadth-first pass, one bit of a `u64`
//! per source (#422), the passes parallelised with Rayon. A node's adjacency list
//! is read once per level on which any of the 64 searches has it in the frontier,
//! not once per search. Weighted (edge weight): one Dijkstra per source,
//! parallelised over sources. Each rayon split allocates its scratch **once** and
//! resets only the nodes it touched between passes or sources. Results are
//! byte-identical to one search per source: the unweighted fold is an exact `i64`
//! sum and count per source (order does not matter), and the weighted fold keeps
//! the original ascending node-order summation so its `f64` total is bit-for-bit
//! unchanged.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::parallel::*;

use super::bfs::TotalF64;
use crate::topology::{Direction, EdgeMask, Topology};

/// Sources per unweighted pass: one bit of a `u64` each (#422).
const BATCH: usize = 64;

/// Reusable scratch for one batch of up to 64 unweighted searches run together
/// (multi-source BFS, Then et al., VLDB 2014). Bit `i` of a node's word belongs
/// to source `i` of the batch. `seen` and `next` are indexed by node and are all
/// zero between batches; the frontier is a dense list of `(node, word)` pairs, so
/// a level reads its words in order. `touched` lists the nodes `seen` set a bit
/// in, so the reset costs `O(reached)` rather than `O(n)`.
struct ClosenessBatchScratch {
    seen: Vec<u64>,
    next: Vec<u64>,
    frontier: Vec<(u32, u64)>,
    reached: Vec<u32>,
    touched: Vec<u32>,
}

impl ClosenessBatchScratch {
    fn new(n: usize) -> Self {
        ClosenessBatchScratch {
            seen: vec![0u64; n],
            next: vec![0u64; n],
            frontier: Vec::new(),
            reached: Vec::new(),
            touched: Vec::new(),
        }
    }
}

/// Reusable per-source scratch for the weighted (Dijkstra) distances.
struct ClosenessWeightedScratch {
    dist: Vec<f64>,
    settled: Vec<bool>,
    heap: BinaryHeap<Reverse<(TotalF64, u32)>>,
    touched: Vec<u32>,
}

impl ClosenessWeightedScratch {
    fn new(n: usize) -> Self {
        ClosenessWeightedScratch {
            dist: vec![f64::INFINITY; n],
            settled: vec![false; n],
            heap: BinaryHeap::new(),
            touched: Vec::new(),
        }
    }
}

/// Closeness centrality per node, directed (following out-edges): for each node
/// `u`, `reachable / Σ dist(u, v)` over the `reachable` nodes `v` at finite
/// distance `≥ 1`. A node that reaches nothing scores `0.0`.
///
/// Only reachable pairs contribute, so this is the standard form for
/// disconnected graphs (equivalently `reachable / total_distance`) rather than
/// the classic `(n − 1) / Σ dist`, which is undefined when some `v` is
/// unreachable. `O(n · (n + m))` — document the cost; use `sample`-based stats
/// for very large graphs.
pub fn closeness(topo: &Topology, mask: Option<&EdgeMask>) -> Vec<f64> {
    closeness_directed(topo, mask, Direction::Out)
}

/// [`closeness`] with the distance direction as a parameter: `Out` measures
/// distance *from* the node along out-edges (the default), `In` distance *to* the
/// node (NetworkX's directed convention), and `Both` the undirected distance —
/// a BFS that ignores edge direction, which is closeness on the undirected view.
pub fn closeness_directed(topo: &Topology, mask: Option<&EdgeMask>, dir: Direction) -> Vec<f64> {
    let n = topo.n_nodes();
    if dir != Direction::Out {
        // Build the lazily cached transpose here, not inside the parallel loop.
        topo.incoming();
    }
    // 64 consecutive sources per pass, the passes in parallel. Each pass writes
    // its own 64 output slots, so there is no cross-pass ordering to preserve, and
    // the scratch is built once per rayon split and reused across its passes.
    let mut out = vec![0.0f64; n];
    out.par_chunks_mut(BATCH).enumerate().for_each_init(
        || ClosenessBatchScratch::new(n),
        |sc, (b, slots)| closeness_batch(topo, mask, dir, (b * BATCH) as u32, slots, sc),
    );
    out
}

/// Unweighted closeness of the sources `first .. first + slots.len()` (at most
/// 64), written to `slots`, from one breadth-first pass that carries all of their
/// searches as bits.
///
/// A step ORs each frontier node's word into its neighbours' `next`, less what
/// they have already seen; a node's adjacency list is read once per level on
/// which any of the searches has it in the frontier, not once per search. When
/// bit `i` first reaches a node at level `d`, `d` is added to source `i`'s
/// distance sum and 1 to its reach count. Those are the integers the
/// one-search-per-source kernel folded (every node at finite distance `≥ 1`
/// once, at its BFS depth), so `reachable / sum` is the same `f64`, bit for bit.
/// Scratch is clean on entry and left clean on exit.
fn closeness_batch(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    dir: Direction,
    first: u32,
    slots: &mut [f64],
    sc: &mut ClosenessBatchScratch,
) {
    let ClosenessBatchScratch {
        seen,
        next,
        frontier,
        reached,
        touched,
    } = sc;
    let k = slots.len();
    debug_assert!(k <= BATCH);

    for i in 0..k {
        let s = first + i as u32;
        let bit = 1u64 << i;
        seen[s as usize] = bit;
        frontier.push((s, bit));
        touched.push(s);
    }

    let mut sum = [0i64; BATCH];
    let mut reach = [0i64; BATCH];
    let mut level = 1i64;
    while !frontier.is_empty() {
        for &(v, word) in frontier.iter() {
            topo.for_each_neighbor(v, dir, mask, |w| {
                let w = w as usize;
                let new = word & !seen[w];
                if new != 0 {
                    if next[w] == 0 {
                        reached.push(w as u32);
                    }
                    next[w] |= new;
                }
            });
        }
        frontier.clear();
        for &w in reached.iter() {
            let new = std::mem::take(&mut next[w as usize]);
            if seen[w as usize] == 0 {
                touched.push(w);
            }
            seen[w as usize] |= new;
            frontier.push((w, new));
            let mut bits = new;
            while bits != 0 {
                let i = bits.trailing_zeros() as usize;
                sum[i] += level;
                reach[i] += 1;
                bits &= bits - 1;
            }
        }
        reached.clear();
        level += 1;
    }

    for (i, slot) in slots.iter_mut().enumerate() {
        *slot = if sum[i] > 0 {
            reach[i] as f64 / sum[i] as f64
        } else {
            0.0
        };
    }
    for &w in touched.iter() {
        seen[w as usize] = 0;
    }
    touched.clear();
}

/// Weighted closeness centrality: as [`closeness`], but distances are minimum
/// total edge weight (Dijkstra) rather than hop count. `reachable / Σ dist(u, v)`
/// over the reachable nodes at finite positive distance. `weights[e]` is the
/// weight of edge row `e` (non-negative; `len == n_edges`).
pub fn closeness_weighted(topo: &Topology, weights: &[f64], mask: Option<&EdgeMask>) -> Vec<f64> {
    closeness_weighted_directed(topo, weights, mask, Direction::Out)
}

/// [`closeness_weighted`] with the distance direction as a parameter (see
/// [`closeness_directed`]). Under `Both`, Dijkstra relaxes every incident edge in
/// either direction, so where several rows join a pair the cheapest one is used.
pub fn closeness_weighted_directed(
    topo: &Topology,
    weights: &[f64],
    mask: Option<&EdgeMask>,
    dir: Direction,
) -> Vec<f64> {
    if dir != Direction::Out {
        topo.incoming();
    }
    assert_eq!(
        weights.len(),
        topo.n_edges(),
        "weights length must equal the edge count"
    );
    let n = topo.n_nodes();
    (0..n as u32)
        .into_par_iter()
        .map_init(
            || ClosenessWeightedScratch::new(n),
            |sc, u| closeness_weighted_one(topo, weights, mask, dir, u, sc),
        )
        .collect()
}

/// One weighted closeness value, over reusable scratch. The `f64` distance sum
/// adds the reached nodes in ascending node order — the order of the original
/// `0..n` scan, whose unreached (infinite) entries were skipped — so the total is
/// bit-for-bit identical (float addition is not associative) at `O(reached)` rather
/// than `O(n)` per source.
fn closeness_weighted_one(
    topo: &Topology,
    weights: &[f64],
    mask: Option<&EdgeMask>,
    dir: Direction,
    u: u32,
    sc: &mut ClosenessWeightedScratch,
) -> f64 {
    let ClosenessWeightedScratch {
        dist,
        settled,
        heap,
        touched,
    } = sc;

    dist[u as usize] = 0.0;
    heap.push(Reverse((TotalF64(0.0), u)));
    while let Some(Reverse((TotalF64(du), v))) = heap.pop() {
        if settled[v as usize] {
            continue;
        }
        settled[v as usize] = true;
        touched.push(v);
        topo.for_each_weighted_neighbor(v, weights, dir, mask, |w, wt| {
            if !settled[w as usize] {
                let nd = du + wt;
                if nd < dist[w as usize] {
                    dist[w as usize] = nd;
                    heap.push(Reverse((TotalF64(nd), w)));
                }
            }
        });
    }

    let mut sum = 0.0f64;
    let mut reachable = 0i64;
    // Ascending node order — the exact summation order of the original `0..n` scan,
    // so the f64 total is bit-for-bit unchanged. `touched` is exactly the finite-
    // distance set (every node given a finite distance is pushed, so eventually
    // settled), so sorting it visits the same nodes in the same order. When most
    // nodes were reached, the plain scan is cheaper than the sort.
    //
    // Count every reachable non-source node — the same rule the unweighted kernel
    // applies (`d >= 1` there excludes only the source). A node reachable at total
    // cost *exactly* 0.0 (via zero-weight edges) is a reachable non-source node, so
    // it is counted: `+1` to `reachable`, `+0.0` to `sum` (a no-op on the f64 total,
    // so the bit-identical-sum guarantee holds). This matches NetworkX, which counts
    // it in the path total and guards `totsp > 0` for the divide — as the `sum > 0.0`
    // check below does. (Previously an extra `d > 0.0` here dropped such nodes from
    // the count, diverging from the unweighted kernel on zero-weight graphs.)
    if touched.len() < dist.len() / 32 {
        touched.sort_unstable();
        for &v in touched.iter() {
            if v != u {
                sum += dist[v as usize];
                reachable += 1;
            }
        }
    } else {
        for (v, &d) in dist.iter().enumerate() {
            if v != u as usize && d.is_finite() {
                sum += d;
                reachable += 1;
            }
        }
    }
    let result = if sum > 0.0 {
        reachable as f64 / sum
    } else {
        0.0
    };

    // Reset only the settled (touched) nodes; the heap is drained by the loop.
    for &w in touched.iter() {
        dist[w as usize] = f64::INFINITY;
        settled[w as usize] = false;
    }
    touched.clear();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // The one-search-per-source kernel this replaced (#422), kept as the
    // reference the batched kernel is compared with bit for bit.

    /// Reusable per-source scratch for the unweighted (hop-count) BFS. `touched` lists
    /// the nodes reached from the current source so they can be reset in `O(reached)`
    /// rather than re-zeroing all `n`.
    struct ClosenessScratch {
        dist: Vec<i32>,
        frontier: Vec<u32>,
        next: Vec<u32>,
        touched: Vec<u32>,
    }

    impl ClosenessScratch {
        fn new(n: usize) -> Self {
            ClosenessScratch {
                dist: vec![-1i32; n],
                frontier: Vec::new(),
                next: Vec::new(),
                touched: Vec::new(),
            }
        }
    }

    /// One unweighted closeness value, over reusable scratch (clean on entry; the
    /// touched nodes are reset to clean on exit). The `i64` distance sum is exact, so
    /// summing the reached set (rather than scanning all `n` in node order) gives an
    /// identical result.
    fn closeness_one(
        topo: &Topology,
        mask: Option<&EdgeMask>,
        dir: Direction,
        u: u32,
        sc: &mut ClosenessScratch,
    ) -> f64 {
        let ClosenessScratch {
            dist,
            frontier,
            next,
            touched,
        } = sc;

        dist[u as usize] = 0;
        touched.push(u);
        frontier.push(u);
        let mut level = 1i32;
        while !frontier.is_empty() {
            next.clear();
            for &v in frontier.iter() {
                topo.for_each_neighbor(v, dir, mask, |w| {
                    if dist[w as usize] < 0 {
                        dist[w as usize] = level;
                        next.push(w);
                        touched.push(w);
                    }
                });
            }
            level += 1;
            std::mem::swap(frontier, next);
        }

        let mut sum = 0i64;
        let mut reachable = 0i64;
        for &w in touched.iter() {
            let d = dist[w as usize];
            if d >= 1 {
                sum += d as i64;
                reachable += 1;
            }
        }
        let result = if sum > 0 {
            reachable as f64 / sum as f64
        } else {
            0.0
        };

        for &w in touched.iter() {
            dist[w as usize] = -1;
        }
        touched.clear();
        frontier.clear();
        next.clear();
        result
    }

    #[test]
    fn closeness_of_a_directed_triangle() {
        // 0->1->2->0: from each node, distances {1, 2} to the other two.
        // reachable = 2, Σ dist = 3  ->  2/3.
        let t = Topology::build(3, vec![0, 1, 2], vec![1, 2, 0]);
        let c = closeness(&t, None);
        for &x in &c {
            assert!((x - 2.0 / 3.0).abs() < 1e-12, "got {x}");
        }
    }

    #[test]
    fn closeness_on_a_directed_line() {
        // 0->1->2->3. Node 0 reaches {1,2,3} at {1,2,3}: 3/6 = 0.5.
        // Node 2 reaches {3} at {1}: 1/1 = 1.0. Node 3 reaches nothing: 0.0.
        let t = Topology::build(4, vec![0, 1, 2], vec![1, 2, 3]);
        let c = closeness(&t, None);
        assert!((c[0] - 0.5).abs() < 1e-12, "got {}", c[0]);
        assert!((c[1] - 2.0 / 3.0).abs() < 1e-12, "got {}", c[1]);
        assert!((c[2] - 1.0).abs() < 1e-12, "got {}", c[2]);
        assert_eq!(c[3], 0.0);
    }

    #[test]
    fn closeness_by_direction_on_a_line() {
        // 0->1->2. In: node 2 is reached from {1, 0} at {1, 2}: 2/3. Both: the
        // undirected path, so node 1 (the middle) is 2/2 = 1 and the ends 2/3.
        let t = Topology::build(3, vec![0, 1], vec![1, 2]);
        let c_in = closeness_directed(&t, None, Direction::In);
        assert_eq!(c_in[0], 0.0);
        assert!((c_in[2] - 2.0 / 3.0).abs() < 1e-12);
        let c_both = closeness_directed(&t, None, Direction::Both);
        assert!((c_both[0] - 2.0 / 3.0).abs() < 1e-12);
        assert!((c_both[1] - 1.0).abs() < 1e-12);
        assert!((c_both[2] - 2.0 / 3.0).abs() < 1e-12);
        let w_both = closeness_weighted_directed(&t, &[1.0, 1.0], None, Direction::Both);
        assert_eq!(w_both, c_both);
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(closeness(&t, None).is_empty());
    }

    #[test]
    fn directed_cycle_is_vertex_symmetric() {
        // A 100-node directed cycle is vertex-transitive: every node reaches the
        // other 99 at distances 1..=99, so closeness is identical for all. Exercises
        // per-thread scratch reuse across many sources; a missed reset would break
        // the symmetry.
        let n = 100u32;
        let src: Vec<u32> = (0..n).collect();
        let dst: Vec<u32> = (0..n).map(|i| (i + 1) % n).collect();
        let t = Topology::build(n as usize, src, dst);
        let c = closeness(&t, None);
        for &x in &c {
            assert!((x - c[0]).abs() < 1e-12, "cycle closeness must be uniform");
        }
        assert!(c[0] > 0.0);
    }

    #[test]
    fn weighted_uses_edge_costs() {
        // 0->1 (cost 1), 1->2 (cost 1), 0->2 (cost 5). From node 0: dist to 1 is 1,
        // to 2 is 2 (via 1, cheaper than the direct 5). reachable 2, Σ 3 -> 2/3.
        let t = Topology::build(3, vec![0, 1, 0], vec![1, 2, 2]);
        let c = closeness_weighted(&t, &[1.0, 1.0, 5.0], None);
        assert!((c[0] - 2.0 / 3.0).abs() < 1e-12, "got {}", c[0]);
        // From node 1: only 2 reachable at cost 1 -> 1/1 = 1.
        assert!((c[1] - 1.0).abs() < 1e-12, "got {}", c[1]);
    }

    #[test]
    fn zero_weight_reachable_node_is_counted() {
        // 0 ->(0.0) 1 ->(1.0) 2. From node 0, node 1 is reachable at total cost
        // exactly 0.0. It must be counted like any reachable non-source node (as the
        // unweighted kernel counts it): reachable = 2 (nodes 1 and 2), Σ dist = 0 + 1
        // = 1  ->  2/1 = 2.0. (A stray `d > 0.0` guard would drop node 1 and give
        // 1/1 = 1.0 — the divergence this reconciles.)
        let t = Topology::build(3, vec![0, 1], vec![1, 2]);
        let c = closeness_weighted(&t, &[0.0, 1.0], None);
        assert!((c[0] - 2.0).abs() < 1e-12, "got {}", c[0]);
        // From node 1: only node 2 reachable at cost 1 -> 1/1 = 1.
        assert!((c[1] - 1.0).abs() < 1e-12, "got {}", c[1]);
    }

    #[test]
    fn weighted_sum_matches_the_node_order_scan_bit_for_bit() {
        // The reached-set sum must equal the original full `0..n` scan exactly, on
        // a sparse graph (small reaches: the sorted reached-set path) and a dense
        // one (large reaches: the scan path), in every direction.
        for (n, m) in [(2000u32, 1500usize), (200, 2000)] {
            let mut state: u64 = 0x2545_F491_4F6C_DD1D;
            let mut next = move || {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u32
            };
            let (mut src, mut dst, mut wt) = (Vec::new(), Vec::new(), Vec::new());
            for _ in 0..m {
                src.push(next() % n);
                dst.push(next() % n);
                wt.push(0.1 + (next() % 1000) as f64 / 3.0);
            }
            let t = Topology::build(n as usize, src, dst);
            for dir in [Direction::Out, Direction::In, Direction::Both] {
                let got = closeness_weighted_directed(&t, &wt, None, dir);
                let mut sc = ClosenessWeightedScratch::new(n as usize);
                for u in 0..n {
                    // Run the kernel's Dijkstra, then fold the full dist array in
                    // node order as the original did.
                    closeness_weighted_one(&t, &wt, None, dir, u, &mut sc);
                    let mut dist = vec![f64::INFINITY; n as usize];
                    let mut heap = BinaryHeap::new();
                    let mut settled = vec![false; n as usize];
                    dist[u as usize] = 0.0;
                    heap.push(Reverse((TotalF64(0.0), u)));
                    while let Some(Reverse((TotalF64(du), v))) = heap.pop() {
                        if std::mem::replace(&mut settled[v as usize], true) {
                            continue;
                        }
                        t.for_each_weighted_neighbor(v, &wt, dir, None, |w, c| {
                            if !settled[w as usize] && du + c < dist[w as usize] {
                                dist[w as usize] = du + c;
                                heap.push(Reverse((TotalF64(du + c), w)));
                            }
                        });
                    }
                    let (mut sum, mut reachable) = (0.0f64, 0i64);
                    for (v, &d) in dist.iter().enumerate() {
                        if v != u as usize && d.is_finite() {
                            sum += d;
                            reachable += 1;
                        }
                    }
                    let want = if sum > 0.0 {
                        reachable as f64 / sum
                    } else {
                        0.0
                    };
                    assert_eq!(
                        got[u as usize].to_bits(),
                        want.to_bits(),
                        "n={n} {dir:?} u={u}"
                    );
                }
            }
        }
    }

    #[test]
    fn batched_kernel_equals_one_search_per_source_bit_for_bit() {
        // A sparse random graph with partial reaches, parallel edges, self-loops
        // and 50 isolated nodes, n not a multiple of 64; every direction, masked
        // and not, against the kernel the batched one replaced.
        let (n, m) = (777u32, 1200usize);
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for _ in 0..m {
            src.push(next() % (n - 50));
            dst.push(next() % (n - 50));
        }
        let keep: Vec<bool> = (0..m).map(|_| next() % 4 != 0).collect();
        let mask = EdgeMask::from_bools(&keep);
        let t = Topology::build(n as usize, src, dst);
        for dir in [Direction::Out, Direction::In, Direction::Both] {
            for mask in [None, Some(&mask)] {
                let got = closeness_directed(&t, mask, dir);
                let mut sc = ClosenessScratch::new(n as usize);
                for u in 0..n {
                    let want = closeness_one(&t, mask, dir, u, &mut sc);
                    assert_eq!(got[u as usize].to_bits(), want.to_bits(), "{dir:?} u={u}");
                }
            }
        }
    }

    #[test]
    fn all_zero_cost_reachable_falls_back_to_zero() {
        // Both out-edges weight 0: from node 0 the reachable set has total cost 0,
        // so the `sum > 0.0` guard returns 0.0 rather than dividing by zero.
        let t = Topology::build(3, vec![0, 0], vec![1, 2]);
        let c = closeness_weighted(&t, &[0.0, 0.0], None);
        assert_eq!(c[0], 0.0);
    }
}
