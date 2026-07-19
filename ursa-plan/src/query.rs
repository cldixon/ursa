//! The unified graph-query entry point: one description in, one DataFusion
//! `LogicalPlan` executed, one Arrow batch out.
//!
//! This is the seam between Ursa's Python dialect and the engine. The Python
//! layer emits a small JSON IR for the output columns (each a named node-valued
//! algorithm) plus the relational tail (`filter` / `sort` / `limit`);
//! [`execute_node_query`] builds the topology once, assembles a single plan —
//! `Limit → Sort → Filter → Extension(GraphAlgorithmNode)` — and runs it on the
//! graph-aware session. Filters/sort/limit are stock DataFusion logical nodes, so
//! DataFusion optimizes and executes them; the graph node is ours.
//!
//! The filter surface is deliberately small (a conjunction of `column <op>
//! literal`); widening it is expression-lowering work that also belongs at this
//! seam (`crate::expr`).

use std::sync::Arc;

use arrow::array::{Int64Array, RecordBatch};
use arrow::compute::concat_batches;
use arrow::datatypes::SchemaRef;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{Expr, Extension, LogicalPlan, LogicalPlanBuilder};
use datafusion::prelude::{col, lit};
use serde::Deserialize;

use crate::logical::{Direction, GraphAlgo};
use crate::node::GraphAlgorithmNode;
use crate::planner::graph_session;
use crate::result::{is_executable, OutputColumn};
use crate::topology::build_topology;

/// A single `column <op> literal` comparison (`op` in `> >= < <= == !=`).
#[derive(Debug, Clone)]
pub struct Comparison {
    pub column: String,
    pub op: String,
    pub value: f64,
}

/// One requested output column, deserialized from the Python query IR.
#[derive(Debug, Deserialize)]
struct ColumnSpec {
    name: String,
    kind: String,
    #[serde(default)]
    damping: Option<f64>,
    #[serde(default)]
    max_iter: Option<u32>,
    #[serde(default)]
    tol: Option<f64>,
    #[serde(default)]
    direction: Option<String>,
}

impl ColumnSpec {
    fn to_column(&self) -> Result<OutputColumn> {
        let algo = match self.kind.as_str() {
            "pagerank" => GraphAlgo::PageRank {
                damping: self.damping.unwrap_or(0.85),
                max_iter: self.max_iter.unwrap_or(30),
                tol: self.tol.unwrap_or(1e-6),
            },
            "degree" => GraphAlgo::Degree {
                direction: parse_direction(self.direction.as_deref().unwrap_or("out"))?,
            },
            "connected_components" => GraphAlgo::ConnectedComponents { strong: false },
            "triangle_count" => GraphAlgo::TriangleCount,
            "clustering_coefficient" => GraphAlgo::ClusteringCoefficient,
            other => {
                return Err(DataFusionError::NotImplemented(format!(
                    "graph algorithm {other:?} is not wired into the execution path"
                )))
            }
        };
        Ok((self.name.clone(), algo))
    }
}

fn parse_direction(d: &str) -> Result<Direction> {
    match d {
        "out" => Ok(Direction::Out),
        "in" => Ok(Direction::In),
        "both" => Ok(Direction::Both),
        other => Err(DataFusionError::NotImplemented(format!(
            "direction must be 'out', 'in', or 'both'; got {other:?}"
        ))),
    }
}

fn comparison_expr(c: &Comparison) -> Result<Expr> {
    let column = col(&c.column);
    let v = lit(c.value);
    Ok(match c.op.as_str() {
        ">" => column.gt(v),
        ">=" => column.gt_eq(v),
        "<" => column.lt(v),
        "<=" => column.lt_eq(v),
        "==" => column.eq(v),
        "!=" => column.not_eq(v),
        other => {
            return Err(DataFusionError::NotImplemented(format!(
                "comparison operator {other:?} is not supported in collect() filters"
            )))
        }
    })
}

/// Build and execute one graph query as a single DataFusion plan.
pub fn execute_node_query(
    src: &Int64Array,
    dst: &Int64Array,
    columns_json: &str,
    filters: &[Comparison],
    sort: Option<(String, bool)>,
    limit: Option<usize>,
) -> Result<RecordBatch> {
    let (topology, ids) =
        build_topology(src, dst).map_err(|e| DataFusionError::Execution(e.to_string()))?;

    let specs: Vec<ColumnSpec> = serde_json::from_str(columns_json)
        .map_err(|e| DataFusionError::Execution(format!("invalid columns spec: {e}")))?;
    if specs.is_empty() {
        return Err(DataFusionError::Execution(
            "graph query has no output columns".into(),
        ));
    }
    let columns: Vec<OutputColumn> = specs
        .iter()
        .map(ColumnSpec::to_column)
        .collect::<Result<_>>()?;
    for (_, algo) in &columns {
        if !is_executable(algo) {
            return Err(DataFusionError::NotImplemented(format!(
                "graph algorithm {algo:?} is not wired into the execution path"
            )));
        }
    }

    // One LogicalPlan: the graph node, then the relational tail as stock nodes.
    let base = LogicalPlan::Extension(Extension {
        node: Arc::new(GraphAlgorithmNode::new(topology, ids, columns)),
    });
    let mut builder = LogicalPlanBuilder::from(base);
    for f in filters {
        builder = builder.filter(comparison_expr(f)?)?;
    }
    if let Some((column, descending)) = sort {
        builder = builder.sort(vec![col(&column).sort(!descending, false)])?;
    }
    if let Some(n) = limit {
        builder = builder.limit(0, Some(n))?;
    }
    let plan = builder.build()?;
    let out_schema: SchemaRef = Arc::new(plan.schema().as_arrow().clone());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| DataFusionError::Execution(format!("failed to build runtime: {e}")))?;
    runtime.block_on(async move {
        let ctx = graph_session();
        let df = ctx.execute_logical_plan(plan).await?;
        let batches = df.collect().await?;
        concat_batches(&out_schema, &batches).map_err(|e| DataFusionError::ArrowError(e, None))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diamond() -> (Int64Array, Int64Array) {
        // node 0 is a hub: 1->0, 2->0, 3->0, 0->1
        (
            Int64Array::from(vec![1, 2, 3, 0]),
            Int64Array::from(vec![0, 0, 0, 1]),
        )
    }

    #[test]
    fn single_column_query() {
        let (src, dst) = diamond();
        let batch = execute_node_query(
            &src,
            &dst,
            r#"[{"name":"pagerank","kind":"pagerank"}]"#,
            &[],
            None,
            None,
        )
        .unwrap();
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(batch.schema().field(1).name(), "pagerank");
    }

    #[test]
    fn composed_query_filter_sort_limit() {
        let (src, dst) = diamond();
        let batch = execute_node_query(
            &src,
            &dst,
            r#"[{"name":"pr","kind":"pagerank"},{"name":"indeg","kind":"degree","direction":"in"}]"#,
            &[Comparison {
                column: "indeg".into(),
                op: ">".into(),
                value: 0.0,
            }],
            Some(("pr".into(), true)),
            Some(1),
        )
        .unwrap();
        // Only node 0 has in-degree > 0 among the hub set; it also ranks highest.
        assert_eq!(batch.num_rows(), 1);
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(ids.value(0), 0);
    }

    #[test]
    fn unwired_algorithm_errors() {
        let (src, dst) = diamond();
        let err = execute_node_query(
            &src,
            &dst,
            r#"[{"name":"c","kind":"closeness"}]"#,
            &[],
            None,
            None,
        );
        assert!(err.is_err());
    }
}
