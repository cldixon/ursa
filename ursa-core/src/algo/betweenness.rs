//! Betweenness centrality — Brandes' algorithm.
//!
//! For each source, a BFS records the number of shortest paths (`sigma`) and the
//! predecessor DAG; a reverse pass over the BFS order accumulates each node's
//! dependency. Ported from the canonical Brandes formulation (spec §Algorithm
//! kernels — "do not invent these"). Directed: paths follow out-edges and ordered
//! pairs `(s, t)` are counted once, so there is no final halving.
//!
//! Exact is `O(n · m)`. `sample = Some(frac)` runs Brandes from a bounded,
//! deterministic strided subset of `⌈frac · n⌉` sources and scales the result by
//! `n / k` (the Brandes–Pich estimator) — reproducible without an RNG. Sources
//! are processed in parallel with Rayon; each contributes a local vector that is
//! summed at the end.

use std::collections::VecDeque;

use rayon::prelude::*;

use crate::topology::Topology;

/// Betweenness centrality per node (directed, following out-edges). See the
/// module docs for the exact-vs-`sample` behaviour and scaling.
pub fn betweenness(topo: &Topology, sample: Option<f64>) -> Vec<f64> {
    let n = topo.n_nodes();
    if n == 0 {
        return Vec::new();
    }
    let sources = sample_sources(n, sample);
    let k = sources.len();
    if k == 0 {
        return vec![0.0; n];
    }

    let bc = sources
        .par_iter()
        .fold(
            || vec![0.0f64; n],
            |mut acc, &s| {
                brandes_from(topo, s, &mut acc);
                acc
            },
        )
        .reduce(
            || vec![0.0f64; n],
            |mut a, b| {
                for (x, y) in a.iter_mut().zip(b) {
                    *x += y;
                }
                a
            },
        );

    if sample.is_some() {
        let scale = n as f64 / k as f64;
        bc.into_iter().map(|x| x * scale).collect()
    } else {
        bc
    }
}

/// One Brandes single-source pass, accumulating dependencies into `bc`.
fn brandes_from(topo: &Topology, s: u32, bc: &mut [f64]) {
    let n = topo.n_nodes();
    let out = topo.out();

    let mut dist = vec![-1i64; n];
    let mut sigma = vec![0.0f64; n];
    let mut preds: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut order: Vec<u32> = Vec::new(); // BFS discovery order (non-decreasing distance)

    dist[s as usize] = 0;
    sigma[s as usize] = 1.0;
    let mut queue = VecDeque::new();
    queue.push_back(s);

    while let Some(v) = queue.pop_front() {
        order.push(v);
        let dv = dist[v as usize];
        for &w in out.neighbors(v) {
            // First time we reach w: set its distance and enqueue.
            if dist[w as usize] < 0 {
                dist[w as usize] = dv + 1;
                queue.push_back(w);
            }
            // w is one hop further along a shortest path through v.
            if dist[w as usize] == dv + 1 {
                sigma[w as usize] += sigma[v as usize];
                preds[w as usize].push(v);
            }
        }
    }

    // Reverse accumulation of dependencies.
    let mut delta = vec![0.0f64; n];
    while let Some(w) = order.pop() {
        let coeff = (1.0 + delta[w as usize]) / sigma[w as usize];
        for &v in &preds[w as usize] {
            delta[v as usize] += sigma[v as usize] * coeff;
        }
        if w != s {
            bc[w as usize] += delta[w as usize];
        }
    }
}

/// `None` → every node; `Some(frac)` → a deterministic evenly-strided subsample
/// of `⌈frac · n⌉` sources (reproducible, no RNG), at least one when `n > 0`.
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
            let step = (n as f64 / k as f64).max(1.0);
            (0..k)
                .map(|i| ((i as f64 * step).floor() as usize).min(n - 1) as u32)
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn betweenness_of_a_directed_line() {
        // 0->1->2->3->4. Node v lies between every ordered pair (s, t) with
        // s < v < t: bc = [0, 3, 4, 3, 0].
        let t = Topology::build(5, vec![0, 1, 2, 3], vec![1, 2, 3, 4]);
        let bc = betweenness(&t, None);
        assert_eq!(bc, vec![0.0, 3.0, 4.0, 3.0, 0.0]);
    }

    #[test]
    fn diamond_splits_paths_evenly() {
        // 0->1, 0->2, 1->3, 2->3: two shortest 0->3 paths, so nodes 1 and 2 each
        // carry half the (0,3) pair -> 0.5 apiece; endpoints 0 and 3 carry none.
        let t = Topology::build(4, vec![0, 0, 1, 2], vec![1, 2, 3, 3]);
        let bc = betweenness(&t, None);
        assert_eq!(bc[0], 0.0);
        assert_eq!(bc[3], 0.0);
        assert!((bc[1] - 0.5).abs() < 1e-12, "got {}", bc[1]);
        assert!((bc[2] - 0.5).abs() < 1e-12, "got {}", bc[2]);
    }

    #[test]
    fn sample_scales_and_stays_bounded() {
        let t = Topology::build(5, vec![0, 1, 2, 3], vec![1, 2, 3, 4]);
        // A partial-source estimate is non-negative and finite; the middle node
        // still ranks at or above the endpoints.
        let bc = betweenness(&t, Some(0.5));
        assert!(bc.iter().all(|&x| x >= 0.0 && x.is_finite()));
        assert!(bc[2] >= bc[0]);
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(betweenness(&t, None).is_empty());
    }
}
