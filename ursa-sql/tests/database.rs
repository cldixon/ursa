//! Sessions with a `.ursa` database: the catalog file, replay, relative paths,
//! read-only sessions, the lock and `ATTACH`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::prelude::SessionContext;
use ursa_engine::database::{Database, OpenOptions};
use ursa_sql::{execute_with_text, open_session, parse_sql_with_text_for};

/// A temporary directory holding copies of the karate CSVs. Statements name
/// them by absolute path ([`here`]): a relative path would resolve against the
/// test process's current directory, which tests cannot change safely. The
/// CLI tests cover running from other directories.
fn karate_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../python/ursa/datasets/data");
    for file in ["karate_edges.csv", "karate_nodes.csv"] {
        std::fs::copy(data.join(file), dir.path().join(file)).unwrap();
    }
    dir
}

fn quick() -> OpenOptions {
    OpenOptions {
        lock_timeout: Duration::from_millis(200),
        ..OpenOptions::default()
    }
}

async fn open(path: &Path) -> SessionContext {
    open_session(path, &quick()).await.unwrap()
}

/// Run `sql`; the last statement's rows as CSV lines (header first).
async fn run(ctx: &SessionContext, sql: &str) -> datafusion::error::Result<Vec<String>> {
    let mut lines = Vec::new();
    for (statement, text) in parse_sql_with_text_for(ctx, sql)? {
        let batches = execute_with_text(ctx, statement, &text)
            .await?
            .collect()
            .await?;
        lines.clear();
        if let Some(first) = batches.first() {
            let header: Vec<_> = first
                .schema()
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect();
            if !header.is_empty() {
                lines.push(header.join(","));
            }
        }
        let options = FormatOptions::default();
        for batch in &batches {
            let formatters: Vec<_> = batch
                .columns()
                .iter()
                .map(|c| ArrayFormatter::try_new(c.as_ref(), &options).unwrap())
                .collect();
            for row in 0..batch.num_rows() {
                let cells: Vec<_> = formatters
                    .iter()
                    .map(|f| f.value(row).to_string())
                    .collect();
                lines.push(cells.join(","));
            }
        }
    }
    Ok(lines)
}

async fn ok(ctx: &SessionContext, sql: &str) -> Vec<String> {
    run(ctx, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)))
}

async fn err(ctx: &SessionContext, sql: &str) -> String {
    match run(ctx, sql).await {
        Ok(rows) => panic!("{sql} should fail; got {rows:?}"),
        Err(e) => ursa_engine::user_message(&e),
    }
}

async fn open_err(path: &Path, options: &OpenOptions) -> String {
    match open_session(path, options).await {
        Ok(_) => panic!("opening {} should fail", path.display()),
        Err(e) => ursa_engine::user_message(&e),
    }
}

fn catalog(path: &Path) -> Vec<String> {
    Database::open(path, &OpenOptions::read_only())
        .unwrap()
        .statements()
}

/// `sql` with each quoted `karate_…` file name made a path into `dir`.
fn here(dir: &Path, sql: &str) -> String {
    sql.replace("'karate_", &format!("'{}/karate_", dir.display()))
}

/// The karate views and graph, over the files in `dir`.
fn define_karate(dir: &Path) -> String {
    here(dir, &format!("{MEMBERS}; {TIES}; {KARATE}"))
}

/// The karate statements as stored: the paths relative to the database.
const MEMBERS: &str = "CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv'";
const TIES: &str = "CREATE VIEW ties AS SELECT * FROM 'karate_edges.csv'";
const KARATE: &str = "CREATE PROPERTY GRAPH karate VERTEX TABLES (members KEY (id)) EDGE TABLES \
     (ties SOURCE KEY (src) REFERENCES members (id) DESTINATION KEY (dst) REFERENCES members (id))";

/// What a session answers about the karate objects: compared before and after
/// a replay.
async fn snapshot(ctx: &SessionContext) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for sql in [
        "SELECT count(*) AS n, sum(id) AS ids FROM members",
        "SELECT * FROM karate.nodes ORDER BY id LIMIT 3",
        "SELECT count(*) AS n FROM karate.edges",
        "SELECT id, degree FROM degree(karate, direction => 'both') ORDER BY degree DESC, id \
         LIMIT 3",
        "SHOW GRAPHS",
        "DESCRIBE GRAPH karate",
    ] {
        out.push(ok(ctx, sql).await);
    }
    out
}

