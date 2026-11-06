//! `read_parquet(path)`, `read_csv(path, header => false)` and
//! `read_json(path)`: files as tables, as in DuckDB.
//!
//! Each is a thin wrapper over the path: `FROM read_csv('edges.csv')` reads
//! the same table as `FROM 'edges.csv'` ([`crate::file_table`]), so a relative
//! path, a glob, a directory or a URL behaves the same, and a graph's index
//! cache knows the files by the same identity. What the function adds is the
//! format, whatever the extension, and a few options:
//!
//! - `read_csv`: `header`, `delim` (or `sep` or `delimiter`), `quote`,
//!   `compression`;
//! - `read_json`: `compression`, and `format => 'array'` for a file holding
//!   one JSON array of objects rather than one object per line;
//! - `read_parquet`: none.
//!
//! The first argument is a path or a list of paths (`['a.csv', 'b.csv']`).
//! Options are named, `name => value` as Ursa's graph functions take them,
//! or `name = value` as DuckDB writes them. `read_csv_auto`, `read_json_auto`
//! and `read_ndjson` are DuckDB's other names for the same functions.
//!
//! ## Opened before planning
//!
//! Opening files is async (it lists them and infers a schema), and a relation
//! planner is not. So [`open_reads`] walks a statement before it is planned,
//! opens every `read_*` call in it, and keeps each table under the call's
//! text in the session's [`ReadTables`]; [`ReadFunctions`], the relation
//! planner, then finds the table there by the same text. Ursa's SQL layer
//! (`ursa_sql::execute`) does this for every statement, so a call in a view,
//! a subquery, a CTE, a `COPY` or an `EXPLAIN` is opened too.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;

use datafusion::catalog::default_table_source::provider_as_source;
use datafusion::catalog::TableProvider;
use datafusion::common::parsers::CompressionTypeVariant;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionState;
use datafusion::logical_expr::planner::{
    PlannedRelation, RelationPlanner, RelationPlannerContext, RelationPlanning,
};
use datafusion::logical_expr::LogicalPlanBuilder;
use datafusion::prelude::SessionConfig;
use datafusion::sql::parser::{CopyToSource, Statement as DFStatement};
use datafusion::sql::sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, Ident, ObjectName, TableFactor,
    TableFunctionArgs, Value, Visit, Visitor,
};
use parking_lot::Mutex;

use crate::file_table::{open_files, FileFormat, FileOptions};
use crate::graph::closest_name;

/// The functions, by name: what each reads, and what it fixes.
const FUNCTIONS: [(&str, Function); 6] = [
    ("read_parquet", Function::Parquet),
    ("read_csv", Function::Csv),
    ("read_csv_auto", Function::Csv),
    ("read_json", Function::Json),
    ("read_json_auto", Function::Json),
    ("read_ndjson", Function::NdJson),
];

/// The names of the `read_*` functions.
pub fn names() -> impl Iterator<Item = &'static str> {
    FUNCTIONS.iter().map(|(name, _)| *name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Function {
    Parquet,
    Csv,
    Json,
    /// `read_ndjson`: JSON, one object per line.
    NdJson,
}

impl Function {
    fn lookup(name: &str) -> Option<Self> {
        FUNCTIONS.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
    }

    /// The options the function takes.
    fn options(self) -> &'static [&'static str] {
        match self {
            Function::Parquet => &[],
            Function::Csv => &[
                "header",
                "delim",
                "sep",
                "delimiter",
                "quote",
                "compression",
            ],
            Function::Json => &["compression", "format"],
            Function::NdJson => &["compression"],
        }
    }

    fn format(self) -> FileFormat {
        match self {
            Function::Parquet => FileFormat::Parquet,
            Function::Csv => FileFormat::Csv,
            Function::Json | Function::NdJson => FileFormat::Json,
        }
    }
}

/// One `read_*` call: the locations it reads and how.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadCall {
    locations: Vec<String>,
    options: FileOptions,
}

/// The tables of the `read_*` calls in the statements being planned, by the
/// call's text, with how many statements are using each.
#[derive(Debug, Default)]
pub struct ReadTables {
    tables: Mutex<HashMap<String, Shared>>,
}

