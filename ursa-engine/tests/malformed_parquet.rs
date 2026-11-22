//! A file named `.parquet` that is not valid Parquet gives an error naming the
//! file, never a panic (#332). parquet 59 subtracts with overflow when a
//! footer claims more metadata than the file holds (`PAR1PAR1`, a truncated
//! file).

use datafusion::error::Result;
use datafusion::prelude::SessionContext;
use futures::FutureExt;
use ursa_engine::{new_session, user_message};

/// Small malformed files: what a truncated download, a file still being
/// written, or a file that was never Parquet looks like; and what each error
/// says is wrong.
fn malformed() -> Vec<(&'static str, Vec<u8>, &'static str)> {
    let mut claims_too_much = b"PAR1".to_vec();
    claims_too_much.extend_from_slice(&[0u8; 16]);
    claims_too_much.extend_from_slice(&1000u32.to_le_bytes());
    claims_too_much.extend_from_slice(b"PAR1");
    let mut no_head = vec![b'x'; 20];
    no_head.extend_from_slice(&4u32.to_le_bytes());
    no_head.extend_from_slice(b"PAR1");
    vec![
        ("magic_only", b"PAR1".to_vec(), "it is 4 bytes"),
        ("two_magics", b"PAR1PAR1".to_vec(), "it is 8 bytes"),
        (
            "text",
            b"src,dst\n0,1\n1,2\n2,3\n3,4\n4,5\n".to_vec(),
            "does not end with the Parquet magic number",
        ),
        ("footer_too_long", claims_too_much, "may be truncated"),
        (
            "no_leading_magic",
            no_head,
            "does not start with the Parquet magic number",
        ),
    ]
}

/// The error `sql` gives, planned and run; a panic fails the test with the
/// statement.
async fn error_of(ctx: &SessionContext, sql: &str) -> String {
    let run = async {
        ctx.sql(sql).await?.collect().await?;
        Result::Ok(())
    };
    match std::panic::AssertUnwindSafe(run).catch_unwind().await {
        Ok(Ok(())) => panic!("{sql} should fail"),
        Ok(Err(e)) => user_message(&e),
        Err(_) => panic!("{sql} panicked"),
    }
}

async fn ok(ctx: &SessionContext, sql: &str) {
    ctx.sql(sql).await.unwrap().collect().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_parquet_files_error_naming_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::path::absolute(dir.path()).unwrap();
    for (name, bytes, why) in malformed() {
        let path = root.join(format!("{name}.parquet"));
        std::fs::write(&path, &bytes).unwrap();
        let shown = path.display().to_string();
        let expected = format!("cannot read '{shown}': it is not a valid Parquet file: ");
        let ctx = new_session();
        let message = error_of(&ctx, &format!("SELECT * FROM '{shown}'")).await;
        assert!(message.starts_with(&expected), "{name}: {message}");
        assert!(message.contains(why), "{name}: {message}");
        let message = error_of(
            &ctx,
            &format!("CREATE EXTERNAL TABLE t STORED AS PARQUET LOCATION '{shown}'"),
        )
        .await;
        assert!(
            message.contains(&expected) && message.contains(why),
            "{name}: {message}"
        );
        std::fs::remove_file(&path).unwrap();
    }
}

/// The number of rows `sql` returns.
async fn rows(ctx: &SessionContext, sql: &str) -> usize {
    ctx.sql(sql)
        .await
        .unwrap()
        .collect()
        .await
        .unwrap()
        .iter()
        .map(|b| b.num_rows())
        .sum()
}

/// The error an empty file gives, naming it.
fn empty_error(path: &std::path::Path) -> String {
    format!(
        "cannot read '{}': it is not a valid Parquet file: the file is empty",
        path.display()
    )
}

