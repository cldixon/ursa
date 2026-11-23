//! DuckDB habits that DataFusion plans differently or refuses with a message
//! that does not say what to write (#417).
//!
//! Accepted, by changing the statement before it is planned
//! ([`accept`]):
//!
//! - `SET threads = n` is `SET datafusion.execution.target_partitions = n`.
//! - `CREATE TEMP TABLE` and `CREATE TEMP VIEW`: every table and view of a
//!   session is temporary already (a database file stores views, never as
//!   `TEMP`), so the keyword is dropped. DataFusion refuses it.
//!
//! Accepted when the statement fails to plan, by planning it again
//! ([`retry`]):
//!
//! - `ORDER BY ALL` (`ASC`, `DESC`, `NULLS …`) is `ORDER BY 1, 2, …`, one
//!   position per column of the `SELECT` list, when that list names its
//!   columns. A column named `all` is still ordered by first.
//!
//! Explained when the statement fails ([`explain`]):
//!
//! - a graph function called as a scalar, `SELECT pagerank(g)`: it is a table
//!   function, `SELECT * FROM pagerank(g)`;
//! - a graph function named as a column, `SELECT pagerank FROM g.nodes`: join
//!   its rows on `id`;
//! - an unquoted file name, `FROM edges.csv`, read as schema `edges`, table
//!   `csv`: quote the path;
//! - `SET` of a name with no `ursa.` or `datafusion.` prefix;
//! - `ORDER BY ALL` after `SELECT *`.

use std::ops::ControlFlow;

use datafusion::common::{Column, SchemaError};
use datafusion::error::DataFusionError;
use datafusion::sql::parser::{CopyToSource, Statement as DFStatement};
use datafusion::sql::sqlparser::ast::{
    Expr, Ident, ObjectName, OrderByKind, Query, Select, SelectItem, Set, SetExpr,
    Statement as SqlStatement, Value, Visit, VisitMut, Visitor, VisitorMut,
};
use ursa_engine::error::matched;
use ursa_engine::graph::functions;

/// DataFusion's setting for DuckDB's `threads`.
const THREADS: [&str; 3] = ["datafusion", "execution", "target_partitions"];

/// `statement` with the DuckDB forms DataFusion refuses but means the same
/// as one it takes (see the module docs).
pub(crate) fn accept(statement: &mut DFStatement) -> datafusion::error::Result<()> {
    let DFStatement::Statement(inner) = statement else {
        return Ok(());
    };
    match inner.as_mut() {
        SqlStatement::Set(Set::SingleAssignment { variable, .. }) if is_word(variable, "threads") => {
            if cfg!(target_arch = "wasm32") {
                return Err(DataFusionError::NotImplemented(
                    "SET threads is not supported in the browser, which runs each query on one \
                     thread"
                        .into(),
                ));
            }
            *variable = ObjectName::from(THREADS.map(Ident::new).to_vec());
        }
        SqlStatement::CreateTable(table) => table.temporary = false,
        SqlStatement::CreateView(view) => view.temporary = false,
        _ => {}
    }
    Ok(())
}

/// Whether `name` is the single unquoted identifier `word`.
fn is_word(name: &ObjectName, word: &str) -> bool {
    match name.0.as_slice() {
        [part] => part
            .as_ident()
            .is_some_and(|i| i.quote_style.is_none() && i.value.eq_ignore_ascii_case(word)),
        _ => false,
    }
}

/// `statement` again, with `ORDER BY ALL` as positions, if `err` is the
/// error DataFusion gives for it and there is an `ORDER BY ALL` to rewrite.
pub(crate) fn retry(statement: &DFStatement, err: &DataFusionError) -> Option<DFStatement> {
    let (field, _) = field_not_found(err)?;
    if field.relation.is_some() || field.name != "all" {
        return None;
    }
    let mut statement = statement.clone();
    let mut rewrite = OrderByAll { rewrote: false };
    for_each_mut(&mut statement, &mut rewrite);
    rewrite.rewrote.then_some(statement)
}

/// Rewrites `ORDER BY ALL` into positions where the `SELECT` list's width is
/// known.
struct OrderByAll {
    rewrote: bool,
}

impl VisitorMut for OrderByAll {
    type Break = ();

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<()> {
        let Some(order_by) = &mut query.order_by else {
            return ControlFlow::Continue(());
        };
        let OrderByKind::Expressions(exprs) = &mut order_by.kind else {
            return ControlFlow::Continue(());
        };
        let ([only], Some(width)) = (exprs.as_mut_slice(), select_width(&query.body)) else {
            return ControlFlow::Continue(());
        };
        if !is_all(&only.expr) {
            return ControlFlow::Continue(());
        }
        let template = only.clone();
        *exprs = (1..=width)
            .map(|n| {
                let mut e = template.clone();
                e.expr = Expr::value(Value::Number(n.to_string(), false));
                e
            })
            .collect();
        self.rewrote = true;
        ControlFlow::Continue(())
    }
}

