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
use std::collections::BinaryHeap;

use crate::parallel::*;

use super::bfs::TotalF64;
use super::rng::{shuffled_order, DEFAULT_SEED};
use crate::topology::{Direction, EdgeMask, Topology};

/// Brandes chunk size: each chunk of this many sources is summed into its own
/// partial in source order, and the partials are summed in chunk order. This
/// fixed two-level grouping *is* the summation order the results are pinned to,
/// so it must not change with the thread count.
const SRC_CHUNK: usize = 64;

/// Per-source Brandes scratch, as [`accumulate_sources`] drives it. After a
/// single-source pass, `order` lists the visited nodes and `delta` holds their
/// dependencies (zero elsewhere) until `reset` returns the scratch to clean.
trait BrandesState {
    fn order(&self) -> &[u32];
    fn delta(&self, w: u32) -> f64;
    fn reset(&mut self);
}

/// Add one source's dependencies (every visited node but the source) into `acc`
/// — the same per-node `+=` the fused reverse pass performs.
fn fold_source<S: BrandesState>(s: u32, sc: &S, acc: &mut [f64]) {
    for &w in sc.order() {
        if w != s {
            acc[w as usize] += sc.delta(w);
        }
    }
}

/// [`fold_source`] for a whole window of sources at once, in parallel over disjoint
/// node blocks: each node takes its additions from `slots` in source order, so its
/// sum is unchanged. Every node is visited, not just the reached ones — an
/// unreached node's `delta` is exactly `+0.0` and `acc` is never `-0.0` (it starts
/// at `+0.0` and only gains non-negative terms), so those additions leave `acc`
/// bit-for-bit as it was. Worth it when the window reaches much of the graph, where
/// a serial random-access fold would leave the pool idle.
fn fold_dense<S: BrandesState + Sync>(slots: &[S], batch: &[u32], acc: &mut [f64]) {
    acc.par_chunks_mut(FOLD_BLOCK)
        .enumerate()
        .for_each(|(b, block)| {
            let base = b * FOLD_BLOCK;
            for (sc, &s) in slots.iter().zip(batch) {
                for (i, x) in block.iter_mut().enumerate() {
                    let w = (base + i) as u32;
                    if w != s {
                        *x += sc.delta(w);
                    }
                }
            }
        });
}

/// Node-block size for the parallel elementwise folds.
const FOLD_BLOCK: usize = 1 << 14;

/// `bc[x] += acc[x]` for every node, in parallel over disjoint node blocks (each
/// node sees exactly one addition, so the split cannot change any sum).
fn add_into(bc: &mut [f64], acc: &[f64]) {
    bc.par_chunks_mut(FOLD_BLOCK)
        .zip(acc.par_chunks(FOLD_BLOCK))
        .for_each(|(b, a)| {
            for (x, y) in b.iter_mut().zip(a) {
                *x += y;
            }
        });
}

