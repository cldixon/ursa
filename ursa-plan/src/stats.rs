//! Whole-graph scalar statistics.
//!
//! `density` needs only node and edge counts; `diameter` / `avg_path_length` run
//! BFS from a set of source nodes. Each runs over a subgraph view (#210): the
//! parent's topology and an optional [`EdgeMask`] of the edge rows it keeps,
//! as `describe` and every graph op read it. A plan runs them as a one-row frame
//! ([`stat_batch`], the `ur.density(...)` family).
//!
//! Every stat takes a [`Direction`]. `Out` (the default everywhere) reads the rows
//! as stored. `In` follows in-edges; over the whole graph it gives the same value
//! as `Out` (the same pairs, reversed), and differs only for a sampled estimate.
//! `Both` reads the graph as undirected — the simple undirected view the triangle
//! kernels use ([`Topology::undirected`]: one edge per distinct unordered pair,
//! self-loops dropped) — which is what NetworkX reports for `G.to_undirected()`.

use std::sync::Arc;

use rayon::prelude::*;

use arrow::array::{ArrayRef, Float64Array, Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::{DataFusionError, Result};
use serde::{Deserialize, Serialize};
use ursa_core::algo::connected_components_weak;
use ursa_core::{Direction, EdgeMask, Topology};

/// Parse a stat's `direction` argument (`"out"`, `"in"` or `"both"`).
pub fn parse_stat_direction(d: &str) -> Result<Direction> {
    Ok(crate::query::parse_direction(d)?.into())
}

/// A scalar whole-graph stat, as a plan names it: `ur.density`,
/// `ur.avg_path_length` and `ur.diameter`, each a one-row frame of one column
/// named after the stat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stat", rename_all = "snake_case")]
pub enum GraphStat {
    Density,
    AvgPathLength {
        #[serde(default)]
        sample: Option<f64>,
    },
    Diameter {
        approximate: bool,
    },
}

impl GraphStat {
    /// The stat's name: its function's, and its column's.
    pub fn name(&self) -> &'static str {
        match self {
            GraphStat::Density => "density",
            GraphStat::AvgPathLength { .. } => "avg_path_length",
            GraphStat::Diameter { .. } => "diameter",
        }
    }

    /// The type of the stat's column.
    pub fn data_type(&self) -> DataType {
        match self {
            GraphStat::Diameter { .. } => DataType::Int64,
            _ => DataType::Float64,
        }
    }
}

/// Parse `describe(nodes=)`: whether it describes only the touched nodes.
/// `shown` is the argument as the caller wrote it, for the error; `None` for
/// `nodes` is an argument that is not a string.
pub fn parse_describe_nodes(nodes: Option<&str>, shown: &str) -> crate::PlanResult<bool> {
    match nodes {
        Some("all") => Ok(false),
        Some("touched") => Ok(true),
        _ => Err(crate::PlanError::Value(format!(
            "describe(nodes={shown}) is not supported; use 'all' or 'touched'."
        ))),
    }
}

/// The nodes at an end of an edge row `mask` keeps, in node order.
fn touched_nodes(topo: &Topology, mask: &EdgeMask) -> Vec<u32> {
    let n = topo.n_nodes();
    let mut seen = vec![false; n];
    for u in 0..n as u32 {
        topo.for_each_neighbor(u, Direction::Out, Some(mask), |v| {
            seen[u as usize] = true;
            seen[v as usize] = true;
        });
    }
    (0..n as u32).filter(|&u| seen[u as usize]).collect()
}

/// The edge count a stat reads under `dir`: the stored row count for `Out`/`In`,
/// or the number of undirected edges (distinct unordered pairs, self-loops
/// dropped) for `Both`.
fn edge_count(topo: &Topology, dir: Direction) -> u64 {
    match dir {
        Direction::Both => topo.undirected().targets.len() as u64 / 2,
        _ => topo.n_edges() as u64,
    }
}

