//! `ursa serve [DIR]`: the shell's page in a browser tab, over a WebSocket to
//! this process's engine (`ursa-server`, the spike of #536).
//!
//! ```text
//! ursa serve web/.data --port 4280
//! ```
//!
//! It prints the URL to open, with the run's token, and serves until Ctrl+C.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Where the page's files are, unless `--assets` says: this checkout's
/// `web/dist` (`bun run build`) and the console's build (`bun run console`).
const ASSETS: [&str; 2] = [
    concat!(env!("CARGO_MANIFEST_DIR"), "/../web/dist-console"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/../web/dist"),
];

#[derive(Debug, Parser)]
#[command(
    name = "ursa serve",
    about = "Serve Ursa's shell (and the console page) to a browser tab on this machine, \
             over a local WebSocket to this process's engine."
)]
struct ServeArgs {
    /// The directory relative paths in queries resolve against, and the
    /// shell's datasets come from. The current directory by default.
    #[arg(value_name = "DIR")]
    dir: Option<PathBuf>,

    /// The port, on 127.0.0.1 and ::1 only; the next free one if it is taken.
    #[arg(long, default_value_t = ursa_server::DEFAULT_PORT)]
    port: u16,

    /// The page's files (a `web/dist`), in place of this checkout's. May be
    /// repeated: the first that has a file serves it.
    #[arg(long, value_name = "DIR")]
    assets: Vec<PathBuf>,
}

/// Run `ursa serve`, given the arguments after `serve`.
pub fn main(args: impl IntoIterator<Item = std::ffi::OsString>) -> ExitCode {
    let args = ServeArgs::parse_from(args);
    let root = args.dir.unwrap_or_else(|| PathBuf::from("."));
    // Relative paths in the session's queries are files in DIR.
    if let Err(e) = std::env::set_current_dir(&root) {
        eprintln!("Error: cannot serve {}: {e}", root.display());
        return ExitCode::FAILURE;
    }
    let assets = if args.assets.is_empty() {
        ASSETS.iter().map(PathBuf::from).collect()
    } else {
        args.assets
    };
    let config = ursa_server::Config {
        root: PathBuf::from("."),
        port: args.port,
        assets,
        // The crate's version, as the browser demo reports its own: the
        // page's banner shows it.
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("Error: cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let served = runtime.block_on(async {
        let bound = ursa_server::bind(config).await?;
        if bound.port() != args.port {
            println!("Port {} is taken; using {}.", args.port, bound.port());
        }
        println!("Ursa is serving {} at", std::env::current_dir()?.display());
        println!("  {}", bound.url());
        println!("Open that link in a browser on this machine. Ctrl+C stops the server.");
        bound.serve().await
    });
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}
