//! Dispatch: computing a [`Call`]'s output columns over a topology.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float32Array, Float64Array, Int64Array, UInt32Array};
use arrow::datatypes::DataType;

use super::call::Call;
use super::stats;
use crate::algo::{
    betweenness, betweenness_weighted, closeness_directed, closeness_weighted_directed,
    clustering_from_triangles, connected_components_strong, connected_components_weak, degree,
    label_propagation, layout_circle, layout_fr, layout_random, louvain, louvain_weighted,
    neighbor_aggregate, pagerank, pagerank_weighted, per_node_triangles, symmetric_view,
    symmetric_view_weighted, undirected_view, LayoutParams, PageRankParams, UndirectedView,
};
use crate::topology::{Direction, EdgeMask, Topology};

/// The Arrow type a float-valued output column is emitted as. Kernels always
/// accumulate in `f64`; `F32` narrows the emitted column (half the size), for a
/// function whose spec takes a `dtype`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OutputDtype {
    #[default]
    F64,
    F32,
}

/// What a call reads besides the topology, already evaluated by the front end.
#[derive(Debug, Clone, Copy, Default)]
pub struct Inputs<'a> {
    /// One weight per edge row (indexed like the edge rows the topology was
    /// built from, not by CSR slot), for a function that takes a `weight`.
    /// `None` is unweighted.
    pub weights: Option<&'a [f64]>,
    /// One value per node in dense order (`None`: no value), for a neighbour
    /// aggregation.
    pub attribute: Option<&'a [Option<f64>]>,
}

