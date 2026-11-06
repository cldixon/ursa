//! `ursa_functions()`: the graph functions, as a table.
//!
//! One row per function SQL can call: every algorithm, statistic and
//! aggregation of `ursa_core::catalog` that SQL offers (not the layouts,
//! hidden until the visualization work; see [`functions`]), then the
//! traversals, then `algorithms`. The list columns are Arrow lists and
//! structs, not strings, so they stay usable both ways:
//!
//! - from SQL: `unnest(parameters)`, `array_has(directions, 'in')`,
//!   `parameters[1]['default']`;
//! - as JSON (`ursa --format json`), where they are real arrays and objects an agent
//!   reads without parsing a format of ours.
//!
//! Columns:
//!
//! | column       | type                                                     |
//! |--------------|----------------------------------------------------------|
//! | `name`       | the function's name                                      |
//! | `family`     | `algorithm`, `statistic`, `aggregation`, `traversal` or `combinator` |
//! | `summary`    | one line on what it computes                             |
//! | `networkx`   | the NetworkX call it matches, or NULL                    |
//! | `parameters` | list of {name, type, default, choices, doc}, call order  |
//! | `directions` | the `direction =>` values it accepts; empty if none     |
//! | `outputs`    | list of {name, type, nullable}: its columns besides `id` |
//!
//! A parameter's `type` is how SQL writes it (`number`, `number or NULL`,
//! `integer`, `integer or NULL`, `boolean`, `string`, `expression`, `query`
//! for `neighbors_agg`'s values, and for a traversal's node ids `node ids` or
//! `node id`), and its `default` is the
//! literal SQL would write (`0.85`, `'out'`, `NULL`), or `required`. A
//! traversal's node-id output columns have the graph's id type, `Int64 or
//! Utf8`.

use std::sync::Arc;

use datafusion::arrow::array::{
    ArrayRef, BooleanBuilder, ListBuilder, StringBuilder, StructBuilder,
};
use datafusion::arrow::datatypes::{DataType, Field, Fields, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::{TableFunctionArgs, TableFunctionImpl, TableProvider};
use datafusion::datasource::MemTable;
use datafusion::error::Result;
use ursa_core::catalog::Family;

use super::functions::{self, plan, sql_value, ALGORITHMS, URSA_FUNCTIONS};
use super::traversal::{OutputType, TraversalParamType, TRAVERSALS};

/// The `ursa_functions()` table function.
#[derive(Debug)]
pub struct UrsaFunctions;

impl TableFunctionImpl for UrsaFunctions {
    fn call_with_args(&self, args: TableFunctionArgs) -> Result<Arc<dyn TableProvider>> {
        if !args.exprs().is_empty() {
            return Err(plan(format!(
                "{URSA_FUNCTIONS}() takes no arguments: SELECT * FROM {URSA_FUNCTIONS}()"
            )));
        }
        let batch = functions_batch()?;
        Ok(Arc::new(MemTable::try_new(
            batch.schema(),
            vec![vec![batch]],
        )?))
    }
}

fn parameter_fields() -> Fields {
    Fields::from(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("default", DataType::Utf8, false),
        Field::new(
            "choices",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            false,
        ),
        Field::new("doc", DataType::Utf8, false),
    ])
}

fn output_fields() -> Fields {
    Fields::from(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("nullable", DataType::Boolean, false),
    ])
}

/// The schema of `ursa_functions()`.
pub fn schema() -> SchemaRef {
    let list = |item: DataType| DataType::List(Arc::new(Field::new("item", item, true)));
    Arc::new(Schema::new(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("family", DataType::Utf8, false),
        Field::new("summary", DataType::Utf8, false),
        Field::new("networkx", DataType::Utf8, true),
        Field::new(
            "parameters",
            list(DataType::Struct(parameter_fields())),
            false,
        ),
        Field::new("directions", list(DataType::Utf8), false),
        Field::new("outputs", list(DataType::Struct(output_fields())), false),
    ]))
}

fn family_name(family: Family) -> &'static str {
    match family {
        Family::Algorithm => "algorithm",
        Family::Layout => "layout",
        Family::Aggregation => "aggregation",
        Family::Statistic => "statistic",
    }
}

/// The summary of the `algorithms` row.
const ALGORITHMS_SUMMARY: &str = "Several per-node functions over one index, one row per node: \
     algorithms(g, degree(direction => 'both'), community => louvain(seed => 1)). Each argument \
     is a call with named parameters; its column is named by its key or after the function.";

