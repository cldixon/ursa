//! The session's history (`design/console.md` §6, §9): what its clients
//! ran, and who ran each, kept by the session so that every client attached
//! to it sees the same list. Each request that runs something (a query, a
//! fed statement, a graph step) becomes an entry: the `activity` event, sent
//! to every client after the request, and what the `history` op gives a
//! client that joins later.
//!
//! An entry is `{id, at, client, op, summary, ms, result?, rows?, drawing?,
//! nodes?, edges?, error?}`:
//!
//! - `id` counts the session's entries from 1, so a client that asks for
//!   `history` while events arrive can tell which it has;
//! - `at` is when the request started, in milliseconds since the Unix epoch;
//!   `ms` how long it took;
//! - `client` is the `{name, kind}` its client said in `hello` (`null` for
//!   one that said none);
//! - `summary` is what ran, as it would be typed: the SQL of a `query`, the
//!   statement a `feed` completed, `.graph`, `.expand 33`;
//! - `result` is a `query`'s kept result (`"r4"`), or the result a
//!   `graph.open` drew; `rows` a `query`'s row count;
//! - `drawing` numbers the session's drawings: a `graph.open` that drew
//!   starts one, and the graph steps after it (`graph.explore`,
//!   `graph.find`) belong to it, until the next; `nodes` and `edges` are
//!   what the drawing shows after the step;
//! - `error` says why it failed (`cancelled` for a cancelled request).

use std::collections::VecDeque;

use serde_json::{json, Map, Value};
use web_time::{Instant, SystemTime, UNIX_EPOCH};

use super::{Error, Event, Reply};

/// How many entries a session keeps: the oldest go first.
pub const HISTORY_LIMIT: usize = 500;

/// The longest `summary` kept, in bytes: a longer statement is cut, and its
/// entry says `truncated`.
const SUMMARY_MAX: usize = 64 * 1024;

/// The operations that make an entry.
const RECORDED: &[&str] = &[
    "query",
    "feed",
    "sources.attach",
    "graph.open",
    "graph.explore",
    "graph.find",
];

/// A session's history, and the entries not yet sent as `activity`.
#[derive(Debug, Default)]
pub(super) struct History {
    /// Who the next requests come from: `hello`'s `client`, or what the host
    /// set ([`super::Session::set_client`]).
    client: Option<Value>,
    entries: VecDeque<Value>,
    /// The next entry's id.
    next: u64,
    /// Entries to send as `activity` ([`super::Session::take_events`]).
    unsent: Vec<Value>,
    /// The request running now, if it makes an entry: it is still here
    /// when the request was cancelled (its future dropped), and is then
    /// sent as cancelled.
    running: Option<Running>,
    /// The lines of a statement being fed, until it ends.
    fed: String,
    /// The open drawing's number (0: none), and the last one given.
    drawing: u64,
    drawings: u64,
    /// The nodes and edges the open drawing shows, as its last step left it.
    shown: Option<(u64, u64)>,
}

/// A request that makes an entry, while it runs.
#[derive(Debug)]
struct Running {
    op: &'static str,
    summary: String,
    /// The kept result a `graph.open` draws.
    result: Option<String>,
    at: u64,
    start: Instant,
}

impl History {
    /// The client the next requests come from.
    pub(super) fn set_client(&mut self, client: Option<Value>) {
        self.client = client.and_then(|c| clean_client(&c));
    }

    /// The client `hello` names, if it names one well.
    pub(super) fn hello(&mut self, args: &Value) {
        if let Some(client) = args.get("client").and_then(clean_client) {
            self.client = Some(client);
        }
    }

    /// `op` is about to run with `args`.
    pub(super) fn begin(&mut self, op: &str, args: &Value) {
        self.running = None;
        let Some(&op) = RECORDED.iter().find(|&&o| o == op) else {
            if op == "cancelInput" {
                self.fed.clear();
            }
            return;
        };
        let text = |name: &str| args.get(name).and_then(Value::as_str).unwrap_or("");
        let mut result = None;
        let summary = match op {
            "query" => text("sql").trim().to_string(),
            "feed" => {
                if !self.fed.is_empty() {
                    self.fed.push('\n');
                }
                self.fed.push_str(text("line"));
                self.fed.trim().to_string()
            }
            "sources.attach" => {
                let url = text("url").replace('\'', "''");
                match args.get("as").and_then(Value::as_str) {
                    Some(name) => format!("ATTACH '{url}' AS {name};"),
                    None => format!("ATTACH '{url}';"),
                }
            }
            "graph.open" => {
                result = args.get("result").and_then(Value::as_str).map(String::from);
                format!(".graph {}", text("text").trim()).trim().to_string()
            }
            _ => text("command").trim().to_string(),
        };
        self.running = Some(Running {
            op,
            summary,
            result,
            at: now_ms(),
            start: Instant::now(),
        });
    }

