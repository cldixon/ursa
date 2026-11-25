//! A shell session as a client drives it: the session protocol's operations
//! (`design/console.md` §4) as Rust methods, on any target.
//!
//! A [`Session`] owns a [`Shell`], the buffers its output is captured in, and
//! the session's files. Its methods are the operations a page asks for: feed
//! a line, open a graph and change it, inspect a node, export the last
//! result. [`Session::handle`] takes an operation by name, with JSON
//! arguments and binary parts, and gives back the JSON result and the binary
//! parts of the reply, as the protocol carries them.
//!
//! Two hosts run it:
//!
//! - the browser demo, in a Web Worker: `ursa-wasm`'s `WebShell` is
//!   wasm-bindgen glue over [`Session::handle`];
//! - `ursa serve`, natively: `ursa-server` runs it on a thread of its own and
//!   carries requests and replies over a WebSocket.
//!
//! Both speak the same operations through the same code, so a page cannot
//! tell them apart.
//!
//! ```no_run
//! # async fn demo() {
//! use ursa_shell::session::{Options, Session};
//! let mut session = Session::new(Options::default());
//! let reply = session
//!     .handle("feed", serde_json::json!({"line": "SELECT 42 AS answer;"}), Vec::new())
//!     .await
//!     .unwrap();
//! print!("{}", reply.result["out"].as_str().unwrap());
//! # }
//! ```

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow::ipc::writer::StreamWriter;
use datafusion::error::{DataFusionError, Result};
use datafusion::parquet::arrow::ArrowWriter;
use futures::TryStreamExt;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use serde_json::{json, Value};
use url::Url;
use ursa_engine::graph::view::inspect::Inspect;
use web_time::Instant;

use crate::highlight;
use crate::print::{json_array_writer, DelimitedWriter, OutputFormat};
use crate::shell::{Flow, Shell};

/// The protocol's version, which `hello` reports.
pub const PROTOCOL: u32 = 1;

/// How a session is set up.
#[derive(Debug, Clone)]
pub struct Options {
    /// What `.version` and `hello` say.
    pub version: String,
    /// Where the engine runs, as `hello` says it: `"wasm"` or `"native"`.
    pub transport: &'static str,
    /// What this host can do beyond the shell's operations, for `hello`.
    pub capabilities: Vec<&'static str>,
    /// Keep the session's files in memory, at `memory://uploads/`: `addFile`
    /// puts them there, and `COPY … TO 'name'` writes there
    /// (`ursa_engine::session_files`). The browser has no disk, so the demo
    /// always has them.
    pub uploads: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            version: env!("CARGO_PKG_VERSION").to_string(),
            transport: "native",
            capabilities: Vec::new(),
            uploads: true,
        }
    }
}

/// Output the shell writes, kept until an operation takes it.
#[derive(Clone, Default)]
struct Buffer(Rc<RefCell<Vec<u8>>>);

impl Buffer {
    fn take(&self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut *self.0.borrow_mut())).into_owned()
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a binary part of a reply holds (`design/console.md` §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    /// A `GraphResult` or a `GraphDelta`, as `ursa-viz/UGR.md` defines them.
    Ugr,
    /// An Arrow IPC stream.
    Arrow,
    /// A little-endian `f32` array.
    F32,
    /// A little-endian `u32` array.
    U32,
    /// A file.
    Bytes,
}

impl PartKind {
    /// The kind's name on the wire.
    pub fn name(self) -> &'static str {
        match self {
            PartKind::Ugr => "ugr",
            PartKind::Arrow => "arrow",
            PartKind::F32 => "f32",
            PartKind::U32 => "u32",
            PartKind::Bytes => "bytes",
        }
    }
}

/// A binary part of a reply. `name` is the field of the result it fills in
/// for a client that reads the reply as one object (`engine-client.js`).
#[derive(Debug, Clone)]
pub struct Part {
    pub name: &'static str,
    pub kind: PartKind,
    pub bytes: Vec<u8>,
}

impl Part {
    fn f32s(name: &'static str, values: &[f32]) -> Part {
        let bytes = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        Part {
            name,
            kind: PartKind::F32,
            bytes,
        }
    }

    fn u32s(name: &'static str, values: &[u32]) -> Part {
        let bytes = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        Part {
            name,
            kind: PartKind::U32,
            bytes,
        }
    }
}

