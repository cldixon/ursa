//! The wording of engine errors a user reads.
//!
//! An engine error is a [`DataFusionError`], from DataFusion itself, from Arrow,
//! or one of Ursa's own messages carried in one. Its `Display` is for DataFusion's
//! developers: it starts with a variant prefix (`Execution error: `, `Arrow error:
//! `, ...), and DataFusion's own messages name its internals (the `?table?`
//! qualifier, signature lists, logical expressions, its type names). [`engine_message`]
//! gives the same error in Ursa's words: Ursa's own messages without the prefix,
//! and the DataFusion and Arrow errors a plan can reach reworded to say what was
//! wrong and what to do. Structured errors (a schema error, Arrow's divide by zero
//! or cast error) are read from their fields; DataFusion's planner reports type
//! errors as text, which is parsed. The original error stays the source
//! (`PlanError::source`, and the `__cause__` of the Python exception).
//!
//! A [`DataFusionError::Context`] says where an error came from. Its label leads
//! the message (`scan_edges('e.parquet'): no file matches the path ...`), unless
//! the message already starts with a call (`join(on=): ...`). A label holding
//! `{}` is a template the message is put into, for advice that depends on where
//! the error came from (`join(on=): the left and right keys have different types
//! ({}); ...`).
//!
//! A CSV value that does not parse as its column's inferred type is a
//! [`CsvValueError`]: the scan reads the column again, in file order, to find
//! the first such value, its line and its file, and names the column.

use arrow::datatypes::DataType;
use arrow::error::ArrowError;
use datafusion::common::SchemaError;
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

const DIVIDE_BY_ZERO: &str = "division by zero in an integer expression; divide by a float \
     (e.g. / 2.0) to get inf or NaN instead, or filter out the zero divisors first";

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
            if label.contains("{}") {
                return label.replacen("{}", &engine_message(inner), 1);
            }
            with_label(label, engine_message(inner))
        }
        DataFusionError::Shared(inner) => engine_message(inner),
        DataFusionError::Diagnostic(_, inner) => engine_message(inner),
        DataFusionError::SchemaError(s, _) => schema_message(s),
        DataFusionError::ArrowError(a, _) => {
            arrow_message(a).unwrap_or_else(|| reword(&e.to_string()))
        }
        DataFusionError::External(x) => match x.downcast_ref::<CsvValueError>() {
            Some(csv) => csv.message(),
            None => reword(&e.to_string()),
        },
        other => reword(&other.to_string()),
    }
}

/// A CSV value that does not parse as its column's inferred type (#222), as the
/// scan located it. Its `Display` is the engine's text; [`engine_message`]
/// gives it in Ursa's words.
#[derive(Debug)]
pub struct CsvValueError {
    /// The column's name.
    pub column: String,
    /// The column's inferred type.
    pub data_type: DataType,
    /// How many rows the types are inferred from.
    pub inferred_from: usize,
    /// The value, when the engine's error has it.
    pub value: Option<String>,
    /// The line of the file, from 1. The header is line 1.
    pub line: usize,
    /// The file, when the scan's paths can name more than one.
    pub file: Option<String>,
    /// The engine's error.
    pub source: ArrowError,
}

impl std::fmt::Display for CsvValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for CsvValueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl CsvValueError {
    /// The error in Ursa's words.
    pub fn message(&self) -> String {
        let ty = crate::compare::pa_name(&self.data_type);
        let at = match &self.file {
            Some(file) => format!("line {} of {}", self.line, crate::plan::py_str(file)),
            None => format!("line {}", self.line),
        };
        let (has, null_value) = match &self.value {
            Some(v) => (
                format!("has {}, which is not a valid {ty}", crate::plan::py_str(v)),
                format!("null_value={}", crate::plan::py_str(v)),
            ),
            None => (
                format!("has a value that is not a valid {ty}"),
                "null_value=".to_string(),
            ),
        };
        format!(
            "column {} was inferred as {ty} from the first {} rows, but {at} {has}. Fix the \
             value, pass {null_value} if it marks a missing value, or read the file with \
             pyarrow.csv (column_types=) and pass the table to ur.from_arrow().",
            crate::plan::py_str(&self.column),
            self.inferred_from
        )
    }
}

