//! The table clauses of `CREATE PROPERTY GRAPH`: the subset of SQL/PGQ Ursa
//! supports, and clear errors for the rest.
//!
//! ```text
//! CREATE [OR REPLACE] PROPERTY GRAPH [IF NOT EXISTS] name
//!   [VERTEX TABLES ( table KEY (column) )]
//!   EDGE TABLES (
//!     table SOURCE KEY (column) [REFERENCES vertex_table (key)]
//!           DESTINATION KEY (column) [REFERENCES vertex_table (key)]
//!   )
//! ```
//!
//! SQL/PGQ allows more: several vertex and edge tables, `AS alias`, `LABEL`,
//! `DEFAULT LABEL`, `PROPERTIES`, `NO PROPERTIES`, an edge table `KEY`,
//! `SOURCE vertex_table` without `KEY`, and multi-column keys. Each is
//! recognised and rejected as not supported yet, never parsed as something
//! else. `NODE TABLES` and `RELATIONSHIP TABLES`, SQL/PGQ's synonyms, are
//! accepted.

use std::fmt;

use datafusion::error::{DataFusionError, Result};
use datafusion::sql::sqlparser::ast::{Ident, ObjectName};
use datafusion::sql::sqlparser::parser::Parser;
use datafusion::sql::sqlparser::tokenizer::Token;

/// `VERTEX TABLES (table KEY (key))`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexTableDef {
    pub table: ObjectName,
    pub key: Ident,
}

/// `EDGE TABLES (table SOURCE … DESTINATION …)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeTableDef {
    pub table: ObjectName,
    pub source: EndpointDef,
    pub destination: EndpointDef,
}

/// `KEY (key) [REFERENCES table (column)]`, after `SOURCE` or `DESTINATION`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointDef {
    pub key: Ident,
    pub references: Option<References>,
}

/// `REFERENCES table (column)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct References {
    pub table: ObjectName,
    pub column: Ident,
}

impl fmt::Display for VertexTableDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VERTEX TABLES ({} KEY ({}))", self.table, self.key)
    }
}

impl fmt::Display for EdgeTableDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "EDGE TABLES ({} SOURCE {} DESTINATION {})",
            self.table, self.source, self.destination
        )
    }
}

impl fmt::Display for EndpointDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KEY ({})", self.key)?;
        if let Some(r) = &self.references {
            write!(f, " REFERENCES {} ({})", r.table, r.column)?;
        }
        Ok(())
    }
}

/// Why everything beyond the subset is rejected.
const ONE_TABLE_EACH: &str = "Ursa graphs have one edge table and at most one vertex table";

/// Parse the table clauses, after the graph's name.
pub(crate) fn parse_tables(p: &mut Parser) -> Result<(Option<VertexTableDef>, EdgeTableDef)> {
    let vertices =
        if consume_words(p, &["VERTEX", "TABLES"]) || consume_words(p, &["NODE", "TABLES"]) {
            p.expect_token(&Token::LParen)?;
            let vertices = parse_vertex_table(p)?;
            if p.peek_token_ref().token == Token::Comma {
                return unsupported(p, "several vertex tables are");
            }
            p.expect_token(&Token::RParen)?;
            Some(vertices)
        } else {
            None
        };
    if !(consume_words(p, &["EDGE", "TABLES"]) || consume_words(p, &["RELATIONSHIP", "TABLES"])) {
        let expected = if vertices.is_some() {
            "EDGE TABLES"
        } else {
            "VERTEX TABLES or EDGE TABLES"
        };
        return Ok(p.expected_ref(expected, p.peek_token_ref())?);
    }
    p.expect_token(&Token::LParen)?;
    let edges = parse_edge_table(p)?;
    if p.peek_token_ref().token == Token::Comma {
        return unsupported(p, "several edge tables are");
    }
    p.expect_token(&Token::RParen)?;
    if word(p, 0, "VERTEX") || word(p, 0, "NODE") {
        return unsupported(p, "VERTEX TABLES after EDGE TABLES is");
    }
    if word(p, 0, "EDGE") || word(p, 0, "RELATIONSHIP") {
        return unsupported(p, "several EDGE TABLES clauses are");
    }
    Ok((vertices, edges))
}

