//! Ursa's SQL engine session.
//!
//! [`new_session`] builds a DataFusion [`SessionContext`] on which a file is a
//! table: `SELECT * FROM 'edges.parquet'`, `'edges.csv.gz'`, `'edges/'`,
//! `'edges/*.parquet'`, `'events.jsonl'`, and the same over `s3://`, `gs://`,
//! `az://` and `https://`, with object stores registered the first time a URL
//! is read. `read_parquet`, `read_csv` and `read_json` read the same with
//! options ([`read_functions`]).
//!
//! Graphs are defined per session ([`graph`]). A session may have a `.ursa`
//! database ([`database`]), which keeps its catalog between processes.

use std::sync::Arc;

use datafusion::catalog::DynamicFileCatalog;
use datafusion::prelude::{SessionConfig, SessionContext};

mod blocking;
pub mod database;
pub mod error;
mod file_table;
pub mod graph;
pub mod options;
pub mod paths;
pub mod read_functions;
pub mod serial;
pub mod stores;

pub use blocking::{run_blocking, BlockingError};
pub use error::user_message;
pub use file_table::{open_file_table, open_files, FileFormat, FileOptions};
pub use stores::{ensure_object_store, ensure_object_store_for};

/// A session with Ursa's defaults.
pub fn new_session() -> SessionContext {
    new_session_with_config(SessionConfig::new())
}

/// A session with Ursa's file tables over `config`. `information_schema` is
/// turned on, so `SHOW TABLES` and `SHOW COLUMNS` work.
///
/// In the browser (wasm32) the session plans for one partition. DataFusion runs
/// each extra partition as a spawned Tokio task, and the browser build has no
/// runtime to spawn on; its futures are driven from JavaScript's event loop, as
/// in DataFusion's own `wasmtest` crate (#278).
pub fn new_session_with_config(config: SessionConfig) -> SessionContext {
    #[cfg(target_arch = "wasm32")]
    let config = config.with_target_partitions(1);
    let config = graph::with_algorithms(graph::with_registry(config.with_information_schema(true)));
    let config = database::with_database_slot(options::with_options(config));
    let config = read_functions::with_read_tables(config);
    let ctx = SessionContext::new_with_config(config);
    // Graph schemas are added to and dropped from the unwrapped catalogs.
    graph::registry_of(&ctx).set_catalogs(Arc::clone(ctx.state().catalog_list()));
    // As `SessionContext::enable_url_table` does, with Ursa's factory in place
    // of DataFusion's.
    let factory = Arc::new(file_table::FileTableFactory::default());
    let catalogs = Arc::new(DynamicFileCatalog::new(
        Arc::clone(ctx.state().catalog_list()),
        Arc::clone(&factory) as _,
    ));
    let session_id = ctx.session_id();
    let ctx = ctx
        .into_state_builder()
        .with_session_id(session_id)
        .with_catalog_list(catalogs)
        .with_query_planner(graph::query_planner())
        .with_physical_optimizer_rule(graph::edge_order_rule());
    // In the browser, last: every plan runs on the task that polls it.
    #[cfg(target_arch = "wasm32")]
    let ctx = ctx.with_physical_optimizer_rule(Arc::new(serial::SingleTask));
    let mut state = ctx.build();
    for ext in ["jsonl", "ndjson"] {
        state
            .register_file_format(Arc::new(file_table::JsonLines(ext)), false)
            .expect("registering a file format cannot fail");
    }
    let ctx: SessionContext = state.into();
    graph::register_functions(&ctx).expect("registering a relation planner cannot fail");
    let reads = read_functions::ReadFunctions::new(ctx.state().config())
        .expect("the session config has a place for read_* tables");
    ctx.register_relation_planner(Arc::new(reads))
        .expect("registering a relation planner cannot fail");
    factory.attach(ctx.state_weak_ref());
    ctx
}
