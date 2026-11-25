//! An adjacency's edge ids, built only when something reads them (#170).
//!
//! `edge_ids[k]` is the original edge row of CSR slot `k`. Only a kernel that
//! goes back to the rows reads it: a weight gather, an edge mask, an edge's
//! row in a result. An unweighted, unmasked pipeline (degree, PageRank,
//! components, BFS, triangles, label propagation over the whole graph) never
//! does, yet the array costs 4 B/edge in each built direction.
//!
//! So an [`EdgeIds`] is either built, or knows how to build itself on first
//! read, and caches what it built:
//!
//! - **identity**: rows already grouped by source (an edge list sorted by
//!   source) have `edge_ids[k] == k`, which costs nothing to rebuild (#155);
//! - **from the rows' sources**: a [`RowSource`] reads back the source of
//!   every row, and the build's stable counting sort is replayed over it
//!   (within a node's segment, its rows in row order);
//! - **the transpose of the out-adjacency's**: an in-adjacency's edge ids
//!   are each target's rows in row order, read off the out-adjacency.
//!
//! What any of them builds is exactly the array an eager build gives, so a
//! result never depends on whether the ids were built eagerly or late.

use std::fmt;
use std::ops::Deref;
use std::sync::{Arc, OnceLock};

use arrow::buffer::ScalarBuffer;

/// Reads back the source endpoint of every edge row, in row order, as the
/// dense ids the topology was built from (before any locality relabel): what
/// rebuilding an out-adjacency's edge ids needs (see [`EdgeIds`]).
///
/// Called at most once per topology for its out-adjacency (the ids are
/// cached once built), and again for a transient rebuild (an in-adjacency's
/// transpose, or a stored index's write) while they are not.
pub trait RowSource: Send + Sync {
    /// The source of every row, `n_edges` of them, in row order.
    ///
    /// # Panics
    ///
    /// May panic if the rows can no longer be read (a spill file's read
    /// failing): there is no result to fall back on.
    fn sources(&self) -> Vec<u32>;
}

/// The rows' sources, held in memory: no saving, but the reference a test
/// compares a spilled source with.
impl RowSource for Vec<u32> {
    fn sources(&self) -> Vec<u32> {
        self.clone()
    }
}

/// The rows' sources written to an anonymous temporary file (created in
/// [`std::env::temp_dir`] and unlinked at once, so it is removed when the
/// last handle closes, even on a crash), read back on demand.
#[cfg(unix)]
#[derive(Debug)]
pub struct SpilledRows {
    file: std::fs::File,
    len: usize,
}

