//! What a result's columns are: the `ursa.*` field metadata (#431).
//!
//! The graph functions and `GRAPH_TABLE` mark their output fields, and
//! DataFusion carries the marks with the columns. These tests read the
//! schema a client gets (the executed stream's, which every batch and every
//! exported file has), and hold DataFusion 55 to what it does, operator by
//! operator: the record of the spike the issue asks for is
//! [`the_operators_that_keep_it_and_the_ones_that_do_not`].

use std::path::{Path, PathBuf};

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::{Schema, SchemaRef};
use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::prelude::SessionContext;

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("python/ursa/datasets/data")
        .join(file)
}

/// Run every statement in `sql`; the schema of the last one's stream (what a
/// client and an exported file get), and its batches.
async fn run(ctx: &SessionContext, sql: &str) -> (SchemaRef, Vec<RecordBatch>) {
    let mut last = None;
    for statement in ursa_sql::parse_sql_for(ctx, sql).unwrap() {
        let frame = ursa_sql::execute(ctx, statement)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)));
        let planned = frame.schema().as_arrow().clone();
        let stream = frame
            .execute_stream()
            .await
            .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)));
        let schema = stream.schema();
        let batches: Vec<RecordBatch> = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)));
        // The plan, the stream and every batch agree.
        assert_eq!(describe(&planned), describe(&schema), "{sql}");
        for batch in &batches {
            assert_eq!(describe(&batch.schema()), describe(&schema), "{sql}");
        }
        last = Some((schema, batches));
    }
    last.expect("a statement")
}

/// A schema's `ursa.*` metadata, one line per column: `name: role graph
/// label var`, with `-` for a key that is not set; then the schema's own.
fn describe(schema: &Schema) -> String {
    let mut lines: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| {
            let get = |key: &str| f.metadata().get(key).map_or("-", String::as_str);
            let line = format!(
                "{}: {} {} {} {}",
                f.name(),
                get("ursa.role"),
                get("ursa.graph"),
                get("ursa.label"),
                get("ursa.var")
            );
            line.trim_end_matches(" -").to_string()
        })
        .collect();
    let mut own: Vec<_> = schema.metadata().iter().collect();
    own.sort();
    lines.extend(own.into_iter().map(|(k, v)| format!("{k} = {v}")));
    lines.join("\n")
}

async fn roles(ctx: &SessionContext, sql: &str) -> String {
    describe(&run(ctx, sql).await.0)
}

/// The Getting Started session: `karate` (with its members) and
/// `karate_edges_only` (endpoints only).
async fn karate() -> SessionContext {
    let ctx = ursa_engine::new_session();
    run(
        &ctx,
        &format!(
            "CREATE VIEW members AS SELECT * FROM '{}';
             CREATE VIEW ties AS SELECT * FROM '{}';
             CREATE PROPERTY GRAPH karate
               VERTEX TABLES (members KEY (id))
               EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                                 DESTINATION KEY (dst) REFERENCES members (id));
             CREATE PROPERTY GRAPH karate_edges_only
               EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
            data("karate_nodes.csv").display(),
            data("karate_edges.csv").display()
        ),
    )
    .await;
    ctx
}

/// Each query's columns are what `expected` says (see [`describe`]).
async fn check(ctx: &SessionContext, cases: &[(&str, &str)]) {
    for (sql, expected) in cases {
        assert_eq!(roles(ctx, sql).await, *expected, "\n{sql}");
    }
}

