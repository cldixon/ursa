//! The session protocol on a WebSocket (`design/console.md` §4.2).
//!
//! A message is a JSON text frame, then one binary frame for each part it
//! lists, in order. A binary frame starts with 8 bytes: the request's `id`
//! (`u32`), the part's index (`u16`) and two reserved bytes, little-endian.
//! Requests and replies are framed alike. A reply's frames are never
//! interleaved with another's: the connection reads a request, waits for
//! its reply and writes it before it reads the next.

use axum::extract::ws::{Message, WebSocket};
use serde_json::{json, Value};
use tokio::sync::oneshot;
use ursa_shell::session::{envelope, Error, Options, Reply};

use crate::session::{self, Job};

/// The 8-byte header of a part's frame.
pub(crate) fn part_header(id: u32, index: u16) -> [u8; 8] {
    let mut head = [0u8; 8];
    head[..4].copy_from_slice(&id.to_le_bytes());
    head[4..6].copy_from_slice(&index.to_le_bytes());
    head
}

/// A request read off the socket.
struct Request {
    id: u64,
    op: String,
    args: Value,
    parts: Vec<Vec<u8>>,
}

/// Serve one connection: a session of its own, until the socket closes.
pub(crate) async fn connection(mut socket: WebSocket, options: Options) {
    let Ok(jobs) = session::spawn(options) else {
        let _ = socket.send(Message::Close(None)).await;
        return;
    };
    loop {
        let request = match read_request(&mut socket).await {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err((id, e)) => {
                // A request that is not one: say so, and stop reading this
                // connection, whose framing may be lost.
                let _ = write_reply(&mut socket, id, Err(e)).await;
                return;
            }
        };
        let outcome = if request.op == "memory" {
            Ok(memory())
        } else {
            let (reply, answer) = oneshot::channel();
            let job = Job {
                op: request.op,
                args: request.args,
                parts: request.parts,
                reply,
            };
            match jobs.send(job).await {
                Ok(()) => answer.await.unwrap_or_else(|_| Err(crash())),
                Err(_) => Err(crash()),
            }
        };
        let crashed = matches!(&outcome, Err(e) if e.kind == "crash");
        if write_reply(&mut socket, request.id, outcome).await.is_err() || crashed {
            return;
        }
    }
}

fn crash() -> Error {
    Error {
        kind: "crash",
        message: "the engine stopped".into(),
    }
}

/// The next request, with its parts; `None` once the socket closes.
async fn read_request(socket: &mut WebSocket) -> Result<Option<Request>, (u64, Error)> {
    let text = loop {
        match socket.recv().await {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return Ok(None),
            Some(Ok(Message::Text(text))) => break text,
            Some(Ok(Message::Binary(_))) => {
                return Err((
                    0,
                    Error::protocol("a binary frame with no request before it"),
                ))
            }
            // Pings are answered by the library.
            Some(Ok(_)) => continue,
        }
    };
    let message: Value = serde_json::from_str(&text).map_err(|e| {
        (
            0,
            Error::protocol(format!("a request that is not JSON: {e}")),
        )
    })?;
    let id = message.get("id").and_then(Value::as_u64).unwrap_or(0);
    let bad = |m: &str| (id, Error::protocol(m));
    if message.get("v").and_then(Value::as_u64) != Some(1) {
        return Err(bad("this server speaks protocol version 1"));
    }
    if id > u64::from(u32::MAX) {
        return Err(bad("a request id must fit in 32 bits"));
    }
    let op = message
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("no op"))?;
    let listed = message
        .get("parts")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let mut parts = Vec::with_capacity(listed);
    while parts.len() < listed {
        let frame = match socket.recv().await {
            Some(Ok(Message::Binary(frame))) => frame,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return Ok(None),
            Some(Ok(_)) => return Err(bad("a request's parts must follow it")),
        };
        if frame.len() < 8 || frame[..8] != part_header(id as u32, parts.len() as u16) {
            return Err(bad("a part out of order, or of another request"));
        }
        parts.push(frame[8..].to_vec());
    }
    Ok(Some(Request {
        id,
        op: op.to_string(),
        args: message.get("args").cloned().unwrap_or_else(|| json!({})),
        parts,
    }))
}

/// Write a reply: its JSON, then each part.
async fn write_reply(
    socket: &mut WebSocket,
    id: u64,
    outcome: Result<Reply, Error>,
) -> Result<(), axum::Error> {
    socket
        .send(Message::Text(
            envelope(Some(id), &outcome).to_string().into(),
        ))
        .await?;
    if let Ok(reply) = outcome {
        for (index, part) in reply.parts.into_iter().enumerate() {
            let mut frame = Vec::with_capacity(8 + part.bytes.len());
            frame.extend_from_slice(&part_header(id as u32, index as u16));
            frame.extend_from_slice(&part.bytes);
            socket.send(Message::Binary(frame.into())).await?;
        }
    }
    Ok(())
}

/// The `memory` operation, natively: the process's resident memory, in
/// bytes (the browser demo's is its WebAssembly memory).
fn memory() -> Reply {
    Reply {
        result: json!({"bytes": resident_bytes(), "kind": "rss"}),
        parts: Vec::new(),
    }
}

/// The process's resident set, from `/proc/self/statm` (Linux), else 0.
pub(crate) fn resident_bytes() -> u64 {
    let pages = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok());
    pages.map_or(0, |p| p * 4096)
}
