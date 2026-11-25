//! NULL edge endpoints: one rule for every path (#336).
//!
//! An edge with a NULL source or destination is not part of the graph, with
//! or without a vertex table: it is dropped, as an inner join of the edges
//! with their nodes drops it, and counted with the edges to no vertex key in
//! `describe`'s `n_dropped_edges`. Neither end of such an edge is a node by
//! it. The same graph then gives the same answer whichever path a query
//! takes:
//!
//! - the index build: the algorithms (`degree`, `connected_components`),
//!   `hop`, `shortest_path` and `describe`;
//! - `GRAPH_TABLE` through the index (a bound node, a path search);
//! - `GRAPH_TABLE` planned as joins, and `<graph>.nodes`.
//!
//! Before #336, without a vertex table the index paths failed ("an edge has a
//! null source or destination"), the join paths dropped the edge, and
//! `<graph>.nodes` (and `MATCH (x)`) listed the non-NULL end of a dropped
//! edge as a node. A vertex table with a NULL key is refused by every path,
//! as it was (#411).

use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::prelude::SessionContext;
use ursa_sql::{execute_with_text, parse_sql_with_text_for};

/// Run every statement of `sql`; the last one's rows as CSV lines.
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

async fn err(ctx: &SessionContext, sql: &str) -> String {
    match run(ctx, sql).await {
        Ok(rows) => panic!("{sql} should fail; got {rows:?}"),
        Err(e) => ursa_engine::user_message(&e),
    }
}

/// The physical plan of `query`.
async fn physical(ctx: &SessionContext, query: &str) -> String {
    ok(ctx, &format!("EXPLAIN {query}")).await.join("\n")
}

/// The edge rows, in order (`""` is NULL), and the weight of each is its
/// row number plus one:
///
/// ```text
/// 0: 1 -> 2       kept
/// 1: 2 -> 3       kept
/// 2: NULL -> 4    NULL source: dropped (4 is on no other edge)
/// 3: 3 -> NULL    NULL destination: dropped
/// 4: NULL -> NULL both NULL: dropped
/// 5: 3 -> 1       kept
/// 6: 5 -> 6       kept
/// 7: 1 -> 9       kept without a vertex table; 9 is no key of `people`
/// ```
const EDGES: [(&str, &str); 8] = [
    ("1", "2"),
    ("2", "3"),
    ("", "4"),
    ("3", ""),
    ("", ""),
    ("3", "1"),
    ("5", "6"),
    ("1", "9"),
];

/// Ids as written: integers, or strings `n1`, `n2`, ….
#[derive(Clone, Copy)]
enum Keys {
    Int,
    Str,
}

impl Keys {
    /// The id `k` (`""`, NULL, stays empty) as a CSV cell.
    fn cell(self, k: &str) -> String {
        match (self, k) {
            (_, "") | (Keys::Int, _) => k.to_string(),
            (Keys::Str, _) => format!("n{k}"),
        }
    }

    /// The id `k` as it prints.
    fn id(self, k: &str) -> String {
        self.cell(k)
    }

    /// The id `k` as a SQL literal.
    fn lit(self, k: &str) -> String {
        match self {
            Keys::Int => k.to_string(),
            Keys::Str => format!("'n{k}'"),
        }
    }

    /// `cells` with each comma-separated id written as `self` writes it;
    /// what is not an id (a cost, a list) comes last, after `#`, as it is.
    fn row(self, cells: &str) -> String {
        let (ids, rest) = match cells.split_once('#') {
            Some((ids, rest)) => (ids.trim_end_matches(','), Some(rest)),
            None => (cells, None),
        };
        let mut out: Vec<String> = ids.split(',').map(|c| self.id(c)).collect();
        out.extend(rest.map(str::to_string));
        out.join(",")
    }

    fn rows(self, rows: &[&str]) -> Vec<String> {
        rows.iter().map(|r| self.row(r)).collect()
    }
}

