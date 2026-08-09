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
//! directed edge contributes its weight in both directions; parallel/opposite
//! edges accumulate). [`louvain`] uses unit edge weights; [`louvain_weighted`]
//! takes a per-edge weight (gathered via `edge_ids`). `resolution` (γ) scales the
//! null-model term: larger γ favours smaller communities.

use std::collections::HashMap;

use rayon::prelude::*;

use super::rng::{shuffled_order, DEFAULT_SEED};
use crate::topology::{EdgeMask, Topology};

/// Community label per node (contiguous `0..k`). Deterministic given `seed`
/// (defaulting to a fixed seed when `None`, so an unseeded run is reproducible).
/// A subgraph `mask` restricts the working graph to kept edges only.
pub fn louvain(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    resolution: f64,
    seed: Option<u64>,
) -> Vec<u32> {
    louvain_impl(topo, None, mask, resolution, seed)
}

/// Weighted Louvain: as [`louvain`], but edge weights come from `weights` (per
/// edge row via `edge_ids`; non-negative, `len == n_edges`) instead of unity.
pub fn louvain_weighted(
    topo: &Topology,
    weights: &[f64],
    mask: Option<&EdgeMask>,
    resolution: f64,
    seed: Option<u64>,
) -> Vec<u32> {
    assert_eq!(
        weights.len(),
        topo.n_edges(),
        "weights length must equal the edge count"
    );
    louvain_impl(topo, Some(weights), mask, resolution, seed)
}

