//! PageRank — pull-based fixpoint.
//!
//! Pull-based: each iteration, every node pulls rank from its *in*-neighbours, so
//! the kernel consults the transpose (CSC). Dangling nodes (out-degree 0) would
//! leak rank, so their mass is redistributed uniformly each iteration.
//!
//! The Rayon parallel sweep over vertex ranges is the shape every fixpoint kernel
//! shares. [`pagerank`] uses unit edge weights; [`pagerank_weighted`] gathers
//! `weight[edge_ids[k]]` per in-edge (the seam is `Adjacency::edge_ids`) and pulls
//! rank in proportion to weighted out-strength.
//!
//! **Multiplicity:** parallel `(u, v)` edges are kept as distinct CSR slots, so an
//! unweighted run pulls rank once per parallel edge (multiplicity-is-rows) — a
//! multigraph's scores differ from collapsing parallels to one edge.
//!
//! **Isolated nodes** (no kept edge) all hold one value, so a graph with many
//! runs over its linked nodes only and carries the isolated ones as that one
//! value, to the same bits (see [`Isolated`], #326).

use crate::parallel::*;

use super::linked::{add_repeated, Linked};
use crate::topology::{EdgeMask, Topology};

/// PageRank parameters, mirroring `ur.pagerank(...)`.
#[derive(Debug, Clone, Copy)]
pub struct PageRankParams {
    pub damping: f64,
    pub max_iter: u32,
    pub tol: f64,
}

impl Default for PageRankParams {
    fn default() -> Self {
        PageRankParams {
            damping: 0.85,
            max_iter: 30,
            tol: 1e-6,
        }
    }
}

/// The node count of one [`l1_delta`] chunk.
const DELTA_CHUNK: usize = 8192;

/// Deterministic L1 delta `Σ |a[i] - b[i]|`. Rayon's default `sum()` reduces in a
/// work-stealing-dependent order, so its result varies at the ULP level run to
/// run — which can flip `delta < tol` near threshold and change the iteration
/// count (and thus the output). Fixed-size chunks summed sequentially and combined
/// in order give the same value every run at any thread count, while staying
/// parallel across chunks.
fn l1_delta(a: &[f64], b: &[f64]) -> f64 {
    a.par_chunks(DELTA_CHUNK)
        .zip(b.par_chunks(DELTA_CHUNK))
        .map(|(ac, bc)| ac.iter().zip(bc).map(|(x, y)| (x - y).abs()).sum::<f64>())
        .collect::<Vec<f64>>()
        .iter()
        .sum()
}

/// The empty `f64` sum, which every `sum()` starts from.
fn empty_sum() -> f64 {
    std::iter::empty::<f64>().sum()
}

/// The isolated nodes of a compact PageRank run (#326), and how they enter its
/// two sums.
///
/// An isolated node has no in-edge, so it pulls nothing, and no out-edge, so
/// it is dangling. Every iteration therefore sets it to exactly
/// `base + d · 0.0 = base`, the same for every isolated node: at iteration `t`
/// each holds `r_t`, with `r_0 = 1/n` and `r_(t+1) = base_t`, where
///
/// ```text
/// base_t = (1 - d)/n + d · (D_t + k · r_t)/n
/// ```
///
/// `n` counts every node, `k` the isolated ones and `D_t` the rank on the
/// linked dangling nodes: an isolated node's score is its teleport share plus
/// its share of the dangling mass, its own included. So the kernel keeps one
/// `r_t` for all of them. To give the full computation's bits, it adds their
/// rank into the dangling mass, and their `|r_(t+1) - r_t|` into the L1 delta,
/// at the positions the full computation adds them, a run of equal values at
/// a time ([`add_repeated`]).
struct Isolated<'a> {
    linked: &'a Linked,
    /// The isolated nodes just before each linked dangling node in node order,
    /// since the previous one; then the count after the last.
    gaps: Vec<u64>,
    tail: u64,
    /// For each [`l1_delta`] chunk of the full node order, the compact range of
    /// its linked nodes.
    chunks: Vec<(usize, usize)>,
}

