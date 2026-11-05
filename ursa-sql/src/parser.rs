//! Ursa's statement parser.
//!
//! It wraps DataFusion's [`DFParser`]: before each statement it peeks at the
//! leading words, parses Ursa's own statements with sqlparser's public helpers,
//! and hands everything else to `DFParser::parse_statement`. sqlparser knows no
//! SQL/PGQ, so `PROPERTY`, `GRAPH` and `GRAPHS` are not keywords; they are
//! matched as unquoted words, case-insensitively.

use std::fmt;

use datafusion::error::Result;
use datafusion::sql::parser::{DFParser, DFParserBuilder, Statement as DFStatement};
use datafusion::sql::sqlparser::ast::{Ident, ObjectName};
use datafusion::sql::sqlparser::dialect::{Dialect, GenericDialect};
use datafusion::sql::sqlparser::parser::ParserError;
use datafusion::sql::sqlparser::tokenizer::{Location, Token};

use crate::property_graph::{parse_tables, EdgeTableDef, VertexTableDef};

/// Same as sqlparser's and DataFusion's default.
pub const DEFAULT_RECURSION_LIMIT: usize = 50;

/// One statement of Ursa SQL.
#[derive(Debug, Clone, PartialEq)]
pub enum UrsaStatement {
    /// Anything DataFusion parses: queries, DDL, `COPY … TO`, `SET`, `EXPLAIN`.
    DataFusion(Box<DFStatement>),
    /// `CREATE [OR REPLACE] PROPERTY GRAPH [IF NOT EXISTS] name …` (SQL/PGQ).
    CreatePropertyGraph(Box<CreatePropertyGraph>),
    /// `DROP PROPERTY GRAPH [IF EXISTS] name`.
    DropPropertyGraph(DropPropertyGraph),
    /// `DESCRIBE GRAPH name` (or `DESC GRAPH name`).
    DescribeGraph(DescribeGraph),
    /// `SHOW GRAPHS`.
    ShowGraphs,
    /// `ATTACH [DATABASE] 'file.ursa' [AS name]`.
    Attach(Attach),
}

/// `CREATE PROPERTY GRAPH`: Ursa's subset of SQL/PGQ (see
/// [`crate::property_graph`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatePropertyGraph {
    pub name: ObjectName,
    pub or_replace: bool,
    pub if_not_exists: bool,
    /// `VERTEX TABLES (…)`, which may be omitted.
    pub vertices: Option<VertexTableDef>,
    /// `EDGE TABLES (…)`.
    pub edges: EdgeTableDef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropPropertyGraph {
    pub name: ObjectName,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribeGraph {
    pub name: ObjectName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attach {
    pub path: String,
    pub alias: Option<Ident>,
}

impl UrsaStatement {
    /// The statement's leading words, as an error message names it.
    pub fn kind(&self) -> &'static str {
        match self {
            UrsaStatement::DataFusion(_) => "SQL statement",
            UrsaStatement::CreatePropertyGraph(_) => "CREATE PROPERTY GRAPH",
            UrsaStatement::DropPropertyGraph(_) => "DROP PROPERTY GRAPH",
            UrsaStatement::DescribeGraph(_) => "DESCRIBE GRAPH",
            UrsaStatement::ShowGraphs => "SHOW GRAPHS",
            UrsaStatement::Attach(_) => "ATTACH",
        }
    }
}

impl fmt::Display for UrsaStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UrsaStatement::DataFusion(s) => write!(f, "{s}"),
            UrsaStatement::CreatePropertyGraph(s) => {
                write!(f, "CREATE ")?;
                if s.or_replace {
                    write!(f, "OR REPLACE ")?;
                }
                write!(f, "PROPERTY GRAPH ")?;
                if s.if_not_exists {
                    write!(f, "IF NOT EXISTS ")?;
                }
                write!(f, "{}", s.name)?;
                if let Some(vertices) = &s.vertices {
                    write!(f, " {vertices}")?;
                }
                write!(f, " {}", s.edges)
            }
            UrsaStatement::DropPropertyGraph(s) => {
                write!(f, "DROP PROPERTY GRAPH ")?;
                if s.if_exists {
                    write!(f, "IF EXISTS ")?;
                }
                write!(f, "{}", s.name)
            }
            UrsaStatement::DescribeGraph(s) => write!(f, "DESCRIBE GRAPH {}", s.name),
            UrsaStatement::ShowGraphs => write!(f, "SHOW GRAPHS"),
            UrsaStatement::Attach(s) => {
                write!(f, "ATTACH '{}'", s.path.replace('\'', "''"))?;
                if let Some(alias) = &s.alias {
                    write!(f, " AS {alias}")?;
                }
                Ok(())
            }
        }
    }
}

