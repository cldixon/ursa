//! Plans that run on one task, for the browser (#278).
//!
//! DataFusion runs each input partition of a [`CoalescePartitionsExec`] as its
//! own Tokio task, and so does `execute_stream` over a plan with several
//! partitions. The browser build has no Tokio runtime: its futures are driven
//! by JavaScript's event loop, so a spawn panics ("there is no reactor
//! running"). `target_partitions = 1` keeps scans and joins to one partition,
//! but a `UNION` still has one partition per input, and a graph's `nodes`
//! table is a union.
//!
//! [`SingleTask`] is a physical optimizer rule, run last in the browser build.
//! It replaces each coalesce over several partitions, and a plan whose output
//! has several partitions, with [`SerialCoalesceExec`], which reads the
//! partitions one after another on the task that polls it. A coalesce returns
//! rows in no fixed order (whichever partition is ready first), so reading
//! them in partition order is one of the orders it may produce. Graph inputs
//! from such plans are sorted by `KeepEdgeOrder` before the index is built, so
//! graph results do not depend on it either.

use std::fmt;
use std::sync::Arc;

use datafusion::common::config::ConfigOptions;
use datafusion::common::tree_node::{Transformed, TreeNode, TreeNodeRecursion};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::TaskContext;
use datafusion::physical_expr::{EquivalenceProperties, PhysicalExpr};
use datafusion::physical_optimizer::PhysicalOptimizerRule;
use datafusion::physical_plan::coalesce_partitions::CoalescePartitionsExec;
use datafusion::physical_plan::repartition::RepartitionExec;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, ExecutionPlanProperties, Partitioning,
    PlanProperties, SendableRecordBatchStream,
};
use futures::StreamExt;

/// The rule that makes every plan run on one task. See the module docs.
#[derive(Debug, Default)]
pub struct SingleTask;

impl PhysicalOptimizerRule for SingleTask {
    fn optimize(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        _config: &ConfigOptions,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let plan = plan
            .transform_up(|node| {
                if node.downcast_ref::<RepartitionExec>().is_some() {
                    // Not planned with one target partition; it would spawn.
                    return Err(DataFusionError::Internal(format!(
                        "the browser build cannot run a repartition: {}",
                        node.name()
                    )));
                }
                match node.downcast_ref::<CoalescePartitionsExec>() {
                    Some(coalesce) if partitions(coalesce.input()) > 1 => Ok(Transformed::yes(
                        Arc::new(SerialCoalesceExec::new(
                            Arc::clone(coalesce.input()),
                            coalesce.fetch(),
                        )) as Arc<dyn ExecutionPlan>,
                    )),
                    _ => Ok(Transformed::no(node)),
                }
            })?
            .data;
        if partitions(&plan) > 1 {
            return Ok(Arc::new(SerialCoalesceExec::new(plan, None)));
        }
        Ok(plan)
    }

    fn name(&self) -> &str {
        "single_task"
    }

    fn schema_check(&self) -> bool {
        true
    }
}

fn partitions(plan: &Arc<dyn ExecutionPlan>) -> usize {
    plan.output_partitioning().partition_count()
}

/// One partition holding every row of its input's partitions, read in
/// partition order on the polling task, with an optional row limit.
#[derive(Debug)]
pub struct SerialCoalesceExec {
    input: Arc<dyn ExecutionPlan>,
    fetch: Option<usize>,
    properties: Arc<PlanProperties>,
}

impl SerialCoalesceExec {
    pub fn new(input: Arc<dyn ExecutionPlan>, fetch: Option<usize>) -> Self {
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(input.schema()),
            Partitioning::UnknownPartitioning(1),
            input.properties().emission_type,
            input.properties().boundedness,
        ));
        Self {
            input,
            fetch,
            properties,
        }
    }
}

impl DisplayAs for SerialCoalesceExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SerialCoalesceExec")?;
        if let Some(fetch) = self.fetch {
            write!(f, ": fetch={fetch}")?;
        }
        Ok(())
    }
}

impl ExecutionPlan for SerialCoalesceExec {
    fn name(&self) -> &str {
        "SerialCoalesceExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.input]
    }

    fn benefits_from_input_partitioning(&self) -> Vec<bool> {
        vec![false]
    }

    fn apply_expressions(
        &self,
        _f: &mut dyn FnMut(&Arc<dyn PhysicalExpr>) -> Result<TreeNodeRecursion>,
    ) -> Result<TreeNodeRecursion> {
        Ok(TreeNodeRecursion::Continue)
    }

