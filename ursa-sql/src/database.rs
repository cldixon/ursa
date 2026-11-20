//! Statements in a session with a `.ursa` database.
//!
//! The engine's [`Database`] keeps the catalog file: the statements that
//! defined each object, in replay order. This module decides what a statement
//! does to that list, and replays it when a database is opened.
//!
//! - `CREATE VIEW`, `CREATE EXTERNAL TABLE` and `CREATE PROPERTY GRAPH` add
//!   their statement once they succeed. An object already in the catalog
//!   (`CREATE OR REPLACE`) has its statement replaced in place, except that
//!   with `IF NOT EXISTS` it keeps its statement.
//! - `DROP VIEW`, `DROP TABLE` and `DROP PROPERTY GRAPH` remove the object's
//!   statement.
//! - Tables with data (`CREATE TABLE`, `INSERT` and other DML) are refused:
//!   version 1 of the file does not store rows. So is any other DDL
//!   (`CREATE SCHEMA`, `CREATE FUNCTION`, `ALTER …`), which the file could not
//!   replay.
//! - `SET` and `RESET` last for the session, as in DuckDB and Postgres, in
//!   any database, writable or not: nothing is written to the catalog (#353).
//!   To keep a setting, put the `SET` in `~/.ursarc` or an `-init` file.
//!   `SET ursa.index_storage_limit` takes effect for the stored indexes at
//!   once, and never removes one: if they already take more than the new
//!   limit, it says so (a notice), and no new index is stored.
//! - Ursa 0.8 and 0.9 kept `SET ursa.index_storage_limit` in a writable
//!   database's catalog (#327). Such a saved setting is no longer applied:
//!   opening the database says so (a notice), and a writable open removes it
//!   from the catalog, so it is said once.
//! - A stored statement that fails when the database is opened (a view over
//!   a file that is gone) does not stop it opening (#381): its object is
//!   unavailable, with one notice, and fails only when used; `DROP` and
//!   `CREATE OR REPLACE` repair it, and its statement is kept as it is.
//!   See `replay`.
//! - `TEMPORARY` views and external tables run and are not stored.
//! - In a read-only session, every statement that defines or drops anything
//!   is refused.
//!
//! A statement is stored as its `Display` form when that parses back to the
//! same statement, and otherwise as the text the user wrote (DataFusion's
//! `Display` for `CREATE EXTERNAL TABLE` leaves out columns and options).
//!
//! **Paths.** A relative path resolves against the current directory when a
//! statement runs, as in DuckDB. Before a definition is stored, each local
//! file path in it (see [`crate::paths`]) is rewritten relative to the
//! database's directory, or made absolute when it is outside that directory;
//! URLs are kept. In the stored-as-written case the paths are replaced in the
//! text token by token, and the statement is refused if the result does not
//! parse back to the rewritten statement. Replay resolves each stored relative
//! path against the database's directory, so the catalog finds the same files
//! from any directory, and a database moved with its data still does. (M3a
//! stored paths as written and resolved them against the database's
//! directory, so its files replay unchanged.)
//!
//! Views and tables share one namespace, keyed by their name resolved against
//! the default catalog and schema; graphs have their own.

use std::path::Path;
use std::sync::Arc;

use datafusion::common::TableReference;
use datafusion::dataframe::DataFrame;
use datafusion::datasource::TableType;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;
use datafusion::sql::parser::{ResetStatement, Statement as DFStatement};
use datafusion::sql::planner::object_name_to_table_reference;
use datafusion::sql::sqlparser::ast::{ObjectName, ObjectType, Set, Statement as SQLStatement};
use ursa_engine::database::{self as db, Database, OpenOptions, UnavailableTable};
use ursa_engine::graph;
use ursa_engine::notices::{self, NoticeLevel, Notifier};
use ursa_engine::options::ByteSize;
use ursa_engine::paths::looks_like_location;

use crate::parser::{Attach, UrsaStatement};
use crate::{graph_ddl, parse_sql_for, paths, run};

