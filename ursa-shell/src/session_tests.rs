//! The session's operations, as a host drives them. The commands themselves
//! are tested in `shell.rs` (`explore_tests.rs`, `find_tests.rs`), and what
//! `graph::view` draws in `ursa-engine`.

use super::*;
use ursa_engine::graph::view::GraphView;

/// A session past its `hello`.
fn session() -> Session {
    Session {
        greeted: true,
        ..Session::new(Options::default())
    }
}

/// The graph `text` opens, as the engine made it: what
/// [`Session::open_graph`] writes the bytes of.
async fn graph_of(session: &mut Session, text: &str) -> Result<GraphView> {
    let (skipped, positions) = session.shell.open_graph_placed(text).await?;
    let result = session
        .shell
        .explorer()
        .expect("a graph was opened")
        .result();
    Ok(GraphView {
        result: result.clone(),
        skipped,
        positions,
        groups: None,
    })
}

async fn run(shell: &mut Session, line: &str) -> Value {
    let fed = shell.feed(line).await;
    json!({"out": fed.out, "err": fed.err, "flow": fed.flow, "command": fed.command})
}

/// `op` with `args` and no parts, which must succeed: its result.
async fn op(shell: &mut Session, op: &str, args: Value) -> Value {
    shell.handle(op, args, Vec::new()).await.unwrap().result
}

#[tokio::test]
async fn a_line_at_a_time_as_the_cli() {
    let mut shell = session();
    let started = op(&mut shell, "start", json!({})).await;
    assert_eq!(started["prompt"], "memory U ");
    assert_eq!(started["version"], env!("CARGO_PKG_VERSION"));
    let first = op(&mut shell, "feed", json!({"line": "SELECT 42 AS answer,"})).await;
    assert_eq!(first["out"], "");
    assert_eq!(first["pending"], true);
    assert_eq!(first["continuation"], "         ");
    let done = op(&mut shell, "feed", json!({"line": "  'x' AS y;"})).await;
    let out = done["out"].as_str().unwrap();
    assert!(out.starts_with("┌"), "{out}");
    assert!(out.contains("│     42 │"), "{out}");
    // Types are dimmed for the terminal.
    assert!(out.contains("\x1b[90m"), "{out}");
    assert_eq!(done["flow"], "continue");
    assert_eq!(done["pending"], false);
    let error = run(&mut shell, "SELECT nope;").await;
    assert!(
        error["err"]
            .as_str()
            .unwrap()
            .starts_with("\x1b[31mError: "),
        "{error}"
    );
    assert_eq!(run(&mut shell, ".quit").await["flow"], "exit");
}

#[tokio::test]
async fn what_is_typed_is_coloured_after_what_is_pending() {
    let mut shell = session();
    let string = format!("{}'a{}", highlight::STRING, highlight::RESET);
    assert_eq!(shell.highlight("'a", false), string);
    run(&mut shell, "SELECT 'it''s").await;
    // The string goes on: the quote ends it.
    let next = op(&mut shell, "highlightAnsi", json!({"text": "b' AS s;"})).await;
    let next = next["text"].as_str().unwrap();
    assert!(
        next.starts_with(&format!("{}b'{}", highlight::STRING, highlight::RESET)),
        "{next:?}"
    );
    // On its own, the same text opens a string.
    let alone = op(
        &mut shell,
        "highlightAnsi",
        json!({"text": "b' AS s;", "alone": true}),
    )
    .await;
    assert!(alone["text"].as_str().unwrap().starts_with("b\x1b[33m'"));
    assert_eq!(
        op(&mut shell, "cancelInput", json!({})).await["prompt"],
        "memory U "
    );
    assert!(shell
        .highlight(".tables", false)
        .starts_with(highlight::COMMAND));
}

#[tokio::test]
async fn tables_fit_the_terminal() {
    let mut shell = session();
    op(&mut shell, "width", json!({"columns": 30})).await;
    let wide = run(
        &mut shell,
        "SELECT 1 AS a, 2 AS b, 3 AS c, 4 AS d, 5 AS e, 6 AS f;",
    )
    .await;
    let out = wide["out"].as_str().unwrap();
    assert!(out.contains("6 columns ("), "{out}");
}

#[tokio::test]
async fn start_is_a_fresh_session_with_the_hosts_commands() {
    let mut shell = session();
    run(&mut shell, "CREATE VIEW v AS SELECT 1 AS x;").await;
    let commands = json!([[".share", "", "Copy a link that replays this session"]]);
    op(
        &mut shell,
        "start",
        json!({"commands": commands, "shortcuts": "Ctrl+K  clear", "graphMaxEdges": 1234}),
    )
    .await;
    let share = run(&mut shell, ".share").await;
    assert_eq!(share["flow"], "host");
    assert_eq!(share["command"], ".share");
    assert!(run(&mut shell, ".help").await["out"]
        .as_str()
        .unwrap()
        .contains(".share"));
    assert!(run(&mut shell, ".help shortcuts").await["out"]
        .as_str()
        .unwrap()
        .contains("Ctrl+K"));
    // What was defined is gone, and the page's setting is made.
    assert!(run(&mut shell, "SELECT * FROM v;").await["err"]
        .as_str()
        .unwrap()
        .contains("Error"));
    let set = run(&mut shell, "SHOW ursa.graph_max_edges;").await;
    assert!(set["out"].as_str().unwrap().contains("1234"), "{set}");
}

