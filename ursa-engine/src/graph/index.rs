//! Graph indexes, and the session's cache of them.
//!
//! A graph node builds its index (the CSR topology and the id map) from the
//! rows of its edge plan when it executes. The cache keeps built indexes for
//! the session, keyed by what the edges are:
//!
//! - **the edge plan's fingerprint**: the optimized logical plan under the
//!   node, rendered with its schema, plus, for each file table it scans, the
//!   table's paths and the options that change what is read (format, CSV
//!   parsing options, partition columns, declared order: two CSV tables over
//!   one file with different delimiters render alike but read differently).
//!   Writer options, statistics and partitioning settings are left out, and
//!   so is DataFusion's `Debug` text of the options, so an unrelated default
//!   or a DataFusion upgrade does not change it (#327). A file named in
//!   `FROM` is a table named by its path as written; in the plan its name is
//!   replaced by its place among the scans, as its paths say what it reads;
//! - **the identity of every file the plan reads**: its location, size,
//!   modification time and e-tag, from the object store's metadata when the
//!   node executes (a `HEAD` per file, not DataFusion's listing cache, which
//!   can be stale);
//! - for a graph with a vertex table, whose keys are its nodes (#311), **the
//!   keys' plan's fingerprint and the identity of the files it reads** too,
//!   so a change to the vertex table builds afresh.
//!
//! A local file's location is its path (`/a/b.csv`). In a session with a
//! database, the key is **portable**: every path inside the database's
//! directory, in the fingerprint and the file identities, is written relative
//! to it (behind a marker, [`DATABASE_DIR`]), as the catalog stores such paths
//! (#253), so a database directory moved together with its data still finds
//! its stored indexes. Paths outside it stay absolute.
//!
//! Rewriting a file changes its identity, so the next query builds afresh.
//! An edge plan that reads anything other than files, literal rows or nothing
//! (an in-memory table, which `INSERT` can change, or a volatile function such
//! as `random()`) has no stable identity and is never cached.
//!
//! Concurrent requests for one key wait on one build, so several algorithms
//! over one graph in one query share one index even when DataFusion runs
//! their nodes at the same time.
//!
//! In a session with a `.ursa` database, a miss here looks in the database's
//! stored indexes under the same full key before building
//! ([`IndexCache::get_or_build_stored`]), and an index built in a writable
//! session is stored there (`crate::database::index_store`). Loads from disk
//! are counted apart from builds ([`index_loads`], [`index_builds`]).
//! Evaluated weights are kept on the index in memory, and stored with it in
//! the database (`crate::database::index_store`), so a weighted query over a
//! stored index in a later process reads no edge rows.

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use datafusion::arrow::buffer::BooleanBuffer;
use datafusion::catalog::default_table_source::source_as_provider;
use datafusion::common::tree_node::{TreeNode, TreeNodeRecursion};
use datafusion::datasource::file_format::arrow::ArrowFormat;
use datafusion::datasource::file_format::csv::CsvFormat;
use datafusion::datasource::file_format::json::JsonFormat;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::file_format::FileFormat as DataFusionFileFormat;
use datafusion::datasource::listing::ListingTable;
use datafusion::datasource::physical_plan::FileScanConfig;
use datafusion::datasource::source::DataSourceExec;
use datafusion::error::Result;
use datafusion::execution::TaskContext;
use datafusion::logical_expr::LogicalPlan;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::{SessionConfig, SessionContext};
use futures::future::BoxFuture;
use object_store::ObjectStoreExt;
use ursa_core::{IdMap, Topology};

use crate::database::{database_of_config, IndexKey, IndexStore};
use crate::notices::{self, NoticeLevel, Notifier};

/// A built graph: its topology, and the map from dense node indexes back to
/// the edges' ids.
///
/// A graph with a vertex table has the vertex table's keys as its nodes, in
/// the table's order, and only the edges whose endpoints are both keys (see
/// `ursa_core::EdgeInterner`): its edge ids number the edges kept, and
/// [`Self::edge_row`] maps them back to rows of the edge table.
#[derive(Debug)]
pub struct GraphIndex {
    pub topology: Arc<Topology>,
    pub ids: Arc<IdMap>,
    /// Edge rows dropped because an endpoint is not a node.
    n_dropped: u64,
    /// Which edge rows were kept, one bit per row of the edge plan, when any
    /// was dropped.
    kept: Option<BooleanBuffer>,
    /// Per edge id, its row in the edge plan, when any row was dropped.
    rows: OnceLock<Vec<u64>>,
    /// Weights evaluated over the edge rows this index was built from.
    weights: WeightCache,
    /// Weights stored with the index on disk, read on first use.
    stored: Option<StoredWeights>,
}

