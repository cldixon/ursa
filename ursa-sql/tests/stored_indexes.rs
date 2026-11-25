//! Graph indexes stored in a `.ursa` database (#237): a later session over the
//! same files loads the index instead of building it. Each session here
//! stands in for a process: it opens the database, and closing it (dropping
//! the session) finishes its index writes and releases the lock. The CLI
//! tests run real processes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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

const SHOW_LIMIT: &str = "SHOW ursa.index_storage_limit";

/// The row `SHOW ursa.index_storage_limit` gives for `limit`, SET.
fn limit_row(limit: &str) -> [String; 1] {
    [format!("ursa.index_storage_limit,{limit}")]
}

/// Whether `rows` (of `SHOW ursa.index_storage_limit`) show the database's
/// default limit.
fn shows_the_default(ctx: &SessionContext, rows: &[String]) -> bool {
    let db = ursa_engine::database::database(&ctx.state()).unwrap();
    let default = ursa_engine::options::ByteSize(db.default_index_storage_limit().bytes);
    rows.len() == 1 && rows[0].starts_with(&format!("ursa.index_storage_limit,{default}"))
}

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
        limit_row(&ursa_engine::options::ByteSize(limit).to_string())
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
    assert_eq!(ok(&ctx, SHOW_LIMIT).await, limit_row("1KiB"));
    ok(&ctx, "SELECT count(*) FROM degree(b)").await;
    ursa_engine::database::flush(&ctx.state());
    assert_eq!(read_manifest(&fixture.index_dir()).len(), 2);
    close(ctx);

    // The setting lasts for the session only (#353), and the two indexes the
    // tiny limit was over are still there.
    let ctx = open(&fixture.db()).await;
    assert!(shows_the_default(&ctx, &ok(&ctx, SHOW_LIMIT).await));
    assert_eq!(read_manifest(&fixture.index_dir()).len(), 2);
    ok(&ctx, "SET ursa.index_storage_limit = '1KiB'").await;
    let err = run(&ctx, "SET ursa.index_storage_limit = '8 gigs'")
        .await
        .unwrap_err();
    assert!(
        ursa_engine::user_message(&err).contains("'8 gigs' is not a size"),
        "{err}"
    );
    // A refused value changes nothing; RESET goes back to the default.
    assert_eq!(ok(&ctx, SHOW_LIMIT).await, limit_row("1KiB"));
    ok(&ctx, "RESET ursa.index_storage_limit").await;
    assert!(shows_the_default(&ctx, &ok(&ctx, SHOW_LIMIT).await));
    close(ctx);
    assert!(!std::fs::read_to_string(fixture.db())
        .unwrap()
        .contains("settings"));
}

/// N33: lowering the limit never removes a stored index. A limit decides only
/// whether a new index is stored: with the stored indexes already over it,
/// none is, and nothing is evicted. The SET says so, once.
#[tokio::test(flavor = "multi_thread")]
async fn a_lower_limit_keeps_what_is_stored_and_stores_nothing_new() {
    let fixture = Fixture::new(20_000).await;
    let dir = fixture.dir.path();
    // b is small enough for the lower limit on its own.
    write_edges(&dir.join("edges_b.parquet"), 2_000, 3);
    let ctx = open(&fixture.db()).await;
    ok(
        &ctx,
        &in_dir(dir, "CREATE PROPERTY GRAPH b EDGE TABLES ('edges_b.parquet' SOURCE KEY (src) DESTINATION KEY (dst));"),
    )
    .await;
    ok(&ctx, "SELECT count(*) FROM degree(g)").await;
    close(ctx);
    let before = read_manifest(&fixture.index_dir());
    assert_eq!(before.len(), 1);
    let g = before[0].size;

    // A limit below what is stored, but with room for b on its own.
    let ctx = open(&fixture.db()).await;
    let said = notices(&ctx, "on").await;
    let limit = g / 2;
    ok(&ctx, &format!("SET ursa.index_storage_limit = {limit}")).await;
    assert_eq!(
        taken(&said),
        [format!(
            "{} of indexes are stored in {}, more than the {} limit; they are kept, and new \
             indexes won't be stored",
            ursa_engine::notices::size(g),
            fixture.db().display(),
            ursa_engine::options::ByteSize(limit)
        )]
    );
    // g is still used, and b is built but not stored: g is not evicted.
    ok(&ctx, "SELECT count(*) FROM degree(g)").await;
    ok(&ctx, "SELECT count(*) FROM degree(b)").await;
    assert_eq!(counts(&ctx), (1, 1));
    ursa_engine::database::flush(&ctx.state());
    let lines = taken(&said);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains(&format!(
            "; not stored: the indexes {} stores take {}, more than ursa.index_storage_limit \
             ({}); they are kept, and new indexes are not stored until it is raised",
            fixture.db().display(),
            ursa_engine::notices::size(g),
            ursa_engine::options::ByteSize(limit)
        )),
        "{lines:?}"
    );
    let after = read_manifest(&fixture.index_dir());
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].file, before[0].file);
    // A limit with room says nothing, and the next index is stored.
    ok(&ctx, "RESET ursa.index_storage_limit").await;
    assert!(taken(&said).is_empty());
    close(ctx);
    let ctx = open(&fixture.db()).await;
    ok(&ctx, "SELECT count(*) FROM degree(b)").await;
    close(ctx);
    assert_eq!(read_manifest(&fixture.index_dir()).len(), 2);
}

/// A collecting notice sink on `ctx`, at `level`.
async fn notices(ctx: &SessionContext, level: &str) -> Arc<Mutex<Vec<String>>> {
    let said = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let said = Arc::clone(&said);
        Arc::new(move |line: &str| said.lock().unwrap().push(line.to_string()))
    };
    ursa_engine::notices::set_sink(&ctx.state(), Some(sink));
    ok(ctx, &format!("SET ursa.notices = '{level}'")).await;
    said
}

fn taken(said: &Mutex<Vec<String>>) -> Vec<String> {
    std::mem::take(&mut *said.lock().unwrap())
}

