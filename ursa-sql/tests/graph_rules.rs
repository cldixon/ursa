//! The result → graph rules through Ursa's whole dialect (#433): what
//! `.graph` draws after each kind of graph query on the karate club graph,
//! row by row of the "should be" column of the design's §1 table
//! (`design/graph-explorer.md`), and a pattern over two vertex tables.
//!
//! The rules themselves, from schemas with and without metadata, are in
//! `ursa-engine`'s `graph::view` tests; this holds them to what `GRAPH_TABLE`
//! and the graph functions really return. The last test overrides them with
//! `AS …`, `ON graph` and `LAYOUT` (#434).

use std::path::{Path, PathBuf};

use datafusion::prelude::SessionContext;
use ursa_engine::graph::view::{interpret, of_command, GraphView, RunSql};
use ursa_sql::Sql;
use ursa_viz::model::{Key, Layout, Path as ModelPath, Values};

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("python/ursa/datasets/data")
        .join(file)
}

async fn karate() -> SessionContext {
    let ctx = ursa_engine::new_session();
    Sql.run(
        &ctx,
        &format!(
            "CREATE VIEW members AS SELECT * FROM '{}';
             CREATE VIEW ties AS SELECT * FROM '{}';
             CREATE PROPERTY GRAPH karate
               VERTEX TABLES (members KEY (id))
               EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                                 DESTINATION KEY (dst) REFERENCES members (id));",
            data("karate_nodes.csv").display(),
            data("karate_edges.csv").display()
        ),
    )
    .await
    .unwrap();
    ctx
}

/// `.graph` after `sql`: the last result, by the rules.
async fn after(ctx: &SessionContext, sql: &str) -> GraphView {
    let (schema, batches) = Sql.run(ctx, sql).await.unwrap();
    interpret(ctx, &Sql, schema, batches)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)))
}

fn ids(view: &GraphView) -> Vec<&str> {
    view.result.node_keys().iter().collect()
}

/// The edges as pairs of ids, each with the smaller first, sorted.
fn pairs(view: &GraphView) -> Vec<(i64, i64)> {
    let r = &view.result;
    let key = |i: u32| match r.node_key(i as usize) {
        Key::Int(v) => v,
        other => panic!("{other:?}"),
    };
    let mut out: Vec<(i64, i64)> = r
        .src()
        .iter()
        .zip(r.dst())
        .map(|(&s, &d)| (key(s).min(key(d)), key(s).max(key(d))))
        .collect();
    out.sort_unstable();
    out
}

/// Karate's ties, as [`pairs`] gives them.
async fn ties(ctx: &SessionContext) -> Vec<(i64, i64)> {
    let view = of_command(ctx, &Sql, "SELECT src, dst FROM ties", None)
        .await
        .unwrap();
    pairs(&view)
}

const ROUTE: &str = "SELECT * FROM GRAPH_TABLE (karate MATCH p = ANY SHORTEST \
     (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26) \
     COLUMNS (path_length(p) AS steps, vertices(p) AS members, edges(p) AS ties))";

#[tokio::test]
async fn pagerank_limit_5_is_five_nodes_sized_with_the_edges_among_them() {
    let ctx = karate().await;
    let view = after(&ctx, "SELECT * FROM pagerank(karate) LIMIT 5").await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "nodes of karate (id, pagerank)");
    // Not 5 edges from each id to its score over 9 "nodes".
    assert_eq!(ids(&view), ["0", "1", "2", "3", "4"]);
    assert_eq!(r.encodings().size_by.as_deref(), Some("pagerank"));
    let Values::F32(sizes) = &r.node_column("pagerank").unwrap().values else {
        panic!("numbers");
    };
    assert!(sizes.iter().all(|v| *v > 0.0 && *v < 1.0), "{sizes:?}");
    // The ties among 0..4.
    let among: Vec<(i64, i64)> = ties(&ctx)
        .await
        .into_iter()
        .filter(|(a, b)| *a < 5 && *b < 5)
        .collect();
    assert_eq!(pairs(&view), among);
    assert_eq!(among.len(), 7);
}

