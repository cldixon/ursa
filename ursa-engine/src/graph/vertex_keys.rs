//! A vertex table's key identifies one row (#411).
//!
//! SQL/PGQ requires a vertex table's key to be unique and not null, and so
//! does Ursa: a vertex table with a duplicate or a NULL key is refused, with
//! one message ([`key_error`]), by whatever reads it as the nodes of a graph:
//!
//! - **a graph index** counts the rows that are no node of their own while it
//!   numbers the keys (`ursa_core::EdgeInterner::key_rows`), so the
//!   algorithms, the traversals and the searches of `GRAPH_TABLE` refuse the
//!   table when they build it (see `exec`);
//! - **`<graph>.nodes` and a node of a `GRAPH_TABLE` pattern** read the
//!   vertex table's rows through [`unique_keys`], a table whose scan is its
//!   inner table's, once the keys are known to be one row each.
//!
//! Before this rule the algorithms counted a duplicated key once and skipped
//! a NULL key, while `<graph>.nodes` and `GRAPH_TABLE` returned every row, so
//! the three saw different node sets.
//!
//! ## When the keys are counted
//!
//! Counting the keys reads the whole key column, which a point query must
//! not pay each time (a `.csv.gz` vertex table decompresses on one core). So
//! a table that reads only files is counted once per version of its files.
//! Its keys are known to be one row each, with nothing read but the files'
//! metadata, when under the fingerprint of its keys' plan and the identity of
//! the files it reads (as a graph index is keyed, see `index`):
//!
//! 1. this session counted them ([`KeyChecks`]); or
//! 2. the session's database holds a stored index built from those keys: a
//!    build refuses a key that repeats, and only an index built with that
//!    check has a key that says so (`exec`'s `index_key`); or
//! 3. the database recorded a count, by this process or an earlier one
//!    (`IndexStore::keys_counted`, beside the stored indexes).
//!
//! Then the scan is the inner table's own plan. Otherwise the scan is that
//! plan under a [`KeyCheckExec`], which counts the keys when the query runs,
//! before its first row, and records the result in the session and, in a
//! writable database, on disk. So `EXPLAIN` reads nothing, and a changed file
//! (another size, modification time or e-tag) is counted again. Anything
//! that does not read only files (an in-memory table, which `INSERT` can
//! change) is counted each time a query over it runs.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use datafusion::arrow::array::{Array, Int64Array};
use datafusion::arrow::datatypes::{DataType, SchemaRef};
use datafusion::catalog::{ScanArgs, ScanResult, Session, TableProvider};
use datafusion::common::tree_node::TreeNodeRecursion;
use datafusion::common::{Column, Constraints, ScalarValue, Statistics, TableReference};
use datafusion::datasource::{provider_as_source, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::TaskContext;
use datafusion::functions_aggregate::count::{count, count_distinct};
use datafusion::logical_expr::{
    lit, Expr, LogicalPlanBuilder, SortExpr, TableProviderFilterPushDown,
};
use datafusion::physical_expr::PhysicalExpr;
use datafusion::physical_plan::execution_plan::CardinalityEffect;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    collect, DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties, SendableRecordBatchStream,
};
use datafusion::prelude::{SessionConfig, SessionContext};
use futures::TryStreamExt;
use ursa_core::KeyRows;

use super::index::{file_identities, fingerprint, portable, FileIdentity};
use crate::database::{database_of_config, IndexKey, IndexStore};

/// The column of a graph node's vertex keys.
pub(crate) const VERTEX_KEY: &str = "__ursa_key";

/// What separates the edges' fingerprint from the vertex keys' in the key of
/// an index whose build refused a key that repeats or is NULL.
pub(crate) const UNIQUE_KEYS: &str = "\n-- unique vertex keys --\n";

