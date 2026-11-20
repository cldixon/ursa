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

use arrow::array::{
    Array, ArrayRef, BooleanBufferBuilder, Int64Array, LargeStringArray, StringArray,
    StringBuilder, StringViewArray,
};
use arrow::buffer::{BooleanBuffer, Buffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::DataType;
use hashbrown::HashTable;
use rustc_hash::FxHashMap;
use std::hash::BuildHasher;

/// Incremental edge interning: feed `(src, dst)` chunks one at a time with
/// [`EdgeInterner::push`], then [`EdgeInterner::finish`] for the id map and the
/// dense endpoints. This is what lets a scan intern each batch as it is decoded and
/// free it straight away, so the Arrow input never accumulates (#149).
/// [`IdMap::from_edge_batches`] is the same loop over an in-memory list.
///
/// The id type is fixed by the first chunk; later chunks must match it
/// ([`IdError::MixedTypes`]). Dense ids follow first-seen order across all chunks.
/// Finishing with no chunks yields an empty `Int64` map.
///
/// **A fixed node set.** A graph whose nodes are given apart from its edges (the
/// SQL engine's vertex table) interns them first, with
/// [`EdgeInterner::push_nodes`], then fixes the set with
/// [`EdgeInterner::close_nodes`]. Its nodes then take the dense ids `0..n` in
/// the order given, nodes on no edge included, and an edge whose source or
/// destination is not a node (or is null) is dropped rather than interned, as
/// an inner join of the edges with the nodes would drop it.
/// [`EdgeInterner::n_dropped`] counts them, and [`EdgeInterner::kept_rows`]
/// says which rows were kept, for anything read per edge row alongside them
/// (weights).
///
/// **Edges before the node set is known.** So that edges can be interned
/// while the nodes are still being read (#385), a second interner made with
/// [`EdgeInterner::provisional`] interns them with a numbering of its own;
/// once the node set is fixed, [`EdgeInterner::adopt`] renumbers them into
/// it. The result is exactly that of pushing the same edges after
/// [`EdgeInterner::close_nodes`]: the same dense endpoints, the same dropped
/// edges and the same [`EdgeInterner::kept_rows`].
#[derive(Debug)]
pub struct EdgeInterner {
    map: Option<IdMap>,
    /// The `with_capacity` edge count.
    rows_hint: usize,
    src_dense: Vec<u32>,
    dst_dense: Vec<u32>,
    /// Whether the node set is fixed (see the type's docs).
    closed: bool,
    /// Edge rows pushed since the node set was fixed, dropped ones included.
    rows: usize,
    /// One bit per edge row pushed since the node set was fixed, set when the
    /// row was kept; started at the first dropped row.
    kept: Option<BooleanBufferBuilder>,
    dropped: u64,
    /// Interning edges before the node set is known: an edge with a null
    /// endpoint is kept as `(MISS, MISS)` rather than refused, for
    /// [`Self::adopt`] to drop (see the type's docs).
    provisional: bool,
}

impl EdgeInterner {
    /// An empty interner whose dense endpoint vectors are sized for `n_edges`. The
    /// count is only a hint: more edges grow the vectors, fewer leave the tail
    /// unused. It also sizes how wide a range of integer ids may stay in a direct
    /// table (see [`IntIndex`]).
    pub fn with_capacity(n_edges: usize) -> Self {
        EdgeInterner {
            map: None,
            rows_hint: n_edges,
            src_dense: Vec::with_capacity(n_edges),
            dst_dense: Vec::with_capacity(n_edges),
            closed: false,
            rows: 0,
            kept: None,
            dropped: 0,
            provisional: false,
        }
    }

    /// An interner for the edges of a graph whose node set is still being
    /// read: it interns them as [`Self::push`] does, numbered among
    /// themselves, except that an edge with a null endpoint is held for
    /// [`Self::adopt`] to drop rather than refused. Sized as by
    /// [`Self::with_capacity`].
    pub fn provisional(n_edges: usize) -> Self {
        EdgeInterner {
            provisional: true,
            ..Self::with_capacity(n_edges)
        }
    }

    /// Take in the edges `provisional` ([`Self::provisional`]) interned while
    /// this interner's nodes were being read, once [`Self::close_nodes`] has
    /// fixed them and before any edge is pushed here. The edges end up exactly
    /// as if they had been pushed here instead, in the same order: those whose
    /// endpoints are both nodes take the nodes' dense ids, the others are
    /// dropped, counted and marked in [`Self::kept_rows`]. Edges pushed here
    /// afterwards follow them.
    ///
    /// Each distinct endpoint is looked up once, then the edges are renumbered
    /// and compacted in place, so this holds no second copy of them.
    ///
    /// # Panics
    ///
    /// If the node set is not fixed, or edges were pushed here already.
    pub fn adopt(&mut self, provisional: EdgeInterner) -> Result<(), IdError> {
        use crate::parallel::*;
        assert!(
            self.closed && self.rows == 0 && self.src_dense.is_empty(),
            "adopt takes edges before any other, once the node set is fixed"
        );
        let EdgeInterner {
            map: edge_map,
            src_dense: mut src,
            dst_dense: mut dst,
            ..
        } = provisional;
        let Some(edge_map) = edge_map else {
            // No edge was pushed.
            return Ok(());
        };
        let map = self.map.get_or_insert_with(|| match &edge_map {
            IdMap::Int64 { .. } => IdMap::new_int64(),
            IdMap::Utf8 { .. } => IdMap::new_utf8(),
        });
        // The node each provisional id is, or MISS for an id that is no node.
        let renumber: Vec<u32> = match (&*map, &edge_map) {
            (IdMap::Int64 { to_dense, .. }, IdMap::Int64 { to_user, .. }) => to_user
                .values()
                .par_iter()
                .map(|&id| to_dense.get(id).unwrap_or(MISS))
                .collect(),
            (IdMap::Utf8 { ids, .. }, IdMap::Utf8 { ids: edge_ids, .. }) => (0..edge_ids.len()
                as u32)
                .into_par_iter()
                .map(|i| ids.get(edge_ids.value(i)).unwrap_or(MISS))
                .collect(),
            _ => return Err(IdError::MixedTypes),
        };
        drop(edge_map);
        const CHUNK: usize = 1 << 16;
        for ends in [&mut src, &mut dst] {
            ends.par_chunks_mut(CHUNK).for_each(|ends| {
                for e in ends {
                    if *e != MISS {
                        *e = renumber[*e as usize];
                    }
                }
            });
        }
        drop(renumber);
        // Keep the edges between nodes, in order, compacting in place.
        let len = src.len();
        let mut kept = 0;
        for i in 0..len {
            let keep = src[i] != MISS && dst[i] != MISS;
            if keep {
                (src[kept], dst[kept]) = (src[i], dst[i]);
                kept += 1;
            }
            self.count_row(i, len, keep);
        }
        src.truncate(kept);
        dst.truncate(kept);
        (self.src_dense, self.dst_dense) = (src, dst);
        self.rows = len;
        Ok(())
    }

    /// Count row `i` of a chunk of `len` rows, pushed since the node set was
    /// fixed, as kept or dropped.
    #[inline]
    fn count_row(&mut self, i: usize, len: usize, keep: bool) {
        if !keep {
            self.dropped += 1;
            if self.kept.is_none() {
                let mut kept = BooleanBufferBuilder::new(self.rows_hint.max(self.rows + len));
                kept.append_n(self.rows + i, true);
                self.kept = Some(kept);
            }
        }
        if let Some(kept) = &mut self.kept {
            kept.append(keep);
        }
    }

    /// Intern one chunk of node ids, in order, before any edge: each id not
    /// seen yet takes the next dense id. A null is not a node and is skipped;
    /// an id given twice is one node. The id type is fixed as by
    /// [`Self::push`], whose edges must then have the same type.
    pub fn push_nodes(&mut self, ids: &dyn Array) -> Result<(), IdError> {
        if self.closed || !self.src_dense.is_empty() {
            return Err(IdError::NodesAfterEdges);
        }
        let kind = id_kind(ids.data_type())?;
        let map = self.map.get_or_insert_with(|| kind.new_map());
        match (kind, &*map) {
            (IdKind::Int64, IdMap::Int64 { .. }) => {
                if let IdMap::Int64 { to_dense, .. } = map {
                    to_dense.budget_rows(self.rows_hint.max(ids.len()));
                }
                let a = as_int64(ids);
                if a.null_count() == 0 {
                    for &id in a.values().iter() {
                        map.intern_i64(id)?;
                    }
                } else {
                    for id in a.iter().flatten() {
                        map.intern_i64(id)?;
                    }
                }
            }
            (IdKind::Utf8, IdMap::Utf8 { .. }) => {
                let view = StrView::new(ids);
                for i in 0..view.len() {
                    if !view.is_null(i) {
                        map.intern_str(view.value(i))?;
                    }
                }
            }
            _ => return Err(IdError::MixedTypes),
        }
        Ok(())
    }

    /// Fix the node set: from now on [`Self::push`] drops an edge whose source
    /// or destination is not a node given to [`Self::push_nodes`] (see the
    /// type's docs).
    pub fn close_nodes(&mut self) {
        self.closed = true;
    }

    /// Edges dropped because an endpoint is not a node (see the type's docs).
    pub fn n_dropped(&self) -> u64 {
        self.dropped
    }

    /// Which of the edge rows pushed since [`Self::close_nodes`] were kept, one
    /// bit per row in order; `None` if every row was.
    pub fn kept_rows(&mut self) -> Option<BooleanBuffer> {
        self.kept.as_mut().map(|k| k.finish())
    }

    /// The edges of one chunk whose endpoints are both nodes, appended in row
    /// order; the others are dropped and counted. The map is only read, so the
    /// lookups run in parallel on a long chunk.
    fn push_closed(
        &mut self,
        src: &dyn Array,
        dst: &dyn Array,
        kind: IdKind,
    ) -> Result<(), IdError> {
        let map = self.map.get_or_insert_with(|| kind.new_map());
        match (kind, &*map) {
            (IdKind::Int64, IdMap::Int64 { .. }) | (IdKind::Utf8, IdMap::Utf8 { .. }) => {}
            _ => return Err(IdError::MixedTypes),
        }
        let len = src.len();
        let (mut s, mut d) = (vec![MISS; len], vec![MISS; len]);
        {
            let map = &*map;
            // The dense id of each row from `start` of `arr`; MISS for a null or
            // an id that is not a node.
            let lookup = |arr: &dyn Array, out: &mut [u32], start: usize| match map {
                IdMap::Int64 { to_dense, .. } => {
                    let a = as_int64(arr);
                    for (k, o) in out.iter_mut().enumerate() {
                        let i = start + k;
                        if a.is_valid(i) {
                            *o = to_dense.get(a.value(i)).unwrap_or(MISS);
                        }
                    }
                }
                IdMap::Utf8 { ids, .. } => {
                    let a = StrView::new(arr);
                    for (k, o) in out.iter_mut().enumerate() {
                        let i = start + k;
                        if !a.is_null(i) {
                            *o = ids.get(a.value(i)).unwrap_or(MISS);
                        }
                    }
                }
            };
            const CHUNK: usize = 4096;
            for (out, arr) in [(&mut s[..], src), (&mut d[..], dst)] {
                if intern_in_parallel(len) {
                    use crate::parallel::*;
                    out.par_chunks_mut(CHUNK)
                        .enumerate()
                        .for_each(|(c, out)| lookup(arr, out, c * CHUNK));
                } else {
                    lookup(arr, out, 0);
                }
            }
        }
        for i in 0..len {
            let keep = s[i] != MISS && d[i] != MISS;
            if keep {
                self.src_dense.push(s[i]);
                self.dst_dense.push(d[i]);
            }
            self.count_row(i, len, keep);
        }
        self.rows += len;
        Ok(())
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
        if self.closed {
            return self.push_closed(src, dst, kind);
        }
        let provisional = self.provisional;
        let map = self.map.get_or_insert_with(|| kind.new_map());
        let (sd, dd) = (&mut self.src_dense, &mut self.dst_dense);
        if let IdMap::Int64 { to_dense, .. } = map {
            to_dense.budget_rows(self.rows_hint.max(sd.len() + src.len()));
        }
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
                    if intern_in_parallel(sv.len()) {
                        let ids = |is_dst: bool| if is_dst { dv } else { sv };
                        intern_two_phase(
                            map,
                            sv.len(),
                            sd,
                            dd,
                            |map, is_dst, i| match map {
                                IdMap::Int64 { to_dense, .. } => to_dense.get(ids(is_dst)[i]),
                                IdMap::Utf8 { .. } => unreachable!("Int64 chunk, Utf8 map"),
                            },
                            |map, is_dst, i| map.intern_i64(ids(is_dst)[i]),
                        )?;
                    } else {
                        for (&s, &d) in sv.iter().zip(dv) {
                            sd.push(map.intern_i64(s)?);
                            dd.push(map.intern_i64(d)?);
                        }
                    }
                } else {
                    for i in 0..s.len() {
                        if s.is_null(i) || d.is_null(i) {
                            if provisional {
                                sd.push(MISS);
                                dd.push(MISS);
                                continue;
                            }
                            return Err(IdError::Null);
                        }
                        sd.push(map.intern_i64(s.value(i))?);
                        dd.push(map.intern_i64(d.value(i))?);
                    }
                }
            }
            (IdKind::Utf8, IdMap::Utf8 { .. }) => {
                let (s, d) = (StrView::new(src), StrView::new(dst));
                if src.null_count() == 0 && dst.null_count() == 0 && intern_in_parallel(s.len()) {
                    let ids = |is_dst: bool| if is_dst { &d } else { &s };
                    return intern_two_phase(
                        map,
                        s.len(),
                        sd,
                        dd,
                        |map, is_dst, i| match map {
                            IdMap::Utf8 { ids: map_ids, .. } => map_ids.get(ids(is_dst).value(i)),
                            IdMap::Int64 { .. } => unreachable!("Utf8 chunk, Int64 map"),
                        },
                        |map, is_dst, i| map.intern_str(ids(is_dst).value(i)),
                    );
                }
                for i in 0..s.len() {
                    if s.is_null(i) || d.is_null(i) {
                        if provisional {
                            sd.push(MISS);
                            dd.push(MISS);
                            continue;
                        }
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
        let mut map = self.map.unwrap_or_else(IdMap::new_int64);
        map.freeze();
        (map, self.src_dense, self.dst_dense)
    }
}

/// A `StringArray` of `value(i)` for each index, sized exactly up front (one pass
/// for the byte total, one to copy), rather than collecting a `Vec<&str>` and
/// letting the value buffer grow by doubling.
fn gather_strings<'a>(
    value: impl Fn(usize) -> &'a str,
    indices: impl Iterator<Item = usize> + Clone,
) -> StringArray {
    let bytes: usize = indices.clone().map(|i| value(i).len()).sum();
    let mut builder = StringBuilder::with_capacity(indices.size_hint().0, bytes);
    for i in indices {
        builder.append_value(value(i));
    }
    builder.finish()
}

/// The distinct integer ids in dense order. Grown as a `Vec` while interning;
/// [`IntIds::freeze`] then moves it, without a copy, into the `Int64Array` that
/// is the cached id column, so the ids are not held twice.
#[derive(Debug, Clone)]
pub struct IntIds(IntStore);

#[derive(Debug, Clone)]
enum IntStore {
    Building(Vec<i64>),
    Frozen(Int64Array),
}

impl IntIds {
    fn new() -> Self {
        IntIds(IntStore::Building(Vec::new()))
    }

    #[inline]
    fn values(&self) -> &[i64] {
        match &self.0 {
            IntStore::Building(v) => v,
            IntStore::Frozen(a) => a.values(),
        }
    }

    fn len(&self) -> usize {
        self.values().len()
    }

    fn push(&mut self, id: i64) {
        if let IntStore::Frozen(a) = &self.0 {
            // Interning after `freeze` (not done by any caller): copy back out.
            self.0 = IntStore::Building(a.values().to_vec());
        }
        let IntStore::Building(v) = &mut self.0 else {
            unreachable!("thawed above")
        };
        v.push(id);
    }

    /// Move the ids into an `Int64Array` (no copy) and return it.
    fn freeze(&mut self) -> Int64Array {
        if let IntStore::Building(v) = &mut self.0 {
            self.0 = IntStore::Frozen(Int64Array::from(std::mem::take(v)));
        }
        match &self.0 {
            IntStore::Frozen(a) => a.clone(),
            IntStore::Building(_) => unreachable!("frozen above"),
        }
    }

    /// The ids as an `Int64Array`: a cheap clone once frozen, a copy before.
    fn to_array(&self) -> Int64Array {
        match &self.0 {
            IntStore::Frozen(a) => a.clone(),
            IntStore::Building(v) => Int64Array::from(v.clone()),
        }
    }
}

/// The distinct string ids, stored once.
///
/// The ids live in two Arrow-shaped buffers, the UTF-8 bytes and `i32` offsets,
/// and a `HashTable` of dense indices finds an id by hashing the string it
/// points at. There is no per-id allocation and no key copy in the index: about
/// `len + 10` bytes per id, against roughly `len + 100` for an `Arc<str>` held by
/// both a vector and a `HashMap` plus a second copy in the cached id array. Once
/// interning is done, [`StrIds::freeze`] moves the buffers into a `StringArray`
/// without copying; that array then serves lookups and is the cached id column.
#[derive(Debug, Clone)]
pub struct StrIds {
    store: StrStore,
    table: StrTable,
}

/// How a [`StrIds`] finds an id.
#[derive(Debug, Clone)]
enum StrTable {
    /// Grown while ids are interned.
    Hash(HashTable<u32>),
    /// The table of an id column read back from a stored index
    /// ([`IdMap::from_stored_ids`]): given with it, or built when first used.
    Stored(StoredTable),
}

/// The hash `StrIds` indexes ids by.
#[inline]
fn hash_str(id: &str) -> u64 {
    rustc_hash::FxBuildHasher.hash_one(id)
}

#[derive(Debug, Clone)]
enum StrStore {
    /// Growing while ids are interned.
    Building { bytes: Vec<u8>, offsets: Vec<i32> },
    /// Frozen into the id column.
    Frozen(StringArray),
}

impl StrStore {
    #[inline]
    fn value(&self, i: u32) -> &str {
        match self {
            StrStore::Building { bytes, offsets } => {
                let (lo, hi) = (
                    offsets[i as usize] as usize,
                    offsets[i as usize + 1] as usize,
                );
                // SAFETY: `bytes` only ever receives whole `&str`s (see `push`), and
                // `offsets` marks their boundaries, so every slice is valid UTF-8.
                unsafe { std::str::from_utf8_unchecked(&bytes[lo..hi]) }
            }
            StrStore::Frozen(array) => array.value(i as usize),
        }
    }

    fn len(&self) -> usize {
        match self {
            StrStore::Building { offsets, .. } => offsets.len() - 1,
            StrStore::Frozen(array) => array.len(),
        }
    }
}

impl StrIds {
    fn new() -> Self {
        StrIds {
            store: StrStore::Building {
                bytes: Vec::new(),
                offsets: vec![0],
            },
            table: StrTable::Hash(HashTable::new()),
        }
    }

    /// The ids of a stored index: `array` is the id column and `table` its
    /// lookup table, if it was stored too.
    fn stored(array: StringArray, table: Option<ScalarBuffer<u32>>) -> Self {
        StrIds {
            store: StrStore::Frozen(array),
            table: StrTable::Stored(StoredTable::new(table)),
        }
    }

    fn len(&self) -> usize {
        self.store.len()
    }

    /// The stored lookup table of these ids, built if it is not there yet;
    /// `None` for ids that are found through a hash table grown in memory.
    fn stored_slots(&self) -> Option<Result<&ScalarBuffer<u32>, IdError>> {
        match &self.table {
            StrTable::Hash(_) => None,
            StrTable::Stored(table) => Some(table.slots(|| self.build_slots())),
        }
    }

    /// The lookup table of these ids, as a stored index holds it.
    fn build_slots(&self) -> Result<Vec<u32>, IdError> {
        build_slots(
            self.len(),
            |i| stored_hash_str(self.value(i as u32)),
            |a, b| self.value(a) == self.value(b),
        )
    }

    #[inline]
    fn value(&self, i: u32) -> &str {
        self.store.value(i)
    }

    /// The dense index of `id`, if interned.
    #[inline]
    fn get(&self, id: &str) -> Option<u32> {
        match &self.table {
            StrTable::Hash(table) => table
                .find(hash_str(id), |&i| self.store.value(i) == id)
                .copied(),
            StrTable::Stored(table) => {
                let slots = table.slots(|| self.build_slots()).ok()?;
                let n = self.store.len();
                probe(slots, stored_hash_str(id), |i| {
                    (i as usize) < n && self.store.value(i) == id
                })
            }
        }
    }

    /// The dense index of `id`, interning it if new. One hash either way.
    fn intern(&mut self, id: &str) -> Result<u32, IdError> {
        let hash = hash_str(id);
        if let StrTable::Stored(_) = &self.table {
            // Interning into a stored index's ids (not done by any caller):
            // move to a table that grows.
            let mut table = HashTable::with_capacity(self.len());
            for i in 0..self.len() as u32 {
                table.insert_unique(hash_str(self.value(i)), i, |&j| hash_str(self.value(j)));
            }
            self.table = StrTable::Hash(table);
        }
        let StrIds { store, table } = self;
        let StrTable::Hash(table) = table else {
            unreachable!("moved to a hash table above")
        };
        if let Some(&i) = table.find(hash, |&i| store.value(i) == id) {
            return Ok(i);
        }
        let next = store.len();
        if next >= u32::MAX as usize {
            return Err(IdError::TooManyNodes);
        }
        if let StrStore::Frozen(array) = store {
            // Interning after `freeze` (not done by any caller): copy back out.
            let (offsets, values, _) = array.clone().into_parts();
            *store = StrStore::Building {
                bytes: values.to_vec(),
                offsets: offsets.to_vec(),
            };
        }
        let StrStore::Building { bytes, offsets } = store else {
            unreachable!("thawed above")
        };
        let end = i32::try_from(bytes.len() + id.len()).map_err(|_| IdError::IdBytesOverflow)?;
        bytes.extend_from_slice(id.as_bytes());
        offsets.push(end);
        let i = next as u32;
        table.insert_unique(hash, i, |&j| hash_str(store.value(j)));
        Ok(i)
    }

    /// Move the buffers into a `StringArray` (no copy) and return it.
    fn freeze(&mut self) -> StringArray {
        if let StrStore::Building { bytes, offsets } = &mut self.store {
            let offsets = OffsetBuffer::new(ScalarBuffer::from(std::mem::take(offsets)));
            let values = Buffer::from_vec(std::mem::take(bytes));
            // SAFETY: the offsets start at 0, never decrease, end at `values.len()`
            // and fall on `&str` boundaries (see `intern`), and there are no nulls.
            let array = unsafe { StringArray::new_unchecked(offsets, values, None) };
            self.store = StrStore::Frozen(array);
        }
        match &self.store {
            StrStore::Frozen(array) => array.clone(),
            StrStore::Building { .. } => unreachable!("frozen above"),
        }
    }

    /// The ids as a `StringArray`: a cheap clone once frozen, a copy before.
    fn to_array(&self) -> StringArray {
        match &self.store {
            StrStore::Frozen(array) => array.clone(),
            StrStore::Building { .. } => gather_strings(|i| self.value(i as u32), 0..self.len()),
        }
    }
}

/// The integer id index stays a direct table while the range of ids seen fits
/// in this many slots per distinct id, or one slot per edge row, plus
/// [`DIRECT_MIN_SLOTS`]; past that it moves to a hash map (see [`IntIndex`]).
const DIRECT_SLOTS_PER_ID: u64 = 4;

/// The fixed allowance of the direct table: any id range up to this many slots
/// (4 MiB) stays direct whatever its density.
const DIRECT_MIN_SLOTS: u64 = 1 << 20;

/// Integer user id -> dense index.
///
/// Most integer ids are compact (`0..n`, or an offset range, as anonymised
/// exports and `from_numpy` inputs are), so the index starts as a direct table:
/// `slot[user - base]` holds the dense index, or `MISS` for an id not seen. A
/// lookup is one array read, against a hash probe that is a cache miss into a
/// table several times larger. The table grows geometrically, in either
/// direction, as ids outside it appear. Once the range of ids seen gets sparse,
/// it moves to a hash map for good: when it would need more than
/// `DIRECT_SLOTS_PER_ID` slots per distinct id and more than one per edge row
/// (plus `DIRECT_MIN_SLOTS`), or more than `u32::MAX` slots. The per-row
/// allowance keeps compact ids direct when they arrive in random order, when
/// few distinct ids are seen yet but each spans the whole range; at 4 bytes per
/// row the table stays below the dense endpoints the interner holds anyway.
/// Neither form decides the dense numbering, which is first-seen order either
/// way, so the output is identical.
#[derive(Debug, Clone)]
pub struct IntIndex {
    table: IntTable,
    /// Edge rows the table may also budget a slot each for (see `make_room`).
    rows: u64,
}

#[derive(Debug, Clone)]
enum IntTable {
    /// Ids `base..base + slot.len()`, of which `lo..=hi` have been seen (the
    /// rest is growth headroom). The table never reaches past `i64::MAX`.
    Direct {
        base: i64,
        slot: Vec<u32>,
        lo: i64,
        hi: i64,
    },
    Hash(FxHashMap<i64, u32>),
    /// The table of an id column read back from a stored index
    /// ([`IdMap::from_stored_ids`]), with the ids it finds: given with them,
    /// or built when first used.
    Stored {
        ids: Int64Array,
        table: StoredTable,
    },
}

impl IntIndex {
    fn new() -> Self {
        IntIndex {
            table: IntTable::Direct {
                base: 0,
                slot: Vec::new(),
                lo: 0,
                hi: 0,
            },
            rows: 0,
        }
    }

    /// The index of a stored index's ids: `ids` is the id column and `table`
    /// its lookup table, if it was stored too.
    fn stored(ids: Int64Array, table: Option<ScalarBuffer<u32>>) -> Self {
        IntIndex {
            table: IntTable::Stored {
                ids,
                table: StoredTable::new(table),
            },
            rows: 0,
        }
    }

    /// The stored lookup table of this index, built if it is not there yet;
    /// `None` for a direct table or a hash map grown in memory.
    fn stored_slots(&self) -> Option<Result<&ScalarBuffer<u32>, IdError>> {
        match &self.table {
            IntTable::Stored { ids, table } => Some(table.slots(|| build_int_slots(ids.values()))),
            _ => None,
        }
    }

    /// Allow the direct table one slot per row for `rows` edge rows.
    fn budget_rows(&mut self, rows: usize) {
        self.rows = self.rows.max(rows as u64);
    }

    /// The dense index of `user`, if interned.
    #[inline]
    fn get(&self, user: i64) -> Option<u32> {
        match &self.table {
            IntTable::Direct { base, slot, .. } => {
                // `user - base` wraps to a value of at least `slot.len()` for an
                // id below `base`, since the table ends at or before `i64::MAX`,
                // so one comparison covers both ends.
                let off = user.wrapping_sub(*base) as u64;
                match off < slot.len() as u64 {
                    true => Some(slot[off as usize]).filter(|&d| d != MISS),
                    false => None,
                }
            }
            IntTable::Hash(map) => map.get(&user).copied(),
            IntTable::Stored { ids, table } => {
                let values = ids.values();
                let slots = table.slots(|| build_int_slots(values)).ok()?;
                probe(slots, stored_hash_i64(user), |i| {
                    values.get(i as usize) == Some(&user)
                })
            }
        }
    }

    /// The dense index of `user`, interning it as `to_user`'s next id if new.
    #[inline]
    fn intern(&mut self, user: i64, to_user: &mut IntIds) -> Result<u32, IdError> {
        use std::collections::hash_map::Entry;
        let next = to_user.len();
        match &self.table {
            IntTable::Direct { base, slot, .. } => {
                if user.wrapping_sub(*base) as u64 >= slot.len() as u64 {
                    self.make_room(user, next as u64 + 1, to_user);
                }
            }
            IntTable::Hash(_) => {}
            // Interning into a stored index's ids (not done by any caller):
            // move to a map that grows.
            IntTable::Stored { .. } => self.table = IntTable::Hash(hash_map_of(to_user)),
        }
        match &mut self.table {
            IntTable::Direct { base, slot, lo, hi } => {
                let s = &mut slot[user.wrapping_sub(*base) as u64 as usize];
                if *s != MISS {
                    return Ok(*s);
                }
                if next >= u32::MAX as usize {
                    return Err(IdError::TooManyNodes);
                }
                *s = next as u32;
                (*lo, *hi) = ((*lo).min(user), (*hi).max(user));
                to_user.push(user);
                Ok(next as u32)
            }
            IntTable::Hash(to_dense) => {
                // One hash probe via `entry` (vs a `get` miss followed by an
                // `insert` re-hash): every new node is a miss.
                match to_dense.entry(user) {
                    Entry::Occupied(e) => Ok(*e.get()),
                    Entry::Vacant(e) => {
                        if next >= u32::MAX as usize {
                            return Err(IdError::TooManyNodes);
                        }
                        e.insert(next as u32);
                        to_user.push(user);
                        Ok(next as u32)
                    }
                }
            }
            IntTable::Stored { .. } => unreachable!("moved to a hash map above"),
        }
    }

    /// Make the direct table cover `user`, which lies outside it, once `ids`
    /// distinct ids are interned: grow it, or move to a hash map if the range
    /// would get too sparse. Afterwards `intern` finds `user` in range or hashes.
    #[cold]
    #[inline(never)]
    fn make_room(&mut self, user: i64, ids: u64, to_user: &IntIds) {
        let rows = self.rows;
        let IntTable::Direct { base, slot, lo, hi } = &mut self.table else {
            return;
        };
        // The range of ids seen, with `user`. Headroom is free to drop, so it
        // does not count against the budget.
        let (new_lo, new_hi) = if slot.is_empty() {
            (user, user)
        } else {
            ((*lo).min(user), (*hi).max(user))
        };
        let need = (new_hi as i128 - new_lo as i128 + 1) as u128;
        let budget = (DIRECT_SLOTS_PER_ID * ids).max(rows) + DIRECT_MIN_SLOTS;
        let limit = budget.min(u32::MAX as u64) as u128;
        if need > limit {
            self.table = IntTable::Hash(hash_map_of(to_user));
            return;
        }
        // Grow past `user` by the table's current length, so each growth at
        // least doubles it and the copies stay linear in total; keep the
        // headroom on the other side. Past the limit, drop headroom first. The
        // ends of the `i64` range cap it too; `i128` keeps it clear of overflow.
        let (old_lo, old_hi) = (*base as i128, *base as i128 + slot.len() as i128 - 1);
        let pad = slot.len() as i128;
        let up = slot.is_empty() || user as i128 > old_hi;
        let (mut ext_lo, mut ext_hi) = match (slot.is_empty(), up) {
            (true, _) => (user as i128, user as i128),
            (false, true) => (old_lo, user as i128 + pad),
            (false, false) => (user as i128 - pad, old_hi),
        };
        if (ext_hi - ext_lo + 1) as u128 > limit {
            (ext_lo, ext_hi) = if up {
                (new_lo as i128, new_lo as i128 + limit as i128 - 1)
            } else {
                (new_hi as i128 + 1 - limit as i128, new_hi as i128)
            };
        }
        let ext_lo = ext_lo.max(i64::MIN as i128);
        let ext_hi = ext_hi.min(i64::MAX as i128);
        let new_len = (ext_hi - ext_lo + 1) as usize;
        if slot.is_empty() || ext_lo == old_lo {
            slot.resize(new_len, MISS);
        } else {
            // Move the ids seen; the old headroom is all `MISS`.
            let from = (*lo as i128 - old_lo) as usize;
            let to = (*lo as i128 - ext_lo) as usize;
            let used = (*hi as i128 - *lo as i128 + 1) as usize;
            let mut grown = vec![MISS; new_len];
            grown[to..to + used].copy_from_slice(&slot[from..from + used]);
            *slot = grown;
        }
        (*base, *lo, *hi) = (ext_lo as i64, new_lo, new_hi);
    }

    /// Trim the direct table to the ids seen, once interning is done.
    fn shrink(&mut self) {
        if let IntTable::Direct { base, slot, lo, hi } = &mut self.table {
            if !slot.is_empty() {
                let from = (*lo as i128 - *base as i128) as usize;
                let used = (*hi as i128 - *lo as i128 + 1) as usize;
                slot.copy_within(from..from + used, 0);
                slot.truncate(used);
                *base = *lo;
            }
            slot.shrink_to_fit();
        }
    }
}

/// A hash map from each of `to_user`'s ids to its dense index.
fn hash_map_of(to_user: &IntIds) -> FxHashMap<i64, u32> {
    let mut map = FxHashMap::default();
    map.reserve(to_user.len());
    for (i, &id) in to_user.values().iter().enumerate() {
        map.insert(id, i as u32);
    }
    map
}

/// The lookup table of a stored index's ids: `slots[p]` is a dense index, or
/// [`MISS`] for an empty slot. It is the one structure of an [`IdMap`] that
/// can be used as it lies in a file, so a process that maps a stored index
/// finds a seed's node by reading a slot or two rather than by hashing every
/// id first (#421).
///
/// **The layout**, which is part of the stored index format (version 4):
///
/// - `n + n / 2 + 1` slots for `n` ids ([`lookup_table_len`]), so at most two
///   in three are taken and there is always an empty one.
/// - An id's first slot is `(hash * slots) >> 64`, of its 64-bit hash
///   ([`stored_hash_i64`], [`stored_hash_str`]) and the slot count as 128-bit
///   integers; it goes in the first empty slot from there on, wrapping at the
///   end (linear probing).
/// - A lookup reads slots from the id's first until it finds the id (it
///   compares the id itself, never only a hash) or an empty slot. Any table
///   that holds every dense index once and keeps that walk unbroken serves,
///   whatever order its ids went in; [`build_slots`] puts them in dense order.
///
/// A table that does not keep to this (a damaged file whose checksum still
/// matched) cannot make a lookup wrong or unsafe, only miss: a slot's index
/// is used only if it is in range and its id is the one asked for, and a walk
/// ends after one turn of the table.
///
/// A stored index without one (format versions 2 and 3) has its table built
/// here, when an id is first looked up: a query that runs an algorithm over
/// the whole graph and names no node never needs it.
#[derive(Debug, Clone)]
struct StoredTable {
    /// The slots, once they are given or built; or why they cannot be built.
    slots: OnceLock<Result<ScalarBuffer<u32>, IdError>>,
}

impl StoredTable {
    fn new(slots: Option<ScalarBuffer<u32>>) -> Self {
        let table = StoredTable {
            slots: OnceLock::new(),
        };
        if let Some(slots) = slots {
            let _ = table.slots.set(Ok(slots));
        }
        table
    }

    /// The slots, built with `build` if they are not there yet.
    #[inline]
    fn slots(
        &self,
        build: impl FnOnce() -> Result<Vec<u32>, IdError>,
    ) -> Result<&ScalarBuffer<u32>, IdError> {
        self.slots
            .get_or_init(|| build().map(ScalarBuffer::from))
            .as_ref()
            .map_err(Clone::clone)
    }
}

/// How many slots the stored lookup table of `n_ids` ids has (see
/// [`IdMap::lookup_table`]).
pub fn lookup_table_len(n_ids: usize) -> u64 {
    let n = n_ids as u64;
    n + n / 2 + 1
}

/// The slot a walk for an id of `hash` starts at, in a table of `slots`.
#[inline]
fn first_slot(hash: u64, slots: usize) -> usize {
    ((hash as u128 * slots as u128) >> 64) as usize
}

/// MurmurHash3's 64-bit finalizer: every input bit reaches every output bit.
#[inline]
fn mix(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

/// The hash a stored lookup table places an integer id by: [`mix`] of its
/// bits. Part of the stored index format, so it must never change.
#[inline]
fn stored_hash_i64(id: i64) -> u64 {
    mix(id as u64)
}

/// The hash a stored lookup table places a string id by. Part of the stored
/// index format, so it must never change, and it is spelled out here rather
/// than taken from a hashing crate, whose output may differ between versions
/// and between 32- and 64-bit targets.
///
/// With `K = 0x9e3779b97f4a7c15` and wrapping arithmetic: start from
/// `len * K`; for each 8 bytes of the id, read as a little-endian word `w`
/// (the last padded with zero bytes, and skipped if empty),
/// `h = (rotl(h, 5) ^ w) * K`; the hash is [`mix`] of the result.
#[inline]
fn stored_hash_str(id: &str) -> u64 {
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    let bytes = id.as_bytes();
    let mut h = (bytes.len() as u64).wrapping_mul(K);
    let mut words = bytes.chunks_exact(8);
    for word in &mut words {
        let w = u64::from_le_bytes(word.try_into().expect("8 bytes"));
        h = (h.rotate_left(5) ^ w).wrapping_mul(K);
    }
    let rest = words.remainder();
    if !rest.is_empty() {
        let mut word = [0u8; 8];
        word[..rest.len()].copy_from_slice(rest);
        h = (h.rotate_left(5) ^ u64::from_le_bytes(word)).wrapping_mul(K);
    }
    mix(h)
}

/// The dense index in `slots` of the id of `hash`, where `is(i)` says
/// whether dense index `i` is in range and has that id.
#[inline]
fn probe(slots: &[u32], hash: u64, is: impl Fn(u32) -> bool) -> Option<u32> {
    let len = slots.len();
    if len == 0 {
        return None;
    }
    let mut at = first_slot(hash, len);
    for _ in 0..len {
        let i = slots[at];
        if i == MISS {
            return None;
        }
        if is(i) {
            return Some(i);
        }
        at += 1;
        if at == len {
            at = 0;
        }
    }
    None
}

/// The stored lookup table of `n` distinct ids, of which `hash(i)` is the
/// hash of id `i` and `same(a, b)` says whether ids `a` and `b` are equal
/// ([`IdError::DuplicateId`] if two are). The ids go in dense order, so the
/// table is the same whatever the thread count; their hashes are computed in
/// parallel first.
fn build_slots(
    n: usize,
    hash: impl Fn(usize) -> u64 + Sync,
    same: impl Fn(u32, u32) -> bool,
) -> Result<Vec<u32>, IdError> {
    use crate::parallel::*;
    if n >= u32::MAX as usize {
        return Err(IdError::TooManyNodes);
    }
    let len = usize::try_from(lookup_table_len(n)).map_err(|_| IdError::TooManyNodes)?;
    // Not from inside a parallel loop: see `parallel::in_worker`.
    let hashes: Vec<u64> = if intern_in_parallel(n) {
        (0..n).into_par_iter().map(&hash).collect()
    } else {
        (0..n).map(&hash).collect()
    };
    let mut slots = vec![MISS; len];
    for (i, &h) in hashes.iter().enumerate() {
        let mut at = first_slot(h, len);
        loop {
            let taken = slots[at];
            if taken == MISS {
                slots[at] = i as u32;
                break;
            }
            if hashes[taken as usize] == h && same(taken, i as u32) {
                return Err(IdError::DuplicateId);
            }
            at += 1;
            if at == len {
                at = 0;
            }
        }
    }
    Ok(slots)
}

/// [`build_slots`] of integer ids.
fn build_int_slots(ids: &[i64]) -> Result<Vec<u32>, IdError> {
    build_slots(
        ids.len(),
        |i| stored_hash_i64(ids[i]),
        |a, b| ids[a as usize] == ids[b as usize],
    )
}

/// Chunks at least this long intern their known ids in parallel (see
/// [`intern_two_phase`]); shorter ones are not worth the fork.
const PARALLEL_MIN_ROWS: usize = 1 << 15;

/// Marks an endpoint the parallel lookup did not find, and an unseen id's slot
/// in the direct table ([`IntIndex`]). Dense ids stay below `u32::MAX` (the
/// interner errors first), so it never collides with one.
const MISS: u32 = u32::MAX;

/// Intern one null-free chunk of `len` rows, giving exactly the ids the serial
/// loop would. Ids already in the map are looked up in parallel against the
/// read-only map (`lookup(map, is_dst, row)`); that lookup is most of the work
/// once a graph's nodes have been seen. One serial pass then interns the misses
/// (`intern(map, is_dst, row)`) in row order, src before dst, as the serial loop
/// does. An id already in the map keeps its index whatever order it is looked up
/// in, and only the serial pass adds ids, so new ids are numbered in first-seen
/// order.
fn intern_two_phase(
    map: &mut IdMap,
    len: usize,
    sd: &mut Vec<u32>,
    dd: &mut Vec<u32>,
    lookup: impl Fn(&IdMap, bool, usize) -> Option<u32> + Sync,
    mut intern: impl FnMut(&mut IdMap, bool, usize) -> Result<u32, IdError>,
) -> Result<(), IdError> {
    use crate::parallel::*;
    const CHUNK: usize = 4096;
    let base = sd.len();
    sd.resize(base + len, MISS);
    dd.resize(base + len, MISS);
    {
        let map = &*map;
        for (out, is_dst) in [(&mut sd[base..], false), (&mut dd[base..], true)] {
            out.par_chunks_mut(CHUNK).enumerate().for_each(|(c, out)| {
                for (k, o) in out.iter_mut().enumerate() {
                    if let Some(dense) = lookup(map, is_dst, c * CHUNK + k) {
                        *o = dense;
                    }
                }
            });
        }
    }
    for i in 0..len {
        if sd[base + i] == MISS {
            sd[base + i] = intern(map, false, i)?;
        }
        if dd[base + i] == MISS {
            dd[base + i] = intern(map, true, i)?;
        }
    }
    Ok(())
}

/// Whether a chunk of `len` rows should take [`intern_two_phase`].
fn intern_in_parallel(len: usize) -> bool {
    len >= PARALLEL_MIN_ROWS
        && crate::parallel::current_num_threads() > 1
        && !crate::parallel::in_worker()
}

/// Bidirectional map between arbitrary user ids and dense `u32` indices.
///
/// `u32` caps the node space at ~4.29B nodes — the correct trade for cache
/// behaviour at the v0.1 target scale. A `u64` node space is a future feature flag.
///
/// String ids are stored once, in Arrow-shaped buffers that become the id column
/// (see [`StrIds`]). The user-id Arrow array is cached (`id_array`): it is
/// immutable once built, so the per-collect `user_id_array()` call is an `Arc`
/// clone instead of a full deep copy of every id.
#[derive(Debug, Clone)]
pub enum IdMap {
    /// Integer user ids (the fast path).
    Int64 {
        to_user: IntIds,
        to_dense: IntIndex,
        id_array: OnceLock<ArrayRef>,
    },
    /// String user ids (also covers UUID-as-string).
    Utf8 {
        ids: StrIds,
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
    /// The distinct string ids total more than `i32::MAX` bytes, beyond the `Utf8`
    /// id column's offset range.
    IdBytesOverflow,
    /// An id appears twice in a list of distinct ids ([`IdMap::from_user_ids`],
    /// or the first lookup in an [`IdMap::from_stored_ids`] given no table).
    DuplicateId,
    /// The lookup table given with a stored index's ids is not one of as
    /// many ids ([`IdMap::from_stored_ids`]).
    LookupTable,
    /// Nodes were given to an [`EdgeInterner`] after its first edge, or after
    /// its node set was fixed.
    NodesAfterEdges,
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
            IdError::Unsupported(t) => {
                write!(f, "node ids must be an integer or string column; got {t}")
            }
            IdError::LengthMismatch => {
                write!(f, "src and dst columns must have the same length")
            }
            IdError::TooManyNodes => write!(
                f,
                "more than u32::MAX (~4.29B) distinct nodes; exceeds the dense-index cap"
            ),
            IdError::DuplicateId => {
                write!(f, "a node id appears twice in a list of distinct ids")
            }
            IdError::NodesAfterEdges => {
                write!(f, "nodes must be given before the first edge")
            }
            IdError::LookupTable => {
                write!(f, "the node ids' lookup table does not fit them")
            }
            IdError::IdBytesOverflow => write!(
                f,
                "the distinct string node ids total more than 2 GiB, beyond the Utf8 id column"
            ),
        }
    }
}

impl std::error::Error for IdError {}

/// Which supported family an Arrow id column belongs to.
#[derive(Clone, Copy)]
enum IdKind {
    Int64,
    Utf8,
}

impl IdKind {
    fn new_map(self) -> IdMap {
        match self {
            IdKind::Int64 => IdMap::new_int64(),
            IdKind::Utf8 => IdMap::new_utf8(),
        }
    }
}

fn id_kind(dt: &DataType) -> Result<IdKind, IdError> {
    match dt {
        DataType::Int64 => Ok(IdKind::Int64),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Ok(IdKind::Utf8),
        other => Err(IdError::Unsupported(format!("{other:?}").to_lowercase())),
    }
}

/// Downcast an id array to `Int64Array`, canonicalizing nothing (the caller has
/// already confirmed the kind).
fn as_int64(arr: &dyn Array) -> &Int64Array {
    arr.as_any()
        .downcast_ref::<Int64Array>()
        .expect("id column confirmed Int64")
}

/// A borrowing view over a `Utf8`, `LargeUtf8` or `Utf8View` id array, reading
/// each layout natively. This replaces a cast to `Utf8`, which (a) **fails** once
/// the concatenated string data exceeds `i32::MAX` bytes — exactly what engines
/// emit for large string columns, so a valid big id column would panic across
/// PyO3 — and (b) copies the whole column before interning even on success.
/// `Utf8View` is what DataFusion, Polars and recent pyarrow produce for strings
/// (a Parquet scan, `pa.string_view()`), so it is read as-is too.
enum StrView<'a> {
    Utf8(&'a StringArray),
    Large(&'a LargeStringArray),
    View(&'a StringViewArray),
}

impl<'a> StrView<'a> {
    /// The caller has already confirmed the array is a string type (`id_kind`).
    fn new(arr: &'a dyn Array) -> Self {
        let any = arr.as_any();
        if let Some(s) = any.downcast_ref::<StringArray>() {
            StrView::Utf8(s)
        } else if let Some(s) = any.downcast_ref::<LargeStringArray>() {
            StrView::Large(s)
        } else {
            StrView::View(
                any.downcast_ref::<StringViewArray>()
                    .expect("id kind confirmed Utf8/LargeUtf8/Utf8View"),
            )
        }
    }
    fn len(&self) -> usize {
        match self {
            StrView::Utf8(a) => a.len(),
            StrView::Large(a) => a.len(),
            StrView::View(a) => a.len(),
        }
    }
    fn is_null(&self, i: usize) -> bool {
        match self {
            StrView::Utf8(a) => a.is_null(i),
            StrView::Large(a) => a.is_null(i),
            StrView::View(a) => a.is_null(i),
        }
    }
    fn value(&self, i: usize) -> &str {
        match self {
            StrView::Utf8(a) => a.value(i),
            StrView::Large(a) => a.value(i),
            StrView::View(a) => a.value(i),
        }
    }
}

impl IdMap {
    fn new_int64() -> Self {
        IdMap::Int64 {
            to_user: IntIds::new(),
            to_dense: IntIndex::new(),
            id_array: OnceLock::new(),
        }
    }

    fn new_utf8() -> Self {
        IdMap::Utf8 {
            ids: StrIds::new(),
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
        map.freeze();
        map
    }

    /// Build the id map from the two edge endpoint columns, and return the edges
    /// re-expressed in dense `u32` space (`src_dense`, `dst_dense`) ready for
    /// [`Topology::build`]. The id type is taken from the Arrow columns: `Int64`
    /// (and other integer types are not accepted here — callers canonicalize to
    /// `Int64` first) or `Utf8`/`LargeUtf8`/`Utf8View`. Errors on a null endpoint, mixed
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

    /// Rebuild a map from its id column: `ids[i]` is the user id of dense node
    /// `i`, as [`Self::user_id_array`] returned it. This is how a stored index
    /// (the SQL engine's `.ursa` database) is read back. `ids` must be `Int64`
    /// or a string type, with no nulls ([`IdError::Null`]) and no id twice
    /// ([`IdError::DuplicateId`]).
    pub fn from_user_ids(ids: &dyn Array) -> Result<Self, IdError> {
        if ids.null_count() > 0 {
            return Err(IdError::Null);
        }
        let mut map = match id_kind(ids.data_type())? {
            IdKind::Int64 => {
                let mut map = IdMap::new_int64();
                if let IdMap::Int64 { to_dense, .. } = &mut map {
                    to_dense.budget_rows(ids.len());
                }
                for (i, &id) in as_int64(ids).values().iter().enumerate() {
                    if map.intern_i64(id)? as usize != i {
                        return Err(IdError::DuplicateId);
                    }
                }
                map
            }
            IdKind::Utf8 => {
                let mut map = IdMap::new_utf8();
                let view = StrView::new(ids);
                for i in 0..view.len() {
                    if map.intern_str(view.value(i))? as usize != i {
                        return Err(IdError::DuplicateId);
                    }
                }
                map
            }
        };
        map.freeze();
        Ok(map)
    }

    /// The map of a stored index, around its id column as it lies in the
    /// file: `ids[i]` is the user id of dense node `i`, and `table` is the
    /// column's lookup table ([`Self::lookup_table`]) if the file has it.
    /// Neither is copied or read here, so this costs the same for any number
    /// of nodes; `ids` then is [`Self::user_id_array`].
    ///
    /// Without a table, one is built when an id is first looked up
    /// ([`Self::dense_from_array`]), and that is when an id that appears
    /// twice is found ([`IdError::DuplicateId`]): a map used only to name
    /// the nodes of a result never builds it. A table that is given is
    /// trusted to hold every id (the file's checksum vouches for it); one
    /// that does not can only fail to find an id (see [`StoredTable`]).
    ///
    /// `ids` must be `Int64` or a string type with no nulls
    /// ([`IdError::Null`]), and `table` must have [`lookup_table_len`] slots
    /// ([`IdError::LookupTable`]). A `LargeUtf8` or `Utf8View` column is
    /// copied into a `Utf8` one first.
    pub fn from_stored_ids(
        ids: ArrayRef,
        table: Option<ScalarBuffer<u32>>,
    ) -> Result<Self, IdError> {
        if ids.null_count() > 0 {
            return Err(IdError::Null);
        }
        if ids.len() >= u32::MAX as usize {
            return Err(IdError::TooManyNodes);
        }
        if table
            .as_ref()
            .is_some_and(|t| t.len() as u64 != lookup_table_len(ids.len()))
        {
            return Err(IdError::LookupTable);
        }
        Ok(match id_kind(ids.data_type())? {
            IdKind::Int64 => {
                let array = as_int64(ids.as_ref()).clone();
                IdMap::Int64 {
                    to_user: IntIds(IntStore::Frozen(array.clone())),
                    to_dense: IntIndex::stored(array, table),
                    id_array: OnceLock::from(ids),
                }
            }
            IdKind::Utf8 => {
                let (array, ids) = match ids.as_any().downcast_ref::<StringArray>() {
                    Some(array) => (array.clone(), ids),
                    None => {
                        let view = StrView::new(ids.as_ref());
                        let bytes: usize = (0..view.len()).map(|i| view.value(i).len()).sum();
                        i32::try_from(bytes).map_err(|_| IdError::IdBytesOverflow)?;
                        let array = gather_strings(|i| view.value(i), 0..view.len());
                        (array.clone(), Arc::new(array) as ArrayRef)
                    }
                };
                IdMap::Utf8 {
                    ids: StrIds::stored(array, table),
                    id_array: OnceLock::from(ids),
                }
            }
        })
    }

    /// The map's lookup table, as a stored index holds it beside the id
    /// column, for [`Self::from_stored_ids`] to be given when the index is
    /// read back: [`lookup_table_len`] slots, each a dense index or
    /// `u32::MAX` for an empty one. Its layout is fixed (see
    /// [`StoredTable`]), and it is the same for the same ids on every
    /// machine and with any number of threads. A map that came with its
    /// table, or has built it, hands that one out.
    pub fn lookup_table(&self) -> Result<ScalarBuffer<u32>, IdError> {
        match self {
            IdMap::Int64 {
                to_user, to_dense, ..
            } => match to_dense.stored_slots() {
                Some(slots) => slots.cloned(),
                None => build_int_slots(to_user.values()).map(ScalarBuffer::from),
            },
            IdMap::Utf8 { ids, .. } => match ids.stored_slots() {
                Some(slots) => slots.cloned(),
                None => ids.build_slots().map(ScalarBuffer::from),
            },
        }
    }

    /// Make sure ids can be looked up: build the lookup table of a stored
    /// index's ids that came without one.
    fn ready(&self) -> Result<(), IdError> {
        let slots = match self {
            IdMap::Int64 { to_dense, .. } => to_dense.stored_slots(),
            IdMap::Utf8 { ids, .. } => ids.stored_slots(),
        };
        slots.map_or(Ok(()), |slots| slots.map(|_| ()))
    }

    #[inline]
    fn intern_i64(&mut self, user: i64) -> Result<u32, IdError> {
        match self {
            IdMap::Int64 {
                to_user, to_dense, ..
            } => to_dense.intern(user, to_user),
            IdMap::Utf8 { .. } => unreachable!("intern_i64 on a Utf8 IdMap"),
        }
    }

    fn intern_str(&mut self, user: &str) -> Result<u32, IdError> {
        match self {
            IdMap::Utf8 { ids, .. } => ids.intern(user),
            IdMap::Int64 { .. } => unreachable!("intern_str on an Int64 IdMap"),
        }
    }

    /// Settle the map once interning is done: the ids move into their final id
    /// column (no copy), which is cached as [`Self::user_id_array`].
    fn freeze(&mut self) {
        let (array, id_array): (ArrayRef, _) = match self {
            IdMap::Int64 {
                to_user,
                to_dense,
                id_array,
            } => {
                to_dense.shrink();
                (Arc::new(to_user.freeze()), id_array)
            }
            IdMap::Utf8 { ids, id_array } => (Arc::new(ids.freeze()), id_array),
        };
        *id_array = OnceLock::new();
        let _ = id_array.set(array);
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
            IdMap::Int64 { to_user, .. } => Arc::new(to_user.to_array()),
            IdMap::Utf8 { ids, .. } => Arc::new(ids.to_array()),
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
                    .map(|&d| to_user.values()[d as usize])
                    .collect::<Vec<_>>(),
            )),
            IdMap::Utf8 { ids, .. } => Arc::new(gather_strings(
                |i| ids.value(i as u32),
                dense.iter().map(|&d| d as usize),
            )),
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
        self.ready()?;
        match self {
            IdMap::Int64 { to_dense, .. } => {
                if !matches!(id_kind(arr.data_type())?, IdKind::Int64) {
                    return Err(IdError::MixedTypes);
                }
                let a = as_int64(arr);
                if a.null_count() == 0 {
                    return Ok(a.values().iter().map(|&u| to_dense.get(u)).collect());
                }
                Ok((0..a.len())
                    .map(|i| {
                        if a.is_null(i) {
                            None
                        } else {
                            to_dense.get(a.value(i))
                        }
                    })
                    .collect())
            }
            IdMap::Utf8 { ids, .. } => {
                if !matches!(id_kind(arr.data_type())?, IdKind::Utf8) {
                    return Err(IdError::MixedTypes);
                }
                let a = StrView::new(arr);
                if arr.null_count() == 0 {
                    return Ok((0..a.len()).map(|i| ids.get(a.value(i))).collect());
                }
                Ok((0..a.len())
                    .map(|i| {
                        if a.is_null(i) {
                            None
                        } else {
                            ids.get(a.value(i))
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
            IdMap::Utf8 { ids, .. } => ids.len(),
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
    fn a_fixed_node_set_keeps_isolated_nodes_and_drops_dangling_edges() {
        let mut interner = EdgeInterner::with_capacity(4);
        interner
            .push_nodes(&Int64Array::from(vec![
                Some(5),
                Some(1),
                None,
                Some(9),
                Some(1),
            ]))
            .unwrap();
        interner.close_nodes();
        // 1->5 kept; 1->7 (7 is no node) dropped; 9->5 kept; null->1 dropped.
        interner
            .push(
                &Int64Array::from(vec![Some(1), Some(1), Some(9), None]),
                &Int64Array::from(vec![Some(5), Some(7), Some(5), Some(1)]),
            )
            .unwrap();
        assert_eq!(interner.n_dropped(), 2);
        let kept = interner.kept_rows().unwrap();
        assert_eq!(
            kept.iter().collect::<Vec<_>>(),
            vec![true, false, true, false]
        );
        assert!(interner.push_nodes(&Int64Array::from(vec![3])).is_err());
        let (map, sd, dd) = interner.finish();
        // The nodes in the order given, 9 with no edge to itself still a node.
        let ids = map.user_id_array();
        let ids = ids.as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(ids.values(), &[5, 1, 9]);
        assert_eq!((sd, dd), (vec![1, 2], vec![0, 0]));

        // Strings, and nothing dropped: no kept bitmap.
        let mut interner = EdgeInterner::with_capacity(1);
        interner
            .push_nodes(&StringArray::from(vec!["a", "b", "c"]))
            .unwrap();
        interner.close_nodes();
        interner
            .push(&StringArray::from(vec!["c"]), &StringArray::from(vec!["a"]))
            .unwrap();
        assert_eq!(interner.n_dropped(), 0);
        assert!(interner.kept_rows().is_none());
        let (map, sd, dd) = interner.finish();
        assert_eq!((map.len(), sd, dd), (3, vec![2], vec![0]));

        // Edges of another type than the nodes.
        let mut interner = EdgeInterner::with_capacity(1);
        interner.push_nodes(&StringArray::from(vec!["a"])).unwrap();
        interner.close_nodes();
        assert_eq!(
            interner.push(&Int64Array::from(vec![1]), &Int64Array::from(vec![1])),
            Err(IdError::MixedTypes)
        );
    }

    /// The dense endpoints, ids, dropped count and kept rows of `interner`.
    type Built = (Vec<u32>, Vec<u32>, ArrayRef, u64, Option<Vec<bool>>);

    fn built(mut interner: EdgeInterner) -> Built {
        let dropped = interner.n_dropped();
        let kept = interner.kept_rows().map(|k| k.iter().collect());
        let (map, sd, dd) = interner.finish();
        (sd, dd, map.user_id_array(), dropped, kept)
    }

    /// Edges interned provisionally while the nodes are read, then adopted,
    /// give what pushing them after the nodes gives, wherever the switch
    /// from provisional to closed falls (#385).
    #[test]
    fn edges_adopted_after_the_nodes_are_the_edges_pushed_after_them() {
        let ints = |v: &[Option<i64>]| -> ArrayRef { Arc::new(Int64Array::from(v.to_vec())) };
        let strs = |v: &[Option<i64>]| -> ArrayRef {
            Arc::new(StringArray::from(
                v.iter()
                    .map(|x| x.map(|x| format!("n{x}")))
                    .collect::<Vec<_>>(),
            ))
        };
        type Ids<'a> = &'a [Option<i64>];
        let nodes = [Some(5), Some(1), None, Some(9), Some(1), Some(30), Some(2)];
        // Chunks of edges: dangling ids (7, 8, 40), nulls, repeats, and ids
        // first seen in another order than the nodes give them.
        let chunks: [(Ids, Ids); 4] = [
            (&[Some(9), Some(7), Some(1)], &[Some(1), Some(5), Some(9)]),
            (&[None, Some(2), Some(30)], &[Some(1), Some(8), Some(2)]),
            (&[Some(2), Some(5), Some(40)], &[Some(30), None, Some(1)]),
            (&[Some(1), Some(9)], &[Some(5), Some(9)]),
        ];
        for make in [ints, strs] {
            let with_nodes = || {
                let mut nodes_interner = EdgeInterner::with_capacity(8);
                nodes_interner.push_nodes(make(&nodes).as_ref()).unwrap();
                nodes_interner.close_nodes();
                nodes_interner
            };
            let mut closed = with_nodes();
            for (s, d) in chunks {
                closed.push(make(s).as_ref(), make(d).as_ref()).unwrap();
            }
            let expected = built(closed);
            assert_eq!(expected.3, 5, "dropped");
            // The first `split` chunks provisional, the rest pushed after.
            for split in 0..=chunks.len() {
                let mut early = EdgeInterner::provisional(8);
                for (s, d) in &chunks[..split] {
                    early.push(make(s).as_ref(), make(d).as_ref()).unwrap();
                }
                let mut interner = with_nodes();
                interner.adopt(early).unwrap();
                for (s, d) in &chunks[split..] {
                    interner.push(make(s).as_ref(), make(d).as_ref()).unwrap();
                }
                let got = built(interner);
                assert_eq!(got.0, expected.0, "src, split {split}");
                assert_eq!(got.1, expected.1, "dst, split {split}");
                assert_eq!(got.2.as_ref(), expected.2.as_ref(), "ids, split {split}");
                assert_eq!((got.3, &got.4), (expected.3, &expected.4), "split {split}");
            }
        }
        // No node at all: every edge is dropped.
        let mut early = EdgeInterner::provisional(2);
        early
            .push(&Int64Array::from(vec![1, 2]), &Int64Array::from(vec![2, 1]))
            .unwrap();
        let mut interner = EdgeInterner::with_capacity(2);
        interner.close_nodes();
        interner.adopt(early).unwrap();
        assert_eq!(interner.n_dropped(), 2);
        assert_eq!(interner.finish().1, Vec::<u32>::new());
    }

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
    fn utf8_view_is_handled_natively_and_round_trips() {
        // Utf8View (DataFusion's Parquet string type, pyarrow's string_view) is
        // read in place, like LargeUtf8: no cast, and the same map as Utf8 ids.
        // Values past 12 bytes live in the data buffers rather than inline.
        let long = "a-node-id-longer-than-twelve-bytes";
        let src = StringViewArray::from(vec!["x", long, "y"]);
        let dst = StringViewArray::from(vec![long, "y", "x"]);
        let (map, sd, dd) = IdMap::from_edge_arrays(&src, &dst).unwrap();
        let (umap, usd, udd) = IdMap::from_edge_arrays(
            &StringArray::from(vec!["x", long, "y"]),
            &StringArray::from(vec![long, "y", "x"]),
        )
        .unwrap();
        assert_eq!(map.user_type(), DataType::Utf8);
        assert_eq!((&sd, &dd), (&usd, &udd));
        assert_eq!(&map.user_id_array(), &umap.user_id_array());
        // A Utf8View lookup array resolves too (seeds, node-attribute join keys).
        let query = StringViewArray::from(vec![Some(long), None, Some("nope")]);
        assert_eq!(
            map.dense_from_array(&query).unwrap(),
            vec![Some(1), None, None]
        );
        // Mixed string layouts across src/dst or chunks are all one id family.
        let (_, msd, mdd) =
            IdMap::from_edge_arrays(&src, &StringArray::from(vec![long, "y", "x"])).unwrap();
        assert_eq!((msd, mdd), (usd, udd));
        // A null view endpoint is still rejected.
        let with_null = StringViewArray::from(vec![Some("x"), None, Some("y")]);
        assert_eq!(
            IdMap::from_edge_arrays(&with_null, &dst).unwrap_err(),
            IdError::Null
        );
    }

    #[test]
    fn utf8_view_chunks_intern_in_parallel_like_utf8() {
        // Above the parallel threshold the view array takes the two-phase path;
        // it must give the same ids as the Utf8 array of the same values.
        let n = 2 * PARALLEL_MIN_ROWS + 5;
        let name = |x: usize| format!("did:plc:{:x}", x * 7919 % 20_000);
        let src: Vec<String> = (0..n).map(name).collect();
        let dst: Vec<String> = (0..n).map(|i| name(i + 1)).collect();
        let utf8 = IdMap::from_edge_arrays(
            &StringArray::from(src.clone()),
            &StringArray::from(dst.clone()),
        )
        .unwrap();
        let view = IdMap::from_edge_arrays(
            &StringViewArray::from_iter_values(&src),
            &StringViewArray::from_iter_values(&dst),
        )
        .unwrap();
        assert_eq!((&view.1, &view.2), (&utf8.1, &utf8.2));
        assert_eq!(&view.0.user_id_array(), &utf8.0.user_id_array());
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
    fn large_string_chunks_intern_exactly_like_small_ones() {
        let n = 2 * PARALLEL_MIN_ROWS + 77;
        let name = |x: u64| format!("did:plc:{x:x}");
        let src: Vec<String> = (0..n as u64).map(|i| name(i * 7919 % 20_000)).collect();
        let dst: Vec<String> = (0..n as u64)
            .map(|i| name(i * 104_729 % 30_011 + 3))
            .collect();
        let (s_all, d_all) = (StringArray::from(src), StringArray::from(dst));
        let big = [
            (
                &s_all.slice(0, 50) as &dyn Array,
                &d_all.slice(0, 50) as &dyn Array,
            ),
            (
                &s_all.slice(50, n - 50) as &dyn Array,
                &d_all.slice(50, n - 50) as &dyn Array,
            ),
        ];
        let (bmap, bsd, bdd) = IdMap::from_edge_batches(&big).unwrap();
        let small: Vec<(StringArray, StringArray)> = (0..n)
            .step_by(999)
            .map(|lo| {
                let len = 999.min(n - lo);
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
    fn int_ids_freeze_shares_the_id_column() {
        let (map, sd, dd) = IdMap::from_edge_arrays(
            &Int64Array::from(vec![10, 20, 30]),
            &Int64Array::from(vec![20, 30, 10]),
        )
        .unwrap();
        assert_eq!((sd, dd), (vec![0, 1, 2], vec![1, 2, 0]));
        // The cached id column is the frozen store itself, not a copy.
        let IdMap::Int64 { to_user, .. } = &map else {
            panic!("int map")
        };
        let col = map.user_id_array();
        let col = col.as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(col.values().as_ptr(), to_user.values().as_ptr());
        assert_eq!(
            map.gather_user(&[2, 0])
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap(),
            &Int64Array::from(vec![30, 10])
        );
        // Thawing for a later intern keeps the ids.
        let mut map = map;
        assert_eq!(map.intern_i64(40).unwrap(), 3);
        assert_eq!(map.intern_i64(20).unwrap(), 1);
    }

    #[test]
    fn str_ids_intern_freeze_and_thaw() {
        let mut ids = StrIds::new();
        let words = ["", "a", "héllo", "a", "🦀", "", "zz"];
        let got: Vec<u32> = words.iter().map(|w| ids.intern(w).unwrap()).collect();
        assert_eq!(got, vec![0, 1, 2, 1, 3, 0, 4]);
        // Enough ids to force the table to grow; lookups must survive the rehash.
        for i in 0..10_000 {
            ids.intern(&format!("id-{i}")).unwrap();
        }
        assert_eq!(ids.get("héllo"), Some(2));
        assert_eq!(ids.get("id-9999"), Some(10_004));
        assert_eq!(ids.get("missing"), None);

        let frozen = ids.freeze();
        assert_eq!(frozen.len(), 10_005);
        assert_eq!(frozen.value(3), "🦀");
        assert_eq!(ids.get("id-0"), Some(5)); // lookups read the frozen array
                                              // Interning after freeze still works (copying back out).
        assert_eq!(ids.intern("zz").unwrap(), 4);
        assert_eq!(ids.intern("new").unwrap(), 10_005);
        assert_eq!(ids.to_array().value(10_005), "new");
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

    /// Intern `chunks` of `(src, dst)` ids, through the direct table (as built
    /// normally) or forced onto the hash map from the start.
    fn intern_chunks(chunks: &[(Vec<i64>, Vec<i64>)], hashed: bool) -> (IdMap, Vec<u32>, Vec<u32>) {
        let rows = chunks.iter().map(|(s, _)| s.len()).sum();
        let mut interner = EdgeInterner::with_capacity(rows);
        if hashed {
            let mut map = IdMap::new_int64();
            let IdMap::Int64 { to_dense, .. } = &mut map else {
                unreachable!()
            };
            to_dense.table = IntTable::Hash(FxHashMap::default());
            interner.map = Some(map);
        }
        for (s, d) in chunks {
            interner
                .push(&Int64Array::from(s.clone()), &Int64Array::from(d.clone()))
                .unwrap();
        }
        interner.finish()
    }

    fn is_direct(map: &IdMap) -> bool {
        matches!(
            map,
            IdMap::Int64 {
                to_dense: IntIndex {
                    table: IntTable::Direct { .. },
                    ..
                },
                ..
            }
        )
    }

    /// Intern `chunks` both ways and require identical output: dense endpoints,
    /// the id column, gathers, and lookups of seen, unseen and null ids. Returns
    /// whether the default build ended on the direct table.
    fn assert_same_as_hash(chunks: &[(Vec<i64>, Vec<i64>)]) -> bool {
        let (dmap, dsd, ddd) = intern_chunks(chunks, false);
        let (hmap, hsd, hdd) = intern_chunks(chunks, true);
        assert!(!is_direct(&hmap));
        assert_eq!((&dsd, &ddd), (&hsd, &hdd));
        assert_eq!(&dmap.user_id_array(), &hmap.user_id_array());
        assert_eq!(&dmap.gather_user(&dsd), &hmap.gather_user(&hsd));
        let mut probe: Vec<i64> = chunks
            .iter()
            .flat_map(|(s, d)| s.iter().chain(d))
            .flat_map(|&u| [u, u.wrapping_add(1), u.wrapping_sub(1)])
            .collect();
        probe.extend([i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX]);
        let probe = Int64Array::from(probe);
        assert_eq!(
            dmap.dense_from_array(&probe).unwrap(),
            hmap.dense_from_array(&probe).unwrap()
        );
        let nullable: Int64Array = probe
            .iter()
            .enumerate()
            .map(|(i, u)| u.filter(|_| i % 3 != 0))
            .collect();
        assert_eq!(
            dmap.dense_from_array(&nullable).unwrap(),
            hmap.dense_from_array(&nullable).unwrap()
        );
        is_direct(&dmap)
    }

    /// One chunk of `ids` read as consecutive `(src, dst)` pairs.
    fn pairs(ids: &[i64]) -> (Vec<i64>, Vec<i64>) {
        ids.chunks_exact(2).map(|p| (p[0], p[1])).unzip()
    }

    #[test]
    fn compact_ids_stay_direct_and_match_the_hash_map() {
        // A shuffled compact range, above the parallel threshold, in two chunks.
        let n = 3 * PARALLEL_MIN_ROWS as i64;
        let src: Vec<i64> = (0..n).map(|i| (i * 7919) % n).collect();
        let dst: Vec<i64> = (0..n).map(|i| (i * 104_729 + 3) % n).collect();
        let (s0, s1) = src.split_at(100);
        let (d0, d1) = dst.split_at(100);
        let chunks = [(s0.to_vec(), d0.to_vec()), (s1.to_vec(), d1.to_vec())];
        assert!(assert_same_as_hash(&chunks));
        // An offset range, first seen from the top down, grows the table downwards.
        let ids: Vec<i64> = (0..10_000).rev().map(|i| 5_000_000_000 + i).collect();
        assert!(assert_same_as_hash(&[pairs(&ids)]));
    }

    #[test]
    fn compact_ids_in_random_order_stay_direct() {
        // Two random ids from `0..1.5M` already span most of the range: the
        // per-row allowance, from the known row count, keeps the table direct.
        use rand::{Rng, SeedableRng};
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(3);
        let ids: Vec<i64> = (0..1_200_000)
            .map(|_| rng.gen_range(0..1_500_000))
            .collect();
        let (src, dst) = pairs(&ids);
        let chunks: Vec<(Vec<i64>, Vec<i64>)> = src
            .chunks(8192)
            .zip(dst.chunks(8192))
            .map(|(s, d)| (s.to_vec(), d.to_vec()))
            .collect();
        assert!(assert_same_as_hash(&chunks));
        // With no row count up front the budget only grows with the rows pushed,
        // so the same input falls back early, with the same output.
        let mut interner = EdgeInterner::with_capacity(0);
        for (s, d) in &chunks {
            interner
                .push(&Int64Array::from(s.clone()), &Int64Array::from(d.clone()))
                .unwrap();
        }
        let (map, sd, dd) = interner.finish();
        assert!(!is_direct(&map));
        let (dmap, dsd, ddd) = intern_chunks(&chunks, false);
        assert_eq!((sd, dd), (dsd, ddd));
        assert_eq!(&map.user_id_array(), &dmap.user_id_array());
    }

    #[test]
    fn gappy_ids_match_the_hash_map() {
        // Every third id: gaps never become nodes.
        let ids: Vec<i64> = (0..60_000).map(|i| (i * 7919 % 60_000) * 3 + 1).collect();
        assert!(assert_same_as_hash(&[pairs(&ids)]));
        let (map, _, _) = intern_chunks(&[pairs(&ids)], false);
        assert_eq!(map.len(), 60_000);
    }

    #[test]
    fn sparse_ids_fall_back_to_the_hash_map() {
        // Sparse from the start.
        let ids: Vec<i64> = (0..1000).map(|i| i * 1_000_003_000).collect();
        assert!(!assert_same_as_hash(&[pairs(&ids)]));
        // Compact, then one far id mid-chunk, then compact ids again: the
        // migration keeps every index already handed out.
        let mut ids: Vec<i64> = (0..5000).collect();
        ids.insert(3001, 1 << 40);
        ids.extend(0..6000);
        assert!(!assert_same_as_hash(&[pairs(&ids)]));
        // The same across chunks, with the far id in a parallel-sized chunk.
        let n = 2 * PARALLEL_MIN_ROWS as i64;
        let first = pairs(&(0..n).collect::<Vec<_>>());
        let mut second: Vec<i64> = (0..n).rev().collect();
        second[n as usize / 2 + 1] = -(1 << 50);
        assert!(!assert_same_as_hash(&[first, pairs(&second)]));
    }

    #[test]
    fn negative_and_extreme_ids_match_the_hash_map() {
        let neg: Vec<i64> = (-5000..5000).rev().collect();
        assert!(assert_same_as_hash(&[pairs(&neg)]));
        // Compact ranges at either end of `i64`: the table stops at the edge.
        let top: Vec<i64> = (0..2000).map(|i| i64::MAX - i).collect();
        assert!(assert_same_as_hash(&[pairs(&top)]));
        let top_up: Vec<i64> = (0..2000).rev().map(|i| i64::MAX - i).collect();
        assert!(assert_same_as_hash(&[pairs(&top_up)]));
        let bottom: Vec<i64> = (0..2000).map(|i| i64::MIN + i).collect();
        assert!(assert_same_as_hash(&[pairs(&bottom)]));
        let bottom_down: Vec<i64> = (0..2000).rev().map(|i| i64::MIN + i).collect();
        assert!(assert_same_as_hash(&[pairs(&bottom_down)]));
        // Both ends at once is the widest range there is.
        assert!(!assert_same_as_hash(&[pairs(&[
            i64::MAX,
            i64::MIN,
            0,
            i64::MAX
        ])]));
        assert!(!assert_same_as_hash(&[pairs(&[i64::MIN, i64::MAX])]));
    }

    #[test]
    fn random_ids_match_the_hash_map() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(171);
        for case in 0..60 {
            // Ranges from dense to sparse, offsets anywhere in `i64`, chunk sizes
            // on both sides of the parallel threshold.
            let span: i64 = [10, 1000, 100_000, 3_000_000, 1 << 40, i64::MAX][case % 6];
            let offset: i64 = match case % 4 {
                0 => 0,
                1 => -span / 2,
                2 => rng.gen(),
                _ => i64::MAX - span,
            };
            let rows = [7, 5000, PARALLEL_MIN_ROWS + 9][case % 3];
            let chunks: Vec<(Vec<i64>, Vec<i64>)> = (0..1 + case % 3)
                .map(|_| {
                    let mut id = || offset.wrapping_add(rng.gen_range(0..span));
                    (0..rows).map(|_| (id(), id())).unzip()
                })
                .collect();
            assert_same_as_hash(&chunks);
        }
    }

    #[test]
    fn freeze_trims_the_direct_table() {
        let (map, _, _) = intern_chunks(&[pairs(&(0..3000).collect::<Vec<_>>())], false);
        let IdMap::Int64 {
            to_dense:
                IntIndex {
                    table: IntTable::Direct { base, slot, .. },
                    ..
                },
            ..
        } = &map
        else {
            panic!("compact ids stay direct")
        };
        assert_eq!((*base, slot.len()), (0, 3000));
    }

    #[test]
    fn dense_from_array_rejects_wrong_type() {
        let m = IdMap::from_ids([1, 2, 3]);
        let wrong = StringArray::from(vec!["1"]);
        assert_eq!(m.dense_from_array(&wrong), Err(IdError::MixedTypes));
    }

    #[test]
    fn from_user_ids_rebuilds_the_map() {
        let src = StringArray::from(vec!["b", "a", "b"]);
        let dst = StringArray::from(vec!["a", "c", "c"]);
        let (m, _, _) = IdMap::from_edge_arrays(&src, &dst).unwrap();
        let back = IdMap::from_user_ids(m.user_id_array().as_ref()).unwrap();
        assert_eq!(back.user_type(), DataType::Utf8);
        assert_eq!(back.user_id_array().as_ref(), m.user_id_array().as_ref());
        let probe = StringArray::from(vec!["c", "a", "z"]);
        assert_eq!(
            back.dense_from_array(&probe).unwrap(),
            vec![Some(2), Some(1), None]
        );

        let ints = Int64Array::from(vec![7, -3, 1 << 40]);
        let back = IdMap::from_user_ids(&ints).unwrap();
        assert_eq!(back.len(), 3);
        let probe = Int64Array::from(vec![1 << 40, 7]);
        assert_eq!(
            back.dense_from_array(&probe).unwrap(),
            vec![Some(2), Some(0)]
        );

        let twice = Int64Array::from(vec![1, 2, 1]);
        assert_eq!(
            IdMap::from_user_ids(&twice).unwrap_err(),
            IdError::DuplicateId
        );
        let null = Int64Array::from(vec![Some(1), None]);
        assert_eq!(IdMap::from_user_ids(&null).unwrap_err(), IdError::Null);
        let empty = Int64Array::from(Vec::<i64>::new());
        assert!(IdMap::from_user_ids(&empty).unwrap().is_empty());
    }

    /// Integer and string id columns of `n` ids, some of them long, with a
    /// non-ASCII one, and integers that are neither compact nor ordered.
    fn stored_columns(n: usize) -> [ArrayRef; 2] {
        let ints: Vec<i64> = (0..n as i64)
            .map(|i| match i % 3 {
                0 => i * 1_000_003,
                1 => -i * 17 - 1,
                _ => i64::MAX - i,
            })
            .collect();
        let strings: Vec<String> = (0..n)
            .map(|i| match i % 4 {
                0 => format!("did:plc:{i:024}"),
                1 => format!("n{i}"),
                2 => format!("h\u{e9}llo-{i}"),
                _ => format!("{i}"),
            })
            .collect();
        [
            Arc::new(Int64Array::from(ints)),
            Arc::new(StringArray::from(strings)),
        ]
    }

    /// Every id of `ids` resolves to its own row in `map`, ids that are not
    /// there and nulls to nothing, and the map names its nodes as `ids`.
    fn assert_finds_every_id(map: &IdMap, ids: &ArrayRef) {
        assert_eq!(map.len(), ids.len());
        assert_eq!(map.user_id_array().as_ref(), ids.as_ref());
        let dense = map.dense_from_array(ids.as_ref()).unwrap();
        let expected: Vec<Option<u32>> = (0..ids.len() as u32).map(Some).collect();
        assert_eq!(dense, expected);
        let rows = [0u32, ids.len() as u32 - 1, ids.len() as u32 / 2];
        assert_eq!(
            map.gather_user(&rows).as_ref(),
            arrow::compute::take(
                ids.as_ref(),
                &arrow::array::UInt32Array::from(rows.to_vec()),
                None
            )
            .unwrap()
            .as_ref()
        );
        let missing: ArrayRef = match map {
            IdMap::Int64 { .. } => Arc::new(Int64Array::from(vec![Some(4), None, Some(i64::MIN)])),
            IdMap::Utf8 { .. } => Arc::new(StringArray::from(vec![Some("nobody"), None, Some("")])),
        };
        assert_eq!(
            map.dense_from_array(missing.as_ref()).unwrap(),
            vec![None, None, None]
        );
    }

    #[test]
    fn a_stored_id_column_is_wrapped_and_its_table_built_on_the_first_lookup() {
        for n in [1, 2, 7, 1000, PARALLEL_MIN_ROWS + 11] {
            for ids in stored_columns(n) {
                let map = IdMap::from_stored_ids(Arc::clone(&ids), None).unwrap();
                // The column itself, not a copy; and no table yet.
                assert!(Arc::ptr_eq(&map.user_id_array(), &ids));
                let built = match &map {
                    IdMap::Int64 { to_dense, .. } => match &to_dense.table {
                        IntTable::Stored { table, .. } => table.slots.get().is_some(),
                        _ => panic!("a stored table"),
                    },
                    IdMap::Utf8 { ids, .. } => match &ids.table {
                        StrTable::Stored(table) => table.slots.get().is_some(),
                        StrTable::Hash(_) => panic!("a stored table"),
                    },
                };
                assert!(!built);
                // Naming nodes, and resolving no ids, do not build it.
                map.gather_user(&[0]);
                map.dense_from_array(ids.slice(0, 0).as_ref()).unwrap();
                assert_finds_every_id(&map, &ids);
                // The same numbering as the map built by interning.
                let eager = IdMap::from_user_ids(ids.as_ref()).unwrap();
                assert_eq!(
                    eager.dense_from_array(ids.as_ref()).unwrap(),
                    map.dense_from_array(ids.as_ref()).unwrap()
                );
            }
        }
    }

    #[test]
    fn a_lookup_table_round_trips_and_is_the_same_from_any_map() {
        for n in [1, 5, 1000, PARALLEL_MIN_ROWS + 11] {
            for ids in stored_columns(n) {
                // From the interned map, the lazily built one, and with any
                // number of threads: one table.
                let interned = IdMap::from_user_ids(ids.as_ref()).unwrap();
                let table = interned.lookup_table().unwrap();
                assert_eq!(table.len() as u64, lookup_table_len(n));
                assert_eq!(table.iter().filter(|&&s| s != MISS).count(), n);
                let lazy = IdMap::from_stored_ids(Arc::clone(&ids), None).unwrap();
                assert_eq!(lazy.lookup_table().unwrap(), table);
                let one = rayon::ThreadPoolBuilder::new()
                    .num_threads(1)
                    .build()
                    .unwrap();
                assert_eq!(one.install(|| interned.lookup_table().unwrap()), table);

                let map = IdMap::from_stored_ids(Arc::clone(&ids), Some(table.clone())).unwrap();
                assert_finds_every_id(&map, &ids);
                assert_eq!(map.lookup_table().unwrap(), table);
            }
        }
        // An empty graph has a table of one empty slot.
        let empty: ArrayRef = Arc::new(StringArray::from(Vec::<&str>::new()));
        let table = IdMap::from_stored_ids(Arc::clone(&empty), None)
            .unwrap()
            .lookup_table()
            .unwrap();
        assert_eq!(table.as_ref(), &[MISS]);
        let map = IdMap::from_stored_ids(empty, Some(table)).unwrap();
        assert_eq!(
            map.dense_from_array(&StringArray::from(vec!["a"])).unwrap(),
            vec![None]
        );
    }

    /// The stored hashes are part of the index file format: these values
    /// must never change.
    #[test]
    fn the_stored_hashes_are_fixed() {
        assert_eq!(stored_hash_i64(0), 0);
        assert_eq!(stored_hash_i64(1), 0xb456_bcfc_34c2_cb2c);
        assert_eq!(stored_hash_i64(-1), 0x64b5_720b_4b82_5f21);
        assert_eq!(stored_hash_str(""), 0);
        assert_eq!(stored_hash_str("a"), 0x22ff_c5de_116a_704f);
        assert_eq!(stored_hash_str("12345678"), 0xbfc9_3f21_27fd_2a1f);
        assert_eq!(
            stored_hash_str("did:plc:abcdefghijklmnopqrstuvwx"),
            0xc6ca_9151_df88_303c
        );
        // A table laid out by hand: three slots for one id.
        let one: ArrayRef = Arc::new(Int64Array::from(vec![1]));
        let table = IdMap::from_stored_ids(one, None)
            .unwrap()
            .lookup_table()
            .unwrap();
        // 0xb456… * 2 >> 64 = 1.
        assert_eq!(table.as_ref(), &[MISS, 0]);
    }

    #[test]
    fn a_stored_column_with_an_id_twice_is_an_error_at_the_first_lookup() {
        let ints: ArrayRef = Arc::new(Int64Array::from(vec![5, 6, 5]));
        let strings: ArrayRef = Arc::new(StringArray::from(vec!["a", "b", "c", "b"]));
        for ids in [ints, strings] {
            let map = IdMap::from_stored_ids(Arc::clone(&ids), None).unwrap();
            assert_eq!(map.len(), ids.len());
            assert_eq!(
                map.dense_from_array(ids.as_ref()).unwrap_err(),
                IdError::DuplicateId
            );
            assert_eq!(map.lookup_table().unwrap_err(), IdError::DuplicateId);
        }
    }

    #[test]
    fn stored_ids_that_are_null_or_whose_table_does_not_fit_are_errors() {
        let null: ArrayRef = Arc::new(Int64Array::from(vec![Some(1), None]));
        assert_eq!(
            IdMap::from_stored_ids(null, None).unwrap_err(),
            IdError::Null
        );
        let ids: ArrayRef = Arc::new(Int64Array::from(vec![1, 2, 3]));
        let short = ScalarBuffer::from(vec![MISS; 4]);
        assert_eq!(
            IdMap::from_stored_ids(Arc::clone(&ids), Some(short)).unwrap_err(),
            IdError::LookupTable
        );
        let floats: ArrayRef = Arc::new(arrow::array::Float64Array::from(vec![1.0]));
        assert!(matches!(
            IdMap::from_stored_ids(floats, None).unwrap_err(),
            IdError::Unsupported(_)
        ));
    }

    /// A table that is not the ids' (a damaged file whose checksum matched)
    /// only ever misses: no panic, no endless walk, no wrong node.
    #[test]
    fn a_wrong_lookup_table_only_misses() {
        for ids in stored_columns(100) {
            let n = ids.len();
            let len = lookup_table_len(n) as usize;
            let good = IdMap::from_stored_ids(Arc::clone(&ids), None)
                .unwrap()
                .lookup_table()
                .unwrap();
            let full_of = |slot: u32| ScalarBuffer::from(vec![slot; len]);
            let reversed: Vec<u32> = good.iter().rev().copied().collect();
            let tables = [
                full_of(MISS),
                // No empty slot, and indices out of range.
                full_of(0),
                full_of(n as u32 + 5),
                ScalarBuffer::from(reversed),
            ];
            for table in tables {
                let map = IdMap::from_stored_ids(Arc::clone(&ids), Some(table)).unwrap();
                let dense = map.dense_from_array(ids.as_ref()).unwrap();
                for (i, d) in dense.into_iter().enumerate() {
                    assert!(d.is_none() || d == Some(i as u32));
                }
            }
        }
    }

    #[test]
    fn large_and_view_strings_are_stored_ids_too() {
        let strings = vec!["a", "bb", "a-long-string-id-that-is-not-inlined"];
        let large: ArrayRef = Arc::new(LargeStringArray::from(strings.clone()));
        let view: ArrayRef = Arc::new(StringViewArray::from(strings.clone()));
        let expected: ArrayRef = Arc::new(StringArray::from(strings));
        for ids in [large, view] {
            let map = IdMap::from_stored_ids(Arc::clone(&ids), None).unwrap();
            assert_eq!(map.user_id_array().as_ref(), expected.as_ref());
            assert_eq!(
                map.dense_from_array(ids.as_ref()).unwrap(),
                vec![Some(0), Some(1), Some(2)]
            );
        }
    }

    #[test]
    fn interning_into_stored_ids_moves_them_to_a_growing_table() {
        for ids in stored_columns(50) {
            let mut map = IdMap::from_stored_ids(Arc::clone(&ids), None).unwrap();
            let next = match &mut map {
                IdMap::Int64 { .. } => map.intern_i64(4).unwrap(),
                IdMap::Utf8 { .. } => map.intern_str("someone new").unwrap(),
            };
            assert_eq!(next, 50);
            map.freeze();
            assert_eq!(map.len(), 51);
            let dense = map.dense_from_array(ids.as_ref()).unwrap();
            assert_eq!(dense, (0..50).map(Some).collect::<Vec<_>>());
        }
    }
}
