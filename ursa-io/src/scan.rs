//! Reading edge/node files into Arrow through a DataFusion scan.
//!
//! This is the "scan produces the Arrow columns" half of the ingress story: a
//! Parquet or CSV path is read through DataFusion — which pushes the projection
//! into the file — and comes back as a `RecordBatch` ready for `build_topology`.
//!
//! Paths may be local, object storage (`s3://`, `gs://`, `az://`, and `file://`),
//! or a plain `http(s)://` URL (a single hosted file); [`crate::store`] registers
//! the matching `object_store` backend and keeps the session a scan runs on.
//! Projection pushdown still applies over the network — only the selected
//! columns' byte ranges are fetched via ranged GETs (for an HTTP store, when the
//! server honors range requests). CSV parsing and inference are in
//! [`crate::csv`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use arrow::array::{Array, RecordBatch};
use arrow::compute::{cast, filter};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::common::DFSchema;
use datafusion::datasource::listing::{ListingTable, ListingTableConfig, ListingTableUrl};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::options::ReadOptions;
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::{
    execute_stream_partitioned, ExecutionPlan, StatisticsArgs, StatisticsContext,
};
use datafusion::prelude::{DataFrame, ParquetReadOptions, SessionContext};
use futures::StreamExt;
use url::Url;
use ursa_core::{EdgeInterner, IdError, IdMap, Topology};

use crate::csv::{csv_value_error, read_csv, CsvOptions};
use crate::ids::{self, unview_field, EndpointTypeError};
use crate::parquet_stats::CheckedBounds;
use crate::store::{scan_state, StoreKey};
use crate::types::pa_name;

/// What a scan reads: its paths (each a file or a glob), read as one table in
/// the order given; the store settings; and how a CSV file parses.
#[derive(Clone, Copy)]
pub struct ScanSource<'a> {
    pub paths: &'a [String],
    pub storage_options: &'a HashMap<String, String>,
    pub csv: &'a CsvOptions,
}

impl<'a> ScanSource<'a> {
    pub fn new(
        paths: &'a [String],
        storage_options: &'a HashMap<String, String>,
        csv: &'a CsvOptions,
    ) -> Self {
        ScanSource {
            paths,
            storage_options,
            csv,
        }
    }

    /// The source as an error's label names it: `'edges.parquet'`, or a list
    /// `['a.csv', 'b.csv']`.
    fn shown(&self) -> String {
        match self.paths {
            [path] => format!("'{path}'"),
            paths => {
                let quoted: Vec<String> = paths.iter().map(|p| format!("'{p}'")).collect();
                format!("[{}]", quoted.join(", "))
            }
        }
    }
}

/// What each scan read, while recording is on (test/observability hook): the
/// scan's kind (`index`, `edges`, `table`, `nodes`), the columns it was asked
/// for, and how many filters it pushed into the read.
static SCAN_LOG: Mutex<Option<Vec<ScanRecord>>> = Mutex::new(None);

/// One logged scan: `(kind, columns, filters pushed)`.
pub type ScanRecord = (String, Vec<String>, usize);

fn log_scan(kind: &str, columns: &[String], pushed: usize) {
    // A dry run reads a file's schema, not its rows (see `crate::dry`).
    if crate::dry::active() {
        return;
    }
    let mut log = SCAN_LOG.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(log) = log.as_mut() {
        log.push((kind.to_string(), columns.to_vec(), pushed));
    }
}

/// Turn scan recording on (`true`, clearing the log) or off, returning what was
/// recorded since it was last turned on or read.
pub fn scan_log(record: bool) -> Vec<ScanRecord> {
    let mut log = SCAN_LOG.lock().unwrap_or_else(PoisonError::into_inner);
    let out = log.take().unwrap_or_default();
    if record {
        *log = Some(Vec::new());
    }
    out
}

/// The Parquet/CSV file format of a scan target, from its extension. Matches on
/// the raw lowercased path so an object-store glob wildcard (`part-?.parquet`,
/// `*.csv`) is preserved — `?`/`*` are glob metacharacters here, not URL syntax.
///
/// A query string on an `http(s)` URL (`…/edges.csv?token=…`) is a special,
/// clearly-rejected case: the scan engine derives the object location from the
/// URL *path* and drops the query before fetching, so a presigned/tokened URL
/// would silently fetch the unsigned path and fail — better to reject it with a
/// pointer to the object-store backends, which do sign requests.
#[derive(Debug)]
pub(crate) enum ScanFormat {
    Parquet,
    Csv,
}

/// Whether `path` is a CSV file (`false`: Parquet), or the error the scan `kind`
/// (`scan_edges`) of it raises, labelled as that scan labels it: the check a
/// scan runs, for the host to run where the scan is made.
pub fn is_csv(path: &str, kind: &'static str) -> Result<bool> {
    let paths = [path.to_string()];
    let (opts, csv) = (HashMap::new(), CsvOptions::default());
    let source = ScanSource::new(&paths, &opts, &csv);
    let format = detect_format(path).map_err(in_scan(kind, source))?;
    Ok(matches!(format, ScanFormat::Csv))
}

pub(crate) fn detect_format(path: &str) -> Result<ScanFormat> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".parquet") {
        return Ok(ScanFormat::Parquet);
    }
    if lower.ends_with(".csv") {
        return Ok(ScanFormat::Csv);
    }
    if let Ok(u) = Url::parse(path) {
        if matches!(u.scheme(), "http" | "https") && u.query().is_some() {
            return Err(DataFusionError::NotImplemented(
                "an http(s) URL with a query string is not supported — the scan drops the \
                 query before fetching, so a presigned/token URL would fetch the unsigned \
                 path and fail. Point at a direct .parquet/.csv URL, or use s3://, gs:// or \
                 az:// with storage_options for signed access"
                    .to_string(),
            ));
        }
    }
    Err(DataFusionError::NotImplemented(
        "only .parquet and .csv files can be scanned; name a file (or a glob) with one of \
         those extensions"
            .to_string(),
    ))
}

/// Label an error from the scan `kind` (`scan_edges`) of `source`:
/// `scan_edges('edges.parquet'): no file found at ...`, or for a list of paths
/// `scan_edges(['a.csv', 'b.csv']): ...`.
/// Only an error about the file is labelled: one from evaluating a pushed-down
/// filter reads as it does on every other path. A CSV value that does not
/// parse as its column's type is located first ([`csv_value_error`]).
fn in_scan<'a>(
    kind: &'static str,
    source: ScanSource<'a>,
) -> impl Fn(DataFusionError) -> DataFusionError + 'a {
    let label = format!("{kind}({})", source.shown());
    move |e| {
        if evaluation_error(&e) {
            e
        } else {
            csv_value_error(e, source).context(label.clone())
        }
    }
}

