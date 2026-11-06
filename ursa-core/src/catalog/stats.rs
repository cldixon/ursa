//! Whole-graph statistics: `density`, `avg_path_length`, `diameter` and
//! `describe`.
//!
//! `density` needs only node and edge counts; `diameter` and `avg_path_length`
//! run BFS from a set of source nodes. Each runs over a subgraph view: the
//! parent's topology and an optional [`EdgeMask`] of the edge rows it keeps.
//!
//! Every statistic takes a [`Direction`]. `Out` reads the rows as stored. `In`
//! follows in-edges; over the whole graph it gives the same value as `Out` (the
//! same pairs, reversed), and differs only for a sampled estimate. `Both` reads
//! the graph as undirected — the simple undirected view the triangle kernels use
//! ([`Topology::undirected`]: one edge per distinct unordered pair, self-loops
//! dropped) — which is what NetworkX reports for `G.to_undirected()`.

use crate::algo::connected_components_weak;
use crate::parallel::*;
use crate::topology::{Direction, EdgeMask, Topology};

/// `describe`'s one row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Description {
    pub n_nodes: i64,
    pub n_edges: i64,
    pub density: f64,
    pub avg_degree: f64,
    /// The weak component count; `None` unless `full`.
    pub n_components: Option<i64>,
}

/// The nodes at an end of an edge row `mask` keeps, in node order.
pub fn touched_nodes(topo: &Topology, mask: &EdgeMask) -> Vec<u32> {
    let seen = on_an_edge(topo, Some(mask));
    (0..topo.n_nodes() as u32)
        .filter(|&u| seen[u as usize])
        .collect()
}

/// Per node, whether it is at an end of an edge row `mask` keeps (`None`:
/// any edge row).
fn on_an_edge(topo: &Topology, mask: Option<&EdgeMask>) -> Vec<bool> {
    let n = topo.n_nodes();
    let mut seen = vec![false; n];
    match mask {
        Some(mask) => {
            for u in 0..n as u32 {
                topo.for_each_neighbor(u, Direction::Out, Some(mask), |v| {
                    seen[u as usize] = true;
                    seen[v as usize] = true;
                });
            }
        }
        None => {
            let out = topo.out();
            for u in 0..n {
                if out.offsets[u + 1] > out.offsets[u] {
                    seen[u] = true;
                }
            }
            for &v in out.targets.iter() {
                seen[v as usize] = true;
            }
        }
    }
    seen
}

/// The nodes at an end of an edge row `mask` keeps (`None`: any edge row), in
/// node order; `None` when that is every node. A graph whose nodes are given
/// apart from its edges (the SQL engine's vertex table) can have nodes on no
/// edge with no mask at all.
fn touched(topo: &Topology, mask: Option<&EdgeMask>) -> Option<Vec<u32>> {
    let seen = on_an_edge(topo, mask);
    if mask.is_none() && seen.iter().all(|&s| s) {
        return None;
    }
    Some(
        (0..topo.n_nodes() as u32)
            .filter(|&u| seen[u as usize])
            .collect(),
    )
}

/// The edge count a statistic reads under `dir` over the view `mask` keeps:
/// the (kept) row count for `Out`/`In`, or the number of distinct unordered
/// pairs among them, self-loops dropped, for `Both`.
fn edge_count(topo: &Topology, mask: Option<&EdgeMask>, dir: Direction) -> u64 {
    match (mask, dir) {
        (None, Direction::Both) => topo.undirected().targets.len() as u64 / 2,
        (None, _) => topo.n_edges() as u64,
        (Some(m), Direction::Both) => topo.undirected_masked(m).targets.len() as u64 / 2,
        (Some(m), _) => m.n_kept() as u64,
    }
}

/// Density from a node and an edge count. Directed (`Out`/`In`):
/// `m / (n * (n - 1))`. Undirected (`Both`): `m_u / (n * (n - 1) / 2)`. `0.0`
/// for fewer than two nodes.
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

/// Edge density over the parent's node set and the edge rows `mask` keeps.
///
/// Directed (`Out`, and `In`, which is identical): `m / (n * (n - 1))`, edges
/// present over edges possible (excluding self-loops). Multiplicity is counted
/// as given — duplicate rows and self-loops contribute to `m`, so a multigraph
/// can report density > 1.
///
/// Undirected (`Both`): `m_u / (n * (n - 1) / 2)` over the distinct unordered
/// pairs joined by any row, self-loops dropped: `nx.density(G.to_undirected())`
/// for a graph without self-loops. Always in `[0, 1]`.
pub fn density(topo: &Topology, mask: Option<&EdgeMask>, dir: Direction) -> f64 {
    density_from(topo.n_nodes(), edge_count(topo, mask, dir), dir)
}

