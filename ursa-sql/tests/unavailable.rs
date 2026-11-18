//! A database opens when a statement of its catalog fails (#381): the object
//! it defines is unavailable, with one notice, and fails only when used.
//! `DROP` and `CREATE OR REPLACE` repair it; objects that use it are
//! unavailable too; the catalog keeps every statement.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::prelude::SessionContext;
use ursa_engine::database::{Database, OpenOptions};
use ursa_sql::{execute_with_text, parse_sql_with_text_for};

fn quick() -> OpenOptions {
    OpenOptions {
        lock_timeout: Duration::from_millis(200),
        ..OpenOptions::default()
    }
}

/// Open `path` as a host does: a notice sink first, then the database. The
/// session, and what it said while opening.
async fn open(path: &Path, options: &OpenOptions) -> (SessionContext, Vec<String>) {
    let ctx = ursa_engine::new_session();
    let said = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&said);
    ursa_engine::notices::set_sink(
        &ctx.state(),
        Some(Arc::new(move |notice: &str| {
            sink.lock().unwrap().push(notice.to_string())
        })),
    );
    ursa_sql::open_database(&ctx, path, options)
        .await
        .unwrap_or_else(|e| panic!("{}", ursa_engine::user_message(&e)));
    let said = said.lock().unwrap().clone();
    (ctx, said)
}

