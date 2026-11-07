//! DuckDB's FROM-first queries (#306): `FROM t [WHERE …]` is
//! `SELECT * FROM t [WHERE …]`.
//!
//! sqlparser reads `FROM t` as a `SELECT` of no columns, which printed
//! nothing, and refuses `FROM t WHERE …` outright. DuckPGQ's queries are
//! often written FROM-first (`FROM GRAPH_TABLE (g MATCH … COLUMNS (…))`).
//! So, before the statement is parsed, a `FROM` that starts a query, with no
//! `SELECT` after it in that query, becomes `SELECT * FROM`. A query starts
//! a statement, follows `(`, `AS`, `EXPLAIN` or a set operator (`UNION
//! [ALL | DISTINCT]`, `INTERSECT`, `EXCEPT`). `FROM t SELECT a`, which
//! sqlparser already reads as `SELECT a FROM t`, is left alone, as are
//! `TRIM(FROM s)`, `x IS DISTINCT FROM y`, strings and comments.
//!
//! Like the `GRAPH_TABLE` rewrite, it works on sqlparser's tokens and keeps
//! the rest of the text byte for byte.

use datafusion::sql::sqlparser::dialect::Dialect;
use datafusion::sql::sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer};

use crate::parser::byte_offset;
use crate::source_map::Rewritten;

/// `sql` with `SELECT * ` before each FROM-first query's `FROM` that has no
/// `SELECT`. Text that does not tokenize is returned as it is.
#[cfg(test)]
fn rewrite_from_first(sql: &str, dialect: &dyn Dialect) -> String {
    rewrite_from_first_mapped(sql, dialect).text
}

/// [`rewrite_from_first`], with a record of what it inserted.
pub(crate) fn rewrite_from_first_mapped(sql: &str, dialect: &dyn Dialect) -> Rewritten {
    let mut out = Rewritten::new(sql.len());
    let Ok(tokens) = Tokenizer::new(dialect, sql).tokenize_with_location() else {
        return out.finish(sql);
    };
    let tokens: Vec<&TokenWithSpan> = tokens
        .iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_) | Token::EOF))
        .collect();
    let word = |i: usize, w: &str| match tokens.get(i).map(|t| &t.token) {
        Some(Token::Word(word)) => word.quote_style.is_none() && word.value.eq_ignore_ascii_case(w),
        _ => false,
    };
    let mut inserts = Vec::new();
    for i in 0..tokens.len() {
        if !word(i, "FROM")
            || !starts_query(&tokens, i, &word)
            || !names_a_table(&tokens, i)
            || has_select(&tokens, i, &word)
        {
            continue;
        }
        if let Some(at) = byte_offset(sql, tokens[i].span.start) {
            inserts.push(at);
        }
    }
    for at in inserts {
        out.replace(sql, at, at, "SELECT * ");
    }
    out.finish(sql)
}

/// Whether something that could be a table follows the `FROM` at `i`: not
/// `)`, `,`, `;` or the end. `KEY (from)` names a column called `from`, and
/// is not a query.
fn names_a_table(tokens: &[&TokenWithSpan], i: usize) -> bool {
    !matches!(
        tokens.get(i + 1).map(|t| &t.token),
        None | Some(Token::RParen | Token::Comma | Token::SemiColon)
    )
}

/// Whether the `FROM` at `i` starts a query.
fn starts_query(tokens: &[&TokenWithSpan], i: usize, word: &dyn Fn(usize, &str) -> bool) -> bool {
    if i == 0 {
        return true;
    }
    let p = i - 1;
    match &tokens[p].token {
        Token::SemiColon => true,
        // The query after a `WITH` list: `WITH s AS (…) FROM s`.
        Token::RParen => opening(tokens, p)
            .is_some_and(|o| o > 0 && (word(o - 1, "AS") || word(o - 1, "MATERIALIZED"))),
        // Not `TRIM(FROM s)`.
        Token::LParen => !(p > 0 && word(p - 1, "TRIM")),
        Token::Word(_) => {
            word(p, "AS")
                || word(p, "EXPLAIN")
                || word(p, "UNION")
                || word(p, "INTERSECT")
                || word(p, "EXCEPT")
                // `UNION ALL FROM`, not `IS DISTINCT FROM`.
                || ((word(p, "ALL") || word(p, "DISTINCT"))
                    && p > 0
                    && (word(p - 1, "UNION") || word(p - 1, "INTERSECT") || word(p - 1, "EXCEPT")))
        }
        _ => false,
    }
}