#[tokio::test]
async fn algorithms_is_the_graph_sized_by_pagerank_coloured_by_community() {
    let ctx = karate().await;
    let view = after(
        &ctx,
        "SELECT * FROM algorithms(karate, pagerank(direction => 'both'), \
         community => louvain(seed => 1))",
    )
    .await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "nodes of karate (id, pagerank, community)");
    assert_eq!((r.n_nodes(), r.n_edges()), (34, 78));
    assert_eq!(pairs(&view), ties(&ctx).await);
    assert_eq!(r.encodings().size_by.as_deref(), Some("pagerank"));
    assert_eq!(r.encodings().color_by.as_deref(), Some("community"));
    let legend = view.legend().unwrap();
    assert_eq!(legend.counts.iter().sum::<u32>(), 34);
    assert!(legend.labels.len() >= 2);
}

#[tokio::test]
async fn hop_is_rings_at_distance_1_and_2_with_the_real_edges() {
    let ctx = karate().await;
    let view = after(
        &ctx,
        "SELECT * FROM hop(karate, seeds => [0], steps => 2, direction => 'both')",
    )
    .await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "rings on karate (seed, id, distance)");
    assert_eq!(r.encodings().layout, Layout::Rings);
    let Values::U32(distance) = &r.node_column("distance").unwrap().values else {
        panic!("distances");
    };
    let count = |d: u32| distance.iter().filter(|&&x| x == d).count();
    assert_eq!((count(0), count(1), count(2)), (1, 16, 9));
    // Not a star: the ties among the 26 nodes, some of them between two
    // nodes of the outer ring's neighbours.
    let reached: Vec<i64> = ids(&view).iter().map(|id| id.parse().unwrap()).collect();
    let among: Vec<(i64, i64)> = ties(&ctx)
        .await
        .into_iter()
        .filter(|(a, b)| reached.contains(a) && reached.contains(b))
        .collect();
    assert_eq!(pairs(&view), among);
    assert!(among.iter().any(|(a, b)| *a != 0 && *b != 0));
}

#[tokio::test]
async fn any_shortest_is_the_route_six_nodes_in_order() {
    let ctx = karate().await;
    let view = after(&ctx, ROUTE).await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "a path on karate (members, ties)");
    // Not one edge from 5 to a list.
    assert_eq!(ids(&view), ["16", "5", "0", "8", "33", "26"]);
    assert_eq!(
        r.highlights().paths,
        [ModelPath {
            nodes: vec![0, 1, 2, 3, 4, 5],
            edges: vec![0, 1, 2, 3, 4]
        }]
    );
    assert_eq!(r.encodings().layout, Layout::Layers);
    // edges(p): each edge has its row in `ties`.
    assert_eq!(r.edge_rows(), Some(&[39, 4, 7, 43, 67][..]));
    assert_eq!(r.edge_labels(), ["ties"]);
    assert_eq!(r.node_labels()[0].name, "members");
    // element_id(p) alone draws the same route.
    let elements = after(
        &ctx,
        &ROUTE.replace(
            "path_length(p) AS steps, vertices(p) AS members, edges(p) AS ties",
            "element_id(p) AS elements",
        ),
    )
    .await;
    assert_eq!(ids(&elements), ids(&view));
    assert_eq!(elements.result.highlights().paths, r.highlights().paths);
    assert_eq!(elements.result.edge_rows(), r.edge_rows());
}

#[tokio::test]
async fn vertices_alone_is_the_paths_union() {
    let ctx = karate().await;
    // One column was an error; now the union of the eight shortest paths.
    let view = after(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (karate MATCH p = ALL SHORTEST \
         (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26) COLUMNS (vertices(p) AS members))",
    )
    .await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "8 paths on karate (members)");
    assert_eq!(r.highlights().paths.len(), 8);
    assert_eq!((r.n_nodes(), r.n_edges()), (10, 13));
    // Every edge is a tie, once; and all paths share their ends: layers.
    let all = ties(&ctx).await;
    let drawn = pairs(&view);
    assert!(drawn.iter().all(|e| all.contains(e)), "{drawn:?}");
    let mut once = drawn.clone();
    once.dedup();
    assert_eq!(once, drawn);
    assert_eq!(r.encodings().layout, Layout::Layers);
    let Values::U32(step) = &r.node_column("step").unwrap().values else {
        panic!("steps");
    };
    assert_eq!((step[0], step.iter().max()), (0, Some(&5)));
}

