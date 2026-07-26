//! Betweenness centrality — Brandes' algorithm.
//!
//! For each source, a BFS records the number of shortest paths (`sigma`) and the
//! predecessor DAG; a reverse pass over the BFS order accumulates each node's
//! dependency. Ported from the canonical Brandes formulation (spec §Algorithm
//! kernels — "do not invent these"). Directed: paths follow out-edges and ordered
//! pairs `(s, t)` are counted once, so there is no final halving.
//!
//! **Multiplicity:** `sigma` is incremented per CSR slot, so parallel edges count
//! as distinct shortest paths — a multigraph's scores therefore differ from a
//! simple-graph reference (e.g. NetworkX). The weighted variant
//! ([`betweenness_weighted`]) counts equal-cost alternate paths as ties only when
//! their total costs are *exactly* float-equal.
//!
//! Exact is `O(n · m)`. `sample = Some(frac)` runs Brandes from a `seed`-shuffled
//! subset of `⌈frac · n⌉` sources and scales the result by `n / k` (the
//! Brandes–Pich estimator); a shuffled subset (not a layout-correlated stride) is
//! unbiased, and the seed makes it reproducible. Sources run in parallel over
//! fixed-size chunks whose partial vectors are combined in a fixed order, so the
//! total is identical run to run at any thread count.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use rayon::prelude::*;

use super::bfs::TotalF64;
use super::rng::{shuffled_order, DEFAULT_SEED};
use crate::topology::Topology;

/// Sum per-source Brandes contributions deterministically. Rayon's `fold`/`reduce`
/// combines partials in a work-stealing-dependent order, so the f64 total varies
/// at the ULP level run to run. Here each fixed-size chunk of sources is
/// accumulated sequentially (in source order), the chunks run in parallel, and the
/// chunk partials are combined in fixed order — identical result every run at any
/// thread count.
///
/// Each chunk allocates its per-source scratch **once** (via `make_scratch`) and
/// reuses it across the chunk's sources; `per_source` resets only the nodes it
/// touched, so the five n-sized buffers (and the per-node predecessor lists) are
/// allocated once per rayon split instead of once per source — turning the
/// exact-betweenness `O(n)` allocations-per-source into `O(1)` while producing
/// byte-identical results (buffers are reset to the same clean state a fresh
/// allocation would have).
fn accumulate_sources<S>(
    n: usize,
    sources: &[u32],
    make_scratch: impl Fn() -> S + Sync,
    per_source: impl Fn(u32, &mut [f64], &mut S) + Sync,
) -> Vec<f64> {
    const SRC_CHUNK: usize = 64;
    let partials: Vec<Vec<f64>> = sources
        .par_chunks(SRC_CHUNK)
        .map(|chunk| {
            let mut acc = vec![0.0f64; n];
            let mut scratch = make_scratch();
            for &s in chunk {
                per_source(s, &mut acc, &mut scratch);
            }
            acc
        })
        .collect();
    let mut bc = vec![0.0f64; n];
    for partial in &partials {
        for (x, y) in bc.iter_mut().zip(partial) {
            *x += y;
        }
    }
    bc
}

/// Reusable per-source scratch for unweighted Brandes. Held clean between sources:
/// every source resets exactly the nodes it visited (recorded in `order`) back to
/// the initial state, so the next source sees the same buffers a fresh allocation
/// would give — no re-zeroing of untouched nodes, no re-allocation.
struct BrandesScratch {
    dist: Vec<i64>,
    sigma: Vec<f64>,
    delta: Vec<f64>,
    preds: Vec<Vec<u32>>,
    order: Vec<u32>,
    queue: VecDeque<u32>,
}

impl BrandesScratch {
    fn new(n: usize) -> Self {
        BrandesScratch {
            dist: vec![-1i64; n],
            sigma: vec![0.0f64; n],
            delta: vec![0.0f64; n],
            preds: vec![Vec::new(); n],
            order: Vec::new(),
            queue: VecDeque::new(),
        }
    }
}

/// Reusable per-source scratch for weighted (Dijkstra) Brandes.
struct BrandesWeightedScratch {
    dist: Vec<f64>,
    sigma: Vec<f64>,
    delta: Vec<f64>,
    preds: Vec<Vec<u32>>,
    order: Vec<u32>,
    settled: Vec<bool>,
    heap: BinaryHeap<Reverse<(TotalF64, u32)>>,
}

