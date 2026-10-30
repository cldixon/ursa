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
    Array, ArrayRef, Int64Array, LargeStringArray, StringArray, StringBuilder, StringViewArray,
};
use arrow::buffer::{Buffer, OffsetBuffer, ScalarBuffer};
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
#[derive(Debug)]
pub struct EdgeInterner {
    map: Option<IdMap>,
    /// The `with_capacity` edge count.
    rows_hint: usize,
    src_dense: Vec<u32>,
    dst_dense: Vec<u32>,
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
    table: HashTable<u32>,
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
            table: HashTable::new(),
        }
    }

    fn len(&self) -> usize {
        self.store.len()
    }

    #[inline]
    fn value(&self, i: u32) -> &str {
        self.store.value(i)
    }

    /// The dense index of `id`, if interned.
    #[inline]
    fn get(&self, id: &str) -> Option<u32> {
        let hash = hash_str(id);
        self.table
            .find(hash, |&i| self.store.value(i) == id)
            .copied()
    }

    /// The dense index of `id`, interning it if new. One hash either way.
    fn intern(&mut self, id: &str) -> Result<u32, IdError> {
        let hash = hash_str(id);
        let StrIds { store, table } = self;
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
        }
    }

    /// The dense index of `user`, interning it as `to_user`'s next id if new.
    #[inline]
    fn intern(&mut self, user: i64, to_user: &mut IntIds) -> Result<u32, IdError> {
        use std::collections::hash_map::Entry;
        let next = to_user.len();
        if let IntTable::Direct { base, slot, .. } = &self.table {
            if user.wrapping_sub(*base) as u64 >= slot.len() as u64 {
                self.make_room(user, next as u64 + 1, to_user);
            }
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
            let mut map = FxHashMap::default();
            map.reserve(to_user.len());
            for (i, &id) in to_user.values().iter().enumerate() {
                map.insert(id, i as u32);
            }
            self.table = IntTable::Hash(map);
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
            IdError::IdBytesOverflow => write!(
                f,
                "the distinct string node ids total more than 2 GiB, beyond the Utf8 id column"
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
}
