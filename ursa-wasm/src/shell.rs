//! The `ursa` shell in the browser: [`ursa_shell::shell::Shell`], the one the
//! CLI runs, fed a line at a time by the page's terminal (xterm.js), as
//! DuckDB's web shell is DuckDB's own shell.
//!
//! ```js
//! WebShell.setBaseUrl(new URL("data/", location.href).href);
//! const shell = new WebShell();
//! shell.setWidth(term.cols);
//! const { out, err, flow } = JSON.parse(await shell.feed("SELECT 42 AS answer;"));
//! term.write(out.replaceAll("\n", "\r\n"));
//! ```
//!
//! What a line prints comes back from [`WebShell::feed`] as text with ANSI
//! colour, for the terminal to show. The page owns the screen: line editing,
//! history and its own dot-commands ([`WebShell::add_host_command`], such as
//! `.share`), which `feed` hands back as `flow: "host"`.
//!
//! The page's Graph view is the shell's too: [`WebShell::graph`] opens a
//! graph to draw, and [`WebShell::explore`] runs the commands that change it
//! (`.expand`, `.hide`, `.show`, `.undo`, `.redo`), each giving back a delta
//! for the view to apply.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;

use datafusion::error::{DataFusionError, Result};
use js_sys::{Promise, Uint8Array};
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use serde_json::json;
use url::Url;
use ursa_shell::highlight;
use ursa_shell::print::OutputFormat;
use ursa_shell::shell::{Flow, Shell};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

use crate::{encode, js_error, Stored};
use ursa_engine::graph::view::inspect::Inspect;
use ursa_engine::graph::view::GraphView;

/// Where added files live: `memory://uploads/`.
pub(crate) fn uploads_url() -> Url {
    Url::parse("memory://uploads").expect("a valid URL")
}

/// Output the shell writes, kept until [`WebShell::feed`] takes it.
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

/// The shell, as a page's terminal drives it.
#[wasm_bindgen]
pub struct WebShell {
    shell: Rc<RefCell<Shell<Buffer, Buffer>>>,
    out: Buffer,
    err: Buffer,
    /// The terminal's width in characters, which tables fit.
    width: Rc<Cell<usize>>,
    /// Files the page added ([`WebShell::add_file`]), at `memory://uploads/`.
    uploads: Arc<InMemory>,
}

/// A graph for the page's Graph view, from [`WebShell::graph`]: a graph
/// result (`ursa-viz`'s model) as one UGR buffer, which the page reads with
/// `ursa-viz/ui/ugr.js`.
#[wasm_bindgen]
pub struct GraphData {
    ugr: Vec<u8>,
    skipped: u32,
}

impl From<GraphView> for GraphData {
    fn from(view: GraphView) -> Self {
        GraphData {
            ugr: view.result.to_bytes(),
            skipped: view.skipped as u32,
        }
    }
}

#[wasm_bindgen]
impl GraphData {
    /// The graph as UGR bytes (`ursa-viz/UGR.md`), in a `Uint8Array` with a
    /// buffer of its own, to transfer: the nodes' ids, the edges as dense
    /// node indexes, the node columns `SIZE BY` and `COLOR BY` made (the
    /// result's encodings name them; `COLOR BY`'s is a dictionary column
    /// whose values are the categories, the largest first), and the SQL and
    /// the rule it came from. It gives the bytes once: a second call gives
    /// none.
    #[wasm_bindgen(js_name = takeUgr)]
    pub fn take_ugr(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.ugr)
    }

    /// Rows left out because an endpoint is NULL.
    #[wasm_bindgen(getter)]
    pub fn skipped(&self) -> u32 {
        self.skipped
    }
}

/// What an explore command did, from [`WebShell::explore`]: what it printed,
/// and the change for the page's view, a graph delta as UGR bytes.
#[wasm_bindgen]
pub struct Explored {
    out: String,
    err: String,
    delta: Vec<u8>,
    nodes: u32,
    edges: u32,
}

#[wasm_bindgen]
impl Explored {
    /// What the command printed: one line (`+16 nodes, +41 edges`, or why
    /// nothing changed), the SQL for `--sql`, or the steps for `.graph
    /// history`.
    #[wasm_bindgen(getter)]
    pub fn out(&self) -> String {
        self.out.clone()
    }

    /// The error it printed, if it failed: no open graph, or an id that is
    /// not a node of it.
    #[wasm_bindgen(getter)]
    pub fn err(&self) -> String {
        self.err.clone()
    }

    /// The change as a UGR delta (`ursa-viz/UGR.md`), in a `Uint8Array` with
    /// a buffer of its own, to transfer and apply to the view; empty when
    /// the command changed nothing. It gives the bytes once.
    #[wasm_bindgen(js_name = takeDelta)]
    pub fn take_delta(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.delta)
    }

    /// How many nodes are shown after the command.
    #[wasm_bindgen(getter)]
    pub fn nodes(&self) -> u32 {
        self.nodes
    }

    /// How many edges are shown after the command.
    #[wasm_bindgen(getter)]
    pub fn edges(&self) -> u32 {
        self.edges
    }
}

impl Default for WebShell {
    fn default() -> Self {
        Self::new()
    }
}

/// What `.find` found, from [`WebShell::find`]: what it printed, and the
/// nodes for the page's view to select and fit.
#[wasm_bindgen]
pub struct FoundNodes {
    out: String,
    err: String,
    selected: Vec<u32>,
    matches: u32,
    not_shown: String,
    not_shown_total: u32,
}

#[wasm_bindgen]
impl FoundNodes {
    /// What the command printed: how many nodes it selected and the matches
    /// that are not shown, or the SQL for `--sql`.
    #[wasm_bindgen(getter)]
    pub fn out(&self) -> String {
        self.out.clone()
    }

