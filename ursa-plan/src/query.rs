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
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray, UInt32Array,
};
use arrow::compute::cast;
use arrow::datatypes::{DataType, SchemaRef};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{Expr, Extension, JoinType, LogicalPlan};
use datafusion::prelude::{col, DataFrame, SessionContext};
use serde::Deserialize;
use ursa_core::algo::AggKind;
use ursa_core::{EdgeMask, IdMap, Topology};

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

/// The relational tail shared by all four graph executors — the stock DataFusion
/// operations that run on top of the graph/traversal output. Applied in a fixed
/// canonical order (see [`apply_tail`]); extensible — a future `group_by`/`join`
/// adds a field here instead of touching the four call sites.
struct Tail<'a> {
    filters: &'a [String],
    /// `(group_keys, aggs)`: group by the key columns and aggregate. Each `agg` is a
    /// serialized `alias(agg(col))` JSON. Applied right after `filters` (so `filter`
    /// is a pre-group WHERE), and it *replaces* the schema with `[keys..., aggs...]`
    /// — the later `sort`/`limit`/`rename` then reference the grouped columns.
    group_keys: &'a [String],
    aggs: &'a [String],
    distinct: bool,
    /// `(n, seed)` for a row sample; `seed = None` uses the default (reproducible).
    sample: Option<(usize, Option<u64>)>,
    sort: Option<(String, bool)>,
    limit: Option<usize>,
    /// `(old, new)` output-column relabels, applied last.
    rename: &'a [(String, String)],
}

/// Apply the relational tail to the base frame. Canonical order:
/// `filters → group_by → distinct → sample → sort → limit → rename`.
///
/// - `group_by` runs right after `filters` (pre-group WHERE) and replaces the output
///   schema, so `sort`/`limit`/`rename` see the grouped columns.
/// - `sample` runs before `sort`/`limit`, so a following `.sort().head()` re-imposes
///   a deterministic order and `head` is honored on the sampled rows.
/// - `rename` runs last and only relabels output columns.
async fn apply_tail(ctx: &SessionContext, mut df: DataFrame, tail: Tail<'_>) -> Result<DataFrame> {
    for f in tail.filters {
        df = df.filter(filter_expr(f)?)?;
    }
    if !tail.group_keys.is_empty() {
        // Clear error for an unknown group key (df.aggregate would otherwise fail
        // with an opaque schema error).
        for k in tail.group_keys {
            if !df.schema().fields().iter().any(|f| f.name() == k) {
                return Err(DataFusionError::Execution(format!(
                    "group_by() references unknown column {k:?}"
                )));
            }
        }
        let group_exprs: Vec<Expr> = tail.group_keys.iter().map(col).collect();
        let mut agg_exprs = Vec::with_capacity(tail.aggs.len());
        for a in tail.aggs {
            let v: serde_json::Value = serde_json::from_str(a).map_err(|e| {
                DataFusionError::Execution(format!("invalid agg expression JSON: {e}"))
            })?;
            let expr = crate::expr::lower(&crate::expr::parse_ursa_expr(&v)?)?;
            // Clear error for an aggregation over an unknown column (df.aggregate
            // would otherwise fail with an opaque schema error) — mirrors the
            // group-key check above and the pyarrow plain path's guard.
            for c in expr.column_refs() {
                if !df.schema().fields().iter().any(|f| f.name() == &c.name) {
                    return Err(DataFusionError::Execution(format!(
                        "agg() references unknown column {:?}",
                        c.name
                    )));
                }
            }
            agg_exprs.push(expr);
        }
        df = df.aggregate(group_exprs, agg_exprs)?;
    }
    if tail.distinct {
        df = df.distinct()?;
    }
    if let Some((n, seed)) = tail.sample {
        let batches = df.collect().await?;
        df = ctx.read_batches(sample_rows(batches, n, seed)?)?;
    }
    if let Some((column, descending)) = tail.sort {
        df = df.sort(vec![col(&column).sort(!descending, false)])?;
    }
    if let Some(n) = tail.limit {
        df = df.limit(0, Some(n))?;
    }
    for (old, new) in tail.rename {
        // with_column_renamed silently no-ops on an absent column; check first so a
        // rename of a column that doesn't exist is a clear error, not a silent drop.
        if !df.schema().fields().iter().any(|f| f.name() == old) {
            return Err(DataFusionError::Execution(format!(
                "rename() references unknown column {old:?}"
            )));
        }
        df = df.with_column_renamed(old, new)?;
    }
    Ok(df)
}

