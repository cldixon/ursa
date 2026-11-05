//! The M2 surface (#236): every catalog function, `weight =>`, `algorithms()`,
//! statistics, subgraphs and `ursa_functions()`. Values against the Python
//! library are in `parity.rs`; this file checks shapes, sharing, the index
//! cache and the messages.

use std::path::{Path, PathBuf};

use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::common::TableReference;
use datafusion::error::Result;
use datafusion::prelude::SessionContext;
use ursa_engine::graph::{index_builds, registry_of, EdgeTable, GraphDef};
use ursa_engine::{new_session, user_message};

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../python/ursa/datasets/data")
        .join(file)
}

/// Define `graph` over the table or view `table`, with endpoints `src` and
/// `dst`.
fn put_graph(ctx: &SessionContext, graph: &str, table: &str) {
    registry_of(ctx).put(GraphDef {
        name: graph.into(),
        edges: EdgeTable {
            table: TableReference::bare(table),
            source: "src".into(),
            destination: "dst".into(),
        },
        vertices: None,
    });
}

/// Define `graph` over a view `view` of the CSV at `path`.
async fn define(ctx: &SessionContext, graph: &str, view: &str, path: &Path) {
    ctx.sql(&format!(
        "CREATE OR REPLACE VIEW {view} AS SELECT * FROM '{}'",
        path.display()
    ))
    .await
    .unwrap();
    put_graph(ctx, graph, view);
}

async fn session() -> SessionContext {
    let ctx = new_session();
    define(&ctx, "karate", "ties", &data("karate_edges.csv")).await;
    define(&ctx, "lesmis", "scenes", &data("lesmis_edges.csv")).await;
    ctx
}

async fn table(ctx: &SessionContext, sql: &str) -> Result<String> {
    let batches = ctx.sql(sql).await?.collect().await?;
    Ok(pretty_format_batches(&batches)?.to_string())
}

async fn csv(ctx: &SessionContext, sql: &str) -> String {
    let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
    let mut out = Vec::new();
    {
        let mut writer = datafusion::arrow::csv::Writer::new(&mut out);
        for b in &batches {
            writer.write(b).unwrap();
        }
    }
    String::from_utf8(out).unwrap()
}

async fn error(ctx: &SessionContext, sql: &str) -> String {
    match ctx.sql(sql).await {
        Ok(frame) => match frame.collect().await {
            Ok(_) => panic!("{sql} should have failed"),
            Err(e) => user_message(&e),
        },
        Err(e) => user_message(&e),
    }
}

/// Getting Started, "Combine algorithms and attributes", as written: one
/// `algorithms()` call joined to the node table with `USING (id)`.
#[tokio::test]
async fn combine_algorithms_and_attributes_with_algorithms() {
    let ctx = session().await;
    ctx.sql(&format!(
        "CREATE VIEW members AS SELECT * FROM '{}'",
        data("karate_nodes.csv").display()
    ))
    .await
    .unwrap();
    let out = table(
        &ctx,
        "SELECT id, club, degree, triangles, community \
         FROM members \
         JOIN algorithms(karate, \
                         degree(direction => 'both'), \
                         triangles => triangle_count(), \
                         community => louvain(seed => 1)) USING (id) \
         ORDER BY degree DESC \
         LIMIT 5",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "\
+----+---------+--------+-----------+-----------+
| id | club    | degree | triangles | community |
+----+---------+--------+-----------+-----------+
| 33 | Officer | 17     | 15        | 2         |
| 0  | Mr. Hi  | 16     | 18        | 0         |
| 32 | Officer | 12     | 13        | 2         |
| 2  | Mr. Hi  | 10     | 11        | 0         |
| 1  | Mr. Hi  | 9      | 12        | 0         |
+----+---------+--------+-----------+-----------+"
    );
    assert_eq!(index_builds(&ctx), 1);
}

