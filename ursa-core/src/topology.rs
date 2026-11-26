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
//!    views (a bitmask over the parent CSR instead of a rebuild after `filter`).
//!    It is built only when something reads it (#170, see [`EdgeIds`]): an
//!    unweighted, unmasked pipeline never does.
//! 3. **Directional laziness** — CSR gives out-neighbours; in-neighbours need the
//!    transpose (CSC). The transpose is built on first demand and cached, so a
//!    pull-based-PageRank-only pipeline never pays for the direction it never uses.
//!    A topology read back from storage can instead be given an
//!    [`IncomingLoader`] that reads a stored transpose on first demand (see
//!    [`Topology::with_incoming_loader`]).
//!
//! Construction is a counting sort over the endpoint columns — serial for small
//! graphs, and a byte-identical parallel counting sort (per-chunk histograms + a
//! disjoint parallel scatter) above a size threshold.
//!
//! [`IdMap`]: crate::id_map::IdMap

use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use arrow::array::{Array, BooleanArray};
use arrow::buffer::ScalarBuffer;

use crate::edge_ids::Rebuild;
#[cfg(unix)]
pub use crate::edge_ids::SpilledRows;
pub use crate::edge_ids::{EdgeIds, KeepRows, RowSource};
pub use crate::packed::Packed;

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

    /// Build from an Arrow boolean array (`keep[e]` = row `e` retained), treating a
    /// null as *not kept*. Works on the packed bits directly: the validity bitmap is
    /// ANDed in and the 64-bit words are copied across, with no per-row loop.
    pub fn from_boolean_array(keep: &BooleanArray) -> Self {
        let bits = match keep.nulls() {
            Some(nulls) => keep.values() & nulls.inner(),
            None => keep.values().clone(),
        };
        // Arrow and `EdgeMask` both store row `e` at bit `e % 64` of word `e / 64`.
        let words: Vec<u64> = bits
            .inner()
            .bit_chunks(bits.offset(), bits.len())
            .iter_padded()
            .take(bits.len().div_ceil(64)) // no trailing word past a 64-bit boundary
            .collect();
        EdgeMask {
            words,
            n_edges: bits.len(),
            n_kept: bits.count_set_bits(),
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
        for ((out, &a), &b) in words.iter_mut().zip(&self.words).zip(&other.words) {
            *out = a & b;
            n_kept += out.count_ones() as usize;
        }
        EdgeMask {
            words,
            n_edges: self.n_edges,
            n_kept,
        }
    }

    /// The subgraph of `topo` induced by a node set: an edge row is kept iff both
    /// of its endpoints are in `nodes` (`nodes[v]`, one flag per dense node).
    /// Built in dense space off the CSR, one pass over the reached nodes' out-slots
    /// (#116). Further restricted by `within` when given (a traversal over a view).
    pub fn induced(topo: &Topology, nodes: &[bool], within: Option<&EdgeMask>) -> EdgeMask {
        use crate::parallel::*;
        use std::sync::atomic::{AtomicU64, Ordering};
        let n_edges = topo.n_edges();
        let words: Vec<AtomicU64> = (0..n_edges.div_ceil(64))
            .map(|_| AtomicU64::new(0))
            .collect();
        let out = topo.out();
        (0..topo.n_nodes() as u32)
            .into_par_iter()
            .filter(|&u| nodes[u as usize])
            .for_each(|u| {
                out.for_each_neighbor_row(u, |v, e| {
                    if nodes[v as usize] && within.is_none_or(|m| m.keep(e)) {
                        words[(e >> 6) as usize].fetch_or(1u64 << (e & 63), Ordering::Relaxed);
                    }
                });
            });
        let words: Vec<u64> = words.into_iter().map(AtomicU64::into_inner).collect();
        let n_kept = words.iter().map(|w| w.count_ones() as usize).sum();
        EdgeMask {
            words,
            n_edges,
            n_kept,
        }
    }

    /// The kept rows' bits, 64 rows per word (row `e` at bit `e % 64` of word
    /// `e / 64`), for hashing or comparing masks.
    pub fn words(&self) -> &[u64] {
        &self.words
    }
}

/// One direction's adjacency in CSR form.
///
/// The arrays are Arrow [`ScalarBuffer`]s: a built adjacency owns them (a
/// `Vec` moved in, no copy), and one read from a stored index can borrow them
/// from a memory-mapped file instead of copying them (the SQL engine's
/// `.ursa` database, #382). Either way they read as slices (`&adj.targets[..]`),
/// and cloning one clones a reference, not the array.
#[derive(Debug, Clone)]
pub struct Adjacency {
    /// Prefix-summed degrees; length `n_nodes + 1`. Neighbours of node `u` live
    /// in `targets[offsets[u]..offsets[u + 1]]`.
    ///
    /// `u64` offsets so a graph with more than `u32::MAX` edges still indexes
    /// correctly (target *nodes* remain `u32`).
    pub offsets: ScalarBuffer<u64>,
    /// Dense target node per CSR slot; length `n_edges`.
    pub targets: ScalarBuffer<u32>,
    /// Original edge row per CSR slot; length `n_edges`. The attribute-gather hook.
    /// Built only when something reads it (#170, see [`EdgeIds`]): reading it
    /// as a slice (`&adj.edge_ids[..]`, [`Adjacency::edge_ids`]) builds it.
    pub edge_ids: EdgeIds,
    /// The compressed layout (#173, see [`crate::packed`]): when set, the
    /// neighbours are these sorted, varint-coded segments, and `targets` is
    /// empty. `offsets` still counts slots, so a node's degree and its edge
    /// ids are read without decoding. Read neighbours through
    /// [`Self::for_each_neighbor`], [`Self::neighbors_in`] or a
    /// [`NeighborCursor`], which serve either layout.
    pub packed: Option<Packed>,
}

/// A position in one node's neighbours, for a walk that leaves a node and
/// comes back to it (a depth-first search's frame): see
/// [`Adjacency::cursor`]. The same for either layout.
#[derive(Debug, Clone, Copy)]
pub struct NeighborCursor {
    /// The next slot.
    slot: usize,
    /// The slot after the node's last.
    end: usize,
    /// The next byte of a compressed segment.
    byte: usize,
    /// The neighbour read last from a compressed segment.
    prev: u32,
}

impl NeighborCursor {
    /// The slot of the next neighbour, which [`Adjacency::advance`] returns
    /// next.
    #[inline]
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// Whether every neighbour has been read.
    #[inline]
    pub fn is_done(&self) -> bool {
        self.slot >= self.end
    }
}

impl Adjacency {
    /// Check that this is a well-formed adjacency over `n_nodes` nodes (see
    /// [`Topology::from_parts`]). `which` names it in the message.
    fn check(&self, n_nodes: usize, which: &str) -> Result<(), String> {
        self.check_shape(n_nodes, which)?;
        let m = self.n_slots();
        let bad = |why: &str| Err(format!("the {which}-adjacency {why}"));
        match &self.packed {
            None => {
                if self.targets.iter().any(|&t| t as usize >= n_nodes) {
                    return bad("has a target out of range");
                }
            }
            Some(p) => {
                for u in 0..n_nodes as u32 {
                    let degree = self.degree(u) as usize;
                    if let Err(why) = crate::packed::check_segment(p.segment(u), degree, n_nodes) {
                        return bad(why);
                    }
                }
            }
        }
        // Ids not built yet are built from the index itself, correct by
        // construction: only ids handed in are checked.
        if !self.edge_ids.is_built() {
            return Ok(());
        }
        let mut seen = vec![false; m];
        for &e in self.edge_ids.iter() {
            match seen.get_mut(e as usize) {
                Some(s) if !*s => *s = true,
                _ => return bad("has edge ids that are not a permutation of its edges"),
            }
        }
        Ok(())
    }

    /// The part of [`Self::check`] that reads only the offsets: the lengths of
    /// the arrays, and `n_nodes + 1` non-decreasing offsets from 0 to the slot
    /// count. It costs `O(n_nodes)` and reads neither targets nor edge ids, so
    /// it leaves a memory-mapped adjacency's large arrays unread. Every
    /// neighbour slice ([`Self::neighbors`]) is then in bounds.
    fn check_shape(&self, n_nodes: usize, which: &str) -> Result<(), String> {
        let bad = |why: String| Err(format!("the {which}-adjacency {why}"));
        let m = match &self.packed {
            None => self.targets.len(),
            Some(p) => {
                if !self.targets.is_empty() {
                    return bad("has both targets and compressed segments".into());
                }
                let (offsets, bytes) = (&p.byte_offsets, p.bytes.len() as u64);
                if offsets.len() != n_nodes + 1 {
                    return bad(format!(
                        "has {} byte offsets for {n_nodes} nodes",
                        offsets.len()
                    ));
                }
                if offsets[0] != 0 || offsets[n_nodes] != bytes {
                    return bad("has byte offsets that do not span its segments".into());
                }
                if offsets.windows(2).any(|w| w[0] > w[1]) {
                    return bad("has decreasing byte offsets".into());
                }
                self.offsets.last().map_or(0, |&m| m as usize)
            }
        };
        if m > u32::MAX as usize {
            return bad(format!("has {m} edges, beyond the u32 edge-id cap"));
        }
        if self.offsets.len() != n_nodes + 1 {
            return bad(format!(
                "has {} offsets for {n_nodes} nodes",
                self.offsets.len()
            ));
        }
        if self.edge_ids.len() != m {
            return bad(format!(
                "has {} edge ids for {m} edges",
                self.edge_ids.len()
            ));
        }
        if self.offsets[0] != 0 || self.offsets[n_nodes] != m as u64 {
            return bad("has offsets that do not span its edges".into());
        }
        if self.offsets.windows(2).any(|w| w[0] > w[1]) {
            return bad("has decreasing offsets".into());
        }
        Ok(())
    }

    /// Counting sort grouping edges by `keys` (the grouping endpoint), recording
    /// the paired `other` endpoint and the original row index per slot. Dispatches
    /// to a parallel build above a size threshold; the parallel and serial builds
    /// produce **byte-identical** output (same within-segment row order).
    #[cfg(test)]
    fn build(n_nodes: usize, keys: &[u32], other: &[u32]) -> Adjacency {
        let (offsets, targets, edge_ids) = Self::build_parts(n_nodes, keys, other, true);
        Adjacency {
            offsets: offsets.into(),
            targets: targets.into(),
            edge_ids: edge_ids.unwrap_or_default().into(),
            packed: None,
        }
    }

    /// [`Self::build`]'s offsets, targets and, if `with_ids`, edge ids.
    fn build_parts(
        n_nodes: usize,
        keys: &[u32],
        other: &[u32],
        with_ids: bool,
    ) -> (Vec<u64>, Vec<u32>, Option<Vec<u32>>) {
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
        // Each chunk carries an `n_nodes`-long u32 histogram, so the transient
        // cost is `chunks · n_nodes · 4` bytes whatever the edge count: on a
        // many-core machine and a sparse graph it would outgrow the CSR itself.
        // Chunks are capped at `m / n_nodes` (histograms within about 4 B/edge,
        // one CSR array's worth), but never below 4, so small machines keep their
        // full parallelism. The output is identical for any chunk count.
        let n_chunks = crate::parallel::current_num_threads().min((m / n_nodes.max(1)).max(4));
        // Also serial on a rayon worker: see `parallel::in_worker`.
        if m < PARALLEL_MIN_EDGES || n_chunks <= 1 || n_nodes == 0 || crate::parallel::in_worker() {
            Self::build_serial(n_nodes, keys, other, with_ids)
        } else {
            Self::build_parallel(n_nodes, keys, other, n_chunks, with_ids)
        }
    }

    /// The serial two-pass counting sort: a degree histogram (prefix-summed into
    /// `offsets`) then a scatter into each node's segment, in original row order.
    fn build_serial(
        n_nodes: usize,
        keys: &[u32],
        other: &[u32],
        with_ids: bool,
    ) -> (Vec<u64>, Vec<u32>, Option<Vec<u32>>) {
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
        let mut edge_ids = vec![0u32; if with_ids { m } else { 0 }];
        let mut cursor: Vec<u64> = offsets[..n_nodes].to_vec();
        for (row, (&k, &o)) in keys.iter().zip(other).enumerate() {
            let pos = cursor[k as usize] as usize;
            targets[pos] = o;
            if with_ids {
                edge_ids[pos] = row as u32;
            }
            cursor[k as usize] += 1;
        }
        (offsets, targets, with_ids.then_some(edge_ids))
    }

