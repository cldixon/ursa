//! The `ursa` binary, run as a user runs it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The workspace root, where the docs' data files live.
fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn karate_edges() -> String {
    workspace()
        .join("python/ursa/datasets/data/karate_edges.csv")
        .display()
        .to_string()
}

fn ursa(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ursa"))
        .args(args)
        .current_dir(workspace())
        .output()
        .expect("ursa runs")
}

fn ursa_with_stdin(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ursa"))
        .args(args)
        .current_dir(workspace())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ursa runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// stdout of a run that must succeed.
fn ok(output: Output) -> String {
    assert!(
        output.status.success(),
        "ursa failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// stderr of a run that must fail.
fn err(output: Output) -> String {
    assert!(
        !output.status.success(),
        "ursa should have failed; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stderr).unwrap()
}

/// Getting Started, "Query a file", run from the workspace root.
#[test]
fn query_a_file_matches_getting_started() {
    let out = ok(ursa(&[
        "-c",
        "SELECT * FROM 'python/ursa/datasets/data/karate_edges.csv' LIMIT 3",
    ]));
    assert_eq!(
        out,
        "\
+-----+-----+
| src | dst |
+-----+-----+
| 0   | 1   |
| 0   | 2   |
| 0   | 3   |
+-----+-----+
"
    );
}

#[test]
fn output_formats() {
    let sql = format!("SELECT * FROM '{}' LIMIT 2", karate_edges());
    assert_eq!(ok(ursa(&["-f", "csv", "-c", &sql])), "src,dst\n0,1\n0,2\n");
    assert_eq!(
        ok(ursa(&["-f", "tsv", "-c", &sql])),
        "src\tdst\n0\t1\n0\t2\n"
    );
    assert_eq!(
        ok(ursa(&["-f", "json", "-c", &sql])),
        "[{\"src\":0,\"dst\":1},{\"src\":0,\"dst\":2}]\n"
    );
    assert_eq!(
        ok(ursa(&["-f", "ndjson", "-c", &sql])),
        "{\"src\":0,\"dst\":1}\n{\"src\":0,\"dst\":2}\n"
    );
    // An Arrow IPC stream starts with the continuation marker.
    let arrow = ursa(&["-f", "arrow", "-c", &sql]);
    assert!(arrow.status.success());
    assert_eq!(&arrow.stdout[..4], &[0xff, 0xff, 0xff, 0xff]);
}

#[test]
fn empty_results_keep_their_header() {
    assert_eq!(
        ok(ursa(&["-c", "SELECT 1 AS x WHERE false"])),
        "+---+\n| x |\n+---+\n+---+\n"
    );
    assert_eq!(
        ok(ursa(&["-f", "csv", "-c", "SELECT 1 AS x WHERE false"])),
        "x\n"
    );
    assert_eq!(
        ok(ursa(&["-f", "json", "-c", "SELECT 1 AS x WHERE false"])),
        "[]\n"
    );
}

#[test]
fn several_statements_print_each_result() {
    let sql = format!(
        "CREATE VIEW ties AS SELECT * FROM '{}';
         SELECT count(*) AS edges FROM ties;
         SELECT max(dst) AS top FROM ties;",
        karate_edges()
    );
    assert_eq!(ok(ursa(&["-f", "csv", "-c", &sql])), "edges\n78\ntop\n33\n");
}

#[test]
fn commands_and_files_run_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("q.sql");
    std::fs::write(&file, "SELECT 2 AS b;\n").unwrap();
    let file = file.display().to_string();
    let out = ok(ursa(&[
        "-f",
        "csv",
        "-c",
        "SELECT 1 AS a",
        "--file",
        &file,
        "-c",
        "SELECT 3 AS c",
    ]));
    assert_eq!(out, "a\n1\nb\n2\nc\n3\n");
}

#[test]
fn sql_from_stdin() {
    let sql = format!("SELECT count(*) AS n FROM '{}';", karate_edges());
    let out = ursa_with_stdin(&["-f", "csv"], &sql);
    assert_eq!(ok(out), "n\n78\n");
}

#[test]
fn copy_to_parquet_and_read_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("edges.parquet").display().to_string();
    let copy = format!(
        "COPY (SELECT * FROM '{}' WHERE src = 0) TO '{target}'",
        karate_edges()
    );
    assert_eq!(ok(ursa(&["-f", "csv", "-c", &copy])), "count\n16\n");
    let read = format!("SELECT count(*) AS n, max(dst) AS top FROM '{target}'");
    assert_eq!(ok(ursa(&["-f", "csv", "-c", &read])), "n,top\n16,31\n");
}

#[test]
fn gzip_csv() {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edges.csv.gz");
    let mut gz = GzEncoder::new(
        std::fs::File::create(&path).unwrap(),
        Compression::default(),
    );
    gz.write_all(b"src,dst\n0,1\n1,2\n2,0\n").unwrap();
    gz.finish().unwrap();
    let sql = format!(
        "SELECT sum(dst) AS s, count(*) AS n FROM '{}'",
        path.display()
    );
    assert_eq!(ok(ursa(&["-f", "csv", "-c", &sql])), "s,n\n3,3\n");
}

#[test]
fn local_glob_and_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.csv"), "src,dst\n0,1\n").unwrap();
    std::fs::write(dir.path().join("b.csv"), "src,dst\n1,2\n2,3\n").unwrap();
    let root = dir.path().display();
    let glob = format!("SELECT count(*) AS n FROM '{root}/*.csv'");
    assert_eq!(ok(ursa(&["-f", "csv", "-c", &glob])), "n\n3\n");
    let directory = format!("SELECT count(*) AS n FROM '{root}/'");
    assert_eq!(ok(ursa(&["-f", "csv", "-c", &directory])), "n\n3\n");
}

#[test]
fn errors_are_for_people() {
    let e = err(ursa(&["-c", "SELECT * FROM 'no_such_file.csv'"]));
    assert_eq!(e, "Error: no file found at 'no_such_file.csv'\n");

    let e = err(ursa(&["-c", "SELEC 1"]));
    assert!(e.starts_with("Error: syntax error: "), "{e}");
    assert!(!e.contains("ParserError"), "{e}");

    let e = err(ursa(&["-c", "SELECT 'a'::int"]));
    assert!(!e.contains("Optimizer rule"), "{e}");

    // A syntax error anywhere runs nothing.
    let out = ursa(&["-c", "SELECT 1 AS a; SELEC 2"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}

#[test]
fn graph_and_database_statements_are_not_supported_yet() {
    for (sql, message) in [
        (
            "CREATE PROPERTY GRAPH karate EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
            "CREATE PROPERTY GRAPH is not supported yet (planned in #235)",
        ),
        (
            "DESCRIBE GRAPH karate",
            "DESCRIBE GRAPH is not supported yet (planned in #236)",
        ),
        ("SHOW GRAPHS", "SHOW GRAPHS is not supported yet (planned in #236)"),
        (
            "ATTACH 'club.ursa'",
            "ATTACH is not supported yet (planned in #237)",
        ),
    ] {
        assert_eq!(err(ursa(&["-c", sql])), format!("Error: {message}\n"));
    }

    let e = err(ursa(&["club.ursa", "-c", "SELECT 1"]));
    assert!(e.contains("planned in #237"), "{e}");
}

#[test]
fn no_sql_points_at_the_options() {
    let out = ursa(&[]);
    assert!(out.status.success());
    let e = String::from_utf8(out.stderr).unwrap();
    assert!(e.contains("interactive shell is not available yet"), "{e}");
}
