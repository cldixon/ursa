//! A vertex table defines a graph's node set (#311): every algorithm,
//! statistic, traversal and pattern sees the nodes `<graph>.nodes` lists,
//! nodes on no edge included, and an edge whose source or destination is not
//! one of them is dropped (and counted by `describe`), as in DuckPGQ.

use std::path::Path;
use std::time::Duration;

use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::prelude::SessionContext;
use ursa_engine::database::OpenOptions;
use ursa_engine::graph::{index_builds, index_loads};
use ursa_sql::{execute_with_text, open_session, parse_sql_with_text_for};

/// Run every statement of `sql`; the last one's rows as CSV lines, without a
/// header.
async fn run(ctx: &SessionContext, sql: &str) -> datafusion::error::Result<Vec<String>> {
    let mut lines = Vec::new();
    for (statement, text) in parse_sql_with_text_for(ctx, sql)? {
        let batches = execute_with_text(ctx, statement, &text)
            .await?
            .collect()
            .await?;
        lines.clear();
        let options = FormatOptions::default().with_null("NULL");
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

/// People 1 to 5; 4 and 5 on no edge. Follows 1->2, 2->3, 3->1, and two
/// edges to or from 9, who is no person.
const PEOPLE: &str = "
CREATE VIEW people AS SELECT * FROM (VALUES
  (1, 'ann'), (2, 'bob'), (3, 'cy'), (4, 'dee'), (5, 'eve')) t(id, name);
CREATE VIEW follows AS SELECT * FROM (VALUES
  (1, 2, 1.0), (9, 1, 5.0), (2, 3, 2.0), (3, 1, 3.0), (3, 9, 4.0)) t(src, dst, w);
CREATE PROPERTY GRAPH g
  VERTEX TABLES (people KEY (id))
  EDGE TABLES (follows SOURCE KEY (src) REFERENCES people (id)
                       DESTINATION KEY (dst) REFERENCES people (id));";

async fn people() -> SessionContext {
    let ctx = ursa_engine::new_session();
    ok(&ctx, PEOPLE).await;
    ctx
}

#[tokio::test]
async fn every_algorithm_sees_the_vertex_tables_nodes() {
    let ctx = people().await;
    let nodes = ["1", "2", "3", "4", "5"];
    assert_eq!(ok(&ctx, "SELECT id FROM g.nodes ORDER BY id").await, nodes);
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM degree(g, direction => 'both') ORDER BY id"
        )
        .await,
        ["1,2", "2,2", "3,2", "4,0", "5,0"]
    );
    for f in [
        "pagerank(g)",
        "connected_components(g)",
        "triangle_count(g)",
        "closeness(g)",
        "betweenness(g)",
        "label_propagation(g)",
        "louvain(g, seed => 1)",
        "clustering_coefficient(g)",
        "algorithms(g, degree(), pagerank())",
        "neighbors_agg(g, attribute => length(name), agg => 'sum')",
    ] {
        assert_eq!(
            ok(&ctx, &format!("SELECT id FROM {f} ORDER BY id")).await,
            nodes,
            "{f}"
        );
    }
    // Nodes 4 and 5 are each a component of their own.
    assert_eq!(
        ok(
            &ctx,
            "SELECT count(DISTINCT connected_components) FROM connected_components(g)"
        )
        .await,
        ["3"]
    );
    // Their neighbours' values: none.
    assert_eq!(
        ok(
            &ctx,
            "SELECT id, neighbors_agg FROM neighbors_agg(g, attribute => id, agg => 'sum', \
             direction => 'both') WHERE id >= 4 ORDER BY id"
        )
        .await,
        ["4,0.0", "5,0.0"]
    );
}