/// Reads the weight columns stored under a key, one `f64` per edge row each
/// (blocking: it reads a file). `None` if they cannot be read.
pub type WeightReader = Box<dyn Fn(&str) -> Option<Vec<Vec<f64>>> + Send + Sync>;

/// The weights stored with an index on disk: their keys, and how to read them.
struct StoredWeights {
    keys: Vec<String>,
    read: WeightReader,
}

impl fmt::Debug for StoredWeights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StoredWeights({} sets)", self.keys.len())
    }
}

impl GraphIndex {
    pub fn new(topology: Arc<Topology>, ids: Arc<IdMap>) -> Self {
        Self {
            topology,
            ids,
            n_dropped: 0,
            kept: None,
            rows: OnceLock::new(),
            weights: WeightCache::default(),
            stored: None,
        }
    }

    /// This index, built from edge rows of which `kept` says which were kept
    /// (`None`: all of them), `n_dropped` having been dropped.
    pub fn with_dropped(mut self, n_dropped: u64, kept: Option<BooleanBuffer>) -> Self {
        self.n_dropped = n_dropped;
        self.kept = kept.filter(|_| n_dropped > 0);
        self
    }

    /// Edge rows dropped because an endpoint is not a node of the graph's
    /// vertex table.
    pub fn n_dropped(&self) -> u64 {
        self.n_dropped
    }

    /// Which rows of the edge plan were kept, one bit per row, if any was
    /// dropped.
    pub fn kept_rows(&self) -> Option<&BooleanBuffer> {
        self.kept.as_ref()
    }

    /// The row of the edge plan edge `edge` was read from: the edge id itself
    /// when no row was dropped.
    pub fn edge_row(&self, edge: u32) -> u64 {
        match &self.kept {
            None => u64::from(edge),
            Some(kept) => self
                .rows
                .get_or_init(|| kept.set_indices().map(|i| i as u64).collect())[edge as usize],
        }
    }

    /// This index, with the weight sets stored on disk under `keys`, which
    /// `read` reads when one is first asked for.
    pub fn with_stored_weights(mut self, keys: Vec<String>, read: WeightReader) -> Self {
        if !keys.is_empty() {
            self.stored = Some(StoredWeights { keys, read });
        }
        self
    }

    /// The weight columns stored under `key` in memory, if they are.
    pub fn weights(&self, key: &str) -> Option<Weights> {
        self.weights.get(key)
    }

    /// Whether weights are stored under `key` on disk.
    pub fn has_stored_weights(&self, key: &str) -> bool {
        self.stored
            .as_ref()
            .is_some_and(|s| s.keys.iter().any(|k| k == key))
    }

    /// The weight columns stored under `key`, in memory or else on disk, if
    /// they are and have `n_columns` columns of one value per edge row. Read
    /// from disk, they are kept in memory too. Blocking: it may read a file.
    pub fn stored_weights(&self, key: &str, n_columns: usize) -> Option<Weights> {
        if let Some(weights) = self.weights(key) {
            return Some(weights);
        }
        if !self.has_stored_weights(key) {
            return None;
        }
        let columns = (self.stored.as_ref()?.read)(key)?;
        let n_edges = self.topology.n_edges();
        if columns.len() != n_columns || columns.iter().any(|c| c.len() != n_edges) {
            return None;
        }
        let weights: Weights = Arc::new(columns.into_iter().map(Arc::new).collect());
        self.store_weights(key.to_string(), Arc::clone(&weights));
        Some(weights)
    }

    /// Every set of weight columns held in memory, with its key.
    pub fn weight_sets(&self) -> Vec<(String, Weights)> {
        self.weights.all()
    }

    /// Store weight columns under `key`. They must be one per edge row of this
    /// index, in its edge-row order.
    pub fn store_weights(&self, key: String, weights: Weights) {
        self.weights.put(key, weights)
    }
}

/// Weight columns, one `f64` per edge row each, in the order of a node's
/// weight expressions.
pub type Weights = Arc<Vec<Arc<Vec<f64>>>>;

