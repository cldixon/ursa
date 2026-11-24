//! Rows of a table by their numbers: how the inspector reads an edge's row
//! once the graph's index has said which it is (#489).
//!
//! [`take`] gives the rows of a table numbered so, when the table is read as
//! one partition (as every table is in the browser): then its rows come in
//! one order, and the edge index numbered them in it.
//!
//! - **A Parquet file** is read with a row selection ([`selected`]): the
//!   reader skips the row groups the rows are not in, and in a row group
//!   passes over the pages before a row without decoding them.
//! - **Any other table** is read from its start up to the last of the rows.
//!
//! A Parquet reader still asks for a row group's whole column chunks, unless
//! the file has a page index: 3.8 MB for a million edges, and in the browser
//! a request of the server for every row read. So the bytes read for a
//! selection are kept ([`ReadCache`]), a session's last [`CACHE_BYTES`] of
//! them, under the file's size, time and entity tag, which a scan looks up
//! afresh: the next row of the same row group asks the server for nothing,
//! and a file that has changed is read again.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use datafusion::arrow::array::RecordBatch;
use datafusion::common::stats::Precision;
use datafusion::dataframe::DataFrame;
use datafusion::datasource::listing::PartitionedFile;
use datafusion::datasource::physical_plan::parquet::{
    DefaultParquetFileReaderFactory, ParquetFileReaderFactory, ParquetRowSelection,
};
use datafusion::datasource::physical_plan::{
    FileGroup, FileScanConfig, FileScanConfigBuilder, FileSource, ParquetSource,
};
use datafusion::datasource::source::DataSourceExec;
use datafusion::error::Result;
use datafusion::parquet::arrow::arrow_reader::{ArrowReaderOptions, RowSelection};
use datafusion::parquet::arrow::async_reader::AsyncFileReader;
use datafusion::parquet::file::metadata::ParquetMetaData;
use datafusion::physical_plan::metrics::ExecutionPlanMetricsSet;
use datafusion::physical_plan::projection::ProjectionExec;
use datafusion::physical_plan::{
    replace_children_if_necessary, ExecutionPlan, ExecutionPlanProperties,
};
use datafusion::prelude::{SessionConfig, SessionContext};
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};

/// The most bytes a session keeps of what it read for rows by number.
pub const CACHE_BYTES: usize = 64 << 20;

/// The rows of `frame` numbered `rows` (ascending, each once), in that
/// order: fewer when it ends before one of them. `None` when it is read as
/// several partitions: its rows then come in no one order, and a row number
/// finds nothing.
pub async fn take(
    ctx: &SessionContext,
    frame: DataFrame,
    rows: &[u64],
) -> Result<Option<Vec<RecordBatch>>> {
    let plan = frame.create_physical_plan().await?;
    if plan.output_partitioning().partition_count() != 1 {
        return Ok(None);
    }
    let mut taken = Vec::with_capacity(rows.len());
    if let Some(plan) = selected(ctx, &plan, rows) {
        let mut stream = plan.execute(0, ctx.task_ctx())?;
        while let Some(batch) = stream.next().await {
            taken.push(batch?);
        }
    } else {
        let mut stream = plan.execute(0, ctx.task_ctx())?;
        let (mut wanted, mut passed) = (rows.iter().copied().peekable(), 0u64);
        while wanted.peek().is_some() {
            let Some(batch) = stream.next().await else {
                break;
            };
            let batch = batch?;
            let end = passed + batch.num_rows() as u64;
            while let Some(row) = wanted.next_if(|&row| row < end) {
                taken.push(batch.slice((row - passed) as usize, 1));
            }
            passed = end;
        }
    }
    Ok(Some(taken))
}

