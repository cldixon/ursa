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

impl Pace {
    /// Give the event loop a turn now, if this pace yields at all, whatever
    /// the clock says: for work that needs the turn itself, not only the
    /// time (a browser frees what was written to the GPU between tasks).
    pub async fn turn(&mut self) {
        if self.since.is_some() {
            wait_a_turn().await;
            self.since = Some(now());
        }
    }

    /// Whether this pace gives turns at all.
    pub fn yields(&self) -> bool {
        self.since.is_some()
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
/// browser's (feature `web`); elsewhere nothing yields, so no clock is read.
fn now() -> f64 {
    #[cfg(all(feature = "web", target_arch = "wasm32"))]
    {
        js_sys::Date::now()
    }
    #[cfg(not(all(feature = "web", target_arch = "wasm32")))]
    {
        0.0
    }
}

/// Give the event loop a turn: a macrotask in the browser (feature `web`),
/// where GPU callbacks are delivered; elsewhere, nothing to wait for.
///
/// The task is a message on a channel of this thread's own, not a timer: a
/// browser makes a timer set from a timer's task wait 4 ms, and a view of
/// a million edges gives sixty turns or more as it is made, which was a
/// quarter of a second of it spent waiting for nothing (#525). A message
/// is the next task the loop takes, after whatever else is due.
pub(crate) async fn yield_now() {
    #[cfg(all(feature = "web", target_arch = "wasm32"))]
    {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            if !turns::post(&resolve) {
                turns::after_a_timer(&resolve);
            }
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}

/// Give the event loop a turn that lasts: a timer's task, which a browser
/// runs no sooner than a millisecond on (and 4 ms on, from a timer's own
/// task). For work that waits for something else to have happened by then
/// (the GPU taking what was written to it), not only for a turn.
pub(crate) async fn wait_a_turn() {
    #[cfg(all(feature = "web", target_arch = "wasm32"))]
    {
        let promise = js_sys::Promise::new(&mut |resolve, _| turns::after_a_timer(&resolve));
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}

/// The turns of the browser's event loop.
#[cfg(all(feature = "web", target_arch = "wasm32"))]
mod turns {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use js_sys::{Array, Function, Reflect};
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::{JsCast, JsValue};

    /// Call `resolve` from a timer's task (or now, where there are no timers).
    pub(super) fn after_a_timer(resolve: &Function) {
        let global = js_sys::global();
        let set_timeout = Reflect::get(&global, &"setTimeout".into())
            .ok()
            .and_then(|f| f.dyn_into::<Function>().ok());
        match set_timeout {
            Some(f) => {
                let _ = f.call2(&global, resolve, &0.into());
            }
            None => {
                let _ = resolve.call0(&JsValue::UNDEFINED);
            }
        }
    }

    /// This thread's channel to itself: what posts to it, and what its
    /// messages call, in order.
    struct Channel {
        port: JsValue,
        post: Function,
        waiting: VecDeque<Function>,
    }

    thread_local! {
        /// `None` once it could not be made: there is no `MessageChannel`.
        static CHANNEL: RefCell<Option<Option<Channel>>> = const { RefCell::new(None) };
    }

    fn open() -> Option<Channel> {
        let make = Reflect::get(&js_sys::global(), &"MessageChannel".into())
            .ok()?
            .dyn_into::<Function>()
            .ok()?;
        let channel = Reflect::construct(&make, &Array::new()).ok()?;
        let (to, from) = (
            Reflect::get(&channel, &"port1".into()).ok()?,
            Reflect::get(&channel, &"port2".into()).ok()?,
        );
        let post = Reflect::get(&to, &"postMessage".into())
            .ok()?
            .dyn_into::<Function>()
            .ok()?;
        // Each message is one turn asked for: the oldest.
        let taken = Closure::<dyn FnMut()>::new(|| {
            let next = CHANNEL.with(|c| {
                c.borrow_mut()
                    .as_mut()
                    .and_then(Option::as_mut)
                    .and_then(|c| c.waiting.pop_front())
            });
            if let Some(resolve) = next {
                let _ = resolve.call0(&JsValue::UNDEFINED);
            }
        });
        Reflect::set(&from, &"onmessage".into(), taken.as_ref()).ok()?;
        // The channel lasts as long as the thread does.
        taken.forget();
        Some(Channel {
            port: to,
            post,
            waiting: VecDeque::new(),
        })
    }

    /// Call `resolve` from the task of a message posted now; `false` if
    /// there is no channel to post it on.
    pub(super) fn post(resolve: &Function) -> bool {
        CHANNEL.with(|c| {
            let mut c = c.borrow_mut();
            let Some(channel) = c.get_or_insert_with(open) else {
                return false;
            };
            channel.waiting.push_back(resolve.clone());
            if channel
                .post
                .call1(&channel.port, &JsValue::UNDEFINED)
                .is_err()
            {
                channel.waiting.pop_back();
                return false;
            }
            true
        })
    }
}