/// Whether `e` comes from evaluating an expression over the rows (a comparison,
/// arithmetic), not from reading the file.
fn evaluation_error(e: &DataFusionError) -> bool {
    use arrow::error::ArrowError;
    if let DataFusionError::ArrowError(a, _) = e.find_root() {
        if matches!(
            a.as_ref(),
            ArrowError::DivideByZero
                | ArrowError::ArithmeticOverflow(_)
                | ArrowError::ComputeError(_)
                | ArrowError::CastError(_)
                | ArrowError::InvalidArgumentError(_)
        ) {
            return true;
        }
    }
    let m = e.to_string();
    [
        "cannot compare ",
        "does not fit the type",
        "Cannot coerce",
        "Cannot infer common argument",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// The error for a role column (`src`, `id`) the file does not have.
fn missing_column(role: &str, name: &str, schema: &Schema) -> DataFusionError {
    let have: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| format!("'{}'", f.name()))
        .collect();
    DataFusionError::Execution(format!(
        "{role} column '{name}' not found in the file; it has [{}]",
        have.join(", ")
    ))
}

/// Check the file has the `src` and `dst` columns.
fn check_endpoints(schema: &Schema, src: &str, dst: &str) -> Result<()> {
    for (role, name) in [("src", src), ("dst", dst)] {
        if schema.index_of(name).is_err() {
            return Err(missing_column(role, name, schema));
        }
    }
    Ok(())
}

/// The canonical Arrow type for a node-id column (see [`ids::canonical_id_type`]),
/// or the error for a type that cannot hold ids.
fn canonical_id_type(dt: &DataType) -> Result<DataType> {
    ids::canonical_id_type(dt)
        .ok_or_else(|| DataFusionError::NotImplemented(ids::id_type_message(&pa_name(dt))))
}

/// Rows per scanned batch. DataFusion's default (8,192) makes the index build
/// intern in small pieces; at 128K rows `EdgeInterner` resolves known ids in
/// parallel, which more than halves a 30M-edge build. Batches stay small next to
/// the graph itself (2 MiB of `Int64` endpoints).
pub(crate) const SCAN_BATCH_ROWS: usize = 1 << 17;

/// Open a scan source into an unprojected `DataFrame`: take the session for the
/// path's object store (see [`scan_state`]) and read the file in the format its
/// extension names. The shared prologue of `scan_edges_batch` and
/// `scan_nodes_batch` — each then applies its own projection and id canonicalization.
///
/// Several paths read as one table, in the order given (#216): each path as
/// one path reads, then their union. A union's partitions are its inputs'
/// partitions in input order, so [`collect_in_file_order`] reads the rows in
/// path order, then file order within a path, as the files read into memory
/// one after another and concatenated would be. The paths must name the same
/// columns with the same types, and be on one store.
///
/// `kind` (`"scan_edges"`/`"scan_nodes"`) only tunes the error message from
/// [`detect_format`]. The returned `DataFrame` carries its own `Arc`-shared session
/// state (including the registered store), so the local `SessionContext` need not
/// outlive this call.
async fn open_scan(source: ScanSource<'_>) -> Result<DataFrame> {
    let Some((first, rest)) = source.paths.split_first() else {
        return Err(DataFusionError::Plan(
            "the list of paths is empty; pass at least one path".to_string(),
        ));
    };
    let mut df = open_path(first, source).await?;
    let store = StoreKey::new(first, source.storage_options);
    for path in rest {
        if StoreKey::new(path, source.storage_options) != store {
            return Err(DataFusionError::NotImplemented(format!(
                "the paths in a list must be on one store, and '{first}' and '{path}' are \
                 not; copy the files to one store, or read them into memory"
            )));
        }
        let next = open_path(path, source).await?;
        let (want, got) = (df.schema().as_arrow(), next.schema().as_arrow());
        if let Some(why) = mismatch(want, got, first, path) {
            return Err(DataFusionError::Execution(format!(
                "the paths in a list are read as one table, so each needs the same columns, \
                 with the same types, in the same order. {why}"
            )));
        }
        df = df.union(next)?;
    }
    Ok(df)
}

/// How `got` (the schema of path `path`) differs from `want` (that of the
/// list's first path, `first`) in a way a union cannot read as one table, with
/// what to do: `None` when the columns and types match. Nullability may
/// differ: the union's column is nullable if any path's is.
fn mismatch(want: &Schema, got: &Schema, first: &str, path: &str) -> Option<String> {
    let names =
        |s: &Schema| -> Vec<String> { s.fields().iter().map(|f| f.name().clone()).collect() };
    if names(want) != names(got) {
        return Some(format!(
            "'{first}' has the columns [{}] and '{path}' has [{}]. Give every file the \
             same columns, or read each with its own scan.",
            names(want).join(", "),
            names(got).join(", ")
        ));
    }
    let differ: Vec<String> = want
        .fields()
        .iter()
        .zip(got.fields())
        .filter(|(a, b)| a.data_type() != b.data_type())
        .map(|(a, b)| {
            format!(
                "column '{}' is {} in '{first}' and {} in '{path}'",
                a.name(),
                pa_name(a.data_type()),
                pa_name(b.data_type())
            )
        })
        .collect();
    if differ.is_empty() {
        return None;
    }
    let mut differ = differ.join("; ");
    differ[..1].make_ascii_uppercase();
    Some(format!(
        "{differ}. Write the files with matching types (in a CSV file, a type is inferred \
         from the values: write 1.0 for a float in every file), or read them into \
         memory, cast the columns to one type and concatenate them."
    ))
}

/// One path of a scan source, read as [`open_scan`] reads a single path.
async fn open_path(path: &str, source: ScanSource<'_>) -> Result<DataFrame> {
    let ctx = SessionContext::new_with_state(scan_state(path, source.storage_options)?);
    Ok(match detect_format(path)? {
        ScanFormat::Parquet => read_parquet(&ctx, path).await?,
        ScanFormat::Csv => read_csv(&ctx, path, source.csv).await?,
    })
}

/// `ctx.read_parquet(path, ParquetReadOptions::default())`, with a column's
/// file-level min/max exact only where every row group backs them
/// ([`CheckedBounds`], #202): DataFusion (54 and 55) would otherwise read a column whose
/// known values are all equal as that constant, the rows of a row group without
/// bounds (all NaN, a long string) included. The table is built as
/// `read_parquet` builds it, sharing the runtime's file statistics cache.
async fn read_parquet(ctx: &SessionContext, path: &str) -> Result<DataFrame> {
    let table_path = ListingTableUrl::parse(path)?;
    let mut options = ParquetReadOptions::default()
        .to_listing_options(&ctx.copied_config(), ctx.copied_table_options());
    if !path.ends_with(options.file_extension.as_str()) && !table_path.is_collection() {
        return Err(DataFusionError::Execution(format!(
            "File path '{}' does not match the expected extension '{}'",
            table_path.as_str(),
            options.file_extension
        )));
    }
    options.format = Arc::new(CheckedBounds::new(options.format));
    let config = ListingTableConfig::new(table_path)
        .with_listing_options(options)
        .infer_schema(&ctx.state())
        .await?;
    let provider = ListingTable::try_new(config)?
        .with_cache(ctx.runtime_env().cache_manager.get_file_statistic_cache());
    ctx.read_table(Arc::new(provider))
}

/// Collect a scan's batches in file row order.
///
/// DataFusion reads a large or multi-file source as several parallel partitions,
/// and `DataFrame::collect` merges them in whichever order they finish. Dense node
/// ids are assigned in first-seen order, so a run-dependent row order changes
/// component labels, `edge_ids`, and floating-point summation order (#148).
/// The partition layout itself is deterministic: files are sorted by path and
/// split into contiguous byte ranges. With work stealing off (see `open_scan`),
/// each partition reads only its own range, so concatenating partitions in order
/// gives the file's own row order on every run, whatever the thread count.
async fn collect_in_file_order(df: DataFrame) -> Result<Vec<RecordBatch>> {
    if crate::dry::active() {
        crate::dry::record(df.logical_plan());
        return Ok(vec![]);
    }
    Ok(df
        .collect_partitioned()
        .await?
        .into_iter()
        .flatten()
        .collect())
}

/// [`collect_in_file_order`], also returning the executed plan (for its metrics).
async fn collect_in_file_order_with_plan(
    df: DataFrame,
) -> Result<(Vec<RecordBatch>, Arc<dyn ExecutionPlan>)> {
    if crate::dry::active() {
        crate::dry::record(df.logical_plan());
        let schema: SchemaRef = Arc::new(df.schema().as_arrow().clone());
        let empty: Arc<dyn ExecutionPlan> =
            Arc::new(datafusion::physical_plan::empty::EmptyExec::new(schema));
        return Ok((vec![], empty));
    }
    let task_ctx = Arc::new(df.task_ctx());
    let plan = df.create_physical_plan().await?;
    let parts = datafusion::physical_plan::collect_partitioned(plan.clone(), task_ctx).await?;
    Ok((parts.into_iter().flatten().collect(), plan))
}

/// Read the `src`/`dst` columns of an edge file into one `(src, dst)` batch, plus
/// any `weight_columns` needed to evaluate a `weight=` expression.
///
/// Format is chosen by extension (`.parquet` / `.csv`), the two forms Ursa v0.1
/// supports. The projection is pushed down, so only the endpoint columns (and any
/// requested weight columns) are read. The endpoints are canonicalized to a
/// supported node-id type — `Int64` (the fast path) or `Utf8` strings; `src` and
/// `dst` must be the same family — and renamed `src`/`dst`. Weight columns keep
/// their file names and types and appear after `dst`, so a caller can evaluate the
/// weight over the same rows.
///
/// A weight column that *is* an endpoint (a `weight=` expression over the `src`
/// or `dst` column, #154) is emitted again under its own name with its file type,
/// just as an in-memory edge table would carry it. It is not read twice: the
/// output column shares the scanned array. The one exception is an endpoint
/// already named `src`/`dst`, whose canonical column carries that name already.
pub fn scan_edges_batch(
    source: ScanSource<'_>,
    src: &str,
    dst: &str,
    weight_columns: &[String],
) -> Result<Vec<RecordBatch>> {
    scan_edges_batch_inner(source, src, dst, weight_columns).map_err(in_scan("scan_edges", source))
}

fn scan_edges_batch_inner(
    source: ScanSource<'_>,
    src: &str,
    dst: &str,
    weight_columns: &[String],
) -> Result<Vec<RecordBatch>> {
    log_scan("edges", weight_columns, 0);
    crate::runtime::block_on(async move {
        let df = open_scan(source).await?;
        check_endpoints(df.schema().as_arrow(), src, dst)?;

        // Project src, dst, then any weight columns not already among them.
        let df = df.select_columns(&edge_projection(src, dst, weight_columns))?;
        let dry = crate::dry::active();
        let dry_schema: SchemaRef = Arc::new(df.schema().as_arrow().clone());
        // Keep the scan's batches separate (no `concat_batches` into one contiguous
        // batch) so the transient ingest footprint stays ~1×, not ~2×, at the
        // 500M-edge target; the topology build consumes them as a stream (#60).
        let batches = collect_in_file_order(df).await?;
        if !dry && (batches.is_empty() || batches.iter().all(|b| b.num_rows() == 0)) {
            return Err(empty_edge_source());
        }

        // The canonical id type is taken once from the read schema (identical across
        // the scan's batches) and every batch is canonicalized to it independently.
        let read_schema = if dry { dry_schema } else { batches[0].schema() };
        let layout = EdgeBatchLayout::new(&read_schema, src, dst, weight_columns)?;
        if dry {
            return Ok(vec![RecordBatch::new_empty(layout.schema)]);
        }

        // Canonicalize each batch independently, in parallel (rayon's collect keeps
        // batch order). Consuming the list releases each batch after its cast, so a
        // narrower integer or a view-typed string id column is converted batch by
        // batch rather than all at once.
        use rayon::prelude::*;
        let out = batches
            .into_par_iter()
            .filter(|batch| batch.num_rows() > 0) // the non-empty guard above ensures ≥1 remains
            .map(|batch| layout.canonical(&batch))
            .collect::<Result<Vec<_>>>()?;
        Ok(out)
    })?
}

/// How [`scan_edges_batch`] lays out a batch it read: `src` and `dst` cast to
/// the canonical id type and renamed, then each weight column by its own name.
struct EdgeBatchLayout {
    id_type: DataType,
    schema: SchemaRef,
    /// The read column behind each output column after `dst`.
    extra: Vec<usize>,
}

impl EdgeBatchLayout {
    /// The layout of batches read as `read_schema` (`src`, `dst`, then the
    /// weight columns that are not endpoints). An endpoint requested as a
    /// weight column (#154) points back at read column 0/1 (uncast).
    fn new(read_schema: &Schema, src: &str, dst: &str, weight_columns: &[String]) -> Result<Self> {
        let id_type = endpoint_id_type(read_schema)?;
        let mut fields = vec![
            Field::new("src", id_type.clone(), true),
            Field::new("dst", id_type.clone(), true),
        ];
        let mut extra: Vec<usize> = Vec::new();
        for (i, f) in read_schema.fields().iter().enumerate().skip(2) {
            fields.push(f.as_ref().clone());
            extra.push(i);
        }
        for (end, idx) in [(src, 0), (dst, 1)] {
            let wanted = weight_columns.iter().any(|c| c == end);
            if wanted && !fields.iter().any(|f| f.name() == end) {
                fields.push(read_schema.field(idx).clone().with_name(end));
                extra.push(idx);
            }
        }
        Ok(Self {
            id_type,
            schema: Arc::new(Schema::new(fields)),
            extra,
        })
    }

    /// A read batch in this layout.
    fn canonical(&self, batch: &RecordBatch) -> Result<RecordBatch> {
        let src_c = cast(batch.column(0), &self.id_type).map_err(arrow_err)?;
        let dst_c = cast(batch.column(1), &self.id_type).map_err(arrow_err)?;
        let mut columns = vec![src_c, dst_c];
        columns.extend(self.extra.iter().map(|&i| batch.column(i).clone()));
        RecordBatch::try_new(self.schema.clone(), columns).map_err(arrow_err)
    }
}

/// The projection [`scan_edges_batch`] reads: `src`, `dst`, then each weight
/// column not already among them.
fn edge_projection<'a>(src: &'a str, dst: &'a str, weight_columns: &'a [String]) -> Vec<&'a str> {
    let mut proj: Vec<&str> = vec![src, dst];
    for c in weight_columns {
        if !proj.contains(&c.as_str()) {
            proj.push(c.as_str());
        }
    }
    proj
}

