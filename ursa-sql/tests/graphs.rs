//! `CREATE PROPERTY GRAPH`, `DROP PROPERTY GRAPH` and the `<graph>.nodes` and
//! `<graph>.edges` tables, run on an engine session.

use std::path::{Path, PathBuf};

use datafusion::arrow::array::{Array, AsArray, RecordBatch};
use datafusion::arrow::datatypes::{DataType, Int64Type};
use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::prelude::SessionContext;
use ursa_engine::graph::registry_of;

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("python/ursa/datasets/data")
        .join(file)
}

/// Run every statement in `sql`; the batches of the last one.
async fn run(ctx: &SessionContext, sql: &str) -> datafusion::error::Result<Vec<RecordBatch>> {
    let mut batches = Vec::new();
    for statement in ursa_sql::parse_sql_for(ctx, sql)? {
        batches = ursa_sql::execute(ctx, statement).await?.collect().await?;
    }
    Ok(batches)
}

async fn ok(ctx: &SessionContext, sql: &str) -> Vec<RecordBatch> {
    run(ctx, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)))
}

/// The user-facing error `sql` fails with.
async fn err(ctx: &SessionContext, sql: &str) -> String {
    match run(ctx, sql).await {
        Ok(batches) => panic!("{sql} should fail; got {}", pretty(&batches)),
        Err(e) => ursa_engine::user_message(&e),
    }
}

fn pretty(batches: &[RecordBatch]) -> String {
    pretty_format_batches(batches).unwrap().to_string()
}

/// The single Int64 value a query returns.
async fn int(ctx: &SessionContext, sql: &str) -> i64 {
    let batches = ok(ctx, sql).await;
    let column = batches[0].column(0);
    assert_eq!(column.len(), 1, "{sql}");
    column.as_primitive::<Int64Type>().value(0)
}

/// A session with the Getting Started views: `members` and `ties`.
async fn karate_session() -> SessionContext {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        &format!(
            "CREATE VIEW members AS SELECT * FROM '{}';
             CREATE VIEW ties AS SELECT * FROM '{}';",
            data("karate_nodes.csv").display(),
            data("karate_edges.csv").display()
        ),
    )
    .await;
    ctx
}

const KARATE: &str = "CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (
    ties SOURCE KEY (src) REFERENCES members (id)
         DESTINATION KEY (dst) REFERENCES members (id)
  )";

const KARATE_EDGES_ONLY: &str =
    "CREATE PROPERTY GRAPH karate_edges_only EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))";

#[tokio::test]
async fn a_graph_with_a_vertex_table() {
    let ctx = karate_session().await;
    // DDL returns an empty result, as CREATE VIEW does.
    let batches = ok(&ctx, KARATE).await;
    assert!(batches
        .iter()
        .all(|b| b.num_rows() == 0 && b.num_columns() == 0));

    assert_eq!(int(&ctx, "SELECT count(*) FROM karate.nodes").await, 34);
    assert_eq!(
        int(&ctx, "SELECT count(DISTINCT id) FROM karate.nodes").await,
        34
    );
    assert_eq!(int(&ctx, "SELECT count(*) FROM karate.edges").await, 78);
    assert_eq!(
        pretty(&ok(&ctx, "SELECT * FROM karate.nodes ORDER BY id LIMIT 3").await),
        "\
+----+--------+
| id | club   |
+----+--------+
| 0  | Mr. Hi |
| 1  | Mr. Hi |
| 2  | Mr. Hi |
+----+--------+"
    );
    // The edges are the edge table, every column unchanged.
    assert_eq!(
        pretty(&ok(&ctx, "SELECT * FROM karate.edges ORDER BY src, dst LIMIT 2").await),
        "\
+-----+-----+
| src | dst |
+-----+-----+
| 0   | 1   |
| 0   | 2   |
+-----+-----+"
    );
    let def = registry_of(&ctx).get("karate").unwrap();
    assert_eq!(def.edges.table.to_string(), "ties");
    assert_eq!(def.vertices.as_ref().unwrap().key, "id");
}

