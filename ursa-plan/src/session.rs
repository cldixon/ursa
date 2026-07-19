//! Session construction: a DataFusion `SessionContext` pre-wired with Ursa's
//! custom optimizer rules and (later) object-store registration.

use datafusion::prelude::SessionContext;

/// Build a DataFusion session configured for Ursa.
///
/// Skeleton: returns a default context. The real version will:
/// - register Ursa's [`crate::logical`] optimizer rules alongside DataFusion's
///   built-ins (push node-set filters before traversal, prune frontier columns,
///   fuse `neighbors().agg` into a segmented CSR reduction, share one topology
///   build across graph ops);
/// - register `object_store` backends (S3/GCS/Azure) so `scan_edges("s3://...")`
///   pushes projections/predicates into Parquet — the same underlying
///   `object_store` crate that `obstore` binds, so a user-supplied store is
///   accepted directly.
pub fn ursa_session() -> SessionContext {
    // TODO(v0.1): SessionStateBuilder with custom optimizer rules + object_store.
    SessionContext::new()
}
