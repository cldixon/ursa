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

use std::collections::HashMap;
use std::fmt::Write as _;
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

/// A built graph: its topology, and the map from dense node indexes back to
/// the edges' ids.
#[derive(Debug)]
pub struct GraphIndex {
    pub topology: Arc<Topology>,
    pub ids: Arc<IdMap>,
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
        if node
            .expressions()
            .iter()
            .any(|e| e.is_volatile())
        {
            cacheable = false;
            return Ok(TreeNodeRecursion::Stop);
        }
        if let LogicalPlan::TableScan(scan) = node {
            let listing = source_as_provider(&scan.source)
                .ok()
                .and_then(|p| p.downcast_ref::<ListingTable>().map(|t| {
                    let paths: Vec<String> =
                        t.table_paths().iter().map(|p| p.to_string()).collect();
                    format!("{paths:?} {:?}", t.options())
                }));
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

    /// The index for `key` (a fingerprint and file identities), built by
    /// `build` if it is not cached. With no fingerprint, always built.
    pub async fn get_or_build(
        &self,
        key: Option<(String, Vec<FileIdentity>)>,
        build: BoxFuture<'_, Result<GraphIndex>>,
    ) -> Result<Arc<GraphIndex>> {
        let Some((fingerprint, files)) = key else {
            self.builds.fetch_add(1, Ordering::Relaxed);
            return build.await.map(Arc::new);
        };
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
        let builds = &self.builds;
        cell.get_or_try_init(|| async move {
            builds.fetch_add(1, Ordering::Relaxed);
            build.await.map(Arc::new)
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
