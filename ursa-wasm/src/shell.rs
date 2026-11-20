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

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;

use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;
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

use crate::graph::{self, EdgeList, Source};
use crate::{encode, js_error, Stored};

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
    /// The shell's session (the same state), for [`WebShell::graph`].
    ctx: SessionContext,
}

/// A graph for the page's Graph view, from [`WebShell::graph`]: `n` nodes,
/// the edges `(src[i], dst[i])` as dense node indexes, and each node's id.
#[wasm_bindgen]
pub struct GraphData(EdgeList);

#[wasm_bindgen]
impl GraphData {
    /// The number of nodes.
    #[wasm_bindgen(getter)]
    pub fn n(&self) -> u32 {
        self.0.n
    }

    /// Each edge's source node, `0..n` (a `Uint32Array`).
    #[wasm_bindgen(getter)]
    pub fn src(&self) -> Vec<u32> {
        self.0.src.clone()
    }

    /// Each edge's destination node (a `Uint32Array`).
    #[wasm_bindgen(getter)]
    pub fn dst(&self) -> Vec<u32> {
        self.0.dst.clone()
    }

    /// Each node's id, as the shell prints it, by node index.
    #[wasm_bindgen(getter)]
    pub fn ids(&self) -> Vec<String> {
        self.0.ids.clone()
    }

    /// Rows left out because an endpoint is NULL.
    #[wasm_bindgen(getter)]
    pub fn skipped(&self) -> u32 {
        self.0.skipped as u32
    }

    /// `SIZE BY`'s value for each node (a `Float32Array`, NaN for a node with
    /// none), or `undefined` when not asked.
    #[wasm_bindgen(getter)]
    pub fn size(&self) -> Option<Vec<f32>> {
        self.0.size.as_ref().map(|s| s.values.clone())
    }

    /// What sizes the nodes, as typed after `SIZE BY`, or `undefined`.
    #[wasm_bindgen(getter, js_name = sizeBy)]
    pub fn size_by(&self) -> Option<String> {
        self.0.size.as_ref().map(|s| s.by.clone())
    }

    /// `COLOR BY`'s colour index for each node (a `Uint32Array`): `0..k` for
    /// the `k` largest categories (at most 8), most nodes first, and `k`
    /// for the rest and for nodes with no value. `undefined` when not asked.
    #[wasm_bindgen(getter)]
    pub fn color(&self) -> Option<Vec<u32>> {
        self.0.color.as_ref().map(|c| c.index.clone())
    }

    /// What colours the nodes, as typed after `COLOR BY`, or `undefined`.
    #[wasm_bindgen(getter, js_name = colorBy)]
    pub fn color_by(&self) -> Option<String> {
        self.0.color.as_ref().map(|c| c.by.clone())
    }

    /// The legend of [`Self::color`]: each coloured category's value, as the
    /// shell prints it, by colour index (empty when not asked).
    #[wasm_bindgen(getter, js_name = colorLabels)]
    pub fn color_labels(&self) -> Vec<String> {
        self.0
            .color
            .as_ref()
            .map_or_else(Vec::new, |c| c.labels.clone())
    }

    /// How many nodes each coloured category has, by colour index.
    #[wasm_bindgen(getter, js_name = colorCounts)]
    pub fn color_counts(&self) -> Vec<u32> {
        self.0
            .color
            .as_ref()
            .map_or_else(Vec::new, |c| c.counts.clone())
    }

    /// Nodes in the categories past the coloured ones ("other").
    #[wasm_bindgen(getter, js_name = colorOther)]
    pub fn color_other(&self) -> u32 {
        self.0.color.as_ref().map_or(0, |c| c.other)
    }

    /// Nodes with no value to colour them by.
    #[wasm_bindgen(getter, js_name = colorMissing)]
    pub fn color_missing(&self) -> u32 {
        self.0.color.as_ref().map_or(0, |c| c.missing)
    }
}

