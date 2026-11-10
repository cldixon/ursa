//! Ursa's SQL engine in the browser.
//!
//! The `ursa-sql` engine compiled to WebAssembly, with a small JavaScript API
//! (#278):
//!
//! ```js
//! import init, { Session, version } from "./ursa_wasm.js";
//! await init();
//! const session = new Session();
//! session.setBaseUrl(new URL("data/", location.href).href);
//! console.log(await session.sql("SELECT * FROM 'karate_edges.csv' LIMIT 3"));
//! ```
//!
//! Everything runs in the page (or a Web Worker), on one thread. Files are read
//! over HTTP with range requests, so a query downloads the parts of a Parquet
//! file it needs. Results are the native engine's, byte for byte: [`Session::sql`]
//! prints them with the same table printer as the `ursa` shell.
//!
//! [`WebShell`] is the `ursa` shell itself, for a page's terminal: fed a line
//! at a time, it prints what the CLI prints, dot-commands and all.
//!
//! For a user interface, [`Session::run`] returns each statement's result as
//! JSON (columns, the first rows formatted as the shell formats them, and the
//! table), and keeps the results so [`Session::export`] can write one as CSV,
//! JSON, Arrow or Parquet without running the query again (#279).

use std::sync::{Arc, Mutex};

use arrow::array::RecordBatch;
use arrow::csv::WriterBuilder as CsvWriterBuilder;
use arrow::datatypes::SchemaRef;
use arrow::ipc::writer::StreamWriter;
use arrow::json::ArrayWriter;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::error::{DataFusionError, Result};
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::prelude::SessionContext;
use futures::StreamExt;
use js_sys::{Promise, Uint8Array};
use serde_json::{json, Value};
use url::Url;
use ursa_shell::table::{Rows, Style, DEFAULT_MAX_ROWS};

mod shell;
pub use shell::WebShell;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

/// The engine's version.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// One statement's result: the rows of a query, or nothing for DDL and `SET`.
struct Stored {
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
}