/// An empty `.parquet` file is an error naming it, as in DuckDB and PyArrow,
/// not zero rows. DataFusion's listing skips a 0-byte file before the format
/// sees it, so Ursa lists the files itself. (`read_parquet` is tested in
/// ursa-cli: it is opened by `ursa_sql::execute`.)
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_parquet_file_named_directly_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = std::path::absolute(dir.path())
        .unwrap()
        .join("empty.parquet");
    std::fs::write(&path, b"").unwrap();
    let shown = path.display();
    let expected = empty_error(&path);
    let ctx = new_session();
    for sql in [
        format!("SELECT * FROM '{shown}'"),
        format!("CREATE EXTERNAL TABLE t STORED AS PARQUET LOCATION '{shown}'"),
    ] {
        let message = error_of(&ctx, &sql).await;
        assert!(message.contains(&expected), "{sql}: {message}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_parquet_file_in_a_glob_or_directory_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::path::absolute(dir.path()).unwrap();
    let ctx = new_session();
    let good = root.join("a.parquet");
    ok(
        &ctx,
        &format!("COPY (SELECT 1 AS x) TO '{}'", good.display()),
    )
    .await;
    let glob = format!("{}/*.parquet", root.display());
    let directory = format!("{}/", root.display());
    assert_eq!(rows(&ctx, &format!("SELECT * FROM '{glob}'")).await, 1);
    assert_eq!(rows(&ctx, &format!("SELECT * FROM '{directory}'")).await, 1);

    let empty = root.join("b.parquet");
    std::fs::write(&empty, b"").unwrap();
    let expected = empty_error(&empty);
    // A new session: DataFusion caches a session's listings.
    let ctx = new_session();
    for sql in [
        format!("SELECT * FROM '{glob}'"),
        format!("SELECT * FROM '{directory}'"),
        format!("CREATE EXTERNAL TABLE g STORED AS PARQUET LOCATION '{glob}'"),
        format!("CREATE EXTERNAL TABLE d STORED AS PARQUET LOCATION '{directory}'"),
    ] {
        let message = error_of(&ctx, &sql).await;
        assert!(message.contains(&expected), "{sql}: {message}");
    }
    // The valid file still reads by itself.
    assert_eq!(
        rows(&ctx, &format!("SELECT * FROM '{}'", good.display())).await,
        1
    );
}

/// Only Parquet changes: DataFusion still skips an empty CSV or JSON file.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_csv_or_json_file_is_still_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::path::absolute(dir.path()).unwrap();
    std::fs::write(root.join("a.csv"), b"x\n1\n").unwrap();
    std::fs::write(root.join("b.csv"), b"").unwrap();
    std::fs::write(root.join("a.json"), b"{\"x\": 1}\n").unwrap();
    std::fs::write(root.join("b.json"), b"").unwrap();
    let ctx = new_session();
    for ext in ["csv", "json"] {
        let glob = format!("{}/*.{ext}", root.display());
        assert_eq!(
            rows(&ctx, &format!("SELECT * FROM '{glob}'")).await,
            1,
            "{ext}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_file_in_a_glob_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::path::absolute(dir.path()).unwrap();
    let ctx = new_session();
    let good = root.join("a.parquet");
    ok(
        &ctx,
        &format!("COPY (SELECT 1 AS x) TO '{}'", good.display()),
    )
    .await;
    let glob = format!("{}/*.parquet", root.display());
    ok(&ctx, &format!("SELECT * FROM '{glob}'")).await;
    // A file still being written: the glob names it.
    let bad = root.join("b.parquet");
    std::fs::write(&bad, b"PAR1PAR1").unwrap();
    // A new session: DataFusion caches a session's listings.
    let ctx = new_session();
    let message = error_of(&ctx, &format!("SELECT * FROM '{glob}'")).await;
    assert!(
        message.contains(&format!(
            "cannot read '{}': it is not a valid Parquet file",
            bad.display()
        )),
        "{message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_table_whose_file_is_truncated_later_errors() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::path::absolute(dir.path()).unwrap();
    let path = root.join("t.parquet");
    let ctx = new_session();
    ok(
        &ctx,
        &format!(
            "COPY (SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS t(x, y)) TO '{}'",
            path.display()
        ),
    )
    .await;
    ok(
        &ctx,
        &format!(
            "CREATE EXTERNAL TABLE t STORED AS PARQUET LOCATION '{}'",
            path.display()
        ),
    )
    .await;
    ok(&ctx, "SELECT count(*) FROM t").await;
    // Cut it to its last 8 bytes: the footer now claims more than the file.
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[bytes.len() - 12..]).unwrap();
    let message = error_of(&ctx, "SELECT * FROM t").await;
    assert!(
        message.contains("it is not a valid Parquet file"),
        "{message}"
    );
}
