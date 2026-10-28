//! Reading edge/node files into Arrow through a DataFusion scan.
//!
//! This is the "scan produces the Arrow columns" half of the ingress story: a
//! Parquet or CSV path is read through DataFusion — which pushes the projection
//! into the file — and comes back as a `RecordBatch` ready for `build_topology`.
//!
//! Paths may be local, object storage (`s3://`, `gs://`, `az://`, and `file://`),
//! or a plain `http(s)://` URL (a single hosted file). For a remote scheme the
//! matching `object_store` backend is registered on the context before the read,
//! seeded with the caller's `storage_options` layered over the backend's default
//! credential chain (`from_env`). Projection pushdown still applies over the
//! network — only the selected columns' byte ranges are fetched via ranged GETs
//! (for an HTTP store, when the server honors range requests).
//!
//! The session a scan runs on is built once per object-store identity and cloned
//! per scan (see [`scan_state`]), so repeated scans reuse the client (its
//! connection pool and credentials) and, for local files, DataFusion's Parquet
//! footer and statistics caches.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use arrow::array::{Array, RecordBatch};
use arrow::compute::kernels::boolean::{and, is_not_null};
use arrow::compute::{cast, filter};
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::cache::cache_manager::CacheManagerConfig;
use datafusion::execution::context::SessionState;
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::physical_plan::{execute_stream_partitioned, ExecutionPlan};
use datafusion::prelude::{
    CsvReadOptions, DataFrame, ParquetReadOptions, SessionConfig, SessionContext,
};
use futures::StreamExt;
use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey};
use object_store::azure::{AzureConfigKey, MicrosoftAzureBuilder};
use object_store::gcp::{GoogleCloudStorageBuilder, GoogleConfigKey};
use object_store::http::HttpBuilder;
use object_store::local::LocalFileSystem;
use object_store::ObjectStore;
use serde_json::Value;
use url::{Position, Url};
use ursa_core::{EdgeInterner, IdError, IdMap, Topology};

/// The object store for a scan `path`, with the `scheme://authority` base it is
/// registered under. A schemeless (bare local) path parses as an error and needs
/// no store — the default registry already serves the local filesystem. Remote
/// backends are seeded from `opts` layered over their default credential chain
/// (`from_env`), so an empty `opts` still works with env/instance-profile
/// credentials.
fn object_store_for(
    path: &str,
    opts: &HashMap<String, String>,
) -> Result<Option<(Url, Arc<dyn ObjectStore>)>> {
    let url = match Url::parse(path) {
        Ok(u) => u,
        Err(_) => return Ok(None), // bare local path (e.g. "/tmp/x.csv" or "edges.csv")
    };
    let scheme = url.scheme();
    let opt_err = |e: object_store::Error| DataFusionError::Execution(e.to_string());
    let store: Arc<dyn ObjectStore> = match scheme {
        "file" => Arc::new(LocalFileSystem::new()),
        "s3" | "s3a" => {
            let mut b = AmazonS3Builder::from_env().with_url(path);
            for (k, v) in opts {
                b = b.with_config(k.parse::<AmazonS3ConfigKey>().map_err(opt_err)?, v.clone());
            }
            Arc::new(b.build().map_err(opt_err)?)
        }
        "gs" => {
            let mut b = GoogleCloudStorageBuilder::from_env().with_url(path);
            for (k, v) in opts {
                b = b.with_config(k.parse::<GoogleConfigKey>().map_err(opt_err)?, v.clone());
            }
            Arc::new(b.build().map_err(opt_err)?)
        }
        "az" | "azure" | "abfs" | "abfss" | "adl" => {
            let mut b = MicrosoftAzureBuilder::from_env().with_url(path);
            for (k, v) in opts {
                b = b.with_config(k.parse::<AzureConfigKey>().map_err(opt_err)?, v.clone());
            }
            Arc::new(b.build().map_err(opt_err)?)
        }
        // A plain HTTP(S) URL is a single-file read over an object_store `http`
        // backend (WebDAV-style). No credentials/globbing — one file at one URL —
        // but it lets a user point `scan_edges` straight at a hosted Parquet/CSV.
        // `allow_http(true)` is required for a plain `http://` URL (object_store
        // rejects non-TLS by default); the user opted into HTTP by using the scheme.
        "http" | "https" => {
            let base = &url[..Position::BeforePath]; // scheme://host[:port]
            let client_opts = object_store::ClientOptions::new().with_allow_http(true);
            Arc::new(
                HttpBuilder::new()
                    .with_url(base)
                    .with_client_options(client_opts)
                    .build()
                    .map_err(opt_err)?,
            )
        }
        other => {
            return Err(DataFusionError::NotImplemented(format!(
                "scan: unsupported URL scheme {other:?} (supported: file, s3, gs, az, http, https)"
            )))
        }
    };
    // DataFusion routes by scheme + authority. Include the port (`Position::
    // BeforePath` yields `scheme://host[:port]`) so an `http://host:PORT/...` URL
    // registers under the exact authority the read will look up — cloud buckets
    // have no port, so this is identical to the old `scheme://host` for them.
    let base = Url::parse(&url[..Position::BeforePath])
        .map_err(|e| DataFusionError::Execution(format!("scan: bad object-store url: {e}")))?;
    Ok(Some((base, store)))
}

