//! Files a session keeps in memory, and `COPY … TO` them (#517).
//!
//! A browser has no disk. The Ursa Shell on the web keeps the files a page
//! adds in an in-memory object store registered at [`URL`]
//! (`memory://uploads/`), and a session with that store has *session files*:
//!
//! - `COPY … TO 'pos.parquet'` writes `memory://uploads/pos.parquet`;
//! - `FROM 'pos.parquet'` reads it, before the name is looked for anywhere
//!   else.
//!
//! The write is Ursa's own. DataFusion's file sinks start tokio tasks to
//! split, serialize and upload what they write, and a browser has no tokio
//! runtime to start them on: the first `tokio::spawn` panics, which ends the
//! engine. [`copy_to`] runs the query, serializes its batches with the
//! writers DataFusion's sinks use (configured from the statement's options
//! the same way) and puts the bytes in the store, all in the task that asked.
//!
//! A native session can have the store for the files a page adds and still
//! write `COPY … TO` to disk ([`set_copy_to_disk`]), as `ursa serve` does:
//! its `COPY` is DataFusion's, under the session's root if it has one
//! (`crate::paths::write_target`). A native session without the store (the
//! CLI) is untouched: its `COPY` is DataFusion's.

use std::collections::HashMap;
use std::sync::Arc;

use datafusion::arrow::array::{RecordBatch, UInt64Array};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::ipc::writer::{FileWriter, IpcWriteOptions};
use datafusion::arrow::ipc::MetadataVersion;
use datafusion::common::config::{CsvOptions, JsonOptions, TableParquetOptions};
use datafusion::common::file_options::csv_writer::CsvWriterOptions;
use datafusion::common::parsers::CompressionTypeVariant;
use datafusion::common::tree_node::{TreeNode, TreeNodeRecursion};
use datafusion::dataframe::DataFrame;
use datafusion::datasource::file_format::arrow::ArrowFormat;
use datafusion::datasource::file_format::csv::{CsvFormat, CsvSerializer};
use datafusion::datasource::file_format::file_type_to_format;
use datafusion::datasource::file_format::json::{JsonFormat, JsonSerializer};
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::file_format::write::BatchSerializer;
use datafusion::datasource::listing::ListingTable;
use datafusion::datasource::source_as_provider;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionState;
use datafusion::execution::runtime_env::RuntimeEnv;
use datafusion::logical_expr::dml::CopyTo;
use datafusion::logical_expr::{LogicalPlan, WriteOp};
use datafusion::parquet::arrow::arrow_writer::ArrowWriterOptions;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::parquet::file::properties::WriterPropertiesBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use futures::TryStreamExt;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use url::Url;

/// Where a session's files live.
pub const URL: &str = "memory://uploads/";

/// The formats [`copy_to`] writes, as a refusal lists them.
const FORMATS: &str = ".parquet, .csv, .json and .arrow";

/// The session's file store, if it has one.
pub fn store(runtime: &RuntimeEnv) -> Option<Arc<dyn ObjectStore>> {
    let url = Url::parse(URL).expect("a valid URL");
    runtime.object_store_registry.get_store(&url).ok()
}

/// Whether `COPY … TO` in this session is [`copy_to`]'s: a session with
/// files that does not write its copies to disk ([`set_copy_to_disk`]), and
/// any session in the browser, where DataFusion's cannot run.
pub fn takes_copy(state: &SessionState) -> bool {
    cfg!(target_arch = "wasm32")
        || (store(state.runtime_env()).is_some() && !copies_to_disk(state.config()))
}

/// Whether a session's `COPY … TO` writes to disk though it has files.
#[derive(Debug)]
struct CopyToDisk(bool);

/// Make `COPY … TO` in the session `ctx` write to disk (`true`), as
/// DataFusion's does, though the session has files in memory for the ones a
/// page adds; or keep its copies with those files (`false`, the default).
/// The browser has no disk: there `COPY` always keeps the file in memory.
pub fn set_copy_to_disk(ctx: &SessionContext, on: bool) {
    let state = ctx.state_ref();
    let mut state = state.write();
    state.config_mut().set_extension(Arc::new(CopyToDisk(on)));
}

