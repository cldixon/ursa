//! Ursa SQL: DataFusion's dialect plus Ursa's own statements.
//!
//! [`parse_sql`] splits a string into [`UrsaStatement`]s. [`execute`] runs one
//! on an engine session (see `ursa_engine::new_session`). Statements DataFusion
//! knows are planned and run by DataFusion, except that one may not drop a
//! table a graph uses, or replace it by one the graph no longer fits
//! (`ursa_engine::graph::check_ddl`). `CREATE PROPERTY GRAPH` and `DROP
//! PROPERTY GRAPH` define graphs (see [`property_graph`] for the subset of
//! SQL/PGQ supported); `SHOW GRAPHS` and `DESCRIBE GRAPH` describe them.
//! `SHOW ursa.<setting>` shows one of Ursa's settings, and `SHOW ALL` lists
//! them with DataFusion's (see `settings`). `SHOW TABLES` lists the tables
//! and views, without `information_schema`'s own views. `read_parquet(...)`,
//! `read_csv(...)` and `read_json(...)` open their files before a statement
//! is planned (see `ursa_engine::read_functions`).
//!
//! A session may have a `.ursa` database ([`database`]): [`open_session`]
//! opens one, and `ATTACH 'club.ursa'` opens one in an in-memory session that
//! has defined nothing yet. In such a session, statements that define or drop
//! views, external tables and graphs are kept in the database's catalog file.
//! Relative file paths resolve against the current directory when a statement
//! runs, as in DuckDB; a stored definition's paths are rewritten relative to
//! the directory holding the file (see [`database`]).

use datafusion::dataframe::DataFrame;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;
use datafusion::sql::parser::Statement as DFStatement;
use datafusion::sql::sqlparser::ast::Statement as SqlStatement;

mod copy_options;
pub mod database;
mod duckdb;
mod from_first;
mod graph_ddl;
pub mod graph_table;
mod parser;
mod paths;
pub mod property_graph;
mod settings;
mod source_map;

pub use database::{open_database, open_session};
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

/// How `ursa_engine::graph::view` runs SQL text (a `.graph` source that is a
/// query, its `NODES (…)`, the algorithms of `SIZE BY pagerank`): Ursa's
/// whole dialect, every statement in turn, giving the rows of the last.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sql;

#[async_trait::async_trait]
impl ursa_engine::graph::view::RunSql for Sql {
    async fn run(
        &self,
        ctx: &SessionContext,
        text: &str,
    ) -> Result<ursa_engine::graph::view::Rows> {
        let mut last = None;
        for (statement, sql) in parse_sql_with_text_for(ctx, text)? {
            last = Some(execute_with_text(ctx, statement, &sql).await?);
        }
        let Some(frame) = last else {
            return Err(DataFusionError::Plan(
                "no query to draw; .graph SELECT src, dst FROM …".into(),
            ));
        };
        let schema = std::sync::Arc::new(frame.schema().as_arrow().clone());
        Ok((schema, frame.collect().await?))
    }
}

/// Run one statement, leaving the database's catalog alone.
pub(crate) async fn run(ctx: &SessionContext, statement: UrsaStatement) -> Result<DataFrame> {
    match statement {
        UrsaStatement::DataFusion(mut statement) => {
            let state = ctx.state();
            // A read-only session (`ursa serve --read-only`) copies nothing,
            // not even into its files in memory.
            if let DFStatement::CopyTo(copy) = statement.as_ref() {
                ursa_engine::paths::check_writable(state.config())
                    .map_err(|e| copy_refused(&copy.target, e))?;
            }
            // A COPY target or external table location in object storage needs
            // its store registered before DataFusion plans the write or read.
            // In a session that keeps its files in memory (the browser), a
            // COPY writes one of those (#517).
            let in_memory = ursa_engine::session_files::takes_copy(&state);
            if in_memory {
                session_copy_target(state.runtime_env(), &mut statement)?;
            }
            // Relative paths in a COPY target or an external table's
            // location, against the session's root.
            rooted_locations(state.config(), &mut statement, !in_memory)?;
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
            // `RESET ursa.<setting>`: DataFusion 55 resets only its own.
            if let DFStatement::Reset(datafusion::sql::parser::ResetStatement::Variable(name)) =
                statement.as_ref()
            {
                if let Some(frame) = settings::reset(ctx, name) {
                    return frame;
                }
            }
            // `SHOW ursa.<setting>` and `SHOW ALL` with Ursa's settings.
            if let DFStatement::Statement(inner) = statement.as_ref() {
                if let SqlStatement::ShowVariable { variable } = inner.as_ref() {
                    if let Some(frame) = settings::show(ctx, variable).await {
                        return frame;
                    }
                }
                // `SHOW TABLES` without `information_schema`'s own views.
                if is_plain_show_tables(inner) {
                    return ctx
                        .sql(
                            "SELECT * FROM information_schema.tables \
                             WHERE table_schema <> 'information_schema'",
                        )
                        .await;
                }
            }
            // DuckDB's `SET threads` and `CREATE TEMP TABLE` (#417).
            duckdb::accept(&mut statement)?;
            let written = statement.as_ref().clone();
            match plan_and_run(ctx, *statement).await {
                Ok(frame) => Ok(frame),
                // DuckDB's `ORDER BY ALL`, planned again as positions.
                Err(e) => match duckdb::retry(&written, &e) {
                    Some(rewritten) => plan_and_run(ctx, rewritten)
                        .await
                        .map_err(|e| duckdb::explain(ctx, &written, e)),
                    None => Err(duckdb::explain(ctx, &written, e)),
                },
            }
        }
        UrsaStatement::CreatePropertyGraph(statement) => graph_ddl::create(ctx, *statement).await,
        UrsaStatement::DropPropertyGraph(statement) => graph_ddl::drop(ctx, statement),
        UrsaStatement::ShowGraphs => graph_ddl::show(ctx),
        UrsaStatement::DescribeGraph(statement) => graph_ddl::describe(ctx, statement).await,
        UrsaStatement::SummarizeGraph(statement) => graph_ddl::summarize(ctx, statement).await,
        // `execute` handles ATTACH before it gets here.
        UrsaStatement::Attach(_) => Err(DataFusionError::Plan(
            "ATTACH cannot run while replaying a database's catalog".into(),
        )),
    }
}

