//! Single-source breadth-first search — the frontier kernel behind unweighted
//! `shortest_path` and the BFS-derived stats (`diameter`, `avg_path_length`).
//!
//! Two variants share one frontier loop: [`bfs_distances`] records the hop
//! distance to every node (for the stats), and [`shortest_path`] additionally
//! keeps predecessor pointers so a single source→target path can be
//! reconstructed. Both run to exhaustion (no `k` cap, unlike `k_hop`).
//!
//! Direction is a per-operation parameter. `Both` is a true undirected walk:
//! `Topology::adjacency(Both)` returns out-adjacency only, so `Both` explicitly
//! visits out- and in-neighbours (mirroring `hop::push_neighbors`).

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
            visit_neighbors(topo, u, dir, |v| {
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
            visit_neighbors(topo, u, dir, |v| {
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

/// Apply `f` to each neighbour of `u` in `dir` (both adjacencies for `Both`).
#[inline]
fn visit_neighbors<F: FnMut(u32)>(topo: &Topology, u: u32, dir: Direction, mut f: F) {
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
}