/// What a statement does to the catalog.
#[derive(Debug)]
enum Effect {
    /// Nothing: a query, `COPY`, `SET`, `SHOW`, `EXPLAIN`, `DESCRIBE`.
    None,
    /// Defines an object (added to, or replacing, its catalog entry).
    Define { key: Key, if_not_exists: bool },
    /// Drops objects (removing their catalog entries).
    Drop { keys: Vec<Key> },
    /// Defines or drops a session-only object (`TEMPORARY`): not stored.
    Session,
    /// Not allowed in a session with a database: the message says why.
    Refused(String),
}

/// An object in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    /// A view or table, by its fully resolved name.
    Table(String),
    /// A graph, by name.
    Graph(String),
}

/// Open a new session on the database at `path`: open the file (creating it
/// unless read-only), then replay its catalog.
pub async fn open_session(path: impl AsRef<Path>, options: &OpenOptions) -> Result<SessionContext> {
    let ctx = ursa_engine::new_session();
    open_into(&ctx, path.as_ref(), options).await?;
    Ok(ctx)
}

/// Open the database at `path` for `ctx`, a session without one, and replay
/// its catalog. Notices (a setting Ursa 0.8 or 0.9 saved) go to `ctx`'s sink,
/// so a host that installs one first hears them.
pub async fn open_database(
    ctx: &SessionContext,
    path: impl AsRef<Path>,
    options: &OpenOptions,
) -> Result<()> {
    open_into(ctx, path.as_ref(), options).await
}

/// Open the database at `path` for `ctx` and replay its catalog.
async fn open_into(ctx: &SessionContext, path: &Path, options: &OpenOptions) -> Result<()> {
    let path = path.to_path_buf();
    let options = options.clone();
    // Opening may wait for another process's lock.
    let database = ursa_engine::run_blocking(move || Database::open(path, &options))
        .await
        .map_err(|e| DataFusionError::External(Box::new(e)))??;
    let database = Arc::new(database);
    db::slot(&ctx.state()).set(Arc::clone(&database))?;
    replay(ctx, &database).await?;
    retire_saved_settings(ctx, &database);
    Ok(())
}

/// Say that the settings Ursa 0.8 or 0.9 saved in the catalog are no longer
/// applied (#353), and remove them from a writable database, so it is said
/// once. Best effort: a catalog that cannot be rewritten says it again next
/// time.
fn retire_saved_settings(ctx: &SessionContext, database: &Database) {
    let saved = database.saved_settings();
    if saved.is_empty() {
        return;
    }
    let notifier = Notifier::of(ctx.state().config());
    for (name, value) in &saved {
        notifier.say(NoticeLevel::On, || {
            format!(
                "{} has a saved {name} = {value}, which is no longer applied; SET now lasts \
                 for the session. To keep it, add SET {name} = '{value}' to ~/.ursarc (or an \
                 -init file)",
                database.name()
            )
        });
    }
    if !database.is_read_only() {
        let _ = database.remove_saved_settings();
    }
}

/// Whether `statement` is `SET ursa.index_storage_limit = …` or `RESET
/// ursa.index_storage_limit`.
fn sets_index_storage_limit(statement: &UrsaStatement) -> bool {
    let UrsaStatement::DataFusion(statement) = statement else {
        return false;
    };
    let name = match statement.as_ref() {
        DFStatement::Statement(s) => match s.as_ref() {
            SQLStatement::Set(Set::SingleAssignment { variable, .. }) => variable,
            _ => return false,
        },
        DFStatement::Reset(ResetStatement::Variable(variable)) => variable,
        _ => return false,
    };
    name.to_string()
        .eq_ignore_ascii_case(db::INDEX_STORAGE_LIMIT)
}

