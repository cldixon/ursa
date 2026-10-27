//! Louvain community detection — modularity optimisation by local moving and
//! graph aggregation.
//!
//! Two phases alternate until modularity stops improving (Blondel et al.):
//!
//! 1. **Local moving.** Each node, visited in a `seed`-derived order, leaves its
//!    community and joins the neighbouring community that most increases
//!    modularity (staying put if nothing does). A moved node's neighbours are
//!    revisited, until no node can improve or the gain becomes negligible (see
//!    [`one_level`]).
//! 2. **Aggregation.** Each community collapses to a super-node; intra-community
//!    edges become a weighted self-loop and inter-community edges become weighted
//!    super-edges. Local moving then runs on the smaller weighted graph.
//!
//! Edges are treated as **undirected**: a directed input is symmetrised (each
//! directed edge contributes its weight in both directions; parallel/opposite
//! edges accumulate). [`louvain`] uses unit edge weights; [`louvain_weighted`]
//! takes a per-edge weight (gathered via `edge_ids`). `resolution` (γ) scales the
//! null-model term: larger γ favours smaller communities.

use std::collections::VecDeque;

use crate::parallel::*;

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

/// Stop a level once a full sweep's worth of node visits (see [`one_level`])
/// raises modularity by less than this. NetworkX's `louvain_communities` default
/// `threshold`; in the same units (a modularity difference, so independent of
/// graph size). On the measured random, scale-free and planted graphs (250k–4M
/// edges) no sweep-equivalent of work gained under 2e-5, so this never trims real
/// progress there; it is a backstop against a level that keeps a few nodes trading
/// places for negligible gains (the shape of #125's tail).
const SWEEP_GAIN_THRESHOLD: f64 = 1e-7;

/// Hard backstop on one level's work, in sweeps of the working graph (one sweep =
/// one visit of every node and every adjacency slot). Measured levels used 2.6–25
/// sweep-equivalents (scale-free and the dense late levels of random graphs at
/// the top), growing only slowly with size, where the former sweep-to-zero-moves
/// loop took up to 1,734 full sweeps (#125). 64 leaves headroom above that while
/// bounding a level at O(64 · (n + m)).
const MAX_SWEEPS: usize = 64;