/// Read an edge file as a table — every column, or the `columns` projection plus
/// both endpoints — as a scanned edge frame collects it (#160). See
/// [`scan_edges_rows_with`], which also takes predicates to push into the scan.
pub fn scan_edges_table(
    source: ScanSource<'_>,
    src: &str,
    dst: &str,
    columns: &[String],
) -> Result<Vec<RecordBatch>> {
    // A projection here always carries both endpoints.
    let mut columns = columns.to_vec();
    if !columns.is_empty() {
        columns.extend([src.to_string(), dst.to_string()]);
    }
    let no_filters: &[()] = &[];
    let (rows, _, _) =
        scan_edges_rows_with(source, src, dst, &columns, no_filters, |_, _| None, false)?;
    Ok(rows.batches)
}

/// An edge file's rows, read by [`scan_edges_rows_with`].
pub struct EdgeRows {
    /// At least one batch, carrying the schema.
    pub batches: Vec<RecordBatch>,
    /// Rows dropped for a null endpoint (always 0 unless `drop_nulls`).
    pub dropped: usize,
}

/// Read an edge file as a table, keeping the rows every predicate in `filters`
/// holds for, and return how many of `filters` were pushed into the scan, with
/// the executed scan plan (for its metrics).
///
/// The result is shaped like the same file read into memory and passed to
/// `EdgeFrame`: columns keep their file names, order and types, so a scanned edge
/// frame collects exactly what an in-memory one does. The endpoint types are
/// validated (an integer or string family, the same for both) but not cast,
/// since an in-memory edge table keeps its endpoint types as well. The one
/// conversion is `Utf8View` to `Utf8` for every column, as in
/// [`scan_nodes_batch`], so a scan and an in-memory read of the same file agree.
///
/// `columns` empty reads every column; otherwise only the named columns are read
/// (projection pushdown), in file order, with the endpoints only when named (or
/// to drop null-endpoint rows). A name the file lacks is skipped here
/// and reported by the caller's own column check, so the error matches the
/// in-memory frame's.
///
/// `filters` are the caller's predicates, in whatever form it holds them;
/// `lower` turns one into a DataFusion expression over the read's schema, or
/// `None` when it must not be pushed (it does not lower, or the scan's
/// statistics could prune rows it keeps). The longest prefix of `filters` that
/// lowers, over columns the read has, is pushed into the scan: a Parquet file
/// skips the row groups whose statistics rule them out (#192). The caller runs
/// the rest. With `drop_nulls` (`on_null="drop"`) rows with a null endpoint are
/// removed and counted in [`EdgeRows::dropped`]; pass no `filters` then, so the
/// count covers the whole file. The graph-op paths never come here: the index
/// build and the weighted scan keep their narrow projections
/// ([`scan_edges_topology`], [`scan_edges_batch`]).
#[allow(clippy::too_many_arguments)]
pub fn scan_edges_rows_with<F>(
    source: ScanSource<'_>,
    src: &str,
    dst: &str,
    columns: &[String],
    filters: &[F],
    lower: impl Fn(&F, &DFSchema) -> Option<Expr>,
    drop_nulls: bool,
) -> Result<(EdgeRows, usize, Arc<dyn ExecutionPlan>)> {
    scan_edges_rows_inner(source, src, dst, columns, filters, lower, drop_nulls)
        .map_err(in_scan("scan_edges", source))
}

#[allow(clippy::too_many_arguments)]
fn scan_edges_rows_inner<F>(
    source: ScanSource<'_>,
    src: &str,
    dst: &str,
    columns: &[String],
    filters: &[F],
    lower: impl Fn(&F, &DFSchema) -> Option<Expr>,
    drop_nulls: bool,
) -> Result<(EdgeRows, usize, Arc<dyn ExecutionPlan>)> {
    crate::runtime::block_on(async move {
        let df = open_scan(source).await?;
        let schema = df.schema().as_arrow().clone();
        check_endpoints(&schema, src, dst)?;
        // Validate the endpoint types exactly as the graph build would.
        let ends = Schema::new(vec![
            schema.field_with_name(src)?.clone(),
            schema.field_with_name(dst)?.clone(),
        ]);
        endpoint_id_type(&ends)?;
        // The endpoints are read when asked for, or to drop null-endpoint rows.
        let df = if columns.is_empty() {
            df
        } else {
            let keep: Vec<&str> = schema
                .fields()
                .iter()
                .map(|f| f.name().as_str())
                .filter(|n| {
                    (drop_nulls && (*n == src || *n == dst)) || columns.iter().any(|c| c == n)
                })
                .collect();
            // Some column is read, so the rows are counted.
            let keep = if keep.is_empty() {
                vec![src, dst]
            } else {
                keep
            };
            df.select_columns(&keep)?
        };
        let read_schema = df.schema().as_arrow().clone();

        let mut filtered = df.clone();
        let mut pushed = 0;
        for f in filters {
            let Some(expr) = lower(f, filtered.schema()) else {
                break;
            };
            let readable = expr
                .column_refs()
                .iter()
                .all(|c| read_schema.index_of(&c.name).is_ok());
            if !readable {
                break;
            }
            filtered = filtered.filter(expr)?;
            pushed += 1;
        }
        let filters = &filters[..pushed];
        let (mut batches, plan) = collect_in_file_order_with_plan(filtered).await?;
        let empty = |b: &[RecordBatch]| b.iter().all(|b| b.num_rows() == 0);
        if empty(&batches) && !crate::dry::active() {
            // An empty file is an error; an empty filter result is not.
            let any = if filters.is_empty() {
                false
            } else {
                !empty(&df.limit(0, Some(1))?.collect().await?)
            };
            if !any {
                return Err(empty_edge_source());
            }
        }
        let mut dropped = 0;
        if drop_nulls {
            let (s, d) = (read_schema.index_of(src)?, read_schema.index_of(dst)?);
            (batches, dropped) = ids::drop_null_endpoint_rows(batches, s, d).map_err(arrow_err)?;
        }
        let base = batches
            .first()
            .map(|b| b.schema())
            .unwrap_or_else(|| Arc::new(read_schema.clone()));
        let out_schema = Arc::new(Schema::new(
            base.fields()
                .iter()
                .map(|f| unview_field(f))
                .collect::<Vec<_>>(),
        ));
        let mut batches = cast_batches(&batches, &out_schema)?;
        if batches.is_empty() {
            batches.push(RecordBatch::new_empty(out_schema));
        }
        log_scan("table", columns, pushed);
        Ok((EdgeRows { batches, dropped }, pushed, plan))
    })?
}

/// Cast each non-empty batch to `out_schema` column by column, passing through
/// any column whose type already matches. The cast is per batch (at most
/// `SCAN_BATCH_ROWS` rows), so the 2 GiB `Utf8` offset limit (#152) bounds a
/// batch, not the column, and every batch shares the one schema so they stay
/// concat-compatible on the Python side.
fn cast_batches(batches: &[RecordBatch], out_schema: &Arc<Schema>) -> Result<Vec<RecordBatch>> {
    let mut out = Vec::with_capacity(batches.len());
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let columns = batch
            .columns()
            .iter()
            .zip(out_schema.fields())
            .map(|(c, f)| {
                if c.data_type() == f.data_type() {
                    Ok(c.clone())
                } else {
                    cast(c, f.data_type()).map_err(arrow_err)
                }
            })
            .collect::<Result<Vec<_>>>()?;
        out.push(RecordBatch::try_new(out_schema.clone(), columns).map_err(arrow_err)?);
    }
    Ok(out)
}

pub(crate) fn arrow_err(e: arrow::error::ArrowError) -> DataFusionError {
    DataFusionError::ArrowError(Box::new(e), None)
}

/// The error for an edge source that resolved to zero rows.
fn empty_edge_source() -> DataFusionError {
    DataFusionError::Execution(
        "the file has no rows; an empty edge set is not a graph (check that the path or glob \
         points at data with the given src/dst columns)"
            .to_string(),
    )
}

/// The canonical node-id type shared by the `src` and `dst` columns (fields 0 and
/// 1 of `schema`). Both must canonicalize to the same family.
fn endpoint_id_type(schema: &Schema) -> Result<DataType> {
    let (src, dst) = (schema.field(0).data_type(), schema.field(1).data_type());
    ids::endpoint_id_type(src, dst).map_err(|e| match e {
        EndpointTypeError::NotAnId(t) => {
            DataFusionError::NotImplemented(ids::id_type_message(&pa_name(&t)))
        }
        EndpointTypeError::Mixed(src, dst) => DataFusionError::NotImplemented(format!(
            "the src and dst columns must have one id type (both integer or both string); got \
             {} and {}",
            pa_name(&src),
            pa_name(&dst)
        )),
    })
}

/// Why [`scan_edges_topology`] failed: reading the file, or interning its edges
/// (a null endpoint, an id type mismatch, too many nodes).
#[derive(Debug)]
pub enum ScanBuildError {
    Scan(DataFusionError),
    Build(IdError),
}

impl From<DataFusionError> for ScanBuildError {
    fn from(e: DataFusionError) -> Self {
        ScanBuildError::Scan(e)
    }
}

impl From<IdError> for ScanBuildError {
    fn from(e: IdError) -> Self {
        ScanBuildError::Build(e)
    }
}

/// A topology built straight from an edge file by [`scan_edges_topology`].
pub struct ScannedTopology {
    pub topo: Arc<Topology>,
    pub ids: Arc<IdMap>,
    /// Rows dropped for a null endpoint (always 0 unless `drop_nulls`).
    pub dropped: usize,
}

/// Scan an edge file and build its topology, interning each batch as it is
/// decoded.
///
/// This produces the same index as `build_topology_batches(scan_edges_batch(..))`
/// without ever holding the whole Arrow input: partitions are consumed in order,
/// and each batch is canonicalized, interned and freed before the next is decoded.
/// The input (16 B/edge for two `Int64` columns) never accumulates, and the batch
/// buffers are reused by the decoder rather than left for the allocator to hand
/// back (#149). Decoding interleaves with interning, which is the slower step, so
/// this is no slower than collecting first.
///
/// With `drop_nulls` (the `on_null="drop"` opt-in), rows with a null endpoint are
/// filtered out and counted in [`ScannedTopology::dropped`]; without it they reach
/// the interner, which rejects them. Weighted scans keep using
/// [`scan_edges_batch`], since their weight columns must outlive the build.
pub fn scan_edges_topology(
    source: ScanSource<'_>,
    src: &str,
    dst: &str,
    drop_nulls: bool,
) -> std::result::Result<ScannedTopology, ScanBuildError> {
    scan_edges_topology_inner(source, src, dst, drop_nulls).map_err(|e| match e {
        ScanBuildError::Scan(e) => ScanBuildError::Scan(in_scan("scan_edges", source)(e)),
        e => e,
    })
}

