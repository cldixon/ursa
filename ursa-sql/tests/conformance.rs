//! The SQL/PGQ and DuckPGQ conformance suite (#299): the sqllogictest files
//! under `tests/conformance/`, each run in a fresh session, and the reviewed
//! list of the cases where Ursa differs, `tests/conformance/known_differences.toml`.
//!
//! The rules (see `tests/conformance/README.md`):
//!
//! 1. A case not on the list must give the reference's result.
//! 2. A case on the list must still differ, in the recorded way: the `error`
//!    regex matches Ursa's error. A case that now passes fails the suite with
//!    "remove this entry", so the list stays true.
//! 3. A debug dump (`AggregateFunction {`, `Physical plan does not support`)
//!    is never an accepted difference.
//!
//! The format is sqllogictest's: `statement ok`, `statement error <regex>`
//! and `query <types> [rowsort]` records, each followed by its SQL, then for a
//! query `----` and the expected rows, one per line, values separated by a
//! space, NULL as `NULL`. A comment `# id: <dir>/<name>` before a record names
//! it; the known-differences list refers to those ids.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::prelude::SessionContext;
use regex::Regex;

/// One record of a `.slt` file.
#[derive(Debug)]
struct Record {
    id: Option<String>,
    /// `file:line`, for messages.
    at: String,
    sql: String,
    expected: Expected,
}

#[derive(Debug)]
enum Expected {
    Ok,
    /// `statement error [regex]`: an empty regex matches any error.
    Error(String),
    Rows {
        columns: usize,
        rowsort: bool,
        rows: Vec<String>,
    },
}

/// One entry of the known-differences list.
#[derive(Debug, Default)]
struct Difference {
    id: String,
    kind: String,
    issue: Option<u32>,
    reason: String,
    /// The error Ursa must give, as a regex.
    error: Option<String>,
}

fn suite_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/conformance")
}

fn parse_slt(path: &Path) -> Vec<Record> {
    let text = std::fs::read_to_string(path).unwrap();
    let name = path.strip_prefix(suite_dir()).unwrap_or(path).display();
    let lines: Vec<&str> = text.lines().collect();
    let mut records = Vec::new();
    let mut id = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        if let Some(comment) = line.strip_prefix('#') {
            if let Some(rest) = comment.trim().strip_prefix("id:") {
                id = rest.split_whitespace().next().map(str::to_string);
            }
            i += 1;
            continue;
        }
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        let at = format!("{name}:{}", i + 1);
        let header: Vec<&str> = line.split_whitespace().collect();
        let mut expected = match header.as_slice() {
            ["statement", "ok"] => Expected::Ok,
            ["statement", "error", ..] => Expected::Error(
                line.trim_start()
                    .trim_start_matches("statement")
                    .trim_start()
                    .trim_start_matches("error")
                    .trim()
                    .to_string(),
            ),
            ["query", types, rest @ ..] => Expected::Rows {
                columns: types.len(),
                rowsort: rest.contains(&"rowsort"),
                rows: Vec::new(),
            },
            _ => panic!("{at}: expected a record, found '{line}'"),
        };
        i += 1;
        let mut sql = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() && lines[i].trim() != "----" {
            sql.push(lines[i]);
            i += 1;
        }
        if i < lines.len() && lines[i].trim() == "----" {
            i += 1;
            let Expected::Rows { rows, .. } = &mut expected else {
                panic!("{at}: only a query has '----'");
            };
            while i < lines.len() && !lines[i].trim().is_empty() {
                rows.push(lines[i].trim().to_string());
                i += 1;
            }
        }
        records.push(Record {
            id: id.take(),
            at,
            sql: sql.join("\n"),
            expected,
        });
    }
    records
}

/// The known differences: a small TOML subset, `[[difference]]` tables of
/// `key = "string"` and `key = number`.
fn parse_differences(path: &Path) -> Vec<Difference> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut out: Vec<Difference> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[difference]]" {
            out.push(Difference::default());
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("known_differences.toml:{}: '{line}'", n + 1));
        let value = value.trim();
        let string = || -> String {
            let inner = value
                .strip_prefix('"')
                .and_then(|v| v.rsplit_once('"'))
                .map(|(v, _)| v)
                .unwrap_or_else(|| panic!("known_differences.toml:{}: a string", n + 1));
            inner
                .replace("\\\\", "\u{0}")
                .replace("\\\"", "\"")
                .replace('\u{0}', "\\")
        };
        let entry = out
            .last_mut()
            .unwrap_or_else(|| panic!("known_differences.toml:{}: outside [[difference]]", n + 1));
        match key.trim() {
            "id" => entry.id = string(),
            "kind" => entry.kind = string(),
            "reason" => entry.reason = string(),
            "error" => entry.error = Some(string()),
            "issue" => entry.issue = value.split('#').next().unwrap().trim().parse().ok(),
            other => panic!("known_differences.toml:{}: unknown key '{other}'", n + 1),
        }
    }
    out
}

/// The rows of `batches`, as sqllogictest writes them.
fn rows(batches: &[RecordBatch]) -> Vec<String> {
    let options = FormatOptions::default().with_null("NULL");
    let mut rows = Vec::new();
    for batch in batches {
        let formatters: Vec<ArrayFormatter> = batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &options).unwrap())
            .collect();
        for row in 0..batch.num_rows() {
            let values: Vec<String> = formatters
                .iter()
                .map(|f| {
                    let value = f.value(row).to_string();
                    match value.is_empty() {
                        true => "(empty)".to_string(),
                        false => value,
                    }
                })
                .collect();
            rows.push(values.join(" "));
        }
    }
    rows
}