/// `plan` reading only its rows numbered `rows` (ascending), when it is a
/// scan of one whole Parquet file with a known number of rows, under nothing
/// but projections: the file's scan with a row selection, its reads kept in
/// the session's [`ReadCache`]. `None` for any other plan, and when a row is
/// past the file's end.
fn selected(
    ctx: &SessionContext,
    plan: &Arc<dyn ExecutionPlan>,
    rows: &[u64],
) -> Option<Arc<dyn ExecutionPlan>> {
    if let Some(projection) = plan.downcast_ref::<ProjectionExec>() {
        let input = selected(ctx, projection.input(), rows)?;
        return replace_children_if_necessary(Arc::clone(plan), vec![input]).ok();
    }
    let config = plan
        .downcast_ref::<DataSourceExec>()?
        .data_source()
        .downcast_ref::<FileScanConfig>()?;
    let source = config.file_source().downcast_ref::<ParquetSource>()?;
    let [group] = config.file_groups.as_slice() else {
        return None;
    };
    let [file] = group.files() else {
        return None;
    };
    // The whole file, every row: no filter in the scan, no limit, and no
    // range but the whole file (a scan of a file over HTTP, as the browser
    // reads a dataset, names the whole file as its range).
    let whole = file
        .range
        .as_ref()
        .is_none_or(|r| r.start <= 0 && r.end >= file.object_meta.size as i64);
    if config.limit.is_some() || !whole || source.filter().is_some() {
        return None;
    }
    let Precision::Exact(total) = file.statistics.as_ref()?.num_rows else {
        return None;
    };
    if rows.last().is_none_or(|&last| last >= total as u64) {
        return None;
    }
    let selection = RowSelection::from_consecutive_ranges(
        rows.iter().map(|&row| row as usize..row as usize + 1),
        total,
    );
    let file = file
        .clone()
        .with_extension(ParquetRowSelection::new(selection));
    let mut builder = FileScanConfigBuilder::from(config.clone())
        .with_file_groups(vec![FileGroup::new(vec![file])]);
    if let Some(cache) = ctx.state().config().get_extension::<ReadCache>() {
        let inner = match source.parquet_file_reader_factory() {
            Some(factory) => Arc::clone(factory),
            None => {
                let store = ctx.runtime_env().object_store(&config.object_store_url);
                Arc::new(DefaultParquetFileReaderFactory::new(store.ok()?))
            }
        };
        let readers = Arc::new(KeptReads {
            inner,
            cache,
            store: config.object_store_url.to_string(),
        });
        builder = builder.with_source(Arc::new(
            source.clone().with_parquet_file_reader_factory(readers),
        ));
    }
    Some(DataSourceExec::from_data_source(builder.build()))
}

/// Install an empty [`ReadCache`] on `config`.
pub(crate) fn with_read_cache(config: SessionConfig) -> SessionConfig {
    config.with_extension(Arc::new(ReadCache::default()))
}

/// A file as it was when its bytes were read: its store and path, and the
/// size, time and entity tag the store gave for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FileVersion {
    store: String,
    location: String,
    size: u64,
    modified: i64,
    e_tag: Option<String>,
}

/// The bytes a session read for rows by number (a session config
/// extension): byte ranges of files, the least recently used dropped once
/// they pass [`CACHE_BYTES`]. A range longer than that is not kept.
#[derive(Debug, Default)]
pub struct ReadCache {
    held: Mutex<Held>,
}

#[derive(Debug, Default)]
struct Held {
    ranges: HashMap<(FileVersion, Range<u64>), (Bytes, u64)>,
    bytes: usize,
    /// Bumped on every use, to find the least recently used range.
    clock: u64,
}

impl ReadCache {
    fn get(&self, file: &FileVersion, range: &Range<u64>) -> Option<Bytes> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        held.clock += 1;
        let now = held.clock;
        let (bytes, used) = held.ranges.get_mut(&(file.clone(), range.clone()))?;
        *used = now;
        Some(bytes.clone())
    }

    fn put(&self, file: &FileVersion, range: Range<u64>, bytes: &Bytes) {
        if bytes.len() > CACHE_BYTES {
            return;
        }
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        // Another version of the file is of no use again.
        let stale = |(other, _): &(FileVersion, Range<u64>)| {
            other != file && other.store == file.store && other.location == file.location
        };
        let mut freed = 0;
        held.ranges.retain(|key, (bytes, _)| {
            let keep = !stale(key);
            if !keep {
                freed += bytes.len();
            }
            keep
        });
        held.bytes -= freed;
        held.clock += 1;
        let now = held.clock;
        if let Some((old, _)) = held
            .ranges
            .insert((file.clone(), range), (bytes.clone(), now))
        {
            held.bytes -= old.len();
        }
        held.bytes += bytes.len();
        while held.bytes > CACHE_BYTES {
            let Some(oldest) = held
                .ranges
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some((bytes, _)) = held.ranges.remove(&oldest) {
                held.bytes -= bytes.len();
            }
        }
    }

    /// How many bytes are held: for tests.
    pub fn bytes(&self) -> usize {
        self.held.lock().unwrap_or_else(|e| e.into_inner()).bytes
    }
}

/// Parquet readers whose reads are kept in a [`ReadCache`]: `inner`'s, which
/// read the file (and keep its footer, if they do).
#[derive(Debug)]
struct KeptReads {
    inner: Arc<dyn ParquetFileReaderFactory>,
    cache: Arc<ReadCache>,
    store: String,
}

impl ParquetFileReaderFactory for KeptReads {
    fn create_reader(
        &self,
        partition_index: usize,
        partitioned_file: PartitionedFile,
        metadata_size_hint: Option<usize>,
        metrics: &ExecutionPlanMetricsSet,
    ) -> Result<Box<dyn AsyncFileReader + Send>> {
        let meta = &partitioned_file.object_meta;
        let file = FileVersion {
            store: self.store.clone(),
            location: meta.location.to_string(),
            size: meta.size,
            modified: meta.last_modified.timestamp_nanos_opt().unwrap_or(i64::MAX),
            e_tag: meta.e_tag.clone(),
        };
        let inner = self.inner.create_reader(
            partition_index,
            partitioned_file,
            metadata_size_hint,
            metrics,
        )?;
        Ok(Box::new(KeptReader {
            inner,
            cache: Arc::clone(&self.cache),
            file,
        }))
    }
}

