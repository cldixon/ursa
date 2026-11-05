//! The SQL docs and the agent skill, checked against the `ursa` binary.
//!
//! Three checks:
//!
//! - **`sql_docs_run`**: the SQL on every page under `docs/sql/` runs and
//!   prints what the page shows.
//! - **`skill_examples_run`**: the same for `skills/ursa-sql/SKILL.md`.
//! - **`function_reference_is_current`**: `docs/sql/reference/functions.md` is
//!   what `SELECT * FROM ursa_functions()` generates. To regenerate it, run
//!
//!   ```text
//!   URSA_BLESS=1 cargo test -p ursa-cli --test docs function_reference
//!   ```
//!
//! # The convention for SQL in the docs
//!
//! - A ```` ```sql ```` block runs. A page's blocks run in order, in one
//!   database, `docs.ursa`, in a fresh directory that holds the bundled
//!   datasets (`python/ursa/datasets/data/`). Each block is one
//!   `ursa docs.ursa -c "<block>"` process, so a block sees every view and
//!   graph the blocks before it defined, as a reader typing them into one
//!   shell would.
//! - A ```` ```text ```` block that directly follows a ```` ```sql ```` block
//!   (only blank lines between) is its output, and is compared exactly with
//!   what the block prints (trailing spaces and blank lines at either end are
//!   ignored). If the expected output starts with `Error:`, the block must
//!   fail, and the text is compared with its error message instead.
//! - A block with no ```` ```text ```` after it must run without error; its
//!   output is not checked.
//! - `<!-- skip -->` on the line before a ```` ```sql ```` block skips it. Use
//!   it for SQL that cannot run here: object storage, placeholder paths.
//! - Blocks indented inside an admonition or a content tab count too.
//! - Shell dot-commands and `ursa …` command lines go in ```` ```text ```` or
//!   ```` ```bash ```` blocks, which do not run. The command lines of the
//!   Getting Started page are checked by `tests/getting_started.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// One ```` ```sql ```` block: where it is, its SQL, and its expected output.
#[derive(Debug)]
struct Example {
    line: usize,
    sql: String,
    expected: Option<String>,
    skip: bool,
}

/// A fenced block: its language, its first line number and its text,
/// dedented to its fence.
struct Fence {
    lang: String,
    line: usize,
    body: String,
    /// Whether `<!-- skip -->` is on the line before it.
    skip: bool,
    /// Whether only blank lines separate it from the previous fence.
    adjacent: bool,
}

fn fences(text: &str) -> Vec<Fence> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    // The index of the line after the previous fence's closing line.
    let mut after_previous: Option<usize> = None;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let Some(info) = trimmed.strip_prefix("```") else {
            i += 1;
            continue;
        };
        let indent = line.len() - trimmed.len();
        let lang = info.split_whitespace().next().unwrap_or("").to_string();
        let skip = i > 0 && lines[i - 1].trim() == "<!-- skip -->";
        let adjacent =
            after_previous.is_some_and(|start| lines[start..i].iter().all(|l| l.trim().is_empty()));
        let start = i + 1;
        let mut end = start;
        while end < lines.len() && lines[end].trim() != "```" {
            end += 1;
        }
        let body = lines[start..end.min(lines.len())]
            .iter()
            .map(|l| l.get(indent..).unwrap_or(l.trim_start()))
            .collect::<Vec<_>>()
            .join("\n");
        out.push(Fence {
            lang,
            line: i + 1,
            body,
            skip,
            adjacent,
        });
        i = end + 1;
        after_previous = Some(i.min(lines.len()));
    }
    out
}

/// The ```` ```sql ```` blocks of a page, each with the ```` ```text ````
/// block that follows it, if any.
fn examples(text: &str) -> Vec<Example> {
    let fences = fences(text);
    let mut out = Vec::new();
    for (i, fence) in fences.iter().enumerate() {
        if fence.lang != "sql" {
            continue;
        }
        let expected = fences
            .get(i + 1)
            .filter(|next| next.lang == "text" && next.adjacent)
            .map(|next| next.body.clone());
        out.push(Example {
            line: fence.line,
            sql: fence.body.clone(),
            expected,
            skip: fence.skip,
        });
    }
    out
}

/// Trailing spaces and blank lines at either end don't count.
fn normalize(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_string()
}

/// A fresh directory holding the bundled datasets.
fn data_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let data = workspace().join("python/ursa/datasets/data");
    for entry in std::fs::read_dir(&data).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            std::fs::copy(&path, dir.path().join(path.file_name().unwrap())).unwrap();
        }
    }
    dir
}

