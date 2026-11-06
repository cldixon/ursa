//! `ursa`: SQL for graph analytics over Parquet and CSV files.
//!
//! ```text
//! ursa -c "SELECT * FROM 'karate_edges.csv' LIMIT 3"
//! ursa --format csv -f query.sql > out.csv
//! echo "SELECT 1" | ursa
//! ursa club.ursa -c "SELECT * FROM degree(karate)"
//! ursa club.ursa          # the interactive shell
//! ```
//!
//! With a database file (`club.ursa`), views, external tables and graphs are
//! kept in it between runs; without one, the session is in memory. With no
//! SQL to run and a terminal on stdin, `ursa` starts the shell ([`shell`],
//! [`repl`]).

use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use arrow::error::ArrowError;
use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser};
use datafusion::error::DataFusionError;
use datafusion::prelude::SessionContext;

mod input;
mod print;
mod repl;
mod shell;
mod startup;

use print::{has_columns, write_stream, OutputFormat};
use shell::{Flow, Shell};

#[derive(Debug, Parser)]
#[command(
    name = "ursa",
    version,
    about = "SQL for graph analytics over Parquet and CSV files, local or in object storage.",
    after_help = "Any file is a table: ursa -c \"SELECT * FROM 'edges.parquet' LIMIT 3\"\n\
                  With no SQL to run, ursa starts an interactive shell; enter .help there."
)]
struct Args {
    /// A database file (.ursa), created if it does not exist. It keeps the
    /// views, external tables and graphs you define. Without one, ursa works in
    /// memory.
    #[arg(value_name = "DATABASE")]
    database: Option<String>,

    /// Open the database without changing it: statements that define or drop
    /// anything are refused, and other processes may write it meanwhile.
    #[arg(long = "readonly", requires = "database")]
    readonly: bool,

    /// Run SQL and exit. Separate statements with ';'. May be repeated.
    #[arg(short = 'c', long = "command", value_name = "SQL")]
    command: Vec<String>,

    /// Run the SQL in a file and exit. May be repeated.
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    file: Vec<PathBuf>,

    /// Output format.
    #[arg(long = "format", value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,

    /// Run the SQL and dot-commands in FILE at startup, before the shell's
    /// prompt or the -c, --file or stdin SQL. Replaces ~/.ursarc. Also
    /// spelled -init, as in DuckDB.
    #[arg(long = "init", value_name = "FILE", conflicts_with = "no_init")]
    init: Option<PathBuf>,

    /// Run no init file, not even ~/.ursarc. Also spelled -no-init.
    #[arg(long = "no-init")]
    no_init: bool,

    /// Keep no shell history. Otherwise it is kept in ~/.ursa_history, or in
    /// the file $URSA_HISTORY names (set it empty to keep none).
    #[arg(long = "no-history")]
    no_history: bool,
}

/// Where SQL comes from, in the order given on the command line.
enum Source {
    Command(String),
    File(PathBuf),
    Stdin(String),
}

fn main() -> ExitCode {
    let matches = Args::command().get_matches_from(single_dash_longs(std::env::args_os()));
    let args = match Args::from_arg_matches(&matches) {
        Ok(args) => args,
        Err(e) => e.exit(),
    };

    if let Some(database) = &args.database {
        if !database.ends_with(".ursa") {
            return fail(&format!(
                "'{database}' is not a database file (those end in .ursa); to run SQL, use \
                 -c \"<sql>\" or --file <path>"
            ));
        }
    }

    let mut sources = sources_in_order(&args, &matches);
    let interactive = sources.is_empty() && io::stdin().is_terminal();
    if interactive && args.format == OutputFormat::Arrow {
        return fail("the shell cannot show the arrow format; use --format arrow with -c or -f");
    }
    if sources.is_empty() && !interactive {
        let mut sql = String::new();
        if let Err(e) = io::stdin().read_to_string(&mut sql) {
            return fail(&format!("cannot read SQL from stdin: {e}"));
        }
        if !sql.trim().is_empty() {
            sources.push(Source::Stdin(sql));
        }
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => return fail(&format!("cannot start: {e}")),
    };
    let format_given = matches.value_source("format") == Some(ValueSource::CommandLine);
    match runtime.block_on(run(sources, &args, interactive, format_given)) {
        Ok(()) => ExitCode::SUCCESS,
        // A reader that stops early (`ursa … | head`) is not an error.
        Err(e) if is_broken_pipe(&e) => ExitCode::SUCCESS,
        Err(e) => fail(&ursa_engine::user_message(&e)),
    }
}