/// Environment variables the cloud builders' `from_env` (and their credential
/// providers) read. They are part of a client's identity: a cached client is
/// reused only while rebuilding it would read the same environment.
fn credential_env() -> Vec<(String, String)> {
    const PREFIXES: [&str; 4] = ["AWS_", "GOOGLE_", "AZURE_", "GCE_"];
    const NAMES: [&str; 2] = ["SERVICE_ACCOUNT", "IDENTITY_ENDPOINT"];
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| PREFIXES.iter().any(|p| k.starts_with(p)) || NAMES.contains(&k.as_str()))
        .collect();
    env.sort();
    env
}

/// What a scan's session depends on: the store's `scheme://authority`, and for a
/// cloud store the `storage_options` and credential environment it is built
/// from. Two scans with equal keys would build identical clients, so they share
/// one. Every local path (bare or `file://`) shares the `None` base.
#[derive(PartialEq, Eq, Hash)]
struct StoreKey {
    base: Option<String>,
    options: Vec<(String, String)>,
    env: Vec<(String, String)>,
}

impl StoreKey {
    fn new(path: &str, opts: &HashMap<String, String>) -> Self {
        let Ok(url) = Url::parse(path) else {
            return Self::local();
        };
        match url.scheme() {
            "file" => Self::local(),
            // The HTTP store takes no options or credentials.
            "http" | "https" => Self {
                base: Some(url[..Position::BeforePath].to_string()),
                options: Vec::new(),
                env: Vec::new(),
            },
            _ => {
                let mut options: Vec<_> =
                    opts.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                options.sort();
                Self {
                    base: Some(url[..Position::BeforePath].to_string()),
                    options,
                    env: credential_env(),
                }
            }
        }
    }

    fn local() -> Self {
        Self {
            base: None,
            options: Vec::new(),
            env: Vec::new(),
        }
    }

    fn is_local(&self) -> bool {
        self.base.is_none()
    }
}

/// Scan session templates by store identity. Bounded so a caller rotating
/// short-lived credentials through `storage_options` cannot grow it without
/// limit: past the cap it starts over (each entry is one client plus its caches).
static SCAN_STATES: OnceLock<Mutex<HashMap<StoreKey, SessionState>>> = OnceLock::new();
const MAX_SCAN_STATES: usize = 64;

/// The session state a scan of `path` runs on: a clone of the template for the
/// path's [`StoreKey`], built on first use by [`build_scan_state`].
///
/// Building a session registers every DataFusion built-in and, for a cloud
/// store, a fresh client from the environment; a clone shares all of that,
/// including the store's connection pool and the runtime's file caches.
fn scan_state(path: &str, opts: &HashMap<String, String>) -> Result<SessionState> {
    let key = StoreKey::new(path, opts);
    let states = SCAN_STATES.get_or_init(Default::default);
    if let Some(state) = states
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
    {
        return Ok(state.clone());
    }
    // Built outside the lock: a cloud builder may read a credentials file.
    let state = build_scan_state(path, opts, key.is_local())?;
    let mut states = states.lock().unwrap_or_else(PoisonError::into_inner);
    if states.len() >= MAX_SCAN_STATES {
        states.clear();
    }
    Ok(states.entry(key).or_insert(state).clone())
}

