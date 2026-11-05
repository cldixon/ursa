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

/// Getting Started, "Define a graph", in one invocation. Getting Started omits
/// the `ORDER BY`; it is here because row order is not guaranteed.
#[test]
fn define_a_graph_matches_getting_started() {
    let data = "python/ursa/datasets/data";
    let sql = format!(
        "CREATE VIEW members AS SELECT * FROM '{data}/karate_nodes.csv';
         CREATE VIEW ties    AS SELECT * FROM '{data}/karate_edges.csv';

         CREATE PROPERTY GRAPH karate
           VERTEX TABLES (members KEY (id))
           EDGE TABLES (
             ties SOURCE KEY (src) REFERENCES members (id)
                  DESTINATION KEY (dst) REFERENCES members (id)
           );

         CREATE PROPERTY GRAPH karate_edges_only
           EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));

         SELECT * FROM karate.nodes ORDER BY id LIMIT 3;"
    );
    assert_eq!(
        ok(ursa(&["-c", &sql])),
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
}

#[test]
fn graphs_last_for_the_invocation() {
    let define = format!(
        "CREATE VIEW ties AS SELECT * FROM '{}';
         CREATE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
        karate_edges()
    );
    let out = ok(ursa(&[
        "-f",
        "csv",
        "-c",
        &define,
        "-c",
        "SELECT count(*) AS nodes FROM g.nodes",
        "-c",
        "SELECT count(*) AS edges FROM g.edges; DROP PROPERTY GRAPH g",
    ]));
    assert_eq!(out, "nodes\n34\nedges\n78\n");

    let e = err(ursa(&["-c", "SELECT * FROM g.nodes"]));
    assert!(e.contains("g.nodes"), "{e}");
    let e = err(ursa(&[
        "-c",
        "CREATE PROPERTY GRAPH g EDGE TABLES (ties AS t SOURCE KEY (src) DESTINATION KEY (dst))",
    ]));
    assert!(
        e.starts_with("Error: table aliases (AS) are not supported yet"),
        "{e}"
    );
}

