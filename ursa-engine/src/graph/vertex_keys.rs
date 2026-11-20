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
//!   vertex table's rows through [`unique_keys`], a table that counts its
//!   inner table's keys when a scan of it is planned, and then scans the
//!   inner table as it is: the plan that runs is the plan of the table itself.
//!
//! Before this rule the algorithms counted a duplicated key once and skipped
//! a NULL key, while `<graph>.nodes` and `GRAPH_TABLE` returned every row, so
//! the three saw different node sets.
//!
//! The count of a table that reads only files is kept for the session
//! ([`KeyChecks`]), under the fingerprint of the keys' plan and the identity
//! of the files it reads, as a graph index is (see `index`): a changed file
//! is counted again. Anything else (an in-memory table, which `INSERT` can
//! change) is counted each time a scan of it is planned.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use datafusion::arrow::array::{Array, Int64Array};
use datafusion::arrow::datatypes::{DataType, SchemaRef};
use datafusion::catalog::{ScanArgs, ScanResult, Session, TableProvider};
use datafusion::common::{Column, Constraints, ScalarValue, Statistics, TableReference};
use datafusion::datasource::{provider_as_source, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::functions_aggregate::count::{count, count_distinct};
use datafusion::logical_expr::{
    lit, Expr, LogicalPlan, LogicalPlanBuilder, SortExpr, TableProviderFilterPushDown,
};
use datafusion::physical_plan::{collect, ExecutionPlan};
use datafusion::prelude::SessionConfig;
use ursa_core::KeyRows;

use super::index::{file_identities, fingerprint, FileIdentity};

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

/// `inner`, the vertex table `table` of `graph`, as a table that refuses to
/// be scanned while its `key` does not identify one row (see the module
/// docs). It has `inner`'s columns, and its scan is `inner`'s.
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
    /// Refuse the table if its key repeats or is NULL.
    async fn check(&self, state: &dyn Session) -> Result<()> {
        let schema = self.inner.schema();
        // A missing key column is reported by what reads it.
        let Ok(index) = schema.index_of(&self.key) else {
            return Ok(());
        };
        let key_type = schema.field(index).data_type().clone();
        let key = Expr::Column(Column::new(Some(self.table.clone()), &self.key));
        let keys = LogicalPlanBuilder::scan(
            self.table.clone(),
            provider_as_source(Arc::clone(&self.inner)),
            Some(vec![index]),
        )?
        .build()?;
        let counts = LogicalPlanBuilder::from(keys.clone())
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
        let context = state.task_ctx();

        // Counted before, as these files are now?
        let checks = state.config().get_extension::<KeyChecks>();
        let identity = match (
            &checks,
            state.optimize(&keys).ok().and_then(|p| fingerprint(&p)),
        ) {
            (Some(_), Some(fingerprint)) => {
                Some((fingerprint, file_identities(&counts, &context).await?))
            }
            _ => None,
        };
        if let (Some(checks), Some((fingerprint, files))) = (&checks, &identity) {
            if checks.holds(fingerprint, files) {
                return Ok(());
            }
        }

        let batches = collect(counts, Arc::clone(&context)).await?;
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
            if let (Some(checks), Some((fingerprint, files))) = (checks, identity) {
                checks.record(fingerprint, files);
            }
            return Ok(());
        }
        if found.duplicates > 0 {
            found.duplicate = self.smallest_duplicate(state, keys, key).await?;
        }
        let key_type = match key_type {
            DataType::Dictionary(_, values) => *values,
            other => other,
        };
        let quoted = matches!(
            key_type,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
        );
        Err(key_error(
            &self.graph,
            &self.name,
            &self.key,
            &found,
            quoted,
        ))
    }

    /// The smallest key of `keys` that more than one row has, as the index
    /// build names it.
    async fn smallest_duplicate(
        &self,
        state: &dyn Session,
        keys: LogicalPlan,
        key: Expr,
    ) -> Result<Option<String>> {
        let rows = Expr::Column(Column::new_unqualified("rows"));
        let plan = LogicalPlanBuilder::from(keys)
            .filter(key.clone().is_not_null())?
            .aggregate(vec![key.alias("key")], vec![count(lit(1i64)).alias("rows")])?
            .filter(rows.gt(lit(1i64)))?
            .sort(vec![SortExpr::new(
                Expr::Column(Column::new_unqualified("key")),
                true,
                false,
            )])?
            .limit(0, Some(1))?
            .build()?;
        let plan = state.create_physical_plan(&plan).await?;
        let batches = collect(plan, state.task_ctx()).await?;
        let Some(batch) = batches.iter().find(|b| b.num_rows() > 0) else {
            return Ok(None);
        };
        Ok(match ScalarValue::try_from_array(batch.column(0), 0)? {
            // A dictionary's value, not `Dictionary(Int32, Utf8("a"))`.
            ScalarValue::Dictionary(_, value) => Some(value.to_string()),
            value => Some(value.to_string()),
        })
    }
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
        self.check(state).await?;
        self.inner.scan(state, projection, filters, limit).await
    }

    async fn scan_with_args<'a>(
        &self,
        state: &dyn Session,
        args: ScanArgs<'a>,
    ) -> Result<ScanResult> {
        self.check(state).await?;
        self.inner.scan_with_args(state, args).await
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

/// How many tables' counts a session keeps.
const CAPACITY: usize = 64;

/// The vertex tables of a session whose keys were counted and found to
/// identify one row each (a session config extension): the fingerprint of
/// each one's keys' plan, and the files it read.
#[derive(Debug, Default)]
pub(crate) struct KeyChecks {
    unique: Mutex<HashMap<String, Vec<FileIdentity>>>,
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
