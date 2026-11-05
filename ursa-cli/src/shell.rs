//! The shell's read–eval loop, without the terminal.
//!
//! [`Shell::feed`] takes one line of input: a dot-command, or a line of SQL
//! that is added to the statement being typed and run once it ends with `;`
//! (see [`crate::input`]). Results go to the shell's output and errors to its
//! error stream, and an error never ends the session. The terminal side (line
//! editing, history, Ctrl-C) is in [`crate::repl`]; `-init` files and
//! `~/.ursarc` run through [`Shell::run_file`], like `.read`.
//!
//! The dot-commands follow DuckDB's shell where it has the same command.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use arrow::array::{Array, AsArray};
use arrow::compute::cast;
use arrow::datatypes::DataType;
use clap::ValueEnum;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;
use datafusion::sql::sqlparser::dialect::{dialect_from_str, Dialect, GenericDialect};

use crate::input::{self, Status};
use crate::print::{has_columns, write_stream, OutputFormat};

/// What the caller does after a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// `.quit` or `.exit`.
    Exit,
}

/// How deeply `.read` may nest, so a file that reads itself stops.
const MAX_READ_DEPTH: usize = 16;

/// The dot-commands, with their arguments and a line of help, in `.help`
/// order.
pub const COMMANDS: &[(&str, &str, &str)] = &[
    (".describe", "NAME", "Describe a graph, table or view"),
    (".exit", "", "Exit the shell (also Ctrl-D)"),
    (
        ".functions",
        "",
        "List the graph functions and their parameters",
    ),
    (".graphs", "", "List the graphs (SHOW GRAPHS)"),
    (".help", "", "Show this message"),
    (
        ".mode",
        "MODE",
        "Set the output mode: table, csv, tsv, json or ndjson",
    ),
    (
        ".open",
        "FILE.ursa",
        "Open a database file, before anything is defined (as ATTACH)",
    ),
    (".quit", "", "Exit the shell (also Ctrl-D)"),
    (".read", "FILE", "Run the SQL and dot-commands in FILE"),
    (".tables", "", "List the tables and views"),
    (".timer", "on|off", "Show how long each statement takes"),
];

/// A shell session: an engine session, the shell's settings, and the
/// statement being typed.
pub struct Shell<O: Write, E: Write> {
    ctx: SessionContext,
    mode: OutputFormat,
    timer: bool,
    /// SQL typed so far that is not yet a complete statement.
    buffer: String,
    /// How many `.read`s (or init files) are running.
    depth: usize,
    out: O,
    err: E,
}

impl<O: Write, E: Write> Shell<O, E> {
    pub fn new(ctx: SessionContext, mode: OutputFormat, out: O, err: E) -> Self {
        Self {
            ctx,
            mode,
            timer: false,
            buffer: String::new(),
            depth: 0,
            out,
            err,
        }
    }

    pub fn ctx(&self) -> &SessionContext {
        &self.ctx
    }

    pub fn mode(&self) -> OutputFormat {
        self.mode
    }

    pub fn set_mode(&mut self, mode: OutputFormat) {
        self.mode = mode;
    }

    /// Whether a statement is partly typed (the prompt shows a continuation).
    pub fn is_pending(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// Drop the statement being typed (Ctrl-C).
    pub fn cancel_input(&mut self) {
        self.buffer.clear();
    }

    /// Take one line of input. A line starting with `.` is a dot-command
    /// unless it continues a statement.
    pub async fn feed(&mut self, line: &str) -> Flow {
        if self.buffer.is_empty() && line.trim_start().starts_with('.') {
            return self.command(line.trim()).await;
        }
        self.buffer.push_str(line);
        self.buffer.push('\n');
        match input::status(&self.buffer, self.dialect().as_ref()) {
            Status::Empty => self.buffer.clear(),
            Status::Incomplete => {}
            Status::Complete => {
                // Taken before running, so a query cancelled with Ctrl-C
                // leaves nothing behind.
                let sql = std::mem::take(&mut self.buffer);
                self.run_sql(&sql).await;
            }
        }
        Flow::Continue
    }

    /// Run a file of SQL and dot-commands, as `.read` does. A last statement
    /// without a `;` runs too. Errors are printed and the file goes on.
    pub async fn run_file(&mut self, path: &Path) -> Flow {
        if self.depth >= MAX_READ_DEPTH {
            self.error(&format!(
                "cannot read '{}': .read is nested more than {MAX_READ_DEPTH} deep",
                path.display()
            ));
            return Flow::Continue;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                self.error(&format!("cannot read '{}': {e}", path.display()));
                return Flow::Continue;
            }
        };
        // The file's statements are its own: whatever was being typed waits.
        let typed = std::mem::take(&mut self.buffer);
        self.depth += 1;
        let mut flow = Flow::Continue;
        for line in text.lines() {
            flow = Box::pin(self.feed(line)).await;
            if flow == Flow::Exit {
                break;
            }
        }
        let rest = std::mem::replace(&mut self.buffer, typed);
        if flow == Flow::Continue && input::status(&rest, self.dialect().as_ref()) != Status::Empty
        {
            self.run_sql(&rest).await;
        }
        self.depth -= 1;
        flow
    }

