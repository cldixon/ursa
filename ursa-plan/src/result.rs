//! Assembling kernel outputs into Arrow `RecordBatch`es.
//!
//! The "Arrow arrays out" half of the operator contract. A query names one or
//! more output columns; each is either a node-valued algorithm over the topology
//! or a neighbour aggregation over a node attribute. Because every column is
//! evaluated in the same `IdMap` order they are all row-aligned, so the result is
//! a single `(id, col_1, col_2, ...)` batch with dense→user id translation done
//! once, here, at the boundary.

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float32Array, Float64Array, Int64Array, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use ursa_core::algo::{
    betweenness, betweenness_weighted, closeness, closeness_weighted, clustering_from_triangles,
    connected_components_strong, connected_components_weak, degree, k_hop, label_propagation,
    layout_circle, layout_fr, layout_random, louvain, louvain_weighted, neighbor_aggregate,
    pagerank, pagerank_weighted, per_node_triangles, random_walk, shortest_path,
    shortest_path_weighted_with_cost, undirected_view, AggKind, LayoutParams, PageRankParams,
    UndirectedView,
};
use ursa_core::{Direction, EdgeMask, IdMap, Topology};

use crate::logical::{Direction as PlanDirection, GraphAlgo, LayoutKind};

/// The Arrow dtype a float-valued output column is emitted as (#117). Kernels
/// always accumulate in `f64`; `F32` narrows the *emitted* column to halve wire and
/// on-disk size (e.g. cached layout positions), where screen-space precision is far
/// below `f64`'s 53 bits. Only legal on float-valued columns — validated where the
/// column is built (`query.rs`); integer columns keep their `u32` type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputDtype {
    #[default]
    F64,
    F32,
}

/// One requested output column.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputColumn {
    /// A node-valued algorithm over the topology. `weights` (an `f64` per edge
    /// row, gathered via `edge_ids`) is present for a weighted algorithm. `dtype`
    /// narrows a float-valued result to `f32` on emit (#117).
    Algo {
        name: String,
        algo: GraphAlgo,
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

    /// The column's Arrow type *before* any `dtype` narrowing — `Float64` for a
    /// float-valued kernel, `UInt32` for an integer-valued one. Public so `query.rs`
    /// can reject a nonsensical `f32` request on an integer column at build time.
    pub fn base_value_type(&self) -> DataType {
        match self {
            OutputColumn::Algo { algo, .. } => match algo {
                GraphAlgo::PageRank { .. }
                | GraphAlgo::ClusteringCoefficient
                | GraphAlgo::Closeness
                | GraphAlgo::Betweenness { .. } => DataType::Float64,
                // Positions are f32 at the source: the kernel computes in f32 and
                // there is no wider value to narrow from, so `dtype` has nothing
                // to do here.
                GraphAlgo::Layout { .. } => DataType::Float32,
                _ => DataType::UInt32,
            },
            OutputColumn::NeighborAgg { .. } => DataType::Float64,
        }
    }

    /// Whether this column emits a floating-point value (the only kind `f32` narrowing
    /// applies to).
    pub fn is_float_valued(&self) -> bool {
        self.base_value_type() == DataType::Float64
    }

    fn value_type(&self) -> DataType {
        match self.dtype() {
            // `F32` is only ever set on a float-valued column (query.rs validates), so
            // narrowing the base `Float64` to `Float32` is always well-typed.
            OutputDtype::F32 => DataType::Float32,
            OutputDtype::F64 => self.base_value_type(),
        }
    }
}

/// A canonical, hashable identity for one kernel computation (#115).
///
/// `GraphAlgo` carries `f64` parameters, so it is neither `Hash` nor `Eq`. Float
/// fields are keyed on their bit patterns, which is exactly the equality wanted
/// here: two columns share a computation only if they named literally the same
/// parameters. Weights compare by `Arc` identity — the same notion
/// `ShortestPathNode` already uses, and the same one the plan node's cache key
/// applies to the mask.
///
/// The output `dtype` is deliberately **not** part of the key: it narrows on emit,
/// after the kernel has run, so `f64` and `f32` spellings of one score share a
/// single computation.
#[derive(PartialEq, Eq, Hash)]
struct KernelKey {
    /// Which `GraphAlgo` variant.
    tag: u8,
    /// Bit-encoded scalar parameters, in a fixed per-variant order.
    params: Vec<u64>,
    /// The weight array by `Arc` identity; `None` for an unweighted kernel.
    weights: Option<usize>,
}

