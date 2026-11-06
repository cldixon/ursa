//! Object stores and the DataFusion session a scan runs on.
//!
//! A path may be local, object storage (`s3://`, `gs://`, `az://`, and `file://`),
//! or a plain `http(s)://` URL (a single hosted file). For a remote scheme the
//! matching `object_store` backend is registered on the session before the read,
//! seeded with the caller's `storage_options` layered over the backend's default
//! credential chain (`from_env`).
//!
//! The session a scan runs on is built once per object-store identity and cloned
//! per scan (see [`scan_state`]), so repeated scans reuse the client (its
//! connection pool and credentials) and, for local files, DataFusion's Parquet
//! footer and statistics caches.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use datafusion::error::{DataFusionError, Result};
use datafusion::execution::cache::cache_manager::CacheManagerConfig;
use datafusion::execution::context::SessionState;
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::SessionConfig;
#[cfg(not(target_arch = "wasm32"))]
use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey};
#[cfg(not(target_arch = "wasm32"))]
use object_store::azure::{AzureConfigKey, MicrosoftAzureBuilder};
#[cfg(not(target_arch = "wasm32"))]
use object_store::gcp::{GoogleCloudStorageBuilder, GoogleConfigKey};
use object_store::http::HttpBuilder;
#[cfg(not(target_arch = "wasm32"))]
use object_store::local::LocalFileSystem;
use object_store::ObjectStore;
use url::{Position, Url};

use crate::scan::{detect_format, ScanFormat, SCAN_BATCH_ROWS};

/// The object store for a scan `path`, with the `scheme://authority` base it is
/// registered under. A schemeless (bare local) path parses as an error and needs
/// no store — the default registry already serves the local filesystem. Remote
/// backends are seeded from `opts` layered over their default credential chain
/// (`from_env`), so an empty `opts` still works with env/instance-profile
/// credentials.
pub(crate) fn object_store_for(
    path: &str,
    opts: &HashMap<String, String>,
) -> Result<Option<(Url, Arc<dyn ObjectStore>)>> {
    let url = match Url::parse(path) {
        Ok(u) => u,
        Err(_) => return Ok(None), // bare local path (e.g. "/tmp/x.csv" or "edges.csv")
    };
    let scheme = url.scheme();
    #[cfg(target_arch = "wasm32")]
    let _ = opts; // only the cloud backends, which wasm32 leaves out, read them
    let opt_err = |e: object_store::Error| DataFusionError::Execution(e.to_string());
    let store: Arc<dyn ObjectStore> = match scheme {
        #[cfg(not(target_arch = "wasm32"))]
        "file" => Arc::new(LocalFileSystem::new()),
        #[cfg(not(target_arch = "wasm32"))]
        "s3" | "s3a" => {
            let mut b = AmazonS3Builder::from_env().with_url(path);
            for (k, v) in opts {
                b = b.with_config(k.parse::<AmazonS3ConfigKey>().map_err(opt_err)?, v.clone());
            }
            Arc::new(b.build().map_err(opt_err)?)
        }
        #[cfg(not(target_arch = "wasm32"))]
        "gs" => {
            let mut b = GoogleCloudStorageBuilder::from_env().with_url(path);
            for (k, v) in opts {
                b = b.with_config(k.parse::<GoogleConfigKey>().map_err(opt_err)?, v.clone());
            }
            Arc::new(b.build().map_err(opt_err)?)
        }
        #[cfg(not(target_arch = "wasm32"))]
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
        #[cfg(target_arch = "wasm32")]
        other @ ("file" | "s3" | "s3a" | "gs" | "az" | "azure" | "abfs" | "abfss" | "adl") => {
            return Err(DataFusionError::NotImplemented(format!(
                "'{other}://' URLs are not available in the browser build; use an http:// or \
                 https:// URL"
            )))
        }
        other => {
            return Err(DataFusionError::NotImplemented(format!(
                "unsupported URL scheme '{other}'; use a local path or a file://, s3://, gs://, \
                 az://, http:// or https:// URL"
            )))
        }
    };
    // DataFusion routes by scheme + authority. Include the port (`Position::
    // BeforePath` yields `scheme://host[:port]`) so an `http://host:PORT/...` URL
    // registers under the exact authority the read will look up — cloud buckets
    // have no port, so this is identical to the old `scheme://host` for them.
    let base = Url::parse(&url[..Position::BeforePath])
        .map_err(|e| DataFusionError::Execution(format!("not a valid URL: {e}")))?;
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
pub(crate) struct StoreKey {
    base: Option<String>,
    options: Vec<(String, String)>,
    env: Vec<(String, String)>,
}

impl StoreKey {
    pub(crate) fn new(path: &str, opts: &HashMap<String, String>) -> Self {
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
pub(crate) fn scan_state(path: &str, opts: &HashMap<String, String>) -> Result<SessionState> {
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

/// The identity of a local scan's input, for a cache key: each file it reads
/// (a glob expanded as the scan expands it), with its size and modification
/// time in nanoseconds, in path order. `None` for a remote path, whose
/// modification time is too coarse to tell a same-size rewrite apart (see
/// [`build_scan_state`]), or a path the scan can't list (it raises its own
/// error when it runs).
///
/// A list of paths is identified by each path's identity, in path order.
pub fn scan_identity(paths: &[String], opts: &HashMap<String, String>) -> Option<String> {
    let ids = paths
        .iter()
        .map(|p| path_identity(p, opts))
        .collect::<Option<Vec<_>>>()?;
    Some(ids.join(";"))
}

/// The identity of one path (see [`scan_identity`]).
fn path_identity(path: &str, opts: &HashMap<String, String>) -> Option<String> {
    use datafusion::datasource::listing::ListingTableUrl;
    use futures::TryStreamExt;
    if Url::parse(path).is_ok_and(|u| u.scheme() != "file") {
        return None;
    }
    let ext = match detect_format(path).ok()? {
        ScanFormat::Parquet => ".parquet",
        ScanFormat::Csv => ".csv",
    };
    let state = scan_state(path, opts).ok()?;
    let url = ListingTableUrl::parse(path).ok()?;
    let store = state.runtime_env().object_store(url.object_store()).ok()?;
    let mut files = crate::runtime::block_on(async {
        url.list_all_files(&state, store.as_ref(), ext)
            .await?
            .try_collect::<Vec<_>>()
            .await
    })
    .ok()?
    .ok()?;
    if files.is_empty() {
        return None;
    }
    files.sort_by(|a, b| a.location.cmp(&b.location));
    Some(
        files
            .iter()
            .map(|f| {
                let t = f.last_modified.timestamp_nanos_opt().unwrap_or_default();
                format!("{}:{}:{t}", f.location, f.size)
            })
            .collect::<Vec<_>>()
            .join(","),
    )
}
