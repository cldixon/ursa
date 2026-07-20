//! Louvain community detection — modularity optimisation by local moving and
//! graph aggregation.
//!
//! Two phases alternate until modularity stops improving (Blondel et al.):
//!
//! 1. **Local moving.** Each node, visited in a `seed`-derived order, leaves its
//!    community and joins the neighbouring community that most increases
//!    modularity (staying put if nothing does). Repeated until a sweep moves no
//!    node.
//! 2. **Aggregation.** Each community collapses to a super-node; intra-community
//!    edges become a weighted self-loop and inter-community edges become weighted
//!    super-edges. Local moving then runs on the smaller weighted graph.
//!
//! Edges are treated as **undirected**: a directed input is symmetrised (each
//! directed edge contributes weight 1 in both directions; parallel/opposite edges
//! accumulate). The input is unweighted for v0.1 — the aggregation phase is where
//! weights first appear — so `weight=` is deferred to the weighted-algorithms
//! work. `resolution` (γ) scales the null-model term: larger γ favours smaller
//! communities.

use std::collections::HashMap;

use super::rng::{shuffled_order, DEFAULT_SEED};
use crate::topology::Topology;

/// Community label per node (contiguous `0..k`). Deterministic given `seed`
/// (defaulting to a fixed seed when `None`, so an unseeded run is reproducible).
pub fn louvain(topo: &Topology, resolution: f64, seed: Option<u64>) -> Vec<u32> {
    let n = topo.n_nodes();
    if n == 0 {
        return Vec::new();
    }
    // No edges: every node is its own community.
    if topo.n_edges() == 0 {
        return (0..n as u32).collect();
    }

    let seed = seed.unwrap_or(DEFAULT_SEED);
    let mut graph = Graph::from_topology(topo);
    // Community of each *original* node, expressed in the current graph's node
    // space; updated (composed) at every level.
    let mut node_comm: Vec<u32> = (0..n as u32).collect();

    loop {
        let comm = one_level(&graph, resolution, seed);
        let c = comm.iter().copied().max().map_or(0, |m| m as usize + 1);
        // Compose this level's assignment onto the original nodes.
        for label in node_comm.iter_mut() {
            *label = comm[*label as usize];
        }
        // No community merged (every super-node stayed distinct) → converged.
        if c == graph.n() {
            break;
        }
        graph = graph.aggregate(&comm, c);
        if c == 1 {
            break;
        }
    }
    node_comm
}