async fn run(ctx: &SessionContext, sql: &str) -> datafusion::error::Result<Vec<String>> {
    let mut lines = Vec::new();
    for (statement, text) in parse_sql_with_text_for(ctx, sql)? {
        let batches = execute_with_text(ctx, statement, &text)
            .await?
            .collect()
            .await?;
        lines.clear();
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

fn catalog(path: &Path) -> Vec<String> {
    Database::open(path, &OpenOptions::read_only())
        .unwrap()
        .statements()
}

/// The table names `SHOW TABLES` lists in the default schema.
async fn tables(ctx: &SessionContext) -> Vec<String> {
    let mut names: Vec<String> = ok(
        ctx,
        "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'",
    )
    .await;
    names.sort();
    names
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        std::path::absolute(self.dir.path()).unwrap().join(name)
    }

    fn db(&self) -> PathBuf {
        self.path("m.ursa")
    }

    fn write_csv(&self, name: &str) {
        std::fs::write(self.path(name), "a,b\n1,2\n3,4\n").unwrap();
    }
}

fn no_file(path: &Path) -> String {
    format!("no file found at '{}'", path.display())
}

#[tokio::test]
async fn a_view_over_a_missing_csv_is_unavailable_until_repaired() {
    let fx = Fixture::new();
    fx.write_csv("t.csv");
    let t = fx.path("t.csv");
    let (ctx, said) = open(&fx.db(), &quick()).await;
    assert!(said.is_empty(), "{said:?}");
    ok(
        &ctx,
        &format!(
            "CREATE VIEW v AS FROM '{}'; CREATE VIEW w AS SELECT 1 AS x; CREATE VIEW vv AS \
             SELECT a FROM v",
            t.display()
        ),
    )
    .await;
    drop(ctx);
    let stored = catalog(&fx.db());
    let saved = std::fs::read_to_string(fx.db()).unwrap();
    std::fs::remove_file(&t).unwrap();

    let message = format!(
        "view v is unavailable: {}; DROP VIEW v or restore the file",
        no_file(&t)
    );
    let dependant = "view vv is unavailable: it uses view v, which is unavailable; repair v, or \
                     DROP VIEW vv";
    for options in [OpenOptions::read_only(), quick()] {
        let (ctx, said) = open(&fx.db(), &options).await;
        assert_eq!(said, [message.clone(), dependant.to_string()]);
        // The rest of the database works, and lists the unavailable views.
        assert_eq!(ok(&ctx, "FROM w").await, ["1"]);
        assert_eq!(tables(&ctx).await, ["v", "vv", "w"]);
        // Using them fails with why, however they are used.
        for sql in [
            "FROM v",
            "SELECT a FROM v",
            "SELECT count(*) FROM v",
            "SELECT * FROM w JOIN v ON true",
            "DESCRIBE v",
        ] {
            let e = err(&ctx, sql).await;
            assert!(e.contains(&message), "{sql}: {e}");
        }
        assert_eq!(err(&ctx, "SELECT a FROM vv").await, dependant);
        drop(ctx);
        // Nothing is rewritten.
        assert_eq!(std::fs::read_to_string(fx.db()).unwrap(), saved);
    }

    // Read-only, it cannot be repaired.
    let (ctx, _) = open(&fx.db(), &OpenOptions::read_only()).await;
    let e = err(&ctx, "DROP VIEW v").await;
    assert!(e.contains("is open read-only"), "{e}");
    drop(ctx);

    // CREATE without OR REPLACE finds it exists.
    let (ctx, _) = open(&fx.db(), &quick()).await;
    let e = err(&ctx, "CREATE VIEW x AS SELECT * FROM v").await;
    assert_eq!(e, message);
    let e = err(&ctx, "CREATE VIEW v AS SELECT 2 AS a").await;
    assert!(e.contains("already exists"), "{e}");
    assert!(err(&ctx, "FROM v").await.contains(&message));
    assert_eq!(catalog(&fx.db()), stored);

    // CREATE OR REPLACE repairs it, in its place in the catalog.
    ok(&ctx, "CREATE OR REPLACE VIEW v AS SELECT 5 AS a").await;
    assert_eq!(ok(&ctx, "FROM v").await, ["5"]);
    let mut replaced = stored.clone();
    replaced[0] = "CREATE OR REPLACE VIEW v AS SELECT 5 AS a".into();
    assert_eq!(catalog(&fx.db()), replaced);
    // vv stays unavailable for the session; it comes back on the next open.
    assert_eq!(err(&ctx, "FROM vv").await, dependant);
    drop(ctx);
    let (ctx, said) = open(&fx.db(), &quick()).await;
    assert!(said.is_empty(), "{said:?}");
    assert_eq!(ok(&ctx, "FROM vv").await, ["5"]);
}

#[tokio::test]
async fn drop_and_drop_if_exists_remove_an_unavailable_view() {
    let fx = Fixture::new();
    fx.write_csv("t.csv");
    let t = fx.path("t.csv");
    let (ctx, _) = open(&fx.db(), &quick()).await;
    ok(
        &ctx,
        &format!(
            "CREATE VIEW v AS FROM '{t}'; CREATE VIEW u AS FROM '{t}'; CREATE VIEW w AS \
             SELECT 1 AS x",
            t = t.display()
        ),
    )
    .await;
    drop(ctx);
    std::fs::remove_file(&t).unwrap();

    let (ctx, said) = open(&fx.db(), &quick()).await;
    assert_eq!(said.len(), 2, "{said:?}");
    ok(&ctx, "DROP VIEW v").await;
    ok(&ctx, "DROP VIEW IF EXISTS u").await;
    assert_eq!(tables(&ctx).await, ["w"]);
    assert_eq!(catalog(&fx.db()), ["CREATE VIEW w AS SELECT 1 AS x"]);
    drop(ctx);
    let (_, said) = open(&fx.db(), &quick()).await;
    assert!(said.is_empty(), "{said:?}");
}

#[tokio::test]
async fn a_graph_over_an_unavailable_view_is_unavailable_too() {
    let fx = Fixture::new();
    fx.write_csv("e.csv");
    let e_csv = fx.path("e.csv");
    let (ctx, _) = open(&fx.db(), &quick()).await;
    ok(
        &ctx,
        &format!(
            "CREATE VIEW ties AS SELECT a AS src, b AS dst FROM '{}'; CREATE PROPERTY GRAPH g \
             EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
            e_csv.display()
        ),
    )
    .await;
    assert_eq!(ok(&ctx, "SELECT count(*) FROM g.edges").await, ["2"]);
    drop(ctx);
    let stored = catalog(&fx.db());
    std::fs::remove_file(&e_csv).unwrap();

    let graph = "graph g is unavailable: it uses view ties, which is unavailable; repair ties, \
                 or DROP PROPERTY GRAPH g";
    for options in [OpenOptions::read_only(), quick()] {
        let (ctx, said) = open(&fx.db(), &options).await;
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(said[0].starts_with("view ties is unavailable"), "{said:?}");
        assert_eq!(said[1], graph);
        for sql in [
            "SELECT * FROM pagerank(g)",
            "SELECT * FROM GRAPH_TABLE (g MATCH (a)-[e]->(b) COLUMNS (a.id AS a))",
            "DESCRIBE GRAPH g",
        ] {
            let e = err(&ctx, sql).await;
            assert!(e.contains(graph), "{sql}: {e}");
        }
        drop(ctx);
        assert_eq!(catalog(&fx.db()), stored);
    }

    // CREATE without OR REPLACE finds it exists.
    let (ctx, _) = open(&fx.db(), &quick()).await;
    let e = err(
        &ctx,
        "CREATE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
    )
    .await;
    assert!(e.contains("graph 'g' already exists"), "{e}");
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH IF NOT EXISTS g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
    )
    .await;
    assert_eq!(catalog(&fx.db()), stored);
    // Repair the view, then the graph.
    ok(
        &ctx,
        "CREATE OR REPLACE VIEW ties AS SELECT 1 AS src, 2 AS dst",
    )
    .await;
    ok(
        &ctx,
        "CREATE OR REPLACE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
    )
    .await;
    assert_eq!(ok(&ctx, "SELECT count(*) FROM g.edges").await, ["1"]);
    drop(ctx);
    let (ctx, said) = open(&fx.db(), &quick()).await;
    assert!(said.is_empty(), "{said:?}");
    assert_eq!(ok(&ctx, "SELECT count(*) FROM g.edges").await, ["1"]);
}

