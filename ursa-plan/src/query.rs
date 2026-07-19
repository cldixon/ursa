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

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{Array, Float64Array, Int64Array, RecordBatch};
use arrow::compute::{cast, concat_batches};
use arrow::datatypes::{DataType, SchemaRef};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{Expr, Extension, JoinType, LogicalPlan};
use datafusion::prelude::{col, lit};
use serde::Deserialize;
use ursa_core::algo::AggKind;
use ursa_core::IdMap;

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
    // neighbours().agg(...) fields
    #[serde(default)]
    agg_fn: Option<String>,
    #[serde(default)]
    agg_column: Option<String>,
}

impl ColumnSpec {
    fn to_algo(&self) -> Result<GraphAlgo> {
        Ok(match self.kind.as_str() {
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
        })
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

fn parse_agg(name: &str) -> Result<AggKind> {
    match name {
        "mean" => Ok(AggKind::Mean),
        "sum" => Ok(AggKind::Sum),
        "min" => Ok(AggKind::Min),
        "max" => Ok(AggKind::Max),
        "count" => Ok(AggKind::Count),
        "n_unique" => Ok(AggKind::NUnique),
        other => Err(DataFusionError::NotImplemented(format!(
            "neighbours aggregation {other:?} is not supported (use mean/sum/min/max/count/n_unique)"
        ))),
    }
}

/// Gather a numeric node-attribute column into a dense, IdMap-aligned vector:
/// `result[d]` is the attribute of the node at dense index `d`, or `None` if that
/// node has no attribute row (or a null value).
fn dense_attr_column(
    nodes: &RecordBatch,
    id_col: &str,
    attr_col: &str,
    ids: &IdMap,
) -> Result<Vec<Option<f64>>> {
    let schema = nodes.schema();
    let id_idx = schema
        .index_of(id_col)
        .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let attr_idx = schema.index_of(attr_col).map_err(|_| {
        DataFusionError::Execution(format!(
            "neighbors().agg(): column {attr_col:?} not found in the node attribute table"
        ))
    })?;

    let id_arr = cast(nodes.column(id_idx), &DataType::Int64)
        .map_err(|e| DataFusionError::ArrowError(e, None))?;
    let id_arr = id_arr.as_any().downcast_ref::<Int64Array>().unwrap();
    let attr_arr = cast(nodes.column(attr_idx), &DataType::Float64).map_err(|_| {
        DataFusionError::NotImplemented(format!(
            "neighbors().agg() supports numeric attribute columns in v0.1; {attr_col:?} is not numeric"
        ))
    })?;
    let attr_arr = attr_arr.as_any().downcast_ref::<Float64Array>().unwrap();

    let mut map: HashMap<i64, f64> = HashMap::with_capacity(nodes.num_rows());
    for i in 0..nodes.num_rows() {
        if id_arr.is_null(i) || attr_arr.is_null(i) {
            continue;
        }
        map.insert(id_arr.value(i), attr_arr.value(i));
    }
    Ok(ids
        .user_ids()
        .iter()
        .map(|uid| map.get(uid).copied())
        .collect())
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
///
/// The base is a [`GraphAlgorithmNode`] emitting `(id, values...)`. When `nodes`
/// is supplied (a node **attribute** table with id column `nodes_id`), the
/// algorithm outputs are LEFT-joined onto it by id — so the result carries the
/// attribute columns plus the computed columns, and `filter`/`sort` can reference
/// either. Filters/sort/limit are stock DataFusion `DataFrame` operations.
#[allow(clippy::too_many_arguments)]
pub fn execute_node_query(
    src: &Int64Array,
    dst: &Int64Array,
    columns_json: &str,
    filters: &[Comparison],
    sort: Option<(String, bool)>,
    limit: Option<usize>,
    nodes: Option<RecordBatch>,
    nodes_id: Option<String>,
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
    let nodes_id_name = nodes_id.clone().unwrap_or_else(|| "id".to_string());
    let mut columns: Vec<OutputColumn> = Vec::with_capacity(specs.len());
    for spec in &specs {
        if spec.kind == "neighbors_agg" {
            let nodes_ref = nodes.as_ref().ok_or_else(|| {
                DataFusionError::NotImplemented(
                    "neighbors().agg() needs a node attribute table \
                     (ur.from_arrow(nodes, id=...))"
                        .into(),
                )
            })?;
            let attr_col = spec.agg_column.as_deref().ok_or_else(|| {
                DataFusionError::Execution("neighbors().agg() is missing its column".into())
            })?;
            let attr = dense_attr_column(nodes_ref, &nodes_id_name, attr_col, &ids)?;
            let direction = parse_direction(spec.direction.as_deref().unwrap_or("out"))?;
            let agg = parse_agg(spec.agg_fn.as_deref().unwrap_or("mean"))?;
            columns.push(OutputColumn::NeighborAgg {
                name: spec.name.clone(),
                attr: Arc::new(attr),
                direction: direction.into(),
                agg,
            });
        } else {
            let algo = spec.to_algo()?;
            if !is_executable(&algo) {
                return Err(DataFusionError::NotImplemented(format!(
                    "graph algorithm {algo:?} is not wired into the execution path"
                )));
            }
            columns.push(OutputColumn::Algo {
                name: spec.name.clone(),
                algo,
            });
        }
    }

    let graph_plan = LogicalPlan::Extension(Extension {
        node: Arc::new(GraphAlgorithmNode::new(topology, ids, columns)),
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| DataFusionError::Execution(format!("failed to build runtime: {e}")))?;
    runtime.block_on(async move {
        let ctx = graph_session();
        let graph_df = ctx.execute_logical_plan(graph_plan).await?;

        // Base frame: either the graph output alone, or a node attribute table
        // with the graph output left-joined onto it by id.
        let mut df = match nodes {
            Some(batch) => {
                let id_col = nodes_id.unwrap_or_else(|| "id".to_string());
                let nodes_df = ctx.read_batch(batch)?;
                // Rename the graph id so the join keys don't collide, then drop it.
                let graph_df = graph_df.with_column_renamed("id", "__ursa_gid")?;
                nodes_df
                    .join(
                        graph_df,
                        JoinType::Left,
                        &[id_col.as_str()],
                        &["__ursa_gid"],
                        None,
                    )?
                    .drop_columns(&["__ursa_gid"])?
            }
            None => graph_df,
        };

        for f in filters {
            df = df.filter(comparison_expr(f)?)?;
        }
        if let Some((column, descending)) = sort {
            df = df.sort(vec![col(&column).sort(!descending, false)])?;
        }
        if let Some(n) = limit {
            df = df.limit(0, Some(n))?;
        }

        let out_schema: SchemaRef = Arc::new(df.schema().as_arrow().clone());
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
            None,
            None,
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
            None,
            None,
        );
        assert!(err.is_err());
    }

    #[test]
    fn joins_algorithm_output_onto_node_attributes() {
        use arrow::array::StringArray;
        use arrow::datatypes::{DataType, Field, Schema};

        let (src, dst) = diamond();
        // Attribute table: ids 0..3 with a "region" column; note id order differs
        // from the graph's IdMap order, so the join must realign by id.
        let nodes = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("region", DataType::Utf8, false),
            ])),
            vec![
                Arc::new(Int64Array::from(vec![3, 2, 1, 0])),
                Arc::new(StringArray::from(vec!["w", "x", "y", "z"])),
            ],
        )
        .unwrap();

        let batch = execute_node_query(
            &src,
            &dst,
            r#"[{"name":"indeg","kind":"degree","direction":"in"}]"#,
            &[],
            Some(("id".into(), false)),
            None,
            Some(nodes),
            Some("id".into()),
        )
        .unwrap();

        // Columns: id, region (attr), indeg (algo)
        let schema = batch.schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert!(names.contains(&"region"));
        assert!(names.contains(&"indeg"));
        assert_eq!(batch.num_rows(), 4);
    }

