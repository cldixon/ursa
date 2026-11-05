//! Ursa SQL: DataFusion's dialect plus Ursa's own statements.
//!
//! [`parse_sql`] splits a string into [`UrsaStatement`]s. [`execute`] runs one
//! on an engine session (see `ursa_engine::new_session`). Statements DataFusion
//! knows are planned and run by DataFusion, except that one may not drop a
//! table a graph uses, or replace it by one the graph no longer fits
//! (`ursa_engine::graph::check_ddl`). `CREATE PROPERTY GRAPH` and `DROP
//! PROPERTY GRAPH` define graphs (see [`property_graph`] for the subset of
//! SQL/PGQ supported); `SHOW GRAPHS` and `DESCRIBE GRAPH` describe them.
//!
//! A session may have a `.ursa` database ([`database`]): [`open_session`]
//! opens one, and `ATTACH 'club.ursa'` opens one in an in-memory session that
//! has defined nothing yet. In such a session, statements that define or drop
//! views, external tables and graphs are kept in the database's catalog file,
//! and relative file paths are relative to the directory holding the file.

use datafusion::dataframe::DataFrame;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;
use datafusion::sql::parser::Statement as DFStatement;

pub mod database;
mod graph_ddl;
mod parser;
pub mod property_graph;

pub use database::open_session;
pub use parser::{
    parse_sql, Attach, CreatePropertyGraph, DescribeGraph, DropPropertyGraph, UrsaParser,
    UrsaStatement, DEFAULT_RECURSION_LIMIT,
};
pub use ursa_engine::database::OpenOptions;

/// Parse `sql` with the session's configured dialect and recursion limit.
pub fn parse_sql_for(ctx: &SessionContext, sql: &str) -> Result<Vec<UrsaStatement>> {
    let options = ctx.state().config().options().sql_parser.clone();
    let dialect = datafusion::sql::sqlparser::dialect::dialect_from_str(options.dialect)
        .ok_or_else(|| {
            DataFusionError::Configuration(format!("unknown SQL dialect '{}'", options.dialect))
        })?;
    UrsaParser::new(sql, dialect.as_ref(), options.recursion_limit.get())?.parse_statements()
}

/// [`parse_sql_for`], with each statement's text as written (see
/// [`execute_with_text`]).
pub fn parse_sql_with_text_for(
    ctx: &SessionContext,
    sql: &str,
) -> Result<Vec<(UrsaStatement, String)>> {
    let options = ctx.state().config().options().sql_parser.clone();
    let dialect = datafusion::sql::sqlparser::dialect::dialect_from_str(options.dialect)
        .ok_or_else(|| {
            DataFusionError::Configuration(format!("unknown SQL dialect '{}'", options.dialect))
        })?;
    UrsaParser::new(sql, dialect.as_ref(), options.recursion_limit.get())?
        .parse_statements_with_text()
}

/// Run one statement on `ctx`. The returned frame is lazy for a query, and
/// already applied for DDL (as with `SessionContext::sql`).
///
/// In a session with a database, a statement that defines an object is
/// stored as its `Display` form; one whose `Display` does not parse back to
/// the same statement (`CREATE EXTERNAL TABLE` with columns or options) needs
/// [`execute_with_text`].
pub async fn execute(ctx: &SessionContext, statement: UrsaStatement) -> Result<DataFrame> {
    execute_inner(ctx, statement, None).await
}

/// [`execute`], with the statement's text as written (from
/// [`parse_sql_with_text_for`]), which a database stores when the statement's
/// `Display` does not round-trip.
pub async fn execute_with_text(
    ctx: &SessionContext,
    statement: UrsaStatement,
    text: &str,
) -> Result<DataFrame> {
    execute_inner(ctx, statement, Some(text)).await
}

async fn execute_inner(
    ctx: &SessionContext,
    statement: UrsaStatement,
    text: Option<&str>,
) -> Result<DataFrame> {
    match ursa_engine::database::database(&ctx.state()) {
        Some(db) => database::execute(ctx, &db, statement, text).await,
        None => match statement {
            UrsaStatement::Attach(attach) => database::attach(ctx, attach).await,
            statement => run(ctx, statement).await,
        },
    }
}

/// Run one statement, leaving the database's catalog alone.
pub(crate) async fn run(ctx: &SessionContext, statement: UrsaStatement) -> Result<DataFrame> {
    match statement {
        UrsaStatement::DataFusion(mut statement) => {
            let state = ctx.state();
            // In a session with a database, a relative path is relative to
            // the database's directory.
            match statement.as_mut() {
                DFStatement::CopyTo(copy) => {
                    copy.target = ursa_engine::database::resolve_location(&state, &copy.target)
                }
                DFStatement::CreateExternalTable(table) => {
                    for location in &mut table.locations {
                        *location = ursa_engine::database::resolve_location(&state, location);
                    }
                }
                _ => {}
            }
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
        // `execute` handles ATTACH before it gets here.
        UrsaStatement::Attach(_) => Err(DataFusionError::Plan(
            "ATTACH cannot run while replaying a database's catalog".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