/// A successful reply: its JSON result, and its binary parts in order.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub result: Value,
    pub parts: Vec<Part>,
}

impl Reply {
    fn json(result: Value) -> Reply {
        Reply {
            result,
            parts: Vec::new(),
        }
    }
}

/// Why a request failed: one of the protocol's error kinds, and the message
/// to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// `sql`, `engine`, `not_found`, `protocol` (`cancelled` and `crash` are
    /// the host's).
    pub kind: &'static str,
    pub message: String,
}

impl Error {
    pub fn protocol(message: impl Into<String>) -> Error {
        Error {
            kind: "protocol",
            message: message.into(),
        }
    }
}

impl From<DataFusionError> for Error {
    fn from(e: DataFusionError) -> Error {
        let kind = match e.find_root() {
            DataFusionError::SQL(..)
            | DataFusionError::Plan(_)
            | DataFusionError::SchemaError(..)
            | DataFusionError::NotImplemented(_) => "sql",
            _ => "engine",
        };
        Error {
            kind,
            message: ursa_engine::user_message(&e),
        }
    }
}

/// A reply or an error as the protocol's JSON (`design/console.md` §4.1),
/// with `id` when the transport numbers requests: `{"id", "ok": true,
/// "result", "parts": [{"name", "kind", "bytes"}]}`, or `{"id", "ok": false,
/// "error": {"kind", "message"}}`. The parts' bytes travel apart from it.
pub fn envelope(id: Option<u64>, outcome: &std::result::Result<Reply, Error>) -> Value {
    let mut message = match outcome {
        Ok(reply) => json!({
            "ok": true,
            "result": reply.result,
            "parts": reply.parts.iter().map(|p| json!({
                "name": p.name,
                "kind": p.kind.name(),
                "bytes": p.bytes.len(),
            })).collect::<Vec<_>>(),
        }),
        Err(e) => json!({"ok": false, "error": {"kind": e.kind, "message": e.message}}),
    };
    if let Some(id) = id {
        message["id"] = json!(id);
    }
    message
}

/// What a fed line printed, and what the host does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fed {
    pub out: String,
    pub err: String,
    /// `continue`, `exit` (`.quit`) or `host` (a command the host added, in
    /// [`Fed::command`]).
    pub flow: &'static str,
    pub command: Option<String>,
}

/// A graph for the host's view ([`Session::open_graph`]).
#[derive(Debug, Clone, PartialEq)]
pub struct GraphData {
    /// The graph result as UGR bytes (`ursa-viz/UGR.md`).
    pub ugr: Vec<u8>,
    /// Rows left out because an endpoint is NULL.
    pub skipped: usize,
    /// Where `POSITIONS (query)` puts the nodes: every `x`, then every `y`.
    pub positions: Option<Vec<f32>>,
}

/// What an explore command did ([`Session::explore`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explored {
    pub out: String,
    pub err: String,
    /// The change as a UGR delta; empty when nothing changed.
    pub delta: Vec<u8>,
    /// How many nodes and edges are shown after the command.
    pub nodes: u32,
    pub edges: u32,
}

/// What `.find` found ([`Session::find`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub out: String,
    pub err: String,
    /// The matches the view shows, as its node indexes, the best first.
    pub selected: Vec<u32>,
    pub matches: u32,
    /// The first matches the view does not show: `[{"key", "hidden"}]`.
    pub not_shown: Value,
    pub not_shown_total: u32,
}

/// The shell as one client drives it. See the module's documentation.
pub struct Session {
    shell: Shell<Buffer, Buffer>,
    out: Buffer,
    err: Buffer,
    /// The host's width in characters, which tables fit.
    width: Rc<Cell<usize>>,
    /// The session's files, at `memory://uploads/`, if it has them.
    uploads: Option<Arc<InMemory>>,
    options: Options,
    /// The console's results ([`Self::query`]), the oldest first.
    results: VecDeque<(String, SchemaRef, Vec<RecordBatch>)>,
    next_result: u64,
}

/// How many of the console's results a session keeps (design §12, question
/// 3: a guess the spike keeps).
const KEPT_RESULTS: usize = 16;