    #[test]
    fn neighbor_aggregation_over_an_attribute() {
        use arrow::datatypes::{DataType, Field, Schema};

        // hub: 1->0, 2->0, 3->0  (node 0's in-neighbours are 1,2,3)
        let src = Int64Array::from(vec![1, 2, 3]);
        let dst = Int64Array::from(vec![0, 0, 0]);
        let nodes = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("capacity", DataType::Int64, false),
            ])),
            vec![
                Arc::new(Int64Array::from(vec![0, 1, 2, 3])),
                Arc::new(Int64Array::from(vec![0, 10, 20, 30])),
            ],
        )
        .unwrap();

        let batch = execute_node_query(
            &src,
            &dst,
            r#"[{"name":"nbr_cap","kind":"neighbors_agg","agg_fn":"mean","agg_column":"capacity","direction":"in"}]"#,
            &[],
            Some(("id".into(), false)),
            None,
            Some(nodes),
            Some("id".into()),
        )
        .unwrap();

        // Find node 0's row and check the mean of its in-neighbours' capacities.
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let schema = batch.schema();
        let nbr_idx = schema.index_of("nbr_cap").unwrap();
        let nbr = batch
            .column(nbr_idx)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        let row0 = (0..ids.len()).find(|&i| ids.value(i) == 0).unwrap();
        assert!((nbr.value(row0) - 20.0).abs() < 1e-12); // mean(10, 20, 30)
    }
}