/// A call's table, and how many statements being planned use it.
type Shared = (Arc<dyn TableProvider>, usize);

/// `config` with a place for the tables of `read_*` calls.
pub(crate) fn with_read_tables(config: SessionConfig) -> SessionConfig {
    config.with_extension(Arc::new(ReadTables::default()))
}

fn read_tables(config: &SessionConfig) -> Result<Arc<ReadTables>> {
    config
        .get_extension::<ReadTables>()
        .ok_or_else(|| DataFusionError::Internal("the session has no read_* tables".into()))
}

/// The tables [`open_reads`] opened for one statement. Dropping it forgets
/// them, once the statement is planned.
#[must_use = "the tables are forgotten when this is dropped"]
pub struct OpenReads {
    tables: Option<Arc<ReadTables>>,
    keys: Vec<String>,
}

impl Drop for OpenReads {
    fn drop(&mut self) {
        let Some(tables) = &self.tables else {
            return;
        };
        let mut tables = tables.tables.lock();
        for key in &self.keys {
            if let Some((_, users)) = tables.get_mut(key) {
                *users -= 1;
                if *users == 0 {
                    tables.remove(key);
                }
            }
        }
    }
}

/// Open the table of every `read_*` call in `statement`, for [`ReadFunctions`]
/// to plan. Keep the result until the statement is planned.
pub async fn open_reads(state: &SessionState, statement: &DFStatement) -> Result<OpenReads> {
    let mut calls = Calls::default();
    collect(statement, &mut calls)?;
    if calls.calls.is_empty() {
        return Ok(OpenReads {
            tables: None,
            keys: Vec::new(),
        });
    }
    let tables = read_tables(state.config())?;
    let mut open = OpenReads {
        tables: Some(Arc::clone(&tables)),
        keys: Vec::new(),
    };
    for (key, call) in calls.calls {
        let table = open_files(state, &call.locations, &call.options).await?;
        let mut tables = tables.tables.lock();
        tables
            .entry(key.clone())
            .and_modify(|(_, users)| *users += 1)
            .or_insert((table, 1));
        open.keys.push(key);
    }
    Ok(open)
}

/// The `read_*` calls in a statement, by their text.
#[derive(Default)]
struct Calls {
    calls: Vec<(String, ReadCall)>,
}

impl Visitor for Calls {
    type Break = DataFusionError;

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
        let TableFactor::Table {
            name,
            args: Some(args),
            ..
        } = factor
        else {
            return ControlFlow::Continue(());
        };
        let Some(function) = function_name(name, |ident| default_normalize(ident.clone())) else {
            return ControlFlow::Continue(());
        };
        let Some(f) = Function::lookup(&function) else {
            return ControlFlow::Continue(());
        };
        let key = call_key(name, args);
        if self.calls.iter().any(|(k, _)| *k == key) {
            return ControlFlow::Continue(());
        }
        match parse_call(&function, f, args) {
            Ok(call) => {
                self.calls.push((key, call));
                ControlFlow::Continue(())
            }
            Err(e) => ControlFlow::Break(e),
        }
    }
}

fn collect(statement: &DFStatement, calls: &mut Calls) -> Result<()> {
    let flow = match statement {
        DFStatement::Statement(s) => s.visit(calls),
        DFStatement::CopyTo(copy) => match &copy.source {
            CopyToSource::Query(query) => query.visit(calls),
            CopyToSource::Relation(_) => ControlFlow::Continue(()),
        },
        DFStatement::Explain(explain) => return collect(&explain.statement, calls),
        _ => ControlFlow::Continue(()),
    };
    match flow {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(e) => Err(e),
    }
}

/// Unquoted identifiers are lowercase, as DataFusion normalizes them by default.
fn default_normalize(ident: Ident) -> String {
    match ident.quote_style {
        Some(_) => ident.value,
        None => ident.value.to_lowercase(),
    }
}

/// The function name of a one-part `name`, normalized.
fn function_name(name: &ObjectName, normalize: impl Fn(&Ident) -> String) -> Option<String> {
    let [part] = name.0.as_slice() else {
        return None;
    };
    part.as_ident().map(normalize)
}

