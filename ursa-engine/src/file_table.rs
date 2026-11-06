//! Files as tables: the `UrlTableFactory` behind `SELECT * FROM 'file.parquet'`.
//!
//! This does what DataFusion's `DynamicListTableFactory` (installed by
//! `SessionContext::enable_url_table`) does, plus four things it does not:
//!
//! - registers the object store for a URL the first time it is read (as
//!   datafusion-cli's `DynamicObjectStoreCatalog` does);
//! - expands globs in URLs, not just in local paths (see [`crate::paths`]);
//! - reads `.jsonl` and `.ndjson` files (and `.jsonl.gz`, …) as
//!   newline-delimited JSON, as DataFusion reads `.json`;
//! - reports why a location could not be opened. DataFusion's factory turns any
//!   failure into "table not found", which hides a missing file or an unknown
//!   extension.
//!
//! [`open_files`] is the same with options: what `read_csv(...)` and its
//! siblings plan (see [`crate::read_functions`]).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::catalog::{TableProvider, UrlTableFactory};
use datafusion::common::parsers::CompressionTypeVariant;
use datafusion::common::GetExt;
use datafusion::datasource::file_format::json::JsonFormatFactory;
use datafusion::datasource::file_format::{FileFormat as DataFusionFileFormat, FileFormatFactory};
use datafusion::datasource::listing::{ListingOptions, ListingTable, ListingTableConfig};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionState;
use futures::StreamExt;
use parking_lot::RwLock;

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
/// directories (`year=2024/`) become columns. A relative path is relative to
/// the current directory, in a session with a database too: a stored
/// definition's paths are made relative to the database's directory when it is
/// stored, and resolved against it on replay (see [`crate::database`]).
pub async fn open_file_table(
    state: &SessionState,
    location: &str,
) -> Result<Arc<dyn TableProvider>> {
    open_files(state, &[location.to_string()], &FileOptions::default()).await
}

/// A file format Ursa reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    Parquet,
    Csv,
    /// Newline-delimited JSON, or a JSON array of objects with
    /// [`FileOptions::newline_delimited`] `false`.
    Json,
    Arrow,
}

impl FileFormat {
    /// The format a file extension names (without its dot or compression
    /// suffix). `json`, `jsonl` and `ndjson` are all newline-delimited JSON.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "parquet" => Some(Self::Parquet),
            "csv" => Some(Self::Csv),
            "json" | "jsonl" | "ndjson" => Some(Self::Json),
            "arrow" => Some(Self::Arrow),
            _ => None,
        }
    }

    /// The extension DataFusion registers the format's factory under.
    fn factory(self) -> &'static str {
        match self {
            Self::Parquet => "parquet",
            Self::Csv => "csv",
            Self::Json => "json",
            Self::Arrow => "arrow",
        }
    }

    /// Whether the format's files are compressed whole (`.csv.gz`).
    fn compressible(self) -> bool {
        matches!(self, Self::Csv | Self::Json)
    }
}

/// How to read files, beyond what their extension says: the options of
/// `read_csv(...)` and its siblings. `None` leaves a choice to the extension
/// or to the session's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileOptions {
    /// The format, whatever the extension.
    pub format: Option<FileFormat>,
    /// The compression of CSV or JSON files, whatever the extension.
    pub compression: Option<CompressionTypeVariant>,
    /// CSV: whether the first line is a header.
    pub has_header: Option<bool>,
    /// CSV: the field delimiter.
    pub delimiter: Option<u8>,
    /// CSV: the quote character.
    pub quote: Option<u8>,
    /// JSON: one object per line (`true`), or one array of objects (`false`).
    pub newline_delimited: Option<bool>,
}

