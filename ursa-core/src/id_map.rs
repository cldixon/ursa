//! User-id ⇄ dense-index mapping.
//!
//! User node ids are arbitrary (gappy `i64`, strings, UUIDs). Kernels never see
//! them: index construction builds a bidirectional mapping from user ids to dense
//! `u32` internal indices `0..n`, and all kernels operate in `u32` space over flat
//! arrays. Results translate back to user ids only when materializing output.
//!
//! Two user-id types are supported, discriminated by the Arrow column type at
//! build time and kept as the [`IdMap`] enum's two variants: **`Int64`** (the
//! zero-overhead common case) and **`Utf8`** (strings, which also cover
//! UUID-as-string). The internal dense index is always `u32`. The id type is
//! discovered once, when the topology is built, and every boundary that touches
//! user ids goes through this module's Arrow-array methods, so nothing downstream
//! needs to know which variant it is.

use std::sync::{Arc, OnceLock};

use arrow::array::{Array, ArrayRef, Int64Array, LargeStringArray, StringArray, StringBuilder};
use arrow::datatypes::DataType;
use rustc_hash::FxHashMap;

/// Incremental edge interning: feed `(src, dst)` chunks one at a time with
/// [`EdgeInterner::push`], then [`EdgeInterner::finish`] for the id map and the
/// dense endpoints. This is what lets a scan intern each batch as it is decoded and
/// free it straight away, so the Arrow input never accumulates (#149).
/// [`IdMap::from_edge_batches`] is the same loop over an in-memory list.
///
/// The id type is fixed by the first chunk; later chunks must match it
/// ([`IdError::MixedTypes`]). Dense ids follow first-seen order across all chunks.
/// Finishing with no chunks yields an empty `Int64` map.
#[derive(Debug)]
pub struct EdgeInterner {
    map: Option<IdMap>,
    src_dense: Vec<u32>,
    dst_dense: Vec<u32>,
}

impl EdgeInterner {
    /// An empty interner whose dense endpoint vectors are sized for `n_edges`. The
    /// count is only a hint: more edges grow the vectors, fewer leave the tail
    /// unused.
    pub fn with_capacity(n_edges: usize) -> Self {
        EdgeInterner {
            map: None,
            src_dense: Vec::with_capacity(n_edges),
            dst_dense: Vec::with_capacity(n_edges),
        }
    }

    /// Edges interned so far.
    pub fn len(&self) -> usize {
        self.src_dense.len()
    }

    /// Whether no edge has been interned yet.
    pub fn is_empty(&self) -> bool {
        self.src_dense.is_empty()
    }

    /// Intern one chunk of endpoints, appending its edges in row order. Errors on
    /// a null endpoint, unequal lengths, mixed src/dst types, a type that differs
    /// from earlier chunks, or an unsupported id type.
    pub fn push(&mut self, src: &dyn Array, dst: &dyn Array) -> Result<(), IdError> {
        if src.len() != dst.len() {
            return Err(IdError::LengthMismatch);
        }
        let kind = match (id_kind(src.data_type())?, id_kind(dst.data_type())?) {
            (IdKind::Int64, IdKind::Int64) => IdKind::Int64,
            (IdKind::Utf8, IdKind::Utf8) => IdKind::Utf8,
            _ => return Err(IdError::MixedTypes),
        };
        let map = self.map.get_or_insert_with(|| match kind {
            IdKind::Int64 => IdMap::new_int64(),
            IdKind::Utf8 => IdMap::new_utf8(),
        });
        let (sd, dd) = (&mut self.src_dense, &mut self.dst_dense);
        match (kind, &*map) {
            (IdKind::Int64, IdMap::Int64 { .. }) => {
                let (s, d) = (as_int64(src), as_int64(dst));
                // Fast path: when neither column has nulls (the common case,
                // e.g. every scanned/canonicalized edge list), intern straight
                // over the raw `&[i64]` value slices — skipping the per-row
                // `is_null`/`value` bounds+validity checks that dominate the
                // loop once the hasher is fast.
                if s.null_count() == 0 && d.null_count() == 0 {
                    let (sv, dv) = (s.values().as_ref(), d.values().as_ref());
                    if sv.len() >= PARALLEL_MIN_ROWS
                        && crate::parallel::current_num_threads() > 1
                        && !crate::parallel::in_worker()
                    {
                        intern_i64_parallel(map, sv, dv, sd, dd)?;
                    } else {
                        for (&s, &d) in sv.iter().zip(dv) {
                            sd.push(map.intern_i64(s)?);
                            dd.push(map.intern_i64(d)?);
                        }
                    }
                } else {
                    for i in 0..s.len() {
                        if s.is_null(i) || d.is_null(i) {
                            return Err(IdError::Null);
                        }
                        sd.push(map.intern_i64(s.value(i))?);
                        dd.push(map.intern_i64(d.value(i))?);
                    }
                }
            }
            (IdKind::Utf8, IdMap::Utf8 { .. }) => {
                let (s, d) = (StrView::new(src), StrView::new(dst));
                for i in 0..s.len() {
                    if s.is_null(i) || d.is_null(i) {
                        return Err(IdError::Null);
                    }
                    sd.push(map.intern_str(s.value(i))?);
                    dd.push(map.intern_str(d.value(i))?);
                }
            }
            // This chunk's id kind differs from the map's (fixed by the first chunk).
            _ => return Err(IdError::MixedTypes),
        }
        Ok(())
    }

