//! Making a view without holding the page's thread for all of it.
//!
//! A view of a million edges takes a quarter of a second to make even when
//! it builds no adjacency: the shaders are compiled, the pipelines linked
//! and megabytes written to the GPU. In a browser that is the page's
//! thread, and nothing else happens on it meanwhile. A view made
//! [paced](Pace::yielding) gives the event loop a turn between those
//! pieces whenever it has held the thread for [`BUDGET_MS`], so a click or
//! a frame waits for one piece and not for all of them. What is made is the
//! same either way.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;

/// How long a paced piece of work holds the thread before it gives the
/// event loop a turn at the next [`Pace::breathe`], in milliseconds. The
/// longest hold is this and the piece that follows: the largest pieces at a
/// million edges (compiling the shader, linking the first pipeline, a pass
/// over the edges) are 10 to 30 ms each.
pub const BUDGET_MS: f64 = 4.0;

/// Whether work gives up the thread as it goes, and when it last did.
#[derive(Debug)]
pub struct Pace {
    /// When the thread was taken; `None` never gives it up.
    since: Option<f64>,
}

impl Pace {
    /// All at once: for a caller with no event loop to give a turn to.
    pub fn never() -> Self {
        Self { since: None }
    }

    /// A turn of the event loop whenever [`BUDGET_MS`] have gone by.
    ///
    /// The first [`Pace::breathe`] gives one whatever the clock says: the
    /// caller has held the thread for a time this pace did not see.
    pub fn yielding() -> Self {
        Self {
            since: Some(f64::NEG_INFINITY),
        }
    }

    /// Between two pieces of work: give the event loop a turn if this pace
    /// yields and the thread has been held for [`BUDGET_MS`].
    pub async fn breathe(&mut self) {
        if let Some(since) = self.since {
            if now() - since >= BUDGET_MS {
                yield_now().await;
                self.since = Some(now());
            }
        }
    }
}

/// The value of a future that never waits: one made with [`Pace::never`].
pub(crate) fn ready<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => unreachable!("a pace that never yields does not wait"),
    }
}

/// Milliseconds on a clock that only moves forward enough to pace by: the
/// browser's; natively nothing yields, so no clock is read.
fn now() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0.0
    }
}

/// Give the event loop a turn: a macrotask in the browser, where GPU
/// callbacks are delivered; natively, nothing to wait for.
pub(crate) async fn yield_now() {
    #[cfg(target_arch = "wasm32")]
    {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            let global = js_sys::global();
            let set_timeout = js_sys::Reflect::get(&global, &"setTimeout".into())
                .ok()
                .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
            match set_timeout {
                Some(f) => {
                    let _ = f.call2(&global, &resolve, &0.into());
                }
                None => {
                    let _ = resolve.call0(&wasm_bindgen::JsValue::UNDEFINED);
                }
            }
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}
