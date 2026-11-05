//! Object stores, registered the first time a query names a URL.
//!
//! DataFusion only knows the local filesystem until a store is registered for a
//! URL's `scheme://authority`. As datafusion-cli does, the engine registers one
//! lazily: when a query first reads (or `COPY`s to) `s3://bucket/…`, a store for
//! `s3://bucket` is built from the standard environment and kept for the
//! session.
//!
//! Credentials come from each backend's environment variables and default
//! credential chain (`object_store`'s `from_env`): `AWS_*` (plus web identity,
//! container and instance credentials), `GOOGLE_*` (plus application default
//! credentials), and `AZURE_*`. A public S3 bucket can be read with
//! `AWS_SKIP_SIGNATURE=true`.

use std::sync::Arc;

use datafusion::error::{DataFusionError, Result};
use datafusion::execution::runtime_env::RuntimeEnv;
use object_store::aws::{resolve_bucket_region, AmazonS3Builder};
use object_store::azure::MicrosoftAzureBuilder;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::http::HttpBuilder;
use object_store::{ClientOptions, ObjectStore};
use url::{Position, Url};

/// Make sure `runtime` has an object store for `url`, building and registering
/// one if this is the first time the session sees its `scheme://authority`.
/// Local paths need none.
pub async fn ensure_object_store(runtime: &RuntimeEnv, url: &Url) -> Result<()> {
    if url.scheme() == "file" || runtime.object_store_registry.get_store(url).is_ok() {
        return Ok(());
    }
    let store = build_object_store(url).await?;
    runtime.register_object_store(&store_base(url)?, store);
    Ok(())
}

/// [`ensure_object_store`] for a location written in SQL (a `COPY … TO` target
/// or an external table's `LOCATION`). A local path needs no store.
pub async fn ensure_object_store_for(runtime: &RuntimeEnv, location: &str) -> Result<()> {
    match Url::parse(location) {
        // A one-letter scheme is a Windows drive letter.
        Ok(url) if url.scheme().len() > 1 => ensure_object_store(runtime, &url).await,
        _ => Ok(()),
    }
}

/// The `scheme://authority[:port]` a store for `url` is registered under.
fn store_base(url: &Url) -> Result<Url> {
    Url::parse(&url[..Position::BeforePath])
        .map_err(|e| DataFusionError::Plan(format!("'{url}' is not a valid URL: {e}")))
}

/// A store for `url`'s scheme and bucket, configured from the environment.
pub async fn build_object_store(url: &Url) -> Result<Arc<dyn ObjectStore>> {
    let base = &url[..Position::BeforePath];
    let failed = |e: object_store::Error| {
        DataFusionError::Execution(format!("cannot connect to {base}: {e}"))
    };
    let store: Arc<dyn ObjectStore> = match url.scheme() {
        "s3" | "s3a" => {
            let mut builder = AmazonS3Builder::from_env().with_url(url.as_str());
            if !aws_region_configured() {
                // Without a region the client assumes us-east-1, and any other
                // bucket answers with a redirect. Ask S3 where the bucket is.
                if let Some(bucket) = url.host_str() {
                    if let Ok(region) = resolve_bucket_region(bucket, &ClientOptions::new()).await {
                        builder = builder.with_region(region);
                    }
                }
            }
            Arc::new(builder.build().map_err(failed)?)
        }
        "gs" | "gcs" => Arc::new(
            GoogleCloudStorageBuilder::from_env()
                .with_url(url.as_str())
                .build()
                .map_err(failed)?,
        ),
        "az" | "azure" | "abfs" | "abfss" | "adl" => Arc::new(
            MicrosoftAzureBuilder::from_env()
                .with_url(url.as_str())
                .build()
                .map_err(failed)?,
        ),
        // A plain HTTP(S) URL reads single files with ranged GETs. `http://` is
        // allowed because the user chose the scheme.
        "http" | "https" => Arc::new(
            HttpBuilder::new()
                .with_url(base)
                .with_client_options(ClientOptions::new().with_allow_http(true))
                .build()
                .map_err(failed)?,
        ),
        other => {
            return Err(DataFusionError::Plan(format!(
                "Ursa cannot read '{other}://' URLs; use a local path or an s3://, gs://, az://, \
                 http:// or https:// URL"
            )))
        }
    };
    Ok(store)
}

/// Whether the environment names an S3 region or endpoint itself.
fn aws_region_configured() -> bool {
    [
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "AWS_ENDPOINT",
        "AWS_ENDPOINT_URL",
    ]
    .iter()
    .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}
