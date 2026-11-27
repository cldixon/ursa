//! The console's catalog, sources, completion and highlight operations, and
//! the `catalog` event, through [`Session::handle`].

use super::*;

/// A session past its `hello`, with `options`.
fn session_with(options: Options) -> Session {
    Session {
        greeted: true,
        ..Session::new(options)
    }
}

fn session() -> Session {
    session_with(Options::default())
}

/// A session rooted at `dir`.
fn rooted(dir: &std::path::Path) -> Session {
    session_with(Options {
        root: Some(Root::Dir(dir.to_path_buf())),
        ..Options::default()
    })
}

async fn op(shell: &mut Session, op: &str, args: Value) -> Value {
    match shell.handle(op, args, Vec::new()).await {
        Ok(reply) => reply.result,
        Err(e) => panic!("{op}: {e:?}"),
    }
}

async fn op_err(shell: &mut Session, op: &str, args: Value) -> Error {
    shell.handle(op, args, Vec::new()).await.unwrap_err()
}

/// `op`, and the events it caused, but `activity` (which
/// `session_history_tests.rs` checks).
async fn op_events(shell: &mut Session, name: &str, args: Value) -> Value {
    let _ = shell.handle(name, args, Vec::new()).await;
    let events = shell.take_events().await;
    Value::Array(
        events
            .iter()
            .filter(|e| e.name != "activity")
            .map(Event::to_json)
            .collect(),
    )
}

#[tokio::test]
async fn catalog_lists_tables_views_files_and_graphs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("people.csv"), "id,name\n1,a\n2,b\n").unwrap();
    let mut shell = rooted(dir.path());
    op(
        &mut shell,
        "query",
        json!({"sql": "CREATE TABLE ties (src BIGINT NOT NULL, dst BIGINT) AS VALUES (1, 2), (2, 3); \
                       CREATE VIEW v AS SELECT src FROM ties; \
                       CREATE EXTERNAL TABLE people STORED AS CSV LOCATION 'people.csv' OPTIONS ('format.has_header' 'true'); \
                       CREATE PROPERTY GRAPH g VERTEX TABLES (people KEY (id)) \
                         EDGE TABLES (ties SOURCE KEY (src) REFERENCES people (id) DESTINATION KEY (dst) REFERENCES people (id))"}),
    )
    .await;
    let catalog = op(&mut shell, "catalog", json!({})).await;
    let databases = catalog["databases"].as_array().unwrap();
    assert_eq!(databases.len(), 1, "{catalog}");
    let schemas = databases[0]["schemas"].as_array().unwrap();
    // The graph's own schema (g.nodes, g.edges) is under graphs.
    let names: Vec<_> = schemas
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["public"], "{catalog}");
    let tables = schemas[0]["tables"].as_array().unwrap();
    let kinds: Vec<_> = tables
        .iter()
        .map(|t| (t["name"].as_str().unwrap(), t["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kinds,
        [("people", "external"), ("ties", "table"), ("v", "view")]
    );
    assert_eq!(
        tables[1]["columns"],
        json!([
            {"name": "src", "type": "Int64", "nullable": false},
            {"name": "dst", "type": "Int64", "nullable": true},
        ])
    );
    assert_eq!(
        catalog["graphs"],
        json!([{
            "name": "g",
            "vertices": [{"name": "people", "table": "people", "key": "id"}],
            "edges": [{"name": "ties", "table": "ties", "source": "src", "destination": "dst"}],
        }])
    );
}

#[tokio::test]
async fn the_catalog_event_follows_what_changes_it() {
    let mut shell = session();
    let catalog = json!([{"event": "catalog"}]);
    let fed = |line: &str| json!({"line": line});
    // A query that defines nothing sends none.
    assert_eq!(
        op_events(&mut shell, "feed", fed("SELECT 1;")).await,
        json!([])
    );
    assert_eq!(
        op_events(&mut shell, "feed", fed("CREATE TABLE t AS SELECT 1 AS x;")).await,
        catalog
    );
    assert_eq!(
        op_events(&mut shell, "query", json!({"sql": "SELECT * FROM t"})).await,
        json!([])
    );
    // A statement that fails changes nothing.
    assert_eq!(
        op_events(&mut shell, "feed", fed("CREATE TABLE t AS SELECT 1;")).await,
        json!([])
    );
    assert_eq!(
        op_events(
            &mut shell,
            "query",
            json!({"sql": "CREATE VIEW v AS SELECT x FROM t"})
        )
        .await,
        catalog
    );
    // A typed line that is not yet a statement runs nothing.
    assert_eq!(
        op_events(&mut shell, "feed", fed("DROP VIEW")).await,
        json!([])
    );
    assert_eq!(op_events(&mut shell, "feed", fed("v;")).await, catalog);
    assert_eq!(
        op_events(
            &mut shell,
            "feed",
            fed("CREATE PROPERTY GRAPH g EDGE TABLES (t SOURCE KEY (x) DESTINATION KEY (x));")
        )
        .await,
        catalog
    );
    // ALTER changes a table's columns, not its name.
    // (DataFusion has no ALTER TABLE; a replaced table is the same test.)
    assert_eq!(
        op_events(
            &mut shell,
            "feed",
            fed("CREATE OR REPLACE TABLE t AS SELECT 1 AS x, 2 AS y;")
        )
        .await,
        catalog
    );
    // Other operations don't look.
    assert_eq!(op_events(&mut shell, "catalog", json!({})).await, json!([]));
    // A fresh shell has none of it.
    assert_eq!(op_events(&mut shell, "start", json!({})).await, catalog);
    assert_eq!(op_events(&mut shell, "start", json!({})).await, json!([]));
}

#[tokio::test]
async fn sources_list_a_directory_under_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("edges.parquet"), b"PAR1").unwrap();
    std::fs::write(root.join("people.csv"), "id\n1\n").unwrap();
    std::fs::write(root.join("notes.txt"), "hi").unwrap();
    std::fs::write(root.join(".secret"), "no").unwrap();
    std::fs::create_dir(root.join("more")).unwrap();
    std::fs::write(root.join("more").join("x.jsonl"), "{}\n").unwrap();
    let mut shell = rooted(root);
    let hello = op(&mut shell, "hello", json!({"protocol": 1})).await;
    assert!(
        hello["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("sources")),
        "{hello}"
    );

    let listed = op(&mut shell, "sources.list", json!({"url": ""})).await;
    let real = std::fs::canonicalize(root).unwrap();
    let url = |name: &str| String::from(Url::from_file_path(real.join(name)).unwrap());
    assert_eq!(
        listed["entries"],
        json!([
            {"name": "more", "url": url("more") + "/", "kind": "dir", "format": null, "bytes": null},
            {"name": "edges.parquet", "url": url("edges.parquet"), "kind": "file", "format": "parquet", "bytes": 4},
            {"name": "notes.txt", "url": url("notes.txt"), "kind": "file", "format": null, "bytes": 2},
            {"name": "people.csv", "url": url("people.csv"), "kind": "file", "format": "csv", "bytes": 5},
        ])
    );
    // A subdirectory, by a relative path or by its URL.
    let more = op(&mut shell, "sources.list", json!({"url": "more"})).await;
    assert_eq!(more["entries"][0]["format"], "json");
    let by_url = op(
        &mut shell,
        "sources.list",
        json!({"url": url("more") + "/"}),
    )
    .await;
    assert_eq!(by_url, more);
    let missing = op_err(&mut shell, "sources.list", json!({"url": "nope"})).await;
    assert_eq!(missing.kind, "not_found");
}

