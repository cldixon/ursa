//! `ursa serve` (the `serve` feature): it prints its link (the console's),
//! serves the pages from `--assets`, and stops on Ctrl+C. ursa-server's own
//! tests cover the server.

#![cfg(feature = "serve")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
    let mut child = Command::new(env!("CARGO_BIN_EXE_ursa"))
        .arg("serve")
        .arg(&data)
        .args(["--port", "0", "--assets"])
        .arg(&console)
        .arg("--assets")
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
    // The link opens the console; the shell is at /.
    let (port, token) = rest.split_once("/console/?token=").expect(link);
    assert_eq!(token.len(), 43, "{link}");
    let _open = lines.next().unwrap().unwrap();
    let shell = lines.next().unwrap().unwrap();
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
        .arg("--assets")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("no index.html there"), "{err}");
}
