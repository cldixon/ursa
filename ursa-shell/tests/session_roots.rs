//! A root per session (URSA-6): each native session reads relative paths
//! from its own directory, and writes `COPY … TO` under it, never outside.

use std::path::Path;

use serde_json::{json, Value};
use ursa_shell::session::{Options, Root, Session};

/// A session rooted at `dir` that writes its copies to disk, as `ursa serve`
/// makes one.
fn rooted(dir: &Path) -> Session {
    Session::new(Options {
        root: Some(Root::Dir(dir.to_path_buf())),
        copy_to_disk: true,
        ..Options::default()
    })
}

/// Feed `line`; its output and errors.
async fn feed(session: &mut Session, line: &str) -> (String, String) {
    let reply = session
        .handle("feed", json!({ "line": line }), Vec::new())
        .await
        .unwrap()
        .result;
    let text = |field: &str| reply[field].as_str().unwrap_or_default().to_string();
    (text("out"), text("err"))
}

/// Feed `line`, which must not fail; its output.
async fn ok(session: &mut Session, line: &str) -> String {
    let (out, err) = feed(session, line).await;
    assert_eq!(err, "", "{line}");
    out
}

/// Feed `line`, which must fail; its error.
async fn refused(session: &mut Session, line: &str) -> String {
    let (_, err) = feed(session, line).await;
    assert!(!err.is_empty(), "{line} should fail");
    err
}

fn edges(dir: &Path, rows: usize) {
    let mut csv = String::from("src,dst\n");
    for i in 0..rows {
        csv.push_str(&format!("{i},{}\n", i + 1));
    }
    std::fs::write(dir.join("edges.csv"), csv).unwrap();
}

#[tokio::test]
async fn two_sessions_read_their_own_roots() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    edges(a.path(), 3);
    edges(b.path(), 5);
    let (mut sa, mut sb) = (rooted(a.path()), rooted(b.path()));
    for (session, n) in [(&mut sa, "3"), (&mut sb, "5")] {
        ok(session, ".mode csv").await;
        let lines = [
            "SELECT count(*) AS n FROM 'edges.csv';",
            "SELECT count(*) AS n FROM './edges.csv';",
            "SELECT count(*) AS n FROM read_csv('edges.csv');",
            "SELECT count(*) AS n FROM '*.csv';",
        ];
        for line in lines {
            assert_eq!(ok(session, line).await, format!("n\n{n}\n"), "{line}");
        }
        ok(
            session,
            "CREATE EXTERNAL TABLE e STORED AS CSV LOCATION 'edges.csv' OPTIONS ('format.has_header' 'true');",
        )
        .await;
        assert_eq!(
            ok(session, "SELECT count(*) AS n FROM e;").await,
            format!("n\n{n}\n")
        );
        // A graph over the root's files.
        ok(
            session,
            "CREATE PROPERTY GRAPH g EDGE TABLES ('edges.csv' SOURCE KEY (src) DESTINATION KEY (dst));",
        )
        .await;
        let degree = ok(session, "SELECT count(*) AS n FROM degree(g);").await;
        assert_eq!(degree, format!("n\n{}\n", n.parse::<u32>().unwrap() + 1));
    }
    // `start` keeps the root.
    sa.handle("start", json!({}), Vec::new()).await.unwrap();
    ok(&mut sa, ".mode csv").await;
    assert_eq!(
        ok(&mut sa, "SELECT count(*) AS n FROM 'edges.csv';").await,
        "n\n3\n"
    );
}

#[tokio::test]
async fn copy_writes_under_the_root() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut sa, mut sb) = (rooted(a.path()), rooted(b.path()));
    ok(
        &mut sa,
        "COPY (SELECT 1 AS x UNION ALL SELECT 2) TO 'out.csv';",
    )
    .await;
    ok(&mut sa, "COPY (SELECT 7 AS y) TO 'sub/dir/y.parquet';").await;
    assert!(a.path().join("out.csv").is_file());
    assert!(a.path().join("sub/dir/y.parquet").is_file());
    assert!(!b.path().join("out.csv").exists());
    ok(&mut sa, ".mode csv").await;
    assert_eq!(
        ok(&mut sa, "SELECT sum(x) AS s FROM 'out.csv';").await,
        "s\n3\n"
    );
    assert_eq!(
        ok(&mut sa, "SELECT y FROM 'sub/dir/y.parquet';").await,
        "y\n7\n"
    );
    // The other session has no such file.
    refused(&mut sb, "SELECT * FROM 'out.csv';").await;
    // Not in the session's files either: those are for what a page adds.
    let files = sa
        .handle("files", json!({}), Vec::new())
        .await
        .unwrap()
        .result;
    assert_eq!(files["files"], json!([]));
    // EXPLAIN plans the copy under the root.
    let explained = ok(&mut sa, "EXPLAIN COPY (SELECT 1 AS x) TO 'e.csv';").await;
    assert!(explained.contains("e.csv"), "{explained}");
    assert!(!a.path().join("e.csv").exists());
}

