//! The CSR topology index.
//!
//! Three load-bearing design details:
//!
//! 1. **Dense internal indexing** — handled upstream by [`IdMap`]; this struct
//!    only ever sees `u32` node indices.
//! 2. **The edge permutation array** (`edge_ids`) — CSR reorders edges by source,
//!    but attributes stay in the original Arrow columns. `edge_ids[k]` maps CSR
//!    slot `k` back to its original row, so a weighted kernel can gather
//!    `weight[edge_ids[k]]` on the fly. It is also the hook for future subgraph
//!    views (a bitmask over the parent CSR instead of a rebuild after `filter`),
//!    so it is materialized from day one even for unweighted use.
//! 3. **Directional laziness** — CSR gives out-neighbours; in-neighbours need the
//!    transpose (CSC). The transpose is built on first demand and cached, so a
//!    pull-based-PageRank-only pipeline never pays for the direction it never uses.
//!
//! Construction is a counting sort over the endpoint columns — serial for small
//! graphs, and a byte-identical parallel counting sort (per-chunk histograms + a
//! disjoint parallel scatter) above a size threshold.
//!
//! [`IdMap`]: crate::id_map::IdMap

use std::sync::OnceLock;

/// Traversal direction — a *per-operation* parameter, never a property of the
/// frame. There is no directed/undirected split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Direction {
    Out,
    In,
    Both,
}

/// A per-edge-row presence mask — the "subgraph view" over a parent CSR (#114).
///
/// `keep(e)` is true when original edge row `e` is in the subgraph. It is indexed
/// by the **same original-row space** as [`Adjacency::edge_ids`], so a kernel
/// consults `mask.keep(edge_ids(u)[k])` at each CSR slot to skip masked-out edges —
/// no rebuild of the CSR, the dense id space and the `edge_ids` permutation are the
/// parent's. Stored as a bitset (1 bit/edge) so a 500M-edge mask is ~60 MB, not the
/// ~500 MB a `Vec<bool>` would cost — the viz WASM target cares.
#[derive(Debug, Clone)]
pub struct EdgeMask {
    words: Vec<u64>,
    n_edges: usize,
    n_kept: usize,
}

impl EdgeMask {
    /// Build from a per-row boolean slice (`keep[e]` = row `e` retained). The slice
    /// is in original edge-row order — the order the topology was built from.
    pub fn from_bools(keep: &[bool]) -> Self {
        let n_edges = keep.len();
        let mut words = vec![0u64; n_edges.div_ceil(64)];
        let mut n_kept = 0usize;
        for (e, &k) in keep.iter().enumerate() {
            if k {
                words[e >> 6] |= 1u64 << (e & 63);
                n_kept += 1;
            }
        }
        EdgeMask {
            words,
            n_edges,
            n_kept,
        }
    }

    /// Is original edge row `e` in the subgraph? Rows `>= n_edges` are not kept.
    #[inline]
    pub fn keep(&self, e: u32) -> bool {
        let e = e as usize;
        e < self.n_edges && (self.words[e >> 6] >> (e & 63)) & 1 == 1
    }

    /// The parent edge-row count this mask is defined over.
    #[inline]
    pub fn n_edges(&self) -> usize {
        self.n_edges
    }

    /// How many rows are kept (popcount) — the subgraph's edge count.
    #[inline]
    pub fn n_kept(&self) -> usize {
        self.n_kept
    }

    /// Intersect two masks over the same row space (a second `.filter()` narrows the
    /// subgraph — presence requires *both* predicates). Panics on a size mismatch.
    pub fn intersect(&self, other: &EdgeMask) -> EdgeMask {
        assert_eq!(
            self.n_edges, other.n_edges,
            "intersecting masks over different edge-row spaces"
        );
        let mut words = vec![0u64; self.words.len()];
        let mut n_kept = 0usize;
        for i in 0..words.len() {
            let w = self.words[i] & other.words[i];
            words[i] = w;
            n_kept += w.count_ones() as usize;
        }
        EdgeMask {
            words,
            n_edges: self.n_edges,
            n_kept,
        }
    }
}

/// One direction's adjacency in CSR form.
#[derive(Debug, Clone)]
pub struct Adjacency {
    /// Prefix-summed degrees; length `n_nodes + 1`. Neighbours of node `u` live
    /// in `targets[offsets[u]..offsets[u + 1]]`.
    ///
    /// `u64` offsets so a graph with more than `u32::MAX` edges still indexes
    /// correctly (target *nodes* remain `u32`).
    pub offsets: Vec<u64>,
    /// Dense target node per CSR slot; length `n_edges`.
    pub targets: Vec<u32>,
    /// Original edge row per CSR slot; length `n_edges`. The attribute-gather hook.
    pub edge_ids: Vec<u32>,
}