/// A one-row summary over the view `mask` keeps (`None`: the whole graph).
///
/// Directed (`Out`/`In`): `n_edges` is the row count and `avg_degree` the mean
/// out-degree (`m / n`). Undirected (`Both`): `n_edges` is the undirected edge
/// count, `density` the undirected density and `avg_degree` the mean undirected
/// degree (`2 * m_u / n`). `n_components` is the weak count, and only computed
/// with `full`.
///
/// With `touched` the node set is only the nodes at an end of a kept edge:
/// `n_nodes` counts them, `density` and `avg_degree` divide by that count, and
/// `n_components` leaves out the parent's other nodes. Over a whole graph of
/// edges it changes nothing; over a graph with nodes on no edge (a vertex
/// table's), it leaves those out.
pub fn describe(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    full: bool,
    dir: Direction,
    touched: bool,
) -> Description {
    let all_nodes = topo.n_nodes();
    let n = match touched {
        true => self::touched(topo, mask).map_or(all_nodes, |t| t.len()),
        false => all_nodes,
    };
    let n_nodes = n as i64;
    let edges = edge_count(topo, mask, dir);
    let n_edges = edges as i64;
    let density = density_from(n, edges, dir);
    let endpoints_per_edge = if dir == Direction::Both { 2.0 } else { 1.0 };
    let avg_degree = if n_nodes == 0 {
        0.0
    } else {
        endpoints_per_edge * n_edges as f64 / n_nodes as f64
    };
    let n_components = full.then(|| {
        // Labels are dense node ids, so a seen-bitmap counts them without hashing.
        let labels = connected_components_weak(topo, mask);
        let mut seen = vec![false; labels.len()];
        let mut distinct = 0i64;
        for &l in &labels {
            if !std::mem::replace(&mut seen[l as usize], true) {
                distinct += 1;
            }
        }
        // The untouched nodes are each a component of their own in the view.
        distinct - (all_nodes - n) as i64
    });
    Description {
        n_nodes,
        n_edges,
        density,
        avg_degree,
        n_components,
    }
}

/// Pick the source nodes for a BFS-based statistic. `None`: every node (exact);
/// `Some(frac)`: a deterministic evenly-strided subsample of about `frac * n`
/// nodes (no RNG). At least one source when `n > 0`.
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
/// `O(reached)` rather than a fresh `n`-length distance array per source.
struct ScanScratch {
    dist: Vec<i32>,
    frontier: Vec<u32>,
    next: Vec<u32>,
    touched: Vec<u32>,
}

/// One source's `(sum, count, max)` over the finite distances `>= 1`, on
/// reusable scratch (clean on entry, reset to clean on exit). The distances are
/// integers, so the `f64` sum is exact.
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

/// Run BFS from each source and fold the finite distances `>= 1`. Returns
/// `(sum, count, max)` over all reachable ordered pairs from the sources.
///
/// The sources run in parallel, each worker reusing one [`ScanScratch`]; the
/// per-source partials are collected in source order and folded sequentially,
/// so the sum is reduced in a fixed order whatever the thread count.
pub(crate) fn bfs_source_scan(
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

/// The BFS sources a path statistic may pick from: the nodes at an end of a
/// kept edge (the others reach nothing and are reached by nothing); `None`
/// when that is every node, as over a whole graph of edges.
fn candidate_sources(topo: &Topology, mask: Option<&EdgeMask>) -> Option<Vec<u32>> {
    touched(topo, mask)
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

/// Average shortest-path length over reachable ordered pairs, following edges
/// in `dir` (`Both` ignores direction). `sample=None` is exact (BFS from every
/// node); `Some(frac)` estimates from a deterministic strided subsample of
/// sources, drawn from the nodes at an end of a kept edge. A graph
/// with no reachable pair gives `0.0`.
pub fn avg_path_length(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    sample: Option<f64>,
    dir: Direction,
) -> f64 {
    let candidates = candidate_sources(topo, mask);
    let sources = pick_sources(topo, candidates.as_deref(), sample);
    let (sum, count, _max) = bfs_source_scan(topo, &sources, dir, mask);
    if count == 0 {
        0.0
    } else {
        sum / count as f64
    }
}

/// The diameter: the longest shortest path over reachable ordered pairs,
/// following edges in `dir`. `approximate=false` is exact (the largest
/// eccentricity over every source); `approximate=true` is a lower bound from a
/// bounded deterministic sample of sources. `0` when no pair is reachable.
pub fn diameter(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    approximate: bool,
    dir: Direction,
) -> i64 {
    let candidates = candidate_sources(topo, mask);
    let n = candidates.as_ref().map_or(topo.n_nodes(), Vec::len);
    // Approximate: sample a bounded number of sources (deterministic stride).
    let frac = (approximate && n > 0).then(|| (128.0 / n as f64).min(1.0));
    let sources = pick_sources(topo, candidates.as_deref(), frac);
    let (_sum, _count, max) = bfs_source_scan(topo, &sources, dir, mask);
    max as i64
}
