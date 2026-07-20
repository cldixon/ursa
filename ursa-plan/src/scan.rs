//! Reading edge files into Arrow through a DataFusion scan.
//!
//! This is the "scan produces the Arrow columns" half of the ingress story: a
//! Parquet or CSV path (local for v0.1; object storage rides the same
//! `SessionContext` once registered) is read through DataFusion — which pushes
//! the `(src, dst)` projection into the file — and the two endpoint columns come
//! back as a single `(src, dst)` Int64 `RecordBatch` ready for `build_topology`.

use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::compute::{cast, concat_batches};
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::{CsvReadOptions, ParquetReadOptions, SessionContext};

/// Read the `src`/`dst` columns of an edge file into one `(src, dst)` Int64 batch.
///
/// Format is chosen by extension (`.parquet` / `.csv`), the two forms Ursa v0.1
/// supports. The projection is pushed down, so only the endpoint columns are read;
/// they are cast to Int64 (the v0.1 node-id type).
pub fn scan_edges_batch(path: &str, src: &str, dst: &str) -> Result<RecordBatch> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| DataFusionError::Execution(format!("failed to build runtime: {e}")))?;

    runtime.block_on(async move {
        let ctx = SessionContext::new();
        let lower = path.to_ascii_lowercase();
        let df = if lower.ends_with(".parquet") {
            ctx.read_parquet(path, ParquetReadOptions::default())
                .await?
        } else if lower.ends_with(".csv") {
            ctx.read_csv(path, CsvReadOptions::default()).await?
        } else {
            return Err(DataFusionError::NotImplemented(format!(
                "scan_edges supports .parquet and .csv in v0.1; got path {path:?}"
            )));
        };

        let df = df.select_columns(&[src, dst])?;
        let batches = df.collect().await?;
        if batches.is_empty() {
            return Err(DataFusionError::Execution(format!(
                "edge file {path:?} produced no rows"
            )));
        }

        let read_schema = batches[0].schema();
        let merged = concat_batches(&read_schema, &batches)?;
        let src_i64 = cast(merged.column(0), &DataType::Int64)
            .map_err(|e| DataFusionError::ArrowError(e, None))?;
        let dst_i64 = cast(merged.column(1), &DataType::Int64)
            .map_err(|e| DataFusionError::ArrowError(e, None))?;

        let schema = Arc::new(Schema::new(vec![
            Field::new("src", DataType::Int64, true),
            Field::new("dst", DataType::Int64, true),
        ]));
        RecordBatch::try_new(schema, vec![src_i64, dst_i64])
            .map_err(|e| DataFusionError::ArrowError(e, None))
    })
}

/// Read a node/attribute file into one `RecordBatch`, keeping **every** column.
///
/// Unlike [`scan_edges_batch`], there is no projection: a node table is an
/// attribute table, so all columns are carried through. Only the `id` column is
/// cast to Int64 (the v0.1 node-id type); attribute columns keep their file
/// types. The result feeds `execute_node_query`'s `nodes` slot, where algorithm
/// outputs are LEFT-joined onto it by id — exactly like an in-memory
/// `from_arrow(..., id=...)` table.
pub fn scan_nodes_batch(path: &str, id: &str) -> Result<RecordBatch> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| DataFusionError::Execution(format!("failed to build runtime: {e}")))?;

    runtime.block_on(async move {
        let ctx = SessionContext::new();
        let lower = path.to_ascii_lowercase();
        let df = if lower.ends_with(".parquet") {
            ctx.read_parquet(path, ParquetReadOptions::default())
                .await?
        } else if lower.ends_with(".csv") {
            ctx.read_csv(path, CsvReadOptions::default()).await?
        } else {
            return Err(DataFusionError::NotImplemented(format!(
                "scan_nodes supports .parquet and .csv in v0.1; got path {path:?}"
            )));
        };

        let batches = df.collect().await?;
        if batches.is_empty() {
            return Err(DataFusionError::Execution(format!(
                "node file {path:?} produced no rows"
            )));
        }

        let read_schema = batches[0].schema();
        let merged = concat_batches(&read_schema, &batches)?;
        let id_idx = read_schema.index_of(id).map_err(|_| {
            DataFusionError::Execution(format!(
                "scan_nodes: id column {id:?} not found in node file {path:?}"
            ))
        })?;

        // Cast only the id column to Int64; attribute columns keep their types.
        let mut columns = merged.columns().to_vec();
        columns[id_idx] = cast(&columns[id_idx], &DataType::Int64)
            .map_err(|e| DataFusionError::ArrowError(e, None))?;
        let fields: Vec<Field> = read_schema
            .fields()
            .iter()
            .enumerate()
            .map(|(i, f)| {
                if i == id_idx {
                    Field::new(f.name(), DataType::Int64, f.is_nullable())
                } else {
                    f.as_ref().clone()
                }
            })
            .collect();
        RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
            .map_err(|e| DataFusionError::ArrowError(e, None))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Int64Array;
    use std::io::Write;

    #[test]
    fn reads_csv_endpoints_and_casts_to_int64() {
        // Write a tiny CSV with extra columns to prove projection + cast.
        let dir = std::env::temp_dir();
        let path = dir.join("ursa_scan_test_edges.csv");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "from,to,weight").unwrap();
            writeln!(f, "10,20,0.5").unwrap();
            writeln!(f, "20,30,0.9").unwrap();
        }
        let batch = scan_edges_batch(path.to_str().unwrap(), "from", "to").unwrap();
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(batch.num_rows(), 2);
        let src = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(src.values(), &[10, 20]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reads_node_attributes_keeping_all_columns() {
        let dir = std::env::temp_dir();
        let path = dir.join("ursa_scan_test_nodes.csv");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "tower_id,region,capacity").unwrap();
            writeln!(f, "1,us,10").unwrap();
            writeln!(f, "2,eu,20").unwrap();
        }
        let batch = scan_nodes_batch(path.to_str().unwrap(), "tower_id").unwrap();
        assert_eq!(batch.num_rows(), 2);
        // All three columns kept; id cast to Int64.
        assert_eq!(batch.num_columns(), 3);
        let schema = batch.schema();
        let id_idx = schema.index_of("tower_id").unwrap();
        assert_eq!(schema.field(id_idx).data_type(), &DataType::Int64);
        let ids = batch
            .column(id_idx)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(ids.values(), &[1, 2]);
        assert!(schema.index_of("region").is_ok());
        assert!(schema.index_of("capacity").is_ok());
        std::fs::remove_file(&path).ok();
    }
}
