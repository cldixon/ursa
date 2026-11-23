//! DuckDB's `COPY … TO 'file' (FORMAT parquet, HEADER, DELIMITER '|')` (#417).
//!
//! DataFusion writes the same statement as `COPY … TO 'file' STORED AS
//! PARQUET OPTIONS ('format.has_header' 'true', …)`, and its parser stops at
//! DuckDB's option list with "Expected: end of statement or ;". So a `COPY`
//! whose target is followed by `(` (or `WITH (`, PostgreSQL's spelling) is
//! parsed here, and its options are mapped:
//!
//! | DuckDB | DataFusion |
//! |---|---|
//! | `FORMAT x` | `STORED AS x` |
//! | `HEADER [true \| false]` | `'format.has_header'` |
//! | `DELIMITER`, `DELIM`, `SEP` | `'format.delimiter'` |
//! | `COMPRESSION x` | `'format.compression'`; for Parquet, `zstd`, `gzip` and `brotli` get a level |
//!
//! Any other option is refused by name. A `COPY` without such a list is
//! DataFusion's, unchanged.

use datafusion::error::{DataFusionError, Result};
use datafusion::sql::parser::{CopyToSource, CopyToStatement, DFParser, Statement as DFStatement};
use datafusion::sql::sqlparser::ast::Value;
use datafusion::sql::sqlparser::keywords::Keyword;
use datafusion::sql::sqlparser::parser::ParserError;
use datafusion::sql::sqlparser::tokenizer::Token;

/// The options this module maps, as a refusal lists them.
const MAPPED: &str = "FORMAT, HEADER, DELIMITER (or DELIM, SEP) and COMPRESSION";

/// Parse the `COPY` statement next in `df` if it has DuckDB's option list;
/// `None`, consuming nothing, if it does not.
pub(crate) fn parse_copy(df: &mut DFParser) -> Result<Option<DFStatement>> {
    if !has_option_list(df) {
        return Ok(None);
    }
    df.parser.expect_keyword(Keyword::COPY)?;
    let source = if df.parser.consume_token(&Token::LParen) {
        let query = df.parser.parse_query()?;
        df.parser.expect_token(&Token::RParen)?;
        CopyToSource::Query(query)
    } else {
        CopyToSource::Relation(df.parser.parse_object_name(true)?)
    };
    df.parser.expect_keyword(Keyword::TO)?;
    let target = df.parser.parse_literal_string()?;
    let _ = df.parser.parse_keyword(Keyword::WITH);
    df.parser.expect_token(&Token::LParen)?;
    let mut written = Vec::new();
    loop {
        let next = df.parser.next_token();
        let key = match next.token {
            Token::Word(w) => w.value,
            Token::SingleQuotedString(s) | Token::DoubleQuotedString(s) => s,
            other => {
                return Err(ParserError::ParserError(format!(
                    "Expected: a COPY option, found: {other}{}",
                    next.span.start
                ))
                .into())
            }
        };
        let value = match df.parser.peek_token().token {
            Token::Comma | Token::RParen => None,
            _ => Some(option_value(df.parse_option_value()?)),
        };
        written.push((key, value));
        let comma = df.parser.consume_token(&Token::Comma);
        if df.parser.consume_token(&Token::RParen) {
            break;
        }
        if !comma {
            let next = df.parser.peek_token();
            return Err(ParserError::ParserError(format!(
                "Expected: ',' or ')' after a COPY option, found: {next}{}",
                next.span.start
            ))
            .into());
        }
    }
    let next = df.parser.peek_token();
    if !matches!(next.token, Token::EOF | Token::SemiColon) {
        return Err(ParserError::ParserError(format!(
            "Expected: end of statement or ;, found: {next}{}",
            next.span.start
        ))
        .into());
    }
    let (stored_as, options) = map_options(&target, written)?;
    Ok(Some(DFStatement::CopyTo(CopyToStatement {
        source,
        target,
        partitioned_by: Vec::new(),
        stored_as,
        options,
    })))
}

