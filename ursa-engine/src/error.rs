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
    /// In the error `ListingTableConfig::infer_options` returns when a
    /// location lists no file (an internal error). Ursa says "no file found
    /// at '…'" instead (`file_table.rs`).
    pub const NO_FILES_FOR_TABLE: &str = "No files for table";

    /// In the error `ListingTableConfig::infer_options` returns for a file
    /// whose extension names no format it knows. Ursa lists the formats it
    /// reads instead (`file_table.rs`).
    pub const NO_FILE_FORMAT_FOR_EXTENSION: &str = "No file_format found with extension";

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
}

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
        DataFusionError::Context(context, inner)
            if context.starts_with(matched::OPTIMIZER_RULE_FAILED) =>
        {
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
    use std::sync::Arc;

    use datafusion::common::tree_node::Transformed;
    use datafusion::common::DFSchema;
    use datafusion::datasource::listing::{
        ListingTableConfig, ListingTableConfigExt, ListingTableUrl,
    };
    use datafusion::logical_expr::{EmptyRelation, LogicalPlan};
    use datafusion::optimizer::{Optimizer, OptimizerConfig, OptimizerContext, OptimizerRule};
    use datafusion::prelude::SessionContext;
    use datafusion::sql::sqlparser::dialect::GenericDialect;
    use datafusion::sql::sqlparser::tokenizer::Tokenizer;

    use super::*;

    /// What `ListingTableConfig::infer_options` says about `location`, which
    /// must fail: the first step of opening a file as a table, as
    /// `file_table.rs` does.
    async fn infer_options_error(location: &std::path::Path) -> String {
        let ctx = SessionContext::new();
        let url = ListingTableUrl::parse(location.to_str().unwrap()).unwrap();
        let err = ListingTableConfig::new(url)
            .infer_options(&ctx.state())
            .await
            .expect_err("infer_options fails");
        err.to_string()
    }

    #[tokio::test]
    async fn datafusion_still_says_no_files_for_table() {
        let dir = tempfile::tempdir().unwrap();
        let message = infer_options_error(dir.path()).await;
        assert!(
            message.contains(matched::NO_FILES_FOR_TABLE),
            "DataFusion's message for a location with no file changed; update \
             matched::NO_FILES_FOR_TABLE: {message}"
        );
    }

    #[tokio::test]
    async fn datafusion_still_says_no_file_format_for_extension() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let message = infer_options_error(&file).await;
        assert!(
            message.contains(matched::NO_FILE_FORMAT_FOR_EXTENSION),
            "DataFusion's message for an unknown extension changed; update \
             matched::NO_FILE_FORMAT_FOR_EXTENSION: {message}"
        );
    }

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
