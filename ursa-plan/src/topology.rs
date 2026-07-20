//! Bridge from Arrow edge columns to a shared [`ursa_core::Topology`].
//!
//! This is the "build the index" half of *Arrow at the boundaries, index in the
//! middle*: Arrow `src`/`dst` columns in, an `Arc<Topology>` plus the `IdMap`
//! that translates dense results back to user ids out. The scan layer (Parquet /
//! CSV / object storage) produces the Arrow columns; everything downstream works
//! in dense `u32` space.

use std::sync::Arc;

use arrow::array::Array;
use ursa_core::{IdError, IdMap, Topology};

/// Build the topology index from two endpoint columns. The node-id type is taken
/// from the Arrow columns — `Int64` (the fast path) or `Utf8` strings; callers
/// canonicalize other integer types to `Int64` first (see `scan.rs` / the Python
/// `_io` layer).
///
/// Returns the shared, immutable topology and the `IdMap` needed to map dense
/// kernel outputs back to user ids. Errors on a null endpoint (the spec's
/// error-by-default null policy; `on_null="drop"` is a higher-layer opt-in), on
/// mixed src/dst id types, or on an unsupported id type.
pub fn build_topology(
    src: &dyn Array,
    dst: &dyn Array,
) -> Result<(Arc<Topology>, Arc<IdMap>), IdError> {
    let (ids, src_dense, dst_dense) = IdMap::from_edge_arrays(src, dst)?;
    let topo = Topology::build(ids.len(), src_dense, dst_dense);
    Ok((Arc::new(topo), Arc::new(ids)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Int64Array;

    #[test]
    fn builds_from_arrow_columns() {
        // 0->1, 0->2, 1->2, 2->0 over gappy user ids
        let src = Int64Array::from(vec![10, 10, 20, 30]);
        let dst = Int64Array::from(vec![20, 30, 30, 10]);
        let (topo, ids) = build_topology(&src, &dst).unwrap();
        assert_eq!(topo.n_nodes(), 3);
        assert_eq!(topo.n_edges(), 4);
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn rejects_null_endpoints() {
        let src = Int64Array::from(vec![Some(1), None]);
        let dst = Int64Array::from(vec![Some(2), Some(3)]);
        assert!(build_topology(&src, &dst).is_err());
    }
}
