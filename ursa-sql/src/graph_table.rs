//! SQL/PGQ's `GRAPH_TABLE ( … )`, rewritten before sqlparser sees it (#296).
//!
//! sqlparser has no SQL/PGQ grammar, and forking it is ruled out. So, as
//! DuckPGQ puts a grammar extension in front of DuckDB's parser, Ursa rewrites
//! each `GRAPH_TABLE ( body )` (or `GRAPH TABLE ( body )`) in a statement's
//! text into an ordinary table function call, `graph_table('body')`, before
//! the statement is parsed. The engine's relation planner parses the body (the
//! graph's name, `MATCH`, the pattern, `WHERE` and `COLUMNS`) and plans it.
//!
//! The rewrite works on sqlparser's own tokens, so a `GRAPH_TABLE` inside a
//! string or a comment is left alone, and the matching `)` is found by counting
//! brackets over tokens, which strings and comments cannot unbalance. Text
//! outside each `GRAPH_TABLE ( … )` is kept byte for byte.

use datafusion::sql::sqlparser::dialect::Dialect;
use datafusion::sql::sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer};

use crate::parser::byte_offset;
use crate::source_map::Rewritten;

/// The table function a `GRAPH_TABLE ( … )` becomes.
pub const GRAPH_TABLE_FUNCTION: &str = "graph_table";

/// `sql` with every `GRAPH_TABLE ( body )` rewritten to `graph_table('body')`.
/// Text that does not tokenize is returned as it is, for the parser to report.
pub fn rewrite_graph_tables(sql: &str, dialect: &dyn Dialect) -> String {
    rewrite_graph_tables_mapped(sql, dialect).text
}

/// [`rewrite_graph_tables`], with a record of what it replaced.
pub(crate) fn rewrite_graph_tables_mapped(sql: &str, dialect: &dyn Dialect) -> Rewritten {
    let mut out = Rewritten::new(sql.len());
    let Ok(tokens) = Tokenizer::new(dialect, sql).tokenize_with_location() else {
        return out.finish(sql);
    };
    // The tokens that are not whitespace or comments.
    let tokens: Vec<&TokenWithSpan> = tokens
        .iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .collect();
    let mut i = 0;
    while i < tokens.len() {
        let Some(open) = graph_table_at(&tokens, i) else {
            i += 1;
            continue;
        };
        let Some(close) = matching_paren(&tokens, open) else {
            break; // unbalanced: let the parser report it
        };
        let (Some(start), Some(body_start), Some(body_end), Some(end)) = (
            byte_offset(sql, tokens[i].span.start),
            byte_offset(sql, tokens[open].span.end),
            byte_offset(sql, tokens[close].span.start),
            byte_offset(sql, tokens[close].span.end),
        ) else {
            break;
        };
        let body = sql[body_start..body_end].trim();
        let call = format!("{GRAPH_TABLE_FUNCTION}('{}')", body.replace('\'', "''"));
        out.replace(sql, start, end, &call);
        i = close + 1;
    }
    out.finish(sql)
}

/// If a `GRAPH_TABLE (` or `GRAPH TABLE (` starts at token `i`, the index of
/// its `(`.
fn graph_table_at(tokens: &[&TokenWithSpan], i: usize) -> Option<usize> {
    let word = |n: usize, w: &str| match tokens.get(n).map(|t| &t.token) {
        Some(Token::Word(word)) => word.quote_style.is_none() && word.value.eq_ignore_ascii_case(w),
        _ => false,
    };
    let paren = |n: usize| matches!(tokens.get(n).map(|t| &t.token), Some(Token::LParen));
    if word(i, "GRAPH_TABLE") && paren(i + 1) {
        Some(i + 1)
    } else if word(i, "GRAPH") && word(i + 1, "TABLE") && paren(i + 2) {
        Some(i + 2)
    } else {
        None
    }
}