impl Session {
    /// A session over a new engine session, drawing tables in colour for a
    /// screen 80 characters wide until [`Self::set_width`] says otherwise.
    pub fn new(options: Options) -> Session {
        let ctx = ursa_engine::new_session();
        let uploads = options.uploads.then(|| {
            let store = Arc::new(InMemory::new());
            ctx.register_object_store(&uploads_url(), Arc::clone(&store) as Arc<dyn ObjectStore>);
            store
        });
        let (out, err) = (Buffer::default(), Buffer::default());
        let width = Rc::new(Cell::new(80));
        let mut shell = Shell::new(ctx, OutputFormat::Table, out.clone(), err.clone());
        let screen = Rc::clone(&width);
        shell.set_screen(Box::new(move || screen.get()), true);
        shell.set_error_color(true);
        shell.keep_last_result();
        shell.add_explore_commands();
        shell.set_version(&options.version);
        Session {
            shell,
            out,
            err,
            width,
            uploads,
            options,
            results: VecDeque::new(),
            next_result: 0,
        }
    }

    /// Run the operation `op` with its JSON `args` and binary `parts`
    /// (`design/console.md` §4.3). The operations:
    ///
    /// | op | args | result, parts |
    /// |---|---|---|
    /// | `hello` | `client?` | `{protocol, version, transport, capabilities}` |
    /// | `start` | `commands?`, `shortcuts?`, `graphMaxEdges?` | a fresh session: `{version, prompt, promptColor}` |
    /// | `feed` | `line` | `{out, err, flow, command, prompt, continuation, pending}` |
    /// | `cancelInput` | — | drops a half-typed statement: `{prompt}` |
    /// | `width` | `columns` | `{}` |
    /// | `highlightAnsi` | `text`, `alone?` | `{text}` |
    /// | `addFile` | `name`; the file as a part | `{url}` |
    /// | `files` | — | `{files: [{name, url, bytes}]}` |
    /// | `export` | `format` | `{}`; `bytes`: the last result |
    /// | `graph.open` | `text` | `{skipped}`; `ugr`, and `positions` (`f32`) with `POSITIONS` |
    /// | `graph.explore` | `command` | `{out, err, nodes, edges, delta: null}`; `delta` (`ugr`) if it changed |
    /// | `graph.find` | `command` | `{out, err, matches, notShown, notShownTotal}`; `selected` (`u32`) |
    /// | `graph.notShown` | — | `counts` (`u32`) |
    /// | `graph.inspect` | `node, key, neighbours?` or `edge, src, dst` | inspect's JSON, with `ms` |
    /// | `graph.close` | — | `{}` |
    /// | `query` | `sql`, `rows?` (1,000) | `{result, columns, rows, more, ms, notices}`; `arrow`: the first `rows` rows |
    ///
    /// `graph.open` takes `result` (an id `query` gave) in place of the last
    /// result.
    ///
    /// SQL and command errors are what `feed`, `graph.explore` and
    /// `graph.find` print; the other operations fail with an [`Error`].
    pub async fn handle(
        &mut self,
        op: &str,
        args: Value,
        mut parts: Vec<Vec<u8>>,
    ) -> std::result::Result<Reply, Error> {
        let a = Args(&args);
        match op {
            "hello" => Ok(Reply::json(json!({
                "protocol": PROTOCOL,
                "version": self.options.version,
                "transport": self.options.transport,
                "capabilities": self.options.capabilities,
            }))),
            "start" => {
                *self = Session::new(self.options.clone());
                if let Some(commands) = args.get("commands").and_then(Value::as_array) {
                    for command in commands {
                        let field = |i: usize| command.get(i).and_then(Value::as_str).unwrap_or("");
                        self.shell.add_host_command(field(0), field(1), field(2));
                    }
                }
                if let Some(shortcuts) = a.str_opt("shortcuts") {
                    self.shell.set_shortcuts(shortcuts);
                }
                // The page's default for the edges a view draws, where it is
                // not the engine's (a phone, #502): a SET like any other,
                // which RESET undoes.
                if let Some(max) = args.get("graphMaxEdges").and_then(Value::as_u64) {
                    self.feed(&format!("SET ursa.graph_max_edges = {max};"))
                        .await;
                }
                Ok(Reply::json(json!({
                    "version": self.options.version,
                    "prompt": self.shell.prompt(),
                    "promptColor": [highlight::PROMPT, highlight::RESET],
                })))
            }
            "feed" => {
                let fed = self.feed(a.str("line")?).await;
                Ok(Reply::json(json!({
                    "out": fed.out,
                    "err": fed.err,
                    "flow": fed.flow,
                    "command": fed.command,
                    "prompt": self.shell.prompt(),
                    "continuation": self.shell.continuation(),
                    "pending": self.shell.is_pending(),
                })))
            }
            "cancelInput" => {
                self.shell.cancel_input();
                Ok(Reply::json(json!({"prompt": self.shell.prompt()})))
            }
            "width" => {
                let columns = args.get("columns").and_then(Value::as_u64);
                let columns = columns.ok_or_else(|| Error::protocol("width needs columns"))?;
                self.set_width(columns as usize);
                Ok(Reply::json(json!({})))
            }
            "highlightAnsi" => {
                let text = a.str("text")?;
                let alone = args.get("alone").and_then(Value::as_bool).unwrap_or(false);
                Ok(Reply::json(json!({"text": self.highlight(text, alone)})))
            }
            "addFile" => {
                if parts.len() != 1 {
                    return Err(Error::protocol("addFile needs the file as one part"));
                }
                let url = self.add_file(a.str("name")?, parts.remove(0)).await?;
                Ok(Reply::json(json!({"url": url})))
            }
            "files" => Ok(Reply::json(json!({"files": self.files().await?}))),
            "export" => {
                let bytes = self.export(a.str("format")?)?;
                Ok(Reply {
                    result: json!({}),
                    parts: vec![Part {
                        name: "bytes",
                        kind: PartKind::Bytes,
                        bytes,
                    }],
                })
            }
            "graph.open" => {
                if let Some(id) = a.str_opt("result") {
                    let (schema, batches) = self.result(id)?;
                    self.shell.set_last_result(schema, batches);
                }
                let graph = self.open_graph(a.str_opt("text").unwrap_or("")).await?;
                let mut parts = vec![Part {
                    name: "ugr",
                    kind: PartKind::Ugr,
                    bytes: graph.ugr,
                }];
                if let Some(positions) = &graph.positions {
                    parts.push(Part::f32s("positions", positions));
                }
                Ok(Reply {
                    result: json!({"skipped": graph.skipped}),
                    parts,
                })
            }
            "graph.explore" => {
                let e = self.explore(a.str("command")?).await;
                let parts = if e.delta.is_empty() {
                    Vec::new()
                } else {
                    vec![Part {
                        name: "delta",
                        kind: PartKind::Ugr,
                        bytes: e.delta,
                    }]
                };
                Ok(Reply {
                    result: json!({
                        "out": e.out, "err": e.err, "nodes": e.nodes, "edges": e.edges, "delta": null,
                    }),
                    parts,
                })
            }
            "graph.find" => {
                let f = self.find(a.str("command")?).await;
                Ok(Reply {
                    result: json!({
                        "out": f.out,
                        "err": f.err,
                        "matches": f.matches,
                        "notShown": f.not_shown,
                        "notShownTotal": f.not_shown_total,
                    }),
                    parts: vec![Part::u32s("selected", &f.selected)],
                })
            }
            "graph.notShown" => Ok(Reply {
                result: json!({}),
                parts: vec![Part::u32s("counts", &self.not_shown().await?)],
            }),
            "graph.inspect" => {
                let start = Instant::now();
                let mut found = if let Some(edge) = args.get("edge").and_then(Value::as_u64) {
                    let what = Inspect::Edge {
                        edge: edge as u32,
                        src: a.str("src")?,
                        dst: a.str("dst")?,
                    };
                    self.inspect(what, false).await?
                } else {
                    let node = args.get("node").and_then(Value::as_u64);
                    let node =
                        node.ok_or_else(|| Error::protocol("inspect needs a node or an edge"))?;
                    let what = Inspect::Node {
                        node: node as u32,
                        key: a.str("key")?,
                    };
                    let neighbours = args
                        .get("neighbours")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    self.inspect(what, neighbours).await?
                };
                found["ms"] = json!(start.elapsed().as_secs_f64() * 1000.0);
                Ok(Reply::json(found))
            }
            "graph.close" => {
                self.close_graph();
                Ok(Reply::json(json!({})))
            }
            "query" => {
                let rows = args.get("rows").and_then(Value::as_u64).unwrap_or(1_000);
                let (result, arrow) = self.query(a.str("sql")?, rows as usize).await?;
                Ok(Reply {
                    result,
                    parts: vec![Part {
                        name: "arrow",
                        kind: PartKind::Arrow,
                        bytes: arrow,
                    }],
                })
            }
            other => Err(Error::protocol(format!("unknown op {other}"))),
        }
    }