#[cfg(unix)]
#[tokio::test]
async fn sources_list_refuses_what_is_outside_the_root() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("private.csv"), "x\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("ok.csv"), "x\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("away")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("private.csv"), root.join("leak.csv")).unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::os::unix::fs::symlink(root.join("ok.csv"), root.join("sub").join("inside.csv")).unwrap();
    let mut shell = rooted(&root);

    for url in [
        "..".to_string(),
        "../".to_string(),
        "sub/../..".to_string(),
        outside.path().display().to_string(),
        String::from(Url::from_file_path(outside.path()).unwrap()),
        "/".to_string(),
        // Through a symbolic link.
        "away".to_string(),
        "away/".to_string(),
        // Not there, outside: refused too, not "not found".
        "../nowhere".to_string(),
    ] {
        let e = op_err(&mut shell, "sources.list", json!({"url": url})).await;
        assert_eq!(e.kind, "protocol", "{url}: {e:?}");
        assert!(
            e.message.contains("outside this session's root"),
            "{url}: {e:?}"
        );
    }
    // Links that lead out are left out of a listing; one that stays in is kept.
    let listed = op(&mut shell, "sources.list", json!({"url": "."})).await;
    let names: Vec<_> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["sub", "ok.csv"]);
    let sub = op(&mut shell, "sources.list", json!({"url": "sub"})).await;
    assert_eq!(sub["entries"][0]["name"], "inside.csv");
    // Other schemes are not listed.
    let http = op_err(
        &mut shell,
        "sources.list",
        json!({"url": "https://user:pw@example.com/x?sig=1"}),
    )
    .await;
    assert_eq!(http.kind, "protocol");
    assert!(
        !http.message.contains("pw") && !http.message.contains("sig"),
        "{http:?}"
    );
}