    /// The session's SQL dialect, for telling where statements end.
    fn dialect(&self) -> Box<dyn Dialect> {
        let name = self.ctx.state().config().options().sql_parser.dialect;
        dialect_from_str(name).unwrap_or_else(|| Box::new(GenericDialect {}))
    }

    fn error(&mut self, message: &str) {
        let _ = writeln!(self.err, "Error: {message}");
        let _ = self.err.flush();
    }

    fn say(&mut self, message: &str) {
        let _ = writeln!(self.out, "{message}");
        let _ = self.out.flush();
    }

    /// Parse all of `sql`, then run each statement and print its result. The
    /// first error is printed and the rest of `sql` is skipped.
    async fn run_sql(&mut self, sql: &str) {
        if let Err(e) = Box::pin(self.try_run_sql(sql)).await {
            self.error(&ursa_engine::user_message(&e));
        }
    }

    async fn try_run_sql(&mut self, sql: &str) -> Result<()> {
        let statements = ursa_sql::parse_sql_with_text_for(&self.ctx, sql)?;
        for (statement, text) in statements {
            let start = Instant::now();
            let frame = ursa_sql::execute_with_text(&self.ctx, statement, &text).await?;
            let stream = frame.execute_stream().await?;
            if has_columns(&stream.schema()) {
                let config = self.ctx.state().config().options().format.clone();
                let options = (&config).try_into()?;
                write_stream(self.mode, stream, &mut self.out, &options).await?;
            }
            if self.timer {
                let seconds = start.elapsed().as_secs_f64();
                self.say(&format!("Run Time: {seconds:.3}s"));
            }
        }
        Ok(())
    }

    /// Run a dot-command (`line` is trimmed and starts with `.`).
    async fn command(&mut self, line: &str) -> Flow {
        let (name, arg) = match line.split_once(char::is_whitespace) {
            Some((name, arg)) => (name, arg.trim()),
            None => (line, ""),
        };
        match name {
            ".quit" | ".exit" => return Flow::Exit,
            ".help" => self.help(),
            ".mode" => self.mode_command(arg),
            ".timer" => match arg.to_ascii_lowercase().as_str() {
                "" => {
                    let state = if self.timer { "on" } else { "off" };
                    self.say(&format!("The timer is {state}."));
                }
                "on" => self.timer = true,
                "off" => self.timer = false,
                _ => self.error("usage: .timer on|off"),
            },
            ".tables" => self.tables().await,
            ".graphs" => self.run_sql("SHOW GRAPHS;").await,
            ".describe" => self.describe(arg).await,
            ".read" => match path_arg(arg) {
                Some(path) => return self.run_file(&path).await,
                None => self.error("usage: .read FILE"),
            },
            ".open" => self.open(arg).await,
            ".functions" => self.functions().await,
            _ => {
                let message = match did_you_mean(name) {
                    Some(command) => format!(
                        "unknown command '{name}'; did you mean '{command}'? Enter .help for \
                         the list"
                    ),
                    None => format!("unknown command '{name}'; enter .help for the list"),
                };
                self.error(&message);
            }
        }
        Flow::Continue
    }

    fn help(&mut self) {
        let mut text = String::from(
            "Enter SQL statements ending with ';'. A statement may span lines.\n\
             Ctrl-C cancels the current input or query; Ctrl-D exits.\n\n",
        );
        for (name, args, help) in COMMANDS {
            let usage = format!("{name} {args}");
            text.push_str(&format!("{usage:<22}{help}\n"));
        }
        let _ = write!(self.out, "{text}");
        let _ = self.out.flush();
    }