    /// The request that began has finished with `outcome`; `drawn` is what
    /// a `graph.open` that drew shows.
    pub(super) fn finish(&mut self, outcome: &Result<Reply, Error>, drawn: Option<(usize, usize)>) {
        let Some(running) = self.running.take() else {
            return;
        };
        let mut entry = Map::new();
        match (running.op, outcome) {
            ("feed", Ok(reply)) => {
                let r = &reply.result;
                if r["pending"].as_bool() == Some(true) {
                    // The statement goes on: the entry waits for its end.
                    return;
                }
                self.fed.clear();
                // A host's command (`.graph`) is the host's to run: its own
                // requests make the entries.
                if r["flow"] == "host" || running.summary.is_empty() {
                    return;
                }
                if let Some(err) = r["err"].as_str().and_then(first_line) {
                    entry.insert("error".into(), json!(err));
                }
            }
            ("query", Ok(reply)) => {
                let r = &reply.result;
                if !r["result"].is_null() {
                    entry.insert("result".into(), r["result"].clone());
                    entry.insert("rows".into(), r["rows"].clone());
                }
            }
            ("graph.open", Ok(_)) => {
                self.drawings += 1;
                self.drawing = self.drawings;
                self.shown = drawn.map(|(n, e)| (n as u64, e as u64));
            }
            ("graph.explore" | "graph.find", Ok(reply)) => {
                let r = &reply.result;
                if let (Some(nodes), Some(edges)) = (r["nodes"].as_u64(), r["edges"].as_u64()) {
                    self.shown = Some((nodes, edges));
                }
                if let Some(err) = r["err"].as_str().and_then(first_line) {
                    entry.insert("error".into(), json!(err));
                }
            }
            (_, Ok(_)) => {}
            (op, Err(e)) => {
                if op == "feed" {
                    self.fed.clear();
                }
                entry.insert("error".into(), json!(e.message));
            }
        }
        if let Some(result) = &running.result {
            entry.insert("result".into(), json!(result));
        }
        // A drawing that failed to open starts none, and is of none.
        let unopened = running.op == "graph.open" && outcome.is_err();
        if running.op.starts_with("graph.") && self.drawing > 0 && !unopened {
            entry.insert("drawing".into(), json!(self.drawing));
            if let Some((nodes, edges)) = self.shown {
                entry.insert("nodes".into(), json!(nodes));
                entry.insert("edges".into(), json!(edges));
            }
        }
        self.record(running, entry);
    }

    /// The open drawing was closed (`graph.close`, `start`).
    pub(super) fn closed_drawing(&mut self) {
        self.drawing = 0;
        self.shown = None;
    }

    /// A new shell (`start`): what was being fed is gone.
    pub(super) fn restarted(&mut self) {
        self.fed.clear();
        self.closed_drawing();
    }

    fn record(&mut self, running: Running, fields: Map<String, Value>) {
        self.next += 1;
        let mut summary = running.summary;
        let truncated = summary.len() > SUMMARY_MAX;
        if truncated {
            let mut cut = SUMMARY_MAX;
            while !summary.is_char_boundary(cut) {
                cut -= 1;
            }
            summary.truncate(cut);
        }
        let mut entry = json!({
            "id": self.next,
            "at": running.at,
            "client": self.client.clone().unwrap_or(Value::Null),
            "op": running.op,
            "summary": summary,
            "ms": running.start.elapsed().as_secs_f64() * 1000.0,
        });
        if truncated {
            entry["truncated"] = json!(true);
        }
        for (k, v) in fields {
            entry[k] = v;
        }
        if self.entries.len() == HISTORY_LIMIT {
            self.entries.pop_front();
        }
        self.entries.push_back(entry.clone());
        self.unsent.push(entry);
    }

    /// The `activity` events to send: a request still running here was
    /// cancelled.
    pub(super) fn take_events(&mut self) -> Vec<Event> {
        if let Some(running) = self.running.take() {
            if running.op == "feed" {
                self.fed.clear();
            }
            self.record(
                running,
                Map::from_iter([("error".into(), json!("cancelled"))]),
            );
        }
        std::mem::take(&mut self.unsent)
            .into_iter()
            .map(|entry| Event {
                name: "activity",
                fields: entry,
            })
            .collect()
    }

    /// The `history` op: the last `limit` entries, oldest first.
    pub(super) fn list(&self, limit: Option<u64>) -> Value {
        let limit = limit.map_or(HISTORY_LIMIT, |l| (l as usize).min(HISTORY_LIMIT));
        let skip = self.entries.len().saturating_sub(limit);
        json!({
            "entries": self.entries.iter().skip(skip).collect::<Vec<_>>(),
            "limit": HISTORY_LIMIT,
        })
    }
}

/// `client` as an entry names it: `{name, kind}`, strings of at most 64
/// characters; anything else is no client.
fn clean_client(client: &Value) -> Option<Value> {
    let field = |name: &str| -> Option<String> {
        let s = client.get(name)?.as_str()?;
        Some(s.chars().take(64).collect())
    };
    let name = field("name")?;
    let kind = field("kind").unwrap_or_else(|| "human".into());
    Some(json!({"name": name, "kind": kind}))
}

/// The first line of what a command printed to its error stream, without
/// colour, or `None` if it printed nothing.
fn first_line(text: &str) -> Option<String> {
    let plain = strip_ansi(text);
    let line = plain.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(line.to_string())
}

/// `text` without its ANSI escape sequences (`ESC [ … letter`).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Now, in milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