/// N11: a read-only session's limit is its own, and never evicts; a later
/// writer obeys its own limit, the default.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_only_limit_never_evicts() {
    let fixture = Fixture::new(20_000).await;
    let dir = fixture.dir.path();
    write_edges(&dir.join("edges_b.parquet"), 20_000, 3);
    let ctx = open(&fixture.db()).await;
    ok(
        &ctx,
        &in_dir(dir, "CREATE PROPERTY GRAPH b EDGE TABLES ('edges_b.parquet' SOURCE KEY (src) DESTINATION KEY (dst));"),
    )
    .await;
    ok(&ctx, "SELECT count(*) FROM degree(g)").await;
    close(ctx);
    let before = read_manifest(&fixture.index_dir());
    assert_eq!(before.len(), 1);

    // A reader may SET a tiny limit: SHOW says so, and nothing is evicted
    // (a reader writes nothing).
    let reader = open_read_only(&fixture.db()).await;
    ok(&reader, "SET ursa.index_storage_limit = '1KiB'").await;
    assert_eq!(ok(&reader, SHOW_LIMIT).await, limit_row("1KiB"));
    ok(&reader, "SELECT count(*) FROM degree(b)").await;
    ok(&reader, "SELECT count(*) FROM degree(g)").await;
    assert_eq!(counts(&reader), (1, 1));
    close(reader);
    assert_eq!(read_manifest(&fixture.index_dir()), before);
    // ... and not stored.
    let catalog = std::fs::read_to_string(fixture.db()).unwrap();
    assert!(!catalog.contains("settings"), "{catalog}");

    // A writer meanwhile obeys its own limit (the default).
    let writer = open(&fixture.db()).await;
    assert!(shows_the_default(&writer, &ok(&writer, SHOW_LIMIT).await));
    ok(&writer, "SELECT count(*) FROM degree(b)").await;
    close(writer);
    assert_eq!(read_manifest(&fixture.index_dir()).len(), 2);
}

/// A limit Ursa 0.8 or 0.9 saved in the catalog (#327) is no longer applied
/// (#353): opening says so, once, and a writable open removes it. A reader
/// says so too, and changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_limit_saved_by_an_earlier_version_is_said_once_and_removed() {
    let fixture = Fixture::new(1_000).await;
    let catalog = std::fs::read_to_string(fixture.db()).unwrap();
    let saved = catalog.replacen(
        "\n}\n",
        ",\n  \"settings\": {\n    \"ursa.index_storage_limit\": \"50MiB\"\n  }\n}\n",
        1,
    );
    std::fs::write(fixture.db(), &saved).unwrap();
    let notice = format!(
        "{} has a saved ursa.index_storage_limit = 50MiB, which is no longer applied; SET now \
         lasts for the session. To keep it, add SET ursa.index_storage_limit = '50MiB' to \
         ~/.ursarc (or an -init file)",
        fixture.db().display()
    );
    // Open as a host does: its notice sink first, then the database.
    let open_heard = |read_only: bool| {
        let db = fixture.db();
        async move {
            let ctx = ursa_engine::new_session();
            let said = notices(&ctx, "on").await;
            let options = if read_only {
                OpenOptions::read_only()
            } else {
                quick()
            };
            ursa_sql::open_database(&ctx, &db, &options).await.unwrap();
            (ctx, said)
        }
    };

    let (reader, said) = open_heard(true).await;
    assert_eq!(taken(&said), std::slice::from_ref(&notice));
    assert!(shows_the_default(&reader, &ok(&reader, SHOW_LIMIT).await));
    close(reader);
    assert_eq!(std::fs::read_to_string(fixture.db()).unwrap(), saved);

    let (writer, said) = open_heard(false).await;
    assert_eq!(taken(&said), [notice]);
    assert!(shows_the_default(&writer, &ok(&writer, SHOW_LIMIT).await));
    let db = ursa_engine::database::database(&writer.state()).unwrap();
    assert_ne!(db.index_storage_limit(writer.state().config()), 50 << 20);
    drop(db);
    close(writer);
    assert_eq!(std::fs::read_to_string(fixture.db()).unwrap(), catalog);

    let (writer, said) = open_heard(false).await;
    assert!(taken(&said).is_empty());
    assert_eq!(ok(&writer, "SELECT count(*) FROM ties").await, ["1000"]);
}

/// Each notice in its case (N11, N12, N17, N4).
#[tokio::test(flavor = "multi_thread")]
async fn notices_say_each_build_store_skip_and_eviction() {
    let fixture = Fixture::new(20_000).await;
    let dir = fixture.dir.path();
    write_edges(&dir.join("edges_b.parquet"), 20_000, 3);

    // Every build and store, at 'all'.
    let ctx = open(&fixture.db()).await;
    let said = notices(&ctx, "all").await;
    ok(
        &ctx,
        &in_dir(dir, "CREATE PROPERTY GRAPH b EDGE TABLES ('edges_b.parquet' SOURCE KEY (src) DESTINATION KEY (dst));"),
    )
    .await;
    ok(&ctx, "SELECT count(*) FROM degree(g)").await;
    let lines = taken(&said);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "building the index for g (20.0K edges)…");
    assert!(
        lines[1].starts_with("built the index for g (20.0K edges) in ")
            && lines[1].contains(" s; stored in ")
            // Out-degree does not build the reverse adjacency (N32).
            && lines[1].ends_with(" once an algorithm adds the reverse adjacency)"),
        "{lines:?}"
    );
    close(ctx);
    let g = read_manifest(&fixture.index_dir())[0].size;

    // At 'on', a quick build that is stored says nothing...
    let ctx = open(&fixture.db()).await;
    let said = notices(&ctx, "on").await;
    // ...and a limit with room for one index evicts g to store b.
    ok(
        &ctx,
        &format!("SET ursa.index_storage_limit = '{}'", g * 3 / 2),
    )
    .await;
    ok(&ctx, "SELECT count(*) FROM degree(b)").await;
    let lines = taken(&said);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("evicted the stored index for g (")
            && lines[0].ends_with(&format!(
                "the least recently used) from {} to stay within ursa.index_storage_limit ({})",
                fixture.db().display(),
                ursa_engine::options::ByteSize(g * 3 / 2)
            )),
        "{lines:?}"
    );
    // A limit below what is stored says so, and keeps it (#353)...
    ok(&ctx, "SET ursa.index_storage_limit = '1KiB'").await;
    let lines = taken(&said);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].ends_with(&format!(
            " of indexes are stored in {}, more than the 1KiB limit; they are kept, and new \
             indexes won't be stored",
            fixture.db().display()
        )),
        "{lines:?}"
    );
    // ...and an index too large for the limit is said however quick.
    ok(&ctx, "SELECT count(*) FROM degree(g)").await;
    let lines = taken(&said);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("built the index for g (20.0K edges) in ")
            && lines[0].contains("; not stored: it takes ")
            && lines[0].contains(
                ", more than ursa.index_storage_limit (1KiB); SET ursa.index_storage_limit = '"
            )
            && lines[0].ends_with("' to keep it"),
        "{lines:?}"
    );
    ursa_engine::database::flush(&ctx.state());
    assert_eq!(read_manifest(&fixture.index_dir()).len(), 1);
    close(ctx);

    // Read-only: a build says it is not stored, at 'all' (and at 'on' when
    // it is slow).
    let reader = open_read_only(&fixture.db()).await;
    let said = notices(&reader, "all").await;
    ok(&reader, "SELECT count(*) FROM degree(g)").await;
    let lines = taken(&said);
    assert!(
        lines.last().unwrap().ends_with(&format!(
            "; not stored: {} is open read-only",
            fixture.db().display()
        )),
        "{lines:?}"
    );
    // A load, at 'all'.
    ok(&reader, "SELECT count(*) FROM degree(b)").await;
    let lines = taken(&said);
    assert!(
        lines[0].starts_with("loaded the index for b (20.0K edges) from "),
        "{lines:?}"
    );
    // 'off' says nothing.
    ok(&reader, "SET ursa.notices = 'off'").await;
    ok(&reader, "SELECT count(*) FROM pagerank(g)").await;
    assert!(taken(&said).is_empty());
}