/// A Parquet reader that asks its [`ReadCache`] first.
struct KeptReader {
    inner: Box<dyn AsyncFileReader + Send>,
    cache: Arc<ReadCache>,
    file: FileVersion,
}

impl AsyncFileReader for KeptReader {
    fn get_bytes(
        &mut self,
        range: Range<u64>,
    ) -> BoxFuture<'_, datafusion::parquet::errors::Result<Bytes>> {
        async move {
            if let Some(bytes) = self.cache.get(&self.file, &range) {
                return Ok(bytes);
            }
            let bytes = self.inner.get_bytes(range.clone()).await?;
            self.cache.put(&self.file, range, &bytes);
            Ok(bytes)
        }
        .boxed()
    }

    fn get_byte_ranges(
        &mut self,
        ranges: Vec<Range<u64>>,
    ) -> BoxFuture<'_, datafusion::parquet::errors::Result<Vec<Bytes>>> {
        async move {
            let mut found: Vec<Option<Bytes>> = ranges
                .iter()
                .map(|range| self.cache.get(&self.file, range))
                .collect();
            let missing: Vec<Range<u64>> = ranges
                .iter()
                .zip(&found)
                .filter(|(_, found)| found.is_none())
                .map(|(range, _)| range.clone())
                .collect();
            if !missing.is_empty() {
                // One request for all of them, as the reader would make.
                let read = self.inner.get_byte_ranges(missing.clone()).await?;
                let mut read = missing.into_iter().zip(read);
                for slot in found.iter_mut().filter(|slot| slot.is_none()) {
                    if let Some((range, bytes)) = read.next() {
                        self.cache.put(&self.file, range, &bytes);
                        *slot = Some(bytes);
                    }
                }
            }
            found
                .into_iter()
                .map(|bytes| {
                    bytes.ok_or_else(|| {
                        datafusion::parquet::errors::ParquetError::General(
                            "a byte range was not read".into(),
                        )
                    })
                })
                .collect()
        }
        .boxed()
    }

    fn get_metadata<'a>(
        &'a mut self,
        options: Option<&'a ArrowReaderOptions>,
    ) -> BoxFuture<'a, datafusion::parquet::errors::Result<Arc<ParquetMetaData>>> {
        self.inner.get_metadata(options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::Int64Array;

    async fn plan_of(ctx: &SessionContext, query: &str) -> Arc<dyn ExecutionPlan> {
        let frame = ctx.sql(query).await.unwrap();
        frame.create_physical_plan().await.unwrap()
    }

    /// Column `at` of what `plan` reads.
    async fn column(ctx: &SessionContext, plan: Arc<dyn ExecutionPlan>, at: usize) -> Vec<i64> {
        let batches = datafusion::physical_plan::collect(plan, ctx.task_ctx())
            .await
            .unwrap();
        batches
            .iter()
            .flat_map(|b| {
                let c = b.column(at).as_any().downcast_ref::<Int64Array>().unwrap();
                c.values().to_vec()
            })
            .collect()
    }

    fn cached(ctx: &SessionContext) -> usize {
        let cache = ctx.state().config().get_extension::<ReadCache>();
        cache.expect("a session has a read cache").bytes()
    }

    /// A scan of one Parquet file reads rows by number with a row selection,
    /// across its row groups and under a projection, and keeps what it read;
    /// anything else has no selection, and `take` passes over its rows.
    #[tokio::test]
    async fn a_parquet_scan_reads_rows_by_a_selection() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str| dir.path().join(name).display().to_string();
        let ctx = crate::new_session();
        for sql in [
            "SET datafusion.execution.target_partitions = 1".to_string(),
            "SET datafusion.execution.parquet.max_row_group_size = 100".to_string(),
            format!(
                "COPY (SELECT value AS i, value * 2 AS j FROM range(0, 1000) ORDER BY i) TO '{}'",
                file("t.parquet")
            ),
            format!(
                "COPY (SELECT value AS i FROM range(0, 1000) ORDER BY i) TO '{}'",
                file("t.csv")
            ),
        ] {
            ctx.sql(&sql).await.unwrap().collect().await.unwrap();
        }
        let table = format!("'{}'", file("t.parquet"));

        let plan = plan_of(&ctx, &format!("SELECT * FROM {table}")).await;
        let rows = [0, 99, 100, 101, 555, 999];
        assert_eq!(cached(&ctx), 0);
        let read = selected(&ctx, &plan, &rows).expect("a row selection");
        assert_eq!(column(&ctx, read, 0).await, [0, 99, 100, 101, 555, 999]);
        // What it read is kept, and read from there the next time: the
        // same rows, and nothing more is kept for them.
        let kept = cached(&ctx);
        assert!(kept > 0);
        let again = selected(&ctx, &plan, &[99, 555]).unwrap();
        assert_eq!(column(&ctx, again, 1).await, [198, 1110]);
        assert_eq!(cached(&ctx), kept);
        // Under a projection, the same rows.
        let plan = plan_of(&ctx, &format!("SELECT j + 1 AS k, i FROM {table}")).await;
        let read = selected(&ctx, &plan, &[7, 640]).expect("a selection under a projection");
        assert_eq!(column(&ctx, read, 0).await, [15, 1281]);

        // A scan that names the whole file as its range (as one of a file
        // over HTTP does) is of every row; one of part of the file is not.
        let ranged = |end: Option<i64>| {
            let config = plan
                .downcast_ref::<DataSourceExec>()
                .and_then(|exec| exec.data_source().downcast_ref::<FileScanConfig>())
                .expect("a scan of the file");
            let file = config.file_groups[0].files()[0].clone();
            let size = file.object_meta.size as i64;
            let file = file.with_range(0, end.unwrap_or(size));
            let config = FileScanConfigBuilder::from(config.clone())
                .with_file_groups(vec![FileGroup::new(vec![file])])
                .build();
            DataSourceExec::from_data_source(config) as Arc<dyn ExecutionPlan>
        };
        let read = selected(&ctx, &ranged(None), &[3, 640]).expect("the whole file's range");
        assert_eq!(column(&ctx, read, 1).await, [3, 640]);
        assert!(selected(&ctx, &ranged(Some(100)), &[3]).is_none());

        // A row past the file's end, no rows, a filter, a limit, and a file
        // of another format: no selection.
        let plan = plan_of(&ctx, &format!("SELECT * FROM {table}")).await;
        assert!(selected(&ctx, &plan, &[5, 1000]).is_none());
        assert!(selected(&ctx, &plan, &[]).is_none());
        for query in [
            format!("SELECT * FROM {table} WHERE i > 500"),
            format!("SELECT * FROM {table} LIMIT 10"),
            format!("SELECT * FROM '{}'", file("t.csv")),
        ] {
            let plan = plan_of(&ctx, &query).await;
            assert!(selected(&ctx, &plan, &[5]).is_none(), "{query}");
        }
        // `take` reads those from their start, and stops at the table's end.
        let csv = ctx
            .sql(&format!("SELECT * FROM '{}'", file("t.csv")))
            .await
            .unwrap();
        let taken = take(&ctx, csv, &[3, 998, 1000]).await.unwrap().unwrap();
        let rows: usize = taken.iter().map(|b| b.num_rows()).sum();
        assert_eq!(rows, 2);

        // The file written again: its rows are read again, not the old ones.
        std::thread::sleep(std::time::Duration::from_millis(20));
        ctx.sql(&format!(
            "COPY (SELECT value + 5000 AS i, value AS j FROM range(0, 1000) ORDER BY i) TO {table}"
        ))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
        let plan = plan_of(&ctx, &format!("SELECT * FROM {table}")).await;
        let read = selected(&ctx, &plan, &[99, 555]).unwrap();
        assert_eq!(column(&ctx, read, 0).await, [5099, 5555]);
    }

    #[test]
    fn the_read_cache_drops_the_least_recently_used_and_stale_versions() {
        let cache = ReadCache::default();
        let version = |location: &str, size: u64| FileVersion {
            store: "file:///".into(),
            location: location.into(),
            size,
            modified: 0,
            e_tag: None,
        };
        let block = Bytes::from(vec![0u8; CACHE_BYTES / 2 - 1]);
        let (a, b) = (version("a", 1), version("b", 1));
        cache.put(&a, 0..1, &block);
        cache.put(&b, 0..1, &block);
        assert!(cache.get(&a, &(0..1)).is_some());
        // A third does not fit: b, used longest ago, goes.
        cache.put(&a, 1..2, &block);
        assert!(cache.get(&b, &(0..1)).is_none());
        assert!(cache.get(&a, &(0..1)).is_some() && cache.get(&a, &(1..2)).is_some());
        // Another version of a: the old one's ranges go.
        cache.put(&version("a", 2), 0..1, &Bytes::from_static(b"new"));
        assert!(cache.get(&a, &(0..1)).is_none());
        assert_eq!(cache.bytes(), 3);
        // More than the cache holds is not kept.
        cache.put(&b, 0..1, &Bytes::from(vec![0u8; CACHE_BYTES + 1]));
        assert_eq!(cache.bytes(), 3);
    }
}