    /// The id map and the dense `(src, dst)` endpoints, ready for
    /// [`Topology::build`](crate::topology::Topology::build).
    pub fn finish(self) -> (IdMap, Vec<u32>, Vec<u32>) {
        let map = self.map.unwrap_or_else(IdMap::new_int64);
        (map, self.src_dense, self.dst_dense)
    }
}

/// A `StringArray` of `strings[i]` for each index, sized exactly up front (one
/// pass for the byte total, one to copy), rather than collecting a `Vec<&str>` and
/// letting the value buffer grow by doubling.
fn gather_strings(
    strings: &[Arc<str>],
    indices: impl Iterator<Item = usize> + Clone,
) -> StringArray {
    let bytes: usize = indices.clone().map(|i| strings[i].len()).sum();
    let mut builder = StringBuilder::with_capacity(indices.size_hint().0, bytes);
    for i in indices {
        builder.append_value(&strings[i]);
    }
    builder.finish()
}

/// Chunks at least this long intern their known ids in parallel (see
/// [`intern_i64_parallel`]); shorter ones are not worth the fork.
const PARALLEL_MIN_ROWS: usize = 1 << 15;

/// Marks an endpoint the parallel lookup did not find. Dense ids stay below
/// `u32::MAX` (the interner errors first), so it never collides with one.
const MISS: u32 = u32::MAX;

/// Intern one null-free `Int64` chunk, giving exactly the ids the serial loop
/// would. Ids already in the map are looked up in parallel against the read-only
/// map; that lookup is most of the work once a graph's nodes have been seen. One
/// serial pass then interns the misses in row order (src before dst, row by row,
/// as the serial loop does). An id already in the map keeps its index whatever
/// order it is looked up in, and only the serial pass adds ids, so new ids are
/// numbered in first-seen order.
fn intern_i64_parallel(
    map: &mut IdMap,
    sv: &[i64],
    dv: &[i64],
    sd: &mut Vec<u32>,
    dd: &mut Vec<u32>,
) -> Result<(), IdError> {
    use crate::parallel::*;
    const CHUNK: usize = 4096;
    let base = sd.len();
    sd.resize(base + sv.len(), MISS);
    dd.resize(base + dv.len(), MISS);
    {
        let IdMap::Int64 { to_dense, .. } = &*map else {
            unreachable!("intern_i64_parallel on a Utf8 IdMap")
        };
        let lookup = |out: &mut [u32], ids: &[i64]| {
            for (o, id) in out.iter_mut().zip(ids) {
                if let Some(&dense) = to_dense.get(id) {
                    *o = dense;
                }
            }
        };
        sd[base..]
            .par_chunks_mut(CHUNK)
            .zip(sv.par_chunks(CHUNK))
            .for_each(|(out, ids)| lookup(out, ids));
        dd[base..]
            .par_chunks_mut(CHUNK)
            .zip(dv.par_chunks(CHUNK))
            .for_each(|(out, ids)| lookup(out, ids));
    }
    for (i, (&s, &d)) in sv.iter().zip(dv).enumerate() {
        if sd[base + i] == MISS {
            sd[base + i] = map.intern_i64(s)?;
        }
        if dd[base + i] == MISS {
            dd[base + i] = map.intern_i64(d)?;
        }
    }
    Ok(())
}