/// N29: over a filter the statistics cannot settle, the build notice gives
/// the rows it reads, not a guess at the edges (#351).
#[tokio::test(flavor = "multi_thread")]
async fn a_filtered_build_says_the_rows_it_reads() {
    let fixture = Fixture::new(20_000).await;
    let ctx = open(&fixture.db()).await;
    ok(
        &ctx,
        "CREATE VIEW few AS SELECT * FROM ties WHERE src % 1000 = 0;
         CREATE PROPERTY GRAPH f EDGE TABLES (few SOURCE KEY (src) DESTINATION KEY (dst));",
    )
    .await;
    let said = notices(&ctx, "all").await;
    ok(&ctx, "SELECT count(*) FROM degree(f)").await;
    let lines = taken(&said);
    assert_eq!(
        lines[0], "reading about 20.0K rows to build the index for f…",
        "{lines:?}"
    );
    // The final notice gives the real count.
    let edges = ok(&ctx, "SELECT count(*) FROM ties WHERE src % 1000 = 0").await;
    let edges: u64 = edges[0].parse().unwrap();
    assert!(edges < 1_000, "{edges}");
    assert!(
        lines[1].starts_with(&format!("built the index for f ({edges} edges) in ")),
        "{lines:?}"
    );
    // Unfiltered, the statistics are exact.
    ok(&ctx, "SELECT count(*) FROM degree(g)").await;
    assert_eq!(taken(&said)[0], "building the index for g (20.0K edges)…");
}

/// N38: `EXPLAIN` marks a path search whose index the session holds, in
/// memory or stored, since its edge and vertex scans then do not run (#351);
/// and it no longer draws them under the node, but counts them on its line
/// (#384, N48).
#[tokio::test(flavor = "multi_thread")]
async fn explain_marks_a_path_search_over_a_held_index() {
    let fixture = Fixture::new(1_000).await;
    const PATHS: &str = "FROM GRAPH_TABLE (g
        MATCH p = ANY SHORTEST (a WHERE a.id = 1)-[e]->*(b) COLUMNS (b.id))";
    // The PathSearchExec line, and the lines of its subtree.
    let explain = |ctx: &SessionContext| {
        let ctx = ctx.clone();
        async move {
            let lines = ok(&ctx, &format!("EXPLAIN {PATHS}")).await;
            let lines: Vec<String> = lines
                .iter()
                .flat_map(|l| l.lines())
                .map(str::to_string)
                .collect();
            let at = lines
                .iter()
                .position(|l| l.contains("PathSearchExec"))
                .unwrap_or_else(|| panic!("no PathSearchExec in {lines:?}"));
            let depth = |l: &str| l.len() - l.trim_start().len();
            let below: Vec<String> = lines[at + 1..]
                .iter()
                .take_while(|l| depth(l) > depth(&lines[at]))
                .cloned()
                .collect();
            (lines[at].clone(), below)
        }
    };
    // The scans of the edge file under the node.
    let edge_scans = |below: &[String]| {
        below
            .iter()
            .filter(|l| l.contains("DataSourceExec") && l.contains("edges.parquet"))
            .count()
    };
    let ctx = open(&fixture.db()).await;
    // Not built yet: the sources are read, and drawn.
    let (line, below) = explain(&ctx).await;
    assert!(!line.contains("index="), "{line}");
    let scans = edge_scans(&below);
    assert!(scans > 0, "{below:#?}");
    ok(&ctx, PATHS).await;
    let (line, below) = explain(&ctx).await;
    assert!(
        line.ends_with(", index=cached, sources: 1 file, not read"),
        "{line}"
    );
    // One scan fewer: the edges' (the start nodes' scan is still read).
    assert_eq!(edge_scans(&below), scans - 1, "{below:#?}");
    close(ctx);
    // A later session holds it stored, and loads it without reading them.
    let ctx = open(&fixture.db()).await;
    let (line, below) = explain(&ctx).await;
    assert!(
        line.ends_with(", index=stored, sources: 1 file, not read"),
        "{line}"
    );
    assert_eq!(edge_scans(&below), scans - 1, "{below:#?}");
    ok(&ctx, PATHS).await;
    assert_eq!(counts(&ctx), (0, 1));
    close(ctx);
}

/// A database directory moved together with its data finds its stored
/// indexes: the key holds paths relative to the database (#327, N13).
#[tokio::test(flavor = "multi_thread")]
async fn a_moved_database_directory_still_hits_its_stored_indexes() {
    let root = tempfile::tempdir().unwrap();
    let before = root.path().join("before");
    std::fs::create_dir(&before).unwrap();
    write_edges(&before.join("edges.parquet"), 20_000, 1);
    let db = before.join("club.ursa");
    let ctx = open(&db).await;
    // A relative path, as a user writes it: stored relative to the database.
    let cwd = std::env::current_dir().unwrap();
    let relative = pathdiff(&before.join("edges.parquet"), &cwd);
    ok(
        &ctx,
        &format!(
            "CREATE VIEW ties AS SELECT * FROM '{}';
             CREATE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
            relative.display()
        ),
    )
    .await;
    let expected = ok(&ctx, QUERY).await;
    assert_eq!(counts(&ctx), (1, 0));
    close(ctx);
    let entries = read_manifest(&before.join("club.ursa.d/indexes"));
    assert_eq!(entries.len(), 1);
    let key = entries[0].key.to_json().to_string();
    assert!(!key.contains(&before.display().to_string()), "{key}");

    // The replayed definition reads the file by its absolute path: a hit.
    let ctx = open(&db).await;
    assert_eq!(ok(&ctx, QUERY).await, expected);
    assert_eq!(counts(&ctx), (0, 1));
    close(ctx);

    let after = root.path().join("after");
    std::fs::rename(&before, &after).unwrap();
    let ctx = open(&after.join("club.ursa")).await;
    assert_eq!(ok(&ctx, QUERY).await, expected);
    assert_eq!(counts(&ctx), (0, 1), "the moved database loads its index");
    close(ctx);
}

