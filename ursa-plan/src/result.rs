//! Assembling kernel outputs into Arrow `RecordBatch`es.
//!
//! The "Arrow arrays out" half of the operator contract. A query names one or
//! more output columns; each is either a node-valued algorithm over the topology
//! or a neighbour aggregation over a node attribute. Because every column is
//! evaluated in the same `IdMap` order they are all row-aligned, so the result is
//! a single `(id, col_1, col_2, ...)` batch with dense→user id translation done
//! once, here, at the boundary.

use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, Int64Array, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use ursa_core::algo::{
    betweenness, betweenness_weighted, closeness, closeness_weighted, clustering_coefficient,
    connected_components_strong, connected_components_weak, degree, k_hop, label_propagation,
    louvain, louvain_weighted, neighbor_aggregate, pagerank, pagerank_weighted, random_walk,
    shortest_path, shortest_path_weighted_with_cost, triangle_count, AggKind, PageRankParams,
};
use ursa_core::{Direction, IdMap, Topology};

use crate::logical::GraphAlgo;

/// One requested output column.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputColumn {
    /// A node-valued algorithm over the topology. `weights` (an `f64` per edge
    /// row, gathered via `edge_ids`) is present for a weighted algorithm.
    Algo {
        name: String,
        algo: GraphAlgo,
        weights: Option<Arc<Vec<f64>>>,
    },
    /// A per-node aggregation of a (dense-aligned) attribute over neighbours.
    NeighborAgg {
        name: String,
        attr: Arc<Vec<Option<f64>>>,
        direction: Direction,
        agg: AggKind,
    },
}

impl OutputColumn {
    pub fn name(&self) -> &str {
        match self {
            OutputColumn::Algo { name, .. } | OutputColumn::NeighborAgg { name, .. } => name,
        }
    }

    fn value_type(&self) -> DataType {
        match self {
            OutputColumn::Algo { algo, .. } => match algo {
                GraphAlgo::PageRank { .. }
                | GraphAlgo::ClusteringCoefficient
                | GraphAlgo::Closeness
                | GraphAlgo::Betweenness { .. } => DataType::Float64,
                _ => DataType::UInt32,
            },
            OutputColumn::NeighborAgg { .. } => DataType::Float64,
        }
    }

    fn value_array(&self, topo: &Topology) -> ArrayRef {
        match self {
            OutputColumn::Algo { algo, weights, .. } => {
                algo_array(topo, algo, weights.as_deref().map(Vec::as_slice))
            }
            OutputColumn::NeighborAgg {
                attr,
                direction,
                agg,
                ..
            } => Arc::new(Float64Array::from(neighbor_aggregate(
                topo, attr, *direction, *agg,
            ))),
        }
    }
}

fn algo_array(topo: &Topology, algo: &GraphAlgo, weights: Option<&[f64]>) -> ArrayRef {
    match algo {
        GraphAlgo::Degree { direction } => {
            Arc::new(UInt32Array::from(degree(topo, None, (*direction).into())))
        }
        GraphAlgo::PageRank {
            damping,
            max_iter,
            tol,
        } => {
            let params = PageRankParams {
                damping: *damping,
                max_iter: *max_iter,
                tol: *tol,
            };
            let scores = match weights {
                Some(w) => pagerank_weighted(topo, w, None, params),
                None => pagerank(topo, None, params),
            };
            Arc::new(Float64Array::from(scores))
        }
        GraphAlgo::ConnectedComponents { strong } => {
            let labels = if *strong {
                connected_components_strong(topo)
            } else {
                connected_components_weak(topo)
            };
            Arc::new(UInt32Array::from(labels))
        }
        GraphAlgo::TriangleCount => Arc::new(UInt32Array::from(triangle_count(topo))),
        GraphAlgo::ClusteringCoefficient => {
            Arc::new(Float64Array::from(clustering_coefficient(topo)))
        }
        GraphAlgo::Closeness => {
            let scores = match weights {
                Some(w) => closeness_weighted(topo, w, None),
                None => closeness(topo, None),
            };
            Arc::new(Float64Array::from(scores))
        }
        GraphAlgo::Betweenness { sample, seed } => {
            let scores = match weights {
                Some(w) => betweenness_weighted(topo, w, *sample, *seed),
                None => betweenness(topo, *sample, *seed),
            };
            Arc::new(Float64Array::from(scores))
        }
        GraphAlgo::LabelPropagation { max_iter, seed } => {
            Arc::new(UInt32Array::from(label_propagation(topo, *max_iter, *seed)))
        }
        GraphAlgo::Louvain { resolution, seed } => {
            let labels = match weights {
                Some(w) => louvain_weighted(topo, w, *resolution, *seed),
                None => louvain(topo, *resolution, *seed),
            };
            Arc::new(UInt32Array::from(labels))
        }
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
pub fn query_batch(topo: &Topology, ids: &IdMap, columns: &[OutputColumn]) -> Result<RecordBatch> {
    let mut arrays: Vec<ArrayRef> = vec![ids.user_id_array()];
    for col in columns {
        arrays.push(col.value_array(topo));
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
    seeds: &[u32],
    n: u32,
    direction: Direction,
) -> Result<RecordBatch> {
    let (seed_dense, reached_dense) = k_hop(topo, seeds, n, direction);
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
        Some(w) => match shortest_path_weighted_with_cost(topo, w, source, target, direction) {
            Some((path, costs)) => (Some(path), Some(costs)),
            None => (None, None),
        },
        None => (shortest_path(topo, source, target, direction), None),
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
    starts: &[u32],
    steps: u32,
    walks_per_node: u32,
    seed: Option<u64>,
) -> Result<RecordBatch> {
    let walks = random_walk(topo, starts, steps, walks_per_node, seed);
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
    use crate::logical::Direction as PlanDirection;
    use crate::topology::build_topology;

    fn diamond() -> (Arc<Topology>, Arc<IdMap>) {
        let src = Int64Array::from(vec![0, 0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 2, 0]);
        build_topology(&src, &dst).unwrap()
    }

    #[test]
    fn multi_column_batch_is_aligned() {
        let (topo, ids) = diamond();
        let columns = vec![
            OutputColumn::Algo {
                name: "deg".to_string(),
                algo: GraphAlgo::Degree {
                    direction: PlanDirection::Out,
                },
                weights: None,
            },
            OutputColumn::Algo {
                name: "pr".to_string(),
                algo: GraphAlgo::PageRank {
                    damping: 0.85,
                    max_iter: 30,
                    tol: 1e-6,
                },
                weights: None,
            },
        ];
        let batch = query_batch(&topo, &ids, &columns).unwrap();
        assert_eq!(batch.num_columns(), 3); // id, deg, pr
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.schema().field(1).name(), "deg");
        assert_eq!(batch.schema().field(2).name(), "pr");
    }
}