    fn mode_command(&mut self, arg: &str) {
        if arg.is_empty() {
            let name = mode_name(self.mode);
            self.say(&format!("The output mode is {name}."));
            return;
        }
        match OutputFormat::from_str(arg, true) {
            Ok(OutputFormat::Arrow) => self.error(
                "the shell cannot show the arrow format; to write an Arrow stream, run \
                 ursa -f arrow -c \"<sql>\" > out.arrows",
            ),
            Ok(mode) => self.mode = mode,
            Err(_) => self.error(&format!(
                "unknown mode '{arg}'; the modes are table, csv, tsv, json and ndjson"
            )),
        }
    }

    /// `.tables`: the tables and views, outside `information_schema` and the
    /// graphs' own schemas (which `.graphs` lists).
    async fn tables(&mut self) {
        let graphs = match Box::pin(self.graph_names()).await {
            Ok(graphs) => graphs,
            Err(e) => return self.error(&ursa_engine::user_message(&e)),
        };
        let options = self.ctx.state().config().options().catalog.clone();
        let mut excluded = vec![quote_literal("information_schema")];
        excluded.extend(graphs.iter().map(|g| quote_literal(g)));
        let sql = format!(
            "SELECT CASE WHEN table_schema = {schema} THEN table_name \
                         ELSE table_schema || '.' || table_name END AS name, \
                    CASE WHEN table_type = 'VIEW' THEN 'view' ELSE 'table' END AS type \
             FROM information_schema.tables \
             WHERE table_catalog = {catalog} AND table_schema NOT IN ({excluded}) \
             ORDER BY table_schema <> {schema}, table_schema, table_name;",
            schema = quote_literal(&options.default_schema),
            catalog = quote_literal(&options.default_catalog),
            excluded = excluded.join(", "),
        );
        self.run_sql(&sql).await;
    }

    /// `.describe NAME`: `DESCRIBE GRAPH` for a graph, `DESCRIBE` otherwise.
    async fn describe(&mut self, name: &str) {
        let name = name.trim_end_matches(';').trim();
        if name.is_empty() {
            return self.error("usage: .describe NAME (a graph, table or view)");
        }
        let graphs = match Box::pin(self.graph_names()).await {
            Ok(graphs) => graphs,
            Err(e) => return self.error(&ursa_engine::user_message(&e)),
        };
        let unquoted = name.trim_matches('"');
        let is_graph = graphs.iter().any(|g| {
            if name.starts_with('"') {
                g == unquoted
            } else {
                g.eq_ignore_ascii_case(name)
            }
        });
        let sql = if is_graph {
            format!("DESCRIBE GRAPH {name};")
        } else {
            format!("DESCRIBE {name};")
        };
        self.run_sql(&sql).await;
    }

    /// `.open FILE.ursa`: `ATTACH`, with its rules (only in a session that
    /// has no database and has defined nothing yet).
    async fn open(&mut self, arg: &str) {
        let Some(path) = path_arg(arg) else {
            return self.error("usage: .open FILE.ursa");
        };
        let shown = path.display().to_string();
        if !shown.ends_with(".ursa") {
            return self.error(&format!(
                "'{shown}' is not a database file (those end in .ursa)"
            ));
        }
        let sql = format!("ATTACH {};", quote_literal(&shown));
        match Box::pin(self.try_run_sql(&sql)).await {
            Ok(()) => self.say(&format!("Opened {shown}.")),
            Err(e) => self.error(&ursa_engine::user_message(&e)),
        }
    }

    /// `.functions`: `SELECT * FROM ursa_functions()`.
    async fn functions(&mut self) {
        self.run_sql("SELECT * FROM ursa_functions();").await;
    }

    /// The graphs' names, from `SHOW GRAPHS`.
    async fn graph_names(&self) -> Result<Vec<String>> {
        let mut statements = ursa_sql::parse_sql_with_text_for(&self.ctx, "SHOW GRAPHS")?;
        let (statement, text) = statements.remove(0);
        let batches = ursa_sql::execute_with_text(&self.ctx, statement, &text)
            .await?
            .collect()
            .await?;
        let mut names = Vec::new();
        for batch in batches {
            let column = batch.column_by_name("name").ok_or_else(|| {
                DataFusionError::Internal("SHOW GRAPHS has no 'name' column".into())
            })?;
            let column = cast(column, &DataType::Utf8)?;
            let column = column.as_string::<i32>();
            names.extend(
                (0..column.len())
                    .filter(|&i| column.is_valid(i))
                    .map(|i| column.value(i).to_string()),
            );
        }
        Ok(names)
    }
}

