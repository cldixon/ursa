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

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{Array, RecordBatch};
use arrow::compute::kernels::boolean::{and, is_not_null};
use arrow::compute::{cast, filter};
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::execute_stream_partitioned;
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
use url::{Position, Url};
use ursa_core::{EdgeInterner, IdError, IdMap, Topology};

/// Register the object store for a scan `path` on `ctx`, keyed by the URL's
/// `scheme://authority`. A schemeless (bare local) path parses as an error and is
/// a no-op — the local filesystem needs no registration. Remote backends are
/// seeded from `opts` layered over their default credential chain (`from_env`), so
/// an empty `opts` still works with env/instance-profile credentials.
fn register_object_store(
    ctx: &SessionContext,
    path: &str,
    opts: &HashMap<String, String>,
) -> Result<()> {
    let url = match Url::parse(path) {
        Ok(u) => u,
        Err(_) => return Ok(()), // bare local path (e.g. "/tmp/x.csv" or "edges.csv")
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
    ctx.register_object_store(&base, store);
    Ok(())
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
/// `Int64` (the fast path), `Utf8`/`LargeUtf8` to `Utf8` (string ids, covering
/// UUID-as-string). Any other type is not a supported node-id type.
fn canonical_id_type(dt: &DataType) -> Result<DataType> {
    use DataType::*;
    match dt {
        Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64 => Ok(Int64),
        Utf8 | LargeUtf8 => Ok(Utf8),
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

/// Open a scan source into an unprojected `DataFrame`: create a fresh session,
/// register the matching object store for the path's scheme, and read the file in
/// the format its extension names. The shared prologue of `scan_edges_batch` and
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
    // Work stealing lets an idle partition read byte ranges planned for a sibling,
    // so rows land in a run-dependent partition. Turning it off keeps each
    // partition on its own contiguous range, which `collect_in_file_order` relies on.
    let mut config = SessionConfig::new().with_batch_size(SCAN_BATCH_ROWS);
    config
        .options_mut()
        .execution
        .enable_file_stream_work_stealing = false;
    let ctx = SessionContext::new_with_config(config);
    register_object_store(&ctx, path, storage_options)?;
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

/// Read the `src`/`dst` columns of an edge file into one `(src, dst)` batch, plus
/// any `weight_columns` needed to evaluate a `weight=` expression.
///
/// Format is chosen by extension (`.parquet` / `.csv`), the two forms Ursa v0.1
/// supports. The projection is pushed down, so only the endpoint columns (and any
/// requested weight columns) are read. The endpoints are canonicalized to a
/// supported node-id type — `Int64` (the fast path) or `Utf8` strings; `src` and
/// `dst` must be the same family. Weight columns keep their file types and appear
/// after `dst`, so a caller can evaluate the weight over the same rows.
pub fn scan_edges_batch(
    path: &str,
    src: &str,
    dst: &str,
    storage_options: &HashMap<String, String>,
    weight_columns: &[String],
) -> Result<Vec<RecordBatch>> {
    crate::runtime::block_on(async move {
        let df = open_scan(path, storage_options, "scan_edges").await?;

        // Project src, dst, then any weight columns not already among them.
        let mut proj: Vec<&str> = vec![src, dst];
        for c in weight_columns {
            if c != src && c != dst && !proj.contains(&c.as_str()) {
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
        // src/dst canonicalized; weight columns (positions 2..) passed through.
        let mut fields = vec![
            Field::new("src", id_type.clone(), true),
            Field::new("dst", id_type.clone(), true),
        ];
        for i in 2..read_schema.fields().len() {
            fields.push(read_schema.field(i).clone());
        }
        let out_schema = Arc::new(Schema::new(fields));

        // Consume the scan's batches so each one is released after its cast; a
        // narrower integer id column is widened per batch, not all at once.
        let mut out = Vec::with_capacity(batches.len());
        for batch in batches {
            if batch.num_rows() == 0 {
                continue; // drop empty batches; the non-empty guard above ensures ≥1 remains
            }
            let src_c = cast(batch.column(0), &id_type).map_err(arrow_err)?;
            let dst_c = cast(batch.column(1), &id_type).map_err(arrow_err)?;
            let mut columns = vec![src_c, dst_c];
            for i in 2..batch.num_columns() {
                columns.push(batch.column(i).clone());
            }
            out.push(
                RecordBatch::try_new(out_schema.clone(), columns)
                    .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?,
            );
        }
        Ok(out)
    })?
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

        // Canonicalize only the id column (integer -> Int64, string -> Utf8);
        // attribute columns keep their file types. The out schema is built once and
        // shared across batches so they stay concat-compatible on the Python side.
        let id_type = canonical_id_type(read_schema.field(id_idx).data_type())?;
        let fields: Vec<Field> = read_schema
            .fields()
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if i == id_idx {
                    Field::new(f.name(), id_type.clone(), f.is_nullable())
                } else {
                    f.as_ref().clone()
                }
            })
            .collect();
        let out_schema = Arc::new(Schema::new(fields));

        let mut out = Vec::with_capacity(batches.len());
        for batch in &batches {
            if batch.num_rows() == 0 {
                continue;
            }
            let mut columns = batch.columns().to_vec();
            columns[id_idx] = cast(&columns[id_idx], &id_type)
                .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
            out.push(
                RecordBatch::try_new(out_schema.clone(), columns)
                    .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?,
            );
        }
        Ok(out)
    })?
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

    #[test]
    fn builds_s3_store_from_storage_options() {
        // The aws cloud feature is enabled and the storage_options -> config-key
        // mapping resolves: an S3 store builds from a region option, lazily and
        // credential-free (auth is deferred to request time). The GCS/Azure
        // builders are referenced in `register_object_store`, so the gcp/azure
        // features are proven enabled at *compile* time; their `.build()` requires
        // real credentials, so they're not exercised in this credential-free test.
        let ctx = SessionContext::new();
        let opts: HashMap<String, String> = [("region".to_string(), "us-east-1".to_string())]
            .into_iter()
            .collect();
        register_object_store(&ctx, "s3://my-bucket/graph/*.parquet", &opts).unwrap();
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        let ctx = SessionContext::new();
        let err = register_object_store(&ctx, "ftp://host/graph.parquet", &no_opts());
        assert!(err.is_err());
    }

    #[test]
    fn registers_an_http_store_with_port() {
        // The `http` feature is enabled: an http(s) URL registers a store, keyed by
        // the full authority including the port (so localhost:PORT reads route
        // correctly). Credential-free and lazy — no request is made here.
        let ctx = SessionContext::new();
        register_object_store(&ctx, "http://127.0.0.1:8080/graph/edges.csv", &no_opts()).unwrap();
        register_object_store(&ctx, "https://example.com/edges.parquet", &no_opts()).unwrap();
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
        let ctx = SessionContext::new();
        let bad: HashMap<String, String> = [("not_a_real_option".to_string(), "x".to_string())]
            .into_iter()
            .collect();
        assert!(register_object_store(&ctx, "s3://my-bucket/f.parquet", &bad).is_err());
    }
}