/// One engine session: its views, graphs and settings live as long as it does.
#[wasm_bindgen]
pub struct Session {
    ctx: Arc<SessionContext>,
    /// The results of the last [`Session::run`], for [`Session::export`].
    last: Arc<Mutex<Vec<Stored>>>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl Session {
    /// A new session with Ursa's defaults.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Session {
        #[cfg(target_arch = "wasm32")]
        console_error_panic_hook::set_once();
        Session {
            ctx: Arc::new(ursa_engine::new_session()),
            last: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Set the URL relative paths resolve against, such as
    /// `https://example.com/data/` (end a directory with `/`). Applies to every
    /// session on the page.
    #[wasm_bindgen(js_name = setBaseUrl)]
    pub fn set_base_url(&self, base: &str) -> Result<(), JsError> {
        set_base_url(base)
    }

    /// Run `sql` (one or more statements) and resolve to every result as the
    /// `ursa` shell prints it: box tables, one after another. Statements that
    /// return no columns (DDL, `SET`) print nothing. A syntax error anywhere runs
    /// nothing. Rejects with the error message the shell would print.
    pub fn sql(&self, sql: String) -> Promise {
        let ctx = Arc::clone(&self.ctx);
        future_to_promise(async move {
            run_tables(&ctx, &sql)
                .await
                .map(|out| JsValue::from_str(&out))
                .map_err(js_error)
        })
    }

    /// Run `sql` and resolve to the last result that has columns, as an Arrow
    /// IPC stream (a `Uint8Array`), for a JavaScript Arrow reader.
    #[wasm_bindgen(js_name = sqlArrow)]
    pub fn sql_arrow(&self, sql: String) -> Promise {
        let ctx = Arc::clone(&self.ctx);
        future_to_promise(async move {
            let results = run_all(&ctx, &sql).await.map_err(js_error)?;
            let last = results
                .into_iter()
                .rev()
                .find(|r| !r.schema.fields().is_empty())
                .ok_or_else(|| {
                    js_error(DataFusionError::Plan(
                        "the SQL returned no result with columns".into(),
                    ))
                })?;
            encode(&last, "arrow")
                .map(|bytes| Uint8Array::from(bytes.as_slice()).into())
                .map_err(js_error)
        })
    }

    /// Run `sql` (one or more statements) and resolve to a JSON array with one
    /// entry per statement:
    ///
    /// ```json
    /// { "statement": "SELECT …", "columns": [{"name": "id", "type": "Int64"}],
    ///   "rows": [["33"], …], "totalRows": 34, "table": "┌────┐\n…" }
    /// ```
    ///
    /// `rows` holds at most `max_rows` rows, each value formatted as the shell
    /// formats it (`null` for a null); `table` is those rows as the shell's
    /// table. A statement with no columns (DDL, `SET`) has `columns: []`. The
    /// results are kept for [`Session::export`] until the next `run`. A syntax
    /// error anywhere runs nothing.
    pub fn run(&self, sql: String, max_rows: usize) -> Promise {
        let ctx = Arc::clone(&self.ctx);
        let last = Arc::clone(&self.last);
        future_to_promise(async move {
            let json = describe_all(&ctx, &sql, max_rows, &last)
                .await
                .map_err(js_error)?;
            Ok(JsValue::from_str(&json))
        })
    }

    /// Result `index` of the last [`Session::run`], written as `format`: `csv`
    /// (with a header row), `json` (an array of row objects), `arrow` (an IPC
    /// stream) or `parquet`. Every row, not just the ones `run` showed.
    pub fn export(&self, index: usize, format: &str) -> Result<Uint8Array, JsError> {
        let last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let stored = last
            .get(index)
            .ok_or_else(|| JsError::new(&format!("there is no result {index}")))?;
        let bytes =
            encode(stored, format).map_err(|e| JsError::new(&ursa_engine::user_message(&e)))?;
        Ok(Uint8Array::from(bytes.as_slice()))
    }
}

/// Set the URL relative paths resolve against, for every session on the page.
fn set_base_url(base: &str) -> Result<(), JsError> {
    let base = Url::parse(base).map_err(|e| JsError::new(&format!("'{base}': {e}")))?;
    #[cfg(target_arch = "wasm32")]
    ursa_engine::paths::browser::set_base_url(Some(base));
    #[cfg(not(target_arch = "wasm32"))]
    let _ = base;
    Ok(())
}

fn js_error(err: DataFusionError) -> JsValue {
    JsError::new(&ursa_engine::user_message(&err)).into()
}

/// Each statement's text and result, in order. Statements that return no
/// columns are kept, with an empty schema.
async fn run_statements(ctx: &SessionContext, sql: &str) -> Result<Vec<(String, Stored)>> {
    // Parse everything first, as the shell does, so a syntax error runs nothing.
    let statements = ursa_sql::parse_sql_with_text_for(ctx, sql)?;
    let mut results = Vec::new();
    for (statement, text) in statements {
        let frame = ursa_sql::execute_with_text(ctx, statement, &text).await?;
        let mut stream = frame.execute_stream().await?;
        let schema = stream.schema();
        let mut batches = Vec::new();
        while let Some(batch) = stream.next().await {
            let batch = batch?;
            if batch.num_rows() > 0 {
                batches.push(batch);
            }
        }
        results.push((text, Stored { schema, batches }));
    }
    Ok(results)
}

/// The results of the statements that return columns, as [`Session::sql`] and
/// [`Session::sql_arrow`] read them.
async fn run_all(ctx: &SessionContext, sql: &str) -> Result<Vec<Stored>> {
    Ok(run_statements(ctx, sql)
        .await?
        .into_iter()
        .map(|(_, stored)| stored)
        .filter(|stored| !stored.schema.fields().is_empty())
        .collect())
}

/// `batches` as the shell's table, showing at most `max_rows` rows (0: every
/// row), with no width limit: what `ursa` prints when its output is not a
/// terminal.
fn table(
    schema: &SchemaRef,
    batches: &[RecordBatch],
    options: &FormatOptions,
    max_rows: usize,
) -> Result<String> {
    let mut rows = Rows::new(max_rows);
    for batch in batches {
        rows.push(batch.clone());
    }
    Ok(rows.finish(schema, options)?.render(Style::default()))
}

/// Every result as the shell's table format prints it (`ursa-cli`'s
/// `print::write_stream`, `-f table`, to a file or pipe): [`ursa_shell`]'s
/// table with the session's `datafusion.format` options and the default
/// number of rows.
async fn run_tables(ctx: &SessionContext, sql: &str) -> Result<String> {
    let results = run_all(ctx, sql).await?;
    let config = ctx.state().config().options().format.clone();
    let options: FormatOptions<'_> = (&config).try_into()?;
    let mut out = String::new();
    for stored in results {
        out.push_str(&table(
            &stored.schema,
            &stored.batches,
            &options,
            DEFAULT_MAX_ROWS,
        )?);
    }
    Ok(out)
}

/// [`Session::run`]: the results as JSON, kept in `last`.
async fn describe_all(
    ctx: &SessionContext,
    sql: &str,
    max_rows: usize,
    last: &Mutex<Vec<Stored>>,
) -> Result<String> {
    let results = run_statements(ctx, sql).await?;
    let config = ctx.state().config().options().format.clone();
    let options: FormatOptions<'_> = (&config).try_into()?;
    let mut out = Vec::with_capacity(results.len());
    let mut kept = Vec::with_capacity(results.len());
    for (text, stored) in results {
        out.push(describe(&text, &stored, max_rows, &options)?);
        kept.push(stored);
    }
    *last.lock().unwrap_or_else(|e| e.into_inner()) = kept;
    Ok(Value::Array(out).to_string())
}

/// One result as JSON: its columns, its first `max_rows` rows formatted as the
/// shell formats them, and those rows as the shell's table.
fn describe(
    text: &str,
    stored: &Stored,
    max_rows: usize,
    options: &FormatOptions,
) -> Result<Value> {
    let columns: Vec<Value> = stored
        .schema
        .fields()
        .iter()
        .map(|f| json!({ "name": f.name(), "type": f.data_type().to_string() }))
        .collect();
    let total: usize = stored.batches.iter().map(RecordBatch::num_rows).sum();
    let mut rows = Vec::with_capacity(total.min(max_rows));
    let mut shown = Vec::new();
    for batch in &stored.batches {
        let take = (max_rows - rows.len()).min(batch.num_rows());
        if take == 0 {
            break;
        }
        let batch = batch.slice(0, take);
        {
            let formatters = batch
                .columns()
                .iter()
                .map(|c| ArrayFormatter::try_new(c.as_ref(), options))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for row in 0..batch.num_rows() {
                let cells: Vec<Value> = batch
                    .columns()
                    .iter()
                    .zip(&formatters)
                    .map(|(column, formatter)| {
                        if column.is_null(row) {
                            Value::Null
                        } else {
                            Value::String(formatter.value(row).to_string())
                        }
                    })
                    .collect();
                rows.push(Value::Array(cells));
            }
        }
        shown.push(batch);
    }
    let table = if columns.is_empty() {
        String::new()
    } else {
        table(&stored.schema, &shown, options, 0)?
    };
    Ok(json!({
        "statement": text,
        "columns": columns,
        "rows": rows,
        "totalRows": total,
        "table": table,
    }))
}

/// `stored` written as `format`.
fn encode(stored: &Stored, format: &str) -> Result<Vec<u8>> {
    let schema = &stored.schema;
    let mut bytes = Vec::new();
    match format {
        "csv" => {
            let mut writer = CsvWriterBuilder::new().with_header(true).build(&mut bytes);
            if stored.batches.is_empty() {
                writer.write(&RecordBatch::new_empty(Arc::clone(schema)))?;
            }
            for batch in &stored.batches {
                writer.write(batch)?;
            }
        }
        "json" => {
            let mut writer = ArrayWriter::new(&mut bytes);
            for batch in &stored.batches {
                writer.write(batch)?;
            }
            writer.finish()?;
        }
        "arrow" => {
            let mut writer = StreamWriter::try_new(&mut bytes, schema)?;
            for batch in &stored.batches {
                writer.write(batch)?;
            }
            writer.finish()?;
        }
        "parquet" => {
            let mut writer = ArrowWriter::try_new(&mut bytes, Arc::clone(schema), None)?;
            for batch in &stored.batches {
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn session_with(sql: &str) -> (SessionContext, Vec<(String, Stored)>) {
        let ctx = ursa_engine::new_session();
        let results = run_statements(&ctx, sql).await.unwrap();
        (ctx, results)
    }

    #[tokio::test]
    async fn describe_formats_rows_as_the_shell_does_and_caps_them() {
        let (ctx, results) = session_with(
            "CREATE VIEW v AS SELECT * FROM (VALUES (1, 'a', 0.5), (2, NULL, 1.25), (3, 'c', 2.0)) t(n, s, x);
             SELECT * FROM v ORDER BY n",
        )
        .await;
        let config = ctx.state().config().options().format.clone();
        let options: FormatOptions<'_> = (&config).try_into().unwrap();
        let ddl = describe(&results[0].0, &results[0].1, 2, &options).unwrap();
        assert_eq!(ddl["columns"], json!([]));
        let rows = describe(&results[1].0, &results[1].1, 2, &options).unwrap();
        assert_eq!(rows["totalRows"], 3);
        assert_eq!(
            rows["rows"],
            json!([["1", "a", "0.5"], ["2", null, "1.25"]])
        );
        assert_eq!(rows["columns"][2], json!({"name": "x", "type": "Float64"}));
        assert!(rows["table"].as_str().unwrap().starts_with("┌───"));
    }

    #[tokio::test]
    async fn exports_every_row_in_each_format() {
        let (_, mut results) =
            session_with("SELECT * FROM (VALUES (1, 'a'), (2, 'b')) t(n, s)").await;
        let stored = results.remove(0).1;
        let csv = String::from_utf8(encode(&stored, "csv").unwrap()).unwrap();
        assert_eq!(csv, "n,s\n1,a\n2,b\n");
        let json = String::from_utf8(encode(&stored, "json").unwrap()).unwrap();
        assert_eq!(json, r#"[{"n":1,"s":"a"},{"n":2,"s":"b"}]"#);
        let parquet = encode(&stored, "parquet").unwrap();
        assert_eq!(&parquet[..4], b"PAR1");
        let arrow = encode(&stored, "arrow").unwrap();
        assert!(!arrow.is_empty());
        assert!(encode(&stored, "xlsx").is_err());
    }
}