    /// The parallel counting sort: per-chunk degree histograms, a global prefix sum,
    /// then a **disjoint** parallel scatter. Each contiguous row-chunk `c` places
    /// its node-`k` edges into `[base_c[k], base_c[k] + count_c[k])`, positioned
    /// after every earlier chunk's node-`k` edges — so within each node's segment the
    /// edges remain in original row order, byte-identical to [`Self::build_serial`].
    /// The scatter writes are provably non-overlapping, so it runs lock-free.
    fn build_parallel(
        n_nodes: usize,
        keys: &[u32],
        other: &[u32],
        n_chunks: usize,
        with_ids: bool,
    ) -> (Vec<u64>, Vec<u32>, Option<Vec<u32>>) {
        use crate::parallel::*;
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
        // The per-node passes over the histograms run in parallel over
        // ranges of nodes: they cost `n_chunks * n_nodes`, which on a graph
        // with many more nodes than edges per node is as much as the
        // scatter (#532). Only the prefix sum is serial.
        const NODES: usize = 1 << 16;
        let mut offsets = vec![0u64; n_nodes + 1];
        offsets[1..]
            .par_chunks_mut(NODES)
            .enumerate()
            .for_each(|(b, out)| {
                for (j, o) in out.iter_mut().enumerate() {
                    let k = b * NODES + j;
                    *o = hists.iter().map(|h| h[k] as u64).sum();
                }
            });
        for i in 0..n_nodes {
            offsets[i + 1] += offsets[i];
        }

        // Convert each chunk's histogram into its per-node **base cursor**, in place:
        // running[k] walks from the segment start, handing each chunk (in order) its
        // slice of node k's segment. Bases fit in u32 (<= m <= u32::MAX).
        let mut running: Vec<u32> = offsets[..n_nodes].par_iter().map(|&o| o as u32).collect();
        for h in hists.iter_mut() {
            h.par_chunks_mut(NODES)
                .zip(running.par_chunks_mut(NODES))
                .for_each(|(h, running)| {
                    for (slot, run) in h.iter_mut().zip(running) {
                        let cnt = *slot;
                        *slot = *run;
                        *run += cnt;
                    }
                });
        }

        // Pass 2 (parallel scatter): each chunk owns disjoint output slots (its base
        // cursors), so the writes never overlap. The output pointers cross the
        // thread boundary as plain `usize` addresses (trivially `Send`), cast back
        // inside — the Vecs outlive the scatter and are untouched until it returns.
        let mut targets = vec![0u32; m];
        let mut edge_ids = vec![0u32; if with_ids { m } else { 0 }];
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
                    if with_ids {
                        *edge_ids_ptr.add(pos) = row as u32;
                    }
                }
            }
        });
        (offsets, targets, with_ids.then_some(edge_ids))
    }

    /// The transpose of `out` (grouped by target instead of source), with each
    /// node's segment in original row order, exactly as [`Self::build`] would
    /// produce it from the per-row endpoints, but without materializing them.
    ///
    /// Its edge ids are kept when `out`'s are held (a pipeline that reads
    /// them reads them in both directions). Otherwise none are built (#170):
    /// for identity out-ids the slot order is the row order; for out-ids
    /// that would be rebuilt from the rows' sources, those sources are
    /// streamed once to place each row ([`transpose_rows`]); and the
    /// transpose's own ids are built from the same when first read.
    ///
    /// [`transpose_rows`]: crate::edge_ids::transpose_rows
    fn transpose(out: &Adjacency, n_nodes: usize) -> Adjacency {
        if out.is_packed() {
            return Self::transpose_packed(out, n_nodes);
        }
        let ids = &out.edge_ids;
        let lazy = |m: usize| {
            EdgeIds::lazy(
                m,
                Rebuild::Transpose {
                    out_offsets: out.offsets.clone(),
                    out_targets: out.targets.clone(),
                    out_ids: ids.clone(),
                    n_nodes,
                },
            )
        };
        let (offsets, targets, edge_ids) = if ids.is_built() {
            let (o, t, e) = transpose_parts(
                &out.offsets,
                &out.targets,
                TransposeIds::Rows(ids.get()),
                n_nodes,
            );
            (o, t, EdgeIds::from(e.unwrap_or_default()))
        } else if ids.is_identity() {
            let (o, t, _) =
                transpose_parts(&out.offsets, &out.targets, TransposeIds::None, n_nodes);
            let m = t.len();
            (o, t, lazy(m))
        } else if let Some((source, node)) = ids.rows_source() {
            let (o, t) = crate::edge_ids::transpose_rows(
                &out.offsets,
                &out.targets,
                source,
                node,
                crate::edge_ids::InSlot::Source,
            );
            let m = t.len();
            (o, t, lazy(m))
        } else {
            // Ids of another kind (a reversed topology's): built for the pass.
            let (o, t, _) = ids.with(|ids| {
                transpose_parts(&out.offsets, &out.targets, TransposeIds::Rows(ids), n_nodes)
            });
            let m = t.len();
            (o, t, lazy(m))
        };
        Adjacency {
            offsets: offsets.into(),
            targets: targets.into(),
            edge_ids,
            packed: None,
        }
    }

    /// Neighbours of dense node `u`, in slot order: a plain adjacency's
    /// slice of its targets.
    ///
    /// # Panics
    ///
    /// On a compressed adjacency ([`Self::is_packed`]), which has no slice
    /// to lend: read it through [`Self::for_each_neighbor`],
    /// [`Self::neighbors_in`] or [`Self::cursor`], which serve both layouts.
    #[inline]
    pub fn neighbors(&self, u: u32) -> &[u32] {
        assert!(
            self.packed.is_none(),
            "a compressed adjacency has no neighbour slices: use neighbors_in"
        );
        let s = self.offsets[u as usize] as usize;
        let e = self.offsets[u as usize + 1] as usize;
        &self.targets[s..e]
    }

    /// Whether this adjacency is compressed (#173, see [`crate::packed`]).
    #[inline]
    pub fn is_packed(&self) -> bool {
        self.packed.is_some()
    }

    /// The number of slots (edges).
    #[inline]
    pub fn n_slots(&self) -> usize {
        self.offsets.last().map_or(0, |&m| m as usize)
    }

    /// Neighbours of dense node `u`, in slot order, as a slice: a plain
    /// adjacency's own, or a compressed one's decoded into `buf` (cleared
    /// first). The per-thread scratch decode of #173; on a plain adjacency
    /// it costs one branch.
    #[inline]
    pub fn neighbors_in<'a>(&'a self, u: u32, buf: &'a mut Vec<u32>) -> &'a [u32] {
        match &self.packed {
            None => {
                let s = self.offsets[u as usize] as usize;
                let e = self.offsets[u as usize + 1] as usize;
                &self.targets[s..e]
            }
            Some(p) => {
                buf.clear();
                crate::packed::decode_into(p.segment(u), buf);
                buf
            }
        }
    }

    /// Call `f` with each neighbour of dense node `u`, in slot order,
    /// decoding a compressed segment as it goes.
    #[inline]
    pub fn for_each_neighbor(&self, u: u32, mut f: impl FnMut(u32)) {
        match &self.packed {
            None => {
                let s = self.offsets[u as usize] as usize;
                let e = self.offsets[u as usize + 1] as usize;
                for &v in &self.targets[s..e] {
                    f(v);
                }
            }
            Some(p) => crate::packed::for_each(p.segment(u), f),
        }
    }

    /// Call `f(v, e)` with each neighbour `v` of dense node `u` and the edge
    /// row `e` of its slot, in slot order. Builds the edge ids on first call
    /// (see [`Self::edge_ids`]).
    #[inline]
    pub fn for_each_neighbor_row(&self, u: u32, mut f: impl FnMut(u32, u32)) {
        let ids = self.edge_ids(u);
        match &self.packed {
            None => {
                let s = self.offsets[u as usize] as usize;
                let e = self.offsets[u as usize + 1] as usize;
                for (&v, &e) in self.targets[s..e].iter().zip(ids) {
                    f(v, e);
                }
            }
            Some(p) => {
                let mut k = 0;
                crate::packed::for_each(p.segment(u), |v| {
                    f(v, ids[k]);
                    k += 1;
                });
            }
        }
    }

    /// Whether `f` holds for some neighbour of dense node `u`, read in slot
    /// order up to the first that does.
    #[inline]
    pub fn any_neighbor(&self, u: u32, mut f: impl FnMut(u32) -> bool) -> bool {
        match &self.packed {
            None => {
                let s = self.offsets[u as usize] as usize;
                let e = self.offsets[u as usize + 1] as usize;
                self.targets[s..e].iter().any(|&v| f(v))
            }
            Some(p) => crate::packed::any(p.segment(u), f),
        }
    }

    /// A cursor at the first neighbour of dense node `u` (see
    /// [`Self::advance`]).
    #[inline]
    pub fn cursor(&self, u: u32) -> NeighborCursor {
        NeighborCursor {
            slot: self.offsets[u as usize] as usize,
            end: self.offsets[u as usize + 1] as usize,
            byte: self
                .packed
                .as_ref()
                .map_or(0, |p| p.byte_offsets[u as usize] as usize),
            prev: 0,
        }
    }

    /// The neighbour at `cursor` and its slot, moving the cursor past it;
    /// `None` once the node's neighbours are all read.
    #[inline]
    pub fn advance(&self, cursor: &mut NeighborCursor) -> Option<(u32, usize)> {
        if cursor.slot >= cursor.end {
            return None;
        }
        let k = cursor.slot;
        cursor.slot += 1;
        match &self.packed {
            None => Some((self.targets[k], k)),
            Some(p) => {
                let (v, at) = crate::packed::next(&p.bytes, cursor.byte, cursor.prev);
                cursor.byte = at;
                cursor.prev = v;
                Some((v, k))
            }
        }
    }

    /// Call `f` with each node's neighbours as a slice, in node order: a
    /// plain adjacency's own, a compressed one's decoded a node at a time.
    pub fn for_each_segment(&self, mut f: impl FnMut(u32, &[u32])) {
        let n = self.offsets.len().saturating_sub(1);
        let mut buf = Vec::new();
        for u in 0..n as u32 {
            f(u, self.neighbors_in(u, &mut buf));
        }
    }

    /// The bytes this adjacency holds in memory: its offsets, its targets
    /// or compressed segments, and its edge ids if they are held.
    pub fn heap_bytes(&self) -> usize {
        8 * self.offsets.len()
            + 4 * self.targets.len()
            + self.packed.as_ref().map_or(0, Packed::heap_bytes)
            + if self.edge_ids.is_built() {
                4 * self.edge_ids.len()
            } else {
                0
            }
    }

    /// Original edge rows of dense node `u`'s incident edges — same order as
    /// [`Adjacency::neighbors`], so a weight column can be gathered per neighbour.
    #[inline]
    ///
    /// Builds the adjacency's edge ids on first call (see [`EdgeIds`]): a
    /// kernel that has no mask, weight or row to read uses
    /// [`Adjacency::neighbors`] alone and never builds them.
    pub fn edge_ids(&self, u: u32) -> &[u32] {
        let s = self.offsets[u as usize] as usize;
        let e = self.offsets[u as usize + 1] as usize;
        &self.edge_ids.get()[s..e]
    }

    /// Out-degree (or in-degree, depending which direction this is) of node `u`.
    #[inline]
    pub fn degree(&self, u: u32) -> u32 {
        (self.offsets[u as usize + 1] - self.offsets[u as usize]) as u32
    }
}

/// The transpose of the adjacency `out_offsets`/`out_targets` whose edge ids
/// are `out_ids`: its offsets, targets and edge ids, each node's segment in
/// original row order, exactly as [`Adjacency::build`] would produce it from
/// the per-row endpoints, but without materializing them.
///
/// A counting sort moves each out-slot into its target's segment, carrying
/// the slot's owner (the source) and original row (`edge_ids`). Slots arrive
/// in source order, so each segment is then sorted by row; for an edge list
/// already sorted by source it is already in order, and the sort only checks.
/// Peak memory is the two CSRs (the former path also held two per-row endpoint
/// arrays, 8 B/edge, at the same time).
pub(crate) fn transpose_parts(
    out_offsets: &[u64],
    out_targets: &[u32],
    ids: TransposeIds<'_>,
    n_nodes: usize,
) -> (Vec<u64>, Vec<u32>, Option<Vec<u32>>) {
    let with_ids = !matches!(ids, TransposeIds::None);
    use crate::parallel::*;
    let m = out_targets.len();
    if m == 0 || n_nodes == 0 {
        return (vec![0; n_nodes + 1], Vec::new(), with_ids.then(Vec::new));
    }
    // Same chunking rule as `build` (serial on a rayon worker; see
    // `parallel::in_worker`).
    const PARALLEL_MIN_EDGES: usize = 1 << 16;
    let threads = current_num_threads();
    let serial = m < PARALLEL_MIN_EDGES || threads <= 1 || in_worker();
    let n_chunks = if serial {
        1
    } else {
        threads.min((m / n_nodes).max(4))
    };
    let chunk_size = m.div_ceil(n_chunks);

    // Pass 1: a target histogram per contiguous chunk of out-slots.
    let hist = |keys: &[u32]| {
        let mut h = vec![0u32; n_nodes];
        for &k in keys {
            h[k as usize] += 1;
        }
        h
    };
    let mut hists: Vec<Vec<u32>> = if serial {
        vec![hist(out_targets)]
    } else {
        out_targets.par_chunks(chunk_size).map(hist).collect()
    };
    let mut offsets = vec![0u64; n_nodes + 1];
    for h in &hists {
        for (k, &c) in h.iter().enumerate() {
            offsets[k + 1] += c as u64;
        }
    }
    for i in 0..n_nodes {
        offsets[i + 1] += offsets[i];
    }
    // Each chunk's histogram becomes its base cursor into every segment.
    let mut running: Vec<u32> = (0..n_nodes).map(|k| offsets[k] as u32).collect();
    for h in hists.iter_mut() {
        for (k, slot) in h.iter_mut().enumerate() {
            let cnt = *slot;
            *slot = running[k];
            running[k] += cnt;
        }
    }
    drop(running);

    // Pass 2: scatter (source, row) into each target's segment. Positions are
    // disjoint across chunks (the base cursors partition each segment), so the
    // writes never overlap; pointers cross threads as plain addresses, as in
    // `build_parallel`.
    let mut targets = vec![0u32; m];
    let mut edge_ids = vec![0u32; if with_ids { m } else { 0 }];
    let (t_addr, e_addr) = (
        targets.as_mut_ptr() as usize,
        edge_ids.as_mut_ptr() as usize,
    );
    let scatter = |(c, mut base): (usize, Vec<u32>)| {
        let (start, end) = (c * chunk_size, ((c + 1) * chunk_size).min(m));
        // The source owning slot `start`: the last node whose segment starts
        // at or before it.
        let mut owner = out_offsets.partition_point(|&o| o as usize <= start) - 1;
        for k in start..end {
            while out_offsets[owner + 1] as usize <= k {
                owner += 1;
            }
            let key = out_targets[k] as usize;
            let pos = base[key] as usize;
            base[key] += 1;
            // SAFETY: `pos < m` and is claimed by exactly one (chunk, slot).
            unsafe {
                *(t_addr as *mut u32).add(pos) = owner as u32;
                match ids {
                    TransposeIds::Rows(out_ids) => *(e_addr as *mut u32).add(pos) = out_ids[k],
                    TransposeIds::Identity => *(e_addr as *mut u32).add(pos) = k as u32,
                    TransposeIds::None => {}
                }
            }
        }
    };
    if serial {
        hists.into_iter().enumerate().for_each(scatter);
    } else {
        hists.into_par_iter().enumerate().for_each(scatter);
    }

    // Pass 3: restore original row order inside each segment.
    // Slots arrive in slot order, which is row order for the identity, and
    // without ids the order of the slots is what is asked for.
    if !matches!(ids, TransposeIds::Rows(_)) {
        return (offsets, targets, with_ids.then_some(edge_ids));
    }
    let sort_segment = |scratch: &mut Vec<(u32, u32)>, v: usize| {
        let (s, e) = (offsets[v] as usize, offsets[v + 1] as usize);
        // SAFETY: segments are disjoint across nodes and inside both buffers.
        let (ts, es) = unsafe {
            (
                std::slice::from_raw_parts_mut((t_addr as *mut u32).add(s), e - s),
                std::slice::from_raw_parts_mut((e_addr as *mut u32).add(s), e - s),
            )
        };
        if es.windows(2).all(|w| w[0] < w[1]) {
            return;
        }
        scratch.clear();
        scratch.extend(es.iter().copied().zip(ts.iter().copied()));
        scratch.sort_unstable_by_key(|&(row, _)| row); // rows are unique
        for (k, &(row, src)) in scratch.iter().enumerate() {
            es[k] = row;
            ts[k] = src;
        }
    };
    if serial {
        let mut scratch = Vec::new();
        (0..n_nodes).for_each(|v| sort_segment(&mut scratch, v));
    } else {
        (0..n_nodes)
            .into_par_iter()
            .for_each_init(Vec::new, sort_segment);
    }

    (offsets, targets, Some(edge_ids))
}

