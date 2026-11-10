//! When typed input is a complete statement.
//!
//! The shell reads SQL a line at a time and runs it once it ends with a `;`
//! outside quotes and comments, as DuckDB's and datafusion-cli's shells do.
//! The text is split into tokens by sqlparser's tokenizer, the one
//! [`ursa_sql::UrsaParser`] parses with, so quoting and comments are read
//! exactly as the parser reads them. Whether the statements are valid is left
//! to the parser when they run: a mistake prints an error, and the text stays
//! in the history to fix.

use datafusion::sql::sqlparser::dialect::Dialect;
use datafusion::sql::sqlparser::tokenizer::{Token, Tokenizer};
use ursa_engine::error::matched;

/// What a buffer of typed SQL is, so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Only whitespace and comments: nothing to run.
    Empty,
    /// More is to come: there is no final `;`, or a quote or comment is open.
    Incomplete,
    /// Ends with a `;` outside quotes and comments.
    Complete,
}

/// The status of `sql`, tokenized with `dialect`.
pub fn status(sql: &str, dialect: &dyn Dialect) -> Status {
    match Tokenizer::new(dialect, sql).tokenize() {
        Ok(tokens) => {
            let last = tokens
                .iter()
                .rev()
                .find(|t| !matches!(t, Token::Whitespace(_) | Token::EOF));
            match last {
                None => Status::Empty,
                Some(Token::SemiColon) => Status::Complete,
                Some(_) => Status::Incomplete,
            }
        }
        // A string, quoted name or block comment still open at the end:
        // the next line may close it.
        Err(e)
            if e.message.contains(matched::TOKENIZER_EOF)
                || e.message.starts_with(matched::TOKENIZER_UNTERMINATED) =>
        {
            Status::Incomplete
        }
        // Any other tokenizer error is the parser's to report, once the
        // statement is ended.
        Err(_) if sql.trim_end().ends_with(';') => Status::Complete,
        Err(_) => Status::Incomplete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::sql::sqlparser::dialect::GenericDialect;

    fn of(sql: &str) -> Status {
        status(sql, &GenericDialect {})
    }

    #[test]
    fn a_semicolon_ends_a_statement() {
        assert_eq!(of("SELECT 1;"), Status::Complete);
        assert_eq!(of("SELECT 1;  \n"), Status::Complete);
        assert_eq!(of("SELECT 1"), Status::Incomplete);
        assert_eq!(of("SELECT 1;\nSELECT 2"), Status::Incomplete);
        assert_eq!(of("SELECT 1; SELECT 2;"), Status::Complete);
        assert_eq!(of(";"), Status::Complete);
    }

    #[test]
    fn statements_span_lines() {
        assert_eq!(of("SELECT id\nFROM t\n"), Status::Incomplete);
        assert_eq!(of("SELECT id\nFROM t\nLIMIT 3;\n"), Status::Complete);
        assert_eq!(
            of("CREATE PROPERTY GRAPH g\n  EDGE TABLES (e SOURCE KEY (a) DESTINATION KEY (b));"),
            Status::Complete
        );
    }

    #[test]
    fn semicolons_in_quotes_do_not_count() {
        assert_eq!(of("SELECT ';"), Status::Incomplete);
        assert_eq!(of("SELECT ';\n';"), Status::Complete);
        assert_eq!(of("SELECT 'it''s;"), Status::Incomplete);
        assert_eq!(of("SELECT 'it''s';"), Status::Complete);
        assert_eq!(of("SELECT \"a;"), Status::Incomplete);
        assert_eq!(of("SELECT \"a;\" FROM t;"), Status::Complete);
    }

    #[test]
    fn semicolons_in_comments_do_not_count() {
        assert_eq!(of("SELECT 1 -- done;"), Status::Incomplete);
        assert_eq!(of("SELECT 1 /* done; */"), Status::Incomplete);
        assert_eq!(of("SELECT 1 /* done;"), Status::Incomplete);
        assert_eq!(of("SELECT 1 /* a\n b */;"), Status::Complete);
        // A comment after the `;` does not reopen the statement.
        assert_eq!(of("SELECT 1; -- the answer"), Status::Complete);
        assert_eq!(of("SELECT 1; /* the answer */"), Status::Complete);
    }

    #[test]
    fn whitespace_and_comments_alone_are_empty() {
        assert_eq!(of(""), Status::Empty);
        assert_eq!(of("   \n\t"), Status::Empty);
        assert_eq!(of("-- a note"), Status::Empty);
        assert_eq!(of("/* a note */"), Status::Empty);
    }
}
