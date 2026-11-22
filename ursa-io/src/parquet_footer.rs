//! A Parquet file's footer checked before the `parquet` crate reads it (#332).
//!
//! parquet 59 trusts the metadata length a footer states: a file that ends in
//! `PAR1` but claims more metadata than it holds (a truncated download, a file
//! still being written, eight bytes of `PAR1PAR1`) makes it subtract with
//! overflow. A debug build panics; a release build wraps and reads a garbage
//! range. Either way the user sees a crash, not an error.
//!
//! [`ValidParquet`] wraps the Parquet format and checks each file it is given
//! before the inner format reads its footer: at least 12 bytes, `PAR1` at both
//! ends, and a stated metadata length that fits between the two. A file that
//! fails is an error naming it, "is not a valid Parquet file". A file is
//! checked once per table (by path, size and modification time), so a scan
//! after the schema was inferred costs no more reads.
//!
//! [`ValidParquetFactory`] is the session's `parquet` format factory wrapped
//! the same way, so `FROM 'x.parquet'`, `read_parquet`, `CREATE EXTERNAL TABLE
//! … STORED AS PARQUET` and graphs over them all get the check.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::common::{GetExt, Statistics};
use datafusion::datasource::file_format::file_compression_type::FileCompressionType;
use datafusion::datasource::file_format::parquet::ParquetFormatFactory;
use datafusion::datasource::file_format::{FileFormat, FileFormatFactory, FileMeta};
use datafusion::datasource::physical_plan::{FileScanConfig, FileSinkConfig, FileSource};
use datafusion::datasource::table_schema::TableSchema;
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_expr::LexRequirement;
use datafusion::physical_expr_common::sort_expr::LexOrdering;
use datafusion::physical_plan::ExecutionPlan;
use object_store::{ObjectMeta, ObjectStore};
use std::collections::HashMap;

/// The magic number at both ends of a Parquet file.
const MAGIC: &[u8; 4] = b"PAR1";
/// The magic number of a file whose footer is encrypted. The `parquet` crate
/// says what it makes of one; this check only asks that it is consistent.
const ENCRYPTED_MAGIC: &[u8; 4] = b"PARE";
/// The footer: a 4-byte little-endian metadata length, then the magic.
const FOOTER: u64 = 8;
/// The smallest a Parquet file can be: the leading magic and the footer.
const SMALLEST: u64 = 4 + FOOTER;

/// Why `head` (the file's first 4 bytes) and `tail` (its last 8) are not the
/// ends of a Parquet file of `size` bytes, if they are not.
fn footer_problem(size: u64, head: &[u8], tail: &[u8]) -> Option<String> {
    if size < SMALLEST {
        return Some(format!(
            "it is {size} byte{}, and a Parquet file is at least {SMALLEST}",
            if size == 1 { "" } else { "s" }
        ));
    }
    let magic = &tail[4..8];
    if magic != MAGIC && magic != ENCRYPTED_MAGIC {
        return Some("it does not end with the Parquet magic number PAR1".into());
    }
    if head != magic {
        return Some(format!(
            "it does not start with the Parquet magic number {}",
            String::from_utf8_lossy(magic)
        ));
    }
    let metadata = u64::from(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]));
    if metadata > size - SMALLEST {
        return Some(format!(
            "its footer says the metadata is {metadata} bytes, but the file is {size} bytes; it \
             may be truncated"
        ));
    }
    None
}

/// How a message names the file at `location` in `store`: a local file by its
/// absolute path, an object by its key.
fn shown(store: &dyn ObjectStore, location: &object_store::path::Path) -> String {
    if store.to_string().starts_with("LocalFileSystem") {
        format!("/{location}")
    } else {
        location.to_string()
    }
}

/// An error unless `object` in `store` has the ends of a Parquet file (see the
/// module docs). Its message starts `cannot read '<file>'`, which the
/// engine's messages keep as it is.
pub async fn check_parquet_file(store: &dyn ObjectStore, object: &ObjectMeta) -> Result<()> {
    let size = object.size;
    let problem = if size < SMALLEST {
        footer_problem(size, &[], &[])
    } else {
        let ends = store
            .get_ranges(&object.location, &[0..4, size - FOOTER..size])
            .await?;
        footer_problem(size, &ends[0], &ends[1])
    };
    match problem {
        None => Ok(()),
        Some(why) => Err(DataFusionError::Plan(format!(
            "cannot read '{}': it is not a valid Parquet file: {why}",
            shown(store, &object.location)
        ))),
    }
}

/// A Parquet file format that checks each file's footer before `inner` reads
/// it. See the module docs.
#[derive(Debug)]
pub struct ValidParquet {
    inner: Arc<dyn FileFormat>,
    /// The files already checked: path, size and modification time.
    checked: Mutex<HashSet<(String, u64, i64)>>,
}

impl ValidParquet {
    pub fn new(inner: Arc<dyn FileFormat>) -> Self {
        Self {
            inner,
            checked: Mutex::default(),
        }
    }

    /// The format this one checks files for.
    pub fn inner(&self) -> &Arc<dyn FileFormat> {
        &self.inner
    }

    async fn check(&self, store: &dyn ObjectStore, object: &ObjectMeta) -> Result<()> {
        let key = (
            object.location.to_string(),
            object.size,
            object.last_modified.timestamp_nanos_opt().unwrap_or(0),
        );
        if self
            .checked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&key)
        {
            return Ok(());
        }
        check_parquet_file(store, object).await?;
        self.checked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key);
        Ok(())
    }
}

#[async_trait]
impl FileFormat for ValidParquet {
    fn get_ext(&self) -> String {
        self.inner.get_ext()
    }

