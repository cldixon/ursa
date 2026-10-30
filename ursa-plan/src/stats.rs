//! Whole-graph scalar statistics.
//!
//! These are the spec's deliberate *eager* exceptions to laziness: they build the
//! topology and return a plain number. `density` needs only node and edge counts;
//! `diameter` / `avg_path_length` run BFS from a set of source nodes.
//!
//! Every stat takes a [`Direction`]. `Out` (the default everywhere) reads the rows
//! as stored. `In` follows in-edges; over the whole graph it gives the same value
//! as `Out` (the same pairs, reversed), and differs only for a sampled estimate.
//! `Both` reads the graph as undirected — the simple undirected view the triangle
//! kernels use ([`Topology::undirected`]: one edge per distinct unordered pair,
//! self-loops dropped) — which is what NetworkX reports for `G.to_undirected()`.

use std::sync::Arc;

use rayon::prelude::*;

use arrow::array::{Float64Array, Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::{DataFusionError, Result};
use ursa_core::algo::connected_components_weak;
use ursa_core::{Direction, EdgeMask, Topology};

/// Parse the `direction` argument (`"out"`, `"in"` or `"both"`) of the stat `stat`.
pub fn parse_stat_direction(stat: &str, d: &str) -> Result<Direction> {
    Ok(crate::query::parse_direction(stat, d)?.into())
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
    describe_masked(topology, None, full, dir)
}

/// [`describe`] over a subgraph view (#207): the parent's node set, and only the
/// edge rows `mask` keeps for `n_edges`, `density`, `avg_degree` and
/// `n_components` — what `degree` and the other graph ops read over the same
/// view. `None` is the whole graph.
pub fn describe_masked(
    topology: &Topology,
    mask: Option<&EdgeMask>,
    full: bool,
    dir: Direction,
) -> Result<RecordBatch> {
    let n_nodes = topology.n_nodes() as i64;
    let edges = match mask {
        Some(m) => masked_edge_count(topology, m, dir),
        None => edge_count(topology, dir),
    };
    let n_edges = edges as i64;
    let density = density_from(topology.n_nodes(), edges, dir);
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
        Some(distinct)
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
fn bfs_fold_one(topo: &Topology, s: u32, dir: Direction, sc: &mut ScanScratch) -> (f64, u64, i32) {
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
            topo.for_each_neighbor(u, dir, None, |v| {
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
///
/// Each source's BFS is independent (O(n·m) overall), so they run in parallel
/// (rayon), each rayon worker reusing one [`ScanScratch`]. Per-source partials are
/// collected in source order and then folded sequentially, so the `sum` is reduced
/// in a fixed order — the total is deterministic run to run regardless of thread
/// scheduling.
fn bfs_source_scan(topo: &Topology, sources: &[u32], dir: Direction) -> (f64, u64, i32) {
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
            |sc, &s| bfs_fold_one(topo, s, dir, sc),
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

/// Average shortest-path length over reachable ordered pairs, following edges in
/// `dir` (`Both` ignores direction: the undirected value, which counts each
/// unordered pair from both ends and so has the same mean as NetworkX's
/// `average_shortest_path_length` on the undirected graph). `sample=None` is exact
/// (BFS from every node); `Some(frac)` estimates from a deterministic strided
/// subsample of sources. Disconnected graphs count only their reachable pairs; a
/// graph with no reachable pair returns `0.0`.
pub fn avg_path_length(topology: &Topology, sample: Option<f64>, dir: Direction) -> Result<f64> {
    let sources = sample_sources(topology.n_nodes(), sample);
    let (sum, count, _max) = bfs_source_scan(topology, &sources, dir);
    Ok(if count == 0 { 0.0 } else { sum / count as f64 })
}

/// Graph diameter — the longest shortest path, over reachable ordered pairs,
/// following edges in `dir` (`Both` ignores direction: the undirected diameter).
/// `approximate=false` is exact (max eccentricity over every source);
/// `approximate=true` (the default) is a lower-bound estimate from a bounded
/// deterministic sample of sources. Returns `0` when no pair is reachable.
pub fn diameter(topology: &Topology, approximate: bool, dir: Direction) -> Result<i64> {
    let n = topology.n_nodes();
    // Approximate: sample a bounded number of sources (deterministic stride).
    let sources = if approximate && n > 0 {
        let frac = (128.0 / n as f64).min(1.0);
        sample_sources(n, Some(frac))
    } else {
        sample_sources(n, None)
    };
    let (_sum, _count, max) = bfs_source_scan(topology, &sources, dir);
    Ok(max as i64)
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
            let b = describe_masked(&t, Some(&mask), true, dir).unwrap();
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
            describe_masked(&t, None, true, Direction::Out).unwrap(),
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
                let got = bfs_source_scan(&t, &sources, dir);
                assert_eq!(got.0.to_bits(), sum.to_bits(), "{dir:?}");
                assert_eq!((got.1, got.2), (count, max), "{dir:?}");
            }
        }
    }

    #[test]
    fn stat_direction_parses() {
        assert_eq!(
            parse_stat_direction("density", "both").unwrap(),
            Direction::Both
        );
        assert!(parse_stat_direction("density", "sideways").is_err());
    }
}