fn scan_edges_topology_inner(
    source: ScanSource<'_>,
    src: &str,
    dst: &str,
    drop_nulls: bool,
) -> std::result::Result<ScannedTopology, ScanBuildError> {
    log_scan("index", &[], 0);
    let (interner, id_type, dropped) = crate::runtime::block_on(async move {
        let df = open_scan(source).await?;
        check_endpoints(df.schema().as_arrow(), src, dst)?;
        let df = df.select_columns(&[src, dst])?;
        let id_type = endpoint_id_type(df.schema().as_arrow())?;
        if crate::dry::active() {
            return Ok::<_, ScanBuildError>((None, Some(id_type), 0));
        }
        let task_ctx = df.task_ctx();
        let plan = df.create_physical_plan().await?;
        // Parquet metadata gives the row count up front, so the dense endpoint
        // vectors are sized once; a CSV has no count and they grow as needed.
        let hint = StatisticsContext::new()
            .compute(plan.as_ref(), &StatisticsArgs::new())?
            .num_rows
            .get_value()
            .copied()
            .unwrap_or(0);
        let mut interner = EdgeInterner::with_capacity(hint);
        let mut dropped = 0;
        // Partition by partition, in order: see `collect_in_file_order`.
        for mut stream in execute_stream_partitioned(plan, Arc::new(task_ctx))? {
            while let Some(batch) = stream.next().await {
                let batch = batch?;
                let src_c = cast(batch.column(0), &id_type).map_err(arrow_err)?;
                let dst_c = cast(batch.column(1), &id_type).map_err(arrow_err)?;
                drop(batch);
                let keep = if drop_nulls {
                    ids::endpoint_mask(&src_c, &dst_c).map_err(arrow_err)?
                } else {
                    None
                };
                if let Some(keep) = keep {
                    let src_c = filter(&src_c, &keep).map_err(arrow_err)?;
                    let dst_c = filter(&dst_c, &keep).map_err(arrow_err)?;
                    dropped += keep.len() - src_c.len();
                    interner.push(&src_c, &dst_c)?;
                } else {
                    interner.push(&src_c, &dst_c)?;
                }
            }
        }
        Ok::<_, ScanBuildError>((Some(interner), None, dropped))
    })??;
    let interner = match interner {
        Some(interner) => interner,
        None => {
            // A dry run: an empty index of the file's id type.
            let (topo, ids) = crate::topology::empty_topology(&id_type.expect("set when dry"));
            return Ok(ScannedTopology { topo, ids, dropped });
        }
    };
    if interner.is_empty() && dropped == 0 {
        return Err(empty_edge_source().into());
    }
    let (ids, src_dense, dst_dense) = interner.finish();
    let topo = Topology::build(ids.len(), src_dense, dst_dense);
    Ok(ScannedTopology {
        topo: Arc::new(topo),
        ids: Arc::new(ids),
        dropped,
    })
}

/// The rows [`scan_edges_stream`] read.
pub struct StreamedEdges {
    /// The index, when the scan was asked to build one.
    pub index: Option<(Arc<Topology>, Arc<IdMap>)>,
    /// Rows read (after any null-endpoint rows were dropped).
    pub rows: usize,
    /// Rows dropped for a null endpoint (always 0 unless `drop_nulls`).
    pub dropped: usize,
}

/// Scan an edge file for its endpoints and `weight_columns` as
/// [`scan_edges_batch`] reads them, handing each batch to `visit` in row order
/// and freeing it before the next is decoded; with `build`, interning each batch
/// as it goes, as [`scan_edges_topology`] does.
///
/// This is how a weighted operation reads a file without holding it: `visit`
/// evaluates the weights a batch at a time, so only the index and one `f64` per
/// edge outlive the scan (#267). The batches `visit` sees are those
/// [`scan_edges_batch`] returns (canonical `src`/`dst`, then the weight
/// columns), less any row with a null endpoint when `drop_nulls`; the index is
/// the one [`scan_edges_topology`] builds. Not for a dry run, which reads no
/// rows: use [`scan_edges_batch`] then.
pub fn scan_edges_stream(
    source: ScanSource<'_>,
    src: &str,
    dst: &str,
    weight_columns: &[String],
    drop_nulls: bool,
    build: bool,
    visit: &mut dyn FnMut(&RecordBatch),
) -> std::result::Result<StreamedEdges, ScanBuildError> {
    scan_edges_stream_inner(source, src, dst, weight_columns, drop_nulls, build, visit).map_err(
        |e| match e {
            ScanBuildError::Scan(e) => ScanBuildError::Scan(in_scan("scan_edges", source)(e)),
            e => e,
        },
    )
}

fn scan_edges_stream_inner(
    source: ScanSource<'_>,
    src: &str,
    dst: &str,
    weight_columns: &[String],
    drop_nulls: bool,
    build: bool,
    visit: &mut dyn FnMut(&RecordBatch),
) -> std::result::Result<StreamedEdges, ScanBuildError> {
    log_scan("edges", weight_columns, 0);
    let (interner, rows, dropped) = crate::runtime::block_on(async move {
        let df = open_scan(source).await?;
        check_endpoints(df.schema().as_arrow(), src, dst)?;
        let df = df.select_columns(&edge_projection(src, dst, weight_columns))?;
        let task_ctx = df.task_ctx();
        let plan = df.create_physical_plan().await?;
        let mut interner = if build {
            let hint = StatisticsContext::new()
                .compute(plan.as_ref(), &StatisticsArgs::new())?
                .num_rows
                .get_value()
                .copied()
                .unwrap_or(0);
            Some(EdgeInterner::with_capacity(hint))
        } else {
            None
        };
        let (mut rows, mut dropped) = (0, 0);
        // Laid out as `scan_edges_batch` lays out its batches, from the first.
        let mut layout: Option<EdgeBatchLayout> = None;
        for mut stream in execute_stream_partitioned(plan, Arc::new(task_ctx))? {
            while let Some(batch) = stream.next().await {
                let batch = batch?;
                if batch.num_rows() == 0 {
                    continue;
                }
                if layout.is_none() {
                    layout = Some(EdgeBatchLayout::new(
                        &batch.schema(),
                        src,
                        dst,
                        weight_columns,
                    )?);
                }
                let layout = layout.as_ref().expect("set above");
                let mut batch = layout.canonical(&batch)?;
                if drop_nulls {
                    let (kept, n) = crate::ids::drop_null_endpoint_rows(vec![batch], 0, 1)
                        .map_err(arrow_err)?;
                    dropped += n;
                    batch = kept.into_iter().next().expect("one batch in, one out");
                }
                if let Some(interner) = interner.as_mut() {
                    interner.push(batch.column(0), batch.column(1))?;
                }
                rows += batch.num_rows();
                visit(&batch);
            }
        }
        Ok::<_, ScanBuildError>((interner, rows, dropped))
    })??;
    if rows == 0 && dropped == 0 {
        return Err(empty_edge_source().into());
    }
    let index = interner.map(|interner| {
        let (ids, src_dense, dst_dense) = interner.finish();
        let topo = Topology::build(ids.len(), src_dense, dst_dense);
        (Arc::new(topo), Arc::new(ids))
    });
    Ok(StreamedEdges {
        index,
        rows,
        dropped,
    })
}

/// Read a node/attribute file into a batch list.
///
/// When `columns` is empty every column is read (the attribute-table default).
/// When non-empty it is a **projection pushdown**: only those columns are read
/// from the file (DataFusion pushes the projection into Parquet, so unread
/// columns' byte ranges are never fetched). The projection must include the `id`
/// column; the Python layer computes the needed set from the whole plan — the join
/// `id`, plus every column any `filter`/`sort`/`agg`/`select` references — so the
/// scan reads only what the output provably needs and never under-projects.
///
/// Only the `id` column is canonicalized to a supported node-id type (integer ->
/// Int64, string -> Utf8); attribute columns keep their file types. The result
/// feeds `execute_node_query`'s `nodes` slot, where algorithm outputs are
/// LEFT-joined onto it by id — exactly like an in-memory `from_arrow(..., id=...)`
/// table.
pub fn scan_nodes_batch(
    source: ScanSource<'_>,
    id: &str,
    columns: &[String],
) -> Result<Vec<RecordBatch>> {
    scan_nodes_batch_inner(source, id, columns).map_err(in_scan("scan_nodes", source))
}

fn scan_nodes_batch_inner(
    source: ScanSource<'_>,
    id: &str,
    columns: &[String],
) -> Result<Vec<RecordBatch>> {
    log_scan("nodes", columns, 0);
    crate::runtime::block_on(async move {
        let df = open_scan(source).await?;

        // Projection pushdown: read only the requested columns (must include id).
        // Empty means "all columns" (the attribute-table default).
        let df = if columns.is_empty() {
            df
        } else {
            let refs: Vec<&str> = columns.iter().map(String::as_str).collect();
            df.select_columns(&refs)?
        };

        // Keep the batches separate (no `concat_batches`); the attribute table
        // crosses the FFI as a batch list and is consumed as a stream (#60).
        let dry = crate::dry::active();
        let dry_schema: SchemaRef = Arc::new(df.schema().as_arrow().clone());
        let batches = collect_in_file_order(df).await?;
        if !dry && (batches.is_empty() || batches.iter().all(|b| b.num_rows() == 0)) {
            return Err(DataFusionError::Execution(
                "the file has no rows (check that the path or glob points at data with the \
                 given id column)"
                    .to_string(),
            ));
        }

        let read_schema = if dry { dry_schema } else { batches[0].schema() };
        let id_idx = read_schema
            .index_of(id)
            .map_err(|_| missing_column("id", id, &read_schema))?;

        // Canonicalize the id column (integer -> Int64, string -> Utf8); attribute
        // columns keep their file types, except that a `Utf8View` column (a file
        // whose Arrow schema says string_view) becomes `Utf8`: the attribute table
        // is filtered and sorted with pyarrow compute, which has few string-view
        // kernels. The cast is per batch (at most `SCAN_BATCH_ROWS` rows), so the
        // 2 GiB `Utf8` offset limit (#152) bounds a batch, not the column. The out
        // schema is built once and shared across batches so they stay
        // concat-compatible on the Python side.
        let id_type = canonical_id_type(read_schema.field(id_idx).data_type())?;
        let fields: Vec<Field> = read_schema
            .fields()
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if i == id_idx {
                    Field::new(f.name(), id_type.clone(), f.is_nullable())
                } else {
                    unview_field(f)
                }
            })
            .collect();
        let out_schema = Arc::new(Schema::new(fields));
        if dry {
            return Ok(vec![RecordBatch::new_empty(out_schema)]);
        }
        cast_batches(&batches, &out_schema)
    })?
}

