//! Error messages for people, not programmers.
//!
//! DataFusion's `Display` for an error prefixes it with the variant it came from
//! ("Error during planning: ", "Arrow error: ", "External error: ") and, for a
//! SQL syntax error, prints the parser error's `Debug` form
//! (`ParserError("…")`). [`user_message`] walks the error instead and keeps only
//! what happened. It reads the error's variants, not its text.
//!
//! Where DataFusion offers no typed error, the engine matches its text. Every
//! such string is in [`matched`], with a test that makes DataFusion (or
//! sqlparser) produce the message itself and checks the string is still in it,
//! so an upgrade that rewords one fails there rather than quietly showing the
//! user DataFusion's raw message.

use datafusion::arrow::error::ArrowError;
use datafusion::common::SchemaError;
use datafusion::error::DataFusionError;
use datafusion::sql::sqlparser::parser::ParserError;

/// The DataFusion and sqlparser messages the engine and the CLI recognize by
/// their text, because no typed error says the same (#265).
///
/// Each is matched as a substring or a prefix, as its doc says. The tests
/// below trigger each message in DataFusion or sqlparser directly, not through
/// Ursa's wrappers, and check the text is still there: when an upgrade changes
/// the wording, they fail and name the string to update.
pub mod matched {
    /// The start of the context DataFusion's optimizer puts on a rule's error
    /// (`Optimizer rule '…' failed`). Which rule failed is DataFusion's
    /// business: [`super::user_message`] drops it and keeps the cause.
    pub const OPTIMIZER_RULE_FAILED: &str = "Optimizer rule";

    /// The start of sqlparser's tokenizer error for a string literal (or a
    /// dollar-quoted or encoded one) still open at the end of the input. The
    /// shell reads the next line instead of reporting it (`ursa-cli`'s
    /// `input.rs`).
    pub const TOKENIZER_UNTERMINATED: &str = "Unterminated";

    /// In sqlparser's tokenizer error for a quoted identifier or a block
    /// comment still open at the end of the input (`… before EOF.`,
    /// `Unexpected EOF while in a multi-line comment`). The shell reads the
    /// next line instead (`ursa-cli`'s `input.rs`).
    pub const TOKENIZER_EOF: &str = "EOF";

    /// In arrow's cast error for a date or timestamp outside the range of a
    /// nanosecond timestamp (`Cannot cast Date32 value -100000000 to
    /// Timestamp(ns): converted value exceeds the representable i64 range`),
    /// which `date_trunc` and other timestamp functions meet on an
    /// out-of-range date. Matched together with [`TIMESTAMP_NS`].
    pub const TIMESTAMP_OVERFLOW: &str = "exceeds the representable i64 range";

    /// The nanosecond timestamp type, as arrow names it in a cast error.
    pub const TIMESTAMP_NS: &str = "Timestamp(ns)";

    /// The start of the context DataFusion puts on an error inferring a CSV
    /// file's schema, followed by the file's object store location, which
    /// has lost its leading `/` (`Users/me/x.csv`): [`super::user_message`]
    /// puts it back (#327, N14).
    pub const CSV_FILE_CONTEXT: &str = "Error when processing CSV file ";
}

/// `location`, an object store location, as the local path the user wrote
/// when it is one (`Users/me/x.csv` is `/Users/me/x.csv` if that exists).
pub fn shown_path(location: &str) -> String {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    if !location.starts_with('/') && !location.contains("://") {
        let absolute = format!("/{location}");
        if std::path::Path::new(&absolute).exists() {
            return absolute;
        }
    }
    location.to_string()
}

/// What to do about a date outside a nanosecond timestamp's range (#328).
const TIMESTAMP_RANGE_HINT: &str = "a timestamp in nanoseconds holds only dates from 1677-09-21 \
     to 2262-04-11, and date_trunc and the other timestamp functions convert to one. Filter \
     the dates first (WHERE d BETWEEN DATE '1678-01-01' AND DATE '2262-04-11'), or use \
     extract(year FROM d) or date_part instead of date_trunc";

/// The message to show a user for `err`: what went wrong, without the names of
/// the internal error types it passed through.
pub fn user_message(err: &DataFusionError) -> String {
    let message = message_of(err);
    if message.contains(matched::TIMESTAMP_OVERFLOW) && message.contains(matched::TIMESTAMP_NS) {
        return format!("{message}; {TIMESTAMP_RANGE_HINT}");
    }
    message
}

