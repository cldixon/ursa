//! Closeness centrality — a BFS from every node, folding finite distances.
//!
//! The cheapest of the centralities: a BFS (hop count) or Dijkstra (edge weight)
//! from each source, parallelised over sources with Rayon. Each rayon chunk
//! allocates its traversal scratch **once** and resets only the nodes it touched
//! between sources, so the per-source distance buffer (and the Dijkstra heap /
//! settled flags) is allocated once per split instead of once per source. Results
//! are byte-identical: the unweighted fold is an exact `i64` sum (order does not
//! matter), and the weighted fold keeps the original `0..n` node-order summation
//! so its `f64` total is bit-for-bit unchanged.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use rayon::prelude::*;

use super::bfs::TotalF64;
use crate::topology::{Direction, Topology};

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
pub fn closeness(topo: &Topology) -> Vec<f64> {
    let n = topo.n_nodes();
    // `map_init` keeps the per-source work-stealing parallelism (each node writes
    // its own output slot, so there is no cross-source ordering to preserve) while
    // building the scratch once per rayon thread and reusing it across that
    // thread's sources.
    (0..n as u32)
        .into_par_iter()
        .map_init(
            || ClosenessScratch::new(n),
            |sc, u| closeness_one(topo, u, sc),
        )
        .collect()
}

/// One unweighted closeness value, over reusable scratch (clean on entry; the
/// touched nodes are reset to clean on exit). The `i64` distance sum is exact, so
/// summing the reached set (rather than scanning all `n` in node order) gives an
/// identical result.
fn closeness_one(topo: &Topology, u: u32, sc: &mut ClosenessScratch) -> f64 {
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
            topo.for_each_neighbor(v, Direction::Out, |w| {
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

/// Weighted closeness centrality: as [`closeness`], but distances are minimum
/// total edge weight (Dijkstra) rather than hop count. `reachable / Σ dist(u, v)`
/// over the reachable nodes at finite positive distance. `weights[e]` is the
/// weight of edge row `e` (non-negative; `len == n_edges`).
pub fn closeness_weighted(topo: &Topology, weights: &[f64]) -> Vec<f64> {
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
            |sc, u| closeness_weighted_one(topo, weights, u, sc),
        )
        .collect()
}

/// One weighted closeness value, over reusable scratch. The `f64` distance sum
/// keeps the original `0..n` node-order scan so the total is bit-for-bit identical
/// to the allocate-per-source version (float addition is not associative).
fn closeness_weighted_one(
    topo: &Topology,
    weights: &[f64],
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
        topo.for_each_weighted_neighbor(v, weights, Direction::Out, |w, wt| {
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
    // Node-order (0..n) scan — the exact summation order of the original, so the
    // f64 total is bit-for-bit unchanged.
    for (v, &d) in dist.iter().enumerate() {
        if v != u as usize && d.is_finite() && d > 0.0 {
            sum += d;
            reachable += 1;
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

    #[test]
    fn closeness_of_a_directed_triangle() {
        // 0->1->2->0: from each node, distances {1, 2} to the other two.
        // reachable = 2, Σ dist = 3  ->  2/3.
        let t = Topology::build(3, vec![0, 1, 2], vec![1, 2, 0]);
        let c = closeness(&t);
        for &x in &c {
            assert!((x - 2.0 / 3.0).abs() < 1e-12, "got {x}");
        }
    }

    #[test]
    fn closeness_on_a_directed_line() {
        // 0->1->2->3. Node 0 reaches {1,2,3} at {1,2,3}: 3/6 = 0.5.
        // Node 2 reaches {3} at {1}: 1/1 = 1.0. Node 3 reaches nothing: 0.0.
        let t = Topology::build(4, vec![0, 1, 2], vec![1, 2, 3]);
        let c = closeness(&t);
        assert!((c[0] - 0.5).abs() < 1e-12, "got {}", c[0]);
        assert!((c[1] - 2.0 / 3.0).abs() < 1e-12, "got {}", c[1]);
        assert!((c[2] - 1.0).abs() < 1e-12, "got {}", c[2]);
        assert_eq!(c[3], 0.0);
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(closeness(&t).is_empty());
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
        let c = closeness(&t);
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
        let c = closeness_weighted(&t, &[1.0, 1.0, 5.0]);
        assert!((c[0] - 2.0 / 3.0).abs() < 1e-12, "got {}", c[0]);
        // From node 1: only 2 reachable at cost 1 -> 1/1 = 1.
        assert!((c[1] - 1.0).abs() < 1e-12, "got {}", c[1]);
    }
}