/// The refusal of the vertex table `table` of `graph`, keyed by `key`, whose
/// rows `rows` are not one node each. `quoted`: the key is a string.
pub(crate) fn key_error(
    graph: &str,
    table: &str,
    key: &str,
    rows: &KeyRows,
    quoted: bool,
) -> DataFusionError {
    let n = |n: u64, what: &str| match n {
        1 => format!("1 row with {what}"),
        n => format!("{n} rows with {what}"),
    };
    let mut found = Vec::new();
    if rows.duplicates > 0 {
        let example = match (&rows.duplicate, quoted) {
            (Some(value), true) => format!(" ({key} = '{}')", value.replace('\'', "''")),
            (Some(value), false) => format!(" ({key} = {value})"),
            (None, _) => String::new(),
        };
        found.push(format!(
            "{}{example}",
            n(rows.duplicates, "a duplicate key")
        ));
    }
    if rows.nulls > 0 {
        found.push(n(rows.nulls, "a NULL key"));
    }
    DataFusionError::Execution(format!(
        "graph '{graph}': vertex table '{table}' has {}; a vertex table's KEY must identify one \
         row. Deduplicate it in a view and define the graph over the view, for example: \
         CREATE VIEW {table}_nodes AS SELECT DISTINCT ON ({key}) * FROM {table} \
         WHERE {key} IS NOT NULL ORDER BY {key}",
        found.join(" and ")
    ))
}

/// `inner`, the vertex table `table` of `graph` (element name `name`), as a
/// table that refuses to be read while its `key` does not identify one row
/// (see the module docs). It has `inner`'s columns, and its scan is `inner`'s.
pub(crate) fn unique_keys(
    graph: &str,
    name: &str,
    table: &TableReference,
    key: &str,
    inner: Arc<dyn TableProvider>,
) -> Arc<dyn TableProvider> {
    Arc::new(UniqueKeys {
        graph: graph.to_string(),
        name: name.to_string(),
        table: table.clone(),
        key: key.to_string(),
        inner,
    })
}

#[derive(Debug)]
struct UniqueKeys {
    graph: String,
    /// The vertex table's element name, for messages.
    name: String,
    table: TableReference,
    key: String,
    inner: Arc<dyn TableProvider>,
}

impl UniqueKeys {
    /// `scan`, a scan of the inner table, as it is if the keys are known to
    /// be one row each, else under a [`KeyCheckExec`].
    async fn checked(
        &self,
        state: &dyn Session,
        scan: Arc<dyn ExecutionPlan>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let schema = self.inner.schema();
        // A missing key column is reported by what reads it.
        let Ok(index) = schema.index_of(&self.key) else {
            return Ok(scan);
        };
        let key_type = match schema.field(index).data_type().clone() {
            DataType::Dictionary(_, values) => *values,
            other => other,
        };
        let key = Expr::Column(Column::new(Some(self.table.clone()), &self.key));
        let rows = LogicalPlanBuilder::scan(
            self.table.clone(),
            provider_as_source(Arc::clone(&self.inner)),
            None,
        )?;
        // The keys as a graph node reads them (`relation`'s `vertex_keys`),
        // so their fingerprint is the one in a stored index's key.
        let keys = rows
            .clone()
            .project(vec![key.clone().alias(VERTEX_KEY)])?
            .build()?;
        let counts = rows
            .clone()
            .aggregate(
                Vec::<Expr>::new(),
                vec![
                    count(lit(1i64)).alias("rows"),
                    count(key.clone()).alias("keys"),
                    count_distinct(key.clone()).alias("distinct"),
                ],
            )?
            .build()?;
        let counts = state.create_physical_plan(&counts).await?;

        let config = state.config();
        let checks = config.get_extension::<KeyChecks>();
        let store = database_of_config(config).map(|db| (db.index_store(), db.dir().to_path_buf()));
        let identity = match state.optimize(&keys).ok().and_then(|p| fingerprint(&p)) {
            Some(fingerprint) => Some((
                fingerprint,
                file_identities(&counts, &state.task_ctx()).await?,
            )),
            None => None,
        };
        // The key the database knows these keys by: its paths relative to it.
        let stored = match (&store, &identity) {
            (Some((store, dir)), Some((fingerprint, files))) => Some((
                Arc::clone(store),
                IndexKey::new(portable(fingerprint, dir), files.clone()),
            )),
            _ => None,
        };
        if let Some((fingerprint, files)) = &identity {
            if checks.as_ref().is_some_and(|c| c.holds(fingerprint, files)) {
                return Ok(scan);
            }
            if let Some((store, key)) = &stored {
                if store.keys_counted(key) || index_proves(&store.entries(), key) {
                    if let Some(checks) = &checks {
                        checks.record(fingerprint.clone(), files.clone());
                    }
                    return Ok(scan);
                }
            }
        }

        // The smallest key more than one row has, read only for a refusal.
        let count_of = Expr::Column(Column::new_unqualified("rows"));
        let duplicate = rows
            .filter(key.clone().is_not_null())?
            .aggregate(vec![key.alias("key")], vec![count(lit(1i64)).alias("rows")])?
            .filter(count_of.gt(lit(1i64)))?
            .sort(vec![SortExpr::new(
                Expr::Column(Column::new_unqualified("key")),
                true,
                false,
            )])?
            .limit(0, Some(1))?
            .build()?;
        let duplicate = state.create_physical_plan(&duplicate).await?;
        Ok(Arc::new(KeyCheckExec {
            input: scan,
            check: Arc::new(Check {
                graph: self.graph.clone(),
                table: self.name.clone(),
                key: self.key.clone(),
                quoted: matches!(
                    key_type,
                    DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
                ),
                counts,
                duplicate,
                checks,
                identity,
                stored,
                done: tokio::sync::OnceCell::new(),
            }),
        }))
    }
}

