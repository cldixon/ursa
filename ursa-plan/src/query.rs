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
//! Each `filter` crosses as a serialized predicate-expression JSON string and is
//! lowered through [`crate::expr`] — the same seam a `weight=` expression uses — so
//! the full predicate algebra (comparisons, boolean combinators, arithmetic, `col
//! <op> col`, string/bool equality) is available. Multiple `.filter()` calls arrive
//! as separate strings and are conjoined by DataFusion's per-filter application.

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{Array, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::compute::cast;
use arrow::datatypes::{DataType, SchemaRef};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{Expr, Extension, JoinType, LogicalPlan};
use datafusion::prelude::{col, DataFrame};
use serde::Deserialize;
use ursa_core::algo::AggKind;
use ursa_core::{IdMap, Topology};

use crate::logical::{Direction, GraphAlgo};
use crate::node::{GraphAlgorithmNode, HopNode, RandomWalkNode, ShortestPathNode};
use crate::planner::graph_session;
use crate::result::{path_schema, OutputColumn};
use crate::weight::evaluate_weight;

/// One requested output column, deserialized from the Python query IR.
///
/// `deny_unknown_fields` makes an unrecognized JSON key a hard error rather than
/// a silent drop — so a parameter added on the Python side that isn't wired here
/// fails loudly instead of being ignored (the review's silent-drop class).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    // community / centrality fields
    #[serde(default)]
    sample: Option<f64>,
    #[serde(default)]
    resolution: Option<f64>,
    #[serde(default)]
    seed: Option<u64>,
    // neighbours().agg(...) fields
    #[serde(default)]
    agg_fn: Option<String>,
    #[serde(default)]
    agg_column: Option<String>,
    // weighted-algorithm field: the serialized weight expression (over edge cols).
    #[serde(default)]
    weight: Option<serde_json::Value>,
    // connected_components mode: "weak" (default) or "strong".
    #[serde(default)]
    mode: Option<String>,
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
            "connected_components" => GraphAlgo::ConnectedComponents {
                strong: parse_cc_strong(self.mode.as_deref())?,
            },
            "triangle_count" => GraphAlgo::TriangleCount,
            "clustering_coefficient" => GraphAlgo::ClusteringCoefficient,
            "closeness" => GraphAlgo::Closeness,
            "betweenness" => GraphAlgo::Betweenness {
                sample: self.sample,
                seed: self.seed,
            },
            "label_propagation" => GraphAlgo::LabelPropagation {
                max_iter: self.max_iter.unwrap_or(20),
                seed: self.seed,
            },
            "louvain" => GraphAlgo::Louvain {
                resolution: self.resolution.unwrap_or(1.0),
                seed: self.seed,
            },
            other => {
                return Err(DataFusionError::NotImplemented(format!(
                    "graph algorithm {other:?} is not wired into the execution path"
                )))
            }
        })
    }
}