impl Adjacency {
    /// Counting sort grouping edges by `keys` (the grouping endpoint), recording
    /// the paired `other` endpoint and the original row index per slot. Dispatches
    /// to a parallel build above a size threshold; the parallel and serial builds
    /// produce **byte-identical** output (same within-segment row order).
    fn build(n_nodes: usize, keys: &[u32], other: &[u32]) -> Adjacency {
        debug_assert_eq!(keys.len(), other.len());
        let m = keys.len();
        // `edge_ids` stores the original row index as u32, so more than u32::MAX
        // edges would wrap silently and corrupt every weighted gather. A `u64` edge
        // space is a future feature flag; until then this is a clear, immediate
        // failure rather than silent corruption.
        assert!(
            m <= u32::MAX as usize,
            "more than u32::MAX (~4.29B) edges is beyond the v0.1 edge-id cap"
        );

        // Below the threshold (or single-worker), the serial two-pass sort wins —
        // the parallel path's per-chunk histograms aren't worth their overhead.
        const PARALLEL_MIN_EDGES: usize = 1 << 16;
        let n_chunks = rayon::current_num_threads();
        if m < PARALLEL_MIN_EDGES || n_chunks <= 1 || n_nodes == 0 {
            Self::build_serial(n_nodes, keys, other)
        } else {
            Self::build_parallel(n_nodes, keys, other, n_chunks)
        }
    }

    /// The serial two-pass counting sort: a degree histogram (prefix-summed into
    /// `offsets`) then a scatter into each node's segment, in original row order.
    fn build_serial(n_nodes: usize, keys: &[u32], other: &[u32]) -> Adjacency {
        let m = keys.len();
        // Pass 1: degree histogram, written into offsets[k + 1].
        let mut offsets = vec![0u64; n_nodes + 1];
        for &k in keys {
            offsets[k as usize + 1] += 1;
        }
        // Prefix sum -> start position of each node's segment.
        for i in 0..n_nodes {
            offsets[i + 1] += offsets[i];
        }

        // Pass 2: scatter each edge into its node's segment.
        let mut targets = vec![0u32; m];
        let mut edge_ids = vec![0u32; m];
        let mut cursor: Vec<u64> = offsets[..n_nodes].to_vec();
        for (row, (&k, &o)) in keys.iter().zip(other).enumerate() {
            let pos = cursor[k as usize] as usize;
            targets[pos] = o;
            edge_ids[pos] = row as u32;
            cursor[k as usize] += 1;
        }

        Adjacency {
            offsets,
            targets,
            edge_ids,
        }
    }

    /// The parallel counting sort: per-chunk degree histograms, a global prefix sum,
    /// then a **disjoint** parallel scatter. Each contiguous row-chunk `c` places
    /// its node-`k` edges into `[base_c[k], base_c[k] + count_c[k])`, positioned
    /// after every earlier chunk's node-`k` edges — so within each node's segment the
    /// edges remain in original row order, byte-identical to [`Self::build_serial`].
    /// The scatter writes are provably non-overlapping, so it runs lock-free.
    fn build_parallel(n_nodes: usize, keys: &[u32], other: &[u32], n_chunks: usize) -> Adjacency {
        use rayon::prelude::*;
        let m = keys.len();
        let chunk_size = m.div_ceil(n_chunks);

        // Pass 1 (parallel): a degree histogram per contiguous row-chunk.
        let mut hists: Vec<Vec<u32>> = keys
            .par_chunks(chunk_size)
            .map(|chunk| {
                let mut h = vec![0u32; n_nodes];
                for &k in chunk {
                    h[k as usize] += 1;
                }
                h
            })
            .collect();

        // Global degree -> offsets (each node's segment start), prefix-summed.
        let mut offsets = vec![0u64; n_nodes + 1];
        for h in &hists {
            for (k, &c) in h.iter().enumerate() {
                offsets[k + 1] += c as u64;
            }
        }
        for i in 0..n_nodes {
            offsets[i + 1] += offsets[i];
        }

        // Convert each chunk's histogram into its per-node **base cursor**, in place:
        // running[k] walks from the segment start, handing each chunk (in order) its
        // slice of node k's segment. Bases fit in u32 (<= m <= u32::MAX).
        let mut running: Vec<u32> = (0..n_nodes).map(|k| offsets[k] as u32).collect();
        for h in hists.iter_mut() {
            for (k, slot) in h.iter_mut().enumerate() {
                let cnt = *slot;
                *slot = running[k];
                running[k] += cnt;
            }
        }

        // Pass 2 (parallel scatter): each chunk owns disjoint output slots (its base
        // cursors), so the writes never overlap. The output pointers cross the
        // thread boundary as plain `usize` addresses (trivially `Send`), cast back
        // inside — the Vecs outlive the scatter and are untouched until it returns.
        let mut targets = vec![0u32; m];
        let mut edge_ids = vec![0u32; m];
        let targets_addr = targets.as_mut_ptr() as usize;
        let edge_ids_addr = edge_ids.as_mut_ptr() as usize;
        hists.into_par_iter().enumerate().for_each(|(c, mut base)| {
            let targets_ptr = targets_addr as *mut u32;
            let edge_ids_ptr = edge_ids_addr as *mut u32;
            let start = c * chunk_size;
            let end = (start + chunk_size).min(m);
            for row in start..end {
                let k = keys[row] as usize;
                let pos = base[k] as usize;
                base[k] += 1;
                // SAFETY: `pos` is unique across all (chunk, row) — the base-cursor
                // partition claims each slot for exactly one chunk, written once —
                // and `pos < m` is within both allocations.
                unsafe {
                    *targets_ptr.add(pos) = other[row];
                    *edge_ids_ptr.add(pos) = row as u32;
                }
            }
        });

        Adjacency {
            offsets,
            targets,
            edge_ids,
        }
    }