/// The name `.mode` shows for a format.
fn mode_name(mode: OutputFormat) -> String {
    mode.to_possible_value()
        .map(|v| v.get_name().to_string())
        .unwrap_or_default()
}

/// A file argument, without surrounding quotes.
fn path_arg(arg: &str) -> Option<PathBuf> {
    let arg = arg.trim();
    let arg = ['\'', '"']
        .iter()
        .find_map(|&q| arg.strip_prefix(q).and_then(|a| a.strip_suffix(q)))
        .unwrap_or(arg);
    (!arg.is_empty()).then(|| PathBuf::from(arg))
}

/// `s` as a SQL string literal.
fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The dot-command closest to `name`, if one is close enough to be a typo.
pub fn did_you_mean(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    COMMANDS
        .iter()
        .map(|(command, _, _)| (*command, edit_distance(&name, command)))
        .filter(|(command, distance)| *distance <= 2.max(command.len() / 3))
        .min_by_key(|(_, distance)| *distance)
        .map(|(command, _)| command)
}

/// Levenshtein distance, in characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous + usize::from(ca != *cb);
            previous = row[j + 1];
            row[j + 1] = substitution.min(row[j] + 1).min(previous + 1);
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestShell = Shell<Vec<u8>, Vec<u8>>;

    fn shell() -> TestShell {
        Shell::new(
            ursa_engine::new_session(),
            OutputFormat::Table,
            Vec::new(),
            Vec::new(),
        )
    }

    /// Feed each line, then take what was printed to stdout and stderr.
    async fn feed(shell: &mut TestShell, lines: &[&str]) -> (String, String) {
        for line in lines {
            shell.feed(line).await;
        }
        take(shell)
    }

    fn take(shell: &mut TestShell) -> (String, String) {
        let out = String::from_utf8(std::mem::take(&mut shell.out)).unwrap();
        let err = String::from_utf8(std::mem::take(&mut shell.err)).unwrap();
        (out, err)
    }

    const KARATE_GRAPH: &str = "CREATE PROPERTY GRAPH karate EDGE TABLES (ties SOURCE KEY (src) \
                                DESTINATION KEY (dst));";

    #[tokio::test]
    async fn a_statement_runs_once_it_ends() {
        let mut shell = shell();
        assert_eq!(shell.feed("SELECT 1 AS x,").await, Flow::Continue);
        assert!(shell.is_pending());
        assert_eq!(take(&mut shell), (String::new(), String::new()));
        shell.feed("  2 AS y").await;
        assert!(shell.is_pending());
        let (out, err) = feed(&mut shell, &[";"]).await;
        assert!(!shell.is_pending());
        assert_eq!(err, "");
        assert_eq!(
            out,
            "+---+---+\n| x | y |\n+---+---+\n| 1 | 2 |\n+---+---+\n"
        );
    }

    #[tokio::test]
    async fn quotes_and_comments_keep_a_statement_open() {
        let mut shell = shell();
        shell.set_mode(OutputFormat::Csv);
        let (out, _) = feed(&mut shell, &["SELECT 'a;", "b' AS s -- not done;"]).await;
        assert_eq!(out, "");
        assert!(shell.is_pending());
        let (out, _) = feed(&mut shell, &["/* still; */ ;"]).await;
        assert_eq!(out, "s\n\"a;\nb\"\n");
    }

    #[tokio::test]
    async fn several_statements_on_a_line() {
        let mut shell = shell();
        shell.set_mode(OutputFormat::Csv);
        let (out, err) = feed(
            &mut shell,
            &["CREATE VIEW v AS SELECT 1 AS x; SELECT x FROM v; SELECT 2 AS y;"],
        )
        .await;
        assert_eq!(err, "");
        assert_eq!(out, "x\n1\ny\n2\n");
    }

    #[tokio::test]
    async fn comments_and_blank_lines_alone_do_nothing() {
        let mut shell = shell();
        let (out, err) = feed(&mut shell, &["", "  ", "-- a note", "/* another */"]).await;
        assert_eq!((out.as_str(), err.as_str()), ("", ""));
        assert!(!shell.is_pending());
    }

    #[tokio::test]
    async fn ursa_statements_work() {
        let mut shell = shell();
        shell.set_mode(OutputFormat::Csv);
        let (out, err) = feed(
            &mut shell,
            &[
                "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2), (2, 3)) AS t(src, dst);",
                "CREATE PROPERTY GRAPH karate",
                "  EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
                "SHOW GRAPHS;",
            ],
        )
        .await;
        assert_eq!(err, "");
        assert!(out.starts_with("name,edge_table"), "{out}");
        assert!(out.contains("karate,ties,src,dst"), "{out}");
    }

    #[tokio::test]
    async fn errors_print_and_the_session_continues() {
        let mut shell = shell();
        shell.set_mode(OutputFormat::Csv);
        let (out, err) = feed(&mut shell, &["SELEC 1;"]).await;
        assert_eq!(out, "");
        assert!(err.starts_with("Error: "), "{err}");
        let (out, err) = feed(&mut shell, &["SELECT * FROM nowhere;"]).await;
        assert_eq!(out, "");
        assert!(err.contains("nowhere"), "{err}");
        // A failing statement skips the rest of its input, as in -c.
        let (out, err) = feed(&mut shell, &["SELECT 1 AS a; SELECT x; SELECT 3 AS c;"]).await;
        assert_eq!(out, "a\n1\n");
        assert!(err.starts_with("Error: "), "{err}");
        let (out, err) = feed(&mut shell, &["SELECT 2 AS b;"]).await;
        assert_eq!((out.as_str(), err.as_str()), ("b\n2\n", ""));
    }

    #[tokio::test]
    async fn cancel_input_drops_the_statement() {
        let mut shell = shell();
        shell.feed("SELECT 1").await;
        assert!(shell.is_pending());
        shell.cancel_input();
        assert!(!shell.is_pending());
        let (out, _) = feed(&mut shell, &[".mode csv", "SELECT 2 AS b;"]).await;
        assert_eq!(out, "b\n2\n");
    }

    #[tokio::test]
    async fn a_dot_inside_a_statement_is_sql() {
        let mut shell = shell();
        shell.set_mode(OutputFormat::Csv);
        let (out, err) = feed(&mut shell, &["SELECT", ".5 AS half;"]).await;
        assert_eq!(err, "");
        assert_eq!(out, "half\n0.5\n");
    }

    #[tokio::test]
    async fn quit_and_exit() {
        let mut shell = shell();
        assert_eq!(shell.feed(".quit").await, Flow::Exit);
        assert_eq!(shell.feed("  .exit  ").await, Flow::Exit);
        assert_eq!(shell.feed(".help").await, Flow::Continue);
    }

    #[tokio::test]
    async fn help_lists_every_command() {
        let mut shell = shell();
        let (out, _) = feed(&mut shell, &[".help"]).await;
        for (name, _, _) in COMMANDS {
            assert!(out.contains(name), "{name} missing from:\n{out}");
        }
    }

    #[tokio::test]
    async fn mode_sets_the_output_format() {
        let mut shell = shell();
        let (out, _) = feed(&mut shell, &[".mode"]).await;
        assert_eq!(out, "The output mode is table.\n");
        let cases = [
            ("csv", "x,y\n1,a\n"),
            ("tsv", "x\ty\n1\ta\n"),
            ("json", "[{\"x\":1,\"y\":\"a\"}]\n"),
            ("ndjson", "{\"x\":1,\"y\":\"a\"}\n"),
            (
                "TABLE",
                "+---+---+\n| x | y |\n+---+---+\n| 1 | a |\n+---+---+\n",
            ),
        ];
        for (mode, expected) in cases {
            let (out, err) = feed(
                &mut shell,
                &[&format!(".mode {mode}"), "SELECT 1 AS x, 'a' AS y;"],
            )
            .await;
            assert_eq!(err, "", "{mode}");
            assert_eq!(out, expected, "{mode}");
        }
        let (_, err) = feed(&mut shell, &[".mode arrow"]).await;
        assert!(err.contains("cannot show the arrow format"), "{err}");
        let (_, err) = feed(&mut shell, &[".mode box"]).await;
        assert!(err.contains("unknown mode 'box'"), "{err}");
        assert_eq!(shell.mode(), OutputFormat::Table);
    }

    #[tokio::test]
    async fn timer_reports_each_statement() {
        let mut shell = shell();
        let (out, _) = feed(&mut shell, &[".timer"]).await;
        assert_eq!(out, "The timer is off.\n");
        let (out, _) = feed(
            &mut shell,
            &[
                ".timer on",
                ".mode csv",
                "SELECT 1 AS x; CREATE VIEW v AS SELECT 1;",
            ],
        )
        .await;
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 4, "{out}");
        assert_eq!(&lines[..2], ["x", "1"]);
        assert!(lines[2].starts_with("Run Time: ") && lines[2].ends_with('s'));
        assert!(lines[3].starts_with("Run Time: "));
        let (out, _) = feed(&mut shell, &[".timer off", "SELECT 1 AS x;"]).await;
        assert_eq!(out, "x\n1\n");
        let (_, err) = feed(&mut shell, &[".timer maybe"]).await;
        assert_eq!(err, "Error: usage: .timer on|off\n");
    }

    #[tokio::test]
    async fn tables_lists_tables_and_views_but_not_graphs() {
        let mut shell = shell();
        shell.set_mode(OutputFormat::Csv);
        let (out, _) = feed(&mut shell, &[".tables"]).await;
        assert_eq!(out, "name,type\n");
        let (_, err) = feed(
            &mut shell,
            &[
                "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2)) AS t(src, dst);",
                "CREATE TABLE numbers AS SELECT 1 AS n;",
                KARATE_GRAPH,
            ],
        )
        .await;
        assert_eq!(err, "");
        let (out, err) = feed(&mut shell, &[".tables"]).await;
        assert_eq!(err, "");
        assert_eq!(out, "name,type\nnumbers,table\nties,view\n");
    }

    #[tokio::test]
    async fn graphs_and_describe() {
        let mut shell = shell();
        shell.set_mode(OutputFormat::Csv);
        let (out, _) = feed(&mut shell, &[".graphs"]).await;
        assert!(out.starts_with("name,edge_table,"), "{out}");
        assert_eq!(out.lines().count(), 1, "{out}");
        feed(
            &mut shell,
            &[
                "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2)) AS t(src, dst);",
                KARATE_GRAPH,
            ],
        )
        .await;
        let (out, err) = feed(&mut shell, &[".graphs"]).await;
        assert_eq!(err, "");
        assert!(out.contains("\nkarate,ties,src,dst"), "{out}");

        // A graph is described as a graph, in any case.
        let (out, err) = feed(&mut shell, &[".describe Karate"]).await;
        assert_eq!(err, "");
        assert!(
            out.starts_with("table_name,column_name,data_type,role,origin\n"),
            "{out}"
        );
        assert!(out.contains("edges,src,Int64,source,ties.src"), "{out}");
        // A view as a view.
        let (out, err) = feed(&mut shell, &[".describe ties"]).await;
        assert_eq!(err, "");
        assert!(out.starts_with("column_name,data_type"), "{out}");
        let (_, err) = feed(&mut shell, &[".describe"]).await;
        assert!(err.contains("usage: .describe NAME"), "{err}");
        let (_, err) = feed(&mut shell, &[".describe nowhere"]).await;
        assert!(err.starts_with("Error: "), "{err}");
    }

    #[tokio::test]
    async fn read_runs_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("setup.sql");
        std::fs::write(
            &script,
            "-- a setup script\n\
             .mode csv\n\
             CREATE VIEW v AS\n  SELECT 1 AS x;\n\
             SELECT x FROM v;\n\
             SELECT nope;\n\
             SELECT 2 AS y",
        )
        .unwrap();
        let mut shell = shell();
        let (out, err) = feed(&mut shell, &[&format!(".read '{}'", script.display())]).await;
        // The error is printed and the file goes on; a last statement needs
        // no `;`.
        assert_eq!(out, "x\n1\ny\n2\n");
        assert!(
            err.starts_with("Error: ") && err.lines().count() == 1,
            "{err}"
        );
        assert_eq!(shell.mode(), OutputFormat::Csv);

        let (_, err) = feed(&mut shell, &[".read"]).await;
        assert_eq!(err, "Error: usage: .read FILE\n");
        let missing = dir.path().join("missing.sql");
        let (_, err) = feed(&mut shell, &[&format!(".read {}", missing.display())]).await;
        assert!(err.starts_with("Error: cannot read '"), "{err}");
    }

    #[tokio::test]
    async fn read_nests_and_stops_on_a_loop() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner.sql");
        let outer = dir.path().join("outer.sql");
        std::fs::write(&inner, "SELECT 2 AS b;\n").unwrap();
        std::fs::write(
            &outer,
            format!(
                ".mode csv\nSELECT 1 AS a;\n.read {}\nSELECT 3 AS c;\n",
                inner.display()
            ),
        )
        .unwrap();
        let mut shell = shell();
        let (out, err) = feed(&mut shell, &[&format!(".read {}", outer.display())]).await;
        assert_eq!(err, "");
        assert_eq!(out, "a\n1\nb\n2\nc\n3\n");

        let looping = dir.path().join("loop.sql");
        std::fs::write(&looping, format!(".read {}\n", looping.display())).unwrap();
        let (_, err) = feed(&mut shell, &[&format!(".read {}", looping.display())]).await;
        assert!(err.contains("nested more than 16 deep"), "{err}");
        assert_eq!(shell.depth, 0);
    }

    #[tokio::test]
    async fn quit_in_a_file_ends_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("quit.sql");
        std::fs::write(
            &script,
            ".mode csv\nSELECT 1 AS a;\n.quit\nSELECT 2 AS b;\n",
        )
        .unwrap();
        let mut shell = shell();
        assert_eq!(shell.run_file(&script).await, Flow::Exit);
        assert_eq!(take(&mut shell).0, "a\n1\n");
    }

    #[tokio::test]
    async fn open_attaches_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("club.ursa");
        let mut shell = shell();
        let (_, err) = feed(&mut shell, &[".open notes.txt"]).await;
        assert!(err.contains("'notes.txt' is not a database file"), "{err}");
        let (_, err) = feed(&mut shell, &[".open"]).await;
        assert_eq!(err, "Error: usage: .open FILE.ursa\n");

        let (out, err) = feed(&mut shell, &[&format!(".open {}", path.display())]).await;
        assert_eq!(err, "");
        assert_eq!(out, format!("Opened {}.\n", path.display()));
        let (_, err) = feed(&mut shell, &["CREATE VIEW v AS SELECT 1 AS x;"]).await;
        assert_eq!(err, "");
        let catalog = std::fs::read_to_string(&path).unwrap();
        assert!(catalog.contains("CREATE VIEW v"), "{catalog}");

        // A second .open is refused, as a second ATTACH is.
        let other = dir.path().join("other.ursa");
        let (_, err) = feed(&mut shell, &[&format!(".open {}", other.display())]).await;
        assert!(err.contains("is already open"), "{err}");
    }

    #[tokio::test]
    async fn open_is_refused_once_something_is_defined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("club.ursa");
        let mut shell = shell();
        feed(&mut shell, &["CREATE VIEW v AS SELECT 1 AS x;"]).await;
        let (out, err) = feed(&mut shell, &[&format!(".open {}", path.display())]).await;
        assert_eq!(out, "");
        assert!(
            err.contains("ATTACH must come before anything is defined"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn functions_waits_for_the_catalog() {
        let mut shell = shell();
        let (out, err) = feed(&mut shell, &[".functions"]).await;
        assert_eq!(err, "");
        assert!(out.contains("pagerank"), "{out}");
    }

    #[tokio::test]
    async fn unknown_commands_suggest_one() {
        let mut shell = shell();
        let (_, err) = feed(&mut shell, &[".tabels"]).await;
        assert_eq!(
            err,
            "Error: unknown command '.tabels'; did you mean '.tables'? Enter .help for the \
             list\n"
        );
        let (_, err) = feed(&mut shell, &[".gaphs"]).await;
        assert!(err.contains("did you mean '.graphs'?"), "{err}");
        let (_, err) = feed(&mut shell, &[".frobnicate"]).await;
        assert_eq!(
            err,
            "Error: unknown command '.frobnicate'; enter .help for the list\n"
        );
    }

    #[test]
    fn did_you_mean_picks_the_closest() {
        assert_eq!(did_you_mean(".quti"), Some(".quit"));
        assert_eq!(did_you_mean(".MODE"), Some(".mode"));
        assert_eq!(did_you_mean(".describ"), Some(".describe"));
        assert_eq!(did_you_mean(".function"), Some(".functions"));
        assert_eq!(did_you_mean(".zzzzzzz"), None);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn path_arguments_may_be_quoted() {
        assert_eq!(path_arg("a b.sql"), Some(PathBuf::from("a b.sql")));
        assert_eq!(path_arg("'a b.sql'"), Some(PathBuf::from("a b.sql")));
        assert_eq!(path_arg("\"x.sql\""), Some(PathBuf::from("x.sql")));
        assert_eq!(path_arg("  "), None);
    }
}