/// [`edge_count`] over the subgraph view `mask` keeps (#207): the kept rows for
/// `Out`/`In`, or the distinct unordered pairs among them for `Both`.
fn masked_edge_count(topo: &Topology, mask: &EdgeMask, dir: Direction) -> u64 {
    match dir {
        Direction::Both => topo.undirected_masked(mask).targets.len() as u64 / 2,
        _ => mask.n_kept() as u64,
    }
}

/// Edge density off an already-built topology. Directed (`Out`/`In`):
/// `m / (n * (n - 1))`. Undirected (`Both`): `m_u / (n * (n - 1) / 2)`. Returns
/// `0.0` for fewer than two nodes.
fn density_of(topo: &Topology, dir: Direction) -> f64 {
    density_from(topo.n_nodes(), edge_count(topo, dir), dir)
}

/// Density from a node and an edge count (as [`density_of`] reads them).
fn density_from(n_nodes: usize, n_edges: u64, dir: Direction) -> f64 {
    let n = n_nodes as f64;
    if n < 2.0 {
        return 0.0;
    }
    let m = n_edges as f64;
    match dir {
        Direction::Both => 2.0 * m / (n * (n - 1.0)),
        _ => m / (n * (n - 1.0)),
    }
}

/// Edge density. Directed (`Out`, and `In`, which is identical): `m / (n * (n - 1))`,
/// i.e. edges present over edges possible (excluding self-loops). Multiplicity is
/// counted as given — duplicate `(src, dst)` rows and self-loops contribute to
/// `m`, so a multigraph can report density > 1. Call `.distinct()` upstream for
/// the simple-graph value.
///
/// Undirected (`Both`): `m_u / (n * (n - 1) / 2)` over the distinct unordered pairs
/// joined by any row, with self-loops dropped — `nx.density(G.to_undirected())`
/// for a graph without self-loops. Always in `[0, 1]`.
///
/// Returns `0.0` for fewer than two nodes.
pub fn density(topology: &Topology, dir: Direction) -> Result<f64> {
    Ok(density_of(topology, dir))
}

/// A one-row whole-graph summary: `n_nodes, n_edges, density, avg_degree,
/// n_components`. Per spec §Open questions #4 the expensive `n_components` is
/// gated behind `full`; when `full` is false it is null (the column stays present
/// so the schema is stable).
///
/// Directed (`Out`/`In`): `n_edges` is the row count and `avg_degree` the mean
/// out-degree (`m / n`). Undirected (`Both`): `n_edges` is the undirected edge
/// count (distinct unordered pairs, self-loops dropped), `density` the undirected
/// density and `avg_degree` the mean undirected degree (`2 * m_u / n`).
/// `n_components` is always the weak (undirected) count.
pub fn describe(topology: &Topology, full: bool, dir: Direction) -> Result<RecordBatch> {
    describe_masked(topology, None, full, dir, false)
}

