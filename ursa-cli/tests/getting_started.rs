//! The Getting Started page, run end to end.
//!
//! The page is written as a session in one database file, `club.ursa`.
//! Each step below is the SQL the page shows, run as a separate `ursa
//! club.ursa -c` process in a directory holding the data files, as a reader
//! following the page would. Where the page prints a result, the test
//! checks it exactly. The page itself lives in the Tempra library
//! `cl_dixon/ursa` at `/sql/getting-started.md`. When the page and this test
//! disagree, one of them is wrong; fix it and say which.
//!
//! Not covered: "Install" (the binary under test is the installed one) and
//! "Files in object storage", which needs a cloud account.

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A directory holding the data files the page downloads.
fn data_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../python/ursa/datasets/data");
    for file in ["karate_edges.csv", "karate_nodes.csv", "lesmis_edges.csv"] {
        std::fs::copy(data.join(file), dir.path().join(file)).unwrap();
    }
    dir
}

/// Run `ursa` in `cwd`; also return how many graph indexes it built and
/// loaded from the database.
fn ursa(cwd: &Path, args: &[&str]) -> (String, (u64, u64)) {
    let output: Output = Command::new(env!("CARGO_BIN_EXE_ursa"))
        .args(args)
        .current_dir(cwd)
        .env("URSA_INDEX_STATS", "1")
        .stdin(Stdio::null())
        .output()
        .expect("ursa runs");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "ursa {args:?} failed: {stderr}");
    let counts = stderr
        .lines()
        .find_map(|l| l.strip_prefix("ursa: graph indexes built "))
        .and_then(|l| l.split_once(", loaded "))
        .map(|(b, l)| (b.trim().parse().unwrap(), l.trim().parse().unwrap()))
        .unwrap_or((0, 0));
    (String::from_utf8(output.stdout).unwrap(), counts)
}

/// Run SQL against `club.ursa`, as the page's shell session does.
fn sql(cwd: &Path, query: &str) -> (String, (u64, u64)) {
    ursa(cwd, &["club.ursa", "-c", query])
}