/// One full local-moving phase over `graph`, returning a contiguous `0..k`
/// community label per node.
///
/// Nodes are visited from a FIFO queue seeded with every node in a `seed`-derived
/// order. When a node moves, its neighbours outside its new community are
/// re-queued (if not already queued), so settled regions are not re-scanned
/// (Traag et al.'s fast local moving, as in Leiden). This is not quite a sweep to
/// zero moves: a move also shifts two community totals, which can tip a
/// non-neighbour, and that node is not revisited. Re-sweeping for those stragglers
/// reintroduces the #125 tail (each pass gains ~1e-3 down to 1e-5 for a full
/// sweep's cost) and measured *lower* final modularity on random, scale-free and
/// planted graphs, so it is not done. The level ends when the queue empties, when
/// a sweep's worth of visits gains less than [`SWEEP_GAIN_THRESHOLD`], or after
/// [`MAX_SWEEPS`] sweeps' worth. Serial and seeded, so deterministic and
/// independent of the thread count.
fn one_level(graph: &Graph, resolution: f64, seed: u64) -> Vec<u32> {
    let n = graph.n();
    let mut comm: Vec<u32> = (0..n as u32).collect();
    let mut tot: Vec<f64> = graph.k.clone(); // Σ weighted-degree of each community
    let mut queue: VecDeque<u32> = shuffled_order(n, seed).into();
    let mut queued = vec![true; n];
    // Weight from the current node into each neighbouring community, dense over
    // community ids (`< n`), plus the communities touched for this node. Summed in
    // adjacency order, exactly as a map entry would be, and reset after each node.
    let mut weight_to = vec![0.0f64; n];
    let mut seen = vec![false; n];
    let mut touched: Vec<u32> = Vec::new();

    // Work is counted in adjacency slots visited (plus one per node visit), so a
    // sweep is `n + 2m` slots whatever the order in which nodes are visited.
    let sweep = n + graph.targets.len();
    let budget = sweep.saturating_mul(MAX_SWEEPS);
    let (mut work, mut next_check) = (0usize, sweep);
    // Raw gain (in `best_gain - stay` units) accumulated since the last check;
    // `· 2 / m2` converts it to a modularity difference.
    let mut gain = 0.0f64;

    while let Some(u) = queue.pop_front() {
        queued[u as usize] = false;
        let ci = comm[u as usize];
        let ku = graph.k[u as usize];
        let (nbrs, ws) = graph.neighbors(u);
        work += nbrs.len() + 1;

        // Weight from u into each neighbouring community.
        for (&v, &w) in nbrs.iter().zip(ws) {
            let c = comm[v as usize];
            if !std::mem::replace(&mut seen[c as usize], true) {
                touched.push(c);
            }
            weight_to[c as usize] += w;
        }

        // Tentatively remove u from its community.
        tot[ci as usize] -= ku;

        // Gain of staying in ci (the baseline every move must strictly beat).
        let stay = weight_to[ci as usize] - resolution * tot[ci as usize] * ku / graph.m2;
        // Scan neighbour communities in ascending-id order, never in the order
        // they were met: with the epsilon, "tie" is non-transitive, so scan
        // order could change the partition. Ascending order with a strict
        // improvement (> best + eps) keeps the smallest id on a tie (it's seen
        // first) without letting chained near-ties drag the anchor.
        touched.sort_unstable();
        let mut best_c = ci;
        let mut best_gain = stay;
        for &c in &touched {
            let w_in = std::mem::take(&mut weight_to[c as usize]);
            seen[c as usize] = false;
            if c == ci {
                continue;
            }
            let gain = w_in - resolution * tot[c as usize] * ku / graph.m2;
            if gain > best_gain + 1e-12 {
                best_gain = gain;
                best_c = c;
            }
        }
        touched.clear();

        // Commit (re-insert into the chosen community).
        tot[best_c as usize] += ku;
        comm[u as usize] = best_c;
        if best_c != ci {
            gain += best_gain - stay;
            for &v in nbrs {
                if !queued[v as usize] && comm[v as usize] != best_c {
                    queued[v as usize] = true;
                    queue.push_back(v);
                }
            }
        }

        if work >= next_check {
            if gain * 2.0 / graph.m2 < SWEEP_GAIN_THRESHOLD || work >= budget {
                break;
            }
            gain = 0.0;
            next_check = work + sweep;
        }
    }
    renumber(&comm)
}

/// An undirected, weighted working graph in flat CSR form (`u32` targets and
/// `f64` weights in parallel arrays). Self-loops are kept apart from the adjacency
/// so the weighted degree `k` can count them (twice, per convention) without
/// polluting the neighbour scan.
struct Graph {
    /// Symmetric adjacency: edge `{u, v}` of weight `w` appears as `(v, w)` in
    /// `u`'s segment and `(u, w)` in `v`'s. Segment `u` is
    /// `offsets[u]..offsets[u + 1]`, sorted by neighbour id with no duplicates.
    offsets: Vec<usize>,
    targets: Vec<u32>,
    weights: Vec<f64>,
    self_loop: Vec<f64>,
    /// Weighted degree: `Σ weights[segment i] + 2 · self_loop[i]`.
    k: Vec<f64>,
    /// `2m` — total weighted degree over all nodes.
    m2: f64,
}

/// Split `0..n` into at most `parts` contiguous node ranges of roughly equal
/// segment volume under `offsets` (length `n + 1`), returned as `parts + 1`
/// boundaries. Only the work split depends on this; every node's result is
/// computed independently, so output never depends on `parts`.
fn node_blocks(offsets: &[usize], parts: usize) -> Vec<usize> {
    let n = offsets.len() - 1;
    let total = offsets[n];
    let mut bounds = vec![0usize];
    for p in 1..parts {
        let target = total / parts * p;
        let b = offsets.partition_point(|&o| o < target).min(n);
        if b > *bounds.last().unwrap() {
            bounds.push(b);
        }
    }
    if *bounds.last().unwrap() < n || bounds.len() == 1 {
        bounds.push(n);
    }
    bounds
}