/// `connected_components` mode → the `strong` flag. `None`/`"weak"` → weak
/// (undirected) components; `"strong"` → strongly-connected components. The Python
/// layer already validates the mode, so an unexpected value here is a hard error.
fn parse_cc_strong(mode: Option<&str>) -> Result<bool> {
    match mode.unwrap_or("weak") {
        "weak" => Ok(false),
        "strong" => Ok(true),
        other => Err(DataFusionError::NotImplemented(format!(
            "connected_components mode must be 'weak' or 'strong'; got {other:?}"
        ))),
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

/// Gather a node-attribute column into a dense, IdMap-aligned vector of `f64`
/// keys: `result[d]` is the attribute of the node at dense index `d`, or `None`
/// if that node has no attribute row (or a null value).
///
/// Numeric columns pass their values straight through. For `n_unique`/`count`,
/// which need only distinctness and presence (never numeric magnitude), a
/// **string** column is accepted too: each distinct string is interned to a
/// stable `f64` code, so the segmented reduction counts distinct strings
/// correctly. `mean`/`sum`/`min`/`max` still require a numeric column.
fn dense_attr_column(
    nodes: &[RecordBatch],
    id_col: &str,
    attr_col: &str,
    ids: &IdMap,
    agg: AggKind,
) -> Result<Vec<Option<f64>>> {
    let Some(first) = nodes.first() else {
        return Ok(vec![None; ids.len()]);
    };
    let schema = first.schema();
    let id_idx = schema
        .index_of(id_col)
        .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let attr_idx = schema.index_of(attr_col).map_err(|_| {
        DataFusionError::Execution(format!(
            "neighbors().agg(): column {attr_col:?} not found in the node attribute table"
        ))
    })?;

    // Branch on the column's declared type, not on cast success: Arrow casts
    // Utf8 -> Float64 by producing nulls for unparseable strings rather than
    // erroring, which would silently swallow a real string column.
    let attr_dtype = schema.field(attr_idx).data_type();
    let is_string = matches!(
        attr_dtype,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
    );
    if is_string && !matches!(agg, AggKind::NUnique | AggKind::Count) {
        return Err(DataFusionError::NotImplemented(format!(
            "neighbors().agg() with mean/sum/min/max needs a numeric attribute column; \
             {attr_col:?} is a string column (only n_unique/count accept strings)"
        )));
    }

    // Scatter attribute values into a dense-indexed vector, streaming the attribute
    // batches one at a time (never concatenating them). Numeric columns pass values
    // through; string columns (n_unique/count only) intern each distinct value to a
    // stable code — the `codes` map is shared across batches so a value's code is
    // stable regardless of which batch it first appears in.
    let mut out: Vec<Option<f64>> = vec![None; ids.len()];
    let mut codes: HashMap<String, f64> = HashMap::new();
    for batch in nodes {
        // Resolve each attribute row's id to a dense index (its type must match the
        // graph's id type — dense_from_array errors on a mismatch, e.g. string attr
        // ids against an int64 graph). Rows whose id is null or absent from the
        // graph resolve to `None` and are skipped.
        let dense_of_row = resolve_dense(ids, batch.column(id_idx))?;
        if is_string {
            let attr_arr = cast(batch.column(attr_idx), &DataType::Utf8)
                .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
            let attr_arr = attr_arr.as_any().downcast_ref::<StringArray>().unwrap();
            for (i, dense) in dense_of_row.iter().enumerate() {
                let (Some(d), false) = (*dense, attr_arr.is_null(i)) else {
                    continue;
                };
                let next = codes.len() as f64;
                let code = *codes.entry(attr_arr.value(i).to_string()).or_insert(next);
                out[d as usize] = Some(code);
            }
        } else {
            let attr_arr = cast(batch.column(attr_idx), &DataType::Float64).map_err(|_| {
                DataFusionError::NotImplemented(format!(
                    "neighbors().agg() supports numeric or string attribute columns in v0.1; \
                     {attr_col:?} ({attr_dtype:?}) is neither"
                ))
            })?;
            let attr_arr = attr_arr.as_any().downcast_ref::<Float64Array>().unwrap();
            for (i, dense) in dense_of_row.iter().enumerate() {
                let (Some(d), false) = (*dense, attr_arr.is_null(i)) else {
                    continue;
                };
                out[d as usize] = Some(attr_arr.value(i));
            }
        }
    }

    Ok(out)
}

/// Lower one serialized predicate-expression JSON string to a DataFusion filter
/// expression via the shared `crate::expr` seam.
fn filter_expr(json: &str) -> Result<Expr> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| DataFusionError::Execution(format!("invalid filter expression JSON: {e}")))?;
    crate::expr::lower(&crate::expr::parse_ursa_expr(&value)?)
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
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    columns_json: &str,
    filters: &[String],
    sort: Option<(String, bool)>,
    limit: Option<usize>,
    nodes: Option<Vec<RecordBatch>>,
    nodes_id: Option<String>,
    edges: Option<Vec<RecordBatch>>,
) -> Result<Vec<RecordBatch>> {
    let specs: Vec<ColumnSpec> = serde_json::from_str(columns_json)
        .map_err(|e| DataFusionError::Execution(format!("invalid columns spec: {e}")))?;
    if specs.is_empty() {
        return Err(DataFusionError::Execution(
            "graph query has no output columns".into(),
        ));
    }
    // Validate output names up front: the result schema prepends the reserved `id`
    // column and DataFusion rejects duplicate unqualified fields, so a collision
    // would otherwise panic inside the node constructor's DFSchema conversion.
    let mut seen = std::collections::HashSet::new();
    for spec in &specs {
        if spec.name == "id" {
            return Err(DataFusionError::Execution(
                "output column name 'id' is reserved (it is the node id column); \
                 give the with_columns output a different name"
                    .into(),
            ));
        }
        if !seen.insert(spec.name.as_str()) {
            return Err(DataFusionError::Execution(format!(
                "duplicate output column name {:?}; each with_columns output needs a unique name",
                spec.name
            )));
        }
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
            let direction = parse_direction(spec.direction.as_deref().unwrap_or("out"))?;
            let agg = parse_agg(spec.agg_fn.as_deref().unwrap_or("mean"))?;
            let attr = dense_attr_column(nodes_ref, &nodes_id_name, attr_col, &ids, agg)?;
            columns.push(OutputColumn::NeighborAgg {
                name: spec.name.clone(),
                attr: Arc::new(attr),
                direction: direction.into(),
                agg,
            });
        } else {
            // `to_algo` is the single place an unknown algorithm kind is rejected.
            let algo = spec.to_algo()?;
            // A weight expression (over edge columns) is evaluated to one f64 per
            // edge row against the edge attribute batch; the kernel gathers it via
            // edge_ids. Weighting is supported on pagerank/closeness/betweenness/
            // louvain (checked below).
            let weights = match &spec.weight {
                None => None,
                Some(weight_json) => {
                    if !matches!(
                        algo,
                        GraphAlgo::PageRank { .. }
                            | GraphAlgo::Closeness
                            | GraphAlgo::Betweenness { .. }
                            | GraphAlgo::Louvain { .. }
                    ) {
                        return Err(DataFusionError::NotImplemented(format!(
                            "weight= is not supported for {:?}",
                            spec.kind
                        )));
                    }
                    let edges_ref = edges.as_ref().ok_or_else(|| {
                        DataFusionError::Execution(
                            "a weighted algorithm needs the edge table, but none was provided"
                                .into(),
                        )
                    })?;
                    let w = evaluate_weight(edges_ref, &weight_json.to_string())?;
                    if w.len() != topology.n_edges() {
                        return Err(DataFusionError::Execution(format!(
                            "weight array length ({}) does not match the edge count ({}); the edge \
                             table and the graph are misaligned",
                            w.len(),
                            topology.n_edges()
                        )));
                    }
                    Some(Arc::new(w))
                }
            };
            columns.push(OutputColumn::Algo {
                name: spec.name.clone(),
                algo,
                weights,
            });
        }
    }

    let graph_plan = LogicalPlan::Extension(Extension {
        node: Arc::new(GraphAlgorithmNode::new(topology, ids, columns)),
    });

    crate::runtime::block_on(async move {
        let ctx = graph_session();
        let graph_df = ctx.execute_logical_plan(graph_plan).await?;

        // Base frame: either the graph output alone, or a node attribute table
        // with the graph output left-joined onto it by id.
        let mut df = match nodes {
            Some(batches) => {
                let id_col = nodes_id.unwrap_or_else(|| "id".to_string());
                let nodes_df = ctx.read_batches(batches)?;
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
            df = df.filter(filter_expr(f)?)?;
        }
        if let Some((column, descending)) = sort {
            df = df.sort(vec![col(&column).sort(!descending, false)])?;
        }
        if let Some(n) = limit {
            df = df.limit(0, Some(n))?;
        }

        collect_batches(df).await
    })?
}

