//! The wording of engine errors a user reads.
//!
//! An engine error is a [`DataFusionError`], from DataFusion itself, from Arrow,
//! or one of Ursa's own messages carried in one. Its `Display` is for DataFusion's
//! developers: it starts with a variant prefix (`Execution error: `, `Arrow error:
//! `, ...), and DataFusion's own messages name its internals (the `?table?`
//! qualifier, signature lists, logical expressions). [`engine_message`] gives the
//! same error in Ursa's words: Ursa's own messages without the prefix, and the
//! DataFusion and Arrow errors a plan can reach reworded to say what was wrong and
//! what to do. The original error stays the source (`PlanError::source`, and the
//! `__cause__` of the Python exception).
//!
//! A [`DataFusionError::Context`] names the step an error belongs to; its label
//! leads the message (`filter(): cannot compare ...`), unless the message already
//! starts with a call (`join(on=): ...`).

pub use datafusion::error::DataFusionError;

/// The prefixes DataFusion's and Arrow's `Display` put before a message.
const PREFIXES: &[&str] = &[
    "Execution error: ",
    "This feature is not implemented: ",
    "Error during planning: ",
    "Internal error: ",
    "Arrow error: ",
    "Schema error: ",
    "Parquet error: ",
    "External error: ",
    "Object Store error: ",
    "IO error: ",
    "Invalid argument error: ",
    "Compute error: ",
    "Resources exhausted: ",
];

/// The trailer DataFusion appends to an internal error.
const BUG_TRAILER: &str = ".\nThis issue was likely caused by a bug in DataFusion's code";

/// An engine error in Ursa's words (see the module docs).
pub fn engine_message(e: &DataFusionError) -> String {
    match e {
        DataFusionError::Context(label, inner) => {
            // DataFusion's own context for a CSV it could not parse; the scan's
            // label names the file.
            if label.starts_with("Error when processing CSV file ") {
                let cause = engine_message(inner);
                let cause = cause
                    .strip_prefix("Generic LineDelimiter error: ")
                    .unwrap_or(&cause);
                return format!("could not read the file as CSV: {cause}");
            }
            with_label(label, engine_message(inner))
        }
        DataFusionError::Shared(inner) => engine_message(inner),
        DataFusionError::Diagnostic(_, inner) => engine_message(inner),
        other => reword(&other.to_string()),
    }
}

/// `message` led by the step `label`, unless it already starts with a call.
pub fn with_label(label: &str, message: String) -> String {
    if names_a_call(&message) {
        message
    } else {
        format!("{label}: {message}")
    }
}

/// Whether `message` starts with a call, such as `join(on=)` or `ur.col(`.
fn names_a_call(message: &str) -> bool {
    let head: String = message
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.'))
        .collect();
    !head.is_empty() && message[head.len()..].starts_with('(')
}

/// Strip the variant prefixes from `raw` and reword what DataFusion and Arrow
/// say in their own terms.
pub fn reword(raw: &str) -> String {
    let mut m = raw.trim();
    if let Some(i) = m.find(BUG_TRAILER) {
        m = &m[..i];
    }
    let m = strip_prefixes(m);
    let m = translate(m).unwrap_or_else(|| m.to_string());
    let m = m.replace("\"?table?\".", "").replace("?table?.", "");
    // A nested error DataFusion printed on its own line reads as one sentence.
    m.split("\ncaused by\n")
        .map(strip_prefixes)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(": ")
}

fn strip_prefixes(mut m: &str) -> &str {
    while let Some(p) = PREFIXES.iter().find(|p| m.starts_with(**p)) {
        m = m[p.len()..].trim_start();
    }
    // Parquet repeats its own prefix inside DataFusion's.
    while let Some(rest) = m.strip_prefix("Parquet error: ") {
        m = rest;
    }
    m
}

/// The text between `start` and the next `end` after it.
fn between<'a>(m: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = m.find(start)? + start.len();
    let j = m[i..].find(end)? + i;
    Some(&m[i..j])
}