/// Carve `buf` into the consecutive pieces `[offsets[bounds[i]]..offsets[bounds[i + 1]]]`.
fn split_blocks<'a, T>(
    mut buf: &'a mut [T],
    offsets: &[usize],
    bounds: &[usize],
) -> Vec<&'a mut [T]> {
    let mut pieces = Vec::with_capacity(bounds.len() - 1);
    for w in bounds.windows(2) {
        let (head, rest) = buf.split_at_mut(offsets[w[1]] - offsets[w[0]]);
        pieces.push(head);
        buf = rest;
    }
    pieces
}

impl Graph {
    fn n(&self) -> usize {
        self.self_loop.len()
    }

    #[inline]
    fn neighbors(&self, u: u32) -> (&[u32], &[f64]) {
        let (s, e) = (self.offsets[u as usize], self.offsets[u as usize + 1]);
        (&self.targets[s..e], &self.weights[s..e])
    }

    /// Build the initial undirected working graph. `weights` (per edge row, gathered
    /// via `edge_ids`) sets each edge's weight; `None` uses unit weights.
    ///
    /// The dominant (level-0) graph is built by counting sort rather than one
    /// `HashMap` per node: each non-self edge `(u, v, w)` scatters `(v, w)` into
    /// `u`'s segment and `(u, w)` into `v`'s, then each segment is sorted by
    /// neighbour id and parallel edges coalesced (weights summed) in place. Sorted
    /// segments make the neighbour order, and with it every weighted sum,
    /// deterministic.
    ///
    /// The histogram and scatter run over contiguous chunks of source nodes in
    /// parallel. Each chunk's per-node counts become its base cursor into every
    /// segment, so a segment receives its entries in exactly the order a serial
    /// row-order pass would produce, whatever the chunking (the same scheme as
    /// `Adjacency::transpose`).
    fn from_topology(topo: &Topology, weights: Option<&[f64]>, mask: Option<&EdgeMask>) -> Graph {
        let n = topo.n_nodes();
        let out = topo.out();
        let m = out.targets.len();
        // Under a subgraph mask, only kept edges enter the working graph. The
        // histogram and scatter below share this predicate so segment sizes match.
        let keep = |e: u32| mask.is_none_or(|m| m.keep(e));

        // Source-node chunks of roughly equal out-slot count. Chunk count is capped
        // by mean degree, as in `transpose`, so the per-chunk histograms (n u32s
        // each) stay within a small multiple of the edge list.
        const PARALLEL_MIN_EDGES: usize = 1 << 16;
        let threads = current_num_threads();
        let n_chunks = if m < PARALLEL_MIN_EDGES || threads <= 1 || in_worker() {
            1
        } else {
            threads.min((m / n.max(1)).max(4))
        };
        let out_offsets: Vec<usize> = out.offsets.iter().map(|&o| o as usize).collect();
        let chunks = node_blocks(&out_offsets, n_chunks);
        let ranges: Vec<(usize, usize)> = chunks.windows(2).map(|w| (w[0], w[1])).collect();

        // Pass 1: per-chunk degree histogram; every non-self edge counts toward
        // both endpoints.
        let hist = |&(lo, hi): &(usize, usize)| {
            let mut h = vec![0u32; n];
            for u in lo as u32..hi as u32 {
                for (&v, &e) in out.neighbors(u).iter().zip(out.edge_ids(u)) {
                    if keep(e) && u != v {
                        h[u as usize] += 1;
                        h[v as usize] += 1;
                    }
                }
            }
            h
        };
        let mut hists: Vec<Vec<u32>> = ranges.par_iter().map(hist).collect();
        let mut offsets = vec![0usize; n + 1];
        for u in 0..n {
            offsets[u + 1] = offsets[u] + hists.iter().map(|h| h[u] as usize).sum::<usize>();
        }
        // Each chunk's histogram becomes its cursor into every segment, relative
        // to the segment start (a node's degree is at most `m`, so `u32` holds it).
        let mut running = vec![0u32; n];
        for h in hists.iter_mut() {
            for (slot, run) in h.iter_mut().zip(running.iter_mut()) {
                let cnt = *slot;
                *slot = *run;
                *run += cnt;
            }
        }
        drop(running);

        // Pass 2: scatter each edge into both endpoints' segments, and sum
        // self-loops (a node's own, so chunk-local). Positions are disjoint across
        // chunks (the cursors partition each segment), so writes never overlap;
        // the pointer crosses threads as a plain address, as in `transpose`.
        let total = offsets[n];
        let mut pairs: Vec<(u32, f64)> = vec![(0, 0.0); total];
        let addr = pairs.as_mut_ptr() as usize;
        let scatter = |(&(lo, hi), mut cur): (&(usize, usize), Vec<u32>)| {
            let mut loops = vec![0.0f64; hi - lo];
            for u in lo as u32..hi as u32 {
                for (&v, &e) in out.neighbors(u).iter().zip(out.edge_ids(u)) {
                    if !keep(e) {
                        continue;
                    }
                    let w = weights.map_or(1.0, |ws| ws[e as usize]);
                    if u == v {
                        loops[u as usize - lo] += w;
                        continue;
                    }
                    for (a, b) in [(u, v), (v, u)] {
                        let pos = offsets[a as usize] + cur[a as usize] as usize;
                        cur[a as usize] += 1;
                        // SAFETY: `pos < total` and is claimed by exactly one
                        // (chunk, slot); `pairs` outlives the loop.
                        unsafe { *(addr as *mut (u32, f64)).add(pos) = (b, w) };
                    }
                }
            }
            loops
        };
        let self_loop: Vec<f64> = ranges
            .par_iter()
            .zip(hists)
            .map(scatter)
            .collect::<Vec<_>>()
            .concat();

        // Pass 3: sort each segment by neighbour id and coalesce parallel edges
        // into its prefix, in place.
        let blocks = node_blocks(&offsets, 4 * threads);
        let block_ranges: Vec<(usize, usize)> = blocks.windows(2).map(|w| (w[0], w[1])).collect();
        let lens: Vec<usize> = split_blocks(&mut pairs, &offsets, &blocks)
            .into_par_iter()
            .zip(block_ranges.par_iter())
            .map(|(buf, &(lo, hi))| {
                let base = offsets[lo];
                let mut lens = Vec::with_capacity(hi - lo);
                for u in lo..hi {
                    let seg = &mut buf[offsets[u] - base..offsets[u + 1] - base];
                    seg.sort_unstable_by_key(|&(v, _)| v);
                    let mut len = 0;
                    for i in 0..seg.len() {
                        if len > 0 && seg[len - 1].0 == seg[i].0 {
                            seg[len - 1].1 += seg[i].1;
                        } else {
                            seg[len] = seg[i];
                            len += 1;
                        }
                    }
                    lens.push(len);
                }
                lens
            })
            .collect::<Vec<_>>()
            .concat();

        // Pass 4: compact the coalesced prefixes into the SoA CSR.
        let mut new_offsets = vec![0usize; n + 1];
        for u in 0..n {
            new_offsets[u + 1] = new_offsets[u] + lens[u];
        }
        let mut targets = vec![0u32; new_offsets[n]];
        let mut wts = vec![0.0f64; new_offsets[n]];
        split_blocks(&mut targets, &new_offsets, &blocks)
            .into_par_iter()
            .zip(split_blocks(&mut wts, &new_offsets, &blocks))
            .zip(block_ranges.par_iter())
            .for_each(|((ts, ws), &(lo, hi))| {
                let mut k = 0;
                for u in lo..hi {
                    for &(v, wt) in &pairs[offsets[u]..offsets[u] + lens[u]] {
                        ts[k] = v;
                        ws[k] = wt;
                        k += 1;
                    }
                }
            });
        Graph::finalize(new_offsets, targets, wts, self_loop)
    }