fn message_of(err: &DataFusionError) -> String {
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
        DataFusionError::SchemaError(e, _) => schema_message(e),
        DataFusionError::ExecutionJoin(e) => e.to_string(),
        DataFusionError::External(e) => external_message(e.as_ref()),
        // Which optimizer rule hit the error is DataFusion's business.
        DataFusionError::Context(context, inner)
            if context.starts_with(matched::OPTIMIZER_RULE_FAILED) =>
        {
            message_of(inner)
        }
        DataFusionError::Context(context, inner) => {
            match context.strip_prefix(matched::CSV_FILE_CONTEXT) {
                Some(location) => format!(
                    "cannot read {}: {}",
                    shown_path(location),
                    message_of(inner)
                ),
                None => format!("{context}: {}", message_of(inner)),
            }
        }
        DataFusionError::Diagnostic(_, inner) => message_of(inner),
        DataFusionError::Collection(errs) => errs
            .first()
            .map(message_of)
            .unwrap_or_else(|| "unknown error".to_string()),
        DataFusionError::Shared(inner) => message_of(inner),
    }
}

/// A schema error, with a hint for a name with a `-` in it: unquoted,
/// `date-created` is `date - created`, so DataFusion reports `date` missing.
fn schema_message(e: &SchemaError) -> String {
    let message = e.to_string();
    let SchemaError::FieldNotFound {
        field,
        valid_fields,
    } = e
    else {
        return message;
    };
    // A valid name that the missing one is a `-`-separated part of.
    let part_of =
        |name: &str| name.split('-').count() > 1 && name.split('-').any(|p| p == field.name);
    match valid_fields.iter().find(|c| part_of(&c.name)) {
        Some(column) => {
            let (first, rest) = message.split_once('\n').unwrap_or((&message, ""));
            let mut out = format!(
                "{first}\nA name that contains '-' must be quoted, or it is read as a \
                 subtraction: write \"{}\"",
                column.name.replace('"', "\"\"")
            );
            out.push_str(&format!(", not {}.", column.name));
            if !rest.is_empty() {
                out.push('\n');
                out.push_str(rest);
            }
            out
        }
        None => message,
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
        message_of(df)
    } else if let Some(arrow) = e.downcast_ref::<ArrowError>() {
        arrow_message(arrow)
    } else {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use datafusion::common::tree_node::Transformed;
    use datafusion::common::DFSchema;
    use datafusion::logical_expr::{EmptyRelation, LogicalPlan};
    use datafusion::optimizer::{Optimizer, OptimizerConfig, OptimizerContext, OptimizerRule};
    use datafusion::prelude::SessionContext;
    use datafusion::sql::sqlparser::dialect::GenericDialect;
    use datafusion::sql::sqlparser::tokenizer::Tokenizer;

    use super::*;

    /// An optimizer rule that always fails.
    #[derive(Debug)]
    struct Failing;

    impl OptimizerRule for Failing {
        fn name(&self) -> &str {
            "failing"
        }

        fn supports_rewrite(&self) -> bool {
            true
        }

        fn rewrite(
            &self,
            _plan: LogicalPlan,
            _config: &dyn OptimizerConfig,
        ) -> datafusion::error::Result<Transformed<LogicalPlan>> {
            Err(DataFusionError::Plan("the cause".into()))
        }
    }

    #[test]
    fn datafusion_still_labels_a_failed_optimizer_rule() {
        let optimizer = Optimizer::with_rules(vec![Arc::new(Failing)]);
        let config = OptimizerContext::new().with_skip_failing_rules(false);
        let plan = LogicalPlan::EmptyRelation(EmptyRelation {
            produce_one_row: false,
            schema: Arc::new(DFSchema::empty()),
        });
        let err = optimizer
            .optimize(plan, &config, |_, _| {})
            .expect_err("the rule fails");
        let DataFusionError::Context(label, _) = &err else {
            panic!(
                "DataFusion no longer wraps a failed optimizer rule's error in a context: {err:?}"
            );
        };
        assert!(
            label.starts_with(matched::OPTIMIZER_RULE_FAILED),
            "DataFusion's label for a failed optimizer rule changed; update \
             matched::OPTIMIZER_RULE_FAILED: {label}"
        );
        // And user_message keeps only the cause.
        assert_eq!(user_message(&err), "the cause");
    }

    /// `user_message` reads a syntax error's variant, not DataFusion's
    /// `ParserError("…")` text: DataFusion must still return the typed error.
    #[tokio::test]
    async fn datafusion_still_returns_syntax_errors_typed() {
        let err = SessionContext::new()
            .sql("SELEC 1")
            .await
            .expect_err("a syntax error");
        assert!(
            matches!(err.find_root(), DataFusionError::SQL(..)),
            "DataFusion no longer returns a syntax error as DataFusionError::SQL: {err:?}"
        );
        let message = user_message(&err);
        assert!(
            message.starts_with("syntax error: ") && !message.contains("ParserError"),
            "{message}"
        );
    }

    /// sqlparser's tokenizer error for `sql`, which must fail.
    fn tokenizer_error(sql: &str) -> String {
        Tokenizer::new(&GenericDialect {}, sql)
            .tokenize()
            .expect_err("the tokenizer fails")
            .message
    }

    #[test]
    fn sqlparser_still_says_unterminated_and_eof() {
        for sql in ["SELECT 'abc", "SELECT 'it''s"] {
            let message = tokenizer_error(sql);
            assert!(
                message.starts_with(matched::TOKENIZER_UNTERMINATED),
                "sqlparser's message for an open string changed; update \
                 matched::TOKENIZER_UNTERMINATED: {sql}: {message}"
            );
        }
        for sql in ["SELECT \"abc", "SELECT 1 /* a note"] {
            let message = tokenizer_error(sql);
            assert!(
                message.contains(matched::TOKENIZER_EOF),
                "sqlparser's message for an open quoted name or comment changed; update \
                 matched::TOKENIZER_EOF: {sql}: {message}"
            );
        }
    }

    /// N14: a CSV file DataFusion cannot read is named by its path, with its
    /// leading `/`, not by its object store location.
    #[tokio::test]
    async fn datafusion_still_names_a_bad_csv_file_by_its_location() {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap().join("bad.csv");
        std::fs::write(&path, "a,b\n1,2\n1,2,3\n").unwrap();
        let ctx = SessionContext::new();
        let err = ctx
            .register_csv("t", path.to_str().unwrap(), Default::default())
            .await
            .unwrap_err();
        let DataFusionError::Context(context, _) = &err else {
            panic!("DataFusion no longer gives a bad CSV file context; update matched::CSV_FILE_CONTEXT: {err:?}");
        };
        assert!(
            context.starts_with(matched::CSV_FILE_CONTEXT),
            "DataFusion's context for a bad CSV file changed; update \
             matched::CSV_FILE_CONTEXT: {context}"
        );
        let message = user_message(&err);
        #[cfg(unix)]
        assert!(
            message.starts_with(&format!("cannot read {}: ", path.display())),
            "{message}"
        );
        #[cfg(not(unix))]
        assert!(message.starts_with("cannot read "), "{message}");
    }

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

    /// #328: an unquoted `date-created` is `date - created`; say to quote it.
    #[tokio::test]
    async fn a_hyphenated_name_says_to_quote_it() {
        let ctx = SessionContext::new();
        let sql = "SELECT date-created FROM (SELECT 1 AS \"date-created\", 2 AS x) t";
        let err = ctx.sql(sql).await.unwrap_err();
        assert_eq!(
            user_message(&err),
            "No field named date.\n\
             A name that contains '-' must be quoted, or it is read as a subtraction: write \
             \"date-created\", not date-created.\n\
             Valid fields are t.\"date-created\", t.x."
        );
        // A missing name that is not part of one with a `-` gets no hint.
        let sql = "SELECT nope FROM (SELECT 1 AS \"date-created\") t";
        let err = ctx.sql(sql).await.unwrap_err();
        assert!(
            !user_message(&err).contains("quoted"),
            "{}",
            user_message(&err)
        );
    }

    /// #328: a date out of a nanosecond timestamp's range, in date_trunc,
    /// says what to do instead (and arrow still words the error as
    /// [`matched::TIMESTAMP_OVERFLOW`] and [`matched::TIMESTAMP_NS`] say).
    #[tokio::test]
    async fn an_out_of_range_date_in_date_trunc_suggests_extract() {
        let ctx = SessionContext::new();
        let err = ctx
            .sql("SELECT date_trunc('year', CAST(-100000000 AS DATE))")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err();
        let raw = message_of(&err);
        assert!(
            raw.contains(matched::TIMESTAMP_OVERFLOW) && raw.contains(matched::TIMESTAMP_NS),
            "arrow's message for an out-of-range timestamp cast changed; update \
             matched::TIMESTAMP_OVERFLOW and TIMESTAMP_NS: {raw}"
        );
        let message = user_message(&err);
        assert!(
            message.ends_with(&format!("; {TIMESTAMP_RANGE_HINT}")),
            "{message}"
        );
        assert!(message.contains("extract(year FROM d)"), "{message}");
    }
}