#[test]
fn database_statements_are_not_supported_yet() {
    assert_eq!(
        err(ursa(&["-c", "ATTACH 'club.ursa'"])),
        "Error: ATTACH is not supported yet (planned in #237)\n"
    );

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

/// The Getting Started graph: `members` and `ties` over the karate files, and
/// `karate` over them.
fn define_karate() -> String {
    let data = "python/ursa/datasets/data";
    format!(
        "CREATE VIEW members AS SELECT * FROM '{data}/karate_nodes.csv';
         CREATE VIEW ties    AS SELECT * FROM '{data}/karate_edges.csv';
         CREATE PROPERTY GRAPH karate
           VERTEX TABLES (members KEY (id))
           EDGE TABLES (
             ties SOURCE KEY (src) REFERENCES members (id)
                  DESTINATION KEY (dst) REFERENCES members (id)
           );"
    )
}

/// Getting Started, "Run an algorithm".
#[test]
fn run_an_algorithm_matches_getting_started() {
    let sql = format!(
        "{}
         SELECT id, round(pagerank, 4) AS pagerank
         FROM pagerank(karate, direction => 'both')
         ORDER BY pagerank DESC
         LIMIT 5;",
        define_karate()
    );
    assert_eq!(
        ok(ursa(&["-c", &sql])),
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
}

/// Getting Started, "Combine algorithms and attributes", with the joins
/// written with `ON`: DataFusion 55 cannot plan a chain of four `USING`
/// joins (see `ursa-engine/tests/graph_functions.rs`).
#[test]
fn combine_algorithms_and_attributes() {
    let sql = format!(
        "{}
         SELECT n.id, club, degree, triangle_count AS triangles, louvain AS community
         FROM karate.nodes AS n
         JOIN degree(karate, direction => 'both') AS d ON d.id = n.id
         JOIN triangle_count(karate)               AS t ON t.id = n.id
         JOIN louvain(karate, seed => 1)           AS l ON l.id = n.id
         ORDER BY degree DESC
         LIMIT 5;",
        define_karate()
    );
    assert_eq!(
        ok(ursa(&["-c", &sql])),
        "\
+----+---------+--------+-----------+-----------+
| id | club    | degree | triangles | community |
+----+---------+--------+-----------+-----------+
| 33 | Officer | 17     | 15        | 2         |
| 0  | Mr. Hi  | 16     | 18        | 0         |
| 32 | Officer | 12     | 13        | 2         |
| 2  | Mr. Hi  | 10     | 11        | 0         |
| 1  | Mr. Hi  | 9      | 12        | 0         |
+----+---------+--------+-----------+-----------+
"
    );
}

/// One `USING` join per algorithm works as written: the function's columns
/// are qualified by its name.
#[test]
fn an_algorithm_joins_using_id() {
    let sql = format!(
        "{}
         SELECT id, club, degree FROM karate.nodes
         JOIN degree(karate, direction => 'both') USING (id)
         ORDER BY degree DESC, id LIMIT 2;",
        define_karate()
    );
    assert_eq!(
        ok(ursa(&["-f", "csv", "-c", &sql])),
        "id,club,degree\n33,Officer,17\n0,Mr. Hi,16\n"
    );
}

#[test]
fn graph_function_errors_are_for_people() {
    let sql = format!("{} SELECT * FROM pagerank(karte)", define_karate());
    assert_eq!(
        err(ursa(&["-c", &sql])),
        "Error: graph 'karte' does not exist; did you mean 'karate'?\n"
    );
}

/// Getting Started, "Find out what is available", after "Define a graph"
/// (`ursa_functions()` is in `ursa_functions_lists_the_catalog`).
#[test]
fn find_out_what_is_available_matches_getting_started() {
    let sql = format!(
        "{}
         CREATE PROPERTY GRAPH karate_edges_only
           EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
        define_karate()
    );
    let out = ok(ursa(&[
        "-c",
        &sql,
        "-c",
        "SHOW GRAPHS; DESCRIBE GRAPH karate;",
    ]));
    assert_eq!(
        out,
        "\
+-------------------+------------+--------+-------------+--------------+------------+
| name              | edge_table | source | destination | vertex_table | vertex_key |
+-------------------+------------+--------+-------------+--------------+------------+
| karate            | ties       | src    | dst         | members      | id         |
| karate_edges_only | ties       | src    | dst         |              |            |
+-------------------+------------+--------+-------------+--------------+------------+
+------------+-------------+-----------+-------------+--------------+
| table_name | column_name | data_type | role        | origin       |
+------------+-------------+-----------+-------------+--------------+
| nodes      | id          | Int64     | node id     | members.id   |
| nodes      | club        | Utf8      |             | members.club |
| edges      | src         | Int64     | source      | ties.src     |
| edges      | dst         | Int64     | destination | ties.dst     |
+------------+-------------+-----------+-------------+--------------+
"
    );

    // As JSON, one object per row; NULLs are left out.
    let out = ok(ursa(&[
        "-f",
        "json",
        "-c",
        &sql,
        "-c",
        "SHOW GRAPHS; DESC GRAPH karate_edges_only",
    ]));
    assert_eq!(
        out,
        "\
[{\"name\":\"karate\",\"edge_table\":\"ties\",\"source\":\"src\",\"destination\":\"dst\",\"vertex_table\":\"members\",\"vertex_key\":\"id\"},\
{\"name\":\"karate_edges_only\",\"edge_table\":\"ties\",\"source\":\"src\",\"destination\":\"dst\"}]
[{\"table_name\":\"nodes\",\"column_name\":\"id\",\"data_type\":\"Int64\",\"role\":\"node id\",\"origin\":\"ties.src, ties.dst\"},\
{\"table_name\":\"edges\",\"column_name\":\"src\",\"data_type\":\"Int64\",\"role\":\"source\",\"origin\":\"ties.src\"},\
{\"table_name\":\"edges\",\"column_name\":\"dst\",\"data_type\":\"Int64\",\"role\":\"destination\",\"origin\":\"ties.dst\"}]
"
    );

    assert_eq!(
        err(ursa(&["-c", &sql, "-c", "DESCRIBE GRAPH karat"])),
        "Error: graph 'karat' does not exist; did you mean 'karate'?\n"
    );
}

/// A graph over bare file paths, with no views: the florentine families.
#[test]
fn a_graph_over_file_paths() {
    let data = "python/ursa/datasets/data";
    let sql = format!(
        "CREATE PROPERTY GRAPH florentine
           EDGE TABLES ('{data}/florentine_edges.csv' SOURCE KEY (src) DESTINATION KEY (dst));
         SHOW GRAPHS;
         SELECT id, degree FROM degree(florentine, direction => 'both')
         ORDER BY degree DESC, id LIMIT 3;
         SELECT count(*) AS families FROM florentine.nodes;"
    );
    assert_eq!(
        ok(ursa(&["-c", &sql])),
        "\
+------------+--------------------------------------------------+--------+-------------+--------------+------------+
| name       | edge_table                                       | source | destination | vertex_table | vertex_key |
+------------+--------------------------------------------------+--------+-------------+--------------+------------+
| florentine | 'python/ursa/datasets/data/florentine_edges.csv' | src    | dst         |              |            |
+------------+--------------------------------------------------+--------+-------------+--------------+------------+
+----------+--------+
| id       | degree |
+----------+--------+
| Medici   | 6      |
| Guadagni | 4      |
| Strozzi  | 4      |
+----------+--------+
+----------+
| families |
+----------+
| 15       |
+----------+
"
    );
}

#[test]
fn dropping_a_table_a_graph_uses_is_refused() {
    let sql = format!("{} DROP VIEW ties", define_karate());
    assert_eq!(
        err(ursa(&["-c", &sql])),
        "Error: cannot drop view 'ties': graph 'karate' uses it; drop property graph karate \
         first\n"
    );
    let sql = format!(
        "{} CREATE OR REPLACE VIEW members AS SELECT id AS member, club FROM members",
        define_karate()
    );
    let e = err(ursa(&["-c", &sql]));
    assert!(
        e.starts_with("Error: cannot replace view 'members': graph 'karate' uses it"),
        "{e}"
    );
    // After DROP PROPERTY GRAPH, the views can go, and SHOW TABLES works.
    let sql = format!(
        "{} DROP PROPERTY GRAPH karate; DROP VIEW ties; DROP VIEW members;
         SELECT count(*) AS n FROM information_schema.tables WHERE table_schema = 'public'",
        define_karate()
    );
    assert_eq!(ok(ursa(&["-f", "csv", "-c", &sql])), "n\n0\n");
}

/// Getting Started, "Combine algorithms and attributes", as written: one
/// `algorithms()` call, joined with `USING (id)`.
#[test]
fn combine_algorithms_and_attributes_with_algorithms() {
    let sql = format!(
        "{}
         SELECT id, club, degree, triangles, community
         FROM karate.nodes
         JOIN algorithms(karate,
                         degree(direction => 'both'),
                         triangles => triangle_count(),
                         community => louvain(seed => 1)) USING (id)
         ORDER BY degree DESC
         LIMIT 5;",
        define_karate()
    );
    assert_eq!(
        ok(ursa(&["-c", &sql])),
        "\
+----+---------+--------+-----------+-----------+
| id | club    | degree | triangles | community |
+----+---------+--------+-----------+-----------+
| 33 | Officer | 17     | 15        | 2         |
| 0  | Mr. Hi  | 16     | 18        | 0         |
| 32 | Officer | 12     | 13        | 2         |
| 2  | Mr. Hi  | 10     | 11        | 0         |
| 1  | Mr. Hi  | 9      | 12        | 0         |
+----+---------+--------+-----------+-----------+
"
    );
}

/// Les Misérables, as Getting Started names it, from the workspace root.
const LESMIS: &str =
    "CREATE VIEW lesmis_edges AS SELECT * FROM 'python/ursa/datasets/data/lesmis_edges.csv';
     CREATE PROPERTY GRAPH lesmis
       EDGE TABLES (lesmis_edges SOURCE KEY (src) DESTINATION KEY (dst));";

/// Getting Started, "Weights".
#[test]
fn weights_match_getting_started() {
    let sql = format!(
        "{LESMIS}
         SELECT * FROM pagerank(lesmis, weight => weight, direction => 'both')
         ORDER BY pagerank DESC
         LIMIT 5;"
    );
    assert_eq!(
        ok(ursa(&["-c", &sql])),
        "\
+----------+----------------------+
| id       | pagerank             |
+----------+----------------------+
| Valjean  | 0.09956068175644767  |
| Marius   | 0.051667656904091475 |
| Myriel   | 0.03923416880292588  |
| Cosette  | 0.03691018363299517  |
| Enjolras | 0.036614491459848165 |
+----------+----------------------+
"
    );
    // The page's second query returns all 77 characters, in six communities;
    // ordered and cut to five rows it prints a table short enough for the page.
    let sql =
        format!("{LESMIS} SELECT * FROM louvain(lesmis, weight => log(1 + weight), seed => 7);");
    let out = ok(ursa(&["-f", "csv", "-c", &sql]));
    assert_eq!(out.lines().count(), 78, "{out}");
    let communities: std::collections::BTreeSet<&str> = out
        .lines()
        .skip(1)
        .map(|l| l.rsplit(',').next().unwrap())
        .collect();
    assert_eq!(communities.len(), 6, "{out}");
    let sql = format!(
        "{LESMIS}
         SELECT * FROM louvain(lesmis, weight => log(1 + weight), seed => 7)
         ORDER BY id
         LIMIT 5;"
    );
    assert_eq!(
        ok(ursa(&["-c", &sql])),
        "\
+------------+---------+
| id         | louvain |
+------------+---------+
| Anzelma    | 3       |
| Babet      | 3       |
| Bahorel    | 5       |
| Bamatabois | 1       |
| BaronessT  | 4       |
+------------+---------+
"
    );
    assert_eq!(
        err(ursa(&[
            "-c",
            &format!("{LESMIS} SELECT * FROM pagerank(lesmis, weight => weight - 2)")
        ])),
        "Error: weight expression produced a negative value (-1); weights must be non-negative\n"
    );
}

/// Getting Started, "Subgraphs".
#[test]
fn subgraphs_match_getting_started() {
    let sql = format!(
        "{LESMIS}
         CREATE VIEW strong_ties AS SELECT * FROM lesmis_edges WHERE weight >= 3;

         CREATE PROPERTY GRAPH lesmis_strong
           EDGE TABLES (strong_ties SOURCE KEY (src) DESTINATION KEY (dst));

         SELECT count(DISTINCT connected_components) AS groups
         FROM connected_components(lesmis_strong);"
    );
    assert_eq!(
        ok(ursa(&["-c", &sql])),
        "\
+--------+
| groups |
+--------+
| 3      |
+--------+
"
    );
}

/// Getting Started, "Find out what is available": `ursa_functions()`, which
/// an agent reads as JSON.
#[test]
fn ursa_functions_lists_the_catalog() {
    let out = ok(ursa(&[
        "-f",
        "json",
        "-c",
        "SELECT * FROM ursa_functions() WHERE name = 'pagerank'",
    ]));
    assert_eq!(
        out,
        "[{\"name\":\"pagerank\",\"family\":\"algorithm\",\
\"summary\":\"PageRank (pull-based fixpoint); a dangling node spreads its score to all nodes.\",\
\"networkx\":\"nx.pagerank(G, alpha=damping) (both: nx.pagerank(U))\",\
\"parameters\":[\
{\"name\":\"damping\",\"type\":\"number\",\"default\":\"0.85\",\"choices\":[],\
\"doc\":\"Probability of following an edge.\"},\
{\"name\":\"max_iter\",\"type\":\"integer\",\"default\":\"30\",\"choices\":[],\
\"doc\":\"Iteration cap.\"},\
{\"name\":\"tol\",\"type\":\"number\",\"default\":\"1e-6\",\"choices\":[],\
\"doc\":\"Convergence tolerance on the L1 change.\"},\
{\"name\":\"weight\",\"type\":\"expression\",\"default\":\"NULL\",\"choices\":[],\
\"doc\":\"A non-negative weight per edge; null is unweighted.\"},\
{\"name\":\"direction\",\"type\":\"string\",\"default\":\"'out'\",\"choices\":[\"out\",\"both\"],\
\"doc\":\"Follow the rows as stored, or read the graph as undirected.\"}],\
\"directions\":[\"out\",\"both\"],\
\"outputs\":[{\"name\":\"pagerank\",\"type\":\"Float64\",\"nullable\":false}]}]\n"
    );
    let out = ok(ursa(&[
        "-f",
        "csv",
        "-c",
        "SELECT name, family, cardinality(parameters) AS parameters, \
         array_to_string(directions, ' ') AS directions FROM ursa_functions()",
    ]));
    assert_eq!(
        out,
        "\
name,family,parameters,directions
degree,algorithm,1,out in both
pagerank,algorithm,5,out both
connected_components,algorithm,1,
triangle_count,algorithm,0,
clustering_coefficient,algorithm,0,
betweenness,algorithm,4,out both
closeness,algorithm,2,out in both
label_propagation,algorithm,2,
louvain,algorithm,3,
layout_fr,layout,4,
layout_random,layout,2,
layout_circle,layout,1,
density,statistic,1,out in both
avg_path_length,statistic,2,out in both
diameter,statistic,2,out in both
describe,statistic,3,out in both
algorithms,combinator,0,
"
    );
    assert_eq!(
        err(ursa(&["-c", "SELECT * FROM ursa_functions('pagerank')"])),
        "Error: ursa_functions() takes no arguments: SELECT * FROM ursa_functions()\n"
    );
}