/// Whether `expr` is the unquoted name `all`.
fn is_all(expr: &Expr) -> bool {
    matches!(expr, Expr::Identifier(i) if i.quote_style.is_none() && i.value.eq_ignore_ascii_case("all"))
}

/// How many columns a `SELECT` returns, when its list names each (no `*`).
fn select_width(body: &SetExpr) -> Option<usize> {
    match body {
        SetExpr::Select(select) => {
            let Select { projection, .. } = select.as_ref();
            projection
                .iter()
                .all(|item| matches!(item, SelectItem::UnnamedExpr(_) | SelectItem::ExprWithAlias { .. }))
                .then_some(projection.len())
        }
        SetExpr::Query(query) => select_width(&query.body),
        SetExpr::SetOperation { left, .. } => select_width(left),
        _ => None,
    }
}

/// `err`, or a message that says what to write instead when it is one of the
/// errors the module docs list.
pub(crate) fn explain(statement: &DFStatement, err: DataFusionError) -> DataFusionError {
    let message = ursa_engine::user_message(&err);
    let hint = scalar_graph_call(statement, &message)
        .or_else(|| graph_function_as_column(&err))
        .or_else(|| unquoted_file(statement, &message))
        .or_else(|| unknown_setting(statement, &message))
        .or_else(|| order_by_all(statement, &err));
    match hint {
        Some(hint) => DataFusionError::Plan(hint),
        None => err,
    }
}

/// Whether `name` is a graph function: a name `ursa_functions()` lists, or
/// `ursa_functions` itself.
fn is_graph_function(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == functions::URSA_FUNCTIONS || functions::names().any(|n| n == name)
}

/// `SELECT pagerank(g)`: "pagerank is a table function: SELECT * FROM
/// pagerank(g)".
fn scalar_graph_call(statement: &DFStatement, message: &str) -> Option<String> {
    let rest = message.strip_prefix(matched::INVALID_FUNCTION)?;
    let name = &rest[..rest.find('\'')?];
    if !is_graph_function(name) {
        return None;
    }
    struct Calls<'a>(&'a str);
    impl Visitor for Calls<'_> {
        type Break = String;
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<String> {
            match expr {
                Expr::Function(f)
                    if f.name.0.last().and_then(|p| p.as_ident()).is_some_and(|i| {
                        i.value.eq_ignore_ascii_case(self.0)
                    }) =>
                {
                    ControlFlow::Break(f.to_string())
                }
                _ => ControlFlow::Continue(()),
            }
        }
    }
    let call = for_each(statement, &mut Calls(name)).unwrap_or_else(|| format!("{name}(…)"));
    Some(format!(
        "{name} is a table function, not a scalar one: call it in FROM, as in SELECT * FROM \
         {call}"
    ))
}

/// The column DataFusion did not find, and the columns it had, if `err` is
/// that error.
fn field_not_found(err: &DataFusionError) -> Option<(&Column, &[Column])> {
    match err {
        DataFusionError::SchemaError(e, _) => match e.as_ref() {
            SchemaError::FieldNotFound {
                field,
                valid_fields,
            } => Some((field.as_ref(), valid_fields.as_slice())),
            _ => None,
        },
        DataFusionError::Context(_, inner) | DataFusionError::Diagnostic(_, inner) => {
            field_not_found(inner)
        }
        DataFusionError::Shared(inner) => field_not_found(inner),
        _ => None,
    }
}

/// `SELECT id, pagerank FROM g.nodes` (#300): pagerank is a function of the
/// graph, not a column; join its rows on `id`.
fn graph_function_as_column(err: &DataFusionError) -> Option<String> {
    let (field, valid) = field_not_found(err)?;
    if !is_graph_function(&field.name) {
        return None;
    }
    // The graph whose nodes or edges table the query reads.
    let graph = valid.iter().find_map(|c| {
        let relation = c.relation.as_ref()?;
        matches!(relation.table(), "nodes" | "edges")
            .then(|| relation.schema())
            .flatten()
    });
    let name = &field.name;
    let (graph, from) = match graph {
        Some(g) => (g.to_string(), format!("{g}.nodes")),
        None => ("g".to_string(), "t".to_string()),
    };
    Some(format!(
        "{}\n{name} is a graph function, not a column: join its rows on id, as in SELECT * FROM \
         {from} JOIN {name}({graph}) USING (id)",
        ursa_engine::user_message(err)
    ))
}

/// The file extensions an unquoted `FROM edges.csv` is taken to name a
/// file by, besides a file of that name being there.
const FILE_EXTENSIONS: &[&str] = &[
    "csv", "tsv", "parquet", "json", "jsonl", "ndjson", "arrow", "avro", "gz", "zst", "bz2", "xz",
];