/// A call the evaluator cannot run with the inputs it was given. Each is a
/// front end's mistake, not a user's: the specs say what a call takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchError {
    /// Weights were given to a function without a `weight` parameter.
    UnexpectedWeights { function: &'static str },
    /// The weights are not one per edge row.
    WeightCount { expected: usize, got: usize },
    /// A neighbour aggregation was called without its attribute.
    MissingAttribute { function: &'static str },
    /// An attribute was given to a function that does not read one.
    UnexpectedAttribute { function: &'static str },
    /// The attribute is not one value per node.
    AttributeCount { expected: usize, got: usize },
    /// The function has fewer outputs than the one asked for.
    NoSuchOutput {
        function: &'static str,
        output: usize,
    },
    /// `F32` was asked of a function without a `dtype` parameter.
    NotNarrowable { function: &'static str },
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DispatchError::UnexpectedWeights { function } => {
                write!(f, "{function}() takes no weight")
            }
            DispatchError::WeightCount { expected, got } => {
                write!(
                    f,
                    "expected one weight per edge row ({expected}); got {got}"
                )
            }
            DispatchError::MissingAttribute { function } => {
                write!(f, "{function}() needs a node attribute")
            }
            DispatchError::UnexpectedAttribute { function } => {
                write!(f, "{function}() takes no node attribute")
            }
            DispatchError::AttributeCount { expected, got } => {
                write!(
                    f,
                    "expected one attribute value per node ({expected}); got {got}"
                )
            }
            DispatchError::NoSuchOutput { function, output } => {
                write!(f, "{function}() has no output {output}")
            }
            DispatchError::NotNarrowable { function } => {
                write!(f, "{function}() has no float output to narrow to f32")
            }
        }
    }
}

impl std::error::Error for DispatchError {}

/// Evaluates calls over one topology and mask, sharing work between them.
///
/// Calls made through one evaluator share:
///
/// 1. **A repeated call.** The same [`Call`] over the same weights is computed
///    once; asking for it again, at another dtype or for another of its
///    outputs, returns the stored arrays. Weights are told apart by address,
///    which the `'a` borrow keeps fixed for the evaluator's life.
/// 2. **The triangle pass.** `triangle_count` and `clustering_coefficient` both
///    rest on one undirected view and one sorted-adjacency intersection pass,
///    computed by whichever is asked for first.
/// 3. **The symmetric views** the `direction='both'` forms of `pagerank` and
///    `betweenness` run over: built once (once per weight column, weighted),
///    and taken from the topology's own cache when there is no mask.
/// 4. **A layout's two columns**, `x` and `y`, from one simulation.
///
/// A neighbour aggregation is not stored: it is a single segmented reduction,
/// cheaper than the key it would need.
pub struct Evaluator<'a> {
    topo: &'a Topology,
    mask: Option<&'a EdgeMask>,
    /// Computed outputs, by call and weight address.
    pub(super) memo: HashMap<(Call, Option<usize>), Arc<[ArrayRef]>>,
    /// The undirected view and its per-node triangle counts.
    pub(super) triangles: Option<(UndirectedView<'a>, Vec<u32>)>,
    /// The masked unweighted symmetric view (unmasked, the topology caches it).
    symmetric: Option<Topology>,
    /// The weighted symmetric views and their per-row weights, by the address of
    /// the weight column.
    symmetric_weighted: HashMap<usize, (Topology, Vec<f64>)>,
}

impl<'a> Evaluator<'a> {
    /// An evaluator over `topo`, reading only the edge rows `mask` keeps
    /// (`None`: every row).
    pub fn new(topo: &'a Topology, mask: Option<&'a EdgeMask>) -> Self {
        Evaluator {
            topo,
            mask,
            memo: HashMap::new(),
            triangles: None,
            symmetric: None,
            symmetric_weighted: HashMap::new(),
        }
    }

    /// Every output column of `call`, at its spec's types, in its spec's order.
    ///
    /// A per-node function's columns have one row per node in dense order; a
    /// statistic's have one row.
    pub fn evaluate(
        &mut self,
        call: &Call,
        inputs: Inputs<'a>,
    ) -> Result<Arc<[ArrayRef]>, DispatchError> {
        let spec = call.spec();
        if let Some(w) = inputs.weights {
            if !spec.weighted() {
                return Err(DispatchError::UnexpectedWeights {
                    function: spec.name,
                });
            }
            if w.len() != self.topo.n_edges() {
                return Err(DispatchError::WeightCount {
                    expected: self.topo.n_edges(),
                    got: w.len(),
                });
            }
        }
        if let Call::NeighborAgg { direction, agg } = *call {
            let attr = inputs.attribute.ok_or(DispatchError::MissingAttribute {
                function: spec.name,
            })?;
            if attr.len() != self.topo.n_nodes() {
                return Err(DispatchError::AttributeCount {
                    expected: self.topo.n_nodes(),
                    got: attr.len(),
                });
            }
            let values = neighbor_aggregate(self.topo, attr, self.mask, direction, agg);
            return Ok(Arc::from(
                [Arc::new(Float64Array::from(values)) as ArrayRef],
            ));
        }
        if inputs.attribute.is_some() {
            return Err(DispatchError::UnexpectedAttribute {
                function: spec.name,
            });
        }
        let key = (*call, inputs.weights.map(|w| w.as_ptr() as usize));
        if let Some(hit) = self.memo.get(&key) {
            return Ok(Arc::clone(hit));
        }
        let arrays: Arc<[ArrayRef]> = Arc::from(self.compute(call, inputs.weights));
        self.memo.insert(key, Arc::clone(&arrays));
        Ok(arrays)
    }

    /// One output column of `call`, emitted at `dtype`.
    ///
    /// Narrowing happens after the shared computation, so the `f64` and `f32`
    /// forms of one call cost one run.
    pub fn column(
        &mut self,
        call: &Call,
        inputs: Inputs<'a>,
        output: usize,
        dtype: OutputDtype,
    ) -> Result<ArrayRef, DispatchError> {
        let function = call.name();
        if dtype == OutputDtype::F32 && !call.spec().narrows() {
            return Err(DispatchError::NotNarrowable { function });
        }
        let outputs = self.evaluate(call, inputs)?;
        let base = outputs
            .get(output)
            .ok_or(DispatchError::NoSuchOutput { function, output })?;
        Ok(match dtype {
            // Float64 -> Float32 is an infallible cast (nulls kept).
            OutputDtype::F32 => arrow::compute::cast(base, &DataType::Float32)
                .expect("narrowing a float column to f32 is infallible"),
            OutputDtype::F64 => Arc::clone(base),
        })
    }

    /// The unweighted symmetric view: the topology's cached one when unmasked,
    /// otherwise built on first use.
    fn symmetric(&mut self) -> &Topology {
        let (topo, mask) = (self.topo, self.mask);
        match mask {
            None => topo.symmetric(),
            Some(m) => self
                .symmetric
                .get_or_insert_with(|| symmetric_view(topo, Some(m))),
        }
    }

    /// The weighted symmetric view over `weights`, built on first use.
    fn symmetric_weighted(&mut self, weights: &[f64]) -> (&Topology, &[f64]) {
        let (topo, mask) = (self.topo, self.mask);
        let (sym, sw) = self
            .symmetric_weighted
            .entry(weights.as_ptr() as usize)
            .or_insert_with(|| symmetric_view_weighted(topo, weights, mask));
        (sym, sw)
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

    /// Run one call, returning every column it produces. `weights` is checked
    /// by the caller.
    fn compute(&mut self, call: &Call, weights: Option<&[f64]>) -> Vec<ArrayRef> {
        let (topo, mask) = (self.topo, self.mask);
        let one = |a: ArrayRef| vec![a];
        match *call {
            Call::LayoutFr {
                iterations,
                k,
                gravity,
                seed,
            } => xy(layout_fr(
                topo,
                mask,
                layout_params(iterations, k, gravity, seed),
            )),
            Call::LayoutRandom { k, seed } => {
                xy(layout_random(topo, layout_params(300, k, 0.02, seed)))
            }
            Call::LayoutCircle { k } => xy(layout_circle(topo, layout_params(300, k, 0.02, None))),
            Call::Degree { direction } => {
                one(Arc::new(UInt32Array::from(degree(topo, mask, direction))))
            }
            Call::PageRank {
                damping,
                max_iter,
                tol,
                direction,
            } => {
                let params = PageRankParams {
                    damping,
                    max_iter,
                    tol,
                };
                // `Both` runs the unchanged directed kernel over the symmetric view
                // (each undirected edge as two rows): NetworkX's `pagerank` on an
                // undirected graph.
                let scores = match (direction, weights) {
                    (Direction::Both, Some(w)) => {
                        let (sym, sw) = self.symmetric_weighted(w);
                        pagerank_weighted(sym, sw, None, params)
                    }
                    (Direction::Both, None) => pagerank(self.symmetric(), None, params),
                    (_, Some(w)) => pagerank_weighted(topo, w, mask, params),
                    (_, None) => pagerank(topo, mask, params),
                };
                one(Arc::new(Float64Array::from(scores)))
            }
            Call::ConnectedComponents { strong } => {
                let labels = if strong {
                    connected_components_strong(topo, mask)
                } else {
                    connected_components_weak(topo, mask)
                };
                one(Arc::new(UInt32Array::from(labels)))
            }
            // The triangle-family pair: both read the shared intermediate, so
            // whichever is asked for first pays for the intersection pass.
            Call::TriangleCount => {
                if topo.n_nodes() == 0 {
                    return one(Arc::new(UInt32Array::from(Vec::<u32>::new())));
                }
                let (_, counts) = self.triangles();
                one(Arc::new(UInt32Array::from(counts.clone())))
            }
            Call::ClusteringCoefficient => {
                if topo.n_nodes() == 0 {
                    return one(Arc::new(Float64Array::from(Vec::<f64>::new())));
                }
                let (view, counts) = self.triangles();
                one(Arc::new(Float64Array::from(clustering_from_triangles(
                    view.get(),
                    counts,
                ))))
            }
            Call::Closeness { direction } => {
                let scores = match weights {
                    Some(w) => closeness_weighted_directed(topo, w, mask, direction),
                    None => closeness_directed(topo, mask, direction),
                };
                one(Arc::new(Float64Array::from(scores)))
            }
            Call::Betweenness {
                sample,
                seed,
                direction,
            } => {
                let scores = match (direction, weights) {
                    // `Both`: Brandes over the symmetric view counts every unordered
                    // pair twice (once from each end), so halve, as NetworkX does
                    // for an undirected graph.
                    (Direction::Both, w) => {
                        let bc = match w {
                            Some(w) => {
                                let (sym, sw) = self.symmetric_weighted(w);
                                betweenness_weighted(sym, sw, None, sample, seed)
                            }
                            None => betweenness(self.symmetric(), None, sample, seed),
                        };
                        bc.into_iter().map(|x| x * 0.5).collect()
                    }
                    (_, Some(w)) => betweenness_weighted(topo, w, mask, sample, seed),
                    (_, None) => betweenness(topo, mask, sample, seed),
                };
                one(Arc::new(Float64Array::from(scores)))
            }
            Call::LabelPropagation { max_iter, seed } => one(Arc::new(UInt32Array::from(
                label_propagation(topo, mask, max_iter, seed),
            ))),
            Call::Louvain { resolution, seed } => {
                let labels = match weights {
                    Some(w) => louvain_weighted(topo, w, mask, resolution, seed),
                    None => louvain(topo, mask, resolution, seed),
                };
                one(Arc::new(UInt32Array::from(labels)))
            }
            Call::NeighborAgg { .. } => unreachable!("evaluated without the memo"),
            Call::Density { direction } => one(Arc::new(Float64Array::from(vec![stats::density(
                topo, mask, direction,
            )]))),
            Call::AvgPathLength { sample, direction } => {
                one(Arc::new(Float64Array::from(vec![stats::avg_path_length(
                    topo, mask, sample, direction,
                )])))
            }
            Call::Diameter {
                approximate,
                direction,
            } => one(Arc::new(Int64Array::from(vec![stats::diameter(
                topo,
                mask,
                approximate,
                direction,
            )]))),
            Call::Describe {
                full,
                direction,
                touched,
            } => {
                let d = stats::describe(topo, mask, full, direction, touched);
                vec![
                    Arc::new(Int64Array::from(vec![d.n_nodes])),
                    Arc::new(Int64Array::from(vec![d.n_edges])),
                    Arc::new(Float64Array::from(vec![d.density])),
                    Arc::new(Float64Array::from(vec![d.avg_degree])),
                    Arc::new(Int64Array::from(vec![d.n_components])),
                ]
            }
        }
    }
}

fn layout_params(iterations: u32, k: f64, gravity: f64, seed: Option<u64>) -> LayoutParams {
    LayoutParams {
        iterations,
        k: k as f32,
        gravity: gravity as f32,
        seed,
    }
}

fn xy((x, y): (Vec<f32>, Vec<f32>)) -> Vec<ArrayRef> {
    vec![
        Arc::new(Float32Array::from(x)) as ArrayRef,
        Arc::new(Float32Array::from(y)) as ArrayRef,
    ]
}
