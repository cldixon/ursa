//! CPU-bound and blocking work, kept off the async workers where there are any.
//!
//! On native targets [`run_blocking`] is `tokio::task::spawn_blocking`: the
//! graph kernels are CPU-bound (and parallel, through rayon), and reading a
//! stored index blocks on the file system, so neither runs on a worker that
//! drives other queries' futures.
//!
//! In the browser (wasm32) there is one thread and no blocking pool, so the
//! closure runs inline, on the task that awaits it. The work and its result
//! are the same; only where it runs differs (#278).

use std::fmt;

/// Run `f` off the async workers (native), or inline (wasm32).
pub async fn run_blocking<F, R>(f: F) -> Result<R, BlockingError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::task::spawn_blocking(f).await.map_err(BlockingError)
    }
    #[cfg(target_arch = "wasm32")]
    {
        Ok(f())
    }
}

/// Why blocking work produced no result: it panicked or was cancelled. Only a
/// native build can see one; inline work on wasm32 cannot fail this way.
#[derive(Debug)]
pub struct BlockingError(#[cfg(not(target_arch = "wasm32"))] tokio::task::JoinError);

impl fmt::Display for BlockingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.0.fmt(f)
        }
        #[cfg(target_arch = "wasm32")]
        {
            f.write_str("blocking work failed")
        }
    }
}

impl std::error::Error for BlockingError {}
