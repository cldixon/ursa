//! The shell's read–eval loop, without the terminal.
//!
//! [`Shell::feed`] takes one line of input: a dot-command, or a line of SQL
//! that is added to the statement being typed and run once it ends with `;`
//! (see [`crate::input`]). Results go to the shell's output and errors to its
//! error stream, and an error never ends the session. The terminal side (line
//! editing, history, Ctrl-C) belongs to the host: `ursa-cli`'s `repl` in a
//! terminal, the browser shell's line editor in a page. `-init` files and
//! `~/.ursarc` run through [`Shell::run_file`], like `.read`.
//!
//! `-c`, `-f` and SQL on stdin run through [`Shell::run_text`] too, so a
//! script may hold dot-commands, as in DuckDB. A script session is made with
//! [`Shell::stop_on_error`]: its first error ends it (see that method).
//!
//! The dot-commands follow DuckDB's shell where it has the same command.

use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use arrow::array::{Array, AsArray};
use arrow::compute::cast;
use arrow::datatypes::{DataType, SchemaRef};
use arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::prelude::SessionContext;
use datafusion::sql::sqlparser::ast::{Ident, ObjectName};
use datafusion::sql::sqlparser::dialect::{dialect_from_str, Dialect, GenericDialect};
use datafusion::sql::sqlparser::parser::Parser;
use datafusion::sql::sqlparser::tokenizer::Token;
use futures::TryStreamExt;

use crate::highlight::{self, ERROR, RESET};
use crate::input::{self, Status};
use crate::print::{
    has_columns, is_broken_pipe, write_stream, OutputFormat, TableStyle, DEFAULT_MAX_ROWS,
};
use crate::table::Style;
use crate::timer::Timer;

/// What the caller does after a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// `.quit` or `.exit`; or, in a script, an error, or a reader that
    /// closed the output.
    Exit,
    /// A dot-command the host added ([`Shell::add_host_command`]), for the
    /// host to run: the whole line, trimmed.
    Host(String),
}

/// `.functions`: each graph function's name, family and the first sentence
/// of its summary (`.functions NAME` shows the rest).
const FUNCTIONS: &str = "SELECT name, family, \
     CASE WHEN strpos(summary, '. ') > 0 THEN split_part(summary, '. ', 1) || '.' \
          ELSE summary END AS summary \
     FROM ursa_functions();";

/// The docs, which `.help` and `ursa --help` point to.
pub const DOCS_URL: &str = "https://cldixon.github.io/ursa/sql/";

/// One step of a script: a dot-command, or SQL that ends with a `;` (or ends
/// the script).
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Command(String),
    /// The SQL, and how many lines of the script come before it (to report
    /// a syntax error at the script's line, not the statement's).
    Sql(String, usize),
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
        "?NAME?",
        "List the graph functions, or show one's parameters",
    ),
    (".graphs", "", "List the graphs (SHOW GRAPHS)"),
    (
        ".help",
        "?PATTERN?",
        "Show help for the commands matching PATTERN",
    ),
    (
        ".maxrows",
        "N",
        "Show at most N rows of a table (default 40; 0 shows all)",
    ),
    (
        ".maxwidth",
        "N",
        "Draw tables at most N characters wide (0: the screen's width)",
    ),
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
    (".print", "STRING...", "Print literal STRING"),
    (".quit", "", "Exit the shell (also Ctrl-D)"),
    (".read", "FILE", "Run the SQL and dot-commands in FILE"),
    (".tables", "", "List the tables and views"),
    (
        ".timer",
        "on|off",
        "Show how long each statement takes (real, user and sys)",
    ),
    (".version", "", "Show the version"),
];

/// A shell session: an engine session, the shell's settings, and the
/// statement being typed.
pub struct Shell<O: Write, E: Write> {
    ctx: SessionContext,
    mode: OutputFormat,
    /// How many rows a table shows; 0 shows every row.
    max_rows: usize,
    /// How wide a table may be; 0: the screen's width, or no limit when
    /// the output is not a screen.
    max_width: usize,
    /// The output is a screen (a terminal, or the browser shell's): its
    /// width now, which tables fit.
    screen: Option<Box<dyn Fn() -> usize>>,
    /// Colour tables and the prompt with ANSI escapes.
    color: bool,
    /// Colour errors red (the error stream is a screen).
    error_color: bool,
    /// Dot-commands the host runs: name, arguments, help.
    host_commands: Vec<(String, String, String)>,
    /// `.help shortcuts`: the host's keys.
    shortcuts: String,
    /// What `.version` says.
    version: String,
    /// Keep the last result's rows ([`Self::last_result`]).
    keep_last: bool,
    last: Option<(SchemaRef, Vec<RecordBatch>)>,
    timer: bool,
    /// SQL typed so far that is not yet a complete statement.
    buffer: String,
    /// Where the next line starts, after `buffer`, for colouring it.
    buffer_state: highlight::State,
    /// How many `.read`s (or init files) are running. A [`Nested`] counts
    /// one, so a `.read` cancelled with Ctrl-C (its future dropped) counts
    /// itself out.
    depth: Rc<Cell<usize>>,
    /// A script: the first error ends the session.
    stop_on_error: bool,
    /// A script's error has ended the session.
    failed: bool,
    /// The reader of the output has gone (`ursa … | head`): stop quietly.
    closed: bool,
    /// How many results have been written, for `--format arrow`'s one.
    results: usize,
    out: O,
    err: E,
}

impl<O: Write, E: Write> Shell<O, E> {
    pub fn new(ctx: SessionContext, mode: OutputFormat, out: O, err: E) -> Self {
        Self {
            ctx,
            mode,
            max_rows: DEFAULT_MAX_ROWS,
            max_width: 0,
            screen: None,
            color: false,
            error_color: false,
            host_commands: Vec::new(),
            shortcuts: TERMINAL_SHORTCUTS.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            keep_last: false,
            last: None,
            timer: false,
            buffer: String::new(),
            buffer_state: highlight::State::Start,
            depth: Rc::new(Cell::new(0)),
            stop_on_error: false,
            failed: false,
            closed: false,
            results: 0,
            out,
            err,
        }
    }

    /// Run as a script (`-c`, `-f`, stdin) from now on: the first error is
    /// printed and ends the session, with [`Self::failed`] set, and SQL
    /// given to [`Self::run_text`] is parsed whole before any of it runs.
    pub fn stop_on_error(&mut self) {
        self.stop_on_error = true;
    }

    /// Whether a script stopped on an error.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// End a script: with `--format arrow`, a script that wrote no result is
    /// an error, since an Arrow stream needs one (its schema) and an empty
    /// output is not a stream another program can read.
    pub fn end_script(&mut self) {
        if self.mode == OutputFormat::Arrow && self.results == 0 && !self.failed && !self.closed {
            self.error(
                "--format arrow writes the result of one query, but no statement returned \
                 one; end the script with a query that returns rows",
            );
        }
    }

    #[cfg(test)]
    pub fn mode(&self) -> OutputFormat {
        self.mode
    }

    pub fn set_mode(&mut self, mode: OutputFormat) {
        self.mode = mode;
    }

    pub fn set_max_rows(&mut self, max_rows: usize) {
        self.max_rows = max_rows;
    }

