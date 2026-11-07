//! Ursa's statement parser.
//!
//! It wraps DataFusion's [`DFParser`]: before each statement it peeks at the
//! leading words, parses Ursa's own statements with sqlparser's public helpers,
//! and hands everything else to `DFParser::parse_statement`. sqlparser knows no
//! SQL/PGQ, so `PROPERTY`, `GRAPH` and `GRAPHS` are not keywords; they are
//! matched as unquoted words, case-insensitively.

use std::fmt;

use datafusion::error::{DataFusionError, Result};
use datafusion::sql::parser::{DFParser, DFParserBuilder, Statement as DFStatement};
use datafusion::sql::sqlparser::ast::{Ident, ObjectName};
use datafusion::sql::sqlparser::dialect::{Dialect, GenericDialect};
use datafusion::sql::sqlparser::parser::ParserError;
use datafusion::sql::sqlparser::tokenizer::{Location, Token, Tokenizer};

use crate::property_graph::{parse_tables, write_tables, EdgeTableDef, VertexTableDef};
use crate::source_map::SourceMap;

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
    /// `VERTEX TABLES (…)`: empty when the clause is left out.
    pub vertices: Vec<VertexTableDef>,
    /// `EDGE TABLES (…)`: empty when the clause is left out.
    pub edges: Vec<EdgeTableDef>,
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
                write_tables(f, &s.vertices, &s.edges)
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
    dialect: &'a dyn Dialect,
    /// The text being parsed, for [`Self::parse_statements_with_text`].
    sql: String,
    /// How `sql` was rewritten from the user's text, so a syntax error names
    /// a place in what the user wrote.
    map: SourceMap,
}

