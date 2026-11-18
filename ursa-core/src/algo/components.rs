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

use super::linked::Linked;
use crate::topology::{EdgeMask, Topology};

/// The linked nodes a components kernel runs over, when enough nodes are
/// isolated to be worth it (#326). An isolated node is a component of its own,
/// labelled by itself, in either kernel: with the compact ids in node order,
/// every union (weak) and every discovery-order comparison (strong) is the
/// full run's, so a compact run's labels, mapped back to nodes, are too.
fn linked_nodes(topo: &Topology, mask: Option<&EdgeMask>) -> Option<Linked> {
    let linked = Linked::of(topo, mask);
    linked.worth_it().then_some(linked)
}

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
pub fn connected_components_weak(topo: &Topology, mask: Option<&EdgeMask>) -> Vec<u32> {
    weak_over(topo, mask, linked_nodes(topo, mask).as_ref())
}

/// [`connected_components_weak`], compact over `linked` when given.
fn weak_over(topo: &Topology, mask: Option<&EdgeMask>, linked: Option<&Linked>) -> Vec<u32> {
    let width = linked.map_or(topo.n_nodes(), Linked::len);
    let node = |u: u32| linked.map_or(u, |l| l.nodes()[u as usize]);
    let at = |v: u32| linked.map_or(v, |l| l.compact(v));
    let mut ds = DisjointSet::new(width);
    let out = topo.out();
    for u in 0..width as u32 {
        let uo = node(u);
        match mask {
            None => {
                for &v in out.neighbors(uo) {
                    ds.union(u, at(v));
                }
            }
            // Subgraph: only kept edges connect their endpoints.
            Some(m) => {
                for (&v, &e) in out.neighbors(uo).iter().zip(out.edge_ids(uo)) {
                    if m.keep(e) {
                        ds.union(u, at(v));
                    }
                }
            }
        }
    }
    let labels: Vec<u32> = (0..width as u32).map(|u| ds.find(u)).collect();
    match linked {
        None => labels,
        Some(l) => l.expand_labels(&labels),
    }
}

/// Strongly connected component label per node, dense-indexed, via **iterative
/// Tarjan** (an explicit DFS stack, so deep graphs cannot overflow the call
/// stack). Follows out-edges only — the transpose is not needed. Two nodes share a
/// label iff each is reachable from the other. Labels are the dense index of each
/// SCC's Tarjan root: an arbitrary but stable representative, matching the
/// weak-component labeling convention. `O(n + m)`.
pub fn connected_components_strong(topo: &Topology, mask: Option<&EdgeMask>) -> Vec<u32> {
    strong_over(topo, mask, linked_nodes(topo, mask).as_ref())
}