/// [`describe`] over a subgraph view (#207): the parent's node set, and only the
/// edge rows `mask` keeps for `n_edges`, `density`, `avg_degree` and
/// `n_components` — what `degree` and the other graph ops read over the same
/// view. `None` is the whole graph.
///
/// With `touched` (`describe(nodes="touched")`, #213) the node set is only the
/// nodes at an end of a kept edge: `n_nodes` counts them, `density` and
/// `avg_degree` divide by that count, and `n_components` leaves out the parent's
/// other nodes (each its own component in the view). Over the whole graph every
/// node is at an end of an edge, so it changes nothing there.
pub fn describe_masked(
    topology: &Topology,
    mask: Option<&EdgeMask>,
    full: bool,
    dir: Direction,
    touched: bool,
) -> Result<RecordBatch> {
    let all_nodes = topology.n_nodes();
    let n = match mask {
        Some(m) if touched => touched_nodes(topology, m).len(),
        _ => all_nodes,
    };
    let n_nodes = n as i64;
    let edges = match mask {
        Some(m) => masked_edge_count(topology, m, dir),
        None => edge_count(topology, dir),
    };
    let n_edges = edges as i64;
    let density = density_from(n, edges, dir);
    let endpoints_per_edge = if dir == Direction::Both { 2.0 } else { 1.0 };
    let avg_degree = if n_nodes == 0 {
        0.0
    } else {
        endpoints_per_edge * n_edges as f64 / n_nodes as f64
    };
    let n_components: Option<i64> = if full {
        // Labels are dense node ids, so a seen-bitmap counts them without hashing.
        let labels = connected_components_weak(topology, mask);
        let mut seen = vec![false; labels.len()];
        let mut distinct = 0i64;
        for &l in &labels {
            if !std::mem::replace(&mut seen[l as usize], true) {
                distinct += 1;
            }
        }
        // The untouched nodes are each a component of their own in the view.
        Some(distinct - (all_nodes - n) as i64)
    } else {
        None
    };

    let schema = Arc::new(Schema::new(vec![
        Field::new("n_nodes", DataType::Int64, false),
        Field::new("n_edges", DataType::Int64, false),
        Field::new("density", DataType::Float64, false),
        Field::new("avg_degree", DataType::Float64, false),
        Field::new("n_components", DataType::Int64, true),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![n_nodes])),
            Arc::new(Int64Array::from(vec![n_edges])),
            Arc::new(Float64Array::from(vec![density])),
            Arc::new(Float64Array::from(vec![avg_degree])),
            Arc::new(Int64Array::from(vec![n_components])),
        ],
    )
    .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}

/// Pick the source nodes for a BFS-based stat. `None` → every node (exact);
/// `Some(frac)` → a deterministic evenly-strided subsample of ~`frac * n` nodes
/// (reproducible, no RNG). At least one source when `n > 0`.
fn sample_sources(n: usize, frac: Option<f64>) -> Vec<u32> {
    match frac {
        None => (0..n as u32).collect(),
        Some(f) => {
            let f = f.clamp(0.0, 1.0);
            let k = ((n as f64) * f).round().max(1.0) as usize;
            let k = k.min(n);
            if k == 0 {
                return Vec::new();
            }
            // Evenly strided across 0..n so the sample isn't a biased prefix.
            let step = (n as f64 / k as f64).max(1.0);
            (0..k)
                .map(|i| ((i as f64 * step).floor() as usize).min(n - 1) as u32)
                .collect()
        }
    }
}

/// Reusable per-source BFS scratch for [`bfs_source_scan`]. `touched` lists the
/// nodes reached from the current source, so the fold and the reset cost
/// `O(reached)` rather than allocating and scanning a fresh `n`-length distance
/// array per source.
struct ScanScratch {
    dist: Vec<i32>,
    frontier: Vec<u32>,
    next: Vec<u32>,
    touched: Vec<u32>,
}

/// One source's `(sum, count, max)` over the finite distances `>= 1`, on reusable
/// scratch (clean on entry, reset to clean on exit). The distances are integers,
/// so the `f64` sum is exact and summing the reached set in discovery order
/// matches the original `0..n` scan.
fn bfs_fold_one(
    topo: &Topology,
    s: u32,
    dir: Direction,
    mask: Option<&EdgeMask>,
    sc: &mut ScanScratch,
) -> (f64, u64, i32) {
    let ScanScratch {
        dist,
        frontier,
        next,
        touched,
    } = sc;
    dist[s as usize] = 0;
    frontier.push(s);
    let mut level = 1i32;
    let mut sum = 0.0f64;
    let mut count = 0u64;
    let mut max = 0i32;
    while !frontier.is_empty() {
        next.clear();
        for &u in frontier.iter() {
            topo.for_each_neighbor(u, dir, mask, |v| {
                if dist[v as usize] < 0 {
                    dist[v as usize] = level;
                    next.push(v);
                }
            });
        }
        if !next.is_empty() {
            sum += level as f64 * next.len() as f64;
            count += next.len() as u64;
            max = level;
        }
        touched.extend_from_slice(next);
        level += 1;
        std::mem::swap(frontier, next);
    }
    dist[s as usize] = -1;
    for &v in touched.iter() {
        dist[v as usize] = -1;
    }
    touched.clear();
    frontier.clear();
    next.clear();
    (sum, count, max)
}