    /// The error it printed, if it failed: no open graph, or a condition no
    /// vertex table can answer.
    #[wasm_bindgen(getter)]
    pub fn err(&self) -> String {
        self.err.clone()
    }

    /// The matches the view shows, as its node indexes, the best first: at
    /// most 1,000. Once: the array is moved out.
    #[wasm_bindgen(js_name = takeSelected)]
    pub fn take_selected(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.selected)
    }

    /// How many matches the view shows: more than were selected, past the
    /// cap.
    #[wasm_bindgen(getter)]
    pub fn matches(&self) -> u32 {
        self.matches
    }

    /// The first matches the view does not show, as JSON: `[{"key": "34",
    /// "hidden": false}, …]`, `hidden` for a node `.show` brings back.
    #[wasm_bindgen(getter, js_name = notShown)]
    pub fn not_shown(&self) -> String {
        self.not_shown.clone()
    }

    /// How many matches the view does not show.
    #[wasm_bindgen(getter, js_name = notShownTotal)]
    pub fn not_shown_total(&self) -> u32 {
        self.not_shown_total
    }
}

#[wasm_bindgen]
impl WebShell {
    /// A shell over a new engine session, drawing tables in colour for a
    /// terminal 80 characters wide until [`Self::set_width`] says otherwise.
    #[wasm_bindgen(constructor)]
    pub fn new() -> WebShell {
        #[cfg(target_arch = "wasm32")]
        console_error_panic_hook::set_once();
        let ctx = ursa_engine::new_session();
        let uploads = Arc::new(InMemory::new());
        ctx.register_object_store(&uploads_url(), Arc::clone(&uploads) as Arc<dyn ObjectStore>);
        let (out, err) = (Buffer::default(), Buffer::default());
        let width = Rc::new(Cell::new(80));
        let mut shell = Shell::new(ctx.clone(), OutputFormat::Table, out.clone(), err.clone());
        let screen = Rc::clone(&width);
        shell.set_screen(Box::new(move || screen.get()), true);
        shell.set_error_color(true);
        shell.keep_last_result();
        shell.add_explore_commands();
        shell.set_version(env!("CARGO_PKG_VERSION"));
        WebShell {
            shell: Rc::new(RefCell::new(shell)),
            out,
            err,
            width,
            uploads,
        }
    }

    /// Set the URL relative paths resolve against, such as
    /// `https://example.com/data/` (end a directory with `/`), for every
    /// shell and session on the page.
    #[wasm_bindgen(js_name = setBaseUrl)]
    pub fn set_base_url(base: &str) -> Result<(), JsError> {
        crate::set_base_url(base)
    }

    /// The terminal's width in characters: tables are drawn no wider.
    #[wasm_bindgen(js_name = setWidth)]
    pub fn set_width(&self, columns: usize) {
        self.width.set(columns);
    }

    /// Run one typed line: a dot-command, or a line of SQL, which runs once
    /// the statement ends with `;`. Resolves to JSON: `{"out", "err", "flow"}`
    /// with what it printed (ANSI text, `\n` line ends) and `flow` one of
    /// `"continue"`, `"exit"` (`.quit`) or `"host"` (a command the page added,
    /// in `"command"`). Call it again only once it has resolved.
    pub fn feed(&self, line: String) -> Promise {
        let (shell, out, err) = (Rc::clone(&self.shell), self.out.clone(), self.err.clone());
        future_to_promise(async move {
            let json = feed(&shell, &out, &err, &line).await.map_err(js_error)?;
            Ok(JsValue::from_str(&json))
        })
    }

    /// The prompt for a new statement (`memory U `), without colour.
    pub fn prompt(&self) -> Result<String, JsError> {
        Ok(self.borrow()?.prompt())
    }

    /// The prompt while a statement continues: spaces as wide.
    pub fn continuation(&self) -> Result<String, JsError> {
        Ok(self.borrow()?.continuation())
    }

    /// Whether a statement is partly typed.
    #[wasm_bindgen(js_name = isPending)]
    pub fn is_pending(&self) -> Result<bool, JsError> {
        Ok(self.borrow()?.is_pending())
    }

    /// Drop the statement being typed (Ctrl-C).
    #[wasm_bindgen(js_name = cancelInput)]
    pub fn cancel_input(&self) -> Result<(), JsError> {
        self.borrow_mut()?.cancel_input();
        Ok(())
    }

    /// Add a dot-command the page runs: `.help` lists it, and
    /// [`Self::feed`] hands it back.
    #[wasm_bindgen(js_name = addHostCommand)]
    pub fn add_host_command(&self, name: &str, args: &str, help: &str) -> Result<(), JsError> {
        self.borrow_mut()?.add_host_command(name, args, help);
        Ok(())
    }

    /// What `.help shortcuts` says: the page's keys.
    #[wasm_bindgen(js_name = setShortcuts)]
    pub fn set_shortcuts(&self, text: &str) -> Result<(), JsError> {
        self.borrow_mut()?.set_shortcuts(text);
        Ok(())
    }

    /// Put a file from the page in memory, and resolve to the URL SQL reads it
    /// by: `memory://uploads/<name>`. Its format comes from its extension, as
    /// for any file.
    #[wasm_bindgen(js_name = addFile)]
    pub fn add_file(&self, name: String, bytes: Vec<u8>) -> Promise {
        let uploads = Arc::clone(&self.uploads);
        future_to_promise(async move {
            add_upload(&uploads, &name, bytes)
                .await
                .map(|url| JsValue::from_str(&url))
                .map_err(js_error)
        })
    }