/// [`connected_components_strong`], compact over `linked` when given.
fn strong_over(topo: &Topology, mask: Option<&EdgeMask>, linked: Option<&Linked>) -> Vec<u32> {
    // Discovery indices are `< n`, which fits `u32` (dense ids are `u32`), so the
    // max value is free to mark an unvisited node.
    const UNVISITED: u32 = u32::MAX;

    let n = linked.map_or(topo.n_nodes(), Linked::len);
    let node = |u: u32| linked.map_or(u, |l| l.nodes()[u as usize]);
    let at = |v: u32| linked.map_or(v, |l| l.compact(v));
    let out = topo.out();

    let mut index = vec![UNVISITED; n]; // discovery order per node
    let mut lowlink = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut labels = vec![0u32; n];
    let mut scc_stack: Vec<u32> = Vec::new();
    let mut next_index: u32 = 0;

    // Explicit DFS: each frame is (node, cursor into its out-neighbours). Replaces
    // recursion so a long path can't blow the native stack. A cursor fits `u32`
    // because a node's degree is at most the edge count, capped at `u32::MAX`.
    let mut call_stack: Vec<(u32, u32)> = Vec::new();

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
            let neighbors = out.neighbors(node(v));
            if (cursor as usize) < neighbors.len() {
                call_stack.last_mut().unwrap().1 = cursor + 1;
                // Subgraph: an edge masked out is not traversed (advance past it).
                if mask.is_some_and(|m| !m.keep(out.edge_ids(node(v))[cursor as usize])) {
                    continue;
                }
                let w = at(neighbors[cursor as usize]);
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
    match linked {
        None => labels,
        Some(l) => l.expand_labels(&labels),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compact runs (#326) give the full runs' labels.
    #[test]
    fn compact_runs_give_the_full_labels() {
        use crate::algo::linked::testing::{sparse, CASES};
        for &(seed, n, m) in CASES {
            let (t, mask) = sparse(seed, n, m);
            for mask in [None, Some(&mask)] {
                let linked = Linked::of(&t, mask);
                assert_eq!(
                    weak_over(&t, mask, Some(&linked)),
                    weak_over(&t, mask, None),
                    "weak: graph {seed}, masked {}",
                    mask.is_some()
                );
                assert_eq!(
                    strong_over(&t, mask, Some(&linked)),
                    strong_over(&t, mask, None),
                    "strong: graph {seed}, masked {}",
                    mask.is_some()
                );
            }
        }
    }

    #[test]
    fn two_components() {
        // {0,1,2} connected; {3,4} connected; edges directed but weak conn.
        let t = Topology::build(5, vec![0, 1, 3], vec![1, 2, 4]);
        let cc = connected_components_weak(&t, None);
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
        let scc = connected_components_strong(&t, None);
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
        let weak = connected_components_weak(&t, None);
        assert_eq!(weak[0], weak[1]);
        assert_eq!(weak[1], weak[2]);
        let strong = connected_components_strong(&t, None);
        assert_ne!(strong[0], strong[1]);
        assert_ne!(strong[1], strong[2]);
        assert_ne!(strong[0], strong[2]);
    }

    /// Exact outputs pinned against the serial kernels as they were before the
    /// parallel versions (#383): a label per node on small graphs, and an FNV-1a
    /// hash of the whole label vector on generated ones. Any change to a label,
    /// not only to the partition, fails here.
    mod pinned {
        use super::*;
        use crate::algo::linked::testing::{next, sparse};

        pub(crate) fn fnv(labels: &[u32]) -> u64 {
            labels.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &l| {
                l.to_le_bytes()
                    .iter()
                    .fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
            })
        }

        /// A random permutation of `0..n`.
        fn perm(st: &mut u64, n: usize) -> Vec<u32> {
            let mut p: Vec<u32> = (0..n as u32).collect();
            for i in (1..n).rev() {
                p.swap(i, (next(st) % (i as u64 + 1)) as usize);
            }
            p
        }

        /// `m` uniform random edges over `n` nodes.
        fn uniform(seed: u64, n: usize, m: usize) -> Topology {
            let mut st = seed;
            let (s, d): (Vec<u32>, Vec<u32>) = (0..m)
                .map(|_| {
                    let a = (next(&mut st) % n as u64) as u32;
                    (a, (next(&mut st) % n as u64) as u32)
                })
                .unzip();
            Topology::build(n, s, d)
        }

        /// `m` R-MAT edges over `2^scale` nodes (many left isolated).
        fn rmat(seed: u64, scale: u32, m: usize) -> Topology {
            let mut st = seed;
            let (a, ab, abc) = (0.57, 0.76, 0.95);
            let (s, d): (Vec<u32>, Vec<u32>) = (0..m)
                .map(|_| {
                    let (mut s, mut d) = (0u32, 0u32);
                    for _ in 0..scale {
                        let r = (next(&mut st) >> 11) as f64 / (1u64 << 53) as f64;
                        s = s << 1 | u32::from(r >= ab);
                        d = d << 1 | u32::from((r >= a && r < ab) || r >= abc);
                    }
                    (s, d)
                })
                .unzip();
            Topology::build(1 << scale, s, d)
        }

        /// Many small components (paths, cycles, stars, cliques, self-loops,
        /// lone nodes) of 1 to 9 nodes, their ids scattered by a permutation.
        fn small_components(seed: u64, n: usize) -> Topology {
            let mut st = seed;
            let p = perm(&mut st, n);
            let (mut s, mut d) = (Vec::new(), Vec::new());
            let mut at = 0usize;
            while at < n {
                let k = (1 + next(&mut st) % 9).min((n - at) as u64) as usize;
                let g = &p[at..at + k];
                match next(&mut st) % 5 {
                    0 => (1..k).for_each(|i| {
                        s.push(g[i - 1]);
                        d.push(g[i]);
                    }),
                    1 => (0..k).for_each(|i| {
                        s.push(g[i]);
                        d.push(g[(i + 1) % k]);
                    }),
                    2 => (1..k).for_each(|i| {
                        s.push(g[i]);
                        d.push(g[0]);
                    }),
                    3 => (0..k).for_each(|i| {
                        (0..k).filter(|&j| j != i).for_each(|j| {
                            s.push(g[i]);
                            d.push(g[j]);
                        })
                    }),
                    _ => g.iter().for_each(|&v| {
                        s.push(v);
                        d.push(v);
                    }),
                }
                at += k;
            }
            Topology::build(n, s, d)
        }

        /// One giant component: a directed cycle through every node in a random
        /// order, plus `extra` random edges.
        fn giant(seed: u64, n: usize, extra: usize) -> Topology {
            let mut st = seed;
            let p = perm(&mut st, n);
            let (mut s, mut d): (Vec<u32>, Vec<u32>) =
                (0..n).map(|i| (p[i], p[(i + 1) % n])).unzip();
            for _ in 0..extra {
                s.push((next(&mut st) % n as u64) as u32);
                d.push((next(&mut st) % n as u64) as u32);
            }
            Topology::build(n, s, d)
        }

        /// Directed cycles of 2 to 20 nodes (and some paths), scattered, each
        /// linked from one earlier group so the groups form a DAG, with a
        /// self-loop now and then: many strong components in few weak ones.
        fn cycles(seed: u64, n: usize) -> Topology {
            let mut st = seed;
            let p = perm(&mut st, n);
            let (mut s, mut d) = (Vec::new(), Vec::new());
            let mut heads: Vec<u32> = Vec::new();
            let mut at = 0usize;
            while at < n {
                let k = (2 + next(&mut st) % 19).min((n - at) as u64) as usize;
                let g = &p[at..at + k];
                if k > 1 && next(&mut st) % 4 != 0 {
                    (0..k).for_each(|i| {
                        s.push(g[i]);
                        d.push(g[(i + 1) % k]);
                    });
                } else {
                    (1..k).for_each(|i| {
                        s.push(g[i - 1]);
                        d.push(g[i]);
                    });
                }
                if next(&mut st) % 7 == 0 {
                    s.push(g[0]);
                    d.push(g[0]);
                }
                let pick = (next(&mut st) % (heads.len() as u64 + 1)) as usize;
                if let Some(&h) = heads.get(pick) {
                    s.push(h);
                    d.push(g[(next(&mut st) % k as u64) as usize]);
                }
                heads.push(g[(next(&mut st) % k as u64) as usize]);
                at += k;
            }
            Topology::build(n, s, d)
        }

        /// A random graph with a self-loop on about every third node.
        fn self_loops(seed: u64, n: usize, m: usize) -> Topology {
            let mut st = seed;
            let (mut s, mut d) = (Vec::new(), Vec::new());
            for _ in 0..m {
                s.push((next(&mut st) % n as u64) as u32);
                d.push((next(&mut st) % n as u64) as u32);
            }
            for v in 0..n as u32 {
                if next(&mut st).is_multiple_of(3) {
                    s.push(v);
                    d.push(v);
                }
            }
            Topology::build(n, s, d)
        }

        /// A mask keeping about `keep` edges in 8.
        fn mask(seed: u64, m: usize, keep: u64) -> EdgeMask {
            let mut st = seed;
            let k: Vec<bool> = (0..m).map(|_| next(&mut st) % 8 < keep).collect();
            EdgeMask::from_bools(&k)
        }

        /// The pinned graphs: a name, the topology and a mask.
        pub(crate) fn graphs() -> Vec<(&'static str, Topology, Option<EdgeMask>)> {
            let mut g: Vec<(&'static str, Topology, Option<EdgeMask>)> = vec![
                ("uniform sparse", uniform(11, 5_000, 4_000), None),
                ("uniform dense", uniform(12, 4_000, 40_000), None),
                ("rmat", rmat(13, 14, 60_000), None),
                ("small components", small_components(14, 20_000), None),
                ("giant", giant(15, 12_000, 30_000), None),
                ("giant, a path", giant(16, 30_000, 0), None),
                ("cycles", cycles(17, 20_000), None),
                ("self-loops", self_loops(18, 3_000, 2_500), None),
            ];
            let t = rmat(19, 13, 30_000);
            let m = mask(20, t.n_edges(), 5);
            g.push(("rmat, masked", t, Some(m)));
            let t = cycles(21, 10_000);
            let m = mask(22, t.n_edges(), 7);
            g.push(("cycles, masked", t, Some(m)));
            // Isolated vertices, as #326 has them: the compact runs.
            let (t, m) = sparse(23, 40_000, 9_000);
            g.push(("isolated", sparse(23, 40_000, 9_000).0, None));
            g.push(("isolated, masked", t, Some(m)));
            g
        }

        /// (weak hash, strong hash) per graph of [`graphs`], from the serial
        /// kernels before #383.
        const PINNED: &[(&str, u64, u64)] = &[
            ("uniform sparse", 0xd969d63f3e302fac, 0x5b35d2f40009d715),
            ("uniform dense", 0x35da70bdb324c525, 0x2c36c2471ceec525),
            ("rmat", 0x389a40a3b0ca34bf, 0x9ec683b86cb6f611),
            ("small components", 0x0608503661e8db16, 0xa3506ef85af28733),
            ("giant", 0x207327a55dadd625, 0x810efd6d3c240925),
            ("giant, a path", 0x792fc743ff1f1025, 0xe94d157b19346225),
            ("cycles", 0x5f935d30b79ecd4e, 0x2612d677587752c1),
            ("self-loops", 0xa99d05d5b6074213, 0xc0d981f1dd370ef0),
            ("rmat, masked", 0x7cb3611586dd14d9, 0xffabc2fe691bf0af),
            ("cycles, masked", 0x39efc3fc11cf1a36, 0xa21f5009fec93113),
            ("isolated", 0x4fce32e9c714b779, 0xe166896181a7b329),
            ("isolated, masked", 0x5228bbcd6fb3536d, 0xe70c68a66c38d11d),
        ];

        #[test]
        fn generated_graphs_keep_their_labels() {
            let graphs = graphs();
            assert_eq!(graphs.len(), PINNED.len());
            for ((name, t, m), &(pname, weak, strong)) in graphs.iter().zip(PINNED) {
                assert_eq!(*name, pname);
                let w = connected_components_weak(t, m.as_ref());
                let s = connected_components_strong(t, m.as_ref());
                assert_eq!(fnv(&w), weak, "weak labels changed: {name}");
                assert_eq!(fnv(&s), strong, "strong labels changed: {name}");
            }
        }

        #[test]
        fn small_graphs_keep_their_labels() {
            // 0->1->2->0, 3->4->6<->7, 8->3, 5 with a self-loop.
            let t = Topology::build(
                9,
                vec![0, 1, 2, 3, 4, 6, 7, 8, 5],
                vec![1, 2, 0, 4, 6, 7, 6, 3, 5],
            );
            assert_eq!(
                connected_components_weak(&t, None),
                vec![0, 0, 0, 3, 3, 5, 3, 3, 3]
            );
            assert_eq!(
                connected_components_strong(&t, None),
                vec![0, 0, 0, 3, 4, 5, 6, 6, 8]
            );
            // A star into the hub 3, and the hub's own self-loop.
            let t = Topology::build(6, vec![0, 1, 2, 4, 5, 3], vec![3, 3, 3, 3, 3, 3]);
            assert_eq!(connected_components_weak(&t, None), vec![0, 0, 0, 0, 0, 0]);
            assert_eq!(
                connected_components_strong(&t, None),
                vec![0, 1, 2, 3, 4, 5]
            );
            // A DFS that enters the cycle {2,4,5} at 4, not at its smallest id.
            let t = Topology::build(6, vec![0, 1, 4, 5, 2, 1], vec![1, 4, 5, 2, 4, 3]);
            assert_eq!(connected_components_weak(&t, None), vec![0, 0, 0, 0, 0, 0]);
            assert_eq!(
                connected_components_strong(&t, None),
                vec![0, 1, 4, 3, 4, 4]
            );
        }
    }
}
