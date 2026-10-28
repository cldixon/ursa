//! The leaves of a query: the graph and traversal extension nodes, and the attach
//! of graph columns to a frame's rows by id.
//!
//! Each builds the DataFusion plan (or the rows) a [`crate::lower::Frame`] starts
//! from; the relational steps written after it are lowered on top of it by
//! [`crate::plan`], in written order (#191).

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Schema};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{Extension, JoinType, LogicalPlan};
use datafusion::prelude::{DataFrame, SessionContext};
use ursa_core::algo::AggKind;
use ursa_core::{EdgeMask, IdMap, Topology};

use crate::expr::GraphVerb;
use crate::logical::{Direction, GraphAlgo, LayoutKind};
use crate::node::{GraphAlgorithmNode, HopNode, RandomWalkNode, ShortestPathNode};
use crate::result::{path_schema, query_batch, OutputColumn, OutputDtype};
use crate::weight::{edge_weights, WeightCache};

/// One output column of a node query: its name and the graph verb computing it.
pub type NodeColumn = (String, GraphVerb);

impl GraphVerb {
    /// The direction of a kernel that takes `'out'` or `'both'` only (`pagerank`,
    /// `betweenness`). `'in'` is a hard error rather than a silent `'out'`.
    fn out_or_both(&self) -> Result<Direction> {
        match parse_direction(&self.direction()?)? {
            Direction::In => Err(DataFusionError::NotImplemented(format!(
                "{} does not support direction='in'; use 'out' or 'both'",
                self.verb
            ))),
            d => Ok(d),
        }
    }