impl<'a> Isolated<'a> {
    fn new(linked: &'a Linked, dangling: &[usize]) -> Self {
        let nodes = linked.nodes();
        // The isolated nodes below compact node c: its node id less c.
        let before = |c: usize| (nodes[c] as usize - c) as u64;
        let mut prev = 0u64;
        let gaps = dangling
            .iter()
            .map(|&c| {
                let b = before(c);
                b - std::mem::replace(&mut prev, b)
            })
            .collect();
        let tail = linked.n_isolated() as u64 - prev;
        let n = linked.n();
        let chunks = (0..n.div_ceil(DELTA_CHUNK))
            .map(|i| {
                let lo = i * DELTA_CHUNK;
                let hi = (lo + DELTA_CHUNK).min(n);
                (
                    nodes.partition_point(|&v| (v as usize) < lo),
                    nodes.partition_point(|&v| (v as usize) < hi),
                )
            })
            .collect();
        Isolated {
            linked,
            gaps,
            tail,
            chunks,
        }
    }

    /// The full computation's dangling mass: the dangling nodes' rank summed
    /// in node order, the isolated ones holding `iso` each.
    fn dangling_mass(&self, dangling: &[usize], rank: &[f64], iso: f64) -> f64 {
        let mut s = empty_sum();
        for (&c, &gap) in dangling.iter().zip(&self.gaps) {
            s = add_repeated(s, iso, gap);
            s += rank[c];
        }
        add_repeated(s, iso, self.tail)
    }

    /// The full computation's [`l1_delta`] of `next` from `rank`, the
    /// isolated nodes differing by `iso_delta` each.
    fn l1_delta(&self, next: &[f64], rank: &[f64], iso_delta: f64) -> f64 {
        let nodes = self.linked.nodes();
        let n = self.linked.n();
        self.chunks
            .par_iter()
            .enumerate()
            .map(|(i, &(clo, chi))| {
                let mut cursor = i * DELTA_CHUNK;
                let hi = (cursor + DELTA_CHUNK).min(n);
                let mut s = empty_sum();
                for c in clo..chi {
                    let v = nodes[c] as usize;
                    s = add_repeated(s, iso_delta, (v - cursor) as u64);
                    s += (next[c] - rank[c]).abs();
                    cursor = v + 1;
                }
                add_repeated(s, iso_delta, (hi - cursor) as u64)
            })
            .collect::<Vec<f64>>()
            .iter()
            .sum()
    }
}

/// The pull-based PageRank fixpoint, shared by the unweighted and weighted kernels.
///
/// `dangling` lists the zero-out-degree (or zero-out-strength) nodes, whose rank is
/// redistributed uniformly each iteration. `contrib(u, rank)` returns node `u`'s
/// pulled contribution — the sum over `u`'s in-edges of each in-neighbour's rank
/// share — which is the only part that differs between the variants. Everything
/// else (the uniform init, the `base + d * contrib` update, the deterministic L1
/// convergence check, the buffer swap, and the early break) lives here once, so
/// both kernels iterate through byte-identical float operations.
///
/// With `split_by = Some(deg)`, each iteration first computes every node's share
/// `rank[v] / deg[v]` (0 when `deg[v] == 0`) and hands `contrib` those shares
/// instead of the ranks. The unweighted kernel would otherwise repeat that same
/// division once per in-edge, with a second random gather into `deg`; dividing
/// once per node is the identical operation, so results are bit-for-bit
/// unchanged.
///
/// With `linked`, the run is compact (#326): every per-node vector, `dangling`
/// and `contrib`'s node are over the linked nodes, `n` still counts every
/// node, and the isolated ones are carried as one value (see [`Isolated`]).
/// The result is per node, the isolated ones filled in, and bit for bit the
/// full computation's.
fn power_iterate(
    n: usize,
    linked: Option<&Linked>,
    params: PageRankParams,
    dangling: &[usize],
    split_by: Option<&[u32]>,
    contrib: impl Fn(usize, &[f64]) -> f64 + Sync,
) -> Vec<f64> {
    let nf = n as f64;
    let d = params.damping;
    let isolated = linked.map(|l| Isolated::new(l, dangling));
    let width = linked.map_or(n, Linked::len);

    let mut rank = vec![1.0 / nf; width];
    let mut next = vec![0.0f64; width];
    let mut share = if split_by.is_some() {
        vec![0.0f64; width]
    } else {
        Vec::new()
    };
    // Every isolated node's rank (see `Isolated`).
    let mut iso_rank = 1.0 / nf;

    for _iter in 0..params.max_iter {
        // Rank stranded on dangling nodes, spread uniformly to everyone. Summed over
        // this fixed ascending-index list (a parallel filter-sum is not deterministic).
        let dangling_mass: f64 = match &isolated {
            None => dangling.iter().map(|&u| rank[u]).sum(),
            Some(iso) => iso.dangling_mass(dangling, &rank, iso_rank),
        };
        let base = (1.0 - d) / nf + d * dangling_mass / nf;

        let pulled: &[f64] = match split_by {
            Some(deg) => {
                share.par_iter_mut().enumerate().for_each(|(v, slot)| {
                    *slot = if deg[v] > 0 {
                        rank[v] / deg[v] as f64
                    } else {
                        0.0
                    };
                });
                &share
            }
            None => &rank,
        };
        next.par_iter_mut().enumerate().for_each(|(u, slot)| {
            *slot = base + d * contrib(u, pulled);
        });
        // An isolated node pulls nothing: its accumulator stays at 0.0.
        let iso_next = base + d * 0.0;

        let delta = match &isolated {
            None => l1_delta(&next, &rank),
            Some(iso) => iso.l1_delta(&next, &rank, (iso_next - iso_rank).abs()),
        };

        std::mem::swap(&mut rank, &mut next);
        iso_rank = iso_next;
        if delta < params.tol {
            break;
        }
    }

    match linked {
        None => rank,
        Some(l) => l.expand(&rank, |_| iso_rank),
    }
}