/// The shared execution spine of every query executor. Build the graph session,
/// let `build` produce the base `DataFrame` — the one part that differs per
/// executor (a traversal/graph `Extension` plan, the node-attribute join, or a
/// join of two materialized frames) — then apply the relational tail in canonical
/// order and collect. Runs on the shared runtime; the caller has released the GIL.
///
/// Each executor thus reduces to *plan/base construction*: the block-on, session
/// setup, [`apply_tail`], and [`collect_batches`] live here once instead of being
/// copy-pasted at every call site.
fn run_query<F>(build: F, tail: Tail<'_>) -> Result<Vec<RecordBatch>>
where
    F: for<'c> FnOnce(&'c SessionContext) -> Pin<Box<dyn Future<Output = Result<DataFrame>> + 'c>>,
{
    crate::runtime::block_on(async move {
        let ctx = graph_session();
        let base = build(&ctx).await?;
        let df = apply_tail(&ctx, base, tail).await?;
        collect_batches(df).await
    })?
}

/// Deterministically sample `n` rows (without replacement) from a collected batch
/// list, returned as a single batch in a stable order.
///
/// The engine may split the input across partitions/threads, and cross-partition
/// row order is not a DataFusion contract — so a position-based sample would vary
/// by thread count. To be partition-independent, rows are put in a **content-
/// canonical** order (arrow's `RowConverter` encodes each full row tuple to
/// comparable bytes) before a seeded PRNG ([`ursa_core::algo::sample_indices`])
/// selects positions. The selected set is thus a pure function of
/// `(row contents, n, seed)` — identical across thread counts and repeated runs,
/// mirroring the `random_walk` determinism guarantee. `seed = None` uses the
/// default seed; `n >= row_count` returns all rows.
fn sample_rows(batches: Vec<RecordBatch>, n: usize, seed: Option<u64>) -> Result<Vec<RecordBatch>> {
    use arrow::row::{RowConverter, SortField};

    let Some(first) = batches.first() else {
        return Ok(batches);
    };
    let schema = first.schema();
    let batch = arrow::compute::concat_batches(&schema, &batches)
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
    let r = batch.num_rows();
    if n >= r {
        return Ok(vec![batch]);
    }
    // Canonical, partition-independent order: sort row indices by the full-tuple
    // byte encoding. Depends only on values, so it is identical regardless of how
    // the rows were partitioned. Duplicate rows encode equal (interchangeable).
    let fields: Vec<SortField> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| SortField::new(f.data_type().clone()))
        .collect();
    let converter =
        RowConverter::new(fields).map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
    let rows = converter
        .convert_columns(batch.columns())
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
    let mut canonical: Vec<usize> = (0..r).collect();
    canonical.sort_by(|&a, &b| rows.row(a).cmp(&rows.row(b)));
    // Seed-deterministic selection over canonical positions (already sorted
    // ascending), mapped back to original row indices; emitted in canonical order.
    let picks = ursa_core::algo::sample_indices(r, n, seed);
    let indices = UInt32Array::from(
        picks
            .into_iter()
            .map(|p| canonical[p] as u32)
            .collect::<Vec<u32>>(),
    );
    let cols = batch
        .columns()
        .iter()
        .map(|c| arrow::compute::take(c, &indices, None))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
    let sampled = RecordBatch::try_new(schema, cols)
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
    Ok(vec![sampled])
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
    distinct: bool,
    sample: Option<(usize, Option<u64>)>,
    rename: Vec<(String, String)>,
    group_keys: Vec<String>,
    aggs: Vec<String>,
    mask: Option<Arc<EdgeMask>>,
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
        node: Arc::new(GraphAlgorithmNode::new(topology, ids, columns, mask)),
    });

    run_query(
        move |ctx| {
            Box::pin(async move {
                let graph_df = ctx.execute_logical_plan(graph_plan).await?;
                // Base frame: either the graph output alone, or a node attribute
                // table with the graph output left-joined onto it by id.
                Ok(match nodes {
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
                })
            })
        },
        Tail {
            filters,
            group_keys: &group_keys,
            aggs: &aggs,
            distinct,
            sample,
            sort,
            limit,
            rename: &rename,
        },
    )
}

