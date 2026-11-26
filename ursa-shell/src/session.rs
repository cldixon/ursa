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
//! session.handle("hello", serde_json::json!({"protocol": 1}), Vec::new()).await.unwrap();
//! let reply = session
//!     .handle("feed", serde_json::json!({"line": "SELECT 42 AS answer;"}), Vec::new())
//!     .await
//!     .unwrap();
//! print!("{}", reply.result["out"].as_str().unwrap());
//! # }
//! ```

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;

use arrow::array::{ArrayData, RecordBatch};
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
use ursa_engine::graph::view::{self, inspect::Inspect};
use web_time::Instant;

use crate::highlight;
use crate::print::{json_array_writer, DelimitedWriter, OutputFormat};
use crate::shell::{Flow, Shell};

pub use ursa_engine::paths::Root;

/// The protocol's version: what `hello` reports, and the only one it
/// accepts.
pub const PROTOCOL: u32 = 1;

/// One operation of the protocol, as the protocol's document lists it
/// (`docs/reference/protocol.md`, generated from [`OPS`]). Each field is
/// Markdown for a table cell.
#[derive(Debug, Clone, Copy)]
pub struct Op {
    /// The operation's name, as a request gives it in `op`.
    pub name: &'static str,
    /// Its arguments; `?` marks an optional one.
    pub args: &'static str,
    /// Its result, then its binary parts by name and kind.
    pub result: &'static str,
    /// What it does.
    pub about: &'static str,
}

