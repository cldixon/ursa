//! What `ursa serve DIR`, `ursa serve club.ursa` and `--read-only` give a
//! session (`design/console.md` §5): its root, its database, and the writes
//! a read-only one refuses.

mod common;

use std::path::Path;

use common::{call, feed, Server, Socket};
use serde_json::json;

/// A session of its own (the shell's), started as the page starts it.
async fn shell(server: &Server) -> Socket {
    let (mut socket, _) = server.session(Some("new")).await;
    let (started, _) = call(&mut socket, 1, "start", json!({}), &[]).await;
    assert_eq!(started["ok"], true, "{started}");
    socket
}

/// The one row and column `sql` gives, as the shell prints it in CSV.
async fn value(socket: &mut Socket, sql: &str) -> String {
    let (_, err) = feed(socket, 2, ".mode csv").await;
    assert_eq!(err, "");
    let (out, err) = feed(socket, 3, sql).await;
    assert_eq!(err, "", "{sql}");
    out.lines().nth(1).unwrap_or_default().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_roots_the_sessions() {
    let server = Server::start().await;
    let root = server.dir.path();
    // Relative paths are the root's, though the process's directory is not.
    assert_ne!(std::env::current_dir().unwrap(), root);
    let (mut shared, hello) = server.session(None).await;
    assert_eq!(
        hello["result"]["session"]["cwd"],
        root.display().to_string(),
        "{hello}"
    );
    assert_eq!(
        value(&mut shared, "SELECT count(*) FROM 'ties.csv';").await,
        "3"
    );
    assert_eq!(
        value(&mut shared, "SELECT count(*) FROM 'sub/n.csv';").await,
        "1"
    );
    // The shared session (the console's, agents') copies to disk, under the
    // root.
    let (_, err) = feed(&mut shared, 4, "COPY (SELECT 1 AS a) TO 'out.csv';").await;
    assert_eq!(err, "");
    assert!(root.join("out.csv").is_file());
    let (_, err) = feed(&mut shared, 5, "COPY (SELECT 1 AS a) TO '../away.csv';").await;
    assert!(err.contains("outside this session's root"), "{err}");

    // The shell's own session reads the root too, and keeps its copies in
    // memory, as the browser demo does.
    let mut page = shell(&server).await;
    assert_eq!(
        value(&mut page, "SELECT count(*) FROM 'ties.csv';").await,
        "3"
    );
    let (_, err) = feed(&mut page, 4, "COPY (SELECT 2 AS b) TO 'kept.csv';").await;
    assert_eq!(err, "");
    assert!(!root.join("kept.csv").exists());
    let (files, _) = call(&mut page, 5, "files", json!({}), &[]).await;
    assert_eq!(files["result"]["files"][0]["name"], "kept.csv", "{files}");
    assert_eq!(value(&mut page, "SELECT b FROM 'kept.csv';").await, "2");
}

/// Define a view over the root's `ties.csv` in `club.ursa`, with a server of
/// its own, as `ursa serve club.ursa` would.
async fn make_club(dir: &Path) {
    std::fs::write(dir.join("ties.csv"), "src,dst\n1,2\n2,3\n3,1\n").unwrap();
    let club = dir.join("club.ursa");
    let mut server = Server::start_with(|_, config| {
        config.root = dir.to_path_buf();
        config.database = Some(club.clone());
    })
    .await;
    let mut page = shell(&server).await;
    let (_, err) = feed(
        &mut page,
        2,
        "CREATE VIEW ties AS SELECT * FROM 'ties.csv';",
    )
    .await;
    assert_eq!(err, "");
    // Stopped, its sessions close the database.
    drop(page);
    server.stop();
    server.served.take().unwrap().await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_database_file_is_attached_in_every_session() {
    let dir = tempfile::tempdir().unwrap();
    make_club(dir.path()).await;
    let club = dir.path().join("club.ursa");
    assert!(club.is_file());
    let server = Server::start_with(|_, config| {
        config.root = dir.path().to_path_buf();
        config.database = Some(club.clone());
    })
    .await;
    // The shell's session opens it as the CLI does: its views are there,
    // and the prompt names it.
    let mut page = shell(&server).await;
    let (fed, _) = call(&mut page, 2, "feed", json!({"line": "SELECT 1;"}), &[]).await;
    assert!(
        fed["result"]["prompt"]
            .as_str()
            .unwrap()
            .starts_with("club"),
        "{fed}"
    );
    assert_eq!(value(&mut page, "SELECT count(*) FROM ties;").await, "3");
    let (_, err) = feed(&mut page, 4, "CREATE VIEW pairs AS SELECT src FROM ties;").await;
    assert_eq!(err, "");
    // `start` (the page's New) opens it again, with what was defined.
    call(&mut page, 5, "start", json!({}), &[]).await;
    assert_eq!(value(&mut page, "SELECT count(*) FROM pairs;").await, "3");

    // Another session meanwhile has it read-only, and says so.
    let mut other = shell(&server).await;
    let (_, err) = feed(&mut other, 2, "SELECT 1;").await;
    assert!(err.contains("has it read-only"), "{err}");
    assert_eq!(value(&mut other, "SELECT count(*) FROM pairs;").await, "3");
    let (_, err) = feed(&mut other, 4, "CREATE VIEW more AS SELECT 1;").await;
    assert!(err.contains("is open read-only"), "{err}");

    // Once the first has closed, a new session has it for writing.
    drop(page);
    drop(other);
    let mut next = shell(&server).await;
    let (_, err) = feed(&mut next, 2, "CREATE VIEW more AS SELECT 1 AS one;").await;
    assert_eq!(err, "");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_server_refuses_every_write() {
    let dir = tempfile::tempdir().unwrap();
    make_club(dir.path()).await;
    let root = dir.path().to_path_buf();
    let club = root.join("club.ursa");
    let catalog = std::fs::read(&club).unwrap();
    let server = Server::start_with(|_, config| {
        config.root = root.clone();
        config.database = Some(club.clone());
        config.read_only = true;
    })
    .await;
    let (mut shared, _) = server.session(None).await;
    let mut page = shell(&server).await;
    for socket in [&mut shared, &mut page] {
        // It reads the database.
        assert_eq!(value(socket, "SELECT count(*) FROM ties;").await, "3");
        let refusals = [
            (
                "COPY (SELECT 1 AS a) TO 'x.csv';",
                "COPY … TO 'x.csv' cannot be written",
            ),
            (
                "COPY (SELECT 1 AS a) TO 'memory://uploads/x.csv';",
                "cannot be written",
            ),
            (
                "EXPLAIN COPY (SELECT 1 AS a) TO 'x.csv';",
                "cannot be written",
            ),
            ("CREATE VIEW v AS SELECT 1;", "is open read-only"),
            ("CREATE TABLE t AS SELECT 1 AS a;", "is open read-only"),
            ("DROP VIEW ties;", "is open read-only"),
            (".output out.txt", "cannot write to 'out.txt'"),
            (".once out.txt", "cannot write to 'out.txt'"),
        ];
        for (line, says) in refusals {
            let (_, err) = feed(socket, 4, line).await;
            assert!(err.contains(says), "{line}: {err}");
            if line.starts_with("COPY") || line.starts_with('.') {
                assert!(err.contains("read-only"), "{line}: {err}");
            }
        }
        assert_eq!(value(socket, "SELECT count(*) FROM ties;").await, "3");
    }
    for name in ["x.csv", "out.txt"] {
        assert!(!root.join(name).exists(), "{name} was written");
    }
    let (files, _) = call(&mut page, 5, "files", json!({}), &[]).await;
    assert_eq!(files["result"]["files"], json!([]), "{files}");
    // The database is as it was.
    assert_eq!(std::fs::read(&club).unwrap(), catalog);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_session_writes_no_files_but_works_in_memory() {
    let server = Server::start_with(|_, config| config.read_only = true).await;
    let root = server.dir.path();
    std::fs::create_dir(root.join("ext")).unwrap();
    let mut page = shell(&server).await;
    // What it defines in memory is its own.
    for line in [
        "CREATE VIEW ties AS SELECT * FROM 'ties.csv';",
        "CREATE TABLE m (a INT);",
        "INSERT INTO m VALUES (1), (2);",
        "CREATE EXTERNAL TABLE ext (a INT) STORED AS CSV LOCATION 'ext/';",
    ] {
        let (_, err) = feed(&mut page, 2, line).await;
        assert_eq!(err, "", "{line}");
    }
    assert_eq!(value(&mut page, "SELECT count(*) FROM m;").await, "2");
    // A table of files is not written to.
    let (_, err) = feed(&mut page, 4, "INSERT INTO ext VALUES (1);").await;
    assert!(err.contains("INSERT INTO ext cannot change it"), "{err}");
    assert!(err.contains("read-only"), "{err}");
    assert_eq!(std::fs::read_dir(root.join("ext")).unwrap().count(), 0);
    // ATTACH creates no database, and opens one that exists read-only.
    let (_, err) = feed(&mut page, 5, "ATTACH 'new.ursa';").await;
    assert!(
        err.contains("cannot ATTACH 'new.ursa': it does not exist"),
        "{err}"
    );
    assert!(err.contains("read-only"), "{err}");
    assert!(!root.join("new.ursa").exists());
    assert!(!root.join("new.ursa.d").exists());
    let (_, err) = feed(&mut page, 6, ".open other.ursa").await;
    assert!(err.contains("does not exist"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_in_a_read_only_session_opens_read_only() {
    let dir = tempfile::tempdir().unwrap();
    make_club(dir.path()).await;
    let server = Server::start_with(|_, config| {
        config.root = dir.path().to_path_buf();
        config.read_only = true;
    })
    .await;
    let mut page = shell(&server).await;
    let (_, err) = feed(&mut page, 2, "ATTACH 'club.ursa';").await;
    assert_eq!(err, "");
    assert_eq!(value(&mut page, "SELECT count(*) FROM ties;").await, "3");
    let (_, err) = feed(&mut page, 4, "DROP VIEW ties;").await;
    assert!(err.contains("is open read-only"), "{err}");
}