    /// Neighbours of dense node `u`.
    #[inline]
    pub fn neighbors(&self, u: u32) -> &[u32] {
        let s = self.offsets[u as usize] as usize;
        let e = self.offsets[u as usize + 1] as usize;
        &self.targets[s..e]
    }

    /// Original edge rows of dense node `u`'s incident edges — same order as
    /// [`Adjacency::neighbors`], so a weight column can be gathered per neighbour.
    #[inline]
    pub fn edge_ids(&self, u: u32) -> &[u32] {
        let s = self.offsets[u as usize] as usize;
        let e = self.offsets[u as usize + 1] as usize;
        &self.edge_ids[s..e]
    }

    /// Out-degree (or in-degree, depending which direction this is) of node `u`.
    #[inline]
    pub fn degree(&self, u: u32) -> u32 {
        (self.offsets[u as usize + 1] - self.offsets[u as usize]) as u32
    }
}

/// A symmetric, deduplicated, sorted undirected adjacency in flat CSR form.
///
/// `out ∪ in` neighbours per node with self-loops dropped and duplicates (parallel
/// edges, or a `u→v` matched by a separate `v→u`) collapsed — the exact semantics
/// the intersection kernels (`triangle_count`, `clustering_coefficient`) need.
/// Cached on [`Topology`] and built once: a pipeline that computes both kernels
/// (the spec's own example) shares the single build instead of rebuilding a fresh
/// `Vec<Vec<u32>>` per call, and the flat layout has far better locality than a
/// vector-of-vectors.
#[derive(Debug)]
pub struct UndirectedCsr {
    /// Prefix-summed undirected degrees; length `n_nodes + 1`.
    pub offsets: Vec<u64>,
    /// Sorted, deduplicated neighbours per node segment.
    pub targets: Vec<u32>,
}

impl UndirectedCsr {
    /// Sorted, deduplicated undirected neighbours of node `u`.
    #[inline]
    pub fn neighbors(&self, u: u32) -> &[u32] {
        let s = self.offsets[u as usize] as usize;
        let e = self.offsets[u as usize + 1] as usize;
        &self.targets[s..e]
    }

    /// Undirected degree of node `u` (distinct incident neighbours).
    #[inline]
    pub fn degree(&self, u: u32) -> usize {
        (self.offsets[u as usize + 1] - self.offsets[u as usize]) as usize
    }
}

/// The shared, immutable topology index.
///
/// Lives on the `EdgeFrame` as `Arc<Topology>`. Property-only transformations
/// clone the `Arc` (the index-preservation contract); structural transformations
/// drop it and it rebuilds lazily on the next graph op.
#[derive(Debug)]
pub struct Topology {
    n_nodes: usize,
    n_edges: usize,
    /// Out-adjacency, always built at construction (the common direction).
    out: Adjacency,
    /// In-adjacency (transpose / CSC), built on first demand from the out-CSR.
    inc: OnceLock<Adjacency>,
    /// Undirected sorted adjacency, built on first demand (triangle/clustering).
    undirected: OnceLock<UndirectedCsr>,
}