/// `FROM karate_edges.csv`, read as schema `karate_edges`, table `csv`:
/// quote the path.
fn unquoted_file(statement: &DFStatement, message: &str) -> Option<String> {
    if !(message.starts_with(matched::TABLE_NOT_FOUND_START)
        && message.ends_with(matched::TABLE_NOT_FOUND_END))
    {
        return None;
    }
    struct Files;
    impl Visitor for Files {
        type Break = String;
        fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<String> {
            match file_name(relation) {
                Some(path) => ControlFlow::Break(path),
                None => ControlFlow::Continue(()),
            }
        }
    }
    let path = match statement {
        DFStatement::CopyTo(copy) => match &copy.source {
            CopyToSource::Relation(name) => file_name(name),
            CopyToSource::Query(_) => None,
        },
        _ => None,
    }
    .or_else(|| for_each(statement, &mut Files))?;
    Some(format!(
        "{message}; to read the file {path}, quote its path: FROM '{path}'"
    ))
}

/// The path an unquoted name of two or more parts spells, if it looks like a
/// file: its last part is a file extension, or a file of that name exists.
fn file_name(name: &ObjectName) -> Option<String> {
    let parts: Vec<&Ident> = name.0.iter().map(|p| p.as_ident()).collect::<Option<_>>()?;
    if parts.len() < 2 || parts.iter().any(|i| i.quote_style.is_some()) {
        return None;
    }
    let path = parts
        .iter()
        .map(|i| i.value.as_str())
        .collect::<Vec<_>>()
        .join(".");
    let extension = parts.last()?.value.to_ascii_lowercase();
    (FILE_EXTENSIONS.contains(&extension.as_str()) || std::path::Path::new(&path).is_file())
        .then_some(path)
}

/// `SET memory_limit = '1GB'`: Ursa's settings start with `ursa.` or
/// `datafusion.`.
fn unknown_setting(statement: &DFStatement, message: &str) -> Option<String> {
    if !message.contains(matched::UNKNOWN_SETTING) {
        return None;
    }
    let DFStatement::Statement(inner) = statement else {
        return None;
    };
    let SqlStatement::Set(Set::SingleAssignment { variable, .. }) = inner.as_ref() else {
        return None;
    };
    let name = variable.to_string();
    let duckdb = if name.eq_ignore_ascii_case("memory_limit") {
        "; DuckDB's memory_limit is datafusion.runtime.memory_limit, as in SET \
         datafusion.runtime.memory_limit = '1G'"
    } else {
        ""
    };
    Some(format!(
        "{name} is not a setting: Ursa's settings start with ursa. or datafusion., and SHOW ALL \
         lists them{duckdb}"
    ))
}

/// `SELECT * … ORDER BY ALL`, which [`retry`] cannot rewrite: name the
/// columns.
fn order_by_all(statement: &DFStatement, err: &DataFusionError) -> Option<String> {
    let (field, _) = field_not_found(err)?;
    if field.relation.is_some() || field.name != "all" {
        return None;
    }
    struct Finds;
    impl Visitor for Finds {
        type Break = ();
        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
            match query.order_by.as_ref().map(|o| &o.kind) {
                Some(OrderByKind::Expressions(exprs))
                    if exprs.len() == 1 && is_all(&exprs[0].expr) =>
                {
                    ControlFlow::Break(())
                }
                _ => ControlFlow::Continue(()),
            }
        }
    }
    for_each(statement, &mut Finds)?;
    Some(format!(
        "{}\nORDER BY ALL is supported only where the SELECT list names its columns; after \
         SELECT *, name the columns or their positions: ORDER BY 1, 2, …",
        ursa_engine::user_message(err)
    ))
}

/// Visit each sqlparser part of `statement` with `visitor`, giving the
/// first break.
fn for_each<V: Visitor>(statement: &DFStatement, visitor: &mut V) -> Option<V::Break> {
    let flow = match statement {
        DFStatement::Statement(s) => s.visit(visitor),
        DFStatement::CopyTo(copy) => match &copy.source {
            CopyToSource::Query(q) => q.visit(visitor),
            CopyToSource::Relation(_) => ControlFlow::Continue(()),
        },
        DFStatement::Explain(explain) => return for_each(&explain.statement, visitor),
        _ => ControlFlow::Continue(()),
    };
    match flow {
        ControlFlow::Break(b) => Some(b),
        ControlFlow::Continue(()) => None,
    }
}

/// [`for_each`], mutably.
fn for_each_mut<V: VisitorMut>(statement: &mut DFStatement, visitor: &mut V) {
    let _ = match statement {
        DFStatement::Statement(s) => s.visit(visitor),
        DFStatement::CopyTo(copy) => match &mut copy.source {
            CopyToSource::Query(q) => q.visit(visitor),
            CopyToSource::Relation(_) => ControlFlow::Continue(()),
        },
        DFStatement::Explain(explain) => {
            for_each_mut(&mut explain.statement, visitor);
            ControlFlow::Continue(())
        }
        _ => ControlFlow::Continue(()),
    };
}