#[tokio::test]
async fn traversals_say_what_their_columns_are() {
    let ctx = karate().await;
    check(
        &ctx,
        &[
            (
                "SELECT * FROM hop(karate, seeds => [0])",
                "seed: seed karate members\nid: node karate members\ndistance: distance karate",
            ),
            // No vertex table, no label.
            (
                "SELECT * FROM hop(karate_edges_only, seeds => [0], steps => 2)",
                "seed: seed karate_edges_only\nid: node karate_edges_only\n\
                 distance: distance karate_edges_only",
            ),
            (
                "SELECT * FROM shortest_path(karate, source => 0, target => 33)",
                "src: src karate members\ndst: dst karate members\nstep: step karate\ncost:",
            ),
            (
                "SELECT * FROM random_walk(karate, seeds => [0], steps => 2)",
                "walk_id: walk karate\nstep: step karate\nid: node karate members",
            ),
            // No rows, the same schema.
            (
                "SELECT * FROM hop(karate, seeds => (SELECT 99))",
                "seed: seed karate members\nid: node karate members\ndistance: distance karate",
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn algorithms_say_what_their_columns_are() {
    let ctx = karate().await;
    check(
        &ctx,
        &[
            (
                "SELECT * FROM pagerank(karate)",
                "id: node karate members\npagerank: metric karate",
            ),
            (
                "SELECT * FROM louvain(karate_edges_only)",
                "id: node karate_edges_only\nlouvain: category karate_edges_only",
            ),
            // A column is what its call is, whatever it is named; a layout's
            // positions have no role.
            (
                "SELECT * FROM algorithms(karate, degree(), c => louvain(), \
                 score => betweenness(), layout_fr())",
                "id: node karate members\ndegree: metric karate\nc: category karate\n\
                 score: metric karate\nx:\ny:",
            ),
            (
                "SELECT * FROM algorithms(karate, connected_components(), label_propagation(), \
                 triangle_count(), clustering_coefficient(), closeness())",
                "id: node karate members\nconnected_components: category karate\n\
                 label_propagation: category karate\ntriangle_count: metric karate\n\
                 clustering_coefficient: metric karate\ncloseness: metric karate",
            ),
            // A neighbour aggregation is a metric when it is a number.
            (
                "SELECT * FROM neighbors_agg(karate, attribute => id, agg => 'mean')",
                "id: node karate members\nneighbors_agg: metric karate",
            ),
            (
                "SELECT * FROM neighbors_agg(karate, attribute => club, agg => 'min')",
                "id: node karate members\nneighbors_agg:",
            ),
            // A statistic's row is about no node.
            ("SELECT * FROM density(karate)", "density:"),
            (
                "SELECT * FROM describe(karate)",
                "n_nodes:\nn_edges:\ndensity:\navg_degree:\nn_components:\nn_dropped_edges:",
            ),
        ],
    )
    .await;
}

/// The spike (#431): which of DataFusion 55's operators keep a field's
/// metadata. A column keeps it wherever the column itself goes; a value
/// computed from it has none.
#[tokio::test]
async fn the_operators_that_keep_it_and_the_ones_that_do_not() {
    let ctx = karate().await;
    let node = "id: node karate members";
    let both = "id: node karate members\npagerank: metric karate";
    check(
        &ctx,
        &[
            // Kept: a projection of the column, an alias, a qualified name.
            ("SELECT id FROM pagerank(karate)", node),
            (
                "SELECT id AS member, pagerank AS score FROM pagerank(karate)",
                "member: node karate members\nscore: metric karate",
            ),
            ("SELECT p.id FROM pagerank(karate) AS p", node),
            // Kept: a filter, a sort, a limit, DISTINCT.
            (
                "SELECT * FROM pagerank(karate) WHERE pagerank > 0.05 ORDER BY pagerank DESC \
                 LIMIT 3 OFFSET 1",
                both,
            ),
            (
                "SELECT DISTINCT louvain FROM louvain(karate)",
                "louvain: category karate",
            ),
            // Kept: a subquery, a CTE, a view.
            (
                "SELECT * FROM (SELECT id, pagerank FROM pagerank(karate)) AS s",
                both,
            ),
            (
                "WITH s AS (SELECT * FROM pagerank(karate)) SELECT id, pagerank FROM s",
                both,
            ),
            (
                "CREATE VIEW ranks AS SELECT * FROM pagerank(karate); \
                 SELECT * FROM ranks WHERE id < 3",
                both,
            ),
            // Kept: a join's columns, each side's own, inner and outer.
            (
                "SELECT m.club, p.id, p.pagerank FROM members m JOIN pagerank(karate) p \
                 ON m.id = p.id",
                "club:\nid: node karate members\npagerank: metric karate",
            ),
            (
                "SELECT p.id, p.pagerank FROM members m LEFT JOIN pagerank(karate) p \
                 ON m.id = p.id",
                both,
            ),
            (
                "SELECT d.id, degree, louvain FROM degree(karate) d JOIN louvain(karate) l \
                 ON d.id = l.id",
                "id: node karate members\ndegree: metric karate\nlouvain: category karate",
            ),
            // Kept: UNION [ALL], INTERSECT and EXCEPT of the column.
            (
                "SELECT id FROM pagerank(karate) UNION ALL SELECT id FROM degree(karate)",
                node,
            ),
            (
                "SELECT id FROM pagerank(karate) UNION SELECT id FROM degree(karate)",
                node,
            ),
            (
                "SELECT id FROM pagerank(karate) INTERSECT SELECT id FROM members",
                node,
            ),
            ("SELECT id FROM pagerank(karate) EXCEPT SELECT 0", node),
            // Kept: a GROUP BY key, and a function that returns one of the
            // column's own values (first_value, lag).
            (
                "SELECT louvain, count(*) AS n, first_value(id) AS first FROM louvain(karate) \
                 GROUP BY louvain",
                "louvain: category karate\nn:\nfirst: node karate members",
            ),
            (
                "SELECT id, lag(id) OVER (ORDER BY id) AS previous, \
                 row_number() OVER (ORDER BY id) AS n FROM pagerank(karate)",
                "id: node karate members\nprevious: node karate members\nn:",
            ),
            // Kept: CAST and unary minus, which DataFusion reads as the same
            // field, converted. `CAST(pagerank AS REAL)` is how a column is
            // narrowed, and it stays a metric.
            (
                "SELECT CAST(pagerank AS REAL) AS narrow, -pagerank AS negated \
                 FROM pagerank(karate)",
                "narrow: metric karate\nnegated: metric karate",
            ),
            // Lost: an aggregate, a scalar subquery, a semi-join's other side.
            (
                "SELECT max(pagerank) AS top, count(id) AS n FROM pagerank(karate)",
                "top:\nn:",
            ),
            (
                "SELECT (SELECT max(pagerank) FROM pagerank(karate)) AS top",
                "top:",
            ),
            (
                "SELECT id FROM members WHERE id IN (SELECT id FROM hop(karate, seeds => [0]))",
                "id:",
            ),
        ],
    )
    .await;
}

/// An expression over a marked column is a new value: it has no role.
#[tokio::test]
async fn a_computed_column_has_no_role() {
    let ctx = karate().await;
    check(
        &ctx,
        &[
            (
                "SELECT round(pagerank, 4) AS pagerank, id + 1 AS id, abs(pagerank) AS a, \
                 pagerank * 2 AS double, CASE WHEN id > 3 THEN id END AS some, \
                 id::varchar || '!' AS text FROM pagerank(karate)",
                "pagerank:\nid:\na:\ndouble:\nsome:\ntext:",
            ),
            (
                "SELECT * FROM GRAPH_TABLE (karate MATCH (a)-[e]->(b) \
                 COLUMNS (a.id + 1 AS next, upper(a.club) AS club, a.id = b.id AS same))",
                "next:\nclub:\nsame:\n\
                 ursa.pattern = [{\"directed\":true,\"edge\":\"e\",\"from\":\"a\",\
                 \"label\":\"ties\",\"to\":\"b\"}]",
            ),
        ],
    )
    .await;
}

/// `FROM members JOIN pagerank(karate) USING (id)` returns `members`' `id`,
/// which the join made equal to the node key: it is marked as one. An outer
/// join's rows need not match, so its plain column stays plain.
#[tokio::test]
async fn an_inner_join_on_a_node_key_makes_the_other_column_one() {
    let ctx = karate().await;
    let joined = "id: node karate members\nclub:\npagerank: metric karate";
    check(
        &ctx,
        &[
            (
                "SELECT * FROM members JOIN pagerank(karate) USING (id) \
                 ORDER BY pagerank DESC LIMIT 3",
                joined,
            ),
            (
                "SELECT m.id, club, pagerank FROM pagerank(karate) p JOIN members m \
                 ON p.id = m.id ORDER BY pagerank DESC LIMIT 3",
                joined,
            ),
            // In a view and in a subquery too.
            (
                "CREATE VIEW ranked AS SELECT * FROM members JOIN pagerank(karate) USING (id); \
                 SELECT id, club, pagerank FROM ranked WHERE club = 'Officer'",
                joined,
            ),
            (
                "SELECT * FROM (SELECT id, club, degree FROM members NATURAL JOIN degree(karate)) \
                 WHERE degree > 10",
                "id: node karate members\nclub:\ndegree: metric karate",
            ),
            (
                "SELECT m.id, p.pagerank FROM members m LEFT JOIN pagerank(karate) p \
                 ON m.id = p.id",
                "id:\npagerank: metric karate",
            ),
            // Equal to a seed says less: nothing is carried.
            (
                "SELECT m.id FROM members m JOIN hop(karate, seeds => [0]) h ON m.id = h.seed",
                "id:",
            ),
        ],
    )
    .await;
    // The rows and the names are the join's.
    let (_, batches) = run(
        &ctx,
        "SELECT * FROM members JOIN pagerank(karate) USING (id) ORDER BY pagerank DESC LIMIT 2",
    )
    .await;
    assert_eq!(
        pretty_format_batches(&batches).unwrap().to_string(),
        "+----+---------+---------------------+\n\
         | id | club    | pagerank            |\n\
         +----+---------+---------------------+\n\
         | 33 | Officer | 0.25904714771295256 |\n\
         | 32 | Officer | 0.09548932925521017 |\n\
         +----+---------+---------------------+"
    );
}

const A_TO_B: &str = "ursa.pattern = [{\"directed\":true,\"edge\":\"e\",\"from\":\"a\",\
                      \"label\":\"ties\",\"to\":\"b\"}]";

#[tokio::test]
async fn graph_table_says_what_its_columns_are() {
    let ctx = karate().await;
    check(
        &ctx,
        &[
            // A node's key, a node's property, an edge's properties, `v.*`.
            (
                "SELECT * FROM GRAPH_TABLE (karate MATCH (a)-[e]->(b) \
                 COLUMNS (a.id AS a, b.id, b.club, e.src AS s, e.*, a.*))",
                &format!(
                    "a: node karate members a\nid: node karate members b\n\
                     club: property karate members b\ns: property karate ties e\n\
                     src: property karate ties e\ndst: property karate ties e\n\
                     id:1: node karate members a\nclub:1: property karate members a\n{A_TO_B}"
                ),
            ),
            // A graph without vertex tables: its nodes have no label.
            (
                "SELECT * FROM GRAPH_TABLE (karate_edges_only MATCH (a)-[e]->(b) \
                 COLUMNS (a.id AS a, b.id AS b))",
                &format!("a: node karate_edges_only - a\nb: node karate_edges_only - b\n{A_TO_B}"),
            ),
            // The pattern's edges: an undirected one, one pointing left
            // (from `c` to `b`), an edge and a node without a name.
            (
                "SELECT * FROM GRAPH_TABLE (karate MATCH (a)-[e]-(b)<-[f]-(c)-[]->() \
                 COLUMNS (a.id, b.id, c.id))",
                "id: node karate members a\nid_1: node karate members b\n\
                 id_2: node karate members c\n\
                 ursa.pattern = [\
                 {\"directed\":false,\"edge\":\"e\",\"from\":\"a\",\"label\":\"ties\",\"to\":\"b\"},\
                 {\"directed\":true,\"edge\":\"f\",\"from\":\"c\",\"label\":\"ties\",\"to\":\"b\"},\
                 {\"directed\":true,\"edge\":null,\"from\":\"c\",\"label\":\"ties\",\"to\":null}]",
            ),
            // A path: its nodes, edges and elements; its length is a number.
            (
                "SELECT * FROM GRAPH_TABLE (karate \
                 MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26) \
                 COLUMNS (path_length(p) AS steps, vertices(p) AS members, edges(p) AS ties, \
                 element_id(p), a.id, b.id AS b))",
                "steps:\nmembers: path_nodes karate members p\nties: path_edges karate ties p\n\
                 element_id(p): path_elements karate - p\nid: node karate members a\n\
                 b: node karate members b\n\
                 ursa.pattern = [{\"directed\":false,\"edge\":\"e\",\"from\":\"a\",\
                 \"label\":\"ties\",\"max\":null,\"min\":0,\"path\":\"p\",\"to\":\"b\"}]",
            ),
            // A quantified edge, through the operators above it, and beside
            // a graph function.
            (
                "SELECT g.a, p.pagerank, count(*) AS n FROM GRAPH_TABLE (karate \
                 MATCH (a)-[e]->{1,2}(b) COLUMNS (a.id AS a, b.id AS b)) g \
                 JOIN pagerank(karate) p ON p.id = g.a GROUP BY g.a, p.pagerank \
                 ORDER BY n DESC LIMIT 3",
                "a: node karate members a\npagerank: metric karate\nn:\n\
                 ursa.pattern = [{\"directed\":true,\"edge\":\"e\",\"from\":\"a\",\
                 \"label\":\"ties\",\"max\":2,\"min\":1,\"to\":\"b\"}]",
            ),
        ],
    )
    .await;
}

/// The ```` ```sql ```` blocks of a docs page, in order.
fn sql_blocks(page: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(page);
    let text = std::fs::read_to_string(&path).unwrap();
    let mut blocks = Vec::new();
    let mut block: Option<String> = None;
    for line in text.lines() {
        match &mut block {
            None if line.trim_start().starts_with("```sql") => block = Some(String::new()),
            None => {}
            Some(sql) if line.trim() == "```" => {
                blocks.push(std::mem::take(sql));
                block = None;
            }
            Some(sql) => {
                sql.push_str(line);
                sql.push('\n');
            }
        }
    }
    blocks
}

/// Every `GRAPH_TABLE` example of the pattern matching page, as the page
/// runs them (`ursa-cli`'s docs test checks their output).
#[tokio::test]
async fn the_pattern_matching_page_examples() {
    let ctx = ursa_engine::new_session();
    let undirected = |edge: &str| {
        format!(
            "{{\"directed\":false,\"edge\":{edge},\"from\":\"a\",\"label\":\"ties\",\"to\":\"b\"}}"
        )
    };
    let search = "ursa.pattern = [{\"directed\":false,\"edge\":\"e\",\"from\":\"a\",\
                  \"label\":\"ties\",\"max\":null,\"min\":0,\"path\":\"p\",\"to\":\"b\"}]";
    let expected = [
        format!(
            "officer: node karate members a\nmember: node karate members b\n\
             ursa.pattern = [{}]",
            undirected("\"e\"")
        ),
        format!(
            "friend_of_friend: node karate members c\nursa.pattern = [{},\
             {{\"directed\":false,\"edge\":null,\"from\":\"b\",\"label\":\"ties\",\"to\":\"c\"}}]",
            undirected("null")
        ),
        // `round(pagerank, 4)` is a new value.
        format!(
            "id: node karate members b\npagerank:\nursa.pattern = [{}]",
            undirected("null")
        ),
        format!(
            "steps:\nmembers: path_nodes karate members p\nties: path_edges karate ties p\n{search}"
        ),
        format!("steps:\nmembers:\n{search}"),
        format!("members: path_nodes karate members p\n{search}"),
        "paths:\nursa.pattern = [{\"directed\":true,\"edge\":\"e\",\"from\":\"a\",\
         \"label\":\"ties\",\"max\":null,\"min\":1,\"to\":\"b\"}]"
            .to_string(),
        format!("steps:\n{search}"),
    ];
    let mut expected = expected.iter();
    for block in sql_blocks("docs/sql/pattern-matching.md") {
        let sql = ["karate_nodes.csv", "karate_edges.csv"]
            .iter()
            .fold(block, |sql, file| {
                sql.replace(&format!("'{file}'"), &format!("'{}'", data(file).display()))
            });
        let got = roles(&ctx, &sql).await;
        if sql.contains("GRAPH_TABLE") {
            let want = expected.next().unwrap_or_else(|| {
                panic!("a new GRAPH_TABLE example; say what it returns:\n{sql}")
            });
            assert_eq!(&got, want, "\n{sql}");
        }
    }
    assert_eq!(expected.next(), None, "an example was removed");
}

/// `COPY … TO` a Parquet or an Arrow file keeps the metadata (the design's
/// decision 2): the file remembers its graph.
#[tokio::test]
async fn exported_files_keep_it() {
    use datafusion::arrow::ipc::reader::FileReader;
    use datafusion::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let ctx = karate().await;
    let dir = tempfile::tempdir().unwrap();
    let query = "SELECT a, b, pagerank FROM GRAPH_TABLE (karate MATCH (a)-[e]->(b) \
                 COLUMNS (a.id AS a, b.id AS b)) JOIN pagerank(karate) p ON p.id = a";
    let expected = format!(
        "a: node karate members a\nb: node karate members b\npagerank: metric karate\n{A_TO_B}"
    );
    assert_eq!(roles(&ctx, query).await, expected);
    let file = |name: &str| dir.path().join(name);
    for name in ["out.parquet", "out.arrow"] {
        run(
            &ctx,
            &format!("COPY ({query}) TO '{}'", file(name).display()),
        )
        .await;
    }
    // As another reader finds them in the files.
    let parquet = std::fs::File::open(file("out.parquet")).unwrap();
    let parquet = ParquetRecordBatchReaderBuilder::try_new(parquet).unwrap();
    assert_eq!(describe(parquet.schema()), expected);
    assert_eq!(parquet.metadata().file_metadata().num_rows(), 78);
    let arrow = FileReader::try_new(std::fs::File::open(file("out.arrow")).unwrap(), None).unwrap();
    assert_eq!(describe(&arrow.schema()), expected);

    // Read back by Ursa, both keep it: an Arrow file always, a Parquet file
    // because Ursa's sessions turn DataFusion's `skip_metadata` off. Turned
    // back on, the Parquet file's roles are dropped.
    let read = |name: &str| format!("SELECT * FROM '{}'", file(name).display());
    assert_eq!(roles(&ctx, &read("out.arrow")).await, expected);
    assert_eq!(roles(&ctx, &read("out.parquet")).await, expected);
    run(
        &ctx,
        "SET datafusion.execution.parquet.skip_metadata = true",
    )
    .await;
    assert_eq!(roles(&ctx, &read("out.parquet")).await, "a:\nb:\npagerank:");
}