#[tokio::test]
async fn a_graph_over_a_missing_file_is_dropped_or_restored() {
    let fx = Fixture::new();
    fx.write_csv("e.csv");
    let e_csv = fx.path("e.csv");
    let define = format!(
        "CREATE PROPERTY GRAPH g EDGE TABLES ('{}' SOURCE KEY (a) DESTINATION KEY (b))",
        e_csv.display()
    );
    let (ctx, _) = open(&fx.db(), &quick()).await;
    ok(&ctx, &define).await;
    drop(ctx);
    std::fs::rename(&e_csv, fx.path("kept.csv")).unwrap();

    let (ctx, said) = open(&fx.db(), &quick()).await;
    assert_eq!(
        said,
        [format!(
            "graph g is unavailable: {}; DROP PROPERTY GRAPH g or restore the file",
            no_file(&e_csv)
        )]
    );
    drop(ctx);

    // Restoring the file brings it back.
    std::fs::rename(fx.path("kept.csv"), &e_csv).unwrap();
    let (ctx, said) = open(&fx.db(), &quick()).await;
    assert!(said.is_empty(), "{said:?}");
    assert_eq!(ok(&ctx, "SELECT count(*) FROM g.edges").await, ["2"]);
    drop(ctx);

    std::fs::remove_file(&e_csv).unwrap();
    let (ctx, _) = open(&fx.db(), &quick()).await;
    ok(&ctx, "DROP PROPERTY GRAPH g").await;
    assert!(catalog(&fx.db()).is_empty());
    ok(&ctx, "DROP PROPERTY GRAPH IF EXISTS g").await;
}