    /// The last result that had columns, every row, written as `format`:
    /// `csv`, `json`, `arrow` or `parquet`.
    #[wasm_bindgen(js_name = exportLast)]
    pub fn export_last(&self, format: &str) -> Result<Uint8Array, JsError> {
        let shell = self.borrow()?;
        let (schema, batches) = shell
            .last_result()
            .ok_or_else(|| JsError::new("no result to export yet; run a query first"))?;
        let stored = Stored {
            schema: Arc::clone(schema),
            batches: batches.clone(),
        };
        let bytes =
            encode(&stored, format).map_err(|e| JsError::new(&ursa_engine::user_message(&e)))?;
        Ok(Uint8Array::from(bytes.as_slice()))
    }

    /// A graph to draw (the page's `.graph`), resolving to a [`GraphData`].
    /// `text` is `[SOURCE] [ON graph] [AS …] [NODES (query)] [SIZE BY x]
    /// [COLOR BY y] [LAYOUT force|rings|layers]`. SOURCE
    /// is empty for the last result, a property graph's name for its edges
    /// (and its vertices' keys), a table's name for its rows, or a query.
    /// Rows are drawn by the first result → graph rule that matches them
    /// (`ursa_engine::graph::view::rules`), read by their `ursa.*` metadata
    /// and else by their names: paths, a hop's rings, edges (`src` and `dst`,
    /// or else the first two columns when both are ids of one kind), a
    /// pattern's matches, or nodes with the graph's edges among them. The
    /// result's provenance says which, and what was not a graph is an error
    /// that says what would be.
    ///
    /// `AS edges(src, dst [, weight])`, `AS nodes(id)`, `AS path(nodes [,
    /// edges])` or `AS hops(seed, id, distance)` says what the rows are and
    /// names the columns, in place of the rules (#434). `ON graph` names the
    /// property graph the ids belong to, and `LAYOUT` the layout, whatever
    /// the rule chose (the result's `encodings.layout`).
    ///
    /// `SIZE BY x` and `COLOR BY y` give each node a value, as a node column
    /// of the result. A name is,
    /// in turn: a column of `NODES (SELECT id, … FROM …)`; a column of the
    /// property graph's vertex tables; `degree`; or a per-node algorithm
    /// (`pagerank`, `louvain`, `betweenness`, …) run on the property graph
    /// through `algorithms(G, …)`, undirected and seeded. Rows (a query, a
    /// table, the last result) whose graph is not known (by their columns,
    /// or `ON graph`) take NODES's columns and degree only.
    ///
    /// Rejects, with a message to show, when there is nothing to draw, more
    /// than 2,000,000 edges, or a name or clause that does not fit.
    ///
    /// The engine resolves all of it (`ursa_engine::graph::view`), with the
    /// shell's session and its last result.
    ///
    /// The graph becomes the shell's open graph, which [`Self::explore`]
    /// changes; a rejection leaves the one that was open.
    pub fn graph(&self, text: String) -> Promise {
        let shell = Rc::clone(&self.shell);
        future_to_promise(async move {
            graph_of(&shell, &text)
                .await
                .map(|g| GraphData::from(g).into())
                .map_err(|e| JsValue::from_str(&ursa_engine::user_message(&e)))
        })
    }

    /// Run an explore command on the open graph, resolving to an
    /// [`Explored`]: `.expand ID [in|out|both] [LIMIT n]`, `.hide ID…`,
    /// `.show ID…|all`, `.undo`, `.redo` or `.graph history`, the whole line
    /// as `feed` handed it back. `--sql` prints a command's SQL and changes
    /// nothing.
    ///
    /// What it printed is `out` (or `err`: no open graph, an id that is not
    /// a node of it), and the change is a UGR delta for the page's view,
    /// which the engine's own copy of the graph has applied already. `.undo`
    /// gives the inverse of the step it takes back, with no query.
    pub fn explore(&self, command: String) -> Promise {
        let shell = Rc::clone(&self.shell);
        let (out, err) = (self.out.clone(), self.err.clone());
        future_to_promise(async move {
            explore(&shell, &out, &err, &command)
                .await
                .map(JsValue::from)
                .map_err(|e| JsValue::from_str(&ursa_engine::user_message(&e)))
        })
    }

    /// Run `.find TEXT` or `.find WHERE condition` on the open graph (the
    /// whole line, as `feed` handed it back), resolving to a [`FoundNodes`]:
    /// what it printed, and the nodes that match for the page's view to
    /// select and fit. The graph does not change.
    pub fn find(&self, command: String) -> Promise {
        let shell = Rc::clone(&self.shell);
        let (out, err) = (self.out.clone(), self.err.clone());
        future_to_promise(async move {
            find(&shell, &out, &err, &command)
                .await
                .map(JsValue::from)
                .map_err(|e| JsValue::from_str(&ursa_engine::user_message(&e)))
        })
    }

    /// For each node slot of the open graph, how many of its edges in the
    /// property graph lead to something that is not shown, resolving to a
    /// `Uint32Array`: what the view's `+N` says of a node, and what
    /// `.expand` would start to add. Empty with no open graph, or one that
    /// is not of a property graph. It reads the graph's degrees once for
    /// each node.
    #[wasm_bindgen(js_name = notShown)]
    pub fn not_shown(&self) -> Promise {
        let shell = Rc::clone(&self.shell);
        future_to_promise(async move {
            not_shown(&shell)
                .await
                .map(|counts| js_sys::Uint32Array::from(counts.as_slice()).into())
                .map_err(|e| JsValue::from_str(&ursa_engine::user_message(&e)))
        })
    }

