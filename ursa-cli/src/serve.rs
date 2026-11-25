//! `ursa serve [DATABASE | DIR]`: Ursa's shell in a browser tab, over a
//! WebSocket to this process's engine (`ursa-server`, design/console.md §5).
//!
//! ```text
//! ursa serve                     # in memory, rooted in the current directory
//! ursa serve web/.data           # rooted in a directory
//! ursa serve club.ursa           # a database file, rooted in its directory
//! ursa serve club.ursa --read-only --no-open --port 4281
//! ```
//!
//! It prints the URL to open, with the run's token, opens it in the default
//! browser (unless `--no-open`), and serves until Ctrl+C, which stops it
//! gracefully: running requests are cancelled and the pages' sessions
//! closed.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

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
    /// A database file (.ursa), opened as `ursa club.ursa` opens one (and
    /// created if it does not exist), or a directory. Relative paths in
    /// queries resolve against the directory (a database file's own), and
    /// the shell's datasets come from it (/data/). Without one, the session
    /// is in memory, in the current directory.
    #[arg(value_name = "DATABASE | DIR")]
    path: Option<PathBuf>,

    /// The port, on 127.0.0.1 and ::1 only; the next free one if it is taken.
    #[arg(long, default_value_t = ursa_server::DEFAULT_PORT)]
    port: u16,

    /// Print the link, but do not open it in the default browser.
    #[arg(long)]
    no_open: bool,

    /// Write no files: COPY … TO, INSERT INTO a table of files, .output,
    /// .once, and statements that change the database (opened read-only)
    /// are refused. For pairing with an agent.
    #[arg(long)]
    read_only: bool,

    /// The page's files: a `web/dist` built by `bun run build`, in place of
    /// the ones built into this binary. May be repeated: the first that has a
    /// file serves it.
    #[arg(long, value_name = "DIR")]
    assets: Vec<PathBuf>,
}

/// What `ursa serve` serves: its root directory, and the database file in
/// it, if any.
#[derive(Debug, PartialEq, Eq)]
struct Served {
    root: PathBuf,
    database: Option<PathBuf>,
}

/// Run `ursa serve`, given the arguments from `serve` on.
pub fn main(args: impl IntoIterator<Item = std::ffi::OsString>) -> ExitCode {
    let args = ServeArgs::parse_from(args);
    let assets = match assets(args.assets) {
        Ok(assets) => assets,
        Err(e) => return fail(&e),
    };
    let served = match served(args.path.as_deref(), args.read_only) {
        Ok(served) => served,
        Err(e) => return fail(&e),
    };
    let what = match &served.database {
        Some(database) => format!("the database {}", database.display()),
        None => served.root.display().to_string(),
    };
    let config = ursa_server::Config {
        root: served.root,
        database: served.database,
        read_only: args.read_only,
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
        Err(e) => return fail(&format!("cannot start: {e}")),
    };
    let result = runtime.block_on(async {
        let bound = ursa_server::bind(config).await?;
        if args.port != 0 && bound.port() != args.port {
            println!("Port {} is taken; using {}.", args.port, bound.port());
        }
        let read_only = if args.read_only { ", read-only," } else { "" };
        println!("Ursa is serving {what}{read_only} at");
        println!("  {}", bound.url());
        if args.no_open {
            println!("Open that link in a browser on this machine. Ctrl+C stops the server.");
        } else {
            println!("Opening it in your browser. Ctrl+C stops the server.");
            open_browser(&bound.url());
        }
        bound
            .serve_with_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                println!("Stopping.");
            })
            .await
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e.to_string()),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("Error: {message}");
    ExitCode::FAILURE
}

/// The root and database `path` names: a database file (one that ends in
/// `.ursa`) in its directory, or a directory; the current directory if none.
/// A read-only server creates no database.
fn served(path: Option<&Path>, read_only: bool) -> Result<Served, String> {
    let Some(path) = path else {
        let root = std::env::current_dir()
            .map_err(|e| format!("cannot read the current directory: {e}"))?;
        return Ok(Served {
            root,
            database: None,
        });
    };
    let absolute =
        std::path::absolute(path).map_err(|e| format!("cannot serve {}: {e}", path.display()))?;
    if absolute.is_dir() {
        return Ok(Served {
            root: absolute,
            database: None,
        });
    }
    let is_database = path.extension().is_some_and(|e| e == "ursa");
    if !is_database {
        return Err(if absolute.exists() {
            format!(
                "{} is not a database file (those end in .ursa) or a directory",
                path.display()
            )
        } else {
            format!("{}: no such directory or database file", path.display())
        });
    }
    if read_only && !absolute.exists() {
        return Err(format!(
            "{}: no such database file, and --read-only creates none",
            path.display()
        ));
    }
    let root = absolute
        .parent()
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
    if !root.is_dir() {
        return Err(format!(
            "cannot create the database {}: the directory {} does not exist",
            path.display(),
            root.display()
        ));
    }
    Ok(Served {
        root,
        database: Some(absolute),
    })
}

/// Open `url` in the default browser, without waiting for it. If that
/// fails, say so: the link is printed above.
fn open_browser(url: &str) {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        // `start`'s first quoted argument is the window's title.
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    } else {
        Command::new("xdg-open")
    };
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let program = command.get_program().to_string_lossy().into_owned();
    match command.spawn() {
        // Reaped on a thread of its own, so it leaves no zombie behind.
        Ok(mut child) => {
            std::thread::spawn(move || {
                if !child.wait().is_ok_and(|status| status.success()) {
                    println!(
                        "Could not open a browser ({program} failed): open the link yourself."
                    );
                }
            });
        }
        Err(e) => println!("Could not open a browser ({program}: {e}): open the link yourself."),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_database_file_or_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            served(Some(root), false).unwrap(),
            Served {
                root: root.to_path_buf(),
                database: None
            }
        );
        let club = root.join("club.ursa");
        assert_eq!(
            served(Some(&club), false).unwrap(),
            Served {
                root: root.to_path_buf(),
                database: Some(club.clone())
            }
        );
        // Read-only, it must exist.
        let e = served(Some(&club), true).unwrap_err();
        assert!(e.contains("--read-only creates none"), "{e}");
        std::fs::write(root.join("notes.txt"), "").unwrap();
        let e = served(Some(&root.join("notes.txt")), false).unwrap_err();
        assert!(e.contains("not a database file"), "{e}");
        let e = served(Some(&root.join("nowhere")), false).unwrap_err();
        assert!(e.contains("no such directory"), "{e}");
        let e = served(Some(&root.join("nowhere/club.ursa")), false).unwrap_err();
        assert!(e.contains("does not exist"), "{e}");
        // No argument: the current directory.
        let here = served(None, false).unwrap();
        assert_eq!(here.root, std::env::current_dir().unwrap());
        assert_eq!(here.database, None);
    }
}