/// A cached node table: the columns it was read with (`None`: every column), and
/// its batches.
type HeldTable = (Option<Vec<String>>, Vec<RecordBatch>);

/// The scanned attribute table of one `scan_nodes` frame, kept across collects
/// (#178, #192).
///
/// It holds one table: the superset of the columns read so far (or every column).
/// A read of columns it holds is a projection of it; any other read scans the
/// union and replaces it. So the cache never holds more than one copy of a
/// column, however many projections a frame's plans ask for.
#[derive(Default)]
pub struct NodeTableCache {
    /// `(columns read, None for all; the table)`.
    table: Mutex<Option<HeldTable>>,
    scans: std::sync::atomic::AtomicUsize,
}

impl NodeTableCache {
    /// The node file's table with `columns` (in that order; `None`: every column,
    /// in file order), read through the cache. Concurrent callers wait for one
    /// scan.
    pub fn read(
        &self,
        source: ScanSource<'_>,
        id: &str,
        columns: Option<&[String]>,
    ) -> Result<Vec<RecordBatch>> {
        let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((held, batches)) = table.as_ref() {
            if let Some(out) = project(held.as_deref(), batches, columns)? {
                return Ok(out);
            }
        }
        // Read the union of what is held and what is asked for.
        let read: Option<Vec<String>> = match (table.as_ref(), columns) {
            (_, None) | (Some((None, _)), _) => None,
            (Some((Some(held), _)), Some(want)) => {
                let mut all = held.clone();
                all.extend(want.iter().filter(|c| !held.contains(c)).cloned());
                Some(all)
            }
            (None, Some(want)) => Some(want.to_vec()),
        };
        let batches = scan_nodes_batch(source, id, read.as_deref().unwrap_or(&[]))?;
        self.scans
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(out) = project(read.as_deref(), &batches, columns)? else {
            // A column the file lacks: the narrow read raises the scan's error.
            return scan_nodes_batch(source, id, columns.unwrap_or(&[]));
        };
        *table = Some((read, batches));
        Ok(out)
    }

    /// Scans this cache has run (test/observability hook).
    pub fn scans(&self) -> usize {
        self.scans.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The columns the cached table holds (`Some(None)`: every column).
    pub fn held(&self) -> Option<Option<Vec<String>>> {
        let table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        table.as_ref().map(|(held, _)| held.clone())
    }
}

/// `batches` (a table read with `held` columns, `None` for all) narrowed to
/// `want`, or `None` when it lacks one of them.
fn project(
    held: Option<&[String]>,
    batches: &[RecordBatch],
    want: Option<&[String]>,
) -> Result<Option<Vec<RecordBatch>>> {
    let Some(want) = want else {
        return Ok(held.is_none().then(|| batches.to_vec()));
    };
    let schema = batches[0].schema();
    let mut indices = Vec::with_capacity(want.len());
    for c in want {
        match schema.index_of(c) {
            Ok(i) => indices.push(i),
            Err(_) => return Ok(None),
        }
    }
    if held.is_some_and(|h| {
        indices.len() == h.len() && indices.iter().enumerate().all(|(i, &j)| i == j)
    }) {
        return Ok(Some(batches.to_vec()));
    }
    batches
        .iter()
        .map(|b| b.project(&indices).map_err(arrow_err))
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{object_store_for, scan_identity};
    use arrow::array::Int64Array;
    use datafusion::prelude::CsvReadOptions;
    use std::io::Write;
    use std::sync::OnceLock;

    /// [`scan_edges_rows_with`] with no filters to push.
    fn scan_edges_rows(
        source: ScanSource<'_>,
        src: &str,
        dst: &str,
        columns: &[String],
        filters: &[()],
        drop_nulls: bool,
    ) -> Result<EdgeRows> {
        let lower = |_: &(), _: &DFSchema| None;
        Ok(scan_edges_rows_with(source, src, dst, columns, filters, lower, drop_nulls)?.0)
    }

    fn no_opts() -> HashMap<String, String> {
        HashMap::new()
    }

    /// A scan of one path, with no storage options and the default CSV options.
    fn one(path: &str) -> ScanSource<'static> {
        many(&[path])
    }

