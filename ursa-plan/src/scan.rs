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

use arrow::array::RecordBatch;
use arrow::compute::cast;
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::{CsvReadOptions, ParquetReadOptions, SessionContext};
use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey};
use object_store::azure::{AzureConfigKey, MicrosoftAzureBuilder};
use object_store::gcp::{GoogleCloudStorageBuilder, GoogleConfigKey};
use object_store::http::HttpBuilder;
use object_store::local::LocalFileSystem;
use object_store::ObjectStore;
use url::{Position, Url};

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

/// The path portion of a scan target, lowercased, for extension matching. For a
/// URL this strips the query/fragment (`.../edges.csv?token=…` → `/edges.csv`) so
/// a presigned/parameterized URL still resolves its format; a bare local path is
/// returned lowercased as-is.
fn ext_path(path: &str) -> String {
    match Url::parse(path) {
        Ok(u) => u.path().to_ascii_lowercase(),
        Err(_) => path.to_ascii_lowercase(),
    }
}

/// The `file_extension` to hand DataFusion's reader. Normally the format's own
/// extension (so a directory/glob read still filters correctly), but a URL with a
/// query string (`edges.csv?token=…`) does not literally end in the extension, so
/// return `""` (no filter) — `ext_path` already proved the format for the single
/// file the caller named.
fn read_ext<'a>(path: &str, default: &'a str) -> &'a str {
    if path.contains('?') {
        ""
    } else {
        default
    }
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
        let ctx = SessionContext::new();
        register_object_store(&ctx, path, storage_options)?;
        let lower = ext_path(path);
        // A URL query string (`edges.csv?token=…`) defeats DataFusion's own
        // end-of-path extension check even though `ext_path` already resolved the
        // format; `read_ext` clears the extension filter in that case so the read
        // proceeds.
        let df = if lower.ends_with(".parquet") {
            let opts = ParquetReadOptions::default().file_extension(read_ext(path, ".parquet"));
            ctx.read_parquet(path, opts).await?
        } else if lower.ends_with(".csv") {
            let opts = CsvReadOptions::default().file_extension(read_ext(path, ".csv"));
            ctx.read_csv(path, opts).await?
        } else {
            return Err(DataFusionError::NotImplemented(format!(
                "scan_edges supports .parquet and .csv in v0.1; got path {path:?}"
            )));
        };

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
        let batches = df.collect().await?;
        if batches.is_empty() || batches.iter().all(|b| b.num_rows() == 0) {
            return Err(DataFusionError::Execution(format!(
                "edge source {path:?} resolved but contained no rows; an empty edge set \
                 is not a graph in v0.1 (check the path/glob points at data with the \
                 given src/dst columns)"
            )));
        }

        // The canonical id type is taken once from the read schema (identical across
        // the scan's batches) and every batch is canonicalized to it independently.
        let read_schema = batches[0].schema();
        let src_type = canonical_id_type(read_schema.field(0).data_type())?;
        let dst_type = canonical_id_type(read_schema.field(1).data_type())?;
        if src_type != dst_type {
            return Err(DataFusionError::NotImplemented(format!(
                "src and dst node-id columns must be the same type (both int or both string); \
                 got {src_type:?} and {dst_type:?} in {path:?}"
            )));
        }
        // src/dst canonicalized; weight columns (positions 2..) passed through.
        let mut fields = vec![
            Field::new("src", src_type.clone(), true),
            Field::new("dst", dst_type.clone(), true),
        ];
        for i in 2..read_schema.fields().len() {
            fields.push(read_schema.field(i).clone());
        }
        let out_schema = Arc::new(Schema::new(fields));

        let mut out = Vec::with_capacity(batches.len());
        for batch in &batches {
            if batch.num_rows() == 0 {
                continue; // drop empty batches; the non-empty guard above ensures ≥1 remains
            }
            let src_c = cast(batch.column(0), &src_type)
                .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
            let dst_c = cast(batch.column(1), &dst_type)
                .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
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
        let ctx = SessionContext::new();
        register_object_store(&ctx, path, storage_options)?;
        let lower = ext_path(path);
        let df = if lower.ends_with(".parquet") {
            let opts = ParquetReadOptions::default().file_extension(read_ext(path, ".parquet"));
            ctx.read_parquet(path, opts).await?
        } else if lower.ends_with(".csv") {
            let opts = CsvReadOptions::default().file_extension(read_ext(path, ".csv"));
            ctx.read_csv(path, opts).await?
        } else {
            return Err(DataFusionError::NotImplemented(format!(
                "scan_nodes supports .parquet and .csv in v0.1; got path {path:?}"
            )));
        };

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
        let batches = df.collect().await?;
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
    fn ext_path_strips_query_string() {
        // A presigned/parameterized URL keeps its format resolvable: the query is
        // stripped before the extension check. Bare local paths pass through.
        assert!(ext_path("https://host/data/edges.csv?token=abc123").ends_with(".csv"));
        assert!(ext_path("s3://bucket/g.parquet?versionId=9").ends_with(".parquet"));
        assert!(ext_path("/tmp/local/edges.CSV").ends_with(".csv"));
        assert!(ext_path("edges.parquet").ends_with(".parquet"));
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