async fn run(ctx: &SessionContext, sql: &str) -> Result<Vec<RecordBatch>, String> {
    let message = |e| ursa_engine::user_message(&e);
    let mut batches = Vec::new();
    for statement in ursa_sql::parse_sql_for(ctx, sql).map_err(message)? {
        let frame = ursa_sql::execute(ctx, statement).await.map_err(message)?;
        batches = frame.collect().await.map_err(message)?;
    }
    Ok(batches)
}

/// `None` when `record` gives the reference's result; else how it differs.
async fn check(ctx: &SessionContext, record: &Record) -> Option<Outcome> {
    let result = run(ctx, &record.sql).await;
    match (&record.expected, result) {
        (Expected::Ok, Ok(_)) => None,
        (Expected::Error(regex), Err(e)) => {
            match regex.is_empty() || Regex::new(regex).unwrap().is_match(&e) {
                true => None,
                false => Some(Outcome::Error(e)),
            }
        }
        (Expected::Error(_), Ok(batches)) => Some(Outcome::Rows(rows(&batches))),
        (Expected::Ok | Expected::Rows { .. }, Err(e)) => Some(Outcome::Error(e)),
        (
            Expected::Rows {
                columns,
                rowsort,
                rows: expected,
            },
            Ok(batches),
        ) => {
            let mut got = rows(&batches);
            let width = batches.first().map(|b| b.num_columns());
            let mut expected = expected.clone();
            if *rowsort {
                got.sort();
                expected.sort();
            }
            match got == expected && width.is_none_or(|w| w == *columns) {
                true => None,
                false => Some(Outcome::Rows(got)),
            }
        }
    }
}

/// What Ursa gave instead of the reference's result.
enum Outcome {
    Error(String),
    Rows(Vec<String>),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Error(e) => write!(f, "error: {e}"),
            Outcome::Rows(rows) => write!(f, "{} rows:\n{}", rows.len(), rows.join("\n")),
        }
    }
}

#[tokio::test]
async fn conformance() {
    let dir = suite_dir();
    let differences = parse_differences(&dir.join("known_differences.toml"));
    let mut files = Vec::new();
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "slt") {
                files.push(path);
            }
        }
    }
    files.sort();
    assert!(!files.is_empty(), "no .slt files under {}", dir.display());

    let mut failures = Vec::new();
    let mut seen = Vec::new();
    // Per directory: (cases, passed, known differences).
    let mut report: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    for file in &files {
        let ctx = ursa_engine::new_session();
        let group = file
            .parent()
            .and_then(|p| p.strip_prefix(&dir).ok())
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        for record in parse_slt(file) {
            let outcome = check(&ctx, &record).await;
            let known = record
                .id
                .as_ref()
                .and_then(|id| differences.iter().find(|d| &d.id == id));
            if let Some(id) = &record.id {
                if seen.contains(id) {
                    failures.push(format!("{}: id '{id}' is used twice", record.at));
                }
                seen.push(id.clone());
            }
            let counts = report.entry(group.clone()).or_default();
            counts.0 += 1;
            match (outcome, known) {
                (None, None) => counts.1 += 1,
                (None, Some(d)) => failures.push(format!(
                    "{} ({}): now gives the reference's result; remove this entry from \
                     known_differences.toml",
                    record.at, d.id
                )),
                (Some(outcome), None) => failures.push(format!(
                    "{}{}: differs from the reference:\n{}\ngot {outcome}",
                    record.at,
                    record
                        .id
                        .as_deref()
                        .map(|i| format!(" ({i})"))
                        .unwrap_or_default(),
                    record.sql
                )),
                (Some(outcome), Some(d)) => {
                    let dump = matches!(&outcome, Outcome::Error(e)
                        if e.contains("AggregateFunction {") || e.contains("Physical plan does not support"));
                    let recorded = match (&d.error, &outcome) {
                        (Some(regex), Outcome::Error(e)) => Regex::new(regex).unwrap().is_match(e),
                        (Some(_), Outcome::Rows(_)) => false,
                        (None, _) => true,
                    };
                    if dump || !recorded {
                        failures.push(format!(
                            "{} ({}): differs, but not as known_differences.toml records:\n{}\ngot {outcome}",
                            record.at, d.id, record.sql
                        ));
                    } else {
                        counts.2 += 1;
                    }
                }
            }
        }
    }
    for d in &differences {
        if !seen.contains(&d.id) {
            failures.push(format!("known_differences.toml: no case has id '{}'", d.id));
        }
        if !["unsupported", "deliberate", "numeric-reference", "bug"].contains(&d.kind.as_str()) {
            failures.push(format!(
                "known_differences.toml: '{}' has kind '{}'",
                d.id, d.kind
            ));
        }
        if d.reason.is_empty() {
            failures.push(format!("known_differences.toml: '{}' needs a reason", d.id));
        }
        if d.kind == "bug" && d.issue.is_none() {
            failures.push(format!(
                "known_differences.toml: '{}' is a bug, so it needs an issue",
                d.id
            ));
        }
    }

    let mut summary =
        String::from("| directory | cases | pass | known differences |\n|---|---|---|---|\n");
    for (group, (cases, pass, known)) in &report {
        writeln!(summary, "| {group} | {cases} | {pass} | {known} |").unwrap();
    }
    println!("{summary}");
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}
