//! `ursa club.ursa`: a database file kept between processes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A temporary directory holding copies of the karate CSVs.
fn karate_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../python/ursa/datasets/data");
    for file in ["karate_edges.csv", "karate_nodes.csv"] {
        std::fs::copy(data.join(file), dir.path().join(file)).unwrap();
    }
    dir
}

/// Run `ursa` in `cwd` with no stdin.
fn ursa_in(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ursa"))
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("ursa runs")
}

/// A directory other than the database's, so relative paths are tested.
fn elsewhere() -> PathBuf {
    std::env::temp_dir()
}

fn ok(output: Output) -> String {
    assert!(
        output.status.success(),
        "ursa failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn err(output: Output) -> String {
    assert!(
        !output.status.success(),
        "ursa should have failed; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stderr).unwrap()
}

const DEFINE: &str = "CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv';
CREATE VIEW ties    AS SELECT * FROM 'karate_edges.csv';
CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (
    ties SOURCE KEY (src) REFERENCES members (id)
         DESTINATION KEY (dst) REFERENCES members (id)
  );";

const QUERY: &str =
    "SELECT id, degree FROM degree(karate, direction => 'both') ORDER BY degree DESC, id LIMIT 5;
SELECT * FROM karate.nodes ORDER BY id LIMIT 3";

const EXPECTED: &str = "\
+----+--------+
| id | degree |
+----+--------+
| 33 | 17     |
| 0  | 16     |
| 32 | 12     |
| 2  | 10     |
| 1  | 9      |
+----+--------+
+----+--------+
| id | club   |
+----+--------+
| 0  | Mr. Hi |
| 1  | Mr. Hi |
| 2  | Mr. Hi |
+----+--------+
";

/// `DEFINE` with each `karate_…` file named by `prefix` (`../db/data/`).
fn define_from(prefix: &str) -> String {
    DEFINE.replace("'karate_", &format!("'{prefix}karate_"))
}

/// The statements of the catalog file at `path`.
fn catalog(path: &Path) -> Vec<String> {
    ursa_engine::database::Database::open(path, &ursa_engine::database::OpenOptions::read_only())
        .unwrap()
        .statements()
}

/// A layout with three directories: `db/` holds the database, `work/` is
/// where the first process runs, and the karate files are in `db/data/`
/// (inside the database's directory) or `data/` (outside it).
fn three_dirs(data_inside: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let parent = tempfile::tempdir().unwrap();
    let db = parent.path().join("db");
    let work = parent.path().join("work");
    std::fs::create_dir_all(&db).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let data = if data_inside {
        db.join("data")
    } else {
        parent.path().join("data")
    };
    std::fs::rename(karate_dir().keep(), &data).unwrap();
    (parent, db, work)
}

/// One process, run from a directory other than the database's, defines the
/// graph with relative paths into a third directory: they resolve against
/// its current directory. A second process, run from yet another directory,
/// queries it without defining anything.
#[test]
fn a_second_process_sees_what_the_first_defined() {
    let (parent, db_dir, work) = three_dirs(true);
    let define = define_from("../db/data/");
    assert_eq!(ok(ursa_in(&work, &["../db/club.ursa", "-c", &define])), "");
    // Stored relative to the database's directory.
    let stored = catalog(&db_dir.join("club.ursa"));
    assert_eq!(
        stored[..2],
        [
            "CREATE VIEW members AS SELECT * FROM 'data/karate_nodes.csv'",
            "CREATE VIEW ties AS SELECT * FROM 'data/karate_edges.csv'",
        ]
    );
    let db = db_dir.join("club.ursa");
    let db = db.to_str().unwrap();
    assert_eq!(ok(ursa_in(&elsewhere(), &[db, "-c", QUERY])), EXPECTED);
    assert_eq!(ok(ursa_in(parent.path(), &[db, "-c", QUERY])), EXPECTED);
    assert!(db_dir.join("club.ursa.d/lock").is_file());
}

/// A path outside the database's directory is stored absolute, and found
/// from anywhere.
#[test]
fn a_path_outside_the_database_directory_is_stored_absolute() {
    let (parent, db_dir, work) = three_dirs(false);
    let define = define_from("../data/");
    ok(ursa_in(&work, &["../db/club.ursa", "-c", &define]));
    let data = std::fs::canonicalize(parent.path().join("data")).unwrap();
    let stored = catalog(&db_dir.join("club.ursa"));
    assert_eq!(
        stored[0],
        format!(
            "CREATE VIEW members AS SELECT * FROM '{}'",
            data.join("karate_nodes.csv").display()
        )
    );
    let db = db_dir.join("club.ursa");
    assert_eq!(
        ok(ursa_in(&elsewhere(), &[db.to_str().unwrap(), "-c", QUERY])),
        EXPECTED
    );
}

/// A database moved with its data keeps working.
#[test]
fn a_database_moves_with_its_files() {
    let (parent, db_dir, work) = three_dirs(true);
    ok(ursa_in(
        &work,
        &["../db/club.ursa", "-c", &define_from("../db/data/")],
    ));
    let moved = parent.path().join("moved");
    std::fs::rename(&db_dir, &moved).unwrap();
    let db = moved.join("club.ursa");
    assert_eq!(
        ok(ursa_in(&elsewhere(), &[db.to_str().unwrap(), "-c", QUERY])),
        EXPECTED
    );
    assert_eq!(ok(ursa_in(&moved, &["club.ursa", "-c", QUERY])), EXPECTED);
}

/// In a session with a database, a query's relative path and a `COPY`
/// target resolve against the current directory, not the database's.
#[test]
fn queries_and_copy_resolve_against_the_current_directory() {
    let (parent, db_dir, work) = three_dirs(true);
    let db = db_dir.join("club.ursa");
    let db = db.to_str().unwrap();
    let out = ok(ursa_in(
        &work,
        &[
            db,
            "--format",
            "csv",
            "-c",
            "SELECT count(*) AS n FROM '../db/data/karate_edges.csv'; \
             COPY (SELECT 1 AS x) TO 'out.csv'",
        ],
    ));
    assert_eq!(out, "n\n78\ncount\n1\n");
    assert!(work.join("out.csv").is_file());
    assert!(!db_dir.join("out.csv").exists());
    // A path relative to the database's directory is not found from
    // elsewhere.
    let e = err(ursa_in(
        parent.path(),
        &[db, "-c", "SELECT count(*) FROM 'data/karate_edges.csv'"],
    ));
    assert!(
        e.contains("no file found at 'data/karate_edges.csv'"),
        "{e}"
    );
    // An external table's LOCATION too; it is stored relative to the
    // database, as written otherwise (its columns and options).
    ok(ursa_in(
        &work,
        &[
            db,
            "-c",
            "CREATE EXTERNAL TABLE edges (src BIGINT, dst BIGINT) STORED AS CSV \
             LOCATION '../db/data/karate_edges.csv' OPTIONS ('format.has_header' 'true')",
        ],
    ));
    assert_eq!(
        catalog(&db_dir.join("club.ursa")),
        [
            "CREATE EXTERNAL TABLE edges (src BIGINT, dst BIGINT) STORED AS CSV \
          LOCATION 'data/karate_edges.csv' OPTIONS ('format.has_header' 'true')"
        ]
    );
    let out = ok(ursa_in(
        &elsewhere(),
        &[
            db,
            "--format",
            "csv",
            "-c",
            "SELECT count(*) AS n FROM edges",
        ],
    ));
    assert_eq!(out, "n\n78\n");
}

/// A graph over bare file paths, defined from another directory.
#[test]
fn a_graph_over_file_paths_defined_from_another_directory() {
    let (_parent, db_dir, work) = three_dirs(true);
    let create = "CREATE PROPERTY GRAPH k VERTEX TABLES ('../db/data/karate_nodes.csv' KEY (id)) \
                  EDGE TABLES ('../db/data/karate_edges.csv' SOURCE KEY (src) \
                  REFERENCES '../db/data/karate_nodes.csv' (id) DESTINATION KEY (dst))";
    ok(ursa_in(&work, &["../db/club.ursa", "-c", create]));
    assert_eq!(
        catalog(&db_dir.join("club.ursa")),
        [create.replace("../db/data/", "data/")]
    );
    let db = db_dir.join("club.ursa");
    let out = ok(ursa_in(
        &elsewhere(),
        &[
            db.to_str().unwrap(),
            "--format",
            "csv",
            "-c",
            "SELECT sum(degree) AS s FROM degree(k)",
        ],
    ));
    assert_eq!(out, "s\n78\n");
}

#[test]
fn readonly_refuses_ddl() {
    let dir = karate_dir();
    let db = dir.path().join("club.ursa");
    let db = db.to_str().unwrap();
    ok(ursa_in(dir.path(), &[db, "-c", DEFINE]));
    let before = std::fs::read(dir.path().join("club.ursa")).unwrap();

    let out = ok(ursa_in(
        dir.path(),
        &[
            "--readonly",
            db,
            "--format",
            "csv",
            "-c",
            "SELECT count(*) AS n FROM karate.edges",
        ],
    ));
    assert_eq!(out, "n\n78\n");
    let e = err(ursa_in(
        dir.path(),
        &[db, "--readonly", "-c", "DROP PROPERTY GRAPH karate"],
    ));
    assert_eq!(
        e,
        format!(
            "Error: {db} is open read-only (--readonly), so DROP PROPERTY GRAPH cannot change it\n"
        )
    );
    assert_eq!(std::fs::read(dir.path().join("club.ursa")).unwrap(), before);

    let e = err(ursa_in(
        dir.path(),
        &["--readonly", "new.ursa", "-c", "SELECT 1"],
    ));
    assert_eq!(
        e,
        "Error: cannot open 'new.ursa' read-only: the file does not exist\n"
    );
    assert!(!dir.path().join("new.ursa").exists());
}

#[test]
fn tables_with_data_are_refused() {
    let dir = karate_dir();
    let e = err(ursa_in(
        dir.path(),
        &["club.ursa", "-c", "CREATE TABLE t AS SELECT 1 AS x"],
    ));
    assert!(
        e.starts_with("Error: CREATE TABLE … AS is not supported in a database file"),
        "{e}"
    );
    assert!(e.contains("COPY (SELECT …) TO 't.parquet'"), "{e}");
    // In memory they work.
    let out = ok(ursa_in(
        dir.path(),
        &[
            "--format",
            "csv",
            "-c",
            "CREATE TABLE t AS SELECT 1 AS x; INSERT INTO t VALUES (2); SELECT sum(x) AS s FROM t",
        ],
    ));
    assert_eq!(out, "count\n1\ns\n3\n");
}

/// A second writer waits briefly, then fails.
#[test]
fn a_database_in_use_is_refused() {
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    let held = ursa_engine::database::Database::open(&path, &Default::default()).unwrap();
    let e = err(ursa_in(dir.path(), &["club.ursa", "-c", "SELECT 1"]));
    assert_eq!(e, "Error: club.ursa is in use by another process\n");
    // A reader is not blocked.
    ok(ursa_in(
        dir.path(),
        &["club.ursa", "--readonly", "-c", "SELECT 1"],
    ));
    drop(held);
    ok(ursa_in(dir.path(), &["club.ursa", "-c", "SELECT 1"]));
}

#[test]
fn attach_opens_a_database() {
    let dir = karate_dir();
    ok(ursa_in(dir.path(), &["club.ursa", "-c", DEFINE]));
    let out = ok(ursa_in(
        &elsewhere(),
        &[
            "--format",
            "csv",
            "-c",
            &format!(
                "ATTACH '{}'; SELECT count(*) AS n FROM karate.nodes",
                dir.path().join("club.ursa").display()
            ),
        ],
    ));
    assert_eq!(out, "n\n34\n");
}

/// Getting Started, "Open a database" and "Define a graph", each statement
/// in its own process, then later sections against the same file.
#[test]
fn open_a_database_matches_getting_started() {
    let dir = karate_dir();
    let cwd = dir.path();
    // `ursa club.ursa` creates the file (with stdin not a terminal, it starts no shell).
    let out = ursa_in(cwd, &["club.ursa"]);
    assert!(out.status.success());
    assert!(cwd.join("club.ursa").is_file());

    for sql in [
        "CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv';
         CREATE VIEW ties    AS SELECT * FROM 'karate_edges.csv';",
        "CREATE PROPERTY GRAPH karate
           VERTEX TABLES (members KEY (id))
           EDGE TABLES (
             ties SOURCE KEY (src) REFERENCES members (id)
                  DESTINATION KEY (dst) REFERENCES members (id)
           );",
        "CREATE PROPERTY GRAPH karate_edges_only
           EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
    ] {
        assert_eq!(ok(ursa_in(cwd, &["club.ursa", "-c", sql])), "");
    }
    // Getting Started omits the ORDER BY; row order is not guaranteed.
    assert_eq!(
        ok(ursa_in(
            cwd,
            &[
                "club.ursa",
                "-c",
                "SELECT * FROM karate.nodes ORDER BY id LIMIT 3;"
            ]
        )),
        "\
+----+--------+
| id | club   |
+----+--------+
| 0  | Mr. Hi |
| 1  | Mr. Hi |
| 2  | Mr. Hi |
+----+--------+
"
    );
    assert_eq!(
        ok(ursa_in(
            cwd,
            &[
                "club.ursa",
                "--format",
                "csv",
                "-c",
                "SELECT count(*) AS n FROM karate_edges_only.nodes"
            ]
        )),
        "n\n34\n"
    );
    assert_eq!(
        ok(ursa_in(
            cwd,
            &[
                "club.ursa",
                "-c",
                "SELECT id, round(pagerank, 4) AS pagerank
                 FROM pagerank(karate, direction => 'both')
                 ORDER BY pagerank DESC
                 LIMIT 5;"
            ]
        )),
        "\
+----+----------+
| id | pagerank |
+----+----------+
| 33 | 0.1009   |
| 0  | 0.097    |
| 32 | 0.0717   |
| 2  | 0.0571   |
| 1  | 0.0529   |
+----+----------+
"
    );
    let catalog = std::fs::read_to_string(cwd.join("club.ursa")).unwrap();
    assert!(catalog.contains("\"version\": 1"), "{catalog}");
    assert_eq!(catalog.matches("CREATE ").count(), 4, "{catalog}");
}

/// Run `ursa` in `cwd` asking for its index counts; its stdout, and
/// `(built, loaded)`.
fn ursa_counting(cwd: &Path, args: &[&str]) -> (String, (u64, u64)) {
    let output = Command::new(env!("CARGO_BIN_EXE_ursa"))
        .args(args)
        .current_dir(cwd)
        .env("URSA_INDEX_STATS", "1")
        .stdin(Stdio::null())
        .output()
        .expect("ursa runs");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = ok(output);
    let line = stderr
        .lines()
        .find_map(|l| l.strip_prefix("ursa: graph indexes built "))
        .unwrap_or_else(|| panic!("no index counts in: {stderr}"));
    let (built, loaded) = line.split_once(", loaded ").unwrap();
    (stdout, (built.parse().unwrap(), loaded.parse().unwrap()))
}

/// Getting Started, "Combine algorithms and attributes", in a `.ursa`
/// session: the second process loads the index the first stored.
#[test]
fn combine_in_a_database_loads_the_stored_index_in_a_second_process() {
    let dir = karate_dir();
    let cwd = dir.path();
    assert_eq!(ok(ursa_in(cwd, &["club.ursa", "-c", DEFINE])), "");
    let combine = "SELECT id, club, degree, triangles, community
         FROM karate.nodes
         JOIN algorithms(karate,
                         degree(direction => 'both'),
                         triangles => triangle_count(),
                         community => louvain(seed => 1)) USING (id)
         ORDER BY degree DESC
         LIMIT 5;";
    let expected = "\
+----+---------+--------+-----------+-----------+
| id | club    | degree | triangles | community |
+----+---------+--------+-----------+-----------+
| 33 | Officer | 17     | 15        | 2         |
| 0  | Mr. Hi  | 16     | 18        | 0         |
| 32 | Officer | 12     | 13        | 2         |
| 2  | Mr. Hi  | 10     | 11        | 0         |
| 1  | Mr. Hi  | 9      | 12        | 0         |
+----+---------+--------+-----------+-----------+
";
    assert_eq!(
        ursa_counting(cwd, &["club.ursa", "-c", combine]),
        (expected.to_string(), (1, 0))
    );
    let indexes = cwd.join("club.ursa.d/indexes");
    assert!(indexes.join("manifest.json").is_file());
    assert_eq!(
        ursa_counting(cwd, &["club.ursa", "-c", combine]),
        (expected.to_string(), (0, 1))
    );
    // A read-only process loads it too.
    assert_eq!(
        ursa_counting(cwd, &["club.ursa", "--readonly", "-c", combine]),
        (expected.to_string(), (0, 1))
    );
    // Editing the edge file makes the next process rebuild.
    let edges = cwd.join("karate_edges.csv");
    let mut text = std::fs::read_to_string(&edges).unwrap();
    text.push_str("0,33\n");
    std::fs::write(&edges, text).unwrap();
    let (_, counts) = ursa_counting(cwd, &["club.ursa", "-c", combine]);
    assert_eq!(counts, (1, 0));
}

/// Write a Parquet edge list of `n` seeded pseudo-random edges.
fn write_parquet_edges(path: &Path, n: usize) {
    use std::sync::Arc;

    use arrow::array::{Int64Array, RecordBatch};
    use arrow::datatypes::{DataType, Field, Schema};
    use datafusion::parquet::arrow::ArrowWriter;

    let schema = Arc::new(Schema::new(vec![
        Field::new("src", DataType::Int64, false),
        Field::new("dst", DataType::Int64, false),
    ]));
    let file = std::fs::File::create(path).unwrap();
    let mut writer = ArrowWriter::try_new(file, Arc::clone(&schema), None).unwrap();
    let nodes = (n / 6) as u64;
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 33) % nodes) as i64
    };
    let mut written = 0;
    while written < n {
        let len = (n - written).min(1 << 16);
        let src: Vec<i64> = (0..len).map(|_| next()).collect();
        let dst: Vec<i64> = (0..len).map(|_| next()).collect();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int64Array::from(src)),
                Arc::new(Int64Array::from(dst)),
            ],
        )
        .unwrap();
        writer.write(&batch).unwrap();
        written += len;
    }
    writer.close().unwrap();
}

