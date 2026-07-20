//! Reading edge/node files into Arrow through a DataFusion scan.
//!
//! This is the "scan produces the Arrow columns" half of the ingress story: a
//! Parquet or CSV path is read through DataFusion — which pushes the projection
//! into the file — and comes back as a `RecordBatch` ready for `build_topology`.
//!
//! Paths may be local, or object storage: `s3://`, `gs://`, `az://` (and
//! `file://`). For a remote scheme the matching `object_store` backend is
//! registered on the context before the read, seeded with the caller's
//! `storage_options` layered over the backend's default credential chain
//! (`from_env`). Projection pushdown still applies over the network — only the
//! selected columns' byte ranges are fetched via ranged GETs.

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::compute::{cast, concat_batches};
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::{CsvReadOptions, ParquetReadOptions, SessionContext};
use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey};
use object_store::azure::{AzureConfigKey, MicrosoftAzureBuilder};
use object_store::gcp::{GoogleCloudStorageBuilder, GoogleConfigKey};
use object_store::local::LocalFileSystem;
use object_store::ObjectStore;
use url::Url;

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
        other => {
            return Err(DataFusionError::NotImplemented(format!(
                "scan: unsupported URL scheme {other:?} (supported: file, s3, gs, az)"
            )))
        }
    };
    // DataFusion routes by scheme + authority (bucket/container/host).
    let host = url.host_str().unwrap_or_default();
    let base = Url::parse(&format!("{scheme}://{host}"))
        .map_err(|e| DataFusionError::Execution(format!("scan: bad object-store url: {e}")))?;
    ctx.register_object_store(&base, store);
    Ok(())
}

/// Read the `src`/`dst` columns of an edge file into one `(src, dst)` Int64 batch.
///
/// Format is chosen by extension (`.parquet` / `.csv`), the two forms Ursa v0.1
/// supports. The projection is pushed down, so only the endpoint columns are read;
/// they are cast to Int64 (the v0.1 node-id type).
pub fn scan_edges_batch(
    path: &str,
    src: &str,
    dst: &str,
    storage_options: &HashMap<String, String>,
) -> Result<RecordBatch> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| DataFusionError::Execution(format!("failed to build runtime: {e}")))?;

    runtime.block_on(async move {
        let ctx = SessionContext::new();
        register_object_store(&ctx, path, storage_options)?;
        let lower = path.to_ascii_lowercase();
        let df = if lower.ends_with(".parquet") {
            ctx.read_parquet(path, ParquetReadOptions::default())
                .await?
        } else if lower.ends_with(".csv") {
            ctx.read_csv(path, CsvReadOptions::default()).await?
        } else {
            return Err(DataFusionError::NotImplemented(format!(
                "scan_edges supports .parquet and .csv in v0.1; got path {path:?}"
            )));
        };

        let df = df.select_columns(&[src, dst])?;
        let batches = df.collect().await?;
        if batches.is_empty() {
            return Err(DataFusionError::Execution(format!(
                "edge file {path:?} produced no rows"
            )));
        }

        let read_schema = batches[0].schema();
        let merged = concat_batches(&read_schema, &batches)?;
        let src_i64 = cast(merged.column(0), &DataType::Int64)
            .map_err(|e| DataFusionError::ArrowError(e, None))?;
        let dst_i64 = cast(merged.column(1), &DataType::Int64)
            .map_err(|e| DataFusionError::ArrowError(e, None))?;

        let schema = Arc::new(Schema::new(vec![
            Field::new("src", DataType::Int64, true),
            Field::new("dst", DataType::Int64, true),
        ]));
        RecordBatch::try_new(schema, vec![src_i64, dst_i64])
            .map_err(|e| DataFusionError::ArrowError(e, None))
    })
}

/// Read a node/attribute file into one `RecordBatch`, keeping **every** column.
///
/// Unlike [`scan_edges_batch`], there is no projection: a node table is an
/// attribute table, so all columns are carried through. Only the `id` column is
/// cast to Int64 (the v0.1 node-id type); attribute columns keep their file
/// types. The result feeds `execute_node_query`'s `nodes` slot, where algorithm
/// outputs are LEFT-joined onto it by id — exactly like an in-memory
/// `from_arrow(..., id=...)` table.
pub fn scan_nodes_batch(
    path: &str,
    id: &str,
    storage_options: &HashMap<String, String>,
) -> Result<RecordBatch> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| DataFusionError::Execution(format!("failed to build runtime: {e}")))?;

    runtime.block_on(async move {
        let ctx = SessionContext::new();
        register_object_store(&ctx, path, storage_options)?;
        let lower = path.to_ascii_lowercase();
        let df = if lower.ends_with(".parquet") {
            ctx.read_parquet(path, ParquetReadOptions::default())
                .await?
        } else if lower.ends_with(".csv") {
            ctx.read_csv(path, CsvReadOptions::default()).await?
        } else {
            return Err(DataFusionError::NotImplemented(format!(
                "scan_nodes supports .parquet and .csv in v0.1; got path {path:?}"
            )));
        };

        let batches = df.collect().await?;
        if batches.is_empty() {
            return Err(DataFusionError::Execution(format!(
                "node file {path:?} produced no rows"
            )));
        }

        let read_schema = batches[0].schema();
        let merged = concat_batches(&read_schema, &batches)?;
        let id_idx = read_schema.index_of(id).map_err(|_| {
            DataFusionError::Execution(format!(
                "scan_nodes: id column {id:?} not found in node file {path:?}"
            ))
        })?;

        // Cast only the id column to Int64; attribute columns keep their types.
        let mut columns = merged.columns().to_vec();
        columns[id_idx] = cast(&columns[id_idx], &DataType::Int64)
            .map_err(|e| DataFusionError::ArrowError(e, None))?;
        let fields: Vec<Field> = read_schema
            .fields()
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if i == id_idx {
                    Field::new(f.name(), DataType::Int64, f.is_nullable())
                } else {
                    f.as_ref().clone()
                }
            })
            .collect();
        RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
            .map_err(|e| DataFusionError::ArrowError(e, None))
    })
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
        let batch = scan_edges_batch(path.to_str().unwrap(), "from", "to", &no_opts()).unwrap();
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(batch.num_rows(), 2);
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
        let batch = scan_nodes_batch(path.to_str().unwrap(), "tower_id", &no_opts()).unwrap();
        assert_eq!(batch.num_rows(), 2);
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
        let batch = scan_edges_batch(&url, "from", "to", &no_opts()).unwrap();
        assert_eq!(batch.num_rows(), 2);
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
    fn unknown_storage_option_errors() {
        let ctx = SessionContext::new();
        let bad: HashMap<String, String> = [("not_a_real_option".to_string(), "x".to_string())]
            .into_iter()
            .collect();
        assert!(register_object_store(&ctx, "s3://my-bucket/f.parquet", &bad).is_err());
    }
}