#[tokio::test]
async fn three_variables_draw_both_edges_of_each_match() {
    let ctx = karate().await;
    // Every two-step walk from 0 through its neighbours 1 and 2.
    let sql =
        "SELECT * FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0)-[]-(b WHERE b.id < 3)-[]-(c) \
               COLUMNS (a.id AS a, b.id AS b, c.id AS c))";
    let view = after(&ctx, sql).await;
    assert_eq!(view.drawn_as(), "pattern matches on karate (a, b, c)");
    // Not only a–b: a–b and b–c of each match, each edge once.
    let (_, batches) = Sql.run(&ctx, sql).await.unwrap();
    let mut expected: Vec<(i64, i64)> = Vec::new();
    for batch in &batches {
        let column = |i: usize| {
            batch
                .column(i)
                .as_any()
                .downcast_ref::<datafusion::arrow::array::Int64Array>()
                .unwrap()
                .clone()
        };
        let (a, b, c) = (column(0), column(1), column(2));
        for i in 0..batch.num_rows() {
            for (x, y) in [(a.value(i), b.value(i)), (b.value(i), c.value(i))] {
                expected.push((x.min(y), x.max(y)));
            }
        }
    }
    expected.sort_unstable();
    expected.dedup();
    assert_eq!(pairs(&view), expected);
    assert!(expected.iter().any(|(x, y)| *x != 0 && *y != 0));
    assert_eq!(view.result.edge_labels(), ["ties"]);
}

#[tokio::test]
async fn ids_beside_properties_are_the_edges_between_ids() {
    let ctx = karate().await;
    let view = after(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (karate MATCH (a)-[e]-(b) WHERE a.club <> b.club \
         COLUMNS (a.club AS ca, b.club AS cb, a.id AS x, b.id AS y))",
    )
    .await;
    let r = &view.result;
    // Not two nodes, `Officer` and `Mr. Hi`.
    assert_eq!(view.drawn_as(), "edges of karate (x, y)");
    assert_eq!(r.n_edges(), 22);
    let all = ties(&ctx).await;
    assert!(pairs(&view).iter().all(|e| all.contains(e)));
    assert!(ids(&view).iter().all(|id| id.parse::<i64>().is_ok()));
    // The properties are on their variable's nodes: every node here is in
    // one club as `a` and has a neighbour in the other.
    let club = |name: &str, node: u32| {
        r.node_column(name)
            .unwrap()
            .values
            .text(node as usize)
            .map(str::to_string)
    };
    for e in 0..r.n_edges() {
        let (a, b) = (r.src()[e], r.dst()[e]);
        assert_ne!(club("ca", a), club("cb", b));
        assert!(club("ca", a).is_some() && club("cb", b).is_some());
    }
    // COLOR BY reads the graph's vertex columns: the result names its graph.
    let last = Sql
        .run(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (karate MATCH (a)-[e]->(b) COLUMNS (a.id AS x, b.id AS y))",
        )
        .await
        .unwrap();
    let view = of_command(&ctx, &Sql, "COLOR BY club SIZE BY pagerank", Some(last))
        .await
        .unwrap();
    assert_eq!((view.result.n_nodes(), view.result.n_edges()), (34, 78));
    assert_eq!(view.legend().unwrap().counts.iter().sum::<u32>(), 34);
}

#[tokio::test]
async fn shortest_path_is_the_same_edges_drawn_as_a_route() {
    let ctx = karate().await;
    let sql =
        "SELECT * FROM shortest_path(karate, source => 16, target => 26, direction => 'both')";
    let view = after(&ctx, sql).await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "a path on karate (src, dst, step)");
    // The same nodes and edges as rows-as-edges drew, in the same order…
    let edges = of_command(&ctx, &Sql, &format!("SELECT src, dst FROM ({sql})"), None)
        .await
        .unwrap();
    assert_eq!(ids(&view), ids(&edges));
    assert_eq!((r.src(), r.dst()), (edges.result.src(), edges.result.dst()));
    // …and now a route: one path along them, in layers, with each step's
    // cost on its edge.
    assert_eq!(
        r.highlights().paths,
        [ModelPath {
            nodes: vec![0, 1, 2, 3, 4, 5],
            edges: vec![0, 1, 2, 3, 4]
        }]
    );
    assert_eq!(r.encodings().layout, Layout::Layers);
    assert_eq!(
        r.edge_column("cost").unwrap().values,
        Values::F32(vec![1.0, 2.0, 3.0, 4.0, 5.0])
    );
    // Sorted another way, it is the same route: the steps say the order.
    let reversed = after(&ctx, &format!("{sql} ORDER BY step DESC")).await;
    assert_eq!(ids(&reversed), ids(&view));
    assert_eq!(reversed.result.highlights().paths, r.highlights().paths);
}