/// Run BFS from each source and fold the finite distances (`>= 1`, i.e. reachable
/// and not the source itself). Returns `(sum, count, max)` over all reachable
/// ordered pairs from the sampled sources — the shared core of both path stats.
/// With a `mask`, the BFS follows only the edge rows it keeps.
///
/// Each source's BFS is independent (O(n·m) overall), so they run in parallel
/// (rayon), each rayon worker reusing one [`ScanScratch`]. Per-source partials are
/// collected in source order and then folded sequentially, so the `sum` is reduced
/// in a fixed order — the total is deterministic run to run regardless of thread
/// scheduling.
fn bfs_source_scan(
    topo: &Topology,
    sources: &[u32],
    dir: Direction,
    mask: Option<&EdgeMask>,
) -> (f64, u64, i32) {
    if dir != Direction::Out {
        // Build the cached transpose once, before the parallel scan requests it.
        topo.incoming();
    }
    let n = topo.n_nodes();
    let partials: Vec<(f64, u64, i32)> = sources
        .par_iter()
        .map_init(
            || ScanScratch {
                dist: vec![-1i32; n],
                frontier: Vec::new(),
                next: Vec::new(),
                touched: Vec::new(),
            },
            |sc, &s| bfs_fold_one(topo, s, dir, mask, sc),
        )
        .collect();

    let mut sum = 0.0f64;
    let mut count = 0u64;
    let mut max = 0i32;
    for (s, c, m) in partials {
        sum += s;
        count += c;
        if m > max {
            max = m;
        }
    }
    (sum, count, max)
}

/// The BFS sources a path stat may pick from: every node over the whole graph,
/// or over a view only the nodes at an end of a kept edge (the others reach
/// nothing and are reached by nothing).
fn candidate_sources(topo: &Topology, mask: Option<&EdgeMask>) -> Option<Vec<u32>> {
    mask.map(|m| touched_nodes(topo, m))
}

/// [`sample_sources`] over `candidates` (`None`: every node of `topo`).
fn pick_sources(topo: &Topology, candidates: Option<&[u32]>, frac: Option<f64>) -> Vec<u32> {
    match candidates {
        None => sample_sources(topo.n_nodes(), frac),
        Some(c) => sample_sources(c.len(), frac)
            .into_iter()
            .map(|i| c[i as usize])
            .collect(),
    }
}

/// Average shortest-path length over reachable ordered pairs, following edges in
/// `dir` (`Both` ignores direction: the undirected value, which counts each
/// unordered pair from both ends and so has the same mean as NetworkX's
/// `average_shortest_path_length` on the undirected graph). `sample=None` is exact
/// (BFS from every node); `Some(frac)` estimates from a deterministic strided
/// subsample of sources. Disconnected graphs count only their reachable pairs; a
/// graph with no reachable pair returns `0.0`.
pub fn avg_path_length(topology: &Topology, sample: Option<f64>, dir: Direction) -> Result<f64> {
    avg_path_length_masked(topology, None, sample, dir)
}

/// [`avg_path_length`] over a subgraph view (#210): only the edge rows `mask`
/// keeps are followed, and a sample draws its sources from the nodes at an end
/// of a kept edge.
pub fn avg_path_length_masked(
    topology: &Topology,
    mask: Option<&EdgeMask>,
    sample: Option<f64>,
    dir: Direction,
) -> Result<f64> {
    let candidates = candidate_sources(topology, mask);
    let sources = pick_sources(topology, candidates.as_deref(), sample);
    let (sum, count, _max) = bfs_source_scan(topology, &sources, dir, mask);
    Ok(if count == 0 { 0.0 } else { sum / count as f64 })
}