/// The rows' targets (original ids, `node[o]` the node of original id `o`
/// when relabelled), read back to put a compressed out-adjacency's edge ids
/// in its sorted order when they are first read (#173): see
/// [`Topology::compressed`].
#[derive(Clone)]
pub(crate) struct SortKeys {
    pub(crate) targets: Arc<dyn RowSource>,
    pub(crate) node: Option<Arc<[u32]>>,
}

/// Run `f(scratch, u)` for every node `0..n`, in parallel over nodes when
/// `m` (the slots) is large enough and the caller is not a rayon worker.
fn each_node<S: Send>(
    n: usize,
    m: usize,
    init: impl Fn() -> S + Sync + Send,
    f: impl Fn(&mut S, usize) + Sync + Send,
) {
    use crate::parallel::*;
    const PARALLEL_MIN_EDGES: usize = 1 << 16;
    if m < PARALLEL_MIN_EDGES || current_num_threads() <= 1 || in_worker() {
        let mut scratch = init();
        (0..n).for_each(|u| f(&mut scratch, u));
    } else {
        (0..n).into_par_iter().for_each_init(init, |s, u| f(s, u));
    }
}

/// Whether every segment of `targets` is in ascending order.
fn segments_sorted(offsets: &[u64], targets: &[u32]) -> bool {
    use crate::parallel::*;
    let n = offsets.len().saturating_sub(1);
    let sorted = |u: usize| {
        targets[offsets[u] as usize..offsets[u + 1] as usize]
            .windows(2)
            .all(|w| w[0] <= w[1])
    };
    if targets.len() < (1 << 16) || current_num_threads() <= 1 || in_worker() {
        (0..n).all(sorted)
    } else {
        (0..n).into_par_iter().all(sorted)
    }
}

/// Sort each segment of `targets` ascending, and when given, `ids` with it,
/// ties (parallel edges) in id order: each segment by `(target, id)`.
fn sort_segments(offsets: &[u64], targets: &mut [u32], ids: Option<&mut [u32]>) {
    let n = offsets.len().saturating_sub(1);
    let m = targets.len();
    let t_addr = targets.as_mut_ptr() as usize;
    let e_addr = ids.map(|ids| ids.as_mut_ptr() as usize);
    each_node(n, m, Vec::<(u32, u32)>::new, |scratch, u| {
        let (s, e) = (offsets[u] as usize, offsets[u + 1] as usize);
        // SAFETY: segments are disjoint across nodes and inside both
        // buffers, which outlive the pass.
        let ts = unsafe { std::slice::from_raw_parts_mut((t_addr as *mut u32).add(s), e - s) };
        match e_addr {
            None => ts.sort_unstable(),
            Some(e_addr) => {
                // SAFETY: as above.
                let es =
                    unsafe { std::slice::from_raw_parts_mut((e_addr as *mut u32).add(s), e - s) };
                if ts
                    .iter()
                    .zip(es.iter())
                    .zip(ts.iter().zip(es.iter()).skip(1))
                    .all(|(a, b)| a < b)
                {
                    return;
                }
                scratch.clear();
                scratch.extend(ts.iter().copied().zip(es.iter().copied()));
                scratch.sort_unstable();
                for (k, &(t, i)) in scratch.iter().enumerate() {
                    ts[k] = t;
                    es[k] = i;
                }
            }
        }
    });
}

/// The compressed segments of the sorted segments `offsets`/`targets`.
fn encode_segments(offsets: &[u64], targets: &[u32]) -> Packed {
    use crate::packed::{encode_segment, segment_len};
    use crate::parallel::*;
    let n = offsets.len().saturating_sub(1);
    let seg = |u: usize| &targets[offsets[u] as usize..offsets[u + 1] as usize];
    let parallel = targets.len() >= (1 << 16) && current_num_threads() > 1 && !in_worker();
    let lens: Vec<u64> = if parallel {
        (0..n)
            .into_par_iter()
            .map(|u| segment_len(seg(u)) as u64)
            .collect()
    } else {
        (0..n).map(|u| segment_len(seg(u)) as u64).collect()
    };
    let mut byte_offsets = vec![0u64; n + 1];
    for u in 0..n {
        byte_offsets[u + 1] = byte_offsets[u] + lens[u];
    }
    drop(lens);
    let mut bytes = vec![0u8; byte_offsets[n] as usize];
    let addr = bytes.as_mut_ptr() as usize;
    let at = &byte_offsets;
    each_node(
        n,
        targets.len(),
        || (),
        |_, u| {
            let (s, e) = (at[u] as usize, at[u + 1] as usize);
            // SAFETY: each node's bytes are its own, inside `bytes`.
            let out = unsafe { std::slice::from_raw_parts_mut((addr as *mut u8).add(s), e - s) };
            encode_segment(seg(u), out);
        },
    );
    Packed {
        byte_offsets: byte_offsets.into(),
        bytes: bytes.into(),
    }
}

/// The values of `buffer` as a vector of their own: taken when it is the
/// only reference, else copied.
fn into_vec(buffer: ScalarBuffer<u32>) -> Vec<u32> {
    match buffer.into_inner().into_vec::<u32>() {
        Ok(v) => v,
        Err(buffer) => buffer.typed_data::<u32>().to_vec(),
    }
}

impl Adjacency {
    /// This adjacency with every segment sorted, ascending, and its edge ids
    /// in the same order (`(target, row)` within a segment): the parts of the
    /// compressed layout before it is encoded (#173).
    ///
    /// Ids held are sorted with the targets. Ids not held stay unbuilt when
    /// the segments were sorted already (a segment's rows are then in
    /// `(target, row)` order already), or are rebuilt when first read from
    /// the rows' targets `keys`; without `keys` they are built now.
    fn sorted_parts(self, keys: Option<&SortKeys>) -> (ScalarBuffer<u64>, Vec<u32>, EdgeIds) {
        assert!(self.packed.is_none(), "the adjacency is compressed already");
        let Adjacency {
            offsets,
            targets,
            edge_ids,
            ..
        } = self;
        let mut targets = into_vec(targets);
        let m = targets.len();
        if !edge_ids.is_built() && segments_sorted(&offsets, &targets) {
            return (offsets, targets, edge_ids);
        }
        let mut eager = match (edge_ids.is_built(), keys) {
            (false, Some(_)) => None,
            _ => Some(edge_ids.with(<[u32]>::to_vec)),
        };
        sort_segments(&offsets, &mut targets, eager.as_deref_mut());
        let edge_ids = match (eager, keys) {
            (Some(ids), _) => EdgeIds::from(ids),
            (None, Some(keys)) => EdgeIds::lazy(
                m,
                Rebuild::Sorted {
                    offsets: offsets.clone(),
                    rows: edge_ids,
                    keys: keys.targets.clone(),
                    node: keys.node.clone(),
                },
            ),
            (None, None) => unreachable!("ids are built now without keys"),
        };
        (offsets, targets, edge_ids)
    }

    /// This adjacency in the compressed layout (#173): see
    /// [`Self::sorted_parts`] for its edge ids.
    fn pack(self, keys: Option<&SortKeys>) -> Adjacency {
        if self.packed.is_some() {
            return self;
        }
        let (offsets, targets, edge_ids) = self.sorted_parts(keys);
        let packed = encode_segments(&offsets, &targets);
        drop(targets);
        Adjacency {
            offsets,
            targets: ScalarBuffer::from(Vec::<u32>::new()),
            edge_ids,
            packed: Some(packed),
        }
    }

    /// This adjacency plain, with every segment sorted as the compressed
    /// layout sorts it: what a compressed adjacency decodes to.
    fn sorted(self, keys: Option<&SortKeys>) -> Adjacency {
        if self.packed.is_some() {
            return self.unpacked();
        }
        let (offsets, targets, edge_ids) = self.sorted_parts(keys);
        Adjacency {
            offsets,
            targets: targets.into(),
            edge_ids,
            packed: None,
        }
    }

    /// This adjacency in the plain layout, its segments in the same order
    /// (a compressed one's decoded, so sorted), sharing its edge ids.
    pub fn unpacked(&self) -> Adjacency {
        let Some(p) = &self.packed else {
            return self.clone();
        };
        let n = self.offsets.len().saturating_sub(1);
        let m = self.n_slots();
        let mut targets = vec![0u32; m];
        let addr = targets.as_mut_ptr() as usize;
        each_node(
            n,
            m,
            || (),
            |_, u| {
                let (s, e) = (self.offsets[u] as usize, self.offsets[u + 1] as usize);
                // SAFETY: node `u`'s slots are its own, inside `targets`.
                let out =
                    unsafe { std::slice::from_raw_parts_mut((addr as *mut u32).add(s), e - s) };
                let mut k = 0;
                crate::packed::for_each(p.segment(u as u32), |v| {
                    out[k] = v;
                    k += 1;
                });
            },
        );
        Adjacency {
            offsets: self.offsets.clone(),
            targets: targets.into(),
            edge_ids: self.edge_ids.clone(),
            packed: None,
        }
    }

    /// [`Self::transpose`] of a compressed adjacency: compressed too. Its
    /// edge ids are built with it when `out`'s are held, else when first
    /// read, from `out`'s.
    fn transpose_packed(out: &Adjacency, n_nodes: usize) -> Adjacency {
        let held = out.edge_ids.is_built().then(|| out.edge_ids.get());
        let (offsets, targets, rows) =
            transpose_packed_parts(out, n_nodes, true, held.map(|ids| &ids[..]));
        let m = targets.len();
        let packed = encode_segments(&offsets, &targets);
        drop(targets);
        let edge_ids = match held {
            Some(_) => EdgeIds::from(rows),
            None => EdgeIds::lazy(
                m,
                Rebuild::PackedTranspose {
                    out: Box::new(out.clone()),
                    n_nodes,
                },
            ),
        };
        Adjacency {
            offsets: offsets.into(),
            targets: ScalarBuffer::from(Vec::<u32>::new()),
            edge_ids,
            packed: Some(packed),
        }
    }
}

