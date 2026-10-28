//! Planner wiring: lower the graph logical node to its physical operator, and a
//! `SessionContext` that knows how to do so.
//!
//! [`GraphExtensionPlanner`] maps [`crate::node::GraphAlgorithmNode`] to
//! [`crate::physical::GraphAlgorithmExec`] during physical planning;
//! `GraphQueryPlanner` installs it into DataFusion's `DefaultPhysicalPlanner`;
//! [`graph_session`] hands out a `SessionContext` with that planner registered. The
//! result: `ctx.execute_logical_plan(plan)` optimizes and executes a plan whose
//! graph nodes are ours, with everything else handled by stock DataFusion. This
//! is where future optimizer rules (push filters before traversal, fuse
//! `neighbors().agg`, share topology builds) will register.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use datafusion::error::Result;
use datafusion::execution::context::{QueryPlanner, SessionState};
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::logical_expr::{LogicalPlan, UserDefinedLogicalNode};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::physical_planner::{DefaultPhysicalPlanner, ExtensionPlanner, PhysicalPlanner};
use datafusion::prelude::{SessionConfig, SessionContext};

use crate::node::{GraphAlgorithmNode, HopNode, RandomWalkNode, ShortestPathNode};
use crate::physical::{GraphAlgorithmExec, HopExec, RandomWalkExec, ShortestPathExec};

/// Maps each Ursa graph logical node to its `ExecutionPlan`.
#[derive(Debug)]
pub struct GraphExtensionPlanner;

#[async_trait]
impl ExtensionPlanner for GraphExtensionPlanner {
    async fn plan_extension(
        &self,
        _planner: &dyn PhysicalPlanner,
        node: &dyn UserDefinedLogicalNode,
        _logical_inputs: &[&LogicalPlan],
        _physical_inputs: &[Arc<dyn ExecutionPlan>],
        _session_state: &SessionState,
    ) -> Result<Option<Arc<dyn ExecutionPlan>>> {
        let any = node.as_any();
        if let Some(n) = any.downcast_ref::<GraphAlgorithmNode>() {
            return Ok(Some(Arc::new(GraphAlgorithmExec::new(
                n.topology.clone(),
                n.ids.clone(),
                n.columns.clone(),
                n.mask.clone(),
            )) as Arc<dyn ExecutionPlan>));
        }
        if let Some(n) = any.downcast_ref::<HopNode>() {
            return Ok(Some(Arc::new(
                HopExec::new(
                    n.topology.clone(),
                    n.ids.clone(),
                    n.seeds.clone(),
                    n.n,
                    n.direction,
                )
                .with_mask(n.mask.clone()),
            ) as Arc<dyn ExecutionPlan>));
        }
        if let Some(n) = any.downcast_ref::<ShortestPathNode>() {
            return Ok(Some(Arc::new(
                ShortestPathExec::new(
                    n.topology.clone(),
                    n.ids.clone(),
                    n.source,
                    n.target,
                    n.direction,
                    n.weights.clone(),
                )
                .with_mask(n.mask.clone()),
            ) as Arc<dyn ExecutionPlan>));
        }
        if let Some(n) = any.downcast_ref::<RandomWalkNode>() {
            return Ok(Some(Arc::new(
                RandomWalkExec::new(
                    n.topology.clone(),
                    n.ids.clone(),
                    n.starts.clone(),
                    n.steps,
                    n.walks_per_node,
                    n.seed,
                )
                .with_mask(n.mask.clone()),
            ) as Arc<dyn ExecutionPlan>));
        }
        Ok(None)
    }
}

/// A `QueryPlanner` whose physical planner knows Ursa's extension nodes.
#[derive(Debug)]
struct GraphQueryPlanner;

#[async_trait]
impl QueryPlanner for GraphQueryPlanner {
    async fn create_physical_plan(
        &self,
        logical_plan: &LogicalPlan,
        session_state: &SessionState,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let planner =
            DefaultPhysicalPlanner::with_extension_planners(vec![Arc::new(GraphExtensionPlanner)]);
        planner
            .create_physical_plan(logical_plan, session_state)
            .await
    }
}

/// The default query session state, built once: registering DataFusion's
/// built-in functions, file formats and optimizer rules costs more than many
/// small queries themselves, and none of it varies between queries.
static GRAPH_STATE: OnceLock<SessionState> = OnceLock::new();

/// A `SessionContext` configured to plan and execute Ursa graph queries.
///
/// Each call clones the shared template state, so a query gets its own context
/// (queries never register tables, but a clone keeps them independent anyway)
/// with exactly the config a fresh build would have: `target_partitions` follows
/// the machine's cores, and round-robin repartitioning is off, so a frame read as
/// one partition keeps its row order through order-keeping operators.
pub fn graph_session() -> SessionContext {
    let state = GRAPH_STATE.get_or_init(|| build_graph_state(SessionConfig::new()));
    SessionContext::new_with_state(state.clone())
}

/// [`graph_session`] with an explicit `SessionConfig` (tests pin
/// `target_partitions` with it). Builds a fresh state rather than the template.
pub fn graph_session_with_config(config: SessionConfig) -> SessionContext {
    SessionContext::new_with_state(build_graph_state(config))
}

fn build_graph_state(mut config: SessionConfig) -> SessionState {
    // A single-partition frame keeps its row order through filters and
    // projections (see `crate::lower`).
    config
        .options_mut()
        .optimizer
        .enable_round_robin_repartition = false;
    SessionStateBuilder::new()
        .with_config(config)
        .with_default_features()
        .with_query_planner(Arc::new(GraphQueryPlanner))
        .build()
}
