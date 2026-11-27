//! The session protocol on a WebSocket (`design/console.md` §4).
//!
//! A message is a JSON text frame, then one binary frame for each part it
//! lists, in order. A binary frame starts with 8 bytes: the request's `id`
//! (`u32`), the part's index (`u16`) and two reserved bytes, zero,
//! little-endian. Requests and replies are framed alike, and a reply's
//! frames are never interleaved with another's.
//!
//! A connection is three tasks: the reader takes requests off the socket,
//! the replier waits for the session's replies in the order the requests
//! were sent, and the writer writes them. So the reader is free while a
//! request runs, and answers at once what bypasses the session's queue
//! (§4.5): `cancel`, `memory`, and `hello` to the shared session once it has
//! answered one. The first request must be `hello`, which attaches the
//! connection to its session: from then on, the session's events
//! (`{"event": …}`, a text frame of their own) come to it too, whichever
//! connection's request caused them, each before that request's reply.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};
use ursa_shell::session::{envelope, Error, Reply};

use crate::session::{crash, Job, SessionHandle};
use crate::Shared;

/// The 8-byte header of a part's frame.
pub(crate) fn part_header(id: u32, index: u16) -> [u8; 8] {
    let mut head = [0u8; 8];
    head[..4].copy_from_slice(&id.to_le_bytes());
    head[4..6].copy_from_slice(&index.to_le_bytes());
    head
}

/// A request read off the socket.
struct Request {
    id: u32,
    op: String,
    args: Value,
    parts: Vec<Vec<u8>>,
}

/// What reading the socket gave.
enum Read {
    Request(Request),
    /// A request that is not one, whose frames were all read: answered with
    /// a `protocol` error, and the connection goes on.
    Bad(Option<u64>, Error),
    /// The framing is lost: answered, and the connection closed.
    Lost(Option<u64>, Error),
    Closed,
}

/// What a connection's writer writes.
pub(crate) enum Outgoing {
    Reply(Option<u64>, Result<Reply, Error>),
    /// An event of the session (`{"event": …}`), in a text frame.
    Event(Value),
    Close(u16, &'static str),
}

/// A request sent to the session, waiting for its reply.
struct Waiting {
    id: u32,
    answer: oneshot::Receiver<Result<Reply, Error>>,
    /// For `hello`: the session to name in its result.
    hello: Option<SessionHandle>,
}

/// The requests of this connection that are queued or running, by id, with
/// what cancels each.
type Running = Arc<Mutex<HashMap<u32, watch::Sender<bool>>>>;

/// Serve one connection until it closes or the server stops.
pub(crate) async fn connection(socket: WebSocket, shared: Arc<Shared>) {
    let _live = shared.live.enter();
    let (sink, mut stream) = socket.split();
    let (out, outgoing) = mpsc::unbounded_channel();
    let writer = tokio::spawn(write(sink, outgoing));
    let (queue, waiting) = mpsc::unbounded_channel();
    let running: Running = Arc::default();
    let replier = tokio::spawn(reply(waiting, out.clone(), Arc::clone(&running)));
    let mut stopping = shared.stopping.clone();
    let mut session: Option<SessionHandle> = None;
    // Who this connection is: its first `hello`'s `client`, which names its
    // requests in the session's history.
    let mut client: Option<Value> = None;
    loop {
        let read = tokio::select! {
            read = read_request(&mut stream) => read,
            _ = stopping.wait_for(|s| *s) => {
                let _ = out.send(Outgoing::Close(1001, "the server stopped"));
                break;
            }
        };
        let request = match read {
            Read::Request(request) => request,
            Read::Bad(id, e) => {
                let _ = out.send(Outgoing::Reply(id, Err(e)));
                continue;
            }
            Read::Lost(id, e) => {
                let _ = out.send(Outgoing::Reply(id, Err(e)));
                let _ = out.send(Outgoing::Close(1002, "the framing was lost"));
                break;
            }
            Read::Closed => break,
        };
        let id = request.id;
        let answer = |result| out.send(Outgoing::Reply(Some(id.into()), result));
        if request.op == "hello" && client.is_none() {
            client = request.args.get("client").cloned();
        }
        match (request.op.as_str(), &session) {
            ("hello", _) => match attach(&shared, &mut session, &request.args, &out) {
                // The shared session may be busy with another connection's
                // request: say what it said before.
                Ok(handle) if handle.shared && handle.hello.get().is_some() => {
                    let mut result = handle.hello.get().cloned().unwrap_or_default();
                    name_session(&mut result, &handle);
                    let _ = answer(Ok(Reply {
                        result,
                        parts: Vec::new(),
                    }));
                }
                Ok(handle) => {
                    let waiting = submit(&handle, request, &running, &client);
                    let _ = queue.send(Waiting {
                        hello: Some(handle),
                        ..waiting
                    });
                }
                Err(e) => {
                    let _ = answer(Err(e));
                }
            },
            ("cancel", _) => cancel(&request.args, &running, answer),
            // The process's memory, as the session would say: it needs no
            // turn.
            ("memory", Some(_)) => {
                let _ = answer(Ok(memory()));
            }
            (_, None) => {
                let _ = answer(Err(Error::protocol(
                    "say hello first: it chooses the session",
                )));
            }
            (_, Some(handle)) => {
                let _ = queue.send(submit(handle, request, &running, &client));
            }
        }
    }
    // Whatever this connection still has queued or running is cancelled
    // (the senders are dropped), and its replies are not waited for.
    running.lock().unwrap_or_else(|e| e.into_inner()).clear();
    replier.abort();
    drop((queue, out, session));
    // The writer ends once it has written what it was given (a close frame).
    let _ = writer.await;
}

/// Attach the connection to the session `hello` asks for: `session: "new"`
/// or `"shared"` (the default). A second `hello` keeps the session the first
/// chose.
fn attach(
    shared: &Shared,
    session: &mut Option<SessionHandle>,
    args: &Value,
    out: &mpsc::UnboundedSender<Outgoing>,
) -> Result<SessionHandle, Error> {
    // The session checks it too; the shared session's later hellos are
    // answered here.
    if let Some(protocol) = args.get("protocol") {
        if protocol.as_u64() != Some(1) {
            return Err(Error::protocol(format!(
                "this server speaks protocol 1, not {protocol}"
            )));
        }
    }
    let wanted = match args.get("session") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s == "new" => Some(false),
        Some(Value::String(s)) if s == "shared" => Some(true),
        Some(other) => {
            return Err(Error::protocol(format!(
                "hello's session is \"new\" or \"shared\", not {other}"
            )))
        }
    };
    if let Some(handle) = session {
        return match wanted {
            Some(want) if want != handle.shared => Err(Error::protocol(
                "this connection has its session already: open another for a different one",
            )),
            _ => Ok(handle.clone()),
        };
    }
    // The protocol's default is the shared session; the shell asks for a
    // new one.
    let handle = if wanted == Some(false) {
        shared.sessions.new_session()
    } else {
        shared.sessions.shared()
    }
    .map_err(|e| Error {
        kind: "crash",
        message: format!("cannot start a session: {e}"),
    })?;
    // Its events come to this connection too, from now on.
    handle.listen(out);
    *session = Some(handle.clone());
    Ok(handle)
}