/// One full local-moving phase over `graph`, returning a contiguous `0..k`
/// community label per node.
fn one_level(graph: &Graph, resolution: f64, seed: u64) -> Vec<u32> {
    let n = graph.n();
    let mut comm: Vec<u32> = (0..n as u32).collect();
    let mut tot: Vec<f64> = graph.k.clone(); // Σ weighted-degree of each community
    let order = shuffled_order(n, seed);
    let mut weight_to: HashMap<u32, f64> = HashMap::new();

    loop {
        let mut moved = false;
        for &u in &order {
            let ci = comm[u as usize];
            let ku = graph.k[u as usize];

            // Weight from u into each neighbouring community.
            weight_to.clear();
            for &(v, w) in &graph.adj[u as usize] {
                *weight_to.entry(comm[v as usize]).or_insert(0.0) += w;
            }

            // Tentatively remove u from its community.
            tot[ci as usize] -= ku;

            // Gain of staying in ci (the baseline every move must strictly beat).
            let stay = weight_to.get(&ci).copied().unwrap_or(0.0)
                - resolution * tot[ci as usize] * ku / graph.m2;
            let mut best_c = ci;
            let mut best_gain = stay;
            for (&c, &w_in) in &weight_to {
                if c == ci {
                    continue;
                }
                let gain = w_in - resolution * tot[c as usize] * ku / graph.m2;
                // Strictly better wins; equal gains break toward the smallest id
                // (deterministic regardless of map order). Never tie into a move
                // away from ci, so every accepted move strictly raises modularity
                // and the loop terminates.
                if gain > best_gain + 1e-12
                    || ((gain - best_gain).abs() <= 1e-12 && best_c != ci && c < best_c)
                {
                    best_gain = gain;
                    best_c = c;
                }
            }

            // Commit (re-insert into the chosen community).
            tot[best_c as usize] += ku;
            comm[u as usize] = best_c;
            if best_c != ci {
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    renumber(&comm)
}

/// An undirected, weighted working graph. Self-loops are kept apart from the
/// adjacency so the weighted degree `k` can count them (twice, per convention)
/// without polluting the neighbour scan.
struct Graph {
    /// Symmetric adjacency: edge `{u, v}` of weight `w` appears as `(v, w)` in
    /// `adj[u]` and `(u, w)` in `adj[v]`.
    adj: Vec<Vec<(u32, f64)>>,
    self_loop: Vec<f64>,
    /// Weighted degree: `Σ adj[i] + 2 · self_loop[i]`.
    k: Vec<f64>,
    /// `2m` — total weighted degree over all nodes.
    m2: f64,
}

impl Graph {
    fn n(&self) -> usize {
        self.adj.len()
    }

    fn from_topology(topo: &Topology) -> Graph {
        let n = topo.n_nodes();
        let mut nbr: Vec<HashMap<u32, f64>> = vec![HashMap::new(); n];
        let mut self_loop = vec![0.0f64; n];
        let out = topo.out();
        for u in 0..n as u32 {
            for &v in out.neighbors(u) {
                if u == v {
                    self_loop[u as usize] += 1.0;
                } else {
                    *nbr[u as usize].entry(v).or_insert(0.0) += 1.0;
                    *nbr[v as usize].entry(u).or_insert(0.0) += 1.0;
                }
            }
        }
        Graph::finalize(maps_to_adj(nbr), self_loop)
    }

    /// Collapse each community into a super-node.
    fn aggregate(&self, comm: &[u32], c: usize) -> Graph {
        let mut nbr: Vec<HashMap<u32, f64>> = vec![HashMap::new(); c];
        let mut self_loop = vec![0.0f64; c];
        for u in 0..self.n() {
            let cu = comm[u] as usize;
            self_loop[cu] += self.self_loop[u];
            for &(v, w) in &self.adj[u] {
                let cv = comm[v as usize] as usize;
                if cu == cv {
                    // Each intra-community edge is seen from both endpoints, so
                    // halve to land one loop-weight per undirected edge.
                    self_loop[cu] += w / 2.0;
                } else {
                    *nbr[cu].entry(cv as u32).or_insert(0.0) += w;
                }
            }
        }
        Graph::finalize(maps_to_adj(nbr), self_loop)
    }

    fn finalize(adj: Vec<Vec<(u32, f64)>>, self_loop: Vec<f64>) -> Graph {
        let n = adj.len();
        let k: Vec<f64> = (0..n)
            .map(|i| adj[i].iter().map(|&(_, w)| w).sum::<f64>() + 2.0 * self_loop[i])
            .collect();
        let m2 = k.iter().sum();
        Graph {
            adj,
            self_loop,
            k,
            m2,
        }
    }
}

fn maps_to_adj(maps: Vec<HashMap<u32, f64>>) -> Vec<Vec<(u32, f64)>> {
    maps.into_iter().map(|m| m.into_iter().collect()).collect()
}

/// Relabel arbitrary community ids to a contiguous `0..k` by first appearance.
fn renumber(comm: &[u32]) -> Vec<u32> {
    let mut map: HashMap<u32, u32> = HashMap::new();
    let mut next = 0u32;
    comm.iter()
        .map(|&c| {
            *map.entry(c).or_insert_with(|| {
                let id = next;
                next += 1;
                id
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two 4-cliques {0,1,2,3} and {4,5,6,7} joined by a single bridge 3-4.
    fn two_cliques() -> Topology {
        let mut src = Vec::new();
        let mut dst = Vec::new();
        for &(a, b) in &[
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 2),
            (1, 3),
            (2, 3),
            (4, 5),
            (4, 6),
            (4, 7),
            (5, 6),
            (5, 7),
            (6, 7),
            (3, 4), // bridge
        ] {
            src.push(a);
            dst.push(b);
        }
        Topology::build(8, src, dst)
    }

    #[test]
    fn recovers_two_communities() {
        let t = two_cliques();
        let comm = louvain(&t, 1.0, Some(1));
        assert!(comm[0] == comm[1] && comm[1] == comm[2] && comm[2] == comm[3]);
        assert!(comm[4] == comm[5] && comm[5] == comm[6] && comm[6] == comm[7]);
        assert_ne!(comm[0], comm[4]);
        // Contiguous labels: exactly two communities.
        let max = *comm.iter().max().unwrap();
        assert_eq!(max, 1);
    }

    #[test]
    fn deterministic_given_a_seed() {
        let t = two_cliques();
        assert_eq!(louvain(&t, 1.0, Some(5)), louvain(&t, 1.0, Some(5)));
    }

    #[test]
    fn no_edges_is_all_singletons() {
        let t = Topology::build(3, vec![], vec![]);
        assert_eq!(louvain(&t, 1.0, None), vec![0, 1, 2]);
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(louvain(&t, 1.0, None).is_empty());
    }
}
