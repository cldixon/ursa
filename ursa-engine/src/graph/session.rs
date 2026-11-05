//! What `new_session` installs for graph algorithms.

use std::sync::Arc;

use datafusion::error::Result;
use datafusion::execution::context::QueryPlanner;
use datafusion::prelude::{SessionConfig, SessionContext};

use super::catalog_table::UrsaFunctions;
use datafusion::physical_optimizer::PhysicalOptimizerRule;

use super::exec::{KeepEdgeOrder, UrsaQueryPlanner};
use super::relation::GraphFunctions;

/// `config` with an index cache, and with file-scan work stealing off so a
/// graph's edges are read in file order (see [`super::exec`]).
pub(crate) fn with_algorithms(config: SessionConfig) -> SessionConfig {
    let mut config = super::index::with_index_cache(config);
    config
        .options_mut()
        .execution
        .enable_file_stream_work_stealing = false;
    config
}

/// The physical optimizer rule that keeps graph nodes' edge rows in order.
pub(crate) fn edge_order_rule() -> Arc<dyn PhysicalOptimizerRule + Send + Sync> {
    Arc::new(KeepEdgeOrder)
}

/// The query planner that plans graph nodes.
pub(crate) fn query_planner() -> Arc<dyn QueryPlanner + Send + Sync> {
    Arc::new(UrsaQueryPlanner)
}

/// Plan graph functions in `FROM` on `ctx`, over its graph registry.
pub(crate) fn register_functions(ctx: &SessionContext) -> Result<()> {
    let catalogs = Arc::clone(ctx.state().catalog_list());
    ctx.register_udtf(super::functions::URSA_FUNCTIONS, Arc::new(UrsaFunctions));
    ctx.register_relation_planner(Arc::new(GraphFunctions::new(
        super::registry_of(ctx),
        catalogs,
    )))
}