/// Equi-join two already-materialized frames on shared key column(s), then apply
/// the standard relational tail to the joined frame.
///
/// This is the user-facing `frame.join(other, on=, how=)` — a join between two
/// arbitrary frames, distinct from the internal LEFT join that attaches node
/// attributes to algorithm outputs (in [`execute_node_query`]). It is
/// topology-independent: both operands are just Arrow tables, so no `GraphIndex`
/// is involved.
///
/// `on` names key columns present in **both** frames (a same-name equi-join). The
/// right side's key columns are renamed to private sentinels for the join and then
/// dropped, so each key appears once (the left copy) in the output — mirroring the
/// attribute-attach join. Non-key column-name collisions are rejected by the caller
/// before we get here. `how` is `inner` or `left`; the whole tail
/// (`filters → group_by → distinct → sample → sort → limit → rename`) then runs on
/// the joined frame.
///
/// v1 supports `inner`/`left` only. `right`/`outer` are deferred deliberately: for
/// a right-only or unmatched row the left key is null, so simply dropping the
/// right key (as we do for `inner`/`left`, where the left key always survives)
/// would lose the key value — a correct `right`/`outer` needs `coalesce(left_key,
/// right_key)`, a follow-up.
#[allow(clippy::too_many_arguments)]
pub fn execute_join_query(
    left: Vec<RecordBatch>,
    right: Vec<RecordBatch>,
    on: Vec<String>,
    how: String,
    filters: &[String],
    sort: Option<(String, bool)>,
    limit: Option<usize>,
    distinct: bool,
    sample: Option<(usize, Option<u64>)>,
    rename: Vec<(String, String)>,
) -> Result<Vec<RecordBatch>> {
    if on.is_empty() {
        return Err(DataFusionError::Execution(
            "join() needs at least one key column (on=)".into(),
        ));
    }
    let join_type = match how.as_str() {
        "inner" => JoinType::Inner,
        "left" => JoinType::Left,
        other => {
            return Err(DataFusionError::NotImplemented(format!(
                "join how={other:?} is not supported yet (use 'inner' or 'left'; \
                 'right'/'outer' are a follow-up)"
            )));
        }
    };
    run_query(
        move |ctx| {
            Box::pin(async move {
                let left_df = ctx.read_batches(left)?;
                let mut right_df = ctx.read_batches(right)?;
                // Rename each right key to a private sentinel so the join keys don't
                // collide, then drop the sentinels after the join — the left key survives.
                let sentinels: Vec<String> = on.iter().map(|k| format!("__ursa_rk_{k}")).collect();
                for (k, s) in on.iter().zip(sentinels.iter()) {
                    right_df = right_df.with_column_renamed(k, s)?;
                }
                let left_keys: Vec<&str> = on.iter().map(String::as_str).collect();
                let right_keys: Vec<&str> = sentinels.iter().map(String::as_str).collect();
                let sentinel_refs: Vec<&str> = sentinels.iter().map(String::as_str).collect();
                let joined = left_df
                    .join(right_df, join_type, &left_keys, &right_keys, None)?
                    .drop_columns(&sentinel_refs)?;
                Ok(joined)
            })
        },
        Tail {
            filters,
            group_keys: &[],
            aggs: &[],
            distinct,
            sample,
            sort,
            limit,
            rename: &rename,
        },
    )
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
    sample: Option<(usize, Option<u64>)>,
    rename: Vec<(String, String)>,
) -> Result<Vec<RecordBatch>> {
    let direction: ursa_core::Direction = parse_direction(direction)?.into();

    // Resolve user-id seeds to dense indices; drop unknowns/nulls (kernel-consistent).
    let seeds_dense: Vec<u32> = resolve_dense(&ids, seeds)?.into_iter().flatten().collect();

    let hop_plan = LogicalPlan::Extension(Extension {
        node: Arc::new(HopNode::new(topology, ids, seeds_dense, n, direction)),
    });

    run_query(
        move |ctx| Box::pin(async move { ctx.execute_logical_plan(hop_plan).await }),
        Tail {
            filters,
            group_keys: &[],
            aggs: &[],
            distinct,
            sample,
            sort,
            limit,
            rename: &rename,
        },
    )
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
    sample: Option<(usize, Option<u64>)>,
    rename: Vec<(String, String)>,
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

    run_query(
        move |ctx| Box::pin(async move { ctx.execute_logical_plan(path_plan).await }),
        Tail {
            filters,
            group_keys: &[],
            aggs: &[],
            distinct,
            sample,
            sort,
            limit,
            rename: &rename,
        },
    )
}

