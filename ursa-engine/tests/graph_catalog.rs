//! The graph catalog through the engine API: `show_graphs`, `describe_graph`,
//! graphs over file paths, the DDL checks, and listings when a table under a
//! graph has gone.

use std::path::{Path, PathBuf};

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::common::TableReference;
use datafusion::error::Result;
use datafusion::prelude::SessionContext;
use ursa_engine::graph::{
    check_ddl, create_graph, describe_graph, drop_graph, show_graphs, EdgeTable, GraphDef,
    VertexTable,
};
use ursa_engine::{new_session, user_message};

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../python/ursa/datasets/data")
        .join(file)
}

fn def(name: &str, edges: &str, vertices: Option<(&str, &str)>) -> GraphDef {
    GraphDef {
        name: name.into(),
        edges: EdgeTable {
            table: TableReference::bare(edges),
            source: "src".into(),
            destination: "dst".into(),
        },
        vertices: vertices.map(|(table, key)| VertexTable {
            table: TableReference::bare(table),
            key: key.into(),
        }),
    }
}

fn pretty(batch: RecordBatch) -> String {
    pretty_format_batches(&[batch]).unwrap().to_string()
}

async fn table(ctx: &SessionContext, sql: &str) -> Result<String> {
    let batches = ctx.sql(sql).await?.collect().await?;
    Ok(pretty_format_batches(&batches)?.to_string())
}

/// The karate views of Getting Started, and the graph `karate` over them.
async fn karate() -> SessionContext {
    let ctx = new_session();
    ctx.sql(&format!(
        "CREATE VIEW members AS SELECT * FROM '{}'",
        data("karate_nodes.csv").display()
    ))
    .await
    .unwrap();
    ctx.sql(&format!(
        "CREATE VIEW ties AS SELECT * FROM '{}'",
        data("karate_edges.csv").display()
    ))
    .await
    .unwrap();
    create_graph(
        &ctx,
        def("karate", "ties", Some(("members", "id"))),
        false,
        false,
    )
    .await
    .unwrap();
    ctx
}

#[tokio::test]
async fn show_and_describe() {
    let ctx = karate().await;
    let florentine = data("florentine_edges.csv").display().to_string();
    create_graph(&ctx, def("florentine", &florentine, None), false, false)
        .await
        .unwrap();
    let shown = pretty(show_graphs(&ctx.state()).unwrap()).replace(&florentine, "f.csv");
    assert!(
        shown.contains("| florentine | 'f.csv'"),
        "a file path is shown quoted: {shown}"
    );
    assert!(shown.contains("| karate     | ties "), "{shown}");

    let described = pretty(describe_graph(&ctx.state(), "karate").await.unwrap());
    assert_eq!(
        described,
        "\
+------------+-------------+-----------+-------------+--------------+
| table_name | column_name | data_type | role        | origin       |
+------------+-------------+-----------+-------------+--------------+
| nodes      | id          | Int64     | node id     | members.id   |
| nodes      | club        | Utf8      |             | members.club |
| edges      | src         | Int64     | source      | ties.src     |
| edges      | dst         | Int64     | destination | ties.dst     |
+------------+-------------+-----------+-------------+--------------+"
    );
    let e = describe_graph(&ctx.state(), "florentin").await.unwrap_err();
    assert_eq!(
        user_message(&e),
        "graph 'florentin' does not exist; did you mean 'florentine'?"
    );
}

/// A graph over a file path plans its algorithms: the `RelationPlanner` is
/// sync, so the file is opened when the graph is created.
#[tokio::test]
async fn a_graph_over_a_file_path_runs_algorithms() {
    let ctx = new_session();
    let path = data("florentine_edges.csv").display().to_string();
    create_graph(&ctx, def("florentine", &path, None), false, false)
        .await
        .unwrap();
    assert_eq!(
        table(
            &ctx,
            "SELECT id, degree FROM degree(florentine, direction => 'both')
             ORDER BY degree DESC, id LIMIT 3"
        )
        .await
        .unwrap(),
        "\
+----------+--------+
| id       | degree |
+----------+--------+
| Medici   | 6      |
| Guadagni | 4      |
| Strozzi  | 4      |
+----------+--------+"
    );
    drop_graph(&ctx, "florentine", false).unwrap();
    assert!(ctx.sql("SELECT * FROM degree(florentine)").await.is_err());
}

#[tokio::test]
async fn ddl_that_would_break_a_graph_is_refused() {
    let ctx = karate().await;
    let check = |sql: &'static str| {
        let ctx = ctx.clone();
        async move {
            let state = ctx.state();
            let plan = state.create_logical_plan(sql).await.unwrap();
            check_ddl(&state, &plan).await.map_err(|e| user_message(&e))
        }
    };
    assert_eq!(
        check("DROP VIEW ties").await.unwrap_err(),
        "cannot drop view 'ties': graph 'karate' uses it; drop property graph karate first"
    );
    assert!(check("DROP VIEW members").await.is_err());
    assert!(check("DROP SCHEMA karate").await.is_err());
    assert!(
        check("CREATE OR REPLACE VIEW ties AS SELECT 1 AS src, 2 AS x")
            .await
            .unwrap_err()
            .starts_with("cannot replace view 'ties'")
    );
    check("CREATE OR REPLACE VIEW ties AS SELECT 1 AS src, 2 AS dst")
        .await
        .unwrap();
    check("DROP VIEW IF EXISTS other").await.unwrap();
    check("SELECT 1").await.unwrap();
}

#[tokio::test]
async fn listings_survive_a_deregistered_table() {
    let ctx = karate().await;
    ctx.deregister_table("members").unwrap();
    // `SHOW TABLES` and `information_schema` still work.
    let shown = table(&ctx, "SHOW TABLES").await.unwrap();
    assert!(shown.contains("| karate             | nodes "), "{shown}");
    table(&ctx, "SELECT * FROM information_schema.columns")
        .await
        .unwrap();
    table(&ctx, "SELECT * FROM information_schema.views")
        .await
        .unwrap();
    // The graph's tables say what is wrong; the edges are still readable.
    let e = ctx
        .sql("SELECT * FROM karate.nodes")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert_eq!(
        user_message(&e),
        "cannot read karate.nodes: table 'members' not found"
    );
    assert!(table(&ctx, "SELECT count(*) FROM karate.edges")
        .await
        .unwrap()
        .contains("| 78 "));
    assert!(show_graphs(&ctx.state()).is_ok());
}