/// How many sets of weight columns an index keeps. Each column is one `f64`
/// per edge (80 MB at 10M edges), so the bound is small, as in ursa-plan's
/// `WeightCache`: enough for a few weightings alternating over one graph.
const WEIGHT_CAPACITY: usize = 4;

/// Evaluated weights, held on the index so they live exactly as long as the
/// edge rows they were evaluated over. Keyed by the weighted edge plan's
/// fingerprint and file identities (see `exec`), most recently used last.
#[derive(Default)]
struct WeightCache {
    entries: Mutex<Vec<(String, Weights)>>,
}

impl fmt::Debug for WeightCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        write!(f, "WeightCache({} entries)", entries.len())
    }
}

impl WeightCache {
    fn get(&self, key: &str) -> Option<Weights> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let i = entries.iter().position(|(k, _)| k == key)?;
        let entry = entries.remove(i);
        let weights = Arc::clone(&entry.1);
        entries.push(entry);
        Some(weights)
    }

    fn all(&self) -> Vec<(String, Weights)> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .iter()
            .map(|(k, w)| (k.clone(), Arc::clone(w)))
            .collect()
    }

    fn put(&self, key: String, weights: Weights) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|(k, _)| *k != key);
        if entries.len() >= WEIGHT_CAPACITY {
            entries.remove(0);
        }
        entries.push((key, weights));
    }
}

/// The identity of one source file, as its store reports it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileIdentity {
    pub store: String,
    pub location: String,
    pub size: u64,
    /// Modification time, in nanoseconds since the epoch.
    pub modified: i64,
    pub e_tag: Option<String>,
}

/// The fingerprint of an edge plan, or `None` if what it reads has no stable
/// identity (see the module docs).
pub fn fingerprint(plan: &LogicalPlan) -> Option<String> {
    let mut rendered = plan.display_indent_schema().to_string();
    let mut cacheable = true;
    let mut locations: Vec<String> = Vec::new();
    let mut scans: Vec<(String, String)> = Vec::new();
    let _ = plan.apply_with_subqueries(|node| {
        if node.expressions().iter().any(|e| e.is_volatile()) {
            cacheable = false;
            return Ok(TreeNodeRecursion::Stop);
        }
        if let LogicalPlan::TableScan(scan) = node {
            let listing = source_as_provider(&scan.source)
                .ok()
                .and_then(|p| p.downcast_ref::<ListingTable>().map(listing_key));
            match listing {
                Some(listing) => {
                    let name = scan.table_name.to_string();
                    scans.push((name.clone(), listing));
                    if crate::paths::looks_like_location(&name) && !locations.contains(&name) {
                        locations.push(name);
                    }
                }
                None => {
                    cacheable = false;
                    return Ok(TreeNodeRecursion::Stop);
                }
            }
        }
        Ok(TreeNodeRecursion::Continue)
    });
    if !cacheable {
        return None;
    }
    // A file named in `FROM` is a table named by its path as written:
    // relative to the current directory in the statement that defined it,
    // absolute when a database replays it. What it reads is its paths, listed
    // after the plan, so in the plan its name is replaced by its place among
    // the scans (longest name first, as a name may be part of another).
    let placeholder = |name: &str| {
        locations
            .iter()
            .position(|l| l == name)
            .map(|i| format!("\u{0}file {i}\u{0}"))
    };
    let mut by_length: Vec<&String> = locations.iter().collect();
    by_length.sort_by_key(|name| std::cmp::Reverse(name.len()));
    for name in by_length {
        if let Some(placeholder) = placeholder(name) {
            rendered = rendered.replace(name.as_str(), &placeholder);
        }
    }
    for (name, listing) in &scans {
        let name = placeholder(name).unwrap_or_else(|| name.clone());
        let _ = write!(rendered, "\n{name}: {listing}");
    }
    Some(rendered)
}