fn kernel_key(algo: &GraphAlgo, weights: Option<&Arc<Vec<f64>>>) -> KernelKey {
    // Each optional parameter contributes a presence flag alongside its value, so
    // `None` can never collide with a `Some` that happens to carry the same bits.
    fn opt_f64(v: &Option<f64>) -> [u64; 2] {
        match v {
            Some(x) => [1, x.to_bits()],
            None => [0, 0],
        }
    }
    fn opt_u64(v: &Option<u64>) -> [u64; 2] {
        match v {
            Some(x) => [1, *x],
            None => [0, 0],
        }
    }
    let (tag, params): (u8, Vec<u64>) = match algo {
        GraphAlgo::PageRank {
            damping,
            max_iter,
            tol,
        } => (0, vec![damping.to_bits(), *max_iter as u64, tol.to_bits()]),
        GraphAlgo::ConnectedComponents { strong } => (1, vec![*strong as u64]),
        GraphAlgo::Degree { direction } => (
            2,
            vec![match direction {
                PlanDirection::Out => 0,
                PlanDirection::In => 1,
                PlanDirection::Both => 2,
            }],
        ),
        GraphAlgo::TriangleCount => (3, Vec::new()),
        GraphAlgo::ClusteringCoefficient => (4, Vec::new()),
        GraphAlgo::Betweenness { sample, seed } => {
            let mut p = Vec::from(opt_f64(sample));
            p.extend(opt_u64(seed));
            (5, p)
        }
        GraphAlgo::Closeness => (6, Vec::new()),
        GraphAlgo::LabelPropagation { max_iter, seed } => {
            let mut p = vec![*max_iter as u64];
            p.extend(opt_u64(seed));
            (7, p)
        }
        GraphAlgo::Louvain { resolution, seed } => {
            let mut p = vec![resolution.to_bits()];
            p.extend(opt_u64(seed));
            (8, p)
        }
        GraphAlgo::Layout {
            kind,
            iterations,
            k,
            gravity,
            seed,
        } => {
            // `field` is deliberately absent: x and y must produce the *same* key,
            // because sharing one simulation between them is the entire point.
            let mut p = vec![
                match kind {
                    LayoutKind::Fr => 0,
                    LayoutKind::Random => 1,
                    LayoutKind::Circle => 2,
                },
                *iterations as u64,
                k.to_bits(),
                gravity.to_bits(),
            ];
            p.extend(opt_u64(seed));
            (9, p)
        }
    };
    KernelKey {
        tag,
        params,
        weights: weights.map(|w| Arc::as_ptr(w) as usize),
    }
}

/// Per-query kernel evaluation, memoized (#115).
///
/// A query can name several output columns that rest on the same work, in two
/// distinct ways — this handles both:
///
/// 1. **The same computation under two names.** `with_columns(a=pagerank(e),
///    b=pagerank(e, dtype="f32"))` is one kernel run emitted twice; `dtype`
///    narrows afterwards and so is not part of the key.
/// 2. **Different kernels over a shared intermediate.** `triangle_count` and
///    `clustering_coefficient` both rest on one sorted-adjacency intersection pass
///    over one undirected view — the expensive half of each. Naming both in one
///    `with_columns` used to do that work twice (and, under a mask, rebuild the
///    masked undirected view twice as well, since only the *unmasked* view is
///    cached on the topology).
///
/// Lives for exactly one `query_batch` call. That is what makes the keys sound:
/// the topology and mask are fixed for its lifetime, so a key needs to encode only
/// the algorithm and its parameters, not the graph they run over.
struct KernelEval<'a> {
    topo: &'a Topology,
    mask: Option<&'a EdgeMask>,
    /// Kernel results by computation key.
    ///
    /// A `Vec` per entry because a kernel may emit several columns from one
    /// invocation — layout emits x and y. A repeat column is an `Arc` clone of an
    /// element rather than a recomputation.
    memo: HashMap<KernelKey, Arc<Vec<ArrayRef>>>,
    /// The undirected view and its per-node triangle counts — the intermediate the
    /// triangle-family kernels share. Computed at most once, on first use.
    triangles: Option<(UndirectedView<'a>, Vec<u32>)>,
}

impl<'a> KernelEval<'a> {
    fn new(topo: &'a Topology, mask: Option<&'a EdgeMask>) -> Self {
        KernelEval {
            topo,
            mask,
            memo: HashMap::new(),
            triangles: None,
        }
    }