/// A DataFusion type name (`Utf8`, `Int64`, ...) as Ursa's messages print types
/// (pyarrow's spelling: `string`, `int64`, `double`, ...).
fn type_name(t: &str) -> String {
    match t.trim() {
        "Utf8" | "LargeUtf8" => "string".into(),
        "Utf8View" => "string_view".into(),
        "Boolean" => "bool".into(),
        "Float16" => "halffloat".into(),
        "Float32" => "float".into(),
        "Float64" => "double".into(),
        "Binary" | "LargeBinary" | "BinaryView" => "binary".into(),
        "Null" => "null".into(),
        other if other.starts_with("Date32") => "date32[day]".into(),
        other if other.starts_with("Date64") => "date64[ms]".into(),
        other => other.to_lowercase(),
    }
}

/// An operator DataFusion printed, as Ursa's expressions spell it.
fn op_symbol(op: &str) -> &str {
    match op {
        "=" => "==",
        "AND" => "&",
        "OR" => "|",
        other => other,
    }
}

/// The Ursa wording of a DataFusion or Arrow message, when there is one.
fn translate(m: &str) -> Option<String> {
    // An aggregation over a column of a type it does not take.
    if let Some(func) = between(m, "Function '", "' failed to match any signature") {
        let fname = match func {
            "avg" => "mean",
            "count" => "count",
            other => other,
        };
        let got = between(m, "but received ", " (DataType")
            .or_else(|| between(m, "received ", " "))
            .map(|t| format!("; got a {} column", type_name(t)))
            .unwrap_or_default();
        return Some(format!(
            "group_by().agg(): {fname}() needs a numeric column{got}. Aggregate a numeric \
             column, or use count() or n_unique(), which take any column"
        ));
    }
    // Arithmetic over operands it does not apply to.
    if let Some(rest) = between(m, "Cannot coerce arithmetic expression ", " to valid types") {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if let [l, op, r] = parts.as_slice() {
            let (l, r) = (type_name(l), type_name(r));
            return Some(format!(
                "cannot apply '{op}' to {l} and {r} ({l} {op} {r}); arithmetic needs \
                 numeric operands"
            ));
        }
    }
    // A boolean combinator or comparison over operands of the wrong types.
    if let Some(rest) =
        m.strip_prefix("Cannot infer common argument type for logical boolean operation ")
    {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if let [l, op, r] = parts.as_slice() {
            let op = op_symbol(op);
            return Some(format!(
                "'{op}' combines boolean predicates (comparisons such as ur.col('a') > 1); \
                 got {} and {}",
                type_name(l),
                type_name(r)
            ));
        }
    }
    if let Some(rest) =
        m.strip_prefix("Cannot infer common argument type for comparison operation ")
    {
        // `~x` is planned as `x IS DISTINCT FROM true`.
        if let Some(t) = rest.strip_suffix(" IS DISTINCT FROM Boolean") {
            return Some(format!(
                "'~' negates a boolean predicate (a comparison such as ur.col('a') > 1); \
                 got {}",
                type_name(t)
            ));
        }
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if let [l, op, r] = parts.as_slice() {
            let (l, r, op) = (type_name(l), type_name(r), op_symbol(op));
            return Some(format!(
                "cannot compare {l} with {r} ({l} {op} {r}); compare values of one type"
            ));
        }
    }
    if m.starts_with("Divide by zero error") {
        return Some(
            "division by zero in an integer expression; divide by a float (e.g. / 2.0) to get \
             inf or NaN instead, or filter out the zero divisors first"
                .into(),
        );
    }
    if let Some(name) = m.strip_prefix("No field named ") {
        let (name, valid) = match name.split_once(". Valid fields are ") {
            Some((n, v)) => (n, Some(v.trim_end_matches('.'))),
            None => (name.trim_end_matches('.'), None),
        };
        let name = unquote(name);
        let have = valid
            .map(|v| {
                let cols: Vec<String> =
                    v.split(", ").map(|c| format!("'{}'", unquote(c))).collect();
                format!("; the frame has [{}]", cols.join(", "))
            })
            .unwrap_or_default();
        return Some(format!("unknown column '{name}'{have}."));
    }
    for marker in [
        "duplicate qualified field name ",
        "duplicate unqualified field name ",
    ] {
        if let Some(i) = m.find(marker) {
            let name = unquote(m[i + marker.len()..].trim());
            return Some(format!(
                "the result would have two columns named '{name}'; give one of them \
                 another name (rename() or a different output name)"
            ));
        }
    }
    if let Some(rest) = m.strip_prefix("Cast error: Cannot cast string ") {
        if let Some((value, ty)) = rest.split_once(" to value of ") {
            let ty = type_name(ty.trim_end_matches(" type"));
            return Some(format!(
                "the string {value} cannot be read as {ty}; the values being matched have \
                 different types, so cast one side to the other's type first"
            ));
        }
    }
    if m.starts_with("must either specify a row count or at least one column") {
        return Some("select() needs at least one column".into());
    }
    if m.starts_with("No files found at ") {
        return Some(
            "no file matches the path; check it (a glob must match at least one file)".into(),
        );
    }
    if m.starts_with("Physical plan does not support logical expression AggregateFunction") {
        return Some(
            "an aggregation (.sum(), .mean(), ...) is not allowed here; aggregations belong in \
             group_by().agg()"
                .into(),
        );
    }
    if m.starts_with("Invalid Parquet file.") || m.starts_with("Corrupt footer") {
        return Some(format!(
            "the file is not valid Parquet ({})",
            m.trim_start_matches("Invalid Parquet file.")
                .trim()
                .trim_end_matches('.')
                .to_lowercase()
        ));
    }
    if let Some(rest) = m.strip_prefix("Error when processing CSV file ") {
        let cause = rest.split_once("\ncaused by\n").map_or("", |(_, c)| c);
        let cause = strip_prefixes(cause);
        let cause = cause
            .strip_prefix("Generic LineDelimiter error: ")
            .unwrap_or(cause);
        return Some(format!("could not read the file as CSV: {cause}"));
    }
    None
}