/// Whether the session with `config` writes `COPY … TO` to disk though it
/// has files ([`set_copy_to_disk`]).
pub fn copies_to_disk(config: &SessionConfig) -> bool {
    config.get_extension::<CopyToDisk>().is_some_and(|on| on.0)
}

/// Whether `name` is one a session file is kept under: ASCII letters, digits,
/// `.`, `_` and `-`, not starting with `.`. Such a name reads back from its
/// URL as it is written: a URL decodes `%`, ends its path at `#` and `?`, and
/// a location globs at `*`, `?` and `[`.
pub fn is_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// What a location is to the session's files.
enum Place<'a> {
    /// A session file's name: `pos.parquet`, or `memory://uploads/pos.parquet`.
    File(&'a str),
    /// A relative path that is no file name, or no name under [`URL`].
    NotAName,
    /// A URL somewhere else.
    Elsewhere,
}

fn place(location: &str) -> Place<'_> {
    let name = match location.strip_prefix(URL) {
        Some(name) => name,
        // A one-letter scheme is a Windows drive letter.
        None if matches!(Url::parse(location), Ok(url) if url.scheme().len() > 1) => {
            return Place::Elsewhere
        }
        None => location.strip_prefix("./").unwrap_or(location),
    };
    if is_file_name(name) {
        Place::File(name)
    } else {
        Place::NotAName
    }
}

/// The URL of the session file `location` names, if the session has it: what
/// `FROM 'pos.parquet'` reads after `COPY … TO 'pos.parquet'`. `None` for
/// any other location, which is read as it always was.
pub async fn existing(runtime: &RuntimeEnv, location: &str) -> Option<String> {
    let Place::File(name) = place(location) else {
        return None;
    };
    let store = store(runtime)?;
    store.head(&ObjectPath::from(name)).await.ok()?;
    Some(format!("{URL}{name}"))
}

/// The URL `COPY … TO 'target'` writes in a session whose `COPY` is
/// [`copy_to`]'s ([`takes_copy`]), or why it cannot be written.
pub fn copy_target(runtime: &RuntimeEnv, target: &str) -> Result<String> {
    let works = "COPY … TO 'name.parquet' (or .csv, .json, .arrow) keeps the file in this \
                 session, where FROM 'name.parquet' reads it";
    if store(runtime).is_none() {
        return Err(DataFusionError::Plan(format!(
            "COPY … TO '{target}' has nowhere to write: a browser has no disk, and this \
             session keeps no files. Run the query and export its result instead"
        )));
    }
    match place(target) {
        Place::File(name) => Ok(format!("{URL}{name}")),
        Place::NotAName => Err(DataFusionError::Plan(format!(
            "COPY … TO '{target}' cannot be written here: a browser has no disk, so a file \
             is kept in the session under a name of letters, digits, '.', '_' and '-', with \
             no folder. {works}"
        ))),
        Place::Elsewhere => Err(DataFusionError::Plan(format!(
            "COPY … TO '{target}' cannot be written from a browser, which writes only this \
             session's files. {works}"
        ))),
    }
}

/// Refuse a target whose extension names no format [`copy_to`] writes, when
/// no `STORED AS` names one (`stored_as`): DataFusion's refusal says only
/// that the extension is not registered.
pub fn check_copy_format(target: &str, stored_as: Option<&str>) -> Result<()> {
    if stored_as.is_some() {
        return Ok(());
    }
    let extension = target.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match extension.as_deref() {
        Some("parquet" | "csv" | "json" | "jsonl" | "ndjson" | "arrow") => Ok(()),
        _ => Err(DataFusionError::Plan(format!(
            "COPY … TO '{target}' names no format a browser session writes: end the name \
             in {}, or say STORED AS PARQUET (or CSV, JSON, ARROW)",
            FORMATS.replace(" and ", " or ")
        ))),
    }
}