fn parse_vertex_table(p: &mut Parser) -> Result<VertexTableDef> {
    let table = p.parse_object_name(false)?;
    reject_element_options(p)?;
    if !consume_words(p, &["KEY"]) {
        if matches!(p.peek_token_ref().token, Token::RParen | Token::Comma) {
            return unsupported(
                p,
                "a vertex table without KEY is (name its key column with KEY (column))",
            );
        }
        return Ok(p.expected_ref("KEY", p.peek_token_ref())?);
    }
    let key = parse_key(p)?;
    reject_element_options(p)?;
    Ok(VertexTableDef { table, key })
}

fn parse_edge_table(p: &mut Parser) -> Result<EdgeTableDef> {
    let table = p.parse_object_name(false)?;
    reject_element_options(p)?;
    if word(p, 0, "KEY") {
        return unsupported(p, "an edge table KEY is");
    }
    expect_word(p, "SOURCE")?;
    let source = parse_endpoint(p, "SOURCE")?;
    expect_word(p, "DESTINATION")?;
    let destination = parse_endpoint(p, "DESTINATION")?;
    reject_element_options(p)?;
    Ok(EdgeTableDef {
        table,
        source,
        destination,
    })
}

fn parse_endpoint(p: &mut Parser, side: &str) -> Result<EndpointDef> {
    if !consume_words(p, &["KEY"]) {
        if matches!(p.peek_token_ref().token, Token::Word(_)) {
            return unsupported(
                p,
                &format!(
                    "{side} without KEY is (name the column with {side} KEY (column) \
                     REFERENCES vertex_table (key))"
                ),
            );
        }
        return Ok(p.expected_ref("KEY", p.peek_token_ref())?);
    }
    let key = parse_key(p)?;
    let references = if consume_words(p, &["REFERENCES"]) {
        let table = p.parse_object_name(false)?;
        let column = parse_key(p)?;
        Some(References { table, column })
    } else {
        None
    };
    Ok(EndpointDef { key, references })
}

/// `(column)`: a key of exactly one column.
fn parse_key(p: &mut Parser) -> Result<Ident> {
    p.expect_token(&Token::LParen)?;
    let column = p.parse_identifier()?;
    if p.peek_token_ref().token == Token::Comma {
        return unsupported(
            p,
            "multi-column keys are (KEY, SOURCE KEY, DESTINATION KEY and REFERENCES take one column)",
        );
    }
    p.expect_token(&Token::RParen)?;
    Ok(column)
}

/// Reject SQL/PGQ's element table options: `AS alias`, `LABEL`, `DEFAULT
/// LABEL`, `PROPERTIES`, `NO PROPERTIES`.
fn reject_element_options(p: &mut Parser) -> Result<()> {
    if word(p, 0, "AS") {
        return unsupported(p, "table aliases (AS) are");
    }
    if word(p, 0, "LABEL") || (word(p, 0, "DEFAULT") && word(p, 1, "LABEL")) {
        return unsupported(p, "labels (LABEL, DEFAULT LABEL) are");
    }
    if word(p, 0, "PROPERTIES") || (word(p, 0, "NO") && word(p, 1, "PROPERTIES")) {
        return unsupported(
            p,
            "PROPERTIES clauses are (every column of the vertex and edge tables is a property)",
        );
    }
    Ok(())
}

/// The error for a SQL/PGQ feature beyond Ursa's subset, at the next token.
fn unsupported<T>(p: &Parser, what: &str) -> Result<T> {
    Err(DataFusionError::NotImplemented(format!(
        "{what} not supported yet: {ONE_TABLE_EACH}{}",
        p.peek_token_ref().span.start
    )))
}

/// Whether the `n`th token ahead is the unquoted word `word`, in any case.
pub(crate) fn word(p: &Parser, n: usize, word: &str) -> bool {
    match &p.peek_nth_token_ref(n).token {
        Token::Word(w) => w.quote_style.is_none() && w.value.eq_ignore_ascii_case(word),
        _ => false,
    }
}

/// Consume `words` if they are next.
pub(crate) fn consume_words(p: &mut Parser, words: &[&str]) -> bool {
    let found = words.iter().enumerate().all(|(i, w)| word(p, i, w));
    if found {
        for _ in words {
            p.next_token();
        }
    }
    found
}