impl<'a> UrsaParser<'a> {
    pub fn new(sql: &str, dialect: &'a dyn Dialect, recursion_limit: usize) -> Result<Self> {
        // SQL/PGQ's GRAPH_TABLE ( … ), which sqlparser cannot parse, becomes
        // the table function graph_table('…') first (see `graph_table`).
        let mut map = SourceMap::new(sql);
        map.push(crate::graph_table::rewrite_graph_tables_mapped(
            sql, dialect,
        ));
        // DuckDB's `FROM t [WHERE …]` becomes `SELECT * FROM t [WHERE …]`.
        map.push(crate::from_first::rewrite_from_first_mapped(
            map.text(),
            dialect,
        ));
        let sql = map.text().to_string();
        let df = DFParserBuilder::new(sql.as_str())
            .with_dialect(dialect)
            .with_recursion_limit(recursion_limit)
            .build()
            .map_err(|e| in_user_text(e, &map, dialect))?;
        Ok(Self {
            df,
            dialect,
            sql,
            map,
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
        let statements = self.parse_all().map_err(|e| match e {
            // A syntax error in a statement that uses DuckDB's `USING SAMPLE`
            // says what to write instead.
            DataFusionError::SQL(..) if self.uses_sample_clause() => {
                DataFusionError::NotImplemented(USING_SAMPLE.to_string())
            }
            e => in_user_text(e, &self.map, self.dialect),
        })?;
        // sqlparser accepts TABLESAMPLE, but DataFusion ignores it and reads
        // every row: a silently wrong answer. Refuse it instead.
        if self.uses_tablesample() {
            return Err(DataFusionError::NotImplemented(TABLESAMPLE.to_string()));
        }
        Ok(statements)
    }

    /// Whether the text holds the word `TABLESAMPLE`, outside quotes and
    /// comments. Tokenizes only when the text contains it at all.
    fn uses_tablesample(&self) -> bool {
        if !self.sql.to_ascii_uppercase().contains("TABLESAMPLE") {
            return false;
        }
        let Ok(tokens) = Tokenizer::new(self.dialect, &self.sql).tokenize() else {
            return false;
        };
        tokens.iter().any(|t| {
            matches!(t, Token::Word(w) if w.quote_style.is_none()
                && w.value.eq_ignore_ascii_case("TABLESAMPLE"))
        })
    }

    fn parse_all(&mut self) -> Result<Vec<(UrsaStatement, String)>> {
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
        if let Some(message) = self.duckdb_statement() {
            return Err(DataFusionError::NotImplemented(message.to_string()));
        }
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
        } else if (self.word(0, "DESCRIBE") || self.word(0, "DESC"))
            && self.word(1, "PROPERTY")
            && self.word(2, "GRAPH")
        {
            // DuckPGQ's spelling of `DESCRIBE GRAPH`.
            for _ in 0..3 {
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

    /// The refusal for a DuckDB statement Ursa does not have, if the next
    /// statement is one (see [`DUCKDB_STATEMENTS`]).
    fn duckdb_statement(&self) -> Option<&'static str> {
        DUCKDB_STATEMENTS
            .iter()
            .find(|(word, _)| self.word(0, word))
            .filter(|(word, _)| !(*word == "LOAD" && self.word(1, "DATA")))
            .map(|(_, message)| *message)
    }

    /// Whether the text holds `USING SAMPLE`, outside quotes and comments.
    /// Only called once parsing has failed, so a statement that parses pays
    /// nothing for it.
    fn uses_sample_clause(&self) -> bool {
        let Ok(tokens) = Tokenizer::new(self.dialect, &self.sql).tokenize() else {
            return false;
        };
        let is = |token: &Token, word: &str| {
            matches!(token, Token::Word(w) if w.quote_style.is_none()
                && w.value.eq_ignore_ascii_case(word))
        };
        let mut words = tokens
            .iter()
            .filter(|t| !matches!(t, Token::Whitespace(_)))
            .peekable();
        while let Some(token) = words.next() {
            if is(token, "USING") && words.peek().is_some_and(|next| is(next, "SAMPLE")) {
                return true;
            }
        }
        false
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

/// DuckDB statements Ursa does not have, by their first word, with what to
/// write instead. Without these, sqlparser's own error ("Expected: an SQL
/// statement, found: SUMMARIZE") or DataFusion's ("Unsupported SQL
/// statement") would be all a user, or an agent that knows DuckDB, sees.
const DUCKDB_STATEMENTS: &[(&str, &str)] = &[
    (
        "SUMMARIZE",
        "SUMMARIZE is not supported; use DESCRIBE t for the columns and their types, and \
         SELECT count(*), min(c), max(c), avg(c) FROM t for a column's statistics",
    ),
    (
        "PIVOT",
        "PIVOT is not supported; use GROUP BY with one aggregate per column, as \
         sum(x) FILTER (WHERE k = 'a') AS a",
    ),
    (
        "UNPIVOT",
        "UNPIVOT is not supported; use UNION ALL of one SELECT per column",
    ),
    (
        "INSTALL",
        "INSTALL is not supported: Ursa has no extensions; Parquet, CSV, JSON and object \
         storage are built in",
    ),
    (
        "LOAD",
        "LOAD is not supported: Ursa has no extensions; Parquet, CSV, JSON and object \
         storage are built in",
    ),
    (
        "PRAGMA",
        "PRAGMA is not supported; use SHOW ALL, SHOW name or SET name = value for settings, \
         and SELECT * FROM ursa_functions() for the graph functions",
    ),
];

/// The refusal for `TABLESAMPLE`, which DataFusion parses but ignores.
const TABLESAMPLE: &str = "TABLESAMPLE is not supported (it would be ignored and every row read); \
     use ORDER BY random() LIMIT n for a sample of n rows";

/// The refusal for DuckDB's `USING SAMPLE` clause.
const USING_SAMPLE: &str =
    "USING SAMPLE is not supported; use ORDER BY random() LIMIT n for a sample of n rows";

/// The text of `sql` between two token locations, or `None` if a location is
/// unknown (line 0) or outside the text.
fn source_text(sql: &str, start: Location, end: Location) -> Option<String> {
    let from = byte_offset(sql, start)?;
    let to = byte_offset(sql, end)?;
    (from < to).then(|| sql[from..to].trim().to_string())
}

/// `e` with the locations in a syntax error moved from the rewritten text to
/// the user's (see [`SourceMap`]), and a hint when a reserved word seems to
/// be used as a name there ([`reserved_word_hint`]).
fn in_user_text(e: DataFusionError, map: &SourceMap, dialect: &dyn Dialect) -> DataFusionError {
    match e {
        DataFusionError::SQL(e, backtrace) => {
            let e = match *e {
                ParserError::ParserError(m) => {
                    let mut m = map.user_message(&m);
                    if let Some(hint) = reserved_word_hint(&m, map.original(), dialect) {
                        m.push_str(&hint);
                    }
                    ParserError::ParserError(m)
                }
                ParserError::TokenizerError(m) => ParserError::TokenizerError(map.user_message(&m)),
                other => other,
            };
            DataFusionError::SQL(Box::new(e), backtrace)
        }
        other => other,
    }
}

/// When a syntax error at "Line: L, Column: C" of `sql` looks like a reserved
/// word used as a name, a hint to quote it: the word is next to the error's
/// token, inside a list (after `(` or `,`, or before `,` or `)`), or after
/// `SELECT`. `SELECT from, to FROM t` gives "`from` is a reserved word; to
/// use it as a name, quote it: "from"".
fn reserved_word_hint(message: &str, sql: &str, dialect: &dyn Dialect) -> Option<String> {
    use datafusion::sql::sqlparser::keywords::{Keyword, RESERVED_FOR_COLUMN_ALIAS};

    let at = message.rfind("Line: ")?;
    let (line, column) = message[at + "Line: ".len()..].split_once(", Column: ")?;
    let line: u64 = line.parse().ok()?;
    let column: u64 = column
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    let tokens = Tokenizer::new(dialect, sql).tokenize_with_location().ok()?;
    let tokens: Vec<_> = tokens
        .into_iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .collect();
    let here = tokens
        .iter()
        .position(|t| t.span.start.line == line && t.span.start.column == column)
        .unwrap_or(tokens.len());
    let reserved = |i: usize| -> Option<&str> {
        match &tokens.get(i)?.token {
            Token::Word(w)
                if w.quote_style.is_none()
                    && w.keyword != Keyword::NoKeyword
                    && RESERVED_FOR_COLUMN_ALIAS.contains(&w.keyword) =>
            {
                Some(w.value.as_str())
            }
            _ => None,
        }
    };
    let is = |i: Option<usize>, kinds: &[Token]| {
        i.and_then(|i| tokens.get(i))
            .is_some_and(|t| kinds.contains(&t.token))
    };
    let is_select = |i: Option<usize>| {
        i.and_then(|i| tokens.get(i))
            .is_some_and(|t| matches!(&t.token, Token::Word(w) if w.keyword == Keyword::SELECT))
    };
    let list_end = [Token::Comma, Token::RParen, Token::EOF, Token::SemiColon];
    let list_start = [Token::Comma, Token::LParen];
    // The word before the error's token, ending a list item; or the error's
    // token itself, starting one.
    let word = here
        .checked_sub(1)
        .filter(|&before| {
            (here >= tokens.len() || is(Some(here), &list_end))
                && (is(before.checked_sub(1), &list_start) || is_select(before.checked_sub(1)))
        })
        .and_then(reserved)
        .or_else(|| {
            (is(here.checked_sub(1), &list_start) || is_select(here.checked_sub(1)))
                .then(|| reserved(here))
                .flatten()
        })?;
    Some(format!(
        ". `{word}` is a reserved word; to use it as a name, quote it: \"{word}\""
    ))
}

/// The byte offset of a tokenizer location (1-based line and column, columns
/// counted in characters).
pub(crate) fn byte_offset(sql: &str, at: Location) -> Option<usize> {
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

    fn syntax_error(sql: &str) -> String {
        ursa_engine::user_message(&parse_sql(sql).unwrap_err())
    }

    /// #328: a key column named `from` is a column, not a FROM-first query.
    #[test]
    fn a_key_column_may_be_named_from() {
        let UrsaStatement::CreatePropertyGraph(g) =
            one("CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (from) DESTINATION KEY (to))")
        else {
            panic!("not a CREATE PROPERTY GRAPH")
        };
        assert_eq!(g.edges[0].source.key.value, "from");
        assert_eq!(g.edges[0].destination.key.value, "to");
    }

    /// #328: syntax errors name a place in the user's text, not in the text
    /// the GRAPH_TABLE and FROM-first rewrites made of it.
    #[test]
    fn syntax_errors_point_at_the_users_text() {
        let sql =
            "FROM GRAPH_TABLE (g MATCH (a WHERE a.name = 'x') COLUMNS (a.id AS s)) t WHERE s = = 1";
        let column = sql.rfind("= =").unwrap() + 3;
        assert_eq!(
            syntax_error(sql),
            format!("syntax error: Expected: an expression, found: = at Line: 1, Column: {column}")
        );
        assert_eq!(
            syntax_error("FROM t\nWHERE s = = 1"),
            "syntax error: Expected: an expression, found: = at Line: 2, Column: 11"
        );
    }

    /// #328: a reserved word used as a name says to quote it.
    #[test]
    fn a_reserved_word_as_a_name_says_to_quote_it() {
        assert_eq!(
            syntax_error("SELECT from, to FROM t"),
            "syntax error: Expected: identifier, found: , at Line: 1, Column: 12. `from` is a \
             reserved word; to use it as a name, quote it: \"from\""
        );
        // Not for other syntax errors.
        assert!(!syntax_error("SELECT a FROM t WHERE s = = 1").contains("reserved"));
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

    #[test]
    fn tablesample_is_refused_not_ignored() {
        for sql in [
            "SELECT * FROM t TABLESAMPLE SYSTEM (10)",
            "select * from t tablesample bernoulli (5 percent)",
        ] {
            let err = parse_sql(sql).unwrap_err().to_string();
            assert!(err.contains("TABLESAMPLE is not supported"), "{sql}: {err}");
        }
        // The word in a string or a quoted name is not the clause.
        parse_sql("SELECT 'tablesample' AS x").unwrap();
        parse_sql("SELECT \"TABLESAMPLE\" FROM t").unwrap();
    }

    #[test]
    fn duckdb_statements_say_what_to_write_instead() {
        let message = |sql: &str| parse_sql(sql).unwrap_err().to_string();
        for (sql, start) in [
            ("SUMMARIZE t", "SUMMARIZE is not supported; use DESCRIBE t"),
            ("summarize SELECT 1", "SUMMARIZE is not supported;"),
            (
                "PIVOT t ON k USING sum(x)",
                "PIVOT is not supported; use GROUP BY",
            ),
            ("UNPIVOT t ON a, b", "UNPIVOT is not supported;"),
            (
                "INSTALL httpfs",
                "INSTALL is not supported: Ursa has no extensions",
            ),
            (
                "LOAD httpfs",
                "LOAD is not supported: Ursa has no extensions",
            ),
            ("PRAGMA version", "PRAGMA is not supported; use SHOW ALL"),
            (
                "SELECT * FROM t USING SAMPLE 10",
                "USING SAMPLE is not supported;",
            ),
            (
                "SELECT * FROM t USING SAMPLE 10%",
                "USING SAMPLE is not supported;",
            ),
            (
                "SELECT 1; SELECT * FROM (SELECT * FROM t USING SAMPLE 5 ROWS)",
                "USING SAMPLE is not supported; use ORDER BY random() LIMIT n",
            ),
        ] {
            let message = message(sql);
            assert!(message.contains(start), "{sql}\n{message}");
        }
        // Not when the words are data, or when the error is elsewhere.
        assert!(parse_sql("SELECT 'USING SAMPLE' AS summarize").is_ok());
        let message = message("SELEC 'USING SAMPLE'");
        assert!(message.contains("Expected: an SQL statement"), "{message}");
    }
}
