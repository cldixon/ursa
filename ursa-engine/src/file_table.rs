//! Files as tables: the `UrlTableFactory` behind `SELECT * FROM 'file.parquet'`.
//!
//! This does what DataFusion's `DynamicListTableFactory` (installed by
//! `SessionContext::enable_url_table`) does, plus three things it does not:
//!
//! - registers the object store for a URL the first time it is read (as
//!   datafusion-cli's `DynamicObjectStoreCatalog` does);
//! - expands globs in URLs, not just in local paths (see [`crate::paths`]);
//! - reports why a location could not be opened. DataFusion's factory turns any
//!   failure into "table not found", which hides a missing file or an unknown
//!   extension.

use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use datafusion::catalog::{TableProvider, UrlTableFactory};
use datafusion::datasource::listing::{ListingTable, ListingTableConfig, ListingTableConfigExt};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionState;
use parking_lot::RwLock;

use crate::database::resolve_location;
use crate::error::user_message;
use crate::paths::{looks_like_location, table_url};
use crate::stores::ensure_object_store;

/// Opens a location named in `FROM` as a listing table.
#[derive(Debug, Default)]
pub(crate) struct FileTableFactory {
    /// The session this factory belongs to, set once the session is built.
    session: OnceLock<Weak<RwLock<SessionState>>>,
}

impl FileTableFactory {
    pub(crate) fn attach(&self, session: Weak<RwLock<SessionState>>) {
        let _ = self.session.set(session);
    }
}

#[async_trait]
impl UrlTableFactory for FileTableFactory {
    async fn try_new(&self, name: &str) -> Result<Option<Arc<dyn TableProvider>>> {
        if !looks_like_location(name) {
            return Ok(None);
        }
        let state = self
            .session
            .get()
            .and_then(Weak::upgrade)
            .map(|state| state.read().clone())
            .ok_or_else(|| DataFusionError::Internal("the session has been dropped".into()))?;
        open_file_table(&state, name).await.map(Some)
    }
}

/// Open `location` (a path, glob, directory or URL) as a listing table. The
/// format and compression come from the file extension; Hive-style partition
/// directories (`year=2024/`) become columns. In a session with a database, a
/// relative path is relative to the database's directory
/// ([`crate::database::resolve_location`]).
pub async fn open_file_table(
    state: &SessionState,
    location: &str,
) -> Result<Arc<dyn TableProvider>> {
    let url = table_url(&resolve_location(state, location))?;
    ensure_object_store(state.runtime_env(), url.get_url()).await?;
    let config = ListingTableConfig::new(url)
        .infer_options(state)
        .await
        .map_err(|e| explain(location, &e))?
        .infer_partitions_from_path(state)
        .await
        .map_err(|e| explain(location, &e))?
        .infer_schema(state)
        .await
        .map_err(|e| explain(location, &e))?;
    Ok(Arc::new(ListingTable::try_new(config)?))
}

/// Say why `location` could not be opened.
fn explain(location: &str, err: &DataFusionError) -> DataFusionError {
    let message = user_message(err);
    if message.contains("No files for table") {
        DataFusionError::Plan(format!("no file found at '{location}'"))
    } else if message.contains("No file_format found with extension") {
        DataFusionError::Plan(format!(
            "cannot tell the format of '{location}' from its extension; Ursa reads .parquet, \
             .csv, .json and .arrow files, and compressed CSV and JSON (.gz, .bz2, .xz, .zst)"
        ))
    } else {
        DataFusionError::Plan(format!("cannot read '{location}': {message}"))
    }
}
