//! `ursa serve [DIR]`: Ursa's shell in a browser tab, over a WebSocket to
//! this process's engine (`ursa-server`, design/console.md §5).
//!
//! ```text
//! ursa serve web/.data --port 4280
//! ```
//!
//! It prints the URL to open, with the run's token, and serves until Ctrl+C,
//! which stops it gracefully: running requests are cancelled and the pages'
//! sessions closed.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use ursa_server::Assets;

/// This checkout's `web/dist` (`bun run build`): where a build without the
/// page's files built in looks for them, unless `--assets` says.
const CHECKOUT_ASSETS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../web/dist");

#[derive(Debug, Parser)]
#[command(
    name = "ursa serve",
    about = "Serve Ursa's shell to a browser tab on this machine, over a local \
             WebSocket to this process's engine."
)]
struct ServeArgs {
    /// The directory relative paths in queries resolve against, and the
    /// shell's datasets come from (/data/). The current directory by default.
    #[arg(value_name = "DIR")]
    dir: Option<PathBuf>,

    /// The port, on 127.0.0.1 and ::1 only; the next free one if it is taken.
    #[arg(long, default_value_t = ursa_server::DEFAULT_PORT)]
    port: u16,

    /// The page's files: a `web/dist` built by `bun run build`, in place of
    /// the ones built into this binary. May be repeated: the first that has a
    /// file serves it.
    #[arg(long, value_name = "DIR")]
    assets: Vec<PathBuf>,
}

/// Run `ursa serve`, given the arguments from `serve` on.
pub fn main(args: impl IntoIterator<Item = std::ffi::OsString>) -> ExitCode {
    let args = ServeArgs::parse_from(args);
    let assets = match assets(args.assets) {
        Ok(assets) => assets,
        Err(e) => {
            eprintln!("Error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let root = args.dir.unwrap_or_else(|| PathBuf::from("."));
    // Relative paths in the sessions' queries are files in DIR: one root for
    // the process until sessions have their own.
    if let Err(e) = std::env::set_current_dir(&root) {
        eprintln!("Error: cannot serve {}: {e}", root.display());
        return ExitCode::FAILURE;
    }
    let config = ursa_server::Config {
        root: PathBuf::from("."),
        port: args.port,
        assets,
        // The crate's version, as the browser demo reports its own (the
        // page's banner shows it as one word: `ursa --version` adds the
        // commit).
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
        if args.port != 0 && bound.port() != args.port {
            println!("Port {} is taken; using {}.", args.port, bound.port());
        }
        println!("Ursa is serving {} at", std::env::current_dir()?.display());
        println!("  {}", bound.url());
        println!("Open that link in a browser on this machine. Ctrl+C stops the server.");
        bound
            .serve_with_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                println!("Stopping.");
            })
            .await
    });
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Where the page's files come from: `--assets`, else the ones built in,
/// else this checkout's `web/dist`.
fn assets(dirs: Vec<PathBuf>) -> Result<Assets, String> {
    if let Some(missing) = dirs.iter().find(|d| !d.join("index.html").is_file()) {
        return Err(format!(
            "--assets {}: no index.html there (build the page with `bun run build` in web/)",
            missing.display()
        ));
    }
    if !dirs.is_empty() {
        // Absolute: `ursa serve` changes directory to DIR.
        let dirs = dirs.into_iter().map(|d| d.canonicalize().unwrap_or(d));
        return Ok(Assets::Dirs(dirs.collect()));
    }
    if Assets::EMBEDDED {
        return Ok(Assets::Embedded);
    }
    let checkout = PathBuf::from(CHECKOUT_ASSETS);
    if checkout.join("index.html").is_file() {
        return Ok(Assets::Dirs(vec![checkout]));
    }
    Err(
        "this ursa has no page built in: pass --assets DIR, a web/dist built \
         by `bun run build` (see web/README.md)"
            .to_string(),
    )
}