/// Whether the `COPY` next in `df` has a `(` or `WITH (` right after its
/// `TO 'file'`, at the statement's own bracket depth.
fn has_option_list(df: &DFParser) -> bool {
    let token = |n: usize| &df.parser.peek_nth_token_ref(n).token;
    let word = |n: usize, w: &str| {
        matches!(token(n), Token::Word(word) if word.quote_style.is_none()
            && word.value.eq_ignore_ascii_case(w))
    };
    let mut depth = 0usize;
    for n in 1.. {
        match token(n) {
            Token::EOF => return false,
            Token::SemiColon if depth == 0 => return false,
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            _ if depth == 0 && word(n, "TO") => {
                if !matches!(token(n + 1), Token::SingleQuotedString(_)) {
                    return false;
                }
                return matches!(token(n + 2), Token::LParen)
                    || (word(n + 2, "WITH") && matches!(token(n + 3), Token::LParen));
            }
            _ => {}
        }
    }
    false
}

/// An option's value as text: `parquet`, `'|'`, `true`, `1`.
fn option_value(value: Value) -> String {
    match value {
        Value::SingleQuotedString(s)
        | Value::DoubleQuotedString(s)
        | Value::EscapedStringLiteral(s) => s,
        Value::Number(n, _) => n,
        other => other.to_string(),
    }
}

/// DuckDB's options as DataFusion's `STORED AS` and `OPTIONS`.
fn map_options(
    target: &str,
    written: Vec<(String, Option<String>)>,
) -> Result<(Option<String>, Vec<(String, Value)>)> {
    let mut stored_as = None;
    let mut options = Vec::new();
    let mut compression = None;
    for (key, value) in written {
        let needs_value = |value: Option<String>| {
            value.ok_or_else(|| {
                DataFusionError::Plan(format!("the COPY option {key} needs a value"))
            })
        };
        match key.to_ascii_uppercase().as_str() {
            "FORMAT" => stored_as = Some(needs_value(value)?.to_ascii_uppercase()),
            "HEADER" => {
                let on = match value.as_deref().map(str::to_ascii_lowercase).as_deref() {
                    None | Some("true" | "1" | "on") => "true",
                    Some("false" | "0" | "off") => "false",
                    Some(other) => {
                        return Err(DataFusionError::Plan(format!(
                            "COPY's HEADER is true or false, not '{other}'"
                        )))
                    }
                };
                options.push(option("format.has_header", on));
            }
            "DELIMITER" | "DELIM" | "SEP" => {
                options.push(option("format.delimiter", &needs_value(value)?));
            }
            "COMPRESSION" => compression = Some(needs_value(value)?),
            _ => {
                return Err(DataFusionError::NotImplemented(format!(
                    "the COPY option {key} is not supported; Ursa maps DuckDB's {MAPPED}. \
                     DataFusion's own options go in OPTIONS (…), as in COPY … TO \
                     'out.parquet' OPTIONS ('format.max_row_group_size' '100000')"
                )))
            }
        }
    }
    if let Some(codec) = compression {
        let parquet = match &stored_as {
            Some(format) => format == "PARQUET",
            None => target.to_ascii_lowercase().ends_with(".parquet"),
        };
        if let Some(codec) = compression_option(&codec, parquet) {
            options.push(option("format.compression", &codec));
        }
    }
    Ok((stored_as, options))
}

/// DuckDB's name for a compression as DataFusion takes it, or `None` for
/// `auto` (the default). DataFusion's Parquet writer needs a level for
/// `zstd`, `gzip` and `brotli`: zstd's is DuckDB's default, 3; the others
/// are the parquet crate's defaults.
fn compression_option(codec: &str, parquet: bool) -> Option<String> {
    let codec = codec.to_ascii_lowercase();
    Some(match codec.as_str() {
        "auto" => return None,
        "none" | "uncompressed" => "uncompressed".to_string(),
        "zstd" if parquet => "zstd(3)".to_string(),
        "gzip" if parquet => "gzip(6)".to_string(),
        "brotli" if parquet => "brotli(1)".to_string(),
        _ => codec,
    })
}

fn option(key: &str, value: &str) -> (String, Value) {
    (key.to_string(), Value::SingleQuotedString(value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression_names() {
        assert_eq!(compression_option("ZSTD", true).as_deref(), Some("zstd(3)"));
        assert_eq!(compression_option("zstd", false).as_deref(), Some("zstd"));
        assert_eq!(compression_option("snappy", true).as_deref(), Some("snappy"));
        assert_eq!(compression_option("zstd(9)", true).as_deref(), Some("zstd(9)"));
        assert_eq!(compression_option("none", false).as_deref(), Some("uncompressed"));
        assert_eq!(compression_option("auto", true), None);
    }
}