    /// What the open graph's tables say of its node `node` (its index in the
    /// graph [`Self::graph`] gave, and `key`, the key the page shows for it,
    /// which is checked), for the page's inspector. Resolves to JSON:
    ///
    /// ```json
    /// {"table": "members", "sql": "SELECT * FROM members WHERE id = 33",
    ///  "columns": [{"name": "id", "numeric": true}, {"name": "club", "numeric": false}],
    ///  "rows": [["33", "Officer"]], "row": null, "note": null}
    /// ```
    ///
    /// `rows` holds the node's row of its vertex table (one query by key),
    /// each value as the shell prints it and `null` for NULL; it is empty,
    /// and `note` says why, when the drawing is of no property graph or the
    /// graph has no row for the key.
    ///
    /// With `neighbours`, the answer is instead `{"neighbours": 17}`: how
    /// many the node has in the whole graph, either way (`null` with no
    /// property graph).
    ///
    /// Nothing is printed and the last result stays. Rejects with no open
    /// graph, or when `node` is not `key` in it.
    #[wasm_bindgen(js_name = inspectNode)]
    pub fn inspect_node(&self, node: u32, key: String, neighbours: bool) -> Promise {
        let shell = Rc::clone(&self.shell);
        future_to_promise(async move {
            let what = Inspect::Node { node, key: &key };
            inspect(&shell, what, neighbours)
                .await
                .map(|text| JsValue::from_str(&text))
                .map_err(|e| JsValue::from_str(&ursa_engine::user_message(&e)))
        })
    }

    /// What the open graph's tables say of its edge `edge` (its index, and
    /// the keys the page shows for its two ends, which are checked):
    /// [`Self::inspect_node`]'s JSON, with the edge's row of its edge table.
    /// The row is found by its ends; where several rows join them, by the
    /// edge's row number (`row`) if the drawing knows it, and else every
    /// such row is given, with a `note`.
    #[wasm_bindgen(js_name = inspectEdge)]
    pub fn inspect_edge(&self, edge: u32, src: String, dst: String) -> Promise {
        let shell = Rc::clone(&self.shell);
        future_to_promise(async move {
            let what = Inspect::Edge {
                edge,
                src: &src,
                dst: &dst,
            };
            inspect(&shell, what, false)
                .await
                .map(|text| JsValue::from_str(&text))
                .map_err(|e| JsValue::from_str(&ursa_engine::user_message(&e)))
        })
    }

    /// Close the open graph (the page closed its view): the explore
    /// commands say there is none until [`Self::graph`] opens another.
    #[wasm_bindgen(js_name = closeGraph)]
    pub fn close_graph(&self) -> Result<(), JsError> {
        self.borrow_mut()?.close_graph();
        Ok(())
    }

    /// `text` coloured as the shell colours what is typed (ANSI), as what
    /// is typed next: part of the statement typed so far, if one is pending,
    /// so a string or comment it left open goes on, and a line starting
    /// with `.` is SQL in it.
    pub fn highlight(&self, text: &str) -> Result<String, JsError> {
        Ok(self.borrow()?.highlight_line(text))
    }

    /// `text` coloured as a statement of its own, whatever is pending (for
    /// the history).
    #[wasm_bindgen(js_name = highlightText)]
    pub fn highlight_text(text: &str) -> String {
        highlight::ansi(text)
    }

    /// The prompt's colour and the reset after it (ANSI), for the page to
    /// draw the prompt as the CLI does.
    #[wasm_bindgen(js_name = promptColor)]
    pub fn prompt_color() -> Vec<String> {
        vec![highlight::PROMPT.to_string(), highlight::RESET.to_string()]
    }
}

impl WebShell {
    fn borrow(&self) -> Result<std::cell::Ref<'_, Shell<Buffer, Buffer>>, JsError> {
        self.shell
            .try_borrow()
            .map_err(|_| JsError::new("the shell is running a line; wait for feed to resolve"))
    }

    fn borrow_mut(&self) -> Result<std::cell::RefMut<'_, Shell<Buffer, Buffer>>, JsError> {
        self.shell
            .try_borrow_mut()
            .map_err(|_| JsError::new("the shell is running a line; wait for feed to resolve"))
    }
}

/// Feed `line` to `shell` and say what it printed, as [`WebShell::feed`]'s
/// JSON.
#[allow(
    clippy::await_holding_refcell_ref,
    reason = "the page feeds one line at a time and waits for it; every other \
              method uses try_borrow and fails instead of panicking"
)]
async fn feed(
    shell: &RefCell<Shell<Buffer, Buffer>>,
    out: &Buffer,
    err: &Buffer,
    line: &str,
) -> Result<String> {
    let flow = {
        let mut shell = shell.try_borrow_mut().map_err(|_| busy())?;
        shell.feed(line).await
    };
    let (flow, command) = match flow {
        Flow::Continue => ("continue", None),
        Flow::Exit => ("exit", None),
        Flow::Host(command) => ("host", Some(command)),
    };
    Ok(json!({
        "out": out.take(),
        "err": err.take(),
        "flow": flow,
        "command": command,
    })
    .to_string())
}

fn busy() -> DataFusionError {
    DataFusionError::Execution("the shell is already running a line".into())
}

/// What [`WebShell::graph`] draws: the shell's open graph from now on.
#[allow(
    clippy::await_holding_refcell_ref,
    reason = "as feed: one call at a time, and the others fail instead of panicking"
)]
async fn graph_of(shell: &RefCell<Shell<Buffer, Buffer>>, text: &str) -> Result<GraphView> {
    let mut shell = shell.try_borrow_mut().map_err(|_| busy())?;
    let skipped = shell.open_graph(text).await?;
    let result = shell.explorer().expect("a graph was opened").result();
    Ok(GraphView {
        result: result.clone(),
        skipped,
    })
}

