//! Weak connected components via union-find with path compression + union by rank.
//!
//! Treats edges as undirected (weak connectivity). The GAP-canonical choice at
//! scale is Afforest (sampled subgraph + link); this straightforward union-find
//! is the correct, simple v0.1 kernel and the natural baseline to benchmark
//! Afforest against later. Strong components are a later release.

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
        let (small, big) = if ra_rank < rb_rank { (ra, rb) } else { (rb, ra) };
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
}