fn expect_word(p: &mut Parser, expected: &str) -> Result<()> {
    if consume_words(p, &[expected]) {
        Ok(())
    } else {
        Ok(p.expected_ref(expected, p.peek_token_ref())?)
    }
}

#[cfg(test)]
mod tests {
    use datafusion::sql::sqlparser::ast::ObjectNamePart;

    use super::*;
    use crate::{parse_sql, CreatePropertyGraph, UrsaStatement};

    fn create(sql: &str) -> CreatePropertyGraph {
        let mut statements = parse_sql(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(statements.len(), 1, "{sql}");
        match statements.pop().unwrap() {
            UrsaStatement::CreatePropertyGraph(g) => *g,
            other => panic!("{sql} parsed as {other:?}"),
        }
    }

    fn error(sql: &str) -> String {
        match parse_sql(sql) {
            Ok(statements) => panic!("{sql} should not parse; got {statements:?}"),
            Err(e) => ursa_engine::user_message(&e),
        }
    }

    fn name(parts: &[&str]) -> ObjectName {
        ObjectName(
            parts
                .iter()
                .map(|p| ObjectNamePart::Identifier(Ident::new(*p)))
                .collect(),
        )
    }

    fn endpoint(key: &str, references: Option<(&str, &str)>) -> EndpointDef {
        EndpointDef {
            key: Ident::new(key),
            references: references.map(|(table, column)| References {
                table: name(&[table]),
                column: Ident::new(column),
            }),
        }
    }

    const GETTING_STARTED: &str = "CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (
    ties SOURCE KEY (src) REFERENCES members (id)
         DESTINATION KEY (dst) REFERENCES members (id)
  )";

    #[test]
    fn the_getting_started_graph() {
        assert_eq!(
            create(GETTING_STARTED),
            CreatePropertyGraph {
                name: name(&["karate"]),
                or_replace: false,
                if_not_exists: false,
                vertices: Some(VertexTableDef {
                    table: name(&["members"]),
                    key: Ident::new("id"),
                }),
                edges: EdgeTableDef {
                    table: name(&["ties"]),
                    source: endpoint("src", Some(("members", "id"))),
                    destination: endpoint("dst", Some(("members", "id"))),
                },
            }
        );
    }