/// `path` relative to `base` (both absolute), as `../..` and names.
fn pathdiff(path: &Path, base: &Path) -> PathBuf {
    let path: Vec<_> = path.components().collect();
    let base: Vec<_> = base.components().collect();
    let common = path.iter().zip(&base).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..base.len() {
        out.push("..");
    }
    for part in &path[common..] {
        out.push(part.as_os_str());
    }
    out
}

/// The key has only the options that change what is read: settings that
/// change DataFusion's other listing defaults leave it as it was (#327, N13).
#[tokio::test(flavor = "multi_thread")]
async fn the_key_ignores_unrelated_listing_options() {
    let fixture = Fixture::new(20_000).await;
    let ctx = open(&fixture.db()).await;
    let expected = ok(&ctx, QUERY).await;
    close(ctx);

    let ctx = open(&fixture.db()).await;
    for set in [
        "SET datafusion.execution.target_partitions = 3",
        "SET datafusion.execution.collect_statistics = false",
        "SET datafusion.execution.parquet.pushdown_filters = true",
        "SET datafusion.execution.parquet.created_by = 'someone else'",
        "SET datafusion.execution.parquet.max_row_group_size = 1000",
    ] {
        ok(&ctx, set).await;
    }
    // A new table over the same file, made under those settings.
    ok(
        &ctx,
        &in_dir(
            fixture.dir.path(),
            "CREATE OR REPLACE VIEW ties AS SELECT * FROM 'edges.parquet'",
        ),
    )
    .await;
    assert_eq!(ok(&ctx, QUERY).await, expected);
    assert_eq!(counts(&ctx), (0, 1));
}

/// An index stored by an older version (manifest version 1) is a miss, and
/// its file is removed once, with a notice.
#[tokio::test(flavor = "multi_thread")]
async fn indexes_stored_by_an_older_version_are_rebuilt() {
    let fixture = Fixture::new(1_000).await;
    std::fs::create_dir_all(fixture.index_dir()).unwrap();
    std::fs::write(fixture.index_dir().join("00000000000000aa.idx"), b"old").unwrap();
    std::fs::write(
        fixture.index_dir().join("manifest.json"),
        r#"{"format": "ursa-indexes", "version": 1, "indexes": [{"file": "00000000000000aa.idx",
            "size": 3, "created": 1, "last_used": 1, "key": {"fingerprint": "f", "files": []}}]}"#,
    )
    .unwrap();
    let ctx = open(&fixture.db()).await;
    let said = notices(&ctx, "on").await;
    ok(&ctx, QUERY).await;
    assert_eq!(counts(&ctx), (1, 0));
    assert_eq!(
        taken(&said),
        [format!(
            "removed 1 stored index (3 B) from {} written by another version of ursa; each is \
             built again when it is first used",
            fixture.db().display()
        )]
    );
    close(ctx);
    let entries = read_manifest(&fixture.index_dir());
    assert_eq!(entries.len(), 1);
    assert_eq!(fixture.index_files(), [entries[0].file.clone()]);
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
    let hop = "SELECT count(*) FROM hop(g, seeds => [0, 1, 2], steps => 2, direction => 'both')";
    let path =
        "SELECT count(*) FROM shortest_path(g, source => 0, target => 1, direction => 'both')";
    // The edges among 200 nodes, and the scan of the file they stand for.
    let induced = "SELECT src, dst FROM induced_edges(g, nodes => \
                   (SELECT * FROM generate_series(0, 199))) ORDER BY src, dst";
    let scan = in_dir(
        fixture.dir.path(),
        "SELECT src, dst FROM 'edges.parquet' WHERE src < 200 AND dst < 200 ORDER BY src, dst",
    );
    let first = open(&fixture.db()).await;
    let expected = (ok(&first, hop).await, ok(&first, path).await);
    let edges = ok(&first, induced).await;
    assert_eq!(edges, ok(&first, &scan).await);
    assert!(edges.len() > 20, "{}", edges.len());
    assert_eq!(counts(&first), (1, 0));
    close(first);
    assert_eq!(read_manifest(&fixture.index_dir()).len(), 1);

    let second = open(&fixture.db()).await;
    // From the stored index: loaded, never built.
    assert_eq!(ok(&second, induced).await, edges);
    assert_eq!(counts(&second), (0, 1));
    assert_eq!((ok(&second, hop).await, ok(&second, path).await), expected);
    assert_eq!(counts(&second), (0, 1));
}

/// The stored index file of the only manifest entry, and its key.
fn the_index(fixture: &Fixture) -> (PathBuf, ursa_engine::database::IndexKey) {
    let entries = read_manifest(&fixture.index_dir());
    assert_eq!(entries.len(), 1, "{entries:?}");
    (
        fixture.index_dir().join(&entries[0].file),
        entries[0].key.clone(),
    )
}

/// The format version of the index file at `path`.
fn version_of(path: &Path) -> u32 {
    let bytes = std::fs::read(path).unwrap();
    u32::from_le_bytes(bytes[8..12].try_into().unwrap())
}

