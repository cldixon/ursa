//! Planner wiring: lower the graph logical node to its physical operator, and a
//! `SessionContext` that knows how to do so.
//!
//! [`GraphExtensionPlanner`] maps [`crate::node::GraphAlgorithmNode`] to
//! [`crate::physical::GraphAlgorithmExec`] during physical planning;
//! [`GraphQueryPlanner`] installs it into DataFusion's `DefaultPhysicalPlanner`;
//! [`graph_session`] builds a `SessionContext` with that planner registered. The
//! result: `ctx.execute_logical_plan(plan)` optimizes and executes a plan whose
//! graph nodes are ours, with everything else handled by stock DataFusion. This
//! is where future optimizer rules (push filters before traversal, fuse
//! `neighbors().agg`, share topology builds) will register.

use std::sync::Arc;

use async_trait::async_trait;
use datafusion::error::Result;
use datafusion::execution::context::{QueryPlanner, SessionState};
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::logical_expr::{LogicalPlan, UserDefinedLogicalNode};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::physical_planner::{DefaultPhysicalPlanner, ExtensionPlanner, PhysicalPlanner};
use datafusion::prelude::SessionContext;

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
            )) as Arc<dyn ExecutionPlan>));
        }
        if let Some(n) = any.downcast_ref::<HopNode>() {
            return Ok(Some(Arc::new(HopExec::new(
                n.topology.clone(),
                n.ids.clone(),
                n.seeds.clone(),
                n.n,
                n.direction,
            )) as Arc<dyn ExecutionPlan>));
        }
        if let Some(n) = any.downcast_ref::<ShortestPathNode>() {
            return Ok(Some(Arc::new(ShortestPathExec::new(
                n.topology.clone(),
                n.ids.clone(),
                n.source,
                n.target,
                n.direction,
            )) as Arc<dyn ExecutionPlan>));
        }
        if let Some(n) = any.downcast_ref::<RandomWalkNode>() {
            return Ok(Some(Arc::new(RandomWalkExec::new(
                n.topology.clone(),
                n.ids.clone(),
                n.starts.clone(),
                n.steps,
                n.walks_per_node,
                n.seed,
            )) as Arc<dyn ExecutionPlan>));
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

/// A `SessionContext` configured to plan and execute Ursa graph queries.
pub fn graph_session() -> SessionContext {
    let state = SessionStateBuilder::new()
        .with_default_features()
        .with_query_planner(Arc::new(GraphQueryPlanner))
        .build();
    SessionContext::new_with_state(state)
}