/// Whether `entries`, a database's stored indexes, hold one built from the
/// keys `key` names: its key says its build checked them, they are these
/// keys, and it read these files as they are now.
fn index_proves(entries: &[crate::database::index_store::Entry], key: &IndexKey) -> bool {
    entries.iter().any(|e| {
        e.key
            .fingerprint
            .split_once(UNIQUE_KEYS)
            .is_some_and(|(_, keys)| keys == key.fingerprint)
            && key.files.iter().all(|f| e.key.files.contains(f))
    })
}

#[async_trait]
impl TableProvider for UniqueKeys {
    fn schema(&self) -> SchemaRef {
        self.inner.schema()
    }

    fn constraints(&self) -> Option<&Constraints> {
        self.inner.constraints()
    }

    fn table_type(&self) -> TableType {
        self.inner.table_type()
    }

    fn get_table_definition(&self) -> Option<&str> {
        self.inner.get_table_definition()
    }

    fn get_column_default(&self, column: &str) -> Option<&Expr> {
        self.inner.get_column_default(column)
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let scan = self.inner.scan(state, projection, filters, limit).await?;
        self.checked(state, scan).await
    }

    async fn scan_with_args<'a>(
        &self,
        state: &dyn Session,
        args: ScanArgs<'a>,
    ) -> Result<ScanResult> {
        let scan = self.inner.scan_with_args(state, args).await?.into_inner();
        Ok(ScanResult::new(self.checked(state, scan).await?))
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        self.inner.supports_filters_pushdown(filters)
    }

    fn statistics(&self) -> Option<Statistics> {
        self.inner.statistics()
    }
}

/// Counting a vertex table's keys: the plans, what a refusal says, and where
/// a clean count is recorded.
#[derive(Debug)]
struct Check {
    graph: String,
    table: String,
    key: String,
    /// The key is a string.
    quoted: bool,
    /// One row: the rows, the non-null keys and the distinct keys.
    counts: Arc<dyn ExecutionPlan>,
    /// The smallest key more than one row has.
    duplicate: Arc<dyn ExecutionPlan>,
    checks: Option<Arc<KeyChecks>>,
    /// The keys' fingerprint and files, if they read only files.
    identity: Option<(String, Vec<FileIdentity>)>,
    /// The database's stored indexes, and the keys' key there.
    stored: Option<(Arc<IndexStore>, IndexKey)>,
    /// Set once the keys were counted and are one row each.
    done: tokio::sync::OnceCell<()>,
}

impl Check {
    /// Count the keys, once for every partition of the scan.
    async fn run(&self, context: &Arc<TaskContext>) -> Result<()> {
        self.done
            .get_or_try_init(|| self.count(context))
            .await
            .map(|_| ())
    }

    async fn count(&self, context: &Arc<TaskContext>) -> Result<()> {
        if let Some(checks) = &self.checks {
            checks.counts.fetch_add(1, Ordering::Relaxed);
        }
        let batches = collect(Arc::clone(&self.counts), Arc::clone(context)).await?;
        let value = |column: usize| -> u64 {
            batches
                .iter()
                .find(|b| b.num_rows() > 0)
                .and_then(|b| b.column(column).as_any().downcast_ref::<Int64Array>())
                .filter(|a| a.is_valid(0))
                .map_or(0, |a| a.value(0).max(0) as u64)
        };
        let (rows, non_null, distinct) = (value(0), value(1), value(2));
        let mut found = KeyRows {
            duplicates: non_null.saturating_sub(distinct),
            nulls: rows.saturating_sub(non_null),
            duplicate: None,
        };
        if found.is_unique() {
            if let (Some(checks), Some((fingerprint, files))) = (&self.checks, &self.identity) {
                checks.record(fingerprint.clone(), files.clone());
            }
            if let Some((store, key)) = &self.stored {
                store.record_keys_counted(key.clone());
            }
            return Ok(());
        }
        if found.duplicates > 0 {
            let batches = collect(Arc::clone(&self.duplicate), Arc::clone(context)).await?;
            if let Some(batch) = batches.iter().find(|b| b.num_rows() > 0) {
                found.duplicate = Some(match ScalarValue::try_from_array(batch.column(0), 0)? {
                    // A dictionary's value, not `Dictionary(Int32, Utf8("a"))`.
                    ScalarValue::Dictionary(_, value) => value.to_string(),
                    value => value.to_string(),
                });
            }
        }
        Err(key_error(
            &self.graph,
            &self.table,
            &self.key,
            &found,
            self.quoted,
        ))
    }
}