/// Every operation [`Session::handle`] runs, in the protocol document's
/// order. `handle` refuses any other name, and a test runs each of these
/// through it and checks the document against this list, so the three agree.
pub const OPS: &[Op] = &[
    Op {
        name: "hello",
        args: "`protocol`, `client?: {name, kind}`, `session?: \"new\" \\| \"shared\"`",
        result: "`{protocol, version, transport, capabilities, session: {kind}}`",
        about: "Must be the first request: any other before it fails with `protocol`. \
                Refuses a `protocol` other than 1. `session` asks the host for a session \
                of its own (`new`, the shell's) or the one it shares (`shared`, the \
                default: the console's and agents')",
    },
    Op {
        name: "cancel",
        args: "`target`",
        result: "`{}`",
        about: "Stops the running request `target`, which then fails with `cancelled`. \
                A host with capability `cancel` answers it ahead of the queue; a request \
                that reaches the session has nothing running, so it answers `{}`",
    },
    Op {
        name: "history",
        args: "`limit?` (500)",
        result: "`{entries: [{id, at, client, op, summary, ms, result?, rows?, drawing?, nodes?, edges?, error?}], limit}`",
        about: "What the session's clients ran, oldest first: the last `limit` of the \
                entries the `activity` event sent. The session keeps its last 500 (`limit` \
                in the result); a client that joins late reads what came before here, and \
                its `id`s tell it which events it has already",
    },
    Op {
        name: "start",
        args: "`commands?`, `shortcuts?`, `graphMaxEdges?`",
        result: "`{version, prompt, promptColor}`",
        about: "A fresh shell, with the host's dot-commands (`[[name, args, help]]`), \
                its `.help shortcuts` text and its default `ursa.graph_max_edges`",
    },
    Op {
        name: "feed",
        args: "`line`",
        result: "`{out, err, flow, command, prompt, continuation, pending}`",
        about: "Runs one typed line, as the CLI does. `flow` is `continue`, `exit` \
                (`.quit`) or `host` (a host's dot-command, in `command`)",
    },
    Op {
        name: "cancelInput",
        args: "—",
        result: "`{prompt}`",
        about: "Drops a half-typed statement (Ctrl-C at the prompt); \
                `cancel` stops a running request",
    },
    Op {
        name: "width",
        args: "`columns`",
        result: "`{}`",
        about: "The terminal's width: tables are drawn no wider (0: no limit)",
    },
    Op {
        name: "highlightAnsi",
        args: "`text`, `alone?`",
        result: "`{text}`",
        about: "`text` coloured as the shell colours what is typed: after what is \
                pending, or with `alone`, as a statement of its own",
    },
    Op {
        name: "memory",
        args: "—",
        result: "`{bytes, kind: \"wasm\" \\| \"rss\"}`",
        about: "The engine's memory: its WebAssembly memory in the browser, the \
                process's resident set natively (`bytes` is `null` where it cannot be read)",
    },
    Op {
        name: "query",
        args: "`sql`, `rows?` (1,000)",
        result: "`{result, columns, metadata, rows, more, ms, notices, graph}`; `arrow` (`arrow`)",
        about: "Runs `sql` for the console. The last statement's result is kept as \
                `result` (`\"r4\"`; `null` for a statement with no columns) for `fetch`, \
                `export` and `graph.open`, and becomes the last result too. `columns` are \
                `[{name, type, nullable, metadata}]`, `metadata` is the schema's \
                (`ursa.pattern`), `rows` the whole result's count, `graph` the rule \
                `graph.open` would draw it by (`\"nodes\"`, `\"edges\"`, `\"path\"`, \
                `\"rings\"`, `\"pattern\"`), read from its columns, or `null` for a \
                result that is not a graph; the part holds the \
                first `rows` rows as an Arrow IPC stream. The session keeps its last 16 \
                results, within 256 MB, and drops the oldest first; a result over 256 MB \
                on its own keeps only the rows the reply sent",
    },
    Op {
        name: "explain",
        args: "`sql`",
        result: "`{plan, ms}`",
        about: "The plan of `sql`'s last statement, as `EXPLAIN` prints it: the logical \
                plan, then the physical plan, as text. Nothing is run, the statements \
                before the last included",
    },
    Op {
        name: "fetch",
        args: "`result`, `offset?` (0), `rows?` (1,000)",
        result: "`{rows, more}`; `arrow` (`arrow`)",
        about: "A page of a kept result: `rows` rows from `offset`, as an Arrow IPC \
                stream, and whether more follow. `not_found` for a result closed or \
                dropped, or for rows past those a result over the cap kept: run the \
                query again",
    },
    Op {
        name: "close",
        args: "`result`",
        result: "`{}`",
        about: "Drops a kept result; `not_found` if it has gone already",
    },
    Op {
        name: "addFile",
        args: "`name`; the file as the request's one part",
        result: "`{url}`",
        about: "Puts a file in the session's files, at `memory://uploads/`",
    },
    Op {
        name: "files",
        args: "—",
        result: "`{files: [{name, url, bytes}]}`",
        about: "The session's files: added ones, and those `COPY … TO 'name'` wrote",
    },
    Op {
        name: "export",
        args: "`format`, `result?`",
        result: "`{}`; `bytes` (`bytes`)",
        about: "A kept result, or without `result` the last result, every row, as \
                `csv`, `json`, `arrow` or `parquet`. `not_found` for a result that has \
                gone, or that kept only its first rows",
    },
    Op {
        name: "catalog",
        args: "—",
        result: "`{databases: [{name, schemas: [{name, tables: [{name, kind, columns: [{name, type, nullable}]}]}]}], graphs: [{name, vertices, edges}]}`",
        about: "What the session has defined, read from its catalog without running a query. \
                A database is one of the session's catalogs; `kind` is `table`, `view` or \
                `external` (a table of files); `information_schema` and the graphs' own \
                schemas (`<graph>.nodes`) are left out. A graph's `vertices` are \
                `[{name, table, key}]` and its `edges` `[{name, table, source, destination}]`; \
                a graph of the database that could not be defined has none, and its `error`. \
                The `catalog` event says when this has changed",
    },
    Op {
        name: "sources.list",
        args: "`url`",
        result: "`{entries: [{name, url, kind: \"dir\" \\| \"file\", format, bytes}]}`",
        about: "Capability `sources`. The entries of a directory under the session's root \
                (a path relative to it, or a `file://` URL) or of a bucket prefix (`s3://`, \
                `gs://`, `az://`), directories first, then by name. `format` is `parquet`, \
                `csv`, `json`, `arrow`, `ursa` or `null`, from the extension; `bytes` is \
                `null` for a directory. A path outside the root, there or through a \
                symbolic link, fails with `protocol`; hidden files, and entries whose links \
                lead outside, are left out. URLs carry no credentials",
    },
    Op {
        name: "sources.attach",
        args: "`url`, `as?`",
        result: "`{}`",
        about: "Capability `sources`. Runs `ATTACH 'url' [AS name]`, as a statement would \
                (a `file://` URL as its path)",
    },
    Op {
        name: "complete",
        args: "`sql`, `cursor`",
        result: "`{from, to, items: [{label, kind, detail}]}`",
        about: "Completions for the word at `cursor`, which replace `from`..`to`: after \
                `t.`, the columns of the table `t` names or aliases (or the tables of the \
                schema `t`); else the statement's tables' columns, tables and views, \
                property graphs, functions (DataFusion's and the graph functions) and \
                keywords; at the start of a line, dot-commands. `kind` is `column`, \
                `table`, `view`, `external`, `schema`, `graph`, `function`, `keyword` or \
                `command`. Offsets are UTF-16 code units, as JavaScript counts them. Runs \
                no query",
    },
    Op {
        name: "highlight",
        args: "`sql`",
        result: "`{spans: [{from, to, kind}]}`",
        about: "The shell's colouring of `sql` (`ursa_shell::highlight::pieces`) as \
                spans: `kind` is `keyword`, `string`, `number`, `comment`, `command` or \
                `arguments`; plain text has none. Offsets are UTF-16 code units",
    },
    Op {
        name: "graph.open",
        args: "`text?`, `result?`",
        result: "`{skipped}`; `ugr` (`ugr`), `positions?` (`f32`)",
        about: "Opens a graph to draw (`.graph TEXT`). A `text` that names no source \
                draws the kept `result`, or without it the last result. `positions`, \
                every `x` then every `y`, only with `POSITIONS`",
    },
    Op {
        name: "graph.explore",
        args: "`command`",
        result: "`{out, err, nodes, edges, delta: null}`; `delta?` (`ugr`)",
        about: "Runs `.expand`, `.hide`, `.show`, `.undo`, `.redo` or `.graph \
                history` on the open graph; `delta` only when the graph changed",
    },
    Op {
        name: "graph.find",
        args: "`command`",
        result: "`{out, err, matches, notShown, notShownTotal}`; `selected` (`u32`)",
        about: "Runs `.find`: the matches shown, as node slots, the best first",
    },
    Op {
        name: "graph.notShown",
        args: "—",
        result: "`{}`; `counts` (`u32`)",
        about: "For each node slot, its edges to nodes not shown: the view's `+N`",
    },
    Op {
        name: "graph.inspect",
        args: "`node, key, neighbours?` or `edge, src, dst`",
        result: "inspect's JSON, with `ms`",
        about: "What the graph's tables say of a node or an edge",
    },
    Op {
        name: "graph.close",
        args: "—",
        result: "`{}`",
        about: "Closes the open graph",
    },
];

