//! `ursa serve [DATABASE | DIR]`: the Ursa Console in a browser tab (and the
//! Web Shell beside it), over a WebSocket to this process's engine
//! (`ursa-server`, design/console.md §5).
//!
//! ```text
//! ursa serve                     # in memory, rooted in the current directory
//! ursa serve web/.data           # rooted in a directory
//! ursa serve club.ursa           # a database file, rooted in its directory
//! ursa serve club.ursa --read-only --no-open --port 4281
//! ```
//!
//! It prints the URL to open, with the run's token, opens it in the default
//! browser (unless `--no-open`) by way of a redirect file only the user can
//! read, so that the token is on no command line (URSA-20), and serves until Ctrl+C, which stops it
//! gracefully: running requests are cancelled and the pages' sessions
//! closed.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

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

    /// The page's files, in place of the ones built into this binary:
    /// `web/dist-console` (the console) and `web/dist` (the shell), built by
    /// `bun run build`. May be repeated: the first that has a file serves it.
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
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => return fail(&format!("cannot start: {e}")),
    };
    // Ctrl+C stops the server from here on: its handler is installed first,
    // so one that comes while the server starts is not lost (it stops the
    // server as soon as it is listening), nor does it end the process by
    // SIGINT's default (URSA-22). Only one in the process's first
    // milliseconds, before main, still does.
    let interrupted = {
        let _runtime = runtime.enter();
        match interrupted() {
            Ok(interrupted) => interrupted,
            Err(e) => return fail(&format!("cannot handle Ctrl+C: {e}")),
        }
    };
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
    let result = runtime.block_on(async {
        let bound = ursa_server::bind(config).await?;
        if args.port != 0 && bound.port() != args.port {
            say(&format!(
                "Port {} is taken; using {}.",
                args.port,
                bound.port()
            ));
        }
        let read_only = if args.read_only { ", read-only," } else { "" };
        say(&format!("Ursa is serving {what}{read_only} at"));
        say(&format!("  {}", bound.url()));
        if args.no_open {
            say("Open that link in a browser on this machine. Ctrl+C stops the server.");
        } else {
            say("Opening it in your browser. Ctrl+C stops the server.");
        }
        if bound.has_console() {
            say(&format!(
                "The Web Shell is at http://127.0.0.1:{}/ once the link has opened.",
                bound.port()
            ));
        }
        // Kept until the server stops, which removes it.
        let _opener = (!args.no_open).then(|| open_browser(&bound.url()));
        bound
            .serve_with_shutdown(async {
                interrupted.await;
                say("Stopping.");
            })
            .await
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e.to_string()),
    }
}

/// Once Ctrl+C (SIGINT) comes, or came after this was called: the handler
/// is installed now, not when the future is first polled.
fn interrupted() -> std::io::Result<impl std::future::Future<Output = ()> + Send + 'static> {
    #[cfg(unix)]
    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    #[cfg(windows)]
    let mut signal = tokio::signal::windows::ctrl_c()?;
    Ok(async move {
        signal.recv().await;
    })
}

/// Print `line` on stdout. A stdout that is closed (`ursa serve | head -2`)
/// is no reason to stop serving, or to panic.
fn say(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stdout().lock(), "{line}");
}

fn fail(message: &str) -> ExitCode {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "Error: {message}");
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

/// The page `ursa serve` opens in the browser, in place of its link
/// (URSA-20): a file that redirects to the link, written where only the
/// user can read it, in a directory of its own that goes when this does.
///
/// A command's arguments are visible to every user of the machine (`ps`)
/// while it runs, and `open`, `xdg-open` or `start` may run for as long as
/// the browser it starts. The token is the server's only credential, so it
/// is never on a command line: the browser is given this file's path, and
/// reads the link from the file. (Jupyter opens its server the same way.)
struct Opener {
    /// The redirect file, `open.html` in `dir`.
    page: PathBuf,
    /// Removed, with the file, when this is dropped: when the server stops.
    _dir: tempfile::TempDir,
}

impl Opener {
    /// Write the page that redirects to `url`: in a new directory in the
    /// temporary directory (on Unix, mode 0700, and the file 0600; on
    /// Windows, `%TEMP%` is the user's own, under their profile, and the
    /// directory and file inherit its permissions).
    fn new(url: &str) -> std::io::Result<Opener> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("ursa-serve-");
        // tempfile makes a directory as `mkdir` does, readable by all
        // (0755 under the usual umask), unless told.
        #[cfg(unix)]
        builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let dir = builder.tempdir()?;
        let page = dir.path().join("open.html");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&page)?;
        std::io::Write::write_all(&mut file, redirect_page(url).as_bytes())?;
        file.sync_all()?;
        Ok(Opener { page, _dir: dir })
    }
}

