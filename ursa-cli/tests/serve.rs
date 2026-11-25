//! `ursa serve` (the `serve` feature): it prints its link, serves the page
//! from `--assets`, and stops on Ctrl+C; it roots its sessions in a
//! directory, or opens a database file. ursa-server's own tests cover the
//! server, and its sessions' roots, databases and `--read-only`
//! (`ursa-server/tests/roots.rs`).

#![cfg(feature = "serve")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn serve_prints_its_link_serves_the_page_and_stops_on_ctrl_c() {
    let dir = tempfile::tempdir().unwrap();
    let assets = dir.path().join("dist");
    std::fs::create_dir(&assets).unwrap();
    std::fs::write(
        assets.join("index.html"),
        "<!doctype html><html><head><title>t</title></head></html>",
    )
    .unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ursa"))
        .arg("serve")
        .arg(&data)
        .args(["--port", "0", "--no-open", "--assets"])
        .arg(&assets)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    assert!(first.starts_with("Ursa is serving "), "{first}");
    let link = lines.next().unwrap().unwrap();
    let link = link.trim();
    let rest = link.strip_prefix("http://127.0.0.1:").expect(link);
    let (port, token) = rest.split_once("/?token=").expect(link);
    assert_eq!(token.len(), 43, "{link}");
    // The page, marked for the socket.
    let mut stream = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut page = String::new();
    stream.read_to_string(&mut page).unwrap();
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains("content=\"socket\""), "{page}");
    // Ctrl+C: it stops, gracefully.
    #[cfg(unix)]
    {
        let killed = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(start.elapsed() < Duration::from_secs(20), "still serving");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(status.success(), "{status}");
        let said: Vec<String> = lines.map_while(Result::ok).collect();
        assert!(said.iter().any(|l| l == "Stopping."), "{said:?}");
    }
    #[cfg(not(unix))]
    {
        child.kill().unwrap();
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
