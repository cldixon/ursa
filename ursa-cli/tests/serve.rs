//! `ursa serve` (the `serve` feature): it prints its link (the console's),
//! serves the pages from `--assets`, and stops on Ctrl+C; it roots its
//! sessions in a directory, or opens a database file. ursa-server's own
//! tests cover the server, and its sessions' roots, databases and
//! `--read-only` (`ursa-server/tests/roots.rs`).

#![cfg(feature = "serve")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// `ursa serve`, running: what it prints on stdout, line by line, and its
/// stderr, read on a thread of its own, so that a failure always shows it
/// (a panic's message is there).
struct Serving {
    child: Child,
    lines: std::io::Lines<BufReader<ChildStdout>>,
    said: Vec<String>,
    stderr: Option<JoinHandle<String>>,
}

/// How `ursa serve` ended.
struct Stopped {
    status: ExitStatus,
    stdout: Vec<String>,
    stderr: String,
}

impl Serving {
    fn start(command: &mut Command) -> Serving {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut err = child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = err.read_to_string(&mut text);
            text
        });
        Serving {
            lines: BufReader::new(child.stdout.take().unwrap()).lines(),
            child,
            said: Vec::new(),
            stderr: Some(stderr),
        }
    }

    /// `ursa serve DIR --port 0 --no-open --assets ASSETS`.
    fn dir(dir: &std::path::Path, assets: &std::path::Path) -> Serving {
        Serving::start(
            Command::new(env!("CARGO_BIN_EXE_ursa"))
                .arg("serve")
                .arg(dir)
                .args(["--port", "0", "--no-open", "--assets"])
                .arg(assets),
        )
    }

    /// The next line on stdout.
    fn line(&mut self) -> String {
        match self.lines.next() {
            Some(Ok(line)) => {
                self.said.push(line.clone());
                line
            }
            _ => {
                let stopped = self.wait();
                panic!("ursa serve said no more\n{}", stopped.report());
            }
        }
    }

    /// Read up to the link: its port and token.
    fn link(&mut self) -> (u16, String) {
        loop {
            let line = self.line();
            if let Some((_, rest)) = line.trim().split_once("http://127.0.0.1:") {
                let (port, token) = rest.split_once("?token=").unwrap();
                let port = port.trim_end_matches('/').trim_end_matches("/console");
                return (port.parse().unwrap(), token.to_string());
            }
        }
    }

    /// Ctrl+C: SIGINT, and wait for it to end.
    #[cfg(unix)]
    fn interrupt(&mut self) -> Stopped {
        let killed = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        self.wait()
    }

    /// Wait (20 s at most) for it to end, and what it said.
    fn wait(&mut self) -> Stopped {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break Some(status);
            }
            if start.elapsed() > Duration::from_secs(20) {
                let _ = self.child.kill();
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut stdout = std::mem::take(&mut self.said);
        stdout.extend(self.lines.by_ref().map_while(Result::ok));
        let stderr = self
            .stderr
            .take()
            .map(|t| t.join().unwrap())
            .unwrap_or_default();
        let Some(status) = status else {
            panic!("still serving after 20 s\nstdout: {stdout:?}\nstderr:\n{stderr}");
        };
        Stopped {
            status,
            stdout,
            stderr,
        }
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Stopped {
    fn report(&self) -> String {
        format!(
            "{}\nstdout: {:?}\nstderr:\n{}",
            self.status, self.stdout, self.stderr
        )
    }

    /// Stopped by Ctrl+C, gracefully: "Stopping.", exit 0, no panic.
    fn assert_clean(&self) {
        assert!(self.status.success(), "{}", self.report());
        assert!(
            self.stdout.iter().any(|l| l == "Stopping."),
            "{}",
            self.report()
        );
        assert!(!self.stderr.contains("panicked"), "{}", self.report());
    }
}

#[test]
fn serve_prints_its_link_serves_the_pages_and_stops_on_ctrl_c() {
    let dir = tempfile::tempdir().unwrap();
    let assets = dir.path().join("dist");
    std::fs::create_dir(&assets).unwrap();
    std::fs::write(
        assets.join("index.html"),
        "<!doctype html><html><head><title>t</title></head></html>",
    )
    .unwrap();
    // The console's files, in a directory of their own (web/dist-console).
    let console = dir.path().join("dist-console");
    std::fs::create_dir_all(console.join("console")).unwrap();
    std::fs::write(
        console.join("console").join("index.html"),
        "<!doctype html><html><head><title>c</title></head></html>",
    )
    .unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let mut serving = Serving::start(
        Command::new(env!("CARGO_BIN_EXE_ursa"))
            .arg("serve")
            .arg(&data)
            .args(["--port", "0", "--no-open", "--assets"])
            .arg(&console)
            .arg("--assets")
            .arg(&assets),
    );
    let first = serving.line();
    assert!(first.starts_with("Ursa is serving "), "{first}");
    let link = serving.line();
    let link = link.trim();
    let rest = link.strip_prefix("http://127.0.0.1:").expect(link);
    // The link opens the console; the shell is at /.
    let (port, token) = rest.split_once("/console/?token=").expect(link);
    assert_eq!(token.len(), 43, "{link}");
    let _open = serving.line();
    let shell = serving.line();
    assert_eq!(
        shell,
        format!("The Web Shell is at http://127.0.0.1:{port}/ once the link has opened.")
    );
    // The pages, marked for the socket.
    for (path, title) in [("/console/", "c"), ("/", "t")] {
        let mut stream = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut page = String::new();
        stream.read_to_string(&mut page).unwrap();
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        assert!(page.contains("content=\"socket\""), "{page}");
        assert!(page.contains(&format!("<title>{title}</title>")), "{page}");
    }
    // Ctrl+C: it stops, gracefully.
    #[cfg(unix)]
    serving.interrupt().assert_clean();
}

/// How many times the Ctrl+C tests below stop a server: the race they
/// guard against (URSA-22) lost about one run in a few hundred.
#[cfg(unix)]
const TIMES: usize = 50;

/// Ctrl+C as soon as the link is printed, again and again: it stops
/// cleanly every time (URSA-22: a listener that stopped between two polls
/// of the shutdown's `select!` was awaited again, and panicked).
#[cfg(unix)]
#[test]
fn ctrl_c_right_after_the_link_stops_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let assets = page(dir.path());
    for _ in 0..TIMES {
        let mut serving = Serving::dir(dir.path(), &assets);
        serving.link();
        serving.interrupt().assert_clean();
    }
}

