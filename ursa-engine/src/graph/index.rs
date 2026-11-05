//! Graph indexes, and the session's cache of them.
//!
//! A graph node builds its index (the CSR topology and the id map) from the
//! rows of its edge plan when it executes. The cache keeps built indexes for
//! the session, keyed by what the edges are:
//!
//! - **the edge plan's fingerprint**: the optimized logical plan under the
//!   node, rendered with its schema, plus, for each file table it scans, the
//!   table's paths and format options (two CSV tables over one file with
//!   different delimiters render alike but read differently);
//! - **the identity of every file the plan reads**: its location, size,
//!   modification time and e-tag, from the object store's metadata when the
//!   node executes (a `HEAD` per file, not DataFusion's listing cache, which
//!   can be stale).
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
//! Evaluated weights live only in memory.

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use datafusion::catalog::default_table_source::source_as_provider;
use datafusion::common::tree_node::{TreeNode, TreeNodeRecursion};
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

/// A built graph: its topology, and the map from dense node indexes back to
/// the edges' ids.
#[derive(Debug)]
pub struct GraphIndex {
    pub topology: Arc<Topology>,
    pub ids: Arc<IdMap>,
    /// Weights evaluated over the edge rows this index was built from.
    weights: WeightCache,
}

impl GraphIndex {
    pub fn new(topology: Arc<Topology>, ids: Arc<IdMap>) -> Self {
        Self {
            topology,
            ids,
            weights: WeightCache::default(),
        }
    }

    /// The weight columns stored under `key`, if they are.
    pub fn weights(&self, key: &str) -> Option<Weights> {
        self.weights.get(key)
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
    let _ = plan.apply_with_subqueries(|node| {
        if node.expressions().iter().any(|e| e.is_volatile()) {
            cacheable = false;
            return Ok(TreeNodeRecursion::Stop);
        }
        if let LogicalPlan::TableScan(scan) = node {
            let listing = source_as_provider(&scan.source).ok().and_then(|p| {
                p.downcast_ref::<ListingTable>().map(|t| {
                    let paths: Vec<String> =
                        t.table_paths().iter().map(|p| p.to_string()).collect();
                    format!("{paths:?} {:?}", t.options())
                })
            });
            match listing {
                Some(listing) => {
                    let _ = write!(rendered, "\n{}: {listing}", scan.table_name);
                }
                None => {
                    cacheable = false;
                    return Ok(TreeNodeRecursion::Stop);
                }
            }
        }
        Ok(TreeNodeRecursion::Continue)
    });
    cacheable.then_some(rendered)
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
    let mut identities = Vec::with_capacity(files.len());
    for (url, location) in files {
        let store = context.runtime_env().object_store(&url)?;
        let meta = store.head(&location).await?;
        identities.push(FileIdentity {
            store: url.to_string(),
            location: location.to_string(),
            size: meta.size,
            modified: meta.last_modified.timestamp_nanos_opt().unwrap_or(i64::MAX),
            e_tag: meta.e_tag,
        });
    }
    Ok(identities)
}

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
        self.get_or_build_in(None, key, build).await
    }

    /// As [`Self::get_or_build`], and on a miss, the index stored under the
    /// same full key in the database of the session `config` belongs to, if
    /// it has one. An index built in a writable database is stored there, in
    /// the background, within the session's `ursa.index_storage_limit`.
    pub async fn get_or_build_stored(
        &self,
        config: &SessionConfig,
        key: Option<(String, Vec<FileIdentity>)>,
        build: BoxFuture<'_, Result<GraphIndex>>,
    ) -> Result<Arc<GraphIndex>> {
        let stored = database_of_config(config).map(|db| {
            (
                db.index_store(),
                crate::options::index_storage_limit(config),
            )
        });
        self.get_or_build_in(stored, key, build).await
    }

    async fn get_or_build_in(
        &self,
        stored: Option<(Arc<IndexStore>, u64)>,
        key: Option<(String, Vec<FileIdentity>)>,
        build: BoxFuture<'_, Result<GraphIndex>>,
    ) -> Result<Arc<GraphIndex>> {
        let Some((fingerprint, files)) = key else {
            self.builds.fetch_add(1, Ordering::Relaxed);
            return build.await.map(Arc::new);
        };
        let stored = stored.map(|(store, limit)| {
            (
                store,
                limit,
                IndexKey::new(fingerprint.clone(), files.clone()),
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
            if let Some((store, _, key)) = &stored {
                let (store, key) = (Arc::clone(store), key.clone());
                // Reading a file and checking it is blocking work.
                let loaded = tokio::task::spawn_blocking(move || store.load(&key))
                    .await
                    .ok()
                    .flatten();
                if let Some(index) = loaded {
                    loads.fetch_add(1, Ordering::Relaxed);
                    return Ok(index);
                }
            }
            builds.fetch_add(1, Ordering::Relaxed);
            let index = Arc::new(build.await?);
            if let Some((store, limit, key)) = stored {
                store.save(key, Arc::clone(&index), limit);
            }
            Ok(index)
        })
        .await
        .cloned()
    }
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
