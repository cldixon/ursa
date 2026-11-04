//! Files as tables: local paths and globs, and globs over object storage
//! (against an in-memory store standing in for S3).

use std::sync::Arc;

use datafusion::arrow::array::Int64Array;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::Result;
use datafusion::prelude::SessionContext;
use futures::TryStreamExt;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use url::Url;
use ursa_engine::{new_session, paths::table_url, user_message};

async fn count(ctx: &SessionContext, sql: &str) -> Result<i64> {
    let batches: Vec<RecordBatch> = ctx.sql(sql).await?.collect().await?;
    let column = batches[0].column(0);
    Ok(column
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("count(*) is an Int64")
        .value(0))
}

/// An in-memory bucket registered as `s3://bucket`, holding three CSV parts
/// and two files the globs below must skip.
async fn bucket(ctx: &SessionContext) -> Arc<InMemory> {
    let store = Arc::new(InMemory::new());
    for (path, body) in [
        ("data/part-1.csv", "src,dst\n0,1\n0,2\n"),
        ("data/part-2.csv", "src,dst\n1,2\n"),
        ("data/part-3.csv", "src,dst\n2,3\n3,4\n4,5\n"),
        ("data/notes.txt", "not a table\n"),
        ("other/part-9.csv", "src,dst\n9,9\n"),
    ] {
        store
            .put(
                &Path::from(path),
                PutPayload::from(body.as_bytes().to_vec()),
            )
            .await
            .unwrap();
    }
    ctx.runtime_env().register_object_store(
        &Url::parse("s3://bucket").unwrap(),
        Arc::clone(&store) as Arc<dyn ObjectStore>,
    );
    store
}

#[tokio::test]
async fn remote_glob_lists_only_matching_objects() {
    let ctx = new_session();
    let store = bucket(&ctx).await;
    let url = table_url("s3://bucket/data/part-*.csv").unwrap();
    let state = ctx.state();
    let mut found: Vec<String> = url
        .list_all_files(&state, store.as_ref(), "")
        .await
        .unwrap()
        .map_ok(|meta| meta.location.to_string())
        .try_collect()
        .await
        .unwrap();
    found.sort();
    assert_eq!(
        found,
        ["data/part-1.csv", "data/part-2.csv", "data/part-3.csv"]
    );

    let url = table_url("s3://bucket/data/part-[12].csv").unwrap();
    let found: Vec<_> = url
        .list_all_files(&state, store.as_ref(), "")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(found.len(), 2);
}

#[tokio::test]
async fn remote_glob_reads_as_one_table() {
    let ctx = new_session();
    bucket(&ctx).await;
    assert_eq!(
        count(&ctx, "SELECT count(*) FROM 's3://bucket/data/*.csv'")
            .await
            .unwrap(),
        6
    );
    assert_eq!(
        count(&ctx, "SELECT count(*) FROM 's3://bucket/data/part-?.csv'")
            .await
            .unwrap(),
        6
    );
    assert_eq!(
        count(&ctx, "SELECT count(*) FROM 's3://bucket/data/part-1.csv'")
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn remote_glob_with_no_match_says_so() {
    let ctx = new_session();
    bucket(&ctx).await;
    let err = ctx
        .sql("SELECT * FROM 's3://bucket/data/*.parquet'")
        .await
        .unwrap_err();
    assert_eq!(
        user_message(&err),
        "no file found at 's3://bucket/data/*.parquet'"
    );
}

#[tokio::test]
async fn local_files_globs_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("a.csv", "x\n1\n2\n"),
        ("b.csv", "x\n3\n"),
        ("c.txt", "x\n4\n"),
    ] {
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    let root = dir.path().display();
    let ctx = new_session();

    assert_eq!(
        count(&ctx, &format!("SELECT count(*) FROM '{root}/*.csv'"))
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        count(&ctx, &format!("SELECT count(*) FROM '{root}/a.csv'"))
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        count(&ctx, &format!("SELECT count(*) FROM 'file://{root}/*.csv'"))
            .await
            .unwrap(),
        3
    );

    let err = ctx
        .sql(&format!("SELECT * FROM '{root}/missing.csv'"))
        .await
        .unwrap_err();
    assert_eq!(
        user_message(&err),
        format!("no file found at '{root}/missing.csv'")
    );

    let err = ctx
        .sql(&format!("SELECT * FROM '{root}/c.txt'"))
        .await
        .unwrap_err();
    assert!(
        user_message(&err).starts_with(&format!("cannot tell the format of '{root}/c.txt'")),
        "{}",
        user_message(&err)
    );

    // A name that is not a location is still an unknown table.
    let err = ctx.sql("SELECT * FROM no_such_table").await.unwrap_err();
    assert!(
        user_message(&err).contains("table 'datafusion.public.no_such_table' not found"),
        "{}",
        user_message(&err)
    );
}