/// The index of the `)` that closes the `(` at `open`.
fn matching_paren(tokens: &[&TokenWithSpan], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (n, token) in tokens.iter().enumerate().skip(open) {
        match token.token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Some(n);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use datafusion::sql::sqlparser::dialect::GenericDialect;

    use super::*;

    fn rewrite(sql: &str) -> String {
        rewrite_graph_tables(sql, &GenericDialect {})
    }

    #[test]
    fn a_graph_table_becomes_a_table_function_call() {
        assert_eq!(
            rewrite(
                "SELECT * FROM GRAPH_TABLE (social MATCH (a:person)-[k:knows]->(b) COLUMNS (a.id, b.id)) t"
            ),
            "SELECT * FROM graph_table('social MATCH (a:person)-[k:knows]->(b) COLUMNS (a.id, b.id)') t"
        );
    }

    #[test]
    fn nested_brackets_quotes_and_two_word_spelling() {
        assert_eq!(
            rewrite(
                "select x from graph table (g MATCH (a WHERE a.name = 'O''Brien (jr)')-[e]->(b) \
                 WHERE (a.x + (b.y)) > 1 COLUMNS (concat(a.name, ')') AS n)) AS t WHERE x > 0"
            ),
            "select x from graph_table('g MATCH (a WHERE a.name = ''O''''Brien (jr)'')-[e]->(b) \
             WHERE (a.x + (b.y)) > 1 COLUMNS (concat(a.name, '')'') AS n)') AS t WHERE x > 0"
        );
    }

    #[test]
    fn strings_comments_and_other_words_are_left_alone() {
        for sql in [
            "SELECT 'GRAPH_TABLE (g MATCH (a))' AS s",
            "SELECT 1 -- GRAPH_TABLE (g MATCH (a))\n",
            "SELECT 1 /* GRAPH_TABLE (g) */",
            r#"SELECT * FROM "GRAPH_TABLE"(1)"#,
            "SELECT graph_table FROM t",
            "SELECT * FROM my_graph_table(1)",
        ] {
            assert_eq!(rewrite(sql), sql);
        }
    }

    #[test]
    fn every_graph_table_in_a_statement_and_in_ctes_subqueries_and_views() {
        assert_eq!(
            rewrite(
                "WITH p AS (SELECT * FROM GRAPH_TABLE (g MATCH (a)-[e]->(b) COLUMNS (a.id AS a)))
                 SELECT * FROM p JOIN (SELECT * FROM GRAPH_TABLE(g MATCH (c) COLUMNS (c.id AS a))) q USING (a)"
            ),
            "WITH p AS (SELECT * FROM graph_table('g MATCH (a)-[e]->(b) COLUMNS (a.id AS a)'))
                 SELECT * FROM p JOIN (SELECT * FROM graph_table('g MATCH (c) COLUMNS (c.id AS a)')) q USING (a)"
        );
        assert_eq!(
            rewrite("CREATE VIEW v AS SELECT * FROM GRAPH_TABLE (g MATCH (a) COLUMNS (a.id))"),
            "CREATE VIEW v AS SELECT * FROM graph_table('g MATCH (a) COLUMNS (a.id)')"
        );
    }

    #[test]
    fn unbalanced_or_untokenizable_text_is_kept_for_the_parser() {
        for sql in [
            "SELECT * FROM GRAPH_TABLE (g MATCH (a)",
            "SELECT 'unterminated FROM GRAPH_TABLE (g)",
        ] {
            assert_eq!(rewrite(sql), sql);
        }
    }

    #[test]
    fn multi_line_statements_keep_their_text() {
        let sql = "SELECT a, b\nFROM GRAPH_TABLE (\n  g\n  MATCH (a)-[e]->(b)\n  COLUMNS (a.id AS a, b.id AS b)\n)\nORDER BY a;";
        assert_eq!(
            rewrite(sql),
            "SELECT a, b\nFROM graph_table('g\n  MATCH (a)-[e]->(b)\n  COLUMNS (a.id AS a, b.id AS b)')\nORDER BY a;"
        );
    }
}
