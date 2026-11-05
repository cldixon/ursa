//! The file paths in a statement that defines an object, and rewriting them.
//!
//! A database stores each definition with its local file paths rewritten
//! relative to the database's directory, and replay resolves them against
//! that directory again (see [`crate::database`]). The paths a definition
//! holds are:
//!
//! - in `CREATE VIEW`, every table in its query named by a quoted name that
//!   reads as a location (`FROM 'karate_edges.csv'`, `JOIN "data/x.parquet"`),
//!   in subqueries, joins and CTEs too;
//! - in `CREATE EXTERNAL TABLE`, its `LOCATION`s;
//! - in `CREATE PROPERTY GRAPH`, a vertex or edge table (or a `REFERENCES`
//!   table) named by such a name.
//!
//! A quoted name that is also a table or view in the session is that table,
//! not a file, as when the statement is planned, so it is left alone.
//!
//! The rewrite works on the parsed statement, which is then rendered with
//! `Display`. When `Display` does not round-trip, the stored text is the
//! statement as written, and [`rewrite_text`] replaces the paths in it token
//! by token, keeping the result only if it parses back to the rewritten
//! statement.

use std::ops::ControlFlow;

use datafusion::common::TableReference;
use datafusion::prelude::SessionContext;
use datafusion::sql::parser::Statement as DFStatement;
use datafusion::sql::sqlparser::ast::{
    visit_relations_mut, Ident, ObjectName, ObjectNamePart, Statement as SQLStatement,
};
use datafusion::sql::sqlparser::dialect::dialect_from_str;
use datafusion::sql::sqlparser::tokenizer::{Token, Tokenizer};
use ursa_engine::paths::looks_like_location;

use crate::parse_sql_for;
use crate::parser::{byte_offset, UrsaStatement};

/// One path rewritten: what the statement had, and what it has now.
pub(crate) type Replacement = (String, String);

/// Rewrite each file path in `statement` that `map` changes (`map` returns
/// `None` to keep a path). Returns the paths changed, in order.
pub(crate) fn rewrite_locations(
    ctx: &SessionContext,
    statement: &mut UrsaStatement,
    mut map: impl FnMut(&str) -> Option<String>,
) -> Vec<Replacement> {
    let mut changed = Vec::new();
    let mut rewrite = |location: &mut String| {
        if let Some(new) = map(location) {
            changed.push((std::mem::replace(location, new.clone()), new));
        }
    };
    match statement {
        UrsaStatement::DataFusion(statement) => match statement.as_mut() {
            DFStatement::CreateExternalTable(table) => {
                for location in &mut table.locations {
                    rewrite(location);
                }
            }
            DFStatement::Statement(s) => {
                if let SQLStatement::CreateView(view) = s.as_mut() {
                    let _ = visit_relations_mut(view.query.as_mut(), |name| {
                        if let Some(ident) = file_name(ctx, name) {
                            rewrite(&mut ident.value);
                        }
                        ControlFlow::<()>::Continue(())
                    });
                }
            }
            _ => {}
        },
        UrsaStatement::CreatePropertyGraph(graph) => {
            let mut names = vec![&mut graph.edges.table];
            if let Some(vertices) = &mut graph.vertices {
                names.push(&mut vertices.table);
            }
            for endpoint in [&mut graph.edges.source, &mut graph.edges.destination] {
                if let Some(references) = &mut endpoint.references {
                    names.push(&mut references.table);
                }
            }
            for name in names {
                if let Some(ident) = file_name(ctx, name) {
                    rewrite(&mut ident.value);
                }
            }
        }
        _ => {}
    }
    changed
}

/// The identifier of `name` if it names a file: one quoted part that reads
/// as a location and is not a table or view in the session.
fn file_name<'a>(ctx: &SessionContext, name: &'a mut ObjectName) -> Option<&'a mut Ident> {
    let [ObjectNamePart::Identifier(ident)] = name.0.as_mut_slice() else {
        return None;
    };
    let is_file = ident.quote_style.is_some()
        && looks_like_location(&ident.value)
        && !ctx
            .table_exist(TableReference::bare(ident.value.as_str()))
            .unwrap_or(false);
    is_file.then_some(ident)
}

/// `text`, the statement as written, with each path in `replacements`
/// replaced, if the result parses back to `expected` (the statement with its
/// paths rewritten). Every string literal or quoted name equal to a replaced
/// path is replaced, then the result is checked, so a path that also appears
/// as something else (an option's value) gives `None`, never a wrong text.
pub(crate) fn rewrite_text(
    ctx: &SessionContext,
    text: &str,
    replacements: &[Replacement],
    expected: &UrsaStatement,
) -> Option<String> {
    let dialect_name = ctx.state().config().options().sql_parser.dialect;
    let dialect = dialect_from_str(dialect_name)?;
    let tokens = Tokenizer::new(dialect.as_ref(), text)
        .tokenize_with_location()
        .ok()?;
    let replacement = |value: &str| {
        replacements
            .iter()
            .find(|(old, _)| old == value)
            .map(|(_, new)| new)
    };
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for token in &tokens {
        let (value, quote) = match &token.token {
            Token::SingleQuotedString(s) => (s, '\''),
            Token::Word(w) => match w.quote_style {
                Some(q @ ('"' | '\'' | '`')) => (&w.value, q),
                _ => continue,
            },
            _ => continue,
        };
        let Some(new) = replacement(value) else {
            continue;
        };
        let start = byte_offset(text, token.span.start)?;
        let end = byte_offset(text, token.span.end)?;
        if start < copied || end <= start {
            return None;
        }
        out.push_str(&text[copied..start]);
        out.push_str(&Ident::with_quote(quote, new.as_str()).to_string());
        copied = end;
    }
    out.push_str(&text[copied..]);
    let parsed = parse_sql_for(ctx, &out).ok()?;
    (parsed.len() == 1 && parsed[0] == *expected).then_some(out)
}