/// A page that sends the browser on to `url` at once (a `<meta>` refresh,
/// which needs no script), with a link to it should it not.
fn redirect_page(url: &str) -> String {
    let url = url
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<!doctype html>\n\
         <html>\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta http-equiv=\"refresh\" content=\"0;url={url}\">\n\
         <title>Ursa</title>\n\
         </head>\n\
         <body>\n\
         <p>Opening Ursa. If nothing happens, <a href=\"{url}\">open it here</a>.</p>\n\
         </body>\n\
         </html>\n"
    )
}

/// The command that opens `target` (a file, or a URL) in the default
/// browser, without starting it: `open` on macOS, `start` on Windows,
/// `xdg-open` elsewhere. Every argument is visible to every user of the
/// machine while it runs, so `target` must hold no secret.
fn open_command(target: &Path) -> Command {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        // `start`'s first quoted argument is the window's title. Given a
        // file, it opens it as Explorer would: an .html file in the default
        // browser.
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    } else {
        Command::new("xdg-open")
    };
    command
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Open `url` in the default browser, without waiting for it, by way of a
/// redirect file ([`Opener`]), which it returns: the file must last until
/// the server stops. If that fails, say so: the link is printed above.
fn open_browser(url: &str) -> Option<Opener> {
    let opener = match Opener::new(url) {
        Ok(opener) => opener,
        Err(e) => {
            say(&format!(
                "Could not open a browser (cannot write the page that opens it: {e}): \
                 open the link yourself."
            ));
            return None;
        }
    };
    let mut command = open_command(&opener.page);
    let program = command.get_program().to_string_lossy().into_owned();
    match command.spawn() {
        // Reaped on a thread of its own, so it leaves no zombie behind.
        Ok(mut child) => {
            std::thread::spawn(move || {
                if !child.wait().is_ok_and(|status| status.success()) {
                    say(&format!(
                        "Could not open a browser ({program} failed): open the link yourself."
                    ));
                }
            });
        }
        Err(e) => say(&format!(
            "Could not open a browser ({program}: {e}): open the link yourself."
        )),
    }
    Some(opener)
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

    const URL: &str = "http://127.0.0.1:4280/console/?token=abcDEF123-_xyz";

    /// The browser is given the redirect file's path, never the link: the
    /// token is on no command line (URSA-20).
    #[test]
    fn the_command_that_opens_the_browser_has_no_token() {
        let opener = Opener::new(URL).unwrap();
        let command = open_command(&opener.page);
        let mut argv = vec![command.get_program().to_os_string()];
        argv.extend(command.get_args().map(std::ffi::OsStr::to_os_string));
        for arg in &argv {
            let arg = arg.to_string_lossy();
            assert!(!arg.contains("abcDEF123"), "{argv:?}");
            assert!(!arg.contains("token"), "{argv:?}");
        }
        assert_eq!(command.get_args().last(), Some(opener.page.as_os_str()));
        let program = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(windows) {
            "cmd"
        } else {
            "xdg-open"
        };
        assert_eq!(command.get_program(), program);
    }

    /// The file sends the browser on to the link; only the user can read
    /// it, and it goes when the server stops.
    #[test]
    fn the_redirect_file_is_the_users_own_and_goes_at_the_end() {
        let opener = Opener::new(URL).unwrap();
        let page = std::fs::read_to_string(&opener.page).unwrap();
        assert!(
            page.contains(&format!(
                "<meta http-equiv=\"refresh\" content=\"0;url={URL}\">"
            )),
            "{page}"
        );
        assert!(page.contains(&format!("<a href=\"{URL}\">")), "{page}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&opener.page), 0o600);
            assert_eq!(mode(opener.page.parent().unwrap()), 0o700);
        }
        let (page, dir) = (
            opener.page.clone(),
            opener.page.parent().unwrap().to_owned(),
        );
        drop(opener);
        assert!(!page.exists() && !dir.exists());
    }

    #[test]
    fn the_redirect_page_escapes_its_link() {
        let page = redirect_page("http://h/?a=1&b=\"<x>\"");
        assert!(
            page.contains("url=http://h/?a=1&amp;b=&quot;&lt;x&gt;&quot;\""),
            "{page}"
        );
    }
}