    /// Draw tables at most `max_width` characters wide; 0: the screen's
    /// width, or no limit when the output is not a screen (`.maxwidth N`).
    pub fn set_max_width(&mut self, max_width: usize) {
        self.max_width = max_width;
    }

    /// Say that the output is a screen whose width `width` reads (a
    /// terminal's may change): tables fit it and, with `color`, dim their
    /// types and nulls.
    pub fn set_screen(&mut self, width: Box<dyn Fn() -> usize>, color: bool) {
        self.screen = Some(width);
        self.color = color;
    }

    /// Add a dot-command that the host runs (the browser's `.share`): `.help`
    /// lists it, and [`Self::feed`] returns [`Flow::Host`] for it.
    pub fn add_host_command(&mut self, name: &str, args: &str, help: &str) {
        self.host_commands
            .push((name.to_string(), args.to_string(), help.to_string()));
    }

    /// Keep every row of the last result that had columns, for
    /// [`Self::last_result`] (the browser's `.export`). It holds the whole
    /// result in memory, so the CLI does not.
    pub fn keep_last_result(&mut self) {
        self.keep_last = true;
    }

    /// The last result that had columns, every row, if
    /// [`Self::keep_last_result`] was called.
    pub fn last_result(&self) -> Option<&(SchemaRef, Vec<RecordBatch>)> {
        self.last.as_ref()
    }

    /// Colour errors red: the error stream is a screen that shows colour.
    pub fn set_error_color(&mut self, color: bool) {
        self.error_color = color;
    }

    /// What `.version` says (`ursa --version`'s, which names a nightly build).
    pub fn set_version(&mut self, version: &str) {
        self.version = version.to_string();
    }

    /// What `.help shortcuts` says: the host's keys.
    pub fn set_shortcuts(&mut self, text: &str) {
        self.shortcuts = text.trim_end().to_string();
    }