/// Sum per-source Brandes contributions deterministically. Rayon's `fold`/`reduce`
/// combines partials in a work-stealing-dependent order, so the f64 total varies
/// at the ULP level run to run. Here every node's total is pinned to one addition
/// order whatever the thread count: each fixed [`SRC_CHUNK`]-sized chunk of sources
/// is accumulated into a fresh zero partial in source order, and the chunk partials
/// are added into the total in chunk order.
///
/// Two schedules produce that order:
///
/// - **Per chunk** (enough chunks to occupy the pool): chunks run in parallel, a
///   bounded window of `2 × threads` at a time, and each window's partials are
///   added in chunk order before the next window starts — so at most that many
///   `n`-length partials are alive, not one per chunk (`k / 64` of them, ~125 GB
///   for exact betweenness at `n = 1M`). `per_source` adds straight into the
///   chunk's partial.
/// - **Per source** (fewer chunks than threads — e.g. a small `sample`, where one
///   chunk would otherwise run serially): within each chunk, a window of sources
///   runs in parallel, each leaving its dependencies in its own scratch, and they
///   are folded into the chunk's partial in source order (serially over the
///   reached nodes, or in parallel over node blocks when the window reached much
///   of the graph — [`fold_dense`]).
///
/// Scratch (`make_scratch`) is pooled and reused across chunks and windows, so the
/// `n`-sized buffers are allocated once per worker rather than once per chunk;
/// `per_source` touches only the nodes it visits and `reset` restores exactly those,
/// so a reused scratch is indistinguishable from a fresh one.
fn accumulate_sources<S: BrandesState + Send + Sync>(
    n: usize,
    sources: &[u32],
    make_scratch: impl Fn() -> S + Sync,
    per_source: impl Fn(u32, Option<&mut [f64]>, &mut S) + Sync,
) -> Vec<f64> {
    let threads = current_num_threads();
    let window = 2 * threads;
    let n_chunks = sources.len().div_ceil(SRC_CHUNK);
    let mut bc = vec![0.0f64; n];

    if threads > 1 && n_chunks < threads {
        let mut slots: Vec<S> = (0..window.min(sources.len()))
            .map(|_| make_scratch())
            .collect();
        for chunk in sources.chunks(SRC_CHUNK) {
            let mut acc = vec![0.0f64; n];
            for batch in chunk.chunks(window) {
                slots
                    .par_iter_mut()
                    .zip(batch.par_iter())
                    .for_each(|(sc, &s)| {
                        sc.reset();
                        per_source(s, None, sc);
                    });
                let reached: usize = slots
                    .iter()
                    .zip(batch)
                    .map(|(sc, _)| sc.order().len())
                    .sum();
                if reached >= n / 4 {
                    fold_dense(&slots, batch, &mut acc);
                } else {
                    for (sc, &s) in slots.iter().zip(batch) {
                        fold_source(s, sc, &mut acc);
                    }
                }
            }
            add_into(&mut bc, &acc);
        }
        return bc;
    }

    let pool = std::sync::Mutex::new(Vec::<S>::new());
    for group in sources.chunks(SRC_CHUNK * window) {
        let partials: Vec<Vec<f64>> = group
            .par_chunks(SRC_CHUNK)
            .map(|chunk| {
                let popped = pool.lock().unwrap().pop();
                let mut sc = popped.unwrap_or_else(&make_scratch);
                let mut acc = vec![0.0f64; n];
                for &s in chunk {
                    per_source(s, Some(&mut acc), &mut sc);
                    sc.reset();
                }
                pool.lock().unwrap().push(sc);
                acc
            })
            .collect();
        for partial in &partials {
            add_into(&mut bc, partial);
        }
    }
    bc
}

/// Brandes' reverse dependency accumulation over stored predecessor lists (the
/// weighted pass; the unweighted pass runs the same walk over its packed node
/// state, reading predecessors off the in-adjacency). Walking the Dijkstra settle
/// `order` in reverse, each node folds `(1 + delta[w]) / sigma[w]` back onto its
/// predecessors and (unless it is the source, and when `bc` is given) adds its
/// dependency into `bc`. `delta[w]` is final once `w` is reached in the reverse
/// walk, so a caller passing `bc = None` and folding `delta` afterwards
/// ([`fold_source`]) adds the same values.
///
/// `order` is iterated without draining, so it still lists the visited nodes for the
/// caller's per-source scratch reset.
fn reverse_accumulate(
    s: u32,
    order: &[u32],
    sigma: &[f64],
    delta: &mut [f64],
    preds: &[Vec<u32>],
    mut bc: Option<&mut [f64]>,
) {
    for &w in order.iter().rev() {
        let coeff = (1.0 + delta[w as usize]) / sigma[w as usize];
        for &v in &preds[w as usize] {
            delta[v as usize] += sigma[v as usize] * coeff;
        }
        if w != s {
            if let Some(bc) = bc.as_deref_mut() {
                bc[w as usize] += delta[w as usize];
            }
        }
    }
}

/// One node's unweighted Brandes state. Kept together (rather than as parallel
/// `dist`/`sigma`/`delta` arrays) because both passes touch all three for a node
/// at once: on graphs larger than cache, that is one memory access per visited
/// edge instead of up to three.
#[derive(Clone, Copy)]
struct NodeState {
    sigma: f64,
    delta: f64,
    dist: i32,
}

const CLEAN: NodeState = NodeState {
    sigma: 0.0,
    delta: 0.0,
    dist: -1,
};

/// Reusable per-source scratch for unweighted Brandes. Held clean between sources:
/// every source resets exactly the nodes it visited (recorded in `order`, which
/// also serves as the BFS queue) back to the initial state, so the next source sees
/// the same buffers a fresh allocation would give — no re-zeroing of untouched
/// nodes, no re-allocation.
struct BrandesScratch {
    node: Vec<NodeState>,
    order: Vec<u32>,
}

impl BrandesScratch {
    fn new(n: usize) -> Self {
        BrandesScratch {
            node: vec![CLEAN; n],
            order: Vec::new(),
        }
    }
}