/// #382: an index stored by 0.10 (format version 2) loads, gives the same
/// results, and a writer rewrites it in the current format, which the next
/// process maps.
#[tokio::test(flavor = "multi_thread")]
async fn an_index_stored_by_0_10_loads_and_is_rewritten_in_the_current_format() {
    use ursa_engine::database::index_file::{read_file, Encoded, VERSION, VERSION_2};
    let fixture = Fixture::new(20_000).await;
    let first = open(&fixture.db()).await;
    let expected = ok(&first, QUERY).await;
    close(first);
    let (path, key) = the_index(&fixture);
    assert_eq!(version_of(&path), VERSION);
    // The same index, as 0.10 wrote it.
    let stored = read_file(&path, &key).unwrap();
    let mut v2 = Vec::new();
    Encoded::new(&key, &stored.topology, &stored.ids)
        .unwrap()
        .as_version_2()
        .write_to(&mut v2)
        .unwrap();
    drop(stored);
    std::fs::write(&path, &v2).unwrap();
    assert_eq!(version_of(&path), VERSION_2);

    // A reader loads it, whole, and leaves it as it is.
    let reader = open_read_only(&fixture.db()).await;
    assert_eq!(ok(&reader, QUERY).await, expected);
    assert_eq!(counts(&reader), (0, 1));
    close(reader);
    assert_eq!(version_of(&path), VERSION_2);

    // A writer loads it and rewrites it, saying so.
    let ctx = open(&fixture.db()).await;
    let said = notices(&ctx, "on").await;
    assert_eq!(ok(&ctx, QUERY).await, expected);
    assert_eq!(counts(&ctx), (0, 1));
    let said = taken(&said);
    assert!(
        said.iter()
            .any(|n| n.contains("rewriting the stored index for g") && n.contains("older version")),
        "{said:?}"
    );
    close(ctx);
    let (path, _) = the_index(&fixture);
    assert_eq!(version_of(&path), VERSION);

    // The next process maps it: the same results, nothing said.
    let ctx = open(&fixture.db()).await;
    let said = notices(&ctx, "on").await;
    assert_eq!(ok(&ctx, QUERY).await, expected);
    assert_eq!(counts(&ctx), (0, 1));
    assert_eq!(taken(&said), Vec::<String>::new());
}

/// #421: an index stored by 0.11 (format version 3) has no lookup table for
/// its node ids. It loads and gives the same results, for a query that names
/// nodes too; a writer rewrites it in the current format, and the next
/// process finds the table in the file.
#[tokio::test(flavor = "multi_thread")]
async fn an_index_stored_by_0_11_loads_and_is_rewritten_with_its_lookup_table() {
    use ursa_engine::database::index_file::{read_file, Encoded, VERSION, VERSION_3};
    let fixture = Fixture::new(20_000).await;
    let hop =
        "SELECT seed, id, distance FROM hop(g, seeds => [0, 1, 2], steps => 2) ORDER BY seed, id";
    let first = open(&fixture.db()).await;
    let expected = ok(&first, QUERY).await;
    let hops = ok(&first, hop).await;
    assert!(hops.len() > 3, "{hops:?}");
    close(first);
    let (path, key) = the_index(&fixture);
    assert_eq!(version_of(&path), VERSION);
    // The same index, as 0.11 wrote it.
    let stored = read_file(&path, &key).unwrap();
    let mut v3 = Vec::new();
    Encoded::new(&key, &stored.topology, &stored.ids)
        .unwrap()
        .as_version_3()
        .write_to(&mut v3)
        .unwrap();
    drop(stored);
    std::fs::write(&path, &v3).unwrap();
    assert_eq!(version_of(&path), VERSION_3);

    // A reader loads it and leaves it as it is: an algorithm, then a
    // traversal from seeds, which builds the lookup table.
    let reader = open_read_only(&fixture.db()).await;
    assert_eq!(ok(&reader, QUERY).await, expected);
    assert_eq!(ok(&reader, hop).await, hops);
    assert_eq!(counts(&reader), (0, 1));
    close(reader);
    assert_eq!(version_of(&path), VERSION_3);

    // A writer loads it and rewrites it, saying so.
    let ctx = open(&fixture.db()).await;
    let said = notices(&ctx, "on").await;
    assert_eq!(ok(&ctx, hop).await, hops);
    assert_eq!(ok(&ctx, QUERY).await, expected);
    assert_eq!(counts(&ctx), (0, 1));
    let said = taken(&said);
    assert!(
        said.iter()
            .any(|n| n.contains("rewriting the stored index for g")
                && n.contains("older version")
                && n.contains("lookup table")),
        "{said:?}"
    );
    close(ctx);
    let (path, _) = the_index(&fixture);
    assert_eq!(version_of(&path), VERSION);

    // The next process maps it: the same results, nothing said.
    let ctx = open(&fixture.db()).await;
    let said = notices(&ctx, "on").await;
    assert_eq!(ok(&ctx, hop).await, hops);
    assert_eq!(ok(&ctx, QUERY).await, expected);
    assert_eq!(counts(&ctx), (0, 1));
    assert_eq!(taken(&said), Vec::<String>::new());
}

/// #421: a later process finds nodes by their string ids through the lookup
/// table stored with the index, and gives the results of the process that
/// built it: for seeds that are nodes, one that is not, and a join on ids.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_process_finds_string_ids_through_the_stored_lookup_table() {
    let fixture = Fixture::new(20_000).await;
    let dir = fixture.dir.path();
    let queries = [
        "SELECT seed, id, distance FROM hop(s, seeds => (SELECT unnest(['node-0', 'node-17', 'nobody'])), steps => 2) ORDER BY seed, id",
        "SELECT count(*) FROM hop(s, seeds => (SELECT id FROM degree(s) WHERE id LIKE '%7'), steps => 1)",
        "SELECT id, round(pagerank, 8) FROM pagerank(s) ORDER BY pagerank DESC, id LIMIT 10",
        "SELECT count(*), sum(length(id)) FROM degree(s)",
    ];
    let first = open(&fixture.db()).await;
    ok(
        &first,
        &in_dir(
            dir,
            "CREATE VIEW named AS SELECT 'node-' || src AS src, 'node-' || dst AS dst FROM 'edges.parquet';
             CREATE PROPERTY GRAPH s EDGE TABLES (named SOURCE KEY (src) DESTINATION KEY (dst));",
        ),
    )
    .await;
    let mut expected = Vec::new();
    for query in queries {
        expected.push(ok(&first, query).await);
    }
    assert!(expected[0].len() > 2, "{:?}", expected[0]);
    assert_eq!(counts(&first), (1, 0));
    close(first);

    for read_only in [true, false] {
        let ctx = match read_only {
            true => open_read_only(&fixture.db()).await,
            false => open(&fixture.db()).await,
        };
        for (query, expected) in queries.iter().zip(&expected) {
            assert_eq!(&ok(&ctx, query).await, expected, "{query}");
        }
        assert_eq!(counts(&ctx), (0, 1));
        close(ctx);
    }
}

/// The byte offset of section `kind` in the version 3 index file `bytes`.
fn section_offset(bytes: &[u8], kind: u32) -> usize {
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    let table = (64 + u64_at(32) as usize).div_ceil(8) * 8;
    (0..u32_at(40) as usize)
        .map(|i| table + 32 * i)
        .find(|&entry| u32_at(entry) == kind)
        .map(|entry| u64_at(entry + 8) as usize)
        .unwrap()
}