/// One event of the protocol (`docs/reference/protocol.md`, generated from
/// [`EVENTS`]): `{"event": name, ...fields}`, sent unprompted.
#[derive(Debug, Clone, Copy)]
pub struct EventSpec {
    /// The event's name, as `event` gives it.
    pub name: &'static str,
    /// Its other fields.
    pub fields: &'static str,
    /// When the session sends it.
    pub when: &'static str,
}

/// Every event a session sends, in the protocol document's order.
pub const EVENTS: &[EventSpec] = &[
    EventSpec {
        name: "catalog",
        fields: "—",
        when: "After a request that changed what `catalog` answers: `CREATE`, `DROP`, \
               `ALTER`, `ATTACH`, `CREATE PROPERTY GRAPH`, `.open`, a loaded dataset, a \
               `start`, through `feed`, `query` or `sources.attach`. Every client attached to \
               the session gets it, before the reply to the request that caused it; clients \
               fetch `catalog` again",
    },
    EventSpec {
        name: "activity",
        fields: "`id`, `at`, `client`, `op`, `summary`, `ms`, `result?`, `rows?`, `drawing?`, \
                 `nodes?`, `edges?`, `error?`",
        when: "After each request that ran something: a `query`, a statement `feed` \
               completed (not a host's command, whose own requests make theirs), \
               `sources.attach`, and the graph steps `graph.open`, `graph.explore` and \
               `graph.find`; failed or cancelled ones too, with `error`. `client` is the \
               `{name, kind}` its client gave `hello` (`null` if none); `at` is when it \
               started (ms since the Unix epoch) and `ms` how long it took; `summary` what \
               ran, as typed (`SELECT …`, `.expand 33`); `result` the kept result a query \
               made or a drawing drew, `rows` a query's row count. `drawing` numbers the \
               drawings: a `graph.open` starts one, and the graph steps after it belong to it; \
               `nodes` and `edges` are what it shows after the step. Every client attached to \
               the session gets it, after the request's other events and before its reply; \
               `history` gives the last 500",
    },
];

/// An event the session sends ([`Session::take_events`]): `{"event": name,
/// ...fields}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub name: &'static str,
    /// Its other fields: a JSON object.
    pub fields: Value,
}

impl Event {
    /// The catalog changed.
    pub fn catalog() -> Event {
        Event {
            name: "catalog",
            fields: json!({}),
        }
    }

    /// The event as the protocol's JSON.
    pub fn to_json(&self) -> Value {
        let mut message = json!({"event": self.name});
        if let Value::Object(fields) = &self.fields {
            for (k, v) in fields {
                message[k] = v.clone();
            }
        }
        message
    }
}

