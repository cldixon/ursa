//! Error messages for people, not programmers.
//!
//! DataFusion's `Display` for an error prefixes it with the variant it came from
//! ("Error during planning: ", "Arrow error: ", "External error: ") and, for a
//! SQL syntax error, prints the parser error's `Debug` form
//! (`ParserError("…")`). [`user_message`] walks the error instead and keeps only
//! what happened.

use datafusion::arrow::error::ArrowError;
use datafusion::error::DataFusionError;
use datafusion::sql::sqlparser::parser::ParserError;

/// The message to show a user for `err`: what went wrong, without the names of
/// the internal error types it passed through.
pub fn user_message(err: &DataFusionError) -> String {
    match err {
        DataFusionError::ArrowError(e, _) => arrow_message(e),
        DataFusionError::ParquetError(e) => format!("Parquet: {e}"),
        DataFusionError::ObjectStore(e) => e.to_string(),
        DataFusionError::IoError(e) => e.to_string(),
        DataFusionError::SQL(e, _) => parser_message(e),
        DataFusionError::NotImplemented(m)
        | DataFusionError::Plan(m)
        | DataFusionError::Configuration(m)
        | DataFusionError::Execution(m)
        | DataFusionError::ResourcesExhausted(m)
        | DataFusionError::Substrait(m)
        | DataFusionError::Ffi(m) => m.clone(),
        DataFusionError::Internal(m) => format!("internal error: {m}"),
        DataFusionError::SchemaError(e, _) => e.to_string(),
        DataFusionError::ExecutionJoin(e) => e.to_string(),
        DataFusionError::External(e) => external_message(e.as_ref()),
        // Which optimizer rule hit the error is DataFusion's business.
        DataFusionError::Context(context, inner) if context.starts_with("Optimizer rule") => {
            user_message(inner)
        }
        DataFusionError::Context(context, inner) => {
            format!("{context}: {}", user_message(inner))
        }
        DataFusionError::Diagnostic(_, inner) => user_message(inner),
        DataFusionError::Collection(errs) => errs
            .first()
            .map(user_message)
            .unwrap_or_else(|| "unknown error".to_string()),
        DataFusionError::Shared(inner) => user_message(inner),
    }
}

fn parser_message(e: &ParserError) -> String {
    match e {
        ParserError::TokenizerError(m) | ParserError::ParserError(m) => {
            format!("syntax error: {m}")
        }
        ParserError::RecursionLimitExceeded => {
            "syntax error: the statement is nested too deeply".to_string()
        }
    }
}

fn arrow_message(e: &ArrowError) -> String {
    match e {
        ArrowError::ExternalError(source) => external_message(source.as_ref()),
        other => other.to_string(),
    }
}

fn external_message(e: &(dyn std::error::Error + Send + Sync + 'static)) -> String {
    if let Some(df) = e.downcast_ref::<DataFusionError>() {
        user_message(df)
    } else if let Some(arrow) = e.downcast_ref::<ArrowError>() {
        arrow_message(arrow)
    } else {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_variant_prefixes() {
        let err = DataFusionError::Plan("table 'x' not found".into());
        assert_eq!(user_message(&err), "table 'x' not found");
        let err = DataFusionError::SQL(
            Box::new(ParserError::ParserError("Expected: an expression".into())),
            None,
        );
        assert_eq!(user_message(&err), "syntax error: Expected: an expression");
        let err = DataFusionError::External(Box::new(DataFusionError::Execution("boom".into())));
        assert_eq!(user_message(&err), "boom");
    }
}