/// A scan session for `path`'s object store.
///
/// The runtime keeps DataFusion's Parquet metadata (footer) and file statistics
/// caches for local files only. Both validate an entry against the file's size
/// and modification time, which a scan re-reads from the filesystem every time,
/// so a file rewritten in place is read afresh. Remote stores keep them off: an
/// S3 or HTTP modification time has one-second resolution, too coarse to tell a
/// same-size rewrite apart. The file-listing cache is off everywhere, since it
/// never revalidates: a glob would miss files added after its first scan, and a
/// stale listing would hand the other caches stale sizes and times to check.
fn build_scan_state(
    path: &str,
    opts: &HashMap<String, String>,
    local: bool,
) -> Result<SessionState> {
    // Work stealing lets an idle partition read byte ranges planned for a sibling,
    // so rows land in a run-dependent partition. Turning it off keeps each
    // partition on its own contiguous range, which `collect_in_file_order` relies on.
    let mut config = SessionConfig::new().with_batch_size(SCAN_BATCH_ROWS);
    let execution = &mut config.options_mut().execution;
    execution.enable_file_stream_work_stealing = false;
    // DataFusion reads every Parquet string column as `Utf8View` by default, which
    // turns a plain string file into view-typed columns: an extra conversion for
    // the ids and, for attribute columns, a type pyarrow compute can barely
    // filter or sort. Read each column as the file declares it instead; a file
    // that really stores string_view still arrives as `Utf8View`.
    execution.parquet.schema_force_view_types = false;
    // A filter pushed into a scan (#192) runs on each file partition in place: a
    // round-robin repartition in front of it would interleave the partitions'
    // rows and lose file order.
    config
        .options_mut()
        .optimizer
        .enable_round_robin_repartition = false;

    let mut caches = CacheManagerConfig::default().with_list_files_cache_limit(0);
    if !local {
        caches = caches
            .with_file_statistics_cache_limit(0)
            .with_metadata_cache_limit(0);
    }
    let runtime = RuntimeEnvBuilder::new()
        .with_cache_manager(caches)
        .build_arc()?;
    if let Some((base, store)) = object_store_for(path, opts)? {
        runtime.register_object_store(&base, store);
    }
    Ok(SessionStateBuilder::new()
        .with_config(config)
        .with_runtime_env(runtime)
        .with_default_features()
        .build())
}

/// What each scan read, while recording is on (test/observability hook): the
/// scan's kind (`index`, `edges`, `table`, `nodes`), the columns it was asked
/// for, and how many filters it pushed into the read.
static SCAN_LOG: Mutex<Option<Vec<ScanRecord>>> = Mutex::new(None);

/// One logged scan: `(kind, columns, filters pushed)`.
pub type ScanRecord = (String, Vec<String>, usize);

fn log_scan(kind: &str, columns: &[String], pushed: usize) {
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
enum ScanFormat {
    Parquet,
    Csv,
}

fn detect_format(path: &str, verb: &str) -> Result<ScanFormat> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".parquet") {
        return Ok(ScanFormat::Parquet);
    }
    if lower.ends_with(".csv") {
        return Ok(ScanFormat::Csv);
    }
    if let Ok(u) = Url::parse(path) {
        if matches!(u.scheme(), "http" | "https") && u.query().is_some() {
            return Err(DataFusionError::NotImplemented(format!(
                "{verb}: an http(s) URL with a query string is not supported — the scan \
                 drops the query before fetching, so a presigned/token URL would fetch the \
                 unsigned path and fail. Point at a direct .parquet/.csv URL, or use \
                 s3://gs://az:// with storage_options for signed access. got {path:?}"
            )));
        }
    }
    Err(DataFusionError::NotImplemented(format!(
        "{verb} supports .parquet and .csv in v0.1; got path {path:?}"
    )))
}

/// The canonical Arrow type for a node-id column: any integer type collapses to
/// `Int64` (the fast path), `Utf8`/`LargeUtf8`/`Utf8View` to `Utf8` (string ids,
/// covering UUID-as-string). A Parquet file whose Arrow schema says string_view
/// (as DataFusion writes it) arrives as `Utf8View`. A dictionary
/// column (a Parquet file written from a pyarrow/pandas categorical keeps that
/// type in its embedded Arrow schema) canonicalizes by its value type; the cast
/// decodes it. Any other type is not a supported node-id type.
fn canonical_id_type(dt: &DataType) -> Result<DataType> {
    use DataType::*;
    match dt {
        Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64 => Ok(Int64),
        Utf8 | LargeUtf8 | Utf8View => Ok(Utf8),
        Dictionary(_, value) => canonical_id_type(value),
        other => Err(DataFusionError::NotImplemented(format!(
            "node ids must be an integer or string column; {other:?} is not a supported id type"
        ))),
    }
}