impl Topology {
    /// Build the topology from edges already expressed in dense `u32` space
    /// (see [`IdMap::from_edge_arrays`]). Out-adjacency is built eagerly; the
    /// transpose is deferred.
    ///
    /// # Panics
    ///
    /// If `src_dense`/`dst_dense` differ in length, or (debug builds) if any dense
    /// endpoint is `>= n_nodes`. Endpoints must already be interned into `0..n_nodes`
    /// (which [`IdMap::from_edge_arrays`] guarantees); an out-of-range id would
    /// otherwise index past the CSR offset array.
    ///
    /// [`IdMap::from_edge_arrays`]: crate::id_map::IdMap::from_edge_arrays
    pub fn build(n_nodes: usize, src_dense: Vec<u32>, dst_dense: Vec<u32>) -> Topology {
        assert_eq!(src_dense.len(), dst_dense.len(), "src/dst length mismatch");
        debug_assert!(
            src_dense
                .iter()
                .chain(&dst_dense)
                .all(|&d| (d as usize) < n_nodes),
            "dense endpoint id out of range (>= n_nodes)"
        );
        let n_edges = src_dense.len();
        let out = Adjacency::build(n_nodes, &src_dense, &dst_dense);
        // `src_dense`/`dst_dense` are intentionally not retained: the transpose and
        // the undirected cache are both recoverable from the out-CSR (see
        // `incoming`), so keeping them would be a permanent 8 B/edge overhead paid
        // even by pipelines that never build the transpose.
        Topology {
            n_nodes,
            n_edges,
            out,
            inc: OnceLock::new(),
            undirected: OnceLock::new(),
        }
    }

    /// Reconstruct the original `(src, dst)` endpoint columns, in original row
    /// order, from the out-CSR. For out-CSR slot `k` owned by node `u`,
    /// `edge_ids[k]` is the original row, `targets[k]` the destination, and the
    /// owner `u` the source — so the two columns invert the CSR exactly. Used to
    /// build the transpose without permanently retaining the dense endpoints;
    /// scattering by original row reproduces the same within-segment ordering the
    /// retained-endpoint build produced (so kernel outputs are unchanged).
    fn reconstruct_endpoints(&self) -> (Vec<u32>, Vec<u32>) {
        let m = self.n_edges;
        let mut src_by_row = vec![0u32; m];
        let mut dst_by_row = vec![0u32; m];
        for u in 0..self.n_nodes as u32 {
            let s = self.out.offsets[u as usize] as usize;
            let e = self.out.offsets[u as usize + 1] as usize;
            for k in s..e {
                let row = self.out.edge_ids[k] as usize;
                src_by_row[row] = u;
                dst_by_row[row] = self.out.targets[k];
            }
        }
        (src_by_row, dst_by_row)
    }

    #[inline]
    pub fn n_nodes(&self) -> usize {
        self.n_nodes
    }

    #[inline]
    pub fn n_edges(&self) -> usize {
        self.n_edges
    }

    /// Out-adjacency (source → destination).
    #[inline]
    pub fn out(&self) -> &Adjacency {
        &self.out
    }

    /// In-adjacency (destination → source). Built and cached on first call by
    /// reconstructing the original endpoints from the out-CSR and grouping by
    /// destination — the same result the retained-endpoint build produced, without
    /// the permanent dense-endpoint storage.
    pub fn incoming(&self) -> &Adjacency {
        self.inc.get_or_init(|| {
            let (src_by_row, dst_by_row) = self.reconstruct_endpoints();
            Adjacency::build(self.n_nodes, &dst_by_row, &src_by_row)
        })
    }

    /// Undirected sorted adjacency (`out ∪ in`, self-loops dropped, deduplicated),
    /// built and cached on first call. Shared by `triangle_count` and
    /// `clustering_coefficient` so a pipeline computing both pays the build once.
    pub fn undirected(&self) -> &UndirectedCsr {
        self.undirected.get_or_init(|| self.build_undirected())
    }