    /// The prompt for a new statement: the database's name (`club` for
    /// `club.ursa`, `memory` with none) and `U`, as DuckDB's is `memory D`.
    pub fn prompt(&self) -> String {
        let name = ursa_engine::database::database(&self.ctx.state())
            .map(|db| {
                let name = Path::new(db.name());
                name.file_stem()
                    .unwrap_or(name.as_os_str())
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_else(|| "memory".to_string());
        format!("{name} U ")
    }

    /// The prompt while a statement continues: spaces as wide as
    /// [`Self::prompt`], so the statement's lines line up and copy as SQL.
    pub fn continuation(&self) -> String {
        " ".repeat(unicode_width::UnicodeWidthStr::width(
            self.prompt().as_str(),
        ))
    }

    /// How tables are drawn now.
    fn table_style(&self) -> TableStyle {
        let max_width = match (self.max_width, &self.screen) {
            (0, Some(width)) => width(),
            (n, _) => n,
        };
        TableStyle {
            max_rows: self.max_rows,
            style: Style {
                max_width,
                color: self.color,
            },
        }
    }

    /// Whether a statement is partly typed (the prompt shows a continuation).
    pub fn is_pending(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// Drop the statement being typed (Ctrl-C).
    pub fn cancel_input(&mut self) {
        self.buffer.clear();
        self.buffer_state = highlight::State::Start;
    }

    /// Where a line typed now starts, for colouring it: in a new statement,
    /// or in the one being typed, perhaps in a string or comment it left
    /// open.
    pub fn highlight_state(&self) -> &highlight::State {
        &self.buffer_state
    }

    /// `line` in ANSI colour, as the next line of what is being typed (see
    /// [`highlight::ansi_from`]).
    pub fn highlight_line(&self, line: &str) -> String {
        highlight::ansi_from(line, &self.buffer_state)
    }

    /// Take one line of input. A line starting with `.` is a dot-command
    /// unless it continues a statement.
    ///
    /// Dropping the future (Ctrl-C) cancels the line: the SQL is taken from
    /// the buffer before it runs, so a cancelled query leaves nothing
    /// behind, and a cancelled `.read` no longer counts as running.
    pub async fn feed(&mut self, line: &str) -> Flow {
        let dialect = self.dialect();
        let before = self.buffer.len();
        let step = next_step(&mut self.buffer, line, dialect.as_ref());
        self.buffer_state = if self.buffer.is_empty() {
            highlight::State::Start
        } else {
            // The buffer grew by this line: read on from where it was.
            match highlight::state_after(&self.buffer[before..], &self.buffer_state) {
                highlight::State::Start => highlight::State::Statement,
                state => state,
            }
        };
        match step {
            Some(step) => self.run_step(step).await,
            None => Flow::Continue,
        }
    }

    async fn run_step(&mut self, step: Step) -> Flow {
        let flow = match step {
            Step::Command(line) => self.command(&line).await,
            Step::Sql(sql, offset) => {
                self.run_sql(&sql, offset).await;
                Flow::Continue
            }
        };
        if self.failed || self.closed {
            Flow::Exit
        } else {
            flow
        }
    }

    /// Run text of SQL and dot-commands, a line at a time, as typed. A last
    /// statement without a `;` runs too. Errors are printed and the text goes
    /// on, unless this is a script ([`Self::stop_on_error`]): then all of its
    /// SQL is parsed first, so a syntax error anywhere runs none of it, and
    /// the first error stops it.
    pub async fn run_text(&mut self, text: &str) -> Flow {
        let steps = steps(text, self.dialect().as_ref());
        if self.stop_on_error {
            for step in &steps {
                if let Step::Sql(sql, offset) = step {
                    if let Err(e) = ursa_sql::parse_sql_with_text_for(&self.ctx, sql) {
                        self.error(&shift_lines(&ursa_engine::user_message(&e), *offset));
                        return Flow::Exit;
                    }
                }
            }
        }
        for step in steps {
            if Box::pin(self.run_step(step)).await == Flow::Exit {
                return Flow::Exit;
            }
        }
        Flow::Continue
    }

    /// Run a file of SQL and dot-commands, as `.read` does (see
    /// [`Self::run_text`]).
    pub async fn run_file(&mut self, path: &Path) -> Flow {
        if self.depth.get() >= MAX_READ_DEPTH {
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
        // The file's statements are its own: whatever was being typed waits
        // (run_text does not touch the buffer).
        let _nested = Nested::enter(&self.depth);
        Box::pin(self.run_text(&text)).await
    }

    /// The session's SQL dialect, for telling where statements end.
    fn dialect(&self) -> Box<dyn Dialect> {
        let name = self.ctx.state().config().options().sql_parser.dialect;
        dialect_from_str(name).unwrap_or_else(|| Box::new(GenericDialect {}))
    }

    fn error(&mut self, message: &str) {
        let _ = if self.error_color {
            writeln!(self.err, "{ERROR}Error: {message}{RESET}")
        } else {
            writeln!(self.err, "Error: {message}")
        };
        let _ = self.err.flush();
        if self.stop_on_error {
            self.failed = true;
        }
    }

    /// Print a line that is not a result: a dot-command's answer or the
    /// timer's. With `--format arrow` it goes to the error stream, so the
    /// output stays one Arrow stream.
    fn say(&mut self, message: &str) {
        self.write_text(&format!("{message}\n"));
    }

    fn write_text(&mut self, text: &str) {
        let written = if self.mode == OutputFormat::Arrow {
            write!(self.err, "{text}").and_then(|()| self.err.flush())
        } else {
            write!(self.out, "{text}").and_then(|()| self.out.flush())
        };
        if written.is_err_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) {
            self.closed = true;
        }
    }

    /// Parse all of `sql`, then run each statement and print its result. The
    /// first error is printed and the rest of `sql` is skipped. `offset`
    /// lines came before `sql` in its script.
    async fn run_sql(&mut self, sql: &str, offset: usize) {
        match Box::pin(self.try_run_sql(sql)).await {
            Ok(()) => {}
            // A reader that stops early (`ursa … | head`) is not an error.
            Err(e) if is_broken_pipe(&e) => self.closed = true,
            Err(e) => self.error(&shift_lines(&ursa_engine::user_message(&e), offset)),
        }
    }

    /// Run `sql`'s statements. A statement that returns columns prints a
    /// result, with its header even when there are no rows; one that returns
    /// none (DDL, `SET`) prints nothing, as in DuckDB.
    async fn try_run_sql(&mut self, sql: &str) -> Result<()> {
        let statements = ursa_sql::parse_sql_with_text_for(&self.ctx, sql)?;
        for (statement, text) in statements {
            let start = Timer::start();
            let frame = ursa_sql::execute_with_text(&self.ctx, statement, &text).await?;
            let stream = frame.execute_stream().await?;
            if has_columns(&stream.schema()) {
                if self.mode == OutputFormat::Arrow && self.results > 0 {
                    return Err(DataFusionError::Plan(
                        "--format arrow writes a single result; run one query that returns rows"
                            .into(),
                    ));
                }
                let config = self.ctx.state().config().options().format.clone();
                let options = (&config).try_into()?;
                let table = self.table_style();
                let stream = if self.keep_last {
                    // Every row, for the host to write out later (`.export`).
                    let schema = stream.schema();
                    let batches: Vec<RecordBatch> = stream.try_collect().await?;
                    self.last = Some((Arc::clone(&schema), batches.clone()));
                    Box::pin(RecordBatchStreamAdapter::new(
                        schema,
                        futures::stream::iter(batches.into_iter().map(Ok)),
                    ))
                } else {
                    stream
                };
                write_stream(self.mode, stream, &mut self.out, &options, table).await?;
                self.results += 1;
            }
            if self.timer {
                self.say(&start.report());
            }
        }
        Ok(())
    }

    /// Run a dot-command (`line` is trimmed and starts with `.`). A `;` at
    /// its end, as SQL has, is not part of it (`.tables;`), except for
    /// `.print`, which prints its text as it is.
    async fn command(&mut self, line: &str) -> Flow {
        let split = |line: &'_ str| match line.split_once(char::is_whitespace) {
            Some((name, arg)) => (name.to_string(), arg.trim().to_string()),
            None => (line.to_string(), String::new()),
        };
        let typed = line;
        let line = line.strip_suffix(';').map_or(line, str::trim_end);
        let (name, mut arg) = split(line);
        if name == ".print" {
            arg = split(typed).1;
        }
        let (name, arg) = (name.as_str(), arg.as_str());
        match name {
            ".quit" | ".exit" => return Flow::Exit,
            ".help" => self.help(arg),
            ".version" => self.say(&format!("Ursa v{}", self.version)),
            ".print" => self.say(arg),
            ".mode" => self.mode_command(arg),
            ".maxrows" => self.max_rows_command(arg),
            ".maxwidth" => self.max_width_command(arg),
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
            ".graphs" => self.run_sql("SHOW GRAPHS;", 0).await,
            ".describe" => self.describe(arg).await,
            ".read" => match path_arg(arg) {
                Some(path) => return self.run_file(&path).await,
                None => self.error("usage: .read FILE"),
            },
            ".open" => self.open(arg).await,
            ".functions" => self.functions(arg).await,
            _ if self.host_commands.iter().any(|(c, _, _)| c == name) => {
                return Flow::Host(line.to_string());
            }
            _ => {
                let names = COMMANDS
                    .iter()
                    .map(|(c, _, _)| *c)
                    .chain(self.host_commands.iter().map(|(c, _, _)| c.as_str()));
                let message = match suggest_command(name, names) {
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

    /// `.help`: every command, as DuckDB lists them. `.help graph` and
    /// `.help shortcuts` are pages of their own; `.help PATTERN` lists the
    /// commands whose name or help has PATTERN in it.
    fn help(&mut self, arg: &str) {
        match arg.to_ascii_lowercase().as_str() {
            "graph" => return self.write_text(&format!("{}\n", graph_help())),
            "shortcuts" => return self.write_text(&format!("{}\n", self.shortcuts)),
            _ => {}
        }
        let needle = arg.trim_start_matches('.').to_ascii_lowercase();
        let mut commands: Vec<(&str, &str, &str)> = COMMANDS
            .iter()
            .copied()
            .chain(
                self.host_commands
                    .iter()
                    .map(|(c, a, h)| (c.as_str(), a.as_str(), h.as_str())),
            )
            .filter(|(c, _, h)| {
                needle.is_empty() || c.contains(&needle) || h.to_ascii_lowercase().contains(&needle)
            })
            .collect();
        if commands.is_empty() {
            return self.error(&format!(
                "no command matches '{arg}'; enter .help for the list"
            ));
        }
        commands.sort_by_key(|(c, a, _)| (*c, *a));
        let usage = |c: &str, a: &str| {
            if a.is_empty() {
                c.to_string()
            } else {
                format!("{c} {a}")
            }
        };
        let width = commands
            .iter()
            .map(|(c, a, _)| usage(c, a).len())
            .max()
            .unwrap_or(0)
            + 4;
        let mut text = String::new();
        for (c, a, h) in commands {
            text.push_str(&format!("{:<width$}{h}\n", usage(c, a)));
        }
        if needle.is_empty() {
            text.push_str(
                "\nRun .help graph for the graph SQL: property graphs, MATCH and algorithms\n\
                 Run .help shortcuts for keyboard shortcuts\n",
            );
        }
        self.write_text(&text);
    }

    fn max_rows_command(&mut self, arg: &str) {
        if arg.is_empty() {
            let shown = match self.max_rows {
                0 => "every row".to_string(),
                n => format!("at most {n} rows"),
            };
            self.say(&format!("Tables show {shown}."));
            return;
        }
        match arg.parse::<usize>() {
            Ok(n) => self.max_rows = n,
            Err(_) => self.error(&format!(
                "usage: .maxrows N, a number of rows (0 shows every row); got '{arg}'"
            )),
        }
    }

    fn max_width_command(&mut self, arg: &str) {
        if arg.is_empty() {
            let shown = match self.max_width {
                0 if self.screen.is_some() => "as wide as the screen".to_string(),
                0 => "as wide as they need".to_string(),
                n => format!("at most {n} characters wide"),
            };
            self.say(&format!("Tables are drawn {shown}."));
            return;
        }
        match arg.parse::<usize>() {
            Ok(n) => self.max_width = n,
            Err(_) => self.error(&format!(
                "usage: .maxwidth N, a number of characters (0: the screen's width); got '{arg}'"
            )),
        }
    }

    fn mode_command(&mut self, arg: &str) {
        if arg.is_empty() {
            let name = self.mode.name();
            self.say(&format!("The output mode is {name}."));
            return;
        }
        match OutputFormat::parse(arg) {
            // Already writing an Arrow stream (`--format arrow`).
            Some(OutputFormat::Arrow) if self.mode == OutputFormat::Arrow => {}
            Some(OutputFormat::Arrow) => self.error(
                ".mode cannot switch to the arrow format: an Arrow stream is one result, \
                 written to a file or a program. Give --format arrow on the command line: \
                 ursa --format arrow -c \"<sql>\" > out.arrows",
            ),
            Some(mode) => self.mode = mode,
            None => self.error(&format!(
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
        self.run_sql(&sql, 0).await;
    }

    /// `.describe NAME`: `DESCRIBE GRAPH` for a graph, `DESCRIBE` otherwise.
    /// NAME is a name, perhaps qualified (`schema.table`) or quoted; written
    /// without quotes, it names a graph, table or view in any case, as
    /// `.tables` and `.graphs` list them (`.describe Mixed` for a table
    /// created as `"Mixed"`).
    async fn describe(&mut self, name: &str) {
        const USAGE: &str = "usage: .describe NAME (a graph, table or view)";
        if name.is_empty() {
            return self.error(USAGE);
        }
        let Some(object) = self.object_name(name) else {
            return self.error(&format!("{USAGE}; '{name}' is not a name"));
        };
        let parts: Vec<&Ident> = object.0.iter().filter_map(|p| p.as_ident()).collect();
        let graphs = match Box::pin(self.graph_names()).await {
            Ok(graphs) => graphs,
            Err(e) => return self.error(&ursa_engine::user_message(&e)),
        };
        let graph = match parts[..] {
            [part] => graphs.iter().find(|g| names(part, g)),
            _ => None,
        };
        let sql = match graph {
            Some(graph) => format!("DESCRIBE GRAPH {};", quote_ident(graph)),
            None => match Box::pin(self.find_table(&parts)).await {
                Ok(Some(table)) => format!(
                    "DESCRIBE {};",
                    table
                        .iter()
                        .map(|p| quote_ident(p))
                        .collect::<Vec<_>>()
                        .join(".")
                ),
                // Not a table this session lists: DESCRIBE says why.
                Ok(None) => format!("DESCRIBE {object};"),
                Err(e) => return self.error(&ursa_engine::user_message(&e)),
            },
        };
        self.run_sql(&sql, 0).await;
    }

    /// `text` as one object name (`t`, `"My Table"`, `s.t`), or `None` if
    /// it is not exactly one.
    fn object_name(&self, text: &str) -> Option<ObjectName> {
        let dialect = self.dialect();
        let mut parser = Parser::new(dialect.as_ref()).try_with_sql(text).ok()?;
        let name = parser.parse_object_name(false).ok()?;
        let whole = parser.peek_token().token == Token::EOF;
        let plain = name.0.iter().all(|p| p.as_ident().is_some());
        (whole && plain && !name.0.is_empty() && name.0.len() <= 3).then_some(name)
    }

    /// The catalog, schema and name of the table or view `parts` names, as
    /// `information_schema` has them: a quoted part matches exactly, and an
    /// unquoted one in any case, preferring the lower case it stands for.
    async fn find_table(&self, parts: &[&Ident]) -> Result<Option<[String; 3]>> {
        let options = self.ctx.state().config().options().catalog.clone();
        let tables = self
            .rows("SELECT table_catalog, table_schema, table_name FROM information_schema.tables")
            .await?;
        // The parts given, after the default catalog and schema.
        let defaults = [&options.default_catalog, &options.default_schema];
        let matching: Vec<[String; 3]> = tables
            .into_iter()
            .filter_map(|row| <[String; 3]>::try_from(row).ok())
            .filter(|row| {
                let given = row.len() - parts.len();
                row[..given].iter().zip(defaults).all(|(a, b)| a == b)
                    && row[given..].iter().zip(parts).all(|(a, p)| names(p, a))
            })
            .collect();
        let exact = |row: &&[String; 3]| {
            row[3 - parts.len()..]
                .iter()
                .zip(parts)
                .all(|(a, p)| p.quote_style.is_some() || *a == p.value.to_lowercase())
        };
        Ok(match matching.iter().find(exact) {
            Some(row) => Some(row.clone()),
            None if matching.len() == 1 => matching.into_iter().next(),
            None => None,
        })
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

    /// `.functions`: the name, family and summary of each graph function
    /// (from `ursa_functions()`). `.functions NAME`: one function's
    /// parameters and outputs.
    async fn functions(&mut self, name: &str) {
        if name.is_empty() {
            return self.run_sql(FUNCTIONS, 0).await;
        }
        match Box::pin(self.describe_function(name)).await {
            Ok(text) => self.write_text(&text),
            Err(e) => self.error(&ursa_engine::user_message(&e)),
        }
    }

    /// `.functions NAME`, as text.
    async fn describe_function(&self, name: &str) -> Result<String> {
        let literal = quote_literal(name);
        let found = self
            .rows(&format!(
                "SELECT name, family, summary, networkx FROM ursa_functions() \
                 WHERE lower(name) = lower({literal})"
            ))
            .await?;
        let Some([name, family, summary, networkx]) = found.first().map(|row| &row[..]) else {
            let names: Vec<String> = self
                .rows("SELECT name FROM ursa_functions()")
                .await?
                .into_iter()
                .map(|mut row| row.remove(0))
                .collect();
            let hint = match closest(name, names.iter().map(String::as_str)) {
                Some(close) => format!("did you mean '{close}'? "),
                None => String::new(),
            };
            return Err(DataFusionError::Plan(format!(
                "no graph function named '{name}'; {hint}.functions lists them"
            )));
        };
        let literal = quote_literal(name);
        let mut text = format!("{name} ({family})\n{summary}\n");
        if !networkx.is_empty() {
            text.push_str(&format!("NetworkX: {networkx}\n"));
        }
        let parameters = self
            .rows(&format!(
                "SELECT p['name'], p['type'], p['default'], array_to_string(p['choices'], ', '), \
                        p['doc'] \
                 FROM (SELECT unnest(parameters) AS p FROM ursa_functions() \
                       WHERE name = {literal})"
            ))
            .await?;
        text.push_str("\nParameters, after the graph, as name => value:\n");
        if parameters.is_empty() {
            text.push_str("  none\n");
        }
        let width = |rows: &[Vec<String>], i: usize| rows.iter().map(|r| r[i].len()).max();
        let name_width = width(&parameters, 0).unwrap_or(0);
        let type_width = width(&parameters, 1).unwrap_or(0);
        let default = |default: &str| match default {
            "required" => default.to_string(),
            _ => format!("default {default}"),
        };
        let default_width = parameters
            .iter()
            .map(|row| default(&row[2]).len())
            .max()
            .unwrap_or(0);
        for row in &parameters {
            let [name, kind, value, choices, doc] = &row[..] else {
                continue;
            };
            let value = default(value);
            let mut line = format!(
                "  {name:<name_width$}  {kind:<type_width$}  {value:<default_width$}  {doc}"
            );
            if !choices.is_empty() {
                line.push_str(&format!(" One of: {choices}."));
            }
            text.push_str(line.trim_end());
            text.push('\n');
        }
        let outputs = self
            .rows(&format!(
                "SELECT o['name'], o['type'] \
                 FROM (SELECT unnest(outputs) AS o FROM ursa_functions() WHERE name = {literal})"
            ))
            .await?;
        text.push_str("\nOutput columns:\n");
        let name_width = width(&outputs, 0).unwrap_or(0);
        for row in &outputs {
            if let [name, kind] = &row[..] {
                text.push_str(&format!("  {name:<name_width$}  {kind}\n"));
            }
        }
        Ok(text)
    }

    /// The graphs' names, from `SHOW GRAPHS` (its first column).
    async fn graph_names(&self) -> Result<Vec<String>> {
        Ok(self
            .rows("SHOW GRAPHS")
            .await?
            .into_iter()
            .map(|mut row| row.remove(0))
            .collect())
    }

    /// The rows of a query, each value as text (a null as "").
    async fn rows(&self, sql: &str) -> Result<Vec<Vec<String>>> {
        let mut statements = ursa_sql::parse_sql_with_text_for(&self.ctx, sql)?;
        let (statement, text) = statements.remove(0);
        let batches = ursa_sql::execute_with_text(&self.ctx, statement, &text)
            .await?
            .collect()
            .await?;
        let mut rows = Vec::new();
        for batch in batches {
            let columns = batch
                .columns()
                .iter()
                .map(|column| cast(column, &DataType::Utf8))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for i in 0..batch.num_rows() {
                rows.push(
                    columns
                        .iter()
                        .map(|column| {
                            let column = column.as_string::<i32>();
                            if column.is_valid(i) {
                                column.value(i).to_string()
                            } else {
                                String::new()
                            }
                        })
                        .collect(),
                );
            }
        }
        Ok(rows)
    }
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

/// `s` as a quoted SQL name.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Whether `part` names `name`: exactly if it was quoted, in any case if not.
fn names(part: &Ident, name: &str) -> bool {
    if part.quote_style.is_some() {
        part.value == name
    } else {
        part.value.eq_ignore_ascii_case(name)
    }
}

/// Counts one `.read` running while it lives ([`Shell::run_file`]).
struct Nested(Rc<Cell<usize>>);

impl Nested {
    fn enter(depth: &Rc<Cell<usize>>) -> Self {
        depth.set(depth.get() + 1);
        Self(Rc::clone(depth))
    }
}

impl Drop for Nested {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

/// The next step, once `line` is added to the SQL typed so far (`buffer`):
/// a line that starts with `.` is a dot-command unless it continues a
/// statement, and SQL is a step once it ends with `;`.
fn next_step(buffer: &mut String, line: &str, dialect: &dyn Dialect) -> Option<Step> {
    if buffer.is_empty() && line.trim_start().starts_with('.') {
        return Some(Step::Command(line.trim().to_string()));
    }
    let continues = !buffer.is_empty();
    buffer.push_str(line);
    buffer.push('\n');
    // An unfinished statement stays unfinished until a line has a `;`, or
    // closes a comment the `;` came before. Skipping the tokenizer for the
    // other lines keeps a long statement (a big INSERT, a row a line) from
    // being read again for every line.
    if continues && !line.contains(';') && !line.contains("*/") {
        return None;
    }
    match input::status(buffer, dialect) {
        Status::Empty => {
            buffer.clear();
            None
        }
        Status::Incomplete => None,
        Status::Complete => Some(Step::Sql(std::mem::take(buffer), 0)),
    }
}

/// The steps of a script, as if typed a line at a time. Unfinished SQL at the
/// end is a step too.
fn steps(text: &str, dialect: &dyn Dialect) -> Vec<Step> {
    let mut buffer = String::new();
    let mut steps = Vec::new();
    let mut lines = 0;
    for line in text.lines() {
        lines += 1;
        if let Some(step) = next_step(&mut buffer, line, dialect) {
            steps.push(with_offset(step, lines));
        }
    }
    if input::status(&buffer, dialect) != Status::Empty {
        steps.push(with_offset(Step::Sql(buffer, 0), lines));
    }
    steps
}

/// `step`, which ends at line `last` of its script, with its offset set.
fn with_offset(step: Step, last: usize) -> Step {
    match step {
        Step::Sql(sql, _) => {
            let offset = last - sql.matches('\n').count();
            Step::Sql(sql, offset)
        }
        command => command,
    }
}

/// `message` with each "Line: N" (sqlparser's location) moved `offset` lines
/// on, so it names the line of the script rather than of the statement.
fn shift_lines(message: &str, offset: usize) -> String {
    const LINE: &str = "Line: ";
    if offset == 0 {
        return message.to_string();
    }
    let mut out = String::new();
    let mut rest = message;
    while let Some(at) = rest.find(LINE) {
        let (before, after) = rest.split_at(at + LINE.len());
        out.push_str(before);
        let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        match after[..digits].parse::<usize>() {
            Ok(line) => out.push_str(&(line + offset).to_string()),
            Err(_) => out.push_str(&after[..digits]),
        }
        rest = &after[digits..];
    }
    out.push_str(rest);
    out
}

/// `.help shortcuts` in a terminal (rustyline's keys).
const TERMINAL_SHORTCUTS: &str = "\
Enter          Run the statement once it ends with ; (or a dot-command)
Up, Down       Step through the history
Ctrl-R         Search the history
Tab            Complete a dot-command, or a file name after .read, .open or a quote
Ctrl-C         Drop what is typed, or cancel the running statement
Ctrl-D         Exit";

/// `.help graph`: the graph SQL on a page. The e2e test of the browser shell
/// runs each form it shows.
fn graph_help() -> String {
    format!(
        "\
Graph SQL: SQL/PGQ (SQL:2023 Part 16), with DuckPGQ's extensions.

Define a graph over tables or views:
  CREATE PROPERTY GRAPH g
    VERTEX TABLES (people KEY (id))
    EDGE TABLES (knows SOURCE KEY (src) REFERENCES people (id)
                       DESTINATION KEY (dst) REFERENCES people (id));
  CREATE PROPERTY GRAPH g EDGE TABLES (edges SOURCE KEY (src) DESTINATION KEY (dst));

Match patterns, with GRAPH_TABLE:
  FROM GRAPH_TABLE (g
    MATCH (a)-[e]->(b) WHERE a.id = 1
    COLUMNS (a.id AS source, b.id AS target));

  (a)-[e]->(b)  (a)<-[e]-(b)  (a)-[e]-(b)   an edge out, in, or either way
  (a WHERE a.age > 30)                      a filter on one element
  -[e]->{{2}}  -[e]->{{1,3}}                    an edge repeated 2 times, or 1 to 3 times
  -[e]->*  -[e]->+                          any number of times, or at least once
                                            (with a selector or a path mode)
  ANY SHORTEST | ALL SHORTEST | SHORTEST k  selectors: which paths to keep
  WALK | TRAIL | SIMPLE | ACYCLIC           path modes: may a path repeat edges or vertices
  p = ... COLUMNS (path_length(p), vertices(p), edges(p))

  FROM GRAPH_TABLE (g
    MATCH p = ANY SHORTEST (a WHERE a.id = 1)-[e]->*(b WHERE b.id = 9)
    COLUMNS (path_length(p) AS steps, vertices(p) AS route));

Run algorithms, as table functions over a graph:
  SELECT * FROM pagerank(g, damping => 0.85);
  SELECT * FROM algorithms(g, pagerank(), community => louvain(seed => 1));

Any file is a table: SELECT * FROM 'edges.csv' LIMIT 3;
.functions lists the algorithms; .functions NAME shows one's parameters.
The SQL reference: {DOCS_URL}"
    )
}

/// The dot-command `name` was meant to be, if one is close enough to be a
/// typo or `name` begins only one (as `.q` begins `.quit`).
pub fn did_you_mean(name: &str) -> Option<&'static str> {
    suggest_command(name, COMMANDS.iter().map(|(command, _, _)| *command))
}

/// The command `name` begins, if it begins only one (`.q`, `.tab`: sqlite
/// and DuckDB users type these); otherwise the closest, if it is close
/// enough to be a typo.
fn suggest_command<'a>(name: &str, commands: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let commands: Vec<&str> = commands.into_iter().collect();
    let lower = name.to_ascii_lowercase();
    if lower.len() > 1 {
        let mut begun = commands.iter().filter(|c| c.starts_with(&lower));
        if let (Some(only), None) = (begun.next(), begun.next()) {
            return Some(only);
        }
    }
    closest(name, commands)
}

/// The candidate closest to `name`, ignoring case, if one is close enough to
/// be a typo.
fn closest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let name = name.to_ascii_lowercase();
    candidates
        .into_iter()
        .map(|candidate| (candidate, edit_distance(&name, candidate)))
        .filter(|(candidate, distance)| *distance <= 2.max(candidate.len() / 3))
        .min_by_key(|(_, distance)| *distance)
        .map(|(candidate, _)| candidate)
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
            "┌───────┬───────┐\n│   x   │   y   │\n│ Int64 │ Int64 │\n├───────┼───────┤\n│     1 │     2 │\n└───────┴───────┘\n"
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
                "┌───────┬──────┐\n│   x   │  y   │\n│ Int64 │ Utf8 │\n├───────┼──────┤\n│     1 │ a    │\n└───────┴──────┘\n",
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
        assert!(
            err.starts_with("Error: .mode cannot switch to the arrow format")
                && err.contains("Give --format arrow on the command line"),
            "{err}"
        );
        let (_, err) = feed(&mut shell, &[".mode box"]).await;
        assert!(err.contains("unknown mode 'box'"), "{err}");
        assert_eq!(shell.mode(), OutputFormat::Table);
    }

    /// Run `sql` in `mode` and return what it printed, failing on an error.
    async fn in_mode(mode: &str, sql: &str) -> String {
        let mut shell = shell();
        let (out, err) = feed(&mut shell, &[&format!(".mode {mode}"), sql]).await;
        assert_eq!(err, "", "{mode}: {sql}");
        out
    }

    /// N34: lists, structs and maps are written as quoted text in CSV and TSV.
    #[tokio::test]
    async fn csv_and_tsv_write_nested_values_as_text() {
        let sql = "SELECT [1, 2] AS l, named_struct('a', 1, 'b', 'x') AS s, \
                   map(['k'], [1]) AS m, [1, NULL] AS n, CAST(NULL AS INT[]) AS z;";
        assert_eq!(
            in_mode("csv", sql).await,
            "l,s,m,n,z\n\"[1, 2]\",\"{a: 1, b: x}\",{k: 1},\"[1, NULL]\",\n"
        );
        assert_eq!(
            in_mode("tsv", sql).await,
            "l\ts\tm\tn\tz\n[1, 2]\t{a: 1, b: x}\t{k: 1}\t[1, NULL]\t\n"
        );
        // Quotes and line breaks in a value are quoted, as CSV requires.
        assert_eq!(
            in_mode("csv", "SELECT 'say \"hi\"' AS q, 'a\nb' AS r, 'x\ty' AS t;").await,
            "q,r,t\n\"say \"\"hi\"\"\",\"a\nb\",x\ty\n"
        );
        // An empty result still writes the header row.
        assert_eq!(in_mode("csv", "SELECT 1 AS a WHERE false;").await, "a\n");
    }

    /// N36: in CSV and TSV a null is an empty field and `''` is `""`.
    #[tokio::test]
    async fn csv_tells_null_from_the_empty_string() {
        let sql = "SELECT NULL AS a, '' AS b, CAST(NULL AS VARCHAR) AS c, 1 AS d;";
        assert_eq!(in_mode("csv", sql).await, "a,b,c,d\n,\"\",,1\n");
        assert_eq!(in_mode("tsv", sql).await, "a\tb\tc\td\n\t\"\"\t\t1\n");
    }

    /// N35: JSON and NDJSON write `null`, so every row has every key.
    #[tokio::test]
    async fn json_writes_explicit_nulls() {
        assert_eq!(
            in_mode("json", "SELECT NULL AS a, '' AS b;").await,
            "[{\"a\":null,\"b\":\"\"}]\n"
        );
        assert_eq!(
            in_mode(
                "ndjson",
                "SELECT * FROM (SELECT NULL AS a, 1 AS b UNION ALL SELECT 2, 3) ORDER BY b;"
            )
            .await,
            "{\"a\":null,\"b\":1}\n{\"a\":2,\"b\":3}\n"
        );
        assert_eq!(
            in_mode("json", "SELECT [1, 2] AS l, CAST(NULL AS VARCHAR) AS s;").await,
            "[{\"l\":[1,2],\"s\":null}]\n"
        );
    }

    /// N37: an untyped NULL is shown as `NULL` in a table, as a typed one is.
    #[tokio::test]
    async fn table_shows_an_untyped_null_as_null() {
        let out = in_mode("table", "SELECT NULL AS n, CAST(NULL AS VARCHAR) AS v;").await;
        assert_eq!(
            out,
            "┌──────┬──────────┐\n│  n   │    v     │\n│ Null │ Utf8View │\n├──────┼──────────┤\n│ NULL │ NULL     │\n└──────┴──────────┘\n"
        );
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
        assert!(
            lines[2].starts_with("Run Time (s): real ")
                && lines[2].contains(" user ")
                && lines[2].contains(" sys ")
        );
        assert!(lines[3].starts_with("Run Time (s): real "));
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
        assert_eq!(shell.depth.get(), 0);
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
    async fn functions_lists_name_family_and_summary() {
        let mut shell = shell();
        shell.set_mode(OutputFormat::Csv);
        let (out, err) = feed(&mut shell, &[".functions"]).await;
        assert_eq!(err, "");
        assert!(out.starts_with("name,family,summary\n"), "{out}");
        assert!(
            out.contains("\ndegree,algorithm,Per-node degree.\n"),
            "{out}"
        );
        // The first sentence of a summary only.
        let hop = out.lines().find(|l| l.starts_with("hop,")).unwrap();
        assert!(!hop.contains("A seed is not its own row"), "{hop}");
    }

    #[tokio::test]
    async fn functions_name_shows_one_function() {
        let mut shell = shell();
        let (out, err) = feed(&mut shell, &[".functions PageRank"]).await;
        assert_eq!(err, "");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "pagerank (algorithm)", "{out}");
        assert!(lines[1].starts_with("PageRank"), "{out}");
        assert!(out.contains("\nParameters, after the graph, as name => value:\n"));
        // One parameter a line: name, type, default, doc, choices.
        let direction = lines
            .iter()
            .find(|l| l.trim_start().starts_with("direction "))
            .unwrap();
        assert!(
            direction.contains("string") && direction.contains("default 'out'"),
            "{direction}"
        );
        assert!(direction.ends_with("One of: out, both."), "{direction}");
        assert!(lines.iter().any(|l| l.trim_start().starts_with("damping ")));
        assert!(
            out.ends_with("\nOutput columns:\n  pagerank  Float64\n"),
            "{out}"
        );
        // A required parameter says so.
        let (out, _) = feed(&mut shell, &[".functions hop"]).await;
        assert!(out.contains("  seeds      node ids  required "), "{out}");
    }

    #[tokio::test]
    async fn functions_name_suggests_a_close_one() {
        let mut shell = shell();
        let (out, err) = feed(&mut shell, &[".functions pagerenk"]).await;
        assert_eq!(out, "");
        assert_eq!(
            err,
            "Error: no graph function named 'pagerenk'; did you mean 'pagerank'? .functions \
             lists them\n"
        );
        let (_, err) = feed(&mut shell, &[".functions frobnicate"]).await;
        assert_eq!(
            err,
            "Error: no graph function named 'frobnicate'; .functions lists them\n"
        );
    }

    #[tokio::test]
    async fn help_points_to_graphs_functions_and_docs() {
        let mut shell = shell();
        let (out, _) = feed(&mut shell, &[".help"]).await;
        assert!(out.contains("Run .help graph"), "{out}");
        let (out, _) = feed(&mut shell, &[".help graph"]).await;
        for needle in [
            "CREATE PROPERTY GRAPH g EDGE TABLES",
            "ANY SHORTEST | ALL SHORTEST | SHORTEST k",
            ".functions lists the algorithms",
            DOCS_URL,
        ] {
            assert!(out.contains(needle), "{needle} missing from:\n{out}");
        }
        let (out, _) = feed(&mut shell, &[".help shortcuts"]).await;
        assert!(out.contains("Ctrl-D"), "{out}");
        let (out, err) = feed(&mut shell, &[".help timer"]).await;
        assert_eq!(out.lines().count(), 1, "{out}{err}");
        let (_, err) = feed(&mut shell, &[".help zzzz"]).await;
        assert!(err.contains("no command matches 'zzzz'"), "{err}");
    }

    #[tokio::test]
    async fn host_commands_are_listed_and_handed_back() {
        let mut shell = shell();
        shell.add_host_command(".share", "", "Copy a link that replays this session");
        let (out, _) = feed(&mut shell, &[".help"]).await;
        assert!(out.contains(".share"), "{out}");
        assert_eq!(
            shell.feed(".share now").await,
            Flow::Host(".share now".into())
        );
        let (_, err) = feed(&mut shell, &[".shar"]).await;
        assert!(err.contains("did you mean '.share'?"), "{err}");
    }

    #[tokio::test]
    async fn version_and_print() {
        let mut shell = shell();
        shell.set_version("1.2.3");
        let (out, _) = feed(&mut shell, &[".version", ".print hello  world"]).await;
        assert_eq!(out, "Ursa v1.2.3\nhello  world\n");
    }

    #[test]
    fn scripts_split_into_steps_with_their_lines() {
        let dialect = GenericDialect {};
        let script = ".timer on\n\
                      -- a note\n\
                      SELECT 1;\n\
                      \n\
                      SELECT\n  2;  SELECT 3;\n\
                      .mode csv\n\
                      SELECT 'a\n.b' AS s;\n\
                      SELECT 4";
        assert_eq!(
            steps(script, &dialect),
            [
                Step::Command(".timer on".into()),
                Step::Sql("SELECT 1;\n".into(), 2),
                Step::Sql("SELECT\n  2;  SELECT 3;\n".into(), 4),
                Step::Command(".mode csv".into()),
                Step::Sql("SELECT 'a\n.b' AS s;\n".into(), 7),
                Step::Sql("SELECT 4\n".into(), 9),
            ]
        );
        // A statement closed by a comment's end, after its `;`.
        assert_eq!(
            steps("SELECT 1; /* a\nnote */\nSELECT 2;", &dialect),
            [
                Step::Sql("SELECT 1; /* a\nnote */\n".into(), 0),
                Step::Sql("SELECT 2;\n".into(), 2),
            ]
        );
    }

    #[test]
    fn syntax_errors_name_the_scripts_line() {
        assert_eq!(
            shift_lines("Expected: x, found: y at Line: 1, Column: 5", 3),
            "Expected: x, found: y at Line: 4, Column: 5"
        );
        assert_eq!(shift_lines("at Line: 2", 0), "at Line: 2");
        assert_eq!(shift_lines("no location", 7), "no location");
    }

    #[tokio::test]
    async fn a_script_stops_at_its_first_error() {
        let mut shell = shell();
        shell.set_mode(OutputFormat::Csv);
        shell.stop_on_error();
        let flow = shell
            .run_text("SELECT 1 AS a;\nSELECT nope;\nSELECT 2 AS b;\n")
            .await;
        assert_eq!(flow, Flow::Exit);
        assert!(shell.failed());
        let (out, err) = take(&mut shell);
        assert_eq!(out, "a\n1\n");
        assert!(
            err.starts_with("Error: ") && err.lines().count() == 1,
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_syntax_error_anywhere_in_a_script_runs_none_of_it() {
        let mut shell = shell();
        shell.stop_on_error();
        let flow = shell
            .run_text(".mode csv\nSELECT 1 AS a;\n\nSELECT\n  2 FROM t WHERE;\n")
            .await;
        assert_eq!(flow, Flow::Exit);
        let (out, err) = take(&mut shell);
        assert_eq!(out, "");
        assert!(err.contains("at Line: 5, Column: "), "{err}");
        // Not even the dot-commands before it.
        assert_eq!(shell.mode(), OutputFormat::Table);
    }

    #[tokio::test]
    async fn an_unknown_command_fails_a_script() {
        let mut shell = shell();
        shell.stop_on_error();
        assert_eq!(shell.run_text(".tabels\nSELECT 1;\n").await, Flow::Exit);
        assert!(shell.failed());
        let (out, err) = take(&mut shell);
        assert_eq!(out, "");
        assert!(err.contains("did you mean '.tables'?"), "{err}");
    }

    #[tokio::test]
    async fn with_arrow_only_the_result_goes_to_the_output() {
        let mut shell = Shell::new(
            ursa_engine::new_session(),
            OutputFormat::Arrow,
            Vec::new(),
            Vec::new(),
        );
        shell.stop_on_error();
        let flow = shell.run_text(".timer on\n.mode\nSELECT 1 AS a;\n").await;
        assert_eq!(flow, Flow::Continue);
        let out = std::mem::take(&mut shell.out);
        let err = String::from_utf8(std::mem::take(&mut shell.err)).unwrap();
        // An Arrow IPC stream starts with its continuation marker.
        assert!(out.starts_with(&[0xff, 0xff, 0xff, 0xff]), "{out:?}");
        assert!(
            err.starts_with("The output mode is arrow.\nRun Time (s): real "),
            "{err}"
        );
        // A second result is refused.
        assert_eq!(shell.run_text("SELECT 2 AS b;").await, Flow::Exit);
        let (out, err) = take(&mut shell);
        assert_eq!(out, "");
        assert!(
            err.contains("--format arrow writes a single result"),
            "{err}"
        );
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_read_counts_itself_out() {
        let dir = tempfile::tempdir().unwrap();
        let slow = dir.path().join("slow.sql");
        std::fs::write(
            &slow,
            "SELECT count(*) FROM generate_series(1, 100000000000);\n",
        )
        .unwrap();
        let read = format!(".read {}", slow.display());
        let mut shell = shell();
        // More than MAX_READ_DEPTH times: each cancelled (dropped) .read
        // counted itself in and must count itself out.
        for _ in 0..MAX_READ_DEPTH + 2 {
            let cancelled =
                tokio::time::timeout(std::time::Duration::from_millis(50), shell.feed(&read)).await;
            assert!(cancelled.is_err(), "the query finished");
            assert_eq!(shell.depth.get(), 0);
        }
        let quick = dir.path().join("quick.sql");
        std::fs::write(&quick, ".mode csv\nSELECT 1 AS a;\n").unwrap();
        let (out, err) = feed(&mut shell, &[&format!(".read {}", quick.display())]).await;
        assert_eq!((out.as_str(), err.as_str()), ("a\n1\n", ""));
    }

    #[tokio::test]
    async fn a_dot_command_may_end_with_a_semicolon() {
        let mut shell = shell();
        let (out, err) = feed(
            &mut shell,
            &[
                ".mode csv;",
                ".maxrows 5;",
                ".timer on ;",
                ".timer off;",
                ".tables;",
            ],
        )
        .await;
        assert_eq!(err, "");
        assert_eq!(out, "name,type\n");
        assert_eq!(shell.mode(), OutputFormat::Csv);
        assert_eq!(shell.max_rows, 5);
        assert!(!shell.timer);
        let (out, _) = feed(&mut shell, &[".maxwidth 30;", ".maxwidth;"]).await;
        assert_eq!(out, "Tables are drawn at most 30 characters wide.\n");
        assert_eq!(shell.feed(".quit;").await, Flow::Exit);
        // Only one, and not .print's: it prints what it is given.
        let (out, err) = feed(&mut shell, &[".print a;", ".maxrows 5;;"]).await;
        assert_eq!(out, "a;\n");
        assert!(err.contains("usage: .maxrows N"), "{err}");
        // A host's command comes back without it.
        shell.add_host_command(".share", "", "Share");
        assert_eq!(shell.feed(".share;").await, Flow::Host(".share".into()));
    }

    #[tokio::test]
    async fn the_start_of_a_command_is_suggested_not_run() {
        let mut shell = shell();
        assert_eq!(shell.feed(".q").await, Flow::Continue);
        let (_, err) = take(&mut shell);
        assert!(
            err.contains("unknown command '.q'; did you mean '.quit'?"),
            "{err}"
        );
        let (out, err) = feed(&mut shell, &[".tab"]).await;
        assert_eq!(out, "");
        assert!(err.contains("did you mean '.tables'?"), "{err}");
        assert_eq!(did_you_mean(".desc"), Some(".describe"));
        assert_eq!(did_you_mean(".EX"), Some(".exit"));
        // `.t` begins .tables and .timer: no guess from that.
        assert_eq!(did_you_mean(".t"), None);
    }

    #[tokio::test]
    async fn describe_finds_a_name_in_any_case_and_runs_nothing_else() {
        let mut shell = shell();
        shell.set_mode(OutputFormat::Csv);
        let (_, err) = feed(
            &mut shell,
            &[
                "CREATE TABLE \"Mixed\" AS SELECT 1 AS x;",
                "CREATE VIEW v AS SELECT 2 AS y;",
                "CREATE SCHEMA s;",
                "CREATE VIEW s.\"Odd Name\" AS SELECT 3 AS z;",
            ],
        )
        .await;
        assert_eq!(err, "");
        let (out, _) = feed(&mut shell, &[".tables"]).await;
        assert!(out.contains("\nMixed,table\n"), "{out}");
        for name in [
            "Mixed",
            "mixed",
            "MIXED",
            "\"Mixed\"",
            "public.Mixed",
            "Mixed;",
        ] {
            let (out, err) = feed(&mut shell, &[&format!(".describe {name}")]).await;
            assert_eq!(err, "", "{name}");
            assert!(
                out.starts_with("column_name,data_type,") && out.contains("\nx,"),
                "{name}: {out}"
            );
        }
        let (out, err) = feed(&mut shell, &[".describe s.\"Odd Name\""]).await;
        assert_eq!(err, "");
        assert!(out.contains("\nz,"), "{out}");
        // Quoted, the case must match.
        let (_, err) = feed(&mut shell, &[".describe \"mixed\""]).await;
        assert!(err.starts_with("Error: "), "{err}");
        // The name is a name, not SQL.
        let (out, err) = feed(&mut shell, &[".describe v; SELECT 99"]).await;
        assert_eq!(out, "");
        assert_eq!(
            err,
            "Error: usage: .describe NAME (a graph, table or view); 'v; SELECT 99' is not a \
             name\n"
        );
    }

    #[tokio::test]
    async fn a_script_with_arrow_must_write_a_result() {
        let mut shell = Shell::new(
            ursa_engine::new_session(),
            OutputFormat::Arrow,
            Vec::new(),
            Vec::new(),
        );
        shell.stop_on_error();
        assert_eq!(
            shell
                .run_text(".mode arrow\nCREATE VIEW v AS SELECT 1 AS a;")
                .await,
            Flow::Continue
        );
        shell.end_script();
        assert!(shell.failed());
        let (out, err) = take(&mut shell);
        assert_eq!(out, "");
        assert!(
            err.starts_with("Error: --format arrow writes the result of one query, but no statement returned one"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn the_next_line_is_coloured_as_part_of_the_statement() {
        use crate::highlight::{State, COMMAND, KEYWORD, STRING};
        let mut shell = shell();
        assert_eq!(shell.highlight_state(), &State::Start);
        assert!(shell.highlight_line(".tables").starts_with(COMMAND));
        shell.feed("SELECT 'a").await;
        assert_eq!(shell.highlight_state(), &State::String { escapes: false });
        assert!(
            shell
                .highlight_line("b' AS s")
                .starts_with(&format!("{STRING}b'{RESET} {KEYWORD}AS")),
            "{:?}",
            shell.highlight_line("b' AS s")
        );
        shell.feed("b' AS s,").await;
        assert_eq!(shell.highlight_state(), &State::Statement);
        // `.5` goes on the statement: SQL, not a command.
        assert!(!shell.highlight_line(".5 AS half").contains(COMMAND));
        shell.feed("/* a /* nested").await;
        assert_eq!(shell.highlight_state(), &State::Comment(2));
        shell.cancel_input();
        assert_eq!(shell.highlight_state(), &State::Start);
        shell.feed("SELECT 1").await;
        shell.feed(";").await;
        assert_eq!(shell.highlight_state(), &State::Start);
        take(&mut shell);
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