    fn get_ext_with_compression(&self, c: &FileCompressionType) -> Result<String> {
        self.inner.get_ext_with_compression(c)
    }

    fn compression_type(&self) -> Option<FileCompressionType> {
        self.inner.compression_type()
    }

    async fn infer_schema(
        &self,
        state: &dyn Session,
        store: &Arc<dyn ObjectStore>,
        objects: &[ObjectMeta],
    ) -> Result<datafusion::arrow::datatypes::SchemaRef> {
        for object in objects {
            self.check(store.as_ref(), object).await?;
        }
        self.inner.infer_schema(state, store, objects).await
    }

    async fn infer_stats(
        &self,
        state: &dyn Session,
        store: &Arc<dyn ObjectStore>,
        table_schema: datafusion::arrow::datatypes::SchemaRef,
        object: &ObjectMeta,
    ) -> Result<Statistics> {
        self.check(store.as_ref(), object).await?;
        self.inner
            .infer_stats(state, store, table_schema, object)
            .await
    }

    async fn infer_ordering(
        &self,
        state: &dyn Session,
        store: &Arc<dyn ObjectStore>,
        table_schema: datafusion::arrow::datatypes::SchemaRef,
        object: &ObjectMeta,
    ) -> Result<Option<LexOrdering>> {
        self.check(store.as_ref(), object).await?;
        self.inner
            .infer_ordering(state, store, table_schema, object)
            .await
    }

    async fn infer_stats_and_ordering(
        &self,
        state: &dyn Session,
        store: &Arc<dyn ObjectStore>,
        table_schema: datafusion::arrow::datatypes::SchemaRef,
        object: &ObjectMeta,
    ) -> Result<FileMeta> {
        self.check(store.as_ref(), object).await?;
        self.inner
            .infer_stats_and_ordering(state, store, table_schema, object)
            .await
    }

    async fn create_physical_plan(
        &self,
        state: &dyn Session,
        conf: FileScanConfig,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        // A file listed after the schema was inferred (a directory or glob
        // that gained one) is checked before it is opened.
        let store = state.runtime_env().object_store(&conf.object_store_url)?;
        for group in &conf.file_groups {
            for file in group.iter() {
                self.check(store.as_ref(), &file.object_meta).await?;
            }
        }
        self.inner.create_physical_plan(state, conf).await
    }

    async fn create_writer_physical_plan(
        &self,
        input: Arc<dyn ExecutionPlan>,
        state: &dyn Session,
        conf: FileSinkConfig,
        order_requirements: Option<LexRequirement>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        self.inner
            .create_writer_physical_plan(input, state, conf, order_requirements)
            .await
    }

    fn file_source(&self, table_schema: TableSchema) -> Arc<dyn FileSource> {
        self.inner.file_source(table_schema)
    }
}

/// DataFusion's `parquet` format factory, its formats wrapped in
/// [`ValidParquet`]. Register it in place of DataFusion's.
#[derive(Debug)]
pub struct ValidParquetFactory {
    inner: ParquetFormatFactory,
}

impl ValidParquetFactory {
    pub fn new() -> Self {
        Self {
            inner: ParquetFormatFactory::new(),
        }
    }
}

impl Default for ValidParquetFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl FileFormatFactory for ValidParquetFactory {
    fn create(
        &self,
        state: &dyn Session,
        format_options: &HashMap<String, String>,
    ) -> Result<Arc<dyn FileFormat>> {
        Ok(Arc::new(ValidParquet::new(
            self.inner.create(state, format_options)?,
        )))
    }

    fn default(&self) -> Arc<dyn FileFormat> {
        Arc::new(ValidParquet::new(self.inner.default()))
    }
}

impl GetExt for ValidParquetFactory {
    fn get_ext(&self) -> String {
        self.inner.get_ext()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tail(metadata: u32, magic: &[u8; 4]) -> Vec<u8> {
        let mut t = metadata.to_le_bytes().to_vec();
        t.extend_from_slice(magic);
        t
    }

    #[test]
    fn footers_that_do_not_fit_the_file_are_problems() {
        // Too small to hold the two magic numbers and a length.
        for size in [0, 4, 8, 11] {
            let why = footer_problem(size, &[], &[]).unwrap();
            assert!(why.contains("at least 12"), "{why}");
        }
        assert!(footer_problem(1, &[], &[]).unwrap().contains("1 byte,"));
        // The smallest consistent file: no metadata at all (the parser says
        // what is wrong with that).
        assert_eq!(footer_problem(12, MAGIC, &tail(0, MAGIC)), None);
        assert_eq!(footer_problem(1012, MAGIC, &tail(1000, MAGIC)), None);
        // One byte more metadata than the file holds: the parquet crate's
        // subtraction overflow (#332).
        let why = footer_problem(1012, MAGIC, &tail(1001, MAGIC)).unwrap();
        assert!(why.contains("may be truncated"), "{why}");
        let why = footer_problem(12, MAGIC, &tail(u32::MAX, MAGIC)).unwrap();
        assert!(why.contains("may be truncated"), "{why}");
        // Not Parquet at either end.
        let why = footer_problem(100, b"src,", b"0,1\n2,3\n").unwrap();
        assert!(why.contains("does not end with"), "{why}");
        let why = footer_problem(100, b"xxxx", &tail(4, MAGIC)).unwrap();
        assert!(why.contains("does not start with"), "{why}");
        // An encrypted footer is the parser's to judge, if both ends agree.
        assert_eq!(
            footer_problem(100, ENCRYPTED_MAGIC, &tail(4, ENCRYPTED_MAGIC)),
            None
        );
        assert!(footer_problem(100, MAGIC, &tail(4, ENCRYPTED_MAGIC)).is_some());
    }
}
