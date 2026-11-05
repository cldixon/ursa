//! `SHOW ursa.<setting>` and `SHOW ALL` with Ursa's settings, in the shape of
//! DataFusion's `SHOW <name>`.

use std::time::Duration;

use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::error::Result;
use datafusion::prelude::SessionContext;
use ursa_engine::database::OpenOptions;
use ursa_engine::user_message;
use ursa_sql::{execute, open_session, parse_sql_for};

/// Run `sql`; the last statement's schema and rows as CSV lines.
async fn run(ctx: &SessionContext, sql: &str) -> Result<(SchemaRef, Vec<String>)> {
    let mut out = None;
    for statement in parse_sql_for(ctx, sql)? {
        let frame = execute(ctx, statement).await?;
        let schema = SchemaRef::new(frame.schema().as_arrow().clone());
        let batches = frame.collect().await?;
        let options = FormatOptions::default();
        let mut lines = Vec::new();
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
        out = Some((schema, lines));
    }
    Ok(out.expect("a statement"))
}

async fn rows(ctx: &SessionContext, sql: &str) -> Vec<String> {
    run(ctx, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", user_message(&e)))
        .1
}

async fn error(ctx: &SessionContext, sql: &str) -> String {
    user_message(&run(ctx, sql).await.expect_err(sql))
}

#[tokio::test]
async fn show_an_ursa_setting() {
    let ctx = ursa_engine::new_session();
    let (schema, lines) = run(&ctx, "SHOW ursa.index_storage_limit").await.unwrap();
    assert_eq!(lines, ["ursa.index_storage_limit,4GiB"]);
    // The shape of DataFusion's own SHOW <name>.
    let (datafusion, _) = run(&ctx, "SHOW datafusion.execution.batch_size")
        .await
        .unwrap();
    assert_eq!(schema, datafusion);
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, ["name", "value"]);

    // It shows what SET set, for the session, named either way round in case.
    assert_eq!(
        rows(
            &ctx,
            "SET ursa.index_storage_limit = '512MiB'; SHOW ursa.index_storage_limit"
        )
        .await,
        ["ursa.index_storage_limit,512MiB"]
    );
    assert_eq!(
        rows(&ctx, "show URSA.Index_Storage_Limit").await,
        ["ursa.index_storage_limit,512MiB"]
    );
    assert_eq!(
        rows(
            &ctx,
            "SET ursa.index_storage_limit = 1048576; SHOW ursa.index_storage_limit"
        )
        .await,
        ["ursa.index_storage_limit,1MiB"]
    );

    // VERBOSE adds the description, as DataFusion's does.
    let (schema, lines) = run(&ctx, "SHOW ursa.index_storage_limit VERBOSE")
        .await
        .unwrap();
    let (datafusion, _) = run(&ctx, "SHOW datafusion.execution.batch_size VERBOSE")
        .await
        .unwrap();
    assert_eq!(schema, datafusion);
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].starts_with("ursa.index_storage_limit,1MiB,The most bytes of graph indexes"),
        "{lines:?}"
    );
}

#[tokio::test]
async fn unknown_ursa_settings_are_named() {
    let ctx = ursa_engine::new_session();
    assert_eq!(
        error(&ctx, "SHOW ursa.index_storage_limt").await,
        "'ursa.index_storage_limt' is not an Ursa setting; did you mean \
         'ursa.index_storage_limit'?"
    );
    assert_eq!(
        error(&ctx, "SHOW ursa.colour").await,
        "'ursa.colour' is not an Ursa setting. Ursa's settings are: ursa.index_storage_limit"
    );
    // DataFusion's own SHOW is unchanged.
    assert_eq!(
        rows(&ctx, "SHOW datafusion.execution.batch_size").await,
        ["datafusion.execution.batch_size,8192"]
    );
    assert_eq!(
        error(&ctx, "SHOW nothing.here").await,
        "'nothing.here' is not a variable which can be viewed with 'SHOW'"
    );
}

#[tokio::test]
async fn show_all_lists_ursa_settings() {
    let ctx = ursa_engine::new_session();
    let (schema, all) = run(&ctx, "SHOW ALL").await.unwrap();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, ["name", "value"]);
    assert!(all.contains(&"ursa.index_storage_limit,4GiB".to_string()));
    assert!(
        all.iter().all(|l| !l.starts_with("index_storage_limit,")),
        "the setting is listed under its SET name only"
    );
    assert!(all.contains(&"datafusion.execution.batch_size,8192".to_string()));
    // Every row of df_settings, ordered by name.
    let settings = rows(&ctx, "SELECT count(*) FROM information_schema.df_settings").await;
    assert_eq!(settings, [all.len().to_string()]);
    let mut sorted = all.clone();
    sorted.sort();
    assert_eq!(all, sorted);

    rows(&ctx, "SET ursa.index_storage_limit = '2GiB'").await;
    assert!(rows(&ctx, "SHOW ALL")
        .await
        .contains(&"ursa.index_storage_limit,2GiB".to_string()));
    let (schema, verbose) = run(&ctx, "SHOW ALL VERBOSE").await.unwrap();
    assert_eq!(schema.fields().len(), 3);
    assert!(verbose
        .iter()
        .any(|l| l.starts_with("ursa.index_storage_limit,2GiB,The most bytes")));
}

/// In a session with a database, SHOW reads the session and stores nothing.
#[tokio::test]
async fn show_in_a_database_session() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("club.ursa");
    let options = OpenOptions {
        lock_timeout: Duration::from_millis(200),
        ..OpenOptions::default()
    };
    let ctx = open_session(&db, &options).await.unwrap();
    assert_eq!(
        rows(
            &ctx,
            "SET ursa.index_storage_limit = '8GiB'; SHOW ursa.index_storage_limit"
        )
        .await,
        ["ursa.index_storage_limit,8GiB"]
    );
    drop(ctx);
    let ctx = open_session(&db, &options).await.unwrap();
    assert_eq!(
        rows(&ctx, "SHOW ursa.index_storage_limit").await,
        ["ursa.index_storage_limit,4GiB"]
    );
}