/// Variables of different vertex tables are nodes of different labels: a
/// person and a place may share a key.
#[tokio::test]
async fn a_pattern_over_two_vertex_tables_keeps_their_keys_apart() {
    let ctx = ursa_engine::new_session();
    Sql.run(
        &ctx,
        "CREATE TABLE people AS SELECT * FROM (VALUES (1, 'ann'), (2, 'bob'), (3, 'cy')) AS t(id, name);
         CREATE TABLE places AS SELECT * FROM (VALUES (1, 'york'), (2, 'bath')) AS t(id, town);
         CREATE TABLE knows AS SELECT * FROM (VALUES (1, 2), (2, 3)) AS t(a, b);
         CREATE TABLE lives AS SELECT * FROM (VALUES (1, 1), (2, 1), (3, 2)) AS t(person, place);
         CREATE PROPERTY GRAPH town
           VERTEX TABLES (people KEY (id), places KEY (id))
           EDGE TABLES (
             knows SOURCE KEY (a) REFERENCES people (id) DESTINATION KEY (b) REFERENCES people (id),
             lives SOURCE KEY (person) REFERENCES people (id) DESTINATION KEY (place) REFERENCES places (id));",
    )
    .await
    .unwrap();
    let view = after(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (town MATCH (a:people)-[k:knows]->(b:people)-[l:lives]->(c:places) \
         COLUMNS (a.id AS a, b.id AS b, c.id AS c, c.town AS town, a.name AS name)) \
         ORDER BY a, b",
    )
    .await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "pattern matches on town (a, b, c)");
    let labels: Vec<&str> = r.node_labels().iter().map(|l| l.name.as_str()).collect();
    assert_eq!(labels, ["people", "places"]);
    // ann→bob→york and bob→cy→bath: people 1, 2, 3 and places 1, 2.
    assert_eq!(ids(&view), ["1", "2", "3", "1", "2"]);
    assert_eq!(r.node_label(), [0, 0, 0, 1, 1]);
    assert_eq!((r.src(), r.dst()), (&[0, 1, 1, 2][..], &[1, 3, 2, 4][..]));
    assert_eq!(r.edge_labels(), ["knows", "lives"]);
    assert_eq!(r.edge_label().to_vec(), [0, 1, 0, 1]);
    // A property is on its variable's nodes, of its vertex table.
    let town = &r.node_column("town").unwrap().values;
    let towns: Vec<Option<&str>> = (0..5).map(|i| town.text(i)).collect();
    assert_eq!(towns, [None, None, None, Some("york"), Some("bath")]);
    let name = &r.node_column("name").unwrap().values;
    assert_eq!(
        (name.text(0), name.text(1), name.text(3)),
        (Some("ann"), Some("bob"), None)
    );
    // Coloured by label, unless COLOR BY says otherwise.
    let legend = view.legend().unwrap();
    assert_eq!(
        (legend.by.as_str(), legend.labels, legend.counts),
        (
            "label",
            vec!["people".to_string(), "places".to_string()],
            vec![3, 2]
        )
    );

    // Two variables of two tables are edges, and the keys still do not meet.
    let view = after(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (town MATCH (a:people)-[l:lives]->(c:places) \
         COLUMNS (a.id AS person, c.id AS place)) ORDER BY person",
    )
    .await;
    assert_eq!(view.result.provenance().rule.as_deref(), Some("edges"));
    assert_eq!(ids(&view), ["1", "2", "3", "1", "2"]);
    assert_eq!(view.result.n_edges(), 3);
}

/// `.graph CLAUSES` after `sql`: the last result, read as the clauses say.
async fn drawn(ctx: &SessionContext, sql: &str, clauses: &str) -> Result<GraphView, String> {
    let last = Sql.run(ctx, sql).await.unwrap();
    of_command(ctx, &Sql, clauses, Some(last))
        .await
        .map_err(|e| ursa_engine::user_message(&e))
}