fn louvain_impl(
    topo: &Topology,
    weights: Option<&[f64]>,
    mask: Option<&EdgeMask>,
    resolution: f64,
    seed: Option<u64>,
) -> Vec<u32> {
    let n = topo.n_nodes();
    if n == 0 {
        return Vec::new();
    }
    // No edges: every node is its own community.
    if topo.n_edges() == 0 {
        return (0..n as u32).collect();
    }

    let seed = seed.unwrap_or(DEFAULT_SEED);
    let mut graph = Graph::from_topology(topo, weights, mask);
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
            // Scan neighbour communities in ascending-id order, not `HashMap`
            // iteration order (which varies per instance/run): with the epsilon,
            // "tie" is non-transitive, so map order could change the partition on a
            // same-seed rerun. Sorting makes it order-independent, and requiring a
            // strict improvement (> best + eps) keeps the smallest id on a tie
            // (it's seen first) without letting chained near-ties drag the anchor.
            let mut candidates: Vec<(u32, f64)> = weight_to.iter().map(|(&c, &w)| (c, w)).collect();
            candidates.sort_unstable_by_key(|&(c, _)| c);
            let mut best_c = ci;
            let mut best_gain = stay;
            for (c, w_in) in candidates {
                if c == ci {
                    continue;
                }
                let gain = w_in - resolution * tot[c as usize] * ku / graph.m2;
                if gain > best_gain + 1e-12 {
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

    /// Build the initial undirected working graph. `weights` (per edge row, gathered
    /// via `edge_ids`) sets each edge's weight; `None` uses unit weights.
    ///
    /// The dominant (level-0) graph is built by counting sort rather than one
    /// `HashMap` per node: each non-self edge `(u, v, w)` scatters `(v, w)` into
    /// `u`'s segment and `(u, w)` into `v`'s, then each segment is sorted by
    /// neighbour id and parallel edges coalesced (weights summed). This drops the
    /// per-node hashing and n allocator round-trips that dominated at the
    /// 100M–500M-edge target. Sorted segments are also *more* deterministic than
    /// the former `HashMap`-iteration order; `one_level` already sorts candidate
    /// communities and requires a strict epsilon improvement (see there), so the
    /// partition is unaffected.
    fn from_topology(topo: &Topology, weights: Option<&[f64]>, mask: Option<&EdgeMask>) -> Graph {
        let n = topo.n_nodes();
        let out = topo.out();
        let mut self_loop = vec![0.0f64; n];
        // Under a subgraph mask, only kept edges enter the working graph. The
        // histogram and scatter below share this predicate so segment sizes match.
        let keep = |e: u32| mask.is_none_or(|m| m.keep(e));

        // Degree histogram: every non-self edge contributes to both endpoints.
        let mut offsets = vec![0u64; n + 1];
        for u in 0..n as u32 {
            for (&v, &e) in out.neighbors(u).iter().zip(out.edge_ids(u)) {
                if keep(e) && u != v {
                    offsets[u as usize + 1] += 1;
                    offsets[v as usize + 1] += 1;
                }
            }
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }

        // Scatter each edge into both endpoints' segments.
        let total = offsets[n] as usize;
        let mut nbr = vec![0u32; total];
        let mut wbuf = vec![0.0f64; total];
        let mut cursor: Vec<u64> = offsets[..n].to_vec();
        for u in 0..n as u32 {
            for (&v, &e) in out.neighbors(u).iter().zip(out.edge_ids(u)) {
                if !keep(e) {
                    continue;
                }
                let w = weights.map_or(1.0, |ws| ws[e as usize]);
                if u == v {
                    self_loop[u as usize] += w;
                } else {
                    let pu = cursor[u as usize] as usize;
                    nbr[pu] = v;
                    wbuf[pu] = w;
                    cursor[u as usize] += 1;
                    let pv = cursor[v as usize] as usize;
                    nbr[pv] = u;
                    wbuf[pv] = w;
                    cursor[v as usize] += 1;
                }
            }
        }

        // Sort each segment by neighbour id and coalesce parallel edges.
        let adj: Vec<Vec<(u32, f64)>> = (0..n)
            .into_par_iter()
            .map(|u| {
                let s = offsets[u] as usize;
                let e = offsets[u + 1] as usize;
                let mut pairs: Vec<(u32, f64)> = (s..e).map(|k| (nbr[k], wbuf[k])).collect();
                pairs.sort_unstable_by_key(|&(v, _)| v);
                let mut coalesced: Vec<(u32, f64)> = Vec::with_capacity(pairs.len());
                for (v, w) in pairs {
                    match coalesced.last_mut() {
                        Some(last) if last.0 == v => last.1 += w,
                        _ => coalesced.push((v, w)),
                    }
                }
                coalesced
            })
            .collect();
        Graph::finalize(adj, self_loop)
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
        let comm = louvain(&t, None, 1.0, Some(1));
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
        assert_eq!(
            louvain(&t, None, 1.0, Some(5)),
            louvain(&t, None, 1.0, Some(5))
        );
    }

    #[test]
    fn no_edges_is_all_singletons() {
        let t = Topology::build(3, vec![], vec![]);
        assert_eq!(louvain(&t, None, 1.0, None), vec![0, 1, 2]);
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(louvain(&t, None, 1.0, None).is_empty());
    }

    #[test]
    fn weighted_uniform_matches_unweighted() {
        // Unit weights on every edge reproduce the unweighted partition.
        let t = two_cliques();
        let ones = vec![1.0; t.n_edges()];
        assert_eq!(
            louvain_weighted(&t, &ones, None, 1.0, Some(1)),
            louvain(&t, None, 1.0, Some(1))
        );
    }

    #[test]
    fn weighted_can_reassign_the_bridge_endpoints() {
        // Two triangles {0,1,2} and {3,4,5} joined by 2-3. With the bridge weighted
        // far heavier than the intra-triangle edges, 2 and 3 bind together.
        // edges: 0-1,1-2,2-0 (triangle A), 3-4,4-5,5-3 (triangle B), 2-3 (bridge)
        let src = vec![0, 1, 2, 3, 4, 5, 2];
        let dst = vec![1, 2, 0, 4, 5, 3, 3];
        let t = Topology::build(6, src, dst);
        let heavy_bridge = vec![1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 50.0];
        let comm = louvain_weighted(&t, &heavy_bridge, None, 1.0, Some(1));
        assert_eq!(comm[2], comm[3], "a heavy bridge binds its endpoints");
    }
}
