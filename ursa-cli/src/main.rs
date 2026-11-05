//! `ursa`: SQL for graph analytics over Parquet and CSV files.
//!
//! ```text
//! ursa -c "SELECT * FROM 'karate_edges.csv' LIMIT 3"
//! ursa -f csv --file query.sql > out.csv
//! echo "SELECT 1" | ursa
//! ursa club.ursa -c "SELECT * FROM degree(karate)"
//! ```
//!
//! With a database file (`club.ursa`), views, external tables and graphs are
//! kept in it between runs; without one, the session is in memory.

use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use arrow::error::ArrowError;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser};
use datafusion::error::DataFusionError;
use datafusion::prelude::SessionContext;

mod print;

use print::{has_columns, write_stream, OutputFormat};

#[derive(Debug, Parser)]
#[command(
    name = "ursa",
    version,
    about = "SQL for graph analytics over Parquet and CSV files, local or in object storage.",
    after_help = "Any file is a table: ursa -c \"SELECT * FROM 'edges.parquet' LIMIT 3\""
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
    #[arg(long = "file", value_name = "PATH")]
    file: Vec<PathBuf>,

    /// Output format.
    #[arg(short = 'f', long = "format", value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

/// Where SQL comes from, in the order given on the command line.
enum Source {
    Command(String),
    File(PathBuf),
    Stdin(String),
}

fn main() -> ExitCode {
    let matches = Args::command().get_matches();
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
    if sources.is_empty() && !io::stdin().is_terminal() {
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
    match runtime.block_on(run(sources, &args)) {
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

fn fail(message: &str) -> ExitCode {
    eprintln!("Error: {message}");
    ExitCode::FAILURE
}

async fn run(sources: Vec<Source>, args: &Args) -> Result<(), DataFusionError> {
    let format = args.format;
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
    if sources.is_empty() {
        eprintln!(
            "The interactive shell is not available yet (planned in #238).\n\
             Run SQL with -c \"<sql>\", --file <path>, or on stdin. See ursa --help."
        );
        return Ok(());
    }
    let result = run_sources(&ctx, sources, format).await;
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

async fn run_sources(
    ctx: &SessionContext,
    sources: Vec<Source>,
    format: OutputFormat,
) -> Result<(), DataFusionError> {
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let mut printed = 0;
    for source in sources {
        let sql = match source {
            Source::Command(sql) | Source::Stdin(sql) => sql,
            Source::File(path) => std::fs::read_to_string(&path).map_err(|e| {
                DataFusionError::Execution(format!("cannot read '{}': {e}", path.display()))
            })?,
        };
        printed += run_sql(ctx, &sql, format, &mut out, printed).await?;
    }
    out.flush()?;
    Ok(())
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
                "-f arrow writes a single result; run one query that returns rows".into(),
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