/// Rows per scanned batch. DataFusion's default (8,192) makes the index build
/// intern in small pieces; at 128K rows `EdgeInterner` resolves known ids in
/// parallel, which more than halves a 30M-edge build. Batches stay small next to
/// the graph itself (2 MiB of `Int64` endpoints).
const SCAN_BATCH_ROWS: usize = 1 << 17;

/// Open a scan source into an unprojected `DataFrame`: take the session for the
/// path's object store (see [`scan_state`]) and read the file in the format its
/// extension names. The shared prologue of `scan_edges_batch` and
/// `scan_nodes_batch` — each then applies its own projection and id canonicalization.
///
/// `kind` (`"scan_edges"`/`"scan_nodes"`) only tunes the error message from
/// [`detect_format`]. The returned `DataFrame` carries its own `Arc`-shared session
/// state (including the registered store), so the local `SessionContext` need not
/// outlive this call.
async fn open_scan(
    path: &str,
    storage_options: &HashMap<String, String>,
    kind: &str,
) -> Result<DataFrame> {
    let ctx = SessionContext::new_with_state(scan_state(path, storage_options)?);
    Ok(match detect_format(path, kind)? {
        ScanFormat::Parquet => {
            ctx.read_parquet(path, ParquetReadOptions::default())
                .await?
        }
        ScanFormat::Csv => ctx.read_csv(path, CsvReadOptions::default()).await?,
    })
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
    path: &str,
    src: &str,
    dst: &str,
    storage_options: &HashMap<String, String>,
    weight_columns: &[String],
) -> Result<Vec<RecordBatch>> {
    log_scan("edges", weight_columns, 0);
    crate::runtime::block_on(async move {
        let df = open_scan(path, storage_options, "scan_edges").await?;

        // Project src, dst, then any weight columns not already among them.
        let mut proj: Vec<&str> = vec![src, dst];
        for c in weight_columns {
            if !proj.contains(&c.as_str()) {
                proj.push(c.as_str());
            }
        }
        let df = df.select_columns(&proj)?;
        // Keep the scan's batches separate (no `concat_batches` into one contiguous
        // batch) so the transient ingest footprint stays ~1×, not ~2×, at the
        // 500M-edge target; the topology build consumes them as a stream (#60).
        let batches = collect_in_file_order(df).await?;
        if batches.is_empty() || batches.iter().all(|b| b.num_rows() == 0) {
            return Err(empty_edge_source(path));
        }

        // The canonical id type is taken once from the read schema (identical across
        // the scan's batches) and every batch is canonicalized to it independently.
        let read_schema = batches[0].schema();
        let id_type = endpoint_id_type(&read_schema, path)?;
        // Output layout: canonical src/dst, then each weight column by its own
        // name, as an index into the read batch. An endpoint requested as a weight
        // column (#154) points back at read column 0/1 (uncast).
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
        let out_schema = Arc::new(Schema::new(fields));

        // Canonicalize each batch independently, in parallel (rayon's collect keeps
        // batch order). Consuming the list releases each batch after its cast, so a
        // narrower integer or a view-typed string id column is converted batch by
        // batch rather than all at once.
        use rayon::prelude::*;
        let out = batches
            .into_par_iter()
            .filter(|batch| batch.num_rows() > 0) // the non-empty guard above ensures ≥1 remains
            .map(|batch| {
                let src_c = cast(batch.column(0), &id_type).map_err(arrow_err)?;
                let dst_c = cast(batch.column(1), &id_type).map_err(arrow_err)?;
                let mut columns = vec![src_c, dst_c];
                columns.extend(extra.iter().map(|&i| batch.column(i).clone()));
                RecordBatch::try_new(out_schema.clone(), columns).map_err(arrow_err)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(out)
    })?
}

/// Read an edge file as a table — every column, or the `columns` projection —
/// for the relational (non-graph) paths: a plain `collect()` and its steps, the
/// sinks, and a subgraph view's edge mask (#160). See [`scan_edges_rows`], which
/// also takes predicates to push into the scan.
pub fn scan_edges_table(
    path: &str,
    src: &str,
    dst: &str,
    storage_options: &HashMap<String, String>,
    columns: &[String],
) -> Result<Vec<RecordBatch>> {
    Ok(scan_edges_rows(path, src, dst, storage_options, columns, &[], false)?.batches)
}

/// An edge file's rows, read by [`scan_edges_rows`].
pub struct EdgeRows {
    /// At least one batch, carrying the schema.
    pub batches: Vec<RecordBatch>,
    /// Rows dropped for a null endpoint (always 0 unless `drop_nulls`).
    pub dropped: usize,
}

/// Read an edge file as a table, keeping the rows every predicate in `filters`
/// holds for.
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
/// (projection pushdown), in file order. A name the file lacks is skipped here
/// and reported by the caller's own column check, so the error matches the
/// in-memory frame's. `filters` (predicate JSON, roles resolved, every column
/// read) are pushed into the scan: a Parquet file skips the row groups whose
/// statistics rule them out (#192). With `drop_nulls` (`on_null="drop"`) rows
/// with a null endpoint are removed and counted in [`EdgeRows::dropped`]; pass no
/// `filters` then, so the count covers the whole file. The graph-op paths never
/// come here: the index build and the weighted scan keep their narrow
/// projections ([`scan_edges_topology`], [`scan_edges_batch`]).
#[allow(clippy::too_many_arguments)]
pub fn scan_edges_rows(
    path: &str,
    src: &str,
    dst: &str,
    storage_options: &HashMap<String, String>,
    columns: &[String],
    filters: &[Value],
    drop_nulls: bool,
) -> Result<EdgeRows> {
    let (rows, pushed, _) = scan_edges_rows_plan(
        path,
        src,
        dst,
        storage_options,
        columns,
        filters,
        drop_nulls,
    )?;
    if pushed < filters.len() {
        return Err(DataFusionError::Execution(format!(
            "scan_edges: filter {pushed} reads a column the scan does not read"
        )));
    }
    Ok(rows)
}

/// [`scan_edges_rows`], pushing the longest prefix of `filters` whose columns
/// the read has, and returning how many it pushed. The caller runs the rest, so
/// a filter over a column the file lacks raises the caller's own error.
#[allow(clippy::too_many_arguments)]
pub fn scan_edges_rows_pushing(
    path: &str,
    src: &str,
    dst: &str,
    storage_options: &HashMap<String, String>,
    columns: &[String],
    filters: &[Value],
    drop_nulls: bool,
) -> Result<(EdgeRows, usize)> {
    let (rows, pushed, _) = scan_edges_rows_plan(
        path,
        src,
        dst,
        storage_options,
        columns,
        filters,
        drop_nulls,
    )?;
    Ok((rows, pushed))
}

/// [`scan_edges_rows`], also returning the executed scan plan (for its metrics).
#[allow(clippy::too_many_arguments)]
fn scan_edges_rows_plan(
    path: &str,
    src: &str,
    dst: &str,
    storage_options: &HashMap<String, String>,
    columns: &[String],
    filters: &[Value],
    drop_nulls: bool,
) -> Result<(EdgeRows, usize, Arc<dyn ExecutionPlan>)> {
    crate::runtime::block_on(async move {
        let df = open_scan(path, storage_options, "scan_edges").await?;
        let schema = df.schema().as_arrow().clone();
        for end in [src, dst] {
            if schema.index_of(end).is_err() {
                return Err(DataFusionError::Execution(format!(
                    "scan_edges: endpoint column {end:?} not found in edge file {path:?}"
                )));
            }
        }
        let df = if columns.is_empty() {
            df
        } else {
            let keep: Vec<&str> = schema
                .fields()
                .iter()
                .map(|f| f.name().as_str())
                .filter(|n| *n == src || *n == dst || columns.iter().any(|c| c == n))
                .collect();
            df.select_columns(&keep)?
        };
        let read_schema = df.schema().as_arrow().clone();
        // Validate the endpoint types exactly as the graph build would.
        let ends = Schema::new(vec![
            read_schema.field_with_name(src)?.clone(),
            read_schema.field_with_name(dst)?.clone(),
        ]);
        endpoint_id_type(&ends, path)?;

        let mut filtered = df.clone();
        let mut pushed = 0;
        for f in filters {
            let Ok(expr) = crate::eval::logical(f, filtered.schema()) else {
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
        if empty(&batches) {
            // An empty file is an error; an empty filter result is not.
            let any = if filters.is_empty() {
                false
            } else {
                !empty(&df.limit(0, Some(1))?.collect().await?)
            };
            if !any {
                return Err(empty_edge_source(path));
            }
        }
        let mut dropped = 0;
        if drop_nulls {
            let (s, d) = (read_schema.index_of(src)?, read_schema.index_of(dst)?);
            for b in &mut batches {
                let keep = and(
                    &is_not_null(b.column(s)).map_err(arrow_err)?,
                    &is_not_null(b.column(d)).map_err(arrow_err)?,
                )
                .map_err(arrow_err)?;
                let kept = arrow::compute::filter_record_batch(b, &keep).map_err(arrow_err)?;
                dropped += b.num_rows() - kept.num_rows();
                *b = kept;
            }
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

/// `f` with a `Utf8View` type replaced by `Utf8` (see [`scan_nodes_batch`]).
fn unview_field(f: &Field) -> Field {
    match f.data_type() {
        DataType::Utf8View => Field::new(f.name(), DataType::Utf8, f.is_nullable()),
        _ => f.clone(),
    }
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

fn arrow_err(e: arrow::error::ArrowError) -> DataFusionError {
    DataFusionError::ArrowError(Box::new(e), None)
}

/// The error for an edge source that resolved to zero rows.
fn empty_edge_source(path: &str) -> DataFusionError {
    DataFusionError::Execution(format!(
        "edge source {path:?} resolved but contained no rows; an empty edge set \
         is not a graph in v0.1 (check the path/glob points at data with the \
         given src/dst columns)"
    ))
}

/// The canonical node-id type shared by the `src` and `dst` columns (fields 0 and
/// 1 of `schema`). Both must canonicalize to the same family.
fn endpoint_id_type(schema: &Schema, path: &str) -> Result<DataType> {
    let src_type = canonical_id_type(schema.field(0).data_type())?;
    let dst_type = canonical_id_type(schema.field(1).data_type())?;
    if src_type != dst_type {
        return Err(DataFusionError::NotImplemented(format!(
            "src and dst node-id columns must be the same type (both int or both string); \
             got {src_type:?} and {dst_type:?} in {path:?}"
        )));
    }
    Ok(src_type)
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
    path: &str,
    src: &str,
    dst: &str,
    storage_options: &HashMap<String, String>,
    drop_nulls: bool,
) -> std::result::Result<ScannedTopology, ScanBuildError> {
    log_scan("index", &[], 0);
    let (interner, dropped) = crate::runtime::block_on(async move {
        let df = open_scan(path, storage_options, "scan_edges").await?;
        let df = df.select_columns(&[src, dst])?;
        let id_type = endpoint_id_type(df.schema().as_arrow(), path)?;
        let task_ctx = df.task_ctx();
        let plan = df.create_physical_plan().await?;
        // Parquet metadata gives the row count up front, so the dense endpoint
        // vectors are sized once; a CSV has no count and they grow as needed.
        let hint = plan
            .partition_statistics(None)?
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
                if drop_nulls && (src_c.null_count() > 0 || dst_c.null_count() > 0) {
                    let keep = and(
                        &is_not_null(&src_c).map_err(arrow_err)?,
                        &is_not_null(&dst_c).map_err(arrow_err)?,
                    )
                    .map_err(arrow_err)?;
                    let src_c = filter(&src_c, &keep).map_err(arrow_err)?;
                    let dst_c = filter(&dst_c, &keep).map_err(arrow_err)?;
                    dropped += keep.len() - src_c.len();
                    interner.push(&src_c, &dst_c)?;
                } else {
                    interner.push(&src_c, &dst_c)?;
                }
            }
        }
        Ok::<_, ScanBuildError>((interner, dropped))
    })??;
    if interner.is_empty() && dropped == 0 {
        return Err(empty_edge_source(path).into());
    }
    let (ids, src_dense, dst_dense) = interner.finish();
    let topo = Topology::build(ids.len(), src_dense, dst_dense);
    Ok(ScannedTopology {
        topo: Arc::new(topo),
        ids: Arc::new(ids),
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
    path: &str,
    id: &str,
    storage_options: &HashMap<String, String>,
    columns: &[String],
) -> Result<Vec<RecordBatch>> {
    log_scan("nodes", columns, 0);
    crate::runtime::block_on(async move {
        let df = open_scan(path, storage_options, "scan_nodes").await?;

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
        let batches = collect_in_file_order(df).await?;
        if batches.is_empty() || batches.iter().all(|b| b.num_rows() == 0) {
            return Err(DataFusionError::Execution(format!(
                "node source {path:?} resolved but contained no rows (check the path/glob \
                 points at data with the given id column)"
            )));
        }

        let read_schema = batches[0].schema();
        let id_idx = read_schema.index_of(id).map_err(|_| {
            DataFusionError::Execution(format!(
                "scan_nodes: id column {id:?} not found in node file {path:?}"
            ))
        })?;

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
        cast_batches(&batches, &Arc::new(Schema::new(fields)))
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
        path: &str,
        id: &str,
        storage_options: &HashMap<String, String>,
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
        let batches = scan_nodes_batch(path, id, storage_options, read.as_deref().unwrap_or(&[]))?;
        self.scans
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(out) = project(read.as_deref(), &batches, columns)? else {
            // A column the file lacks: the narrow read raises the scan's error.
            return scan_nodes_batch(path, id, storage_options, columns.unwrap_or(&[]));
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
    use arrow::array::Int64Array;
    use std::io::Write;

    fn no_opts() -> HashMap<String, String> {
        HashMap::new()
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
        let batches = scan_edges_batch(p, "from", "to", &no_opts(), &[]).unwrap();
        let pairs: Vec<(&dyn Array, &dyn Array)> = batches
            .iter()
            .map(|b| (b.column(0).as_ref(), b.column(1).as_ref()))
            .collect();
        let (topo, ids) = crate::build_topology_batches(&pairs).unwrap();
        let scanned = scan_edges_topology(p, "from", "to", &no_opts(), false).unwrap();
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
        let pred = serde_json::json!({"kind": "binary", "op": ">=",
            "left": {"kind": "col", "name": "from"}, "right": {"kind": "lit", "value": 1_990_000}});
        let (rows, pushed, plan) =
            scan_edges_rows_plan(p, "from", "to", &no_opts(), &[], &[pred], false).unwrap();
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
        let none = serde_json::json!({"kind": "binary", "op": "<",
            "left": {"kind": "col", "name": "from"}, "right": {"kind": "lit", "value": 0}});
        let rows = scan_edges_rows(p, "from", "to", &no_opts(), &[], &[none], false).unwrap();
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
            .read(p, "id", &no_opts(), Some(&cols(&["id", "a", "b"])))
            .unwrap();
        assert_eq!(names(&ab), ["id", "a", "b"]);
        // A subset, in another order, is served from the held table.
        let ba = cache
            .read(p, "id", &no_opts(), Some(&cols(&["b", "id"])))
            .unwrap();
        assert_eq!(names(&ba), ["b", "id"]);
        assert_eq!(cache.scans(), 1);
        // A new column widens the held table to the union.
        let c = cache
            .read(p, "id", &no_opts(), Some(&cols(&["id", "c"])))
            .unwrap();
        assert_eq!(names(&c), ["id", "c"]);
        assert_eq!(cache.held(), Some(Some(cols(&["id", "a", "b", "c"]))));
        assert_eq!(cache.scans(), 2);
        // Every column, then any projection, read it once more at most.
        let all = cache.read(p, "id", &no_opts(), None).unwrap();
        assert_eq!(names(&all), ["id", "a", "b", "c"]);
        cache
            .read(p, "id", &no_opts(), Some(&cols(&["a", "id"])))
            .unwrap();
        assert_eq!(cache.scans(), 3);
        // A column the file lacks is the scan's own error.
        let err = cache
            .read(p, "id", &no_opts(), Some(&cols(&["id", "nope"])))
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
            scan_edges_topology(p, "from", "to", &no_opts(), false),
            Err(ScanBuildError::Build(IdError::Null))
        ));
        let scanned = scan_edges_topology(p, "from", "to", &no_opts(), true).unwrap();
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

        let batches = scan_edges_batch(p, "from", "to", &no_opts(), &[]).unwrap();
        assert_eq!(batches[0].column(0).data_type(), &DataType::Utf8);
        let scanned = scan_edges_topology(p, "from", "to", &no_opts(), false).unwrap();
        assert_eq!((scanned.topo.n_nodes(), scanned.topo.n_edges()), (3, 3));
        let nodes = scan_nodes_batch(p, "from", &no_opts(), &[]).unwrap();
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
            let batches = scan_edges_batch(p, "from", "to", &no_opts(), &label).unwrap();
            assert_eq!(batches[0].column(0).data_type(), &DataType::Utf8, "{name}");
            assert_eq!(batches[0].column(1).data_type(), &DataType::Utf8, "{name}");
            assert_eq!(batches[0].num_columns(), 3, "{name}");
            let scanned = scan_edges_topology(p, "from", "to", &no_opts(), false).unwrap();
            let nodes = scan_nodes_batch(p, "from", &no_opts(), &[]).unwrap();
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
            let table = scan_edges_table(p, "from", "to", &no_opts(), &[]).unwrap();
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
            let batches =
                scan_edges_batch(path.to_str().unwrap(), "from", "to", &no_opts(), &[]).unwrap();
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
        let batches =
            scan_edges_table(path.to_str().unwrap(), "from", "to", &no_opts(), &[]).unwrap();
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
        let batches = scan_edges_table(p, "from", "to", &no_opts(), &[]).unwrap();
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
        let batches = scan_edges_table(p, "from", "to", &no_opts(), &cols).unwrap();
        let schema = batches[0].schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, ["from", "label", "to"]);

        // A missing endpoint names the column; a non-id endpoint is rejected as
        // the graph build would reject it.
        let err = scan_edges_table(p, "from", "nope", &no_opts(), &[]).unwrap_err();
        assert!(err.to_string().contains("\"nope\" not found"), "{err}");
        let err = scan_edges_table(p, "from", "w", &no_opts(), &[]).unwrap_err();
        assert!(err.to_string().contains("not a supported id type"), "{err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn edge_table_scan_reads_every_csv_column() {
        let path = std::env::temp_dir().join("ursa_scan_test_edge_table.csv");
        std::fs::write(&path, "from,to,weight,label\n10,20,0.5,a\n20,30,0.9,b\n").unwrap();
        let batches =
            scan_edges_table(path.to_str().unwrap(), "from", "to", &no_opts(), &[]).unwrap();
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
        let batches = scan_edges_batch(p, "from", "to", &no_opts(), &cols).unwrap();
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
        let batches =
            scan_edges_batch(path.to_str().unwrap(), "src", "dst", &no_opts(), &cols).unwrap();
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
        let batches =
            scan_edges_batch(path.to_str().unwrap(), "from", "to", &no_opts(), &[]).unwrap();
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
        let batches =
            scan_nodes_batch(path.to_str().unwrap(), "tower_id", &no_opts(), &[]).unwrap();
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
        let batches =
            scan_nodes_batch(path.to_str().unwrap(), "tower_id", &no_opts(), &cols).unwrap();
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
        let batches = scan_edges_batch(&url, "from", "to", &no_opts(), &[]).unwrap();
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
        scan_edges_batch(path, "from", "to", &no_opts(), &[])
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
        assert_eq!(options.execution.batch_size, SCAN_BATCH_ROWS);
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
            detect_format("edges.parquet", "scan_edges").unwrap(),
            ScanFormat::Parquet
        ));
        assert!(matches!(
            detect_format("/tmp/local/edges.CSV", "scan_edges").unwrap(),
            ScanFormat::Csv
        ));
        // An object-store glob keeps `?`/`*` as glob metacharacters (regression
        // guard: the query-stripping approach broke `part-?.parquet`).
        assert!(matches!(
            detect_format("s3://bucket/part-?.parquet", "scan_edges").unwrap(),
            ScanFormat::Parquet
        ));
        assert!(matches!(
            detect_format("gs://bucket/*.csv", "scan_nodes").unwrap(),
            ScanFormat::Csv
        ));
    }

    #[test]
    fn detect_format_rejects_http_query_string_clearly() {
        // The engine drops the query before fetching, so a presigned/token URL
        // would silently fetch the unsigned path — reject it with a clear message.
        let err = detect_format("https://host/data/edges.csv?token=abc123", "scan_edges");
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
}