/// What arrow-csv's error for a value that does not parse as its column's type
/// says: [`csv_parse_fields`].
#[derive(Debug, PartialEq, Eq)]
pub struct CsvParseFields {
    /// The column's index in the file.
    pub column: usize,
    /// arrow-csv's line: the record's index from 0, the header included.
    pub line: usize,
    /// The value, when the error has it.
    pub value: Option<String>,
}

/// The fields of arrow-csv's error for a value that does not parse as its
/// column's type, or `None` for any other error. Arrow carries them as text
/// only (`ArrowError::ParseError`), in one of two fixed forms: one for a
/// boolean or a number or a date, and one for a timestamp.
pub fn csv_parse_fields(a: &ArrowError) -> Option<CsvParseFields> {
    let ArrowError::ParseError(m) = a else {
        return None;
    };
    // `Error while parsing value '{value}' as type '{type}' for column {i} at
    // line {n}. Row data: '{row}'`. The value may hold any text, so each
    // `' as type '` is tried until the rest has this form.
    if let Some(rest) = m.strip_prefix("Error while parsing value '") {
        for (i, _) in rest.match_indices("' as type '") {
            // The type's name has no quote in it.
            let Some((ty, tail)) = rest[i + "' as type '".len()..].split_once("' for column ")
            else {
                continue;
            };
            if ty.contains('\'') {
                continue;
            }
            let Some((column, tail)) = number_then(tail, " at line ") else {
                continue;
            };
            let Some((line, _)) = number_then(tail, ". Row data: '") else {
                continue;
            };
            return Some(CsvParseFields {
                column,
                line,
                value: Some(rest[..i].to_string()),
            });
        }
        return None;
    }
    // `Error parsing column {i} at line {n}: {cause}`, where the cause names
    // the value unless the value's time zone or range was the problem.
    let rest = m.strip_prefix("Error parsing column ")?;
    let (column, rest) = number_then(rest, " at line ")?;
    let (line, cause) = number_then(rest, ": ")?;
    let value = cause
        .strip_prefix("Parser error: Error parsing timestamp from '")
        .and_then(|v| v.rsplit_once("': "))
        .map(|(v, _)| v.to_string());
    Some(CsvParseFields {
        column,
        line,
        value,
    })
}

/// The number `s` starts with, and the text after the `sep` that follows it.
fn number_then<'a>(s: &'a str, sep: &str) -> Option<(usize, &'a str)> {
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    let rest = s[digits..].strip_prefix(sep)?;
    Some((s[..digits].parse().ok()?, rest))
}

/// Whether an engine error is about a column the frame does not have (Python's
/// `ColumnNotFoundError`): a schema error, or one of Ursa's own messages that
/// says a column is not found. A message from storage (a missing object whose
/// path holds the word "column") is not.
pub fn is_column_not_found(e: &DataFusionError) -> bool {
    match e {
        DataFusionError::Context(_, inner) | DataFusionError::Diagnostic(_, inner) => {
            is_column_not_found(inner)
        }
        DataFusionError::Shared(inner) => is_column_not_found(inner),
        DataFusionError::SchemaError(..) => true,
        DataFusionError::External(x) if x.is::<CsvValueError>() => false,
        DataFusionError::ObjectStore(_)
        | DataFusionError::IoError(_)
        | DataFusionError::ParquetError(_) => false,
        other => says_column_not_found(&other.to_string()),
    }
}

/// Whether `e` comes from values of one type read as another: Arrow's cast
/// error, or DataFusion finding no common type for a comparison.
pub fn mismatched_types(e: &DataFusionError) -> bool {
    match e.find_root() {
        DataFusionError::ArrowError(a, _) => matches!(a.as_ref(), ArrowError::CastError(_)),
        root => root
            .to_string()
            .contains("Cannot infer common argument type for comparison"),
    }
}