/// The rows of `ursa_functions()`.
pub fn functions_batch() -> Result<RecordBatch> {
    let mut name = StringBuilder::new();
    let mut family = StringBuilder::new();
    let mut summary = StringBuilder::new();
    let mut networkx = StringBuilder::new();
    let mut parameters =
        ListBuilder::new(StructBuilder::from_fields(parameter_fields(), 0)).with_field(Arc::new(
            Field::new("item", DataType::Struct(parameter_fields()), true),
        ));
    let mut directions = ListBuilder::new(StringBuilder::new());
    let mut outputs = ListBuilder::new(StructBuilder::from_fields(output_fields(), 0)).with_field(
        Arc::new(Field::new("item", DataType::Struct(output_fields()), true)),
    );

    for spec in functions::specs() {
        name.append_value(spec.name);
        family.append_value(family_name(spec.family));
        summary.append_value(spec.summary);
        networkx.append_option(spec.networkx);
        let params = parameters.values();
        for p in functions::sql_params(spec) {
            params
                .field_builder::<StringBuilder>(0)
                .expect("name")
                .append_value(p.name);
            params
                .field_builder::<StringBuilder>(1)
                .expect("type")
                .append_value(functions::sql_type(p.name, p.ty));
            params
                .field_builder::<StringBuilder>(2)
                .expect("default")
                .append_value(sql_value(&p.default));
            let choices = params
                .field_builder::<ListBuilder<Box<dyn datafusion::arrow::array::ArrayBuilder>>>(3)
                .expect("choices");
            for c in p.ty.choices() {
                choices
                    .values()
                    .as_any_mut()
                    .downcast_mut::<StringBuilder>()
                    .expect("choices are strings")
                    .append_value(c);
            }
            choices.append(true);
            params
                .field_builder::<StringBuilder>(4)
                .expect("doc")
                .append_value(functions::sql_doc(p));
            params.append(true);
        }
        parameters.append(true);
        for d in spec.directions() {
            directions.values().append_value(d);
        }
        directions.append(true);
        let outs = outputs.values();
        for o in functions::outputs(spec) {
            outs.field_builder::<StringBuilder>(0)
                .expect("name")
                .append_value(o.name);
            outs.field_builder::<StringBuilder>(1)
                .expect("type")
                .append_value(o.ty.data_type().to_string());
            outs.field_builder::<BooleanBuilder>(2)
                .expect("nullable")
                .append_value(o.nullable);
            outs.append(true);
        }
        outputs.append(true);
    }

    for spec in TRAVERSALS {
        name.append_value(spec.name);
        family.append_value("traversal");
        summary.append_value(spec.summary);
        networkx.append_option(spec.networkx);
        let params = parameters.values();
        for p in spec.params {
            let (ty, choices): (&str, &[&str]) = match &p.ty {
                TraversalParamType::Nodes => ("node ids", &[]),
                TraversalParamType::Node => ("node id", &[]),
                TraversalParamType::Param(ty) => (functions::sql_type(p.name, *ty), ty.choices()),
            };
            params
                .field_builder::<StringBuilder>(0)
                .expect("name")
                .append_value(p.name);
            params
                .field_builder::<StringBuilder>(1)
                .expect("type")
                .append_value(ty);
            params
                .field_builder::<StringBuilder>(2)
                .expect("default")
                .append_value(p.default.as_ref().map_or("required".to_string(), sql_value));
            let choice_list = params
                .field_builder::<ListBuilder<Box<dyn datafusion::arrow::array::ArrayBuilder>>>(3)
                .expect("choices");
            for c in choices {
                choice_list
                    .values()
                    .as_any_mut()
                    .downcast_mut::<StringBuilder>()
                    .expect("choices are strings")
                    .append_value(c);
            }
            choice_list.append(true);
            params
                .field_builder::<StringBuilder>(4)
                .expect("doc")
                .append_value(p.doc);
            params.append(true);
        }
        parameters.append(true);
        for d in spec.directions() {
            directions.values().append_value(d);
        }
        directions.append(true);
        let outs = outputs.values();
        for o in spec.outputs {
            let ty = match o.ty {
                OutputType::Id => "Int64 or Utf8".to_string(),
                other => other.data_type(&DataType::Null).to_string(),
            };
            outs.field_builder::<StringBuilder>(0)
                .expect("name")
                .append_value(o.name);
            outs.field_builder::<StringBuilder>(1)
                .expect("type")
                .append_value(ty);
            outs.field_builder::<BooleanBuilder>(2)
                .expect("nullable")
                .append_value(false);
            outs.append(true);
        }
        outputs.append(true);
    }

    name.append_value(ALGORITHMS);
    family.append_value("combinator");
    summary.append_value(ALGORITHMS_SUMMARY);
    networkx.append_null();
    parameters.append(true);
    directions.append(true);
    outputs.append(true);

    let columns: Vec<ArrayRef> = vec![
        Arc::new(name.finish()),
        Arc::new(family.finish()),
        Arc::new(summary.finish()),
        Arc::new(networkx.finish()),
        Arc::new(parameters.finish()),
        Arc::new(directions.finish()),
        Arc::new(outputs.finish()),
    ];
    Ok(RecordBatch::try_new(schema(), columns)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_row_per_function() {
        let batch = functions_batch().unwrap();
        let expected = functions::names().count();
        assert_eq!(batch.num_rows(), expected);
        assert_eq!(batch.schema(), schema());
    }
}