    fn with_new_children(
        self: Arc<Self>,
        mut children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        match (children.pop(), children.is_empty()) {
            (Some(input), true) => Ok(Arc::new(Self::new(input, self.fetch))),
            _ => Err(DataFusionError::Internal(
                "SerialCoalesceExec has one child".into(),
            )),
        }
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        if partition != 0 {
            return Err(DataFusionError::Internal(format!(
                "SerialCoalesceExec has one partition; asked for {partition}"
            )));
        }
        let input = Arc::clone(&self.input);
        // Each partition starts only when the one before it is finished.
        let batches = futures::stream::iter(0..partitions(&input))
            .flat_map(move |p| match input.execute(p, Arc::clone(&context)) {
                Ok(stream) => stream.left_stream(),
                Err(e) => futures::stream::once(async move { Err(e) }).right_stream(),
            })
            .scan(self.fetch, |remaining, batch| {
                let item = match (batch, remaining.as_mut()) {
                    (Err(e), _) => Some(Err(e)),
                    (Ok(batch), None) => Some(Ok(batch)),
                    (Ok(_), Some(0)) => None,
                    (Ok(batch), Some(left)) => {
                        let take = (*left).min(batch.num_rows());
                        *left -= take;
                        Some(Ok(batch.slice(0, take)))
                    }
                };
                futures::future::ready(item)
            });
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            self.input.schema(),
            batches,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::util::pretty::pretty_format_batches;
    use datafusion::physical_plan::collect;
    use datafusion::prelude::{SessionConfig, SessionContext};

    /// Run `sql` with several target partitions, with and without the rule,
    /// and return both results printed.
    async fn both(sql: &str) -> (String, String, Arc<dyn ExecutionPlan>) {
        let ctx = SessionContext::new_with_config(SessionConfig::new().with_target_partitions(4));
        let plan = ctx
            .sql(sql)
            .await
            .unwrap()
            .create_physical_plan()
            .await
            .unwrap();
        let expected =
            pretty_format_batches(&collect(Arc::clone(&plan), ctx.task_ctx()).await.unwrap())
                .unwrap()
                .to_string();
        let serial = SingleTask
            .optimize(plan, &ConfigOptions::default())
            .unwrap();
        let got =
            pretty_format_batches(&collect(Arc::clone(&serial), ctx.task_ctx()).await.unwrap())
                .unwrap()
                .to_string();
        (expected, got, serial)
    }

    fn has_coalesce_over_many(plan: &Arc<dyn ExecutionPlan>) -> bool {
        plan.exists(|node| {
            Ok(node
                .downcast_ref::<CoalescePartitionsExec>()
                .is_some_and(|c| partitions(c.input()) > 1))
        })
        .unwrap()
    }

    #[tokio::test]
    async fn a_union_runs_serially_with_the_same_rows() {
        // Sorted, so the parallel and serial runs must agree exactly.
        let (expected, got, plan) =
            both("SELECT x FROM (SELECT 1 AS x UNION ALL SELECT 2 UNION ALL SELECT 3) ORDER BY x")
                .await;
        assert_eq!(expected, got);
        assert_eq!(partitions(&plan), 1);
        assert!(!has_coalesce_over_many(&plan));
    }

    #[tokio::test]
    async fn a_limit_over_a_union_keeps_its_fetch() {
        let (_, got, plan) =
            both("SELECT * FROM (SELECT 1 AS x UNION ALL SELECT 2 UNION ALL SELECT 3) LIMIT 2")
                .await;
        assert_eq!(got.lines().filter(|l| l.starts_with("| ")).count(), 1 + 2);
        assert_eq!(partitions(&plan), 1);
    }

    #[tokio::test]
    async fn a_multi_partition_root_gets_one_partition() {
        let ctx = SessionContext::new_with_config(SessionConfig::new().with_target_partitions(4));
        let plan = ctx
            .sql("SELECT 1 AS x UNION ALL SELECT 2")
            .await
            .unwrap()
            .create_physical_plan()
            .await
            .unwrap();
        assert!(partitions(&plan) > 1);
        let serial = SingleTask
            .optimize(plan, &ConfigOptions::default())
            .unwrap();
        assert_eq!(partitions(&serial), 1);
        let rows: usize = collect(serial, ctx.task_ctx())
            .await
            .unwrap()
            .iter()
            .map(|b| b.num_rows())
            .sum();
        assert_eq!(rows, 2);
    }
}
