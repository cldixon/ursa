//! `ursa serve [DIR]`: the Ursa Console in a browser tab (and the Web Shell
//! beside it), over a WebSocket to this process's engine (`ursa-server`,
//! design/console.md §5).
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

/// This checkout's `web/dist-console` and `web/dist` (`bun run build`): where
/// a build without the page's files built in looks for them, unless
/// `--assets` says. The console's files are its own directory's, so the
/// deployed shell (`web/dist`) has none of them.
const CHECKOUT_ASSETS: [&str; 2] = [
    concat!(env!("CARGO_MANIFEST_DIR"), "/../web/dist-console"),
    concat!(env!("CARGO_MANIFEST_DIR"), "/../web/dist"),
];

#[derive(Debug, Parser)]
#[command(
    name = "ursa serve",
    about = "Serve the Ursa Console (and the Web Shell) to a browser tab on this \
             machine, over a local WebSocket to this process's engine."
)]
struct ServeArgs {
    /// The directory relative paths in queries resolve against, and the
    /// shell's datasets come from (/data/). The current directory by default.
    #[arg(value_name = "DIR")]
    dir: Option<PathBuf>,

    /// The port, on 127.0.0.1 and ::1 only; the next free one if it is taken.
    #[arg(long, default_value_t = ursa_server::DEFAULT_PORT)]
    port: u16,

    /// The page's files, in place of the ones built into this binary:
    /// `web/dist-console` (the console) and `web/dist` (the shell), built by
    /// `bun run build`. May be repeated: the first that has a file serves it.
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
        if bound.has_console() {
            println!(
                "The Web Shell is at http://127.0.0.1:{}/ once the link has opened.",
                bound.port()
            );
        }
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

/// Whether `dir` has a page: the shell's `index.html`, or the console's.
fn has_page(dir: &std::path::Path) -> bool {
    dir.join("index.html").is_file() || dir.join("console").join("index.html").is_file()
}

/// Where the page's files come from: `--assets`, else the ones built in,
/// else this checkout's `web/dist-console` and `web/dist`.
fn assets(dirs: Vec<PathBuf>) -> Result<Assets, String> {
    if let Some(missing) = dirs.iter().find(|d| !has_page(d)) {
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
    let checkout: Vec<PathBuf> = CHECKOUT_ASSETS
        .iter()
        .map(PathBuf::from)
        .filter(|dir| has_page(dir))
        .collect();
    if checkout.iter().any(|dir| dir.join("index.html").is_file()) {
        return Ok(Assets::Dirs(checkout));
    }
    Err(
        "this ursa has no page built in: pass --assets DIR, a web/dist built \
         by `bun run build` (see web/README.md)"
            .to_string(),
    )
}