/// The index of the `(` that the `)` at `close` closes.
fn opening(tokens: &[&TokenWithSpan], close: usize) -> Option<usize> {
    let mut depth = 0usize;
    for n in (0..=close).rev() {
        match tokens[n].token {
            Token::RParen => depth += 1,
            Token::LParen => {
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

/// Whether the query whose `FROM` is at `i` has a `SELECT` of its own: one
/// at its bracket depth before it ends (a `)` closing it, `;`, the end, or a
/// set operator).
fn has_select(tokens: &[&TokenWithSpan], i: usize, word: &dyn Fn(usize, &str) -> bool) -> bool {
    let mut depth = 0i32;
    for (n, token) in tokens.iter().enumerate().skip(i + 1) {
        match token.token {
            Token::LParen => depth += 1,
            Token::RParen => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            Token::SemiColon if depth == 0 => return false,
            _ if depth == 0 => {
                if word(n, "SELECT") {
                    return true;
                }
                if word(n, "UNION") || word(n, "INTERSECT") || word(n, "EXCEPT") {
                    return false;
                }
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use datafusion::sql::sqlparser::dialect::GenericDialect;

    use super::*;

    fn rewrite(sql: &str) -> String {
        rewrite_from_first(sql, &GenericDialect {})
    }

    #[test]
    fn from_first_queries_get_select_star() {
        for (sql, expected) in [
            ("FROM t", "SELECT * FROM t"),
            ("from t where x > 1", "SELECT * from t where x > 1"),
            ("SELECT * FROM (FROM t WHERE x > 1)", "SELECT * FROM (SELECT * FROM t WHERE x > 1)"),
            (
                "WITH s AS (FROM t), u AS MATERIALIZED (FROM s) FROM s; FROM u",
                "WITH s AS (SELECT * FROM t), u AS MATERIALIZED (SELECT * FROM s) SELECT * FROM s; SELECT * FROM u",
            ),
            (
                "FROM t UNION ALL FROM u EXCEPT FROM v",
                "SELECT * FROM t UNION ALL SELECT * FROM u EXCEPT SELECT * FROM v",
            ),
            ("CREATE VIEW v AS FROM t", "CREATE VIEW v AS SELECT * FROM t"),
            ("EXPLAIN FROM t", "EXPLAIN SELECT * FROM t"),
            ("COPY (FROM t) TO 'x.csv'", "COPY (SELECT * FROM t) TO 'x.csv'"),
            (
                "FROM t WHERE x IN (SELECT y FROM u)",
                "SELECT * FROM t WHERE x IN (SELECT y FROM u)",
            ),
        ] {
            assert_eq!(rewrite(sql), expected);
        }
    }

    #[test]
    fn other_froms_are_left_alone() {
        for sql in [
            "SELECT * FROM t",
            "FROM t SELECT a WHERE b",
            "SELECT trim(FROM ' x '), extract(year FROM d), substring(s FROM 2) FROM t",
            "SELECT a IS DISTINCT FROM b, a IS NOT DISTINCT FROM b FROM t",
            "DELETE FROM t",
            "SELECT 'FROM t' AS s -- FROM t",
            "SELECT * FROM t UNION SELECT * FROM u",
            "SELECT cast(x AS int) FROM t",
            "WITH s AS (SELECT * FROM t) SELECT * FROM s",
            "SELECT 'unterminated FROM",
            // A column named `from` (#328).
            "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (from) DESTINATION KEY (to))",
            "SELECT * FROM t WHERE x IN (from, to)",
        ] {
            assert_eq!(rewrite(sql), sql);
        }
    }
}