#[tokio::test]
async fn a_graph_without_a_vertex_table() {
    let ctx = karate_session().await;
    ok(&ctx, KARATE_EDGES_ONLY).await;
    let batches = ok(&ctx, "SELECT * FROM karate_edges_only.nodes").await;
    assert_eq!(batches[0].schema().fields().len(), 1);
    assert_eq!(batches[0].schema().field(0).name(), "id");
    assert_eq!(
        int(&ctx, "SELECT count(*) FROM karate_edges_only.nodes").await,
        34
    );
    assert_eq!(
        int(
            &ctx,
            "SELECT count(DISTINCT id) FROM karate_edges_only.nodes"
        )
        .await,
        34
    );
    assert_eq!(
        int(&ctx, "SELECT count(*) FROM karate_edges_only.edges").await,
        78
    );
}

#[tokio::test]
async fn an_endpoint_missing_from_the_vertex_table_is_a_node_without_attributes() {
    let ctx = karate_session().await;
    ok(
        &ctx,
        "CREATE VIEW more_ties AS SELECT src, dst FROM ties UNION ALL SELECT 0 AS src, 99 AS dst;
         CREATE PROPERTY GRAPH g VERTEX TABLES (members KEY (id))
           EDGE TABLES (more_ties SOURCE KEY (src) DESTINATION KEY (dst))",
    )
    .await;
    assert_eq!(int(&ctx, "SELECT count(*) FROM g.nodes").await, 35);
    assert_eq!(
        pretty(&ok(&ctx, "SELECT * FROM g.nodes WHERE id >= 33 ORDER BY id").await),
        "\
+----+---------+
| id | club    |
+----+---------+
| 33 | Officer |
| 99 |         |
+----+---------+"
    );
    assert_eq!(
        int(&ctx, "SELECT count(*) FROM g.nodes WHERE club IS NULL").await,
        1
    );
}

#[tokio::test]
async fn string_ids() {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        &format!(
            "CREATE VIEW marriages AS SELECT * FROM '{}';
             CREATE PROPERTY GRAPH florentine
               EDGE TABLES (marriages SOURCE KEY (src) DESTINATION KEY (dst))",
            data("florentine_edges.csv").display()
        ),
    )
    .await;
    let expected = int(
        &ctx,
        "SELECT count(*) FROM (SELECT src FROM marriages UNION SELECT dst FROM marriages)",
    )
    .await;
    assert_eq!(
        int(&ctx, "SELECT count(*) FROM florentine.nodes").await,
        expected
    );
    assert_eq!(
        int(
            &ctx,
            "SELECT count(*) FROM florentine.nodes WHERE id = 'Medici'"
        )
        .await,
        1
    );

    // With a vertex table keyed by strings, one family missing from it.
    ok(
        &ctx,
        "CREATE VIEW families AS
           SELECT DISTINCT src AS name, length(src) AS letters FROM marriages WHERE src <> 'Medici';
         CREATE PROPERTY GRAPH florentine_families
           VERTEX TABLES (families KEY (name))
           EDGE TABLES (marriages SOURCE KEY (src) REFERENCES families (name)
                                  DESTINATION KEY (dst) REFERENCES families (name))",
    )
    .await;
    assert_eq!(
        int(&ctx, "SELECT count(*) FROM florentine_families.nodes").await,
        expected
    );
    assert_eq!(
        pretty(
            &ok(
                &ctx,
                "SELECT * FROM florentine_families.nodes WHERE id IN ('Medici', 'Strozzi') ORDER BY id"
            )
            .await
        ),
        "\
+---------+---------+
| id      | letters |
+---------+---------+
| Medici  |         |
| Strozzi | 7       |
+---------+---------+"
    );
}

#[tokio::test]
async fn integer_ids_of_different_widths() {
    let ctx = karate_session().await;
    ok(
        &ctx,
        "CREATE VIEW members32 AS SELECT CAST(id AS INT) AS member, club FROM members;
         CREATE PROPERTY GRAPH g VERTEX TABLES (members32 KEY (member))
           EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
    )
    .await;
    let batches = ok(&ctx, "SELECT * FROM g.nodes").await;
    assert_eq!(batches[0].schema().field(0).name(), "id");
    assert_eq!(batches[0].schema().field(0).data_type(), &DataType::Int64);
    assert_eq!(int(&ctx, "SELECT count(*) FROM g.nodes").await, 34);
}