#[tokio::test]
async fn views_and_a_graph_round_trip() {
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    let before = {
        let ctx = open(&path).await;
        ok(&ctx, &define_karate(dir.path())).await;
        snapshot(&ctx).await
    };
    assert_eq!(catalog(&path), [MEMBERS, TIES, KARATE]);
    assert_eq!(before[3], ["id,degree", "33,17", "0,16", "32,12"]);
    let ctx = open(&path).await;
    assert_eq!(snapshot(&ctx).await, before);
    // The file reads as the design page shows it.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.starts_with("{\n  \"format\": \"ursa\",\n  \"version\": 1,\n  \"statements\": [\n    \"CREATE VIEW members"),
        "{text}"
    );
}

#[tokio::test]
async fn an_external_table_round_trips_as_written() {
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    // DataFusion's Display for CREATE EXTERNAL TABLE drops the columns and
    // options, so the statement is stored as written, its path replaced by
    // the stored one.
    let create = "CREATE EXTERNAL TABLE edges (src BIGINT, dst BIGINT)\n  STORED AS CSV \
                  LOCATION 'karate_edges.csv' OPTIONS ('format.has_header' 'true')";
    let before = {
        let ctx = open(&path).await;
        ok(&ctx, &here(dir.path(), create)).await;
        ok(&ctx, "SELECT count(*) AS n, sum(src) AS s FROM edges").await
    };
    assert_eq!(before, ["n,s", "78,864"]);
    assert_eq!(catalog(&path), [create]);
    let ctx = open(&path).await;
    assert_eq!(
        ok(&ctx, "SELECT count(*) AS n, sum(src) AS s FROM edges").await,
        before
    );
    assert_eq!(
        ok(&ctx, "SELECT data_type FROM information_schema.columns WHERE table_name = 'edges' AND column_name = 'src'").await,
        ["data_type", "Int64"]
    );
    ok(&ctx, "DROP TABLE edges").await;
    assert!(catalog(&path).is_empty());
}

#[tokio::test]
async fn a_graph_over_bare_file_paths_round_trips() {
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    let create = "CREATE PROPERTY GRAPH k VERTEX TABLES ('karate_nodes.csv' KEY (id)) EDGE TABLES \
                  ('karate_edges.csv' SOURCE KEY (src) DESTINATION KEY (dst))";
    let query = "SELECT club, count(*) AS n FROM k.nodes GROUP BY club ORDER BY club";
    let before = {
        let ctx = open(&path).await;
        ok(&ctx, &here(dir.path(), create)).await;
        ok(&ctx, query).await
    };
    assert_eq!(before, ["club,n", "Mr. Hi,17", "Officer,17"]);
    assert_eq!(catalog(&path), [create]);
    let ctx = open(&path).await;
    assert_eq!(ok(&ctx, query).await, before);
    assert_eq!(
        ok(&ctx, "SELECT sum(degree) AS s FROM degree(k)").await,
        ["s", "78"]
    );
}

#[tokio::test]
async fn replace_is_in_place_and_drop_removes() {
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    let ctx = open(&path).await;
    ok(&ctx, &define_karate(dir.path())).await;

    let strong =
        "CREATE OR REPLACE VIEW members AS SELECT * FROM 'karate_nodes.csv' WHERE id < 100";
    ok(&ctx, &here(dir.path(), strong)).await;
    assert_eq!(catalog(&path), [strong, TIES, KARATE]);

    // IF NOT EXISTS over an existing object keeps its statement.
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH IF NOT EXISTS karate EDGE TABLES (members SOURCE KEY (id) \
         DESTINATION KEY (id))",
    )
    .await;
    assert_eq!(catalog(&path), [strong, TIES, KARATE]);

    let replaced = "CREATE OR REPLACE PROPERTY GRAPH karate EDGE TABLES (ties SOURCE KEY (src) \
                    DESTINATION KEY (dst))";
    ok(&ctx, replaced).await;
    assert_eq!(catalog(&path), [strong, TIES, replaced]);

    ok(&ctx, "DROP PROPERTY GRAPH karate; DROP VIEW members").await;
    assert_eq!(catalog(&path), [TIES]);
    ok(&ctx, "DROP VIEW IF EXISTS nothing; DROP VIEW ties").await;
    assert!(catalog(&path).is_empty());

    // Lower-case, quoted and qualified names are one object.
    ok(&ctx, "create view Members as select 1 as id").await;
    ok(
        &ctx,
        "CREATE OR REPLACE VIEW public.members AS SELECT 2 AS id",
    )
    .await;
    assert_eq!(
        catalog(&path),
        ["CREATE OR REPLACE VIEW public.members AS SELECT 2 AS id"]
    );
    ok(&ctx, "DROP VIEW datafusion.public.MEMBERS").await;
    assert!(catalog(&path).is_empty());
}