/// Parse `sql` (one or more statements separated by `;`) with the generic dialect.
pub fn parse_sql(sql: &str) -> Result<Vec<UrsaStatement>> {
    UrsaParser::new(sql, &GenericDialect {}, DEFAULT_RECURSION_LIMIT)?.parse_statements()
}

/// Ursa's parser: DataFusion's, with Ursa's statements recognised first.
pub struct UrsaParser<'a> {
    df: DFParser<'a>,
    /// The text being parsed, for [`Self::parse_statements_with_text`].
    sql: String,
}

impl<'a> UrsaParser<'a> {
    pub fn new(sql: &str, dialect: &'a dyn Dialect, recursion_limit: usize) -> Result<Self> {
        let df = DFParserBuilder::new(sql)
            .with_dialect(dialect)
            .with_recursion_limit(recursion_limit)
            .build()?;
        Ok(Self {
            df,
            sql: sql.to_string(),
        })
    }

    /// Parse every statement, separated by `;`. Empty statements are skipped.
    /// (The loop is `DFParser::parse_statements`, calling our `parse_statement`.)
    pub fn parse_statements(&mut self) -> Result<Vec<UrsaStatement>> {
        Ok(self
            .parse_statements_with_text()?
            .into_iter()
            .map(|(statement, _)| statement)
            .collect())
    }

    /// [`Self::parse_statements`], with each statement's text as written
    /// (from its first token to its last, without the `;`). A database file
    /// stores this text when a statement's `Display` does not round-trip.
    pub fn parse_statements_with_text(&mut self) -> Result<Vec<(UrsaStatement, String)>> {
        let mut statements = Vec::new();
        let mut expecting_delimiter = false;
        loop {
            while self.df.parser.consume_token(&Token::SemiColon) {
                expecting_delimiter = false;
            }
            let next = self.df.parser.peek_token();
            if next.token == Token::EOF {
                break;
            }
            if expecting_delimiter {
                return Err(ParserError::ParserError(format!(
                    "Expected: end of statement, found: {next}{}",
                    next.span.start
                ))
                .into());
            }
            let start = next.span.start;
            let statement = self.parse_statement()?;
            let end = self.df.parser.get_current_token().span.end;
            let text = source_text(&self.sql, start, end).unwrap_or_else(|| statement.to_string());
            statements.push((statement, text));
            expecting_delimiter = true;
        }
        Ok(statements)
    }

    /// Parse one statement.
    pub fn parse_statement(&mut self) -> Result<UrsaStatement> {
        if self.word(0, "CREATE") {
            let at = if self.word(1, "OR") && self.word(2, "REPLACE") {
                3
            } else {
                1
            };
            if self.word(at, "PROPERTY") && self.word(at + 1, "GRAPH") {
                return self.parse_create_property_graph();
            }
        } else if self.word(0, "DROP") && self.word(1, "PROPERTY") && self.word(2, "GRAPH") {
            return Ok(self.parse_drop_property_graph()?);
        } else if (self.word(0, "DESCRIBE") || self.word(0, "DESC"))
            && self.word(1, "GRAPH")
            && matches!(self.df.parser.peek_nth_token_ref(2).token, Token::Word(_))
        {
            // `DESCRIBE graph` alone still describes a table named `graph`.
            for _ in 0..2 {
                self.df.parser.next_token();
            }
            let name = self.df.parser.parse_object_name(false)?;
            return Ok(UrsaStatement::DescribeGraph(DescribeGraph { name }));
        } else if self.word(0, "SHOW") && self.word(1, "GRAPHS") {
            for _ in 0..2 {
                self.df.parser.next_token();
            }
            return Ok(UrsaStatement::ShowGraphs);
        } else if self.word(0, "ATTACH") {
            return Ok(self.parse_attach()?);
        }
        Ok(UrsaStatement::DataFusion(Box::new(
            self.df.parse_statement()?,
        )))
    }

