//! Assembling kernel outputs into Arrow `RecordBatch`es.
//!
//! The "Arrow arrays out" half of the operator contract. A query names one or
//! more output columns; each is either a node-valued algorithm over the topology
//! or a neighbour aggregation over a node attribute. Because every column is
//! evaluated in the same `IdMap` order they are all row-aligned, so the result is
//! a single `(id, col_1, col_2, ...)` batch with dense→user id translation done
//! once, here, at the boundary.

use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, Int64Array};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use ursa_core::algo::{
    k_hop_masked, random_walk_masked, shortest_path_masked,
    shortest_path_weighted_with_cost_masked, AggKind,
};
use ursa_core::catalog::{Evaluator, Inputs};
use ursa_core::{Direction, EdgeMask, IdMap, Topology};

use crate::logical::Call;

pub use ursa_core::catalog::OutputDtype;

/// One requested output column.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputColumn {
    /// A node-valued algorithm or layout over the topology. `weights` (an `f64`
    /// per edge row, gathered via `edge_ids`) is present for a weighted
    /// algorithm. `dtype` narrows a float-valued result to `f32` on emit (#117).
    Algo {
        name: String,
        algo: Call,
        weights: Option<Arc<Vec<f64>>>,
        dtype: OutputDtype,
        /// Which of the kernel's outputs this column takes.
        ///
        /// Zero for every single-output kernel, which is all of them but layout.
        /// A multi-output kernel is invoked once and its results memoized
        /// together, so `x` and `y` are two columns over one simulation rather
        /// than two simulations — the distinction #115 exists for.
        field: usize,
    },
    /// A per-node aggregation of a (dense-aligned) attribute over neighbours.
    NeighborAgg {
        name: String,
        attr: Arc<Vec<Option<f64>>>,
        direction: Direction,
        agg: AggKind,
        dtype: OutputDtype,
    },
}

impl OutputColumn {
    pub fn name(&self) -> &str {
        match self {
            OutputColumn::Algo { name, .. } | OutputColumn::NeighborAgg { name, .. } => name,
        }
    }

    fn dtype(&self) -> OutputDtype {
        match self {
            OutputColumn::Algo { dtype, .. } | OutputColumn::NeighborAgg { dtype, .. } => *dtype,
        }
    }

    /// The call this column takes an output of.
    fn call(&self) -> Call {
        match self {
            OutputColumn::Algo { algo, .. } => *algo,
            OutputColumn::NeighborAgg { direction, agg, .. } => Call::NeighborAgg {
                direction: *direction,
                agg: *agg,
            },
        }
    }

    /// The column's Arrow type *before* any `dtype` narrowing, as the catalog
    /// declares the output.
    pub fn base_value_type(&self) -> DataType {
        let field = match self {
            OutputColumn::Algo { field, .. } => *field,
            OutputColumn::NeighborAgg { .. } => 0,
        };
        let outputs = self.call().spec().outputs;
        outputs.get(field).unwrap_or(&outputs[0]).ty.data_type()
    }

    pub fn value_type(&self) -> DataType {
        match self.dtype() {
            // `F32` is only ever set on a float-valued column (query.rs validates), so
            // narrowing the base `Float64` to `Float32` is always well-typed.
            OutputDtype::F32 => DataType::Float32,
            OutputDtype::F64 => self.base_value_type(),
        }
    }

    /// The column's array, from the query's shared evaluator.
    fn array<'a>(&'a self, eval: &mut Evaluator<'a>) -> Result<ArrayRef> {
        let (inputs, field) = match self {
            OutputColumn::Algo { weights, field, .. } => (
                Inputs {
                    weights: weights.as_deref().map(Vec::as_slice),
                    attribute: None,
                },
                *field,
            ),
            OutputColumn::NeighborAgg { attr, .. } => (
                Inputs {
                    weights: None,
                    attribute: Some(attr.as_slice()),
                },
                0,
            ),
        };
        eval.column(&self.call(), inputs, field, self.dtype())
            .map_err(|e| DataFusionError::Execution(e.to_string()))
    }
}