/// Run every example of the page at `path`, in order, in one database.
/// Returns the failures, each naming the page and line.
fn run_page(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap();
    let shown = path
        .strip_prefix(workspace())
        .unwrap_or(path)
        .display()
        .to_string();
    let dir = data_dir();
    let mut failures = Vec::new();
    for example in examples(&text) {
        if example.skip {
            continue;
        }
        let output = Command::new(env!("CARGO_BIN_EXE_ursa"))
            .args(["docs.ursa", "-c", &example.sql])
            .current_dir(dir.path())
            .env_remove("URSA_INDEX_STATS")
            .stdin(Stdio::null())
            .output()
            .expect("ursa runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let at = format!("{shown}:{}", example.line);
        match (&example.expected, output.status.success()) {
            (Some(expected), _) if expected.trim_start().starts_with("Error:") => {
                if output.status.success() {
                    failures.push(format!(
                        "{at}: expected an error, but the block ran\n{}",
                        example.sql
                    ));
                } else if normalize(&stderr) != normalize(expected) {
                    failures.push(format!(
                        "{at}: the error differs\n--- expected\n{}\n--- got\n{}",
                        normalize(expected),
                        normalize(&stderr)
                    ));
                }
            }
            (_, false) => {
                failures.push(format!("{at}: the block failed\n{}\n{stderr}", example.sql))
            }
            (Some(expected), true) => {
                if normalize(&stdout) != normalize(expected) {
                    failures.push(format!(
                        "{at}: the output differs\n--- expected\n{}\n--- got\n{}",
                        normalize(expected),
                        normalize(&stdout)
                    ));
                }
            }
            (None, true) => {}
        }
    }
    failures
}

fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            markdown_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

fn check(pages: &[PathBuf]) {
    let mut failures = Vec::new();
    let mut ran = 0;
    for page in pages {
        ran += examples(&std::fs::read_to_string(page).unwrap())
            .iter()
            .filter(|e| !e.skip)
            .count();
        failures.extend(run_page(page));
    }
    assert!(ran > 0, "no SQL examples found in {pages:?}");
    assert!(
        failures.is_empty(),
        "{} of {ran} SQL examples failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn sql_docs_run() {
    let mut pages = Vec::new();
    markdown_files(&workspace().join("docs/sql"), &mut pages);
    check(&pages);
}

#[test]
fn skill_examples_run() {
    check(&[workspace().join("skills/ursa-sql/SKILL.md")]);
}

#[test]
fn the_convention_is_parsed() {
    let page = "Text.\n\n```sql\nSELECT 1;\n```\n\n```text\nout\n```\n\n\
                <!-- skip -->\n```sql\nSELECT 2;\n```\n\nProse.\n\n```text\nnot output\n```\n\n\
                !!! note\n\n    ```sql\n    SELECT 3;\n    ```\n";
    let examples = examples(page);
    assert_eq!(examples.len(), 3);
    assert_eq!(examples[0].sql, "SELECT 1;");
    assert_eq!(examples[0].expected.as_deref(), Some("out"));
    assert!(!examples[0].skip);
    assert!(examples[1].skip && examples[1].expected.is_none());
    assert_eq!(examples[2].sql, "SELECT 3;");
}

// The function reference.

/// Where the generated reference lives.
fn reference_path() -> PathBuf {
    workspace().join("docs/sql/reference/functions.md")
}

const REGENERATE: &str = "URSA_BLESS=1 cargo test -p ursa-cli --test docs function_reference";

#[test]
fn function_reference_is_current() {
    let output = Command::new(env!("CARGO_BIN_EXE_ursa"))
        .args(["-f", "json", "-c", "SELECT * FROM ursa_functions()"])
        .stdin(Stdio::null())
        .output()
        .expect("ursa runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    let generated = reference(&rows);
    let path = reference_path();
    if std::env::var_os("URSA_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &generated).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "docs/sql/reference/functions.md is out of date with ursa_functions(). \
         Regenerate it with\n\n    {REGENERATE}\n\nand commit the result."
    );
}

fn text<'a>(value: &'a serde_json::Value, key: &str) -> &'a str {
    value.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn list<'a>(value: &'a serde_json::Value, key: &str) -> &'a [serde_json::Value] {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map_or(&[], Vec::as_slice)
}

/// A table cell: pipes escaped, empty shown as a dash.
fn cell(text: &str) -> String {
    if text.is_empty() {
        "–".to_string()
    } else {
        text.replace('|', "\\|")
    }
}

fn code(text: &str) -> String {
    format!("`{text}`")
}

const FAMILIES: [(&str, &str); 5] = [
    ("algorithm", "Algorithms"),
    ("statistic", "Statistics"),
    ("aggregation", "Neighbour aggregation"),
    ("traversal", "Traversals"),
    ("combinator", "Combining functions"),
];

