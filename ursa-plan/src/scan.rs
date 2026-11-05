//! Reading edge/node files: [`ursa_io::scan`], re-exported, plus the edge-table
//! scan that pushes a plan's filters (Ursa predicate JSON) into the read.
//!
//! Everything about reading a file — the formats, CSV options and inference,
//! object stores, row order and the scan's error labels — is in `ursa-io`. What
//! stays here is the one part that needs the plan's expression dialect: lowering
//! a filter step to a DataFusion expression the scan may push down.

use datafusion::common::DFSchema;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::Expr;
use serde_json::Value;

pub use ursa_io::csv::CsvOptions;
pub use ursa_io::scan::*;
pub use ursa_io::store::scan_identity;

/// Read an edge file as a table, keeping the rows every predicate in `filters`
/// holds for (see [`scan_edges_rows_with`]).
///
/// `filters` (predicate JSON, roles resolved, every column read) are pushed
/// into the scan; one that cannot be raises an error.
pub fn scan_edges_rows(
    source: ScanSource<'_>,
    src: &str,
    dst: &str,
    columns: &[String],
    filters: &[Value],
    drop_nulls: bool,
) -> Result<EdgeRows> {
    let (rows, pushed, _) =
        scan_edges_rows_with(source, src, dst, columns, filters, lower, drop_nulls)?;
    if pushed < filters.len() {
        return Err(DataFusionError::Execution(format!(
            "scan_edges: filter {pushed} reads a column the scan does not read"
        )));
    }
    Ok(rows)
}

/// [`scan_edges_rows`], pushing the longest prefix of `filters` whose columns
/// the read has, and returning how many it pushed. The caller runs the rest, so
/// a filter over a column the file lacks raises the caller's own error.
pub fn scan_edges_rows_pushing(
    source: ScanSource<'_>,
    src: &str,
    dst: &str,
    columns: &[String],
    filters: &[Value],
    drop_nulls: bool,
) -> Result<(EdgeRows, usize)> {
    let (rows, pushed, _) =
        scan_edges_rows_with(source, src, dst, columns, filters, lower, drop_nulls)?;
    Ok((rows, pushed))
}