/// `algorithms()` gives exactly what the single functions give.
#[tokio::test]
async fn algorithms_equals_the_single_functions() {
    let ctx = session().await;
    let combined = csv(
        &ctx,
        "SELECT * FROM algorithms(lesmis, \
             pagerank(direction => 'both', weight => weight), \
             cc => clustering_coefficient(), \
             triangle_count(), \
             pos => layout_fr(seed => 3), \
             betweenness(weight => weight), \
             louvain(seed => 2)) \
         ORDER BY id",
    )
    .await;
    let separate = csv(
        &ctx,
        "SELECT p.id, pagerank, clustering_coefficient AS cc, triangle_count, \
                l.x AS pos_x, l.y AS pos_y, betweenness, louvain \
         FROM pagerank(lesmis, direction => 'both', weight => weight) AS p \
         JOIN clustering_coefficient(lesmis) AS c ON c.id = p.id \
         JOIN triangle_count(lesmis) AS t ON t.id = p.id \
         JOIN layout_fr(lesmis, seed => 3) AS l ON l.id = p.id \
         JOIN betweenness(lesmis, weight => weight) AS b ON b.id = p.id \
         JOIN louvain(lesmis, seed => 2) AS v ON v.id = p.id \
         ORDER BY p.id",
    )
    .await;
    assert!(combined.starts_with("id,pagerank,cc,triangle_count,pos_x,pos_y,betweenness,louvain\n"));
    assert_eq!(combined, separate);
    assert_eq!(combined.lines().count(), 78);
    // Weighted and unweighted calls, in one query or several, share the index.
    assert_eq!(index_builds(&ctx), 1);
}