    /// Collapse each community into a super-node.
    ///
    /// Each community's members are visited in ascending node order, summing edge
    /// weights into a dense per-community accumulator; its neighbours come out
    /// sorted by community id. Every float sum therefore runs in a fixed order.
    /// Communities are independent, so contiguous blocks of them are built in
    /// parallel, each with one scratch accumulator reused across its communities.
    fn aggregate(&self, comm: &[u32], c: usize) -> Graph {
        let n = self.n();
        // Members of each community, ascending, via a counting sort on `comm`.
        let mut start = vec![0usize; c + 1];
        for &cu in comm {
            start[cu as usize + 1] += 1;
        }
        for i in 0..c {
            start[i + 1] += start[i];
        }
        let mut members = vec![0u32; n];
        let mut cursor = start[..c].to_vec();
        for (u, &cu) in comm.iter().enumerate() {
            members[cursor[cu as usize]] = u as u32;
            cursor[cu as usize] += 1;
        }
        // Contiguous community blocks of roughly equal member count, each emitting
        // its slice of the CSR; the dense scratch is per worker, not per block.
        let blocks: Vec<(usize, usize)> = node_blocks(&start, 4 * current_num_threads())
            .windows(2)
            .map(|w| (w[0], w[1]))
            .collect();

        type Block = (Vec<usize>, Vec<u32>, Vec<f64>, Vec<f64>);
        let built: Vec<Block> = blocks
            .par_iter()
            .map_init(
                || (vec![0.0f64; c], vec![false; c], Vec::<u32>::new()),
                |(acc, seen, touched), &(lo, hi)| {
                    let (mut lens, mut ts, mut ws, mut loops) =
                        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                    for cu in lo..hi {
                        let mut self_loop = 0.0f64;
                        for &u in &members[start[cu]..start[cu + 1]] {
                            self_loop += self.self_loop[u as usize];
                            let (nbrs, wts) = self.neighbors(u);
                            for (&v, &wt) in nbrs.iter().zip(wts) {
                                let cv = comm[v as usize] as usize;
                                if cv == cu {
                                    // Each intra-community edge is seen from both
                                    // endpoints, so halve to land one loop-weight per
                                    // undirected edge.
                                    self_loop += wt / 2.0;
                                } else {
                                    if !std::mem::replace(&mut seen[cv], true) {
                                        touched.push(cv as u32);
                                    }
                                    acc[cv] += wt;
                                }
                            }
                        }
                        touched.sort_unstable();
                        for &cv in touched.iter() {
                            seen[cv as usize] = false;
                            ts.push(cv);
                            ws.push(std::mem::take(&mut acc[cv as usize]));
                        }
                        lens.push(touched.len());
                        touched.clear();
                        loops.push(self_loop);
                    }
                    (lens, ts, ws, loops)
                },
            )
            .collect();

        let mut offsets = Vec::with_capacity(c + 1);
        offsets.push(0usize);
        let (mut targets, mut weights, mut self_loop) =
            (Vec::new(), Vec::new(), Vec::with_capacity(c));
        for (lens, ts, ws, loops) in built {
            for len in lens {
                offsets.push(offsets.last().unwrap() + len);
            }
            targets.extend(ts);
            weights.extend(ws);
            self_loop.extend(loops);
        }
        Graph::finalize(offsets, targets, weights, self_loop)
    }