/// Ctrl+C while the server starts, at varied moments up to its link: never
/// a panic. One that comes before the handler is installed (in the first
/// milliseconds) ends the process as SIGINT does; any later one stops it
/// cleanly, once it is listening.
#[cfg(unix)]
#[test]
fn ctrl_c_while_starting_never_panics() {
    use std::os::unix::process::ExitStatusExt;
    let dir = tempfile::tempdir().unwrap();
    let assets = page(dir.path());
    for i in 0..TIMES {
        let mut serving = Serving::dir(dir.path(), &assets);
        std::thread::sleep(Duration::from_micros(400 * i as u64));
        let stopped = serving.interrupt();
        if stopped.status.signal() == Some(2) {
            assert!(!stopped.stderr.contains("panicked"), "{}", stopped.report());
        } else {
            stopped.assert_clean();
        }
    }
}

/// A statement that runs until it is cancelled: a sum over 10^11 rows.
#[cfg(unix)]
const FOREVER: &str = "SELECT sum(value) AS s FROM range(0, 100000000000);";

/// Ctrl+C while a request runs (or is starting), again and again: the
/// request is cancelled, the page's socket closed (1001), and the server
/// stops cleanly.
#[cfg(unix)]
#[test]
fn ctrl_c_during_a_running_request_stops_cleanly() {
    use tungstenite::client::IntoClientRequest;
    use tungstenite::http::HeaderValue;
    use tungstenite::Message;
    let dir = tempfile::tempdir().unwrap();
    let assets = page(dir.path());
    for i in 0..TIMES {
        let mut serving = Serving::dir(dir.path(), &assets);
        let (port, token) = serving.link();
        let mut request = format!("ws://127.0.0.1:{port}/session?token={token}")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Origin",
            HeaderValue::from_str(&format!("http://127.0.0.1:{port}")).unwrap(),
        );
        let (mut socket, _) = tungstenite::connect(request).unwrap();
        let send = |socket: &mut tungstenite::WebSocket<_>, id: u32, op: &str, args: &str| {
            let text = format!(r#"{{"v": 1, "id": {id}, "op": "{op}", "args": {args}}}"#);
            socket.send(Message::text(text)).unwrap();
        };
        send(
            &mut socket,
            0,
            "hello",
            r#"{"protocol": 1, "session": "new"}"#,
        );
        let hello = socket.read().unwrap();
        assert!(hello.to_text().unwrap().contains(r#""ok":true"#), "{hello}");
        let line = serde_json::json!({ "line": FOREVER }).to_string();
        send(&mut socket, 1, "feed", &line);
        // Starting, or running.
        std::thread::sleep(Duration::from_millis((i % 10) as u64 * 3));
        let stopped = serving.interrupt();
        stopped.assert_clean();
        // The page heard why.
        let closed = loop {
            match socket.read() {
                Ok(Message::Close(frame)) => break frame.map(|f| u16::from(f.code)),
                Ok(_) => continue,
                Err(e) => panic!("not closed: {e}\n{}", stopped.report()),
            }
        };
        assert_eq!(closed, Some(1001), "{}", stopped.report());
    }
}

#[test]
fn serve_needs_a_page_to_serve() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ursa"))
        .arg("serve")
        .arg("--no-open")
        .arg("--assets")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("no index.html there"), "{err}");
}