impl BrandesWeightedScratch {
    fn new(n: usize) -> Self {
        BrandesWeightedScratch {
            dist: vec![f64::INFINITY; n],
            sigma: vec![0.0f64; n],
            delta: vec![0.0f64; n],
            preds: vec![Vec::new(); n],
            order: Vec::new(),
            settled: vec![false; n],
            heap: BinaryHeap::new(),
        }
    }
}

/// Betweenness centrality per node (directed, following out-edges). See the
/// module docs for the exact-vs-`sample` behaviour and scaling.
pub fn betweenness(topo: &Topology, sample: Option<f64>, seed: Option<u64>) -> Vec<f64> {
    let n = topo.n_nodes();
    if n == 0 {
        return Vec::new();
    }
    let sources = sample_sources(n, sample, seed);
    let k = sources.len();
    if k == 0 {
        return vec![0.0; n];
    }

    let bc = accumulate_sources(
        n,
        &sources,
        || BrandesScratch::new(n),
        |s, acc, sc| brandes_from(topo, s, acc, sc),
    );

    if sample.is_some() {
        let scale = n as f64 / k as f64;
        bc.into_iter().map(|x| x * scale).collect()
    } else {
        bc
    }
}

/// One Brandes single-source pass, accumulating dependencies into `bc`. `sc` is
/// reusable scratch, clean on entry; on exit exactly the visited nodes are reset
/// to clean so the next source can reuse it. The traversal order, predecessor
/// insertion order, and reverse-accumulation sum order are unchanged from the
/// allocate-per-source version, so the result is byte-identical (the determinism
/// guarantee this kernel is held to).
fn brandes_from(topo: &Topology, s: u32, bc: &mut [f64], sc: &mut BrandesScratch) {
    let out = topo.out();
    let BrandesScratch {
        dist,
        sigma,
        delta,
        preds,
        order,
        queue,
    } = sc;

    dist[s as usize] = 0;
    sigma[s as usize] = 1.0;
    queue.push_back(s);

    while let Some(v) = queue.pop_front() {
        order.push(v);
        let dv = dist[v as usize];
        for &w in out.neighbors(v) {
            // First time we reach w: set its distance and enqueue.
            if dist[w as usize] < 0 {
                dist[w as usize] = dv + 1;
                queue.push_back(w);
            }
            // w is one hop further along a shortest path through v.
            if dist[w as usize] == dv + 1 {
                sigma[w as usize] += sigma[v as usize];
                preds[w as usize].push(v);
            }
        }
    }

    // Reverse accumulation of dependencies. Iterate `order` in reverse without
    // draining it, so it still lists the visited nodes for the reset below.
    for &w in order.iter().rev() {
        let coeff = (1.0 + delta[w as usize]) / sigma[w as usize];
        for &v in &preds[w as usize] {
            delta[v as usize] += sigma[v as usize] * coeff;
        }
        if w != s {
            bc[w as usize] += delta[w as usize];
        }
    }

    // Reset exactly the touched nodes back to the clean initial state (queue is
    // already drained), leaving `sc` ready for the next source with no reallocation.
    for &w in order.iter() {
        let wi = w as usize;
        dist[wi] = -1;
        sigma[wi] = 0.0;
        delta[wi] = 0.0;
        preds[wi].clear();
    }
    order.clear();
}

/// Weighted betweenness centrality (Dijkstra-based Brandes). As [`betweenness`],
/// but shortest paths are minimum total edge weight; `weights[e]` is the weight of
/// edge row `e` (non-negative; `len == n_edges`). Ties (equal-cost alternate
/// paths) are counted when their total costs are exactly equal.
pub fn betweenness_weighted(
    topo: &Topology,
    weights: &[f64],
    sample: Option<f64>,
    seed: Option<u64>,
) -> Vec<f64> {
    assert_eq!(
        weights.len(),
        topo.n_edges(),
        "weights length must equal the edge count"
    );
    let n = topo.n_nodes();
    if n == 0 {
        return Vec::new();
    }
    let sources = sample_sources(n, sample, seed);
    let k = sources.len();
    if k == 0 {
        return vec![0.0; n];
    }

    let bc = accumulate_sources(
        n,
        &sources,
        || BrandesWeightedScratch::new(n),
        |s, acc, sc| brandes_from_weighted(topo, weights, s, acc, sc),
    );

    if sample.is_some() {
        let scale = n as f64 / k as f64;
        bc.into_iter().map(|x| x * scale).collect()
    } else {
        bc
    }
}