/// What [`WebShell::not_shown`] resolves to.
#[allow(
    clippy::await_holding_refcell_ref,
    reason = "as feed: one call at a time, and the others fail instead of panicking"
)]
async fn not_shown(shell: &RefCell<Shell<Buffer, Buffer>>) -> Result<Vec<u32>> {
    let mut shell = shell.try_borrow_mut().map_err(|_| busy())?;
    shell.not_shown().await
}

/// Run an explore command, and take what it printed.
#[allow(
    clippy::await_holding_refcell_ref,
    reason = "as feed: one call at a time, and the others fail instead of panicking"
)]
async fn explore(
    shell: &RefCell<Shell<Buffer, Buffer>>,
    out: &Buffer,
    err: &Buffer,
    command: &str,
) -> Result<Explored> {
    let mut shell = shell.try_borrow_mut().map_err(|_| busy())?;
    let delta = shell.explore(command).await;
    let (nodes, edges) = shell.explorer().map_or((0, 0), |s| s.shown());
    Ok(Explored {
        out: out.take(),
        err: err.take(),
        delta: delta.map(|d| d.to_bytes()).unwrap_or_default(),
        nodes: nodes as u32,
        edges: edges as u32,
    })
}

/// Run `.find`, and take what it printed.
#[allow(
    clippy::await_holding_refcell_ref,
    reason = "as feed: one call at a time, and the others fail instead of panicking"
)]
async fn find(
    shell: &RefCell<Shell<Buffer, Buffer>>,
    out: &Buffer,
    err: &Buffer,
    command: &str,
) -> Result<FoundNodes> {
    let mut shell = shell.try_borrow_mut().map_err(|_| busy())?;
    let found = shell.find(command).await.unwrap_or_default();
    let not_shown: Vec<serde_json::Value> = found
        .not_shown
        .iter()
        .map(|n| serde_json::json!({"key": n.key, "hidden": n.hidden}))
        .collect();
    Ok(FoundNodes {
        out: out.take(),
        err: err.take(),
        selected: found.selected,
        matches: found.matches as u32,
        not_shown: serde_json::Value::Array(not_shown).to_string(),
        not_shown_total: found.not_shown_total as u32,
    })
}

/// [`WebShell::inspect_node`]'s and [`WebShell::inspect_edge`]'s JSON.
#[allow(
    clippy::await_holding_refcell_ref,
    reason = "as feed: one call at a time, and the others fail instead of panicking"
)]
async fn inspect(
    shell: &RefCell<Shell<Buffer, Buffer>>,
    what: Inspect<'_>,
    neighbours: bool,
) -> Result<String> {
    let shell = shell.try_borrow().map_err(|_| busy())?;
    if let (true, Inspect::Node { node, .. }) = (neighbours, what) {
        let count = shell.graph_neighbours(node).await?;
        return Ok(json!({ "neighbours": count }).to_string());
    }
    let found = shell.inspect(what).await?;
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
    })
    .to_string())
}