    /// Whether the `n`th token ahead is the unquoted word `word`, in any case.
    fn word(&self, n: usize, word: &str) -> bool {
        match &self.df.parser.peek_nth_token_ref(n).token {
            Token::Word(w) => w.quote_style.is_none() && w.value.eq_ignore_ascii_case(word),
            _ => false,
        }
    }

    /// Consume the next token if it is the unquoted word `word`.
    fn consume_word(&mut self, word: &str) -> bool {
        let found = self.word(0, word);
        if found {
            self.df.parser.next_token();
        }
        found
    }

    fn consume_words(&mut self, words: &[&str]) -> bool {
        let found = words.iter().enumerate().all(|(i, w)| self.word(i, w));
        if found {
            for _ in words {
                self.df.parser.next_token();
            }
        }
        found
    }

    fn parse_create_property_graph(&mut self) -> Result<UrsaStatement> {
        self.df.parser.next_token(); // CREATE
        let or_replace = self.consume_words(&["OR", "REPLACE"]);
        self.consume_words(&["PROPERTY", "GRAPH"]);
        let if_not_exists = self.consume_words(&["IF", "NOT", "EXISTS"]);
        let name = self.df.parser.parse_object_name(false)?;
        let (vertices, edges) = parse_tables(&mut self.df.parser)?;
        Ok(UrsaStatement::CreatePropertyGraph(Box::new(
            CreatePropertyGraph {
                name,
                or_replace,
                if_not_exists,
                vertices,
                edges,
            },
        )))
    }

    fn parse_drop_property_graph(&mut self) -> Result<UrsaStatement, ParserError> {
        self.consume_words(&["DROP", "PROPERTY", "GRAPH"]);
        let if_exists = self.consume_words(&["IF", "EXISTS"]);
        let name = self.df.parser.parse_object_name(false)?;
        Ok(UrsaStatement::DropPropertyGraph(DropPropertyGraph {
            name,
            if_exists,
        }))
    }

    fn parse_attach(&mut self) -> Result<UrsaStatement, ParserError> {
        self.consume_word("ATTACH");
        self.consume_word("DATABASE");
        let path = self.df.parser.parse_literal_string()?;
        let alias = if self.consume_word("AS") {
            Some(self.df.parser.parse_identifier()?)
        } else {
            None
        };
        Ok(UrsaStatement::Attach(Attach { path, alias }))
    }
}

/// The text of `sql` between two token locations, or `None` if a location is
/// unknown (line 0) or outside the text.
fn source_text(sql: &str, start: Location, end: Location) -> Option<String> {
    let from = byte_offset(sql, start)?;
    let to = byte_offset(sql, end)?;
    (from < to).then(|| sql[from..to].trim().to_string())
}

