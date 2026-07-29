//! Whole-graph scalar statistics.
//!
//! These are the spec's deliberate *eager* exceptions to laziness: they build the
//! topology and return a plain number. `density` needs only node and edge counts;
//! `diameter` / `avg_path_length` run BFS from a set of source nodes.

use std::collections::HashSet;
use std::sync::Arc;

use rayon::prelude::*;

use arrow::array::{Float64Array, Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::{DataFusionError, Result};
use ursa_core::algo::{bfs_distances, connected_components_weak};
use ursa_core::{Direction, Topology};

/// Directed edge density off an already-built topology: `m / (n * (n - 1))`.
/// Returns `0.0` for fewer than two nodes.
fn density_of(topo: &Topology) -> f64 {
    let n = topo.n_nodes() as f64;
    if n < 2.0 {
        0.0
    } else {
        topo.n_edges() as f64 / (n * (n - 1.0))
    }
}

/// Directed edge density: `m / (n * (n - 1))`, i.e. edges present over edges
/// possible (excluding self-loops). Returns `0.0` for fewer than two nodes.
///
/// Multiplicity is counted as given — duplicate `(src, dst)` rows and self-loops
/// contribute to `m`, so a multigraph can report density > 1. Call `.distinct()`
/// upstream for the simple-graph value.
pub fn density(topology: &Topology) -> Result<f64> {
    Ok(density_of(topology))
}

/// A one-row whole-graph summary: `n_nodes, n_edges, density, avg_degree,
/// n_components`. Per spec §Open questions #4 the expensive `n_components` is
/// gated behind `full`; when `full` is false it is null (the column stays present
/// so the schema is stable). `avg_degree` is the mean out-degree (`m / n`).
pub fn describe(topology: &Topology, full: bool) -> Result<RecordBatch> {
    let n_nodes = topology.n_nodes() as i64;
    let n_edges = topology.n_edges() as i64;
    let density = density_of(topology);
    let avg_degree = if n_nodes == 0 {
        0.0
    } else {
        n_edges as f64 / n_nodes as f64
    };
    let n_components: Option<i64> = if full {
        let distinct: HashSet<u32> = connected_components_weak(topology).into_iter().collect();
        Some(distinct.len() as i64)
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

/// Run BFS from each source and fold the finite distances (`>= 1`, i.e. reachable
/// and not the source itself). Returns `(sum, count, max)` over all reachable
/// ordered pairs from the sampled sources — the shared core of both path stats.
///
/// Each source's BFS is independent (O(n·m) overall), so they run in parallel
/// (rayon). Per-source partials are collected in source order and then folded
/// sequentially, so the `sum` is reduced in a fixed order — the total is
/// deterministic run to run regardless of thread scheduling.
fn bfs_source_scan(topo: &Topology, sources: &[u32], dir: Direction) -> (f64, u64, i32) {
    let partials: Vec<(f64, u64, i32)> = sources
        .par_iter()
        .map(|&s| {
            let mut sum = 0.0f64;
            let mut count = 0u64;
            let mut max = 0i32;
            for d in bfs_distances(topo, s, dir) {
                if d >= 1 {
                    sum += d as f64;
                    count += 1;
                    if d > max {
                        max = d;
                    }
                }
            }
            (sum, count, max)
        })
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

/// Average shortest-path length over reachable ordered pairs (directed, following
/// out-edges). `sample=None` is exact (BFS from every node); `Some(frac)` estimates
/// from a deterministic strided subsample of sources. Disconnected graphs count
/// only their reachable pairs; a graph with no reachable pair returns `0.0`.
pub fn avg_path_length(topology: &Topology, sample: Option<f64>) -> Result<f64> {
    let sources = sample_sources(topology.n_nodes(), sample);
    let (sum, count, _max) = bfs_source_scan(topology, &sources, Direction::Out);
    Ok(if count == 0 { 0.0 } else { sum / count as f64 })
}

/// Graph diameter — the longest shortest path, over reachable ordered pairs
/// (directed, following out-edges). `approximate=false` is exact (max eccentricity
/// over every source); `approximate=true` (the default) is a lower-bound estimate
/// from a bounded deterministic sample of sources. Returns `0` when no pair is
/// reachable.
pub fn diameter(topology: &Topology, approximate: bool) -> Result<i64> {
    let n = topology.n_nodes();
    // Approximate: sample a bounded number of sources (deterministic stride).
    let sources = if approximate && n > 0 {
        let frac = (128.0 / n as f64).min(1.0);
        sample_sources(n, Some(frac))
    } else {
        sample_sources(n, None)
    };
    let (_sum, _count, max) = bfs_source_scan(topology, &sources, Direction::Out);
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
        assert!((density(&topo(&[0, 1, 2], &[1, 2, 0])).unwrap() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn single_node_has_zero_density() {
        assert_eq!(density(&topo(&[0], &[0])).unwrap(), 0.0);
    }

    #[test]
    fn describe_summarizes_a_directed_triangle() {
        use arrow::array::Float64Array;
        let batch = describe(&topo(&[0, 1, 2], &[1, 2, 0]), true).unwrap();
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
        let batch = describe(&topo(&[0, 1, 2], &[1, 2, 0]), false).unwrap();
        let i = batch.schema().index_of("n_components").unwrap();
        // null unless full=true
        assert!(batch.column(i).is_null(0));
    }

    #[test]
    fn avg_path_length_of_a_directed_triangle() {
        // 0->1->2->0: 6 reachable ordered pairs, distances {1,2} x3 -> mean 1.5
        assert!(
            (avg_path_length(&topo(&[0, 1, 2], &[1, 2, 0]), None).unwrap() - 1.5).abs() < 1e-12
        );
    }

    #[test]
    fn diameter_of_a_directed_triangle() {
        let t = topo(&[0, 1, 2], &[1, 2, 0]);
        assert_eq!(diameter(&t, false).unwrap(), 2); // exact
                                                     // approximate is a lower bound on the exact diameter
        assert!(diameter(&t, true).unwrap() <= 2);
    }

    #[test]
    fn path_stats_on_a_line_and_when_disconnected() {
        // line 0->1->2->3: longest shortest path is 0->3 (dist 3)
        assert_eq!(diameter(&topo(&[0, 1, 2], &[1, 2, 3]), false).unwrap(), 3);
        // two disjoint edges: only within-component pairs are reachable
        let t = topo(&[0, 2], &[1, 3]);
        assert!((avg_path_length(&t, None).unwrap() - 1.0).abs() < 1e-12);
        assert_eq!(diameter(&t, false).unwrap(), 1);
    }
}