/// The operations that may change the catalog: after each, the session
/// compares it with what it was ([`Session::take_events`]).
const CHANGE_CATALOG: &[&str] = &["start", "feed", "query", "sources.attach"];

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
    /// (`ursa_engine::session_files`), unless `copy_to_disk`. The browser
    /// has no disk, so the demo always has them.
    pub uploads: bool,
    /// Where relative paths resolve ([`Root`]): a directory natively (`ursa
    /// serve DIR`), a URL in the browser. A session with a directory root
    /// writes files only under it. `None`: natively, the process's working
    /// directory, as the CLI; in the browser, nowhere.
    pub root: Option<Root>,
    /// Write `COPY … TO 'name'` to disk (under `root`, if it is a
    /// directory), as DataFusion's `COPY` does, though the session has
    /// `uploads`: these are then for the files a page adds only. Native only:
    /// the browser always keeps a copy in memory.
    pub copy_to_disk: bool,
    /// A database file to open, as the CLI opens one (`ursa club.ursa`):
    /// natively, by `ursa serve club.ursa`. It is opened before the first
    /// request after `hello`, and again after each `start`. Another session
    /// of this process holding it for writing, the session opens it
    /// read-only, and says so.
    pub database: Option<std::path::PathBuf>,
    /// Write no files (`ursa serve --read-only`,
    /// `ursa_engine::paths::set_read_only`): `COPY … TO`, `INSERT INTO` a
    /// table of files, `.output` and `.once` are refused, and a database is
    /// opened read-only, so what changes it is refused too.
    pub read_only: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            version: env!("CARGO_PKG_VERSION").to_string(),
            transport: "native",
            capabilities: Vec::new(),
            uploads: true,
            root: None,
            copy_to_disk: false,
            database: None,
            read_only: false,
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

    fn arrow(bytes: Vec<u8>) -> Part {
        Part {
            name: "arrow",
            kind: PartKind::Arrow,
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

    pub fn not_found(message: impl Into<String>) -> Error {
        Error {
            kind: "not_found",
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
    /// Whether `hello` came: every other operation waits for it.
    greeted: bool,
    /// Whether [`Options::database`] is open (or there is none to open).
    opened: bool,
    /// The results `query` kept, for `fetch`, `export`, `graph.open`.
    results: Results,
    /// The catalog's fingerprint when last looked at, for the `catalog`
    /// event; `None` until a request that may change it.
    catalog_seen: Option<u64>,
    /// Whether a request that may change the catalog ran since it was last
    /// looked at.
    catalog_dirty: bool,
    /// What the session's clients ran: `history`, and `activity` events.
    history: history::History,
}

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
        ursa_engine::paths::set_root(&ctx, options.root.clone());
        ursa_engine::session_files::set_copy_to_disk(&ctx, options.copy_to_disk);
        ursa_engine::paths::set_read_only(&ctx, options.read_only);
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
            opened: options.database.is_none(),
            options,
            greeted: false,
            results: Results::default(),
            catalog_seen: None,
            catalog_dirty: false,
            history: history::History::default(),
        }
    }

    /// Say who sends the next requests: a client's `{name, kind}`, as its
    /// `hello` gave it, for the history's entries and `activity` events. A
    /// host that serves several clients on one session (`ursa serve`'s
    /// shared session) sets it before each request; `hello` sets it too.
    pub fn set_client(&mut self, client: Option<Value>) {
        self.history.set_client(client);
    }

    /// Open [`Options::database`], if it is not open yet: as the CLI opens
    /// one, or read-only in a read-only session, or while another session
    /// of this process has it open for writing (which the session's next
    /// output says).
    async fn open_database(&mut self) -> std::result::Result<(), Error> {
        let Some(path) = self.options.database.clone().filter(|_| !self.opened) else {
            return Ok(());
        };
        let ctx = self.shell.context().clone();
        let options = ursa_sql::OpenOptions {
            read_only: self.options.read_only,
            ..Default::default()
        };
        match ursa_sql::open_database(&ctx, &path, &options).await {
            Ok(()) => {}
            Err(e) if !options.read_only && ursa_engine::database::is_in_use(&e) => {
                let read_only = ursa_sql::OpenOptions::read_only();
                ursa_sql::open_database(&ctx, &path, &read_only).await?;
                let _ = writeln!(
                    self.err,
                    "Note: another session of this server has {} open for writing, so this \
                     one has it read-only.",
                    path.display()
                );
            }
            Err(e) => return Err(e.into()),
        }
        self.opened = true;
        Ok(())
    }

    /// Run the operation `op` with its JSON `args` and binary `parts`
    /// (`design/console.md` §4.3). [`OPS`] lists the operations, and
    /// `docs/reference/protocol.md` is generated from it. The first request
    /// must be `hello`.
    ///
    /// SQL and command errors are what `feed`, `graph.explore` and
    /// `graph.find` print; the other operations fail with an [`Error`].
    ///
    /// A request that runs something (a query, a statement, a graph step)
    /// is kept in the session's history, and sent as an `activity` event
    /// ([`Self::take_events`]).
    pub async fn handle(
        &mut self,
        op: &str,
        args: Value,
        parts: Vec<Vec<u8>>,
    ) -> std::result::Result<Reply, Error> {
        if !OPS.iter().any(|o| o.name == op) {
            return Err(Error::protocol(format!("unknown op {op}")));
        }
        if !self.greeted && op != "hello" {
            return Err(Error::protocol(format!(
                "the first request must be hello, not {op}"
            )));
        }
        self.history.begin(op, &args);
        let outcome = self.run_op(op, args, parts).await;
        // What a new drawing shows, for its entry: counted once, here (an
        // explore step's reply says it; a search changes nothing).
        let drawn = (op == "graph.open" && outcome.is_ok())
            .then(|| self.shell.explorer().map(|e| e.shown()))
            .flatten();
        self.history.finish(&outcome, drawn);
        outcome
    }

    /// [`Self::handle`]'s operation `op`, past its checks.
    async fn run_op(
        &mut self,
        op: &str,
        args: Value,
        mut parts: Vec<Vec<u8>>,
    ) -> std::result::Result<Reply, Error> {
        let a = Args(&args);
        if CHANGE_CATALOG.contains(&op) {
            // What it was before, the first time; marked before the request
            // runs, so that one cancelled halfway is still compared.
            if self.catalog_seen.is_none() {
                self.catalog_seen = Some(catalog::fingerprint(self.shell.context()).await);
            }
            self.catalog_dirty = true;
        }
        // The database opens before the first request that may use it
        // (`start` opens it in the session it makes).
        if !matches!(op, "hello" | "cancel" | "memory" | "start") {
            self.open_database().await?;
        }
        match op {
            "hello" => {
                let protocol = args.get("protocol").and_then(Value::as_u64);
                if protocol != Some(u64::from(PROTOCOL)) {
                    let asked = args.get("protocol").map_or("none".into(), Value::to_string);
                    return Err(Error::protocol(format!(
                        "this session speaks protocol {PROTOCOL}, not {asked}"
                    )));
                }
                let kind = match args.get("session") {
                    None | Some(Value::Null) => "shared",
                    Some(v) => match v.as_str() {
                        Some("new") => "new",
                        Some("shared") => "shared",
                        _ => {
                            return Err(Error::protocol(format!(
                                "hello's session is \"new\" or \"shared\", not {v}"
                            )))
                        }
                    },
                };
                self.greeted = true;
                self.history.hello(&args);
                Ok(Reply::json(json!({
                    "protocol": PROTOCOL,
                    "version": self.options.version,
                    "transport": self.options.transport,
                    "capabilities": self.capabilities(),
                    "session": {"kind": kind},
                })))
            }
            // The host answers `cancel` while `target` runs; here nothing is
            // running, so there is nothing to stop.
            "cancel" => Ok(Reply::json(json!({}))),
            "memory" => Ok(Reply::json(memory())),
            "history" => {
                let limit = args.get("limit").and_then(Value::as_u64);
                Ok(Reply::json(self.history.list(limit)))
            }
            "query" => {
                let rows = args.get("rows").and_then(Value::as_u64).unwrap_or(1_000);
                let (result, arrow) = self.query(a.str("sql")?, rows as usize).await?;
                Ok(Reply {
                    result,
                    parts: vec![Part::arrow(arrow)],
                })
            }
            "explain" => {
                let start = Instant::now();
                let plan = self.explain(a.str("sql")?).await?;
                Ok(Reply::json(json!({
                    "plan": plan,
                    "ms": start.elapsed().as_secs_f64() * 1000.0,
                })))
            }
            "fetch" => {
                let id = a.str("result")?;
                let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
                let rows = args.get("rows").and_then(Value::as_u64).unwrap_or(1_000);
                let (result, arrow) = self.fetch(id, offset as usize, rows as usize)?;
                Ok(Reply {
                    result,
                    parts: vec![Part::arrow(arrow)],
                })
            }
            "close" => {
                self.close_result(a.str("result")?)?;
                Ok(Reply::json(json!({})))
            }
            "start" => {
                let mut history = std::mem::take(&mut self.history);
                history.restarted();
                *self = Session {
                    greeted: true,
                    catalog_seen: self.catalog_seen,
                    catalog_dirty: true,
                    history,
                    ..Session::new(self.options.clone())
                };
                // The old session's database is closed: open it here.
                self.open_database().await?;
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
                let format = a.str("format")?;
                let bytes = match a.id_opt("result")? {
                    Some(id) => self.export_result(id, format)?,
                    None => self.export(format)?,
                };
                Ok(Reply {
                    result: json!({}),
                    parts: vec![Part {
                        name: "bytes",
                        kind: PartKind::Bytes,
                        bytes,
                    }],
                })
            }
            "catalog" => Ok(Reply::json(catalog::catalog(self.shell.context()).await)),
            "sources.list" => {
                let root = self.sources_root()?;
                let listed = sources::list(self.shell.context(), &root, a.str("url")?).await?;
                Ok(Reply::json(listed))
            }
            "sources.attach" => {
                self.sources_root()?;
                let sql = sources::attach_sql(a.str("url")?, a.str_opt("as"))?;
                self.run_sql(&sql).await?;
                Ok(Reply::json(json!({})))
            }
            "complete" => {
                let sql = a.str("sql")?;
                let cursor = args.get("cursor").and_then(Value::as_u64);
                let cursor = cursor.ok_or_else(|| Error::protocol("complete needs a cursor"))?;
                let commands = self
                    .shell
                    .commands()
                    .map(|(name, _, help)| (name.to_string(), help.to_string()))
                    .collect();
                let ctx = self.shell.context();
                Ok(Reply::json(
                    complete::complete(ctx, commands, sql, cursor as usize).await,
                ))
            }
            "highlight" => Ok(Reply::json(complete::highlight(a.str("sql")?))),
            "graph.open" => {
                let text = a.str_opt("text").unwrap_or("");
                let graph = match a.id_opt("result")? {
                    Some(id) => self.open_graph_of(text, id).await?,
                    None => self.open_graph(text).await?,
                };
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
                self.history.closed_drawing();
                Ok(Reply::json(json!({})))
            }
            other => unreachable!("{other} is in OPS but not handled"),
        }
    }

    /// Run `sql` (one or more statements) for the console: the last
    /// statement's result is kept, by the id in `result`, for
    /// [`Self::fetch`], [`Self::export_result`] and [`Self::open_graph_of`],
    /// and becomes the shell's last result (`.graph`, `export`). Gives
    /// `{result, columns, metadata, rows, more, ms, notices}`, where `columns`
    /// are `[{name, type, nullable, metadata}]`, `metadata` is the schema's
    /// own (`ursa.pattern` sits there for a `GRAPH_TABLE` result) and `rows`
    /// the whole result's count; and the first `rows` rows as an Arrow IPC
    /// stream, the metadata inside its schema. A statement with no columns
    /// (`CREATE`, `SET`) keeps nothing: its `result` is `null`.
    ///
    /// Every row is collected before the first are sent: the reply gives the
    /// whole result's count, and the shell's last result holds every row.
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
        let first = page(&batches, 0, rows);
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
        // A statement with no columns (CREATE, SET) says it ran.
        let notices: Vec<&str> = if columns.is_empty() {
            vec!["OK"]
        } else {
            vec![]
        };
        let id = if columns.is_empty() {
            None
        } else {
            self.shell
                .set_last_result(Arc::clone(&schema), batches.clone());
            Some(self.results.keep(Arc::clone(&schema), batches, rows)?)
        };
        // The rule `graph.open` would draw it by, from its columns alone.
        let graph = if columns.is_empty() {
            None
        } else {
            view::rules::rule_of(&schema).ok().map(view::Rule::name)
        };
        let result = json!({
            "result": id,
            "columns": columns,
            "metadata": schema.metadata(),
            "rows": total,
            "more": total > rows,
            "ms": start.elapsed().as_secs_f64() * 1000.0,
            "notices": notices,
            "graph": graph,
        });
        Ok((result, arrow))
    }

    /// The plan of `sql`'s last statement, as `EXPLAIN` gives it: each of
    /// its plans (`logical_plan`, `physical_plan`) under its name, as text.
    /// Nothing runs: not the statement, nor those before it, so a statement
    /// that reads what an earlier one creates cannot be explained until that
    /// one has run.
    pub async fn explain(&mut self, sql: &str) -> Result<String> {
        use arrow::array::AsArray;
        use arrow::datatypes::DataType;
        let ctx = self.shell.context().clone();
        let (_, text) = ursa_sql::parse_sql_with_text_for(&ctx, sql)?
            .pop()
            .ok_or_else(|| DataFusionError::Plan("no statement to explain".into()))?;
        let text = text.trim().trim_end_matches(';').trim_end();
        let explain = format!("EXPLAIN {text}");
        let mut plan = String::new();
        for (statement, text) in ursa_sql::parse_sql_with_text_for(&ctx, &explain)? {
            let frame = ursa_sql::execute_with_text(&ctx, statement, &text).await?;
            for batch in frame.collect().await? {
                if batch.num_columns() < 2 {
                    continue;
                }
                let kind = arrow::compute::cast(batch.column(0), &DataType::Utf8)?;
                let body = arrow::compute::cast(batch.column(1), &DataType::Utf8)?;
                let (kind, body) = (kind.as_string::<i32>(), body.as_string::<i32>());
                for row in 0..batch.num_rows() {
                    if !plan.is_empty() {
                        plan.push('\n');
                    }
                    plan.push_str(kind.value(row));
                    plan.push('\n');
                    plan.push_str(body.value(row).trim_end());
                    plan.push('\n');
                }
            }
        }
        Ok(plan)
    }

    /// The events the requests since the last call caused, to send before
    /// the last one's reply (`design/console.md` §4.5): `catalog`, if the
    /// catalog changed, then `activity`, for a request that ran something
    /// (one cancelled with its future dropped is sent as cancelled). A host
    /// calls it after each request, whether it succeeded, failed or was
    /// cancelled.
    pub async fn take_events(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if std::mem::take(&mut self.catalog_dirty) {
            let now = catalog::fingerprint(self.shell.context()).await;
            let changed = self.catalog_seen.is_some_and(|seen| seen != now);
            self.catalog_seen = Some(now);
            if changed {
                events.push(Event::catalog());
            }
        }
        // What ran: the request, or, if it was cancelled, what it was.
        events.extend(self.history.take_events());
        events
    }

    /// What `hello` says this session can do: the host's capabilities, and
    /// `sources` for a session with a directory root.
    fn capabilities(&self) -> Vec<&'static str> {
        let mut capabilities = self.options.capabilities.clone();
        if matches!(self.options.root, Some(Root::Dir(_))) && !capabilities.contains(&"sources") {
            capabilities.push("sources");
        }
        capabilities
    }

    /// The directory root `sources.list` and `sources.attach` need.
    fn sources_root(&self) -> std::result::Result<Root, Error> {
        match &self.options.root {
            Some(root @ Root::Dir(_)) => Ok(root.clone()),
            _ => Err(Error::protocol(
                "this session has no sources: only a session with a directory root \
                 (ursa serve DIR) lists and attaches files",
            )),
        }
    }

    /// Run `sql`'s statements to their end, keeping no result.
    async fn run_sql(&mut self, sql: &str) -> Result<()> {
        let ctx = self.shell.context().clone();
        for (statement, text) in ursa_sql::parse_sql_with_text_for(&ctx, sql)? {
            ursa_sql::execute_with_text(&ctx, statement, &text)
                .await?
                .collect()
                .await?;
        }
        Ok(())
    }

    /// `rows` rows of the kept result `id` from `offset`, as an Arrow IPC
    /// stream, with `{rows, more}`: how many it holds, and whether rows
    /// follow. Fails with `not_found` for a result that has gone, and for
    /// rows past those a result over the cap kept.
    pub fn fetch(
        &self,
        id: &str,
        offset: usize,
        rows: usize,
    ) -> std::result::Result<(Value, Vec<u8>), Error> {
        let kept = self.results.get(id)?;
        let end = offset.saturating_add(rows).min(kept.rows);
        if end > kept.kept {
            return Err(kept.past_its_rows());
        }
        let slices = page(&kept.batches, offset, rows);
        let sent: usize = slices.iter().map(RecordBatch::num_rows).sum();
        let arrow = encode(&kept.schema, &slices, "arrow")?;
        let more = offset.saturating_add(sent) < kept.rows;
        Ok((json!({"rows": sent, "more": more}), arrow))
    }

    /// Drop the kept result `id`; `not_found` if it has gone already.
    pub fn close_result(&mut self, id: &str) -> std::result::Result<(), Error> {
        self.results.close(id)
    }

    /// The kept result `id`, every row, written as `format` (see
    /// [`encode`]). Fails with `not_found` for a result that has gone, or
    /// that kept only its first rows.
    pub fn export_result(&self, id: &str, format: &str) -> std::result::Result<Vec<u8>, Error> {
        let kept = self.results.get(id)?.whole()?;
        Ok(encode(&kept.schema, &kept.batches, format)?)
    }

    /// [`Self::open_graph`], with the kept result `id` as what `text` draws
    /// when it names no source. The last result stays. Fails with
    /// `not_found` for a result that has gone, or that kept only its first
    /// rows.
    pub async fn open_graph_of(
        &mut self,
        text: &str,
        id: &str,
    ) -> std::result::Result<GraphData, Error> {
        let kept = self.results.get(id)?.whole()?;
        let rows = (Arc::clone(&kept.schema), kept.batches.clone());
        let placed = self.shell.open_graph_of(text, Some(rows)).await?;
        Ok(self.graph_data(placed))
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
        let placed = self.shell.open_graph_placed(text).await?;
        Ok(self.graph_data(placed))
    }

    /// The graph the shell just opened, for the host's view.
    fn graph_data(&self, (skipped, positions): (usize, Option<view::Positions>)) -> GraphData {
        // The bytes are written from the shell's own result: it is not
        // copied first (8 bytes an edge).
        let result = self.shell.explorer().expect("a graph was opened").result();
        GraphData {
            ugr: result.to_bytes(),
            skipped,
            positions: positions.map(|mut p| {
                p.x.append(&mut p.y);
                p.x
            }),
        }
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

/// The engine's memory, `{bytes, kind}`: in the browser its WebAssembly
/// memory (`wasm`), natively the process's resident set (`rss`), which is
/// `null` where it cannot be read (only Linux's `/proc` is).
fn memory() -> Value {
    #[cfg(target_arch = "wasm32")]
    {
        let pages = core::arch::wasm32::memory_size::<0>() as u64;
        json!({"bytes": pages * 65_536, "kind": "wasm"})
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let rss = || -> Option<u64> {
            let status = std::fs::read_to_string("/proc/self/status").ok()?;
            let line = status.lines().find_map(|l| l.strip_prefix("VmRSS:"))?;
            let kb: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
            Some(kb * 1024)
        };
        json!({"bytes": rss(), "kind": "rss"})
    }
}

/// How many results a session keeps.
pub const RESULTS_KEPT: usize = 16;

/// How many bytes of results a session keeps: 256 MB.
pub const RESULTS_BYTES: usize = 256 << 20;

/// A result `query` kept ([`Results`]).
struct Kept {
    id: u64,
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    /// The whole result's rows.
    rows: usize,
    /// The rows kept: all of them, or for a result over the cap, the first
    /// ones, which the reply sent.
    kept: usize,
    /// What its buffers hold ([`held_bytes`]).
    bytes: usize,
}

impl Kept {
    fn name(&self) -> String {
        format!("r{}", self.id)
    }

    /// This result, if it kept every row.
    fn whole(&self) -> std::result::Result<&Kept, Error> {
        if self.kept < self.rows {
            return Err(self.past_its_rows());
        }
        Ok(self)
    }

    /// Why rows past those a result over the cap kept cannot be had.
    fn past_its_rows(&self) -> Error {
        Error::not_found(format!(
            "result {} kept only its first {} of {} rows, as it is over the {} MB \
             the session keeps; run the query again",
            self.name(),
            self.kept,
            self.rows,
            RESULTS_BYTES >> 20,
        ))
    }
}

/// The results a session keeps for `fetch`, `export`, `graph.open` and
/// `close`, by id (`"r4"`): the last [`RESULTS_KEPT`], within
/// [`RESULTS_BYTES`], the oldest dropped first. A result over the bytes on
/// its own keeps only its first rows.
struct Results {
    kept: VecDeque<Kept>,
    /// The next result's id: ids are never reused within a session.
    next: u64,
    max_count: usize,
    max_bytes: usize,
}

impl Default for Results {
    fn default() -> Self {
        Results {
            kept: VecDeque::new(),
            next: 1,
            max_count: RESULTS_KEPT,
            max_bytes: RESULTS_BYTES,
        }
    }
}

impl Results {
    /// Keep a result, dropping the oldest to make room, and give its id.
    /// Over the cap on its own, it keeps its first `sent` rows, copied so
    /// they hold none of the rest; or none, if even those are over.
    fn keep(
        &mut self,
        schema: SchemaRef,
        batches: Vec<RecordBatch>,
        sent: usize,
    ) -> Result<String> {
        let rows = batches.iter().map(RecordBatch::num_rows).sum();
        let mut bytes = held_bytes(&batches);
        let (batches, kept) = if bytes <= self.max_bytes {
            (batches, rows)
        } else {
            let first = page(&batches, 0, sent);
            let first = arrow::compute::concat_batches(&schema, &first)?;
            // Taken, not sliced: a slice, and concat of one batch, would
            // hold the whole result's buffers.
            let all = arrow::array::UInt32Array::from_iter_values(0..first.num_rows() as u32);
            let first = vec![arrow::compute::take_record_batch(&first, &all)?];
            bytes = held_bytes(&first);
            if bytes <= self.max_bytes {
                let kept = first[0].num_rows();
                (first, kept)
            } else {
                bytes = 0;
                (Vec::new(), 0)
            }
        };
        while self.kept.len() >= self.max_count
            || (!self.kept.is_empty() && self.bytes() + bytes > self.max_bytes)
        {
            self.kept.pop_front();
        }
        let id = self.next;
        self.next += 1;
        self.kept.push_back(Kept {
            id,
            schema,
            batches,
            rows,
            kept,
            bytes,
        });
        Ok(format!("r{id}"))
    }

    fn bytes(&self) -> usize {
        self.kept.iter().map(|k| k.bytes).sum()
    }

    fn position(&self, id: &str) -> std::result::Result<usize, Error> {
        let number = id.strip_prefix('r').and_then(|n| n.parse::<u64>().ok());
        number
            .and_then(|n| self.kept.iter().position(|k| k.id == n))
            .ok_or_else(|| {
                Error::not_found(format!(
                    "no result {id}: it was closed, or dropped to keep the session's last \
                     {RESULTS_KEPT} results within {} MB; run the query again",
                    RESULTS_BYTES >> 20
                ))
            })
    }

    fn get(&self, id: &str) -> std::result::Result<&Kept, Error> {
        Ok(&self.kept[self.position(id)?])
    }

    fn close(&mut self, id: &str) -> std::result::Result<(), Error> {
        let at = self.position(id)?;
        self.kept.remove(at);
        Ok(())
    }
}

/// Up to `rows` rows of `batches` from `offset`, as slices of them.
fn page(batches: &[RecordBatch], mut offset: usize, rows: usize) -> Vec<RecordBatch> {
    let mut slices = Vec::new();
    let mut left = rows;
    for batch in batches {
        if left == 0 {
            break;
        }
        let n = batch.num_rows();
        if offset >= n {
            offset -= n;
            continue;
        }
        let take = left.min(n - offset);
        slices.push(batch.slice(offset, take));
        offset = 0;
        left -= take;
    }
    slices
}

/// The bytes the buffers of `batches` hold, each buffer counted once: a
/// batch sliced from another, or a column shared by two, holds the same
/// memory. A buffer shared with a table still counts, so this is what the
/// result would hold once the table has gone.
fn held_bytes(batches: &[RecordBatch]) -> usize {
    fn walk(data: &ArrayData, seen: &mut HashSet<usize>, bytes: &mut usize) {
        let nulls = data.nulls().map(|n| n.buffer());
        for buffer in data.buffers().iter().chain(nulls) {
            if seen.insert(buffer.data_ptr().as_ptr() as usize) {
                *bytes += buffer.capacity().max(buffer.len());
            }
        }
        for child in data.child_data() {
            walk(child, seen, bytes);
        }
    }
    let (mut seen, mut bytes) = (HashSet::new(), 0);
    for batch in batches {
        for column in batch.columns() {
            walk(&column.to_data(), &mut seen, &mut bytes);
        }
    }
    bytes
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

    /// An optional result id: absent or `null` is none, anything but a
    /// string a protocol error.
    fn id_opt(&self, name: &str) -> std::result::Result<Option<&'a str>, Error> {
        match self.0.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(id)) => Ok(Some(id)),
            Some(other) => Err(Error::protocol(format!(
                "'{name}' is a result id such as \"r4\", not {other}"
            ))),
        }
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

#[path = "session_catalog.rs"]
mod catalog;
#[path = "session_complete.rs"]
mod complete;
#[path = "session_history.rs"]
mod history;
pub use history::HISTORY_LIMIT;
#[path = "session_sources.rs"]
mod sources;

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "session_ops_tests.rs"]
mod ops_tests;

#[cfg(test)]
#[path = "session_history_tests.rs"]
mod history_tests;