    #[test]
    fn accepted_forms() {
        let g = create(
            "CREATE PROPERTY GRAPH karate_edges_only
               EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
        );
        assert_eq!(g.vertices, None);
        assert_eq!(g.edges.source, endpoint("src", None));
        assert_eq!(g.edges.destination, endpoint("dst", None));

        // Keywords in any case; qualified and quoted names; REFERENCES on one side.
        let g = create(
            r#"create or replace property graph if not exists "Social"
                 node tables (db.public."People" key ("Id"))
                 relationship tables (db.follows source key (a) references db.public."People" ("Id")
                                                  destination key (b))"#,
        );
        assert!(g.or_replace && g.if_not_exists);
        assert_eq!(g.name.to_string(), r#""Social""#);
        let vertices = g.vertices.unwrap();
        assert_eq!(vertices.table.to_string(), r#"db.public."People""#);
        assert_eq!(vertices.key, Ident::with_quote('"', "Id"));
        assert_eq!(g.edges.table.to_string(), "db.follows");
        assert_eq!(
            g.edges.source.references.unwrap().table.to_string(),
            r#"db.public."People""#
        );
        assert_eq!(g.edges.destination.references, None);
    }

    #[test]
    fn display_round_trips() {
        for sql in [
            GETTING_STARTED,
            "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (a) DESTINATION KEY (b))",
            r#"CREATE OR REPLACE PROPERTY GRAPH IF NOT EXISTS "G" VERTEX TABLES (s.v KEY ("K"))
               EDGE TABLES (s.e SOURCE KEY (a) DESTINATION KEY (b) REFERENCES s.v ("K"))"#,
        ] {
            let parsed = UrsaStatement::CreatePropertyGraph(Box::new(create(sql)));
            let printed = parsed.to_string();
            let mut again = parse_sql(&printed).unwrap();
            assert_eq!(again.pop().unwrap(), parsed, "{printed}");
        }
        assert_eq!(
            UrsaStatement::CreatePropertyGraph(Box::new(create(GETTING_STARTED))).to_string(),
            "CREATE PROPERTY GRAPH karate VERTEX TABLES (members KEY (id)) EDGE TABLES (ties \
             SOURCE KEY (src) REFERENCES members (id) DESTINATION KEY (dst) REFERENCES members (id))"
        );
    }

    #[test]
    fn the_statement_after_a_graph_still_parses() {
        let sql =
            "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (a) DESTINATION KEY (b)); SELECT 1";
        assert_eq!(parse_sql(sql).unwrap().len(), 2);
    }

    #[test]
    fn sql_pgq_beyond_the_subset_is_rejected() {
        let not_supported = "not supported yet: Ursa graphs have one edge table and at most one \
                             vertex table";
        let properties =
            "PROPERTIES clauses are (every column of the vertex and edge tables is a property)";
        let multi_column =
            "multi-column keys are (KEY, SOURCE KEY, DESTINATION KEY and REFERENCES take one column)";
        for (sql, what) in [
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (a KEY (id), b KEY (id))
                   EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))",
                "several vertex tables are",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d),
                   f SOURCE KEY (s) DESTINATION KEY (d))",
                "several edge tables are",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))
                   EDGE TABLES (f SOURCE KEY (s) DESTINATION KEY (d))",
                "several EDGE TABLES clauses are",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))
                   VERTEX TABLES (v KEY (id))",
                "VERTEX TABLES after EDGE TABLES is",
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (members AS m KEY (id))
                   EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))",
                "table aliases (AS) are",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e AS ties SOURCE KEY (s) DESTINATION KEY (d))",
                "table aliases (AS) are",
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id) LABEL person)
                   EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))",
                "labels (LABEL, DEFAULT LABEL) are",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d) DEFAULT LABEL)",
                "labels (LABEL, DEFAULT LABEL) are",
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id) PROPERTIES (name))
                   EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))",
                properties,
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id) PROPERTIES ARE ALL COLUMNS)
                   EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))",
                properties,
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d) NO PROPERTIES)",
                properties,
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (a, b))
                   EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))",
                multi_column,
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s1, s2) DESTINATION KEY (d))",
                multi_column,
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id))
                   EDGE TABLES (e SOURCE KEY (s) REFERENCES v (id, x) DESTINATION KEY (d))",
                multi_column,
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e KEY (id) SOURCE KEY (s) DESTINATION KEY (d))",
                "an edge table KEY is",
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id))
                   EDGE TABLES (e SOURCE v DESTINATION v)",
                "SOURCE without KEY is (name the column with SOURCE KEY (column) REFERENCES \
                 vertex_table (key))",
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v)
                   EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))",
                "a vertex table without KEY is (name its key column with KEY (column))",
            ),
        ] {
            let message = error(sql);
            assert!(
                message.starts_with(&format!("{what} {not_supported} at Line: ")),
                "{sql}\n{message}"
            );
        }
    }

    #[test]
    fn syntax_errors_read_like_datafusions() {
        for (sql, expected) in [
            (
                "CREATE PROPERTY GRAPH g",
                "syntax error: Expected: VERTEX TABLES or EDGE TABLES, found: EOF",
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id))",
                "syntax error: Expected: EDGE TABLES, found: EOF",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES e SOURCE KEY (s) DESTINATION KEY (d)",
                "syntax error: Expected: (, found: e at Line: 1, Column: 37",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s))",
                "syntax error: Expected: DESTINATION, found: ) at Line: 1, Column: 54",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e DESTINATION KEY (d) SOURCE KEY (s))",
                "syntax error: Expected: SOURCE, found: DESTINATION at Line: 1, Column: 40",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY s DESTINATION KEY (d))",
                "syntax error: Expected: (, found: s at Line: 1, Column: 51",
            ),
            (
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d)",
                "syntax error: Expected: ), found: EOF",
            ),
            (
                "CREATE PROPERTY GRAPH g VERTEX TABLES (v ID (id)) EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))",
                "syntax error: Expected: KEY, found: ID at Line: 1, Column: 42",
            ),
        ] {
            assert_eq!(error(sql), expected, "{sql}");
        }
    }
}