/// The output schema for a query: an `id` column (of the graph's user-id type)
/// followed by one column per output.
pub fn query_schema(columns: &[OutputColumn], id_type: DataType) -> SchemaRef {
    let mut fields = vec![Field::new("id", id_type, false)];
    for col in columns {
        // Neighbour aggregates can be null (undefined over no attributed
        // neighbours); algorithm columns are dense and non-null.
        let nullable = matches!(col, OutputColumn::NeighborAgg { .. });
        fields.push(Field::new(col.name(), col.value_type(), nullable));
    }
    Arc::new(Schema::new(fields))
}

/// Materialize the `(id, values...)` batch for a query. The columns are all
/// `n_nodes` long and match the schema by construction, so `try_new` is expected
/// to succeed; it returns `Result` rather than `expect`-panicking so a future
/// column-length regression surfaces as a catchable engine error, not a process
/// abort across the FFI.
///
/// The columns share one [`Evaluator`], so columns resting on the same work
/// (one call under two names or dtypes, `triangle_count` with
/// `clustering_coefficient`, a layout's `x` and `y`) pay for it once (#115).
pub fn query_batch<'a>(
    topo: &'a Topology,
    ids: &IdMap,
    columns: &'a [OutputColumn],
    mask: Option<&'a EdgeMask>,
) -> Result<RecordBatch> {
    let mut eval = Evaluator::new(topo, mask);
    let mut arrays: Vec<ArrayRef> = vec![ids.user_id_array()];
    for col in columns {
        arrays.push(col.array(&mut eval)?);
    }
    RecordBatch::try_new(query_schema(columns, ids.user_type()), arrays)
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}

/// The output schema for a `hop`: an edge frame `(src, dst)` (of the graph's
/// user-id type) where `src` is the seed and `dst` the reached node. Both non-null.
pub fn hop_schema(id_type: DataType) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("src", id_type.clone(), false),
        Field::new("dst", id_type, false),
    ]))
}

/// Materialize a `hop`'s `(src, dst)` edge batch: run `k_hop` over the topology
/// and translate the dense `(seed, reached)` pairs back to user ids.
pub fn hop_batch(
    topo: &Topology,
    ids: &IdMap,
    mask: Option<&EdgeMask>,
    seeds: &[u32],
    n: u32,
    direction: Direction,
) -> Result<RecordBatch> {
    let (seed_dense, reached_dense) = k_hop_masked(topo, mask, seeds, n, direction);
    RecordBatch::try_new(
        hop_schema(ids.user_type()),
        vec![
            ids.gather_user(&seed_dense),
            ids.gather_user(&reached_dense),
        ],
    )
    .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}

/// The output schema for a `shortest_path`: an edge frame `(src, dst, hop, cost)` —
/// one row per edge on the path, in order, with `hop` the 0-based position and
/// `cost` the cumulative path cost from the path source to that edge's destination.
/// `src`/`dst` carry the graph's user-id type; `hop` is Int64; `cost` is Float64
/// (weighted: summed edge weight; unweighted: the hop count `hop + 1`). All non-null.
pub fn path_schema(id_type: DataType) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("src", id_type.clone(), false),
        Field::new("dst", id_type, false),
        Field::new("hop", DataType::Int64, false),
        Field::new("cost", DataType::Float64, false),
    ]))
}