/// `-c` and `--file` sources in the order they were given.
fn sources_in_order(args: &Args, matches: &ArgMatches) -> Vec<Source> {
    let commands = matches
        .indices_of("command")
        .into_iter()
        .flatten()
        .zip(args.command.iter().cloned().map(Source::Command));
    let files = matches
        .indices_of("file")
        .into_iter()
        .flatten()
        .zip(args.file.iter().cloned().map(Source::File));
    let mut sources: Vec<(usize, Source)> = commands.chain(files).collect();
    sources.sort_by_key(|(index, _)| *index);
    sources.into_iter().map(|(_, source)| source).collect()
}

/// `-f` runs a file, as in DuckDB and psql. A missing file named like an
/// output format (`ursa -f json`) was most likely meant as `--format`.
fn format_hint(path: &std::path::Path) -> String {
    use clap::ValueEnum;
    let name = path.to_string_lossy();
    let is_format = OutputFormat::value_variants().iter().any(|f| {
        f.to_possible_value()
            .is_some_and(|v| v.matches(&name, true))
    });
    if is_format && !path.exists() {
        format!("; to choose the output format, use --format {name}")
    } else {
        String::new()
    }
}

/// DuckDB spells its long options with one dash (`-init`); clap needs two.
/// Rewrite `-init` and `-no-init`, except as the value of an option.
fn single_dash_longs(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    const TAKE_VALUES: [&str; 6] = ["-c", "--command", "--file", "-f", "--format", "--init"];
    let mut out: Vec<OsString> = Vec::new();
    for arg in args {
        let is_value = out.len() > 1
            && out
                .last()
                .is_some_and(|previous| TAKE_VALUES.iter().any(|option| previous == option));
        let rewritten = match arg.to_str() {
            _ if is_value || out.is_empty() => arg,
            Some("-init") => "--init".into(),
            Some("-no-init") => "--no-init".into(),
            Some(a) if a.starts_with("-init=") => format!("-{a}").into(),
            _ => arg,
        };
        out.push(rewritten);
    }
    out
}

fn fail(message: &str) -> ExitCode {
    eprintln!("Error: {message}");
    ExitCode::FAILURE
}

async fn run(
    sources: Vec<Source>,
    args: &Args,
    interactive: bool,
    format_given: bool,
) -> Result<(), DataFusionError> {
    // Open (or create) the database even with no SQL to run, so `ursa
    // club.ursa` makes the file, as Getting Started says.
    let ctx = match &args.database {
        Some(path) => {
            let options = ursa_sql::OpenOptions {
                read_only: args.readonly,
                ..Default::default()
            };
            ursa_sql::open_session(path, &options).await?
        }
        None => ursa_engine::new_session(),
    };
    let result = run_session(ctx.clone(), sources, args, interactive, format_given).await;
    // Finish storing the indexes this process built before it exits.
    ursa_engine::database::flush(&ctx.state());
    if std::env::var_os(INDEX_STATS_VAR).is_some() {
        eprintln!(
            "ursa: graph indexes built {}, loaded {}",
            ursa_engine::graph::index_builds(&ctx),
            ursa_engine::graph::index_loads(&ctx)
        );
    }
    result
}

/// Set to print, on exit, how many graph indexes the process built and how
/// many it loaded from the database (for tests and for checking that a
/// database's stored indexes are used).
const INDEX_STATS_VAR: &str = "URSA_INDEX_STATS";

/// Run the init file, then the shell or the SQL given, on `ctx`.
async fn run_session(
    ctx: SessionContext,
    sources: Vec<Source>,
    args: &Args,
    interactive: bool,
    format_given: bool,
) -> Result<(), DataFusionError> {
    let home = startup::home_dir();
    let init = startup::init_file(
        args.init.as_deref(),
        args.no_init,
        interactive,
        home.as_deref(),
    );
    if !interactive && init.is_none() && sources.is_empty() {
        return Ok(());
    }
    let mut shell = Shell::new(ctx, args.format, io::stdout(), io::stderr());
    if interactive {
        println!("{}", banner(args));
    }
    if let Some(init) = init {
        if shell.run_file(&init).await == Flow::Exit {
            return Ok(());
        }
        // `-f` given on the command line wins over a `.mode` in the file.
        if format_given {
            shell.set_mode(args.format);
        }
    }
    if interactive {
        let history = startup::history_file(
            args.no_history,
            std::env::var_os(startup::HISTORY_VAR),
            home.as_deref(),
        );
        return repl::run(&mut shell, history)
            .await
            .map_err(|e| DataFusionError::External(Box::new(e)));
    }

    let format = shell.mode();
    let ctx = shell.ctx().clone();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let mut printed = 0;
    for source in sources {
        let sql = match source {
            Source::Command(sql) | Source::Stdin(sql) => sql,
            Source::File(path) => std::fs::read_to_string(&path).map_err(|e| {
                DataFusionError::Execution(format!(
                    "cannot read '{}': {e}{}",
                    path.display(),
                    format_hint(&path)
                ))
            })?,
        };
        printed += run_sql(&ctx, &sql, format, &mut out, printed).await?;
    }
    out.flush()?;
    Ok(())
}