/// The node set reached within `n` hops of `seeds` (seeds included), as a user-id
/// array — the reached region behind "graph op over a hop result" (#116).
///
/// Backs the subgraph mask that a node-valued kernel runs over: the reached nodes
/// induce a subgraph of the parent CSR. Runs the same multi-source BFS as `ur.hop`
/// but returns the reached node *set* instead of `(seed, reached)` pairs. Unknown
/// seeds are dropped (kernel-consistent).
pub fn hop_reached_nodes(
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    seeds: &dyn Array,
    n: u32,
    direction: &str,
) -> Result<ArrayRef> {
    let direction: ursa_core::Direction = parse_direction(direction)?.into();
    let seeds_dense: Vec<u32> = resolve_dense(&ids, seeds)?.into_iter().flatten().collect();
    let reached = ursa_core::algo::k_hop_reached_set(&topology, &seeds_dense, n, direction);
    Ok(ids.gather_user(&reached))
}

/// The nodes on the shortest path from `source` to `target` (inclusive), as a
/// user-id array — the reached region behind "graph op over a shortest_path result"
/// (#116). An unknown/unreachable endpoint yields an empty array (no path).
///
/// Mirrors [`execute_path_query`]'s weighting: pass `weight` (a serialized edge
/// expression) with the edge table for minimum-cost Dijkstra; omit both for
/// unweighted BFS.
#[allow(clippy::too_many_arguments)]
pub fn shortest_path_nodes(
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    source: &dyn Array,
    target: &dyn Array,
    direction: &str,
    weight: Option<&str>,
    edges: Option<Vec<RecordBatch>>,
) -> Result<ArrayRef> {
    let direction: ursa_core::Direction = parse_direction(direction)?.into();
    let source = resolve_dense(&ids, source)?.into_iter().next().flatten();
    let target = resolve_dense(&ids, target)?.into_iter().next().flatten();
    let (Some(source), Some(target)) = (source, target) else {
        return Ok(ids.gather_user(&[]));
    };

    let path = match weight {
        None => ursa_core::algo::shortest_path(&topology, source, target, direction),
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
            ursa_core::algo::shortest_path_weighted(&topology, &w, source, target, direction)
        }
    };
    Ok(ids.gather_user(&path.unwrap_or_default()))
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
    sample: Option<(usize, Option<u64>)>,
    rename: Vec<(String, String)>,
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

    run_query(
        move |ctx| Box::pin(async move { ctx.execute_logical_plan(walk_plan).await }),
        Tail {
            filters,
            group_keys: &[],
            aggs: &[],
            distinct,
            sample,
            sort,
            limit,
            rename: &rename,
        },
    )
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

    fn ints(vals: &[i64]) -> RecordBatch {
        use arrow::datatypes::{Field, Schema};
        let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, false)]));
        RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(vals.to_vec()))]).unwrap()
    }

    fn sampled_vals(batches: Vec<RecordBatch>, n: usize, seed: Option<u64>) -> Vec<i64> {
        let out = sample_rows(batches, n, seed).unwrap();
        let b = &out[0];
        b.column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .values()
            .to_vec()
    }

    #[test]
    fn sample_rows_is_partition_independent_and_seeded() {
        // The same 6 rows arriving in different batch splits (as different partition
        // layouts would produce) must yield the identical sample for a given seed —
        // the content-canonical order removes all dependence on arrival order.
        let split_a = vec![ints(&[5, 1, 4, 2, 6, 3])]; // one batch
        let split_b = vec![ints(&[3, 6, 2]), ints(&[4, 1, 5])]; // two batches, reordered
        let a = sampled_vals(split_a, 3, Some(9));
        let b = sampled_vals(split_b, 3, Some(9));
        assert_eq!(a, b);
        assert_eq!(a.len(), 3);
        // n >= row count returns all rows.
        assert_eq!(sampled_vals(vec![ints(&[7, 8])], 5, Some(1)).len(), 2);
        // a different seed can pick a different subset.
        let s9 = sampled_vals(vec![ints(&[1, 2, 3, 4, 5, 6, 7, 8])], 3, Some(9));
        let s10 = sampled_vals(vec![ints(&[1, 2, 3, 4, 5, 6, 7, 8])], 3, Some(10));
        assert_ne!(s9, s10);
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
            false,
            None,
            vec![],
            vec![],
            vec![],
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
             false, None, vec![], vec![], vec![], None)
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
            false,
            None,
            vec![],
            vec![],
            vec![],
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
            let batch = one(execute_node_query(
                t,
                ids,
                &spec,
                &[],
                None,
                None,
                None,
                None,
                None,
                false,
                None,
                vec![],
                vec![],
                vec![],
                None,
            )
            .unwrap_or_else(|e| panic!("{kind} failed: {e}")));
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
            false,
            None,
            vec![],
            vec![],
            vec![],
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
             false, None, vec![], vec![], vec![], None)
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
             false, None, vec![], vec![], vec![], None)
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
        let batch = one(execute_hop_query(
            t,
            ids,
            &seeds,
            2,
            "out",
            &[],
            None,
            None,
            false,
            None,
            vec![],
        )
        .unwrap());
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
        let batch = one(execute_path_query(
            t,
            ids,
            &s,
            &tg,
            "out",
            None,
            None,
            &[],
            None,
            None,
            false,
            None,
            vec![],
        )
        .unwrap());
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
        let batch = one(execute_path_query(
            t,
            ids,
            &s,
            &tg,
            "out",
            None,
            None,
            &[],
            None,
            None,
            false,
            None,
            vec![],
        )
        .unwrap());
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
            None,
            vec![],
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
            None,
            vec![],
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
            false,
            None,
            vec![],
            vec![],
            vec![],
            None,
        );
        assert!(err.is_err()); // mean over strings is not supported
    }

    #[test]
    fn node_query_group_by_aggregates_over_a_category() {
        use arrow::datatypes::{DataType, Field, Schema};

        // in-degrees: 0<-{1,2,3}=3, 1<-{0}=1, 2=0, 3=0
        let src = Int64Array::from(vec![1, 2, 3, 0]);
        let dst = Int64Array::from(vec![0, 0, 0, 1]);
        let nodes = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new("region", DataType::Utf8, false),
            ])),
            vec![
                Arc::new(Int64Array::from(vec![0, 1, 2, 3])),
                Arc::new(arrow::array::StringArray::from(vec![
                    "us", "us", "eu", "eu",
                ])),
            ],
        )
        .unwrap();

        let (t, ids) = build(&src, &dst);
        // Compute in-degree per node, then group by region: sum(indeg), count.
        let batch = one(
            execute_node_query(
                t,
                ids,
                r#"[{"name":"indeg","kind":"degree","direction":"in"}]"#,
                &[],
                Some(("region".into(), false)),
                None,
                Some(vec![nodes]),
                Some("id".into()),
                None,
                false,
                None,
                vec![],
                vec!["region".to_string()],
                vec![
                    r#"{"kind":"alias","name":"total","operand":{"kind":"agg","fn":"sum","operand":{"kind":"col","name":"indeg"}}}"#.to_string(),
                    r#"{"kind":"alias","name":"n","operand":{"kind":"agg","fn":"count","operand":{"kind":"col","name":"indeg"}}}"#.to_string(),
                ],
             None)
            .unwrap(),
        );

        // Output schema is [region, total, n] — the group replaces the node schema.
        let schema = batch.schema();
        assert_eq!(schema.field(0).name(), "region");
        assert!(schema.index_of("total").is_ok());
        assert!(schema.index_of("n").is_ok());
        assert_eq!(batch.num_rows(), 2); // us, eu

        let region = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        let total = batch
            .column(schema.index_of("total").unwrap())
            .as_any()
            .downcast_ref::<arrow::array::UInt64Array>()
            .unwrap();
        let us = (0..region.len())
            .find(|&i| region.value(i) == "us")
            .unwrap();
        let eu = (0..region.len())
            .find(|&i| region.value(i) == "eu")
            .unwrap();
        assert_eq!(total.value(us), 4); // indeg 3 + 1
        assert_eq!(total.value(eu), 0); // indeg 0 + 0
    }

    #[test]
    fn node_query_group_by_unknown_key_errors() {
        let src = Int64Array::from(vec![0, 1]);
        let dst = Int64Array::from(vec![1, 2]);
        let (t, ids) = build(&src, &dst);
        let err = execute_node_query(
            t,
            ids,
            r#"[{"name":"deg","kind":"degree","direction":"out"}]"#,
            &[],
            None,
            None,
            None,
            None,
            None,
            false,
            None,
            vec![],
            vec!["nope".to_string()],
            vec![
                r#"{"kind":"alias","name":"n","operand":{"kind":"agg","fn":"count","operand":{"kind":"col","name":"deg"}}}"#.to_string(),
            ],
         None);
        assert!(err.is_err()); // group_by references an unknown column
    }

    fn id_attr_batch(ids: Vec<i64>, attr_name: &str, attr: Vec<i64>) -> RecordBatch {
        use arrow::datatypes::{DataType, Field, Schema};
        RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("id", DataType::Int64, false),
                Field::new(attr_name, DataType::Int64, false),
            ])),
            vec![
                Arc::new(Int64Array::from(ids)),
                Arc::new(Int64Array::from(attr)),
            ],
        )
        .unwrap()
    }

    #[test]
    fn join_inner_by_id() {
        // left: id 0,1,2 with a; right: id 1,2,3 with b. inner -> ids 1,2 only.
        let left = id_attr_batch(vec![0, 1, 2], "a", vec![10, 11, 12]);
        let right = id_attr_batch(vec![1, 2, 3], "b", vec![21, 22, 23]);
        let batch = one(execute_join_query(
            vec![left],
            vec![right],
            vec!["id".into()],
            "inner".into(),
            &[],
            Some(("id".into(), false)),
            None,
            false,
            None,
            vec![],
        )
        .unwrap());
        // Output columns: id, a, b (the right key is dropped) — one id column.
        let schema = batch.schema();
        assert_eq!(schema.index_of("id").unwrap(), 0);
        assert!(schema.index_of("a").is_ok() && schema.index_of("b").is_ok());
        assert_eq!(schema.fields().len(), 3);
        assert_eq!(batch.num_rows(), 2); // ids 1, 2
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(ids.value(0), 1);
        assert_eq!(ids.value(1), 2);
    }

    #[test]
    fn join_left_keeps_all_left_rows() {
        // left ids 0,1,2; right ids 1,2 -> left join keeps 0 with null b.
        let left = id_attr_batch(vec![0, 1, 2], "a", vec![10, 11, 12]);
        let right = id_attr_batch(vec![1, 2], "b", vec![21, 22]);
        let batch = one(execute_join_query(
            vec![left],
            vec![right],
            vec!["id".into()],
            "left".into(),
            &[],
            Some(("id".into(), false)),
            None,
            false,
            None,
            vec![],
        )
        .unwrap());
        assert_eq!(batch.num_rows(), 3); // all left rows
        let b = batch
            .column(batch.schema().index_of("b").unwrap())
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        // id 0 has no right match -> null b.
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let row0 = (0..ids.len()).find(|&i| ids.value(i) == 0).unwrap();
        assert!(b.is_null(row0));
    }

    #[test]
    fn join_unsupported_how_errors() {
        let left = id_attr_batch(vec![0], "a", vec![1]);
        let right = id_attr_batch(vec![0], "b", vec![2]);
        let err = execute_join_query(
            vec![left],
            vec![right],
            vec!["id".into()],
            "outer".into(),
            &[],
            None,
            None,
            false,
            None,
            vec![],
        );
        assert!(err.is_err()); // right/outer deferred
    }
}