    /// Run `sql` (one or more statements) for the console: the last
    /// statement's result is kept under a new id, and becomes the shell's
    /// last result (`.graph`, `export`). Gives `{result, columns, metadata,
    /// rows, more, ms, notices}`, where `columns` are `[{name, type, nullable,
    /// metadata}]` and `rows` the whole result's count, and the first
    /// `rows` rows as an Arrow IPC stream (with the schema's metadata).
    pub async fn query(&mut self, sql: &str, rows: usize) -> Result<(Value, Vec<u8>)> {
        let start = Instant::now();
        let ctx = self.shell.context().clone();
        let mut last = None;
        for (statement, text) in ursa_sql::parse_sql_with_text_for(&ctx, sql)? {
            let frame = ursa_sql::execute_with_text(&ctx, statement, &text).await?;
            let schema: SchemaRef = Arc::clone(frame.schema().inner());
            let batches = frame.collect().await?;
            last = Some((schema, batches));
        }
        let (schema, batches) =
            last.ok_or_else(|| DataFusionError::Plan("no statement to run".into()))?;
        let total: usize = batches.iter().map(RecordBatch::num_rows).sum();
        let mut first = Vec::new();
        let mut left = rows;
        for batch in &batches {
            if left == 0 {
                break;
            }
            let take = left.min(batch.num_rows());
            first.push(batch.slice(0, take));
            left -= take;
        }
        let arrow = encode(&schema, &first, "arrow")?;
        let columns: Vec<Value> = schema
            .fields()
            .iter()
            .map(|f| {
                json!({
                    "name": f.name(),
                    "type": f.data_type().to_string(),
                    "nullable": f.is_nullable(),
                    "metadata": f.metadata(),
                })
            })
            .collect();
        let notices: Vec<&str> = if columns.is_empty() {
            vec!["OK"]
        } else {
            vec![]
        };
        self.next_result += 1;
        let id = format!("r{}", self.next_result);
        if !columns.is_empty() {
            self.shell
                .set_last_result(Arc::clone(&schema), batches.clone());
        }
        self.results
            .push_back((id.clone(), Arc::clone(&schema), batches));
        while self.results.len() > KEPT_RESULTS {
            self.results.pop_front();
        }
        let result = json!({
            "result": id,
            "columns": columns,
            // The schema's own metadata: `ursa.pattern`, for GRAPH_TABLE.
            "metadata": schema.metadata(),
            "rows": total,
            "more": total > rows,
            "ms": start.elapsed().as_secs_f64() * 1000.0,
            "notices": notices,
        });
        Ok((result, arrow))
    }

