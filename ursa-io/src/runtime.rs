//! One process-wide Tokio runtime for all blocking plan execution.
//!
//! Every `execute_*_query`, scan, and weight evaluation drives an async
//! DataFusion plan to completion from a synchronous, GIL-released Python thread.
//! Each of those call sites used to build a fresh multi-thread runtime — a worker
//! thread pool spun up and torn down *per `.collect()`* (and two or three times
//! for a weighted or scan-backed collect). That per-call thread churn is pure
//! setup waste; the plans themselves are the work.
//!
//! Instead they share the single [`runtime()`] below, built once on first use.
//! Kernels still dispatch through `tokio::task::spawn_blocking` inside each
//! `ExecutionPlan`, so the Rayon-on-Tokio trap is unaffected — this only removes
//! the runtime construction/teardown.
//!
//! ## The "never from inside a runtime" constraint
//!
//! [`Runtime::block_on`] panics if called from a thread already inside a Tokio
//! runtime ("Cannot start a runtime from within a runtime"). Every current caller
//! is a synchronous Python thread with the GIL released, so this cannot happen
//! today — but [`block_on`] guards it explicitly with
//! [`Handle::try_current`], returning a clear [`DataFusionError`] instead of
//! panicking, so a future async surface or embedding gets an error it can handle
//! rather than a process abort.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::OnceLock;

use datafusion::error::{DataFusionError, Result};
#[cfg(not(target_arch = "wasm32"))]
use tokio::runtime::{Handle, Runtime};

#[cfg(not(target_arch = "wasm32"))]
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// The shared multi-thread runtime, built on first use and kept for the life of
/// the process. Panics only if the runtime itself cannot be constructed (an OS
/// failure to spawn the worker pool), which is unrecoverable.
#[cfg(not(target_arch = "wasm32"))]
pub fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build the shared Tokio runtime")
    })
}

/// Drive a future to completion on the shared runtime.
///
/// Returns an error rather than panicking if called from within an existing
/// Tokio runtime (see the module docs). This is the single entry point every
/// blocking plan-execution site uses in place of building its own runtime.
///
/// On wasm32 (the browser) nothing may block: a `fetch` only completes once
/// control returns to the JavaScript event loop. There this always returns an
/// error. The SQL engine never calls it; it drives DataFusion's futures itself
/// (#278).
#[cfg(target_arch = "wasm32")]
pub fn block_on<F: std::future::Future>(_future: F) -> Result<F::Output> {
    Err(DataFusionError::NotImplemented(
        "blocking plan execution is not available in the browser build".to_string(),
    ))
}

/// Drive a future to completion on the shared runtime (native targets).
#[cfg(not(target_arch = "wasm32"))]
pub fn block_on<F: std::future::Future>(future: F) -> Result<F::Output> {
    if Handle::try_current().is_ok() {
        return Err(DataFusionError::Execution(
            "Ursa plan execution cannot be driven from inside an async runtime; \
             call it from a synchronous thread"
                .to_string(),
        ));
    }
    Ok(runtime().block_on(future))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_runs_a_future_and_reuses_the_runtime() {
        let a = block_on(async { 1 + 1 }).unwrap();
        assert_eq!(a, 2);
        // Same runtime instance on a second call (OnceLock identity).
        let r1 = runtime() as *const Runtime;
        let r2 = runtime() as *const Runtime;
        assert_eq!(r1, r2);
    }

    #[tokio::test]
    async fn block_on_errors_inside_a_runtime_instead_of_panicking() {
        // Called from within tokio's test runtime: must be a clean Err, not a panic.
        let err = block_on(async { 42 }).unwrap_err();
        assert!(
            err.to_string().contains("inside an async runtime"),
            "got: {err}"
        );
    }
}