/// A serde error reading the argument `label` (`rename()`, `join(how=)`) from its
/// JSON, in Ursa's words: `join(how=) must be a string; got 1`.
pub fn serde_message(label: &str, e: &str) -> String {
    let e = match e.find(" at line ") {
        Some(i) => &e[..i],
        None => e,
    };
    let parsed = e
        .strip_prefix("invalid type: ")
        .or_else(|| e.strip_prefix("invalid value: "))
        .and_then(|rest| rest.split_once(", expected "));
    let Some((got, want)) = parsed else {
        return format!("{label} could not read its argument: {e}");
    };
    let want = match want {
        "f64" | "f32" => "a number".to_string(),
        "u32" => "a non-negative integer below 2**32".to_string(),
        "u64" | "usize" => "a non-negative integer".to_string(),
        "i64" | "isize" => "an integer".to_string(),
        "a sequence" => "a list".to_string(),
        "a map" => "a dict".to_string(),
        w if w.starts_with("a tuple of size 2") => "a pair of strings".to_string(),
        w => w.to_string(),
    };
    let quoted = |g: &str| g.split('`').nth(1).map(str::to_string);
    let got = if got == "null" {
        "None".to_string()
    } else if got == "sequence" {
        "a list".to_string()
    } else if got == "map" {
        "a dict".to_string()
    } else if let Some(s) = got.strip_prefix("string ") {
        format!("'{}'", s.trim_matches('"'))
    } else if got.starts_with("boolean") {
        match quoted(got).as_deref() {
            Some("true") => "True".into(),
            _ => "False".into(),
        }
    } else {
        quoted(got).unwrap_or_else(|| got.to_string())
    };
    format!("{label} must be {want}; got {got}")
}

