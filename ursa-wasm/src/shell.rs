//! The `ursa` shell in the browser: [`ursa_shell::session::Session`], the
//! session protocol's operations over the shell the CLI runs, for a page's
//! terminal (xterm.js) in a Web Worker, as DuckDB's web shell is DuckDB's own
//! shell.
//!
//! ```js
//! WebShell.setBaseUrl(new URL("data/", location.href).href);
//! const shell = new WebShell();
//! await shell.handle("hello", JSON.stringify({ protocol: 1, session: "new" }), []);
//! const { message, parts } = await shell.handle("feed", JSON.stringify({ line: "SELECT 42 AS answer;" }), []);
//! const { ok, result } = JSON.parse(message);
//! term.write(result.out.replaceAll("\n", "\r\n"));
//! ```
//!
//! Every operation goes through [`WebShell::handle`]: the operation's name,
//! its arguments as JSON, and its binary parts in; the reply as the
//! protocol's JSON and its binary parts out (`design/console.md` §4).
//! `ursa serve` runs the same `Session` natively, over a WebSocket, so the
//! page cannot tell the two apart. The operations are listed at
//! [`Session::handle`].

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{Array, Object, Promise, Reflect, Uint8Array};
use serde_json::Value;
use ursa_shell::session::{envelope, Event, Options, Reply, Session};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

/// The shell, as a page's terminal drives it.
#[wasm_bindgen]
pub struct WebShell {
    session: Rc<RefCell<Session>>,
}

impl Default for WebShell {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl WebShell {
    /// A shell over a new engine session, with files at
    /// `memory://uploads/`, drawing tables in colour for a terminal 80
    /// characters wide until the `width` operation says otherwise.
    #[wasm_bindgen(constructor)]
    pub fn new() -> WebShell {
        #[cfg(target_arch = "wasm32")]
        console_error_panic_hook::set_once();
        let options = Options {
            version: env!("CARGO_PKG_VERSION").to_string(),
            transport: "wasm",
            capabilities: vec!["uploads"],
            uploads: true,
            root: crate::page_root(),
            copy_to_disk: false,
        };
        WebShell {
            session: Rc::new(RefCell::new(Session::new(options))),
        }
    }

    /// Set the URL relative paths resolve against, such as
    /// `https://example.com/data/` (end a directory with `/`): the page's
    /// root, which every shell and session made after this takes (and keeps
    /// through `start`). Call it before `new WebShell()`.
    #[wasm_bindgen(js_name = setBaseUrl)]
    pub fn set_base_url(base: &str) -> Result<(), JsError> {
        crate::set_base_url(base)
    }

    /// Run the operation `op` with `args` (a JSON object, as text) and
    /// `parts` (an array of `Uint8Array`s), and resolve to `{message,
    /// parts, events}`: `message` is the reply as the protocol's JSON text,
    /// `{"ok": true, "result", "parts": [{"name", "kind", "bytes"}]}` or
    /// `{"ok": false, "error": {"kind", "message"}}`; `parts` the reply's
    /// binary parts, each a `Uint8Array` with a buffer of its own, to
    /// transfer; and `events` the events the request caused, as JSON text of
    /// an array (`[{"event": "catalog"}]`), to deliver before the reply.
    ///
    /// It rejects only when the request is not one: `args` that are not
    /// JSON, or a second call before the first resolved (the page runs one
    /// at a time).
    pub fn handle(&self, op: String, args: &str, parts: Array) -> Promise {
        let session = Rc::clone(&self.session);
        let args: Result<Value, _> = serde_json::from_str(args);
        let parts: Vec<Vec<u8>> = parts
            .iter()
            .map(|part| Uint8Array::new(&part).to_vec())
            .collect();
        future_to_promise(async move {
            let args = args.map_err(|e| JsError::new(&format!("args: {e}")))?;
            let (outcome, events) = handle(&session, &op, args, parts).await?;
            let buffers = Array::new();
            if let Ok(reply) = &outcome {
                for part in &reply.parts {
                    buffers.push(&Uint8Array::from(part.bytes.as_slice()));
                }
            }
            let answer = Object::new();
            let message = envelope(None, &outcome).to_string();
            Reflect::set(&answer, &"message".into(), &message.into())?;
            Reflect::set(&answer, &"parts".into(), &buffers)?;
            let events: Vec<Value> = events.iter().map(Event::to_json).collect();
            let events = Value::Array(events).to_string();
            Reflect::set(&answer, &"events".into(), &events.into())?;
            Ok(answer.into())
        })
    }
}

/// [`WebShell::handle`]'s reply and the events its request caused, or why
/// there is none.
#[allow(
    clippy::await_holding_refcell_ref,
    reason = "the page sends one request at a time and waits for it; a second \
              one fails with try_borrow_mut instead of panicking"
)]
async fn handle(
    session: &RefCell<Session>,
    op: &str,
    args: Value,
    parts: Vec<Vec<u8>>,
) -> Result<(Result<Reply, ursa_shell::session::Error>, Vec<Event>), JsError> {
    let mut session = session
        .try_borrow_mut()
        .map_err(|_| JsError::new("the shell is running a request; wait for it to resolve"))?;
    let outcome = session.handle(op, args, parts).await;
    Ok((outcome, session.take_events().await))
}