/// The transpose of the compressed out-adjacency `out`: each in-segment
/// lists its sources in ascending order (the out-segments are read in node
/// order), so it is sorted already, as its own compressed segments want it.
/// Returns the offsets; with `sources`, each in-slot's source; with `ids`
/// (the out-adjacency's, in its sorted order), each in-slot's row, which
/// follows the same order, `(source, row)` within a segment.
pub(crate) fn transpose_packed_parts(
    out: &Adjacency,
    n_nodes: usize,
    sources: bool,
    ids: Option<&[u32]>,
) -> (Vec<u64>, Vec<u32>, Vec<u32>) {
    use crate::parallel::*;
    let p = out.packed.as_ref().expect("a compressed adjacency");
    let m = out.n_slots();
    let n = n_nodes;
    const PARALLEL_MIN_EDGES: usize = 1 << 16;
    let threads = current_num_threads();
    let serial = m < PARALLEL_MIN_EDGES || threads <= 1 || in_worker() || n == 0;
    // Contiguous ranges of sources of about equal slots, one a chunk. Each
    // chunk's histogram is `n` long, so chunks are capped as in the build.
    let n_chunks = if serial {
        1
    } else {
        threads.min((m / n).max(4))
    };
    let mut bounds: Vec<usize> = (0..=n_chunks)
        .map(|c| {
            let target = (m as u128 * c as u128 / n_chunks as u128) as u64;
            out.offsets.partition_point(|&o| o < target).min(n)
        })
        .collect();
    bounds[0] = 0;
    bounds[n_chunks] = n;
    let ranges: Vec<(usize, usize)> = bounds.windows(2).map(|w| (w[0], w[1])).collect();
    let hist = |&(lo, hi): &(usize, usize)| {
        let mut h = vec![0u32; n];
        for u in lo..hi {
            crate::packed::for_each(p.segment(u as u32), |v| h[v as usize] += 1);
        }
        h
    };
    let mut hists: Vec<Vec<u32>> = if serial {
        ranges.iter().map(hist).collect()
    } else {
        ranges.par_iter().map(hist).collect()
    };
    let mut offsets = vec![0u64; n + 1];
    for h in &hists {
        for (v, &c) in h.iter().enumerate() {
            offsets[v + 1] += u64::from(c);
        }
    }
    for v in 0..n {
        offsets[v + 1] += offsets[v];
    }
    // Every write below is to a slot below the total: it must be `m`.
    assert_eq!(
        offsets[n], m as u64,
        "the compressed segments do not hold their degrees' values"
    );
    let mut running: Vec<u32> = offsets[..n].iter().map(|&o| o as u32).collect();
    for h in hists.iter_mut() {
        for (slot, run) in h.iter_mut().zip(running.iter_mut()) {
            let cnt = *slot;
            *slot = *run;
            *run += cnt;
        }
    }
    drop(running);
    let mut targets = vec![0u32; if sources { m } else { 0 }];
    let mut rows = vec![0u32; if ids.is_some() { m } else { 0 }];
    let (t_addr, r_addr) = (targets.as_mut_ptr() as usize, rows.as_mut_ptr() as usize);
    let scatter = |(c, mut base): (usize, Vec<u32>)| {
        let (lo, hi) = ranges[c];
        for u in lo..hi {
            let mut k = out.offsets[u] as usize;
            crate::packed::for_each(p.segment(u as u32), |v| {
                let pos = base[v as usize] as usize;
                base[v as usize] += 1;
                // SAFETY: `pos < m`, claimed by exactly one (chunk, slot).
                unsafe {
                    if sources {
                        *(t_addr as *mut u32).add(pos) = u as u32;
                    }
                    if let Some(ids) = ids {
                        *(r_addr as *mut u32).add(pos) = ids[k];
                    }
                }
                k += 1;
            });
        }
    };
    if serial {
        hists.into_iter().enumerate().for_each(scatter);
    } else {
        hists.into_par_iter().enumerate().for_each(scatter);
    }
    (offsets, targets, rows)
}

/// The edge ids [`transpose_parts`] reads and writes.
#[derive(Clone, Copy)]
pub(crate) enum TransposeIds<'a> {
    /// The out-adjacency's ids: each in-segment is put in row order, and
    /// its ids written.
    Rows(&'a [u32]),
    /// The out-adjacency's ids are the identity: slot order is row order,
    /// and the ids are written.
    Identity,
    /// None: each in-segment in out-slot (source) order, no ids. That is
    /// row order only when the out-ids are the identity; otherwise it is
    /// for a view where only the set of neighbours matters (undirected).
    None,
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

/// [`UndirectedCsr::from_edges`] sorts a node's edge ends when they are
/// fewer than this (or than an eighth of the words of the bitmap's
/// summary, which reading a run off the bitmap scans), and reads a longer
/// run off the bitmap.
const UNSORTED_FROM: usize = 48;

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

    /// The undirected adjacency of the edges `(src[i], dst[i])` between
    /// `n_nodes` nodes, built from the edge list alone: the same offsets and
    /// targets as [`Topology::build`] followed by [`Topology::undirected`],
    /// without the out-adjacency, the transpose and the edge ids that a
    /// [`Topology`] holds beside it. For a caller that needs only this (a
    /// layout): its peak is the edge list and one buffer of two slots an
    /// edge.
    ///
    /// Serial, and the result does not depend on the order of the edges.
    ///
    /// # Panics
    ///
    /// If `src` and `dst` differ in length, or an endpoint is `>= n_nodes`.
    pub fn from_edges(n_nodes: usize, src: &[u32], dst: &[u32]) -> UndirectedCsr {
        assert_eq!(src.len(), dst.len(), "src/dst length mismatch");
        // Each node's slots before deduplication: one per edge end, self-loops
        // left out.
        let mut bound = vec![0u64; n_nodes + 1];
        for (&s, &d) in src.iter().zip(dst) {
            if s != d {
                bound[s as usize + 1] += 1;
                bound[d as usize + 1] += 1;
            }
        }
        let mut total = 0u64;
        for slot in &mut bound[1..] {
            let len = *slot;
            *slot = total;
            total += len;
        }
        // Filling moves each node's start to its end, the next node's start.
        let mut targets = vec![0u32; total as usize];
        for (&s, &d) in src.iter().zip(dst) {
            if s != d {
                for (u, v) in [(s, d), (d, s)] {
                    let next = &mut bound[u as usize + 1];
                    targets[*next as usize] = v;
                    *next += 1;
                }
            }
        }
        // Sort and deduplicate each segment, sliding it left over the slots
        // the earlier segments gave up.
        // A long segment is not sorted: its targets are marked in a bitmap
        // of the nodes (`met`, with a bit in `any` for each of its words
        // that has one) and read back off it, ascending and once each, for
        // what the marking costs and the words it touched. The result is
        // the sort's.
        let words = n_nodes.div_ceil(64);
        let mut met = vec![0u64; words];
        let mut any = vec![0u64; words.div_ceil(64)];
        let mut write = 0usize;
        let mut start = 0usize;
        for u in 0..n_nodes {
            let end = bound[u + 1] as usize;
            bound[u] = write as u64;
            if end - start >= UNSORTED_FROM.max(any.len() / 8) {
                for &v in &targets[start..end] {
                    let word = v as usize / 64;
                    met[word] |= 1 << (v % 64);
                    any[word / 64] |= 1 << (word % 64);
                }
                // The marks are all made before any is written back, and
                // there are no more of them than the segment had.
                for (top, some) in any.iter_mut().enumerate() {
                    let mut some = std::mem::take(some);
                    while some != 0 {
                        let word = top * 64 + some.trailing_zeros() as usize;
                        some &= some - 1;
                        let mut bits = std::mem::take(&mut met[word]);
                        while bits != 0 {
                            targets[write] = (word * 64) as u32 + bits.trailing_zeros();
                            write += 1;
                            bits &= bits - 1;
                        }
                    }
                }
            } else {
                targets[start..end].sort_unstable();
                let mut last = None;
                for i in start..end {
                    let v = targets[i];
                    if last != Some(v) {
                        targets[write] = v;
                        write += 1;
                        last = Some(v);
                    }
                }
            }
            start = end;
        }
        bound[n_nodes] = write as u64;
        targets.truncate(write);
        targets.shrink_to_fit();
        UndirectedCsr {
            offsets: bound,
            targets,
        }
    }

    /// An undirected adjacency from its parts, as another holder of the
    /// graph hands them over (the fields of an [`UndirectedCsr`] it built):
    /// nothing is sorted and nothing is copied.
    ///
    /// The parts are checked in one pass, so a malformed adjacency is an
    /// error and never a wrong answer later: `n_nodes + 1` offsets rising
    /// from 0 to the number of targets; each node's targets strictly
    /// ascending (sorted, no duplicates), below `n_nodes`, and never the
    /// node itself; and every pair in both nodes' lists.
    pub fn from_parts(
        n_nodes: usize,
        offsets: Vec<u64>,
        targets: Vec<u32>,
    ) -> Result<UndirectedCsr, String> {
        if n_nodes > u32::MAX as usize {
            return Err(format!("{n_nodes} nodes is beyond the u32 dense-index cap"));
        }
        if offsets.len() != n_nodes + 1 {
            return Err(format!(
                "the adjacency has {} offsets; {n_nodes} nodes need {}",
                offsets.len(),
                n_nodes + 1
            ));
        }
        if offsets[0] != 0 || offsets[n_nodes] != targets.len() as u64 {
            return Err(format!(
                "the adjacency's offsets run from {} to {}, not from 0 to its {} targets",
                offsets[0],
                offsets[n_nodes],
                targets.len()
            ));
        }
        if let Some(u) = (0..n_nodes).find(|&u| offsets[u] > offsets[u + 1]) {
            return Err(format!("the adjacency's offsets fall at node {u}"));
        }
        // Nodes are read in ascending order, so node `v` meets its
        // neighbours in ascending order too: `seen[v]` of them so far, and
        // the next must be the one reading it.
        let mut seen = vec![0u32; n_nodes];
        for u in 0..n_nodes {
            let list = &targets[offsets[u] as usize..offsets[u + 1] as usize];
            let mut last = None;
            for &v in list {
                if v as usize >= n_nodes || v as usize == u || last.is_some_and(|l| l >= v) {
                    return Err(format!(
                        "node {u}'s neighbours are not distinct other nodes in ascending order"
                    ));
                }
                last = Some(v);
                let at = offsets[v as usize] + u64::from(seen[v as usize]);
                if at >= offsets[v as usize + 1] || targets[at as usize] != u as u32 {
                    return Err(format!(
                        "node {u} has the neighbour {v}, but node {v} does not have {u}"
                    ));
                }
                seen[v as usize] += 1;
            }
        }
        Ok(UndirectedCsr { offsets, targets })
    }
}

/// Reads a stored in-adjacency on first demand: see
/// [`Topology::with_incoming_loader`]. `None` means it could not be read, and
/// the transpose is built instead.
pub type IncomingLoader = Box<dyn FnOnce() -> Option<Adjacency> + Send>;

/// The pending [`IncomingLoader`] of a topology, taken on first use, and
/// whether what it reads is checksummed (so only its shape is checked).
#[derive(Default)]
struct LazyIncoming(Mutex<Option<(IncomingLoader, bool)>>);

impl fmt::Debug for LazyIncoming {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pending = self
            .0
            .lock()
            .map(|l| l.is_some())
            .unwrap_or_else(|e| e.into_inner().is_some());
        write!(f, "LazyIncoming(pending: {pending})")
    }
}

impl LazyIncoming {
    fn take(&self) -> Option<(IncomingLoader, bool)> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
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
    /// In-adjacency (transpose / CSC), built on first demand from the out-CSR,
    /// or read by `inc_loader` if there is one.
    inc: OnceLock<Adjacency>,
    /// Reads a stored in-adjacency on first demand (see
    /// [`Topology::with_incoming_loader`]).
    inc_loader: LazyIncoming,
    /// Undirected sorted adjacency, built on first demand (triangle/clustering).
    undirected: OnceLock<UndirectedCsr>,
    /// The unmasked symmetric view (see [`Topology::symmetric`]), built on first
    /// demand from `undirected`.
    symmetric: OnceLock<Box<Topology>>,
    /// Set on a symmetric view: its transpose is its own out-CSR, so `incoming`
    /// returns `out` rather than building a copy.
    self_transpose: bool,
    /// The node ids this topology's nodes had before a locality relabel
    /// ([`Topology::build_ordered`], #172); `None` when they were not
    /// relabelled. Shared by every view derived from the topology.
    relabel: Option<Arc<Relabel>>,
}

/// How the nodes of a topology are numbered (#172).
///
/// The ids given to [`Topology::build_ordered`] are the *original* ids (an
/// [`IdMap`]'s first-seen order). A locality order renumbers the nodes so the
/// ones the kernels read most often sit together in memory, and keeps the
/// original id of each node, so that what must not depend on the numbering
/// (component labels, the visit orders of the seeded community kernels,
/// sampled sources) is defined on the original ids instead.
///
/// [`IdMap`]: crate::id_map::IdMap
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeOrder {
    /// The ids as given: no relabel.
    AsGiven,
    /// By in-degree, highest first, ties in original id order: the nodes that
    /// are most often a neighbour (a pull PageRank's reads, a traversal's
    /// targets) come first and share cache lines. From 4,194,304 edges the
    /// in-degrees are counted on an eighth of the rows (fixed blocks), which
    /// keeps the relabel's cost within a quarter of the build's.
    InDegree,
}

/// A topology's original node ids (see [`NodeOrder`]).
#[derive(Debug)]
struct Relabel {
    /// `original[v]`: the original id of node `v`.
    original: ScalarBuffer<u32>,
    /// `node[o]`: the node whose original id is `o`, built on first use.
    node: OnceLock<Arc<[u32]>>,
}

impl Relabel {
    /// The inverse permutation, shared.
    fn node_map(&self) -> Arc<[u32]> {
        self.nodes();
        Arc::clone(self.node.get().expect("built just now"))
    }

    /// The inverse permutation.
    fn nodes(&self) -> &[u32] {
        self.node.get_or_init(|| {
            let mut node = vec![0u32; self.original.len()];
            for (v, &o) in self.original.iter().enumerate() {
                node[o as usize] = v as u32;
            }
            node.into()
        })
    }
}