/// Run `SET` or `RESET` of `ursa.index_storage_limit`, for the session, and
/// have the database's stored indexes obey it at once. A lower limit removes
/// nothing: if the indexes stored take more than it, say so (they are kept,
/// and no new index is stored).
async fn run_index_storage_limit(
    ctx: &SessionContext,
    database: &Database,
    statement: UrsaStatement,
) -> Result<DataFrame> {
    let frame = run(ctx, statement).await?;
    let state = ctx.state();
    let config = state.config();
    database.use_index_storage_limit(config);
    if !database.is_read_only() {
        let limit = database.index_storage_limit(config);
        let stored = database.index_store().stored_bytes();
        if stored > limit {
            Notifier::of(config).say(NoticeLevel::On, || {
                format!(
                    "{} of indexes are stored in {}, more than the {} limit; they are kept, and \
                     new indexes won't be stored",
                    notices::size(stored),
                    database.name(),
                    ByteSize(limit)
                )
            });
        }
    }
    Ok(frame)
}

/// Run the catalog's statements on `ctx`, in order. A statement that fails is
/// retried after the others, in case an edit of the file put it before what it
/// depends on.
///
/// A statement that still fails does not stop the database opening (#381):
/// the object it defines is unavailable
/// ([`ursa_engine::database::unavailable`]), and one notice says why. So is
/// each object whose statement fails because it uses an unavailable one, with
/// its own notice. The statements stay in the catalog as they are, so
/// restoring what they read and opening the database again brings the objects
/// back.
async fn replay(ctx: &SessionContext, database: &Database) -> Result<()> {
    let mut pending: Vec<(usize, String)> = database.statements().into_iter().enumerate().collect();
    let mut unavailable: Vec<Unavailable> = Vec::new();
    // Statements that do not define an object (an edited file): kept, skipped.
    let mut skipped: Vec<(usize, String)> = Vec::new();
    while !pending.is_empty() {
        let before = pending.len();
        let mut failed = Vec::new();
        for (index, sql) in pending {
            if let Err(e) = replay_one(ctx, database, &sql).await {
                failed.push((index, sql, ursa_engine::user_message(&e)));
            }
        }
        // A statement that failed for using an unavailable object is
        // unavailable too.
        let mut still = Vec::new();
        for (index, sql, cause) in failed {
            let uses = unavailable
                .iter()
                .find(|u| cause.contains(&u.message))
                .map(|u| (u.kind, u.name.clone()));
            match (uses, defined_object(ctx, &sql)) {
                (Some((kind, name)), Some(object)) => {
                    let message = format!(
                        "{} {} is unavailable: it uses {kind} {name}, which is unavailable; \
                         repair {name}, or {}",
                        object.kind, object.name, object.drop
                    );
                    unavailable.push(object.mark(ctx, index, message));
                }
                _ => still.push((index, sql, cause)),
            }
        }
        if !still.is_empty() && still.len() == before {
            // No progress: the first that still fails is unavailable in its
            // own right.
            let (index, sql, cause) = still.remove(0);
            match defined_object(ctx, &sql) {
                Some(object) => {
                    let message = format!(
                        "{} {} is unavailable: {cause}; {} or restore the file",
                        object.kind, object.name, object.drop
                    );
                    unavailable.push(object.mark(ctx, index, message));
                }
                None => skipped.push((
                    index,
                    format!(
                        "statement {} of the catalog of {} failed, and is skipped (it is kept \
                         in the catalog): {cause}\n  the statement: {sql}",
                        index + 1,
                        database.name()
                    ),
                )),
            }
        }
        pending = still.into_iter().map(|(i, sql, _)| (i, sql)).collect();
    }
    let mut notices: Vec<(usize, String)> = unavailable
        .into_iter()
        .map(|u| (u.index, u.message))
        .chain(skipped)
        .collect();
    notices.sort();
    let notifier = Notifier::of(ctx.state().config());
    for (_, notice) in notices {
        notifier.say(NoticeLevel::On, || notice);
    }
    Ok(())
}

/// An object of the catalog that could not be defined when it was opened.
struct Unavailable {
    /// The statement's place in the catalog, for the order of the notices.
    index: usize,
    kind: &'static str,
    name: String,
    /// Why, and how to repair it: what its notice says, and what using it
    /// fails with.
    message: String,
}