/// Run `copy` into the session's files: its query's rows, written as its
/// format and options say, at its target ([`copy_target`] made it). The frame
/// is DataFusion's for a `COPY`: one row, `count`.
pub async fn copy_to(ctx: &SessionContext, copy: &CopyTo) -> Result<DataFrame> {
    let state = ctx.state();
    let url = copy_target(state.runtime_env(), &copy.output_url)?;
    let name = &url[URL.len()..];
    if !copy.partition_by.is_empty() {
        return Err(DataFusionError::Plan(
            "COPY … PARTITIONED BY writes a directory of files, and a browser session keeps \
             single files; leave PARTITIONED BY out"
                .into(),
        ));
    }
    // The options, read as DataFusion's physical planner reads them.
    let mut options: HashMap<String, String> = copy.options.clone();
    match options
        .remove("single_file_output")
        .as_deref()
        .map(str::trim)
    {
        None | Some("true") => {}
        Some(_) => {
            return Err(DataFusionError::Plan(
                "COPY … TO writes one file in a browser session; leave single_file_output out"
                    .into(),
            ))
        }
    }
    let format = file_type_to_format(&copy.file_type)?.create(&state, &options)?;
    // Ursa's Parquet format checks the files it reads around DataFusion's.
    let format = match format.downcast_ref::<ursa_io::parquet_footer::ValidParquet>() {
        Some(checked) => Arc::clone(checked.inner()),
        None => format,
    };
    let writer = if let Some(csv) = format.downcast_ref::<CsvFormat>() {
        Writer::csv(csv.options())?
    } else if let Some(json) = format.downcast_ref::<JsonFormat>() {
        Writer::json(json.options())?
    } else if let Some(parquet) = format.downcast_ref::<ParquetFormat>() {
        Writer::Parquet(Box::new(parquet.options().clone()))
    } else if format.is::<ArrowFormat>() {
        Writer::Arrow
    } else {
        return Err(DataFusionError::Plan(format!(
            "COPY … TO writes {FORMATS} files in a browser session, and '{}' is none of them",
            copy.file_type.get_ext()
        )));
    };

    let schema: SchemaRef = Arc::clone(copy.input.schema().inner());
    let frame = DataFrame::new(state, copy.input.as_ref().clone());
    let batches: Vec<RecordBatch> = frame.execute_stream().await?.try_collect().await?;
    let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
    let bytes = writer.write(&schema, batches)?;

    let store = store(ctx.state().runtime_env().as_ref())
        .ok_or_else(|| DataFusionError::Internal("the session's files are gone".into()))?;
    store
        .put(&ObjectPath::from(name), PutPayload::from(bytes))
        .await
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

    let count = Arc::new(Schema::new(vec![Field::new(
        "count",
        DataType::UInt64,
        false,
    )]));
    let batch = RecordBatch::try_new(count, vec![Arc::new(UInt64Array::from(vec![rows as u64]))])?;
    ctx.read_batch(batch)
}

/// Refuse an `INSERT INTO` a table of files, in a session whose `COPY` is
/// [`copy_to`]'s: the insert runs the file sinks a browser cannot.
pub fn check_insert(plan: &LogicalPlan) -> Result<()> {
    let mut refused = None;
    plan.apply(|node| {
        if let LogicalPlan::Dml(dml) = node {
            let files = source_as_provider(&dml.target)
                .is_ok_and(|table| table.downcast_ref::<ListingTable>().is_some());
            if files && matches!(dml.op, WriteOp::Insert(_)) {
                refused = Some(dml.table_name.to_string());
                return Ok(TreeNodeRecursion::Stop);
            }
        }
        Ok(TreeNodeRecursion::Continue)
    })?;
    match refused {
        None => Ok(()),
        Some(table) => Err(DataFusionError::Plan(format!(
            "INSERT INTO {table} cannot run in a browser session: {table} is a table of \
             files, and a browser cannot append to them. COPY (SELECT …) TO 'name.parquet' \
             writes a file, and CREATE TABLE … AS keeps rows in the session"
        ))),
    }
}

/// How a file is serialized.
enum Writer {
    Csv(CsvSerializer),
    Json,
    Parquet(Box<TableParquetOptions>),
    Arrow,
}

