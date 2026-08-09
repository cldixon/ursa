//! Single-source breadth-first search — the frontier kernel behind unweighted
//! `shortest_path` and the BFS-derived stats (`diameter`, `avg_path_length`).
//!
//! Two variants share one frontier loop: [`bfs_distances`] records the hop
//! distance to every node (for the stats), and [`shortest_path`] additionally
//! keeps predecessor pointers so a single source→target path can be
//! reconstructed. Both run to exhaustion (no `k` cap, unlike `k_hop`).
//!
//! Direction is a per-operation parameter. `Both` is a true undirected walk over
//! out- and in-neighbours, via the shared [`Topology::for_each_neighbor`] /
//! [`Topology::for_each_weighted_neighbor`] helpers.

use crate::topology::{Direction, Topology};

/// Hop distance from `source` to every node: `dist[v]` is the number of edges on
/// a shortest `source → v` path, or `-1` if `v` is unreachable. `dist[source]` is
/// `0`. An out-of-range `source` yields an all-`-1` vector.
pub fn bfs_distances(topo: &Topology, source: u32, dir: Direction) -> Vec<i32> {
    let n = topo.n_nodes();
    let mut dist = vec![-1i32; n];
    if (source as usize) >= n {
        return dist;
    }
    dist[source as usize] = 0;
    let mut frontier = vec![source];
    let mut next = Vec::new();
    let mut level = 1;
    while !frontier.is_empty() {
        next.clear();
        for &u in &frontier {
            topo.for_each_neighbor(u, dir, |v| {
                if dist[v as usize] < 0 {
                    dist[v as usize] = level;
                    next.push(v);
                }
            });
        }
        level += 1;
        std::mem::swap(&mut frontier, &mut next);
    }
    dist
}

/// The unweighted shortest path from `source` to `target` as an inclusive dense
/// node sequence `[source, .., target]`, or `None` if `target` is unreachable.
/// `source == target` yields `Some(vec![source])` (a zero-edge path). Out-of-range
/// endpoints yield `None`.
pub fn shortest_path(
    topo: &Topology,
    source: u32,
    target: u32,
    dir: Direction,
) -> Option<Vec<u32>> {
    let n = topo.n_nodes();
    if (source as usize) >= n || (target as usize) >= n {
        return None;
    }
    if source == target {
        return Some(vec![source]);
    }

    // parent[v] == -1: unvisited; parent[source] == source (its own root).
    let mut parent = vec![-1i64; n];
    parent[source as usize] = source as i64;
    let mut frontier = vec![source];
    let mut next = Vec::new();
    let mut found = false;

    'search: while !frontier.is_empty() {
        next.clear();
        for &u in &frontier {
            let mut hit = false;
            topo.for_each_neighbor(u, dir, |v| {
                if parent[v as usize] < 0 {
                    parent[v as usize] = u as i64;
                    if v == target {
                        hit = true;
                    }
                    next.push(v);
                }
            });
            if hit {
                found = true;
                break 'search;
            }
        }
        std::mem::swap(&mut frontier, &mut next);
    }

    if !found {
        return None;
    }
    // Backtrack target → source, then reverse into forward order.
    let mut path = vec![target];
    let mut cur = target;
    while cur != source {
        cur = parent[cur as usize] as u32;
        path.push(cur);
    }
    path.reverse();
    Some(path)
}

/// Total order over `f64` (only `PartialOrd` by default) so distances can key a
/// `BinaryHeap`. Uses `total_cmp`, which orders NaN consistently; real distances
/// here are non-negative and finite. Shared by the weighted kernels (Dijkstra
/// distances, weighted Brandes).
#[derive(PartialEq)]
pub(crate) struct TotalF64(pub(crate) f64);
impl Eq for TotalF64 {}
impl PartialOrd for TotalF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TotalF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Weighted single-source distances (Dijkstra): `dist[v]` is the minimum total
/// edge weight from `source` to `v`, `+INFINITY` if unreachable, `0.0` for the
/// source. The weighted analogue of [`bfs_distances`], reused by weighted
/// closeness. `weights` must be non-negative (validated by the caller).
pub fn dijkstra_distances(
    topo: &Topology,
    weights: &[f64],
    source: u32,
    dir: Direction,
) -> Vec<f64> {
    assert_eq!(
        weights.len(),
        topo.n_edges(),
        "weights length must equal the edge count"
    );
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let n = topo.n_nodes();
    let mut dist = vec![f64::INFINITY; n];
    if (source as usize) >= n {
        return dist;
    }
    let mut settled = vec![false; n];
    dist[source as usize] = 0.0;
    let mut heap = BinaryHeap::new();
    heap.push(Reverse((TotalF64(0.0), source)));

    while let Some(Reverse((TotalF64(du), u))) = heap.pop() {
        if settled[u as usize] {
            continue;
        }
        settled[u as usize] = true;
        topo.for_each_weighted_neighbor(u, weights, dir, |v, w| {
            if !settled[v as usize] {
                let nd = du + w;
                if nd < dist[v as usize] {
                    dist[v as usize] = nd;
                    heap.push(Reverse((TotalF64(nd), v)));
                }
            }
        });
    }
    dist
}