#[tokio::test]
async fn a_failed_statement_leaves_the_catalog_alone() {
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    let ctx = open(&path).await;
    ok(&ctx, &define_karate(dir.path())).await;
    let before = std::fs::read(&path).unwrap();
    err(&ctx, "CREATE VIEW nope AS SELECT * FROM 'missing.csv'").await;
    err(&ctx, "CREATE VIEW members AS SELECT 1").await;
    let e = err(&ctx, "DROP VIEW ties").await;
    assert!(e.contains("karate"), "{e}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn the_database_and_its_files_move_together() {
    let parent = tempfile::tempdir().unwrap();
    let first = parent.path().join("first");
    std::fs::rename(karate_dir().keep(), &first).unwrap();
    {
        let ctx = open(&first.join("club.ursa")).await;
        ok(&ctx, &define_karate(&first)).await;
    }
    let moved = parent.path().join("moved");
    std::fs::rename(&first, &moved).unwrap();
    let ctx = open(&moved.join("club.ursa")).await;
    assert_eq!(
        ok(&ctx, "SELECT count(*) AS n FROM karate.nodes").await,
        ["n", "34"]
    );
    assert_eq!(catalog(&moved.join("club.ursa")), [MEMBERS, TIES, KARATE]);
}

#[tokio::test]
async fn tables_with_data_are_refused_in_a_database_only() {
    let dir = karate_dir();
    let ctx = open(&dir.path().join("club.ursa")).await;
    let e = err(&ctx, "CREATE TABLE t AS SELECT 1 AS x").await;
    assert_eq!(
        e,
        "CREATE TABLE … AS is not supported in a database file: version 1 of the .ursa file \
         stores views, external tables and graphs, not tables with data. Define a view instead \
         (CREATE VIEW t AS SELECT …), or write the rows to a file with COPY (SELECT …) TO \
         't.parquet' and define a view over it (CREATE VIEW t AS SELECT * FROM 't.parquet')"
    );
    let e = err(&ctx, "INSERT INTO t VALUES (1)").await;
    assert!(
        e.starts_with("INSERT is not supported in a database file"),
        "{e}"
    );
    let e = err(&ctx, "CREATE SCHEMA s").await;
    assert!(
        e.starts_with("CREATE SCHEMA is not supported in a database file"),
        "{e}"
    );

    let memory = ursa_engine::new_session();
    ok(
        &memory,
        "CREATE TABLE t AS SELECT 1 AS x; INSERT INTO t VALUES (2)",
    )
    .await;
    assert_eq!(ok(&memory, "SELECT sum(x) AS s FROM t").await, ["s", "3"]);
}

#[tokio::test]
async fn read_only_sessions_refuse_ddl_and_take_no_lock() {
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    let writer = open(&path).await;
    ok(&writer, &define_karate(dir.path())).await;

    // The writer still has it open.
    let reader = open_session(&path, &OpenOptions::read_only())
        .await
        .unwrap();
    assert_eq!(
        ok(&reader, "SELECT count(*) AS n FROM karate.edges").await,
        ["n", "78"]
    );
    for sql in [
        "CREATE VIEW v AS SELECT 1",
        "DROP VIEW members",
        "CREATE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
        "DROP PROPERTY GRAPH karate",
        "CREATE TABLE t AS SELECT 1",
    ] {
        let e = err(&reader, sql).await;
        assert!(
            e.starts_with(&format!(
                "{} is open read-only (--readonly), so ",
                path.display()
            )),
            "{sql}: {e}"
        );
    }
    let e = err(&reader, "DROP VIEW members").await;
    assert!(e.ends_with("so DROP VIEW cannot change it"), "{e}");
    assert_eq!(catalog(&path), [MEMBERS, TIES, KARATE]);

    let missing = open_err(&dir.path().join("none.ursa"), &OpenOptions::read_only()).await;
    assert!(missing.contains("does not exist"), "{missing}");
}

#[tokio::test]
async fn a_second_writer_fails_while_the_first_is_open() {
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    let first = open(&path).await;
    let e = open_err(&path, &quick()).await;
    assert_eq!(
        e,
        format!("{} is in use by another process", path.display())
    );
    drop(first);
    open(&path).await;
}

#[tokio::test]
async fn attach_opens_a_database_in_an_empty_session() {
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    {
        let ctx = open(&path).await;
        ok(&ctx, &define_karate(dir.path())).await;
    }
    let attach = format!("ATTACH '{}'", path.display());

    let ctx = ursa_engine::new_session();
    ok(&ctx, &attach).await;
    assert_eq!(
        ok(&ctx, "SELECT count(*) AS n FROM karate.nodes").await,
        ["n", "34"]
    );
    // What is defined now is stored.
    ok(
        &ctx,
        "CREATE VIEW small AS SELECT * FROM members WHERE id < 3",
    )
    .await;
    let e = err(&ctx, &attach).await;
    assert!(e.contains("is already open"), "{e}");
    drop(ctx);
    assert_eq!(catalog(&path).len(), 4);

    let ctx = ursa_engine::new_session();
    ok(&ctx, "CREATE VIEW v AS SELECT 1 AS x").await;
    let e = err(&ctx, &attach).await;
    assert!(
        e.starts_with("ATTACH must come before anything is defined"),
        "{e}"
    );

    let ctx = ursa_engine::new_session();
    let e = err(&ctx, &format!("{attach} AS club")).await;
    assert!(
        e.starts_with("ATTACH … AS club is not supported yet"),
        "{e}"
    );
}

#[tokio::test]
async fn a_catalog_that_does_not_replay_is_a_clear_error() {
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    std::fs::write(
        &path,
        r#"{"format": "ursa", "version": 1, "statements": ["CREATE VIEW v AS SELECT * FROM 'gone.csv'"]}"#,
    )
    .unwrap();
    let e = open_err(&path, &quick()).await;
    // Replay looks for the file in the database's directory, and says where.
    assert_eq!(
        e,
        format!(
            "cannot open {}: statement 1 of its catalog failed: no file found at '{}'\n  \
             the statement: CREATE VIEW v AS SELECT * FROM 'gone.csv'",
            path.display(),
            std::path::absolute(dir.path())
                .unwrap()
                .join("gone.csv")
                .display()
        )
    );
}

#[tokio::test]
async fn replay_tolerates_statements_out_of_order() {
    let dir = karate_dir();
    let path: PathBuf = dir.path().join("club.ursa");
    let text = serde_json_text(&[KARATE, TIES, MEMBERS]);
    std::fs::write(&path, text).unwrap();
    let ctx = open(&path).await;
    assert_eq!(
        ok(&ctx, "SELECT count(*) AS n FROM karate.edges").await,
        ["n", "78"]
    );
}

fn serde_json_text(statements: &[&str]) -> String {
    let quoted: Vec<String> = statements
        .iter()
        .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!(
        "{{\"format\": \"ursa\", \"version\": 1, \"statements\": [{}]}}",
        quoted.join(", ")
    )
}

#[tokio::test]
async fn paths_in_a_view_are_stored_relative_wherever_they_appear() {
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    let ctx = open(&path).await;
    // A CTE, a join, a subquery, and single and double quotes.
    let view = |d: &str| {
        format!(
            "CREATE VIEW pairs AS WITH m AS (SELECT * FROM \"{d}karate_nodes.csv\") \
             SELECT count(*) AS n FROM m JOIN (SELECT * FROM '{d}karate_edges.csv') AS e \
             ON m.id = e.src WHERE m.id IN (SELECT id FROM '{d}karate_nodes.csv')"
        )
    };
    ok(&ctx, &view(&format!("{}/", dir.path().display()))).await;
    // A quoted name that is a view stays the view.
    ok(&ctx, "CREATE VIEW \"my.view\" AS SELECT 1 AS x").await;
    ok(&ctx, "CREATE VIEW mine AS SELECT * FROM \"my.view\"").await;
    assert_eq!(
        catalog(&path),
        [
            view(""),
            "CREATE VIEW \"my.view\" AS SELECT 1 AS x".into(),
            "CREATE VIEW mine AS SELECT * FROM \"my.view\"".into(),
        ]
    );
    let expected = ok(&ctx, "SELECT n FROM pairs").await;
    assert_eq!(expected, ["n", "78"]);
    drop(ctx);
    let ctx = open(&path).await;
    assert_eq!(ok(&ctx, "SELECT n FROM pairs").await, expected);
    assert_eq!(ok(&ctx, "SELECT x FROM mine").await, ["x", "1"]);
}

#[tokio::test]
async fn a_path_outside_the_database_directory_is_stored_absolute() {
    let data = karate_dir();
    let db_dir = tempfile::tempdir().unwrap();
    let path = db_dir.path().join("club.ursa");
    let ctx = open(&path).await;
    ok(&ctx, &define_karate(data.path())).await;
    let absolute = std::path::absolute(data.path()).unwrap();
    assert_eq!(
        catalog(&path),
        [
            here(&absolute, MEMBERS),
            here(&absolute, TIES),
            KARATE.into()
        ]
    );
    drop(ctx);
    let ctx = open(&path).await;
    assert_eq!(
        ok(&ctx, "SELECT count(*) AS n FROM karate.edges").await,
        ["n", "78"]
    );
}

#[tokio::test]
async fn an_m3a_catalog_replays_unchanged() {
    // M3a stored paths as written and resolved them against the database's
    // directory: that is already the stored form.
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    let external = "CREATE EXTERNAL TABLE edges (src BIGINT, dst BIGINT)\n  STORED AS CSV \
                    LOCATION 'karate_edges.csv' OPTIONS ('format.has_header' 'true')";
    let bare = "CREATE PROPERTY GRAPH k EDGE TABLES ('karate_edges.csv' SOURCE KEY (src) \
                DESTINATION KEY (dst))";
    let statements = [MEMBERS, TIES, KARATE, external, bare];
    let file = ursa_engine::database::CatalogFile {
        statements: statements.iter().map(|s| s.to_string()).collect(),
    };
    std::fs::write(&path, file.to_text()).unwrap();
    let ctx = open(&path).await;
    assert_eq!(
        ok(&ctx, "SELECT count(*) AS n FROM karate.edges").await,
        ["n", "78"]
    );
    assert_eq!(
        ok(&ctx, "SELECT count(*) AS n, sum(src) AS s FROM edges").await,
        ["n,s", "78,864"]
    );
    assert_eq!(
        ok(&ctx, "SELECT sum(degree) AS s FROM degree(k)").await,
        ["s", "78"]
    );
    // Saving the catalog again keeps the old statements as they were.
    ok(&ctx, "CREATE VIEW one AS SELECT 1 AS x").await;
    let mut expected: Vec<String> = statements.iter().map(|s| s.to_string()).collect();
    expected.push("CREATE VIEW one AS SELECT 1 AS x".into());
    assert_eq!(catalog(&path), expected);
}

#[tokio::test]
async fn an_external_table_whose_text_cannot_be_rewritten_exactly_is_refused() {
    let dir = karate_dir();
    let path = dir.path().join("club.ursa");
    let ctx = open(&path).await;
    // The path appears twice, once as an option's value: replacing both
    // would change the option, so the statement is refused, not mis-stored.
    let file = dir.path().join("karate_edges.csv");
    let create = format!(
        "CREATE EXTERNAL TABLE edges (src BIGINT, dst BIGINT) STORED AS CSV LOCATION '{f}' \
         OPTIONS ('format.has_header' 'true', 'format.comment' '{f}')",
        f = file.display()
    );
    let e = err(&ctx, &create).await;
    assert_eq!(
        e,
        format!(
            "cannot store this CREATE EXTERNAL TABLE in {}: its path '{}' is stored as \
             'karate_edges.csv' (relative to the database's directory, {}), and the statement \
             as written could not be rewritten exactly; write each path as stored, from the \
             database's directory",
            path.display(),
            file.display(),
            std::path::absolute(dir.path()).unwrap().display()
        )
    );
    assert!(catalog(&path).is_empty());
    err(&ctx, "SELECT * FROM edges").await;
}