/// The byte offset of a tokenizer location (1-based line and column, columns
/// counted in characters).
fn byte_offset(sql: &str, at: Location) -> Option<usize> {
    if at.line == 0 || at.column == 0 {
        return None;
    }
    let mut line_start = 0;
    for _ in 1..at.line {
        line_start += sql[line_start..].find('\n')? + 1;
    }
    let line = &sql[line_start..];
    let column = usize::try_from(at.column - 1).ok()?;
    match line.char_indices().nth(column) {
        Some((offset, _)) => Some(line_start + offset),
        None if line.chars().count() == column => Some(sql.len()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(sql: &str) -> UrsaStatement {
        let mut statements = parse_sql(sql).unwrap();
        assert_eq!(statements.len(), 1, "{sql}");
        statements.pop().unwrap()
    }

    #[test]
    fn create_property_graph() {
        let sql = "CREATE PROPERTY GRAPH karate
              VERTEX TABLES (members KEY (id))
              EDGE TABLES (
                ties SOURCE KEY (src) REFERENCES members (id)
                     DESTINATION KEY (dst) REFERENCES members (id)
              )";
        let UrsaStatement::CreatePropertyGraph(g) = one(sql) else {
            panic!("not a CREATE PROPERTY GRAPH")
        };
        assert_eq!(g.name.to_string(), "karate");
        assert!(!g.or_replace && !g.if_not_exists);

        let s = one("create or replace property graph if not exists db.social edge tables (e source key (a) destination key (b))");
        let UrsaStatement::CreatePropertyGraph(g) = s else {
            panic!("not a CREATE PROPERTY GRAPH")
        };
        assert_eq!(g.name.to_string(), "db.social");
        assert!(g.or_replace && g.if_not_exists);
    }

    #[test]
    fn drop_describe_show_attach() {
        assert_eq!(
            one("DROP PROPERTY GRAPH IF EXISTS karate"),
            UrsaStatement::DropPropertyGraph(DropPropertyGraph {
                name: ObjectName::from(vec![Ident::new("karate")]),
                if_exists: true,
            })
        );
        assert_eq!(
            one("describe graph karate").to_string(),
            "DESCRIBE GRAPH karate"
        );
        assert_eq!(one("DESC GRAPH karate").kind(), "DESCRIBE GRAPH");
        assert_eq!(one("SHOW GRAPHS"), UrsaStatement::ShowGraphs);
        assert_eq!(
            one("ATTACH 'club.ursa' AS club"),
            UrsaStatement::Attach(Attach {
                path: "club.ursa".into(),
                alias: Some(Ident::new("club")),
            })
        );
        assert_eq!(
            one("attach database 'club.ursa'").to_string(),
            "ATTACH 'club.ursa'"
        );
    }

    #[test]
    fn everything_else_is_datafusion() {
        for sql in [
            "SELECT * FROM 'edges.csv' LIMIT 3",
            "COPY (SELECT 1) TO 'out.parquet'",
            "CREATE VIEW ties AS SELECT * FROM 'karate_edges.csv'",
            "DESCRIBE graph",
            "SHOW TABLES",
            "CREATE TABLE property (graph INT)",
        ] {
            assert!(
                matches!(one(sql), UrsaStatement::DataFusion(_)),
                "{sql} should go to DataFusion"
            );
        }
    }

    #[test]
    fn statements_split_on_semicolons() {
        let statements = parse_sql(
            "SELECT 1; ; CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (a) DESTINATION KEY (b));
             SHOW GRAPHS; SELECT ';' AS semi;",
        )
        .unwrap();
        let kinds: Vec<_> = statements.iter().map(UrsaStatement::kind).collect();
        assert_eq!(
            kinds,
            [
                "SQL statement",
                "CREATE PROPERTY GRAPH",
                "SHOW GRAPHS",
                "SQL statement"
            ]
        );
    }

    #[test]
    fn statements_keep_their_text() {
        let sql =
            "SELECT 1;\n  CREATE EXTERNAL TABLE t (a INT)\n  STORED AS CSV LOCATION 'é.csv'  ;\
                   CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (a) DESTINATION KEY (b))";
        let statements = UrsaParser::new(sql, &GenericDialect {}, DEFAULT_RECURSION_LIMIT)
            .unwrap()
            .parse_statements_with_text()
            .unwrap();
        let texts: Vec<_> = statements.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(
            texts,
            [
                "SELECT 1",
                "CREATE EXTERNAL TABLE t (a INT)\n  STORED AS CSV LOCATION 'é.csv'",
                "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (a) DESTINATION KEY (b))",
            ]
        );
    }

    #[test]
    fn missing_delimiter_is_a_syntax_error() {
        let err = parse_sql("SHOW GRAPHS SELECT 1").unwrap_err();
        assert!(err.to_string().contains("end of statement"), "{err}");
    }
}
