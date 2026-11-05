//! Graph indexes stored in a `.ursa` database (#237): a later session over the
//! same files loads the index instead of building it. Each session here
//! stands in for a process: it opens the database, and closing it (dropping
//! the session) finishes its index writes and releases the lock. The CLI
//! tests run real processes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use datafusion::arrow::array::{Int64Array, RecordBatch};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::prelude::SessionContext;
use ursa_engine::database::index_store::read_manifest;
use ursa_engine::database::OpenOptions;
use ursa_engine::graph::{index_builds, index_loads};
use ursa_sql::{execute_with_text, open_session, parse_sql_with_text_for};

/// Write a Parquet edge list of `n` edges over about `n / 6` nodes, from a
/// seeded generator, in batches of 64K rows.
fn write_edges(path: &Path, n: usize, seed: u64) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("src", DataType::Int64, false),
        Field::new("dst", DataType::Int64, false),
    ]));
    let file = std::fs::File::create(path).unwrap();
    let mut writer = ArrowWriter::try_new(file, Arc::clone(&schema), None).unwrap();
    let nodes = (n / 6).max(2) as u64;
    let mut state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) % nodes
    };
    let mut written = 0;
    while written < n {
        let len = (n - written).min(1 << 16);
        let src: Vec<i64> = (0..len).map(|_| next() as i64).collect();
        let dst: Vec<i64> = (0..len).map(|_| next() as i64).collect();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int64Array::from(src)),
                Arc::new(Int64Array::from(dst)),
            ],
        )
        .unwrap();
        writer.write(&batch).unwrap();
        written += len;
    }
    writer.close().unwrap();
}

fn quick() -> OpenOptions {
    OpenOptions {
        lock_timeout: Duration::from_millis(500),
        ..OpenOptions::default()
    }
}

async fn open(path: &Path) -> SessionContext {
    open_session(path, &quick()).await.unwrap()
}

async fn open_read_only(path: &Path) -> SessionContext {
    open_session(path, &OpenOptions::read_only()).await.unwrap()
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
    run(ctx, sql).await.unwrap_or_else(|e| {
        panic!("{sql}: {}", ursa_engine::user_message(&e));
    })
}

/// `(builds, loads)` of `ctx`'s index cache.
fn counts(ctx: &SessionContext) -> (u64, u64) {
    (index_builds(ctx), index_loads(ctx))
}

/// Close a session as a process exit would: drop it, which waits for its
/// index writes and releases the database.
fn close(ctx: SessionContext) {
    drop(ctx);
}

/// `sql` with each quoted file name made a path into `dir`: a relative path
/// would resolve against the test process's current directory.
fn in_dir(dir: &Path, sql: &str) -> String {
    sql.replace("'edges", &format!("'{}/edges", dir.display()))
        .replace(
            "'lesmis_edges.csv'",
            &format!("'{}/lesmis_edges.csv'", dir.display()),
        )
}

const DEFINE: &str = "CREATE VIEW ties AS SELECT * FROM 'edges.parquet';
CREATE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));";

/// `SHOW` reads only DataFusion's own settings; `df_settings` lists extensions'
/// too, under their names without the prefix.
const SHOW_LIMIT: &str =
    "SELECT value FROM information_schema.df_settings WHERE name LIKE '%index_storage_limit'";