/// Bidirectional map between arbitrary user ids and dense `u32` indices.
///
/// `u32` caps the node space at ~4.29B nodes — the correct trade for cache
/// behaviour at the v0.1 target scale. A `u64` node space is a future feature flag.
///
/// String ids are stored as `Arc<str>` shared between the `to_user` vector and the
/// `to_dense` map, so each distinct id is heap-allocated **once** (not twice, as a
/// `Vec<String>` + `HashMap<String, _>` would). The user-id Arrow array is built
/// lazily and cached (`id_array`): it is immutable once built, so the per-collect
/// `user_id_array()` call is an `Arc` clone instead of a full deep copy of every id.
#[derive(Debug, Clone)]
pub enum IdMap {
    /// Integer user ids (the fast path).
    Int64 {
        to_user: Vec<i64>,
        to_dense: FxHashMap<i64, u32>,
        id_array: OnceLock<ArrayRef>,
    },
    /// String user ids (also covers UUID-as-string).
    Utf8 {
        to_user: Vec<Arc<str>>,
        to_dense: FxHashMap<Arc<str>, u32>,
        id_array: OnceLock<ArrayRef>,
    },
}

/// Errors raised while building an [`IdMap`] from Arrow edge columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdError {
    /// A `src`/`dst` value was null. The spec's lean is *error by default*, with an
    /// `on_null="drop"` opt-in resolved at a higher layer.
    Null,
    /// `src` and `dst` had different id types (e.g. one string, one integer).
    MixedTypes,
    /// The id column's Arrow type is not a supported node-id type.
    Unsupported(String),
    /// `src` and `dst` columns had different lengths.
    LengthMismatch,
    /// More than `u32::MAX` distinct nodes — beyond the `u32` dense-index cap (a
    /// `u64` node space is a future feature flag). Better a clear error than the
    /// silent index wraparound it would otherwise cause.
    TooManyNodes,
}

impl std::fmt::Display for IdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdError::Null => write!(
                f,
                "null value in src/dst column; set on_null=\"drop\" to skip"
            ),
            IdError::MixedTypes => write!(
                f,
                "src and dst node-id columns must have the same type (both int64 or both string)"
            ),
            IdError::Unsupported(t) => write!(
                f,
                "unsupported node-id type {t}; node ids must be int64 or string (Utf8)"
            ),
            IdError::LengthMismatch => {
                write!(f, "src and dst columns must have the same length")
            }
            IdError::TooManyNodes => write!(
                f,
                "more than u32::MAX (~4.29B) distinct nodes; exceeds the dense-index cap"
            ),
        }
    }
}

impl std::error::Error for IdError {}

/// Which supported family an Arrow id column belongs to.
enum IdKind {
    Int64,
    Utf8,
}

fn id_kind(dt: &DataType) -> Result<IdKind, IdError> {
    match dt {
        DataType::Int64 => Ok(IdKind::Int64),
        DataType::Utf8 | DataType::LargeUtf8 => Ok(IdKind::Utf8),
        other => Err(IdError::Unsupported(format!("{other:?}"))),
    }
}

/// Downcast an id array to `Int64Array`, canonicalizing nothing (the caller has
/// already confirmed the kind).
fn as_int64(arr: &dyn Array) -> &Int64Array {
    arr.as_any()
        .downcast_ref::<Int64Array>()
        .expect("id column confirmed Int64")
}