/// A session over CSV files: the edges above as `e`; `people` (keys 1 to 6:
/// 4 is on no kept edge, 9 is no key) and `nk` (the same with a NULL key in
/// place of 4); and the graphs `g` (no vertex table), `v` (`people`) and `k`
/// (`nk`).
async fn session(dir: &std::path::Path, keys: Keys) -> SessionContext {
    let mut csv = String::from("src,dst,w\n");
    for (i, (s, d)) in EDGES.iter().enumerate() {
        csv.push_str(&format!("{},{},{}\n", keys.cell(s), keys.cell(d), i + 1));
    }
    let table = |name: &str, ids: [&str; 6]| {
        let mut text = String::from("id,name\n");
        for k in ids {
            text.push_str(&format!("{},p{k}\n", keys.cell(k)));
        }
        let path = dir.join(format!("{name}.csv"));
        std::fs::write(&path, text).unwrap();
        path
    };
    let edges = dir.join("edges.csv");
    std::fs::write(&edges, csv).unwrap();
    let people = table("people", ["1", "2", "3", "4", "5", "6"]);
    let nk = table("nk", ["1", "2", "3", "", "5", "6"]);
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        &format!(
            "CREATE VIEW e AS SELECT * FROM '{}';
             CREATE VIEW people AS SELECT * FROM '{}';
             CREATE VIEW nk AS SELECT * FROM '{}';
             CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (src) DESTINATION KEY (dst));
             CREATE PROPERTY GRAPH v VERTEX TABLES (people KEY (id))
               EDGE TABLES (e SOURCE KEY (src) REFERENCES people (id)
                              DESTINATION KEY (dst) REFERENCES people (id));
             CREATE PROPERTY GRAPH k VERTEX TABLES (nk KEY (id))
               EDGE TABLES (e SOURCE KEY (src) REFERENCES nk (id)
                              DESTINATION KEY (dst) REFERENCES nk (id));",
            edges.display(),
            people.display(),
            nk.display()
        ),
    )
    .await;
    ctx
}

/// What every path says of graph `graph`, whose kept edges are `kept` (as
/// `src,dst`), whose nodes are `nodes`, and which dropped `dropped` edge
/// rows.
struct Expected<'a> {
    graph: &'a str,
    nodes: &'a [&'a str],
    kept: &'a [&'a str],
    dropped: usize,
}

