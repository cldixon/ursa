//! Parquet file statistics that claim exact bounds only where every row group
//! has them (#202).
//!
//! DataFusion 54 merges a file's row-group min/max into file-level bounds and
//! marks them exact when one row group's exact bound equals the merged value,
//! even if another row group carries no bound at all. Its Parquet opener then
//! reads a column whose exact min equals its exact max (with no nulls) as that
//! constant (`constant_columns_from_stats`), and `AggregateStatistics` answers
//! `min` / `max` from them. A row group can carry no bound for an ordinary
//! reason: a float group whose values are all NaN (Parquet leaves NaN out of
//! min/max), a string longer than the writer's statistics limit (pyarrow's is
//! 4 KiB), or a group written without statistics. Beside a constant group, every
//! row of the file then reads back as that constant.
//!
//! Float bounds are never exact either: NaN is left out of them, so a single row
//! group `[NaN, 1.0, 1.0]` reports `min == max == 1.0`.
//!
//! [`CheckedBounds`] wraps the Parquet format and marks a column's file-level
//! min/max inexact unless every row group with a non-null value carries an
//! exact min and max for it, and always for a float column. Inexact bounds still
//! prune; they no longer stand in for the values. Row and null counts are
//! unchanged.

use std::sync::Arc;

use arrow::datatypes::{DataType, Schema, SchemaRef};
use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::common::Statistics;
use datafusion::datasource::file_format::file_compression_type::FileCompressionType;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::file_format::{FileFormat, FileMeta};
use datafusion::datasource::physical_plan::parquet::metadata::DFParquetMetadata;
use datafusion::datasource::physical_plan::{FileScanConfig, FileSinkConfig, FileSource};
use datafusion::datasource::table_schema::TableSchema;
use datafusion::error::Result;
use datafusion::parquet::arrow::arrow_reader::statistics::StatisticsConverter;
use datafusion::parquet::file::metadata::ParquetMetaData;
use datafusion::physical_expr::LexRequirement;
use datafusion::physical_expr_common::sort_expr::LexOrdering;
use datafusion::physical_plan::ExecutionPlan;
use object_store::{ObjectMeta, ObjectStore};

/// A file format that infers the statistics `inner` does, with a column's
/// min/max exact only where the file's row groups back them. See the module docs.
#[derive(Debug)]
pub struct CheckedBounds {
    inner: Arc<dyn FileFormat>,
    /// The Parquet format's footer size hint, for reading the row groups.
    metadata_size_hint: Option<usize>,
}

impl CheckedBounds {
    pub fn new(inner: Arc<dyn FileFormat>) -> Self {
        let metadata_size_hint = inner
            .downcast_ref::<ParquetFormat>()
            .and_then(ParquetFormat::metadata_size_hint);
        Self {
            inner,
            metadata_size_hint,
        }
    }

    /// `stats` with each column's min/max inexact unless it may stay exact
    /// ([`exact_columns`]). The footer is read (through the runtime's metadata
    /// cache, where the inner format left it) only when some bound is exact; a
    /// footer that can't be read leaves no bound exact.
    async fn check(
        &self,
        stats: Statistics,
        state: &dyn Session,
        store: &Arc<dyn ObjectStore>,
        schema: &Schema,
        object: &ObjectMeta,
    ) -> Statistics {
        let any_exact = stats
            .column_statistics
            .iter()
            .any(|c| c.min_value.is_exact() == Some(true) || c.max_value.is_exact() == Some(true));
        if !any_exact {
            return stats;
        }
        let cache = state.runtime_env().cache_manager.get_file_metadata_cache();
        let metadata = DFParquetMetadata::new(store.as_ref(), object)
            .with_metadata_size_hint(self.metadata_size_hint)
            .with_file_metadata_cache(Some(cache))
            .fetch_metadata()
            .await;
        let exact = match metadata {
            Ok(m) => exact_columns(&m, schema),
            Err(_) => vec![false; schema.fields().len()],
        };
        inexact_bounds(stats, &exact)
    }
}

fn is_float(t: &DataType) -> bool {
    match t {
        DataType::Float16 | DataType::Float32 | DataType::Float64 => true,
        DataType::Dictionary(_, v) => is_float(v),
        _ => false,
    }
}

/// Per field of `schema`: whether its file-level min/max may stay exact. A
/// float's never may; any other column's may when every row group that holds
/// a non-null value carries an exact min and max for it. A column the file
/// lacks keeps what the inner format reported (all null).
fn exact_columns(metadata: &ParquetMetaData, schema: &Schema) -> Vec<bool> {
    let parquet_schema = metadata.file_metadata().schema_descr();
    schema
        .fields()
        .iter()
        .map(|field| {
            if is_float(field.data_type()) {
                return false;
            }
            let Ok(converter) = StatisticsConverter::try_new(field.name(), schema, parquet_schema)
            else {
                return false;
            };
            let Some(i) = converter.parquet_column_index() else {
                return true;
            };
            metadata.row_groups().iter().all(|rg| {
                rg.column(i).statistics().is_some_and(|s| {
                    s.null_count_opt() == Some(rg.num_rows() as u64)
                        || (s.min_is_exact() && s.max_is_exact())
                })
            })
        })
        .collect()
}

/// `stats` (one column statistic per field, in order) with the min/max of each
/// column whose `exact` entry is false marked inexact.
fn inexact_bounds(mut stats: Statistics, exact: &[bool]) -> Statistics {
    for (col, &exact) in stats.column_statistics.iter_mut().zip(exact) {
        if !exact {
            col.min_value = col.min_value.clone().to_inexact();
            col.max_value = col.max_value.clone().to_inexact();
        }
    }
    stats
}

