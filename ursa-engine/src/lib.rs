//! Ursa's SQL engine session.
//!
//! [`new_session`] builds a DataFusion [`SessionContext`] on which a file is a
//! table: `SELECT * FROM 'edges.parquet'`, `'edges.csv.gz'`, `'edges/'`,
//! `'edges/*.parquet'`, and the same over `s3://`, `gs://`, `az://` and
//! `https://`, with object stores registered the first time a URL is read.
//!
//! Graphs are defined per session ([`graph`]). Their plan nodes and the
//! `.ursa` database file arrive in later milestones (cldixon/ursa#232).

use std::sync::Arc;

use datafusion::catalog::DynamicFileCatalog;
use datafusion::prelude::{SessionConfig, SessionContext};

pub mod error;
mod file_table;
pub mod graph;
pub mod paths;
pub mod stores;

pub use error::user_message;
pub use file_table::open_file_table;
pub use stores::{ensure_object_store, ensure_object_store_for};

/// A session with Ursa's defaults.
pub fn new_session() -> SessionContext {
    new_session_with_config(SessionConfig::new())
}

/// A session with Ursa's file tables over `config`. `information_schema` is
/// turned on, so `SHOW TABLES` and `SHOW COLUMNS` work.
pub fn new_session_with_config(config: SessionConfig) -> SessionContext {
    let config = graph::with_algorithms(graph::with_registry(
        config.with_information_schema(true),
    ));
    let ctx = SessionContext::new_with_config(config);
    // As `SessionContext::enable_url_table` does, with Ursa's factory in place
    // of DataFusion's.
    let factory = Arc::new(file_table::FileTableFactory::default());
    let catalogs = Arc::new(DynamicFileCatalog::new(
        Arc::clone(ctx.state().catalog_list()),
        Arc::clone(&factory) as _,
    ));
    let session_id = ctx.session_id();
    let ctx: SessionContext = ctx
        .into_state_builder()
        .with_session_id(session_id)
        .with_catalog_list(catalogs)
        .with_query_planner(graph::query_planner())
        .build()
        .into();
    graph::register_functions(&ctx).expect("registering a relation planner cannot fail");
    factory.attach(ctx.state_weak_ref());
    ctx
}