    fn finalize(
        offsets: Vec<usize>,
        targets: Vec<u32>,
        weights: Vec<f64>,
        self_loop: Vec<f64>,
    ) -> Graph {
        let k: Vec<f64> = (0..self_loop.len())
            .into_par_iter()
            .map(|i| weights[offsets[i]..offsets[i + 1]].iter().sum::<f64>() + 2.0 * self_loop[i])
            .collect();
        let m2 = k.iter().sum();
        Graph {
            offsets,
            targets,
            weights,
            self_loop,
            k,
            m2,
        }
    }
}

/// Relabel community ids (each `< comm.len()`) to a contiguous `0..k` by first
/// appearance.
fn renumber(comm: &[u32]) -> Vec<u32> {
    let mut map = vec![u32::MAX; comm.len()];
    let mut next = 0u32;
    comm.iter()
        .map(|&c| {
            let slot = &mut map[c as usize];
            if *slot == u32::MAX {
                *slot = next;
                next += 1;
            }
            *slot
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
    fn weighted_runs_are_reproducible_with_inexact_weights() {
        // Irrational-ish weights make float sums order-sensitive, so this pins the
        // aggregation order: every run must agree exactly.
        let n = 600u32;
        let (mut src, mut dst, mut w) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..6000u32 {
            let a = (i * 7) % n;
            let b = (a + 1 + (i * 13) % 40) % n; // local structure -> real communities
            src.push(a);
            dst.push(b);
            w.push(((i % 97) as f64 + 1.0).sqrt() / 3.0);
        }
        let t = Topology::build(n as usize, src, dst);
        let first = louvain_weighted(&t, &w, None, 1.0, Some(5));
        assert!(
            first.iter().max().unwrap() > &0,
            "found more than one community"
        );
        for _ in 0..5 {
            assert_eq!(louvain_weighted(&t, &w, None, 1.0, Some(5)), first);
        }
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

    /// A deterministic pseudo-random graph with inexact weights (an LCG, so no RNG
    /// dependency): enough edges to take `from_topology`'s parallel path, with
    /// parallel edges, opposite edges and self-loops.
    fn random_weighted(n: u32, m: usize) -> (Topology, Vec<f64>) {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        let (mut src, mut dst, mut w) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..m {
            let a = next() % n;
            // Mostly local edges (real communities), some long-range.
            let b = if i % 4 == 0 {
                next() % n
            } else {
                (a + next() % 30) % n
            };
            src.push(a);
            dst.push(b);
            w.push(((i % 89) as f64 + 1.0).sqrt() / 7.0);
        }
        (Topology::build(n as usize, src, dst), w)
    }

    #[test]
    fn recovers_a_ring_of_cliques() {
        // Eight 5-cliques, each joined to the next by one edge: the textbook
        // modularity optimum is one community per clique.
        let (k, size) = (8u32, 5u32);
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for c in 0..k {
            for a in 0..size {
                for b in a + 1..size {
                    src.push(c * size + a);
                    dst.push(c * size + b);
                }
            }
            src.push(c * size);
            dst.push(((c + 1) % k) * size + 1);
        }
        let t = Topology::build((k * size) as usize, src, dst);
        for seed in 0..5 {
            let comm = louvain(&t, None, 1.0, Some(seed));
            for u in 0..(k * size) as usize {
                assert_eq!(comm[u], comm[u - u % size as usize], "seed {seed}");
            }
            assert_eq!(*comm.iter().max().unwrap(), k - 1, "seed {seed}");
        }
    }

    #[cfg(feature = "rayon")]
    #[test]
    fn working_graph_is_identical_on_any_thread_count() {
        // The chunked histogram/scatter must hand every segment its entries in row
        // order whatever the chunking, so the coalesced weights match bit for bit.
        let (t, w) = random_weighted(5000, 120_000);
        let build = |threads: usize| {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                let g = Graph::from_topology(&t, Some(&w), None);
                let comm: Vec<u32> = (0..g.n() as u32).map(|u| u / 7).collect();
                let agg = g.aggregate(&comm, g.n().div_ceil(7));
                (g, agg)
            })
        };
        let bits = |g: &Graph| {
            (
                g.offsets.clone(),
                g.targets.clone(),
                g.weights.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                g.self_loop.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                g.k.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                g.m2.to_bits(),
            )
        };
        let (g1, a1) = build(1);
        for threads in [2, 3, 8] {
            let (g, a) = build(threads);
            assert!(
                bits(&g) == bits(&g1),
                "level-0 graph diverged at {threads} threads"
            );
            assert!(
                bits(&a) == bits(&a1),
                "aggregate diverged at {threads} threads"
            );
        }
        // Coalescing: segments are strictly sorted (parallel edges merged).
        for u in 0..g1.n() as u32 {
            assert!(g1.neighbors(u).0.windows(2).all(|p| p[0] < p[1]));
        }
    }
}