    /// A result [`Self::query`] kept.
    fn result(&self, id: &str) -> std::result::Result<(SchemaRef, Vec<RecordBatch>), Error> {
        let kept = self.results.iter().find(|(kept, ..)| kept == id);
        let (_, schema, batches) = kept.ok_or_else(|| Error {
            kind: "not_found",
            message: format!("no result {id}: the session keeps the last {KEPT_RESULTS}"),
        })?;
        Ok((Arc::clone(schema), batches.clone()))
    }

    /// The table width: tables are drawn no wider.
    pub fn set_width(&self, columns: usize) {
        self.width.set(columns);
    }

    /// Run one typed line: a dot-command, or a line of SQL, which runs once
    /// the statement ends with `;`.
    pub async fn feed(&mut self, line: &str) -> Fed {
        let (flow, command) = match self.shell.feed(line).await {
            Flow::Continue => ("continue", None),
            Flow::Exit => ("exit", None),
            Flow::Host(command) => ("host", Some(command)),
        };
        Fed {
            out: self.out.take(),
            err: self.err.take(),
            flow,
            command,
        }
    }

    /// `text` coloured as the shell colours what is typed (ANSI): as the next
    /// line of what is pending, or with `alone`, as a statement of its own.
    pub fn highlight(&self, text: &str, alone: bool) -> String {
        if alone {
            highlight::ansi(text)
        } else {
            self.shell.highlight_line(text)
        }
    }