/// `adj` renumbered by its original ids (see [`Topology::in_original_order`]):
/// original node `o`'s segment is the segment of node `relabel.nodes()[o]`,
/// its targets renamed to their original ids, its edge ids as they are.
fn in_original(adj: &Adjacency, relabel: &Relabel) -> Adjacency {
    use crate::parallel::*;
    if adj.is_packed() {
        // Renamed, a segment is no longer sorted: plain.
        return in_original(&adj.unpacked(), relabel);
    }
    let node = relabel.nodes();
    let n = node.len();
    let mut offsets = vec![0u64; n + 1];
    for (o, &v) in node.iter().enumerate() {
        offsets[o + 1] = offsets[o] + u64::from(adj.degree(v));
    }
    let m = adj.targets.len();
    let (mut targets, mut edge_ids) = (vec![0u32; m], vec![0u32; m]);
    let (t_addr, e_addr) = (
        targets.as_mut_ptr() as usize,
        edge_ids.as_mut_ptr() as usize,
    );
    let offsets_ref = &offsets;
    adj.edge_ids.with(|adj_ids| {
        let copy = |o: usize| {
            let v = node[o];
            let (lo, hi) = (
                adj.offsets[v as usize] as usize,
                adj.offsets[v as usize + 1] as usize,
            );
            let at = offsets_ref[o] as usize;
            for (i, k) in (lo..hi).enumerate() {
                // SAFETY: segments `offsets[o]..offsets[o + 1]` are disjoint
                // across nodes and inside both buffers, which outlive the pass.
                unsafe {
                    *(t_addr as *mut u32).add(at + i) = relabel.original[adj.targets[k] as usize];
                    *(e_addr as *mut u32).add(at + i) = adj_ids[k];
                }
            }
        };
        if in_worker() || m < (1 << 16) {
            (0..n).for_each(copy);
        } else {
            (0..n).into_par_iter().for_each(copy);
        }
    });
    Adjacency {
        offsets: offsets.into(),
        targets: targets.into(),
        edge_ids: edge_ids.into(),
        packed: None,
    }
}

/// Whether `ids` is a permutation of `0..ids.len()`.
fn is_permutation(ids: &[u32]) -> bool {
    let mut seen = vec![false; ids.len()];
    ids.iter().all(|&o| match seen.get_mut(o as usize) {
        Some(s) if !*s => {
            *s = true;
            true
        }
        _ => false,
    })
}

/// The out-adjacency of `src -> dst` (original ids) with its nodes numbered
/// by in-degree, highest first, ties by original id (#172): the adjacency,
/// the original id of each node, and the node of each original id. From
/// 4,194,304 edges, the in-degrees are counted on one block of 4,096 rows in
/// eight (an estimate: the order is a heuristic, and the hubs it is for are
/// counted closely).
///
/// The relabel is fused into the counting sort rather than run as passes of
/// its own. The first pass counts, per chunk of rows, both endpoints' degrees
/// over the original ids. The in-degrees give the order (a counting sort by
/// degree); the out-degrees, gathered into it, give the offsets,
/// and each chunk's counts become its cursors, still indexed by original id.
/// The targets are renamed in place, in one streaming pass, and the scatter
/// places each row by its source's original id, so the sources are never
/// renamed. Each
/// segment holds its rows in row order: the output is exactly
/// [`Adjacency::build`] over the renamed endpoints, on any thread count.
///
/// The edge ids are built as `ids` says (see [`IdsPlan`]); `node` is checked
/// for the identity first.
fn build_in_degree(
    n: usize,
    src: &[u32],
    dst: &mut [u32],
    ids: &mut KeepRows<'_>,
) -> (Adjacency, Vec<u32>, Arc<[u32]>) {
    // See pass 1.
    const SAMPLE_MIN_EDGES: usize = 1 << 22;
    const SAMPLE_BLOCK: usize = 1 << 12;
    const SAMPLE: usize = 8;
    use crate::parallel::*;
    let m = src.len();
    assert!(
        m <= u32::MAX as usize,
        "more than u32::MAX (~4.29B) edges is beyond the v0.1 edge-id cap"
    );
    // The chunking rule of `Adjacency::build`; one chunk when serial.
    const PARALLEL_MIN_EDGES: usize = 1 << 16;
    let threads = current_num_threads().min((m / n.max(1)).max(4));
    let serial = m < PARALLEL_MIN_EDGES || threads <= 1 || n == 0 || in_worker();
    let n_chunks = if serial { 1 } else { threads };
    let chunk = m.div_ceil(n_chunks).max(1);
    // Disjoint blocks of nodes for the per-node passes.
    const BLOCK: usize = 1 << 14;
    let blocks = n.div_ceil(BLOCK);
    let par_blocks = |f: &(dyn Fn(std::ops::Range<usize>) + Sync)| {
        let run = |b: usize| f(b * BLOCK..((b + 1) * BLOCK).min(n));
        if serial {
            (0..blocks).for_each(run);
        } else {
            (0..blocks).into_par_iter().for_each(run);
        }
    };

    // Pass 1: per chunk, out- and in-degree over original ids. The
    // in-degrees only order the nodes, so a large graph counts them on one
    // block of rows in `SAMPLE` (fixed blocks of the row order, so the same
    // on any thread count): the random increments are most of this pass.
    let sampled = m >= SAMPLE_MIN_EDGES;
    let count = |c: usize| {
        let (lo, hi) = (c * chunk, ((c + 1) * chunk).min(m));
        let (mut out, mut inc) = (vec![0u32; n], vec![0u32; n]);
        for &k in &src[lo..hi] {
            out[k as usize] += 1;
        }
        let mut r = lo;
        while r < hi {
            let block = r / SAMPLE_BLOCK;
            let end = ((block + 1) * SAMPLE_BLOCK).min(hi);
            if !sampled || block.is_multiple_of(SAMPLE) {
                for &d in &dst[r..end] {
                    inc[d as usize] += 1;
                }
            }
            r = end;
        }
        (out, inc)
    };
    let (mut outs, incs): (Vec<Vec<u32>>, Vec<Vec<u32>>) = if serial {
        (0..n_chunks).map(count).unzip()
    } else {
        (0..n_chunks).into_par_iter().map(count).unzip()
    };

    // The in-degree of each node.
    let mut degree = vec![0u32; n];
    let d_addr = degree.as_mut_ptr() as usize;
    par_blocks(&|range| {
        for k in range {
            // SAFETY: blocks are disjoint and inside `degree`.
            unsafe { *(d_addr as *mut u32).add(k) = incs.iter().map(|h| h[k]).sum() };
        }
    });
    drop(incs);
    // The order: a counting sort by degree, highest first, each degree's
    // nodes in original id order (a stable scatter in id order). The
    // buckets run to the highest degree, at most the edge count.
    let top = degree.iter().copied().max().unwrap_or(0) as usize;
    let mut start = vec![0u32; top + 2];
    for &d in &degree {
        start[top - d as usize + 1] += 1;
    }
    for b in 0..=top {
        start[b + 1] += start[b];
    }
    let mut original = vec![0u32; n];
    for (k, &d) in degree.iter().enumerate() {
        let slot = &mut start[top - d as usize];
        original[*slot as usize] = k as u32;
        *slot += 1;
    }
    drop((degree, start));

    // The node of each original id, and the offsets in the new order (each
    // node's out-degree, prefix-summed below), in one pass.
    let mut node = vec![0u32; n];
    let mut offsets = vec![0u64; n + 1];
    let (n_addr, o_addr) = (node.as_mut_ptr() as usize, offsets.as_mut_ptr() as usize);
    let original_ref = &original;
    par_blocks(&|range| {
        for v in range {
            let k = original_ref[v] as usize;
            let d: u64 = outs.iter().map(|h| u64::from(h[k])).sum();
            // SAFETY: `original` is a permutation, so each `node` slot is
            // written once; blocks are disjoint and `v + 1 <= n`.
            unsafe {
                *(n_addr as *mut u32).add(k) = v as u32;
                *(o_addr as *mut u64).add(v + 1) = d;
            }
        }
    });
    for v in 0..n {
        offsets[v + 1] += offsets[v];
    }
    // How the edge ids are had: none needed for rows already grouped by
    // (renamed) source, else read back later, else built here.
    let plan = IdsPlan::decide(src, Some(&node), ids);
    let with_ids = plan.builds();
    // Each chunk's counts become its cursors into every segment, in place,
    // still indexed by original id.
    let addrs: Vec<usize> = outs.iter_mut().map(|h| h.as_mut_ptr() as usize).collect();
    let (offsets_ref, node_ref) = (&offsets, &node);
    par_blocks(&|range| {
        for k in range {
            let mut at = offsets_ref[node_ref[k] as usize] as u32;
            for &a in &addrs {
                // SAFETY: blocks of `k` are disjoint, inside every histogram.
                unsafe {
                    let slot = (a as *mut u32).add(k);
                    let cnt = *slot;
                    *slot = at;
                    at += cnt;
                }
            }
        }
    });

    // The targets renamed in place: a streaming pass whose lookups overlap,
    // where the scatter's would wait on its writes.
    if serial {
        dst.iter_mut().for_each(|d| *d = node_ref[*d as usize]);
    } else {
        dst.par_chunks_mut(1 << 16)
            .for_each(|c| c.iter_mut().for_each(|d| *d = node_ref[*d as usize]));
    }
    let dst = &*dst;

    // Pass 2: scatter each row by its source's original id, its target
    // (renamed above). Positions are disjoint across chunks (the cursors partition
    // each segment), as in `build_parallel`.
    let (mut targets, mut edge_ids) = (vec![0u32; m], vec![0u32; if with_ids { m } else { 0 }]);
    let (t_addr, e_addr) = (
        targets.as_mut_ptr() as usize,
        edge_ids.as_mut_ptr() as usize,
    );
    let scatter = |(c, mut base): (usize, Vec<u32>)| {
        let (lo, hi) = (c * chunk, ((c + 1) * chunk).min(m));
        for r in lo..hi {
            let k = src[r] as usize;
            let pos = base[k] as usize;
            base[k] += 1;
            // SAFETY: `pos < m` and is claimed by exactly one (chunk, row).
            unsafe {
                *(t_addr as *mut u32).add(pos) = dst[r];
                if with_ids {
                    *(e_addr as *mut u32).add(pos) = r as u32;
                }
            }
        }
    };
    if serial {
        outs.into_iter().enumerate().for_each(scatter);
    } else {
        outs.into_par_iter().enumerate().for_each(scatter);
    }
    let node: Arc<[u32]> = node.into();
    let offsets: ScalarBuffer<u64> = offsets.into();
    let edge_ids = plan.edge_ids(m, &offsets, Some(&node), edge_ids);
    let adjacency = Adjacency {
        offsets,
        targets: targets.into(),
        edge_ids,
        packed: None,
    };
    (adjacency, original, node)
}

/// How a build has its out-adjacency's edge ids (#170).
enum IdsPlan {
    /// The rows are grouped by source: the identity, built only if read.
    Identity,
    /// Read the rows' sources back when the ids are first read.
    Later(Arc<dyn RowSource>),
    /// Build them with the adjacency.
    Now,
}

impl IdsPlan {
    /// The plan for the rows whose sources are `src` (`node[s]` for source
    /// `s` when relabelled): the identity if they are grouped by source, else
    /// what `rows` makes of them (a source to read them back from, or none,
    /// to build them now). `rows` is not called for the identity.
    fn decide(src: &[u32], node: Option<&[u32]>, rows: &mut KeepRows<'_>) -> IdsPlan {
        let grouped = match node {
            None => src.windows(2).all(|w| w[0] <= w[1]),
            Some(node) => src
                .windows(2)
                .all(|w| node[w[0] as usize] <= node[w[1] as usize]),
        };
        if grouped {
            return IdsPlan::Identity;
        }
        match rows(src) {
            Some(source) => IdsPlan::Later(source),
            None => IdsPlan::Now,
        }
    }

    /// Whether the build writes the ids.
    fn builds(&self) -> bool {
        matches!(self, IdsPlan::Now)
    }