#[cfg(unix)]
impl SpilledRows {
    /// Write `sources` to a fresh temporary file. An error (no temporary
    /// directory, a full disk) leaves nothing behind, and the caller keeps
    /// the ids in memory instead.
    pub fn write(sources: &[u32]) -> std::io::Result<SpilledRows> {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = dir.join(format!(
            "ursa-edge-rows-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        // Unlinked now: the open handle keeps the data until it is dropped.
        let removed = std::fs::remove_file(&path);
        let mut writer = std::io::BufWriter::with_capacity(1 << 20, &file);
        let mut bytes = Vec::with_capacity(1 << 16);
        for chunk in sources.chunks(1 << 14) {
            bytes.clear();
            bytes.extend(chunk.iter().flat_map(|s| s.to_le_bytes()));
            writer.write_all(&bytes)?;
        }
        writer.flush()?;
        drop(writer);
        removed?;
        Ok(SpilledRows {
            file,
            len: sources.len(),
        })
    }
}

#[cfg(unix)]
impl RowSource for SpilledRows {
    fn sources(&self) -> Vec<u32> {
        use std::os::unix::fs::FileExt;
        let mut out = Vec::with_capacity(self.len);
        let mut bytes = vec![0u8; 1 << 20];
        let mut at = 0usize;
        while at < self.len {
            let n = (self.len - at).min(bytes.len() / 4);
            let buf = &mut bytes[..n * 4];
            self.file
                .read_exact_at(buf, (at * 4) as u64)
                .unwrap_or_else(|e| panic!("reading back the spilled edge rows failed: {e}"));
            out.extend(
                buf.chunks_exact(4)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            );
            at += n;
        }
        out
    }
}

/// How an [`EdgeIds`] that is not built builds itself.
pub(crate) enum Rebuild {
    /// Built at construction: nothing to rebuild.
    Built,
    /// `edge_ids[k] == k`.
    Identity,
    /// The out-adjacency's: a stable counting sort of the rows by source,
    /// into the segments `offsets` gives. `node[o]` is the node of original
    /// id `o` when the nodes were relabelled.
    Rows {
        offsets: ScalarBuffer<u64>,
        node: Option<Arc<[u32]>>,
        source: Arc<dyn RowSource>,
    },
    /// An in-adjacency's: the transpose of the out-adjacency's ids.
    Transpose {
        out_offsets: ScalarBuffer<u64>,
        out_targets: ScalarBuffer<u32>,
        out_ids: EdgeIds,
        n_nodes: usize,
    },
}

struct Inner {
    len: usize,
    built: OnceLock<ScalarBuffer<u32>>,
    rebuild: Rebuild,
}

/// The original edge row of each CSR slot of one [`Adjacency`], built when
/// first read (see the module docs). It reads as a `[u32]` (`&ids[..]`,
/// `ids[k]`), building itself the first time; [`EdgeIds::with`] reads it
/// without keeping what it built.
///
/// Cloning one shares it: a clone that builds the ids builds them for both.
///
/// [`Adjacency`]: crate::topology::Adjacency
#[derive(Clone)]
pub struct EdgeIds(Arc<Inner>);

impl EdgeIds {
    /// Ids already built.
    pub fn built(ids: ScalarBuffer<u32>) -> EdgeIds {
        let built = OnceLock::new();
        let len = ids.len();
        let _ = built.set(ids);
        EdgeIds(Arc::new(Inner {
            len,
            built,
            rebuild: Rebuild::Built,
        }))
    }

    /// The identity over `len` slots (`edge_ids[k] == k`), not built until
    /// read.
    pub fn identity(len: usize) -> EdgeIds {
        Self::lazy(len, Rebuild::Identity)
    }

    /// The out-adjacency ids of the rows whose sources `source` reads back,
    /// grouped into the segments `offsets` gives (node `node[o]` for
    /// original id `o`, when relabelled), not built until read.
    pub(crate) fn from_rows(
        len: usize,
        offsets: ScalarBuffer<u64>,
        node: Option<Arc<[u32]>>,
        source: Arc<dyn RowSource>,
    ) -> EdgeIds {
        Self::lazy(
            len,
            Rebuild::Rows {
                offsets,
                node,
                source,
            },
        )
    }

    pub(crate) fn lazy(len: usize, rebuild: Rebuild) -> EdgeIds {
        EdgeIds(Arc::new(Inner {
            len,
            built: OnceLock::new(),
            rebuild,
        }))
    }

    /// The number of slots.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len
    }

    /// Whether there are no slots.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.len == 0
    }

    /// Whether the ids are held in memory (built at construction, or since).
    pub fn is_built(&self) -> bool {
        self.0.built.get().is_some()
    }

    /// Whether these are the identity (`edge_ids[k] == k`), known without
    /// reading them: rows grouped by source.
    pub fn is_identity(&self) -> bool {
        matches!(self.0.rebuild, Rebuild::Identity)
    }

    /// The ids, built and kept on first call.
    #[inline]
    pub fn get(&self) -> &ScalarBuffer<u32> {
        self.0.built.get_or_init(|| self.compute())
    }

    /// Call `f` with the ids, built for the call and then dropped if they
    /// were not built already: for one pass that should not keep them (a
    /// transpose, a stored index's write).
    pub fn with<R>(&self, f: impl FnOnce(&[u32]) -> R) -> R {
        match self.0.built.get() {
            Some(ids) => f(ids),
            None => f(&self.compute()),
        }
    }

    /// The ids, without keeping them if they are not held: the held ones
    /// (a reference, not a copy), or a fresh build that the caller owns.
    pub fn detached(&self) -> ScalarBuffer<u32> {
        match self.0.built.get() {
            Some(ids) => ids.clone(),
            None => self.compute(),
        }
    }

    /// Build the ids.
    fn compute(&self) -> ScalarBuffer<u32> {
        let len = self.0.len;
        match &self.0.rebuild {
            // `built` was set at construction.
            Rebuild::Built => unreachable!("built edge ids are always held"),
            Rebuild::Identity => (0..len as u32).collect::<Vec<u32>>().into(),
            Rebuild::Rows {
                offsets,
                node,
                source,
            } => {
                let sources = source.sources();
                assert_eq!(
                    sources.len(),
                    len,
                    "the edge rows read back are not the rows the index was built from"
                );
                rows_by_key(offsets, &sources, node.as_deref()).into()
            }
            Rebuild::Transpose {
                out_offsets,
                out_targets,
                out_ids,
                n_nodes,
            } => out_ids
                .with(|ids| {
                    crate::topology::transpose_parts(out_offsets, out_targets, ids, *n_nodes).2
                })
                .into(),
        }
    }
}