#[tokio::test]
async fn edges_to_or_from_no_vertex_are_dropped_and_counted() {
    let ctx = people().await;
    // The edge table is the edge table: every row.
    assert_eq!(ok(&ctx, "SELECT count(*) FROM g.edges").await, ["5"]);
    // 9 is no node: its two edges are dropped, and describe says so.
    assert_eq!(
        ok(&ctx, "SELECT * FROM describe(g, full => true)").await,
        ["5,3,0.15,0.6,3,2"]
    );
    assert_eq!(
        ok(
            &ctx,
            "SELECT n_nodes, n_edges, n_components, n_dropped_edges \
             FROM describe(g, full => true, nodes => 'touched')"
        )
        .await,
        ["3,3,1,2"]
    );
    // Weights line up with the edges kept: 1->2 (1.0), 2->3 (2.0).
    assert_eq!(
        ok(
            &ctx,
            "SELECT src, dst, cost FROM shortest_path(g, source => 1, target => 3, weight => w)"
        )
        .await,
        ["1,2,1.0", "2,3,3.0"]
    );
    // A path's edges are rows of the edge table, from 0, dropped rows
    // counted: 1->2 is row 0, 2->3 row 2, 3->1 row 3.
    assert_eq!(
        ok(
            &ctx,
            "SELECT a, b, edges FROM GRAPH_TABLE (g MATCH p = (x)-[e]->{2}(y) \
             COLUMNS (x.id AS a, y.id AS b, edges(p) AS edges)) ORDER BY a"
        )
        .await,
        ["1,3,[0, 2]", "2,1,[2, 3]", "3,2,[3, 0]"]
    );
    // Without a vertex table nothing is dropped, and 9 is a node.
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH bare EDGE TABLES (follows SOURCE KEY (src) DESTINATION KEY (dst))",
    )
    .await;
    assert_eq!(
        ok(
            &ctx,
            "SELECT n_nodes, n_edges, n_dropped_edges FROM describe(bare)"
        )
        .await,
        ["4,5,0"]
    );
}

#[tokio::test]
async fn a_subgraph_keeps_every_vertex_unless_touched() {
    let ctx = people().await;
    ok(
        &ctx,
        "CREATE VIEW first AS SELECT * FROM follows WHERE src = 1;
         CREATE PROPERTY GRAPH sub
           VERTEX TABLES (people KEY (id))
           EDGE TABLES (first SOURCE KEY (src) REFERENCES people (id)
                              DESTINATION KEY (dst) REFERENCES people (id));",
    )
    .await;
    assert_eq!(
        ok(&ctx, "SELECT n_nodes, n_edges FROM describe(sub)").await,
        ["5,1"]
    );
    assert_eq!(
        ok(
            &ctx,
            "SELECT n_nodes, n_edges FROM describe(sub, nodes => 'touched')"
        )
        .await,
        ["2,1"]
    );
    assert_eq!(ok(&ctx, "SELECT count(*) FROM degree(sub)").await, ["5"]);
}

#[tokio::test]
async fn a_node_on_no_edge_reaches_nothing() {
    let ctx = people().await;
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM hop(g, seeds => [4], direction => 'both')"
        )
        .await,
        Vec::<String>::new()
    );
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM shortest_path(g, source => 4, target => 1)"
        )
        .await,
        Vec::<String>::new()
    );
    // A walk from it stays put: its start is its only step.
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM random_walk(g, start => [5], steps => 3, seed => 1)"
        )
        .await,
        ["0,0,5"]
    );
    // 9 is no node: a seed of 9 reaches nothing either.
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM hop(g, seeds => [9], direction => 'both')"
        )
        .await,
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn graph_table_matches_the_same_nodes() {
    let ctx = people().await;
    assert_eq!(
        ok(
            &ctx,
            "SELECT id FROM GRAPH_TABLE (g MATCH (a) COLUMNS (a.id AS id)) ORDER BY id"
        )
        .await,
        ok(&ctx, "SELECT id FROM g.nodes ORDER BY id").await
    );
    // Fixed-length edges join the vertex table: no edge to or from 9.
    assert_eq!(
        ok(
            &ctx,
            "SELECT a, b FROM GRAPH_TABLE (g MATCH (x)-[e]->(y) COLUMNS (x.id AS a, y.id AS b)) \
             ORDER BY a"
        )
        .await,
        ["1,2", "2,3", "3,1"]
    );
    // A quantified edge searches the same graph: a path of no edges from
    // every node, 4 and 5 included, and none through 9.
    assert_eq!(
        ok(
            &ctx,
            "SELECT a, b FROM GRAPH_TABLE (g MATCH (x)-[e]->{0,1}(y) \
             COLUMNS (x.id AS a, y.id AS b)) ORDER BY a, b"
        )
        .await,
        ["1,1", "1,2", "2,2", "2,3", "3,1", "3,3", "4,4", "5,5"]
    );
}