/// The paths `factor` reads, if it is a `read_*` call: the strings of its
/// first argument, for a database to rewrite relative to its directory.
pub fn paths_mut(factor: &mut TableFactor) -> Vec<&mut String> {
    let TableFactor::Table {
        name,
        args: Some(args),
        ..
    } = factor
    else {
        return Vec::new();
    };
    let is_read = function_name(name, |ident| default_normalize(ident.clone()))
        .is_some_and(|f| Function::lookup(&f).is_some());
    let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(first))) = args.args.first_mut() else {
        return Vec::new();
    };
    if !is_read {
        return Vec::new();
    }
    fn string_mut(expr: &mut Expr) -> Option<&mut String> {
        match expr {
            Expr::Value(v) => match &mut v.value {
                Value::SingleQuotedString(s) => Some(s),
                _ => None,
            },
            Expr::Nested(inner) => string_mut(inner),
            _ => None,
        }
    }
    match first {
        Expr::Array(array) => array.elem.iter_mut().filter_map(string_mut).collect(),
        expr => string_mut(expr).into_iter().collect(),
    }
}

/// What a call's table is kept under: its text.
fn call_key(name: &ObjectName, args: &TableFunctionArgs) -> String {
    let args: Vec<String> = args.args.iter().map(ToString::to_string).collect();
    format!("{name}({})", args.join(", "))
}

fn plan_error(message: String) -> DataFusionError {
    DataFusionError::Plan(message)
}

/// Read a `read_*` call's arguments.
fn parse_call(name: &str, function: Function, args: &TableFunctionArgs) -> Result<ReadCall> {
    let usage = || match function {
        Function::Parquet => format!("{name}('edges.parquet')"),
        Function::Csv => format!("{name}('edges.csv', header => true)"),
        Function::Json | Function::NdJson => format!("{name}('edges.jsonl')"),
    };
    if args.settings.is_some() {
        return Err(plan_error(format!(
            "{name}() takes no SETTINGS; write {}",
            usage()
        )));
    }
    let mut args = args.args.iter();
    let locations = match args.next() {
        Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))) => locations(expr),
        _ => None,
    }
    .ok_or_else(|| {
        plan_error(format!(
            "{name}() takes a path, or a list of paths, as its first argument; write {}",
            usage()
        ))
    })?;
    let mut options = FileOptions {
        format: Some(function.format()),
        ..FileOptions::default()
    };
    if function == Function::NdJson {
        options.newline_delimited = Some(true);
    }
    let mut seen: Vec<String> = Vec::new();
    for arg in args {
        let (option, value) = match arg {
            FunctionArg::Named {
                name: option,
                arg: FunctionArgExpr::Expr(value),
                ..
            } => (default_normalize(option.clone()), value),
            // DuckDB's `header = true`.
            FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::BinaryOp {
                left,
                op: BinaryOperator::Eq,
                right,
            })) if matches!(left.as_ref(), Expr::Identifier(_)) => {
                let Expr::Identifier(option) = left.as_ref() else {
                    unreachable!("matched above")
                };
                (default_normalize(option.clone()), right.as_ref())
            }
            other => {
                return Err(plan_error(format!(
                    "{name}() takes its options by name, as name => value; got {other}. \
                     Write {}",
                    usage()
                )))
            }
        };
        let known = function.options();
        if !known.contains(&option.as_str()) {
            let hint = closest_name(&option, known.iter().copied())
                .map(|c| format!("; did you mean '{c}'?"))
                .unwrap_or_default();
            let takes = match known {
                [] => "it takes none".to_string(),
                known => format!("it takes {}", known.join(", ")),
            };
            return Err(plan_error(format!(
                "{name}() has no option '{option}'{hint} ({takes})"
            )));
        }
        let canonical = match option.as_str() {
            "sep" | "delimiter" => "delim",
            option => option,
        };
        if seen.iter().any(|s| s == canonical) {
            return Err(plan_error(format!(
                "{name}() was given '{option}' more than once"
            )));
        }
        seen.push(canonical.to_string());
        let invalid =
            |what: &str| plan_error(format!("{name}({option} =>) must be {what}; got {value}"));
        match canonical {
            "header" => {
                options.has_header = Some(boolean(value).ok_or_else(|| invalid("true or false"))?)
            }
            "delim" | "quote" => {
                let byte = string(value)
                    .and_then(|s| single_byte(&s))
                    .ok_or_else(|| invalid("a string of one character, such as '|' or '\\t'"))?;
                if canonical == "delim" {
                    options.delimiter = Some(byte);
                } else {
                    options.quote = Some(byte);
                }
            }
            "compression" => {
                options.compression = compression(value)
                    .ok_or_else(|| invalid("'auto', 'none', 'gzip', 'bzip2', 'xz' or 'zstd'"))?
            }
            "format" => {
                let format = string(value).map(|s| s.to_ascii_lowercase());
                options.newline_delimited = match format.as_deref() {
                    Some("newline_delimited") => Some(true),
                    Some("array") => Some(false),
                    _ => return Err(invalid("'newline_delimited' or 'array'")),
                }
            }
            _ => unreachable!("checked against the function's options"),
        }
    }
    Ok(ReadCall { locations, options })
}