/// Materialize a `shortest_path`'s `(src, dst, hop, cost)` batch: run the path kernel
/// (unweighted BFS, or weighted Dijkstra when `weights` is present) and zip the
/// dense node sequence into consecutive edges (translated back to user ids). An
/// unreachable target (or a trivial one-node path) yields an empty batch.
pub fn path_batch(
    topo: &Topology,
    ids: &IdMap,
    mask: Option<&EdgeMask>,
    source: u32,
    target: u32,
    direction: Direction,
    weights: Option<&[f64]>,
) -> Result<RecordBatch> {
    let mut src_dense = Vec::new();
    let mut dst_dense = Vec::new();
    let mut hop = Vec::new();
    let mut cost = Vec::new();
    // Weighted paths carry per-node cumulative costs (Dijkstra's settled distances);
    // unweighted paths derive the cost as the hop count (`hop + 1`) for schema
    // uniformity, so `cost` is always present and downstream never special-cases it.
    let (route, node_costs) = match weights {
        Some(w) => {
            match shortest_path_weighted_with_cost_masked(topo, mask, w, source, target, direction)
            {
                Some((path, costs)) => (Some(path), Some(costs)),
                None => (None, None),
            }
        }
        None => (
            shortest_path_masked(topo, mask, source, target, direction),
            None,
        ),
    };
    if let Some(nodes) = route {
        for (i, window) in nodes.windows(2).enumerate() {
            src_dense.push(window[0]);
            dst_dense.push(window[1]);
            hop.push(i as i64);
            // Cost to reach this edge's destination (`window[1]`, node index `i + 1`
            // on the route). Weighted: the settled distance; unweighted: `i + 1`.
            cost.push(match &node_costs {
                Some(costs) => costs[i + 1],
                None => (i + 1) as f64,
            });
        }
    }
    RecordBatch::try_new(
        path_schema(ids.user_type()),
        vec![
            ids.gather_user(&src_dense),
            ids.gather_user(&dst_dense),
            Arc::new(Int64Array::from(hop)),
            Arc::new(Float64Array::from(cost)),
        ],
    )
    .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}

/// The output schema for a `random_walk`: a node frame `(walk_id, step, node)` —
/// one row per visited node, `walk_id` identifying the walk and `step` its 0-based
/// position along it. All non-null.
pub fn walk_schema(id_type: DataType) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("walk_id", DataType::Int64, false),
        Field::new("step", DataType::Int64, false),
        Field::new("node", id_type, false),
    ]))
}