/// Each path, on `expected.graph`, gives the answer the kept edges and
/// nodes give: they all agree.
async fn every_path_agrees(ctx: &SessionContext, keys: Keys, expected: &Expected<'_>) {
    let g = expected.graph;
    let lit = |k: &str| keys.lit(k);
    let mut nodes = keys.rows(expected.nodes);
    nodes.sort();
    let kept: Vec<(String, String)> = expected
        .kept
        .iter()
        .map(|e| {
            let (s, d) = e.split_once(',').unwrap();
            (keys.id(s), keys.id(d))
        })
        .collect();
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };

    // describe: the nodes, the kept edges, the dropped rows.
    assert_eq!(
        ok(
            ctx,
            &format!("SELECT n_nodes, n_edges, n_dropped_edges FROM describe({g}, full => true)")
        )
        .await,
        [format!(
            "{},{},{}",
            nodes.len(),
            kept.len(),
            expected.dropped
        )],
        "{g}: describe"
    );

    // <graph>.nodes and a node pattern planned as a join: the same nodes.
    assert_eq!(
        sorted(ok(ctx, &format!("SELECT id FROM {g}.nodes")).await),
        nodes,
        "{g}.nodes"
    );
    assert_eq!(
        sorted(
            ok(
                ctx,
                &format!("SELECT a FROM GRAPH_TABLE ({g} MATCH (x) COLUMNS (x.id AS a))")
            )
            .await
        ),
        nodes,
        "{g}: MATCH (x)"
    );

    // degree (an algorithm over the index) is the kept edges counted at each
    // node, both ways; and the edge pattern planned as joins is the kept
    // edges themselves.
    let degree = sorted(
        nodes
            .iter()
            .map(|n| {
                let d = kept.iter().filter(|(s, _)| s == n).count()
                    + kept.iter().filter(|(_, t)| t == n).count();
                format!("{n},{d}")
            })
            .collect(),
    );
    assert_eq!(
        sorted(
            ok(
                ctx,
                &format!("SELECT * FROM degree({g}, direction => 'both')")
            )
            .await
        ),
        degree,
        "{g}: degree"
    );
    let joined = sorted(
        ok(
            ctx,
            &format!(
                "SELECT a, b FROM GRAPH_TABLE ({g} MATCH (x)-[r]->(y) COLUMNS (x.id AS a, y.id AS b))"
            ),
        )
        .await,
    );
    assert_eq!(
        joined,
        sorted(kept.iter().map(|(s, d)| format!("{s},{d}")).collect()),
        "{g}: MATCH (x)-[r]->(y)"
    );
    // A join's degree is the algorithm's.
    let join_degree = sorted(
        ok(
            ctx,
            &format!(
                "SELECT n, count(*) FROM (
                   SELECT a AS n FROM GRAPH_TABLE ({g} MATCH (x)-[r]->(y) COLUMNS (x.id AS a))
                   UNION ALL
                   SELECT b AS n FROM GRAPH_TABLE ({g} MATCH (x)-[r]->(y) COLUMNS (y.id AS b))
                 ) GROUP BY n"
            ),
        )
        .await,
    );
    let positive: Vec<String> = degree
        .iter()
        .filter(|d| !d.ends_with(",0"))
        .cloned()
        .collect();
    assert_eq!(join_degree, positive, "{g}: degree by joins");

    // connected_components: as many as the kept edges make of the nodes.
    let components = {
        let mut parent: Vec<usize> = (0..nodes.len()).collect();
        fn find(p: &mut [usize], i: usize) -> usize {
            if p[i] != i {
                let r = find(p, p[i]);
                p[i] = r;
            }
            p[i]
        }
        let at = |n: &String| nodes.iter().position(|m| m == n).unwrap();
        for (s, d) in &kept {
            let (a, b) = (find(&mut parent, at(s)), find(&mut parent, at(d)));
            parent[a] = b;
        }
        (0..nodes.len())
            .filter(|&i| find(&mut parent, i) == i)
            .count()
    };
    assert_eq!(
        ok(
            ctx,
            &format!("SELECT count(DISTINCT connected_components) FROM connected_components({g})")
        )
        .await,
        [components.to_string()],
        "{g}: connected_components"
    );

    // hop from 1, both ways: the component of 1 less 1 itself, which the
    // undirected join reaches in as many steps.
    let hop = sorted(
        ok(
            ctx,
            &format!(
                "SELECT id FROM hop({g}, seeds => [{}], steps => 3, direction => 'both')",
                lit("1")
            ),
        )
        .await,
    );
    let walked = sorted(
        ok(
            ctx,
            &format!(
                "SELECT DISTINCT b FROM GRAPH_TABLE ({g}
                   MATCH (x WHERE x.id = {} OR false)-[r]-{{1,3}}(y) COLUMNS (y.id AS b))
                 WHERE b <> {}",
                lit("1"),
                lit("1")
            ),
        )
        .await,
    );
    assert_eq!(hop, walked, "{g}: hop and a walk");
    assert!(!hop.is_empty());

    // A bound node expands through the index; the same match, bound to
    // nothing (`OR false`), is planned as joins: both give the kept edges.
    let bound = format!(
        "SELECT b FROM GRAPH_TABLE ({g} MATCH (x WHERE x.id = {})-[r]->(y) COLUMNS (y.id AS b))",
        lit("3")
    );
    let unbound = bound.replacen(
        &format!("x.id = {}", lit("3")),
        &format!("x.id = {} OR false", lit("3")),
        1,
    );
    assert!(physical(ctx, &bound).await.contains("PathSearchExec"));
    assert!(!physical(ctx, &unbound).await.contains("PathSearchExec"));
    let from_3 = sorted(
        kept.iter()
            .filter(|(s, _)| *s == keys.id("3"))
            .map(|(_, d)| d.clone())
            .collect(),
    );
    assert_eq!(sorted(ok(ctx, &bound).await), from_3, "{g}: bound");
    assert_eq!(sorted(ok(ctx, &unbound).await), from_3, "{g}: unbound");

    // A bound node with a NULL source edge to it: 4 has none kept.
    let to_4 = format!(
        "SELECT a FROM GRAPH_TABLE ({g} MATCH (x)-[r]->(y WHERE y.id = {}) COLUMNS (x.id AS a))",
        lit("4")
    );
    assert_eq!(ok(ctx, &to_4).await, Vec::<String>::new(), "{g}: to 4");
}

#[tokio::test]
async fn every_path_drops_an_edge_with_a_null_end() {
    for keys in [Keys::Int, Keys::Str] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = session(dir.path(), keys).await;
        // No vertex table: the nodes are the ends of the kept edges; 4,
        // whose one edge has a NULL source, is none.
        every_path_agrees(
            &ctx,
            keys,
            &Expected {
                graph: "g",
                nodes: &["1", "2", "3", "5", "6", "9"],
                kept: &["1,2", "2,3", "3,1", "5,6", "1,9"],
                dropped: 3,
            },
        )
        .await;
        // A vertex table: its keys are the nodes, 4 included; the three
        // NULL-ended edges and the edge to 9 are dropped alike.
        every_path_agrees(
            &ctx,
            keys,
            &Expected {
                graph: "v",
                nodes: &["1", "2", "3", "4", "5", "6"],
                kept: &["1,2", "2,3", "3,1", "5,6"],
                dropped: 4,
            },
        )
        .await;
    }
}