/// Queue `request` on the session, cancellable by its id.
fn submit(
    handle: &SessionHandle,
    request: Request,
    running: &Running,
    client: &Option<Value>,
) -> Waiting {
    let (cancel, cancelled) = watch::channel(false);
    running
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(request.id, cancel);
    let (reply, answer) = oneshot::channel();
    handle.send(Job {
        client: client.clone(),
        op: request.op,
        args: request.args,
        parts: request.parts,
        cancel: cancelled,
        reply,
    });
    Waiting {
        id: request.id,
        answer,
        hello: None,
    }
}

/// `cancel {target}`: stop the request `target` of this connection, queued
/// or running; it replies `cancelled`. A request that has already replied
/// is not an error: the two crossed.
///
/// `cancel`'s own reply is handed to the writer before the target is told to
/// stop, so it is always written first: once told, the target's `cancelled`
/// can reach the writer at once from the session's thread.
fn cancel<T>(args: &Value, running: &Running, answer: impl FnOnce(Result<Reply, Error>) -> T) {
    let Some(target) = args.get("target").and_then(Value::as_u64) else {
        answer(Err(Error::protocol(
            "cancel needs a target: a request's id",
        )));
        return;
    };
    // Held until the target is told, so it cannot reply and leave between
    // the two (it is removed from `running` only once it has replied).
    let running = running.lock().unwrap_or_else(|e| e.into_inner());
    answer(Ok(Reply {
        result: json!({}),
        parts: Vec::new(),
    }));
    if let Some(cancel) = u32::try_from(target).ok().and_then(|t| running.get(&t)) {
        cancel.send_replace(true);
    }
}

/// Wait for each reply in the order the requests were sent, and pass it to
/// the writer.
async fn reply(
    mut waiting: mpsc::UnboundedReceiver<Waiting>,
    out: mpsc::UnboundedSender<Outgoing>,
    running: Running,
) {
    while let Some(Waiting { id, answer, hello }) = waiting.recv().await {
        // No reply: the session stopped (its engine panicked).
        let mut outcome = answer.await.unwrap_or_else(|_| Err(crash()));
        running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        if let (Some(handle), Ok(reply)) = (hello, &mut outcome) {
            handle.hello.get_or_init(|| reply.result.clone());
            name_session(&mut reply.result, &handle);
        }
        if out.send(Outgoing::Reply(Some(id.into()), outcome)).is_err() {
            return;
        }
    }
}

/// Put the session's id and root directory in `hello`'s result: `session:
/// {kind, id, cwd}`, beside what the session says of itself.
fn name_session(result: &mut Value, handle: &SessionHandle) {
    let Some(result) = result.as_object_mut() else {
        return;
    };
    let session = result.entry("session").or_insert_with(|| json!({}));
    if !session.is_object() {
        *session = json!({});
    }
    session["kind"] = json!(if handle.shared { "shared" } else { "new" });
    session["id"] = json!(handle.id);
    // Relative paths resolve against the session's root.
    session["cwd"] = json!(&*handle.root);
}