/// A name DataFusion printed, without its quotes and qualifier.
fn unquote(name: &str) -> String {
    let name = name.trim();
    let name = name.rsplit_once("\".").map(|(_, n)| n).unwrap_or(name);
    let name = name.strip_prefix("?table?.").unwrap_or(name);
    name.trim_matches('"').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(m: &str) -> String {
        engine_message(&DataFusionError::Execution(m.into()))
    }

    #[test]
    fn strips_the_variant_prefix() {
        assert_eq!(exec("weight must be numeric"), "weight must be numeric");
        let e = DataFusionError::NotImplemented("hop(direction='x') is not supported".into());
        assert_eq!(engine_message(&e), "hop(direction='x') is not supported");
    }

    #[test]
    fn rewords_datafusion_and_arrow_messages() {
        let plan = |m: &str| engine_message(&DataFusionError::Plan(m.into()));
        assert_eq!(
            plan("Cannot coerce arithmetic expression Utf8 * Int64 to valid types"),
            "cannot apply '*' to string and int64 (string * int64); arithmetic needs numeric \
             operands"
        );
        assert_eq!(
            plan(
                "Cannot infer common argument type for logical boolean operation Boolean AND Int64"
            ),
            "'&' combines boolean predicates (comparisons such as ur.col('a') > 1); got bool \
             and int64"
        );
        assert_eq!(
            plan("Cannot infer common argument type for comparison operation Int64 IS DISTINCT FROM Boolean"),
            "'~' negates a boolean predicate (a comparison such as ur.col('a') > 1); got int64"
        );
        let schema = DataFusionError::SchemaError(
            Box::new(datafusion::common::SchemaError::FieldNotFound {
                field: Box::new(datafusion::common::Column::new_unqualified("nope")),
                valid_fields: vec![
                    datafusion::common::Column::new(Some("?table?"), "s"),
                    datafusion::common::Column::new(Some("?table?"), "d"),
                ],
            }),
            Box::new(None),
        );
        assert_eq!(
            engine_message(&schema),
            "unknown column 'nope'; the frame has ['s', 'd']."
        );
        let arrow =
            DataFusionError::ArrowError(Box::new(arrow::error::ArrowError::DivideByZero), None);
        assert!(engine_message(&arrow).starts_with("division by zero in an integer expression"));
        let sum = plan(
            "Internal error: Function 'sum' failed to match any signature, errors: Error during \
             planning: Function 'sum' requires Decimal, but received String (DataType: Utf8).",
        );
        assert!(
            sum.starts_with("group_by().agg(): sum() needs a numeric column; got a string column"),
            "{sum}"
        );
    }

    #[test]
    fn serde_errors_name_the_argument() {
        assert_eq!(
            serde_message("join(how=)", "invalid type: integer `1`, expected a string"),
            "join(how=) must be a string; got 1"
        );
        assert_eq!(
            serde_message(
                "rename()",
                "invalid type: null, expected a string at line 1 column 4"
            ),
            "rename() must be a string; got None"
        );
        assert_eq!(
            serde_message("x", "invalid value: integer `-1`, expected u32"),
            "x must be a non-negative integer below 2**32; got -1"
        );
    }

    #[test]
    fn a_context_labels_the_message_unless_it_names_a_call() {
        let inner = DataFusionError::Execution("cannot compare string with int64".into());
        let e = DataFusionError::Context("filter()".into(), Box::new(inner));
        assert_eq!(
            engine_message(&e),
            "filter(): cannot compare string with int64"
        );
        let inner = DataFusionError::Execution("join(on=): key 'x' is missing".into());
        let e = DataFusionError::Context("join()".into(), Box::new(inner));
        assert_eq!(engine_message(&e), "join(on=): key 'x' is missing");
    }
}