/// A path, or a list of paths: plain strings, which [`paths_mut`] rewrites.
fn locations(expr: &Expr) -> Option<Vec<String>> {
    fn path(expr: &Expr) -> Option<String> {
        match expr {
            Expr::Value(v) => match &v.value {
                Value::SingleQuotedString(s) => Some(s.clone()),
                _ => None,
            },
            Expr::Nested(inner) => path(inner),
            _ => None,
        }
    }
    match expr {
        Expr::Array(array) => {
            let paths: Option<Vec<String>> = array.elem.iter().map(path).collect();
            paths.filter(|p| !p.is_empty())
        }
        expr => path(expr).map(|s| vec![s]),
    }
}

fn string(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Value(v) => match &v.value {
            Value::SingleQuotedString(s) | Value::EscapedStringLiteral(s) => Some(s.clone()),
            _ => None,
        },
        Expr::Nested(inner) => string(inner),
        _ => None,
    }
}

fn boolean(expr: &Expr) -> Option<bool> {
    match expr {
        Expr::Value(v) => match &v.value {
            Value::Boolean(b) => Some(*b),
            _ => None,
        },
        _ => None,
    }
}

/// One ASCII character; `\t` (as DuckDB writes a tab in a plain string) is a tab.
fn single_byte(s: &str) -> Option<u8> {
    match s {
        "\\t" => Some(b'\t'),
        s if s.len() == 1 && s.is_ascii() => Some(s.as_bytes()[0]),
        _ => None,
    }
}

/// A compression option: `Some(None)` for `'auto'` (from the extension).
fn compression(expr: &Expr) -> Option<Option<CompressionTypeVariant>> {
    let value = string(expr)?.to_ascii_lowercase();
    match value.as_str() {
        "auto" => Some(None),
        "none" | "uncompressed" => Some(Some(CompressionTypeVariant::UNCOMPRESSED)),
        other => other.parse().ok().map(Some),
    }
}

/// Plans `read_*` calls in `FROM` as scans of the tables [`open_reads`]
/// opened.
#[derive(Debug)]
pub struct ReadFunctions {
    tables: Arc<ReadTables>,
}

impl ReadFunctions {
    pub(crate) fn new(config: &SessionConfig) -> Result<Self> {
        Ok(Self {
            tables: read_tables(config)?,
        })
    }
}