/// One weighted Brandes single-source pass: Dijkstra settles nodes in
/// non-decreasing distance, recording shortest-path counts (`sigma`) and
/// predecessors, then a reverse pass over the settle order accumulates
/// dependencies into `bc`. `sc` is reusable scratch, clean on entry and reset to
/// clean (only its touched nodes) on exit — byte-identical to the
/// allocate-per-source version.
fn brandes_from_weighted(
    topo: &Topology,
    weights: &[f64],
    s: u32,
    bc: &mut [f64],
    sc: &mut BrandesWeightedScratch,
) {
    let out = topo.out();
    let BrandesWeightedScratch {
        dist,
        sigma,
        delta,
        preds,
        order,
        settled,
        heap,
    } = sc;

    dist[s as usize] = 0.0;
    sigma[s as usize] = 1.0;
    heap.push(Reverse((TotalF64(0.0), s)));

    while let Some(Reverse((TotalF64(du), u))) = heap.pop() {
        if settled[u as usize] {
            continue;
        }
        settled[u as usize] = true;
        order.push(u);
        for (&w_node, &e) in out.neighbors(u).iter().zip(out.edge_ids(u)) {
            if settled[w_node as usize] {
                continue;
            }
            let nd = du + weights[e as usize];
            if nd < dist[w_node as usize] {
                // Strictly shorter: reset counts and predecessors.
                dist[w_node as usize] = nd;
                sigma[w_node as usize] = sigma[u as usize];
                preds[w_node as usize].clear();
                preds[w_node as usize].push(u);
                heap.push(Reverse((TotalF64(nd), w_node)));
            } else if nd == dist[w_node as usize] {
                // Equal-cost alternate path: accumulate.
                sigma[w_node as usize] += sigma[u as usize];
                preds[w_node as usize].push(u);
            }
        }
    }

    // Reverse accumulation over the settle order (iterated without draining, so it
    // still lists the visited nodes for the reset below).
    for &w in order.iter().rev() {
        let coeff = (1.0 + delta[w as usize]) / sigma[w as usize];
        for &v in &preds[w as usize] {
            delta[v as usize] += sigma[v as usize] * coeff;
        }
        if w != s {
            bc[w as usize] += delta[w as usize];
        }
    }

    // Reset exactly the touched nodes; the heap is already drained by the loop.
    for &w in order.iter() {
        let wi = w as usize;
        dist[wi] = f64::INFINITY;
        sigma[wi] = 0.0;
        delta[wi] = 0.0;
        settled[wi] = false;
        preds[wi].clear();
    }
    order.clear();
}