/// Graph diameter — the longest shortest path, over reachable ordered pairs,
/// following edges in `dir` (`Both` ignores direction: the undirected diameter).
/// `approximate=false` is exact (max eccentricity over every source);
/// `approximate=true` (the default) is a lower-bound estimate from a bounded
/// deterministic sample of sources. Returns `0` when no pair is reachable.
pub fn diameter(topology: &Topology, approximate: bool, dir: Direction) -> Result<i64> {
    diameter_masked(topology, None, approximate, dir)
}

/// [`diameter`] over a subgraph view (#210): only the edge rows `mask` keeps
/// are followed, and the approximate estimate samples its sources from the
/// nodes at an end of a kept edge.
pub fn diameter_masked(
    topology: &Topology,
    mask: Option<&EdgeMask>,
    approximate: bool,
    dir: Direction,
) -> Result<i64> {
    let candidates = candidate_sources(topology, mask);
    let n = candidates.as_ref().map_or(topology.n_nodes(), Vec::len);
    // Approximate: sample a bounded number of sources (deterministic stride).
    let frac = (approximate && n > 0).then(|| (128.0 / n as f64).min(1.0));
    let sources = pick_sources(topology, candidates.as_deref(), frac);
    let (_sum, _count, max) = bfs_source_scan(topology, &sources, dir, mask);
    Ok(max as i64)
}

/// [`density`] over a subgraph view (#210): the parent's node count, and the
/// edge rows `mask` keeps, as [`describe_masked`] reports it.
pub fn density_masked(topology: &Topology, mask: Option<&EdgeMask>, dir: Direction) -> f64 {
    let edges = match mask {
        Some(m) => masked_edge_count(topology, m, dir),
        None => edge_count(topology, dir),
    };
    density_from(topology.n_nodes(), edges, dir)
}

