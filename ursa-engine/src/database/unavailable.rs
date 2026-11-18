//! Objects of a database's catalog that could not be defined when it was
//! opened (#381): a view over a file that was moved or deleted, a graph over
//! such a view.
//!
//! The database opens anyway. Such a view or external table is registered as
//! an [`UnavailableTable`], so `SHOW TABLES` lists it and `DROP` and `CREATE OR
//! REPLACE` work on it as on any table, while a query that uses it fails with
//! the reason. Such a graph is marked in the graph registry
//! ([`crate::graph::GraphRegistry::mark_unavailable`]): using it fails with
//! the reason, and `DROP PROPERTY GRAPH` and `CREATE OR REPLACE PROPERTY
//! GRAPH` repair it. Its statement stays in the catalog untouched, so
//! restoring the file and opening the database again brings it back.

use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::datatypes::{Schema, SchemaRef};
use datafusion::catalog::{Session, TableProvider};
use datafusion::datasource::TableType;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::Expr;
use datafusion::physical_plan::ExecutionPlan;

/// A view or external table of the catalog that could not be defined when the
/// database was opened. It has no columns; reading it fails with
/// [`UnavailableTable::message`].
#[derive(Debug)]
pub struct UnavailableTable {
    message: String,
    table_type: TableType,
}

impl UnavailableTable {
    /// An unavailable view (`table_type` [`TableType::View`]) or external
    /// table ([`TableType::Base`]); `message` says why, and how to repair it.
    pub fn new(message: impl Into<String>, table_type: TableType) -> Self {
        Self {
            message: message.into(),
            table_type,
        }
    }

    /// Why the object is unavailable: `view v is unavailable: …`.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The error for using it.
    pub fn error(&self) -> DataFusionError {
        DataFusionError::Plan(self.message.clone())
    }
}

#[async_trait]
impl TableProvider for UnavailableTable {
    fn schema(&self) -> SchemaRef {
        Arc::new(Schema::empty())
    }

    fn table_type(&self) -> TableType {
        self.table_type
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        _projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Err(self.error())
    }
}

/// `provider` as an unavailable table, if it is one.
pub fn as_unavailable(provider: &dyn TableProvider) -> Option<&UnavailableTable> {
    provider.downcast_ref::<UnavailableTable>()
}

/// An error if `provider` is an unavailable table: what using it gives.
pub fn check_available(provider: &dyn TableProvider) -> Result<()> {
    match as_unavailable(provider) {
        Some(table) => Err(table.error()),
        None => Ok(()),
    }
}