    /// A scan of several paths, read as one table in this order.
    fn many(paths: &[&str]) -> ScanSource<'static> {
        static OPTS: OnceLock<HashMap<String, String>> = OnceLock::new();
        static CSV: OnceLock<CsvOptions> = OnceLock::new();
        let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        ScanSource::new(
            Box::leak(paths.into_boxed_slice()),
            OPTS.get_or_init(HashMap::new),
            CSV.get_or_init(CsvOptions::default),
        )
    }

    /// Write `n` edges `(i, i + 1)` to a Parquet file with many small row groups, so
    /// DataFusion splits the scan across several partitions.
    fn write_multi_row_group_parquet(path: &std::path::Path, n: i64) {
        use datafusion::parquet::arrow::ArrowWriter;
        use datafusion::parquet::file::properties::WriterProperties;
        let schema = Arc::new(Schema::new(vec![
            Field::new("from", DataType::Int64, false),
            Field::new("to", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from_iter_values(0..n)),
                Arc::new(Int64Array::from_iter_values(1..n + 1)),
            ],
        )
        .unwrap();
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(4_096))
            .build();
        let file = std::fs::File::create(path).unwrap();
        let mut w = ArrowWriter::try_new(file, schema, Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }

    #[test]
    fn scan_topology_matches_the_collect_then_build_path() {
        // A multi-partition file: the streamed build must give the same dense ids
        // and CSR as building from the collected batches.
        let path = std::env::temp_dir().join("ursa_scan_test_topology.parquet");
        write_multi_row_group_parquet(&path, 2_000_000);
        let p = path.to_str().unwrap();
        let batches = scan_edges_batch(one(p), "from", "to", &[]).unwrap();
        let pairs: Vec<(&dyn Array, &dyn Array)> = batches
            .iter()
            .map(|b| (b.column(0).as_ref(), b.column(1).as_ref()))
            .collect();
        let (topo, ids) = crate::topology::build_topology_batches(&pairs).unwrap();
        let scanned = scan_edges_topology(one(p), "from", "to", false).unwrap();
        assert_eq!(scanned.dropped, 0);
        assert_eq!(&scanned.ids.user_id_array(), &ids.user_id_array());
        assert_eq!(scanned.topo.out().offsets, topo.out().offsets);
        assert_eq!(scanned.topo.out().targets, topo.out().targets);
        assert_eq!(scanned.topo.out().edge_ids, topo.out().edge_ids);
        std::fs::remove_file(&path).ok();
    }

    /// The units a pruning metric pruned, summed over every node of a plan.
    fn metric(plan: &Arc<dyn ExecutionPlan>, name: &str) -> usize {
        use datafusion::physical_plan::metrics::MetricValue;
        let own: usize = plan
            .metrics()
            .map(|m| {
                m.iter()
                    .filter_map(|x| match x.value() {
                        MetricValue::PruningMetrics {
                            name: n,
                            pruning_metrics,
                        } if n.as_ref() == name => Some(pruning_metrics.pruned()),
                        _ => None,
                    })
                    .sum()
            })
            .unwrap_or(0);
        own + plan
            .children()
            .iter()
            .map(|c| metric(c, name))
            .sum::<usize>()
    }

    #[test]
    fn a_pushed_filter_skips_row_groups_and_keeps_file_order() {
        // 2M rows in 4,096-row groups, `from` ascending: `from >= 1_990_000` can
        // only hold in the last few groups, so the rest are pruned by statistics.
        let path = std::env::temp_dir().join("ursa_scan_test_pushdown.parquet");
        write_multi_row_group_parquet(&path, 2_000_000);
        let p = path.to_str().unwrap();
        use datafusion::prelude::{col, lit};
        let same = |e: &Expr, _: &DFSchema| Some(e.clone());
        let pred = col("from").gt_eq(lit(1_990_000i64));
        let (rows, pushed, plan) =
            scan_edges_rows_with(one(p), "from", "to", &[], &[pred], same, false).unwrap();
        assert_eq!(pushed, 1);
        let got: Vec<i64> = rows
            .batches
            .iter()
            .flat_map(|b| {
                b.column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .values()
                    .to_vec()
            })
            .collect();
        assert_eq!(got, (1_990_000..2_000_000).collect::<Vec<_>>());
        let pruned = metric(&plan, "row_groups_pruned_statistics");
        assert!(pruned >= 480, "pruned {pruned} of 489 row groups");

        // A filter that keeps nothing is an empty result, not an empty source.
        let none = col("from").lt(lit(0i64));
        let (rows, pushed, _) =
            scan_edges_rows_with(one(p), "from", "to", &[], &[none], same, false).unwrap();
        assert_eq!(pushed, 1);
        assert_eq!(rows.batches.len(), 1);
        assert_eq!(rows.batches[0].num_rows(), 0);
        assert_eq!(rows.batches[0].num_columns(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_node_cache_keeps_one_superset_table() {
        let path = std::env::temp_dir().join("ursa_scan_test_node_cache.csv");
        std::fs::write(&path, "id,a,b,c\n1,x,10,0.5\n2,y,20,1.5\n").unwrap();
        let p = path.to_str().unwrap();
        let cache = NodeTableCache::default();
        let cols = |c: &[&str]| c.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let names = |b: &[RecordBatch]| -> Vec<String> {
            b[0].schema()
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect()
        };
        let ab = cache
            .read(one(p), "id", Some(&cols(&["id", "a", "b"])))
            .unwrap();
        assert_eq!(names(&ab), ["id", "a", "b"]);
        // A subset, in another order, is served from the held table.
        let ba = cache.read(one(p), "id", Some(&cols(&["b", "id"]))).unwrap();
        assert_eq!(names(&ba), ["b", "id"]);
        assert_eq!(cache.scans(), 1);
        // A new column widens the held table to the union.
        let c = cache.read(one(p), "id", Some(&cols(&["id", "c"]))).unwrap();
        assert_eq!(names(&c), ["id", "c"]);
        assert_eq!(cache.held(), Some(Some(cols(&["id", "a", "b", "c"]))));
        assert_eq!(cache.scans(), 2);
        // Every column, then any projection, read it once more at most.
        let all = cache.read(one(p), "id", None).unwrap();
        assert_eq!(names(&all), ["id", "a", "b", "c"]);
        cache.read(one(p), "id", Some(&cols(&["a", "id"]))).unwrap();
        assert_eq!(cache.scans(), 3);
        // A column the file lacks is the scan's own error.
        let err = cache
            .read(one(p), "id", Some(&cols(&["id", "nope"])))
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn scan_topology_drops_null_rows_only_when_asked() {
        let path = std::env::temp_dir().join("ursa_scan_test_topology_nulls.csv");
        std::fs::write(&path, "from,to\n1,2\n,3\n3,\n2,3\n").unwrap();
        let p = path.to_str().unwrap();
        // Default: the null endpoint is an interning error, not a scan error.
        assert!(matches!(
            scan_edges_topology(one(p), "from", "to", false),
            Err(ScanBuildError::Build(IdError::Null))
        ));
        let scanned = scan_edges_topology(one(p), "from", "to", true).unwrap();
        assert_eq!(scanned.dropped, 2);
        assert_eq!(scanned.topo.n_edges(), 2);
        assert_eq!(scanned.topo.n_nodes(), 3);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parquet_string_ids_scan_and_build() {
        // Plain Parquet strings scan to Utf8 ids, through every scan entry point.
        use datafusion::parquet::arrow::ArrowWriter;
        let path = std::env::temp_dir().join("ursa_scan_test_string_ids.parquet");
        let schema = Arc::new(Schema::new(vec![
            Field::new("from", DataType::Utf8, false),
            Field::new("to", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(arrow::array::StringArray::from(vec!["a", "b", "c"])),
                Arc::new(arrow::array::StringArray::from(vec!["b", "c", "a"])),
            ],
        )
        .unwrap();
        let mut w =
            ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        let p = path.to_str().unwrap();

        let batches = scan_edges_batch(one(p), "from", "to", &[]).unwrap();
        assert_eq!(batches[0].column(0).data_type(), &DataType::Utf8);
        let scanned = scan_edges_topology(one(p), "from", "to", false).unwrap();
        assert_eq!((scanned.topo.n_nodes(), scanned.topo.n_edges()), (3, 3));
        let nodes = scan_nodes_batch(one(p), "from", &[]).unwrap();
        assert_eq!(nodes[0].column(0).data_type(), &DataType::Utf8);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parquet_view_and_dictionary_string_ids_scan_and_build() {
        // A file written with Utf8View or dictionary-encoded string columns stores
        // that type in its embedded Arrow schema (Athena/DataFusion output, pandas
        // categoricals), and DataFusion reads it back as such. Both must scan to
        // Utf8 ids and the same graph as a plain Utf8 file.
        use arrow::array::{ArrayRef, DictionaryArray, StringArray, StringViewArray};
        use arrow::datatypes::Int32Type;
        use datafusion::parquet::arrow::ArrowWriter;
        let src = ["a", "b", "c", "a-node-id-longer-than-twelve-bytes"];
        let dst = ["b", "c", "a-node-id-longer-than-twelve-bytes", "a"];
        let make = |layout: &str, v: &[&str]| -> ArrayRef {
            match layout {
                "utf8" => Arc::new(StringArray::from(v.to_vec())),
                "view" => Arc::new(StringViewArray::from(v.to_vec())),
                _ => Arc::new(v.iter().copied().collect::<DictionaryArray<Int32Type>>()),
            }
        };
        let mut built = Vec::new();
        for name in ["utf8", "view", "dict"] {
            let path = std::env::temp_dir().join(format!("ursa_scan_test_ids_{name}.parquet"));
            let (s, d) = (make(name, &src), make(name, &dst));
            let attr = make(name, &src);
            let schema = Arc::new(Schema::new(vec![
                Field::new("from", s.data_type().clone(), false),
                Field::new("to", d.data_type().clone(), false),
                Field::new("label", attr.data_type().clone(), false),
            ]));
            let batch = RecordBatch::try_new(schema.clone(), vec![s, d, attr]).unwrap();
            let file = std::fs::File::create(&path).unwrap();
            let mut w = ArrowWriter::try_new(file, schema, None).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
            let p = path.to_str().unwrap();

            let label = ["label".to_string()];
            let batches = scan_edges_batch(one(p), "from", "to", &label).unwrap();
            assert_eq!(batches[0].column(0).data_type(), &DataType::Utf8, "{name}");
            assert_eq!(batches[0].column(1).data_type(), &DataType::Utf8, "{name}");
            assert_eq!(batches[0].num_columns(), 3, "{name}");
            let scanned = scan_edges_topology(one(p), "from", "to", false).unwrap();
            let nodes = scan_nodes_batch(one(p), "from", &[]).unwrap();
            assert_eq!(nodes[0].column(0).data_type(), &DataType::Utf8, "{name}");
            // A string attribute is read as the file declares it, with views
            // turned into Utf8 for the pyarrow-side relational tail.
            let label_type = nodes[0].column(2).data_type().clone();
            let want_label = match name {
                "dict" => DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
                _ => DataType::Utf8,
            };
            assert_eq!(label_type, want_label, "{name}");
            // The relational table scan (#160) keeps every column under its own
            // name; views become Utf8 (endpoints too), a dictionary stays one.
            let table = scan_edges_table(one(p), "from", "to", &[]).unwrap();
            let schema = table[0].schema();
            let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
            assert_eq!(names, ["from", "to", "label"], "{name}");
            for f in schema.fields() {
                assert_eq!(f.data_type(), &want_label, "{name} {}", f.name());
            }
            built.push((
                scanned.ids.user_id_array(),
                scanned.topo.out().targets.clone(),
            ));
            std::fs::remove_file(&path).ok();
        }
        assert_eq!(built[0], built[1]);
        assert_eq!(built[0], built[2]);
    }

    #[test]
    fn parquet_scan_preserves_file_row_order() {
        // #148: a multi-row-group file is read as several parallel partitions. The
        // scan must return rows in file order on every run, not in partition
        // completion order, or dense ids (and so kernel output) change per run.
        let path = std::env::temp_dir().join("ursa_scan_test_row_order.parquet");
        let n: i64 = 2_000_000; // ~32 MB: above the 10 MB repartition threshold
        write_multi_row_group_parquet(&path, n);
        for _ in 0..5 {
            let batches = scan_edges_batch(one(path.to_str().unwrap()), "from", "to", &[]).unwrap();
            let mut next = 0i64;
            for b in &batches {
                let src = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
                for v in src.values() {
                    assert_eq!(*v, next, "scan returned rows out of file order");
                    next += 1;
                }
            }
            assert_eq!(next, n);
        }
        let batches = scan_edges_table(one(path.to_str().unwrap()), "from", "to", &[]).unwrap();
        let mut next = 0i64;
        for b in &batches {
            let src = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
            for v in src.values() {
                assert_eq!(*v, next, "table scan returned rows out of file order");
                next += 1;
            }
        }
        assert_eq!(next, n);
        std::fs::remove_file(&path).ok();
    }

    /// A Parquet edge file with narrow `Int32` endpoints between two attribute
    /// columns: `w, from, label, to`.
    fn write_attr_edges_parquet(path: &std::path::Path) {
        use arrow::array::{Float64Array, Int32Array, StringArray};
        use datafusion::parquet::arrow::ArrowWriter;
        let schema = Arc::new(Schema::new(vec![
            Field::new("w", DataType::Float64, false),
            Field::new("from", DataType::Int32, false),
            Field::new("label", DataType::Utf8, false),
            Field::new("to", DataType::Int32, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Float64Array::from(vec![0.5, 1.5, 2.5])),
                Arc::new(Int32Array::from(vec![1, 2, 3])),
                Arc::new(StringArray::from(vec!["a", "b", "c"])),
                Arc::new(Int32Array::from(vec![2, 3, 1])),
            ],
        )
        .unwrap();
        let file = std::fs::File::create(path).unwrap();
        let mut w = ArrowWriter::try_new(file, schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }

    #[test]
    fn edge_table_scan_keeps_every_column_as_the_file_has_it() {
        // #160: the relational scan returns the file's columns in file order, with
        // their own names and types (endpoints validated, not cast), so it matches
        // the same file read into an in-memory edge table.
        let path = std::env::temp_dir().join("ursa_scan_test_edge_table.parquet");
        write_attr_edges_parquet(&path);
        let p = path.to_str().unwrap();
        let batches = scan_edges_table(one(p), "from", "to", &[]).unwrap();
        let schema = batches[0].schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, ["w", "from", "label", "to"]);
        let types: Vec<&DataType> = schema.fields().iter().map(|f| f.data_type()).collect();
        assert_eq!(
            types,
            [
                &DataType::Float64,
                &DataType::Int32,
                &DataType::Utf8,
                &DataType::Int32
            ]
        );
        assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 3);

        // A projection reads the named columns plus both endpoints, in file order;
        // a name the file lacks is left for the caller's column check.
        let cols = ["label".to_string(), "nope".to_string()];
        let batches = scan_edges_table(one(p), "from", "to", &cols).unwrap();
        let schema = batches[0].schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, ["from", "label", "to"]);

        // A missing endpoint names the column; a non-id endpoint is rejected as
        // the graph build would reject it.
        // (ursa-plan's scan tests check how `engine_message` words these.)
        let err = scan_edges_table(one(p), "from", "nope", &[]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.ends_with(
                "dst column 'nope' not found in the file; it has ['w', 'from', 'label', 'to']"
            ),
            "{msg}"
        );
        assert!(msg.starts_with("scan_edges('"), "{msg}");
        let err = scan_edges_table(one(p), "from", "w", &[]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("node ids must be an integer or string column; got double"),
            "{msg}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn edge_table_scan_reads_every_csv_column() {
        let path = std::env::temp_dir().join("ursa_scan_test_edge_table.csv");
        std::fs::write(&path, "from,to,weight,label\n10,20,0.5,a\n20,30,0.9,b\n").unwrap();
        let batches = scan_edges_table(one(path.to_str().unwrap()), "from", "to", &[]).unwrap();
        let schema = batches[0].schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, ["from", "to", "weight", "label"]);
        assert_eq!(schema.field(2).data_type(), &DataType::Float64);
        assert_eq!(schema.field(3).data_type(), &DataType::Utf8);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn weight_columns_may_name_an_endpoint() {
        // #154: a weight expression over the src/dst column gets that column by
        // its own name and file type, after the canonical src/dst pair.
        let path = std::env::temp_dir().join("ursa_scan_test_weight_endpoint.parquet");
        write_attr_edges_parquet(&path);
        let p = path.to_str().unwrap();
        let cols = ["from".to_string(), "w".to_string()];
        let batches = scan_edges_batch(one(p), "from", "to", &cols).unwrap();
        let schema = batches[0].schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, ["src", "dst", "w", "from"]);
        assert_eq!(schema.field(0).data_type(), &DataType::Int64);
        assert_eq!(schema.field(3).data_type(), &DataType::Int32);
        let from = batches[0]
            .column(3)
            .as_any()
            .downcast_ref::<arrow::array::Int32Array>()
            .unwrap();
        assert_eq!(from.values(), &[1, 2, 3]);
        std::fs::remove_file(&path).ok();

        // An endpoint already named `src` is the canonical column; no duplicate.
        let path = std::env::temp_dir().join("ursa_scan_test_weight_endpoint.csv");
        std::fs::write(&path, "src,dst\n1,2\n2,3\n").unwrap();
        let cols = ["src".to_string()];
        let batches = scan_edges_batch(one(path.to_str().unwrap()), "src", "dst", &cols).unwrap();
        assert_eq!(batches[0].num_columns(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reads_csv_endpoints_and_casts_to_int64() {
        // Write a tiny CSV with extra columns to prove projection + cast.
        let dir = std::env::temp_dir();
        let path = dir.join("ursa_scan_test_edges.csv");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "from,to,weight").unwrap();
            writeln!(f, "10,20,0.5").unwrap();
            writeln!(f, "20,30,0.9").unwrap();
        }
        let batches = scan_edges_batch(one(path.to_str().unwrap()), "from", "to", &[]).unwrap();
        let batch = &batches[0];
        assert_eq!(batch.num_columns(), 2);
        let n: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(n, 2);
        let src = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(src.values(), &[10, 20]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reads_node_attributes_keeping_all_columns() {
        let dir = std::env::temp_dir();
        let path = dir.join("ursa_scan_test_nodes.csv");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "tower_id,region,capacity").unwrap();
            writeln!(f, "1,us,10").unwrap();
            writeln!(f, "2,eu,20").unwrap();
        }
        let batches = scan_nodes_batch(one(path.to_str().unwrap()), "tower_id", &[]).unwrap();
        let batch = &batches[0];
        let n: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(n, 2);
        // All three columns kept; id cast to Int64.
        assert_eq!(batch.num_columns(), 3);
        let schema = batch.schema();
        let id_idx = schema.index_of("tower_id").unwrap();
        assert_eq!(schema.field(id_idx).data_type(), &DataType::Int64);
        let ids = batch
            .column(id_idx)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(ids.values(), &[1, 2]);
        assert!(schema.index_of("region").is_ok());
        assert!(schema.index_of("capacity").is_ok());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn projects_only_requested_node_columns() {
        // Projection pushdown: ask for id + capacity only; region is not read.
        let dir = std::env::temp_dir();
        let path = dir.join("ursa_scan_test_nodes_proj.csv");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "tower_id,region,capacity").unwrap();
            writeln!(f, "1,us,10").unwrap();
            writeln!(f, "2,eu,20").unwrap();
        }
        let cols = vec!["tower_id".to_string(), "capacity".to_string()];
        let batches = scan_nodes_batch(one(path.to_str().unwrap()), "tower_id", &cols).unwrap();
        let schema = batches[0].schema();
        assert_eq!(schema.fields().len(), 2);
        assert!(schema.index_of("tower_id").is_ok());
        assert!(schema.index_of("capacity").is_ok());
        assert!(schema.index_of("region").is_err()); // not projected -> not read
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reads_via_a_file_url() {
        // A file:// URL exercises the object-store registration + read path with
        // no credentials — the same code an s3:// path takes, minus the network.
        let dir = std::env::temp_dir();
        let path = dir.join("ursa_scan_test_file_url.csv");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "from,to").unwrap();
            writeln!(f, "1,2").unwrap();
            writeln!(f, "2,3").unwrap();
        }
        let url = format!("file://{}", path.to_str().unwrap());
        let batches = scan_edges_batch(one(&url), "from", "to", &[]).unwrap();
        let n: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(n, 2);
        std::fs::remove_file(&path).ok();
    }

    /// Write the edges `(from[i], to[i])` to a one-row-group Parquet file.
    fn write_edges_parquet(path: &std::path::Path, from: &[i64], to: &[i64]) {
        use datafusion::parquet::arrow::ArrowWriter;
        let schema = Arc::new(Schema::new(vec![
            Field::new("from", DataType::Int64, false),
            Field::new("to", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(from.to_vec())),
                Arc::new(Int64Array::from(to.to_vec())),
            ],
        )
        .unwrap();
        let file = std::fs::File::create(path).unwrap();
        let mut w = ArrowWriter::try_new(file, schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }

    fn scanned_sources(path: &str) -> Vec<i64> {
        scan_edges_batch(one(path), "from", "to", &[])
            .unwrap()
            .iter()
            .flat_map(|b| {
                let c = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
                c.values().to_vec()
            })
            .collect()
    }

    #[test]
    fn a_file_rewritten_in_place_is_read_afresh() {
        // The shared scan session caches Parquet footers and statistics; a rewrite
        // between two scans must still be seen, whether it changes the file's size
        // or only its contents and modification time.
        let dir = std::env::temp_dir().join("ursa_scan_test_rewrite");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("edges.parquet");
        let p = path.to_str().unwrap();
        write_edges_parquet(&path, &[1, 2], &[2, 3]);
        assert_eq!(scanned_sources(p), vec![1, 2]);
        // The footer is cached, so the rewrites below test its validation.
        let state = scan_state(p, &no_opts()).unwrap();
        let footers = state.runtime_env().cache_manager.get_file_metadata_cache();
        assert!(footers
            .list_entries()
            .keys()
            .any(|k| k.as_ref().ends_with("ursa_scan_test_rewrite/edges.parquet")));

        // A different size.
        write_edges_parquet(&path, &[7, 8, 9], &[8, 9, 7]);
        assert_eq!(scanned_sources(p), vec![7, 8, 9]);

        // The same size: only the modification time tells the files apart. Set it
        // explicitly so a coarse filesystem clock cannot hide the rewrite.
        let size = std::fs::metadata(&path).unwrap().len();
        write_edges_parquet(&path, &[4, 5, 6], &[5, 6, 4]);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size);
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(scanned_sources(&format!("file://{p}")), vec![4, 5, 6]);
        assert_eq!(scanned_sources(p), vec![4, 5, 6]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_glob_sees_files_added_between_scans() {
        // The file-listing cache stays off: a glob is listed on every scan.
        let dir = std::env::temp_dir().join("ursa_scan_test_glob_grows");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let glob = format!("{}/*.parquet", dir.to_str().unwrap());
        write_edges_parquet(&dir.join("part-0.parquet"), &[1], &[2]);
        assert_eq!(scanned_sources(&glob), vec![1]);
        write_edges_parquet(&dir.join("part-1.parquet"), &[3], &[4]);
        assert_eq!(scanned_sources(&glob), vec![1, 3]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_sessions_are_shared_by_store_identity() {
        let runtime = |path: &str, opts: &HashMap<String, String>| {
            scan_state(path, opts).unwrap().runtime_env().clone()
        };
        // Every local path shares one runtime (and so one set of file caches).
        let local = runtime("/tmp/a.parquet", &no_opts());
        assert!(Arc::ptr_eq(
            &local,
            &runtime("file:///tmp/b.csv", &no_opts())
        ));

        // A cloud client is reused only for the same bucket and the same options.
        let east: HashMap<String, String> = [("region".to_string(), "us-east-1".to_string())]
            .into_iter()
            .collect();
        let west: HashMap<String, String> = [("region".to_string(), "us-west-2".to_string())]
            .into_iter()
            .collect();
        let a = runtime("s3://ursa-test-bucket/a.parquet", &east);
        assert!(Arc::ptr_eq(
            &a,
            &runtime("s3://ursa-test-bucket/b/*.parquet", &east)
        ));
        assert!(!Arc::ptr_eq(
            &a,
            &runtime("s3://ursa-test-bucket/a.parquet", &west)
        ));
        assert!(!Arc::ptr_eq(
            &a,
            &runtime("s3://ursa-other-bucket/a.parquet", &east)
        ));
        assert!(!Arc::ptr_eq(&a, &local));

        // The scan config survives the clone.
        let state = scan_state("/tmp/a.parquet", &no_opts()).unwrap();
        let options = state.config().options();
        assert_eq!(options.execution.batch_size.get(), SCAN_BATCH_ROWS);
        assert!(!options.execution.enable_file_stream_work_stealing);
        assert!(!options.execution.parquet.schema_force_view_types);
    }

    #[test]
    fn builds_s3_store_from_storage_options() {
        // The aws cloud feature is enabled and the storage_options -> config-key
        // mapping resolves: an S3 store builds from a region option, lazily and
        // credential-free (auth is deferred to request time). The GCS/Azure
        // builders are referenced in `object_store_for`, so the gcp/azure
        // features are proven enabled at *compile* time; their `.build()` requires
        // real credentials, so they're not exercised in this credential-free test.
        let opts: HashMap<String, String> = [("region".to_string(), "us-east-1".to_string())]
            .into_iter()
            .collect();
        object_store_for("s3://my-bucket/graph/*.parquet", &opts).unwrap();
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        let err = object_store_for("ftp://host/graph.parquet", &no_opts());
        assert!(err.is_err());
    }

    #[test]
    fn registers_an_http_store_with_port() {
        // The `http` feature is enabled: an http(s) URL registers a store, keyed by
        // the full authority including the port (so localhost:PORT reads route
        // correctly). Credential-free and lazy — no request is made here.
        object_store_for("http://127.0.0.1:8080/graph/edges.csv", &no_opts()).unwrap();
        object_store_for("https://example.com/edges.parquet", &no_opts()).unwrap();
    }

    #[test]
    fn detect_format_matches_extension_and_globs() {
        // Plain files + case-insensitive.
        assert!(matches!(
            detect_format("edges.parquet").unwrap(),
            ScanFormat::Parquet
        ));
        assert!(matches!(
            detect_format("/tmp/local/edges.CSV").unwrap(),
            ScanFormat::Csv
        ));
        // An object-store glob keeps `?`/`*` as glob metacharacters (regression
        // guard: the query-stripping approach broke `part-?.parquet`).
        assert!(matches!(
            detect_format("s3://bucket/part-?.parquet").unwrap(),
            ScanFormat::Parquet
        ));
        assert!(matches!(
            detect_format("gs://bucket/*.csv").unwrap(),
            ScanFormat::Csv
        ));
    }

    #[test]
    fn detect_format_rejects_http_query_string_clearly() {
        // The engine drops the query before fetching, so a presigned/token URL
        // would silently fetch the unsigned path — reject it with a clear message.
        let err = detect_format("https://host/data/edges.csv?token=abc123");
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("query string"));
    }

    #[test]
    fn unknown_storage_option_errors() {
        let bad: HashMap<String, String> = [("not_a_real_option".to_string(), "x".to_string())]
            .into_iter()
            .collect();
        assert!(object_store_for("s3://my-bucket/f.parquet", &bad).is_err());
    }

    /// An edge file `s, d, r` with `r` written as `values` (`None`: null), in
    /// row groups of `group` rows.
    fn write_float_parquet<T>(path: &std::path::Path, values: Vec<Option<T::Native>>, group: usize)
    where
        T: arrow::datatypes::ArrowPrimitiveType,
    {
        use arrow::array::PrimitiveArray;
        use datafusion::parquet::arrow::ArrowWriter;
        use datafusion::parquet::file::properties::WriterProperties;
        let n = values.len() as i64;
        let r: PrimitiveArray<T> = values.into_iter().collect();
        let schema = Arc::new(Schema::new(vec![
            Field::new("s", DataType::Int64, false),
            Field::new("d", DataType::Int64, false),
            Field::new("r", r.data_type().clone(), true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from((0..n).collect::<Vec<_>>())),
                Arc::new(Int64Array::from(
                    (0..n).map(|i| (i + 1) % n).collect::<Vec<_>>(),
                )),
                Arc::new(r),
            ],
        )
        .unwrap();
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(group))
            .build();
        let file = std::fs::File::create(path).unwrap();
        let mut w = ArrowWriter::try_new(file, schema, Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }

    /// Column `r` of `batches`, as `f64` bit patterns (`None`: null), so NaN
    /// compares equal to itself.
    fn float_bits(batches: &[RecordBatch]) -> Vec<Option<u64>> {
        use arrow::array::Float64Array;
        let mut out = vec![];
        for b in batches {
            let r = cast(b.column_by_name("r").unwrap(), &DataType::Float64).unwrap();
            let r = r.as_any().downcast_ref::<Float64Array>().unwrap();
            out.extend(r.iter().map(|v| v.map(f64::to_bits)));
        }
        out
    }

    #[test]
    fn nan_rows_read_back_as_nan_beside_a_constant_float_row_group() {
        // #202: a float column whose non-NaN values are all equal was read as that
        // constant, NaN rows included, through every scan path.
        use arrow::datatypes::{Float32Type, Float64Type};
        let nan = f64::NAN;
        let cases: Vec<(Vec<Option<f64>>, usize)> = vec![
            (vec![Some(nan), Some(nan), Some(1.0)], 2), // all-NaN group, constant group
            (vec![Some(nan), Some(1.0), Some(1.0)], 3), // one group, NaN beside a constant
            (vec![Some(nan), Some(nan), Some(nan)], 2), // all NaN
            (vec![None, None, Some(1.0)], 2),           // all-null group
            (vec![Some(nan), None, Some(1.0), Some(1.0)], 2), // mixed
            (vec![Some(nan), Some(2.0), Some(1.0)], 2), // the issue's control
        ];
        for (i, (values, group)) in cases.into_iter().enumerate() {
            let want: Vec<Option<u64>> = values.iter().map(|v| v.map(f64::to_bits)).collect();
            for f32_col in [false, true] {
                let path =
                    std::env::temp_dir().join(format!("ursa_scan_test_nan_{i}_{f32_col}.parquet"));
                if f32_col {
                    let v = values.iter().map(|v| v.map(|x| x as f32)).collect();
                    write_float_parquet::<Float32Type>(&path, v, group);
                } else {
                    write_float_parquet::<Float64Type>(&path, values.clone(), group);
                }
                let p = path.to_str().unwrap();
                let rows = scan_edges_rows(one(p), "s", "d", &[], &[], false).unwrap();
                assert_eq!(
                    float_bits(&rows.batches),
                    want,
                    "rows: case {i}, f32 {f32_col}"
                );
                let weighted = scan_edges_batch(one(p), "s", "d", &["r".to_string()]).unwrap();
                assert_eq!(
                    float_bits(&weighted),
                    want,
                    "weights: case {i}, f32 {f32_col}"
                );
                std::fs::remove_file(&path).ok();
            }
        }
    }

    /// The `from` column of every batch, in order.
    fn sources_of(batches: &[RecordBatch]) -> Vec<i64> {
        batches
            .iter()
            .flat_map(|b| {
                let c = cast(b.column(0), &DataType::Int64).unwrap();
                let c = c.as_any().downcast_ref::<Int64Array>().unwrap();
                c.values().to_vec()
            })
            .collect()
    }

    #[test]
    fn a_list_of_paths_reads_in_path_order() {
        // #216: the paths are one table, the first path's rows first, whatever
        // order the files sort in, through every read.
        let dir = std::env::temp_dir().join("ursa_scan_test_list_order");
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.parquet"), dir.join("b.parquet"));
        write_multi_row_group_parquet(&a, 300_000);
        write_edges_parquet(&b, &[-1, -2], &[-3, -4]);
        let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());
        let mut want = vec![-1, -2];
        want.extend(0..300_000);
        let source = many(&[b, a]);
        assert_eq!(
            sources_of(&scan_edges_batch(source, "from", "to", &[]).unwrap()),
            want
        );
        let rows = scan_edges_rows(source, "from", "to", &[], &[], false).unwrap();
        assert_eq!(sources_of(&rows.batches), want);
        let nodes = scan_nodes_batch(source, "from", &[]).unwrap();
        assert_eq!(sources_of(&nodes), want);
        let scanned = scan_edges_topology(source, "from", "to", false).unwrap();
        let one = scan_edges_topology(one(b), "from", "to", false).unwrap();
        assert_eq!(scanned.topo.n_edges(), 300_002);
        // The first path's ids are interned first.
        assert_eq!(one.ids.len(), 4);
        assert!(scanned.ids.len() > 300_000);

        let ida = scan_identity(&[a.to_string()], &no_opts()).unwrap();
        let idb = scan_identity(&[b.to_string()], &no_opts()).unwrap();
        let both = scan_identity(&[b.to_string(), a.to_string()], &no_opts()).unwrap();
        assert_eq!(both, format!("{idb};{ida}"));
    }

    #[test]
    fn a_list_of_paths_needs_one_schema() {
        let dir = std::env::temp_dir().join("ursa_scan_test_list_schema");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.parquet");
        write_edges_parquet(&a, &[1], &[2]);
        let c = dir.join("c.csv");
        std::fs::write(&c, "from,to,w\n1,2,0.5\n").unwrap();
        let (a, c) = (a.to_str().unwrap(), c.to_str().unwrap());
        let err = scan_edges_rows(many(&[a, c]), "from", "to", &[], &[], false)
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("has the columns [from, to] and") && err.contains("has [from, to, w]"),
            "{err}"
        );
        let err = scan_edges_batch(many(&[]), "from", "to", &[]).unwrap_err();
        assert!(
            err.to_string().contains("the list of paths is empty"),
            "{err}"
        );
    }

    #[test]
    fn a_csv_column_with_a_pyarrow_float_spelling_reads_as_float64() {
        // Both paths: the default and the `null_value` one (#221).
        let path = std::env::temp_dir().join("ursa_scan_test_float_spellings.csv");
        std::fs::write(&path, "from,to,w,t\n1,2,+5,+5\n2,3,Inf,a\n3,1,-nan,\n").unwrap();
        let paths = [path.to_str().unwrap().to_string()];
        let opts = no_opts();
        for csv in [
            CsvOptions::default(),
            CsvOptions {
                null_value: Some("NA".into()),
                ..CsvOptions::default()
            },
        ] {
            let source = ScanSource::new(&paths, &opts, &csv);
            let rows = scan_edges_rows(source, "from", "to", &[], &[], false).unwrap();
            let w = rows.batches[0].column_by_name("w").unwrap();
            let w = w
                .as_any()
                .downcast_ref::<arrow::array::Float64Array>()
                .unwrap();
            let bits: Vec<u64> = w.values().iter().map(|v| v.to_bits()).collect();
            assert_eq!(
                bits,
                [
                    5f64.to_bits(),
                    f64::INFINITY.to_bits(),
                    0xfff8_0000_0000_0000
                ]
            );
            let t = rows.batches[0].column_by_name("t").unwrap();
            assert_eq!(t.data_type(), &DataType::Utf8);
        }
    }

    #[test]
    fn csv_options_parse_the_file() {
        let path = std::env::temp_dir().join("ursa_scan_test_csv_options.csv");
        std::fs::write(&path, "# note\nfrom;to;w\n1;2;NA\n2;3;'1.5'\n").unwrap();
        let p = path.to_str().unwrap();
        let csv = CsvOptions {
            delimiter: b';',
            quote: b'\'',
            comment: Some(b'#'),
            null_value: Some("NA".into()),
            ..CsvOptions::default()
        };
        let opts = no_opts();
        let paths = [p.to_string()];
        let source = ScanSource::new(&paths, &opts, &csv);
        let rows = scan_edges_rows(source, "from", "to", &[], &[], false).unwrap();
        let w = rows.batches[0].column_by_name("w").unwrap();
        assert_eq!(w.data_type(), &DataType::Float64);
        assert!(w.is_null(0));
        assert_eq!(sources_of(&rows.batches), vec![1, 2]);
        assert!(CsvOptions::default().is_default() && !csv.is_default());
        // The defaults are DataFusion's own.
        let d = CsvOptions::default().read_options();
        let df = CsvReadOptions::default();
        assert_eq!(
            (
                d.has_header,
                d.delimiter,
                d.quote,
                d.escape,
                d.comment,
                d.null_regex
            ),
            (
                df.has_header,
                df.delimiter,
                df.quote,
                df.escape,
                df.comment,
                df.null_regex
            )
        );
    }
}
