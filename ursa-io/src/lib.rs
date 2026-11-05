//! # ursa-io
//!
//! How Ursa reads its sources: Parquet and CSV files, local or on object
//! storage, read through DataFusion into Arrow batches or straight into a
//! [`ursa_core::Topology`]. It holds no plan types, so every engine that reads
//! a file (today's DataFrame library, through `ursa-plan`) reads it the same
//! way: the same CSV options and type inference, the same Parquet statistics
//! guard, the same row order and the same error messages.
//!
//! - [`scan`]: edge and node scans, the scanned-node-table cache, and the error
//!   labels a scan puts on what it raises.
//! - [`csv`]: [`csv::CsvOptions`], CSV type inference (#216, #221) and the
//!   location of a value that does not parse (#222).
//! - [`store`]: object-store backends for a URL and the session a scan runs on.
//! - [`topology`]: Arrow endpoint columns to a topology index.
//! - [`runtime`]: the process-wide Tokio runtime blocking reads run on.
//! - [`dry`]: dry runs, which read a file's schema but no rows.
//! - [`types`]: Arrow types as messages name them.
//!
//! Errors are [`DataFusionError`]s, the engine error the scan's reads raise,
//! with Ursa's own messages carried in one (a CSV value error as
//! [`csv::CsvValueError`]); the host words them for its users.

pub mod csv;
pub mod dry;
mod parquet_stats;
pub mod runtime;
pub mod scan;
pub mod store;
pub mod topology;
pub mod types;

pub use datafusion::error::DataFusionError;

pub use self::csv::CsvOptions;
pub use scan::{
    scan_edges_batch, scan_edges_table, scan_edges_topology, scan_nodes_batch, ScanBuildError,
    ScanSource, ScannedTopology,
};
pub use topology::{build_topology, build_topology_batches};
pub use types::pa_name;