/// A borrowing view over a `Utf8` or `LargeUtf8` id array, reading either offset
/// width natively. This replaces a `LargeUtf8 -> Utf8` cast, which (a) **fails**
/// once the concatenated string data exceeds `i32::MAX` bytes — exactly what
/// engines emit for large string columns, so a valid big id column would panic
/// across PyO3 — and (b) copies the whole column before interning even on success.
enum StrView<'a> {
    Utf8(&'a StringArray),
    Large(&'a LargeStringArray),
}

impl<'a> StrView<'a> {
    /// The caller has already confirmed the array is `Utf8`/`LargeUtf8` (`id_kind`).
    fn new(arr: &'a dyn Array) -> Self {
        if let Some(s) = arr.as_any().downcast_ref::<StringArray>() {
            StrView::Utf8(s)
        } else {
            StrView::Large(
                arr.as_any()
                    .downcast_ref::<LargeStringArray>()
                    .expect("id kind confirmed Utf8/LargeUtf8"),
            )
        }
    }
    fn len(&self) -> usize {
        match self {
            StrView::Utf8(a) => a.len(),
            StrView::Large(a) => a.len(),
        }
    }
    fn is_null(&self, i: usize) -> bool {
        match self {
            StrView::Utf8(a) => a.is_null(i),
            StrView::Large(a) => a.is_null(i),
        }
    }
    fn value(&self, i: usize) -> &str {
        match self {
            StrView::Utf8(a) => a.value(i),
            StrView::Large(a) => a.value(i),
        }
    }
}

impl IdMap {
    fn new_int64() -> Self {
        IdMap::Int64 {
            to_user: Vec::new(),
            to_dense: FxHashMap::default(),
            id_array: OnceLock::new(),
        }
    }

    fn new_utf8() -> Self {
        IdMap::Utf8 {
            to_user: Vec::new(),
            to_dense: FxHashMap::default(),
            id_array: OnceLock::new(),
        }
    }

    /// Build an integer map from an iterator of user ids, first-seen order. (Test
    /// / convenience helper for the `Int64` fast path.)
    pub fn from_ids<I: IntoIterator<Item = i64>>(ids: I) -> Self {
        let mut map = IdMap::new_int64();
        for id in ids {
            map.intern_i64(id)
                .expect("from_ids is a small-input convenience; node count is within the u32 cap");
        }
        map
    }

    /// Build the id map from the two edge endpoint columns, and return the edges
    /// re-expressed in dense `u32` space (`src_dense`, `dst_dense`) ready for
    /// [`Topology::build`]. The id type is taken from the Arrow columns: `Int64`
    /// (and other integer types are not accepted here — callers canonicalize to
    /// `Int64` first) or `Utf8`/`LargeUtf8`. Errors on a null endpoint, mixed
    /// src/dst types, or an unsupported id type.
    ///
    /// A thin wrapper over [`Self::from_edge_batches`] for the single-chunk case.
    ///
    /// [`Topology::build`]: crate::topology::Topology::build
    pub fn from_edge_arrays(
        src: &dyn Array,
        dst: &dyn Array,
    ) -> Result<(Self, Vec<u32>, Vec<u32>), IdError> {
        if src.len() != dst.len() {
            return Err(IdError::LengthMismatch);
        }
        Self::from_edge_batches(&[(src, dst)])
    }

    /// Build the id map from a **stream of `(src, dst)` chunk pairs**, interning
    /// incrementally across all chunks, and return the edges re-expressed in dense
    /// `u32` space. This is the streaming ingress path: a scan's collected batches
    /// (or an in-memory table's chunks) are consumed one at a time, so the caller
    /// never has to `concat_batches` the whole edge table into one contiguous batch
    /// first — halving the transient ingest memory at the 500M-edge target.
    ///
    /// The id type is taken from the first chunk pair and must be consistent across
    /// all chunks (they come from one typed column pair, so they are — a mismatch is
    /// a defensive [`IdError::MixedTypes`]). An empty chunk list yields an empty
    /// `Int64` map (callers guard against a truly empty edge set upstream). Errors
    /// on a null endpoint, mixed src/dst types, unequal chunk lengths, or an
    /// unsupported id type — exactly like [`Self::from_edge_arrays`].
    pub fn from_edge_batches(
        chunks: &[(&dyn Array, &dyn Array)],
    ) -> Result<(Self, Vec<u32>, Vec<u32>), IdError> {
        let total = chunks.iter().map(|(s, _)| s.len()).sum();
        let mut interner = EdgeInterner::with_capacity(total);
        for (src, dst) in chunks {
            interner.push(*src, *dst)?;
        }
        Ok(interner.finish())
    }

    fn intern_i64(&mut self, user: i64) -> Result<u32, IdError> {
        use std::collections::hash_map::Entry;
        match self {
            IdMap::Int64 {
                to_user, to_dense, ..
            } => {
                // One hash probe via `entry` (vs a `get` miss followed by an
                // `insert` re-hash): interning is a hot inner loop, and every new
                // node is a miss, so halving the miss-path hashing matters.
                let next = to_user.len();
                match to_dense.entry(user) {
                    Entry::Occupied(e) => Ok(*e.get()),
                    Entry::Vacant(e) => {
                        if next >= u32::MAX as usize {
                            return Err(IdError::TooManyNodes);
                        }
                        let idx = next as u32;
                        e.insert(idx);
                        to_user.push(user);
                        Ok(idx)
                    }
                }
            }
            IdMap::Utf8 { .. } => unreachable!("intern_i64 on a Utf8 IdMap"),
        }
    }

    fn intern_str(&mut self, user: &str) -> Result<u32, IdError> {
        match self {
            IdMap::Utf8 {
                to_user, to_dense, ..
            } => {
                if let Some(&idx) = to_dense.get(user) {
                    return Ok(idx);
                }
                if to_user.len() >= u32::MAX as usize {
                    return Err(IdError::TooManyNodes);
                }
                let idx = to_user.len() as u32;
                // One heap allocation for the id, shared (Arc) between both maps.
                let key: Arc<str> = Arc::from(user);
                to_user.push(Arc::clone(&key));
                to_dense.insert(key, idx);
                Ok(idx)
            }
            IdMap::Int64 { .. } => unreachable!("intern_str on an Int64 IdMap"),
        }
    }

    /// The Arrow type of the user id column: `Int64` or `Utf8`. Schemas for
    /// result batches are built from this so the id/src/dst/node columns carry the
    /// original id type.
    pub fn user_type(&self) -> DataType {
        match self {
            IdMap::Int64 { .. } => DataType::Int64,
            IdMap::Utf8 { .. } => DataType::Utf8,
        }
    }

    /// All user ids in dense-index order, as an Arrow array (`Int64Array` |
    /// `StringArray`) — the id column of a per-node result.
    ///
    /// Built once and cached: the id set is immutable after construction, so
    /// repeated collects over one frame return an `Arc` clone rather than deep-
    /// copying every id (for string ids, every byte) into a fresh array each time.
    pub fn user_id_array(&self) -> ArrayRef {
        let cache = match self {
            IdMap::Int64 { id_array, .. } => id_array,
            IdMap::Utf8 { id_array, .. } => id_array,
        };
        cache.get_or_init(|| self.build_user_id_array()).clone()
    }

    /// Materialize the user-id column afresh (the cache-miss path of
    /// [`Self::user_id_array`]).
    fn build_user_id_array(&self) -> ArrayRef {
        match self {
            IdMap::Int64 { to_user, .. } => Arc::new(Int64Array::from(to_user.clone())),
            IdMap::Utf8 { to_user, .. } => Arc::new(gather_strings(to_user, 0..to_user.len())),
        }
    }

    /// User ids for a list of dense indices, as an Arrow array (`Int64Array` |
    /// `StringArray`) — for hop/path/walk output id columns. Panics on an
    /// out-of-range index (kernels never produce one).
    pub fn gather_user(&self, dense: &[u32]) -> ArrayRef {
        match self {
            IdMap::Int64 { to_user, .. } => Arc::new(Int64Array::from(
                dense
                    .iter()
                    .map(|&d| to_user[d as usize])
                    .collect::<Vec<_>>(),
            )),
            IdMap::Utf8 { to_user, .. } => {
                Arc::new(gather_strings(to_user, dense.iter().map(|&d| d as usize)))
            }
        }
    }

    /// Resolve a user-id array to dense indices (`None` for ids not in the map).
    /// The array's type must match the map's id type; a mismatch is an
    /// [`IdError::MixedTypes`]. Nulls resolve to `None`. Used for traversal
    /// seeds/source/target/starts and the node-attribute join key.
    pub fn dense_from_array(&self, arr: &dyn Array) -> Result<Vec<Option<u32>>, IdError> {
        // An empty id array resolves to no indices regardless of type — so empty
        // seeds work against a graph of either id type without a spurious mismatch.
        if arr.is_empty() {
            return Ok(Vec::new());
        }
        match self {
            IdMap::Int64 { to_dense, .. } => {
                if !matches!(id_kind(arr.data_type())?, IdKind::Int64) {
                    return Err(IdError::MixedTypes);
                }
                let a = as_int64(arr);
                Ok((0..a.len())
                    .map(|i| {
                        if a.is_null(i) {
                            None
                        } else {
                            to_dense.get(&a.value(i)).copied()
                        }
                    })
                    .collect())
            }
            IdMap::Utf8 { to_dense, .. } => {
                if !matches!(id_kind(arr.data_type())?, IdKind::Utf8) {
                    return Err(IdError::MixedTypes);
                }
                let a = StrView::new(arr);
                Ok((0..a.len())
                    .map(|i| {
                        if a.is_null(i) {
                            None
                        } else {
                            to_dense.get(a.value(i)).copied()
                        }
                    })
                    .collect())
            }
        }
    }

    /// Number of distinct nodes.
    pub fn len(&self) -> usize {
        match self {
            IdMap::Int64 { to_user, .. } => to_user.len(),
            IdMap::Utf8 { to_user, .. } => to_user.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_int_ids_in_first_seen_order() {
        let m = IdMap::from_ids([100, 7, 100, 42, 7]);
        assert_eq!(m.len(), 3);
        assert_eq!(m.user_type(), DataType::Int64);
        let ids = m.user_id_array();
        let ids = ids.as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(ids.values(), &[100, 7, 42]);
    }

    #[test]
    fn builds_int_map_from_edge_arrays() {
        let src = Int64Array::from(vec![10, 10, 20, 30]);
        let dst = Int64Array::from(vec![20, 30, 30, 10]);
        let (map, sd, dd) = IdMap::from_edge_arrays(&src, &dst).unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(sd, vec![0, 0, 1, 2]);
        assert_eq!(dd, vec![1, 2, 2, 0]);
    }

    #[test]
    fn builds_string_map_and_gathers_back() {
        // a->b, a->c, b->c, c->a over string ids
        let src = StringArray::from(vec!["a", "a", "b", "c"]);
        let dst = StringArray::from(vec!["b", "c", "c", "a"]);
        let (map, sd, dd) = IdMap::from_edge_arrays(&src, &dst).unwrap();
        assert_eq!(map.user_type(), DataType::Utf8);
        assert_eq!(map.len(), 3);
        assert_eq!(sd, vec![0, 0, 1, 2]);
        assert_eq!(dd, vec![1, 2, 2, 0]);

        // dense -> user
        let ids = map.user_id_array();
        let ids = ids.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(ids.value(0), "a");
        assert_eq!(ids.value(2), "c");

        // user -> dense (bulk), unknown -> None, null -> None
        let query = StringArray::from(vec![Some("c"), Some("zzz"), None]);
        let dense = map.dense_from_array(&query).unwrap();
        assert_eq!(dense, vec![Some(2), None, None]);

        // gather a subset back to user ids
        let gathered = map.gather_user(&[2, 0]);
        let gathered = gathered.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(gathered.value(0), "c");
        assert_eq!(gathered.value(1), "a");
    }

    #[test]
    fn large_utf8_is_handled_natively_and_round_trips() {
        // LargeUtf8 is read directly (no LargeUtf8->Utf8 cast, which would panic
        // past the 2 GiB Utf8 offset limit). Values round-trip through the map.
        use arrow::array::LargeStringArray;
        let src = LargeStringArray::from(vec!["x", "x", "y"]);
        let dst = LargeStringArray::from(vec!["y", "z", "z"]);
        let (map, sd, dd) = IdMap::from_edge_arrays(&src, &dst).unwrap();
        assert_eq!(map.user_type(), DataType::Utf8);
        assert_eq!(map.len(), 3);
        assert_eq!(sd, vec![0, 0, 1]);
        assert_eq!(dd, vec![1, 2, 2]);
        // dense -> user, and a LargeUtf8 lookup array resolves too
        let ids = map.user_id_array();
        let ids = ids.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!((ids.value(0), ids.value(1), ids.value(2)), ("x", "y", "z"));
        let query = LargeStringArray::from(vec![Some("z"), Some("nope")]);
        assert_eq!(map.dense_from_array(&query).unwrap(), vec![Some(2), None]);
    }

    #[test]
    fn from_edge_batches_interns_across_chunks() {
        // The same 0->1,0->2,1->2,2->0 graph split across two chunks must intern
        // identically to the single-array path — ids are global, not per-chunk.
        let s0 = Int64Array::from(vec![10, 10]);
        let d0 = Int64Array::from(vec![20, 30]);
        let s1 = Int64Array::from(vec![20, 30]);
        let d1 = Int64Array::from(vec![30, 10]);
        let (map, sd, dd) = IdMap::from_edge_batches(&[
            (&s0 as &dyn Array, &d0 as &dyn Array),
            (&s1 as &dyn Array, &d1 as &dyn Array),
        ])
        .unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(sd, vec![0, 0, 1, 2]);
        assert_eq!(dd, vec![1, 2, 2, 0]);

        // Identical to the single contiguous build.
        let src = Int64Array::from(vec![10, 10, 20, 30]);
        let dst = Int64Array::from(vec![20, 30, 30, 10]);
        let (_, sd1, dd1) = IdMap::from_edge_arrays(&src, &dst).unwrap();
        assert_eq!((sd, dd), (sd1, dd1));
    }

    #[test]
    fn from_edge_batches_strings_across_chunks() {
        let s0 = StringArray::from(vec!["a", "a"]);
        let d0 = StringArray::from(vec!["b", "c"]);
        let s1 = StringArray::from(vec!["b", "c"]);
        let d1 = StringArray::from(vec!["c", "a"]);
        let (map, sd, dd) = IdMap::from_edge_batches(&[
            (&s0 as &dyn Array, &d0 as &dyn Array),
            (&s1 as &dyn Array, &d1 as &dyn Array),
        ])
        .unwrap();
        assert_eq!(map.user_type(), DataType::Utf8);
        assert_eq!(sd, vec![0, 0, 1, 2]);
        assert_eq!(dd, vec![1, 2, 2, 0]);
    }

    #[test]
    fn from_empty_edge_batches_is_an_empty_map() {
        let (map, sd, dd) = IdMap::from_edge_batches(&[]).unwrap();
        assert!(map.is_empty());
        assert!(sd.is_empty() && dd.is_empty());
    }

    #[test]
    fn large_chunks_intern_exactly_like_small_ones() {
        // One chunk above the parallel threshold vs the same rows in chunks below
        // it: the parallel lookup + serial miss pass must give identical ids. The
        // ids mix repeats (known by the second chunk) with fresh ones throughout.
        let n = 3 * PARALLEL_MIN_ROWS + 123;
        let src: Vec<i64> = (0..n as i64).map(|i| (i * 7919) % 50_000).collect();
        let dst: Vec<i64> = (0..n as i64).map(|i| (i * 104_729) % 90_001 + 7).collect();
        let (s_all, d_all) = (Int64Array::from(src.clone()), Int64Array::from(dst.clone()));
        let big = [
            (
                &s_all.slice(0, 100) as &dyn Array,
                &d_all.slice(0, 100) as &dyn Array,
            ),
            (
                &s_all.slice(100, n - 100) as &dyn Array,
                &d_all.slice(100, n - 100) as &dyn Array,
            ),
        ];
        let (bmap, bsd, bdd) = IdMap::from_edge_batches(&big).unwrap();

        let small: Vec<(Int64Array, Int64Array)> = (0..n)
            .step_by(1000)
            .map(|lo| {
                let len = 1000.min(n - lo);
                (s_all.slice(lo, len), d_all.slice(lo, len))
            })
            .collect();
        let pairs: Vec<(&dyn Array, &dyn Array)> = small
            .iter()
            .map(|(s, d)| (s as &dyn Array, d as &dyn Array))
            .collect();
        let (smap, ssd, sdd) = IdMap::from_edge_batches(&pairs).unwrap();
        assert_eq!((bsd, bdd), (ssd, sdd));
        assert_eq!(&bmap.user_id_array(), &smap.user_id_array());
    }

    #[test]
    fn interner_fed_chunk_by_chunk_matches_from_edge_batches() {
        let (s0, d0) = (
            Int64Array::from(vec![10, 20]),
            Int64Array::from(vec![20, 30]),
        );
        let (s1, d1) = (
            Int64Array::from(vec![30, 40]),
            Int64Array::from(vec![10, 20]),
        );
        let (bmap, bsd, bdd) = IdMap::from_edge_batches(&[
            (&s0 as &dyn Array, &d0 as &dyn Array),
            (&s1 as &dyn Array, &d1 as &dyn Array),
        ])
        .unwrap();
        // A zero hint must still work: the dense vectors grow as needed.
        let mut interner = EdgeInterner::with_capacity(0);
        interner.push(&s0, &d0).unwrap();
        interner.push(&s1, &d1).unwrap();
        assert_eq!(interner.len(), 4);
        let (map, sd, dd) = interner.finish();
        assert_eq!((sd, dd), (bsd, bdd));
        assert_eq!(map.len(), bmap.len());

        // A later chunk of a different id type is rejected, not silently mixed.
        let mut interner = EdgeInterner::with_capacity(0);
        interner.push(&s0, &d0).unwrap();
        let (ss, ds) = (StringArray::from(vec!["a"]), StringArray::from(vec!["b"]));
        assert_eq!(interner.push(&ss, &ds), Err(IdError::MixedTypes));
    }

    #[test]
    fn length_mismatch_is_a_typed_error() {
        let src = Int64Array::from(vec![1, 2, 3]);
        let dst = Int64Array::from(vec![1, 2]);
        assert_eq!(
            IdMap::from_edge_arrays(&src, &dst).err(),
            Some(IdError::LengthMismatch)
        );
    }

    #[test]
    fn rejects_null_mixed_and_unsupported() {
        // null endpoint
        let src = Int64Array::from(vec![Some(1), None]);
        let dst = Int64Array::from(vec![Some(2), Some(3)]);
        assert_eq!(
            IdMap::from_edge_arrays(&src, &dst).err(),
            Some(IdError::Null)
        );

        // mixed types
        let s = Int64Array::from(vec![1, 2]);
        let d = StringArray::from(vec!["a", "b"]);
        assert_eq!(
            IdMap::from_edge_arrays(&s, &d).err(),
            Some(IdError::MixedTypes)
        );

        // unsupported type (float)
        use arrow::array::Float64Array;
        let s = Float64Array::from(vec![1.0, 2.0]);
        let d = Float64Array::from(vec![2.0, 1.0]);
        assert!(matches!(
            IdMap::from_edge_arrays(&s, &d),
            Err(IdError::Unsupported(_))
        ));
    }

    #[test]
    fn dense_from_array_rejects_wrong_type() {
        let m = IdMap::from_ids([1, 2, 3]);
        let wrong = StringArray::from(vec!["1"]);
        assert_eq!(m.dense_from_array(&wrong), Err(IdError::MixedTypes));
    }
}