/// Plan `statement` and run it.
async fn plan_and_run(ctx: &SessionContext, statement: DFStatement) -> Result<DataFrame> {
    let state = ctx.state();
    // `read_csv(...)` and its siblings open their files first: the
    // planner that plans them cannot wait on a listing.
    let reads = ursa_engine::read_functions::open_reads(&state, &statement).await?;
    let plan = state.statement_to_plan(statement).await?;
    drop(reads);
    // A column an inner join equates with a node key is one (#431).
    let plan = ursa_engine::graph::provenance::carry_through_joins(plan)?;
    ursa_engine::graph::check_ddl(&state, &plan).await?;
    if ursa_engine::paths::is_read_only(state.config()) {
        refuse_writes(&plan)?;
    }
    // A session that keeps its files in memory (the browser) writes them
    // itself, and cannot run DataFusion's file sinks (#517).
    if ursa_engine::session_files::takes_copy(&state) {
        if let datafusion::logical_expr::LogicalPlan::Copy(copy) = &plan {
            return ursa_engine::session_files::copy_to(ctx, copy).await;
        }
        ursa_engine::session_files::check_insert(&plan)?;
    } else {
        // A session with a root inserts only into files under it.
        check_inserts(state.config(), &plan)?;
    }
    ctx.execute_logical_plan(plan).await
}

/// Resolve the relative paths DataFusion would read against the working
/// directory: an external table's `LOCATION`s, against the session's root
/// ([`ursa_engine::paths::resolve`]), and a `COPY … TO` target (or the one an
/// `EXPLAIN` explains), which in a session with a directory root must be
/// under it ([`ursa_engine::paths::write_target`]). A session without a root
/// is left as it is. `copies_to_disk` is false in a session that keeps its
/// copies in memory, whose target [`session_copy_target`] has already made a
/// session file's URL.
fn rooted_locations(
    config: &datafusion::prelude::SessionConfig,
    statement: &mut DFStatement,
    copies_to_disk: bool,
) -> Result<()> {
    if ursa_engine::paths::root(config).is_none() {
        return Ok(());
    }
    match statement {
        DFStatement::CopyTo(copy) if copies_to_disk => {
            if let Some(target) = ursa_engine::paths::write_target(config, &copy.target)
                .map_err(|e| copy_refused(&copy.target, e))?
            {
                copy.target = target;
            }
        }
        DFStatement::CreateExternalTable(table) => {
            for location in &mut table.locations {
                *location = ursa_engine::paths::resolve(config, location)?;
            }
        }
        DFStatement::Explain(explain) => {
            rooted_locations(config, &mut explain.statement, copies_to_disk)?
        }
        _ => {}
    }
    Ok(())
}

/// A refusal of `COPY … TO 'target'`, saying which statement it is.
fn copy_refused(target: &str, e: DataFusionError) -> DataFusionError {
    match e {
        DataFusionError::Plan(why) => {
            DataFusionError::Plan(format!("COPY … TO '{target}' cannot be written: {why}"))
        }
        e => e,
    }
}

/// Refuse an `INSERT INTO` a table of files outside the session's root
/// ([`ursa_engine::paths::check_write`]).
fn check_inserts(
    config: &datafusion::prelude::SessionConfig,
    plan: &datafusion::logical_expr::LogicalPlan,
) -> Result<()> {
    use datafusion::common::tree_node::{TreeNode, TreeNodeRecursion};
    use datafusion::datasource::listing::ListingTable;
    use datafusion::datasource::source_as_provider;
    use datafusion::logical_expr::LogicalPlan;
    if !matches!(
        ursa_engine::paths::root(config),
        Some(ursa_engine::paths::Root::Dir(_))
    ) {
        return Ok(());
    }
    let mut refused = Ok(());
    plan.apply(|node| {
        if let LogicalPlan::Dml(dml) = node {
            if let Ok(table) = source_as_provider(&dml.target) {
                if let Some(listing) = table.downcast_ref::<ListingTable>() {
                    for path in listing.table_paths() {
                        if let Err(e) = ursa_engine::paths::check_write(config, path.get_url()) {
                            refused = Err(e);
                            return Ok(TreeNodeRecursion::Stop);
                        }
                    }
                }
            }
        }
        Ok(TreeNodeRecursion::Continue)
    })?;
    refused
}