/// Put `bytes` at `name` in `uploads`; the URL SQL reads it by.
pub(crate) async fn add_upload(uploads: &InMemory, name: &str, bytes: Vec<u8>) -> Result<String> {
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
    Ok(format!("memory://uploads/{key}"))
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
mod tests {
    use super::*;
    use serde_json::Value;

    async fn run(shell: &WebShell, line: &str) -> Value {
        let json = feed(&shell.shell, &shell.out, &shell.err, line)
            .await
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[tokio::test]
    async fn a_line_at_a_time_as_the_cli() {
        let shell = WebShell::new();
        assert_eq!(shell.prompt().unwrap(), "memory U ");
        let first = run(&shell, "SELECT 42 AS answer,").await;
        assert_eq!(first["out"], "");
        assert!(shell.is_pending().unwrap());
        assert_eq!(shell.continuation().unwrap(), "         ");
        let done = run(&shell, "  'x' AS y;").await;
        let out = done["out"].as_str().unwrap();
        assert!(out.starts_with("┌"), "{out}");
        assert!(out.contains("│     42 │"), "{out}");
        // Types are dimmed for the terminal.
        assert!(out.contains("\x1b[90m"), "{out}");
        assert_eq!(done["flow"], "continue");
        let error = run(&shell, "SELECT nope;").await;
        assert!(
            error["err"]
                .as_str()
                .unwrap()
                .starts_with("\x1b[31mError: "),
            "{error}"
        );
        assert_eq!(run(&shell, ".quit").await["flow"], "exit");
    }

    #[tokio::test]
    async fn what_is_typed_is_coloured_after_what_is_pending() {
        let shell = WebShell::new();
        let string = format!("{}'a{}", highlight::STRING, highlight::RESET);
        assert_eq!(shell.highlight("'a").unwrap(), string);
        run(&shell, "SELECT 'it''s").await;
        // The string goes on: the quote ends it.
        let next = shell.highlight("b' AS s;").unwrap();
        assert!(
            next.starts_with(&format!("{}b'{}", highlight::STRING, highlight::RESET)),
            "{next:?}"
        );
        // On its own, the same text opens a string.
        assert!(WebShell::highlight_text("b' AS s;").starts_with("b\x1b[33m'"));
        shell.cancel_input().unwrap();
        assert!(shell
            .highlight(".tables")
            .unwrap()
            .starts_with(highlight::COMMAND));
    }

    #[tokio::test]
    async fn tables_fit_the_terminal() {
        let shell = WebShell::new();
        shell.set_width(30);
        let wide = run(
            &shell,
            "SELECT 1 AS a, 2 AS b, 3 AS c, 4 AS d, 5 AS e, 6 AS f;",
        )
        .await;
        let out = wide["out"].as_str().unwrap();
        assert!(out.contains("6 columns ("), "{out}");
    }

    #[tokio::test]
    async fn host_commands_come_back() {
        let shell = WebShell::new();
        shell
            .add_host_command(".share", "", "Copy a link that replays this session")
            .unwrap();
        let share = run(&shell, ".share").await;
        assert_eq!(share["flow"], "host");
        assert_eq!(share["command"], ".share");
        assert!(run(&shell, ".help").await["out"]
            .as_str()
            .unwrap()
            .contains(".share"));
    }

    #[tokio::test]
    async fn an_added_file_is_read_by_its_url() {
        let shell = WebShell::new();
        let url = add_upload(&shell.uploads, "edges.csv", b"src,dst\n1,2\n2,3\n".to_vec())
            .await
            .unwrap();
        assert_eq!(url, "memory://uploads/edges.csv");
        let out = run(&shell, &format!("SELECT count(*) AS n FROM '{url}';")).await;
        assert!(out["out"].as_str().unwrap().contains("│     2 │"), "{out}");
        // Names a URL would read otherwise are kept under safe ones.
        let mut urls = Vec::new();
        for (name, kept) in [
            ("données.csv", "donn_es-"),
            ("a#b.csv", "a_b-"),
            ("a?b.csv", "a_b-"),
            ("50%.csv", "50_-"),
            ("what?.csv", "what_-"),
            ("[x] y*.csv", "_x__y_-"),
            ("it's.csv", "it_s-"),
        ] {
            let bytes = b"src,dst\n1,2\n2,3\n".to_vec();
            let url = add_upload(&shell.uploads, name, bytes).await.unwrap();
            assert!(
                url.starts_with(&format!("memory://uploads/{kept}")),
                "{url}"
            );
            assert!(url.ends_with(".csv"), "{url}");
            let out = run(&shell, &format!("SELECT count(*) AS n FROM '{url}';")).await;
            assert!(
                out["out"].as_str().unwrap().contains("│     2 │"),
                "{name}: {out}"
            );
            urls.push(url);
        }
        // Names that differ only in what was replaced are kept apart.
        urls.sort();
        urls.dedup();
        assert_eq!(urls.len(), 7, "{urls:?}");
        assert_eq!(upload_key("edges.csv"), "edges.csv");
        for bad in ["", "a/b.csv", "..", ".hidden"] {
            assert!(
                add_upload(&shell.uploads, bad, vec![]).await.is_err(),
                "{bad}"
            );
        }
    }

    fn ids(g: &GraphView) -> Vec<&str> {
        g.result.node_keys().iter().collect()
    }

    /// The whole of `.graph`, through the shell's SQL and its last result.
    /// What `graph::view` draws of each source and style is tested in
    /// `ursa-engine` (`tests/graph_view.rs`).
    #[tokio::test]
    async fn graph_draws_the_last_result_a_query_or_a_property_graph() {
        let shell = WebShell::new();
        let err = graph_of(&shell.shell, "").await.unwrap_err();
        assert!(err.to_string().contains("no result to draw yet"), "{err}");

        run(&shell, "CREATE VIEW people AS SELECT * FROM (VALUES ('ann'), ('bob'), ('cy'), ('dee')) AS t(name);").await;
        run(&shell, "CREATE VIEW knows AS SELECT * FROM (VALUES ('bob', 'ann', 1), ('cy', 'bob', 2)) AS t(a, b, w);").await;
        run(&shell, "SELECT w, a AS Src, b AS DST FROM knows;").await;
        // The last result: src and dst by name, not position.
        let g = graph_of(&shell.shell, "").await.unwrap();
        assert_eq!(ids(&g), ["bob", "ann", "cy"]);
        assert_eq!((g.result.src(), g.result.dst()), (&[0, 2][..], &[1, 0][..]));
        assert_eq!(g.result.provenance().rule.as_deref(), Some("edges"));

        // A query, with or without its semicolon; the first two columns.
        let g = graph_of(&shell.shell, " SELECT b, a FROM knows; ")
            .await
            .unwrap();
        assert_eq!(ids(&g), ["ann", "bob", "cy"]);
        assert_eq!(
            g.result.provenance().sql.as_deref(),
            Some("SELECT b, a FROM knows")
        );
        // Ursa's dialect: a FROM-first query.
        let g = graph_of(&shell.shell, "FROM knows").await.unwrap();
        assert_eq!(g.result.n_nodes(), 3);
        // A table by name.
        let g = graph_of(&shell.shell, "knows").await.unwrap();
        assert_eq!(g.result.n_nodes(), 3);

        // A property graph: its vertices first, so dee, with no edges, is drawn.
        run(&shell, "CREATE PROPERTY GRAPH social VERTEX TABLES (people KEY (name)) EDGE TABLES (knows SOURCE KEY (a) REFERENCES people (name) DESTINATION KEY (b) REFERENCES people (name));").await;
        let g = graph_of(&shell.shell, "Social").await.unwrap();
        assert_eq!(ids(&g), ["ann", "bob", "cy", "dee"]);
        assert_eq!((g.result.src(), g.result.dst()), (&[1, 2][..], &[0, 1][..]));
        assert_eq!(g.result.graph(), Some("social"));

        // What cannot be drawn says why.
        for (text, says) in [
            ("nothing_here", "no graph or table named 'nothing_here'"),
            ("SELECT 1 AS x", "1 column"),
            ("SELECT * FROM knows WHERE false", "no rows"),
            ("SELECT nope", "nope"),
            ("-- nothing", "no query to draw"),
        ] {
            let err = graph_of(&shell.shell, text).await.unwrap_err();
            assert!(
                ursa_engine::user_message(&err).contains(says),
                "{text}: {err}"
            );
        }
        run(&shell, "SELECT 42 AS answer;").await;
        let err = graph_of(&shell.shell, "").await.unwrap_err();
        assert!(err.to_string().contains("1 column"), "{err}");
    }

    /// An explore command changes the shell's open graph and gives the page
    /// what it printed and the change as UGR bytes. The commands themselves
    /// are tested in `ursa-shell` (`explore_tests.rs`).
    #[tokio::test]
    async fn explore_gives_the_page_a_line_and_a_delta() {
        use ursa_viz::model::{GraphDelta, GraphResult};
        let shell = WebShell::new();
        let explore =
            |command: &'static str| explore(&shell.shell, &shell.out, &shell.err, command);
        // The commands are the shell's own: listed, and handed back.
        let help = run(&shell, ".help").await;
        assert!(
            help["out"].as_str().unwrap().contains(".expand ID"),
            "{help}"
        );
        let back = run(&shell, ".expand 3;").await;
        assert_eq!(
            (back["flow"].as_str(), back["command"].as_str()),
            (Some("host"), Some(".expand 3"))
        );
        let none = explore(".expand 3").await.unwrap();
        assert!(none.err.contains("Error: no graph is open"), "{}", none.err);
        assert!(none.out.is_empty() && none.delta.is_empty());

        run(&shell, "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2), (2, 3), (3, 1), (3, 4), (4, 5)) AS t(src, dst);").await;
        run(
            &shell,
            "CREATE PROPERTY GRAPH club EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
        )
        .await;
        let g = graph_of(
            &shell.shell,
            "SELECT * FROM hop(club, seeds => [1], direction => 'both')",
        )
        .await
        .unwrap();
        let mut view = GraphResult::from_bytes(&g.result.to_bytes()).unwrap();
        assert_eq!((view.n_nodes(), view.n_edges()), (3, 3));

        let mut expanded = explore(".expand 3").await.unwrap();
        assert_eq!(
            (expanded.out(), expanded.err()),
            ("+1 node, +1 edge\n".to_string(), String::new())
        );
        assert_eq!((expanded.nodes(), expanded.edges()), (4, 4));
        let bytes = expanded.take_delta();
        assert!(expanded.take_delta().is_empty());
        view.apply(&GraphDelta::from_bytes(&bytes).unwrap())
            .unwrap();
        assert_eq!(
            view.node_keys().iter().collect::<Vec<_>>(),
            ["1", "2", "3", "4"]
        );

        let mut undone = explore(".undo").await.unwrap();
        assert_eq!(undone.out(), "Undid .expand 3: -1 node, -1 edge\n");
        view.apply(&GraphDelta::from_bytes(&undone.take_delta()).unwrap())
            .unwrap();
        assert_eq!(view.to_bytes(), g.result.to_bytes());

        let missing = explore(".hide 9").await.unwrap();
        assert!(
            missing
                .err()
                .contains("no node '9' in the graph that is drawn"),
            "{}",
            missing.err()
        );
        assert_eq!((missing.nodes(), missing.edges()), (3, 3));
        shell.close_graph().unwrap();
        assert!(explore(".undo")
            .await
            .unwrap()
            .err()
            .contains("no graph is open"));
    }

    /// `.find` gives the page what it printed and the nodes to select. The
    /// command itself is tested in `ursa-shell` (`find_tests.rs`).
    #[tokio::test]
    async fn find_gives_the_page_a_line_and_the_nodes_to_select() {
        let shell = WebShell::new();
        let find = |command: &'static str| find(&shell.shell, &shell.out, &shell.err, command);
        let back = run(&shell, ".find 3;").await;
        assert_eq!(
            (back["flow"].as_str(), back["command"].as_str()),
            (Some("host"), Some(".find 3"))
        );
        let none = find(".find 3").await.unwrap();
        assert!(none.err.contains("Error: no graph is open"), "{}", none.err);
        assert!(none.selected.is_empty() && none.not_shown == "[]");

        run(&shell, "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2), (2, 3), (3, 1), (3, 4), (4, 35)) AS t(src, dst);").await;
        run(
            &shell,
            "CREATE PROPERTY GRAPH club EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
        )
        .await;
        graph_of(
            &shell.shell,
            "SELECT * FROM hop(club, seeds => [1], direction => 'both')",
        )
        .await
        .unwrap();
        let mut found = find(".find 3").await.unwrap();
        assert_eq!(
            (found.out(), found.err()),
            (
                "Selected 1 node\nNot shown: 35\n".to_string(),
                String::new()
            )
        );
        assert_eq!((found.matches(), found.not_shown_total()), (1, 1));
        assert_eq!(found.not_shown(), r#"[{"hidden":false,"key":"35"}]"#);
        // Node 3 is the view's third: hop gives 1, 2, 3.
        assert_eq!(found.take_selected(), [2]);
        assert!(found.take_selected().is_empty());
    }

    #[tokio::test]
    async fn inspect_gives_the_page_a_row_as_json() {
        let shell = WebShell::new();
        let inspect = |what, neighbours| inspect(&shell.shell, what, neighbours);
        let node = Inspect::Node { node: 0, key: "1" };
        let none = inspect(node, false).await.unwrap_err();
        assert!(none.to_string().contains("no graph is open"), "{none}");

        run(&shell, "CREATE TABLE people AS SELECT * FROM (VALUES (1, '<i>Ann</i>'), (2, NULL)) AS t(id, name);").await;
        run(
            &shell,
            "CREATE TABLE calls AS SELECT * FROM (VALUES (1, 2, 0.5)) AS t(src, dst, hours);",
        )
        .await;
        run(
            &shell,
            "CREATE PROPERTY GRAPH phone VERTEX TABLES (people KEY (id)) EDGE TABLES (calls \
             SOURCE KEY (src) REFERENCES people (id) DESTINATION KEY (dst) REFERENCES people (id));",
        )
        .await;
        run(&shell, "SELECT 41 AS kept;").await;
        graph_of(&shell.shell, "phone").await.unwrap();

        let row: serde_json::Value =
            serde_json::from_str(&inspect(node, false).await.unwrap()).unwrap();
        assert_eq!(
            row,
            json!({
                "table": "people",
                "sql": "SELECT * FROM people WHERE id = 1",
                "columns": [{"name": "id", "numeric": true}, {"name": "name", "numeric": false}],
                "rows": [["1", "<i>Ann</i>"]],
                "row": null,
                "note": null,
            })
        );
        let null = Inspect::Node { node: 1, key: "2" };
        let row: serde_json::Value =
            serde_json::from_str(&inspect(null, false).await.unwrap()).unwrap();
        assert_eq!(row["rows"], json!([["2", null]]));
        assert_eq!(inspect(node, true).await.unwrap(), r#"{"neighbours":1}"#);

        let edge = Inspect::Edge {
            edge: 0,
            src: "1",
            dst: "2",
        };
        let row: serde_json::Value =
            serde_json::from_str(&inspect(edge, false).await.unwrap()).unwrap();
        assert_eq!(row["table"], "calls");
        assert_eq!(row["rows"], json!([["1", "2", "0.5"]]));

        // The page's picture is not the engine's: an error, not another row.
        let stale = Inspect::Node { node: 0, key: "2" };
        assert!(inspect(stale, false).await.is_err());
        // Nothing was printed, and the last result is the user's still.
        assert_eq!(shell.out.take(), "");
        let last = shell.shell.borrow();
        let (schema, _) = last.last_result().unwrap();
        assert_eq!(schema.field(0).name(), "kept");
    }

    #[tokio::test]
    async fn graph_styles_reach_the_page_as_ugr_bytes() {
        use ursa_viz::model::{GraphResult, Values};
        let shell = WebShell::new();
        // Two triangles joined by one edge: two communities.
        run(&shell, "CREATE VIEW people AS SELECT * FROM (VALUES (1, 'red', 10, 'a'), (2, 'red', 20, 'b'), (3, 'red', 30, 'c'), (4, 'blue', 40, 'd'), (5, 'blue', 50, 'e'), (6, 'blue', 60, 'f')) AS t(id, side, age, name);").await;
        run(&shell, "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2), (2, 3), (3, 1), (4, 5), (5, 6), (6, 4), (3, NULL), (3, 4)) AS t(src, dst);").await;
        run(&shell, "CREATE PROPERTY GRAPH club VERTEX TABLES (people KEY (id)) EDGE TABLES (ties SOURCE KEY (src) REFERENCES people (id) DESTINATION KEY (dst) REFERENCES people (id));").await;

        // Algorithms, run on the graph through the shell's SQL, joined on id.
        let g = graph_of(&shell.shell, "club SIZE BY pagerank COLOR BY louvain")
            .await
            .unwrap();
        assert_eq!(g.legend().unwrap().counts, vec![3, 3]);
        // The page's copy: the bytes once, and what was skipped.
        let view = g.clone();
        let mut data = GraphData::from(g);
        assert_eq!(data.skipped(), 1);
        let bytes = data.take_ugr();
        assert!(data.take_ugr().is_empty());
        let read = GraphResult::from_bytes(&bytes).unwrap();
        assert_eq!(read, view.result);
        assert_eq!(read.encodings().size_by.as_deref(), Some("pagerank"));
        assert_eq!(read.encodings().color_by.as_deref(), Some("louvain"));
        let Values::F32(size) = &read.node_column("pagerank").unwrap().values else {
            panic!("pagerank is numbers");
        };
        // 3 and 4, the bridge's ends, rank highest.
        assert!(size[2] > size[0] && size.len() == 6, "{size:?}");

        // The last result keeps what its columns are, so the rules read it:
        // a hop's rings, with the graph's edges.
        run(
            &shell,
            "SELECT * FROM hop(club, seeds => [3], n => 1, direction => 'both');",
        )
        .await;
        let g = graph_of(&shell.shell, "").await.unwrap();
        assert_eq!(g.drawn_as(), "rings on club (seed, id, distance)");
        // 3 and its neighbours 1, 2 and 4: the triangle and the bridge.
        assert_eq!((g.result.n_nodes(), g.result.n_edges()), (4, 4));

        // A query with a subquery in NODES, and an error from a clause.
        let g = graph_of(&shell.shell, "FROM ties NODES (FROM people) SIZE BY age")
            .await
            .unwrap();
        assert_eq!(g.result.encodings().size_by.as_deref(), Some("age"));
        let err = graph_of(&shell.shell, "ties COLOR BY louvain")
            .await
            .unwrap_err();
        assert!(
            ursa_engine::user_message(&err)
                .contains("draw the graph by name (.graph NAME COLOR BY louvain)"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn the_last_result_is_kept_for_export() {
        let shell = WebShell::new();
        run(&shell, "SELECT * FROM (VALUES (1), (2), (3)) AS t(x);").await;
        let borrowed = shell.borrow().unwrap();
        let (_, batches) = borrowed.last_result().unwrap();
        assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 3);
    }
}