#[tokio::test]
async fn a_multi_table_graph_runs_on_the_named_vertex_table() {
    let ctx = people().await;
    ok(
        &ctx,
        "CREATE VIEW teams AS SELECT * FROM (VALUES (10), (11)) t(id);
         CREATE VIEW plays AS SELECT * FROM (VALUES (1, 10), (2, 10), (2, 12)) t(person, team);
         CREATE PROPERTY GRAPH club
           VERTEX TABLES (people KEY (id), teams KEY (id))
           EDGE TABLES (
             follows SOURCE KEY (src) REFERENCES people (id)
                     DESTINATION KEY (dst) REFERENCES people (id) LABEL knows,
             plays SOURCE KEY (person) REFERENCES people (id)
                   DESTINATION KEY (team) REFERENCES teams (id));",
    )
    .await;
    assert_eq!(
        ok(
            &ctx,
            "SELECT id, degree FROM degree(club, people, knows, direction => 'both') ORDER BY id"
        )
        .await,
        ["1,2", "2,2", "3,2", "4,0", "5,0"]
    );
    assert_eq!(
        ok(
            &ctx,
            "SELECT n_nodes, n_edges, n_dropped_edges FROM describe(club, people, knows)"
        )
        .await,
        ["5,3,2"]
    );
    // A path from a person over `plays` reaches a team: the search's nodes
    // are both tables' keys; the row to team 12, which is no team, is dropped.
    assert_eq!(
        ok(
            &ctx,
            "SELECT a, b, edges FROM GRAPH_TABLE (club MATCH p = (x:people)-[e:plays]->(y:teams) \
             COLUMNS (x.id AS a, y.id AS b, edges(p) AS edges)) ORDER BY a, b"
        )
        .await,
        ["1,10,[0]", "2,10,[1]"]
    );
    // And over `knows`, the people: 4 and 5 included.
    assert_eq!(
        ok(
            &ctx,
            "SELECT count(*) FROM GRAPH_TABLE (club MATCH (x:people)-[e:knows]->{0,0}(y:people) \
             COLUMNS (x.id AS a))"
        )
        .await,
        ["5"]
    );
}

/// A stored index is keyed by the vertex table too: rewriting the vertex
/// file builds the index afresh, in this process and the next.
#[tokio::test]
async fn a_changed_vertex_file_rebuilds_the_stored_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("club.ursa");
    let people = dir.path().join("people.csv");
    let edges = dir.path().join("edges.csv");
    std::fs::write(&edges, "src,dst\n1,2\n2,3\n3,1\n3,9\n").unwrap();
    std::fs::write(&people, "id\n1\n2\n3\n").unwrap();
    let options = OpenOptions {
        lock_timeout: Duration::from_millis(500),
        ..OpenOptions::default()
    };
    let define = format!(
        "CREATE VIEW people AS SELECT * FROM '{}';
         CREATE VIEW ties AS SELECT * FROM '{}';
         CREATE PROPERTY GRAPH g VERTEX TABLES (people KEY (id))
           EDGE TABLES (ties SOURCE KEY (src) REFERENCES people (id)
                             DESTINATION KEY (dst) REFERENCES people (id));",
        people.display(),
        edges.display()
    );
    let query = "SELECT n_nodes, n_edges, n_dropped_edges FROM describe(g)";
    let open = |path: &Path| {
        let path = path.to_path_buf();
        let options = options.clone();
        async move { open_session(&path, &options).await.unwrap() }
    };

    let ctx = open(&path).await;
    ok(&ctx, &define).await;
    assert_eq!(ok(&ctx, query).await, ["3,3,1"]);
    assert_eq!((index_builds(&ctx), index_loads(&ctx)), (1, 0));
    drop(ctx);

    // A later process loads it, n_dropped_edges and all.
    let ctx = open(&path).await;
    assert_eq!(ok(&ctx, query).await, ["3,3,1"]);
    assert_eq!((index_builds(&ctx), index_loads(&ctx)), (0, 1));

    // The vertex file changes: 9 becomes a person, and 4 one on no edge.
    std::fs::write(&people, "id\n1\n2\n3\n9\n4\n").unwrap();
    assert_eq!(ok(&ctx, query).await, ["5,4,0"]);
    assert_eq!((index_builds(&ctx), index_loads(&ctx)), (1, 1));
    drop(ctx);

    let ctx = open(&path).await;
    assert_eq!(ok(&ctx, query).await, ["5,4,0"]);
    assert_eq!(
        ok(&ctx, "SELECT id, degree FROM degree(g) ORDER BY id").await,
        ["1,1", "2,1", "3,2", "4,0", "9,0"]
    );
    assert_eq!((index_builds(&ctx), index_loads(&ctx)), (0, 1));
}