/// Refuse what writes in a read-only session
/// (`ursa_engine::paths::set_read_only`): a `COPY … TO` anywhere in `plan`
/// (an `EXPLAIN`'s too), and an `INSERT` (or other change) to a table that
/// is not in memory, such as a table of files.
fn refuse_writes(plan: &datafusion::logical_expr::LogicalPlan) -> Result<()> {
    use datafusion::common::tree_node::{TreeNode, TreeNodeRecursion};
    use datafusion::datasource::{source_as_provider, MemTable};
    use datafusion::logical_expr::{LogicalPlan, WriteOp};
    let mut refused = Ok(());
    plan.apply(|node| {
        let why = match node {
            LogicalPlan::Copy(copy) => Some(copy_refused(
                &copy.output_url,
                DataFusionError::Plan(ursa_engine::paths::READ_ONLY.into()),
            )),
            LogicalPlan::Dml(dml) => {
                let in_memory = source_as_provider(&dml.target)
                    .is_ok_and(|table| table.downcast_ref::<MemTable>().is_some());
                (!in_memory).then(|| {
                    let op = match &dml.op {
                        WriteOp::Insert(_) => "INSERT INTO",
                        WriteOp::Delete => "DELETE FROM",
                        WriteOp::Update => "UPDATE",
                        WriteOp::Truncate => "TRUNCATE",
                        _ => "a write to",
                    };
                    DataFusionError::Plan(format!(
                        "{op} {} cannot change it: {}",
                        dml.table_name,
                        ursa_engine::paths::READ_ONLY
                    ))
                })
            }
            _ => None,
        };
        match why {
            Some(e) => {
                refused = Err(e);
                Ok(TreeNodeRecursion::Stop)
            }
            None => Ok(TreeNodeRecursion::Continue),
        }
    })?;
    refused
}

/// Point a `COPY … TO` (or the one an `EXPLAIN` explains) at the session
/// file its target names, or refuse a target that is none. `EXPLAIN ANALYZE`
/// of a `COPY` is refused: it would run DataFusion's file sink, which a
/// browser cannot (see `ursa_engine::session_files`).
fn session_copy_target(
    runtime: &datafusion::execution::runtime_env::RuntimeEnv,
    statement: &mut DFStatement,
) -> Result<()> {
    match statement {
        DFStatement::CopyTo(copy) => {
            let target = ursa_engine::session_files::copy_target(runtime, &copy.target)?;
            ursa_engine::session_files::check_copy_format(&copy.target, copy.stored_as.as_deref())?;
            copy.target = target;
        }
        DFStatement::Explain(explain) => {
            if explain.options.analyze && matches!(*explain.statement, DFStatement::CopyTo(_)) {
                return Err(DataFusionError::Plan(
                    "EXPLAIN ANALYZE of a COPY cannot run in a browser session; run the COPY, \
                     or EXPLAIN it"
                        .into(),
                ));
            }
            session_copy_target(runtime, &mut explain.statement)?;
        }
        _ => {}
    }
    Ok(())
}

/// Whether `statement` is `SHOW TABLES`, with no option DataFusion would
/// refuse or treat differently.
fn is_plain_show_tables(statement: &SqlStatement) -> bool {
    let SqlStatement::ShowTables {
        terse: false,
        history: false,
        extended: false,
        full: false,
        external: false,
        show_options,
    } = statement
    else {
        return false;
    };
    show_options.show_in.is_none()
        && show_options.starts_with.is_none()
        && show_options.limit.is_none()
        && show_options.limit_from.is_none()
        && show_options.filter_position.is_none()
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

    #[tokio::test]
    async fn show_tables_leaves_out_information_schema() {
        let ctx = ursa_engine::new_session();
        for sql in ["CREATE VIEW v AS SELECT 1 AS x", "SHOW TABLES"] {
            let statement = parse_sql_for(&ctx, sql).unwrap().remove(0);
            let batches = execute(&ctx, statement)
                .await
                .unwrap()
                .collect()
                .await
                .unwrap();
            if sql == "SHOW TABLES" {
                let formatted = datafusion::arrow::util::pretty::pretty_format_batches(&batches)
                    .unwrap()
                    .to_string();
                assert!(
                    formatted.contains("| public       | v          | VIEW"),
                    "{formatted}"
                );
                assert!(!formatted.contains("information_schema"), "{formatted}");
            }
        }
    }
}