#[tokio::test]
async fn copy_never_leaves_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let mut session = rooted(&root);
    let outside = dir.path().join("outside.csv");
    let file_url = url::Url::from_file_path(&outside).unwrap();
    let targets = [
        "../outside.csv".to_string(),
        "a/../../outside.csv".to_string(),
        outside.display().to_string(),
        file_url.to_string(),
        "~/outside.csv".to_string(),
        "memory://uploads/outside.csv".to_string(),
    ];
    for target in &targets {
        let err = refused(
            &mut session,
            &format!("COPY (SELECT 1 AS x) TO '{target}';"),
        )
        .await;
        assert!(
            err.contains("outside this session's root"),
            "{target}: {err}"
        );
        // Nor through EXPLAIN ANALYZE, which runs the copy.
        let err = refused(
            &mut session,
            &format!("EXPLAIN ANALYZE COPY (SELECT 1 AS x) TO '{target}';"),
        )
        .await;
        assert!(
            err.contains("outside this session's root"),
            "{target}: {err}"
        );
        // Nor through .output.
        let err = refused(&mut session, &format!(".output '{target}'")).await;
        assert!(
            err.contains("outside this session's root"),
            "{target}: {err}"
        );
    }
    assert!(!outside.exists());
    #[cfg(unix)]
    {
        let away = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(away.path(), root.join("away")).unwrap();
        let err = refused(&mut session, "COPY (SELECT 1 AS x) TO 'away/x.csv';").await;
        assert!(err.contains("symbolic link"), "{err}");
        assert!(!away.path().join("x.csv").exists());
    }
    // INSERT INTO a table of files: under the root, not outside it.
    std::fs::write(&outside, "x\n1\n").unwrap();
    std::fs::write(root.join("inside.csv"), "x\n1\n").unwrap();
    ok(
        &mut session,
        &format!(
            "CREATE EXTERNAL TABLE o (x BIGINT) STORED AS CSV LOCATION '{}' OPTIONS ('format.has_header' 'true');",
            outside.display()
        ),
    )
    .await;
    ok(
        &mut session,
        "CREATE EXTERNAL TABLE i (x BIGINT) STORED AS CSV LOCATION 'inside.csv' OPTIONS ('format.has_header' 'true');",
    )
    .await;
    let err = refused(&mut session, "INSERT INTO o VALUES (2);").await;
    assert!(err.contains("outside this session's root"), "{err}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "x\n1\n");
    // Reads are not confined: the native engine reads any file.
    ok(&mut session, ".mode csv").await;
    assert_eq!(
        ok(&mut session, "SELECT count(*) AS n FROM o;").await,
        "n\n1\n"
    );
}

#[tokio::test]
async fn output_and_read_use_the_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("script.sql"), "SELECT 41 + 1 AS answer;\n").unwrap();
    let mut session = rooted(dir.path());
    ok(&mut session, ".mode csv").await;
    ok(&mut session, ".once answer.csv").await;
    ok(&mut session, ".read script.sql").await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("answer.csv")).unwrap_or_default(),
        "answer\n42\n"
    );
}

#[tokio::test]
async fn a_rooted_session_can_keep_its_copies_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::new(Options {
        root: Some(Root::Dir(dir.path().to_path_buf())),
        ..Options::default()
    });
    ok(&mut session, "COPY (SELECT 1 AS x) TO 'kept.csv';").await;
    assert!(!dir.path().join("kept.csv").exists());
    let files: Value = session
        .handle("files", json!({}), Vec::new())
        .await
        .unwrap()
        .result;
    assert_eq!(files["files"][0]["name"], "kept.csv");
}