fn table(lines: &[&str]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

#[test]
fn the_getting_started_page_runs_as_written() {
    let dir = data_dir();
    let cwd = dir.path();

    // Query a file (no database).
    let (out, _) = ursa(cwd, &["-c", "SELECT * FROM 'karate_edges.csv' LIMIT 3"]);
    assert_eq!(
        out,
        table(&[
            "┌───────┬───────┐",
            "│  src  │  dst  │",
            "│ Int64 │ Int64 │",
            "├───────┼───────┤",
            "│     0 │     1 │",
            "│     0 │     2 │",
            "│     0 │     3 │",
            "└───────┴───────┘",
        ])
    );
    // `read_csv` reads the same file, with options.
    let (read, _) = ursa(
        cwd,
        &[
            "-c",
            "SELECT * FROM read_csv('karate_edges.csv', header => true, delim => ',') LIMIT 3",
        ],
    );
    assert_eq!(read, out);

    // Open a database, and define a graph.
    let (out, _) = sql(
        cwd,
        "CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv';
         CREATE VIEW ties    AS SELECT * FROM 'karate_edges.csv';
         CREATE PROPERTY GRAPH karate
           VERTEX TABLES (members KEY (id))
           EDGE TABLES (
             ties SOURCE KEY (src) REFERENCES members (id)
                  DESTINATION KEY (dst) REFERENCES members (id)
           );
         CREATE PROPERTY GRAPH karate_edges_only
           EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
    );
    assert_eq!(out, "");
    assert!(cwd.join("club.ursa").exists());

    let (out, _) = sql(cwd, "SELECT * FROM karate.nodes LIMIT 3;");
    assert_eq!(
        out,
        table(&[
            "┌───────┬────────┐",
            "│  id   │  club  │",
            "│ Int64 │  Utf8  │",
            "├───────┼────────┤",
            "│     0 │ Mr. Hi │",
            "│     1 │ Mr. Hi │",
            "│     2 │ Mr. Hi │",
            "└───────┴────────┘",
        ])
    );

    // Run an algorithm: the first query over the graph builds its index.
    let (out, (built, _)) = sql(
        cwd,
        "SELECT id, round(pagerank, 4) AS pagerank
         FROM pagerank(karate, direction => 'both')
         ORDER BY pagerank DESC
         LIMIT 5;",
    );
    assert_eq!(
        out,
        table(&[
            "┌───────┬──────────┐",
            "│  id   │ pagerank │",
            "│ Int64 │ Float64  │",
            "├───────┼──────────┤",
            "│    33 │   0.1009 │",
            "│     0 │    0.097 │",
            "│    32 │   0.0717 │",
            "│     2 │   0.0571 │",
            "│     1 │   0.0529 │",
            "└───────┴──────────┘",
        ])
    );
    assert_eq!(built, 1, "the first query builds the index");

    // The page's two parameter examples run.
    sql(cwd, "SELECT * FROM louvain(karate, seed => 1);");
    sql(
        cwd,
        "SELECT * FROM pagerank(karate, damping => 0.9, direction => 'both');",
    );

    // Combine algorithms and attributes: a later process loads the stored
    // index instead of building it.
    let (out, (built, loaded)) = sql(
        cwd,
        "SELECT id, club, degree, triangles, community
         FROM karate.nodes
         JOIN algorithms(karate,
                         degree(direction => 'both'),
                         triangles => triangle_count(),
                         community => louvain(seed => 1)) USING (id)
         ORDER BY degree DESC
         LIMIT 5;",
    );
    assert_eq!(
        out,
        table(&[
            "┌───────┬─────────┬────────┬───────────┬───────────┐",
            "│  id   │  club   │ degree │ triangles │ community │",
            "│ Int64 │  Utf8   │ UInt32 │  UInt32   │  UInt32   │",
            "├───────┼─────────┼────────┼───────────┼───────────┤",
            "│    33 │ Officer │     17 │        15 │         2 │",
            "│     0 │ Mr. Hi  │     16 │        18 │         0 │",
            "│    32 │ Officer │     12 │        13 │         2 │",
            "│     2 │ Mr. Hi  │     10 │        11 │         0 │",
            "│     1 │ Mr. Hi  │      9 │        12 │         0 │",
            "└───────┴─────────┴────────┴───────────┴───────────┘",
        ])
    );
    assert_eq!((built, loaded), (0, 1), "the stored index is loaded");

    // Weights.
    sql(
        cwd,
        "CREATE VIEW lesmis_edges AS SELECT * FROM 'lesmis_edges.csv';
         CREATE PROPERTY GRAPH lesmis
           EDGE TABLES (lesmis_edges SOURCE KEY (src) DESTINATION KEY (dst));",
    );
    let (out, _) = sql(
        cwd,
        "SELECT * FROM pagerank(lesmis, weight => weight, direction => 'both')
         ORDER BY pagerank DESC
         LIMIT 5;",
    );
    assert_eq!(
        out,
        table(&[
            "┌──────────┬──────────────────────┐",
            "│    id    │       pagerank       │",
            "│   Utf8   │       Float64        │",
            "├──────────┼──────────────────────┤",
            "│ Valjean  │  0.09956068175644767 │",
            "│ Marius   │ 0.051667656904091475 │",
            "│ Myriel   │  0.03923416880292588 │",
            "│ Cosette  │  0.03691018363299517 │",
            "│ Enjolras │ 0.036614491459848165 │",
            "└──────────┴──────────────────────┘",
        ])
    );
    let (out, _) = sql(
        cwd,
        "SELECT * FROM louvain(lesmis, weight => log10(1 + weight), seed => 7)
         ORDER BY id
         LIMIT 5;",
    );
    assert_eq!(
        out,
        table(&[
            "┌────────────┬─────────┐",
            "│     id     │ louvain │",
            "│    Utf8    │ UInt32  │",
            "├────────────┼─────────┤",
            "│ Anzelma    │       3 │",
            "│ Babet      │       3 │",
            "│ Bahorel    │       5 │",
            "│ Bamatabois │       1 │",
            "│ BaronessT  │       4 │",
            "└────────────┴─────────┘",
        ])
    );

    // Subgraphs.
    let (out, _) = sql(
        cwd,
        "CREATE VIEW strong_ties AS SELECT * FROM lesmis_edges WHERE weight >= 3;
         CREATE PROPERTY GRAPH lesmis_strong
           EDGE TABLES (strong_ties SOURCE KEY (src) DESTINATION KEY (dst));
         SELECT count(DISTINCT connected_components) AS groups
         FROM connected_components(lesmis_strong);",
    );
    assert_eq!(
        out,
        table(&[
            "┌────────┐",
            "│ groups │",
            "│ Int64  │",
            "├────────┤",
            "│      3 │",
            "└────────┘",
        ])
    );

    // Traversals.
    let (out, _) = ursa(
        cwd,
        &[
            "club.ursa",
            "--format",
            "csv",
            "-c",
            "SELECT * FROM hop(karate, seeds => [0], n => 1, direction => 'both');",
        ],
    );
    assert_eq!(out.lines().count(), 1 + 16, "a header and 16 rows: {out}");
    let (out, _) = ursa(
        cwd,
        &[
            "club.ursa",
            "--format",
            "csv",
            "-c",
            "SELECT * FROM hop(karate,
                               seeds => (SELECT id FROM karate.nodes WHERE club = 'Officer'),
                               n => 2);",
        ],
    );
    assert_eq!(out.lines().count(), 1 + 39, "a header and 39 rows: {out}");
    let (out, _) = sql(
        cwd,
        "SELECT * FROM shortest_path(karate, source => 0, target => 33, direction => 'both');",
    );
    assert_eq!(
        out,
        table(&[
            "┌───────┬───────┬───────┬─────────┐",
            "│  src  │  dst  │  hop  │  cost   │",
            "│ Int64 │ Int64 │ Int64 │ Float64 │",
            "├───────┼───────┼───────┼─────────┤",
            "│     0 │     8 │     0 │     1.0 │",
            "│     8 │    33 │     1 │     2.0 │",
            "└───────┴───────┴───────┴─────────┘",
        ])
    );
    let (walk, _) = sql(
        cwd,
        "SELECT * FROM random_walk(karate, start => [0, 1], steps => 4, walks_per_node => 2, seed => 7);",
    );
    let (again, _) = sql(
        cwd,
        "SELECT * FROM random_walk(karate, start => [0, 1], steps => 4, walks_per_node => 2, seed => 7);",
    );
    assert_eq!(walk, again, "a seeded walk repeats");

    // Output.
    let (out, _) = ursa(
        cwd,
        &[
            "club.ursa",
            "--format",
            "json",
            "-c",
            "SELECT * FROM pagerank(karate) ORDER BY pagerank DESC LIMIT 3",
        ],
    );
    assert!(out.trim_start().starts_with('['), "{out}");
    let (out, _) = ursa(
        cwd,
        &[
            "club.ursa",
            "--format",
            "csv",
            "-c",
            "SELECT * FROM louvain(karate, seed => 1)",
        ],
    );
    assert_eq!(out.lines().next(), Some("id,louvain"));
    assert_eq!(out.lines().count(), 1 + 34);
    let (out, _) = sql(
        cwd,
        "COPY (SELECT * FROM pagerank(karate)) TO 'pagerank.parquet';",
    );
    assert_eq!(
        out,
        table(&[
            "┌────────┐",
            "│ count  │",
            "│ UInt64 │",
            "├────────┤",
            "│     34 │",
            "└────────┘",
        ])
    );
    let (out, _) = ursa(cwd, &["-c", "SELECT count(*) AS n FROM 'pagerank.parquet'"]);
    assert!(out.contains("│    34 │"), "{out}");

    // Find out what is available.
    let (out, _) = sql(cwd, "SHOW GRAPHS;");
    for graph in ["karate", "karate_edges_only", "lesmis", "lesmis_strong"] {
        assert!(out.contains(graph), "{graph} in {out}");
    }
    let (out, _) = sql(cwd, "DESCRIBE GRAPH karate;");
    assert!(out.contains("members.id"), "{out}");
    let (out, _) = ursa(
        cwd,
        &[
            "club.ursa",
            "--format",
            "csv",
            "-c",
            "SELECT name, family FROM ursa_functions() ORDER BY name;",
        ],
    );
    for f in [
        "algorithms,",
        "pagerank,algorithm",
        "hop,traversal",
        "density,",
    ] {
        assert!(out.contains(f), "{f} in {out}");
    }
}
