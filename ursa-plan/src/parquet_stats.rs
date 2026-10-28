//! Parquet file statistics that never claim exact bounds for a float column (#202).
//!
//! Parquet min/max statistics leave NaN out (the format says writers must), so
//! a float column's min and max bound only its non-NaN values. DataFusion 54
//! still marks them exact: a file whose non-NaN values are all `1.0` reports
//! `min == max == 1.0`, exact, with no nulls, and the Parquet opener then reads
//! the column as that constant (`constant_columns_from_stats`), so every NaN in
//! the file reads back as `1.0`. A row group whose values are all NaN carries no
//! min/max at all, and the file-level merge skips it, so one such group beside a
//! constant group is enough.
//!
//! [`InexactFloatBounds`] wraps the Parquet format and marks the min/max of each
//! float column inexact in the statistics it infers. The bounds still prune; a
//! column is only read as a constant (or a `min` / `max` answered from the
//! footer) when they are exact, which they now never are for a float. Every
//! other column's statistics are unchanged.

use std::sync::Arc;

use arrow::datatypes::{DataType, Schema, SchemaRef};
use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::common::Statistics;
use datafusion::datasource::file_format::file_compression_type::FileCompressionType;
use datafusion::datasource::file_format::{FileFormat, FileMeta};
use datafusion::datasource::physical_plan::{FileScanConfig, FileSinkConfig, FileSource};
use datafusion::datasource::table_schema::TableSchema;
use datafusion::error::Result;
use datafusion::physical_expr::LexRequirement;
use datafusion::physical_expr_common::sort_expr::LexOrdering;
use datafusion::physical_plan::ExecutionPlan;
use object_store::{ObjectMeta, ObjectStore};

/// A file format that infers the statistics `inner` does, with the min/max of
/// every float column marked inexact. See the module docs.
#[derive(Debug)]
pub struct InexactFloatBounds {
    inner: Arc<dyn FileFormat>,
}

impl InexactFloatBounds {
    pub fn new(inner: Arc<dyn FileFormat>) -> Self {
        Self { inner }
    }
}

fn is_float(t: &DataType) -> bool {
    match t {
        DataType::Float16 | DataType::Float32 | DataType::Float64 => true,
        DataType::Dictionary(_, v) => is_float(v),
        _ => false,
    }
}

/// `stats` (one column statistic per field of `schema`, in order) with the
/// min/max of each float column inexact.
fn inexact_float_bounds(mut stats: Statistics, schema: &Schema) -> Statistics {
    for (field, col) in schema.fields().iter().zip(&mut stats.column_statistics) {
        if is_float(field.data_type()) {
            col.min_value = col.min_value.clone().to_inexact();
            col.max_value = col.max_value.clone().to_inexact();
        }
    }
    stats
}

#[async_trait]
impl FileFormat for InexactFloatBounds {
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
        Ok(inexact_float_bounds(stats, &table_schema))
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
        meta.statistics = inexact_float_bounds(meta.statistics, &table_schema);
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
    use arrow::datatypes::Field;
    use datafusion::common::stats::Precision;
    use datafusion::common::{ColumnStatistics, ScalarValue};

    #[test]
    fn only_float_bounds_become_inexact() {
        let schema = Schema::new(vec![
            Field::new("i", DataType::Int64, false),
            Field::new("f", DataType::Float64, false),
            Field::new("h", DataType::Float32, false),
        ]);
        let col = |v: ScalarValue| ColumnStatistics {
            null_count: Precision::Exact(0),
            min_value: Precision::Exact(v.clone()),
            max_value: Precision::Exact(v),
            ..ColumnStatistics::new_unknown()
        };
        let stats = Statistics {
            num_rows: Precision::Exact(3),
            total_byte_size: Precision::Absent,
            column_statistics: vec![
                col(ScalarValue::Int64(Some(1))),
                col(ScalarValue::Float64(Some(1.0))),
                col(ScalarValue::Float32(Some(1.0))),
            ],
        };
        let out = inexact_float_bounds(stats.clone(), &schema);
        assert_eq!(out.num_rows, stats.num_rows);
        assert_eq!(out.column_statistics[0], stats.column_statistics[0]);
        for (c, v) in [
            (1, ScalarValue::Float64(Some(1.0))),
            (2, ScalarValue::Float32(Some(1.0))),
        ] {
            let got = &out.column_statistics[c];
            assert_eq!(got.min_value, Precision::Inexact(v.clone()));
            assert_eq!(got.max_value, Precision::Inexact(v));
            assert_eq!(got.null_count, Precision::Exact(0));
        }
    }
}