/// Weighted single-pair shortest path (Dijkstra). Returns the inclusive dense node
/// sequence `[source, .., target]` of minimum total edge weight, or `None` if
/// `target` is unreachable. `source == target` yields `Some(vec![source])`;
/// out-of-range endpoints yield `None`. Mirrors [`shortest_path`]'s return shape.
///
/// `weights[e]` is the weight of original edge row `e` (`weights.len() ==
/// topo.n_edges()`), gathered per CSR slot via `Adjacency::edge_ids`. Weights must
/// be non-negative (Dijkstra's requirement); the caller validates this.
pub fn shortest_path_weighted(
    topo: &Topology,
    weights: &[f64],
    source: u32,
    target: u32,
    dir: Direction,
) -> Option<Vec<u32>> {
    shortest_path_weighted_with_cost(topo, weights, source, target, dir).map(|(path, _)| path)
}

/// Like [`shortest_path_weighted`], but also returns the cumulative cost to each
/// node on the route: `costs[i]` is the minimum total edge weight from `source` to
/// `path[i]`, so `costs[0] == 0.0` and `costs.last()` is the whole path's cost.
/// These are the exact `dist[]` values Dijkstra already settled (correct even when
/// parallel edges connect two nodes — the settled distance reflects the minimum),
/// read off for free during backtrack rather than recomputed.
pub fn shortest_path_weighted_with_cost(
    topo: &Topology,
    weights: &[f64],
    source: u32,
    target: u32,
    dir: Direction,
) -> Option<(Vec<u32>, Vec<f64>)> {
    assert_eq!(
        weights.len(),
        topo.n_edges(),
        "weights length must equal the edge count"
    );
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let n = topo.n_nodes();
    if (source as usize) >= n || (target as usize) >= n {
        return None;
    }
    if source == target {
        return Some((vec![source], vec![0.0]));
    }

    let mut dist = vec![f64::INFINITY; n];
    let mut parent = vec![-1i64; n];
    let mut settled = vec![false; n];
    dist[source as usize] = 0.0;
    parent[source as usize] = source as i64;

    // Min-heap on tentative distance.
    let mut heap = BinaryHeap::new();
    heap.push(Reverse((TotalF64(0.0), source)));

    while let Some(Reverse((TotalF64(du), u))) = heap.pop() {
        if settled[u as usize] {
            continue; // stale heap entry
        }
        settled[u as usize] = true;
        if u == target {
            break; // settled the target — its distance is final
        }
        topo.for_each_weighted_neighbor(u, weights, dir, |v, w| {
            if !settled[v as usize] {
                let nd = du + w;
                if nd < dist[v as usize] {
                    dist[v as usize] = nd;
                    parent[v as usize] = u as i64;
                    heap.push(Reverse((TotalF64(nd), v)));
                }
            }
        });
    }

    if !settled[target as usize] {
        return None; // unreachable
    }
    // Backtrack target -> source, then reverse into forward order.
    let mut path = vec![target];
    let mut cur = target;
    while cur != source {
        cur = parent[cur as usize] as u32;
        path.push(cur);
    }
    path.reverse();
    // Cumulative cost to each node on the route, in the same forward order.
    let costs: Vec<f64> = path.iter().map(|&v| dist[v as usize]).collect();
    Some((path, costs))
}

#[cfg(test)]
mod tests {
    use super::*;

    // path 0 -> 1 -> 2 -> 3, plus a branch 1 -> 4
    fn path() -> Topology {
        Topology::build(5, vec![0, 1, 2, 1], vec![1, 2, 3, 4])
    }

    #[test]
    fn distances_along_a_path() {
        let t = path();
        let d = bfs_distances(&t, 0, Direction::Out);
        assert_eq!(d[0], 0);
        assert_eq!(d[1], 1);
        assert_eq!(d[2], 2);
        assert_eq!(d[3], 3);
        assert_eq!(d[4], 2); // 0 -> 1 -> 4
    }