#[tokio::test]
async fn graphs_and_algorithms_join_on_id() {
    let ctx = karate_session().await;
    ok(&ctx, KARATE).await;
    // What `JOIN pagerank(karate) USING (id)` will do, with a stand-in.
    assert_eq!(
        int(
            &ctx,
            "SELECT count(*) FROM karate.nodes
             JOIN (SELECT src AS id, count(*) AS degree FROM karate.edges GROUP BY src) AS d USING (id)",
        )
        .await,
        int(&ctx, "SELECT count(DISTINCT src) FROM ties").await
    );
}

#[tokio::test]
async fn or_replace_and_if_not_exists() {
    let ctx = karate_session().await;
    ok(&ctx, KARATE).await;
    assert_eq!(err(&ctx, KARATE).await, "graph 'karate' already exists");

    // IF NOT EXISTS keeps the existing graph.
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH IF NOT EXISTS karate EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
    )
    .await;
    assert!(registry_of(&ctx).get("karate").unwrap().vertices.is_some());

    // OR REPLACE replaces it.
    ok(
        &ctx,
        "CREATE OR REPLACE PROPERTY GRAPH karate EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
    )
    .await;
    assert!(registry_of(&ctx).get("karate").unwrap().vertices.is_none());
    let batches = ok(&ctx, "SELECT * FROM karate.nodes").await;
    assert_eq!(batches[0].schema().fields().len(), 1);

    // OR REPLACE also creates.
    ok(
        &ctx,
        "CREATE OR REPLACE PROPERTY GRAPH other EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
    )
    .await;
    assert_eq!(int(&ctx, "SELECT count(*) FROM other.edges").await, 78);

    assert_eq!(
        err(
            &ctx,
            "CREATE OR REPLACE PROPERTY GRAPH IF NOT EXISTS karate EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
        )
        .await,
        "'IF NOT EXISTS' cannot coexist with 'REPLACE'"
    );

    // A failed replacement keeps the graph as it was.
    let failed = err(
        &ctx,
        "CREATE OR REPLACE PROPERTY GRAPH karate EDGE TABLES (ties SOURCE KEY (nope) DESTINATION KEY (dst))",
    )
    .await;
    assert!(failed.contains("'nope' not found"), "{failed}");
    assert_eq!(int(&ctx, "SELECT count(*) FROM karate.nodes").await, 34);
}

#[tokio::test]
async fn drop_property_graph() {
    let ctx = karate_session().await;
    ok(&ctx, KARATE).await;
    ok(&ctx, "DROP PROPERTY GRAPH karate").await;
    assert!(registry_of(&ctx).get("karate").is_none());
    let message = err(&ctx, "SELECT * FROM karate.nodes").await;
    assert!(message.contains("karate"), "{message}");
    assert_eq!(
        int(
            &ctx,
            "SELECT count(*) FROM information_schema.schemata WHERE schema_name = 'karate'"
        )
        .await,
        0
    );
    assert_eq!(
        err(&ctx, "DROP PROPERTY GRAPH karate").await,
        "graph 'karate' does not exist"
    );
    ok(&ctx, "DROP PROPERTY GRAPH IF EXISTS karate").await;

    // The name is free again, and a near miss is suggested.
    ok(&ctx, KARATE).await;
    assert_eq!(
        err(&ctx, "DROP PROPERTY GRAPH karat").await,
        "graph 'karat' does not exist; did you mean 'karate'?"
    );
    // The underlying views are untouched.
    assert_eq!(int(&ctx, "SELECT count(*) FROM ties").await, 78);
}

#[tokio::test]
async fn show_tables_lists_the_graph_tables() {
    let ctx = karate_session().await;
    ok(&ctx, KARATE).await;
    let tables = pretty(
        &ok(
            &ctx,
            "SELECT table_schema, table_name FROM information_schema.tables
             WHERE table_schema = 'karate' ORDER BY table_name",
        )
        .await,
    );
    assert_eq!(
        tables,
        "\
+--------------+------------+
| table_schema | table_name |
+--------------+------------+
| karate       | edges      |
| karate       | nodes      |
+--------------+------------+"
    );
    let shown = pretty(&ok(&ctx, "SHOW TABLES").await);
    assert!(shown.contains("| karate             | nodes "), "{shown}");
    let columns = pretty(&ok(&ctx, "SHOW COLUMNS FROM karate.nodes").await);
    assert!(columns.contains("club"), "{columns}");
}