/// Start `ursa serve` with `args` (and `--no-open --port 0`, a page from
/// `assets`): what it prints up to its link, and the server.
fn serve(
    args: &[&std::ffi::OsStr],
    assets: &std::path::Path,
) -> (Vec<String>, std::process::Child) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ursa"))
        .arg("serve")
        .args(args)
        .args(["--port", "0", "--no-open", "--assets"])
        .arg(assets)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut said = Vec::new();
    for line in lines.by_ref() {
        let line = line.unwrap();
        let link = line.contains("?token=");
        said.push(line);
        if link {
            break;
        }
    }
    (said, child)
}

fn page(dir: &std::path::Path) -> std::path::PathBuf {
    let assets = dir.join("dist");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("index.html"), "<!doctype html>").unwrap();
    assets
}

#[test]
fn serve_opens_a_database_file_in_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let assets = page(dir.path());
    let club = dir.path().join("club.ursa");
    let (said, mut child) = serve(&[club.as_os_str()], &assets);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        said[0],
        format!("Ursa is serving the database {} at", club.display()),
        "{said:?}"
    );
    // Read-only, it must exist: `ursa serve` creates no database then.
    let output = Command::new(env!("CARGO_BIN_EXE_ursa"))
        .args(["serve", "--read-only", "--no-open"])
        .arg(dir.path().join("none.ursa"))
        .arg("--assets")
        .arg(&assets)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("--read-only creates none"), "{err}");
    // A file that is no database, and a directory that is not there.
    let notes = dir.path().join("notes.txt");
    std::fs::write(&notes, "").unwrap();
    for (path, says) in [
        (
            notes,
            "is not a database file (those end in .ursa) or a directory",
        ),
        (
            dir.path().join("nowhere"),
            "no such directory or database file",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ursa"))
            .args(["serve", "--no-open"])
            .arg(&path)
            .arg("--assets")
            .arg(&assets)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains(says), "{err}");
    }
}

#[test]
fn serve_roots_its_sessions_in_a_directory_and_says_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let assets = page(dir.path());
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let read_only = std::ffi::OsStr::new("--read-only");
    let (said, mut child) = serve(&[data.as_os_str(), read_only], &assets);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        said[0],
        format!("Ursa is serving {}, read-only, at", data.display()),
        "{said:?}"
    );
    // No argument: the current directory.
    let mut child = Command::new(env!("CARGO_BIN_EXE_ursa"))
        .args(["serve", "--port", "0", "--no-open", "--assets"])
        .arg(&assets)
        .current_dir(&data)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let first = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .next()
        .unwrap()
        .unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let here = data.canonicalize().unwrap();
    assert!(
        first == format!("Ursa is serving {} at", here.display())
            || first == format!("Ursa is serving {} at", data.display()),
        "{first}"
    );
}