#[tokio::test]
async fn operations_fail_with_a_kind_and_a_message() {
    let mut shell = session();
    let unknown = shell
        .handle("nope", json!({}), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(unknown.kind, "protocol");
    let missing = shell
        .handle("feed", json!({}), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(missing.kind, "protocol");
    let none = shell
        .handle("export", json!({"format": "csv"}), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(
        (none.kind, none.message.as_str()),
        ("sql", "no result to export yet; run a query first")
    );
    let outcome = Err(none);
    assert_eq!(
        envelope(Some(7), &outcome),
        json!({"id": 7, "ok": false, "error": {"kind": "sql", "message": "no result to export yet; run a query first"}})
    );
}

#[tokio::test]
async fn export_and_files_go_as_parts() {
    let mut shell = session();
    run(&mut shell, "SELECT * FROM (VALUES (1), (2)) AS t(x);").await;
    let reply = shell
        .handle("export", json!({"format": "csv"}), Vec::new())
        .await
        .unwrap();
    assert_eq!(reply.parts.len(), 1);
    assert_eq!(
        (reply.parts[0].name, reply.parts[0].kind),
        ("bytes", PartKind::Bytes)
    );
    assert_eq!(reply.parts[0].bytes, b"x\n1\n2\n");
    let meta = envelope(None, &Ok(reply));
    assert_eq!(
        meta["parts"],
        json!([{"name": "bytes", "kind": "bytes", "bytes": 6}])
    );

    let added = shell
        .handle(
            "addFile",
            json!({"name": "e.csv"}),
            vec![b"src,dst\n1,2\n".to_vec()],
        )
        .await
        .unwrap();
    assert_eq!(added.result["url"], "memory://uploads/e.csv");
    let files = op(&mut shell, "files", json!({})).await;
    assert_eq!(files["files"][0]["name"], "e.csv");
    // A session without files says so.
    let mut bare = Session::new(Options {
        uploads: false,
        ..Options::default()
    });
    op(&mut bare, "hello", json!({"protocol": 1})).await;
    assert_eq!(op(&mut bare, "files", json!({})).await["files"], json!([]));
    let refused = bare
        .handle("addFile", json!({"name": "e.csv"}), vec![vec![]])
        .await;
    assert!(refused.unwrap_err().message.contains("keeps no files"));
}

#[tokio::test]
async fn an_added_file_is_read_by_its_url() {
    let mut shell = session();
    let url = add_upload(
        shell.uploads().unwrap(),
        "edges.csv",
        b"src,dst\n1,2\n2,3\n".to_vec(),
    )
    .await
    .unwrap();
    assert_eq!(url, "memory://uploads/edges.csv");
    let out = run(&mut shell, &format!("SELECT count(*) AS n FROM '{url}';")).await;
    assert!(out["out"].as_str().unwrap().contains("│     2 │"), "{out}");
    // Names a URL would read otherwise are kept under safe ones.
    let mut urls = Vec::new();
    for (name, kept) in [
        ("données.csv", "donn_es-"),
        ("a#b.csv", "a_b-"),
        ("a?b.csv", "a_b-"),
        ("50%.csv", "50_-"),
        ("what?.csv", "what_-"),
        ("[x] y*.csv", "_x__y_-"),
        ("it's.csv", "it_s-"),
    ] {
        let bytes = b"src,dst\n1,2\n2,3\n".to_vec();
        let url = add_upload(shell.uploads().unwrap(), name, bytes)
            .await
            .unwrap();
        assert!(
            url.starts_with(&format!("memory://uploads/{kept}")),
            "{url}"
        );
        assert!(url.ends_with(".csv"), "{url}");
        let out = run(&mut shell, &format!("SELECT count(*) AS n FROM '{url}';")).await;
        assert!(
            out["out"].as_str().unwrap().contains("│     2 │"),
            "{name}: {out}"
        );
        urls.push(url);
    }
    // Names that differ only in what was replaced are kept apart.
    urls.sort();
    urls.dedup();
    assert_eq!(urls.len(), 7, "{urls:?}");
    assert_eq!(upload_key("edges.csv"), "edges.csv");
    for bad in ["", "a/b.csv", "..", ".hidden"] {
        assert!(
            add_upload(shell.uploads().unwrap(), bad, vec![])
                .await
                .is_err(),
            "{bad}"
        );
    }
}

/// #517: `COPY … TO` in a session with files writes one of them, which a
/// later read finds by its name.
#[tokio::test]
async fn copy_writes_a_session_file_that_reads_back_by_name() {
    let mut shell = session();
    run(
        &mut shell,
        "CREATE TABLE t AS SELECT * FROM (VALUES (1, 'a'), (2, 'b'), (3, NULL)) v(n, s);",
    )
    .await;
    for (target, read) in [
        ("t.parquet", "t.parquet"),
        ("t.csv", "t.csv"),
        ("t.json", "t.json"),
        ("t.arrow", "t.arrow"),
        ("memory://uploads/u.parquet", "u.parquet"),
        ("./v.csv", "memory://uploads/v.csv"),
    ] {
        let out = run(&mut shell, &format!("COPY t TO '{target}';")).await;
        assert_eq!(out["err"], "", "{target}");
        assert!(out["out"].as_str().unwrap().contains("│      3 │"), "{out}");
        let out = run(
            &mut shell,
            &format!("SELECT concat(count(*), '-', sum(n), '-', count(s)) AS r FROM '{read}';"),
        )
        .await;
        assert_eq!(out["err"], "", "{read}");
        assert!(
            out["out"].as_str().unwrap().contains("3-6-2"),
            "{read}: {out}"
        );
    }
    let files = list_uploads(shell.uploads().unwrap()).await.unwrap();
    let names: Vec<_> = files
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "t.arrow",
            "t.csv",
            "t.json",
            "t.parquet",
            "u.parquet",
            "v.csv"
        ]
    );
    assert_eq!(files[3]["url"], "memory://uploads/t.parquet");
    assert!(files[3]["bytes"].as_u64().unwrap() > 0);

    // A query, options in both spellings, and a file written again.
    let out = run(
        &mut shell,
        "COPY (SELECT n FROM t WHERE n > 1) TO 't.csv' (HEADER false, DELIMITER '|');",
    )
    .await;
    assert_eq!(out["err"], "", "{out}");
    let out = run(
        &mut shell,
        "SELECT * FROM read_csv('t.csv', header = false);",
    )
    .await;
    let out = out["out"].as_str().unwrap();
    assert!(
        out.contains("column_1") && out.contains("│        3 │"),
        "{out}"
    );
    let out = run(
        &mut shell,
        "COPY t TO 'z.parquet' OPTIONS ('format.compression' 'snappy');",
    )
    .await;
    assert_eq!(out["err"], "", "{out}");
    let out = run(
        &mut shell,
        "COPY (SELECT 1 AS x WHERE false) TO 'none.csv';",
    )
    .await;
    assert_eq!(out["err"], "", "{out}");
    let out = run(&mut shell, "COPY t TO 'out' STORED AS PARQUET;").await;
    assert_eq!(out["err"], "", "{out}");
    let out = run(&mut shell, "SELECT count(*) AS n FROM read_parquet('out');").await;
    assert!(out["out"].as_str().unwrap().contains("│     3 │"), "{out}");
    let out = run(&mut shell, "EXPLAIN COPY t TO 't.parquet';").await;
    assert_eq!(out["err"], "", "{out}");
}

/// #517: the write starts no tokio task. A browser has no tokio runtime,
/// and DataFusion's file sinks, which spawn their writers, panic there:
/// that was the crash. Here the futures run with no runtime, as there.
#[test]
fn copy_needs_no_tokio_runtime() {
    futures::executor::block_on(async {
        let mut shell = session();
        run(&mut shell, "CREATE TABLE t AS SELECT 1 AS n, 'a' AS s;").await;
        for file in ["t.parquet", "t.csv", "t.json", "t.arrow"] {
            let out = run(&mut shell, &format!("COPY t TO 'memory://uploads/{file}';")).await;
            assert_eq!(out["err"], "", "{file}");
            let out = run(&mut shell, &format!("SELECT s FROM '{file}';")).await;
            assert_eq!(out["err"], "", "{file}");
        }
    });
}

/// #517: what the session cannot write is refused, saying what works.
#[tokio::test]
async fn copy_refuses_what_a_session_file_cannot_be() {
    let mut shell = session();
    run(&mut shell, "CREATE TABLE t AS SELECT 1 AS n, 'a' AS s;").await;
    for (sql, says) in [
        ("COPY t TO 'out/t.parquet';", "with no folder"),
        ("COPY t TO '/tmp/t.parquet';", "with no folder"),
        ("COPY t TO 'my file.csv';", "letters, digits"),
        (
            "COPY t TO 'https://example.com/t.parquet';",
            "writes only this session's files",
        ),
        (
            "COPY t TO 's3://bucket/t.parquet';",
            "writes only this session's files",
        ),
        (
            "COPY t TO 't.parquet' PARTITIONED BY (s);",
            "PARTITIONED BY",
        ),
        ("COPY t TO 't.csv' (COMPRESSION gzip);", "compressed CSV"),
        (
            "COPY t TO 't.json' OPTIONS ('format.compression' 'gzip');",
            "compressed JSON",
        ),
        (
            "COPY t TO 't.avro';",
            "end the name in .parquet, .csv, .json or .arrow",
        ),
        ("COPY t TO 't';", "or say STORED AS PARQUET"),
        ("EXPLAIN ANALYZE COPY t TO 't.parquet';", "EXPLAIN ANALYZE"),
    ] {
        let out = run(&mut shell, sql).await;
        let err = out["err"].as_str().unwrap();
        assert!(!err.is_empty() && err.contains(says), "{sql}: {out}");
    }
    assert_eq!(
        list_uploads(shell.uploads().unwrap()).await.unwrap(),
        json!([])
    );
    // An INSERT into a table of files runs the same sinks: refused too,
    // where one into a table in memory runs.
    for sql in [
        "CREATE EXTERNAL TABLE d (n BIGINT) STORED AS CSV LOCATION 'memory://uploads/d/';",
        "CREATE TABLE m (n BIGINT);",
        "INSERT INTO m VALUES (2);",
    ] {
        assert_eq!(run(&mut shell, sql).await["err"], "", "{sql}");
    }
    for sql in [
        "INSERT INTO d VALUES (2);",
        "EXPLAIN ANALYZE INSERT INTO d VALUES (2);",
    ] {
        let out = run(&mut shell, sql).await;
        assert!(
            out["err"]
                .as_str()
                .unwrap()
                .contains("d is a table of files"),
            "{sql}: {out}"
        );
    }
}

/// #517 with #516: a layout saved with `COPY` draws from its file.
#[tokio::test]
async fn saved_positions_round_trip_through_a_session_file() {
    let mut shell = session();
    for sql in [
        "CREATE TABLE e AS SELECT * FROM (VALUES (0, 1), (1, 2), (2, 0), (2, 3)) v(src, dst);",
        "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (src) DESTINATION KEY (dst));",
    ] {
        assert_eq!(run(&mut shell, sql).await["err"], "", "{sql}");
    }
    let out = run(
        &mut shell,
        "COPY (SELECT * FROM layout_fa2(g)) TO 'pos.parquet';",
    )
    .await;
    assert_eq!(out["err"], "", "{out}");
    let g = graph_of(&mut shell, "g POSITIONS (SELECT * FROM 'pos.parquet')")
        .await
        .unwrap();
    assert_eq!(g.result.node_keys().len(), 4);
    assert_eq!(g.positions.map(|p| p.x.len()), Some(4));
    // The protocol's graph.open: the graph as UGR, and the places as f32s,
    // every x then every y.
    let reply = shell
        .handle(
            "graph.open",
            json!({"text": "g POSITIONS (SELECT * FROM 'pos.parquet')"}),
            Vec::new(),
        )
        .await
        .unwrap();
    let kinds: Vec<_> = reply
        .parts
        .iter()
        .map(|p| (p.name, p.kind, p.bytes.len()))
        .collect();
    assert_eq!(kinds[1], ("positions", PartKind::F32, 8 * 4));
    assert_eq!(kinds[0].0, "ugr");
    assert_eq!(reply.result, json!({"skipped": 0}));
}

fn ids(g: &GraphView) -> Vec<&str> {
    g.result.node_keys().iter().collect()
}

/// The whole of `.graph`, through the shell's SQL and its last result.
/// What `graph::view` draws of each source and style is tested in
/// `ursa-engine` (`tests/graph_view.rs`).
#[tokio::test]
async fn graph_draws_the_last_result_a_query_or_a_property_graph() {
    let mut shell = session();
    let err = graph_of(&mut shell, "").await.unwrap_err();
    assert!(err.to_string().contains("no result to draw yet"), "{err}");

    run(&mut shell, "CREATE VIEW people AS SELECT * FROM (VALUES ('ann'), ('bob'), ('cy'), ('dee')) AS t(name);").await;
    run(&mut shell, "CREATE VIEW knows AS SELECT * FROM (VALUES ('bob', 'ann', 1), ('cy', 'bob', 2)) AS t(a, b, w);").await;
    run(&mut shell, "SELECT w, a AS Src, b AS DST FROM knows;").await;
    // The last result: src and dst by name, not position.
    let g = graph_of(&mut shell, "").await.unwrap();
    assert_eq!(ids(&g), ["bob", "ann", "cy"]);
    assert_eq!((g.result.src(), g.result.dst()), (&[0, 2][..], &[1, 0][..]));
    assert_eq!(g.result.provenance().rule.as_deref(), Some("edges"));

    // A query, with or without its semicolon; the first two columns.
    let g = graph_of(&mut shell, " SELECT b, a FROM knows; ")
        .await
        .unwrap();
    assert_eq!(ids(&g), ["ann", "bob", "cy"]);
    assert_eq!(
        g.result.provenance().sql.as_deref(),
        Some("SELECT b, a FROM knows")
    );
    // Ursa's dialect: a FROM-first query.
    let g = graph_of(&mut shell, "FROM knows").await.unwrap();
    assert_eq!(g.result.n_nodes(), 3);
    // A table by name.
    let g = graph_of(&mut shell, "knows").await.unwrap();
    assert_eq!(g.result.n_nodes(), 3);

    // A property graph: its vertices first, so dee, with no edges, is drawn.
    run(&mut shell, "CREATE PROPERTY GRAPH social VERTEX TABLES (people KEY (name)) EDGE TABLES (knows SOURCE KEY (a) REFERENCES people (name) DESTINATION KEY (b) REFERENCES people (name));").await;
    let g = graph_of(&mut shell, "Social").await.unwrap();
    assert_eq!(ids(&g), ["ann", "bob", "cy", "dee"]);
    assert_eq!((g.result.src(), g.result.dst()), (&[1, 2][..], &[0, 1][..]));
    assert_eq!(g.result.graph(), Some("social"));

    // What cannot be drawn says why.
    for (text, says) in [
        ("nothing_here", "no graph or table named 'nothing_here'"),
        ("SELECT 1 AS x", "1 column"),
        ("SELECT * FROM knows WHERE false", "no rows"),
        ("SELECT nope", "nope"),
        ("-- nothing", "no query to draw"),
    ] {
        let err = graph_of(&mut shell, text).await.unwrap_err();
        assert!(
            ursa_engine::user_message(&err).contains(says),
            "{text}: {err}"
        );
    }
    run(&mut shell, "SELECT 42 AS answer;").await;
    let err = graph_of(&mut shell, "").await.unwrap_err();
    assert!(err.to_string().contains("1 column"), "{err}");
}

/// An explore command changes the shell's open graph and gives the page
/// what it printed and the change as UGR bytes. The commands themselves
/// are tested in `ursa-shell` (`explore_tests.rs`).
#[tokio::test]
async fn explore_gives_the_page_a_line_and_a_delta() {
    use ursa_viz::model::{GraphDelta, GraphResult};
    let mut shell = session();
    // The commands are the shell's own: listed, and handed back.
    let help = run(&mut shell, ".help").await;
    assert!(
        help["out"].as_str().unwrap().contains(".expand ID"),
        "{help}"
    );
    let back = run(&mut shell, ".expand 3;").await;
    assert_eq!(
        (back["flow"].as_str(), back["command"].as_str()),
        (Some("host"), Some(".expand 3"))
    );
    let none = shell.explore(".expand 3").await;
    assert!(none.err.contains("Error: no graph is open"), "{}", none.err);
    assert!(none.out.is_empty() && none.delta.is_empty());

    run(&mut shell, "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2), (2, 3), (3, 1), (3, 4), (4, 5)) AS t(src, dst);").await;
    run(
        &mut shell,
        "CREATE PROPERTY GRAPH club EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
    )
    .await;
    let g = graph_of(
        &mut shell,
        "SELECT * FROM hop(club, seeds => [1], direction => 'both')",
    )
    .await
    .unwrap();
    let mut view = GraphResult::from_bytes(&g.result.to_bytes()).unwrap();
    assert_eq!((view.n_nodes(), view.n_edges()), (3, 3));

    let expanded = shell.explore(".expand 3").await;
    assert_eq!(
        (expanded.out.clone(), expanded.err.clone()),
        ("+1 node, +1 edge\n".to_string(), String::new())
    );
    assert_eq!((expanded.nodes, expanded.edges), (4, 4));
    let bytes = expanded.delta;
    view.apply(&GraphDelta::from_bytes(&bytes).unwrap())
        .unwrap();
    assert_eq!(
        view.node_keys().iter().collect::<Vec<_>>(),
        ["1", "2", "3", "4"]
    );

    let undone = shell.explore(".undo").await;
    assert_eq!(undone.out, "Undid .expand 3: -1 node, -1 edge\n");
    view.apply(&GraphDelta::from_bytes(&undone.delta).unwrap())
        .unwrap();
    assert_eq!(view.to_bytes(), g.result.to_bytes());

    let missing = shell.explore(".hide 9").await;
    assert!(
        missing
            .err
            .contains("no node '9' in the graph that is drawn"),
        "{}",
        missing.err.clone()
    );
    assert_eq!((missing.nodes, missing.edges), (3, 3));
    shell.close_graph();
    assert!(shell
        .explore(".undo")
        .await
        .err
        .contains("no graph is open"));
}

/// `.find` gives the page what it printed and the nodes to select. The
/// command itself is tested in `ursa-shell` (`find_tests.rs`).
#[tokio::test]
async fn find_gives_the_page_a_line_and_the_nodes_to_select() {
    let mut shell = session();
    let back = run(&mut shell, ".find 3;").await;
    assert_eq!(
        (back["flow"].as_str(), back["command"].as_str()),
        (Some("host"), Some(".find 3"))
    );
    let none = shell.find(".find 3").await;
    assert!(none.err.contains("Error: no graph is open"), "{}", none.err);
    assert!(none.selected.is_empty() && none.not_shown == json!([]));

    run(&mut shell, "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2), (2, 3), (3, 1), (3, 4), (4, 35)) AS t(src, dst);").await;
    run(
        &mut shell,
        "CREATE PROPERTY GRAPH club EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
    )
    .await;
    graph_of(
        &mut shell,
        "SELECT * FROM hop(club, seeds => [1], direction => 'both')",
    )
    .await
    .unwrap();
    let found = shell.find(".find 3").await;
    assert_eq!(
        (found.out.clone(), found.err.clone()),
        (
            "Selected 1 node\nNot shown: 35\n".to_string(),
            String::new()
        )
    );
    assert_eq!((found.matches, found.not_shown_total), (1, 1));
    assert_eq!(found.not_shown, json!([{"hidden": false, "key": "35"}]));
    // Node 3 is the view's third: hop gives 1, 2, 3.
    assert_eq!(found.selected, [2]);
}

#[tokio::test]
async fn inspect_gives_the_page_a_row_as_json() {
    let mut shell = session();
    let node = Inspect::Node { node: 0, key: "1" };
    let none = shell.inspect(node, false).await.unwrap_err();
    assert!(none.to_string().contains("no graph is open"), "{none}");

    run(&mut shell, "CREATE TABLE people AS SELECT * FROM (VALUES (1, '<i>Ann</i>'), (2, NULL)) AS t(id, name);").await;
    run(
        &mut shell,
        "CREATE TABLE calls AS SELECT * FROM (VALUES (1, 2, 0.5)) AS t(src, dst, hours);",
    )
    .await;
    run(
        &mut shell,
        "CREATE PROPERTY GRAPH phone VERTEX TABLES (people KEY (id)) EDGE TABLES (calls \
         SOURCE KEY (src) REFERENCES people (id) DESTINATION KEY (dst) REFERENCES people (id));",
    )
    .await;
    run(&mut shell, "SELECT 41 AS kept;").await;
    graph_of(&mut shell, "phone").await.unwrap();

    let row: serde_json::Value = shell.inspect(node, false).await.unwrap();
    assert_eq!(
        row,
        json!({
            "table": "people",
            "sql": "SELECT * FROM people WHERE id = 1",
            "columns": [{"name": "id", "numeric": true}, {"name": "name", "numeric": false}],
            "rows": [["1", "<i>Ann</i>"]],
            "row": null,
            "note": null,
            "indexed": false,
        })
    );
    let null = Inspect::Node { node: 1, key: "2" };
    let row: serde_json::Value = shell.inspect(null, false).await.unwrap();
    assert_eq!(row["rows"], json!([["2", null]]));
    assert_eq!(
        shell.inspect(node, true).await.unwrap(),
        json!({"neighbours": 1})
    );

    let edge = Inspect::Edge {
        edge: 0,
        src: "1",
        dst: "2",
    };
    let row: serde_json::Value = shell.inspect(edge, false).await.unwrap();
    assert_eq!(row["table"], "calls");
    assert_eq!(row["rows"], json!([["1", "2", "0.5"]]));
    // Found through the graph's index, not by a scan of the edge table.
    assert_eq!(row["indexed"], true);

    // The page's picture is not the engine's: an error, not another row.
    let stale = Inspect::Node { node: 0, key: "2" };
    assert!(shell.inspect(stale, false).await.is_err());
    // Nothing was printed, and the last result is the user's still.
    assert_eq!(shell.out.take(), "");
    let (schema, _) = shell.shell.last_result().unwrap();
    assert_eq!(schema.field(0).name(), "kept");
}

#[tokio::test]
async fn graph_styles_reach_the_page_as_ugr_bytes() {
    use ursa_viz::model::{GraphResult, Values};
    let mut shell = session();
    // Two triangles joined by one edge: two communities.
    run(&mut shell, "CREATE VIEW people AS SELECT * FROM (VALUES (1, 'red', 10, 'a'), (2, 'red', 20, 'b'), (3, 'red', 30, 'c'), (4, 'blue', 40, 'd'), (5, 'blue', 50, 'e'), (6, 'blue', 60, 'f')) AS t(id, side, age, name);").await;
    run(&mut shell, "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2), (2, 3), (3, 1), (4, 5), (5, 6), (6, 4), (3, NULL), (3, 4)) AS t(src, dst);").await;
    run(&mut shell, "CREATE PROPERTY GRAPH club VERTEX TABLES (people KEY (id)) EDGE TABLES (ties SOURCE KEY (src) REFERENCES people (id) DESTINATION KEY (dst) REFERENCES people (id));").await;

    // Algorithms, run on the graph through the shell's SQL, joined on id.
    let g = graph_of(&mut shell, "club SIZE BY pagerank COLOR BY louvain")
        .await
        .unwrap();
    assert_eq!(g.legend().unwrap().counts, vec![3, 3]);
    // The page's copy: the bytes once, and what was skipped.
    assert_eq!(g.skipped, 1);
    let view = g;
    let data = shell
        .open_graph("club SIZE BY pagerank COLOR BY louvain")
        .await
        .unwrap();
    assert_eq!(data.skipped, 1);
    let bytes = data.ugr;
    let read = GraphResult::from_bytes(&bytes).unwrap();
    assert_eq!(read, view.result);
    assert_eq!(read.encodings().size_by.as_deref(), Some("pagerank"));
    assert_eq!(read.encodings().color_by.as_deref(), Some("louvain"));
    let Values::F32(size) = &read.node_column("pagerank").unwrap().values else {
        panic!("pagerank is numbers");
    };
    // 3 and 4, the bridge's ends, rank highest.
    assert!(size[2] > size[0] && size.len() == 6, "{size:?}");

    // The last result keeps what its columns are, so the rules read it:
    // a hop's rings, with the graph's edges.
    run(
        &mut shell,
        "SELECT * FROM hop(club, seeds => [3], steps => 1, direction => 'both');",
    )
    .await;
    let g = graph_of(&mut shell, "").await.unwrap();
    assert_eq!(g.drawn_as(), "rings on club (seed, id, distance)");
    // 3 and its neighbours 1, 2 and 4: the triangle and the bridge.
    assert_eq!((g.result.n_nodes(), g.result.n_edges()), (4, 4));

    // A query with a subquery in NODES, and an error from a clause.
    let g = graph_of(&mut shell, "FROM ties NODES (FROM people) SIZE BY age")
        .await
        .unwrap();
    assert_eq!(g.result.encodings().size_by.as_deref(), Some("age"));
    let err = graph_of(&mut shell, "ties COLOR BY louvain")
        .await
        .unwrap_err();
    assert!(
        ursa_engine::user_message(&err)
            .contains("draw the graph by name (.graph NAME COLOR BY louvain)"),
        "{err}"
    );
}

#[tokio::test]
async fn the_last_result_is_kept_for_export() {
    let mut shell = session();
    run(&mut shell, "SELECT * FROM (VALUES (1), (2), (3)) AS t(x);").await;
    let (_, batches) = shell.shell.last_result().unwrap();
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 3);
}

