//! The fixed sub-rounds of the opt-in parallel community kernels: Louvain's
//! local moving (#387) and label propagation (#453).
//!
//! Both visit their nodes in a seeded shuffled order. Their parallel forms cut
//! that order into a fixed number of runs (a constant of the kernel, never the
//! thread count), let the nodes of a run decide in parallel against the state
//! as it was when the run started, and then write the decisions in run order.
//! The runs depend only on the node count, the seed and the number of runs, so
//! the result is the same on any number of threads.

use super::linked::Linked;
use super::rng::shuffled_order;

/// The visit order of a kernel's working nodes and where each of `runs` runs
/// starts in it (`runs + 1` boundaries into the order).
///
/// The runs are cut from the seeded order of all `n_full` nodes into runs of
/// `n_full / runs` (rounded up). With `level`, the working nodes are compact
/// (`level.nodes()` of a full graph of `level.n()` nodes, the rest isolated):
/// the order holds the compact ids of the linked nodes, in the order the full
/// permutation gives them, and each run holds the linked nodes that fall in
/// its stretch of the full order. So a compact run makes the full run's
/// decisions, an isolated node never deciding anything.
pub(crate) fn sub_rounds(
    n_full: usize,
    seed: u64,
    level: Option<&Linked>,
    runs: usize,
) -> (Vec<u32>, Vec<usize>) {
    let run_len = n_full.div_ceil(runs).max(1);
    let full = shuffled_order(n_full, seed);
    match level {
        None => {
            let bounds = (0..=runs).map(|r| (r * run_len).min(n_full)).collect();
            (full, bounds)
        }
        Some(l) => {
            let (order, gaps, _) = l.compact_order(&full);
            drop(full);
            let mut bounds = vec![0usize];
            let mut skipped = 0usize;
            for (i, &gap) in gaps.iter().enumerate() {
                // `i + skipped` is the node's place in the full order.
                skipped += gap as usize;
                while bounds.len() <= runs && i + skipped >= bounds.len() * run_len {
                    bounds.push(i);
                }
            }
            bounds.resize(runs + 1, order.len());
            (order, bounds)
        }
    }
}

/// Each node's place in `order` (a permutation of `0..order.len()`).
pub(crate) fn places(order: &[u32]) -> Vec<u32> {
    let mut place = vec![0u32; order.len()];
    for (i, &u) in order.iter().enumerate() {
        place[u as usize] = i as u32;
    }
    place
}

/// The run that place `i` of the order falls in, given the runs' `bounds`.
#[inline]
pub(crate) fn run_of(bounds: &[usize], i: usize) -> usize {
    bounds.partition_point(|&b| b <= i) - 1
}