    #[test]
    fn unreachable_is_negative_one() {
        let t = path();
        // node 3 has no out-edges; from 3 nothing else is reachable
        let d = bfs_distances(&t, 3, Direction::Out);
        assert_eq!(d[3], 0);
        assert_eq!(d[0], -1);
        assert_eq!(d[1], -1);
    }

    #[test]
    fn both_direction_merges_adjacencies() {
        let t = path();
        // undirected: from 3 we can walk back to 0 (dist 3) and to 4 (3->2->1->4 = 3)
        let d = bfs_distances(&t, 3, Direction::Both);
        assert_eq!(d[0], 3);
        assert_eq!(d[4], 3);
    }

    #[test]
    fn shortest_path_reconstructs_the_route() {
        let t = path();
        assert_eq!(
            shortest_path(&t, 0, 3, Direction::Out),
            Some(vec![0, 1, 2, 3])
        );
        assert_eq!(shortest_path(&t, 0, 4, Direction::Out), Some(vec![0, 1, 4]));
    }

    #[test]
    fn shortest_path_source_equals_target() {
        let t = path();
        assert_eq!(shortest_path(&t, 2, 2, Direction::Out), Some(vec![2]));
    }

    #[test]
    fn shortest_path_unreachable_is_none() {
        let t = path();
        // no directed path 3 -> 0
        assert_eq!(shortest_path(&t, 3, 0, Direction::Out), None);
        // out-of-range endpoint
        assert_eq!(shortest_path(&t, 0, 99, Direction::Out), None);
    }

    #[test]
    fn shortest_path_backward_via_in_edges() {
        let t = path();
        assert_eq!(
            shortest_path(&t, 3, 0, Direction::In),
            Some(vec![3, 2, 1, 0])
        );
    }

    #[test]
    fn weighted_prefers_cheaper_longer_route() {
        // 0->1->3 (cost 1+1=2, two hops) vs the direct 0->3 (cost 5, one hop).
        // Unweighted BFS takes the single hop; Dijkstra takes the cheaper route.
        // edge rows: 0=(0->1), 1=(1->3), 2=(0->3)
        let t = Topology::build(4, vec![0, 1, 0], vec![1, 3, 3]);
        assert_eq!(shortest_path(&t, 0, 3, Direction::Out), Some(vec![0, 3]));
        assert_eq!(
            shortest_path_weighted(&t, &[1.0, 1.0, 5.0], 0, 3, Direction::Out),
            Some(vec![0, 1, 3])
        );
        // Make the direct edge cheap and the two-hop route expensive -> flips back.
        assert_eq!(
            shortest_path_weighted(&t, &[5.0, 5.0, 1.0], 0, 3, Direction::Out),
            Some(vec![0, 3])
        );
    }

    #[test]
    fn weighted_source_equals_target_and_unreachable() {
        let t = Topology::build(4, vec![0, 1, 0], vec![1, 3, 3]);
        assert_eq!(
            shortest_path_weighted(&t, &[1.0, 1.0, 5.0], 2, 2, Direction::Out),
            Some(vec![2])
        );
        // node 2 is isolated -> unreachable target
        assert_eq!(
            shortest_path_weighted(&t, &[1.0, 1.0, 5.0], 0, 2, Direction::Out),
            None
        );
    }

    #[test]
    fn weighted_with_cost_reports_cumulative_distance() {
        // 0->1 (1), 1->3 (1), 0->3 (5). Cheapest 0->3 is via 1 at total cost 2.
        let t = Topology::build(4, vec![0, 1, 0], vec![1, 3, 3]);
        let (path, costs) =
            shortest_path_weighted_with_cost(&t, &[1.0, 1.0, 5.0], 0, 3, Direction::Out).unwrap();
        assert_eq!(path, vec![0, 1, 3]);
        // Cumulative cost to each node on the route: source 0.0, then 1.0, then 2.0.
        assert_eq!(costs, vec![0.0, 1.0, 2.0]);
        // source == target: single node, zero cost.
        assert_eq!(
            shortest_path_weighted_with_cost(&t, &[1.0, 1.0, 5.0], 0, 0, Direction::Out),
            Some((vec![0], vec![0.0]))
        );
        // A parallel cheaper edge is honored: add a second 0->3 at cost 0.5.
        let t2 = Topology::build(4, vec![0, 1, 0, 0], vec![1, 3, 3, 3]);
        let (p2, c2) =
            shortest_path_weighted_with_cost(&t2, &[1.0, 1.0, 5.0, 0.5], 0, 3, Direction::Out)
                .unwrap();
        assert_eq!(p2, vec![0, 3]);
        assert_eq!(c2, vec![0.0, 0.5]);
    }
}