/// The reference page, from the rows of `ursa_functions()`.
fn reference(rows: &[serde_json::Value]) -> String {
    let mut out = String::new();
    out.push_str(
        "<!-- Generated from SELECT * FROM ursa_functions(). Do not edit by hand: run\n     ",
    );
    out.push_str(REGENERATE);
    out.push_str("\n-->\n\n");
    out.push_str("# Function reference\n\n");
    out.push_str(
        "Every graph function Ursa's SQL engine offers. The page is generated from \
         `SELECT * FROM ursa_functions()`, so it matches the engine you run.\n\n\
         An algorithm or a statistic takes the graph as its first argument, then named \
         parameters with `=>`. It returns one row per node: the node's `id` and its output \
         columns. `neighbors_agg` does too, from a value per node it reads. A traversal \
         returns edges or walks. `algorithms` runs several per-node functions in one \
         call.\n\n\
         A parameter's default is the SQL literal used when the parameter is left out. \
         `required` means the call must give it. A `string` parameter with choices takes one \
         of them, quoted: `direction => 'both'`. An `expression` parameter takes an \
         expression over the edge table's columns: `weight => log10(1 + weight)`, or for \
         `neighbors_agg`'s `attribute` over the node table's. A `query` parameter takes a \
         subquery in parentheses.\n\n\
         A `node ids` or `node id` parameter takes integers or strings: a list, a query of \
         one column, or one value. On a graph with integer ids, a string must be an integer, \
         such as `'7'`; floats (even `2.0`), booleans and dates are refused. On a graph with \
         string ids, an integer is taken as its string. A NULL, or an id not in the graph, \
         matches no node.\n\n",
    );
    for (family, heading) in FAMILIES {
        let members: Vec<&serde_json::Value> = rows
            .iter()
            .filter(|row| text(row, "family") == family)
            .collect();
        if members.is_empty() {
            continue;
        }
        out.push_str(&format!("## {heading}\n\n"));
        let links: Vec<String> = members
            .iter()
            .map(|row| text(row, "name"))
            .map(|name| format!("[`{name}`](#{name})"))
            .collect();
        out.push_str(&links.join(" · "));
        out.push_str("\n\n");
        for row in members {
            out.push_str(&function_section(row));
        }
    }
    // Any family this page does not know yet still appears.
    let others: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|row| !FAMILIES.iter().any(|(f, _)| text(row, "family") == *f))
        .collect();
    if !others.is_empty() {
        out.push_str("## Other functions\n\n");
        for row in others {
            out.push_str(&function_section(row));
        }
    }
    out.truncate(out.trim_end().len());
    out.push('\n');
    out
}

fn function_section(row: &serde_json::Value) -> String {
    let name = text(row, "name");
    let mut out = format!("### {name}\n\n{}\n\n", text(row, "summary"));
    let params = list(row, "parameters");

    // The call, with every parameter at its default.
    let mut call = vec!["graph".to_string()];
    for p in params {
        let default = text(p, "default");
        call.push(format!(
            "{} => {}",
            text(p, "name"),
            if default == "required" {
                "…"
            } else {
                default
            }
        ));
    }
    if name != "algorithms" {
        out.push_str(&format!("```text\n{name}({})\n```\n\n", call.join(", ")));
    }

    if !params.is_empty() {
        out.push_str("| Parameter | Type | Default | Choices | Description |\n");
        out.push_str("|---|---|---|---|---|\n");
        for p in params {
            let choices: Vec<String> = list(p, "choices")
                .iter()
                .filter_map(|c| c.as_str())
                .map(|c| code(&format!("'{c}'")))
                .collect();
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                code(text(p, "name")),
                cell(text(p, "type")),
                code(text(p, "default")),
                cell(&choices.join(", ")),
                cell(text(p, "doc")),
            ));
        }
        out.push('\n');
    }

    let directions: Vec<String> = list(row, "directions")
        .iter()
        .filter_map(|d| d.as_str())
        .map(|d| code(&format!("'{d}'")))
        .collect();
    if !directions.is_empty() {
        out.push_str(&format!("**Directions:** {}\n\n", directions.join(", ")));
    }

    let outputs = list(row, "outputs");
    if !outputs.is_empty() {
        let mut columns: Vec<String> = Vec::new();
        let shape = match text(row, "family") {
            "algorithm" => {
                columns.push(format!("{} (the node)", code("id")));
                "One row per node: "
            }
            "statistic" => "One row: ",
            _ => "",
        };
        for o in outputs {
            let nullable = if o.get("nullable").and_then(|n| n.as_bool()) == Some(true) {
                ", may be NULL"
            } else {
                ""
            };
            columns.push(format!(
                "{} ({}{nullable})",
                code(text(o, "name")),
                text(o, "type")
            ));
        }
        out.push_str(&format!("**Returns:** {shape}{}\n\n", columns.join(", ")));
    }

    let networkx = text(row, "networkx");
    if !networkx.is_empty() {
        out.push_str(&format!("**NetworkX:** `{networkx}`\n\n"));
    }
    out
}