#[async_trait]
impl FileFormat for CheckedBounds {
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
    ) -> Result<SchemaRef> {
        self.inner.infer_schema(state, store, objects).await
    }

    async fn infer_stats(
        &self,
        state: &dyn Session,
        store: &Arc<dyn ObjectStore>,
        table_schema: SchemaRef,
        object: &ObjectMeta,
    ) -> Result<Statistics> {
        let stats = self
            .inner
            .infer_stats(state, store, Arc::clone(&table_schema), object)
            .await?;
        Ok(self.check(stats, state, store, &table_schema, object).await)
    }

    async fn infer_ordering(
        &self,
        state: &dyn Session,
        store: &Arc<dyn ObjectStore>,
        table_schema: SchemaRef,
        object: &ObjectMeta,
    ) -> Result<Option<LexOrdering>> {
        self.inner
            .infer_ordering(state, store, table_schema, object)
            .await
    }

    async fn infer_stats_and_ordering(
        &self,
        state: &dyn Session,
        store: &Arc<dyn ObjectStore>,
        table_schema: SchemaRef,
        object: &ObjectMeta,
    ) -> Result<FileMeta> {
        let mut meta = self
            .inner
            .infer_stats_and_ordering(state, store, Arc::clone(&table_schema), object)
            .await?;
        meta.statistics = self
            .check(meta.statistics, state, store, &table_schema, object)
            .await;
        Ok(meta)
    }

    async fn create_physical_plan(
        &self,
        state: &dyn Session,
        conf: FileScanConfig,
    ) -> Result<Arc<dyn ExecutionPlan>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, Int64Array, RecordBatch, StringArray};
    use arrow::datatypes::Field;
    use datafusion::common::stats::Precision;
    use datafusion::common::{ColumnStatistics, ScalarValue};
    use datafusion::parquet::arrow::ArrowWriter;
    use datafusion::parquet::file::properties::WriterProperties;
    use datafusion::parquet::file::reader::{FileReader, SerializedFileReader};

    /// Columns `i` (constant), `s` (constant), `f` (constant float) and `n`
    /// (a null row group, then a constant one), in two row groups of two rows.
    fn constant_file(name: &str) -> (ParquetMetaData, Schema) {
        let schema = Schema::new(vec![
            Field::new("i", DataType::Int64, false),
            Field::new("s", DataType::Utf8, false),
            Field::new("f", DataType::Float64, false),
            Field::new("n", DataType::Int64, true),
        ]);
        let batch = RecordBatch::try_new(
            Arc::new(schema.clone()),
            vec![
                Arc::new(Int64Array::from(vec![7; 4])),
                Arc::new(StringArray::from(vec!["a"; 4])),
                Arc::new(Float64Array::from(vec![1.0; 4])),
                Arc::new(Int64Array::from(vec![None, None, Some(3), Some(3)])),
            ],
        )
        .unwrap();
        let path = std::env::temp_dir().join(name);
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(2))
            .build();
        let file = std::fs::File::create(&path).unwrap();
        let mut w = ArrowWriter::try_new(file, Arc::new(schema.clone()), Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        let reader = SerializedFileReader::new(std::fs::File::open(&path).unwrap()).unwrap();
        let metadata = reader.metadata().clone();
        std::fs::remove_file(&path).ok();
        (metadata, schema)
    }

    #[test]
    fn bounds_stay_exact_only_where_every_row_group_has_them() {
        let (metadata, schema) = constant_file("ursa_parquet_stats_exact.parquet");
        // A float is never exact; an all-null row group needs no bounds.
        assert_eq!(exact_columns(&metadata, &schema), [true, true, false, true]);

        // Row group 1 written without statistics for `i` (or with a value too
        // long to bound, or all NaN): its bounds are unknown, so the file's are
        // not exact.
        let row_groups = metadata
            .row_groups()
            .iter()
            .enumerate()
            .map(|(g, rg)| {
                let columns = rg
                    .columns()
                    .iter()
                    .enumerate()
                    .map(|(c, col)| {
                        if g == 1 && c == 0 {
                            col.clone()
                                .into_builder()
                                .clear_statistics()
                                .build()
                                .unwrap()
                        } else {
                            col.clone()
                        }
                    })
                    .collect();
                rg.clone()
                    .into_builder()
                    .set_column_metadata(columns)
                    .build()
                    .unwrap()
            })
            .collect();
        let metadata = metadata.into_builder().set_row_groups(row_groups).build();
        assert_eq!(
            exact_columns(&metadata, &schema),
            [false, true, false, true]
        );
    }

    #[test]
    fn inexact_bounds_keep_counts() {
        let col = |v: i64| ColumnStatistics {
            null_count: Precision::Exact(0),
            min_value: Precision::Exact(ScalarValue::Int64(Some(v))),
            max_value: Precision::Exact(ScalarValue::Int64(Some(v))),
            ..ColumnStatistics::new_unknown()
        };
        let stats = Statistics {
            num_rows: Precision::Exact(3),
            total_byte_size: Precision::Absent,
            column_statistics: vec![col(1), col(2)],
        };
        let out = inexact_bounds(stats.clone(), &[true, false]);
        assert_eq!(out.num_rows, stats.num_rows);
        assert_eq!(out.column_statistics[0], stats.column_statistics[0]);
        let got = &out.column_statistics[1];
        assert_eq!(
            got.min_value,
            Precision::Inexact(ScalarValue::Int64(Some(2)))
        );
        assert_eq!(
            got.max_value,
            Precision::Inexact(ScalarValue::Int64(Some(2)))
        );
        assert_eq!(got.null_count, Precision::Exact(0));
    }
}
