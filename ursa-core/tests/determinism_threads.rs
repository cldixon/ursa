//! Determinism across rayon thread-pool sizes (#76).
//!
//! The #40 work made the parallel kernels' outputs independent of execution order
//! (fixed-order f64 reductions, order-independent Louvain tie-breaks, seeded
//! sampled betweenness). The rest of the suite checks a *single* thread count,
//! which cannot catch a non-associative parallel reduction — that only diverges as
//! the pool size varies. Here each determinism-sensitive kernel runs on one fixed
//! graph across several `num_threads` values and every result must be bit-identical
//! (exact `==`, not approximate) to the single-threaded run. This locks the #40
//! guarantees against future parallelization work silently reintroducing order
//! dependence.
//!
//! Gated on the `rayon` feature: without it there is no thread pool to vary, and the
//! kernels run through the serial fallback (`ursa-core::parallel`) — whose whole point
//! is to produce the *same* bytes. The per-kernel unit tests, which assert exact
//! expected values, cover that serial path when the suite is run with
//! `--no-default-features`; this cross-pool-size test only has meaning with `rayon` on.
#![cfg(feature = "rayon")]

use rayon::ThreadPoolBuilder;
use ursa_core::algo::{betweenness, label_propagation, louvain, pagerank, PageRankParams};
use ursa_core::Topology;

/// A deterministic pseudo-random directed graph via a hand-rolled LCG, so the test
/// pulls in no RNG dependency and the graph is identical on every run and platform.
fn seeded_graph(n: usize, m: usize) -> Topology {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // High bits have the best distribution for an LCG.
        (state >> 33) as u32
    };
    let mut src = Vec::with_capacity(m);
    let mut dst = Vec::with_capacity(m);
    for _ in 0..m {
        src.push(next() % n as u32);
        dst.push(next() % n as u32);
    }
    Topology::build(n, src, dst)
}

/// Run `f` on a fresh rayon pool pinned to exactly `threads` workers.
fn on_pool<T: Send>(threads: usize, f: impl FnOnce() -> T + Send) -> T {
    ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("thread pool builds")
        .install(f)
}

/// Thread counts to cross-check. 1 is the reference; the rest must match it. 3 is
/// deliberately non-power-of-two so an uneven work split can't hide a bug.
const THREADS: [usize; 4] = [2, 3, 4, 8];

#[test]
fn pagerank_is_thread_count_independent() {
    let t = seeded_graph(400, 4000);
    let params = PageRankParams {
        damping: 0.85,
        max_iter: 100,
        tol: 1e-12,
    };
    let reference = on_pool(1, || pagerank(&t, None, params));
    for &threads in &THREADS {
        assert_eq!(
            on_pool(threads, || pagerank(&t, None, params)),
            reference,
            "pagerank diverged at {threads} threads"
        );
    }
}

#[test]
fn betweenness_full_is_thread_count_independent() {
    let t = seeded_graph(300, 2500);
    let reference = on_pool(1, || betweenness(&t, None, None, None));
    for &threads in &THREADS {
        assert_eq!(
            on_pool(threads, || betweenness(&t, None, None, None)),
            reference,
            "full betweenness diverged at {threads} threads"
        );
    }
}

#[test]
fn betweenness_sampled_is_thread_count_independent() {
    // Sampled + seeded: the #54 guarantee is that a fixed seed pins the sampled
    // source set and the result regardless of how many threads process it.
    let t = seeded_graph(400, 4000);
    let reference = on_pool(1, || betweenness(&t, None, Some(0.25), Some(42)));
    for &threads in &THREADS {
        assert_eq!(
            on_pool(threads, || betweenness(&t, None, Some(0.25), Some(42))),
            reference,
            "sampled betweenness diverged at {threads} threads"
        );
    }
}

#[test]
fn louvain_is_thread_count_independent() {
    let t = seeded_graph(400, 4000);
    let reference = on_pool(1, || louvain(&t, None, 1.0, Some(7)));
    for &threads in &THREADS {
        assert_eq!(
            on_pool(threads, || louvain(&t, None, 1.0, Some(7))),
            reference,
            "louvain diverged at {threads} threads"
        );
    }
}

#[test]
fn label_propagation_is_thread_count_independent() {
    let t = seeded_graph(400, 4000);
    let reference = on_pool(1, || label_propagation(&t, None, 20, Some(7)));
    for &threads in &THREADS {
        assert_eq!(
            on_pool(threads, || label_propagation(&t, None, 20, Some(7))),
            reference,
            "label propagation diverged at {threads} threads"
        );
    }
}