/// Materialize a `random_walk`'s `(walk_id, step, node)` batch: run the walk
/// kernel from the dense start set and translate the visited dense nodes back to
/// user ids.
pub fn walk_batch(
    topo: &Topology,
    ids: &IdMap,
    mask: Option<&EdgeMask>,
    starts: &[u32],
    steps: u32,
    walks_per_node: u32,
    seed: Option<u64>,
) -> Result<RecordBatch> {
    let walks = random_walk_masked(topo, mask, starts, steps, walks_per_node, seed);
    let node = ids.gather_user(&walks.node);
    RecordBatch::try_new(
        walk_schema(ids.user_type()),
        vec![
            Arc::new(Int64Array::from(walks.walk_id)),
            Arc::new(Int64Array::from(walks.step)),
            node,
        ],
    )
    .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::build_topology;

    // The sharing between columns (#115) is tested where it lives, on the
    // evaluator in `ursa_core::catalog`; these test the batch around it.

    fn diamond() -> (Arc<Topology>, Arc<IdMap>) {
        let src = Int64Array::from(vec![0, 0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 2, 0]);
        build_topology(&src, &dst).unwrap()
    }

    fn algo_col(name: &str, algo: Call, dtype: OutputDtype) -> OutputColumn {
        OutputColumn::Algo {
            name: name.to_string(),
            algo,
            weights: None,
            dtype,
            field: 0,
        }
    }

    fn pagerank_algo() -> Call {
        Call::PageRank {
            damping: 0.85,
            max_iter: 30,
            tol: 1e-6,
            direction: Direction::Out,
        }
    }

    #[test]
    fn multi_column_batch_is_aligned() {
        let (topo, ids) = diamond();
        let columns = vec![
            algo_col(
                "deg",
                Call::Degree {
                    direction: Direction::Out,
                },
                OutputDtype::F64,
            ),
            algo_col("pr", pagerank_algo(), OutputDtype::F64),
        ];
        let batch = query_batch(&topo, &ids, &columns, None).unwrap();
        assert_eq!(batch.num_columns(), 3); // id, deg, pr
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.schema().field(1).name(), "deg");
        assert_eq!(batch.schema().field(2).name(), "pr");
    }

    #[test]
    fn f32_dtype_narrows_a_float_column_to_float32_and_matches_f64() {
        use arrow::array::Float32Array;

        let (topo, ids) = diamond();
        let pr = |dtype| algo_col("pr", pagerank_algo(), dtype);
        let f64_batch = query_batch(&topo, &ids, &[pr(OutputDtype::F64)], None).unwrap();
        let f32_batch = query_batch(&topo, &ids, &[pr(OutputDtype::F32)], None).unwrap();

        assert_eq!(f64_batch.schema().field(1).data_type(), &DataType::Float64);
        assert_eq!(f32_batch.schema().field(1).data_type(), &DataType::Float32);

        // Same values, just narrowed: each f32 score is the f64 score cast down.
        let wide = f64_batch
            .column(1)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        let narrow = f32_batch
            .column(1)
            .as_any()
            .downcast_ref::<Float32Array>()
            .unwrap();
        assert_eq!(wide.len(), narrow.len());
        for i in 0..wide.len() {
            assert_eq!(narrow.value(i), wide.value(i) as f32);
        }
    }

    #[test]
    fn f32_on_an_integer_column_is_rejected_at_query_build() {
        // The JSON path (node_columns) is where the float-only rule is enforced;
        // a degree column asking for f32 must error, not silently emit a lossy float.
        let (topo, ids) = diamond();
        let json = r#"[{"name":"deg","kind":"degree","direction":"out","dtype":"f32"}]"#;
        let err = crate::query::node_columns(
            &topo,
            &ids,
            &crate::query::cols(json),
            None,
            None,
            None,
            None,
        );
        let Err(err) = err else {
            panic!("f32 on an integer column was accepted")
        };
        let msg = format!("{err}");
        assert!(msg.contains("f32"), "unexpected error: {msg}");
    }

    #[test]
    fn a_layout_fills_both_columns_from_one_run() {
        let (topo, ids) = diamond();
        let layout = Call::LayoutFr {
            iterations: 20,
            k: 1.0,
            gravity: 0.02,
            seed: Some(1),
        };
        let col = |name: &str, field| OutputColumn::Algo {
            name: name.to_string(),
            algo: layout,
            weights: None,
            dtype: OutputDtype::F64,
            field,
        };
        let batch = query_batch(&topo, &ids, &[col("x", 0), col("y", 1)], None).unwrap();
        assert_eq!(batch.schema().field(1).data_type(), &DataType::Float32);
        assert_eq!(batch.schema().field(2).data_type(), &DataType::Float32);
        // Distinct arrays selected out of one result, not the same array twice.
        assert!(!Arc::ptr_eq(batch.column(1), batch.column(2)));
    }

    #[test]
    fn the_triangle_family_survives_an_empty_graph() {
        // n_nodes == 0 short-circuits before the undirected view is resolved; both
        // columns must still produce a well-typed empty array.
        let src = Int64Array::from(Vec::<i64>::new());
        let dst = Int64Array::from(Vec::<i64>::new());
        let (topo, ids) = build_topology(&src, &dst).unwrap();
        let columns = vec![
            algo_col("tri", Call::TriangleCount, OutputDtype::F64),
            algo_col("cc", Call::ClusteringCoefficient, OutputDtype::F64),
        ];
        let batch = query_batch(&topo, &ids, &columns, None).unwrap();
        assert_eq!(batch.num_rows(), 0);
        assert_eq!(batch.schema().field(1).data_type(), &DataType::UInt32);
        assert_eq!(batch.schema().field(2).data_type(), &DataType::Float64);
    }
}