/// `AS …` is rule 0 (#434): a graph query's result drawn as the clause
/// says, not as its metadata would have it.
#[tokio::test]
async fn as_overrides_what_a_graph_query_says_it_is() {
    let ctx = karate().await;
    // A hop result is rings by rule 2; as edges it is the star it was before
    // #433, and as nodes the reached nodes with the ties among them.
    let hop = "SELECT * FROM hop(karate, seeds => [0], steps => 2, direction => 'both')";
    let rows: usize = Sql
        .run(&ctx, hop)
        .await
        .unwrap()
        .1
        .iter()
        .map(|b| b.num_rows())
        .sum();
    let rings = after(&ctx, hop).await;
    let view = drawn(&ctx, hop, "AS edges(seed, id)").await.unwrap();
    assert_eq!(view.drawn_as(), "edges of karate (seed, id)");
    assert_eq!(view.result.n_edges(), rows);
    assert!(view.result.src().iter().all(|&s| s == 0));
    assert!(view.result.highlights().is_empty());
    assert_eq!(view.result.encodings().layout, Layout::Force);
    let view = drawn(&ctx, hop, "AS nodes(id)").await.unwrap();
    assert_eq!(view.drawn_as(), "nodes of karate (id)");
    assert!(view.result.highlights().is_empty());
    let all = ties(&ctx).await;
    assert!(!pairs(&view).is_empty());
    assert!(pairs(&view).iter().all(|p| all.contains(p)));
    // The rule's own reading, laid out otherwise.
    let view = drawn(&ctx, hop, "LAYOUT force").await.unwrap();
    assert_eq!(view.drawn_as(), rings.drawn_as());
    assert_eq!(view.result.encodings().layout, Layout::Force);
    assert_eq!(view.result.highlights(), rings.result.highlights());
    assert_eq!(pairs(&view), pairs(&rings));

    // A pattern's three variables are both edges of each match by rule 4;
    // as edges(a, c), the two ends of each two-step walk.
    let three = "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0)-[]-(b WHERE b.id = 1)-[]-(c) \
                 COLUMNS (a.id AS a, b.id AS b, c.id AS c))";
    let view = drawn(&ctx, three, "AS edges(a, c)").await.unwrap();
    assert_eq!(view.drawn_as(), "edges of karate (a, c)");
    let walks: usize = Sql
        .run(&ctx, three)
        .await
        .unwrap()
        .1
        .iter()
        .map(|b| b.num_rows())
        .sum();
    assert_eq!((walks, view.result.n_edges()), (9, 9));
    // Every edge starts at a (node 0); the result names no edge table for them.
    assert!(view.result.src().iter().all(|&s| s == 0));
    assert_eq!(view.result.edge_labels(), [""]);

    // A path's members without its ties: the same route, its edges' rows
    // not known.
    let route = "FROM GRAPH_TABLE (karate MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b \
                 WHERE b.id = 26) COLUMNS (path_length(p) AS steps, vertices(p) AS members, \
                 edges(p) AS ties))";
    let view = drawn(&ctx, route, "AS path(members)").await.unwrap();
    assert_eq!(view.drawn_as(), "a path on karate (members)");
    assert_eq!(ids(&view), ["16", "5", "0", "8", "33", "26"]);
    assert_eq!(view.result.edge_row(0), None);
    let view = drawn(&ctx, route, "AS path(members, ties)").await.unwrap();
    assert_eq!(view.drawn_as(), "a path on karate (members, ties)");
    assert!(view.result.edge_row(0).is_some());

    // The message of rule 6 names a clause that now draws: club and n.
    let counts = "SELECT club, count(*) AS n FROM members GROUP BY club";
    let err = drawn(&ctx, counts, "").await.unwrap_err();
    assert!(err.contains(".graph AS nodes(club)"), "{err}");
    let view = drawn(&ctx, counts, "AS nodes(club)").await.unwrap();
    assert_eq!(view.result.n_nodes(), 2);

    // What does not fit says what to write.
    for (sql, clauses, says) in [
        (
            hop,
            "AS hops(seed, id, dist)",
            "AS hops(seed, id, dist): the result has no column dist. The result's columns are \
             seed (",
        ),
        (
            "SELECT * FROM pagerank(karate)",
            "AS edges(id, pagerank)",
            "column pagerank is a number that is not whole (Float64), and an edge's end is an id",
        ),
        (
            route,
            "AS path(steps)",
            "and a path's nodes are a list of ids (integers, strings or dates), as vertices(p) \
             gives",
        ),
        (
            "SELECT * FROM pagerank(karate)",
            "LAYOUT rings",
            "LAYOUT rings places each node by its distance from a seed or its step along a path, \
             and nodes have neither",
        ),
        (
            hop,
            "ON lesmis",
            "ON lesmis: there is no property graph named 'lesmis'. The graphs are karate",
        ),
    ] {
        let err = drawn(&ctx, sql, clauses).await.unwrap_err();
        assert!(err.contains(says), "{clauses}: {err}");
    }
}