const QUERY: &str =
    "SELECT id, round(pagerank, 8) FROM pagerank(g) ORDER BY pagerank DESC, id LIMIT 10";

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    /// A directory with `edges.parquet` of `n` edges, and `club.ursa`
    /// defining graph `g` over it.
    async fn new(n: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        write_edges(&dir.path().join("edges.parquet"), n, 1);
        let fixture = Self { dir };
        let ctx = open(&fixture.db()).await;
        ok(&ctx, &in_dir(fixture.dir.path(), DEFINE)).await;
        close(ctx);
        fixture
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("club.ursa")
    }

    fn index_dir(&self) -> PathBuf {
        self.dir.path().join("club.ursa.d/indexes")
    }

    /// The `.idx` files in the index directory.
    fn index_files(&self) -> Vec<String> {
        let mut files: Vec<String> = std::fs::read_dir(self.index_dir())
            .map(|listing| {
                listing
                    .map(|e| e.unwrap().file_name().into_string().unwrap())
                    .filter(|name| name.ends_with(".idx"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_session_loads_the_index_instead_of_building_it() {
    let fixture = Fixture::new(300_000).await;

    let started = Instant::now();
    let first = open(&fixture.db()).await;
    let expected = ok(&first, QUERY).await;
    let first_query = started.elapsed();
    assert_eq!(counts(&first), (1, 0));
    // The same session reuses its in-memory index.
    ok(&first, QUERY).await;
    assert_eq!(counts(&first), (1, 0));
    close(first);
    let first_total = started.elapsed();

    let entries = read_manifest(&fixture.index_dir());
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(fixture.index_files(), [entries[0].file.clone()]);
    let size = std::fs::metadata(fixture.index_dir().join(&entries[0].file))
        .unwrap()
        .len();
    assert_eq!(entries[0].size, size);
    assert!(entries[0].key.fingerprint.contains("edges.parquet"));
    assert_eq!(entries[0].key.files.len(), 1);

    let started = Instant::now();
    let second = open(&fixture.db()).await;
    assert_eq!(ok(&second, QUERY).await, expected);
    let second_query = started.elapsed();
    assert_eq!(counts(&second), (0, 1));
    // Another algorithm over the same graph uses the loaded index.
    ok(&second, "SELECT count(*) FROM degree(g)").await;
    assert_eq!(counts(&second), (0, 1));
    close(second);

    let after = read_manifest(&fixture.index_dir());
    assert_eq!(after.len(), 1);
    assert!(
        after[0].last_used > entries[0].last_used,
        "a hit updates last_used"
    );
    assert_eq!(after[0].created, entries[0].created);
    eprintln!(
        "300K edges: first session {first_query:?} to the result ({first_total:?} with the \
         index write), second session {second_query:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_source_file_is_rebuilt_and_its_stale_index_never_used() {
    let fixture = Fixture::new(20_000).await;
    let first = open(&fixture.db()).await;
    let before = ok(&first, QUERY).await;
    close(first);
    let stale = read_manifest(&fixture.index_dir());
    assert_eq!(stale.len(), 1);

    // Another edge list at the same path.
    write_edges(&fixture.dir.path().join("edges.parquet"), 20_000, 2);
    let second = open(&fixture.db()).await;
    let after = ok(&second, QUERY).await;
    assert_eq!(counts(&second), (1, 0), "the stale index must not be used");
    assert_ne!(after, before);
    close(second);

    // The new index replaced the stale one (same fingerprint).
    let entries = read_manifest(&fixture.index_dir());
    assert_eq!(entries.len(), 1);
    assert_ne!(entries[0].key, stale[0].key);
    assert_eq!(fixture.index_files(), [entries[0].file.clone()]);

    // And it is what a fresh in-memory computation gives.
    let third = open(&fixture.db()).await;
    assert_eq!(ok(&third, QUERY).await, after);
    assert_eq!(counts(&third), (0, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_or_truncated_index_file_is_a_miss() {
    let fixture = Fixture::new(20_000).await;
    let first = open(&fixture.db()).await;
    let expected = ok(&first, QUERY).await;
    close(first);

    type Damage = fn(&mut Vec<u8>);
    let damages: [(&str, Damage); 4] = [
        ("truncated", |b| b.truncate(b.len() / 2)),
        ("one byte flipped", |b| {
            let at = b.len() / 2;
            b[at] ^= 1;
        }),
        ("empty", |b| b.clear()),
        ("not an index", |b| *b = b"hello".to_vec()),
    ];
    for (what, damage) in damages {
        let entries = read_manifest(&fixture.index_dir());
        assert_eq!(entries.len(), 1, "{what}");
        let path = fixture.index_dir().join(&entries[0].file);
        let mut bytes = std::fs::read(&path).unwrap();
        damage(&mut bytes);
        std::fs::write(&path, bytes).unwrap();

        let ctx = open(&fixture.db()).await;
        assert_eq!(ok(&ctx, QUERY).await, expected, "{what}");
        assert_eq!(counts(&ctx), (1, 0), "{what}");
        close(ctx);
        // The damaged file was replaced by a good one.
        let ctx = open(&fixture.db()).await;
        assert_eq!(ok(&ctx, QUERY).await, expected, "{what}");
        assert_eq!(counts(&ctx), (0, 1), "{what}");
        close(ctx);
    }

    // A missing file is a miss too, and so is a corrupt manifest.
    let entries = read_manifest(&fixture.index_dir());
    std::fs::remove_file(fixture.index_dir().join(&entries[0].file)).unwrap();
    let ctx = open(&fixture.db()).await;
    assert_eq!(ok(&ctx, QUERY).await, expected);
    assert_eq!(counts(&ctx), (1, 0));
    close(ctx);
    std::fs::write(fixture.index_dir().join("manifest.json"), "{not json").unwrap();
    let ctx = open(&fixture.db()).await;
    assert_eq!(ok(&ctx, QUERY).await, expected);
    assert_eq!(counts(&ctx), (1, 0));
    close(ctx);
    // The unlisted file left by the corrupt manifest was cleaned up.
    assert_eq!(fixture.index_files().len(), 1);
    assert_eq!(read_manifest(&fixture.index_dir()).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn files_the_manifest_does_not_list_are_removed() {
    let fixture = Fixture::new(1_000).await;
    std::fs::create_dir_all(fixture.index_dir()).unwrap();
    for leftover in ["0123456789abcdef.idx", "0123456789abcdef.idx.tmp"] {
        std::fs::write(fixture.index_dir().join(leftover), b"leftover").unwrap();
    }
    std::fs::write(fixture.index_dir().join("notes.txt"), b"mine").unwrap();
    let ctx = open(&fixture.db()).await;
    ok(&ctx, QUERY).await;
    close(ctx);
    let entries = read_manifest(&fixture.index_dir());
    assert_eq!(fixture.index_files(), [entries[0].file.clone()]);
    assert!(!fixture
        .index_dir()
        .join("0123456789abcdef.idx.tmp")
        .exists());
    // Files that are not Ursa's are left alone.
    assert!(fixture.index_dir().join("notes.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn indexes_are_evicted_least_recently_used_first_past_the_limit() {
    let fixture = Fixture::new(20_000).await;
    let dir = fixture.dir.path();
    write_edges(&dir.join("edges_b.parquet"), 20_000, 3);
    write_edges(&dir.join("edges_c.parquet"), 20_000, 4);
    let ctx = open(&fixture.db()).await;
    ok(
        &ctx,
        &in_dir(dir, "CREATE PROPERTY GRAPH b EDGE TABLES ('edges_b.parquet' SOURCE KEY (src) DESTINATION KEY (dst));
         CREATE PROPERTY GRAPH c EDGE TABLES ('edges_c.parquet' SOURCE KEY (src) DESTINATION KEY (dst));"),
    )
    .await;
    // Learn each index's size. Out-degree never builds the in-adjacency, so
    // the sizes do not depend on when the background write runs.
    for graph in ["g", "b", "c"] {
        ok(&ctx, &format!("SELECT count(*) FROM degree({graph})")).await;
    }
    close(ctx);
    let size_of = |name: &str| {
        read_manifest(&fixture.index_dir())
            .iter()
            .find(|e| e.key.fingerprint.contains(name))
            .unwrap()
            .size
    };
    let (g, b, c) = (
        size_of("/edges.parquet"),
        size_of("edges_b.parquet"),
        size_of("edges_c.parquet"),
    );
    // Start again from g alone.
    std::fs::remove_dir_all(fixture.index_dir()).unwrap();
    let ctx = open(&fixture.db()).await;
    ok(&ctx, "SELECT count(*) FROM degree(g)").await;
    close(ctx);

    // Room for g and either of b and c, not all three.
    let limit = g + b.max(c);
    let set = format!("SET ursa.index_storage_limit = '{limit}'");
    let ctx = open(&fixture.db()).await;
    ok(&ctx, &set).await;
    assert_eq!(
        ok(&ctx, SHOW_LIMIT).await,
        [ursa_engine::options::ByteSize(limit).to_string()]
    );
    ok(&ctx, "SELECT count(*) FROM degree(b)").await;
    // Wait for b's write, then use g again, so b is the least recently used.
    ursa_engine::database::flush(&ctx.state());
    ok(&ctx, "SELECT count(*) FROM degree(g)").await;
    assert_eq!(counts(&ctx), (1, 1));
    ursa_engine::database::flush(&ctx.state());
    assert_eq!(read_manifest(&fixture.index_dir()).len(), 2);
    ok(&ctx, "SELECT count(*) FROM degree(c)").await;
    ursa_engine::database::flush(&ctx.state());
    let kept: Vec<String> = read_manifest(&fixture.index_dir())
        .iter()
        .map(|e| e.key.fingerprint.clone())
        .collect();
    assert_eq!(kept.len(), 2, "{kept:?}");
    assert!(
        kept.iter().any(|f| f.contains("edges_c.parquet")),
        "{kept:?}"
    );
    assert!(
        kept.iter()
            .any(|f| f.contains("'edges.parquet'") || f.contains("/edges.parquet")),
        "{kept:?}"
    );
    assert!(
        !kept.iter().any(|f| f.contains("edges_b.parquet")),
        "{kept:?}"
    );
    assert_eq!(fixture.index_files().len(), 2);

    // An index larger than the limit on its own is not stored, and evicts
    // nothing.
    ok(&ctx, "SET ursa.index_storage_limit = '1KiB'").await;
    assert_eq!(ok(&ctx, SHOW_LIMIT).await, ["1KiB"]);
    ok(&ctx, "SELECT count(*) FROM degree(b)").await;
    ursa_engine::database::flush(&ctx.state());
    assert_eq!(read_manifest(&fixture.index_dir()).len(), 2);
    close(ctx);

    // The setting lasts for the session only.
    let ctx = open(&fixture.db()).await;
    assert_eq!(ok(&ctx, SHOW_LIMIT).await, ["4GiB"]);
    let err = run(&ctx, "SET ursa.index_storage_limit = '8 gigs'")
        .await
        .unwrap_err();
    assert!(
        ursa_engine::user_message(&err).contains("'8 gigs' is not a size"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_index_a_running_query_holds_is_not_evicted() {
    let fixture = Fixture::new(20_000).await;
    let dir = fixture.dir.path();
    write_edges(&dir.join("edges_b.parquet"), 20_000, 3);
    let ctx = open(&fixture.db()).await;
    ok(
        &ctx,
        &in_dir(dir, "CREATE PROPERTY GRAPH b EDGE TABLES ('edges_b.parquet' SOURCE KEY (src) DESTINATION KEY (dst));"),
    )
    .await;
    ok(&ctx, QUERY).await;
    close(ctx);
    let size = read_manifest(&fixture.index_dir())[0].size;

    // Room for one index. One query over both graphs: g is loaded, b built;
    // storing b would evict g, but the query holds g.
    let ctx = open(&fixture.db()).await;
    ok(
        &ctx,
        &format!("SET ursa.index_storage_limit = '{}'", size * 3 / 2),
    )
    .await;
    ok(
        &ctx,
        "SELECT count(*) FROM degree(g) AS a JOIN degree(b) AS b ON a.id = b.id",
    )
    .await;
    assert_eq!(counts(&ctx), (1, 1));
    close(ctx);
    // Either both are kept (the write ran while the query held g) or g was
    // evicted after the query ended; never b, the index just written.
    let kept = read_manifest(&fixture.index_dir());
    assert!(
        kept.iter()
            .any(|e| e.key.fingerprint.contains("edges_b.parquet")),
        "{kept:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_session_uses_stored_indexes_but_writes_none() {
    let fixture = Fixture::new(20_000).await;

    // Nothing stored yet: a reader builds, and stores nothing.
    let reader = open_read_only(&fixture.db()).await;
    let expected = ok(&reader, QUERY).await;
    assert_eq!(counts(&reader), (1, 0));
    close(reader);
    assert!(!fixture.index_dir().exists());

    let writer = open(&fixture.db()).await;
    ok(&writer, QUERY).await;
    close(writer);
    let entries = read_manifest(&fixture.index_dir());
    let manifest = std::fs::read(fixture.index_dir().join("manifest.json")).unwrap();

    // A reader loads it, even while a writer has the database open.
    let writer = open(&fixture.db()).await;
    let reader = open_read_only(&fixture.db()).await;
    assert_eq!(ok(&reader, QUERY).await, expected);
    assert_eq!(counts(&reader), (0, 1));
    close(reader);
    close(writer);
    // ... and does not touch the manifest, not even last_used.
    assert_eq!(
        std::fs::read(fixture.index_dir().join("manifest.json")).unwrap(),
        manifest
    );
    assert_eq!(read_manifest(&fixture.index_dir()), entries);
}

#[tokio::test(flavor = "multi_thread")]
async fn indexes_without_a_stable_key_are_not_stored() {
    let fixture = Fixture::new(1_000).await;
    let ctx = open(&fixture.db()).await;
    ok(
        &ctx,
        &in_dir(
            fixture.dir.path(),
            "CREATE VIEW sampled AS SELECT * FROM 'edges.parquet' WHERE random() >= 0;
         CREATE PROPERTY GRAPH s EDGE TABLES (sampled SOURCE KEY (src) DESTINATION KEY (dst));",
        ),
    )
    .await;
    ok(&ctx, "SELECT count(*) FROM degree(s)").await;
    close(ctx);
    assert!(read_manifest(&fixture.index_dir()).is_empty());
    assert!(fixture.index_files().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn string_ids_and_weights_work_with_a_loaded_index() {
    let dir = tempfile::tempdir().unwrap();
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../python/ursa/datasets/data");
    std::fs::copy(
        data.join("lesmis_edges.csv"),
        dir.path().join("lesmis_edges.csv"),
    )
    .unwrap();
    let db = dir.path().join("club.ursa");
    let define = "CREATE VIEW lesmis_edges AS SELECT * FROM 'lesmis_edges.csv';
         CREATE PROPERTY GRAPH lesmis EDGE TABLES (lesmis_edges SOURCE KEY (src) DESTINATION KEY (dst));";
    let query = "SELECT id, round(pagerank, 6) FROM pagerank(lesmis, weight => weight)
                 ORDER BY pagerank DESC, id LIMIT 5";
    let ctx = open(&db).await;
    ok(&ctx, &in_dir(dir.path(), define)).await;
    let expected = ok(&ctx, query).await;
    let unweighted = ok(
        &ctx,
        "SELECT id, degree FROM degree(lesmis) ORDER BY degree DESC, id LIMIT 5",
    )
    .await;
    assert_eq!(counts(&ctx), (1, 0));
    close(ctx);

    let ctx = open(&db).await;
    assert_eq!(ok(&ctx, query).await, expected);
    assert_eq!(
        ok(
            &ctx,
            "SELECT id, degree FROM degree(lesmis) ORDER BY degree DESC, id LIMIT 5"
        )
        .await,
        unweighted
    );
    assert_eq!(counts(&ctx), (0, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn traversals_use_a_stored_index() {
    let fixture = Fixture::new(20_000).await;
    let hop = "SELECT count(*) FROM hop(g, seeds => [0, 1, 2], n => 2, direction => 'both')";
    let path =
        "SELECT count(*) FROM shortest_path(g, source => 0, target => 1, direction => 'both')";
    let first = open(&fixture.db()).await;
    let expected = (ok(&first, hop).await, ok(&first, path).await);
    assert_eq!(counts(&first), (1, 0));
    close(first);
    assert_eq!(read_manifest(&fixture.index_dir()).len(), 1);

    let second = open(&fixture.db()).await;
    assert_eq!((ok(&second, hop).await, ok(&second, path).await), expected);
    assert_eq!(counts(&second), (0, 1));
}