/// Whether a rendered message says a column is not found.
pub fn says_column_not_found(m: &str) -> bool {
    let lower = m.to_lowercase();
    lower.contains("no field named")
        || (lower.contains("column") && lower.contains("not found"))
        || lower.contains("schema error")
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

fn schema_message(s: &SchemaError) -> String {
    match s {
        SchemaError::FieldNotFound {
            field,
            valid_fields,
        } => {
            let have: Vec<String> = valid_fields
                .iter()
                .map(|c| crate::plan::py_str(&c.name))
                .collect();
            format!(
                "unknown column {}; the frame has [{}].",
                crate::plan::py_str(&field.name),
                have.join(", ")
            )
        }
        SchemaError::DuplicateQualifiedField { name, .. }
        | SchemaError::DuplicateUnqualifiedField { name } => duplicate_column(name),
        SchemaError::AmbiguousReference { field } => format!(
            "the column name {} is ambiguous: two inputs have it; rename() one of them first",
            crate::plan::py_str(&field.name)
        ),
    }
}

fn duplicate_column(name: &str) -> String {
    format!(
        "the result would have two columns named {}; give one of them another name \
         (rename() or a different output name)",
        crate::plan::py_str(name)
    )
}

fn arrow_message(a: &ArrowError) -> Option<String> {
    match a {
        ArrowError::DivideByZero => Some(DIVIDE_BY_ZERO.into()),
        ArrowError::CastError(m) => Some(cast_message(m)),
        ArrowError::InvalidArgumentError(m)
            if m.starts_with("must either specify a row count or at least one column") =>
        {
            Some("select() needs at least one column".into())
        }
        _ => None,
    }
}

/// Arrow's cast error in Ursa's words. Where it came from (a join's keys, a graph
/// column's ids) adds the advice, through a template context.
fn cast_message(m: &str) -> String {
    if let Some(rest) = m.strip_prefix("Cannot cast string ") {
        if let Some((value, ty)) = rest.rsplit_once(" to value of ") {
            let ty = type_name(ty.trim_end_matches(" type"));
            return format!("the string {value} cannot be read as {ty}");
        }
    }
    if let Some(rest) = m.strip_prefix("Casting from ") {
        if let Some((from, to)) = rest.split_once(" to ") {
            let to = to.trim_end_matches(" not supported");
            return format!(
                "a {} value cannot be converted to {}",
                type_name(from),
                type_name(to)
            );
        }
    }
    format!("a value cannot be converted: {m}")
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
    m
}

/// The text between `start` and the next `end` after it.
fn between<'a>(m: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = m.find(start)? + start.len();
    let j = m[i..].find(end)? + i;
    Some(&m[i..j])
}

/// A DataFusion type name (`Utf8`, `Dictionary(Int32, Utf8)`, `Timestamp(µs)`, ...)
/// as Ursa's messages print types (pyarrow's spelling: `string`,
/// `dictionary<values=string, ...>`, `timestamp[us]`, ...).
pub fn type_name(t: &str) -> String {
    let t = t.trim();
    match t.parse::<DataType>() {
        Ok(dt) => crate::compare::pa_name(&dt),
        Err(_) => t.to_lowercase(),
    }
}

/// The binary operators DataFusion prints between two types, longest first.
const BINARY_OPS: &[&str] = &[
    " IS NOT DISTINCT FROM ",
    " IS DISTINCT FROM ",
    " AND ",
    " OR ",
    " <= ",
    " >= ",
    " != ",
    " = ",
    " < ",
    " > ",
    " + ",
    " - ",
    " * ",
    " / ",
    " % ",
];

/// `left op right` split at the first operator outside any brackets, so a type
/// such as `Dictionary(Int32, Utf8)` or `Decimal128(10, 2)` stays whole.
fn split_binary(s: &str) -> Option<(&str, &str, &str)> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ if depth == 0 => {
                if let Some(op) = BINARY_OPS.iter().find(|op| s[i..].starts_with(**op)) {
                    return Some((&s[..i], op.trim(), &s[i + op.len()..]));
                }
            }
            _ => {}
        }
    }
    None
}

/// An operator DataFusion printed, as Ursa's expressions spell it.
fn op_symbol(op: &str) -> &str {
    match op {
        "=" | "IS NOT DISTINCT FROM" => "==",
        "AND" => "&",
        "OR" => "|",
        other => other,
    }
}