/// The shell's first line: the version, and what the session is.
fn banner(args: &Args) -> String {
    let session = match &args.database {
        Some(path) if args.readonly => format!("database {path} (read-only)"),
        Some(path) => format!("database {path}"),
        None => "in memory".to_string(),
    };
    format!(
        "ursa {}, {session}. Enter .help for commands, .quit to exit.",
        env!("CARGO_PKG_VERSION")
    )
}

/// Parse all of `sql` (so a syntax error anywhere runs nothing), then run each
/// statement and print its result. Returns how many results were printed.
async fn run_sql<W: Write>(
    ctx: &SessionContext,
    sql: &str,
    format: OutputFormat,
    out: &mut W,
    printed_before: usize,
) -> Result<usize, DataFusionError> {
    let statements = ursa_sql::parse_sql_with_text_for(ctx, sql)?;
    let config = ctx.state().config().options().format.clone();
    let options = (&config).try_into()?;
    let mut printed = 0;
    for (statement, text) in statements {
        let frame = ursa_sql::execute_with_text(ctx, statement, &text).await?;
        let stream = frame.execute_stream().await?;
        if !has_columns(&stream.schema()) {
            continue;
        }
        if format == OutputFormat::Arrow && printed_before + printed > 0 {
            return Err(DataFusionError::Plan(
                "--format arrow writes a single result; run one query that returns rows".into(),
            ));
        }
        write_stream(format, stream, out, &options).await?;
        printed += 1;
    }
    Ok(printed)
}

fn is_broken_pipe(err: &DataFusionError) -> bool {
    match err {
        DataFusionError::IoError(e) => e.kind() == io::ErrorKind::BrokenPipe,
        DataFusionError::ArrowError(e, _) => {
            matches!(e.as_ref(), ArrowError::IoError(_, e) if e.kind() == io::ErrorKind::BrokenPipe)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(args: &[&str]) -> Vec<String> {
        single_dash_longs(args.iter().map(OsString::from))
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn single_dash_init_is_rewritten() {
        assert_eq!(
            rewrite(&["ursa", "-init", "a.sql", "club.ursa"]),
            ["ursa", "--init", "a.sql", "club.ursa"]
        );
        assert_eq!(rewrite(&["ursa", "-no-init"]), ["ursa", "--no-init"]);
        assert_eq!(rewrite(&["ursa", "-init=a.sql"]), ["ursa", "--init=a.sql"]);
        // Not as the value of an option.
        assert_eq!(
            rewrite(&["ursa", "-c", "-init", "--init", "-no-init"]),
            ["ursa", "-c", "-init", "--init", "-no-init"]
        );
        // The program name is left alone.
        assert_eq!(rewrite(&["-init"]), ["-init"]);
    }

    #[test]
    fn flags_parse() {
        let args =
            Args::try_parse_from(rewrite(&["ursa", "-init", "a.sql", "--no-history"])).unwrap();
        assert_eq!(args.init, Some(PathBuf::from("a.sql")));
        assert!(args.no_history && !args.no_init);
        assert!(Args::try_parse_from(["ursa", "--init", "a.sql", "--no-init"]).is_err());
    }

    #[test]
    fn the_banner_names_the_session() {
        let version = env!("CARGO_PKG_VERSION");
        let args = Args::try_parse_from(["ursa"]).unwrap();
        assert_eq!(
            banner(&args),
            format!("ursa {version}, in memory. Enter .help for commands, .quit to exit.")
        );
        let args = Args::try_parse_from(["ursa", "club.ursa", "--readonly"]).unwrap();
        assert!(
            banner(&args).starts_with(&format!("ursa {version}, database club.ursa (read-only).")),
            "{}",
            banner(&args)
        );
    }
}