/// The linked nodes PageRank runs over, when enough are isolated to be worth
/// it (see [`Linked::worth_it`]). The kernel pulls over the transpose, so it
/// is built first: unmasked, the linked nodes are then read off the offsets.
fn linked_nodes(topo: &Topology, mask: Option<&EdgeMask>) -> Option<Linked> {
    topo.incoming();
    let linked = Linked::of(topo, mask);
    linked.worth_it().then_some(linked)
}

/// Dense PageRank vector; `result[u]` is the score of dense node `u`. Scores sum
/// to ~1.0 (up to dangling redistribution and early convergence).
pub fn pagerank(topo: &Topology, mask: Option<&EdgeMask>, params: PageRankParams) -> Vec<f64> {
    if topo.n_nodes() == 0 {
        return Vec::new();
    }
    pagerank_over(topo, mask, params, linked_nodes(topo, mask).as_ref())
}

/// [`pagerank`], compact over `linked` when given.
fn pagerank_over(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    params: PageRankParams,
    linked: Option<&Linked>,
) -> Vec<f64> {
    let n = topo.n_nodes();
    // The node of compact node `c`, and the compact node of node `v` (the
    // identity when not compact).
    let node = |c: usize| linked.map_or(c as u32, |l| l.nodes()[c]);
    let at = |v: u32| linked.map_or(v as usize, |l| l.compact(v) as usize);
    let width = linked.map_or(n, Linked::len);

    // Precompute out-degrees (rank is divided by them when pushed forward). Under a
    // subgraph mask, a node's out-degree counts only its *kept* out-edges — rank
    // splits among the edges present in the subgraph.
    let out = topo.out();
    let out_deg: Vec<u32> = match mask {
        None => (0..width).map(|c| out.degree(node(c))).collect(),
        Some(m) => (0..width)
            .map(|c| out.edge_ids(node(c)).iter().filter(|&&e| m.keep(e)).count() as u32)
            .collect(),
    };
    let inc = topo.incoming();

    // Dangling nodes (out-degree 0), listed once — O(#dangling) rather than an O(n)
    // scan per iteration.
    let dangling_nodes: Vec<usize> = (0..width).filter(|&u| out_deg[u] == 0).collect();

    // `share[v]` is `rank[v] / out_deg[v]`, or 0 for a dangling `v` (whose rank is
    // redistributed through `base` instead). Adding that 0 leaves the non-negative
    // accumulator bit-identical to skipping the edge.
    power_iterate(
        n,
        linked,
        params,
        &dangling_nodes,
        Some(&out_deg),
        |u, share| {
            let u = node(u);
            let mut acc = 0.0;
            match (mask, linked) {
                // Fast path: unmasked full-graph pull.
                (None, None) => {
                    for &v in inc.neighbors(u) {
                        acc += share[v as usize];
                    }
                }
                (None, Some(_)) => {
                    for &v in inc.neighbors(u) {
                        acc += share[at(v)];
                    }
                }
                // Subgraph pull: skip in-edges whose original row is masked out.
                (Some(m), _) => {
                    for (&v, &e) in inc.neighbors(u).iter().zip(inc.edge_ids(u)) {
                        if m.keep(e) {
                            acc += share[at(v)];
                        }
                    }
                }
            }
            acc
        },
    )
}