/// The Ursa wording of a DataFusion or Arrow message, when there is one.
fn translate(m: &str) -> Option<String> {
    // An aggregation over a column of a type it does not take.
    if let Some(func) = between(m, "Function '", "' failed to match any signature") {
        let got = between(m, "but received ", " (DataType")
            .map(|t| format!(" a {} column", type_name(t)))
            .unwrap_or_else(|| " this column".into());
        return Some(match func {
            "sum" | "avg" => {
                let name = if func == "avg" { "mean" } else { "sum" };
                format!(
                    "{name}() needs a numeric column, and got{got}; sum() and mean() take \
                     numbers only, while min(), max(), count() and n_unique() take strings too"
                )
            }
            other => format!("{other}() cannot aggregate{got}"),
        });
    }
    // Arithmetic over operands it does not apply to.
    if let Some(rest) = between(m, "Cannot coerce arithmetic expression ", " to valid types") {
        if let Some((l, op, r)) = split_binary(rest) {
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
        if let Some((l, op, r)) = split_binary(rest) {
            return Some(format!(
                "'{}' combines boolean predicates (comparisons such as ur.col('a') > 1); \
                 got {} and {}",
                op_symbol(op),
                type_name(l),
                type_name(r)
            ));
        }
    }
    if let Some(rest) =
        m.strip_prefix("Cannot infer common argument type for comparison operation ")
    {
        if let Some((l, op, r)) = split_binary(rest) {
            // `~x` is planned as `x IS DISTINCT FROM true`.
            if op == "IS DISTINCT FROM" && r.trim() == "Boolean" {
                return Some(format!(
                    "'~' negates a boolean predicate (a comparison such as ur.col('a') > 1); \
                     got {}",
                    type_name(l)
                ));
            }
            let (l, r) = (type_name(l), type_name(r));
            let sym = op_symbol(op);
            // A join matches its keys with `IS NOT DISTINCT FROM`; its context
            // adds the advice.
            return Some(if op == "IS NOT DISTINCT FROM" {
                format!("cannot compare {l} with {r} ({l} {sym} {r})")
            } else {
                format!("cannot compare {l} with {r} ({l} {sym} {r}); compare values of one type")
            });
        }
    }
    if let Some(rest) = m.strip_prefix("Cannot get result type for temporal operation ") {
        let ops = rest.split(": ").next().unwrap_or(rest);
        if let Some((l, op, r)) = split_binary(ops) {
            let (l, r) = (type_name(l), type_name(r));
            return Some(format!(
                "cannot apply '{op}' to {l} and {r} ({l} {op} {r}); this arithmetic is not \
                 defined for these types"
            ));
        }
    }
    // object_store's errors, reaching a store: the URL and what failed.
    if let Some(rest) = m.strip_prefix("Generic ") {
        if let Some((_, detail)) = rest.split_once(" error: ") {
            if let Some(req) = detail.strip_prefix("Error performing ") {
                let url = req.split_whitespace().nth(1).unwrap_or("");
                let cause = detail
                    .rsplit_once(" - ")
                    .map_or("", |(_, c)| c)
                    .trim_start_matches("HTTP error: ")
                    .trim();
                return Some(format!("could not read {url}: {cause}"));
            }
            return Some(detail.to_string());
        }
    }
    if let Some(rest) = m.strip_prefix("Configuration key: ") {
        if let Some((key, store)) = rest.split_once(" is not valid for store ") {
            return Some(format!(
                "storage_options key {key} is not an option of the {} store; see the object \
                 store's documentation for its keys",
                store.trim_end_matches('.').trim_matches('\'')
            ));
        }
    }
    if m.starts_with("Divide by zero error") {
        return Some(DIVIDE_BY_ZERO.into());
    }
    // A schema error DataFusion printed into another message; the structured one
    // is read from its fields (`schema_message`).
    if let Some(name) = m.strip_prefix("No field named ") {
        let name = name.split(". ").next().unwrap_or(name);
        return Some(format!(
            "unknown column {}.",
            crate::plan::py_str(&unquote(name))
        ));
    }
    for marker in [
        "duplicate qualified field name ",
        "duplicate unqualified field name ",
    ] {
        if let Some(i) = m.find(marker) {
            return Some(duplicate_column(&unquote(m[i + marker.len()..].trim())));
        }
    }
    if let Some(rest) = m.strip_prefix("Cast error: ") {
        return Some(cast_message(rest));
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

/// A field name DataFusion printed (`"?table?".x`, `x`), without its quotes and
/// qualifier.
fn unquote(name: &str) -> String {
    let name = name.trim();
    let name = name.rsplit_once("\".").map(|(_, n)| n).unwrap_or(name);
    let name = name.strip_prefix("?table?.").unwrap_or(name);
    name.trim_matches('"').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::common::Column;

    fn exec(m: &str) -> String {
        engine_message(&DataFusionError::Execution(m.into()))
    }

    fn plan(m: &str) -> String {
        engine_message(&DataFusionError::Plan(m.into()))
    }

    #[test]
    fn strips_the_variant_prefix() {
        assert_eq!(exec("weight must be numeric"), "weight must be numeric");
        let e = DataFusionError::NotImplemented("hop(direction='x') is not supported".into());
        assert_eq!(engine_message(&e), "hop(direction='x') is not supported");
    }

    #[test]
    fn rewords_datafusion_type_errors() {
        assert_eq!(
            plan("Cannot coerce arithmetic expression Utf8 * Int64 to valid types"),
            "cannot apply '*' to string and int64 (string * int64); arithmetic needs numeric \
             operands"
        );
        assert_eq!(
            plan(
                "Cannot coerce arithmetic expression Dictionary(Int32, Utf8) + Int64 to valid \
                 types"
            ),
            "cannot apply '+' to dictionary<values=string, indices=int32, ordered=0> and int64 \
             (dictionary<values=string, indices=int32, ordered=0> + int64); arithmetic needs \
             numeric operands"
        );
        assert_eq!(
            plan(
                "Cannot coerce arithmetic expression Decimal128(10, 2) * Dictionary(Int32, Utf8) \
                 to valid types"
            ),
            "cannot apply '*' to decimal128(10, 2) and dictionary<values=string, indices=int32, \
             ordered=0> (decimal128(10, 2) * dictionary<values=string, indices=int32, \
             ordered=0>); arithmetic needs numeric operands"
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
        assert_eq!(
            plan("Cannot infer common argument type for comparison operation Int64 IS NOT DISTINCT FROM Utf8"),
            "cannot compare int64 with string (int64 == string)"
        );
        let sum = plan(
            "Internal error: Function 'sum' failed to match any signature, errors: Error during \
             planning: Function 'sum' requires Decimal, but received String (DataType: Utf8).",
        );
        assert!(
            sum.starts_with("sum() needs a numeric column, and got a string column; "),
            "{sum}"
        );
    }

    #[test]
    fn rewords_storage_and_temporal_errors() {
        assert_eq!(
            exec(
                "Generic HTTP error: Error performing HEAD http://h:1/e.parquet in 2.06s, after \
                 10 retries, max_retries: 10, retry_timeout: 180s  - HTTP error: error sending \
                 request"
            ),
            "could not read http://h:1/e.parquet: error sending request"
        );
        assert_eq!(
            exec("Configuration key: 'nope' is not valid for store 'S3'."),
            "storage_options key 'nope' is not an option of the S3 store; see the object \
             store's documentation for its keys"
        );
        assert_eq!(
            plan(
                "Cannot get result type for temporal operation Timestamp(µs) * Timestamp(µs): \
                 Invalid argument error: Invalid timestamp arithmetic operation"
            ),
            "cannot apply '*' to timestamp[us] and timestamp[us] (timestamp[us] * \
             timestamp[us]); this arithmetic is not defined for these types"
        );
    }

    #[test]
    fn reads_structured_errors_from_their_fields() {
        let schema = DataFusionError::SchemaError(
            Box::new(SchemaError::FieldNotFound {
                field: Box::new(Column::new_unqualified("nope")),
                valid_fields: vec![
                    Column::new(Some("?table?"), "s, t"),
                    Column::new(Some("?table?"), "?table?.d"),
                ],
            }),
            Box::new(None),
        );
        // A name holding ", " or the qualifier's text stays whole.
        assert_eq!(
            engine_message(&schema),
            "unknown column 'nope'; the frame has ['s, t', '?table?.d']."
        );
        assert!(is_column_not_found(&schema));
        let dup = DataFusionError::SchemaError(
            Box::new(SchemaError::DuplicateUnqualifiedField { name: "age".into() }),
            Box::new(None),
        );
        assert!(engine_message(&dup).starts_with("the result would have two columns named 'age'"));
        let arrow = |a| DataFusionError::ArrowError(Box::new(a), None);
        assert_eq!(
            engine_message(&arrow(ArrowError::DivideByZero)),
            DIVIDE_BY_ZERO
        );
        assert_eq!(
            engine_message(&arrow(ArrowError::CastError(
                "Cannot cast string 'a' to value of Int64 type".into()
            ))),
            "the string 'a' cannot be read as int64"
        );
    }

    #[test]
    fn a_missing_object_is_not_a_missing_column() {
        let e = DataFusionError::ObjectStore(Box::new(object_store::Error::NotFound {
            path: "b/columns/x.parquet".into(),
            source: "not found".into(),
        }));
        assert!(!is_column_not_found(&e));
        assert!(is_column_not_found(&DataFusionError::Execution(
            "src column 'x' not found in the file".into()
        )));
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
    fn reads_the_fields_of_a_csv_parse_error() {
        let parse = |m: &str| csv_parse_fields(&ArrowError::ParseError(m.into()));
        assert_eq!(
            parse(
                "Error while parsing value 'yes' as type 'Boolean' for column 2 at line 1001. \
                 Row data: '[2,3,yes]'"
            ),
            Some(CsvParseFields {
                column: 2,
                line: 1001,
                value: Some("yes".into())
            })
        );
        // A value may hold the text that follows it.
        assert_eq!(
            parse(
                "Error while parsing value 'a' as type 'x' for' as type 'Int64' for column 0 at \
                 line 3. Row data: '[a' as type 'x' for]'"
            )
            .and_then(|f| f.value),
            Some("a' as type 'x' for".into())
        );
        assert_eq!(
            parse(
                "Error parsing column 1 at line 7: Parser error: Error parsing timestamp from \
                 'soon': timestamp must contain at least 10 characters"
            ),
            Some(CsvParseFields {
                column: 1,
                line: 7,
                value: Some("soon".into())
            })
        );
        assert_eq!(
            parse("Error parsing column 1 at line 7: Parser error: Invalid timezone \"X\"")
                .map(|f| f.value),
            Some(None)
        );
        assert_eq!(parse("encountered unterminated string"), None);
        assert_eq!(csv_parse_fields(&ArrowError::DivideByZero), None);
    }

    #[test]
    fn a_csv_value_error_names_the_column_and_what_to_do() {
        let e = |value: Option<&str>, file: Option<&str>| CsvValueError {
            column: "f".into(),
            data_type: DataType::Boolean,
            inferred_from: 1000,
            value: value.map(Into::into),
            line: 1002,
            file: file.map(Into::into),
            source: ArrowError::ParseError("engine text".into()),
        };
        let wrapped = DataFusionError::Context(
            "scan_edges('g.csv')".into(),
            Box::new(DataFusionError::External(Box::new(e(Some("yes"), None)))),
        );
        assert_eq!(
            engine_message(&wrapped),
            "scan_edges('g.csv'): column 'f' was inferred as bool from the first 1000 rows, but \
             line 1002 has 'yes', which is not a valid bool. Fix the value, pass \
             null_value='yes' if it marks a missing value, or read the file with pyarrow.csv \
             (column_types=) and pass the table to ur.from_arrow()."
        );
        assert!(!is_column_not_found(&wrapped));
        assert!(e(None, Some("/d/b.csv")).message().contains(
            "but line 1002 of '/d/b.csv' has a value that is not a valid bool. Fix the value, \
             pass null_value= if"
        ));
        assert_eq!(e(None, None).to_string(), "Parser error: engine text");
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
        let inner = DataFusionError::Execution("x".into());
        let e = DataFusionError::Context("a ({}) b".into(), Box::new(inner));
        assert_eq!(engine_message(&e), "a (x) b");
    }
}
