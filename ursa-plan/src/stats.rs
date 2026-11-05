//! Whole-graph statistics as a plan runs them: one-row frames.
//!
//! The statistics themselves (`density`, `avg_path_length`, `diameter`,
//! `describe`) are in the catalog in `ursa-core`
//! ([`ursa_core::catalog::stats`]), with their parameters and output columns.
//! This module holds the plan's form of them ([`GraphStat`]), the Python API's
//! argument checks, and the batches a plan collects ([`stat_batch`],
//! [`describe_masked`]).

use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::{DataFusionError, Result};
use serde::{Deserialize, Serialize};
use ursa_core::catalog::{self, Call, Evaluator, Inputs};
use ursa_core::{Direction, EdgeMask, Topology};

/// Parse the `direction` argument (`"out"`, `"in"` or `"both"`) of the stat `stat`.
pub fn parse_stat_direction(stat: &str, d: &str) -> Result<Direction> {
    crate::query::parse_direction(stat, d)
}

/// A scalar whole-graph stat, as a plan names it: `ur.density`,
/// `ur.avg_path_length` and `ur.diameter`, each a one-row frame of one column
/// named after the stat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stat", rename_all = "snake_case")]
pub enum GraphStat {
    Density,
    AvgPathLength {
        #[serde(default)]
        sample: Option<f64>,
    },
    Diameter {
        approximate: bool,
    },
}

impl GraphStat {
    /// The catalog call this stat makes, read in `direction`.
    pub fn call(&self, direction: Direction) -> Call {
        match *self {
            GraphStat::Density => Call::Density { direction },
            GraphStat::AvgPathLength { sample } => Call::AvgPathLength { sample, direction },
            GraphStat::Diameter { approximate } => Call::Diameter {
                approximate,
                direction,
            },
        }
    }

    /// The stat's name: its function's, and its column's.
    pub fn name(&self) -> &'static str {
        self.call(Direction::Out).name()
    }

    /// The type of the stat's column.
    pub fn data_type(&self) -> DataType {
        self.call(Direction::Out).spec().outputs[0].ty.data_type()
    }
}

/// Parse `describe(nodes=)`: whether it describes only the touched nodes.
/// `shown` is the argument as the caller wrote it, for the error; `None` for
/// `nodes` is an argument that is not a string.
pub fn parse_describe_nodes(nodes: Option<&str>, shown: &str) -> crate::PlanResult<bool> {
    let accepted = catalog::lookup("describe")
        .and_then(|s| s.param("nodes"))
        .map_or(&[][..], |p| p.ty.choices());
    match nodes {
        Some(n) if accepted.contains(&n) => Ok(n == "touched"),
        _ => Err(crate::PlanError::Value(format!(
            "describe(nodes={shown}) is not supported; use 'all' or 'touched'."
        ))),
    }
}

/// The one-row frame of a statistic `call` over a subgraph view: one column per
/// output the catalog declares.
fn one_row(topology: &Topology, mask: Option<&EdgeMask>, call: &Call) -> Result<RecordBatch> {
    let arrays = Evaluator::new(topology, mask)
        .evaluate(call, Inputs::default())
        .map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let fields: Vec<Field> = call
        .spec()
        .outputs
        .iter()
        .map(|o| Field::new(o.name, o.ty.data_type(), o.nullable))
        .collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays.to_vec())
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}

/// `describe` over a subgraph view (#207): the parent's node set (or, with
/// `touched`, only the nodes at an end of a kept edge, #213) and the edge rows
/// `mask` keeps. `None` is the whole graph. See [`catalog::stats::describe`].
pub fn describe_masked(
    topology: &Topology,
    mask: Option<&EdgeMask>,
    full: bool,
    dir: Direction,
    touched: bool,
) -> Result<RecordBatch> {
    let call = Call::Describe {
        full,
        direction: dir,
        touched,
    };
    one_row(topology, mask, &call)
}

/// A scalar stat over a subgraph view, as its one-row frame: one non-null
/// column named after the stat.
pub fn stat_batch(
    topology: &Topology,
    mask: Option<&EdgeMask>,
    stat: &GraphStat,
    dir: Direction,
) -> Result<RecordBatch> {
    one_row(topology, mask, &stat.call(dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::build_topology;
    use arrow::array::{Float64Array, Int64Array};

    /// Build a topology from edge endpoints for the tests.
    fn topo(src: &[i64], dst: &[i64]) -> Arc<Topology> {
        let (t, _ids) = build_topology(
            &Int64Array::from(src.to_vec()),
            &Int64Array::from(dst.to_vec()),
        )
        .unwrap();
        t
    }

    #[test]
    fn describe_is_one_row_with_a_nullable_component_count() {
        let t = topo(&[0, 1, 2], &[1, 2, 0]);
        let b = describe_masked(&t, None, true, Direction::Out, false).unwrap();
        assert_eq!(b.num_rows(), 1);
        let schema = b.schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(
            names,
            [
                "n_nodes",
                "n_edges",
                "density",
                "avg_degree",
                "n_components"
            ]
        );
        let nullable: Vec<bool> = schema.fields().iter().map(|f| f.is_nullable()).collect();
        assert_eq!(nullable, [false, false, false, false, true]);
        let i = |name: &str| {
            b.column(b.schema().index_of(name).unwrap())
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(0)
        };
        let density = b
            .column(2)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap()
            .value(0);
        assert_eq!((i("n_nodes"), i("n_edges"), i("n_components")), (3, 3, 1));
        assert_eq!(density, 0.5);
        // n_components is null unless full=true.
        let b = describe_masked(&t, None, false, Direction::Out, false).unwrap();
        assert!(b.column(4).is_null(0));
    }

    #[test]
    fn stat_batch_is_one_non_null_column() {
        let t = topo(&[0, 1, 2], &[1, 2, 0]);
        for (stat, ty) in [
            (GraphStat::Density, DataType::Float64),
            (GraphStat::AvgPathLength { sample: None }, DataType::Float64),
            (GraphStat::Diameter { approximate: false }, DataType::Int64),
        ] {
            let b = stat_batch(&t, None, &stat, Direction::Out).unwrap();
            assert_eq!(b.num_rows(), 1);
            let field = b.schema().field(0).clone();
            assert_eq!(
                (field.name().as_str(), field.data_type()),
                (stat.name(), &ty)
            );
            assert!(!field.is_nullable());
        }
    }

    #[test]
    fn describe_nodes_parses() {
        assert!(!parse_describe_nodes(Some("all"), "'all'").unwrap());
        assert!(parse_describe_nodes(Some("touched"), "'touched'").unwrap());
        let e = parse_describe_nodes(None, "1").unwrap_err();
        assert!(e.to_string().contains("describe(nodes=1) is not supported"));
        let e = parse_describe_nodes(Some("x"), "'x'").unwrap_err();
        assert!(e
            .to_string()
            .contains("describe(nodes='x') is not supported"));
    }

    #[test]
    fn stat_direction_parses() {
        assert_eq!(
            parse_stat_direction("density", "both").unwrap(),
            Direction::Both
        );
        assert!(parse_stat_direction("density", "sideways").is_err());
    }
}