#[tokio::test]
async fn identifiers_are_normalized_as_table_names_are() {
    let ctx = karate_session().await;
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH Karate VERTEX TABLES (Members KEY (ID))
           EDGE TABLES (TIES SOURCE KEY (Src) REFERENCES MEMBERS (Id) DESTINATION KEY (DST))",
    )
    .await;
    assert_eq!(registry_of(&ctx).names(), vec!["karate".to_string()]);
    assert_eq!(int(&ctx, "SELECT count(*) FROM KARATE.Nodes").await, 34);

    // Quoted names keep their case.
    ok(
        &ctx,
        r#"CREATE VIEW "Ties" AS SELECT src AS "Src", dst AS "Dst" FROM ties;
           CREATE PROPERTY GRAPH "Quoted" EDGE TABLES ("Ties" SOURCE KEY ("Src") DESTINATION KEY ("Dst"))"#,
    )
    .await;
    assert!(registry_of(&ctx).get("Quoted").is_some());
    assert_eq!(
        int(&ctx, r#"SELECT count(*) FROM "Quoted".edges"#).await,
        78
    );
    let message = err(
        &ctx,
        r#"CREATE PROPERTY GRAPH g EDGE TABLES ("Ties" SOURCE KEY (Src) DESTINATION KEY ("Dst"))"#,
    )
    .await;
    assert!(
        message.contains("SOURCE KEY column 'src' not found in table 'Ties'"),
        "{message}"
    );
}

#[tokio::test]
async fn a_graph_over_a_file_reads_its_current_contents() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("edges.csv");
    std::fs::write(&file, "src,dst\n1,2\n2,3\n").unwrap();
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        &format!(
            "CREATE VIEW e AS SELECT * FROM '{}';
             CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (src) DESTINATION KEY (dst))",
            file.display()
        ),
    )
    .await;
    assert_eq!(int(&ctx, "SELECT count(*) FROM g.nodes").await, 3);
    std::fs::write(&file, "src,dst\n1,2\n2,3\n3,4\n").unwrap();
    assert_eq!(int(&ctx, "SELECT count(*) FROM g.edges").await, 3);
    assert_eq!(int(&ctx, "SELECT count(*) FROM g.nodes").await, 4);
}

#[tokio::test]
async fn a_graph_over_a_dropped_view_says_so() {
    let ctx = karate_session().await;
    ok(&ctx, KARATE_EDGES_ONLY).await;
    ok(&ctx, "DROP VIEW ties").await;
    let message = err(&ctx, "SELECT * FROM karate_edges_only.edges").await;
    assert!(
        message.contains("cannot read karate_edges_only.edges: table 'ties' not found"),
        "{message}"
    );
}