    /// The edge ids of an `m`-slot out-adjacency with `offsets`, given what
    /// the build wrote (`built`, when it [`Self::builds`]).
    fn edge_ids(
        self,
        m: usize,
        offsets: &ScalarBuffer<u64>,
        node: Option<&Arc<[u32]>>,
        built: Vec<u32>,
    ) -> EdgeIds {
        match self {
            IdsPlan::Identity => EdgeIds::identity(m),
            IdsPlan::Later(source) => EdgeIds::from_rows(m, offsets.clone(), node.cloned(), source),
            IdsPlan::Now => EdgeIds::from(built),
        }
    }
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
        Self::build_with_rows(
            n_nodes,
            src_dense,
            dst_dense,
            NodeOrder::AsGiven,
            &mut |_| None,
        )
    }

    /// [`Self::build_ordered`], with the out-adjacency's edge ids built as
    /// `rows` says (#170): given the rows' sources (the dense ids given,
    /// before any relabel), it returns a [`RowSource`] that reads them back
    /// later (a [`SpilledRows`] file), and then the ids are not built until a
    /// kernel reads them; or `None`, and they are built with the adjacency,
    /// as [`Self::build_ordered`] builds them. Rows already grouped by source
    /// need neither: their ids are the identity, and `rows` is not called.
    ///
    /// Every result is the same either way: the ids built late are exactly
    /// the ones built eagerly.
    ///
    /// [`SpilledRows`]: crate::edge_ids::SpilledRows
    pub fn build_with_rows(
        n_nodes: usize,
        src_dense: Vec<u32>,
        dst_dense: Vec<u32>,
        order: NodeOrder,
        rows: &mut KeepRows<'_>,
    ) -> Topology {
        assert_eq!(src_dense.len(), dst_dense.len(), "src/dst length mismatch");
        if order == NodeOrder::InDegree {
            let mut dst_dense = dst_dense;
            let (out, original, node) = build_in_degree(n_nodes, &src_dense, &mut dst_dense, rows);
            drop((src_dense, dst_dense));
            let identity = original.iter().enumerate().all(|(v, &o)| v as u32 == o);
            return Topology {
                n_nodes,
                n_edges: out.n_slots(),
                out,
                inc: OnceLock::new(),
                inc_loader: LazyIncoming::default(),
                undirected: OnceLock::new(),
                symmetric: OnceLock::new(),
                self_transpose: false,
                relabel: (!identity).then(|| {
                    Arc::new(Relabel {
                        original: original.into(),
                        node: OnceLock::from(node),
                    })
                }),
            };
        }
        debug_assert!(
            src_dense
                .iter()
                .chain(&dst_dense)
                .all(|&d| (d as usize) < n_nodes),
            "dense endpoint id out of range (>= n_nodes)"
        );
        let n_edges = src_dense.len();
        let plan = IdsPlan::decide(&src_dense, None, rows);
        let (offsets, targets, ids) =
            Adjacency::build_parts(n_nodes, &src_dense, &dst_dense, plan.builds());
        let offsets: ScalarBuffer<u64> = offsets.into();
        let edge_ids = plan.edge_ids(n_edges, &offsets, None, ids.unwrap_or_default());
        let out = Adjacency {
            offsets,
            targets: targets.into(),
            edge_ids,
            packed: None,
        };
        // `src_dense`/`dst_dense` are intentionally not retained: the transpose and
        // the undirected cache are both recoverable from the out-CSR (see
        // `incoming`), so keeping them would be a permanent 8 B/edge overhead paid
        // even by pipelines that never build the transpose.
        drop((src_dense, dst_dense));
        Topology {
            n_nodes,
            n_edges,
            out,
            inc: OnceLock::new(),
            inc_loader: LazyIncoming::default(),
            undirected: OnceLock::new(),
            symmetric: OnceLock::new(),
            self_transpose: false,
            relabel: None,
        }
    }

    /// [`Self::build`], with the nodes renumbered in `order` for locality
    /// (#172): the dense ids given are the *original* ids, and node `v` of
    /// the result is the node whose original id is
    /// [`Self::original_id`]`(v)`. The relabel is fused into the counting
    /// sort (its first pass counts the in-degrees too, a counting sort
    /// orders the nodes, one streaming pass renames the targets in place),
    /// parallel where the build is. Each node's out-segment keeps its edges in row order, and
    /// `edge_ids` still names the original rows, so weights and edge masks
    /// apply unchanged.
    ///
    /// A caller that maps dense ids to anything (an [`IdMap`]) renumbers it
    /// the same way, through [`Self::original_ids`].
    ///
    /// [`IdMap`]: crate::id_map::IdMap
    pub fn build_ordered(
        n_nodes: usize,
        src_dense: Vec<u32>,
        dst_dense: Vec<u32>,
        order: NodeOrder,
    ) -> Topology {
        Self::build_with_rows(n_nodes, src_dense, dst_dense, order, &mut |_| None)
    }

    /// [`Self::build`] with the nodes renumbered by `original`, a permutation
    /// of `0..n_nodes` that gives the original id of each new node (see
    /// [`Self::build_ordered`]). The identity is no relabel at all.
    ///
    /// # Panics
    ///
    /// If `original` is not a permutation of `0..n_nodes`.
    pub fn build_relabelled(
        n_nodes: usize,
        mut src_dense: Vec<u32>,
        mut dst_dense: Vec<u32>,
        original: Vec<u32>,
    ) -> Topology {
        use crate::parallel::*;
        assert_eq!(original.len(), n_nodes, "the order must name every node");
        assert!(is_permutation(&original), "the order must be a permutation");
        if original.iter().enumerate().all(|(v, &o)| v as u32 == o) {
            return Self::build(n_nodes, src_dense, dst_dense);
        }
        let mut node = vec![0u32; n_nodes];
        for (v, &o) in original.iter().enumerate() {
            node[o as usize] = v as u32;
        }
        let rename = |ids: &mut [u32]| {
            ids.par_chunks_mut(1 << 16)
                .for_each(|chunk| chunk.iter_mut().for_each(|x| *x = node[*x as usize]));
        };
        rename(&mut src_dense);
        rename(&mut dst_dense);
        let relabel = Relabel {
            original: original.into(),
            node: OnceLock::from(Arc::<[u32]>::from(node)),
        };
        let mut topology = Self::build(n_nodes, src_dense, dst_dense);
        topology.relabel = Some(Arc::new(relabel));
        topology
    }

    /// This topology, its nodes known to carry the original ids `original`
    /// (`original[v]` for node `v`; see [`Self::build_ordered`]): how a
    /// stored index that was relabelled is read back. An error if `original`
    /// is not a permutation of the nodes.
    pub fn with_original_ids(mut self, original: ScalarBuffer<u32>) -> Result<Topology, String> {
        if original.len() != self.n_nodes || !is_permutation(&original) {
            return Err("the original node ids are not a permutation of the nodes".into());
        }
        self.relabel = (!original.iter().enumerate().all(|(v, &o)| v as u32 == o)).then(|| {
            Arc::new(Relabel {
                original,
                node: OnceLock::new(),
            })
        });
        Ok(self)
    }

    /// A topology over the same nodes, numbered as `other`'s are (a view
    /// built from it).
    pub(crate) fn numbered_as(mut self, other: &Topology) -> Topology {
        self.relabel = other.relabel.clone();
        self
    }

    /// The original id of each node, if the nodes were relabelled
    /// ([`Self::build_ordered`]); `None` when each node's id is its original
    /// one.
    pub fn original_ids(&self) -> Option<&[u32]> {
        self.relabel.as_ref().map(|r| &r.original[..])
    }

    /// The nodes in original id order (the node of each original id), if the
    /// nodes were relabelled ([`Self::build_ordered`]): the order to emit
    /// per-node results in so that they come out as an unrelabelled index
    /// gives them. Built on first use.
    pub fn nodes_in_original_order(&self) -> Option<&[u32]> {
        self.relabel.as_ref().map(|r| r.nodes())
    }

    /// The original id of node `v` (`v` itself without a relabel).
    #[inline]
    pub fn original_id(&self, v: u32) -> u32 {
        self.relabel.as_ref().map_or(v, |r| r.original[v as usize])
    }

    /// The node whose original id is `o` (`o` itself without a relabel).
    #[inline]
    pub fn node_of_original(&self, o: u32) -> u32 {
        self.relabel.as_ref().map_or(o, |r| r.nodes()[o as usize])
    }

    /// Per-node labels made canonical: each node's label becomes the
    /// smallest original id among the nodes that share its label, so the
    /// labels name the same partition whatever the numbering (#150, #172).
    /// Linear in the nodes. Labels must be below the node count.
    pub(crate) fn canonical_labels(&self, labels: &mut [u32]) {
        let mut least = vec![u32::MAX; labels.len()];
        for (v, &l) in labels.iter().enumerate() {
            let o = self.original_id(v as u32);
            let m = &mut least[l as usize];
            *m = (*m).min(o);
        }
        for l in labels.iter_mut() {
            *l = least[*l as usize];
        }
    }

    /// This topology numbered by its original ids, as [`Self::build`] gives
    /// it from the same rows: `None` when it was not relabelled. The layouts
    /// run over it (their starting positions follow the node ids), and a test
    /// can compare a relabelled build with a first-seen one through it.
    ///
    /// No sort is needed: node `o`'s segment, in either direction, is the
    /// segment of the node whose original id is `o`, in the same row order,
    /// its neighbours renamed. So this is one streaming pass per direction
    /// (the in-adjacency too, when this topology has it built), parallel.
    pub fn in_original_order(&self) -> Option<Topology> {
        let relabel = self.relabel.as_ref()?;
        let out = in_original(&self.out, relabel);
        let inc = OnceLock::new();
        if let Some(incoming) = self.incoming_if_built() {
            let _ = inc.set(in_original(incoming, relabel));
        }
        Some(Topology {
            n_nodes: self.n_nodes,
            n_edges: self.n_edges,
            out,
            inc,
            inc_loader: LazyIncoming::default(),
            undirected: OnceLock::new(),
            symmetric: OnceLock::new(),
            self_transpose: false,
            relabel: None,
        })
    }

    /// This topology in the compressed layout (#173, see [`crate::packed`]):
    /// each node's neighbours sorted and stored as varint-coded gaps, in
    /// both directions (the in-adjacency is transposed compressed when first
    /// asked for). The out-adjacency is sorted and encoded in one pass after
    /// the build, so the peak is the plain adjacency and its bytes, once.
    ///
    /// Its edge ids follow the sorted order: each segment's rows by
    /// `(target, row)` (and `(source, row)` in the in-adjacency). Ids held
    /// are sorted with the targets. Ids not held are built only when a query
    /// reads them, as for the plain layout (#170), from the rows' targets
    /// that `targets` reads back (the dense ids the topology was built from,
    /// before any relabel); without it, they are built now, unless the
    /// segments were in order already.
    ///
    /// Every kernel gives the results it gives on the plain layout with its
    /// segments sorted the same way ([`Self::with_sorted_segments`]), bit
    /// for bit.
    pub fn compressed(self, targets: Option<Arc<dyn RowSource>>) -> Topology {
        let keys = targets.map(|targets| SortKeys {
            targets,
            node: self.relabel.as_ref().map(|r| r.node_map()),
        });
        let Topology {
            n_nodes,
            n_edges,
            out,
            undirected,
            symmetric,
            self_transpose,
            relabel,
            ..
        } = self;
        Topology {
            n_nodes,
            n_edges,
            out: out.pack(keys.as_ref()),
            // A plain in-adjacency (or a stored one) is in another order.
            inc: OnceLock::new(),
            inc_loader: LazyIncoming::default(),
            undirected,
            symmetric,
            self_transpose,
            relabel,
        }
    }

    /// This topology in the plain layout with every segment sorted as
    /// [`Self::compressed`] sorts it, its edge ids built: what a compressed
    /// topology decodes to, for comparing the two layouts.
    #[doc(hidden)]
    pub fn with_sorted_segments(self) -> Topology {
        let Topology {
            n_nodes,
            n_edges,
            out,
            undirected,
            symmetric,
            self_transpose,
            relabel,
            ..
        } = self;
        let out = out.sorted(None);
        let inc = OnceLock::new();
        if !self_transpose {
            let _ = inc.set(Adjacency::transpose(&out, n_nodes).sorted(None));
        }
        Topology {
            n_nodes,
            n_edges,
            out,
            inc,
            inc_loader: LazyIncoming::default(),
            undirected,
            symmetric,
            self_transpose,
            relabel,
        }
    }

    /// Whether this topology is in the compressed layout (#173).
    pub fn is_compressed(&self) -> bool {
        self.out.is_packed()
    }

    /// The bytes the adjacencies built so far hold in memory (see
    /// [`Adjacency::heap_bytes`]): the out-adjacency, and the in-adjacency
    /// if it is built.
    pub fn heap_bytes(&self) -> usize {
        self.out.heap_bytes()
            + if self.self_transpose {
                0
            } else {
                self.inc.get().map_or(0, Adjacency::heap_bytes)
            }
    }

    /// A symmetric topology taken directly from an undirected CSR: node `u`'s
    /// out-segment is its sorted, deduplicated neighbour list, and slot `k` is row
    /// `k` (`edge_ids` is the identity). That is exactly what [`Topology::build`]
    /// produces from the rows `u -> v` in (source, target) order, without the
    /// counting sort.
    ///
    /// Each segment is ascending and every pair appears once in each direction, so
    /// the transpose has the same offsets and targets as `out`; `incoming` returns
    /// `out` itself. The one difference from a built transpose is that an in-slot's
    /// `edge_ids` entry names the mirror row (`u -> v` instead of `v -> u`). The two
    /// rows of a pair are interchangeable — same endpoints, and the same weight in
    /// the weighted view — and a view is never masked, so no kernel can tell.
    pub(crate) fn from_undirected(
        n_nodes: usize,
        offsets: Vec<u64>,
        targets: Vec<u32>,
    ) -> Topology {
        debug_assert_eq!(offsets.len(), n_nodes + 1);
        let n_edges = targets.len();
        assert!(
            n_edges <= u32::MAX as usize,
            "more than u32::MAX (~4.29B) edges is beyond the v0.1 edge-id cap"
        );
        Topology {
            n_nodes,
            n_edges,
            out: Adjacency {
                offsets: offsets.into(),
                targets: targets.into(),
                edge_ids: EdgeIds::identity(n_edges),
                packed: None,
            },
            inc: OnceLock::new(),
            inc_loader: LazyIncoming::default(),
            undirected: OnceLock::new(),
            symmetric: OnceLock::new(),
            self_transpose: true,
            relabel: None,
        }
    }

    /// Reassemble a topology from its parts: the out-adjacency, and the
    /// in-adjacency if it was built. This is how a stored index (the SQL
    /// engine's `.ursa` database) is read back; the parts are what
    /// [`Topology::out`] and [`Topology::incoming_if_built`] returned for a
    /// topology made by [`Topology::build`].
    ///
    /// The parts are checked, so malformed input is an error and never a later
    /// out-of-bounds panic in a kernel: each adjacency has `n_nodes + 1`
    /// non-decreasing offsets from 0 to its slot count, as many targets and edge
    /// ids as slots, targets below `n_nodes`, and edge ids that are a
    /// permutation of the edge rows; both directions have the same number of
    /// edges. That the in-adjacency is the out-adjacency's transpose is not
    /// checked (that would cost a transpose); a caller that cannot vouch for it
    /// passes `None`, and it is rebuilt on demand.
    pub fn from_parts(
        n_nodes: usize,
        out: Adjacency,
        incoming: Option<Adjacency>,
    ) -> Result<Topology, String> {
        if n_nodes > u32::MAX as usize {
            return Err(format!("{n_nodes} nodes is beyond the u32 dense-index cap"));
        }
        out.check(n_nodes, "out")?;
        let n_edges = out.n_slots();
        let inc = OnceLock::new();
        if let Some(incoming) = incoming {
            incoming.check(n_nodes, "in")?;
            if incoming.n_slots() != n_edges {
                return Err(format!(
                    "the in-adjacency has {} edges; the out-adjacency has {n_edges}",
                    incoming.n_slots()
                ));
            }
            let _ = inc.set(incoming);
        }
        Ok(Topology {
            n_nodes,
            n_edges,
            out,
            inc,
            inc_loader: LazyIncoming::default(),
            undirected: OnceLock::new(),
            symmetric: OnceLock::new(),
            self_transpose: false,
            relabel: None,
        })
    }

    /// [`Self::from_parts`] for parts whose integrity the caller vouches for
    /// with a checksum (a stored index, #382): only their shape is checked,
    /// in `O(n_nodes)` (the arrays' lengths, and `n_nodes + 1` non-decreasing
    /// offsets from 0 to the slot count), never the targets or edge ids, so
    /// arrays memory-mapped from a file stay unread until a kernel reads them.
    ///
    /// Every neighbour slice is in bounds. A target or edge id that is out of
    /// range (a file damaged in a way its checksum would catch, read before
    /// the checksum is checked) can only make a kernel panic on a bounds
    /// check: no kernel reads one unchecked.
    pub fn from_checksummed_parts(n_nodes: usize, out: Adjacency) -> Result<Topology, String> {
        if n_nodes > u32::MAX as usize {
            return Err(format!("{n_nodes} nodes is beyond the u32 dense-index cap"));
        }
        out.check_shape(n_nodes, "out")?;
        Ok(Topology {
            n_nodes,
            n_edges: out.n_slots(),
            out,
            inc: OnceLock::new(),
            inc_loader: LazyIncoming::default(),
            undirected: OnceLock::new(),
            symmetric: OnceLock::new(),
            self_transpose: false,
            relabel: None,
        })
    }

    /// This topology, with its in-adjacency read by `loader` the first time
    /// it is asked for ([`Topology::incoming`]) instead of transposed from the
    /// out-CSR. This is how a stored index keeps its in-adjacency on disk until
    /// an algorithm needs it.
    ///
    /// What the loader returns is checked as [`Topology::from_parts`] checks an
    /// in-adjacency; if it is malformed, or the loader returns `None`, the
    /// transpose is built as usual. As in `from_parts`, that it *is* the
    /// transpose is the caller's to vouch for. Ignored if the in-adjacency is
    /// already there, and on a symmetric view, whose transpose is itself.
    pub fn with_incoming_loader(self, loader: IncomingLoader) -> Topology {
        if self.inc.get().is_none() && !self.self_transpose {
            *self.inc_loader.0.lock().unwrap_or_else(|e| e.into_inner()) = Some((loader, false));
        }
        self
    }

    /// [`Self::with_incoming_loader`], for a loader whose in-adjacency is
    /// checksummed: only its shape is checked, as
    /// [`Self::from_checksummed_parts`] checks the out-adjacency's.
    pub fn with_checksummed_incoming_loader(self, loader: IncomingLoader) -> Topology {
        if self.inc.get().is_none() && !self.self_transpose {
            *self.inc_loader.0.lock().unwrap_or_else(|e| e.into_inner()) = Some((loader, true));
        }
        self
    }

    /// The stored in-adjacency `loader` reads, if it is well formed (in shape
    /// only, if `checksummed`).
    fn load_incoming(&self, loader: IncomingLoader, checksummed: bool) -> Option<Adjacency> {
        let incoming = loader()?;
        let checked = match checksummed {
            true => incoming.check_shape(self.n_nodes, "in"),
            false => incoming.check(self.n_nodes, "in"),
        };
        (incoming.n_slots() == self.n_edges && checked.is_ok()).then_some(incoming)
    }

    /// Whether [`Topology::incoming`] would return without building the
    /// transpose: it is built, it is the out-adjacency (a symmetric view), or a
    /// stored one is waiting to be read. Nothing is built or read.
    pub fn incoming_is_cheap(&self) -> bool {
        self.self_transpose
            || self.inc.get().is_some()
            || self
                .inc_loader
                .0
                .lock()
                .map(|l| l.is_some())
                .unwrap_or_else(|e| e.into_inner().is_some())
    }

    /// The in-adjacency if it has been built (or loaded), without building or
    /// loading it.
    pub fn incoming_if_built(&self) -> Option<&Adjacency> {
        if self.self_transpose {
            return Some(&self.out);
        }
        self.inc.get()
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
    /// transposing the out-CSR (see [`Adjacency::transpose`]): the same result the
    /// retained-endpoint build produced, without the permanent dense-endpoint
    /// storage. A topology with an [`IncomingLoader`] reads it instead.
    pub fn incoming(&self) -> &Adjacency {
        if self.self_transpose {
            return &self.out;
        }
        self.inc.get_or_init(|| {
            self.inc_loader
                .take()
                .and_then(|(loader, checksummed)| self.load_incoming(loader, checksummed))
                .unwrap_or_else(|| Adjacency::transpose(&self.out, self.n_nodes))
        })
    }

    /// The reversed graph: every row `u -> v` read as `v -> u`, so a directed
    /// kernel run over it follows in-edges (`direction='in'` for `pagerank` and
    /// `betweenness`, #415). Its out-adjacency is this topology's in-adjacency
    /// and the other way round; both are shared, not copied (cloning an
    /// [`Adjacency`] clones references). Edge ids are kept, so a per-row weight
    /// or an [`EdgeMask`] of this topology applies to the reversed one unchanged.
    pub fn transposed(&self) -> Topology {
        let inc = OnceLock::new();
        let _ = inc.set(self.out.clone());
        Topology {
            n_nodes: self.n_nodes,
            n_edges: self.n_edges,
            out: self.incoming().clone(),
            inc,
            inc_loader: LazyIncoming::default(),
            undirected: OnceLock::new(),
            symmetric: OnceLock::new(),
            self_transpose: false,
            relabel: self.relabel.clone(),
        }
    }

    /// Undirected sorted adjacency (`out ∪ in`, self-loops dropped, deduplicated),
    /// built and cached on first call. Shared by `triangle_count` and
    /// `clustering_coefficient` so a pipeline computing both pays the build once.
    pub fn undirected(&self) -> &UndirectedCsr {
        self.undirected.get_or_init(|| self.build_undirected(None))
    }

    /// The unmasked symmetric view — one `u -> v` and one `v -> u` row per
    /// distinct unordered pair, self-loops dropped — that the `direction="both"`
    /// forms of the directed kernels run over. Built on first call from the cached
    /// [`Topology::undirected`] CSR with no re-sort (see
    /// [`Topology::from_undirected`]) and cached, so repeated queries share it.
    pub fn symmetric(&self) -> &Topology {
        self.symmetric.get_or_init(|| {
            let u = self.undirected();
            Box::new(
                Topology::from_undirected(self.n_nodes, u.offsets.clone(), u.targets.clone())
                    .numbered_as(self),
            )
        })
    }

    /// Build the flat undirected CSR: per-node sorted/deduped `out ∪ in` neighbour
    /// lists (self-loops filtered), written straight into one buffer. With a
    /// `mask`, only kept edges contribute (the per-subgraph view).
    ///
    /// Each node first gets an upper-bound segment of `out_deg + in_deg` slots,
    /// fills it with its neighbours, then sorts and deduplicates it in place
    /// (parallel, disjoint segments). One serial pass then slides the deduplicated
    /// prefixes left into a contiguous buffer. There are no per-node allocations,
    /// and the peak is one `2m`-slot buffer rather than per-node vectors plus their
    /// flattened copy.
    fn build_undirected(&self, mask: Option<&EdgeMask>) -> UndirectedCsr {
        use crate::parallel::*;
        let n = self.n_nodes;
        let out = &self.out;
        // Each segment is sorted and deduplicated, so only the set of
        // in-neighbours matters, not their order. Without a mask, and with
        // no in-adjacency at hand, they are transposed for this build alone,
        // in slot order, from the targets: no row order, no edge ids, and no
        // in-adjacency kept (#170).
        let unordered;
        // A compressed one's transpose is compressed too, and kept: small.
        let inc = if mask.is_none() && !self.incoming_is_cheap() && !out.is_packed() {
            let (offsets, targets, _) =
                transpose_parts(&out.offsets, &out.targets, TransposeIds::None, n);
            let m = targets.len();
            unordered = Adjacency {
                offsets: offsets.into(),
                targets: targets.into(),
                edge_ids: EdgeIds::identity(m),
                packed: None,
            };
            &unordered
        } else {
            self.incoming()
        };
        if mask.is_some() {
            // Built once here, not on first read inside the parallel fill.
            let _ = (out.edge_ids.get(), inc.edge_ids.get());
        }

        let mut bound = vec![0u64; n + 1];
        for u in 0..n {
            let d = out.degree(u as u32) as u64 + inc.degree(u as u32) as u64;
            bound[u + 1] = bound[u] + d;
        }
        let mut targets = vec![0u32; bound[n] as usize];
        let addr = targets.as_mut_ptr() as usize;
        let fill = |u: u32| {
            let start = bound[u as usize] as usize;
            let cap = (bound[u as usize + 1] - bound[u as usize]) as usize;
            // SAFETY: segments `bound[u]..bound[u + 1]` are disjoint across
            // nodes and lie inside `targets`, which outlives the loop.
            let seg = unsafe { std::slice::from_raw_parts_mut((addr as *mut u32).add(start), cap) };
            let mut len = 0;
            for adj in [out, inc] {
                match mask {
                    None => adj.for_each_neighbor(u, |w| {
                        if w != u {
                            seg[len] = w;
                            len += 1;
                        }
                    }),
                    Some(m) => adj.for_each_neighbor_row(u, |w, e| {
                        if w != u && m.keep(e) {
                            seg[len] = w;
                            len += 1;
                        }
                    }),
                }
            }
            let seg = &mut seg[..len];
            seg.sort_unstable();
            // In-place dedup of the sorted prefix.
            let mut kept = 0;
            for i in 0..seg.len() {
                if kept == 0 || seg[i] != seg[kept - 1] {
                    seg[kept] = seg[i];
                    kept += 1;
                }
            }
            kept as u32
        };
        // Serial on a rayon worker: see `parallel::in_worker`.
        let lens: Vec<u32> = if in_worker() {
            (0..n as u32).map(fill).collect()
        } else {
            (0..n as u32).into_par_iter().map(fill).collect()
        };

        // Compact: each segment's kept prefix moves left (never right), in order.
        let mut offsets = vec![0u64; n + 1];
        let mut write = 0usize;
        for u in 0..n {
            let (start, len) = (bound[u] as usize, lens[u] as usize);
            targets.copy_within(start..start + len, write);
            write += len;
            offsets[u + 1] = write as u64;
        }
        targets.truncate(write);
        targets.shrink_to_fit();
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
            None => adj.for_each_neighbor(u, &mut f),
            Some(m) => adj.for_each_neighbor_row(u, |v, e| {
                if m.keep(e) {
                    f(v);
                }
            }),
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
            adj.for_each_neighbor_row(u, |v, e| {
                if mask.is_none_or(|m| m.keep(e)) {
                    f(v, weights[e as usize]);
                }
            })
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
        self.build_undirected(Some(mask))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0 -> 1, 0 -> 2, 1 -> 2, 2 -> 0
    fn diamond() -> Topology {
        Topology::build(3, vec![0, 0, 1, 2], vec![1, 2, 2, 0])
    }

    fn adjacency(offsets: &[u64], targets: &[u32], edge_ids: &[u32]) -> Adjacency {
        Adjacency {
            offsets: offsets.to_vec().into(),
            targets: targets.to_vec().into(),
            edge_ids: edge_ids.to_vec().into(),
            packed: None,
        }
    }

    #[test]
    fn parts_round_trip() {
        let built = diamond();
        assert!(built.incoming_if_built().is_none());
        let incoming = built.incoming().clone();
        let back = Topology::from_parts(3, built.out().clone(), Some(incoming.clone())).unwrap();
        assert_eq!((back.n_nodes(), back.n_edges()), (3, 4));
        assert_eq!(back.out().offsets, built.out().offsets);
        assert_eq!(back.out().targets, built.out().targets);
        assert_eq!(back.out().edge_ids, built.out().edge_ids);
        assert_eq!(back.incoming_if_built().unwrap().targets, incoming.targets);
        // Without the in-adjacency, it is rebuilt on demand, the same.
        let lazy = Topology::from_parts(3, built.out().clone(), None).unwrap();
        assert!(lazy.incoming_if_built().is_none());
        assert_eq!(lazy.incoming().targets, incoming.targets);
        assert_eq!(lazy.incoming().edge_ids, incoming.edge_ids);
    }

    #[test]
    fn a_loader_supplies_the_in_adjacency_on_first_demand() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let built = diamond();
        let incoming = built.incoming().clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let lazy = {
            let (calls, incoming) = (Arc::clone(&calls), incoming.clone());
            Topology::from_parts(3, built.out().clone(), None)
                .unwrap()
                .with_incoming_loader(Box::new(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Some(incoming)
                }))
        };
        // Not read until asked for.
        assert!(lazy.incoming_if_built().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(lazy.incoming().edge_ids, incoming.edge_ids);
        assert_eq!(lazy.incoming().targets, incoming.targets);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(lazy.incoming_if_built().is_some());

        // A loader that fails, or returns a malformed adjacency: transposed.
        for loaded in [
            None,
            Some(adjacency(&[0, 1, 2, 3], &[2, 0, 0], &[3, 0, 1])),
            Some(adjacency(&[0, 1, 3, 3], &[2, 0, 7], &[3, 0, 1, 2])),
        ] {
            let topo = Topology::from_parts(3, built.out().clone(), None)
                .unwrap()
                .with_incoming_loader(Box::new(move || loaded));
            assert_eq!(topo.incoming().targets, incoming.targets);
            assert_eq!(topo.incoming().edge_ids, incoming.edge_ids);
        }
    }

    #[test]
    fn malformed_parts_are_errors() {
        let ok = adjacency(&[0, 2, 3, 3], &[1, 2, 0], &[0, 1, 2]);
        assert!(Topology::from_parts(3, ok.clone(), None).is_ok());
        for (bad, why) in [
            (adjacency(&[0, 2, 3], &[1, 2, 0], &[0, 1, 2]), "offsets"),
            (adjacency(&[0, 2, 3, 4], &[1, 2, 0], &[0, 1, 2]), "span"),
            (adjacency(&[1, 2, 3, 3], &[1, 2, 0], &[0, 1, 2]), "span"),
            (
                adjacency(&[0, 3, 2, 3], &[1, 2, 0], &[0, 1, 2]),
                "decreasing",
            ),
            (adjacency(&[0, 2, 3, 3], &[1, 3, 0], &[0, 1, 2]), "target"),
            (adjacency(&[0, 2, 3, 3], &[1, 2, 0], &[0, 1]), "edge ids"),
            (
                adjacency(&[0, 2, 3, 3], &[1, 2, 0], &[0, 1, 1]),
                "permutation",
            ),
            (
                adjacency(&[0, 2, 3, 3], &[1, 2, 0], &[0, 1, 3]),
                "permutation",
            ),
        ] {
            let err = Topology::from_parts(3, bad, None).unwrap_err();
            assert!(err.contains(why), "{err}");
        }
        let short = adjacency(&[0, 1, 2, 2], &[1, 2], &[0, 1]);
        let err = Topology::from_parts(3, ok, Some(short)).unwrap_err();
        assert!(err.contains("in-adjacency has 2 edges"), "{err}");
    }

    #[cfg(feature = "rayon")]
    #[test]
    fn lazy_caches_requested_inside_a_parallel_loop_do_not_deadlock() {
        // Every iteration of a parallel loop requests the lazy transpose and
        // undirected view. A parallel initializer would let the worker running it
        // steal another iteration while it waits, re-enter the same OnceLock on the
        // same thread and hang. Repeated on fresh topologies (small ones and one
        // big enough for the parallel CSR build) to give the race many chances.
        use rayon::prelude::*;
        for (n, m) in [(4u32, 3u32), (64, 500), (5_000, 200_000)] {
            let src: Vec<u32> = (0..m)
                .map(|i| (i as u64 * 7919 % n as u64) as u32)
                .collect();
            let dst: Vec<u32> = (0..m)
                .map(|i| (i as u64 * 104_729 % n as u64) as u32)
                .collect();
            let eager = Topology::build(n as usize, src.clone(), dst.clone());
            let expect: Vec<usize> = (0..n)
                .map(|u| eager.incoming().neighbors(u).len() + eager.undirected().degree(u))
                .collect();
            for _ in 0..50 {
                let lazy = Topology::build(n as usize, src.clone(), dst.clone());
                let got: Vec<usize> = (0..n)
                    .into_par_iter()
                    .with_max_len(1)
                    .map(|u| lazy.incoming().neighbors(u).len() + lazy.undirected().degree(u))
                    .collect();
                assert_eq!(got, expect);
                assert_eq!(lazy.incoming().edge_ids, eager.incoming().edge_ids);
            }
        }
    }

    #[test]
    fn mask_from_boolean_array_matches_from_bools() {
        // Nulls are not kept; an offset slice must read its own bits.
        let vals: Vec<Option<bool>> = (0..200)
            .map(|i| match i % 5 {
                0 => None,
                1 | 3 => Some(true),
                _ => Some(false),
            })
            .collect();
        let arr = BooleanArray::from(vals.clone());
        for (start, len) in [(0, 200), (3, 130), (64, 64), (7, 0)] {
            let slice = arr.slice(start, len);
            let slice = slice.as_any().downcast_ref::<BooleanArray>().unwrap();
            let bools: Vec<bool> = vals[start..start + len]
                .iter()
                .map(|v| v.unwrap_or(false))
                .collect();
            let (a, b) = (
                EdgeMask::from_boolean_array(slice),
                EdgeMask::from_bools(&bools),
            );
            assert_eq!(a.words, b.words, "start={start} len={len}");
            assert_eq!((a.n_edges, a.n_kept), (b.n_edges, b.n_kept));
        }
    }

    #[test]
    fn transpose_matches_building_from_the_endpoints() {
        // Unsorted random rows (so segments need the row sort), small enough for
        // the serial path and large enough for the parallel one.
        use rand::{Rng, SeedableRng};
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(11);
        for (n, m) in [(7usize, 40usize), (1_000, 70_000), (50_000, 300_000)] {
            let src: Vec<u32> = (0..m).map(|_| rng.gen_range(0..n as u32)).collect();
            let dst: Vec<u32> = (0..m).map(|_| rng.gen_range(0..n as u32)).collect();
            let expect = Adjacency::build(n, &dst, &src);
            let t = Topology::build(n, src, dst);
            let got = t.incoming();
            assert_eq!(got.offsets, expect.offsets, "n={n} m={m}");
            assert_eq!(got.targets, expect.targets, "n={n} m={m}");
            assert_eq!(got.edge_ids, expect.edge_ids, "n={n} m={m}");
        }
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
            let serial = Adjacency::build_serial(n, &keys, &other, true);
            for n_chunks in [2usize, 3, 8] {
                let par = Adjacency::build_parallel(n, &keys, &other, n_chunks, true);
                assert_eq!(serial.0, par.0, "offsets (n={n}, chunks={n_chunks})");
                assert_eq!(serial.1, par.1, "targets (n={n}, chunks={n_chunks})");
                assert_eq!(serial.2, par.2, "edge_ids (n={n}, chunks={n_chunks})");
                // Without edge ids, the same offsets and targets (#170).
                let bare = Adjacency::build_parallel(n, &keys, &other, n_chunks, false);
                assert_eq!((&serial.0, &serial.1, None), (&bare.0, &bare.1, bare.2));
            }
            let bare = Adjacency::build_serial(n, &keys, &other, false);
            assert_eq!((&serial.0, &serial.1, None), (&bare.0, &bare.1, bare.2));
            // The ids rebuilt from the keys alone are the ones the build wrote.
            assert_eq!(
                Some(crate::edge_ids::rows_by_key(&serial.0, &keys, None)),
                serial.2
            );
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
    fn induced_mask_keeps_edges_between_kept_nodes() {
        // rows: 0:0->1, 1:1->2, 2:2->0, 3:0->2
        let t = Topology::build(3, vec![0, 1, 2, 0], vec![1, 2, 0, 2]);
        let m = EdgeMask::induced(&t, &[true, false, true], None);
        let kept: Vec<bool> = (0..4).map(|e| m.keep(e)).collect();
        assert_eq!(kept, vec![false, false, true, true]);
        assert_eq!(m.n_kept(), 2);
        let within = EdgeMask::from_bools(&[true, true, false, true]);
        let m = EdgeMask::induced(&t, &[true, false, true], Some(&within));
        assert_eq!((0..4).filter(|&e| m.keep(e)).collect::<Vec<_>>(), vec![3]);
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
    fn symmetric_view_is_cached_and_its_own_transpose() {
        let t = diamond();
        let s = t.symmetric();
        assert!(std::ptr::eq(s, t.symmetric()));
        // Pairs {0,1}, {0,2}, {1,2}; the reciprocal 0->2 / 2->0 collapses.
        assert_eq!(s.out().offsets, vec![0, 2, 4, 6]);
        assert_eq!(s.out().targets, vec![1, 2, 0, 2, 0, 1]);
        assert_eq!(s.out().edge_ids, vec![0, 1, 2, 3, 4, 5]);
        assert!(std::ptr::eq(s.incoming(), s.out()));
        assert!(!std::ptr::eq(t.incoming(), t.out()));
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
    fn undirected_from_edges_is_the_topologys() {
        // A skewed multigraph with self-loops, repeats, both directions and
        // nodes with no edges.
        let n = 300u32;
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for _ in 0..4000 {
            let (a, b) = (next() % 280, next() % 280);
            src.push(((a * a) / 280) as u32);
            dst.push(b as u32);
        }
        for n_nodes in [n as usize, 0] {
            let (src, dst) = if n_nodes == 0 {
                (Vec::new(), Vec::new())
            } else {
                (src.clone(), dst.clone())
            };
            let direct = UndirectedCsr::from_edges(n_nodes, &src, &dst);
            let topology = Topology::build(n_nodes, src, dst);
            let built = topology.undirected();
            assert_eq!(direct.offsets, built.offsets);
            assert_eq!(direct.targets, built.targets);
            // And it passes as parts, unchanged.
            let parts =
                UndirectedCsr::from_parts(n_nodes, direct.offsets.clone(), direct.targets.clone())
                    .unwrap();
            assert_eq!(parts.offsets, built.offsets);
            assert_eq!(parts.targets, built.targets);
        }
    }

    #[test]
    fn undirected_from_edges_is_the_same_sorted_or_read_off_the_bitmap() {
        // By sorting every pair: what `from_edges` gives whether a node's
        // run is long (a hub's, read off the bitmap), short (sorted), or
        // long but among so many nodes that scanning the bitmap's summary
        // would cost more than the sort (#525).
        let reference = |n: usize, src: &[u32], dst: &[u32]| {
            let mut pairs: Vec<(u32, u32)> = src
                .iter()
                .zip(dst)
                .filter(|(s, d)| s != d)
                .flat_map(|(&s, &d)| [(s, d), (d, s)])
                .collect();
            pairs.sort_unstable();
            pairs.dedup();
            let mut offsets = vec![0u64; n + 1];
            for &(u, _) in &pairs {
                offsets[u as usize + 1] += 1;
            }
            for u in 0..n {
                offsets[u + 1] += offsets[u];
            }
            (
                offsets,
                pairs.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
            )
        };
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n) as u32
        };
        // A hub of 5,000 ends (with repeats and loops) among 400 nodes,
        // and 3,000,000 nodes (a summary of 733 words: a run is sorted
        // below 91 ends) with runs of 60 and of 200.
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for _ in 0..5_000 {
            src.push(7);
            dst.push(next(400));
        }
        for _ in 0..2_000 {
            src.push(next(400));
            dst.push(next(400));
        }
        let small = (400usize, src, dst);
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for (hub, ends) in [(11u32, 60), (2_999_999, 200), (1_500_000, 200)] {
            for _ in 0..ends {
                src.push(hub);
                dst.push(next(3_000_000));
                // Repeats, in both directions.
                dst.push(hub);
                src.push(*dst.first().unwrap());
            }
        }
        let large = (3_000_000usize, src, dst);
        for (n, src, dst) in [small, large] {
            let built = UndirectedCsr::from_edges(n, &src, &dst);
            let (offsets, targets) = reference(n, &src, &dst);
            assert_eq!(built.offsets, offsets);
            assert_eq!(built.targets, targets);
        }
    }

    #[test]
    fn malformed_undirected_parts_are_errors() {
        let bad = |n, offsets: &[u64], targets: &[u32]| {
            UndirectedCsr::from_parts(n, offsets.to_vec(), targets.to_vec()).unwrap_err()
        };
        // 0 - 1, 1 - 2.
        assert!(UndirectedCsr::from_parts(3, vec![0, 1, 3, 4], vec![1, 0, 2, 1]).is_ok());
        assert!(bad(3, &[0, 1, 3], &[1, 0, 2, 1]).contains("offsets"));
        assert!(bad(3, &[0, 1, 3, 5], &[1, 0, 2, 1]).contains("not from 0"));
        assert!(bad(3, &[0, 3, 1, 4], &[1, 0, 2, 1]).contains("fall"));
        assert!(bad(3, &[0, 1, 3, 4], &[1, 2, 0, 1]).contains("does not have"));
        assert!(bad(3, &[0, 1, 3, 4], &[1, 2, 0, 0]).contains("does not have"));
        assert!(bad(4, &[0, 2, 3, 4, 4], &[2, 1, 0, 0]).contains("ascending"));
        assert!(bad(3, &[0, 1, 3, 4], &[1, 0, 7, 1]).contains("ascending"));
        assert!(bad(2, &[0, 1, 2], &[0, 1]).contains("ascending"));
        assert!(bad(3, &[0, 2, 3, 4], &[1, 1, 0, 1]).contains("ascending"));
        // 0 names 2, and 2 does not name 0.
        assert!(bad(3, &[0, 2, 3, 4], &[1, 2, 0, 1]).contains("does not have"));
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