/// Build and execute one `hop` traversal as a single DataFusion plan.
///
/// A [`HopNode`] emits the reached `(src, dst)` edge frame; the optional
/// relational tail (`filter` / `sort` / `limit` / `distinct`) runs as stock
/// DataFusion operations on top, so a hop composes exactly like a node query.
/// `seeds` are user ids; unknown seeds contribute nothing (matching the kernel).
#[allow(clippy::too_many_arguments)]
pub fn execute_hop_query(
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    seeds: &dyn Array,
    n: u32,
    direction: &str,
    filters: &[String],
    sort: Option<(String, bool)>,
    limit: Option<usize>,
    distinct: bool,
) -> Result<Vec<RecordBatch>> {
    let direction: ursa_core::Direction = parse_direction(direction)?.into();

    // Resolve user-id seeds to dense indices; drop unknowns/nulls (kernel-consistent).
    let seeds_dense: Vec<u32> = resolve_dense(&ids, seeds)?.into_iter().flatten().collect();

    let hop_plan = LogicalPlan::Extension(Extension {
        node: Arc::new(HopNode::new(topology, ids, seeds_dense, n, direction)),
    });

    crate::runtime::block_on(async move {
        let ctx = graph_session();
        let mut df = ctx.execute_logical_plan(hop_plan).await?;

        for f in filters {
            df = df.filter(filter_expr(f)?)?;
        }
        if distinct {
            df = df.distinct()?;
        }
        if let Some((column, descending)) = sort {
            df = df.sort(vec![col(&column).sort(!descending, false)])?;
        }
        if let Some(n) = limit {
            df = df.limit(0, Some(n))?;
        }

        collect_batches(df).await
    })?
}