    /// Build the flat undirected CSR: per-node sorted/deduped `out ∪ in` neighbour
    /// lists (parallel, self-loops filtered), then flattened into one offsets +
    /// targets buffer.
    fn build_undirected(&self) -> UndirectedCsr {
        use rayon::prelude::*;
        let n = self.n_nodes;
        let out = &self.out;
        let inc = self.incoming();
        let lists: Vec<Vec<u32>> = (0..n as u32)
            .into_par_iter()
            .map(|u| {
                let mut nbrs: Vec<u32> = out
                    .neighbors(u)
                    .iter()
                    .chain(inc.neighbors(u))
                    .copied()
                    .filter(|&w| w != u)
                    .collect();
                nbrs.sort_unstable();
                nbrs.dedup();
                nbrs
            })
            .collect();
        let mut offsets = vec![0u64; n + 1];
        for u in 0..n {
            offsets[u + 1] = offsets[u] + lists[u].len() as u64;
        }
        let mut targets = Vec::with_capacity(offsets[n] as usize);
        for list in &lists {
            targets.extend_from_slice(list);
        }
        UndirectedCsr { offsets, targets }
    }

    /// Apply `f` to each neighbour of `u` in `dir` — out-neighbours, in-neighbours,
    /// or both adjacencies concatenated for `Both`. The one canonical directional
    /// neighbour walk shared by the frontier kernels (`bfs`, `k_hop`).
    ///
    /// With `mask = Some(m)` this is a **subgraph view**: a neighbour is visited only
    /// if its original edge row is kept (`m.keep(edge_ids[k])`), skipping masked-out
    /// edges without touching the CSR. `mask = None` is the full-graph fast path (no
    /// `edge_ids` gather).
    #[inline]
    pub fn for_each_neighbor<F: FnMut(u32)>(
        &self,
        u: u32,
        dir: Direction,
        mask: Option<&EdgeMask>,
        mut f: F,
    ) {
        let mut visit = |adj: &Adjacency| match mask {
            None => adj.neighbors(u).iter().for_each(|&v| f(v)),
            Some(m) => {
                for (&v, &e) in adj.neighbors(u).iter().zip(adj.edge_ids(u)) {
                    if m.keep(e) {
                        f(v);
                    }
                }
            }
        };
        match dir {
            Direction::Out => visit(&self.out),
            Direction::In => visit(self.incoming()),
            Direction::Both => {
                visit(&self.out);
                visit(self.incoming());
            }
        }
    }

    /// Apply `f(v, w)` to each neighbour of `u` in `dir` and the weight `w` of the
    /// connecting edge, gathered per CSR slot via `edge_ids` (both adjacencies
    /// merged for `Both`). `weights` is indexed by original edge row. With
    /// `mask = Some(m)`, masked-out edges are skipped (subgraph view).
    #[inline]
    pub fn for_each_weighted_neighbor<F: FnMut(u32, f64)>(
        &self,
        u: u32,
        weights: &[f64],
        dir: Direction,
        mask: Option<&EdgeMask>,
        mut f: F,
    ) {
        let mut visit = |adj: &Adjacency| {
            for (&v, &e) in adj.neighbors(u).iter().zip(adj.edge_ids(u)) {
                if mask.is_none_or(|m| m.keep(e)) {
                    f(v, weights[e as usize]);
                }
            }
        };
        match dir {
            Direction::Out => visit(&self.out),
            Direction::In => visit(self.incoming()),
            Direction::Both => {
                visit(&self.out);
                visit(self.incoming());
            }
        }
    }

