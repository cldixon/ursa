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

use std::sync::Arc;

use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use arrow::util::display::FormatOptions;
use arrow::util::pretty::pretty_format_batches_with_options;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;
use futures::StreamExt;
use js_sys::{Promise, Uint8Array};
use url::Url;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

/// The engine's version.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// One engine session: its views, graphs and settings live as long as it does.
#[wasm_bindgen]
pub struct Session {
    ctx: Arc<SessionContext>,
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
        }
    }

    /// Set the URL relative paths resolve against, such as
    /// `https://example.com/data/` (end a directory with `/`). Applies to every
    /// session on the page.
    #[wasm_bindgen(js_name = setBaseUrl)]
    pub fn set_base_url(&self, base: &str) -> Result<(), JsError> {
        let base = Url::parse(base).map_err(|e| JsError::new(&format!("'{base}': {e}")))?;
        #[cfg(target_arch = "wasm32")]
        ursa_engine::paths::browser::set_base_url(Some(base));
        #[cfg(not(target_arch = "wasm32"))]
        let _ = base;
        Ok(())
    }

    /// Run `sql` (one or more statements) and resolve to every result as the
    /// `ursa` shell prints it: ASCII tables, one after another. Statements that
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
            run_arrow(&ctx, &sql)
                .await
                .map(|bytes| Uint8Array::from(bytes.as_slice()).into())
                .map_err(js_error)
        })
    }
}

fn js_error(err: DataFusionError) -> JsValue {
    JsError::new(&ursa_engine::user_message(&err)).into()
}

/// The batches of each statement's result that has columns.
async fn run_all(ctx: &SessionContext, sql: &str) -> Result<Vec<Vec<RecordBatch>>> {
    // Parse everything first, as the shell does, so a syntax error runs nothing.
    let statements = ursa_sql::parse_sql_with_text_for(ctx, sql)?;
    let mut results = Vec::new();
    for (statement, text) in statements {
        let frame = ursa_sql::execute_with_text(ctx, statement, &text).await?;
        let mut stream = frame.execute_stream().await?;
        let schema = stream.schema();
        if schema.fields().is_empty() {
            continue;
        }
        let mut batches = Vec::new();
        while let Some(batch) = stream.next().await {
            let batch = batch?;
            if batch.num_rows() > 0 {
                batches.push(batch);
            }
        }
        if batches.is_empty() {
            batches.push(RecordBatch::new_empty(schema));
        }
        results.push(batches);
    }
    Ok(results)
}

/// Every result as the shell's table format prints it (`ursa-cli`'s
/// `print::write_stream`, `-f table`): Arrow's pretty printer with the
/// session's `datafusion.format` options, and a newline after each table.
async fn run_tables(ctx: &SessionContext, sql: &str) -> Result<String> {
    let results = run_all(ctx, sql).await?;
    let config = ctx.state().config().options().format.clone();
    let options: FormatOptions<'_> = (&config).try_into()?;
    let mut out = String::new();
    for batches in results {
        out.push_str(&pretty_format_batches_with_options(&batches, &options)?.to_string());
        out.push('\n');
    }
    Ok(out)
}

async fn run_arrow(ctx: &SessionContext, sql: &str) -> Result<Vec<u8>> {
    let batches = run_all(ctx, sql)
        .await?
        .pop()
        .ok_or_else(|| DataFusionError::Plan("the SQL returned no result with columns".into()))?;
    let mut bytes = Vec::new();
    let mut writer = StreamWriter::try_new(&mut bytes, &batches[0].schema())?;
    for batch in &batches {
        writer.write(batch)?;
    }
    writer.finish()?;
    drop(writer);
    Ok(bytes)
}