/// Build and execute one `shortest_path` traversal as a single DataFusion plan.
///
/// A [`ShortestPathNode`] emits the path's `(src, dst, hop, cost)` edge frame; the same
/// optional relational tail as [`execute_hop_query`] runs on top. `source`/`target`
/// are user ids; if either is unknown the result is an empty path. Pass `weight`
/// (a serialized edge-weight expression) with the edge table for a minimum-cost
/// Dijkstra path; omit both for unweighted BFS.
#[allow(clippy::too_many_arguments)]
pub fn execute_path_query(
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    source: &dyn Array,
    target: &dyn Array,
    direction: &str,
    weight: Option<&str>,
    edges: Option<Vec<RecordBatch>>,
    filters: &[String],
    sort: Option<(String, bool)>,
    limit: Option<usize>,
    distinct: bool,
) -> Result<Vec<RecordBatch>> {
    let direction: ursa_core::Direction = parse_direction(direction)?.into();

    // A weight expression (over edge columns) becomes one non-negative f64 per
    // edge row; Dijkstra gathers it via edge_ids. Omit for unweighted BFS.
    let weights = match weight {
        None => None,
        Some(weight_json) => {
            let edges_ref = edges.as_ref().ok_or_else(|| {
                DataFusionError::Execution(
                    "weighted shortest_path needs the edge table, but none was provided".into(),
                )
            })?;
            let w = evaluate_weight(edges_ref, weight_json)?;
            if w.len() != topology.n_edges() {
                return Err(DataFusionError::Execution(format!(
                    "weight array length ({}) does not match the edge count ({})",
                    w.len(),
                    topology.n_edges()
                )));
            }
            Some(Arc::new(w))
        }
    };

    // source/target arrive as 1-element user-id arrays; resolve each to a dense
    // index. An unknown (or absent) endpoint -> no path (an empty edge frame),
    // short-circuiting the plan.
    let source = resolve_dense(&ids, source)?.into_iter().next().flatten();
    let target = resolve_dense(&ids, target)?.into_iter().next().flatten();
    let (Some(source), Some(target)) = (source, target) else {
        let empty = RecordBatch::try_new(
            path_schema(ids.user_type()),
            vec![
                ids.gather_user(&[]),
                ids.gather_user(&[]),
                Arc::new(Int64Array::from(Vec::<i64>::new())),
                Arc::new(Float64Array::from(Vec::<f64>::new())),
            ],
        )
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
        return Ok(vec![empty]);
    };

    let path_plan = LogicalPlan::Extension(Extension {
        node: Arc::new(ShortestPathNode::new(
            topology, ids, source, target, direction, weights,
        )),
    });

    crate::runtime::block_on(async move {
        let ctx = graph_session();
        let mut df = ctx.execute_logical_plan(path_plan).await?;

        for f in filters {
            df = df.filter(filter_expr(f)?)?;
        }
        if distinct {
            df = df.distinct()?;
        }
        if let Some((column, descending)) = sort {
            df = df.sort(vec![col(&column).sort(!descending, false)])?;
        }
        if let Some(n) = limit {
            df = df.limit(0, Some(n))?;
        }

        collect_batches(df).await
    })?
}