impl Deref for EdgeIds {
    type Target = [u32];

    #[inline]
    fn deref(&self) -> &[u32] {
        self.get()
    }
}

impl PartialEq for EdgeIds {
    fn eq(&self, other: &EdgeIds) -> bool {
        self.len() == other.len() && self.with(|a| other.with(|b| a == b))
    }
}

impl PartialEq<Vec<u32>> for EdgeIds {
    fn eq(&self, other: &Vec<u32>) -> bool {
        self.len() == other.len() && self.with(|a| a == &other[..])
    }
}

impl PartialEq<ScalarBuffer<u32>> for EdgeIds {
    fn eq(&self, other: &ScalarBuffer<u32>) -> bool {
        self.len() == other.len() && self.with(|a| a == &other[..])
    }
}

impl fmt::Debug for EdgeIds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let how = match &self.0.rebuild {
            Rebuild::Built => "built",
            Rebuild::Identity => "identity",
            Rebuild::Rows { .. } => "from rows",
            Rebuild::Transpose { .. } => "transpose",
        };
        write!(
            f,
            "EdgeIds({} slots, {how}, {})",
            self.0.len,
            if self.is_built() { "held" } else { "not held" }
        )
    }
}

impl From<ScalarBuffer<u32>> for EdgeIds {
    fn from(ids: ScalarBuffer<u32>) -> EdgeIds {
        EdgeIds::built(ids)
    }
}

impl From<Vec<u32>> for EdgeIds {
    fn from(ids: Vec<u32>) -> EdgeIds {
        EdgeIds::built(ids.into())
    }
}

/// The rows grouped by `keys[row]` (mapped through `node` when given) into
/// the segments `offsets` gives, each segment's rows ascending: the edge ids
/// of the counting sort that built the adjacency. Parallel above a size
/// threshold, with the same result.
pub(crate) fn rows_by_key(offsets: &[u64], keys: &[u32], node: Option<&[u32]>) -> Vec<u32> {
    use crate::parallel::*;
    let n = offsets.len() - 1;
    let m = keys.len();
    let key = |k: u32| node.map_or(k, |node| node[k as usize]) as usize;
    let mut ids = vec![0u32; m];
    const PARALLEL_MIN_EDGES: usize = 1 << 16;
    let n_chunks = current_num_threads().min((m / n.max(1)).max(4));
    if m < PARALLEL_MIN_EDGES || n_chunks <= 1 || n == 0 || in_worker() {
        let mut cursor: Vec<u64> = offsets[..n].to_vec();
        for (row, &k) in keys.iter().enumerate() {
            let c = &mut cursor[key(k)];
            ids[*c as usize] = row as u32;
            *c += 1;
        }
        return ids;
    }
    // As `Adjacency::build_parallel`: a histogram per contiguous chunk of
    // rows, turned into that chunk's cursor into every segment, then a
    // disjoint scatter.
    let chunk = m.div_ceil(n_chunks);
    let mut hists: Vec<Vec<u32>> = keys
        .par_chunks(chunk)
        .map(|keys| {
            let mut h = vec![0u32; n];
            for &k in keys {
                h[key(k)] += 1;
            }
            h
        })
        .collect();
    const NODES: usize = 1 << 16;
    let mut running: Vec<u32> = offsets[..n].par_iter().map(|&o| o as u32).collect();
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
    drop(running);
    let addr = ids.as_mut_ptr() as usize;
    hists.into_par_iter().enumerate().for_each(|(c, mut base)| {
        let ptr = addr as *mut u32;
        let (lo, hi) = (c * chunk, ((c + 1) * chunk).min(m));
        for (row, &k) in keys.iter().enumerate().take(hi).skip(lo) {
            let slot = &mut base[key(k)];
            // SAFETY: the cursors partition every segment among the chunks,
            // so each position is written once, and it is below `m`.
            unsafe { *ptr.add(*slot as usize) = row as u32 };
            *slot += 1;
        }
    });
    ids
}
