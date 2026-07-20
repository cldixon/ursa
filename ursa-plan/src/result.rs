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
use ursa_core::algo::{
    betweenness, closeness, clustering_coefficient, connected_components_weak, degree, k_hop,
    label_propagation, louvain, neighbor_aggregate, pagerank, random_walk, shortest_path,
    triangle_count, AggKind, PageRankParams,
};
use ursa_core::{Direction, IdMap, Topology};

use crate::logical::GraphAlgo;

/// One requested output column.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputColumn {
    /// A node-valued algorithm over the topology.
    Algo { name: String, algo: GraphAlgo },
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
            OutputColumn::Algo { algo, .. } => algo_array(topo, algo),
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

/// Whether an algorithm has a kernel wired into the execution path.
pub fn is_executable(algo: &GraphAlgo) -> bool {
    matches!(
        algo,
        GraphAlgo::Degree { .. }
            | GraphAlgo::PageRank { .. }
            | GraphAlgo::ConnectedComponents { .. }
            | GraphAlgo::TriangleCount
            | GraphAlgo::ClusteringCoefficient
            | GraphAlgo::Closeness
            | GraphAlgo::Betweenness { .. }
            | GraphAlgo::LabelPropagation { .. }
            | GraphAlgo::Louvain { .. }
    )
}

fn algo_array(topo: &Topology, algo: &GraphAlgo) -> ArrayRef {
    match algo {
        GraphAlgo::Degree { direction } => {
            Arc::new(UInt32Array::from(degree(topo, (*direction).into())))
        }
        GraphAlgo::PageRank {
            damping,
            max_iter,
            tol,
        } => Arc::new(Float64Array::from(pagerank(
            topo,
            PageRankParams {
                damping: *damping,
                max_iter: *max_iter,
                tol: *tol,
            },
        ))),
        GraphAlgo::ConnectedComponents { .. } => {
            Arc::new(UInt32Array::from(connected_components_weak(topo)))
        }
        GraphAlgo::TriangleCount => Arc::new(UInt32Array::from(triangle_count(topo))),
        GraphAlgo::ClusteringCoefficient => {
            Arc::new(Float64Array::from(clustering_coefficient(topo)))
        }
        GraphAlgo::Closeness => Arc::new(Float64Array::from(closeness(topo))),
        GraphAlgo::Betweenness { sample } => {
            Arc::new(Float64Array::from(betweenness(topo, *sample)))
        }
        GraphAlgo::LabelPropagation { max_iter, seed } => {
            Arc::new(UInt32Array::from(label_propagation(topo, *max_iter, *seed)))
        }
        GraphAlgo::Louvain { resolution, seed } => {
            Arc::new(UInt32Array::from(louvain(topo, *resolution, *seed)))
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

/// Materialize the `(id, values...)` batch for a query.
pub fn query_batch(topo: &Topology, ids: &IdMap, columns: &[OutputColumn]) -> RecordBatch {
    let mut arrays: Vec<ArrayRef> = vec![ids.user_id_array()];
    for col in columns {
        arrays.push(col.value_array(topo));
    }
    RecordBatch::try_new(query_schema(columns, ids.user_type()), arrays)
        .expect("all columns have length n_nodes and match the schema")
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
) -> RecordBatch {
    let (seed_dense, reached_dense) = k_hop(topo, seeds, n, direction);
    RecordBatch::try_new(
        hop_schema(ids.user_type()),
        vec![
            ids.gather_user(&seed_dense),
            ids.gather_user(&reached_dense),
        ],
    )
    .expect("src and dst are equal-length id columns matching the hop schema")
}

/// The output schema for a `shortest_path`: an edge frame `(src, dst, hop)` — one
/// row per edge on the path, in order, with `hop` the 0-based position. `src`/`dst`
/// carry the graph's user-id type; `hop` is Int64. All non-null.
pub fn path_schema(id_type: DataType) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("src", id_type.clone(), false),
        Field::new("dst", id_type, false),
        Field::new("hop", DataType::Int64, false),
    ]))
}

/// Materialize a `shortest_path`'s `(src, dst, hop)` batch: run the unweighted BFS
/// path kernel and zip the dense node sequence into consecutive edges (translated
/// back to user ids). An unreachable target (or a trivial one-node path) yields an
/// empty batch.
pub fn path_batch(
    topo: &Topology,
    ids: &IdMap,
    source: u32,
    target: u32,
    direction: Direction,
) -> RecordBatch {
    let mut src_dense = Vec::new();
    let mut dst_dense = Vec::new();
    let mut hop = Vec::new();
    if let Some(nodes) = shortest_path(topo, source, target, direction) {
        for (i, window) in nodes.windows(2).enumerate() {
            src_dense.push(window[0]);
            dst_dense.push(window[1]);
            hop.push(i as i64);
        }
    }
    RecordBatch::try_new(
        path_schema(ids.user_type()),
        vec![
            ids.gather_user(&src_dense),
            ids.gather_user(&dst_dense),
            Arc::new(Int64Array::from(hop)),
        ],
    )
    .expect("src, dst, hop are equal-length columns matching the path schema")
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
) -> RecordBatch {
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
    .expect("walk_id, step, node are equal-length columns matching the walk schema")
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
            },
            OutputColumn::Algo {
                name: "pr".to_string(),
                algo: GraphAlgo::PageRank {
                    damping: 0.85,
                    max_iter: 30,
                    tol: 1e-6,
                },
            },
        ];
        let batch = query_batch(&topo, &ids, &columns);
        assert_eq!(batch.num_columns(), 3); // id, deg, pr
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.schema().field(1).name(), "deg");
        assert_eq!(batch.schema().field(2).name(), "pr");
    }
}