/// A scalar stat over a subgraph view, as its one-row frame: one non-null
/// column named after the stat.
pub fn stat_batch(
    topology: &Topology,
    mask: Option<&EdgeMask>,
    stat: &GraphStat,
    dir: Direction,
) -> Result<RecordBatch> {
    let value: ArrayRef = match stat {
        GraphStat::Density => Arc::new(Float64Array::from(vec![density_masked(
            topology, mask, dir,
        )])),
        GraphStat::AvgPathLength { sample } => {
            Arc::new(Float64Array::from(vec![avg_path_length_masked(
                topology, mask, *sample, dir,
            )?]))
        }
        GraphStat::Diameter { approximate } => Arc::new(Int64Array::from(vec![diameter_masked(
            topology,
            mask,
            *approximate,
            dir,
        )?])),
    };
    let schema = Arc::new(Schema::new(vec![Field::new(
        stat.name(),
        stat.data_type(),
        false,
    )]));
    RecordBatch::try_new(schema, vec![value])
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::build_topology;

    /// Build a topology from edge endpoints for the tests.
    fn topo(src: &[i64], dst: &[i64]) -> std::sync::Arc<Topology> {
        let (t, _ids) = build_topology(
            &Int64Array::from(src.to_vec()),
            &Int64Array::from(dst.to_vec()),
        )
        .unwrap();
        t
    }

    #[test]
    fn density_of_a_directed_triangle() {
        // 3 nodes, 3 directed edges; possible = 3*2 = 6 -> 0.5
        assert!(
            (density(&topo(&[0, 1, 2], &[1, 2, 0]), Direction::Out).unwrap() - 0.5).abs() < 1e-12
        );
    }

    #[test]
    fn single_node_has_zero_density() {
        assert_eq!(density(&topo(&[0], &[0]), Direction::Out).unwrap(), 0.0);
    }

    #[test]
    fn describe_summarizes_a_directed_triangle() {
        use arrow::array::Float64Array;
        let batch = describe(&topo(&[0, 1, 2], &[1, 2, 0]), true, Direction::Out).unwrap();
        assert_eq!(batch.num_rows(), 1);
        let get_i64 = |name: &str| {
            let i = batch.schema().index_of(name).unwrap();
            batch
                .column(i)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(0)
        };
        let get_f64 = |name: &str| {
            let i = batch.schema().index_of(name).unwrap();
            batch
                .column(i)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .value(0)
        };
        assert_eq!(get_i64("n_nodes"), 3);
        assert_eq!(get_i64("n_edges"), 3);
        assert!((get_f64("density") - 0.5).abs() < 1e-12);
        assert!((get_f64("avg_degree") - 1.0).abs() < 1e-12);
        assert_eq!(get_i64("n_components"), 1);
    }

    #[test]
    fn describe_gates_n_components_behind_full() {
        let batch = describe(&topo(&[0, 1, 2], &[1, 2, 0]), false, Direction::Out).unwrap();
        let i = batch.schema().index_of("n_components").unwrap();
        // null unless full=true
        assert!(batch.column(i).is_null(0));
    }

    #[test]
    fn describe_masked_reads_only_the_kept_edges() {
        use arrow::array::Float64Array;
        // #207: 0->1, 0->2, 1->2, 2->3 with 0->2 and 1->2 kept. The view keeps
        // all 4 nodes (3 isolated) and 2 edges.
        let t = topo(&[0, 0, 1, 2], &[1, 2, 2, 3]);
        let mask = EdgeMask::from_bools(&[false, true, true, false]);
        let row = |dir| {
            let b = describe_masked(&t, Some(&mask), true, dir, false).unwrap();
            let col = |name: &str| b.column(b.schema().index_of(name).unwrap()).clone();
            let i = |name: &str| {
                col(name)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .value(0)
            };
            let f = |name: &str| {
                col(name)
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .unwrap()
                    .value(0)
            };
            (
                i("n_nodes"),
                i("n_edges"),
                f("density"),
                f("avg_degree"),
                i("n_components"),
            )
        };
        assert_eq!(row(Direction::Out), (4, 2, 2.0 / 12.0, 0.5, 2));
        assert_eq!(row(Direction::Both), (4, 2, 2.0 / 6.0, 1.0, 2));
        // No mask is the whole graph, as `describe` reports it.
        assert_eq!(
            describe_masked(&t, None, true, Direction::Out, false).unwrap(),
            describe(&t, true, Direction::Out).unwrap()
        );
    }

    #[test]
    fn avg_path_length_of_a_directed_triangle() {
        // 0->1->2->0: 6 reachable ordered pairs, distances {1,2} x3 -> mean 1.5
        assert!(
            (avg_path_length(&topo(&[0, 1, 2], &[1, 2, 0]), None, Direction::Out).unwrap() - 1.5)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn diameter_of_a_directed_triangle() {
        let t = topo(&[0, 1, 2], &[1, 2, 0]);
        assert_eq!(diameter(&t, false, Direction::Out).unwrap(), 2); // exact
                                                                     // approximate is a lower bound on the exact diameter
        assert!(diameter(&t, true, Direction::Out).unwrap() <= 2);
    }

    #[test]
    fn path_stats_on_a_line_and_when_disconnected() {
        // line 0->1->2->3: longest shortest path is 0->3 (dist 3)
        assert_eq!(
            diameter(&topo(&[0, 1, 2], &[1, 2, 3]), false, Direction::Out).unwrap(),
            3
        );
        // two disjoint edges: only within-component pairs are reachable
        let t = topo(&[0, 2], &[1, 3]);
        assert!((avg_path_length(&t, None, Direction::Out).unwrap() - 1.0).abs() < 1e-12);
        assert_eq!(diameter(&t, false, Direction::Out).unwrap(), 1);
    }

    #[test]
    fn undirected_stats_read_the_simple_undirected_view() {
        use arrow::array::Float64Array;
        // A path 0-1-2-3 entered with a reciprocal pair (0->1, 1->0), a parallel
        // edge (1->2 twice) and a self-loop (3->3): undirected it is 3 edges.
        let t = topo(&[0, 1, 1, 1, 2, 3], &[1, 0, 2, 2, 3, 3]);
        // 3 undirected edges over 4*3/2 = 6 pairs.
        assert!((density(&t, Direction::Both).unwrap() - 0.5).abs() < 1e-12);
        // directed is unchanged: 6 rows over 12 ordered pairs
        assert!((density(&t, Direction::Out).unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(
            density(&t, Direction::In).unwrap(),
            density(&t, Direction::Out).unwrap()
        );
        assert_eq!(diameter(&t, false, Direction::Both).unwrap(), 3);
        // path distances: 1,2,3,1,2,1 over 6 unordered pairs -> 10/6
        assert!((avg_path_length(&t, None, Direction::Both).unwrap() - 10.0 / 6.0).abs() < 1e-12);
        let b = describe(&t, true, Direction::Both).unwrap();
        let col = |name: &str| b.column(b.schema().index_of(name).unwrap()).clone();
        let i = |name: &str| {
            col(name)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(0)
        };
        let f = |name: &str| {
            col(name)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .value(0)
        };
        assert_eq!(i("n_nodes"), 4);
        assert_eq!(i("n_edges"), 3);
        assert!((f("density") - 0.5).abs() < 1e-12);
        assert!((f("avg_degree") - 1.5).abs() < 1e-12);
        assert_eq!(i("n_components"), 1);
    }

    #[test]
    fn in_direction_matches_out_over_the_whole_graph() {
        let t = topo(&[0, 1, 2, 2], &[1, 2, 3, 0]);
        assert_eq!(
            diameter(&t, false, Direction::In).unwrap(),
            diameter(&t, false, Direction::Out).unwrap()
        );
        assert!(
            (avg_path_length(&t, None, Direction::In).unwrap()
                - avg_path_length(&t, None, Direction::Out).unwrap())
            .abs()
                < 1e-12
        );
    }

    #[test]
    fn path_scan_matches_a_full_distance_fold() {
        // The scratch-reusing scan must fold to exactly what a fresh `bfs_distances`
        // per source, scanned over all `n`, gives — sparse and dense, every direction.
        use ursa_core::algo::bfs_distances;
        for (n, m) in [(3000i64, 2500usize), (300, 3000)] {
            let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut next = move || {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as i64
            };
            let (mut src, mut dst) = (Vec::new(), Vec::new());
            for _ in 0..m {
                src.push(next() % n);
                dst.push(next() % n);
            }
            let t = topo(&src, &dst);
            let sources: Vec<u32> = (0..t.n_nodes() as u32).collect();
            for dir in [Direction::Out, Direction::In, Direction::Both] {
                let (mut sum, mut count, mut max) = (0.0f64, 0u64, 0i32);
                for &s in &sources {
                    let mut ps = 0.0f64;
                    for d in bfs_distances(&t, s, dir) {
                        if d >= 1 {
                            ps += d as f64;
                            count += 1;
                            max = max.max(d);
                        }
                    }
                    sum += ps;
                }
                let got = bfs_source_scan(&t, &sources, dir, None);
                assert_eq!(got.0.to_bits(), sum.to_bits(), "{dir:?}");
                assert_eq!((got.1, got.2), (count, max), "{dir:?}");
            }
        }
    }

    #[test]
    fn masked_stats_read_only_the_kept_edges() {
        // #210: 0->1, 0->2, 1->2, 2->3 with 0->2 and 1->2 kept.
        let t = topo(&[0, 0, 1, 2], &[1, 2, 2, 3]);
        let mask = EdgeMask::from_bools(&[false, true, true, false]);
        let m = Some(&mask);
        assert_eq!(density_masked(&t, m, Direction::Out), 2.0 / 12.0);
        assert_eq!(diameter_masked(&t, m, false, Direction::Out).unwrap(), 1);
        assert_eq!(diameter_masked(&t, m, false, Direction::Both).unwrap(), 2);
        assert_eq!(
            avg_path_length_masked(&t, m, None, Direction::Out).unwrap(),
            1.0
        );
        // No mask is the whole graph, bit for bit.
        for dir in [Direction::Out, Direction::In, Direction::Both] {
            assert_eq!(
                density_masked(&t, None, dir).to_bits(),
                density(&t, dir).unwrap().to_bits()
            );
            for frac in [None, Some(0.5)] {
                assert_eq!(
                    avg_path_length_masked(&t, None, frac, dir)
                        .unwrap()
                        .to_bits(),
                    avg_path_length(&t, frac, dir).unwrap().to_bits()
                );
            }
            for approximate in [false, true] {
                assert_eq!(
                    diameter_masked(&t, None, approximate, dir).unwrap(),
                    diameter(&t, approximate, dir).unwrap()
                );
            }
        }
    }

    #[test]
    fn a_masked_estimate_samples_the_touched_nodes() {
        // A 1,000-node path with a 2-edge view far from node 0: the strided
        // sample over all nodes would miss it.
        let n = 1000i64;
        let src: Vec<i64> = (0..n - 1).collect();
        let dst: Vec<i64> = (1..n).collect();
        let t = topo(&src, &dst);
        let keep: Vec<bool> = (0..n - 1).map(|i| i == 700 || i == 701).collect();
        let mask = EdgeMask::from_bools(&keep);
        assert_eq!(touched_nodes(&t, &mask), vec![700, 701, 702]);
        assert_eq!(
            diameter_masked(&t, Some(&mask), true, Direction::Out).unwrap(),
            2
        );
    }

    #[test]
    fn describe_touched_counts_only_the_nodes_of_kept_edges() {
        use arrow::array::Float64Array;
        let t = topo(&[0, 0, 1, 2], &[1, 2, 2, 3]);
        let mask = EdgeMask::from_bools(&[false, true, true, false]);
        let b = describe_masked(&t, Some(&mask), true, Direction::Out, true).unwrap();
        let col = |name: &str| b.column(b.schema().index_of(name).unwrap()).clone();
        let i = |name: &str| {
            col(name)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(0)
        };
        let f = |name: &str| {
            col(name)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .value(0)
        };
        assert_eq!((i("n_nodes"), i("n_edges"), i("n_components")), (3, 2, 1));
        assert_eq!((f("density"), f("avg_degree")), (2.0 / 6.0, 2.0 / 3.0));
        // Over the whole graph every node is touched.
        assert_eq!(
            describe_masked(&t, None, true, Direction::Out, true).unwrap(),
            describe(&t, true, Direction::Out).unwrap()
        );
    }

    #[test]
    fn stat_batch_is_one_non_null_column() {
        let t = topo(&[0, 1, 2], &[1, 2, 0]);
        for (stat, ty) in [
            (GraphStat::Density, DataType::Float64),
            (GraphStat::AvgPathLength { sample: None }, DataType::Float64),
            (GraphStat::Diameter { approximate: false }, DataType::Int64),
        ] {
            let b = stat_batch(&t, None, &stat, Direction::Out).unwrap();
            assert_eq!(b.num_rows(), 1);
            let field = b.schema().field(0).clone();
            assert_eq!(
                (field.name().as_str(), field.data_type()),
                (stat.name(), &ty)
            );
            assert!(!field.is_nullable());
        }
    }

    #[test]
    fn describe_nodes_parses() {
        assert!(!parse_describe_nodes(Some("all"), "'all'").unwrap());
        assert!(parse_describe_nodes(Some("touched"), "'touched'").unwrap());
        let e = parse_describe_nodes(None, "1").unwrap_err();
        assert!(e.to_string().contains("describe(nodes=1) is not supported"));
    }

    #[test]
    fn stat_direction_parses() {
        assert_eq!(parse_stat_direction("both").unwrap(), Direction::Both);
        assert!(parse_stat_direction("sideways").is_err());
    }
}
