//! # ursa-py
//!
//! PyO3 bindings for Ursa. This crate is deliberately *thin*: its job is to build
//! expression/plan trees, orchestrate `collect()`, and move Arrow across the FFI
//! boundary (zero-copy, via the Arrow PyCapsule interface). Compute lives in
//! `ursa-core`; planning lives in `ursa-plan`.
//!
//! ## Two rules this layer enforces (spec §Runtime integration)
//!
//! 1. **Release the GIL** for the duration of `collect()` (`py.allow_threads`) so
//!    Ursa behaves inside threaded Python servers. The demo kernels below already
//!    do this.
//! 2. **Arrow FFI via the PyCapsule interface** for zero-copy exchange with
//!    polars/pyarrow — wired in once `collect()` returns real Arrow batches.
//!
//! ## Status: skeleton
//!
//! Exposes `__core_version` and a `_demo_*` family that runs the *real*
//! `ursa-core` kernels over dense edge lists. These exist to prove the
//! Python↔Rust↔kernel wiring end-to-end (see `tests/test_smoke.py`) and will be
//! superseded by the `EdgeFrame`/`NodeFrame` plan-building API.

use pyo3::prelude::*;

use ursa_core::algo::{connected_components_weak, degree, pagerank, PageRankParams};
use ursa_core::topology::{Direction, Topology};

/// Build a topology from parallel dense-or-arbitrary i64 edge endpoints.
///
/// The demo interns ids itself (small graphs); the real path builds the `IdMap`
/// from Arrow columns in `ursa-plan`. Returns the topology plus the interned
/// user-id order so callers can map dense results back.
fn build_demo_topology(src: &[i64], dst: &[i64]) -> (Topology, Vec<i64>) {
    use ursa_core::IdMap;
    let mut map = IdMap::default();
    let src_dense: Vec<u32> = src.iter().map(|&u| map.intern(u)).collect();
    let dst_dense: Vec<u32> = dst.iter().map(|&u| map.intern(u)).collect();
    let n = map.len();
    (
        Topology::build(n, src_dense, dst_dense),
        map.user_ids().to_vec(),
    )
}

/// The `ursa-core` version — the simplest possible proof the native module loaded.
#[pyfunction]
fn __core_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Demo: unweighted pull-based PageRank over a dense edge list.
/// Returns `(user_id, score)` pairs. Releases the GIL during compute.
#[pyfunction]
#[pyo3(signature = (src, dst, damping=0.85, max_iter=30, tol=1e-6))]
fn _demo_pagerank(
    py: Python<'_>,
    src: Vec<i64>,
    dst: Vec<i64>,
    damping: f64,
    max_iter: u32,
    tol: f64,
) -> Vec<(i64, f64)> {
    let (topo, ids) = build_demo_topology(&src, &dst);
    let scores = py.allow_threads(|| {
        pagerank(
            &topo,
            PageRankParams {
                damping,
                max_iter,
                tol,
            },
        )
    });
    ids.into_iter().zip(scores).collect()
}

/// Demo: degree per node. `direction` is "out" | "in" | "both".
#[pyfunction]
#[pyo3(signature = (src, dst, direction="out"))]
fn _demo_degree(src: Vec<i64>, dst: Vec<i64>, direction: &str) -> PyResult<Vec<(i64, u32)>> {
    let dir = match direction {
        "out" => Direction::Out,
        "in" => Direction::In,
        "both" => Direction::Both,
        other => {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "direction must be 'out', 'in', or 'both'; got {other:?}"
            )))
        }
    };
    let (topo, ids) = build_demo_topology(&src, &dst);
    let deg = degree(&topo, dir);
    Ok(ids.into_iter().zip(deg).collect())
}

/// Demo: weak connected component label per node.
#[pyfunction]
fn _demo_connected_components(src: Vec<i64>, dst: Vec<i64>) -> Vec<(i64, u32)> {
    let (topo, ids) = build_demo_topology(&src, &dst);
    let cc = connected_components_weak(&topo);
    ids.into_iter().zip(cc).collect()
}

/// The native extension module, imported by Python as `ursa._ursa`.
#[pymodule]
fn _ursa(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(__core_version, m)?)?;
    m.add_function(wrap_pyfunction!(_demo_pagerank, m)?)?;
    m.add_function(wrap_pyfunction!(_demo_degree, m)?)?;
    m.add_function(wrap_pyfunction!(_demo_connected_components, m)?)?;
    Ok(())
}