/// `None` → every node; `Some(frac)` → a deterministic, seed-derived random subset
/// of `⌈frac · n⌉` sources, at least one when `n > 0`.
///
/// A `seed`-shuffled subset (not an evenly-strided one): dense index order is
/// first-seen input order, which correlates with data layout (files sorted by
/// community / crawl order), so a stride would be a *biased* sample for the
/// Brandes–Pich `n/k` estimator. `shuffled_order(n, seed)[..k]` is unbiased and
/// still fully reproducible given the seed.
fn sample_sources(n: usize, frac: Option<f64>, seed: Option<u64>) -> Vec<u32> {
    match frac {
        None => (0..n as u32).collect(),
        Some(f) => {
            let f = f.clamp(0.0, 1.0);
            let k = (((n as f64) * f).ceil() as usize).max(1).min(n);
            if k == 0 {
                return Vec::new();
            }
            let order = shuffled_order(n, seed.unwrap_or(DEFAULT_SEED));
            order[..k].to_vec()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn betweenness_of_a_directed_line() {
        // 0->1->2->3->4. Node v lies between every ordered pair (s, t) with
        // s < v < t: bc = [0, 3, 4, 3, 0].
        let t = Topology::build(5, vec![0, 1, 2, 3], vec![1, 2, 3, 4]);
        let bc = betweenness(&t, None, None);
        assert_eq!(bc, vec![0.0, 3.0, 4.0, 3.0, 0.0]);
    }

    #[test]
    fn diamond_splits_paths_evenly() {
        // 0->1, 0->2, 1->3, 2->3: two shortest 0->3 paths, so nodes 1 and 2 each
        // carry half the (0,3) pair -> 0.5 apiece; endpoints 0 and 3 carry none.
        let t = Topology::build(4, vec![0, 0, 1, 2], vec![1, 2, 3, 3]);
        let bc = betweenness(&t, None, None);
        assert_eq!(bc[0], 0.0);
        assert_eq!(bc[3], 0.0);
        assert!((bc[1] - 0.5).abs() < 1e-12, "got {}", bc[1]);
        assert!((bc[2] - 0.5).abs() < 1e-12, "got {}", bc[2]);
    }

    #[test]
    fn weighted_routes_dependency_through_the_cheap_node() {
        // Diamond 0->1->3 and 0->2->3. Equal weights split the (0,3) pair evenly
        // (0.5 each). Make the 0->1->3 route strictly cheaper and all of it flows
        // through node 1; node 2 carries none.
        // edge rows: 0=(0->1),1=(0->2),2=(1->3),3=(2->3)
        let t = Topology::build(4, vec![0, 0, 1, 2], vec![1, 2, 3, 3]);
        let even = betweenness_weighted(&t, &[1.0, 1.0, 1.0, 1.0], None, None);
        assert!((even[1] - 0.5).abs() < 1e-12 && (even[2] - 0.5).abs() < 1e-12);

        let cheap_left = betweenness_weighted(&t, &[1.0, 5.0, 1.0, 5.0], None, None);
        assert!((cheap_left[1] - 1.0).abs() < 1e-12, "got {}", cheap_left[1]);
        assert!(cheap_left[2].abs() < 1e-12, "got {}", cheap_left[2]);
    }

    #[test]
    fn sample_scales_and_stays_bounded() {
        let t = Topology::build(5, vec![0, 1, 2, 3], vec![1, 2, 3, 4]);
        // A partial-source estimate is non-negative and finite; the middle node
        // still ranks at or above the endpoints.
        let bc = betweenness(&t, Some(0.5), Some(7));
        assert!(bc.iter().all(|&x| x >= 0.0 && x.is_finite()));
        assert!(bc[2] >= bc[0]);
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(betweenness(&t, None, None).is_empty());
    }

    #[test]
    fn directed_cycle_is_vertex_symmetric_across_chunks() {
        // An 80-node directed cycle is vertex-transitive, so every node has the
        // *same* betweenness. 80 > the 64-source accumulation chunk, so this
        // exercises per-source scratch reuse both within and across chunks; a
        // missed reset between sources would make the values diverge.
        let n = 80u32;
        let src: Vec<u32> = (0..n).collect();
        let dst: Vec<u32> = (0..n).map(|i| (i + 1) % n).collect();
        let t = Topology::build(n as usize, src, dst);
        let bc = betweenness(&t, None, None);
        assert_eq!(bc.len(), n as usize);
        for &x in &bc {
            assert!(
                (x - bc[0]).abs() < 1e-9,
                "cycle betweenness must be uniform"
            );
        }
        assert!(bc[0] > 0.0);
    }

    #[test]
    fn sampled_betweenness_is_seed_reproducible() {
        // 7-node ring plus a couple of chords, so a 0.5 sample is a proper subset.
        let t = Topology::build(
            7,
            vec![0, 1, 2, 3, 4, 5, 6, 0, 2],
            vec![1, 2, 3, 4, 5, 6, 0, 3, 5],
        );
        assert_eq!(
            betweenness(&t, Some(0.5), Some(7)),
            betweenness(&t, Some(0.5), Some(7)),
            "same seed must give the same sampled estimate"
        );
        // sample_sources picks a seed-shuffled subset, so different seeds generally
        // pick different sources (and here different estimates).
        let a = sample_sources(7, Some(0.5), Some(1));
        let b = sample_sources(7, Some(0.5), Some(2));
        assert_eq!(a.len(), b.len());
        assert_ne!(a, b);
    }
}