/// #382: a stored index's edge arrays are checked before an algorithm reads
/// them whole, not when a traversal reads a few of them. One found damaged
/// is dropped and built again, with a notice, and the algorithm's result is
/// the right one.
#[tokio::test(flavor = "multi_thread")]
async fn a_damaged_stored_index_is_built_again_before_an_algorithm_reads_it() {
    let fixture = Fixture::new(20_000).await;
    let hop = "SELECT count(*) FROM hop(g, seeds => [0, 1, 2], steps => 1)";
    let first = open(&fixture.db()).await;
    let expected = ok(&first, QUERY).await;
    close(first);
    let (path, _) = the_index(&fixture);
    let mut bytes = std::fs::read(&path).unwrap();
    // The low bit of an edge id, deep in the edge arrays.
    let at = section_offset(&bytes, 3) + 4 * 1000;
    bytes[at] ^= 1;
    std::fs::write(&path, &bytes).unwrap();

    // A reader cannot drop it, and builds the index instead of using it.
    let reader = open_read_only(&fixture.db()).await;
    assert_eq!(ok(&reader, QUERY).await, expected);
    assert_eq!(counts(&reader), (1, 0));
    assert_eq!(ok(&reader, QUERY).await, expected);
    close(reader);
    assert!(path.exists());

    let ctx = open(&fixture.db()).await;
    let said = notices(&ctx, "on").await;
    ok(&ctx, hop).await;
    assert_eq!(counts(&ctx), (0, 1));
    assert_eq!(ok(&ctx, QUERY).await, expected);
    // Loaded, found damaged before PageRank read it, built again.
    assert_eq!(counts(&ctx), (1, 0));
    let said = taken(&said);
    assert!(
        said.iter()
            .any(|n| n.contains("the stored index for g is damaged")
                && n.contains("does not match")
                && n.contains("building it again")),
        "{said:?}"
    );
    close(ctx);

    // The damaged file was replaced by a good one.
    let ctx = open(&fixture.db()).await;
    assert_eq!(ok(&ctx, QUERY).await, expected);
    assert_eq!(counts(&ctx), (0, 1));
}

/// #382: a process keeps reading the index it loaded while another process
/// replaces its file: evicts it (the file is removed), then stores it again
/// (a new file renamed into its place). On Unix the first process has the
/// file mapped, and reads the old file's pages, whole, after both.
#[tokio::test(flavor = "multi_thread")]
async fn an_index_in_use_survives_another_process_replacing_its_file() {
    let fixture = Fixture::new(20_000).await;
    let dir = fixture.dir.path();
    write_edges(&dir.join("edges_b.parquet"), 20_000, 3);
    let ctx = open(&fixture.db()).await;
    ok(
        &ctx,
        &in_dir(dir, "CREATE PROPERTY GRAPH b EDGE TABLES ('edges_b.parquet' SOURCE KEY (src) DESTINATION KEY (dst));"),
    )
    .await;
    let expected = ok(&ctx, QUERY).await;
    close(ctx);
    let (path, _) = the_index(&fixture);
    let size = read_manifest(&fixture.index_dir())[0].size;

    // A reader loads g, for a traversal.
    let reader = open_read_only(&fixture.db()).await;
    ok(
        &reader,
        "SELECT count(*) FROM hop(g, seeds => [0], steps => 1)",
    )
    .await;
    assert_eq!(counts(&reader), (0, 1));

    // A writer with room for one index stores b, evicting g, then g again,
    // evicting b: g's file is removed, then written anew in its place.
    let writer = open(&fixture.db()).await;
    let set = format!("SET ursa.index_storage_limit = '{}'", size * 3 / 2);
    ok(&writer, &set).await;
    ok(&writer, "SELECT count(*) FROM pagerank(b)").await;
    ursa_engine::database::flush(&writer.state());
    assert!(!path.exists());
    assert_eq!(ok(&writer, QUERY).await, expected);
    close(writer);
    assert!(path.exists());

    // The reader runs PageRank over the index it loaded before: every array
    // read whole, its checksums checked, the same result.
    assert_eq!(ok(&reader, QUERY).await, expected);
    assert_eq!(counts(&reader), (0, 1));
}

/// N47 (#381): the reverse adjacency, appended to a stored index when the
/// database closes, is said at 'all', and at 'on' when it brings the stored
/// indexes close to the limit.
#[tokio::test(flavor = "multi_thread")]
async fn storing_the_reverse_adjacency_is_said() {
    let size = ursa_engine::notices::size;
    let stored = |fixture: &Fixture, before: u64, after: u64| {
        format!(
            "stored the reverse adjacency of g in {} (+{}, {} in all)",
            fixture.db().display(),
            size(after - before),
            size(after)
        )
    };
    let fixture = Fixture::new(20_000).await;
    let ctx = open(&fixture.db()).await;
    let said = notices(&ctx, "all").await;
    ok(&ctx, "SELECT count(*) FROM degree(g)").await;
    ursa_engine::database::flush(&ctx.state());
    let before = read_manifest(&fixture.index_dir())[0].size;
    taken(&said);
    ok(&ctx, "SELECT count(*) FROM pagerank(g)").await;
    ursa_engine::database::flush(&ctx.state());
    let after = read_manifest(&fixture.index_dir())[0].size;
    assert!(after > before);
    assert_eq!(taken(&said), [stored(&fixture, before, after)]);
    close(ctx);

    // At 'on', only when it brings them close to the limit.
    for (limit, heard) in [(after * 100, false), (after + after / 20, true)] {
        let fixture = Fixture::new(20_000).await;
        let ctx = open(&fixture.db()).await;
        let said = notices(&ctx, "on").await;
        ok(&ctx, &format!("SET ursa.index_storage_limit = {limit}")).await;
        ok(&ctx, "SELECT count(*) FROM degree(g)").await;
        ursa_engine::database::flush(&ctx.state());
        ok(&ctx, "SELECT count(*) FROM pagerank(g)").await;
        ursa_engine::database::flush(&ctx.state());
        let lines = taken(&said);
        if heard {
            assert_eq!(
                lines,
                [format!(
                    "{}; the stored indexes now take {} of ursa.index_storage_limit ({})",
                    stored(&fixture, before, after),
                    size(after),
                    ursa_engine::options::ByteSize(limit)
                )]
            );
        } else {
            assert!(lines.is_empty(), "{lines:?}");
        }
        close(ctx);
    }
}