    /// The undirected view honoring an edge mask (`out ∪ in` over kept edges only,
    /// self-loops dropped, deduplicated) — the subgraph-view input for the
    /// intersection kernels (`triangle_count`, `clustering_coefficient`). Unlike
    /// [`Topology::undirected`] this is **not cached**: it is specific to one mask,
    /// rebuilt per subgraph. It rebuilds only the undirected adjacency, not the
    /// directed CSR or the id map, so the parent index and dense id space are
    /// unchanged (no `build_index`).
    pub fn undirected_masked(&self, mask: &EdgeMask) -> UndirectedCsr {
        use rayon::prelude::*;
        let n = self.n_nodes;
        let out = &self.out;
        let inc = self.incoming();
        let kept = |adj: &Adjacency, u: u32| -> Vec<u32> {
            adj.neighbors(u)
                .iter()
                .zip(adj.edge_ids(u))
                .filter_map(|(&v, &e)| (mask.keep(e) && v != u).then_some(v))
                .collect()
        };
        let lists: Vec<Vec<u32>> = (0..n as u32)
            .into_par_iter()
            .map(|u| {
                let mut nbrs = kept(out, u);
                nbrs.extend(kept(inc, u));
                nbrs.sort_unstable();
                nbrs.dedup();
                nbrs
            })
            .collect();
        let mut offsets = vec![0u64; n + 1];
        for u in 0..n {
            offsets[u + 1] = offsets[u] + lists[u].len() as u64;
        }
        let mut targets = Vec::with_capacity(offsets[n] as usize);
        for list in &lists {
            targets.extend_from_slice(list);
        }
        UndirectedCsr { offsets, targets }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0 -> 1, 0 -> 2, 1 -> 2, 2 -> 0
    fn diamond() -> Topology {
        Topology::build(3, vec![0, 0, 1, 2], vec![1, 2, 2, 0])
    }

    #[test]
    fn parallel_build_is_byte_identical_to_serial() {
        use rand::{Rng, SeedableRng};
        use rand_chacha::ChaCha8Rng;
        let mut rng = ChaCha8Rng::seed_from_u64(0xC0FFEE);
        // Sizes that cross the parallel threshold, plus a skewed one (few nodes,
        // many edges) that stresses the per-chunk base-cursor partition.
        for &(n, m) in &[(2000usize, 100_000usize), (50_000, 300_000), (16, 80_000)] {
            let keys: Vec<u32> = (0..m).map(|_| rng.gen_range(0..n as u32)).collect();
            let other: Vec<u32> = (0..m).map(|_| rng.gen_range(0..n as u32)).collect();
            let serial = Adjacency::build_serial(n, &keys, &other);
            for n_chunks in [2usize, 3, 8] {
                let par = Adjacency::build_parallel(n, &keys, &other, n_chunks);
                assert_eq!(
                    serial.offsets, par.offsets,
                    "offsets (n={n}, chunks={n_chunks})"
                );
                assert_eq!(
                    serial.targets, par.targets,
                    "targets (n={n}, chunks={n_chunks})"
                );
                assert_eq!(
                    serial.edge_ids, par.edge_ids,
                    "edge_ids (n={n}, chunks={n_chunks})"
                );
            }
        }
    }

    #[test]
    fn out_adjacency_groups_by_source() {
        let t = diamond();
        assert_eq!(t.out().neighbors(0), &[1, 2]);
        assert_eq!(t.out().neighbors(1), &[2]);
        assert_eq!(t.out().neighbors(2), &[0]);
        assert_eq!(t.out().degree(0), 2);
    }

    #[test]
    fn transpose_is_lazy_and_correct() {
        let t = diamond();
        // in-neighbours of node 2 are 0 and 1
        let mut inc: Vec<u32> = t.incoming().neighbors(2).to_vec();
        inc.sort_unstable();
        assert_eq!(inc, vec![0, 1]);
        // node 0's only in-neighbour is 2
        assert_eq!(t.incoming().neighbors(0), &[2]);
    }

    #[test]
    fn edge_ids_track_original_rows() {
        let t = diamond();
        // node 0's incident out-edges are original rows 0 and 1
        let mut rows: Vec<u32> = t.out().edge_ids(0).to_vec();
        rows.sort_unstable();
        assert_eq!(rows, vec![0, 1]);
    }

    #[test]
    fn undirected_is_sorted_deduped_and_self_loop_free() {
        // 0->1, 0->2, 1->2, 2->0, plus a self-loop 1->1 and a duplicate 0->1.
        let t = Topology::build(3, vec![0, 0, 1, 2, 1, 0], vec![1, 2, 2, 0, 1, 1]);
        let u = t.undirected();
        // node 0: undirected neighbours {1, 2} (self-loop/dupes dropped)
        assert_eq!(u.neighbors(0), &[1, 2]);
        assert_eq!(u.degree(0), 2);
        // node 1: {0, 2}; node 2: {0, 1}
        assert_eq!(u.neighbors(1), &[0, 2]);
        assert_eq!(u.neighbors(2), &[0, 1]);
        // cached: a second call returns the same slice contents
        assert_eq!(t.undirected().neighbors(0), &[1, 2]);
    }

    #[test]
    fn incoming_matches_a_rebuild_from_endpoints() {
        // The transpose reconstructed from the out-CSR must equal a direct build.
        let t = diamond();
        for node in 0..3u32 {
            let got = t.incoming().neighbors(node).to_vec();
            // direct transpose of the diamond edges (0->1,0->2,1->2,2->0)
            let expected: Vec<u32> = match node {
                0 => vec![2],
                1 => vec![0],
                2 => vec![0, 1],
                _ => vec![],
            };
            let mut got_sorted = got.clone();
            got_sorted.sort_unstable();
            assert_eq!(got_sorted, expected, "in-neighbours of {node}");
        }
    }
}