/// A filter step as the expression a scan pushes, or `None` when it must run
/// after the read: it does not lower over the read's columns, or it compares
/// floats. Parquet min/max statistics leave NaN out, so a float comparison
/// would prune row groups whose NaN rows match.
fn lower(filter: &Value, schema: &DFSchema) -> Option<Expr> {
    let expr = crate::eval::logical(filter, schema).ok()?;
    (!crate::compare::compares_floats(&expr, schema)).then_some(expr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, OnceLock};

    use arrow::array::{Int64Array, RecordBatch};
    use arrow::datatypes::{DataType, Field, Schema};
    use datafusion::physical_plan::ExecutionPlan;

    /// A scan of one path, with no storage options and the default CSV options.
    fn one(path: &str) -> ScanSource<'static> {
        static OPTS: OnceLock<HashMap<String, String>> = OnceLock::new();
        static CSV: OnceLock<CsvOptions> = OnceLock::new();
        ScanSource::new(
            Box::leak(vec![path.to_string()].into_boxed_slice()),
            OPTS.get_or_init(HashMap::new),
            CSV.get_or_init(CsvOptions::default),
        )
    }

    /// Write `n` edges `(i, i + 1)` to a Parquet file with many small row groups.
    fn write_multi_row_group_parquet(path: &std::path::Path, n: i64) {
        use datafusion::parquet::arrow::ArrowWriter;
        use datafusion::parquet::file::properties::WriterProperties;
        let schema = Arc::new(Schema::new(vec![
            Field::new("from", DataType::Int64, false),
            Field::new("to", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from_iter_values(0..n)),
                Arc::new(Int64Array::from_iter_values(1..n + 1)),
            ],
        )
        .unwrap();
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(4_096))
            .build();
        let file = std::fs::File::create(path).unwrap();
        let mut w = ArrowWriter::try_new(file, schema, Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }

    /// The units a pruning metric pruned, summed over every node of a plan.
    fn metric(plan: &Arc<dyn ExecutionPlan>, name: &str) -> usize {
        use datafusion::physical_plan::metrics::MetricValue;
        let own: usize = plan
            .metrics()
            .map(|m| {
                m.iter()
                    .filter_map(|x| match x.value() {
                        MetricValue::PruningMetrics {
                            name: n,
                            pruning_metrics,
                        } if n.as_ref() == name => Some(pruning_metrics.pruned()),
                        _ => None,
                    })
                    .sum()
            })
            .unwrap_or(0);
        own + plan
            .children()
            .iter()
            .map(|c| metric(c, name))
            .sum::<usize>()
    }

    #[test]
    fn a_pushed_filter_skips_row_groups_and_keeps_file_order() {
        // 2M rows in 4,096-row groups, `from` ascending: `from >= 1_990_000` can
        // only hold in the last few groups, so the rest are pruned by statistics.
        let path = std::env::temp_dir().join("ursa_plan_scan_test_pushdown.parquet");
        write_multi_row_group_parquet(&path, 2_000_000);
        let p = path.to_str().unwrap();
        let pred = serde_json::json!({"kind": "binary", "op": ">=",
            "left": {"kind": "col", "name": "from"}, "right": {"kind": "lit", "value": 1_990_000}});
        let (rows, pushed, plan) =
            scan_edges_rows_with(one(p), "from", "to", &[], &[pred], lower, false).unwrap();
        assert_eq!(pushed, 1);
        let got: Vec<i64> = rows
            .batches
            .iter()
            .flat_map(|b| {
                b.column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .values()
                    .to_vec()
            })
            .collect();
        assert_eq!(got, (1_990_000..2_000_000).collect::<Vec<_>>());
        let pruned = metric(&plan, "row_groups_pruned_statistics");
        assert!(pruned >= 480, "pruned {pruned} of 489 row groups");

        // A filter that keeps nothing is an empty result, not an empty source.
        let none = serde_json::json!({"kind": "binary", "op": "<",
            "left": {"kind": "col", "name": "from"}, "right": {"kind": "lit", "value": 0}});
        let rows = scan_edges_rows(one(p), "from", "to", &[], &[none], false).unwrap();
        assert_eq!(rows.batches.len(), 1);
        assert_eq!(rows.batches[0].num_rows(), 0);
        assert_eq!(rows.batches[0].num_columns(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_float_comparison_is_not_pushed() {
        let path = std::env::temp_dir().join("ursa_plan_scan_test_float_filter.csv");
        std::fs::write(&path, "from,to,w\n1,2,0.5\n2,3,1.5\n").unwrap();
        let p = path.to_str().unwrap();
        let pred = serde_json::json!({"kind": "binary", "op": ">",
            "left": {"kind": "col", "name": "w"}, "right": {"kind": "lit", "value": 1.0}});
        let (rows, pushed) =
            scan_edges_rows_pushing(one(p), "from", "to", &[], &[pred], false).unwrap();
        assert_eq!(pushed, 0);
        assert_eq!(rows.batches.iter().map(|b| b.num_rows()).sum::<usize>(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_scan_error_reads_in_ursas_words() {
        let path = std::env::temp_dir().join("ursa_plan_scan_test_error_words.csv");
        std::fs::write(&path, "w,from,label,to\n0.5,1,a,2\n").unwrap();
        let p = path.to_str().unwrap();
        // A missing endpoint names the column; a non-id endpoint is rejected as
        // the graph build would reject it.
        let err = scan_edges_table(one(p), "from", "nope", &[]).unwrap_err();
        let msg = crate::message::engine_message(&err);
        assert!(
            msg.ends_with(
                "dst column 'nope' not found in the file; it has ['w', 'from', 'label', 'to']"
            ),
            "{msg}"
        );
        assert!(msg.starts_with("scan_edges('"), "{msg}");
        let err = scan_edges_table(one(p), "from", "w", &[]).unwrap_err();
        let msg = crate::message::engine_message(&err);
        assert!(
            msg.contains("node ids must be an integer or string column; got double"),
            "{msg}"
        );
        std::fs::remove_file(&path).ok();
    }
}