/// FNV-1a over `rows`, each followed by a newline: a hash that is the same
/// on every platform and run.
fn fnv(rows: &[String]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for row in rows {
        for &b in row.as_bytes().iter().chain(b"\n") {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// Path searches give the same rows, in the same order, from an index the
/// session built and from one a later session loads from the database
/// (memory-mapped), and those rows are pinned: recorded before the search
/// was made faster (#451). The queries have no `ORDER BY` where the order
/// is the search's own.
#[tokio::test(flavor = "multi_thread")]
async fn path_searches_give_pinned_rows_from_a_built_and_a_stored_index() {
    let fixture = Fixture::new(20_000).await;
    let queries: &[(&str, usize, u64)] = &[
        // A {1,3} search from one node, every path column.
        (
            "FROM GRAPH_TABLE (g MATCH p = ANY SHORTEST (a WHERE a.id = 0)-[e]->{1,3}(b)
               COLUMNS (b.id AS b, path_length(p) AS len, vertices(p) AS v, edges(p) AS es))",
            130,
            4398551478105111179,
        ),
        // Its histogram, as a high-degree start's would be asked.
        (
            "SELECT len, count(*) FROM GRAPH_TABLE (g
               MATCH p = ANY SHORTEST (a WHERE a.id = 0)-[e]->{1,3}(b)
               COLUMNS (path_length(p) AS len)) GROUP BY len ORDER BY len",
            3,
            15902166953952693435,
        ),
        // Unbounded, to every node it reaches.
        (
            "FROM GRAPH_TABLE (g MATCH p = ANY SHORTEST (a WHERE a.id = 3)-[e]->*(b)
               COLUMNS (b.id AS b, vertices(p) AS v))",
            3325,
            13449080192362342466,
        ),
        // ALL SHORTEST, either way along the edges.
        (
            "FROM GRAPH_TABLE (g MATCH p = ALL SHORTEST (a WHERE a.id = 5)-[e]-{1,2}(b)
               COLUMNS (b.id AS b, edges(p) AS es))",
            214,
            12504163379020412563,
        ),
        // Against the edges.
        (
            "FROM GRAPH_TABLE (g MATCH p = ANY SHORTEST (a WHERE a.id = 9)<-[e]-{1,3}(b)
               COLUMNS (b.id AS b, vertices(p) AS v))",
            213,
            11368705239152041053,
        ),
        // Several start nodes.
        (
            "FROM GRAPH_TABLE (g
               MATCH p = ANY SHORTEST (a WHERE a.id IN (3, 1, 4, 159, 26))-[e]->{1,2}(b)
               COLUMNS (a.id AS a, b.id AS b, edges(p) AS es))",
            179,
            6849450228634005670,
        ),
        // One pair: the search stops at the target.
        (
            "FROM GRAPH_TABLE (g MATCH p = ANY SHORTEST (a WHERE a.id = 0)-[e]->*(b WHERE b.id = 77)
               COLUMNS (vertices(p) AS v, edges(p) AS es))",
            1,
            2679366451992929664,
        ),
        // SHORTEST 2 trails to three targets.
        (
            "FROM GRAPH_TABLE (g
               MATCH p = SHORTEST 2 TRAIL (a WHERE a.id = 2)-[e]->{1,3}(b WHERE b.id IN (10, 20, 30))
               COLUMNS (b.id AS b, edges(p) AS es))",
            1,
            11495189137844326480,
        ),
        // Every walk of one or two edges.
        (
            "FROM GRAPH_TABLE (g MATCH p = (a WHERE a.id = 1)-[e]->{1,2}(b)
               COLUMNS (b.id AS b, edges(p) AS es))",
            52,
            15388977107142198429,
        ),
        // ACYCLIC, either way.
        (
            "FROM GRAPH_TABLE (g MATCH p = ACYCLIC (a WHERE a.id = 7)-[e]-{2,2}(b)
               COLUMNS (b.id AS b, vertices(p) AS v))",
            85,
            12601560286801748730,
        ),
    ];
    // Under a limit of 50: the walks are refused; ANY SHORTEST counts the
    // paths it keeps, never fewer than one per node, so its 592 are not.
    const LIMITED: &str = "SET ursa.max_paths = 50;
        SELECT count(*) FROM GRAPH_TABLE (g
          MATCH p = (a WHERE a.id IN (0, 1, 2))-[e]->{1,3}(b) COLUMNS (b.id))";
    const KEPT: &str = "SELECT count(*) FROM GRAPH_TABLE (g
          MATCH p = ANY SHORTEST (a WHERE a.id IN (0, 1, 2))-[e]->{1,3}(b) COLUMNS (b.id))";
    let all = |ctx: SessionContext| async move {
        let mut out = Vec::new();
        for (sql, _, _) in queries {
            let rows = ok(&ctx, sql).await;
            out.push((rows.len(), fnv(&rows)));
        }
        let limited = run(&ctx, LIMITED)
            .await
            .map_err(|e| ursa_engine::user_message(&e));
        assert_eq!(ok(&ctx, KEPT).await, ["592"]);
        ok(&ctx, "RESET ursa.max_paths").await;
        (out, limited, ctx)
    };
    let (built, built_limited, ctx) = all(open(&fixture.db()).await).await;
    assert_eq!(counts(&ctx), (1, 0));
    // The same again in the same session: the order is the search's.
    let (again, _, ctx) = all(ctx).await;
    assert_eq!(again, built);
    close(ctx);
    let (stored, stored_limited, ctx) = all(open(&fixture.db()).await).await;
    assert_eq!(counts(&ctx), (0, 1));
    close(ctx);
    assert_eq!(stored, built, "stored and built differ");
    assert_eq!(stored_limited, built_limited);
    let pinned: Vec<(usize, u64)> = queries.iter().map(|&(_, n, h)| (n, h)).collect();
    assert_eq!(built, pinned, "the pinned rows changed");
    let message = built_limited.unwrap_err();
    assert!(
        message.contains("the path search reached 50 paths"),
        "{message}"
    );
}