/// Two `ursa club.ursa -c` processes over one Parquet edge list: the second
/// loads the index. Wall times are printed (run with `--nocapture`), not
/// asserted.
#[test]
fn a_second_process_over_parquet_loads_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let n: usize = std::env::var("URSA_TEST_EDGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400_000);
    write_parquet_edges(&cwd.join("edges.parquet"), n);
    assert_eq!(
        ok(ursa_in(
            cwd,
            &[
                "club.ursa",
                "-c",
                "CREATE VIEW ties AS SELECT * FROM 'edges.parquet';
                 CREATE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));"
            ]
        )),
        ""
    );
    let query = "SELECT id, round(pagerank, 8) AS pagerank FROM pagerank(g)
                 ORDER BY pagerank DESC, id LIMIT 3";
    let args = ["club.ursa", "--format", "csv", "-c", query];
    let started = std::time::Instant::now();
    let (first, counts) = ursa_counting(cwd, &args);
    let first_time = started.elapsed();
    assert_eq!(counts, (1, 0));
    let started = std::time::Instant::now();
    let (second, counts) = ursa_counting(cwd, &args);
    let second_time = started.elapsed();
    assert_eq!(counts, (0, 1));
    assert_eq!(second, first);
    eprintln!("{n} edges, pagerank: first process {first_time:?}, second process {second_time:?}");
}