#[tokio::test]
async fn sources_list_a_bucket_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = rooted(dir.path());
    // An in-memory store stands in for S3.
    let store = Arc::new(InMemory::new());
    for (key, bytes) in [
        ("data/edges.parquet", 10),
        ("data/people.csv", 3),
        ("data/.hidden", 1),
        ("data/2024/a.parquet", 7),
        ("other.json", 2),
    ] {
        store
            .put(&ObjectPath::from(key), PutPayload::from(vec![0u8; bytes]))
            .await
            .unwrap();
    }
    shell.shell.context().register_object_store(
        &Url::parse("s3://bucket").unwrap(),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
    );
    let listed = op(
        &mut shell,
        "sources.list",
        json!({"url": "s3://bucket/data/"}),
    )
    .await;
    assert_eq!(
        listed["entries"],
        json!([
            {"name": "2024", "url": "s3://bucket/data/2024/", "kind": "dir", "format": null, "bytes": null},
            {"name": "edges.parquet", "url": "s3://bucket/data/edges.parquet", "kind": "file", "format": "parquet", "bytes": 10},
            {"name": "people.csv", "url": "s3://bucket/data/people.csv", "kind": "file", "format": "csv", "bytes": 3},
        ])
    );
    let top = op(&mut shell, "sources.list", json!({"url": "s3://bucket"})).await;
    let names: Vec<_> = top["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["url"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["s3://bucket/data/", "s3://bucket/other.json"]);
    // Credentials in the URL asked for are not repeated.
    let creds = op(
        &mut shell,
        "sources.list",
        json!({"url": "s3://key:secret@bucket/data/2024/?X-Amz-Signature=abc"}),
    )
    .await;
    let text = creds.to_string();
    assert!(
        !text.contains("secret") && !text.contains("Signature"),
        "{text}"
    );
}

#[tokio::test]
async fn sources_need_a_directory_root() {
    let mut shell = session();
    let hello = op(&mut shell, "hello", json!({"protocol": 1})).await;
    assert!(
        !hello["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("sources")),
        "{hello}"
    );
    for (name, args) in [
        ("sources.list", json!({"url": "."})),
        ("sources.attach", json!({"url": "x.ursa"})),
    ] {
        let e = op_err(&mut shell, name, args).await;
        assert_eq!(e.kind, "protocol");
        assert!(e.message.contains("no sources"), "{e:?}");
    }
}

#[tokio::test]
async fn sources_attach_is_attach() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = rooted(dir.path());
    // As ATTACH: an alias is not supported yet.
    let aliased = op_err(
        &mut shell,
        "sources.attach",
        json!({"url": "club.ursa", "as": "club"}),
    )
    .await;
    assert_eq!(aliased.kind, "sql");
    assert!(aliased.message.contains("not supported yet"), "{aliased:?}");
    let _ = shell.take_events().await;
    // A new database file, then a view in it: the catalog changes.
    let url = String::from(Url::from_file_path(dir.path().join("club.ursa")).unwrap());
    let attached = shell
        .handle("sources.attach", json!({"url": url}), Vec::new())
        .await;
    assert_eq!(attached.unwrap().result, json!({}));
    let _ = shell.take_events().await;
    let created = op_events(
        &mut shell,
        "query",
        json!({"sql": "CREATE VIEW members AS SELECT 1 AS id"}),
    )
    .await;
    assert_eq!(created, json!([{"event": "catalog"}]));
    assert!(dir.path().join("club.ursa").exists());
}