/// Open `locations` (paths, globs, directories or URLs) as one listing table,
/// read as `options` say. What `options` leaves open comes from the extension
/// of the first file found, as DataFusion infers it; a directory or glob
/// lists only the files with that extension. Each location must name a file.
pub async fn open_files(
    state: &SessionState,
    locations: &[String],
    options: &FileOptions,
) -> Result<Arc<dyn TableProvider>> {
    let shown = locations.join("', '");
    let mut urls = Vec::with_capacity(locations.len());
    let mut first_file = None;
    // Each location's first file's extensions (`.jsonl.gz`), to list by.
    let mut extensions = Vec::with_capacity(locations.len());
    for location in locations {
        let url = table_url(location)?;
        ensure_object_store(state.runtime_env(), url.get_url()).await?;
        let store = state.runtime_env().object_store(&url)?;
        let file = url
            .list_all_files(state, store.as_ref(), "")
            .await
            .map_err(|e| explain(location, &e))?
            .next()
            .await
            .transpose()
            .map_err(|e| explain(location, &e))?
            .ok_or_else(|| DataFusionError::Plan(format!("no file found at '{location}'")))?;
        let (ext, suffix) = extensions_of(file.location.as_ref())?;
        // A directory or glob lists the files with the first file's extension
        // and compression suffix, as DataFusion's `infer_options` does; a file
        // whose extension names no format (`read_csv('data.txt')`) by its own.
        extensions.push(match (&ext, &suffix) {
            (Some(ext), Some(suffix)) => format!(".{ext}.{suffix}"),
            (Some(ext), None) => format!(".{ext}"),
            (None, _) => String::new(),
        });
        first_file.get_or_insert((ext, suffix));
        urls.push(url);
    }
    let (ext, suffix) =
        first_file.ok_or_else(|| DataFusionError::Plan("no file to read".into()))?;
    let named = ext.as_deref().and_then(FileFormat::from_extension);
    let format = options.format.or(named).ok_or_else(|| {
        DataFusionError::Plan(format!(
            "cannot tell the format of '{shown}' from its extension; Ursa reads .parquet, \
             .csv, .json, .jsonl, .ndjson and .arrow files, and compressed CSV and JSON (.gz, \
             .bz2, .xz, .zst). read_parquet(), read_csv() and read_json() read a file \
             whatever its extension"
        ))
    })?;
    let mut format_options = HashMap::new();
    let mut set = |key: &str, value: String| {
        format_options.insert(format!("format.{key}"), value);
    };
    if format.compressible() {
        let compression = match (options.compression, &suffix) {
            (Some(c), _) => Some(c),
            (None, Some(s)) => Some(s.parse::<CompressionTypeVariant>()?),
            (None, None) => None,
        };
        if let Some(c) = compression {
            set("compression", compression_name(c));
        }
    }
    if format == FileFormat::Csv {
        if let Some(header) = options.has_header {
            set("has_header", header.to_string());
        }
        if let Some(delimiter) = options.delimiter {
            set("delimiter", char::from(delimiter).to_string());
        }
        if let Some(quote) = options.quote {
            set("quote", char::from(quote).to_string());
        }
    }
    if format == FileFormat::Json {
        if let Some(newline_delimited) = options.newline_delimited {
            set("newline_delimited", newline_delimited.to_string());
        }
    }
    let file_format = state
        .get_file_format_factory(format.factory())
        .ok_or_else(|| {
            DataFusionError::Internal(format!("no {} file format factory", format.factory()))
        })?
        .create(state, &format_options)?;
    // One listing extension for all the locations: theirs if they agree
    // (`['a.jsonl', 'b.ndjson']` do not), else none.
    let extension = if extensions.windows(2).all(|w| w[0] == w[1]) {
        extensions.swap_remove(0)
    } else {
        String::new()
    };
    let config = ListingTableConfig::new_with_multi_paths(urls)
        .with_listing_options(ListingOptions::new(file_format).with_file_extension(extension))
        .infer_partitions_from_path(state)
        .await
        .map_err(|e| explain(&shown, &e))?
        .infer_schema(state)
        .await
        .map_err(|e| explain(&shown, &e))?;
    Ok(Arc::new(ListingTable::try_new(config)?))
}

/// JSON under another extension, for DataFusion's own lookups of a format by
/// extension: `COPY … TO 'out.jsonl'` writes newline-delimited JSON, as `COPY
/// … TO 'out.json'` does.
#[derive(Debug)]
pub(crate) struct JsonLines(pub(crate) &'static str);

impl FileFormatFactory for JsonLines {
    fn create(
        &self,
        state: &dyn Session,
        format_options: &HashMap<String, String>,
    ) -> Result<Arc<dyn DataFusionFileFormat>> {
        JsonFormatFactory::new().create(state, format_options)
    }

    fn default(&self) -> Arc<dyn DataFusionFileFormat> {
        JsonFormatFactory::new().default()
    }
}

impl GetExt for JsonLines {
    fn get_ext(&self) -> String {
        self.0.to_string()
    }
}

/// A file's extension and compression suffix, as DataFusion infers them
/// (`x.jsonl.gz` has `jsonl` and `gz`); no extension for a file without one.
fn extensions_of(path: &str) -> Result<(Option<String>, Option<String>)> {
    let (ext, suffix) = ListingTableConfig::infer_file_extension_and_compression_type(path)?;
    // For a file without an extension, DataFusion gives the whole path.
    let ext = (!ext.contains('/') && ext != path).then_some(ext);
    Ok((ext, suffix))
}

/// The value DataFusion's `format.compression` option takes for `c`.
fn compression_name(c: CompressionTypeVariant) -> String {
    match c {
        CompressionTypeVariant::UNCOMPRESSED => "uncompressed".to_string(),
        other => other.to_string().to_lowercase(),
    }
}

/// Say why `location` could not be opened.
fn explain(location: &str, err: &DataFusionError) -> DataFusionError {
    DataFusionError::Plan(format!("cannot read '{location}': {}", user_message(err)))
}
