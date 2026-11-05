//! Ursa SQL: DataFusion's dialect plus Ursa's own statements.
//!
//! [`parse_sql`] splits a string into [`UrsaStatement`]s. [`execute`] runs one
//! on an engine session (see `ursa_engine::new_session`). Statements DataFusion
//! knows are planned and run by DataFusion, except that one may not drop a
//! table a graph uses, or replace it by one the graph no longer fits
//! (`ursa_engine::graph::check_ddl`). `CREATE PROPERTY GRAPH` and `DROP
//! PROPERTY GRAPH` define graphs (see [`property_graph`] for the subset of
//! SQL/PGQ supported); `SHOW GRAPHS` and `DESCRIBE GRAPH` describe them.
//! `ATTACH` is recognised now and runs in a later milestone; until then it
//! returns a "not supported yet" error naming the issue that delivers it.

use datafusion::dataframe::DataFrame;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;
use datafusion::sql::parser::Statement as DFStatement;

mod graph_ddl;
mod parser;
pub mod property_graph;

pub use parser::{
    parse_sql, Attach, CreatePropertyGraph, DescribeGraph, DropPropertyGraph, UrsaParser,
    UrsaStatement, DEFAULT_RECURSION_LIMIT,
};

/// Parse `sql` with the session's configured dialect and recursion limit.
pub fn parse_sql_for(ctx: &SessionContext, sql: &str) -> Result<Vec<UrsaStatement>> {
    let options = ctx.state().config().options().sql_parser.clone();
    let dialect = datafusion::sql::sqlparser::dialect::dialect_from_str(options.dialect)
        .ok_or_else(|| {
            DataFusionError::Configuration(format!("unknown SQL dialect '{}'", options.dialect))
        })?;
    UrsaParser::new(sql, dialect.as_ref(), options.recursion_limit.get())?.parse_statements()
}

/// Run one statement on `ctx`. The returned frame is lazy for a query, and
/// already applied for DDL (as with `SessionContext::sql`).
pub async fn execute(ctx: &SessionContext, statement: UrsaStatement) -> Result<DataFrame> {
    match statement {
        UrsaStatement::DataFusion(statement) => {
            let state = ctx.state();
            // A COPY target or external table location in object storage needs
            // its store registered before DataFusion plans the write or read.
            match statement.as_ref() {
                DFStatement::CopyTo(copy) => {
                    ursa_engine::ensure_object_store_for(state.runtime_env(), &copy.target).await?
                }
                DFStatement::CreateExternalTable(table) => {
                    for location in &table.locations {
                        ursa_engine::ensure_object_store_for(state.runtime_env(), location).await?
                    }
                }
                _ => {}
            }
            let plan = state.statement_to_plan(*statement).await?;
            ursa_engine::graph::check_ddl(&state, &plan).await?;
            ctx.execute_logical_plan(plan).await
        }
        UrsaStatement::CreatePropertyGraph(statement) => graph_ddl::create(ctx, *statement).await,
        UrsaStatement::DropPropertyGraph(statement) => graph_ddl::drop(ctx, statement),
        UrsaStatement::ShowGraphs => graph_ddl::show(ctx),
        UrsaStatement::DescribeGraph(statement) => graph_ddl::describe(ctx, statement).await,
        other => Err(not_supported_yet(&other)),
    }
}

/// The error for a recognised statement whose milestone has not landed.
fn not_supported_yet(statement: &UrsaStatement) -> DataFusionError {
    let planned = match statement {
        UrsaStatement::Attach(_) => "#237",
        UrsaStatement::DataFusion(_)
        | UrsaStatement::CreatePropertyGraph(_)
        | UrsaStatement::DropPropertyGraph(_)
        | UrsaStatement::DescribeGraph(_)
        | UrsaStatement::ShowGraphs => unreachable!("{} is supported", statement.kind()),
    };
    DataFusionError::NotImplemented(format!(
        "{} is not supported yet (planned in {planned})",
        statement.kind()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn graph_statements_say_when_they_arrive() {
        let ctx = ursa_engine::new_session();
        for (sql, expected) in [(
            "ATTACH 'club.ursa'",
            "ATTACH is not supported yet (planned in #237)",
        )] {
            let statement = parse_sql_for(&ctx, sql).unwrap().remove(0);
            let err = execute(&ctx, statement).await.unwrap_err();
            assert_eq!(ursa_engine::user_message(&err), expected);
        }
    }

    #[tokio::test]
    async fn runs_datafusion_statements() {
        let ctx = ursa_engine::new_session();
        let mut statements = parse_sql_for(
            &ctx,
            "CREATE VIEW v AS SELECT 1 AS x UNION ALL SELECT 2; SELECT sum(x) AS s FROM v",
        )
        .unwrap();
        let select = statements.pop().unwrap();
        execute(&ctx, statements.pop().unwrap()).await.unwrap();
        let batches = execute(&ctx, select)
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let formatted = datafusion::arrow::util::pretty::pretty_format_batches(&batches)
            .unwrap()
            .to_string();
        assert!(formatted.contains("| 3 |"), "{formatted}");
    }
}