#[tokio::test]
async fn complete_names_tables_columns_functions_and_commands() {
    let mut shell = session();
    op(
        &mut shell,
        "query",
        json!({"sql": "CREATE TABLE members (id BIGINT, club VARCHAR) AS VALUES (1, 'a'); \
                       CREATE TABLE ties (src BIGINT, dst BIGINT) AS VALUES (1, 1); \
                       CREATE PROPERTY GRAPH karate VERTEX TABLES (members KEY (id)) \
                         EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id) DESTINATION KEY (dst) REFERENCES members (id))"}),
    )
    .await;
    let complete = |sql: &str| json!({"sql": sql, "cursor": sql.encode_utf16().count()});
    let labels = |v: &Value, kind: &str| -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["kind"] == kind)
            .map(|i| i["label"].as_str().unwrap().to_string())
            .collect()
    };

    // A table, a graph and functions, by prefix.
    let got = op(&mut shell, "complete", complete("SELECT * FROM mem")).await;
    assert_eq!(
        (got["from"].clone(), got["to"].clone()),
        (json!(14), json!(17))
    );
    assert_eq!(labels(&got, "table"), ["members"]);
    let got = op(&mut shell, "complete", complete("SELECT * FROM pager")).await;
    assert!(
        labels(&got, "function").contains(&"pagerank".to_string()),
        "{got}"
    );
    let item = got["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["label"] == "pagerank")
        .unwrap();
    assert_eq!(item["detail"], "graph function");
    let got = op(
        &mut shell,
        "complete",
        complete("SELECT * FROM pagerank(kar"),
    )
    .await;
    assert_eq!(labels(&got, "graph"), ["karate"]);
    let got = op(&mut shell, "complete", complete("SELECT ab")).await;
    assert!(
        labels(&got, "function").contains(&"abs".to_string()),
        "{got}"
    );

    // Columns: the statement's tables', and qualified after `t.`.
    let got = op(&mut shell, "complete", complete("SELECT cl FROM members")).await;
    // The cursor is after `cl`, not at the end.
    let sql = "SELECT cl FROM members";
    let at = json!({"sql": sql, "cursor": 9});
    let got_mid = op(&mut shell, "complete", at).await;
    assert_eq!(labels(&got_mid, "column"), ["club"]);
    assert_eq!(labels(&got, "column"), Vec::<String>::new());
    let got = op(&mut shell, "complete", complete("SELECT members.")).await;
    assert_eq!(labels(&got, "column"), ["id", "club"]);
    let sql = "SELECT m. FROM members AS m JOIN ties t ON m.id = t.src";
    let got = op(&mut shell, "complete", json!({"sql": sql, "cursor": 9})).await;
    assert_eq!(labels(&got, "column"), ["id", "club"]);
    assert_eq!(labels(&got, "function"), Vec::<String>::new());
    let got = op(
        &mut shell,
        "complete",
        json!({"sql": "SELECT t.s FROM ties t", "cursor": 10}),
    )
    .await;
    assert_eq!(labels(&got, "column"), ["src"]);
    assert_eq!(
        (got["from"].clone(), got["to"].clone()),
        (json!(9), json!(10))
    );
    // A schema's tables; a graph's own.
    let got = op(&mut shell, "complete", complete("SELECT * FROM public.t")).await;
    assert_eq!(labels(&got, "table"), ["ties"]);
    let got = op(&mut shell, "complete", complete("SELECT * FROM karate.")).await;
    assert_eq!(labels(&got, "view"), ["nodes", "edges"]);

    // Dot-commands, at the start of a line only.
    let got = op(&mut shell, "complete", complete(".ta")).await;
    assert_eq!(labels(&got, "command"), [".tables"]);
    assert_eq!(got["from"], 0);
    let got = op(&mut shell, "complete", complete("SELECT 1;\n.gr")).await;
    assert!(
        labels(&got, "command").contains(&".graphs".to_string()),
        "{got}"
    );
    assert_eq!(got["from"], 10);
    // Nothing in a string or a comment.
    for sql in ["SELECT 'mem", "SELECT 1 -- mem", "SELECT /* mem"] {
        let got = op(&mut shell, "complete", complete(sql)).await;
        assert_eq!(got["items"], json!([]), "{sql}");
    }

    // Offsets are UTF-16: an emoji is two units.
    let sql = "SELECT '😀', mem";
    let got = op(&mut shell, "complete", complete(sql)).await;
    assert_eq!(got["from"], 13);
    assert_eq!(labels(&got, "table"), ["members"]);

    let e = op_err(&mut shell, "complete", json!({"sql": "x"})).await;
    assert_eq!(e.kind, "protocol");
}

#[tokio::test]
async fn highlight_gives_spans_in_utf16() {
    let mut shell = session();
    let got = op(
        &mut shell,
        "highlight",
        json!({"sql": "SELECT 'é😀', 42 -- n"}),
    )
    .await;
    assert_eq!(
        got["spans"],
        json!([
            {"from": 0, "to": 6, "kind": "keyword"},
            {"from": 7, "to": 12, "kind": "string"},
            {"from": 14, "to": 16, "kind": "number"},
            {"from": 17, "to": 21, "kind": "comment"},
        ])
    );
    let got = op(&mut shell, "highlight", json!({"sql": ".open club.ursa"})).await;
    assert_eq!(
        got["spans"],
        json!([
            {"from": 0, "to": 5, "kind": "command"},
            {"from": 5, "to": 15, "kind": "arguments"},
        ])
    );
}