impl RelationPlanner for ReadFunctions {
    fn plan_relation(
        &self,
        relation: TableFactor,
        context: &mut dyn RelationPlannerContext,
    ) -> Result<RelationPlanning> {
        let TableFactor::Table {
            name,
            args: Some(args),
            ..
        } = &relation
        else {
            return Ok(RelationPlanning::Original(Box::new(relation)));
        };
        let Some(function) = function_name(name, |ident| context.normalize_ident(ident.clone()))
        else {
            return Ok(RelationPlanning::Original(Box::new(relation)));
        };
        let Some(f) = Function::lookup(&function) else {
            // A misspelt `read_*` call: a name close to one, given a path.
            let given_a_path = matches!(
                args.args.first(),
                Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))) if locations(expr).is_some()
            );
            if given_a_path {
                if let Some(close) = closest_name(&function, names()) {
                    return Err(plan_error(format!(
                        "there is no table function '{function}'; did you mean '{close}'?"
                    )));
                }
            }
            return Ok(RelationPlanning::Original(Box::new(relation)));
        };
        let key = call_key(name, args);
        let table = self
            .tables
            .tables
            .lock()
            .get(&key)
            .map(|(t, _)| Arc::clone(t));
        let Some(table) = table else {
            // Not opened: the statement did not come through `open_reads`.
            // Read the arguments anyway, so a bad one is reported as such.
            parse_call(&function, f, args)?;
            return Err(DataFusionError::Internal(format!(
                "{function}() was not opened before planning; run the statement with \
                 ursa_sql::execute"
            )));
        };
        let TableFactor::Table {
            alias,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_hints,
            ..
        } = relation
        else {
            unreachable!("matched above")
        };
        if !with_hints.is_empty()
            || version.is_some()
            || with_ordinality
            || !partitions.is_empty()
            || json_path.is_some()
            || sample.is_some()
            || !index_hints.is_empty()
        {
            return Err(plan_error(format!("{function}() takes no table modifiers")));
        }
        let plan = LogicalPlanBuilder::scan(function.as_str(), provider_as_source(table), None)?
            .build()?;
        Ok(RelationPlanning::Planned(Box::new(PlannedRelation::new(
            plan, alias,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::sql::sqlparser::dialect::GenericDialect;
    use datafusion::sql::sqlparser::parser::Parser;

    fn call(sql: &str) -> Result<ReadCall> {
        let query = format!("SELECT * FROM {sql}");
        let statement = Parser::parse_sql(&GenericDialect {}, &query)
            .unwrap()
            .remove(0);
        let mut calls = Calls::default();
        match statement.visit(&mut calls) {
            ControlFlow::Break(e) => Err(e),
            ControlFlow::Continue(()) => Ok(calls.calls.remove(0).1),
        }
    }

    fn message(sql: &str) -> String {
        crate::user_message(&call(sql).unwrap_err())
    }

    #[test]
    fn reads_paths_and_options() {
        let c = call("read_csv('a.csv', header => false, delim = '\\t', compression => 'gzip')")
            .unwrap();
        assert_eq!(c.locations, ["a.csv"]);
        assert_eq!(c.options.format, Some(FileFormat::Csv));
        assert_eq!(c.options.has_header, Some(false));
        assert_eq!(c.options.delimiter, Some(b'\t'));
        assert_eq!(c.options.compression, Some(CompressionTypeVariant::GZIP));

        let c = call("read_parquet(['a.parquet', 'b.parquet'])").unwrap();
        assert_eq!(c.locations, ["a.parquet", "b.parquet"]);

        let c = call("read_json('a.json', format => 'array')").unwrap();
        assert_eq!(c.options.newline_delimited, Some(false));
        let c = call("read_ndjson('a.txt', compression => 'auto')").unwrap();
        assert_eq!(c.options.newline_delimited, Some(true));
        assert_eq!(c.options.compression, None);
    }

    #[test]
    fn reports_bad_arguments() {
        assert_eq!(
            message("read_csv('a.csv', hedaer => true)"),
            "read_csv() has no option 'hedaer'; did you mean 'header'? (it takes header, \
             delim, sep, delimiter, quote, compression)"
        );
        assert_eq!(
            message("read_parquet('a.parquet', header => true)"),
            "read_parquet() has no option 'header' (it takes none)"
        );
        assert_eq!(
            message("read_csv(1)"),
            "read_csv() takes a path, or a list of paths, as its first argument; write \
             read_csv('edges.csv', header => true)"
        );
        assert_eq!(
            message("read_csv('a.csv', delim => ';;')"),
            "read_csv(delim =>) must be a string of one character, such as '|' or '\\t'; \
             got ';;'"
        );
        assert_eq!(
            message("read_csv('a.csv', sep => ';', delim => ',')"),
            "read_csv() was given 'delim' more than once"
        );
    }
}