impl Writer {
    fn csv(options: &CsvOptions) -> Result<Writer> {
        let options = CsvWriterOptions::try_from(options)?;
        uncompressed("CSV", options.compression)?;
        let header = options.writer_options.header();
        Ok(Writer::Csv(
            CsvSerializer::new()
                .with_builder(options.writer_options)
                .with_header(header),
        ))
    }

    fn json(options: &JsonOptions) -> Result<Writer> {
        uncompressed("JSON", options.compression)?;
        Ok(Writer::Json)
    }

    /// The file's bytes. A result with no batch still writes its schema (a
    /// CSV its header).
    fn write(self, schema: &SchemaRef, mut batches: Vec<RecordBatch>) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        match self {
            Writer::Csv(serializer) => {
                if batches.is_empty() {
                    batches.push(RecordBatch::new_empty(Arc::clone(schema)));
                }
                for (i, batch) in batches.into_iter().enumerate() {
                    bytes.extend_from_slice(&serializer.serialize(batch, i == 0)?);
                }
            }
            // One object a line, as DataFusion writes JSON.
            Writer::Json => {
                let serializer = JsonSerializer::new();
                for (i, batch) in batches.into_iter().enumerate() {
                    bytes.extend_from_slice(&serializer.serialize(batch, i == 0)?);
                }
            }
            Writer::Parquet(options) => {
                // As DataFusion's sink: the Arrow schema goes in the file's
                // metadata through the options, unless they say to skip it.
                let mut options = *options;
                if !options.global.skip_arrow_metadata {
                    options.arrow_schema(schema);
                }
                let properties = WriterPropertiesBuilder::try_from(&options)?.build();
                let options = ArrowWriterOptions::new()
                    .with_properties(properties)
                    .with_skip_arrow_metadata(true);
                let mut writer =
                    ArrowWriter::try_new_with_options(&mut bytes, Arc::clone(schema), options)?;
                for batch in &batches {
                    writer.write(batch)?;
                }
                writer.close()?;
            }
            // An Arrow IPC file, not compressed (DataFusion's is, with LZ4)
            // and aligned to 8 bytes, not DataFusion's 64: its reader finds
            // the schema of a file in an object store 8 bytes after the
            // magic number, so it cannot read back the files its sink writes.
            Writer::Arrow => {
                let options = IpcWriteOptions::try_new(8, false, MetadataVersion::V5)?;
                let mut writer = FileWriter::try_new_with_options(&mut bytes, schema, options)?;
                for batch in &batches {
                    writer.write(batch)?;
                }
                writer.finish()?;
            }
        }
        Ok(bytes)
    }
}

/// Refuse a compressed text file: DataFusion compresses one as it uploads it,
/// in the tasks a browser cannot start.
fn uncompressed(format: &str, compression: CompressionTypeVariant) -> Result<()> {
    if compression == CompressionTypeVariant::UNCOMPRESSED {
        return Ok(());
    }
    Err(DataFusionError::Plan(format!(
        "COPY … TO does not write compressed {format} in a browser session; leave the \
         compression out, or write a .parquet file, which is compressed"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places() {
        assert!(matches!(place("pos.parquet"), Place::File("pos.parquet")));
        assert!(matches!(place("./pos.parquet"), Place::File("pos.parquet")));
        assert!(matches!(
            place("memory://uploads/pos.parquet"),
            Place::File("pos.parquet")
        ));
        for not in [
            "out/pos.parquet",
            "/tmp/pos.parquet",
            "../pos.parquet",
            "my file.csv",
            "p*.csv",
            ".hidden",
            "",
            "memory://uploads/a/b.csv",
            "memory://uploads/",
        ] {
            assert!(matches!(place(not), Place::NotAName), "{not}");
        }
        for elsewhere in [
            "https://example.com/pos.parquet",
            "s3://bucket/pos.parquet",
            "memory://other/pos.parquet",
            "file:///tmp/pos.parquet",
        ] {
            assert!(matches!(place(elsewhere), Place::Elsewhere), "{elsewhere}");
        }
    }
}