#[tokio::test]
async fn definitions_are_checked_when_created() {
    let ctx = karate_session().await;
    ok(
        &ctx,
        "CREATE VIEW floats AS SELECT CAST(src AS DOUBLE) AS src, CAST(dst AS DOUBLE) AS dst FROM ties;
         CREATE VIEW mixed AS SELECT src, CAST(dst AS VARCHAR) AS dst FROM ties;
         CREATE VIEW named AS SELECT CAST(id AS VARCHAR) AS name, club FROM members;
         CREATE VIEW with_id AS SELECT id AS member, id, club FROM members;
         CREATE SCHEMA taken;",
    )
    .await;
    for (sql, expected) in [
        (
            "CREATE PROPERTY GRAPH g EDGE TABLES (tiez SOURCE KEY (src) DESTINATION KEY (dst))",
            "table 'tiez' not found; did you mean 'ties'?",
        ),
        (
            "CREATE PROPERTY GRAPH g EDGE TABLES (nothing_like_it SOURCE KEY (src) DESTINATION KEY (dst))",
            "table 'nothing_like_it' not found",
        ),
        (
            "CREATE PROPERTY GRAPH g VERTEX TABLES (member KEY (id))
               EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
            "table 'member' not found; did you mean 'members'?",
        ),
        (
            "CREATE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (source) DESTINATION KEY (dst))",
            "SOURCE KEY column 'source' not found in table 'ties'; its columns are src, dst",
        ),
        (
            "CREATE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dsts))",
            "DESTINATION KEY column 'dsts' not found in table 'ties'; did you mean 'dst'?",
        ),
        (
            "CREATE PROPERTY GRAPH g VERTEX TABLES (members KEY (member))
               EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
            "KEY column 'member' not found in table 'members'; its columns are id, club",
        ),
        (
            "CREATE PROPERTY GRAPH g EDGE TABLES (mixed SOURCE KEY (src) DESTINATION KEY (dst))",
            "the source and destination columns of edge table 'mixed' must have the same type, \
             but src is Int64 and dst is Utf8View",
        ),
        (
            "CREATE PROPERTY GRAPH g EDGE TABLES (floats SOURCE KEY (src) DESTINATION KEY (dst))",
            "the endpoints of edge table 'floats' (src and dst) are Float64; node ids must be \
             integers or strings",
        ),
        (
            "CREATE PROPERTY GRAPH g VERTEX TABLES (named KEY (name))
               EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
            "the key of vertex table 'named' (name) is Utf8View, but the edge endpoints are Int64; \
             both must be integers or both strings",
        ),
        (
            "CREATE PROPERTY GRAPH g VERTEX TABLES (members KEY (club))
               EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
            "the key of vertex table 'members' (club) is Utf8, but the edge endpoints are Int64; \
             both must be integers or both strings",
        ),
        (
            "CREATE PROPERTY GRAPH g VERTEX TABLES (with_id KEY (member))
               EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
            "vertex table 'with_id' has a column named 'id' that is not its key (member); \
             g.nodes names the key 'id', so rename that column (in a view) first",
        ),
        (
            "CREATE PROPERTY GRAPH public EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
            "cannot name a graph 'public': a schema of that name already exists, and a graph's \
             name is the name of the schema holding its nodes and edges tables",
        ),
        (
            "CREATE PROPERTY GRAPH taken EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
            "cannot name a graph 'taken': a schema of that name already exists, and a graph's \
             name is the name of the schema holding its nodes and edges tables",
        ),
        (
            "CREATE PROPERTY GRAPH information_schema EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
            "cannot name a graph 'information_schema': a schema of that name already exists, and \
             a graph's name is the name of the schema holding its nodes and edges tables",
        ),
        (
            "CREATE PROPERTY GRAPH db.g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))",
            "a graph name is a single identifier, not 'db.g': the graph's name is also the name \
             of the schema holding its nodes and edges tables",
        ),
        (
            "CREATE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                                                  DESTINATION KEY (dst))",
            "SOURCE KEY (src) REFERENCES members, but the graph has no VERTEX TABLES; add \
             VERTEX TABLES (members KEY (id)) or drop the REFERENCES clause",
        ),
        (
            "CREATE PROPERTY GRAPH g VERTEX TABLES (members KEY (id))
               EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                                 DESTINATION KEY (dst) REFERENCES ties (id))",
            "DESTINATION KEY (dst) REFERENCES ties (id), but the vertex table is members KEY \
             (id); REFERENCES must name the vertex table and its key",
        ),
        (
            "CREATE PROPERTY GRAPH g VERTEX TABLES (members KEY (id))
               EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (club)
                                 DESTINATION KEY (dst))",
            "SOURCE KEY (src) REFERENCES members (club), but the vertex table is members KEY \
             (id); REFERENCES must name the vertex table and its key",
        ),
    ] {
        assert_eq!(err(&ctx, sql).await, expected, "{sql}");
    }
    // None of them defined a graph or left a schema behind.
    assert!(registry_of(&ctx).all().is_empty());
    assert_eq!(
        int(
            &ctx,
            "SELECT count(*) FROM information_schema.schemata WHERE schema_name = 'g'"
        )
        .await,
        0
    );
    // A qualified reference to the vertex table matches it.
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH g VERTEX TABLES (members KEY (id))
           EDGE TABLES (ties SOURCE KEY (src) REFERENCES public.members (id)
                             DESTINATION KEY (dst) REFERENCES datafusion.public.members (id))",
    )
    .await;
}

#[tokio::test]
async fn a_graph_over_another_graphs_tables() {
    let ctx = karate_session().await;
    ok(&ctx, KARATE).await;
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH again VERTEX TABLES (karate.nodes KEY (id))
           EDGE TABLES (karate.edges SOURCE KEY (src) REFERENCES karate.nodes (id)
                                     DESTINATION KEY (dst) REFERENCES karate.nodes (id))",
    )
    .await;
    assert_eq!(int(&ctx, "SELECT count(*) FROM again.nodes").await, 34);
    assert_eq!(
        int(&ctx, "SELECT count(*) FROM again.nodes WHERE club IS NULL").await,
        0
    );
}
