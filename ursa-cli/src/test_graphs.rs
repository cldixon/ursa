//! A test-only way to define graphs until `CREATE PROPERTY GRAPH` lands
//! (cldixon/ursa#235, in a sibling PR). Remove it then.
//!
//! `URSA_TEST_GRAPHS="karate=ties(src,dst);g=e(a,b)"` registers each graph
//! over its edge table and endpoint columns when the session starts. It is
//! not documented to users and is not part of the CLI's interface.

use datafusion::common::TableReference;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;
use ursa_engine::graph::{registry_of, EdgeTable, GraphDef};

pub const VARIABLE: &str = "URSA_TEST_GRAPHS";

/// Register the graphs `URSA_TEST_GRAPHS` names, if it is set.
pub fn register(ctx: &SessionContext) -> Result<()> {
    let Ok(spec) = std::env::var(VARIABLE) else {
        return Ok(());
    };
    for def in spec.split(';').filter(|d| !d.trim().is_empty()) {
        registry_of(ctx).put(parse(def.trim()).ok_or_else(|| {
            DataFusionError::Configuration(format!(
                "{VARIABLE}: expected name=table(source,destination); got '{def}'"
            ))
        })?);
    }
    Ok(())
}

fn parse(def: &str) -> Option<GraphDef> {
    let (name, rest) = def.split_once('=')?;
    let (table, rest) = rest.split_once('(')?;
    let (source, destination) = rest.strip_suffix(')')?.split_once(',')?;
    Some(GraphDef {
        name: name.trim().to_string(),
        edges: EdgeTable {
            table: TableReference::bare(table.trim()),
            source: source.trim().to_string(),
            destination: destination.trim().to_string(),
        },
        vertices: None,
    })
}