/// Write each reply, its JSON then its parts, until a close or the end.
async fn write(
    mut sink: SplitSink<WebSocket, Message>,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
) {
    while let Some(message) = outgoing.recv().await {
        let written = match message {
            Outgoing::Reply(id, outcome) => write_reply(&mut sink, id, outcome).await,
            Outgoing::Event(event) => sink.send(Message::Text(event.to_string().into())).await,
            Outgoing::Close(code, reason) => {
                let frame = CloseFrame {
                    code,
                    reason: reason.into(),
                };
                let _ = sink.send(Message::Close(Some(frame))).await;
                return;
            }
        };
        if written.is_err() {
            return;
        }
    }
    let _ = sink.close().await;
}

/// Write a reply: its JSON, then each part, flushed together.
async fn write_reply(
    sink: &mut SplitSink<WebSocket, Message>,
    id: Option<u64>,
    outcome: Result<Reply, Error>,
) -> Result<(), axum::Error> {
    let text = envelope(id, &outcome).to_string();
    sink.feed(Message::Text(text.into())).await?;
    if let Ok(reply) = outcome {
        let id = id.unwrap_or(0) as u32;
        for (index, part) in reply.parts.into_iter().enumerate() {
            let mut frame = Vec::with_capacity(8 + part.bytes.len());
            frame.extend_from_slice(&part_header(id, index as u16));
            frame.extend_from_slice(&part.bytes);
            sink.feed(Message::Binary(frame.into())).await?;
        }
    }
    sink.flush().await
}

/// The next request, with its parts.
async fn read_request(stream: &mut SplitStream<WebSocket>) -> Read {
    let text = loop {
        match stream.next().await {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return Read::Closed,
            Some(Ok(Message::Text(text))) => break text,
            Some(Ok(Message::Binary(_))) => {
                return Read::Lost(
                    None,
                    Error::protocol("a binary frame with no request before it"),
                )
            }
            // Pings are answered by the library.
            Some(Ok(_)) => continue,
        }
    };
    let message: Value = match serde_json::from_str(&text) {
        Ok(message @ Value::Object(_)) => message,
        Ok(_) => return Read::Lost(None, Error::protocol("a request is a JSON object")),
        Err(e) => {
            return Read::Lost(
                None,
                Error::protocol(format!("a request that is not JSON: {e}")),
            )
        }
    };
    let id = message.get("id").and_then(Value::as_u64);
    let listed: &[Value] = match message.get("parts") {
        None | Some(Value::Null) => &[],
        Some(Value::Array(parts)) => parts,
        Some(_) => return Read::Lost(id, Error::protocol("parts is a list")),
    };
    // Read the parts first, so that a bad request loses no frames.
    let mut parts = Vec::with_capacity(listed.len());
    if !listed.is_empty() {
        let Some(id32) = id.and_then(|id| u32::try_from(id).ok()) else {
            return Read::Lost(
                id,
                Error::protocol("a request with parts needs a 32-bit id"),
            );
        };
        while parts.len() < listed.len() {
            let frame = match stream.next().await {
                Some(Ok(Message::Binary(frame))) => frame,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return Read::Closed,
                Some(Ok(_)) => {
                    return Read::Lost(id, Error::protocol("a request's parts must follow it"))
                }
            };
            if frame.len() < 8 || frame[..8] != part_header(id32, parts.len() as u16) {
                return Read::Lost(
                    id,
                    Error::protocol("a part out of order, or of another request"),
                );
            }
            parts.push(frame[8..].to_vec());
        }
    }
    let bad = |m: &str| Read::Bad(id, Error::protocol(m));
    if message.get("v").and_then(Value::as_u64) != Some(1) {
        return bad("this server speaks protocol version 1");
    }
    let Some(id) = id.and_then(|id| u32::try_from(id).ok()) else {
        return bad("a request needs an id that fits in 32 bits");
    };
    let Some(op) = message.get("op").and_then(Value::as_str) else {
        return bad("a request needs an op");
    };
    for (part, bytes) in listed.iter().zip(&parts) {
        if part.get("bytes").and_then(Value::as_u64) != Some(bytes.len() as u64) {
            return bad("a part's length is not what the request listed");
        }
    }
    let args = match message.get("args") {
        None | Some(Value::Null) => json!({}),
        Some(args @ Value::Object(_)) => args.clone(),
        Some(_) => return bad("args is an object"),
    };
    Read::Request(Request {
        id,
        op: op.to_string(),
        args,
        parts,
    })
}

/// The `memory` operation, natively: `{bytes, kind: "rss"}`, the process's
/// resident memory (the browser demo's is its WebAssembly memory, `"wasm"`).
fn memory() -> Reply {
    Reply {
        result: json!({"bytes": resident_bytes(), "kind": "rss"}),
        parts: Vec::new(),
    }
}

/// The process's resident set, from `/proc/self/statm` (Linux), else 0.
fn resident_bytes() -> u64 {
    let pages = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok());
    pages.map_or(0, |p| p * 4096)
}
