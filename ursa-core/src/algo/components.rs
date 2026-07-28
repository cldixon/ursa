//! Connected components: weak (union-find over the undirected view) and strong
//! (iterative Tarjan over out-edges).
//!
//! Weak connectivity treats edges as undirected via union-find with path
//! compression + union by rank. The GAP-canonical choice at scale is Afforest
//! (sampled subgraph + link); this straightforward union-find is the correct,
//! simple v0.1 kernel and the natural baseline to benchmark Afforest against
//! later. Strong connectivity ([`connected_components_strong`]) groups nodes that
//! are mutually reachable, computed by iterative Tarjan (an explicit DFS stack, so
//! deep graphs don't overflow the native call stack).

use crate::topology::Topology;

struct DisjointSet {
    parent: Vec<u32>,
    rank: Vec<u8>,
}

impl DisjointSet {
    fn new(n: usize) -> Self {
        DisjointSet {
            parent: (0..n as u32).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: u32) -> u32 {
        // Path compression by halving.
        while self.parent[x as usize] != x {
            let gp = self.parent[self.parent[x as usize] as usize];
            self.parent[x as usize] = gp;
            x = gp;
        }
        x
    }

    fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (ra_rank, rb_rank) = (self.rank[ra as usize], self.rank[rb as usize]);
        let (small, big) = if ra_rank < rb_rank {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small as usize] = big;
        if ra_rank == rb_rank {
            self.rank[big as usize] += 1;
        }
    }
}

/// Weak connected component label per node, dense-indexed. Labels are the dense
/// index of each component's representative (not necessarily contiguous `0..k`);
/// callers that want canonical small ids can densify the output.
pub fn connected_components_weak(topo: &Topology) -> Vec<u32> {
    let n = topo.n_nodes();
    let mut ds = DisjointSet::new(n);
    let out = topo.out();
    for u in 0..n as u32 {
        for &v in out.neighbors(u) {
            ds.union(u, v);
        }
    }
    (0..n as u32).map(|u| ds.find(u)).collect()
}

/// Strongly connected component label per node, dense-indexed, via **iterative
/// Tarjan** (an explicit DFS stack, so deep graphs cannot overflow the call
/// stack). Follows out-edges only — the transpose is not needed. Two nodes share a
/// label iff each is reachable from the other. Labels are the dense index of each
/// SCC's Tarjan root: an arbitrary but stable representative, matching the
/// weak-component labeling convention. `O(n + m)`.
pub fn connected_components_strong(topo: &Topology) -> Vec<u32> {
    const UNVISITED: i64 = -1;

    let n = topo.n_nodes();
    let out = topo.out();

    let mut index = vec![UNVISITED; n]; // discovery order per node
    let mut lowlink = vec![0i64; n];
    let mut on_stack = vec![false; n];
    let mut labels = vec![0u32; n];
    let mut scc_stack: Vec<u32> = Vec::new();
    let mut next_index: i64 = 0;

    // Explicit DFS: each frame is (node, cursor into its out-neighbours). Replaces
    // recursion so a long path can't blow the native stack.
    let mut call_stack: Vec<(u32, usize)> = Vec::new();

    for start in 0..n as u32 {
        if index[start as usize] != UNVISITED {
            continue;
        }
        call_stack.push((start, 0));
        while let Some(&(v, cursor)) = call_stack.last() {
            if cursor == 0 {
                // First time we reach v: stamp its index/lowlink, push to SCC stack.
                index[v as usize] = next_index;
                lowlink[v as usize] = next_index;
                next_index += 1;
                scc_stack.push(v);
                on_stack[v as usize] = true;
            }
            let neighbors = out.neighbors(v);
            if cursor < neighbors.len() {
                let w = neighbors[cursor];
                call_stack.last_mut().unwrap().1 = cursor + 1;
                if index[w as usize] == UNVISITED {
                    call_stack.push((w, 0)); // descend into w
                } else if on_stack[w as usize] {
                    // Edge to a node still on the SCC stack: relax v's lowlink.
                    lowlink[v as usize] = lowlink[v as usize].min(index[w as usize]);
                }
            } else {
                // v's out-edges are exhausted; it roots an SCC iff lowlink == index.
                if lowlink[v as usize] == index[v as usize] {
                    loop {
                        let w = scc_stack.pop().expect("scc stack non-empty at a root");
                        on_stack[w as usize] = false;
                        labels[w as usize] = v; // representative = the root v
                        if w == v {
                            break;
                        }
                    }
                }
                call_stack.pop();
                // Propagate v's lowlink up the DFS-tree edge to its parent.
                if let Some(&(parent, _)) = call_stack.last() {
                    lowlink[parent as usize] = lowlink[parent as usize].min(lowlink[v as usize]);
                }
            }
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_components() {
        // {0,1,2} connected; {3,4} connected; edges directed but weak conn.
        let t = Topology::build(5, vec![0, 1, 3], vec![1, 2, 4]);
        let cc = connected_components_weak(&t);
        assert_eq!(cc[0], cc[1]);
        assert_eq!(cc[1], cc[2]);
        assert_eq!(cc[3], cc[4]);
        assert_ne!(cc[0], cc[3]);
    }

    #[test]
    fn strong_components_partition_directed_cycles() {
        // SCCs: {0,1,2} (a 3-cycle), {3,4} (a 2-cycle), {5} (isolated).
        // 2->3 links the first two but only one way, so they stay distinct.
        let t = Topology::build(6, vec![0, 1, 2, 2, 3, 4], vec![1, 2, 0, 3, 4, 3]);
        let scc = connected_components_strong(&t);
        assert_eq!(scc[0], scc[1]);
        assert_eq!(scc[1], scc[2]);
        assert_eq!(scc[3], scc[4]);
        // The three components are mutually distinct.
        assert_ne!(scc[0], scc[3]);
        assert_ne!(scc[0], scc[5]);
        assert_ne!(scc[3], scc[5]);
    }

    #[test]
    fn strong_differs_from_weak_on_a_directed_chain() {
        // 0->1->2: weakly one component, but strongly three singletons (no cycle).
        let t = Topology::build(3, vec![0, 1], vec![1, 2]);
        let weak = connected_components_weak(&t);
        assert_eq!(weak[0], weak[1]);
        assert_eq!(weak[1], weak[2]);
        let strong = connected_components_strong(&t);
        assert_ne!(strong[0], strong[1]);
        assert_ne!(strong[1], strong[2]);
        assert_ne!(strong[0], strong[2]);
    }
}
