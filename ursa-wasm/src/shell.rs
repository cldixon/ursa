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
        let mut shell = Shell::new(ctx, OutputFormat::Table, out.clone(), err.clone());
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

    /// `text` coloured as the shell colours what is typed (ANSI).
    pub fn highlight(text: &str) -> String {
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

/// Put `bytes` at `name` in `uploads`; the URL SQL reads it by.
pub(crate) async fn add_upload(uploads: &InMemory, name: &str, bytes: Vec<u8>) -> Result<String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return Err(DataFusionError::Plan(format!(
            "'{name}' is not a file name to add; give a name with no path, such as edges.csv"
        )));
    }
    uploads
        .put(&ObjectPath::from(name), PutPayload::from(bytes))
        .await
        .map_err(|e| DataFusionError::External(Box::new(e)))?;
    Ok(format!("memory://uploads/{name}"))
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
        for bad in ["", "a/b.csv", "..", ".hidden"] {
            assert!(
                add_upload(&shell.uploads, bad, vec![]).await.is_err(),
                "{bad}"
            );
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