/// Weighted PageRank (pull-based). Identical to [`pagerank`] except a node's rank
/// is split among its out-edges in proportion to the edge weights rather than
/// uniformly: each in-neighbour `v` contributes `w * rank[v] / out_strength[v]`,
/// where `w` is the weight of the `v -> u` edge and `out_strength[v]` is `v`'s
/// total outgoing weight. A node with zero out-strength is dangling and its rank
/// is redistributed uniformly, as in the unweighted kernel.
///
/// `weights[e]` is the weight of original edge row `e`; `weights.len()` must equal
/// `topo.n_edges()`. Weights are gathered per CSR slot via `Adjacency::edge_ids`.
pub fn pagerank_weighted(
    topo: &Topology,
    weights: &[f64],
    mask: Option<&EdgeMask>,
    params: PageRankParams,
) -> Vec<f64> {
    if topo.n_nodes() == 0 {
        return Vec::new();
    }
    assert_eq!(
        weights.len(),
        topo.n_edges(),
        "weights length must equal the edge count"
    );
    let linked = linked_nodes(topo, mask);
    pagerank_weighted_over(topo, weights, mask, params, linked.as_ref())
}

/// [`pagerank_weighted`], compact over `linked` when given.
fn pagerank_weighted_over(
    topo: &Topology,
    weights: &[f64],
    mask: Option<&EdgeMask>,
    params: PageRankParams,
    linked: Option<&Linked>,
) -> Vec<f64> {
    let n = topo.n_nodes();
    let node = |c: usize| linked.map_or(c as u32, |l| l.nodes()[c]);
    let at = |v: u32| linked.map_or(v as usize, |l| l.compact(v) as usize);
    let width = linked.map_or(n, Linked::len);

    // Precompute weighted out-strength: the total outgoing weight of each node. Under
    // a mask, only kept out-edges contribute to the strength.
    let out = topo.out();
    let out_str: Vec<f64> = (0..width)
        .map(|c| {
            out.edge_ids(node(c))
                .iter()
                .filter(|&&e| mask.is_none_or(|m| m.keep(e)))
                .map(|&e| weights[e as usize])
                .sum()
        })
        .collect();
    let inc = topo.incoming();

    // Dangling nodes (zero out-strength), listed once — see the unweighted kernel.
    let dangling_nodes: Vec<usize> = (0..width).filter(|&u| out_str[u] <= 0.0).collect();

    power_iterate(n, linked, params, &dangling_nodes, None, |u, rank| {
        let u = node(u);
        let mut acc = 0.0;
        // In-neighbours and their edge rows are aligned element-for-element.
        let nbrs = inc.neighbors(u);
        let edges = inc.edge_ids(u);
        for (&v, &e) in nbrs.iter().zip(edges) {
            if !mask.is_none_or(|m| m.keep(e)) {
                continue;
            }
            let v = at(v);
            let os = out_str[v];
            if os > 0.0 {
                acc += weights[e as usize] * rank[v] / os;
            }
        }
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_sum_to_one_and_rank_hub_highest() {
        // A small graph where node 2 is a sink hub: 0->2, 1->2, 3->2, 2->0
        let t = Topology::build(4, vec![0, 1, 3, 2], vec![2, 2, 2, 0]);
        let pr = pagerank(&t, None, PageRankParams::default());
        let sum: f64 = pr.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "sum was {sum}");
        // The hub everyone points at should outrank every other node.
        let hub = pr[2];
        assert!(pr.iter().enumerate().all(|(i, &v)| i == 2 || v < hub));
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(pagerank(&t, None, PageRankParams::default()).is_empty());
    }

    #[test]
    fn weight_shifts_rank_toward_the_heavier_target() {
        // Node 0 splits between 1 and 2 (edge rows 0 and 1); 1 and 2 are sinks
        // pointing back at 0 so mass recirculates. Weighting the 0->2 edge far
        // heavier should make node 2 outrank node 1.
        let t = Topology::build(3, vec![0, 0, 1, 2], vec![1, 2, 0, 0]);
        let uniform = pagerank_weighted(&t, &[1.0, 1.0, 1.0, 1.0], None, PageRankParams::default());
        assert!(
            (uniform[1] - uniform[2]).abs() < 1e-9,
            "uniform ties 1 and 2"
        );

        // rows: 0=(0->1), 1=(0->2), 2=(1->0), 3=(2->0). Make 0->2 heavy.
        let weighted =
            pagerank_weighted(&t, &[1.0, 9.0, 1.0, 1.0], None, PageRankParams::default());
        assert!(weighted[2] > weighted[1], "heavier 0->2 lifts node 2");
        let sum: f64 = weighted.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "sum was {sum}");
    }

    fn bits(v: &[f64]) -> Vec<u64> {
        v.iter().map(|x| x.to_bits()).collect()
    }

    /// The compact run (#326) gives the full run's scores, bit for bit:
    /// unweighted and weighted, masked and not, and at a tolerance that stops
    /// it early as well as one that runs every iteration.
    #[test]
    fn compact_runs_give_the_full_scores() {
        use crate::algo::linked::testing::{sparse, weights, CASES};
        for &(seed, n, m) in CASES {
            let (t, mask) = sparse(seed, n, m);
            let w = weights(seed, m);
            for tol in [1e-6, 0.0] {
                let params = PageRankParams {
                    tol,
                    ..PageRankParams::default()
                };
                for mask in [None, Some(&mask)] {
                    let linked = Linked::of(&t, mask);
                    assert!(linked.n_isolated() > 0 || n < 4);
                    assert_eq!(
                        bits(&pagerank_over(&t, mask, params, Some(&linked))),
                        bits(&pagerank_over(&t, mask, params, None)),
                        "seed {seed}, masked {}",
                        mask.is_some()
                    );
                    assert_eq!(
                        bits(&pagerank_weighted_over(&t, &w, mask, params, Some(&linked))),
                        bits(&pagerank_weighted_over(&t, &w, mask, params, None)),
                        "weighted: seed {seed}, masked {}",
                        mask.is_some()
                    );
                }
            }
        }
    }

    /// Every isolated node scores the closed form: `r_0 = 1/n`, then
    /// `r_(t+1) = (1 - d)/n + d · D_t/n`, with `D_t` all the rank on dangling
    /// nodes, the isolated ones' included.
    #[test]
    fn an_isolated_node_scores_its_teleport_and_dangling_share() {
        // 0 -> 1 -> 2 -> 0, 3 -> 0 (3 dangling-free, 0..3 a cycle), 4 a sink;
        // 5..9 isolated.
        let t = Topology::build(10, vec![0, 1, 2, 3, 3], vec![1, 2, 0, 0, 4]);
        let params = PageRankParams {
            max_iter: 1,
            ..PageRankParams::default()
        };
        let (n, d) = (10.0, params.damping);
        let pr = pagerank(&t, None, params);
        // After one iteration from uniform 1/n: the dangling nodes are 4 and
        // the five isolated ones, each holding 1/n.
        let want = (1.0 - d) / n + d * (6.0 / n) / n;
        for &r in &pr[5..] {
            assert!((r - want).abs() < 1e-15, "{r} != {want}");
        }
        assert!(pr[5..].iter().all(|&r| r.to_bits() == pr[5].to_bits()));
    }

    #[test]
    fn zero_weight_out_edges_are_dangling() {
        // 0 -> 1 with weight 0 makes node 0 effectively dangling (no out-strength).
        let t = Topology::build(2, vec![0], vec![1]);
        let pr = pagerank_weighted(&t, &[0.0], None, PageRankParams::default());
        let sum: f64 = pr.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "sum was {sum}");
    }
}
