//! User-id ⇄ dense-index mapping.
//!
//! User node ids are arbitrary (gappy `i64`, strings, UUIDs). Kernels never see
//! them: index construction builds a bidirectional mapping from user ids to dense
//! `u32` internal indices `0..n`, and all kernels operate in `u32` space over flat
//! arrays. Results translate back to user ids only when materializing output.
//!
//! The skeleton implements the `i64` case (the common one — integer node ids).
//! String / UUID ids follow the same shape with a different key type; that is
//! future work and deliberately not abstracted prematurely here.

use std::collections::HashMap;

use arrow::array::{Array, Int64Array};

/// Bidirectional map between arbitrary user ids and dense `u32` indices.
///
/// `u32` caps the node space at ~4.29B nodes — the correct trade for cache
/// behaviour at the v0.1 target scale. A `u64` node space is a future feature flag.
#[derive(Debug, Clone, Default)]
pub struct IdMap {
    /// dense index -> user id (gathered by position on output)
    to_user: Vec<i64>,
    /// user id -> dense index (hash map consulted on input)
    to_dense: HashMap<i64, u32>,
}

/// Error raised when `src`/`dst` contains a null. The spec's lean is
/// *error by default*, with an `on_null="drop"` opt-in resolved at a higher layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NullNodeId;

impl std::fmt::Display for NullNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "null value in src/dst column; set on_null=\"drop\" to skip"
        )
    }
}

impl std::error::Error for NullNodeId {}

impl IdMap {
    /// Build a map from an iterator of user ids, assigning dense indices in
    /// first-seen order.
    pub fn from_ids<I: IntoIterator<Item = i64>>(ids: I) -> Self {
        let mut map = IdMap::default();
        for id in ids {
            map.intern(id);
        }
        map
    }

    /// Build the id map from the two edge endpoint columns, and return the edges
    /// re-expressed in dense `u32` space (`src_dense`, `dst_dense`) ready for
    /// [`Topology::build`]. Errors on any null endpoint.
    ///
    /// [`Topology::build`]: crate::topology::Topology::build
    pub fn from_edge_arrays(
        src: &Int64Array,
        dst: &Int64Array,
    ) -> Result<(Self, Vec<u32>, Vec<u32>), NullNodeId> {
        assert_eq!(src.len(), dst.len(), "src/dst length mismatch");
        let m = src.len();
        let mut map = IdMap::default();
        let mut src_dense = Vec::with_capacity(m);
        let mut dst_dense = Vec::with_capacity(m);
        for i in 0..m {
            if src.is_null(i) || dst.is_null(i) {
                return Err(NullNodeId);
            }
            src_dense.push(map.intern(src.value(i)));
            dst_dense.push(map.intern(dst.value(i)));
        }
        Ok((map, src_dense, dst_dense))
    }

    /// Intern a user id, returning its dense index (assigning a new one if unseen).
    #[inline]
    pub fn intern(&mut self, user: i64) -> u32 {
        if let Some(&d) = self.to_dense.get(&user) {
            return d;
        }
        let d = self.to_user.len() as u32;
        self.to_user.push(user);
        self.to_dense.insert(user, d);
        d
    }

    /// Dense index for a user id, if present.
    #[inline]
    pub fn dense(&self, user: i64) -> Option<u32> {
        self.to_dense.get(&user).copied()
    }

    /// User id for a dense index. Panics if out of range (kernels never produce
    /// out-of-range indices).
    #[inline]
    pub fn user(&self, dense: u32) -> i64 {
        self.to_user[dense as usize]
    }

    /// Number of distinct nodes.
    #[inline]
    pub fn len(&self) -> usize {
        self.to_user.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.to_user.is_empty()
    }

    /// The dense-index -> user-id vector, for gathering result columns in bulk.
    pub fn user_ids(&self) -> &[i64] {
        &self.to_user
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_in_first_seen_order() {
        let m = IdMap::from_ids([100, 7, 100, 42, 7]);
        assert_eq!(m.len(), 3);
        assert_eq!(m.dense(100), Some(0));
        assert_eq!(m.dense(7), Some(1));
        assert_eq!(m.dense(42), Some(2));
        assert_eq!(m.user(0), 100);
        assert_eq!(m.dense(999), None);
    }

    #[test]
    fn errors_on_null_endpoint() {
        let src = Int64Array::from(vec![Some(1), None]);
        let dst = Int64Array::from(vec![Some(2), Some(3)]);
        assert_eq!(IdMap::from_edge_arrays(&src, &dst).err(), Some(NullNodeId));
    }
}