    /// The undirected view and per-node triangle counts, computed on first use.
    fn triangles(&mut self) -> &(UndirectedView<'a>, Vec<u32>) {
        if self.triangles.is_none() {
            let view = undirected_view(self.topo, self.mask);
            let counts = per_node_triangles(view.get());
            self.triangles = Some((view, counts));
        }
        self.triangles
            .as_ref()
            .expect("the intermediate was just populated")
    }

    /// Run one kernel, returning every column it produces.
    ///
    /// One element for all but layout, which returns x and y from a single
    /// simulation. Callers go through [`Self::column_array`], which memoizes.
    fn compute(&mut self, algo: &GraphAlgo, weights: Option<&[f64]>) -> Vec<ArrayRef> {
        let (topo, mask) = (self.topo, self.mask);
        match algo {
            GraphAlgo::Layout {
                kind,
                iterations,
                k,
                gravity,
                seed,
            } => {
                let params = LayoutParams {
                    iterations: *iterations,
                    k: *k as f32,
                    gravity: *gravity as f32,
                    seed: *seed,
                };
                let (x, y) = match kind {
                    LayoutKind::Fr => layout_fr(topo, mask, params),
                    LayoutKind::Random => layout_random(topo, params),
                    LayoutKind::Circle => layout_circle(topo, params),
                };
                vec![
                    Arc::new(Float32Array::from(x)) as ArrayRef,
                    Arc::new(Float32Array::from(y)) as ArrayRef,
                ]
            }
            GraphAlgo::Degree { direction } => {
                vec![
                    Arc::new(UInt32Array::from(degree(topo, mask, (*direction).into())))
                        as ArrayRef,
                ]
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
                    Some(w) => pagerank_weighted(topo, w, mask, params),
                    None => pagerank(topo, mask, params),
                };
                vec![Arc::new(Float64Array::from(scores)) as ArrayRef]
            }
            GraphAlgo::ConnectedComponents { strong } => {
                let labels = if *strong {
                    connected_components_strong(topo, mask)
                } else {
                    connected_components_weak(topo, mask)
                };
                vec![Arc::new(UInt32Array::from(labels)) as ArrayRef]
            }
            // The triangle-family pair: both read the shared intermediate, so
            // whichever is named first pays for the intersection pass and the other
            // is a cheap derivation of it.
            GraphAlgo::TriangleCount => {
                if topo.n_nodes() == 0 {
                    return vec![Arc::new(UInt32Array::from(Vec::<u32>::new())) as ArrayRef];
                }
                let (_, counts) = self.triangles();
                vec![Arc::new(UInt32Array::from(counts.clone())) as ArrayRef]
            }
            GraphAlgo::ClusteringCoefficient => {
                if topo.n_nodes() == 0 {
                    return vec![Arc::new(Float64Array::from(Vec::<f64>::new())) as ArrayRef];
                }
                let (view, counts) = self.triangles();
                vec![Arc::new(Float64Array::from(clustering_from_triangles(
                    view.get(),
                    counts,
                ))) as ArrayRef]
            }
            GraphAlgo::Closeness => {
                let scores = match weights {
                    Some(w) => closeness_weighted(topo, w, mask),
                    None => closeness(topo, mask),
                };
                vec![Arc::new(Float64Array::from(scores)) as ArrayRef]
            }
            GraphAlgo::Betweenness { sample, seed } => {
                let scores = match weights {
                    Some(w) => betweenness_weighted(topo, w, mask, *sample, *seed),
                    None => betweenness(topo, mask, *sample, *seed),
                };
                vec![Arc::new(Float64Array::from(scores)) as ArrayRef]
            }
            GraphAlgo::LabelPropagation { max_iter, seed } => vec![Arc::new(UInt32Array::from(
                label_propagation(topo, mask, *max_iter, *seed),
            )) as ArrayRef],
            GraphAlgo::Louvain { resolution, seed } => {
                let labels = match weights {
                    Some(w) => louvain_weighted(topo, w, mask, *resolution, *seed),
                    None => louvain(topo, mask, *resolution, *seed),
                };
                vec![Arc::new(UInt32Array::from(labels)) as ArrayRef]
            }
        }
    }

    /// The array for one output column, at its requested dtype.
    ///
    /// Narrowing happens *after* the memo, so two columns differing only in `dtype`
    /// share the kernel run and diverge only at the cast.
    fn column_array(&mut self, col: &OutputColumn) -> ArrayRef {
        let base = match col {
            OutputColumn::Algo {
                algo,
                weights,
                field,
                ..
            } => {
                let key = kernel_key(algo, weights.as_ref());
                // Clone the hit out before computing: the borrow of `self.memo` must
                // end before `compute` takes `&mut self`.
                let outputs = match self.memo.get(&key).cloned() {
                    Some(hit) => hit,
                    None => {
                        let arrays =
                            Arc::new(self.compute(algo, weights.as_deref().map(Vec::as_slice)));
                        self.memo.insert(key, Arc::clone(&arrays));
                        arrays
                    }
                };
                // A field past the kernel's output count is a plan-construction
                // bug, not user input — query.rs only ever sets a field the kernel
                // declares — so fall back to the first column rather than panic
                // across the FFI.
                Arc::clone(outputs.get(*field).unwrap_or(&outputs[0]))
            }
            // Not memoized: a neighbour aggregation is a single segmented CSR
            // reduction — the cheapest column kind there is, so sharing one would
            // buy less than the key it costs to build.
            OutputColumn::NeighborAgg {
                attr,
                direction,
                agg,
                ..
            } => Arc::new(Float64Array::from(neighbor_aggregate(
                self.topo, attr, self.mask, *direction, *agg,
            ))),
        };
        match col.dtype() {
            // Narrow the f64 kernel output to f32 (#117). Casting Float64 -> Float32 is
            // infallible; the `expect` can only fire on a validation bug (f32 requested
            // for a non-float column), which query.rs rejects before we get here.
            OutputDtype::F32 => arrow::compute::cast(&base, &DataType::Float32)
                .expect("narrowing a float column to f32 is infallible"),
            OutputDtype::F64 => base,
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
pub fn query_batch(
    topo: &Topology,
    ids: &IdMap,
    columns: &[OutputColumn],
    mask: Option<&EdgeMask>,
) -> Result<RecordBatch> {
    let mut eval = KernelEval::new(topo, mask);
    let mut arrays: Vec<ArrayRef> = vec![ids.user_id_array()];
    for col in columns {
        arrays.push(eval.column_array(col));
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
                dtype: OutputDtype::F64,
                field: 0,
            },
            OutputColumn::Algo {
                name: "pr".to_string(),
                algo: GraphAlgo::PageRank {
                    damping: 0.85,
                    max_iter: 30,
                    tol: 1e-6,
                },
                weights: None,
                dtype: OutputDtype::F64,
                field: 0,
            },
        ];
        let batch = query_batch(&topo, &ids, &columns, None).unwrap();
        assert_eq!(batch.num_columns(), 3); // id, deg, pr
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.schema().field(1).name(), "deg");
        assert_eq!(batch.schema().field(2).name(), "pr");
    }

    #[test]
    fn f32_dtype_narrows_a_float_column_to_float32_and_matches_f64() {
        use arrow::array::{Float32Array, Float64Array};

        let (topo, ids) = diamond();
        let pr = |dtype| OutputColumn::Algo {
            name: "pr".to_string(),
            algo: GraphAlgo::PageRank {
                damping: 0.85,
                max_iter: 30,
                tol: 1e-6,
            },
            weights: None,
            dtype,
            field: 0,
        };
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
        // The JSON path (execute_node_query) is where the float-only rule is enforced;
        // a degree column asking for f32 must error, not silently emit a lossy float.
        let (topo, ids) = diamond();
        let json = r#"[{"name":"deg","kind":"degree","direction":"out","dtype":"f32"}]"#;
        let err = crate::query::execute_node_query(
            topo,
            ids,
            json,
            &[],        // filters
            None,       // sort
            None,       // limit
            None,       // nodes
            None,       // nodes_id
            None,       // edges
            false,      // distinct
            None,       // sample
            Vec::new(), // rename
            Vec::new(), // group_keys
            Vec::new(), // aggs
            None,       // mask
        );
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("f32"), "unexpected error: {msg}");
    }

    // --- #115: sharing work across the columns of one query --------------------

    fn algo_col(name: &str, algo: GraphAlgo, dtype: OutputDtype) -> OutputColumn {
        OutputColumn::Algo {
            name: name.to_string(),
            algo,
            weights: None,
            dtype,
            field: 0,
        }
    }

    fn pagerank_algo() -> GraphAlgo {
        GraphAlgo::PageRank {
            damping: 0.85,
            max_iter: 30,
            tol: 1e-6,
        }
    }

    /// A graph with actual triangles, so the triangle-family kernels have work to
    /// share and non-trivial values to compare: the triangle 0-1-2, plus 3 hanging
    /// off 0 (degree 1 -> clustering 0) and the chord 1-3.
    fn triangly() -> (Arc<Topology>, Arc<IdMap>) {
        let src = Int64Array::from(vec![0, 1, 2, 0, 1]);
        let dst = Int64Array::from(vec![1, 2, 0, 3, 3]);
        build_topology(&src, &dst).unwrap()
    }

    #[test]
    fn two_columns_naming_one_kernel_share_a_single_run() {
        let (topo, _ids) = diamond();
        let mut eval = KernelEval::new(&topo, None);
        let a = eval.column_array(&algo_col("a", pagerank_algo(), OutputDtype::F64));
        let b = eval.column_array(&algo_col("b", pagerank_algo(), OutputDtype::F64));

        assert_eq!(eval.memo.len(), 1, "one computation should be cached");
        // The strongest available proof of sharing: both columns are literally the
        // same allocation, so the second cannot have re-run the kernel.
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn dtype_is_not_part_of_the_key_so_f64_and_f32_share_the_run() {
        use arrow::array::{Float32Array, Float64Array};

        let (topo, _ids) = diamond();
        let mut eval = KernelEval::new(&topo, None);
        let wide = eval.column_array(&algo_col("a", pagerank_algo(), OutputDtype::F64));
        let narrow = eval.column_array(&algo_col("b", pagerank_algo(), OutputDtype::F32));

        assert_eq!(eval.memo.len(), 1, "narrowing happens after the memo");
        assert_eq!(narrow.data_type(), &DataType::Float32);

        let wide = wide.as_any().downcast_ref::<Float64Array>().unwrap();
        let narrow = narrow.as_any().downcast_ref::<Float32Array>().unwrap();
        for i in 0..wide.len() {
            assert_eq!(narrow.value(i), wide.value(i) as f32);
        }
    }

    #[test]
    fn differing_parameters_do_not_share_a_run() {
        let (topo, _ids) = diamond();
        let mut eval = KernelEval::new(&topo, None);
        eval.column_array(&algo_col("a", pagerank_algo(), OutputDtype::F64));
        eval.column_array(&algo_col(
            "b",
            GraphAlgo::PageRank {
                damping: 0.5, // a different damping is a different computation
                max_iter: 30,
                tol: 1e-6,
            },
            OutputDtype::F64,
        ));
        // Also distinct: a different *variant* entirely.
        eval.column_array(&algo_col(
            "c",
            GraphAlgo::Degree {
                direction: PlanDirection::Out,
            },
            OutputDtype::F64,
        ));
        assert_eq!(eval.memo.len(), 3);
    }

    #[test]
    fn an_optional_parameter_that_is_absent_never_collides_with_one_that_is_set() {
        // seed=None and seed=Some(0) must key differently — the presence flag in
        // `kernel_key` is what stops `None` and a zero-valued `Some` colliding.
        let none = kernel_key(
            &GraphAlgo::LabelPropagation {
                max_iter: 20,
                seed: None,
            },
            None,
        );
        let zero = kernel_key(
            &GraphAlgo::LabelPropagation {
                max_iter: 20,
                seed: Some(0),
            },
            None,
        );
        assert!(none != zero);
    }

    fn layout_col(name: &str, field: usize) -> OutputColumn {
        OutputColumn::Algo {
            name: name.to_string(),
            algo: GraphAlgo::Layout {
                kind: LayoutKind::Fr,
                iterations: 20,
                k: 1.0,
                gravity: 0.02,
                seed: Some(1),
            },
            weights: None,
            dtype: OutputDtype::F64,
            field,
        }
    }

    #[test]
    fn a_layout_runs_once_for_both_of_its_columns() {
        // The multi-output half of #115. `x` and `y` are two columns over one
        // simulation, so the field selector must not reach the memo key — if it
        // did, each axis would come from a *different* run of a stochastic layout
        // and the drawing would be incoherent while every column still looked
        // individually plausible.
        let (topo, _ids) = diamond();
        let mut eval = KernelEval::new(&topo, None);
        let x = eval.column_array(&layout_col("x", 0));
        let y = eval.column_array(&layout_col("y", 1));

        assert_eq!(eval.memo.len(), 1, "x and y should share one cached run");
        assert_eq!(x.data_type(), &DataType::Float32);
        assert_eq!(y.data_type(), &DataType::Float32);
        // Distinct arrays selected out of one result, not the same array twice.
        assert!(!Arc::ptr_eq(&x, &y));
        assert_eq!(x.len(), y.len());
    }

    #[test]
    fn layout_kinds_and_parameters_key_separately() {
        let (topo, _ids) = diamond();
        let mut eval = KernelEval::new(&topo, None);
        eval.column_array(&layout_col("x", 0));
        let mut circle = layout_col("c", 0);
        if let OutputColumn::Algo { algo, .. } = &mut circle {
            *algo = GraphAlgo::Layout {
                kind: LayoutKind::Circle,
                iterations: 20,
                k: 1.0,
                gravity: 0.02,
                seed: Some(1),
            };
        }
        eval.column_array(&circle);
        assert_eq!(eval.memo.len(), 2, "a different layout is a different run");
    }

    #[test]
    fn triangle_family_columns_share_one_intersection_pass() {
        let (topo, _ids) = triangly();
        let mut eval = KernelEval::new(&topo, None);

        eval.column_array(&algo_col("tri", GraphAlgo::TriangleCount, OutputDtype::F64));
        let first = eval.triangles.as_ref().unwrap().1.as_ptr();

        eval.column_array(&algo_col(
            "cc",
            GraphAlgo::ClusteringCoefficient,
            OutputDtype::F64,
        ));
        let second = eval.triangles.as_ref().unwrap().1.as_ptr();

        // Same allocation backing both columns' triangle counts: the clustering
        // column reused the pass the triangle column paid for.
        assert_eq!(
            first, second,
            "clustering_coefficient re-ran the triangle pass"
        );
    }

    /// The refactor that enabled the sharing split `clustering_coefficient` into
    /// `per_node_triangles` + `clustering_from_triangles`. These pin the shared path
    /// to the standalone kernels, masked and unmasked — the regression that would
    /// matter most if the split were ever wrong.
    #[test]
    fn shared_path_matches_the_standalone_kernels() {
        use arrow::array::Float64Array;
        use ursa_core::algo::{clustering_coefficient, triangle_count};

        let (topo, _ids) = triangly();
        for mask in [
            None,
            Some(EdgeMask::from_bools(&[true, true, true, false, true])),
        ] {
            let mut eval = KernelEval::new(&topo, mask.as_ref());
            let tri =
                eval.column_array(&algo_col("tri", GraphAlgo::TriangleCount, OutputDtype::F64));
            let cc = eval.column_array(&algo_col(
                "cc",
                GraphAlgo::ClusteringCoefficient,
                OutputDtype::F64,
            ));

            let want_tri = triangle_count(&topo, mask.as_ref());
            let want_cc = clustering_coefficient(&topo, mask.as_ref());

            let tri = tri.as_any().downcast_ref::<UInt32Array>().unwrap();
            let cc = cc.as_any().downcast_ref::<Float64Array>().unwrap();
            assert_eq!(tri.values(), &want_tri[..], "triangle counts diverged");
            // Exact equality, not approximate: the shared path must run the identical
            // float operations, not merely a close approximation of them.
            assert_eq!(
                cc.values(),
                &want_cc[..],
                "clustering coefficients diverged"
            );
        }
    }

    #[test]
    fn the_triangle_family_survives_an_empty_graph() {
        // n_nodes == 0 short-circuits before the undirected view is resolved; both
        // columns must still produce a well-typed empty array.
        let src = Int64Array::from(Vec::<i64>::new());
        let dst = Int64Array::from(Vec::<i64>::new());
        let (topo, ids) = build_topology(&src, &dst).unwrap();
        let columns = vec![
            algo_col("tri", GraphAlgo::TriangleCount, OutputDtype::F64),
            algo_col("cc", GraphAlgo::ClusteringCoefficient, OutputDtype::F64),
        ];
        let batch = query_batch(&topo, &ids, &columns, None).unwrap();
        assert_eq!(batch.num_rows(), 0);
        assert_eq!(batch.schema().field(1).data_type(), &DataType::UInt32);
        assert_eq!(batch.schema().field(2).data_type(), &DataType::Float64);
    }
}