    fn to_algo(&self) -> Result<GraphAlgo> {
        let p = Self::param::<f64>;
        let n = Self::param::<u32>;
        let seed = Self::param::<u64>(&self.seed)?;
        Ok(match self.verb.as_str() {
            "pagerank" => GraphAlgo::PageRank {
                damping: p(&self.damping)?.unwrap_or(0.85),
                max_iter: n(&self.max_iter)?.unwrap_or(30),
                tol: p(&self.tol)?.unwrap_or(1e-6),
                direction: self.out_or_both()?,
            },
            "layout_fr" | "layout_random" | "layout_circle" => GraphAlgo::Layout {
                kind: match self.verb.as_str() {
                    "layout_fr" => LayoutKind::Fr,
                    "layout_random" => LayoutKind::Random,
                    _ => LayoutKind::Circle,
                },
                iterations: n(&self.iterations)?.unwrap_or(300),
                k: p(&self.k)?.unwrap_or(1.0),
                gravity: p(&self.gravity)?.unwrap_or(0.02),
                seed,
            },
            "degree" => GraphAlgo::Degree {
                direction: parse_direction(&self.direction()?)?,
            },
            "connected_components" => GraphAlgo::ConnectedComponents {
                strong: parse_cc_strong(self.mode.as_ref().map(|m| m.as_str().unwrap_or("")))?,
            },
            "triangle_count" => GraphAlgo::TriangleCount,
            "clustering_coefficient" => GraphAlgo::ClusteringCoefficient,
            "closeness" => GraphAlgo::Closeness {
                direction: parse_direction(&self.direction()?)?,
            },
            "betweenness" => GraphAlgo::Betweenness {
                sample: p(&self.sample)?,
                seed,
                direction: self.out_or_both()?,
            },
            "label_propagation" => GraphAlgo::LabelPropagation {
                max_iter: n(&self.max_iter)?.unwrap_or(20),
                seed,
            },
            "louvain" => GraphAlgo::Louvain {
                resolution: p(&self.resolution)?.unwrap_or(1.0),
                seed,
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

/// Parse the optional per-column output dtype (#117). `None`/`"f64"` keep the native
/// type; `"f32"` narrows a float-valued column on emit. An unknown value is a hard
/// error (a typo shouldn't silently keep f64).
fn parse_output_dtype(dtype: Option<&str>) -> Result<OutputDtype> {
    match dtype {
        None | Some("f64") => Ok(OutputDtype::F64),
        Some("f32") => Ok(OutputDtype::F32),
        Some(other) => Err(DataFusionError::NotImplemented(format!(
            "output dtype must be 'f32' or 'f64'; got {other:?}"
        ))),
    }
}

pub(crate) fn parse_direction(d: &str) -> Result<Direction> {
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
    // erroring, which would silently swallow a real string column. A dictionary
    // column (a categorical) is a string column when its values are.
    let attr_dtype = schema.field(attr_idx).data_type();
    let value_dtype = match attr_dtype {
        DataType::Dictionary(_, value) => value.as_ref(),
        other => other,
    };
    let is_string = matches!(
        value_dtype,
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
                // Look up before inserting, so only a new value allocates a key.
                let value = attr_arr.value(i);
                let code = match codes.get(value) {
                    Some(&code) => code,
                    None => {
                        let code = codes.len() as f64;
                        codes.insert(value.to_owned(), code);
                        code
                    }
                };
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

/// Validate and build the output columns of a `with_columns` of graph verbs:
/// one [`OutputColumn`] per `(name, verb)`. `nodes` is the attribute table a
/// neighbour aggregation reads (with its id column `nodes_id`); `edges` holds the
/// edge columns a weight reads, unless `shared_weights` (the cache on the graph
/// index) already has every weight.
#[allow(clippy::too_many_arguments)]
pub fn node_columns(
    topology: &Topology,
    ids: &IdMap,
    columns: &[NodeColumn],
    nodes: Option<&[RecordBatch]>,
    nodes_id: Option<&str>,
    edges: Option<&[RecordBatch]>,
    shared_weights: Option<(&WeightCache, &str)>,
) -> Result<Vec<OutputColumn>> {
    if columns.is_empty() {
        return Err(DataFusionError::Execution(
            "graph query has no output columns".into(),
        ));
    }
    // Validate output names up front: the result schema prepends the reserved `id`
    // column and DataFusion rejects duplicate unqualified fields, so a collision
    // would otherwise panic inside the node constructor's DFSchema conversion.
    let mut seen = std::collections::HashSet::new();
    for (name, _) in columns {
        if name == "id" {
            return Err(DataFusionError::Execution(
                "output column name 'id' is reserved (it is the node id column); \
                 give the with_columns output a different name"
                    .into(),
            ));
        }
        if !seen.insert(name.as_str()) {
            return Err(DataFusionError::Execution(format!(
                "duplicate output column name {name:?}; each with_columns output needs a unique name"
            )));
        }
    }
    let nodes_id_name = nodes_id.unwrap_or("id");
    let specs = columns;
    let mut columns: Vec<OutputColumn> = Vec::with_capacity(specs.len());
    let mut weight_cache: HashMap<String, Arc<Vec<f64>>> = HashMap::new();
    for (name, spec) in specs {
        let dtype = parse_output_dtype(spec.dtype.as_ref().map(|d| d.as_str().unwrap_or("")))?;
        if spec.verb == "neighbors_agg" {
            let nodes_ref = nodes.ok_or_else(|| {
                DataFusionError::NotImplemented(
                    "neighbors().agg() needs a node attribute table \
                     (ur.from_arrow(nodes, id=...))"
                        .into(),
                )
            })?;
            let attr_col = spec.agg_column().ok_or_else(|| {
                DataFusionError::Execution("neighbors().agg() is missing its column".into())
            })?;
            let direction = parse_direction(&spec.direction()?)?;
            let agg = parse_agg(spec.agg_fn().unwrap_or("mean"))?;
            let attr = dense_attr_column(nodes_ref, nodes_id_name, attr_col, ids, agg)?;
            columns.push(OutputColumn::NeighborAgg {
                name: name.clone(),
                attr: Arc::new(attr),
                direction: direction.into(),
                agg,
                dtype, // neighbour aggregation is always float-valued
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
                            | GraphAlgo::Closeness { .. }
                            | GraphAlgo::Betweenness { .. }
                            | GraphAlgo::Louvain { .. }
                    ) {
                        return Err(DataFusionError::NotImplemented(format!(
                            "weight= is not supported for {:?}",
                            spec.verb
                        )));
                    }
                    // One evaluation per distinct expression: columns weighted by
                    // the same expression share its `Arc`, which is also what lets
                    // the kernel memo (keyed on that `Arc`) share their runs.
                    let key = weight_json.to_string();
                    let w = match weight_cache.get(&key) {
                        Some(w) => w.clone(),
                        None => {
                            let n_edges = topology.n_edges();
                            let what = "a weighted algorithm";
                            let w = match shared_weights {
                                Some((c, scope)) => c.get_or_eval(
                                    &format!("{scope}{key}"),
                                    &key,
                                    edges,
                                    n_edges,
                                    what,
                                )?,
                                None => Arc::new(edge_weights(edges, &key, n_edges, what)?),
                            };
                            weight_cache.insert(key, w.clone());
                            w
                        }
                    };
                    Some(w)
                }
            };
            let field = GraphVerb::param::<usize>(&spec.field)?.unwrap_or(0);
            // Only layout produces a second output; a field selector on anything
            // else is a caller mistake worth naming rather than silently ignoring.
            if field > 0 && !matches!(algo, GraphAlgo::Layout { .. }) {
                return Err(DataFusionError::NotImplemented(format!(
                    "{:?} emits a single column; field={field} has nothing to select",
                    spec.verb
                )));
            }
            if field > 1 {
                return Err(DataFusionError::NotImplemented(format!(
                    "a layout emits two columns (x=0, y=1); field={field} is out of range"
                )));
            }
            let column = OutputColumn::Algo {
                name: name.clone(),
                algo,
                weights,
                dtype,
                field,
            };
            // f32 narrowing is only meaningful for a float-valued column; requesting it
            // on an integer kernel (degree, components, triangle_count, ...) is a type
            // error, not a silent cast to a lossy float.
            if dtype == OutputDtype::F32 && !column.is_float_valued() {
                return Err(DataFusionError::NotImplemented(format!(
                    "dtype='f32' is only supported for float-valued kernels; {:?} emits integers",
                    spec.verb
                )));
            }
            columns.push(column);
        }
    }
    Ok(columns)
}

/// The graph leaf of a node query: a [`GraphAlgorithmNode`] emitting
/// `(id, values...)` for every node, in dense node order, as one partition.
pub fn graph_leaf(
    ctx: &SessionContext,
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    columns: Vec<OutputColumn>,
    mask: Option<Arc<EdgeMask>>,
) -> DataFrame {
    extension(
        ctx,
        Arc::new(GraphAlgorithmNode::new(topology, ids, columns, mask)),
    )
}

/// A plan of one of Ursa's extension nodes.
fn extension(
    ctx: &SessionContext,
    node: Arc<dyn datafusion::logical_expr::UserDefinedLogicalNode>,
) -> DataFrame {
    DataFrame::new(ctx.state(), LogicalPlan::Extension(Extension { node }))
}

/// Attach the graph columns `columns` to `batches` (rows with the id column
/// `id_col`, at least one batch) by id: each row takes its id's values, or nulls
/// for an id that is null or not in the graph. The rows keep their order and
/// columns; the computed columns are appended.
pub fn attach_columns(
    ctx: &SessionContext,
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    columns: Vec<OutputColumn>,
    mask: Option<Arc<EdgeMask>>,
    batches: Vec<RecordBatch>,
    id_col: &str,
) -> Result<Vec<RecordBatch>> {
    let graph = graph_batch(topology, ids.clone(), columns, mask)?;
    if let Some(gathered) = attach_by_gather(&batches, id_col, &graph, &ids)? {
        return Ok(gathered);
    }
    // An id column the gather does not read: the hash LEFT join, which coerces
    // the key types, numbered for its order.
    let ord = crate::lower::fresh_name(&batches[0].schema());
    let joined = attach_by_join(ctx, batches, id_col, graph, &ord)?;
    crate::lower::Frame::from_numbered(ctx.clone(), joined, ord).collect()
}

/// The graph columns as one `(id, values...)` batch in dense order, computed on
/// the runtime's blocking pool, as the graph node's operator does: kernels that
/// allocate per call run measurably faster off the calling (Python) thread.
fn graph_batch(
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    columns: Vec<OutputColumn>,
    mask: Option<Arc<EdgeMask>>,
) -> Result<RecordBatch> {
    crate::runtime::block_on(async move {
        tokio::task::spawn_blocking(move || query_batch(&topology, &ids, &columns, mask.as_deref()))
            .await
            .map_err(|e| DataFusionError::Execution(format!("graph kernel panicked: {e}")))?
    })?
}

/// The leaf of a `hop` traversal: a [`HopNode`] emitting the reached `(src, dst)`
/// edge frame in its own order (seed order, then BFS order), as one partition.
/// `seeds` are user ids; unknown seeds contribute nothing (matching the kernel).
pub fn hop_leaf(
    ctx: &SessionContext,
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    mask: Option<Arc<EdgeMask>>,
    seeds: &dyn Array,
    n: u32,
    direction: &str,
) -> Result<DataFrame> {
    let direction: ursa_core::Direction = parse_direction(direction)?.into();
    // Resolve user-id seeds to dense indices; drop unknowns/nulls (kernel-consistent).
    let seeds_dense: Vec<u32> = resolve_dense(&ids, seeds)?.into_iter().flatten().collect();
    Ok(extension(
        ctx,
        Arc::new(HopNode::new(topology, ids, seeds_dense, n, direction).with_mask(mask)),
    ))
}

/// The leaf of a `shortest_path`: a [`ShortestPathNode`] emitting the path's
/// `(src, dst, hop, cost)` edge frame in path order. `source`/`target` are user
/// ids; if either is unknown the result is an empty path. Pass `weight` (a
/// serialized edge-weight expression) with the edge table for a minimum-cost
/// Dijkstra path; omit both for unweighted BFS.
#[allow(clippy::too_many_arguments)]
pub fn path_leaf(
    ctx: &SessionContext,
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    mask: Option<Arc<EdgeMask>>,
    source: &dyn Array,
    target: &dyn Array,
    direction: &str,
    weight: Option<&str>,
    edges: Option<Vec<RecordBatch>>,
) -> Result<crate::lower::Frame> {
    let direction: ursa_core::Direction = parse_direction(direction)?.into();
    // source/target arrive as 1-element user-id arrays; resolve each to a dense
    // index. An unknown (or absent) endpoint -> no path (an empty edge frame).
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
        return crate::lower::Frame::from_batches(ctx.clone(), vec![empty]);
    };
    // A weight expression (over edge columns) becomes one non-negative f64 per
    // edge row; Dijkstra gathers it via edge_ids. Evaluated only once both
    // endpoints resolve.
    let weights = match weight {
        None => None,
        Some(weight_json) => Some(Arc::new(edge_weights(
            edges.as_deref(),
            weight_json,
            topology.n_edges(),
            "weighted shortest_path",
        )?)),
    };
    let node =
        ShortestPathNode::new(topology, ids, source, target, direction, weights).with_mask(mask);
    Ok(crate::lower::Frame::from_ordered(
        ctx.clone(),
        extension(ctx, Arc::new(node)),
    ))
}

/// The leaf of a `random_walk`: a [`RandomWalkNode`] emitting the
/// `(walk_id, step, node)` frame in walk-then-step order. `starts` are user ids
/// (unknown ids are dropped, kernel-consistent); `seed` makes the walk
/// reproducible.
#[allow(clippy::too_many_arguments)]
pub fn walk_leaf(
    ctx: &SessionContext,
    topology: Arc<Topology>,
    ids: Arc<IdMap>,
    mask: Option<Arc<EdgeMask>>,
    starts: &dyn Array,
    steps: u32,
    walks_per_node: u32,
    seed: Option<u64>,
) -> Result<DataFrame> {
    let starts_dense: Vec<u32> = resolve_dense(&ids, starts)?.into_iter().flatten().collect();
    Ok(extension(
        ctx,
        Arc::new(
            RandomWalkNode::new(topology, ids, starts_dense, steps, walks_per_node, seed)
                .with_mask(mask),
        ),
    ))
}

/// Resolve a user-id array to dense indices (`None` per unknown/null id), mapping
/// an id-type mismatch to a clear execution error. The shared seam for every
/// traversal's seed/source/target/start resolution.
pub(crate) fn resolve_dense(
    ids: &IdMap,
    arr: &dyn Array,
) -> Result<Vec<Option<u32>>, DataFusionError> {
    ids.dense_from_array(arr)
        .map_err(|e| DataFusionError::Execution(e.to_string()))
}

/// The LEFT join of the node attribute table `batches` onto `graph` by id, with
/// the table's rows numbered by the ordinal `ord` (its output order depends on the
/// partition count). The fallback of [`attach_by_gather`].
fn attach_by_join(
    ctx: &SessionContext,
    batches: Vec<RecordBatch>,
    id_col: &str,
    graph: RecordBatch,
    ord: &str,
) -> Result<DataFrame> {
    let nodes_df = ctx.read_batches(crate::lower::attach_ordinal(batches, ord)?)?;
    // Rename the graph id so the join keys don't collide, then drop it.
    let graph_df = ctx
        .read_batch(graph)?
        .with_column_renamed("id", "__ursa_gid")?;
    nodes_df
        .join(graph_df, JoinType::Left, &[id_col], &["__ursa_gid"], None)?
        .drop_columns(&["__ursa_gid"])
}

/// Attach the computed columns of `graph` (the `(id, values...)` batch, in dense
/// order) to the node attribute table `batches` by id: each table row takes the
/// values of its id's node, or nulls for an id that is null or not in the graph.
/// This is the LEFT join of the table onto `graph`, with the same output schema,
/// but each batch keeps its rows in table order and its attribute columns as they
/// are: the ids resolve to dense indices once, and each computed column is a
/// `take` by them.
///
/// `None` when the table's id column is not one the gather reads (an `int64` id
/// for an `int64` graph; a string or string-dictionary id for a string graph) or
/// a table column shares a computed column's name; the caller joins instead.
fn attach_by_gather(
    batches: &[RecordBatch],
    id_col: &str,
    graph: &RecordBatch,
    ids: &IdMap,
) -> Result<Option<Vec<RecordBatch>>> {
    use arrow::array::{AsArray, UInt32Array};
    use arrow::compute::take;
    use rayon::prelude::*;
    let arrow_err = |e| DataFusionError::ArrowError(Box::new(e), None);

    let Some(first) = batches.first() else {
        return Ok(None);
    };
    let schema = first.schema();
    let Ok(id_idx) = schema.index_of(id_col) else {
        return Ok(None);
    };
    let strings = |dt: &DataType| {
        matches!(
            dt,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
        )
    };
    let readable = match (ids, schema.field(id_idx).data_type()) {
        (IdMap::Int64 { .. }, DataType::Int64) => true,
        (IdMap::Utf8 { .. }, DataType::Dictionary(_, value)) => strings(value),
        (IdMap::Utf8 { .. }, dt) => strings(dt),
        _ => false,
    };
    let graph_schema = graph.schema();
    let computed = &graph_schema.fields()[1..];
    let clash = computed
        .iter()
        .any(|f| schema.fields().iter().any(|t| t.name() == f.name()));
    if !readable || clash || batches.iter().any(|b| b.schema() != schema) {
        return Ok(None);
    }

    let mut fields = schema.fields().to_vec();
    fields.extend(
        computed
            .iter()
            .map(|f| Arc::new(f.as_ref().clone().with_nullable(true))),
    );
    let out_schema = Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone()));

    // Resolve the ids in parallel slices; a dictionary resolves its values once
    // and gathers them by its keys.
    const SLICE: usize = 1 << 16;
    let resolve = |arr: &dyn Array| -> Result<UInt32Array> {
        let dense: Vec<Vec<Option<u32>>> = (0..arr.len())
            .step_by(SLICE)
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|s| resolve_dense(ids, arr.slice(s, SLICE.min(arr.len() - s)).as_ref()))
            .collect::<Result<_>>()?;
        Ok(dense.into_iter().flatten().collect())
    };
    batches
        .iter()
        .map(|b| {
            let id = b.column(id_idx);
            let rows = match id.as_any_dictionary_opt() {
                Some(dict) => {
                    let values = resolve(dict.values().as_ref())?;
                    take(&values, dict.keys(), None).map_err(arrow_err)?
                }
                None => Arc::new(resolve(id.as_ref())?) as ArrayRef,
            };
            let mut cols = b.columns().to_vec();
            cols.extend(
                graph.columns()[1..]
                    .par_iter()
                    .map(|c| take(c.as_ref(), rows.as_ref(), None).map_err(arrow_err))
                    .collect::<Result<Vec<_>>>()?,
            );
            RecordBatch::try_new(out_schema.clone(), cols).map_err(arrow_err)
        })
        .collect::<Result<_>>()
        .map(Some)
}

/// Test helper: node columns from the compact JSON the unit tests write, a list of
/// `{"name": ..., "kind": <verb>, ...params}`.
#[cfg(test)]
pub(crate) fn cols(json: &str) -> Vec<NodeColumn> {
    let specs: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
    specs
        .into_iter()
        .map(|mut spec| {
            let obj = spec.as_object_mut().unwrap();
            let name = obj.remove("name").unwrap().as_str().unwrap().to_string();
            let verb = obj.remove("kind").unwrap();
            if let (Some(f), Some(c)) = (obj.remove("agg_fn"), obj.remove("agg_column")) {
                obj.insert(
                    "agg".into(),
                    serde_json::json!({"kind": "agg", "fn": f, "operand": {"kind": "col", "name": c}}),
                );
            }
            obj.insert("verb".into(), verb);
            (name, GraphVerb::from_value(&spec).unwrap())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lower::{col_named, Frame, TEST_PARTITIONS};
    use arrow::compute::concat_batches;
    use arrow::datatypes::Field;
    use datafusion::prelude::col;

    // The query entry points these tests were written against took their
    // relational steps in fixed slots. They are adapters here, over the leaves
    // above and `lower::Frame`, applying the slots in the order they had.

    fn query_session() -> SessionContext {
        crate::lower::session()
    }

    fn tail(
        frame: &mut Frame,
        filters: &[String],
        group: (Vec<String>, Vec<String>),
        distinct: bool,
        sort: Option<Vec<(String, bool)>>,
        limit: Option<usize>,
        rename: Vec<(String, String)>,
    ) -> Result<()> {
        for f in filters {
            let v: serde_json::Value = serde_json::from_str(f).unwrap();
            let e = crate::eval::logical(&v, frame.df_schema())?;
            frame.filter(e)?;
        }
        let (keys, aggs) = group;
        if !keys.is_empty() {
            for k in &keys {
                if !frame.has_column(k) {
                    return Err(DataFusionError::Execution(format!(
                        "group_by() references unknown column {k:?}"
                    )));
                }
            }
            let aggs = aggs
                .iter()
                .map(|a| {
                    let v: serde_json::Value = serde_json::from_str(a).unwrap();
                    crate::expr::lower(&crate::expr::parse_ursa_expr(&v)?)
                })
                .collect::<Result<Vec<_>>>()?;
            frame.group_by(keys.iter().map(col).collect(), aggs, true)?;
        }
        if distinct {
            frame.distinct()?;
        }
        match sort.filter(|k| !k.is_empty()) {
            Some(keys) => {
                for (c, _) in &keys {
                    if !frame.has_column(c) {
                        return Err(DataFusionError::Execution(format!(
                            "sort references unknown column {c:?} (column not found)"
                        )));
                    }
                }
                let keys = keys.into_iter().map(|(c, d)| (col_named(&c), d)).collect();
                frame.sort(keys, limit)?;
            }
            None => {
                if let Some(n) = limit {
                    frame.head(n)?;
                }
            }
        }
        if !rename.is_empty() {
            for (old, _) in &rename {
                if !frame.has_column(old) {
                    return Err(DataFusionError::Execution(format!(
                        "rename() references unknown column {old:?}"
                    )));
                }
            }
            frame.rename(&rename)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_node_query(
        topology: Arc<Topology>,
        ids: Arc<IdMap>,
        columns: &[NodeColumn],
        filters: &[String],
        sort: Option<Vec<(String, bool)>>,
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
        assert!(sample.is_none(), "sample is drawn by the plan's host");
        let built = node_columns(
            &topology,
            &ids,
            columns,
            nodes.as_deref(),
            nodes_id.as_deref(),
            edges.as_deref(),
            None,
        )?;
        let ctx = query_session();
        let mut frame = match nodes {
            None => Frame::from_ordered(ctx.clone(), graph_leaf(&ctx, topology, ids, built, mask)),
            Some(batches) => {
                let id = nodes_id.as_deref().unwrap_or("id");
                let rows = attach_columns(&ctx, topology, ids, built, mask, batches, id)?;
                Frame::from_batches(ctx, rows)?
            }
        };
        tail(
            &mut frame,
            filters,
            (group_keys, aggs),
            distinct,
            sort,
            limit,
            rename,
        )?;
        frame.collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_join_query(
        left: Vec<RecordBatch>,
        right: Vec<RecordBatch>,
        on: Vec<String>,
        how: String,
        filters: &[String],
        sort: Option<Vec<(String, bool)>>,
        limit: Option<usize>,
        distinct: bool,
        sample: Option<(usize, Option<u64>)>,
        rename: Vec<(String, String)>,
    ) -> Result<Vec<RecordBatch>> {
        assert!(sample.is_none(), "sample is drawn by the plan's host");
        let how = crate::lower::How::parse(&how)
            .ok_or_else(|| DataFusionError::NotImplemented(format!("join how={how:?}")))?;
        let ctx = query_session();
        let left = Frame::from_batches(ctx.clone(), left)?;
        let mut frame = crate::lower::join(left, Frame::from_batches(ctx, right)?, &on, how)?;
        tail(
            &mut frame,
            filters,
            Default::default(),
            distinct,
            sort,
            limit,
            rename,
        )?;
        frame.collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_hop_query(
        topology: Arc<Topology>,
        ids: Arc<IdMap>,
        mask: Option<Arc<EdgeMask>>,
        seeds: &dyn Array,
        n: u32,
        direction: &str,
        filters: &[String],
        sort: Option<Vec<(String, bool)>>,
        limit: Option<usize>,
        distinct: bool,
        sample: Option<(usize, Option<u64>)>,
        rename: Vec<(String, String)>,
    ) -> Result<Vec<RecordBatch>> {
        assert!(sample.is_none(), "sample is drawn by the plan's host");
        let ctx = query_session();
        let leaf = hop_leaf(&ctx, topology, ids, mask, seeds, n, direction)?;
        let mut frame = Frame::from_ordered(ctx, leaf);
        tail(
            &mut frame,
            filters,
            Default::default(),
            distinct,
            sort,
            limit,
            rename,
        )?;
        frame.collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_path_query(
        topology: Arc<Topology>,
        ids: Arc<IdMap>,
        mask: Option<Arc<EdgeMask>>,
        source: &dyn Array,
        target: &dyn Array,
        direction: &str,
        weight: Option<&str>,
        edges: Option<Vec<RecordBatch>>,
        filters: &[String],
        sort: Option<Vec<(String, bool)>>,
        limit: Option<usize>,
        distinct: bool,
        sample: Option<(usize, Option<u64>)>,
        rename: Vec<(String, String)>,
    ) -> Result<Vec<RecordBatch>> {
        assert!(sample.is_none(), "sample is drawn by the plan's host");
        let ctx = query_session();
        let mut frame = path_leaf(
            &ctx, topology, ids, mask, source, target, direction, weight, edges,
        )?;
        tail(
            &mut frame,
            filters,
            Default::default(),
            distinct,
            sort,
            limit,
            rename,
        )?;
        frame.collect()
    }

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
            &cols(r#"[{"name":"pagerank","kind":"pagerank"}]"#),
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
                &cols(r#"[{"name":"pr","kind":"pagerank"},{"name":"indeg","kind":"degree","direction":"in"}]"#),
                &[r#"{"kind":"binary","op":">",
                    "left":{"kind":"col","name":"indeg"},
                    "right":{"kind":"lit","value":0.0}}"#
                    .to_string()],
                Some(vec![("pr".into(), true)]),
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
            &cols(r#"[{"name":"x","kind":"no_such_algorithm"}]"#),
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
                &cols(&spec),
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
            &cols(r#"[{"name":"indeg","kind":"degree","direction":"in"}]"#),
            &[],
            Some(vec![("id".into(), false)]),
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
                &cols(r#"[{"name":"nbr_cap","kind":"neighbors_agg","agg_fn":"mean","agg_column":"capacity","direction":"in"}]"#),
                &[],
                Some(vec![("id".into(), false)]),
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
        use arrow::array::{ArrayRef, DictionaryArray, StringArray, StringViewArray};
        use arrow::datatypes::{DataType, Field, Int32Type, Schema};

        // hub: 1->0, 2->0, 3->0. In-neighbours of 0 are 1,2,3 with regions
        // us/us/eu -> 2 distinct. The same for every string layout: a dictionary
        // (categorical) column must count its values, not cast to null floats.
        let regions = ["x", "us", "us", "eu"];
        let layouts: [ArrayRef; 3] = [
            Arc::new(StringArray::from(regions.to_vec())),
            Arc::new(StringViewArray::from(regions.to_vec())),
            Arc::new(regions.into_iter().collect::<DictionaryArray<Int32Type>>()),
        ];
        let src = Int64Array::from(vec![1, 2, 3]);
        let dst = Int64Array::from(vec![0, 0, 0]);
        for region in layouts {
            let nodes = RecordBatch::try_new(
                Arc::new(Schema::new(vec![
                    Field::new("id", DataType::Int64, false),
                    Field::new("region", region.data_type().clone(), false),
                ])),
                vec![Arc::new(Int64Array::from(vec![0, 1, 2, 3])), region],
            )
            .unwrap();

            let (t, ids) = build(&src, &dst);
            let batch = one(
                execute_node_query(
                    t,
                    ids,
                    &cols(r#"[{"name":"nbr_regions","kind":"neighbors_agg","agg_fn":"n_unique","agg_column":"region","direction":"in"}]"#),
                    &[],
                    Some(vec![("id".into(), false)]),
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            &cols(
                r#"[{"name":"m","kind":"neighbors_agg","agg_fn":"mean","agg_column":"region","direction":"in"}]"#,
            ),
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
                &cols(r#"[{"name":"indeg","kind":"degree","direction":"in"}]"#),
                &[],
                Some(vec![("region".into(), false)]),
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
            &cols(r#"[{"name":"deg","kind":"degree","direction":"out"}]"#),
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
            Some(vec![("id".into(), false)]),
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
    fn sort_by_several_keys_with_mixed_directions() {
        // a ascending, then id descending: a=0 -> ids 3, 1; a=1 -> ids 2, 0.
        let left = id_attr_batch(vec![0, 1, 2, 3], "a", vec![1, 0, 1, 0]);
        let right = id_attr_batch(vec![0, 1, 2, 3], "b", vec![0, 0, 0, 0]);
        let batch = one(execute_join_query(
            vec![left.clone()],
            vec![right.clone()],
            vec!["id".into()],
            "inner".into(),
            &[],
            Some(vec![("a".into(), false), ("id".into(), true)]),
            None,
            false,
            None,
            vec![],
        )
        .unwrap());
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(ids.values().to_vec(), vec![3, 1, 2, 0]);

        let err = execute_join_query(
            vec![left],
            vec![right],
            vec!["id".into()],
            "inner".into(),
            &[],
            Some(vec![("a".into(), false), ("nope".into(), true)]),
            None,
            false,
            None,
            vec![],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown column \"nope\""), "{err}");
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
            Some(vec![("id".into(), false)]),
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
            "cross".into(),
            &[],
            None,
            None,
            false,
            None,
            vec![],
        );
        assert!(err.is_err());
    }

    // --- implicit sort tiebreaker (#165) -----------------------------------------

    fn i64s(batch: &RecordBatch, name: &str) -> Vec<i64> {
        let c = batch.column(batch.schema().index_of(name).unwrap());
        let c = cast(c, &DataType::Int64).unwrap();
        c.as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .values()
            .to_vec()
    }

    fn assert_no_hidden(batch: &RecordBatch) {
        for f in batch.schema().fields() {
            assert!(!f.name().starts_with("__ursa"), "leaked {}", f.name());
        }
    }

    /// `n` rows `(k = i % 3, v = i, c = 0)` split into small batches, so the sort
    /// sees many batches and, with several target partitions, a round-robin
    /// repartition.
    fn tied_batches(n: i64) -> Vec<RecordBatch> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("k", DataType::Int64, false),
            Field::new("v", DataType::Int64, false),
            Field::new("c", DataType::Int64, false),
        ]));
        (0..n)
            .step_by(256)
            .map(|start| {
                let r: Vec<i64> = (start..(start + 256).min(n)).collect();
                let k: Vec<i64> = r.iter().map(|i| i % 3).collect();
                let c = vec![0i64; r.len()];
                RecordBatch::try_new(
                    schema.clone(),
                    vec![
                        Arc::new(Int64Array::from(k)),
                        Arc::new(Int64Array::from(r)),
                        Arc::new(Int64Array::from(c)),
                    ],
                )
                .unwrap()
            })
            .collect()
    }

    /// Run `batches` through the tail's sort (+ limit) on a session with
    /// `partitions` target partitions, with or without the ordinal tiebreaker.
    fn sort_on(
        batches: Vec<RecordBatch>,
        partitions: usize,
        tiebreak: bool,
        sort: Vec<(String, bool)>,
        limit: Option<usize>,
    ) -> RecordBatch {
        use datafusion::prelude::SessionConfig;
        let n_cols = batches[0].num_columns();
        let out = if tiebreak {
            on_partitions(partitions, || {
                let mut frame = Frame::from_batches(query_session(), batches).unwrap();
                let keys = sort.iter().map(|(c, d)| (col_named(c), *d)).collect();
                frame.sort(keys, limit).unwrap();
                frame.collect().unwrap()
            })
        } else {
            crate::runtime::block_on(async {
                let ctx = SessionContext::new_with_config(
                    SessionConfig::new()
                        .with_target_partitions(partitions)
                        .with_batch_size(512),
                );
                let keys = sort.iter().map(|(c, d)| col(c).sort(!d, false)).collect();
                let mut df = ctx.read_batches(batches).unwrap().sort(keys).unwrap();
                if let Some(n) = limit {
                    df = df.limit(0, Some(n)).unwrap();
                }
                df.collect().await.unwrap()
            })
            .unwrap()
        };
        let batch = one(out);
        assert_no_hidden(&batch);
        assert_eq!(batch.num_columns(), n_cols);
        batch
    }

    fn kv(batch: &RecordBatch) -> Vec<(i64, i64)> {
        i64s(batch, "k").into_iter().zip(i64s(batch, "v")).collect()
    }

    #[test]
    fn sort_ties_keep_input_order_for_any_partition_count() {
        let n = 20_000;
        // Stable reference: k descending, ties in input (v) order.
        let mut expected: Vec<(i64, i64)> = (0..n).map(|i| (i % 3, i)).collect();
        expected.sort_by_key(|&(k, _)| std::cmp::Reverse(k));
        let k_desc = || vec![("k".to_string(), true)];
        // Without the tiebreaker the order of ties is not the input order (and
        // varies with the partition count).
        assert_ne!(
            kv(&sort_on(tied_batches(n), 4, false, k_desc(), None)),
            expected
        );
        // One key, full sort (the integer-pair path); two keys (the DataFusion sort
        // with the ordinal as its last key); one key with a limit (DataFusion's
        // top-k, ordinal included).
        type Case = (Vec<(String, bool)>, Option<usize>);
        let cases: Vec<Case> = vec![
            (k_desc(), None),
            (vec![("k".into(), true), ("c".into(), false)], None),
            (k_desc(), Some(7_000)),
            (vec![("k".into(), true), ("c".into(), true)], Some(13_000)),
        ];
        for (sort, limit) in cases {
            let want = &expected[..limit.unwrap_or(expected.len())];
            for partitions in [1, 2, 3, 4, 8, 16] {
                for _ in 0..2 {
                    let got = sort_on(tied_batches(n), partitions, true, sort.clone(), limit);
                    assert_eq!(kv(&got), want, "{sort:?} limit={limit:?} p={partitions}");
                }
            }
        }
    }

    #[test]
    fn already_sorted_input_comes_back_unchanged() {
        let n = 5_000;
        let got = sort_on(tied_batches(n), 4, true, vec![("v".into(), false)], None);
        assert_eq!(i64s(&got, "v"), (0..n).collect::<Vec<_>>());
        let got = sort_on(tied_batches(n), 4, true, vec![("c".into(), true)], None);
        assert_eq!(i64s(&got, "v"), (0..n).collect::<Vec<_>>());
    }

    #[test]
    fn single_key_fast_path_matches_the_datafusion_sort() {
        // Floats with NaN, -0.0/+0.0, infinities and nulls; ints with negatives;
        // bools. The integer-pair path (one key) must order exactly like
        // DataFusion's sort with the ordinal as a final key (forced here by a
        // second, constant key), in both directions, nulls last.
        let f = Float64Array::from(vec![
            Some(1.5),
            None,
            Some(f64::NAN),
            Some(-0.0),
            Some(0.0),
            Some(f64::NEG_INFINITY),
            Some(1.5),
            Some(-2.0),
            None,
            Some(f64::INFINITY),
            Some(-0.0),
            Some(-f64::NAN),
        ]);
        let n = f.len() as i64;
        let i = Int64Array::from(vec![3, -1, 3, i64::MIN, 0, 7, -1, i64::MAX, 3, 0, -5, 2]);
        let u = arrow::array::UInt8Array::from(vec![
            Some(3),
            Some(255),
            None,
            Some(0),
            Some(3),
            Some(1),
            Some(0),
            Some(255),
            Some(3),
            None,
            Some(1),
            Some(0),
        ]);
        let b = arrow::array::BooleanArray::from(vec![
            Some(true),
            Some(false),
            None,
            Some(true),
            Some(false),
            Some(true),
            None,
            Some(false),
            Some(true),
            Some(true),
            Some(false),
            Some(false),
        ]);
        let schema = Arc::new(Schema::new(vec![
            Field::new("f", DataType::Float64, true),
            Field::new("i", DataType::Int64, false),
            Field::new("u", DataType::UInt8, true),
            Field::new("b", DataType::Boolean, true),
            Field::new("v", DataType::Int64, false),
            Field::new("c", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(f),
                Arc::new(i),
                Arc::new(u),
                Arc::new(b),
                Arc::new(Int64Array::from_iter_values(0..n)),
                Arc::new(Int64Array::from(vec![0; n as usize])),
            ],
        )
        .unwrap();
        let halves = || vec![batch.slice(0, 5), batch.slice(5, n as usize - 5)];
        for key in ["f", "i", "u", "b"] {
            for descending in [false, true] {
                let fast = sort_on(halves(), 4, true, vec![(key.into(), descending)], None);
                let reference = sort_on(
                    halves(),
                    4,
                    true,
                    vec![(key.into(), descending), ("c".into(), false)],
                    None,
                );
                assert_eq!(
                    i64s(&fast, "v"),
                    i64s(&reference, "v"),
                    "{key} desc={descending}"
                );
                // Nulls last in either direction.
                let col = fast.column(fast.schema().index_of(key).unwrap());
                let nulls = col.null_count();
                assert!((0..col.len()).all(|r| col.is_null(r) == (r >= col.len() - nulls)));
            }
        }
        // Spot-check the float order (#201): -inf < -2 < -0 = 0 < 1.5 < inf < NaN,
        // every NaN equal whatever its sign; ties (the zeros, the two 1.5s, the two
        // NaNs) and nulls in input order.
        let fast = sort_on(halves(), 2, true, vec![("f".into(), false)], None);
        assert_eq!(i64s(&fast, "v"), vec![5, 7, 3, 4, 10, 0, 6, 9, 2, 11, 1, 8]);
    }

    /// A graph with many out-degree ties: node i -> i + 1 and, for even i, also
    /// i -> i + 2 (mod n). Ids are scrambled so dense order differs from id order.
    /// Returns the ids in dense (first-appearance) order.
    fn tie_graph(n: i64) -> (Arc<Topology>, Arc<IdMap>, Vec<i64>) {
        let id = |i: i64| (i * 7919) % 100_003;
        let (mut s, mut d) = (vec![], vec![]);
        for i in 0..n {
            s.push(id(i));
            d.push(id((i + 1) % n));
            if i % 2 == 0 {
                s.push(id(i));
                d.push(id((i + 2) % n));
            }
        }
        let (t, ids) = build(&Int64Array::from(s), &Int64Array::from(d));
        (t, ids, (0..n).map(id).collect())
    }

    fn degree_query(
        t: &Arc<Topology>,
        ids: &Arc<IdMap>,
        sort: Vec<(String, bool)>,
        nodes: Option<Vec<RecordBatch>>,
        group: Option<(Vec<String>, Vec<String>)>,
    ) -> RecordBatch {
        let (group_keys, aggs) = group.unwrap_or_default();
        let nodes_id = nodes.as_ref().map(|_| "id".to_string());
        let batch = one(execute_node_query(
            t.clone(),
            ids.clone(),
            &cols(r#"[{"name":"deg","kind":"degree","direction":"out"}]"#),
            &[],
            Some(sort),
            None,
            nodes,
            nodes_id,
            None,
            false,
            None,
            vec![],
            group_keys,
            aggs,
            None,
        )
        .unwrap());
        assert_no_hidden(&batch);
        batch
    }

    #[test]
    fn engine_sort_ties_follow_node_order() {
        let n = 30_000;
        let (t, ids, dense) = tie_graph(n as i64);
        // Graph-derived frame: out-degree 2 (even i) first, then 1 (odd i); ties keep
        // dense node order.
        let expected: Vec<i64> = dense
            .iter()
            .step_by(2)
            .chain(dense.iter().skip(1).step_by(2))
            .copied()
            .collect();
        for _ in 0..3 {
            let batch = degree_query(&t, &ids, vec![("deg".into(), true)], None, None);
            assert_eq!(i64s(&batch, "id"), expected);
        }
        // Attribute frame in reversed node order, two batches, sorted by a constant:
        // ties keep the table's row order, like the plain frame's stable sort.
        let rev: Vec<i64> = dense.iter().rev().copied().collect();
        let attrs = id_attr_batch(rev.clone(), "c", vec![1; n]);
        let attrs = vec![attrs.slice(0, 10_000), attrs.slice(10_000, n - 10_000)];
        let batch = degree_query(&t, &ids, vec![("c".into(), false)], Some(attrs), None);
        assert_eq!(i64s(&batch, "id"), rev);
        // group_by + sort: groups that tie on the sort key keep the order of their
        // first input row. Rows: dense[1] (deg 1), dense[0] (deg 2), dense[3] (deg 1).
        let attrs = id_attr_batch(vec![dense[1], dense[0], dense[3]], "c", vec![5, 5, 5]);
        let agg = r#"{"kind":"alias","name":"m","operand":{"kind":"agg","fn":"min","operand":{"kind":"col","name":"c"}}}"#;
        let batch = degree_query(
            &t,
            &ids,
            vec![("m".into(), false)],
            Some(vec![attrs]),
            Some((vec!["deg".into()], vec![agg.into()])),
        );
        assert_eq!(i64s(&batch, "deg"), vec![1, 2]);
    }

    #[test]
    fn engine_sort_ties_follow_traversal_and_join_order() {
        let (t, ids, dense) = tie_graph(5_000);
        let seeds = Int64Array::from(dense[..1_000].to_vec());
        let hop = |sort: Option<Vec<(String, bool)>>| {
            one(execute_hop_query(
                t.clone(),
                ids.clone(),
                None,
                &seeds,
                2,
                "out",
                &[],
                sort,
                None,
                false,
                None,
                vec![],
            )
            .unwrap())
        };
        let pairs = |b: &RecordBatch| -> Vec<(i64, i64)> {
            i64s(b, "src").into_iter().zip(i64s(b, "dst")).collect()
        };
        // Ties on `src` keep the unsorted traversal order (seed order, then BFS).
        let mut want = pairs(&hop(None));
        want.sort_by_key(|&(s, _)| std::cmp::Reverse(s)); // stable
        for _ in 0..3 {
            let sorted = hop(Some(vec![("src".into(), true)]));
            assert_no_hidden(&sorted);
            assert_eq!(pairs(&sorted), want);
        }

        // Join: ties follow the left row, then the right row.
        let left = id_attr_batch(vec![3, 1, 2, 1], "a", vec![0, 0, 0, 0]);
        let right = id_attr_batch(vec![1, 2, 1, 3], "b", vec![7, 8, 9, 10]);
        for how in ["inner", "left"] {
            let batch = one(execute_join_query(
                vec![left.clone()],
                vec![right.clone()],
                vec!["id".into()],
                how.into(),
                &[],
                Some(vec![("a".into(), false)]),
                None,
                false,
                None,
                vec![],
            )
            .unwrap());
            assert_no_hidden(&batch);
            assert_eq!(i64s(&batch, "id"), vec![3, 1, 1, 2, 1, 1]);
            assert_eq!(i64s(&batch, "b"), vec![10, 7, 9, 8, 7, 9]);
        }
    }

    #[test]
    fn tiebreak_column_name_avoids_user_columns() {
        // A user column already named like the ordinal keeps its values and stays in
        // the output; the ordinal takes a fresh name and is still dropped.
        let left = id_attr_batch(vec![2, 1, 3], "__ursa_ord_l", vec![9, 8, 7]);
        let right = id_attr_batch(vec![1, 2, 3], "b", vec![0, 0, 0]);
        let batch = one(execute_join_query(
            vec![left],
            vec![right],
            vec!["id".into()],
            "inner".into(),
            &[],
            Some(vec![("b".into(), false)]),
            None,
            false,
            None,
            vec![],
        )
        .unwrap());
        assert_eq!(i64s(&batch, "id"), vec![2, 1, 3]);
        assert_eq!(i64s(&batch, "__ursa_ord_l"), vec![9, 8, 7]);
        assert_eq!(batch.num_columns(), 3);
    }

    /// Run `f` with the query session pinned to `partitions` target partitions.
    fn on_partitions<T>(partitions: usize, f: impl FnOnce() -> T) -> T {
        TEST_PARTITIONS.with(|c| c.set(Some(partitions)));
        let out = f();
        TEST_PARTITIONS.with(|c| c.set(None));
        out
    }

    const PARTITION_COUNTS: [usize; 5] = [1, 2, 4, 8, 16];

    /// An unsorted out-degree node query, with an attribute table or without.
    fn unsorted_degree_query(
        t: &Arc<Topology>,
        ids: &Arc<IdMap>,
        nodes: Option<Vec<RecordBatch>>,
        filters: &[String],
        limit: Option<usize>,
        distinct: bool,
        group: Option<(Vec<String>, Vec<String>)>,
    ) -> RecordBatch {
        let (group_keys, aggs) = group.unwrap_or_default();
        let nodes_id = nodes.as_ref().map(|_| "id".to_string());
        let batch = one(execute_node_query(
            t.clone(),
            ids.clone(),
            &cols(r#"[{"name":"deg","kind":"degree","direction":"out"}]"#),
            filters,
            None,
            limit,
            nodes,
            nodes_id,
            None,
            distinct,
            None,
            vec![],
            group_keys,
            aggs,
            None,
        )
        .unwrap());
        assert_no_hidden(&batch);
        batch
    }

    #[test]
    fn unsorted_attribute_output_follows_table_order() {
        let n = 30_000;
        let (t, ids, dense) = tie_graph(n as i64);
        // Attribute rows in a scrambled order, `c` = row position, in small batches.
        let order: Vec<i64> = (0..n).map(|i| dense[(i * 4241) % n]).collect();
        let attrs = id_attr_batch(order.clone(), "c", (0..n as i64).collect());
        let chunks = || -> Vec<RecordBatch> {
            (0..n)
                .step_by(3_000)
                .map(|s| attrs.slice(s, 3_000.min(n - s)))
                .collect()
        };
        let deg: HashMap<i64, i64> = {
            let b = unsorted_degree_query(&t, &ids, None, &[], None, false, None);
            i64s(&b, "id").into_iter().zip(i64s(&b, "deg")).collect()
        };
        let deg_one = r#"{"kind":"binary","op":"==","left":{"kind":"col","name":"deg"},"right":{"kind":"lit","value":1}}"#;
        // Groups (by degree) in order of first appearance, with that first position.
        let mut first: Vec<(i64, i64)> = vec![];
        for (c, i) in order.iter().enumerate() {
            if !first.iter().any(|&(d, _)| d == deg[i]) {
                first.push((deg[i], c as i64));
            }
        }
        let min_c = r#"{"kind":"alias","name":"m","operand":{"kind":"agg","fn":"min","operand":{"kind":"col","name":"c"}}}"#;
        for p in PARTITION_COUNTS {
            on_partitions(p, || {
                let all = unsorted_degree_query(&t, &ids, Some(chunks()), &[], None, false, None);
                assert_eq!(i64s(&all, "id"), order, "p={p}");
                let want_deg: Vec<i64> = order.iter().map(|i| deg[i]).collect();
                assert_eq!(i64s(&all, "deg"), want_deg, "p={p}");
                // An unsorted head takes the first rows in table order, within the
                // first batch, across several, past the end, and none.
                for k in [100, 7_777, n + 5, 0] {
                    let head =
                        unsorted_degree_query(&t, &ids, Some(chunks()), &[], Some(k), false, None);
                    assert_eq!(i64s(&head, "id"), order[..k.min(n)], "p={p} k={k}");
                    assert_eq!(i64s(&head, "deg"), want_deg[..k.min(n)], "p={p} k={k}");
                }
                // A filter keeps the surviving rows in table order.
                let f = [deg_one.to_string()];
                let got = unsorted_degree_query(&t, &ids, Some(chunks()), &f, None, false, None);
                let want: Vec<i64> = (0..n as i64)
                    .filter(|&c| deg[&order[c as usize]] == 1)
                    .collect();
                assert_eq!(i64s(&got, "c"), want, "p={p}");
                let got =
                    unsorted_degree_query(&t, &ids, Some(chunks()), &f, Some(5_000), false, None);
                assert_eq!(i64s(&got, "c"), want[..5_000], "p={p}");
                // distinct (every row is unique) keeps every row in place.
                let got = unsorted_degree_query(&t, &ids, Some(chunks()), &[], None, true, None);
                assert_eq!(i64s(&got, "id"), order, "p={p}");
                // group_by: groups in order of their first row, so `min(c)` ascends.
                let group = Some((vec!["deg".to_string()], vec![min_c.to_string()]));
                let got = unsorted_degree_query(&t, &ids, Some(chunks()), &[], None, false, group);
                let got: Vec<(i64, i64)> =
                    i64s(&got, "deg").into_iter().zip(i64s(&got, "m")).collect();
                assert_eq!(got, first, "p={p}");
            });
        }
    }

    #[test]
    fn unsorted_graph_output_groups_follow_node_order() {
        // No attribute table: the plain output is in dense order and pays nothing,
        // and a group_by returns its groups in order of first appearance.
        let (t, ids, dense) = tie_graph(20_000);
        let count = r#"{"kind":"alias","name":"n","operand":{"kind":"agg","fn":"count","operand":{"kind":"col","name":"id"}}}"#;
        for p in PARTITION_COUNTS {
            on_partitions(p, || {
                let all = unsorted_degree_query(&t, &ids, None, &[], None, false, None);
                assert_eq!(i64s(&all, "id"), dense, "p={p}");
                let group = Some((vec!["deg".to_string()], vec![count.to_string()]));
                let got = unsorted_degree_query(&t, &ids, None, &[], None, false, group);
                assert_eq!(i64s(&got, "deg"), vec![2, 1], "p={p}");
                assert_eq!(i64s(&got, "n"), vec![10_000, 10_000], "p={p}");
            });
        }
    }

    #[test]
    fn unsorted_join_follows_left_then_right_order() {
        // Left ids repeat (each id twice), right ids 0, 1 or 2 rows per id, both
        // scrambled; the expected order is a left-major nested-loop join.
        let n: i64 = 20_000;
        let lids: Vec<i64> = (0..n).map(|i| (i * 7919) % (n / 2)).collect();
        let rids: Vec<i64> = (0..n)
            .map(|i| (i * 104_729) % (n / 2))
            .filter(|id| id % 5 != 0)
            .collect();
        let left = id_attr_batch(lids.clone(), "a", (0..n).collect());
        let right = id_attr_batch(rids.clone(), "b", (0..rids.len() as i64).collect());
        let slices = |b: &RecordBatch| -> Vec<RecordBatch> {
            (0..b.num_rows())
                .step_by(2_500)
                .map(|s| b.slice(s, 2_500.min(b.num_rows() - s)))
                .collect()
        };
        let mut by_id: HashMap<i64, Vec<i64>> = HashMap::new();
        for (pos, id) in rids.iter().enumerate() {
            by_id.entry(*id).or_default().push(pos as i64);
        }
        for how in ["inner", "left"] {
            let mut want: Vec<(i64, i64, Option<i64>)> = vec![];
            for (a, id) in lids.iter().enumerate() {
                match by_id.get(id) {
                    Some(bs) => want.extend(bs.iter().map(|&b| (*id, a as i64, Some(b)))),
                    None if how == "left" => want.push((*id, a as i64, None)),
                    None => {}
                }
            }
            for p in PARTITION_COUNTS {
                for limit in [None, Some(100), Some(6_000), Some(0)] {
                    let batch = on_partitions(p, || {
                        one(execute_join_query(
                            slices(&left),
                            slices(&right),
                            vec!["id".into()],
                            how.into(),
                            &[],
                            None,
                            limit,
                            false,
                            None,
                            vec![],
                        )
                        .unwrap())
                    });
                    assert_no_hidden(&batch);
                    let b = batch.column(batch.schema().index_of("b").unwrap());
                    let b = b.as_any().downcast_ref::<Int64Array>().unwrap();
                    let got: Vec<(i64, i64, Option<i64>)> = i64s(&batch, "id")
                        .into_iter()
                        .zip(i64s(&batch, "a"))
                        .zip(b.iter())
                        .map(|((id, a), b)| (id, a, b))
                        .collect();
                    let want = &want[..limit.unwrap_or(want.len()).min(want.len())];
                    assert_eq!(got, want, "how={how} p={p} limit={limit:?}");
                }
            }
        }
    }

    /// The attribute table `ids` (one column per id type), in batches of `chunk`
    /// rows, with a row-position column `c`, field and schema metadata, and a
    /// non-null field: everything the join's output schema carries.
    fn attr_batches(ids: ArrayRef, chunk: usize) -> Vec<RecordBatch> {
        use arrow::datatypes::{Field, Schema};
        let n = ids.len();
        let schema = Arc::new(Schema::new_with_metadata(
            vec![
                Field::new("c", DataType::Int64, false)
                    .with_metadata([("k".to_string(), "v".to_string())].into()),
                Field::new("id", ids.data_type().clone(), true),
            ],
            [("top".to_string(), "x".to_string())].into(),
        ));
        let c: ArrayRef = Arc::new(Int64Array::from_iter_values(0..n as i64));
        let b = RecordBatch::try_new(schema, vec![c, ids]).unwrap();
        (0..n.max(1))
            .step_by(chunk)
            .map(|s| b.slice(s, chunk.min(n - s)))
            .collect()
    }

    #[test]
    fn attribute_gather_matches_the_join() {
        use arrow::array::{DictionaryArray, LargeStringArray, StringViewArray};
        use arrow::datatypes::Int32Type;

        // Ids repeat, include values absent from the graph and nulls, and come in
        // several batches; the graph has both id types.
        let n = 5_000usize;
        let raw: Vec<Option<i64>> = (0..n)
            .map(|i| (i % 13 != 0).then_some(((i * 7919) % 1_200) as i64))
            .collect();
        let src = Int64Array::from_iter_values((0..1_000).map(|i| (i * 31) % 1_000));
        let dst = Int64Array::from_iter_values((0..1_000).map(|i| (i * 17 + 3) % 1_000));
        let strs =
            |a: &Int64Array| StringArray::from_iter(a.iter().map(|v| v.map(|v| v.to_string())));
        let raw_str: Vec<Option<String>> = raw.iter().map(|v| v.map(|v| v.to_string())).collect();
        let int_graph = crate::topology::build_topology(&src, &dst).unwrap();
        let str_graph = crate::topology::build_topology(&strs(&src), &strs(&dst)).unwrap();
        type Graph = (Arc<Topology>, Arc<IdMap>);
        let cases: Vec<(&str, &Graph, ArrayRef)> = vec![
            ("int64", &int_graph, Arc::new(Int64Array::from(raw.clone()))),
            (
                "utf8",
                &str_graph,
                Arc::new(StringArray::from(raw_str.clone())),
            ),
            (
                "large_utf8",
                &str_graph,
                Arc::new(LargeStringArray::from(raw_str.clone())),
            ),
            (
                "utf8_view",
                &str_graph,
                Arc::new(StringViewArray::from(raw_str.clone())),
            ),
            (
                "dictionary",
                &str_graph,
                Arc::new(
                    raw_str
                        .iter()
                        .map(|v| v.as_deref())
                        .collect::<DictionaryArray<Int32Type>>(),
                ),
            ),
        ];
        let columns = vec![
            OutputColumn::Algo {
                name: "deg".into(),
                algo: GraphAlgo::Degree {
                    direction: Direction::In,
                },
                weights: None,
                dtype: OutputDtype::F64,
                field: 0,
            },
            OutputColumn::Algo {
                name: "pr".into(),
                algo: GraphAlgo::PageRank {
                    damping: 0.85,
                    max_iter: 30,
                    tol: 1e-6,
                    direction: Direction::Out,
                },
                weights: None,
                dtype: OutputDtype::F32,
                field: 0,
            },
        ];
        for (label, (t, ids), id_arr) in cases {
            let graph = query_batch(t, ids, &columns, None).unwrap();
            for chunk in [700, n] {
                let batches = attr_batches(id_arr.clone(), chunk);
                let gathered = attach_by_gather(&batches, "id", &graph, ids)
                    .unwrap()
                    .unwrap_or_else(|| panic!("{label}: gather declined"));
                assert_eq!(gathered.len(), batches.len(), "{label}");
                let gathered = one(gathered);
                for p in [1, 4] {
                    let joined = on_partitions(p, || {
                        let ctx = query_session();
                        let ord = crate::lower::fresh_name(&batches[0].schema());
                        let df = attach_by_join(&ctx, batches.clone(), "id", graph.clone(), &ord)
                            .unwrap();
                        Frame::from_numbered(ctx, df, ord).collect().unwrap()
                    });
                    let joined = one(joined);
                    assert_eq!(gathered.schema(), joined.schema(), "{label} p={p}");
                    assert_eq!(gathered, joined, "{label} chunk={chunk} p={p}");
                }
            }
        }
        // An id column the gather does not read declines, and the caller joins.
        let (t, ids) = &int_graph;
        let graph = query_batch(t, ids, &columns, None).unwrap();
        let i32_ids: ArrayRef = Arc::new(arrow::array::Int32Array::from(vec![1, 2]));
        let batches = attr_batches(i32_ids, 2);
        assert!(attach_by_gather(&batches, "id", &graph, ids)
            .unwrap()
            .is_none());
        let str_ids: ArrayRef = Arc::new(StringArray::from(vec!["1"]));
        let batches = attr_batches(str_ids, 2);
        assert!(attach_by_gather(&batches, "id", &graph, ids)
            .unwrap()
            .is_none());
    }

    #[test]
    fn attribute_output_on_an_empty_table_keeps_its_schema() {
        // #153: a 0-row table (one empty batch) gives an empty frame with the
        // table's columns plus the computed ones.
        let (src, dst) = diamond();
        let (t, ids) = build(&src, &dst);
        let empty = attr_batches(Arc::new(Int64Array::from(Vec::<i64>::new())), 1);
        assert_eq!(empty.len(), 1);
        for filters in [vec![], vec![r#"{"kind":"binary","op":">","left":{"kind":"col","name":"deg"},"right":{"kind":"lit","value":0}}"#.to_string()]] {
            let batch =
                unsorted_degree_query(&t, &ids, Some(empty.clone()), &filters, None, false, None);
            assert_eq!(batch.num_rows(), 0);
            let schema = batch.schema();
            let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
            assert_eq!(names, ["c", "id", "deg"]);
        }
    }

    #[test]
    fn unsorted_attribute_rename_follows_table_order() {
        // The gathered rows reach a bare tail as one ordered partition: a rename
        // keeps the table's row order across batches and partition counts.
        let n = 30_000;
        let (t, ids, dense) = tie_graph(n as i64);
        let order: Vec<i64> = (0..n).map(|i| dense[(i * 4241) % n]).collect();
        let attrs = id_attr_batch(order.clone(), "c", (0..n as i64).collect());
        for p in PARTITION_COUNTS {
            let chunks: Vec<RecordBatch> = (0..n)
                .step_by(3_000)
                .map(|s| attrs.slice(s, 3_000.min(n - s)))
                .collect();
            let batch = on_partitions(p, || {
                one(execute_node_query(
                    t.clone(),
                    ids.clone(),
                    &cols(r#"[{"name":"deg","kind":"degree","direction":"out"}]"#),
                    &[],
                    None,
                    Some(20_000),
                    Some(chunks),
                    Some("id".into()),
                    None,
                    false,
                    None,
                    vec![("c".into(), "pos".into())],
                    vec![],
                    vec![],
                    None,
                )
                .unwrap())
            });
            assert_eq!(i64s(&batch, "id"), order[..20_000], "p={p}");
            assert_eq!(
                i64s(&batch, "pos"),
                (0..20_000).collect::<Vec<_>>(),
                "p={p}"
            );
        }
    }
}