#[tokio::test]
async fn algorithms_names_its_columns() {
    let ctx = session().await;
    let schema = table(
        &ctx,
        "DESCRIBE SELECT * FROM algorithms(karate, degree(), in_degree => degree(direction => \
         'in'), layout_circle(), pos => layout_fr(), closeness())",
    )
    .await
    .unwrap();
    let names: Vec<&str> = schema
        .lines()
        .skip(3)
        .filter_map(|l| l.split('|').nth(1).map(str::trim))
        .filter(|n| !n.is_empty())
        .collect();
    assert_eq!(
        names,
        [
            "id",
            "degree",
            "in_degree",
            "x",
            "y",
            "pos_x",
            "pos_y",
            "closeness"
        ]
    );
    assert!(
        schema.contains("| x           | Float32   | NO          |"),
        "{schema}"
    );
    // The columns are qualified by the function's name, as a table's are.
    table(
        &ctx,
        "SELECT algorithms.id, algorithms.degree FROM algorithms(karate, degree())",
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn algorithms_errors() {
    let ctx = session().await;
    for (sql, expected) in [
        (
            "SELECT * FROM algorithms(karate, degree(), degree(direction => 'in'))",
            "algorithms(): two columns are named 'degree'; give each call its own name, as in \
             in_degree => degree(direction => 'in'), out_degree => degree()",
        ),
        (
            "SELECT * FROM algorithms(karate, layout_fr(), layout_circle())",
            "algorithms(): two columns are named 'x'; give each call its own name, as in \
             in_degree => degree(direction => 'in'), out_degree => degree()",
        ),
        (
            "SELECT * FROM algorithms(karate, id => degree())",
            "algorithms(): no column can be named 'id', which holds the node ids; give degree() \
             another name with name => degree(...)",
        ),
        (
            "SELECT * FROM algorithms(karate, density())",
            "algorithms(): density() is a statistic, with one row for the whole graph, not one \
             per node; call it on its own: SELECT * FROM density(<graph>)",
        ),
        (
            "SELECT * FROM algorithms(karate, pagerenk())",
            "algorithms(): there is no graph function 'pagerenk'; did you mean 'pagerank'?",
        ),
        (
            "SELECT * FROM algorithms(karate, upper('x'))",
            "algorithms(): there is no graph function 'upper'. The graph functions are: degree, \
             pagerank, connected_components, triangle_count, clustering_coefficient, \
             betweenness, closeness, label_propagation, louvain, layout_fr, layout_random, \
             layout_circle",
        ),
        (
            "SELECT * FROM algorithms(karate, pagerank(0.9))",
            "algorithms(): pagerank() takes its parameters by name, as name => value; got 0.9. \
             Write pagerank(damping => 0.85, max_iter => 30, tol => 1e-6, weight => NULL, \
             direction => 'out')",
        ),
        (
            "SELECT * FROM algorithms(karate, degree(karate))",
            "algorithms(): degree() takes its parameters by name, as name => value; got karate. \
             The graph is algorithms()'s first argument only: write degree(direction => 'out')",
        ),
        (
            "SELECT * FROM algorithms(karate, pagerank(dampng => 0.9))",
            "algorithms(): pagerank() has no parameter 'dampng'; did you mean 'damping'? Its \
             parameters are: damping, max_iter, tol, weight, direction",
        ),
        (
            "SELECT * FROM algorithms(karate, pagerank(direction => 'in'))",
            "algorithms(): pagerank(direction => 'in') is not supported; use one of 'out', \
             'both'",
        ),
        (
            "SELECT * FROM algorithms(karate, degree)",
            "algorithms(): each argument after the graph is a call to a per-node graph \
             function, as degree() or name => louvain(seed => 1); got degree (write degree())",
        ),
        (
            "SELECT * FROM algorithms(karate, damping => 0.9)",
            "algorithms(): each argument after the graph is a call to a per-node graph \
             function, as degree() or name => louvain(seed => 1); got damping => 0.9",
        ),
        (
            "SELECT * FROM algorithms(karate)",
            "algorithms() needs at least one call after the graph: algorithms(<graph>, \
             degree(direction => 'both'), community => louvain(seed => 1), ...)",
        ),
        (
            "SELECT * FROM algorithms(degree())",
            "algorithms() needs a graph as its first argument: algorithms(<graph>, \
             degree(direction => 'both'), community => louvain(seed => 1), ...)",
        ),
        (
            "SELECT * FROM algorithm(karate, degree())",
            "there is no graph function 'algorithm'; did you mean 'algorithms'?",
        ),
    ] {
        assert_eq!(error(&ctx, sql).await, expected, "{sql}");
    }
}

/// Statistics: one row, no `id`, the Python API's parameters.
#[tokio::test]
async fn statistics_return_one_row() {
    let ctx = session().await;
    assert_eq!(
        table(
            &ctx,
            "SELECT * FROM describe(karate, full => true, direction => 'both')"
        )
        .await
        .unwrap(),
        "\
+---------+---------+---------------------+-------------------+--------------+
| n_nodes | n_edges | density             | avg_degree        | n_components |
+---------+---------+---------------------+-------------------+--------------+
| 34      | 78      | 0.13903743315508021 | 4.588235294117647 | 1            |
+---------+---------+---------------------+-------------------+--------------+"
    );
    // Without `full`, n_components is NULL, as in Python.
    assert_eq!(
        csv(
            &ctx,
            "SELECT n_components IS NULL AS unset FROM describe(karate)"
        )
        .await,
        "unset\ntrue\n"
    );
    assert_eq!(
        csv(
            &ctx,
            "SELECT * FROM diameter(karate, approximate => false, direction => 'both')"
        )
        .await,
        "diameter\n5\n"
    );
    assert_eq!(
        csv(
            &ctx,
            "SELECT round(avg_path_length, 4) AS apl FROM avg_path_length(karate, direction \
             => 'both')"
        )
        .await,
        "apl\n2.4082\n"
    );
    assert_eq!(
        csv(
            &ctx,
            "SELECT d.density = s.density AS same FROM density(karate) AS d, \
             describe(karate) AS s"
        )
        .await,
        "same\ntrue\n"
    );
    let schema = table(&ctx, "DESCRIBE SELECT * FROM describe(karate)")
        .await
        .unwrap();
    assert!(
        schema.contains("| n_components | Int64     | YES         |"),
        "{schema}"
    );
    assert!(
        schema.contains("| n_nodes      | Int64     | NO          |"),
        "{schema}"
    );
    for (sql, expected) in [
        (
            "SELECT * FROM diameter(karate, approximate => 1)",
            "diameter(approximate =>) must be a boolean; got 1",
        ),
        (
            "SELECT * FROM describe(karate, nodes => 'some')",
            "describe(nodes => 'some') is not supported; use one of 'all', 'touched'",
        ),
        (
            "SELECT * FROM density(karate, weight => weight)",
            "density() has no parameter 'weight'. Its parameters are: direction",
        ),
    ] {
        assert_eq!(error(&ctx, sql).await, expected, "{sql}");
    }
}

/// Getting Started, "Weights".
#[tokio::test]
async fn weights_match_getting_started() {
    let ctx = new_session();
    ctx.sql(&format!(
        "CREATE VIEW lesmis_edges AS SELECT * FROM '{}'",
        data("lesmis_edges.csv").display()
    ))
    .await
    .unwrap();
    put_graph(&ctx, "lesmis", "lesmis_edges");
    assert_eq!(
        table(
            &ctx,
            "SELECT * FROM pagerank(lesmis, weight => weight, direction => 'both') \
             ORDER BY pagerank DESC LIMIT 5"
        )
        .await
        .unwrap(),
        "\
+----------+----------------------+
| id       | pagerank             |
+----------+----------------------+
| Valjean  | 0.09956068175644767  |
| Marius   | 0.051667656904091475 |
| Myriel   | 0.03923416880292588  |
| Cosette  | 0.03691018363299517  |
| Enjolras | 0.036614491459848165 |
+----------+----------------------+"
    );
    // The Python library's louvain(weight=log10(1 + weight), seed=7), over a
    // frame with that column precomputed.
    assert_eq!(
        table(
            &ctx,
            "SELECT * FROM louvain(lesmis, weight => log(1 + weight), seed => 7) \
             ORDER BY id LIMIT 5"
        )
        .await
        .unwrap(),
        "\
+------------+---------+
| id         | louvain |
+------------+---------+
| Anzelma    | 3       |
| Babet      | 3       |
| Bahorel    | 5       |
| Bamatabois | 1       |
| BaronessT  | 4       |
+------------+---------+"
    );
    // Two weightings and none: one index.
    table(&ctx, "SELECT * FROM degree(lesmis)").await.unwrap();
    assert_eq!(index_builds(&ctx), 1);
}

#[tokio::test]
async fn weights_change_results_and_are_cached_on_the_index() {
    let ctx = session().await;
    let unweighted = csv(&ctx, "SELECT * FROM closeness(lesmis) ORDER BY id").await;
    let weighted = csv(
        &ctx,
        "SELECT * FROM closeness(lesmis, weight => weight) ORDER BY id",
    )
    .await;
    assert_ne!(unweighted, weighted);
    // NULL is unweighted; a constant weight is a scaled hop count.
    assert_eq!(
        csv(
            &ctx,
            "SELECT * FROM closeness(lesmis, weight => NULL) ORDER BY id"
        )
        .await,
        unweighted
    );
    let again = csv(
        &ctx,
        "SELECT * FROM closeness(lesmis, weight => weight) ORDER BY id",
    )
    .await;
    assert_eq!(again, weighted);
    assert_eq!(index_builds(&ctx), 1);
    // A cached weighted plan reads only the endpoints for the index; the
    // weights come from a second child that reads the weight column.
    let plan = table(
        &ctx,
        "EXPLAIN SELECT * FROM pagerank(lesmis, weight => weight * 2)",
    )
    .await
    .unwrap();
    assert!(
        plan.contains(
            "outputs=[pagerank(damping => 0.85, max_iter => 30, tol => 1e-6, \
             weight => weight * 2, direction => 'out')]"
        ),
        "{plan}"
    );
    assert!(plan.contains("projection=[src, dst]"), "{plan}");
    assert!(plan.contains("projection=[src, dst, weight]"), "{plan}");
    assert!(!plan.contains("RoundRobinBatch"), "{plan}");
}

#[tokio::test]
async fn weight_errors() {
    let ctx = session().await;
    for (sql, expected) in [
        (
            "SELECT * FROM pagerank(lesmis, weight => weight - 5)",
            "weight expression produced a negative value (-4); weights must be non-negative",
        ),
        (
            "SELECT * FROM pagerank(lesmis, weight => CASE WHEN weight > 3 THEN weight END)",
            "weight expression produced a null value; weights must be non-null",
        ),
        (
            "SELECT * FROM pagerank(lesmis, weight => src)",
            "pagerank(weight => src): weight expression must be numeric; it produced a column of \
             type Utf8 (a string or other non-numeric column cannot be a weight)",
        ),
        (
            "SELECT * FROM pagerank(lesmis, weight => wieght)",
            "pagerank(weight => wieght): No field named wieght. Did you mean 'scenes.weight'? \
             Valid fields are scenes.src, scenes.dst, scenes.weight.",
        ),
        (
            "SELECT * FROM pagerank(lesmis, weight => weight, weight => weight)",
            "pagerank() was given 'weight' more than once",
        ),
        (
            "SELECT * FROM triangle_count(lesmis, weight => weight)",
            "triangle_count() takes no parameters besides the graph; got 'weight'",
        ),
        (
            "SELECT * FROM degree(lesmis, weight => weight)",
            "degree() has no parameter 'weight'. Its parameters are: direction",
        ),
        (
            "SELECT * FROM pagerank(karate, weight => weight)",
            "pagerank(weight => weight): No field named weight. Valid fields are ties.src, \
             ties.dst.",
        ),
        (
            "SELECT * FROM pagerank(lesmis, dtype => 'f32')",
            "pagerank() has no parameter 'dtype' in SQL; cast its column instead, for example \
             CAST(pagerank AS REAL)",
        ),
    ] {
        assert_eq!(error(&ctx, sql).await, expected, "{sql}");
    }
}

/// Getting Started, "Subgraphs": a graph over a filtered view is its own
/// graph, whose nodes are the endpoints of the kept edges. The Python library
/// gives 3 for a graph over the same rows.
#[tokio::test]
async fn subgraphs_match_getting_started() {
    let ctx = new_session();
    ctx.sql(&format!(
        "CREATE VIEW lesmis_edges AS SELECT * FROM '{}'",
        data("lesmis_edges.csv").display()
    ))
    .await
    .unwrap();
    put_graph(&ctx, "lesmis", "lesmis_edges");
    ctx.sql("CREATE VIEW strong_ties AS SELECT * FROM lesmis_edges WHERE weight >= 3")
        .await
        .unwrap();
    put_graph(&ctx, "lesmis_strong", "strong_ties");
    assert_eq!(
        table(
            &ctx,
            "SELECT count(DISTINCT connected_components) AS groups \
             FROM connected_components(lesmis_strong)"
        )
        .await
        .unwrap(),
        "\
+--------+
| groups |
+--------+
| 3      |
+--------+"
    );
    assert_eq!(
        csv(&ctx, "SELECT n_nodes, n_edges FROM describe(lesmis_strong)").await,
        "n_nodes,n_edges\n44,107\n"
    );
    // Weights over a subgraph read the subgraph's rows.
    let weighted = csv(
        &ctx,
        "SELECT count(*) AS n, sum(CASE WHEN pagerank > 0 THEN 1 END) AS positive \
         FROM pagerank(lesmis_strong, weight => weight)",
    )
    .await;
    assert_eq!(weighted, "n,positive\n44,44\n");
    // The parent and the subgraph each have their index; the subgraph's is
    // built once for its weighted and unweighted queries.
    table(&ctx, "SELECT * FROM degree(lesmis)").await.unwrap();
    assert_eq!(index_builds(&ctx), 2);
    let plan = table(
        &ctx,
        "EXPLAIN SELECT * FROM connected_components(lesmis_strong)",
    )
    .await
    .unwrap();
    assert!(!plan.contains("RoundRobinBatch"), "{plan}");
}

/// Getting Started, "Find out what is available": one query lists the whole
/// surface.
#[tokio::test]
async fn ursa_functions_lists_every_function() {
    let ctx = new_session();
    let names = csv(&ctx, "SELECT name, family FROM ursa_functions()").await;
    assert_eq!(
        names,
        "\
name,family
degree,algorithm
pagerank,algorithm
connected_components,algorithm
triangle_count,algorithm
clustering_coefficient,algorithm
betweenness,algorithm
closeness,algorithm
label_propagation,algorithm
louvain,algorithm
layout_fr,layout
layout_random,layout
layout_circle,layout
density,statistic
avg_path_length,statistic
diameter,statistic
describe,statistic
algorithms,combinator
"
    );
    assert_eq!(
        csv(
            &ctx,
            "SELECT name, summary, networkx, array_to_string(directions, ' ') AS directions \
             FROM ursa_functions() WHERE name = 'pagerank'"
        )
        .await,
        "\
name,summary,networkx,directions
pagerank,PageRank (pull-based fixpoint); a dangling node spreads its score to all nodes.,\
\"nx.pagerank(G, alpha=damping) (both: nx.pagerank(U))\",out both
"
    );
    // The list columns are lists and structs: unnest them in SQL.
    assert_eq!(
        csv(
            &ctx,
            "SELECT p['name'] AS name, p['type'] AS type, p['default'] AS default \
             FROM (SELECT unnest(parameters) AS p FROM ursa_functions() WHERE name = 'pagerank')"
        )
        .await,
        "\
name,type,default
damping,number,0.85
max_iter,integer,30
tol,number,1e-6
weight,expression,NULL
direction,string,'out'
"
    );
    assert_eq!(
        csv(
            &ctx,
            "SELECT name FROM ursa_functions() WHERE array_has(directions, 'in') ORDER BY name"
        )
        .await,
        "name\navg_path_length\ncloseness\ndegree\ndensity\ndescribe\ndiameter\n"
    );
    assert_eq!(
        csv(
            &ctx,
            "SELECT o['name'] AS name, o['type'] AS type, o['nullable'] AS nullable \
             FROM (SELECT unnest(outputs) AS o FROM ursa_functions() WHERE name = 'describe')"
        )
        .await,
        "\
name,type,nullable
n_nodes,Int64,false
n_edges,Int64,false
density,Float64,false
avg_degree,Float64,false
n_components,Int64,true
"
    );
    // Nothing SQL cannot call is listed, and dtype is not a SQL parameter.
    assert_eq!(
        csv(
            &ctx,
            "SELECT count(*) AS n FROM (SELECT unnest(parameters) AS p FROM ursa_functions()) \
             WHERE p['name'] = 'dtype'"
        )
        .await,
        "n\n0\n"
    );
    assert_eq!(
        error(&ctx, "SELECT * FROM ursa_functions(1)").await,
        "ursa_functions() takes no arguments: SELECT * FROM ursa_functions()"
    );
}

#[tokio::test]
async fn every_function_runs_on_an_empty_graph() {
    let ctx = session().await;
    ctx.sql("CREATE VIEW none AS SELECT * FROM scenes WHERE weight < 0")
        .await
        .unwrap();
    put_graph(&ctx, "empty", "none");
    for f in ursa_engine::graph::functions::specs() {
        let sql = format!("SELECT count(*) AS n FROM {}(empty)", f.name);
        let expected = if f.shape == ursa_core::catalog::Shape::PerNode {
            "n\n0\n"
        } else {
            "n\n1\n"
        };
        assert_eq!(csv(&ctx, &sql).await, expected, "{sql}");
    }
    let weighted = "SELECT count(*) AS n FROM algorithms(empty, pagerank(weight => weight), \
                    louvain(weight => weight), closeness(weight => weight))";
    assert_eq!(csv(&ctx, weighted).await, "n\n0\n");
}
