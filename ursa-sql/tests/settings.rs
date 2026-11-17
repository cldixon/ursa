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

/// What SHOW says of `ursa.index_storage_limit` without a database.
const AUTO: &str = "auto (the default: in a database, 25% of the free space on its disk; at \
                    least 4GiB, at most 64GiB)";

#[tokio::test]
async fn show_an_ursa_setting() {
    let ctx = ursa_engine::new_session();
    let (schema, lines) = run(&ctx, "SHOW ursa.index_storage_limit").await.unwrap();
    assert_eq!(lines, [format!("ursa.index_storage_limit,{AUTO}")]);
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
        ["ursa.index_storage_limit,512MiB (SET for this session)"]
    );
    assert_eq!(
        rows(&ctx, "show URSA.Index_Storage_Limit").await,
        ["ursa.index_storage_limit,512MiB (SET for this session)"]
    );
    assert_eq!(
        rows(
            &ctx,
            "SET ursa.index_storage_limit = 1048576; SHOW ursa.index_storage_limit"
        )
        .await,
        ["ursa.index_storage_limit,1MiB (SET for this session)"]
    );
    // RESET, or 'auto', is the default again.
    assert_eq!(
        rows(
            &ctx,
            "RESET ursa.index_storage_limit; SHOW ursa.index_storage_limit"
        )
        .await,
        [format!("ursa.index_storage_limit,{AUTO}")]
    );
    rows(&ctx, "SET ursa.index_storage_limit = '1MiB'").await;
    assert_eq!(
        rows(
            &ctx,
            "SET ursa.index_storage_limit = 'auto'; SHOW ursa.index_storage_limit"
        )
        .await,
        [format!("ursa.index_storage_limit,{AUTO}")]
    );
    rows(&ctx, "SET ursa.index_storage_limit = 1048576").await;

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
        lines[0].starts_with(
            "ursa.index_storage_limit,1MiB (SET for this session),The most bytes of graph indexes"
        ),
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
        "'ursa.colour' is not an Ursa setting. Ursa's settings are: ursa.index_storage_limit, ursa.notices"
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
    assert!(all.contains(&format!("ursa.index_storage_limit,{AUTO}")));
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
        .contains(&"ursa.index_storage_limit,2GiB (SET for this session)".to_string()));
    let (schema, verbose) = run(&ctx, "SHOW ALL VERBOSE").await.unwrap();
    assert_eq!(schema.fields().len(), 3);
    assert!(verbose
        .iter()
        .any(|l| l
            .starts_with("ursa.index_storage_limit,2GiB (SET for this session),The most bytes")));
}

/// In a session with a database, SET lasts for the session and is never
/// stored, writable or not, as in DuckDB and Postgres (#353). SHOW says the
/// limit in effect: the database's default, a share of the free space on its
/// disk, unless SET.
#[tokio::test]
async fn show_in_a_database_session() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("club.ursa");
    let options = OpenOptions {
        lock_timeout: Duration::from_millis(200),
        ..OpenOptions::default()
    };
    let ctx = open_session(&db, &options).await.unwrap();
    let default = ursa_engine::database::database(&ctx.state())
        .unwrap()
        .default_index_storage_limit();
    let shown = format!(
        "ursa.index_storage_limit,{} (the default: ",
        ursa_engine::options::ByteSize(default.bytes)
    );
    let limit = rows(&ctx, "SHOW ursa.index_storage_limit").await;
    assert!(limit[0].starts_with(&shown), "{limit:?}");
    if default.free.is_some() {
        assert!(
            limit[0].ends_with(&format!(
                " free on the disk of {}; at least 4GiB, at most 64GiB)",
                db.display()
            )),
            "{limit:?}"
        );
    }
    assert!(rows(&ctx, "SHOW ALL").await.contains(&limit[0]));
    assert_eq!(
        rows(
            &ctx,
            "SET ursa.index_storage_limit = '8GiB'; SHOW ursa.index_storage_limit"
        )
        .await,
        ["ursa.index_storage_limit,8GiB (SET for this session)"]
    );
    rows(&ctx, "SET ursa.notices = 'off'").await;
    drop(ctx);
    // Nothing was stored: a later session has the defaults.
    assert!(!std::fs::read_to_string(&db).unwrap().contains("settings"));
    for read_only in [false, true] {
        let options = OpenOptions {
            read_only,
            ..options.clone()
        };
        let ctx = open_session(&db, &options).await.unwrap();
        let limit = rows(&ctx, "SHOW ursa.index_storage_limit").await;
        assert!(limit[0].starts_with(&shown), "{limit:?}");
        assert_eq!(rows(&ctx, "SHOW ursa.notices").await, ["ursa.notices,on"]);
        // A read-only session may SET too, for itself.
        rows(&ctx, "SET ursa.index_storage_limit = '1GiB'").await;
        rows(&ctx, "RESET ursa.index_storage_limit").await;
    }
    assert!(!std::fs::read_to_string(&db).unwrap().contains("settings"));
}