impl BrandesState for BrandesScratch {
    fn order(&self) -> &[u32] {
        &self.order
    }
    fn delta(&self, w: u32) -> f64 {
        self.node[w as usize].delta
    }
    fn reset(&mut self) {
        for &w in self.order.iter() {
            self.node[w as usize] = CLEAN;
        }
        self.order.clear();
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

impl BrandesState for BrandesWeightedScratch {
    fn order(&self) -> &[u32] {
        &self.order
    }
    fn delta(&self, w: u32) -> f64 {
        self.delta[w as usize]
    }
    fn reset(&mut self) {
        // The heap is already drained by the Dijkstra loop.
        for &w in self.order.iter() {
            let wi = w as usize;
            self.dist[wi] = f64::INFINITY;
            self.sigma[wi] = 0.0;
            self.delta[wi] = 0.0;
            self.settled[wi] = false;
            self.preds[wi].clear();
        }
        self.order.clear();
    }
}

/// Betweenness centrality per node (directed, following out-edges). See the
/// module docs for the exact-vs-`sample` behaviour and scaling.
pub fn betweenness(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    sample: Option<f64>,
    seed: Option<u64>,
) -> Vec<f64> {
    let n = topo.n_nodes();
    if n == 0 {
        return Vec::new();
    }
    let sources = sample_sources(n, sample, seed);
    let k = sources.len();
    if k == 0 {
        return vec![0.0; n];
    }
    // The reverse pass reads predecessors off the in-adjacency; build the cached
    // transpose here, not inside the parallel loop.
    topo.incoming();

    let bc = accumulate_sources(
        n,
        &sources,
        || BrandesScratch::new(n),
        |s, acc, sc| brandes_from(topo, mask, s, acc, sc),
    );

    if sample.is_some() {
        let scale = n as f64 / k as f64;
        bc.into_iter().map(|x| x * scale).collect()
    } else {
        bc
    }
}

/// One Brandes single-source pass, accumulating dependencies into `bc` (or, with
/// `bc = None`, leaving them in `sc.delta` for [`fold_source`]). `sc` is reusable
/// scratch, clean on entry; the caller resets it ([`BrandesState::reset`]) before
/// the next source. `order` doubles as the BFS queue (a head index walks it), which
/// dequeues in exactly the order a separate FIFO would.
///
/// Predecessors are not stored: `v` is a shortest-path predecessor of `w` exactly
/// when a kept edge `v -> w` has `dist[v] == dist[w] - 1`, so the reverse pass reads
/// them off the in-adjacency (one entry per edge slot, so parallel edges still count
/// once each). That visits a node's predecessors in in-CSR order rather than BFS
/// order, but within one `w` each predecessor's `delta` receives the same value, so
/// every node's `delta` still sees the same additions in the same order. The
/// traversal order and reverse-accumulation sum order are unchanged from the
/// allocate-per-source version, so the result is byte-identical (the determinism
/// guarantee this kernel is held to).
fn brandes_from(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    s: u32,
    mut bc: Option<&mut [f64]>,
    sc: &mut BrandesScratch,
) {
    let BrandesScratch { node, order } = sc;

    node[s as usize].dist = 0;
    node[s as usize].sigma = 1.0;
    order.push(s);

    let mut head = 0;
    while head < order.len() {
        let v = order[head];
        head += 1;
        let NodeState {
            sigma: sv,
            dist: dv,
            ..
        } = node[v as usize];
        // Visit each out-neighbour `w`; under a mask, masked-out rows are skipped
        // (the subgraph's shortest-path counts).
        topo.for_each_neighbor(v, Direction::Out, mask, |w| {
            let nw = &mut node[w as usize];
            // First time we reach w: set its distance and enqueue.
            if nw.dist < 0 {
                nw.dist = dv + 1;
                order.push(w);
            }
            // w is one hop further along a shortest path through v.
            if nw.dist == dv + 1 {
                nw.sigma += sv;
            }
        });
    }

    // Reverse accumulation of dependencies — `reverse_accumulate`'s walk, over the
    // packed node state. The source is `order[0]`; it has no predecessors and no
    // dependency of its own, so the walk stops before it.
    for &w in order[1..].iter().rev() {
        let NodeState {
            sigma: sw,
            delta: dw,
            dist: dpw,
        } = node[w as usize];
        let coeff = (1.0 + dw) / sw;
        topo.for_each_neighbor(w, Direction::In, mask, |v| {
            let nv = &mut node[v as usize];
            if nv.dist == dpw - 1 {
                nv.delta += nv.sigma * coeff;
            }
        });
        if let Some(bc) = bc.as_deref_mut() {
            bc[w as usize] += dw;
        }
    }
}

/// Weighted betweenness centrality (Dijkstra-based Brandes). As [`betweenness`],
/// but shortest paths are minimum total edge weight; `weights[e]` is the weight of
/// edge row `e` (non-negative; `len == n_edges`). Ties (equal-cost alternate
/// paths) are counted when their total costs are exactly equal.
pub fn betweenness_weighted(
    topo: &Topology,
    weights: &[f64],
    mask: Option<&EdgeMask>,
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
        |s, acc, sc| brandes_from_weighted(topo, weights, mask, s, acc, sc),
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
/// dependencies into `bc` (or leaves them in `sc.delta` when `bc` is `None`). `sc`
/// is reusable scratch, clean on entry and reset by the caller — byte-identical to
/// the allocate-per-source version.
fn brandes_from_weighted(
    topo: &Topology,
    weights: &[f64],
    mask: Option<&EdgeMask>,
    s: u32,
    bc: Option<&mut [f64]>,
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
            if !mask.is_none_or(|m| m.keep(e)) {
                continue; // edge not in the subgraph
            }
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

    // Reverse accumulation over the settle order (shared with the unweighted pass).
    reverse_accumulate(s, order, sigma, delta, preds, bc);
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
        let bc = betweenness(&t, None, None, None);
        assert_eq!(bc, vec![0.0, 3.0, 4.0, 3.0, 0.0]);
    }

    #[test]
    fn diamond_splits_paths_evenly() {
        // 0->1, 0->2, 1->3, 2->3: two shortest 0->3 paths, so nodes 1 and 2 each
        // carry half the (0,3) pair -> 0.5 apiece; endpoints 0 and 3 carry none.
        let t = Topology::build(4, vec![0, 0, 1, 2], vec![1, 2, 3, 3]);
        let bc = betweenness(&t, None, None, None);
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
        let even = betweenness_weighted(&t, &[1.0, 1.0, 1.0, 1.0], None, None, None);
        assert!((even[1] - 0.5).abs() < 1e-12 && (even[2] - 0.5).abs() < 1e-12);

        let cheap_left = betweenness_weighted(&t, &[1.0, 5.0, 1.0, 5.0], None, None, None);
        assert!((cheap_left[1] - 1.0).abs() < 1e-12, "got {}", cheap_left[1]);
        assert!(cheap_left[2].abs() < 1e-12, "got {}", cheap_left[2]);
    }

    #[test]
    fn sample_scales_and_stays_bounded() {
        let t = Topology::build(5, vec![0, 1, 2, 3], vec![1, 2, 3, 4]);
        // A partial-source estimate is non-negative and finite; the middle node
        // still ranks at or above the endpoints.
        let bc = betweenness(&t, None, Some(0.5), Some(7));
        assert!(bc.iter().all(|&x| x >= 0.0 && x.is_finite()));
        assert!(bc[2] >= bc[0]);
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(betweenness(&t, None, None, None).is_empty());
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
        let bc = betweenness(&t, None, None, None);
        assert_eq!(bc.len(), n as usize);
        for &x in &bc {
            assert!(
                (x - bc[0]).abs() < 1e-9,
                "cycle betweenness must be uniform"
            );
        }
        assert!(bc[0] > 0.0);
    }

    /// The pre-#176 kernel, verbatim in its summation order: a fresh allocation per
    /// source, a FIFO queue, stored predecessor lists, per-64-source chunk partials
    /// summed serially in chunk order. The optimised kernel must match it exactly.
    fn reference(
        t: &Topology,
        w: Option<&[f64]>,
        mask: Option<&EdgeMask>,
        sample: Option<f64>,
        seed: Option<u64>,
    ) -> Vec<f64> {
        use std::collections::VecDeque;
        let n = t.n_nodes();
        let out = t.out();
        let sources = sample_sources(n, sample, seed);
        let mut bc = vec![0.0f64; n];
        for chunk in sources.chunks(SRC_CHUNK) {
            let mut acc = vec![0.0f64; n];
            for &s in chunk {
                let mut sigma = vec![0.0f64; n];
                let mut delta = vec![0.0f64; n];
                let mut preds: Vec<Vec<u32>> = vec![Vec::new(); n];
                let mut order = Vec::new();
                sigma[s as usize] = 1.0;
                let kept = |e: u32| mask.is_none_or(|m| m.keep(e));
                match w {
                    None => {
                        let mut dist = vec![-1i64; n];
                        dist[s as usize] = 0;
                        let mut queue = VecDeque::from([s]);
                        while let Some(v) = queue.pop_front() {
                            order.push(v);
                            let dv = dist[v as usize];
                            for (&x, &e) in out.neighbors(v).iter().zip(out.edge_ids(v)) {
                                if !kept(e) {
                                    continue;
                                }
                                if dist[x as usize] < 0 {
                                    dist[x as usize] = dv + 1;
                                    queue.push_back(x);
                                }
                                if dist[x as usize] == dv + 1 {
                                    sigma[x as usize] += sigma[v as usize];
                                    preds[x as usize].push(v);
                                }
                            }
                        }
                    }
                    Some(wt) => {
                        let mut dist = vec![f64::INFINITY; n];
                        let mut settled = vec![false; n];
                        let mut heap = BinaryHeap::new();
                        dist[s as usize] = 0.0;
                        heap.push(Reverse((TotalF64(0.0), s)));
                        while let Some(Reverse((TotalF64(du), u))) = heap.pop() {
                            if std::mem::replace(&mut settled[u as usize], true) {
                                continue;
                            }
                            order.push(u);
                            for (&x, &e) in out.neighbors(u).iter().zip(out.edge_ids(u)) {
                                if !kept(e) || settled[x as usize] {
                                    continue;
                                }
                                let nd = du + wt[e as usize];
                                if nd < dist[x as usize] {
                                    dist[x as usize] = nd;
                                    sigma[x as usize] = sigma[u as usize];
                                    preds[x as usize] = vec![u];
                                    heap.push(Reverse((TotalF64(nd), x)));
                                } else if nd == dist[x as usize] {
                                    sigma[x as usize] += sigma[u as usize];
                                    preds[x as usize].push(u);
                                }
                            }
                        }
                    }
                }
                for &x in order.iter().rev() {
                    let coeff = (1.0 + delta[x as usize]) / sigma[x as usize];
                    for &v in &preds[x as usize] {
                        delta[v as usize] += sigma[v as usize] * coeff;
                    }
                    if x != s {
                        acc[x as usize] += delta[x as usize];
                    }
                }
            }
            for (b, a) in bc.iter_mut().zip(&acc) {
                *b += a;
            }
        }
        if sample.is_some() {
            let scale = n as f64 / sources.len() as f64;
            bc.iter_mut().for_each(|x| *x *= scale);
        }
        bc
    }

    /// A deterministic pseudo-random multigraph (parallel edges and self-loops
    /// included) with float weights, from a hand-rolled LCG.
    fn lcg_graph(n: usize, m: usize) -> (Topology, Vec<f64>) {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        let (mut src, mut dst, mut wt) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..m {
            src.push(next() % n as u32);
            dst.push(next() % n as u32);
            // Few distinct weights, so equal-cost ties actually occur.
            wt.push(0.5 + (next() % 4) as f64 * 0.25);
        }
        (Topology::build(n, src, dst), wt)
    }

    #[test]
    fn matches_the_reference_kernel_bit_for_bit() {
        // Dense (each source reaches most nodes) and sparse (small reaches) graphs;
        // exact, single-chunk and multi-chunk samples, so both accumulation
        // schedules and both fold paths run; with and without a mask.
        for (n, m) in [(300, 2400), (600, 500)] {
            let (t, wt) = lcg_graph(n, m);
            let keep: Vec<bool> = (0..m).map(|e| e % 5 != 0).collect();
            let mask = EdgeMask::from_bools(&keep);
            for sample in [None, Some(0.05), Some(0.3)] {
                for mk in [None, Some(&mask)] {
                    assert_eq!(
                        betweenness(&t, mk, sample, Some(3)),
                        reference(&t, None, mk, sample, Some(3)),
                        "unweighted n={n} sample={sample:?} masked={}",
                        mk.is_some()
                    );
                    assert_eq!(
                        betweenness_weighted(&t, &wt, mk, sample, Some(3)),
                        reference(&t, Some(&wt), mk, sample, Some(3)),
                        "weighted n={n} sample={sample:?} masked={}",
                        mk.is_some()
                    );
                }
            }
        }
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
            betweenness(&t, None, Some(0.5), Some(7)),
            betweenness(&t, None, Some(0.5), Some(7)),
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