const TIES: &str = "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2), (2, 3), (3, 1), (3, 4), (4, 5)) AS t(src, dst);";
const CLUB: &str =
    "CREATE PROPERTY GRAPH club EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));";

/// `hello` must come first, says what the host is, and refuses a protocol
/// version other than 1.
#[tokio::test]
async fn hello_comes_first_and_speaks_protocol_1() {
    let mut shell = Session::new(Options::default());
    let early = shell
        .handle("feed", json!({"line": "SELECT 1;"}), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(
        (early.kind, early.message.as_str()),
        ("protocol", "the first request must be hello, not feed")
    );
    for (args, message) in [
        (json!({}), "this session speaks protocol 1, not none"),
        (
            json!({"protocol": 2}),
            "this session speaks protocol 1, not 2",
        ),
        (
            json!({"protocol": "1"}),
            "this session speaks protocol 1, not \"1\"",
        ),
        (
            json!({"protocol": 1, "session": "mine"}),
            "hello's session is \"new\" or \"shared\", not \"mine\"",
        ),
    ] {
        let refused = shell.handle("hello", args, Vec::new()).await.unwrap_err();
        assert_eq!(
            (refused.kind, refused.message.as_str()),
            ("protocol", message)
        );
    }
    // A refused hello is no hello.
    assert!(shell.handle("files", json!({}), Vec::new()).await.is_err());

    let hello = op(
        &mut shell,
        "hello",
        json!({"protocol": 1, "client": {"name": "test", "kind": "human"}, "session": "new"}),
    )
    .await;
    assert_eq!(
        hello,
        json!({
            "protocol": 1,
            "version": env!("CARGO_PKG_VERSION"),
            "transport": "native",
            "capabilities": [],
            "session": {"kind": "new"},
        })
    );
    let shared = op(&mut shell, "hello", json!({"protocol": 1})).await;
    assert_eq!(shared["session"], json!({"kind": "shared"}));
    // `start` is a fresh shell, not a fresh conversation.
    op(&mut shell, "start", json!({})).await;
    op(&mut shell, "feed", json!({"line": "SELECT 1;"})).await;
}

/// The name the protocol does not have fails before `hello` is asked for.
#[tokio::test]
async fn an_unknown_op_is_a_protocol_error() {
    let mut shell = Session::new(Options::default());
    let unknown = shell
        .handle("cancelled", json!({}), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(
        (unknown.kind, unknown.message.as_str()),
        ("protocol", "unknown op cancelled")
    );
}

/// `cancel` reaches the session only once nothing runs: there is nothing to
/// stop. `cancelInput` drops what is typed.
#[tokio::test]
async fn cancel_stops_nothing_and_cancel_input_drops_the_statement() {
    let mut shell = session();
    assert_eq!(
        op(&mut shell, "cancel", json!({"target": 3})).await,
        json!({})
    );
    let typed = op(&mut shell, "feed", json!({"line": "SELECT 1"})).await;
    assert_eq!(typed["pending"], true);
    let dropped = op(&mut shell, "cancelInput", json!({})).await;
    assert_eq!(dropped["prompt"], "memory U ");
    let next = op(&mut shell, "feed", json!({"line": "SELECT 2 AS two;"})).await;
    assert!(next["out"].as_str().unwrap().contains("two"), "{next}");
    assert_eq!(next["err"], "");
}

/// `memory` says what it measured: natively, the resident set.
#[tokio::test]
async fn memory_says_what_it_measured() {
    let mut shell = session();
    let memory = op(&mut shell, "memory", json!({})).await;
    assert_eq!(memory["kind"], "rss");
    if cfg!(target_os = "linux") {
        assert!(memory["bytes"].as_u64().unwrap() > 1 << 20, "{memory}");
    }
}

/// The console's `query`: typed columns with their `ursa.*` metadata, the
/// schema's own metadata, the first rows as Arrow IPC, and the result kept as
/// the last one.
#[tokio::test]
async fn query_gives_typed_columns_metadata_and_arrow_rows() {
    use arrow::ipc::reader::StreamReader;
    let mut shell = session();
    run(&mut shell, TIES).await;
    run(&mut shell, CLUB).await;
    let reply = shell
        .handle(
            "query",
            json!({"sql": "SELECT * FROM pagerank(club) ORDER BY id", "rows": 3}),
            Vec::new(),
        )
        .await
        .unwrap();
    let r = &reply.result;
    assert_eq!(
        (r["rows"].as_u64(), r["more"].as_bool()),
        (Some(5), Some(true))
    );
    assert_eq!(r["columns"][1]["name"], "pagerank");
    assert_eq!(r["columns"][1]["type"], "Float64");
    assert_eq!(r["columns"][0]["metadata"]["ursa.role"], "node", "{r}");
    assert_eq!(r["notices"], json!([]));
    assert_eq!(
        (reply.parts.len(), reply.parts[0].name, reply.parts[0].kind),
        (1, "arrow", PartKind::Arrow)
    );
    let reader = StreamReader::try_new(reply.parts[0].bytes.as_slice(), None).unwrap();
    let schema = reader.schema();
    assert_eq!(reader.map(|b| b.unwrap().num_rows()).sum::<usize>(), 3);
    let role = schema.field(0).metadata().get("ursa.role").cloned();
    assert_eq!(role.as_deref(), Some("node"));
    // Every row is the last result, for export.
    let csv = shell.export("csv").unwrap();
    assert_eq!(String::from_utf8(csv).unwrap().lines().count(), 6);

    // GRAPH_TABLE's pattern is on the schema, not on a column.
    let pattern = shell
        .handle(
            "query",
            json!({"sql": "SELECT * FROM GRAPH_TABLE (club MATCH (a)-[e]->(b) COLUMNS (e.src AS a, e.dst AS b))"}),
            Vec::new(),
        )
        .await
        .unwrap();
    let metadata = &pattern.result["metadata"];
    assert!(metadata["ursa.pattern"].is_string(), "{metadata}");
    let reader = StreamReader::try_new(pattern.parts[0].bytes.as_slice(), None).unwrap();
    assert!(reader.schema().metadata().contains_key("ursa.pattern"));

    // A statement with no rows, and an error.
    let created = op(
        &mut shell,
        "query",
        json!({"sql": "CREATE VIEW v AS SELECT 1 AS x"}),
    )
    .await;
    assert_eq!(
        (created["columns"].clone(), created["notices"].clone()),
        (json!([]), json!(["OK"]))
    );
    let wrong = shell
        .handle("query", json!({"sql": "SELECT nope"}), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(wrong.kind, "sql");
}

/// The graph operations through `handle`, with their parts named.
#[tokio::test]
async fn graph_ops_reply_with_named_parts() {
    use ursa_viz::model::{GraphDelta, GraphResult};
    let mut shell = session();
    run(&mut shell, TIES).await;
    run(&mut shell, CLUB).await;
    let opened = shell
        .handle(
            "graph.open",
            json!({"text": "SELECT * FROM hop(club, seeds => [1], direction => 'both')"}),
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(opened.result, json!({"skipped": 0}));
    // No `positions`: they are only there with POSITIONS.
    let names: Vec<_> = opened.parts.iter().map(|p| (p.name, p.kind)).collect();
    assert_eq!(names, [("ugr", PartKind::Ugr)]);
    let mut view = GraphResult::from_bytes(&opened.parts[0].bytes).unwrap();
    assert_eq!((view.n_nodes(), view.n_edges()), (3, 3));

    let expanded = shell
        .handle("graph.explore", json!({"command": ".expand 3"}), Vec::new())
        .await
        .unwrap();
    assert_eq!(
        expanded.result,
        json!({"out": "+1 node, +1 edge\n", "err": "", "nodes": 4, "edges": 4, "delta": null})
    );
    assert_eq!(
        (expanded.parts[0].name, expanded.parts[0].kind),
        ("delta", PartKind::Ugr)
    );
    view.apply(&GraphDelta::from_bytes(&expanded.parts[0].bytes).unwrap())
        .unwrap();
    assert_eq!(view.n_nodes(), 4);
    // A command that changes nothing has no `delta`.
    let missing = shell
        .handle("graph.explore", json!({"command": ".hide 9"}), Vec::new())
        .await
        .unwrap();
    assert!(missing.parts.is_empty());
    assert!(missing.result["err"]
        .as_str()
        .unwrap()
        .contains("no node '9'"));

    let found = shell
        .handle("graph.find", json!({"command": ".find 4"}), Vec::new())
        .await
        .unwrap();
    assert_eq!(found.result["matches"], 1);
    assert_eq!(
        (found.parts[0].name, found.parts[0].kind),
        ("selected", PartKind::U32)
    );
    assert_eq!(found.parts[0].bytes, 3u32.to_le_bytes());

    let counts = shell
        .handle("graph.notShown", json!({}), Vec::new())
        .await
        .unwrap();
    assert_eq!(
        (counts.parts[0].name, counts.parts[0].kind),
        ("counts", PartKind::U32)
    );
    // 4 nodes shown, a u32 each; node 4's edge to 5 is not shown.
    let counts: Vec<u32> = counts.parts[0]
        .bytes
        .chunks(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(counts, [0, 0, 0, 1]);

    let inspected = op(
        &mut shell,
        "graph.inspect",
        json!({"node": 3, "key": "4", "neighbours": true}),
    )
    .await;
    assert_eq!(inspected["neighbours"], 2);
    assert!(inspected["ms"].is_number());
    let edge = op(
        &mut shell,
        "graph.inspect",
        json!({"edge": 0, "src": "1", "dst": "2"}),
    )
    .await;
    assert_eq!(edge["table"], "ties");
    let neither = shell
        .handle("graph.inspect", json!({}), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(neither.kind, "protocol");

    assert_eq!(op(&mut shell, "graph.close", json!({})).await, json!({}));
    let closed = op(&mut shell, "graph.explore", json!({"command": ".undo"})).await;
    assert!(closed["err"].as_str().unwrap().contains("no graph is open"));

    // POSITIONS gives the second, optional part.
    let placed = shell
        .handle(
            "graph.open",
            json!({"text": "SELECT * FROM ties POSITIONS (SELECT src AS id, src * 1.0 AS x, 0.0 AS y FROM ties)"}),
            Vec::new(),
        )
        .await
        .unwrap();
    let names: Vec<_> = placed.parts.iter().map(|p| (p.name, p.kind)).collect();
    assert_eq!(
        names,
        [("ugr", PartKind::Ugr), ("positions", PartKind::F32)]
    );
}

/// Every operation of [`OPS`] reaches `handle`'s own code: none is unknown.
#[tokio::test]
async fn every_listed_op_is_handled() {
    for o in OPS {
        let mut shell = session();
        if let Err(e) = shell.handle(o.name, json!({}), Vec::new()).await {
            assert!(!e.message.starts_with("unknown op"), "{}: {e:?}", o.name);
        }
    }
}

/// The protocol's document is [`OPS`] as a table: `UPDATE_PROTOCOL_DOC=1
/// cargo test -p ursa-shell protocol_doc` writes it.
#[test]
fn protocol_doc_lists_the_ops() {
    const BEGIN: &str = "<!-- ops: generated from ursa_shell::session::OPS -->\n";
    const END: &str = "<!-- /ops -->";
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/reference/protocol.md");
    let doc = std::fs::read_to_string(path).unwrap();
    let mut table =
        String::from("| op | args | result; parts (kind) | what it does |\n|---|---|---|---|\n");
    for o in OPS {
        table += &format!(
            "| `{}` | {} | {} | {}. |\n",
            o.name, o.args, o.result, o.about
        );
    }
    let start = doc.find(BEGIN).expect("the document marks the table") + BEGIN.len();
    let end = doc[start..].find(END).expect("the table's end is marked") + start;
    if std::env::var_os("UPDATE_PROTOCOL_DOC").is_some() {
        std::fs::write(path, format!("{}{table}{}", &doc[..start], &doc[end..])).unwrap();
    } else {
        assert_eq!(
            doc[start..end],
            table,
            "docs/reference/protocol.md is not OPS: run UPDATE_PROTOCOL_DOC=1 cargo test -p ursa-shell protocol_doc"
        );
    }
}