/// What a file table reads, for the fingerprint: its paths, and only the
/// options that change which rows and values are read (the format and its
/// parsing options, partition columns, declared order). Writer options,
/// statistics and partitioning settings, and DataFusion's `Debug` text of
/// them, are left out, so an unrelated default or a DataFusion upgrade does
/// not change the key (#327).
fn listing_key(table: &ListingTable) -> String {
    let paths: Vec<String> = table.table_paths().iter().map(|p| p.to_string()).collect();
    let options = table.options();
    let partitions: Vec<String> = options
        .table_partition_cols
        .iter()
        .map(|(name, data_type)| format!("{name} {data_type}"))
        .collect();
    let order: Vec<String> = options
        .file_sort_order
        .iter()
        .map(|exprs| {
            exprs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .collect();
    format!(
        "paths {paths:?}; extension {:?}; {}; partitions {partitions:?}; order {order:?}",
        options.file_extension,
        format_key(options.format.as_ref())
    )
}

/// A file format and the options of it that change what is read.
fn format_key(format: &dyn DataFusionFileFormat) -> String {
    if let Some(csv) = format.downcast_ref::<CsvFormat>() {
        let o = csv.options();
        return format!(
            "csv (header {:?}, delimiter {}, quote {}, terminator {:?}, escape {:?}, \
             double_quote {:?}, comment {:?}, null_regex {:?}, null_value {:?}, \
             truncated_rows {:?}, compression {:?}, formats {:?})",
            o.has_header,
            o.delimiter,
            o.quote,
            o.terminator,
            o.escape,
            o.double_quote,
            o.comment,
            o.null_regex,
            o.null_value,
            o.truncated_rows,
            o.compression,
            [
                &o.date_format,
                &o.datetime_format,
                &o.timestamp_format,
                &o.timestamp_tz_format,
                &o.time_format
            ],
        );
    }
    if let Some(json) = format.downcast_ref::<JsonFormat>() {
        let o = json.options();
        return format!(
            "json (newline_delimited {}, compression {:?})",
            o.newline_delimited, o.compression
        );
    }
    if let Some(parquet) = format.downcast_ref::<ParquetFormat>() {
        let o = &parquet.options().global;
        return format!(
            "parquet (binary_as_string {}, schema_force_view_types {}, coerce_int96 {:?} {:?})",
            o.binary_as_string, o.schema_force_view_types, o.coerce_int96, o.coerce_int96_tz
        );
    }
    if format.downcast_ref::<ArrowFormat>().is_some() {
        return "arrow".to_string();
    }
    // A format Ursa does not know: all of it, to be safe.
    format!("{} {format:?}", format.get_ext())
}

/// What a path inside the database's directory becomes in a stored key: the
/// directory is replaced by this marker, so a database moved together with
/// its data keeps its keys (#327). The NUL characters keep it from meeting
/// anything a path or a literal in a plan would hold.
pub(crate) const DATABASE_DIR: &str = "\u{0}database\u{0}";

/// The forms the absolute directory `dir` takes in a plan's text and in
/// object store locations, longest first: `file:///a/b/`, `/a/b/`. Its
/// canonical form too, if it differs (a symlinked temporary directory).
/// Empty for the root directory, whose every path would match.
fn dir_forms(dir: &Path) -> Vec<String> {
    let mut forms = Vec::new();
    let canonical = std::fs::canonicalize(dir).ok();
    for dir in std::iter::once(dir).chain(canonical.as_deref()) {
        let text = dir.to_string_lossy().replace('\\', "/");
        let text = text.trim_end_matches('/');
        if text.is_empty() {
            continue;
        }
        let url = match text.strip_prefix('/') {
            Some(rest) => format!("file:///{rest}/"),
            None => format!("file:///{text}/"),
        };
        for form in [url, format!("{text}/")] {
            if !forms.contains(&form) {
                forms.push(form);
            }
        }
    }
    forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
    forms
}

/// `text` (a fingerprint or a location) with every path inside `dir` made
/// relative to it, behind [`DATABASE_DIR`].
pub(crate) fn portable(text: &str, dir: &Path) -> String {
    let mut text = text.to_string();
    for form in dir_forms(dir) {
        text = text.replace(&form, &format!("{DATABASE_DIR}/"));
    }
    text
}

/// [`portable`] against the directory of the database of the session
/// `config` belongs to; `text` as it is without one.
pub(crate) fn portable_in(text: &str, config: &SessionConfig) -> String {
    match database_of_config(config) {
        Some(db) => portable(text, db.dir()),
        None => text.to_string(),
    }
}

/// A local file's path from its object store location, as the user wrote
/// it: `/a/b.csv`, not `a/b.csv` (#327, N14).
fn local_path(location: &str) -> String {
    if cfg!(windows) || location.starts_with('/') {
        location.to_string()
    } else {
        format!("/{location}")
    }
}

/// The identity of every file `plan` reads, sorted, looked up in the object
/// store now.
pub async fn file_identities(
    plan: &Arc<dyn ExecutionPlan>,
    context: &TaskContext,
) -> Result<Vec<FileIdentity>> {
    let mut files = Vec::new();
    let _ = plan.apply(|node| {
        if let Some(config) = node
            .downcast_ref::<DataSourceExec>()
            .and_then(|exec| exec.data_source().downcast_ref::<FileScanConfig>())
        {
            for group in &config.file_groups {
                for file in group.files() {
                    files.push((
                        config.object_store_url.clone(),
                        file.object_meta.location.clone(),
                    ));
                }
            }
        }
        Ok(TreeNodeRecursion::Continue)
    });
    files.sort();
    files.dedup();
    let database = database_of_config(context.session_config());
    let mut identities = Vec::with_capacity(files.len());
    for (url, location) in files {
        let store = context.runtime_env().object_store(&url)?;
        let meta = store.head(&location).await?;
        let store = url.to_string();
        // A local file by its path, inside a database's directory relative
        // to it, so the key survives moving the two together.
        let location = if store == LOCAL_STORE {
            let path = local_path(location.as_ref());
            match &database {
                Some(db) => portable(&path, db.dir()),
                None => path,
            }
        } else {
            location.to_string()
        };
        identities.push(FileIdentity {
            store,
            location,
            size: meta.size,
            modified: meta.last_modified.timestamp_nanos_opt().unwrap_or(i64::MAX),
            e_tag: meta.e_tag,
        });
    }
    identities.sort();
    Ok(identities)
}

/// The object store URL of the local file system.
const LOCAL_STORE: &str = "file:///";

/// How many fingerprints the cache keeps. A newer identity of a fingerprint
/// replaces the older one, so this bounds the number of distinct edge plans.
const CAPACITY: usize = 16;

/// The session's graph indexes (a session config extension).
#[derive(Debug, Default)]
pub struct IndexCache {
    slots: Mutex<HashMap<String, Slot>>,
    /// Bumped on every use, to find the least recently used slot.
    clock: AtomicU64,
    builds: AtomicU64,
    loads: AtomicU64,
}

#[derive(Debug)]
struct Slot {
    files: Vec<FileIdentity>,
    cell: Arc<tokio::sync::OnceCell<Arc<GraphIndex>>>,
    used: u64,
}

impl IndexCache {
    /// How many indexes this cache has built.
    pub fn builds(&self) -> u64 {
        self.builds.load(Ordering::Relaxed)
    }

    /// How many indexes this cache has loaded from the database's stored
    /// indexes instead of building them.
    pub fn loads(&self) -> u64 {
        self.loads.load(Ordering::Relaxed)
    }

    /// Count a build made outside the cache (an index that cannot be shared).
    pub fn record_build(&self) {
        self.builds.fetch_add(1, Ordering::Relaxed);
    }

    /// The index for `key`, if it is built and cached; nothing is built.
    pub fn get(&self, key: &(String, Vec<FileIdentity>)) -> Option<Arc<GraphIndex>> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let slot = slots.get_mut(&key.0).filter(|s| s.files == key.1)?;
        let index = slot.cell.get().cloned()?;
        slot.used = self.clock.fetch_add(1, Ordering::Relaxed);
        Some(index)
    }

    /// The index for `key` (a fingerprint and file identities), built by
    /// `build` if it is not cached. With no fingerprint, always built. The
    /// database's stored indexes are not consulted: see
    /// [`Self::get_or_build_stored`].
    pub async fn get_or_build(
        &self,
        key: Option<(String, Vec<FileIdentity>)>,
        build: BoxFuture<'_, Result<GraphIndex>>,
    ) -> Result<Arc<GraphIndex>> {
        self.get_or_build_in(None, &Notifier::silent(), &BuildInfo::default(), key, build)
            .await
    }

    /// As [`Self::get_or_build`], and on a miss, the index stored under the
    /// same full key in the database of the session `config` belongs to, if
    /// it has one. An index built in a writable database is stored there, in
    /// the background, within the database's `ursa.index_storage_limit`.
    pub async fn get_or_build_stored(
        &self,
        config: &SessionConfig,
        key: Option<(String, Vec<FileIdentity>)>,
        build: BoxFuture<'_, Result<GraphIndex>>,
    ) -> Result<Arc<GraphIndex>> {
        self.get_or_build_for(config, key, &BuildInfo::default(), build)
            .await
    }

    /// As [`Self::get_or_build_stored`], with notices about the build
    /// (`crate::notices`) naming the graph in `info`.
    pub async fn get_or_build_for(
        &self,
        config: &SessionConfig,
        key: Option<(String, Vec<FileIdentity>)>,
        info: &BuildInfo,
        build: BoxFuture<'_, Result<GraphIndex>>,
    ) -> Result<Arc<GraphIndex>> {
        let stored =
            database_of_config(config).map(|db| (db.index_store(), db.dir().to_path_buf()));
        let notifier = Notifier::of(config);
        self.get_or_build_in(stored, &notifier, info, key, build)
            .await
    }

    async fn get_or_build_in(
        &self,
        stored: Option<(Arc<IndexStore>, PathBuf)>,
        notifier: &Notifier,
        info: &BuildInfo,
        key: Option<(String, Vec<FileIdentity>)>,
        build: BoxFuture<'_, Result<GraphIndex>>,
    ) -> Result<Arc<GraphIndex>> {
        let Some((fingerprint, files)) = key else {
            self.builds.fetch_add(1, Ordering::Relaxed);
            return build_noticed(notifier, info, build, Kept::Never).await;
        };
        // The stored key is portable: paths inside the database's directory
        // relative to it (the file identities already are).
        let stored = stored.map(|(store, dir)| {
            (
                store,
                IndexKey::new(portable(&fingerprint, &dir), files.clone()),
            )
        });
        let cell = {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            let used = self.clock.fetch_add(1, Ordering::Relaxed);
            match slots.get_mut(&fingerprint) {
                Some(slot) if slot.files == files => {
                    slot.used = used;
                    Arc::clone(&slot.cell)
                }
                _ => {
                    if !slots.contains_key(&fingerprint) && slots.len() >= CAPACITY {
                        if let Some(oldest) = slots
                            .iter()
                            .min_by_key(|(_, s)| s.used)
                            .map(|(k, _)| k.clone())
                        {
                            slots.remove(&oldest);
                        }
                    }
                    let cell = Arc::new(tokio::sync::OnceCell::new());
                    slots.insert(
                        fingerprint,
                        Slot {
                            files,
                            cell: Arc::clone(&cell),
                            used,
                        },
                    );
                    cell
                }
            }
        };
        // A failed build leaves the cell empty, so the next query tries again.
        let (builds, loads) = (&self.builds, &self.loads);
        cell.get_or_try_init(|| async move {
            if let Some((store, key)) = &stored {
                let (store, key) = (Arc::clone(store), key.clone());
                let started = clock(notifier, NoticeLevel::All);
                let reader = notifier.clone();
                // Reading a file and checking it is blocking work.
                let loaded = crate::run_blocking(move || {
                    let loaded = store.load(&key, &reader);
                    loaded.map(|(index, size)| (index, size, store))
                })
                .await
                .ok()
                .flatten();
                if let Some((index, size, store)) = loaded {
                    loads.fetch_add(1, Ordering::Relaxed);
                    notifier.say(NoticeLevel::All, || {
                        format!(
                            "loaded the index for {} ({} edges) from {} ({}){}",
                            info.graph(),
                            notices::count(index.topology.n_edges() as u64),
                            store.database(),
                            notices::size(size),
                            took(started)
                        )
                    });
                    return Ok(index);
                }
            }
            builds.fetch_add(1, Ordering::Relaxed);
            let kept = match &stored {
                Some((store, key)) => Kept::Stored(Arc::clone(store), key.clone()),
                None => Kept::InMemory,
            };
            build_noticed(notifier, info, build, kept).await
        })
        .await
        .cloned()
    }
}

/// What a build is for, as notices describe it.
#[derive(Debug, Clone, Default)]
pub struct BuildInfo {
    /// The graph, as the query names it.
    pub graph: Option<String>,
    /// About how many edge rows the build reads, and whether that is exact,
    /// if the plan's statistics say.
    pub edges: Option<(u64, bool)>,
}

impl BuildInfo {
    fn graph(&self) -> &str {
        self.graph.as_deref().unwrap_or("a graph")
    }
}

/// Where a built index is kept, for its notice.
enum Kept {
    /// In the session's cache, and the database's stored indexes under this
    /// key.
    Stored(Arc<IndexStore>, IndexKey),
    /// In the session's cache only: the session has no database.
    InMemory,
    /// Nowhere: what its edges read has no stable identity.
    Never,
}

/// Now, if notices at `level` are said (the browser has no clock to read
/// otherwise).
fn clock(notifier: &Notifier, level: NoticeLevel) -> Option<Instant> {
    (notifier.level() >= level).then(Instant::now)
}

/// ` in 30.7 s`, since `started`, if it was read.
fn took(started: Option<Instant>) -> String {
    started
        .map(|s| format!(" in {}", notices::duration(s.elapsed())))
        .unwrap_or_default()
}

/// Run `build`, announcing it once it has run for a second (at once at
/// `ursa.notices = all`), then keep the index as `kept` says (storing it in
/// the database), and say what became of it: always at `all`, and at `on` if
/// the build was announced or the index could not be stored (see
/// `crate::notices`).
async fn build_noticed(
    notifier: &Notifier,
    info: &BuildInfo,
    build: BoxFuture<'_, Result<GraphIndex>>,
    kept: Kept,
) -> Result<Arc<GraphIndex>> {
    let started = clock(notifier, NoticeLevel::On);
    let estimate = match info.edges {
        Some((n, true)) if n > 0 => format!(" ({} edges)", notices::count(n)),
        Some((n, false)) if n > 0 => format!(" (about {} edges)", notices::count(n)),
        _ => String::new(),
    };
    let delay = match notifier.level() {
        NoticeLevel::All => Duration::ZERO,
        _ => notices::BUILD_NOTICE_AFTER,
    };
    let announcement = notifier.announce_after(
        delay,
        format!("building the index for {}{estimate}…", info.graph()),
    );
    let built = build.await;
    let announced = announcement.finish();
    let index = Arc::new(built?);
    let (outcome, loses_work) = match kept {
        Kept::Stored(store, key) => {
            let saved = store.save(key, Arc::clone(&index), info.graph(), notifier);
            (
                format!("; {}", saved.describe(store.database())),
                saved.loses_work(),
            )
        }
        Kept::InMemory => (
            "; kept for this session only (open a .ursa database to store it)".to_string(),
            false,
        ),
        Kept::Never => (
            "; not kept: what its edges read can change (an in-memory table, or a volatile \
             function such as random())"
                .to_string(),
            false,
        ),
    };
    let level = if announced || loses_work {
        NoticeLevel::On
    } else {
        NoticeLevel::All
    };
    notifier.say(level, || {
        format!(
            "built the index for {} ({} edges){}{outcome}",
            info.graph(),
            notices::count(index.topology.n_edges() as u64),
            took(started)
        )
    });
    Ok(index)
}

/// Install an empty index cache on `config`.
pub(crate) fn with_index_cache(config: SessionConfig) -> SessionConfig {
    config.with_extension(Arc::new(IndexCache::default()))
}

/// How many graph indexes `ctx` has built: for tests of the cache.
pub fn index_builds(ctx: &SessionContext) -> u64 {
    ctx.state()
        .config()
        .get_extension::<IndexCache>()
        .map(|c| c.builds())
        .unwrap_or(0)
}

/// How many graph indexes `ctx` has loaded from its database's stored
/// indexes instead of building them: for tests of stored indexes.
pub fn index_loads(ctx: &SessionContext) -> u64 {
    ctx.state()
        .config()
        .get_extension::<IndexCache>()
        .map(|c| c.loads())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_inside_the_database_directory_are_portable() {
        let dir = Path::new("/home/me/proj");
        let text =
            "TableScan: /home/me/proj/edges.csv\npaths [\"file:///home/me/proj/edges.csv\"] \
                    /home/me/project2/x.csv /home/me/proj";
        assert_eq!(
            portable(text, dir),
            format!(
                "TableScan: {DATABASE_DIR}/edges.csv\npaths [\"{DATABASE_DIR}/edges.csv\"] \
                 /home/me/project2/x.csv /home/me/proj"
            )
        );
        // The root directory: nothing is replaced.
        assert_eq!(portable("/a/b.csv", Path::new("/")), "/a/b.csv");
        // Moved, the same file has the same portable form.
        assert_eq!(
            portable("/home/me/proj/edges.csv", dir),
            portable("/mnt/elsewhere/edges.csv", Path::new("/mnt/elsewhere/"))
        );
    }

    #[test]
    fn local_locations_keep_their_leading_slash() {
        #[cfg(not(windows))]
        assert_eq!(local_path("home/me/a.csv"), "/home/me/a.csv");
        assert_eq!(local_path("/home/me/a.csv"), "/home/me/a.csv");
    }
}