    /// Open a graph to draw (the host's `.graph TEXT`): the shell's open
    /// graph from now on. A rejection leaves the one that was open.
    pub async fn open_graph(&mut self, text: &str) -> Result<GraphData> {
        let (skipped, positions) = self.shell.open_graph_placed(text).await?;
        // The bytes are written from the shell's own result: it is not
        // copied first (8 bytes an edge).
        let result = self.shell.explorer().expect("a graph was opened").result();
        Ok(GraphData {
            ugr: result.to_bytes(),
            skipped,
            positions: positions.map(|mut p| {
                p.x.append(&mut p.y);
                p.x
            }),
        })
    }

    /// Run an explore command on the open graph (`.expand`, `.hide`,
    /// `.show`, `.undo`, `.redo`, `.graph history`), and take what it
    /// printed.
    pub async fn explore(&mut self, command: &str) -> Explored {
        let delta = self.shell.explore(command).await;
        let (nodes, edges) = self.shell.explorer().map_or((0, 0), |s| s.shown());
        Explored {
            out: self.out.take(),
            err: self.err.take(),
            delta: delta.map(|d| d.to_bytes()).unwrap_or_default(),
            nodes: nodes as u32,
            edges: edges as u32,
        }
    }

    /// Run `.find` on the open graph, and take what it printed.
    pub async fn find(&mut self, command: &str) -> Found {
        let found = self.shell.find(command).await.unwrap_or_default();
        let not_shown: Vec<Value> = found
            .not_shown
            .iter()
            .map(|n| json!({"key": n.key, "hidden": n.hidden}))
            .collect();
        Found {
            out: self.out.take(),
            err: self.err.take(),
            selected: found.selected,
            matches: found.matches as u32,
            not_shown: Value::Array(not_shown),
            not_shown_total: found.not_shown_total as u32,
        }
    }

    /// For each node slot of the open graph, how many of its edges in the
    /// property graph lead to something not shown: the view's `+N`.
    pub async fn not_shown(&mut self) -> Result<Vec<u32>> {
        self.shell.not_shown().await
    }

    /// What the open graph's tables say of a node or an edge, for the
    /// host's inspector, as JSON:
    ///
    /// ```json
    /// {"table": "members", "sql": "SELECT * FROM members WHERE id = 33",
    ///  "columns": [{"name": "id", "numeric": true}, {"name": "club", "numeric": false}],
    ///  "rows": [["33", "Officer"]], "row": null, "note": null, "indexed": false}
    /// ```
    ///
    /// With `neighbours` (a node only), instead `{"neighbours": 17}`: how
    /// many it has in the whole graph (`null` with no property graph).
    /// Nothing is printed and the last result stays.
    pub async fn inspect(&self, what: Inspect<'_>, neighbours: bool) -> Result<Value> {
        if let (true, Inspect::Node { node, .. }) = (neighbours, what) {
            let count = self.shell.graph_neighbours(node).await?;
            return Ok(json!({ "neighbours": count }));
        }
        let found = self.shell.inspect(what).await?;
        let columns: Vec<_> = found
            .columns
            .iter()
            .map(|c| json!({ "name": c.name, "numeric": c.numeric }))
            .collect();
        Ok(json!({
            "table": found.table,
            "sql": found.sql,
            "columns": columns,
            "rows": found.rows,
            "row": found.row,
            "note": found.note,
            "indexed": found.indexed,
        }))
    }

    /// Close the open graph (the host closed its view).
    pub fn close_graph(&mut self) {
        self.shell.close_graph();
    }

    /// The last result that had columns, every row, written as `format`:
    /// `csv`, `json`, `arrow` or `parquet`.
    pub fn export(&self, format: &str) -> Result<Vec<u8>> {
        let (schema, batches) = self.shell.last_result().ok_or_else(|| {
            DataFusionError::Plan("no result to export yet; run a query first".into())
        })?;
        encode(schema, batches, format)
    }

    /// Put a file from the host in the session's files, and give the URL
    /// SQL reads it by: `memory://uploads/<name>`.
    pub async fn add_file(&self, name: &str, bytes: Vec<u8>) -> Result<String> {
        add_upload(self.uploads()?, name, bytes).await
    }

    /// The session's files, `[{"name", "url", "bytes"}]` in name order: the
    /// ones the host added and the ones `COPY … TO 'name'` wrote. None in a
    /// session without files.
    pub async fn files(&self) -> Result<Value> {
        match &self.uploads {
            Some(uploads) => list_uploads(uploads).await,
            None => Ok(json!([])),
        }
    }