/// Build and execute one `random_walk` as a single DataFusion plan.
///
/// A [`RandomWalkNode`] emits the `(walk_id, step, node)` node frame; the same
/// optional relational tail as [`execute_hop_query`] runs on top. `starts` are
/// user ids (unknown ids are dropped, kernel-consistent). `seed` makes the walk
/// reproducible.
#[allow(clippy::too_many_arguments)]
pub fn execute_walk_query(
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    starts: &dyn Array,
    steps: u32,
    walks_per_node: u32,
    seed: Option<u64>,
    filters: &[String],
    sort: Option<(String, bool)>,
    limit: Option<usize>,
    distinct: bool,
) -> Result<Vec<RecordBatch>> {
    let starts_dense: Vec<u32> = resolve_dense(&ids, starts)?.into_iter().flatten().collect();

    let walk_plan = LogicalPlan::Extension(Extension {
        node: Arc::new(RandomWalkNode::new(
            topology,
            ids,
            starts_dense,
            steps,
            walks_per_node,
            seed,
        )),
    });

    crate::runtime::block_on(async move {
        let ctx = graph_session();
        let mut df = ctx.execute_logical_plan(walk_plan).await?;

        for f in filters {
            df = df.filter(filter_expr(f)?)?;
        }
        if distinct {
            df = df.distinct()?;
        }
        if let Some((column, descending)) = sort {
            df = df.sort(vec![col(&column).sort(!descending, false)])?;
        }
        if let Some(n) = limit {
            df = df.limit(0, Some(n))?;
        }

        collect_batches(df).await
    })?
}

/// Resolve a user-id array to dense indices (`None` per unknown/null id), mapping
/// an id-type mismatch to a clear execution error. The shared seam for every
/// traversal's seed/source/target/start resolution.
fn resolve_dense(ids: &IdMap, arr: &dyn Array) -> Result<Vec<Option<u32>>, DataFusionError> {
    ids.dense_from_array(arr)
        .map_err(|e| DataFusionError::Execution(e.to_string()))
}