/// Write a vertex table (`id`) of `n` keys and an edge table (`src`, `dst`)
/// of `m` edges between them, as Parquet in `dir` (`nodes_<tag>.parquet`,
/// `edges_<tag>.parquet`), from a seeded generator. The keys are spread
/// over 2^40 (so integer keys are hashed, not held in a direct table) in a
/// scrambled order, a quarter of them are on no edge, and about one edge in
/// 97 has an endpoint that is no key, so it is dropped. With `strings`, each
/// key is `u` and its digits.
fn write_vertex_graph(dir: &Path, tag: &str, n: u64, m: usize, strings: bool) {
    use datafusion::arrow::array::{ArrayRef, StringArray};
    let key = |i: u64| ((i * 2_654_435_761) % (1 << 40)) as i64;
    let column = |values: Vec<i64>| -> ArrayRef {
        match strings {
            true => Arc::new(StringArray::from_iter_values(
                values.iter().map(|v| format!("u{v}")),
            )),
            false => Arc::new(Int64Array::from(values)),
        }
    };
    let id_type = if strings {
        DataType::Utf8
    } else {
        DataType::Int64
    };
    let write = |path: PathBuf, names: &[&str], columns: Vec<ArrayRef>| {
        let fields: Vec<Field> = names
            .iter()
            .map(|n| Field::new(*n, id_type.clone(), false))
            .collect();
        let schema = Arc::new(Schema::new(fields));
        let file = std::fs::File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, Arc::clone(&schema), None).unwrap();
        let batch = RecordBatch::try_new(schema, columns).unwrap();
        let rows = batch.num_rows();
        let mut at = 0;
        while at < rows {
            let len = (rows - at).min(1 << 16);
            writer.write(&batch.slice(at, len)).unwrap();
            at += len;
        }
        writer.close().unwrap();
    };
    // The keys in a scrambled order: `i * 7919 mod n` visits each once (n
    // is not a multiple of 7919).
    assert_ne!(n % 7919, 0);
    let nodes: Vec<i64> = (0..n).map(|i| key(i * 7919 % n)).collect();
    write(
        dir.join(format!("nodes_{tag}.parquet")),
        &["id"],
        vec![column(nodes)],
    );
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state >> 33
    };
    // Endpoints among the first three quarters of the keys, skewed toward
    // the low ones; the rest of the keys are isolated nodes.
    let reach = n * 3 / 4;
    let mut endpoint = || {
        let r = next() % reach;
        key(r * (next() % reach) / reach)
    };
    let (mut src, mut dst) = (Vec::with_capacity(m), Vec::with_capacity(m));
    for e in 0..m {
        src.push(endpoint());
        let d = endpoint();
        dst.push(if e % 97 == 5 { (1 << 41) + e as i64 } else { d });
    }
    write(
        dir.join(format!("edges_{tag}.parquet")),
        &["src", "dst"],
        vec![column(src), column(dst)],
    );
}

/// A hash of a stored index's contents (#532): its topology, its ids in
/// dense order, its id lookup table and the edges it dropped, FNV-1a over
/// their little-endian bytes. The same for the same index on every
/// platform and run.
fn index_hash(path: &Path, key: &ursa_engine::database::IndexKey) -> u64 {
    use datafusion::arrow::array::{Array, AsArray};
    use datafusion::arrow::datatypes::Int64Type;
    let stored = ursa_engine::database::index_file::read_file(path, key).unwrap();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    let topology = &stored.topology;
    eat(&(topology.n_nodes() as u64).to_le_bytes());
    eat(&(topology.n_edges() as u64).to_le_bytes());
    let out = topology.out();
    for &o in out.offsets.iter() {
        eat(&o.to_le_bytes());
    }
    for (&t, &e) in out.targets.iter().zip(out.edge_ids.iter()) {
        eat(&t.to_le_bytes());
        eat(&e.to_le_bytes());
    }
    let ids = stored.ids.user_id_array();
    match ids.data_type() {
        DataType::Int64 => {
            for &id in ids.as_primitive::<Int64Type>().values().iter() {
                eat(&id.to_le_bytes());
            }
        }
        _ => {
            let ids = ids.as_string::<i32>();
            for i in 0..ids.len() {
                eat(ids.value(i).as_bytes());
                eat(b"\n");
            }
        }
    }
    for &slot in stored.ids.lookup_table().unwrap().iter() {
        eat(&slot.to_le_bytes());
    }
    eat(&stored.dropped.n_dropped.to_le_bytes());
    if let Some(kept) = &stored.dropped.kept {
        for keep in kept.iter() {
            eat(&[keep as u8]);
        }
    }
    h
}

/// The index built for a graph with a vertex table is pinned (#532): the
/// `describe(g, full => true)` row and a hash of the stored index (its node
/// numbering, topology, id lookup table and dropped edges), for integer and
/// for string keys. The hashes were recorded before the vertex-table build
/// was made faster, so the build gives the same index it gave before.
#[tokio::test(flavor = "multi_thread")]
async fn a_vertex_table_build_gives_the_pinned_index() {
    let dir = tempfile::tempdir().unwrap();
    let (n, m) = (60_001, 250_000);
    write_vertex_graph(dir.path(), "int", n, m, false);
    write_vertex_graph(dir.path(), "str", n, m, true);
    let db = dir.path().join("pinned.ursa");
    let ctx = open(&db).await;
    for tag in ["int", "str"] {
        let nodes = dir.path().join(format!("nodes_{tag}.parquet"));
        let edges = dir.path().join(format!("edges_{tag}.parquet"));
        ok(
            &ctx,
            &format!(
                "CREATE VIEW n_{tag} AS FROM '{}';
                 CREATE VIEW e_{tag} AS FROM '{}';
                 CREATE PROPERTY GRAPH g_{tag} VERTEX TABLES (n_{tag})
                   EDGE TABLES (e_{tag} SOURCE KEY (src) REFERENCES n_{tag} (id)
                                        DESTINATION KEY (dst) REFERENCES n_{tag} (id));",
                nodes.display(),
                edges.display()
            ),
        )
        .await;
    }
    let mut rows = Vec::new();
    for tag in ["int", "str"] {
        let row = ok(
            &ctx,
            &format!(
                "SELECT n_nodes, n_edges, n_components, n_dropped_edges, round(density, 12)
                 FROM describe(g_{tag}, full => true)"
            ),
        )
        .await;
        rows.push(row.join(";"));
    }
    close(ctx);
    let index_dir = dir.path().join("pinned.ursa.d/indexes");
    let entries = read_manifest(&index_dir);
    let mut hashes = Vec::new();
    for tag in ["int", "str"] {
        let entry = entries
            .iter()
            .find(|e| e.graph == format!("g_{tag}"))
            .unwrap_or_else(|| panic!("no stored index for g_{tag}"));
        hashes.push(index_hash(&index_dir.join(&entry.file), &entry.key));
    }
    let row = "60001,247422,18810,2578,0.000068727188";
    assert_eq!(rows, [row, row], "the describe rows changed");
    assert_eq!(
        hashes,
        [236861135069851834, 8257638453979474348],
        "the stored indexes changed"
    );
}