/// The object a catalog statement defines, as messages name it.
struct DefinedObject {
    /// `view`, `table` or `graph`.
    kind: &'static str,
    /// Its name as written.
    name: String,
    /// The statement that drops it: `DROP VIEW v`.
    drop: String,
    /// What it is in the session.
    target: Target,
}

enum Target {
    Table(TableReference, TableType),
    Graph(String),
}

impl DefinedObject {
    /// Mark the object unavailable in `ctx`, with `message`.
    fn mark(self, ctx: &SessionContext, index: usize, message: String) -> Unavailable {
        match self.target {
            Target::Table(reference, table_type) => {
                // A name already taken (a duplicate entry) keeps its table.
                let _ = ctx.register_table(
                    reference,
                    Arc::new(UnavailableTable::new(message.clone(), table_type)),
                );
            }
            Target::Graph(name) => graph::registry_of(ctx).mark_unavailable(&name, message.clone()),
        }
        Unavailable {
            index,
            kind: self.kind,
            name: self.name,
            message,
        }
    }
}

/// The view, external table or graph the stored statement `sql` defines, if
/// it parses as one.
fn defined_object(ctx: &SessionContext, sql: &str) -> Option<DefinedObject> {
    let statements = parse_sql_for(ctx, sql).ok()?;
    let [statement] = <[UrsaStatement; 1]>::try_from(statements).ok()?;
    let table = |name: &ObjectName, kind: &'static str, table_type| {
        let reference = object_name_to_table_reference(name.clone(), normalizes(ctx)).ok()?;
        Some(DefinedObject {
            kind,
            name: name.to_string(),
            drop: format!(
                "DROP {} {name}",
                if kind == "view" { "VIEW" } else { "TABLE" }
            ),
            target: Target::Table(reference, table_type),
        })
    };
    match &statement {
        UrsaStatement::CreatePropertyGraph(g) => {
            let name = graph_ddl::graph_name(g.name.clone(), normalizes(ctx)).ok()?;
            Some(DefinedObject {
                kind: "graph",
                name: g.name.to_string(),
                drop: format!("DROP PROPERTY GRAPH {}", g.name),
                target: Target::Graph(name),
            })
        }
        UrsaStatement::DataFusion(df) => match df.as_ref() {
            DFStatement::CreateExternalTable(t) => table(&t.name, "table", TableType::Base),
            DFStatement::Statement(s) => match s.as_ref() {
                SQLStatement::CreateView(v) => table(&v.name, "view", TableType::View),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// Run one stored statement, its relative paths resolved against the
/// database's directory, wherever the process runs.
async fn replay_one(ctx: &SessionContext, database: &Database, sql: &str) -> Result<()> {
    let mut statements = parse_sql_for(ctx, sql)?;
    if statements.len() != 1 {
        return Err(DataFusionError::Plan(
            "a catalog entry must be exactly one statement".into(),
        ));
    }
    let mut statement = statements.remove(0);
    paths::rewrite_locations(ctx, &mut statement, |location| {
        database.replayed_location(location)
    });
    let Effect::Define { key, .. } = effect(ctx, &statement)? else {
        return Err(DataFusionError::Plan(format!(
            "a catalog entry must define a view, an external table or a graph, not run {}",
            statement.kind()
        )));
    };
    check_available(ctx, &statement, Some(&key)).await?;
    run(ctx, statement).await.map(|_| ())
}

/// Refuse `statement` if it reads a view or table that is unavailable (see
/// [`replay`]), with why. `defines` is the object it defines, which it may
/// replace. Graphs are checked where they are used.
async fn check_available(
    ctx: &SessionContext,
    statement: &UrsaStatement,
    defines: Option<&Key>,
) -> Result<()> {
    let UrsaStatement::DataFusion(df) = statement else {
        return Ok(());
    };
    let state = ctx.state();
    let Ok(references) = state.resolve_table_references(df) else {
        return Ok(());
    };
    let options = &state.config().options().catalog;
    for reference in references {
        if matches!(&reference, TableReference::Bare { table } if looks_like_location(table)) {
            continue;
        }
        let resolved = reference
            .clone()
            .resolve(&options.default_catalog, &options.default_schema);
        if defines == Some(&Key::Table(resolved.to_string())) {
            continue;
        }
        let Ok(schema) = state.schema_for_ref(reference) else {
            continue;
        };
        if let Ok(Some(provider)) = schema.table(&resolved.table).await {
            db::check_available(provider.as_ref())?;
        }
    }
    Ok(())
}

/// `ATTACH 'club.ursa'` in a session without a database.
pub(crate) async fn attach(ctx: &SessionContext, attach: Attach) -> Result<DataFrame> {
    if let Some(alias) = &attach.alias {
        return Err(DataFusionError::NotImplemented(format!(
            "ATTACH … AS {alias} is not supported yet: a session has one database in this \
             version of ursa; use ATTACH '{}' without AS",
            attach.path
        )));
    }
    if defines_anything(ctx)? {
        return Err(DataFusionError::Plan(format!(
            "ATTACH must come before anything is defined: this session already has tables, \
             views or graphs, which would not be in '{}'. Run ATTACH first, or open the \
             database with: ursa {}",
            attach.path, attach.path
        )));
    }
    open_into(ctx, Path::new(&attach.path), &OpenOptions::default()).await?;
    graph_ddl::empty(ctx)
}

/// Whether `ctx` has any graph, table or view (outside `information_schema`).
fn defines_anything(ctx: &SessionContext) -> Result<bool> {
    if !graph::registry_of(ctx).names().is_empty() {
        return Ok(true);
    }
    for catalog_name in ctx.catalog_names() {
        let Some(catalog) = ctx.catalog(&catalog_name) else {
            continue;
        };
        for schema_name in catalog.schema_names() {
            if schema_name == "information_schema" {
                continue;
            }
            if let Some(schema) = catalog.schema(&schema_name) {
                if !schema.table_names().is_empty() {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Run `statement` in a session with `database`, keeping the catalog file in
/// step. `text` is the statement as written, if known.
pub(crate) async fn execute(
    ctx: &SessionContext,
    database: &Database,
    statement: UrsaStatement,
    text: Option<&str>,
) -> Result<DataFrame> {
    if let UrsaStatement::Attach(_) = statement {
        return Err(DataFusionError::Plan(format!(
            "{} is already open, and a session has one database in this version of ursa",
            database.name()
        )));
    }
    let effect = effect(ctx, &statement)?;
    if database.is_read_only() && !matches!(effect, Effect::None) {
        return Err(DataFusionError::Plan(format!(
            "{} is open read-only (--readonly), so {} cannot change it",
            database.name(),
            leading_words(&statement)
        )));
    }
    if !matches!(effect, Effect::Drop { .. }) {
        let defines = match &effect {
            Effect::Define { key, .. } => Some(key),
            _ => None,
        };
        check_available(ctx, &statement, defines).await?;
    }
    match effect {
        Effect::None if sets_index_storage_limit(&statement) => {
            run_index_storage_limit(ctx, database, statement).await
        }
        Effect::None => run(ctx, statement).await,
        Effect::Session => run(ctx, statement).await,
        Effect::Refused(message) => Err(DataFusionError::Plan(message)),
        Effect::Define {
            key, if_not_exists, ..
        } => {
            let stored = stored_text(ctx, database, &statement, text)?;
            let frame = run(ctx, statement).await?;
            let mut statements = database.statements();
            match position(ctx, &statements, &key) {
                Some(_) if if_not_exists => {}
                Some(i) => statements[i] = stored,
                None => statements.push(stored),
            }
            database.save(statements)?;
            Ok(frame)
        }
        Effect::Drop { keys } => {
            let frame = run(ctx, statement).await?;
            let statements = database
                .statements()
                .into_iter()
                .filter(|s| !stored_key(ctx, s).is_some_and(|k| keys.contains(&k)))
                .collect();
            database.save(statements)?;
            Ok(frame)
        }
    }
}

/// Where the object `key` is defined in `statements`.
fn position(ctx: &SessionContext, statements: &[String], key: &Key) -> Option<usize> {
    statements
        .iter()
        .position(|s| stored_key(ctx, s).as_ref() == Some(key))
}

/// The object a stored statement defines.
fn stored_key(ctx: &SessionContext, sql: &str) -> Option<Key> {
    let statements = parse_sql_for(ctx, sql).ok()?;
    match <[UrsaStatement; 1]>::try_from(statements) {
        Ok([statement]) => match effect(ctx, &statement).ok()? {
            Effect::Define { key, .. } => Some(key),
            _ => None,
        },
        Err(_) => None,
    }
}

/// The text to store for `statement`, with its local file paths rewritten
/// for the catalog ([`Database::stored_location`]): relative to the
/// database's directory, or absolute outside it. That is the rewritten
/// statement's `Display` form if it parses back to the same statement; else
/// the text as written, with the paths replaced token by token
/// ([`paths::rewrite_text`]), or an error if that cannot be done exactly.
fn stored_text(
    ctx: &SessionContext,
    database: &Database,
    statement: &UrsaStatement,
    text: Option<&str>,
) -> Result<String> {
    let cwd = std::env::current_dir().map_err(|e| {
        DataFusionError::Execution(format!(
            "cannot store this {} in {}: cannot read the current directory: {e}",
            leading_words(statement),
            database.name()
        ))
    })?;
    let mut rewritten = statement.clone();
    let replacements = paths::rewrite_locations(ctx, &mut rewritten, |location| {
        database.stored_location(location, &cwd)
    });
    let display = rewritten.to_string();
    let round_trips = parse_sql_for(ctx, &display)
        .map(|parsed| parsed.len() == 1 && parsed[0] == rewritten)
        .unwrap_or(false);
    if round_trips {
        return Ok(display);
    }
    let Some(text) = text else {
        return Err(DataFusionError::Plan(format!(
            "cannot store this {} in the database without the text it was written as; run it \
             with ursa_sql::execute_with_text",
            leading_words(statement)
        )));
    };
    if replacements.is_empty() {
        return Ok(text.to_string());
    }
    paths::rewrite_text(ctx, text, &replacements, &rewritten).ok_or_else(|| {
        let (old, new) = &replacements[0];
        DataFusionError::Plan(format!(
            "cannot store this {} in {}: its path '{old}' is stored as '{new}' (relative to the \
             database's directory, {}), and the statement as written could not be rewritten \
             exactly; write each path as stored, from the database's directory",
            leading_words(statement),
            database.name(),
            database.dir().display()
        ))
    })
}

/// What `statement` does to the catalog.
fn effect(ctx: &SessionContext, statement: &UrsaStatement) -> Result<Effect> {
    Ok(match statement {
        UrsaStatement::CreatePropertyGraph(g) => Effect::Define {
            key: Key::Graph(graph_ddl::graph_name(g.name.clone(), normalizes(ctx))?),
            if_not_exists: g.if_not_exists,
        },
        UrsaStatement::DropPropertyGraph(g) => Effect::Drop {
            keys: vec![Key::Graph(graph_ddl::graph_name(
                g.name.clone(),
                normalizes(ctx),
            )?)],
        },
        UrsaStatement::DescribeGraph(_) | UrsaStatement::ShowGraphs | UrsaStatement::Attach(_) => {
            Effect::None
        }
        UrsaStatement::DataFusion(statement) => match statement.as_ref() {
            DFStatement::CreateExternalTable(t) if t.temporary => Effect::Session,
            DFStatement::CreateExternalTable(t) => Effect::Define {
                key: table_key(ctx, &t.name)?,
                if_not_exists: t.if_not_exists,
            },
            DFStatement::Statement(s) => sql_effect(ctx, s, statement)?,
            DFStatement::CopyTo(_) | DFStatement::Explain(_) | DFStatement::Reset(_) => {
                Effect::None
            }
        },
    })
}

fn sql_effect(ctx: &SessionContext, s: &SQLStatement, whole: &DFStatement) -> Result<Effect> {
    Ok(match s {
        SQLStatement::CreateView(v) if v.temporary => Effect::Session,
        SQLStatement::CreateView(v) if v.materialized => Effect::Refused(not_stored(whole)),
        SQLStatement::CreateView(v) => Effect::Define {
            key: table_key(ctx, &v.name)?,
            if_not_exists: v.if_not_exists,
        },
        SQLStatement::CreateTable(t) if t.temporary => Effect::Session,
        SQLStatement::CreateTable(t) => Effect::Refused(tables_with_data(
            if t.query.is_some() {
                "CREATE TABLE … AS"
            } else {
                "CREATE TABLE"
            },
            Some(&t.name),
        )),
        SQLStatement::Insert(_) => Effect::Refused(tables_with_data("INSERT", None)),
        SQLStatement::Update(_)
        | SQLStatement::Delete(_)
        | SQLStatement::Truncate(_)
        | SQLStatement::Merge(_) => {
            Effect::Refused(tables_with_data(&leading_words_df(whole), None))
        }
        SQLStatement::Drop {
            object_type: ObjectType::Table | ObjectType::View,
            names,
            ..
        } => Effect::Drop {
            keys: names
                .iter()
                .map(|n| table_key(ctx, n))
                .collect::<Result<_>>()?,
        },
        _ => {
            let first = whole.to_string();
            let first = first
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            if matches!(first.as_str(), "CREATE" | "DROP" | "ALTER") {
                Effect::Refused(not_stored(whole))
            } else {
                Effect::None
            }
        }
    })
}

/// The refusal for a statement that would make or change a table with data.
fn tables_with_data(kind: &str, name: Option<&ObjectName>) -> String {
    let name = name.map_or_else(|| "name".to_string(), ToString::to_string);
    format!(
        "{kind} is not supported in a database file: version 1 of the .ursa file stores views, \
         external tables and graphs, not tables with data. Define a view instead (CREATE VIEW \
         {name} AS SELECT …), or write the rows to a file with COPY (SELECT …) TO \
         '{name}.parquet' and define a view over it (CREATE VIEW {name} AS SELECT * FROM \
         '{name}.parquet')"
    )
}

/// The refusal for other DDL, which the catalog file could not replay.
fn not_stored(statement: &DFStatement) -> String {
    format!(
        "{} is not supported in a database file: version 1 of the .ursa file stores views, \
         external tables and graphs only; run it in a session without a database",
        leading_words_df(statement)
    )
}

/// The statement's first two words (`CREATE VIEW`, `DROP TABLE`), as messages
/// name it.
fn leading_words(statement: &UrsaStatement) -> String {
    match statement {
        UrsaStatement::DataFusion(s) => leading_words_df(s),
        other => other.kind().to_string(),
    }
}

fn leading_words_df(statement: &DFStatement) -> String {
    let text = statement.to_string();
    let mut words = text.split_whitespace().map(str::to_ascii_uppercase);
    let first = words.next().unwrap_or_default();
    let mut kind = first.clone();
    // `CREATE OR REPLACE VIEW` is named `CREATE VIEW`.
    for word in words {
        if matches!(
            word.as_str(),
            "OR" | "REPLACE" | "TEMPORARY" | "EXTERNAL" | "MATERIALIZED"
        ) && first == "CREATE"
        {
            if word == "EXTERNAL" || word == "MATERIALIZED" {
                kind.push(' ');
                kind.push_str(&word);
            }
            continue;
        }
        if matches!(first.as_str(), "CREATE" | "DROP" | "ALTER") {
            kind.push(' ');
            kind.push_str(&word);
        }
        break;
    }
    kind
}

/// A view or table's key: its name resolved against the default catalog and
/// schema.
fn table_key(ctx: &SessionContext, name: &ObjectName) -> Result<Key> {
    let state = ctx.state();
    let options = &state.config().options().catalog;
    let reference = object_name_to_table_reference(name.clone(), normalizes(ctx))?;
    Ok(Key::Table(
        reference
            .resolve(&options.default_catalog, &options.default_schema)
            .to_string(),
    ))
}

fn normalizes(ctx: &SessionContext) -> bool {
    graph_ddl::normalizes(ctx)
}