/// Collect a `DataFrame` to a **batch list** that always carries the schema (an
/// empty result becomes a single zero-row batch), so a query's columns transport
/// across the FFI even when no rows match. Returning the list — rather than
/// `concat_batches`-ing it into one contiguous batch — is what keeps peak result
/// memory flat for a large hop/walk/collect output (#60).
async fn collect_batches(df: DataFrame) -> Result<Vec<RecordBatch>> {
    let out_schema: SchemaRef = Arc::new(df.schema().as_arrow().clone());
    let mut batches = df.collect().await?;
    if batches.is_empty() {
        batches.push(RecordBatch::new_empty(out_schema));
    }
    Ok(batches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::compute::concat_batches;

    /// Concatenate a query's batch-list result into one batch for assertions (the
    /// FFI now returns the list; tests still check a single materialized batch).
    fn one(batches: Vec<RecordBatch>) -> RecordBatch {
        let schema = batches[0].schema();
        concat_batches(&schema, &batches).unwrap()
    }

    fn diamond() -> (Int64Array, Int64Array) {
        // node 0 is a hub: 1->0, 2->0, 3->0, 0->1
        (
            Int64Array::from(vec![1, 2, 3, 0]),
            Int64Array::from(vec![0, 0, 0, 1]),
        )
    }

    /// Build the cached (topology, ids) pair the query fns now take.
    fn build(src: &Int64Array, dst: &Int64Array) -> (Arc<Topology>, Arc<IdMap>) {
        crate::topology::build_topology(src, dst).unwrap()
    }

    #[test]
    fn single_column_query() {
        let (src, dst) = diamond();
        let (t, ids) = build(&src, &dst);
        let batch = one(execute_node_query(
            t,
            ids,
            r#"[{"name":"pagerank","kind":"pagerank"}]"#,
            &[],
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap());
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(batch.schema().field(1).name(), "pagerank");
    }

    #[test]
    fn composed_query_filter_sort_limit() {
        let (src, dst) = diamond();
        let (t, ids) = build(&src, &dst);
        let batch = one(
            execute_node_query(
                t,
                ids,
                r#"[{"name":"pr","kind":"pagerank"},{"name":"indeg","kind":"degree","direction":"in"}]"#,
                &[r#"{"kind":"binary","op":">",
                    "left":{"kind":"col","name":"indeg"},
                    "right":{"kind":"lit","value":0.0}}"#
                    .to_string()],
                Some(("pr".into(), true)),
                Some(1),
                None,
                None,
                None,
            )
            .unwrap(),
        );
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
        let (t, ids) = build(&src, &dst);
        let err = execute_node_query(
            t,
            ids,
            r#"[{"name":"x","kind":"no_such_algorithm"}]"#,
            &[],
            None,
            None,
            None,
            None,
            None,
        );
        assert!(err.is_err());
    }

    #[test]
    fn runs_the_new_node_algorithms() {
        // A directed triangle with a pendant, exercised through each new verb to
        // confirm they lower, execute, and return a well-typed column.
        let src = Int64Array::from(vec![0, 1, 2, 2]);
        let dst = Int64Array::from(vec![1, 2, 0, 3]);
        for (kind, want) in [
            ("closeness", DataType::Float64),
            ("betweenness", DataType::Float64),
            ("label_propagation", DataType::UInt32),
            ("louvain", DataType::UInt32),
        ] {
            let (t, ids) = build(&src, &dst);
            let spec = format!(r#"[{{"name":"v","kind":"{kind}"}}]"#);
            let batch = one(
                execute_node_query(t, ids, &spec, &[], None, None, None, None, None)
                    .unwrap_or_else(|e| panic!("{kind} failed: {e}")),
            );
            assert_eq!(batch.num_rows(), 4, "{kind}");
            assert_eq!(batch.schema().field(1).data_type(), &want, "{kind}");
        }
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

        let (t, ids) = build(&src, &dst);
        let batch = one(execute_node_query(
            t,
            ids,
            r#"[{"name":"indeg","kind":"degree","direction":"in"}]"#,
            &[],
            Some(("id".into(), false)),
            None,
            Some(vec![nodes]),
            Some("id".into()),
            None,
        )
        .unwrap());

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

        let (t, ids) = build(&src, &dst);
        let batch = one(
            execute_node_query(
                t,
                ids,
                r#"[{"name":"nbr_cap","kind":"neighbors_agg","agg_fn":"mean","agg_column":"capacity","direction":"in"}]"#,
                &[],
                Some(("id".into(), false)),
                None,
                Some(vec![nodes]),
                Some("id".into()),
                None,
            )
            .unwrap(),
        );

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

    #[test]
    fn neighbor_n_unique_over_a_string_attribute() {
        use arrow::array::StringArray;
        use arrow::datatypes::{DataType, Field, Schema};

        // hub: 1->0, 2->0, 3->0. In-neighbours of 0 are 1,2,3 with regions
        // us/us/eu -> 2 distinct.
        let src = Int64Array::from(vec![1, 2, 3]);
        let dst = Int64Array::from(vec![0, 0, 0]);
        let nodes = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("region", DataType::Utf8, false),
            ])),
            vec![
                Arc::new(Int64Array::from(vec![0, 1, 2, 3])),
                Arc::new(StringArray::from(vec!["x", "us", "us", "eu"])),
            ],
        )
        .unwrap();

        let (t, ids) = build(&src, &dst);
        let batch = one(
            execute_node_query(
                t,
                ids,
                r#"[{"name":"nbr_regions","kind":"neighbors_agg","agg_fn":"n_unique","agg_column":"region","direction":"in"}]"#,
                &[],
                Some(("id".into(), false)),
                None,
                Some(vec![nodes]),
                Some("id".into()),
                None,
            )
            .unwrap(),
        );

        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let schema = batch.schema();
        let idx = schema.index_of("nbr_regions").unwrap();
        let vals = batch
            .column(idx)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        let row0 = (0..ids.len()).find(|&i| ids.value(i) == 0).unwrap();
        assert!((vals.value(row0) - 2.0).abs() < 1e-12); // {us, eu}
    }

    #[test]
    fn hop_query_returns_reached_edges() {
        // path 0 -> 1 -> 2 -> 3
        let src = Int64Array::from(vec![0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 3]);
        let seeds = Int64Array::from(vec![0]);
        let (t, ids) = build(&src, &dst);
        let batch =
            one(execute_hop_query(t, ids, &seeds, 2, "out", &[], None, None, false).unwrap());
        // from 0 within 2 hops -> reaches 1 and 2
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.schema().field(0).name(), "src");
        assert_eq!(batch.schema().field(1).name(), "dst");
        let dsts = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let mut reached: Vec<i64> = dsts.values().to_vec();
        reached.sort_unstable();
        assert_eq!(reached, vec![1, 2]);
    }

    #[test]
    fn path_query_returns_ordered_path_edges() {
        // path 0 -> 1 -> 2 -> 3
        let src = Int64Array::from(vec![0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 3]);
        let (t, ids) = build(&src, &dst);
        let (s, tg) = (Int64Array::from(vec![0]), Int64Array::from(vec![3]));
        let batch =
            one(
                execute_path_query(t, ids, &s, &tg, "out", None, None, &[], None, None, false)
                    .unwrap(),
            );
        assert_eq!(batch.num_columns(), 4);
        assert_eq!(batch.num_rows(), 3);
        let schema = batch.schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, vec!["src", "dst", "hop", "cost"]);
        let hop = batch
            .column(2)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(hop.values(), &[0, 1, 2]);
        // Unweighted path: cost is the cumulative hop count (hop + 1).
        let cost = batch
            .column(3)
            .as_any()
            .downcast_ref::<arrow::array::Float64Array>()
            .unwrap();
        assert_eq!(cost.values(), &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn path_query_unknown_endpoint_is_empty() {
        let src = Int64Array::from(vec![0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 3]);
        // 99 is not a node
        let (t, ids) = build(&src, &dst);
        let (s, tg) = (Int64Array::from(vec![0]), Int64Array::from(vec![99]));
        let batch =
            one(
                execute_path_query(t, ids, &s, &tg, "out", None, None, &[], None, None, false)
                    .unwrap(),
            );
        assert_eq!(batch.num_columns(), 4);
        assert_eq!(batch.num_rows(), 0);
    }

    #[test]
    fn path_query_weighted_errors() {
        let src = Int64Array::from(vec![0, 1]);
        let dst = Int64Array::from(vec![1, 2]);
        let (t, ids) = build(&src, &dst);
        let (s, tg) = (Int64Array::from(vec![0]), Int64Array::from(vec![2]));
        // A weighted path without the edge table cannot resolve the weight.
        let err = execute_path_query(
            t,
            ids,
            &s,
            &tg,
            "out",
            Some(r#"{"kind":"col","name":"w"}"#),
            None,
            &[],
            None,
            None,
            false,
        );
        assert!(err.is_err());
    }

    #[test]
    fn hop_query_tail_filters_and_limits() {
        // hub: 1->0, 2->0, 3->0, 0->1 ; seed 1,2,3 all reach 0 in one hop
        let src = Int64Array::from(vec![1, 2, 3, 0]);
        let dst = Int64Array::from(vec![0, 0, 0, 1]);
        let seeds = Int64Array::from(vec![1, 2, 3]);
        // one hop out: (1,0),(2,0),(3,0); keep dst == 0, limit 2
        let (t, ids) = build(&src, &dst);
        let batch = one(execute_hop_query(
            t,
            ids,
            &seeds,
            1,
            "out",
            &[r#"{"kind":"binary","op":"==",
                "left":{"kind":"col","name":"dst"},
                "right":{"kind":"lit","value":0}}"#
                .to_string()],
            None,
            Some(2),
            false,
        )
        .unwrap());
        assert_eq!(batch.num_rows(), 2);
    }

    #[test]
    fn neighbor_mean_over_a_string_attribute_errors() {
        use arrow::array::StringArray;
        use arrow::datatypes::{DataType, Field, Schema};

        let src = Int64Array::from(vec![1]);
        let dst = Int64Array::from(vec![0]);
        let nodes = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("region", DataType::Utf8, false),
            ])),
            vec![
                Arc::new(Int64Array::from(vec![0, 1])),
                Arc::new(StringArray::from(vec!["x", "y"])),
            ],
        )
        .unwrap();

        let (t, ids) = build(&src, &dst);
        let err = execute_node_query(
            t,
            ids,
            r#"[{"name":"m","kind":"neighbors_agg","agg_fn":"mean","agg_column":"region","direction":"in"}]"#,
            &[],
            None,
            None,
            Some(vec![nodes]),
            Some("id".into()),
            None,
        );
        assert!(err.is_err()); // mean over strings is not supported
    }
}