    fn uploads(&self) -> Result<&InMemory> {
        self.uploads
            .as_deref()
            .ok_or_else(|| DataFusionError::Plan("this session keeps no files".into()))
    }
}

/// An operation's arguments, read by name.
struct Args<'a>(&'a Value);

impl<'a> Args<'a> {
    fn str(&self, name: &str) -> std::result::Result<&'a str, Error> {
        self.str_opt(name)
            .ok_or_else(|| Error::protocol(format!("missing string argument '{name}'")))
    }

    fn str_opt(&self, name: &str) -> Option<&'a str> {
        self.0.get(name).and_then(Value::as_str)
    }
}

/// Where the session's files live: `memory://uploads/`. The engine reads a
/// file there by its name, and `COPY … TO 'name'` writes one
/// (`ursa_engine::session_files`).
pub fn uploads_url() -> Url {
    Url::parse(ursa_engine::session_files::URL).expect("a valid URL")
}

/// `batches` written as `format`: `csv`, `json`, `arrow` (an IPC stream) or
/// `parquet`.
pub fn encode(schema: &SchemaRef, batches: &[RecordBatch], format: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    match format {
        "csv" => {
            let mut writer = DelimitedWriter::new(&mut bytes, b',', schema)?;
            for batch in batches {
                writer.write(batch)?;
            }
        }
        "json" => {
            let mut writer = json_array_writer(&mut bytes);
            for batch in batches {
                writer.write(batch)?;
            }
            writer.finish()?;
        }
        "arrow" => {
            let mut writer = StreamWriter::try_new(&mut bytes, schema)?;
            for batch in batches {
                writer.write(batch)?;
            }
            writer.finish()?;
        }
        "parquet" => {
            let mut writer = ArrowWriter::try_new(&mut bytes, Arc::clone(schema), None)?;
            for batch in batches {
                writer.write(batch)?;
            }
            writer.close()?;
        }
        other => {
            return Err(DataFusionError::Plan(format!(
                "cannot export as '{other}'; use csv, json, arrow or parquet"
            )))
        }
    }
    Ok(bytes)
}

/// Put `bytes` at `name` in `uploads`; the URL SQL reads it by.
pub async fn add_upload(uploads: &InMemory, name: &str, bytes: Vec<u8>) -> Result<String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return Err(DataFusionError::Plan(format!(
            "'{name}' is not a file name to add; give a name with no path, such as edges.csv"
        )));
    }
    let key = upload_key(name);
    uploads
        .put(&ObjectPath::from(key.as_str()), PutPayload::from(bytes))
        .await
        .map_err(|e| DataFusionError::External(Box::new(e)))?;
    Ok(format!("{}{key}", ursa_engine::session_files::URL))
}

/// The files in `uploads`, as [`Session::files`] gives them.
pub async fn list_uploads(uploads: &InMemory) -> Result<Value> {
    let mut files: Vec<_> = uploads
        .list(None)
        .try_collect()
        .await
        .map_err(|e| DataFusionError::External(Box::new(e)))?;
    files.sort_by(|a, b| a.location.as_ref().cmp(b.location.as_ref()));
    Ok(files
        .iter()
        .map(|f| {
            let name = f.location.as_ref();
            json!({
                "name": name,
                "url": format!("{}{name}", ursa_engine::session_files::URL),
                "bytes": f.size,
            })
        })
        .collect())
}

/// The name a file is kept under: its name with every character but ASCII
/// letters, digits, `.`, `_` and `-` made `_`, so its URL reads back as it is
/// written. A URL decodes `%`, ends its path at `#` and `?`, and globs at `*`,
/// `?` and `[`; and non-ASCII names are percent-encoded on the way in. A name
/// that changes also gets a hash of the original before its extension, so two
/// files (`a#b.csv`, `a?b.csv`) are never kept as one.
fn upload_key(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe == name {
        return safe;
    }
    // FNV-1a: stable across builds, unlike the standard library's hasher.
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    let tag = format!("-{:08x}", hash as u32);
    match safe.rfind('.').filter(|&i| i > 0) {
        Some(dot) => format!("{}{tag}{}", &safe[..dot], &safe[dot..]),
        None => safe + &tag,
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