/// A path search, through the index, gives a path's edges as rows of the
/// edge table, the dropped rows counted; weights line up with the kept
/// edges. The same on both graphs, with or without a vertex table.
#[tokio::test]
async fn a_path_names_edge_table_rows_and_weights_line_up() {
    for keys in [Keys::Int, Keys::Str] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = session(dir.path(), keys).await;
        let (one, three) = (keys.lit("1"), keys.lit("3"));
        for (g, reached) in [
            ("g", &["1,#[0, 1, 5]", "2,#[0]", "3,#[0, 1]", "9,#[7]"][..]),
            ("v", &["1,#[0, 1, 5]", "2,#[0]", "3,#[0, 1]"][..]),
        ] {
            let search = format!(
                "SELECT b, es FROM GRAPH_TABLE ({g} MATCH p = ANY SHORTEST (x WHERE x.id = {one})-[r]->{{1,3}}(y)
                   COLUMNS (y.id AS b, edges(p) AS es)) ORDER BY b"
            );
            assert!(physical(&ctx, &search).await.contains("PathSearchExec"));
            assert_eq!(ok(&ctx, &search).await, keys.rows(reached), "{g}");
            // Row 0 weighs 1 and row 1 weighs 2: the NULL rows between are
            // not counted in.
            for warm in [false, true] {
                if warm {
                    // The index is cached unweighted first: the weighted
                    // rows are then checked against it, NULL rows skipped.
                    ok(&ctx, &format!("SELECT * FROM degree({g})")).await;
                }
                assert_eq!(
                    ok(
                        &ctx,
                        &format!(
                            "SELECT src, dst, cost FROM shortest_path({g}, source => {one}, \
                             target => {three}, weight => w)"
                        )
                    )
                    .await,
                    keys.rows(&["1,2,#1.0", "2,3,#3.0"]),
                    "{g}"
                );
            }
        }
    }
}

/// A value that is only the non-NULL end of a dropped edge is no node: `hop`
/// refuses it as a seed, as it refuses a value that is no vertex key, and a
/// join from it finds nothing.
#[tokio::test]
async fn the_end_of_a_dropped_edge_is_no_node() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = session(dir.path(), Keys::Int).await;
    assert_eq!(
        ok(&ctx, "SELECT count(*) FROM g.nodes WHERE id = 4").await,
        ["0"]
    );
    for (g, seed) in [("g", 4), ("v", 9)] {
        assert_eq!(
            err(
                &ctx,
                &format!("SELECT * FROM hop({g}, seeds => [{seed}], steps => 1)")
            )
            .await,
            format!("hop(seeds =>): {seed} is not a node of graph '{g}'")
        );
    }
    assert_eq!(
        ok(
            &ctx,
            "SELECT count(*) FROM GRAPH_TABLE (g MATCH (x WHERE x.id = 4)-[r]-(y) COLUMNS (y.id AS b))"
        )
        .await,
        ["0"]
    );
}

/// A vertex table with a NULL key is refused by every path, as it was (#411):
/// a key must identify one row. The NULL-ended edges change nothing.
#[tokio::test]
async fn a_null_vertex_key_is_refused_by_every_path() {
    for keys in [Keys::Int, Keys::Str] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = session(dir.path(), keys).await;
        let one = keys.lit("1");
        for query in [
            "SELECT * FROM describe(k, full => true)".to_string(),
            "SELECT * FROM degree(k)".to_string(),
            "SELECT * FROM connected_components(k)".to_string(),
            format!("SELECT * FROM hop(k, seeds => [{one}], steps => 1)"),
            "SELECT * FROM k.nodes".to_string(),
            "SELECT * FROM GRAPH_TABLE (k MATCH (x)-[r]->(y) COLUMNS (x.id AS a))".to_string(),
            format!(
                "SELECT * FROM GRAPH_TABLE (k MATCH (x WHERE x.id = {one})-[r]->(y) COLUMNS (y.id AS b))"
            ),
            format!(
                "SELECT * FROM GRAPH_TABLE (k MATCH p = ANY SHORTEST (x WHERE x.id = {one})-[r]->{{1,3}}(y) \
                 COLUMNS (y.id AS b))"
            ),
        ] {
            let message = err(&ctx, &query).await;
            assert!(
                message.contains("vertex table 'nk' has 1 row with a NULL key"),
                "{query}: {message}"
            );
        }
    }
}