/// A scan of a vertex table whose keys are not yet known to be one row each:
/// it counts them before its first row, and refuses the table if a key
/// repeats or is NULL (see the module docs). Its rows are its input's.
#[derive(Debug)]
struct KeyCheckExec {
    input: Arc<dyn ExecutionPlan>,
    check: Arc<Check>,
}

impl DisplayAs for KeyCheckExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "VertexKeyCheckExec: graph={}, table={}, key={}",
            self.check.graph, self.check.table, self.check.key
        )
    }
}

impl ExecutionPlan for KeyCheckExec {
    fn name(&self) -> &str {
        "VertexKeyCheckExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        self.input.properties()
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.input]
    }

    fn maintains_input_order(&self) -> Vec<bool> {
        vec![true]
    }

    fn benefits_from_input_partitioning(&self) -> Vec<bool> {
        vec![false]
    }

    fn cardinality_effect(&self) -> CardinalityEffect {
        CardinalityEffect::Equal
    }

    fn apply_expressions(
        &self,
        _f: &mut dyn FnMut(&Arc<dyn PhysicalExpr>) -> Result<TreeNodeRecursion>,
    ) -> Result<TreeNodeRecursion> {
        Ok(TreeNodeRecursion::Continue)
    }

    fn with_new_children(
        self: Arc<Self>,
        mut children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        match (children.pop(), children.is_empty()) {
            (Some(input), true) => Ok(Arc::new(Self {
                input,
                check: Arc::clone(&self.check),
            })),
            _ => Err(DataFusionError::Internal(
                "VertexKeyCheckExec has one child".to_string(),
            )),
        }
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let (input, check) = (Arc::clone(&self.input), Arc::clone(&self.check));
        let rows = futures::stream::once(async move {
            check.run(&context).await?;
            input.execute(partition, context)
        })
        .try_flatten();
        Ok(Box::pin(RecordBatchStreamAdapter::new(self.schema(), rows)))
    }
}

/// How many tables' counts a session keeps.
const CAPACITY: usize = 64;

/// The vertex tables of a session whose keys were counted and found to
/// identify one row each (a session config extension): the fingerprint of
/// each one's keys' plan, and the files it read.
#[derive(Debug, Default)]
pub(crate) struct KeyChecks {
    unique: Mutex<HashMap<String, Vec<FileIdentity>>>,
    /// How many times this session counted a vertex table's keys.
    counts: AtomicU64,
}

impl KeyChecks {
    fn holds(&self, fingerprint: &str, files: &[FileIdentity]) -> bool {
        let unique = self.unique.lock().unwrap_or_else(|e| e.into_inner());
        unique.get(fingerprint).is_some_and(|f| f == files)
    }

    fn record(&self, fingerprint: String, files: Vec<FileIdentity>) {
        let mut unique = self.unique.lock().unwrap_or_else(|e| e.into_inner());
        if unique.len() >= CAPACITY && !unique.contains_key(&fingerprint) {
            unique.clear();
        }
        unique.insert(fingerprint, files);
    }
}

/// Install an empty record of key counts on `config`.
pub(crate) fn with_key_checks(config: SessionConfig) -> SessionConfig {
    config.with_extension(Arc::new(KeyChecks::default()))
}

/// How many times `ctx` has counted a vertex table's keys to read it as
/// `<graph>.nodes` or a `GRAPH_TABLE` node: for tests of what is kept.
pub fn vertex_key_counts(ctx: &SessionContext) -> u64 {
    ctx.state()
        .config()
        .get_extension::<KeyChecks>()
        .map(|c| c.counts.load(Ordering::Relaxed))
        .unwrap_or(0)
}