impl Default for WebShell {
    fn default() -> Self {
        Self::new()
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
        shell.set_version(env!("CARGO_PKG_VERSION"));
        WebShell {
            shell: Rc::new(RefCell::new(shell)),
            out,
            err,
            width,
            uploads,
            ctx,
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
    /// `text` is `[SOURCE] [NODES (query)] [SIZE BY x] [COLOR BY y]`. SOURCE
    /// is empty for the last result, a property graph's name for its edges
    /// (and its vertices' keys), a table's name for its rows, or a query.
    /// Each row is an edge from its `src` column to its `dst` column (in any
    /// case), or else from its first column to its second; the ids, of any
    /// type, become node indexes in the order they are first seen.
    ///
    /// `SIZE BY x` and `COLOR BY y` give each node a value
    /// ([`GraphData::size`], [`GraphData::color`] and its legend). A name is,
    /// in turn: a column of `NODES (SELECT id, … FROM …)`; a column of the
    /// property graph's vertex tables; `degree`; or a per-node algorithm
    /// (`pagerank`, `louvain`, `betweenness`, …) run on the property graph
    /// through `algorithms(G, …)`, undirected and seeded. Rows (a query, a
    /// table, the last result) take NODES's columns and degree only.
    ///
    /// Rejects, with a message to show, when there is nothing to draw, more
    /// than 2,000,000 edges, or a name or clause that does not fit.
    pub fn graph(&self, text: String) -> Promise {
        let ctx = self.ctx.clone();
        let last = match self.borrow() {
            Ok(shell) => shell.last_result().cloned(),
            Err(e) => return Promise::reject(&e.into()),
        };
        future_to_promise(async move {
            graph_of(&ctx, last, &text)
                .await
                .map(|g| GraphData(g).into())
                .map_err(|e| JsValue::from_str(&ursa_engine::user_message(&e)))
        })
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
        let mut shell = shell.try_borrow_mut().map_err(|_| {
            DataFusionError::Execution("the shell is already running a line".into())
        })?;
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

/// What [`WebShell::graph`] draws, given the shell's last result.
async fn graph_of(
    ctx: &SessionContext,
    last: Option<(arrow::datatypes::SchemaRef, Vec<arrow::array::RecordBatch>)>,
    text: &str,
) -> Result<EdgeList> {
    let (text, style) = graph::parse(text)?;
    let source = if !text.is_empty() {
        Source::of_text(ctx, &text).await?
    } else {
        let Some((schema, batches)) = last else {
            return Err(DataFusionError::Plan(
                "no result to draw yet: run a query whose rows are edges (columns src and dst, \
                 or else the first two), then .graph; or .graph NAME for a property graph"
                    .into(),
            ));
        };
        Source::Rows(schema, batches)
    };
    graph::draw(ctx, source, &style).await
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

    #[tokio::test]
    async fn graph_draws_the_last_result_a_query_or_a_property_graph() {
        let shell = WebShell::new();
        let last = || shell.borrow().unwrap().last_result().cloned();
        let err = graph_of(&shell.ctx, last(), "").await.unwrap_err();
        assert!(err.to_string().contains("no result to draw yet"), "{err}");

        run(&shell, "CREATE VIEW people AS SELECT * FROM (VALUES ('ann'), ('bob'), ('cy'), ('dee')) AS t(name);").await;
        run(&shell, "CREATE VIEW knows AS SELECT * FROM (VALUES ('bob', 'ann', 1), ('cy', 'bob', 2)) AS t(a, b, w);").await;
        run(&shell, "SELECT w, a AS Src, b AS DST FROM knows;").await;
        // The last result: src and dst by name, not position.
        let g = graph_of(&shell.ctx, last(), "").await.unwrap();
        assert_eq!(g.ids, vec!["bob", "ann", "cy"]);
        assert_eq!((g.src, g.dst), (vec![0, 2], vec![1, 0]));

        // A query, with or without its semicolon; the first two columns.
        let g = graph_of(&shell.ctx, last(), " SELECT b, a FROM knows; ")
            .await
            .unwrap();
        assert_eq!(g.ids, vec!["ann", "bob", "cy"]);
        let g = graph_of(&shell.ctx, last(), "FROM knows").await.unwrap();
        assert_eq!(g.n, 3);
        // A table by name.
        assert_eq!(graph_of(&shell.ctx, last(), "knows").await.unwrap().n, 3);

        // A property graph: its vertices first, so dee, with no edges, is drawn.
        run(&shell, "CREATE PROPERTY GRAPH social VERTEX TABLES (people KEY (name)) EDGE TABLES (knows SOURCE KEY (a) REFERENCES people (name) DESTINATION KEY (b) REFERENCES people (name));").await;
        let g = graph_of(&shell.ctx, last(), "Social").await.unwrap();
        assert_eq!(g.ids, vec!["ann", "bob", "cy", "dee"]);
        assert_eq!((g.src, g.dst), (vec![1, 2], vec![0, 1]));

        // What cannot be drawn says why.
        for (text, says) in [
            ("nothing_here", "no graph or table named 'nothing_here'"),
            ("SELECT 1 AS x", "1 column"),
            ("SELECT * FROM knows WHERE false", "no rows"),
            ("SELECT nope", "nope"),
        ] {
            let err = graph_of(&shell.ctx, last(), text).await.unwrap_err();
            assert!(
                ursa_engine::user_message(&err).contains(says),
                "{text}: {err}"
            );
        }
        run(&shell, "SELECT 42 AS answer;").await;
        let err = graph_of(&shell.ctx, last(), "").await.unwrap_err();
        assert!(err.to_string().contains("1 column"), "{err}");
    }

    #[tokio::test]
    async fn graph_styles_size_and_colour_nodes() {
        let shell = WebShell::new();
        let last = || shell.borrow().unwrap().last_result().cloned();
        // Two triangles joined by one edge: two communities.
        run(&shell, "CREATE VIEW people AS SELECT * FROM (VALUES (1, 'red', 10, 'a'), (2, 'red', 20, 'b'), (3, 'red', 30, 'c'), (4, 'blue', 40, 'd'), (5, 'blue', 50, 'e'), (6, 'blue', 60, 'f')) AS t(id, side, age, name);").await;
        run(&shell, "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2), (2, 3), (3, 1), (4, 5), (5, 6), (6, 4), (3, 4)) AS t(src, dst);").await;
        run(&shell, "CREATE PROPERTY GRAPH club VERTEX TABLES (people KEY (id)) EDGE TABLES (ties SOURCE KEY (src) REFERENCES people (id) DESTINATION KEY (dst) REFERENCES people (id));").await;

        // Algorithms, run on the graph, joined on id.
        let g = graph_of(&shell.ctx, last(), "club SIZE BY pagerank COLOR BY louvain")
            .await
            .unwrap();
        let size = g.size.unwrap();
        assert_eq!(size.by, "pagerank");
        assert_eq!(size.values.len(), 6);
        assert!(
            size.values.iter().all(|v| v.is_finite() && *v > 0.0),
            "{size:?}"
        );
        // 3 and 4, the bridge's ends, rank highest.
        assert!(size.values[2] > size.values[0], "{size:?}");
        let color = g.color.unwrap();
        assert_eq!(color.counts, vec![3, 3]);
        assert_eq!(color.index[..3], [color.index[0]; 3]);
        assert_eq!(color.index[3..], [color.index[3]; 3]);
        assert_ne!(color.index[0], color.index[3]);

        // Vertex columns come before algorithms; degree is the drawing's.
        let g = graph_of(&shell.ctx, last(), "club SIZE BY age COLOR BY Side")
            .await
            .unwrap();
        assert_eq!(
            g.size.unwrap().values,
            vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0]
        );
        let color = g.color.unwrap();
        assert_eq!(
            (color.labels, color.counts),
            (vec!["red".to_string(), "blue".into()], vec![3, 3])
        );
        let g = graph_of(&shell.ctx, last(), "club SIZE BY degree")
            .await
            .unwrap();
        assert_eq!(g.size.unwrap().values, vec![2.0, 2.0, 3.0, 3.0, 2.0, 2.0]);

        // NODES gives values for rows, and wins over the graph's own.
        let g = graph_of(
            &shell.ctx,
            last(),
            "SELECT src, dst FROM ties NODES (SELECT id, id * 1.5 AS score, CASE WHEN id < 4 THEN 'x' END AS tag FROM people WHERE id <> 6) SIZE BY score COLOR BY tag",
        )
        .await
        .unwrap();
        let size = g.size.unwrap();
        assert_eq!(&size.values[..5], &[1.5, 3.0, 4.5, 6.0, 7.5]);
        assert!(size.values[5].is_nan());
        assert_eq!(size.missing, 1);
        let color = g.color.unwrap();
        assert_eq!((color.labels, color.missing), (vec!["x".to_string()], 3));
        let g = graph_of(
            &shell.ctx,
            last(),
            "club NODES (SELECT id, 7 AS age FROM people) SIZE BY age",
        )
        .await
        .unwrap();
        assert_eq!(g.size.unwrap().values, vec![7.0; 6]);
        // Rows: degree, from the edges drawn.
        run(&shell, "SELECT * FROM ties;").await;
        let g = graph_of(&shell.ctx, last(), "SIZE BY degree")
            .await
            .unwrap();
        assert_eq!(g.size.unwrap().values.len(), 6);
        // The key itself is a vertex column too.
        let g = graph_of(&shell.ctx, last(), "club COLOR BY id")
            .await
            .unwrap();
        assert_eq!(g.color.unwrap().labels, vec!["1", "2", "3", "4", "5", "6"]);
        // Without a style, as before.
        let g = graph_of(&shell.ctx, last(), "club").await.unwrap();
        assert!(g.size.is_none() && g.color.is_none());

        for (text, says) in [
            ("club SIZE BY nope", "SIZE BY nope: graph 'club' has no vertex column or algorithm named 'nope'. Its vertex columns are age, id, name, side; the algorithms are degree, pagerank"),
            ("club COLOR BY wcc", "no vertex column or algorithm named 'wcc'"),
            ("club SIZE BY side", "SIZE BY side needs numbers, and side is text; colour nodes by it instead: COLOR BY side"),
            ("SELECT src, dst FROM ties SIZE BY pagerank", "SIZE BY pagerank runs pagerank() on a property graph, and this draws rows"),
            ("ties COLOR BY louvain", "draw the graph by name (.graph NAME COLOR BY louvain)"),
            ("SIZE BY age", "SIZE BY age: no column named 'age'. Rows drawn as edges are sized or coloured by degree, or by a column of NODES"),
            ("NODES (SELECT id, age FROM people) SIZE BY agee", "NODES (…) has age"),
            ("club NODES (SELECT id FROM people) SIZE BY id", "NODES (…) needs two columns or more"),
            ("club NODES (SELECT id, age FROM people)", "NODES (…) gives values for SIZE BY or COLOR BY to use"),
            ("club NODES (SELECT 'zz' AS id, 1 AS v) SIZE BY v", "SIZE BY v: no node has a value"),
            ("club SIZE BY", "SIZE BY needs"),
        ] {
            let err = graph_of(&shell.ctx, last(), text).await.unwrap_err();
            let message = ursa_engine::user_message(&err);
            assert!(message.contains(says), "{text}: {message}");
        }
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