#[tokio::test]
async fn missing_parquet_globs_and_external_tables_are_unavailable() {
    let fx = Fixture::new();
    std::fs::create_dir(fx.path("parts")).unwrap();
    fx.write_csv("parts/one.csv");
    let (ctx, _) = open(&fx.db(), &quick()).await;
    let parquet = fx.path("t.parquet");
    ok(
        &ctx,
        &format!("COPY (SELECT 1 AS a) TO '{}'", parquet.display()),
    )
    .await;
    ok(
        &ctx,
        &format!(
            "CREATE VIEW p AS FROM '{}'; CREATE VIEW g AS FROM '{}/parts/*.csv'; CREATE \
             EXTERNAL TABLE x STORED AS PARQUET LOCATION '{}'; CREATE VIEW ok AS SELECT 1 AS a",
            parquet.display(),
            fx.dir.path().display(),
            parquet.display()
        ),
    )
    .await;
    drop(ctx);
    let saved = std::fs::read_to_string(fx.db()).unwrap();
    std::fs::remove_file(&parquet).unwrap();
    std::fs::remove_file(fx.path("parts/one.csv")).unwrap();

    let (ctx, said) = open(&fx.db(), &quick()).await;
    assert_eq!(said.len(), 3, "{said:?}");
    assert!(said[0].starts_with("view p is unavailable: "), "{said:?}");
    assert!(said[1].starts_with("view g is unavailable: "), "{said:?}");
    assert!(said[2].starts_with("table x is unavailable: "), "{said:?}");
    assert!(
        said[2].ends_with("DROP TABLE x or restore the file"),
        "{said:?}"
    );
    assert_eq!(tables(&ctx).await, ["g", "ok", "p", "x"]);
    assert_eq!(ok(&ctx, "FROM ok").await, ["1"]);
    let e = err(&ctx, "FROM x").await;
    assert_eq!(e, said[2]);
    ok(&ctx, "DROP TABLE x").await;
    ok(&ctx, "DROP VIEW p").await;
    ok(&ctx, "DROP VIEW g").await;
    assert_eq!(catalog(&fx.db()), ["CREATE VIEW ok AS SELECT 1 AS a"]);
    drop(ctx);
    assert_ne!(std::fs::read_to_string(fx.db()).unwrap(), saved);
}

#[tokio::test]
async fn a_database_copied_without_its_files_opens() {
    let fx = Fixture::new();
    fx.write_csv("t.csv");
    let (ctx, _) = open(&fx.db(), &quick()).await;
    // Stored relative to the database's directory.
    ok(
        &ctx,
        &format!(
            "CREATE VIEW v AS FROM '{}'; CREATE VIEW w AS SELECT 1 AS x",
            fx.path("t.csv").display()
        ),
    )
    .await;
    drop(ctx);
    let elsewhere = tempfile::tempdir().unwrap();
    let copy = std::path::absolute(elsewhere.path())
        .unwrap()
        .join("copy.ursa");
    std::fs::copy(fx.db(), &copy).unwrap();

    for options in [OpenOptions::read_only(), quick()] {
        let (ctx, said) = open(&copy, &options).await;
        assert_eq!(
            said,
            [format!(
                "view v is unavailable: {}; DROP VIEW v or restore the file",
                no_file(&copy.with_file_name("t.csv"))
            )]
        );
        assert_eq!(ok(&ctx, "FROM w").await, ["1"]);
    }
    // Copying the file over brings it back.
    std::fs::copy(fx.path("t.csv"), copy.with_file_name("t.csv")).unwrap();
    let (ctx, said) = open(&copy, &quick()).await;
    assert!(said.is_empty(), "{said:?}");
    assert_eq!(ok(&ctx, "SELECT count(*) FROM v").await, ["2"]);
}

#[tokio::test]
async fn a_statement_that_defines_nothing_is_skipped_and_kept() {
    let fx = Fixture::new();
    let text = r#"{"format": "ursa", "version": 1, "statements": ["CREATE VIEW w AS SELECT 1 AS x", "SELEC nonsense"]}"#;
    std::fs::write(fx.db(), text).unwrap();
    let (ctx, said) = open(&fx.db(), &quick()).await;
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].starts_with(&format!(
            "statement 2 of the catalog of {} failed, and is skipped (it is kept in the \
             catalog): ",
            fx.db().display()
        )),
        "{said:?}"
    );
    assert_eq!(ok(&ctx, "FROM w").await, ["1"]);
    ok(&ctx, "CREATE VIEW y AS SELECT 2 AS x").await;
    assert_eq!(
        catalog(&fx.db()),
        [
            "CREATE VIEW w AS SELECT 1 AS x",
            "SELEC nonsense",
            "CREATE VIEW y AS SELECT 2 AS x"
        ]
    );
}
