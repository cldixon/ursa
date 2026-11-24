//! `GROUP BY` (#499): a drawing aggregated by a column, and a group opened
//! in place. The grouped result is checked against SQL's own `GROUP BY`
//! over the same tables: the groups and their sizes, the edges inside each
//! and the edges between each two.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use datafusion::arrow::array::{Array, AsArray};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::{DataType, Int64Type};
use datafusion::common::TableReference;
use datafusion::prelude::SessionContext;
use ursa_engine::graph::view::groups::{
    disc_layout, COUNT, DISC_FILL, GROUP, INSIDE, MAX_GROUPS, MEMBERS, X, Y,
};
use ursa_engine::graph::view::{of_command, parse, DataFusionSql, GraphView, RunSql, Session};
use ursa_engine::graph::{create_graph, EdgeTable, GraphDef, VertexTable};
use ursa_engine::{new_session, user_message};
use ursa_viz::model::{GraphDelta, GraphResult, Key, Op, Values, NULL};

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../python/ursa/datasets/data")
        .join(file)
}

async fn run(ctx: &SessionContext, sql: &str) {
    ctx.sql(sql).await.unwrap().collect().await.unwrap();
}

/// A session with karate as a property graph.
async fn karate() -> SessionContext {
    let ctx = new_session();
    for (name, file) in [
        ("members", "karate_nodes.csv"),
        ("ties", "karate_edges.csv"),
    ] {
        run(
            &ctx,
            &format!(
                "CREATE VIEW {name} AS SELECT * FROM '{}'",
                data(file).display()
            ),
        )
        .await;
    }
    let def = GraphDef {
        name: "karate".into(),
        edges: EdgeTable::new(TableReference::bare("ties"), "src", "dst"),
        vertices: Some(VertexTable::new(TableReference::bare("members"), "id")),
    };
    create_graph(&ctx, def, false, false).await.unwrap();
    ctx
}

async fn draw(ctx: &SessionContext, text: &str) -> GraphView {
    of_command(ctx, &DataFusionSql, text, None)
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"))
}

async fn refusal(ctx: &SessionContext, text: &str) -> String {
    let err = of_command(ctx, &DataFusionSql, text, None)
        .await
        .expect_err(text);
    user_message(&err)
}

/// A query's rows as text and a count: `(a, n)` or `(a, b, n)` rows.
async fn counted(ctx: &SessionContext, sql: &str) -> Vec<(Vec<String>, u32)> {
    let (_, batches) = DataFusionSql.run(ctx, sql).await.unwrap();
    let mut out = Vec::new();
    for b in &batches {
        let last = b.num_columns() - 1;
        let texts: Vec<_> = (0..last)
            .map(|c| cast(b.column(c), &DataType::Utf8).unwrap())
            .collect();
        let n = cast(b.column(last), &DataType::Int64).unwrap();
        let n = n.as_primitive::<Int64Type>();
        for i in 0..b.num_rows() {
            let names = texts
                .iter()
                .map(|t| {
                    if t.is_null(i) {
                        "NULL".to_string()
                    } else {
                        t.as_string::<i32>().value(i).to_string()
                    }
                })
                .collect();
            out.push((names, n.value(i) as u32));
        }
    }
    out
}

fn u32s<'a>(result: &'a GraphResult, column: &str, edges: bool) -> &'a [u32] {
    let column = if edges {
        result.edge_column(column)
    } else {
        result.node_column(column)
    };
    match &column.expect("the column").values {
        Values::U32(values) => values,
        other => panic!("{other:?}"),
    }
}

/// Each group's name, by node.
fn names(result: &GraphResult) -> Vec<String> {
    let by = result.encodings().group_by.as_deref().expect("grouped");
    match &result.node_column(by).expect("group").values {
        Values::Dict { values, codes } => codes
            .iter()
            .map(|&c| values.get(c as usize).to_string())
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// The grouped `view` is what SQL's GROUP BY says: `sizes` gives `(group,
/// members)`, `inside` `(group, edges)` for the groups that have any, and
/// `between` `(group, group, edges)`, each pair once in either order.
async fn same_as_sql(
    ctx: &SessionContext,
    view: &GraphView,
    sizes: &str,
    inside: &str,
    between: &str,
) {
    let r = &view.result;
    let names = names(r);
    let members = u32s(r, MEMBERS, false);
    // The groups and their sizes, the largest first.
    let want: BTreeMap<String, u32> = counted(ctx, sizes)
        .await
        .into_iter()
        .map(|(mut n, c)| (n.remove(0), c))
        .collect();
    let got: BTreeMap<String, u32> = names.iter().cloned().zip(members.iter().copied()).collect();
    assert_eq!(got, want, "the groups and their sizes");
    assert_eq!(got.len(), names.len(), "a group has one node");
    assert!(members.windows(2).all(|w| w[0] >= w[1]), "{members:?}");
    assert_eq!(
        r.node_keys().iter().collect::<Vec<_>>(),
        (0..names.len()).map(|g| g.to_string()).collect::<Vec<_>>()
    );
    assert_eq!(r.encodings().size_by.as_deref(), Some(MEMBERS));
    assert_eq!(r.node_labels()[0].name, GROUP);
    assert!(r.node_label().iter().all(|&l| l == 0));

    // The edges inside each group.
    let mut want: BTreeMap<String, u32> = names.iter().map(|n| (n.clone(), 0)).collect();
    for (mut n, c) in counted(ctx, inside).await {
        *want.get_mut(&n.remove(0)).expect("a group") += c;
    }
    let got: BTreeMap<String, u32> = names
        .iter()
        .cloned()
        .zip(u32s(r, INSIDE, false).iter().copied())
        .collect();
    assert_eq!(got, want, "the edges inside each group");

    // The edges between each two, whichever way they point.
    let pair = |a: &str, b: &str| {
        if a <= b {
            (a.to_string(), b.to_string())
        } else {
            (b.to_string(), a.to_string())
        }
    };
    let mut want: BTreeMap<(String, String), u32> = BTreeMap::new();
    for (n, c) in counted(ctx, between).await {
        *want.entry(pair(&n[0], &n[1])).or_insert(0) += c;
    }
    let (src, dst) = (r.src(), r.dst());
    let counts = u32s(r, COUNT, true);
    let mut got: BTreeMap<(String, String), u32> = BTreeMap::new();
    for e in 0..r.n_edges() {
        assert!(src[e] < dst[e], "an edge is from the lower group");
        let had = got.insert(
            pair(&names[src[e] as usize], &names[dst[e] as usize]),
            counts[e],
        );
        assert!(had.is_none(), "two groups have one edge");
    }
    assert_eq!(got, want, "the edges between groups");
    let ends: Vec<(u32, u32)> = src.iter().copied().zip(dst.iter().copied()).collect();
    assert!(ends.windows(2).all(|w| w[0] < w[1]), "in order: {ends:?}");

    // Every node is in one group, and every edge inside one or between two.
    let groups = view.groups.as_ref().expect("the groups' members");
    assert_eq!(groups.len(), names.len());
    let mut all: Vec<u32> = (0..groups.len())
        .flat_map(|g| {
            assert_eq!(groups.members(g).len(), members[g] as usize);
            assert_eq!(groups.name(g), names[g]);
            groups.members(g).iter().copied()
        })
        .collect();
    all.sort_unstable();
    assert_eq!(all, (0..groups.n_nodes() as u32).collect::<Vec<_>>());
}

#[tokio::test]
async fn groups_are_what_sqls_group_by_says() {
    let ctx = karate().await;

    // A vertex column: the two clubs, and the ties between them.
    let view = draw(&ctx, "karate GROUP BY club").await;
    same_as_sql(
        &ctx,
        &view,
        "SELECT club, count(*) FROM members GROUP BY club",
        "SELECT a.club, count(*) FROM ties t JOIN members a ON a.id = t.src \
         JOIN members b ON b.id = t.dst WHERE a.club = b.club GROUP BY a.club",
        "SELECT a.club, b.club, count(*) FROM ties t JOIN members a ON a.id = t.src \
         JOIN members b ON b.id = t.dst WHERE a.club <> b.club GROUP BY a.club, b.club",
    )
    .await;
    let r = &view.result;
    assert_eq!((r.n_nodes(), r.n_edges()), (2, 1));
    assert_eq!(u32s(r, MEMBERS, false), [17, 17]);
    assert_eq!(u32s(r, COUNT, true).len(), 1);
    assert_eq!(
        u32s(r, COUNT, true)[0] + u32s(r, INSIDE, false).iter().sum::<u32>(),
        78,
        "every tie is inside a club or between the two"
    );
    assert_eq!(r.graph(), Some("karate"));
    assert_eq!(r.encodings().group_by.as_deref(), Some("club"));
    assert_eq!(
        view.grouped_by().as_deref(),
        Some("grouped by club: 2 groups")
    );
    assert_eq!(view.drawn_as(), "groups of karate");
    assert_eq!(view.legend(), None);

    // An algorithm: louvain's communities.
    run(
        &ctx,
        "CREATE TABLE found AS SELECT id, louvain AS community FROM louvain(karate)",
    )
    .await;
    let sizes = "SELECT community, count(*) FROM found GROUP BY community";
    let inside = "SELECT a.community, count(*) FROM ties t JOIN found a ON a.id = t.src \
                  JOIN found b ON b.id = t.dst WHERE a.community = b.community \
                  GROUP BY a.community";
    let between = "SELECT a.community, b.community, count(*) FROM ties t \
                   JOIN found a ON a.id = t.src JOIN found b ON b.id = t.dst \
                   WHERE a.community <> b.community GROUP BY a.community, b.community";
    let louvain = draw(&ctx, "karate GROUP BY louvain").await;
    same_as_sql(&ctx, &louvain, sizes, inside, between).await;
    assert!(louvain.result.n_nodes() > 2);
    // The same groups by a column of NODES (…), of a query in parentheses,
    // and of the result's own rows.
    for text in [
        "karate NODES (SELECT id, community AS c FROM found) GROUP BY c",
        "(SELECT * FROM found) GROUP BY community",
        "SELECT * FROM found AS nodes(id) ON karate GROUP BY community",
    ] {
        let other = draw(&ctx, text).await;
        same_as_sql(&ctx, &other, sizes, inside, between).await;
        assert_eq!(
            u32s(&other.result, COUNT, true),
            u32s(&louvain.result, COUNT, true),
            "{text}"
        );
        assert_eq!(other.result.src(), louvain.result.src(), "{text}");
    }
    // The same bytes every time.
    let again = draw(&ctx, "karate GROUP BY louvain").await;
    assert_eq!(again.result.to_bytes(), louvain.result.to_bytes());
    assert_eq!(again.groups, louvain.groups);

    // Rows drawn as edges, grouped by a column of NODES (…): nodes with no
    // value are a group, as SQL's NULLs are.
    run(
        &ctx,
        "CREATE TABLE some AS SELECT id, CASE WHEN id < 30 THEN club END AS club FROM members",
    )
    .await;
    let edges = draw(
        &ctx,
        "SELECT src, dst FROM ties NODES (SELECT id, club FROM some) GROUP BY club",
    )
    .await;
    same_as_sql(
        &ctx,
        &edges,
        "SELECT club, count(*) FROM some GROUP BY club",
        "SELECT a.club, count(*) FROM ties t JOIN some a ON a.id = t.src \
         JOIN some b ON b.id = t.dst WHERE a.club IS NOT DISTINCT FROM b.club GROUP BY a.club",
        "SELECT a.club, b.club, count(*) FROM ties t JOIN some a ON a.id = t.src \
         JOIN some b ON b.id = t.dst WHERE a.club IS DISTINCT FROM b.club GROUP BY a.club, b.club",
    )
    .await;
    assert!(names(&edges.result).contains(&"NULL".to_string()));
    assert_eq!(edges.result.graph(), None);
}

#[tokio::test]
async fn groups_past_the_cap_are_one() {
    let ctx = new_session();
    // 2,500 groups: group g (0..2500) has 1 + (g % 3) nodes. A ring over
    // the nodes in id order.
    run(
        &ctx,
        "CREATE TABLE v AS SELECT g * 3 + k AS id, g FROM generate_series(0, 2499) AS s(g), \
         generate_series(0, 2) AS t(k) WHERE k <= g % 3",
    )
    .await;
    run(
        &ctx,
        "CREATE TABLE e AS SELECT id AS src, lead(id, 1, 0) OVER (ORDER BY id) AS dst FROM v",
    )
    .await;
    let def = GraphDef {
        name: "ring".into(),
        edges: EdgeTable::new(TableReference::bare("e"), "src", "dst"),
        vertices: Some(VertexTable::new(TableReference::bare("v"), "id")),
    };
    create_graph(&ctx, def, false, false).await.unwrap();
    let view = draw(&ctx, "ring GROUP BY g").await;
    let r = &view.result;
    assert_eq!(r.n_nodes(), MAX_GROUPS + 1);
    let names = names(r);
    assert_eq!(names[MAX_GROUPS], "other (500 groups)");
    let members = u32s(r, MEMBERS, false);
    // 833 groups of 3 and 833 of 2 are 1,666; the other 334 kept have one
    // node, and so have the 500 folded.
    assert_eq!(members[..833], [3; 833]);
    assert_eq!(members[833..1666], [2; 833]);
    assert_eq!(members[1666..MAX_GROUPS], [1; 334]);
    assert_eq!(members[MAX_GROUPS], 500);
    assert_eq!(members.iter().sum::<u32>(), 4999);
    // Equal groups are in the order first seen: by g.
    assert_eq!(names[..3], ["2", "5", "8"]);
    let total =
        u32s(r, COUNT, true).iter().sum::<u32>() + u32s(r, INSIDE, false).iter().sum::<u32>();
    assert_eq!(total, 4999, "every edge is counted once");
    assert!(view
        .drawn_as()
        .contains("the 2,000 largest of 2,500 groups are drawn, and the other 500 as one"));
    assert_eq!(
        view.grouped_by().as_deref(),
        Some("grouped by g: 2,001 groups")
    );
    let groups = view.groups.as_ref().unwrap();
    assert_eq!(groups.members(MAX_GROUPS).len(), 500);
}

/// The members shown of a session: `(key, group, x, y)`.
fn shown_members(session: &Session) -> Vec<(String, u32, f32, f32)> {
    let r = session.result();
    let groups = session.grouping().unwrap().len();
    let by = r.encodings().group_by.as_deref().unwrap();
    let Values::Dict { codes, .. } = &r.node_column(by).unwrap().values else {
        panic!()
    };
    let (Values::F32(x), Values::F32(y)) = (
        &r.node_column(X).unwrap().values,
        &r.node_column(Y).unwrap().values,
    ) else {
        panic!()
    };
    (groups..r.n_nodes())
        .filter(|&i| !r.node_is_removed(i))
        .map(|i| (r.node_keys().get(i).to_string(), codes[i], x[i], y[i]))
        .collect()
}

fn shown_edges(session: &Session) -> usize {
    session.shown().1
}

#[tokio::test]
async fn a_group_opens_in_place_and_closes() {
    let ctx = karate().await;
    let view = draw(&ctx, "karate GROUP BY club").await;
    let base = view.result.clone();
    let names = names(&base);
    let club_of = |g: usize| names[g].clone();
    let mut sent: Vec<Vec<Vec<u8>>> = Vec::new();
    for _ in 0..2 {
        let mut session = Session::grouped(base.clone(), view.groups.clone().unwrap());
        let mut bytes = Vec::new();
        assert_eq!(session.shown(), (2, 1));

        // --sql prints the statement and changes nothing.
        let sql = session
            .run(&ctx, &DataFusionSql, ".expand group:0 --sql")
            .await
            .unwrap();
        assert!(sql.delta.is_none());
        assert!(
            sql.text
                .starts_with("SELECT src, dst, edge FROM induced_edges(\"karate\", nodes => ["),
            "{}",
            sql.text
        );

        // The first group: its 17 members, and the ties among them.
        let opened = session
            .run(&ctx, &DataFusionSql, ".expand group:0")
            .await
            .unwrap();
        let inside = u32s(&base, INSIDE, false);
        assert_eq!(opened.text, format!("+17 nodes, +{} edges", inside[0]));
        let members = shown_members(&session);
        let want: HashSet<String> = counted(
            &ctx,
            &format!("SELECT id, 1 FROM members WHERE club = '{}'", club_of(0)),
        )
        .await
        .into_iter()
        .map(|(mut n, _)| n.remove(0))
        .collect();
        assert_eq!(
            members.iter().map(|m| m.0.clone()).collect::<HashSet<_>>(),
            want
        );
        for (key, group, x, y) in &members {
            assert_eq!(*group, 0, "{key}");
            assert!(
                (x * x + y * y).sqrt() <= DISC_FILL + 1e-6,
                "{key} is in its group's disc: ({x}, {y})"
            );
        }
        // No two members at one place.
        let places: HashSet<(u32, u32)> = members
            .iter()
            .map(|m| (m.2.to_bits(), m.3.to_bits()))
            .collect();
        assert_eq!(places.len(), 17);
        // The members are of the vertex table's label, and the group's node
        // and its edge to the other group stay.
        let r = session.result();
        assert!(r.node_label()[2..].iter().all(|&l| l == 1));
        assert_eq!(r.node(0, &Key::Int(0)), Some(0));
        assert!(!r.node_is_removed(0));
        assert_eq!(shown_edges(&session), 1 + inside[0] as usize);
        let delta = opened.delta.unwrap();
        assert!(
            matches!(
                &delta.ops[..],
                [Op::AddNodes(_), Op::AddEdgeLabel(_), Op::AddEdges(_)]
                    | [Op::AddNodes(_), Op::AddEdges(_)]
            ),
            "{:?}",
            delta.ops
        );

        // Again: nothing.
        let again = session
            .run(&ctx, &DataFusionSql, ".expand group:0")
            .await
            .unwrap();
        assert!(again.delta.is_none());
        assert!(
            again.text.contains("group 0 is open already"),
            "{}",
            again.text
        );

        // The second: its members, the ties among them, and the ties
        // between the two clubs' members. Every tie is drawn now.
        let second = session
            .run(&ctx, &DataFusionSql, ".expand group:1")
            .await
            .unwrap();
        let between = u32s(&base, COUNT, true)[0];
        assert_eq!(
            second.text,
            format!("+17 nodes, +{} edges", inside[1] + between)
        );
        assert_eq!(shown_edges(&session), 1 + 78);
        assert_eq!(shown_members(&session).len(), 34);

        // Closing a group hides its members; opening it again shows them,
        // with no query, where they were.
        let before = shown_members(&session);
        let closed = session
            .run(&ctx, &DataFusionSql, ".collapse group:0")
            .await
            .unwrap();
        assert_eq!(
            closed.text,
            format!("-17 nodes, -{} edges", inside[0] + between)
        );
        assert!(shown_members(&session).iter().all(|m| m.1 == 1));
        let not_open = session
            .run(&ctx, &DataFusionSql, ".collapse group:0")
            .await
            .unwrap();
        assert!(
            not_open.text.contains("group 0 is not open"),
            "{}",
            not_open.text
        );
        let reopened = session
            .run(&ctx, &DataFusionSql, ".expand group:0")
            .await
            .unwrap();
        assert!(matches!(
            &reopened.delta.as_ref().unwrap().ops[..],
            [Op::RestoreNodes(nodes)] if nodes.len() == 17
        ));
        assert!(session.steps().last().unwrap().sql.is_empty());
        assert_eq!(shown_members(&session), before);

        // A view that has the base applies each step's bytes, and undoing
        // every step is the closed drawing again, to the byte.
        let mut seen = GraphResult::from_bytes(&base.to_bytes()).unwrap();
        for step in session.steps() {
            let wire = step.delta.to_bytes();
            seen.apply(&GraphDelta::from_bytes(&wire).unwrap()).unwrap();
            bytes.push(wire);
        }
        assert_eq!(seen, *session.result());
        while session.applied() > 0 {
            session.run(&ctx, &DataFusionSql, ".undo").await.unwrap();
        }
        assert_eq!(session.result().to_bytes(), base.to_bytes());
        assert_eq!(session.shown(), (2, 1));
        // And redone, the open one.
        session.run(&ctx, &DataFusionSql, ".redo").await.unwrap();
        assert_eq!(shown_members(&session).len(), 17);
        bytes.push(session.result().to_bytes());
        sent.push(bytes);

        // What is not a group, and what a grouped drawing does not do.
        for (line, says) in [
            (".expand 5", "takes one of its groups"),
            (".expand group:2", "its groups are group:0 to group:1"),
            (".collapse 5", "takes one of its groups"),
            (".collapse", "usage: .collapse group:N"),
        ] {
            let err = session.run(&ctx, &DataFusionSql, line).await.unwrap_err();
            assert!(user_message(&err).contains(says), "{line}: {err}");
        }
        // A group is hidden and shown as a node is, by its id.
        let hidden = session
            .run(&ctx, &DataFusionSql, ".hide group:1")
            .await
            .unwrap();
        assert_eq!(hidden.text, "-1 node, -1 edge");
        let err = session
            .run(&ctx, &DataFusionSql, ".expand group:1")
            .await
            .unwrap_err();
        assert!(user_message(&err).contains("group 1 is hidden"), "{err}");
        // A member, by its key.
        let member = shown_members(&session)[0].0.clone();
        let one = session
            .run(&ctx, &DataFusionSql, &format!(".hide {member}"))
            .await
            .unwrap();
        assert!(one.text.starts_with("-1 node"), "{}", one.text);
        // No badges: a group has no degree in the graph.
        assert!(session
            .not_shown(&ctx, &DataFusionSql)
            .await
            .unwrap()
            .is_empty());
    }
    assert_eq!(sent[0], sent[1], "the same bytes every time");

    // A drawing that is not grouped has no groups to close.
    let plain = draw(&ctx, "karate").await;
    let mut session = Session::new(plain.result);
    let err = session
        .run(&ctx, &DataFusionSql, ".collapse group:0")
        .await
        .unwrap_err();
    assert!(
        user_message(&err).contains("this one is not grouped"),
        "{err}"
    );

    // Rows that name no graph are grouped, but a group cannot be opened.
    let rows = draw(
        &ctx,
        "SELECT src, dst FROM ties NODES (SELECT id, club FROM members) GROUP BY club",
    )
    .await;
    let mut session = Session::grouped(rows.result, rows.groups.unwrap());
    let err = session
        .run(&ctx, &DataFusionSql, ".expand group:0")
        .await
        .unwrap_err();
    assert!(
        user_message(&err).contains("from a property graph"),
        "{err}"
    );
}

#[test]
fn a_groups_members_fill_its_disc() {
    // A path of 200 nodes, and 50 nodes with no edge.
    let n = 250;
    let src: Vec<u32> = (0..199).collect();
    let dst: Vec<u32> = (1..200).collect();
    let (x, y) = disc_layout(n, src.clone(), dst.clone());
    assert_eq!((x.len(), y.len()), (n, n));
    let mut far: Vec<f32> = (0..n).map(|i| (x[i] * x[i] + y[i] * y[i]).sqrt()).collect();
    far.sort_by(f32::total_cmp);
    // The k-th from the middle is DISC_FILL * sqrt((k + 0.5) / n) from it.
    for (k, d) in far.iter().enumerate() {
        let want = DISC_FILL * ((k as f32 + 0.5) / n as f32).sqrt();
        assert!((d - want).abs() < 1e-5, "{k}: {d} for {want}");
    }
    assert!(far[n - 1] < 1.0);
    // The same bits again.
    let again = disc_layout(n, src, dst);
    assert!(x
        .iter()
        .zip(&again.0)
        .all(|(a, b)| a.to_bits() == b.to_bits()));
    assert!(y
        .iter()
        .zip(&again.1)
        .all(|(a, b)| a.to_bits() == b.to_bits()));
    // Neighbours are nearer one another than two nodes are on average.
    let gap = |a: usize, b: usize| ((x[a] - x[b]).powi(2) + (y[a] - y[b]).powi(2)).sqrt();
    let tied: f32 = (0..199).map(|i| gap(i, i + 1)).sum::<f32>() / 199.0;
    let any: f32 = (0..199).map(|i| gap(i, (i * 37 + 91) % 200)).sum::<f32>() / 199.0;
    assert!(
        tied < any / 2.0,
        "{tied} between neighbours, {any} between any two"
    );
    // One node is at the middle, and none is nowhere.
    assert_eq!(disc_layout(1, vec![], vec![]), (vec![0.0], vec![0.0]));
    assert_eq!(disc_layout(0, vec![], vec![]), (vec![], vec![]));
    let _ = NULL;
}

#[tokio::test]
async fn group_by_is_a_clause_where_it_is_not_sqls() {
    let by = |text: &str| {
        let (source, style) = parse(text).unwrap();
        (source, style.group.map(|g| g.ident))
    };
    assert_eq!(
        by("karate GROUP BY club"),
        ("karate".into(), Some("club".into()))
    );
    assert_eq!(by("GROUP BY Club"), (String::new(), Some("club".into())));
    assert_eq!(
        by("karate group by \"Club\";"),
        ("karate".into(), Some("Club".into()))
    );
    assert_eq!(
        by("(SELECT * FROM louvain(karate)) GROUP BY community"),
        (
            "(SELECT * FROM louvain(karate))".into(),
            Some("community".into())
        )
    );
    assert_eq!(
        by("SELECT a, b FROM t AS edges(a, b) GROUP BY c"),
        ("SELECT a, b FROM t".into(), Some("c".into()))
    );
    assert_eq!(
        by("ON karate GROUP BY club"),
        (String::new(), Some("club".into()))
    );
    // SQL's own.
    for sql in [
        "SELECT a, count(*) FROM t GROUP BY a",
        "SELECT a, b FROM (SELECT 1) GROUP BY a, b",
        "(SELECT a FROM t) UNION (SELECT a FROM u GROUP BY a)",
    ] {
        assert_eq!(by(sql), (sql.to_string(), None), "{sql}");
    }
    let err = |text: &str| user_message(&parse(text).unwrap_err());
    assert!(err("karate GROUP BY").contains("GROUP BY needs a column"));
    assert!(err("karate GROUP BY a GROUP BY b").contains("GROUP BY is given twice"));
    assert!(err("karate GROUP BY a, b").contains("unexpected ','"));

    // What does not go with it, and what it cannot find.
    let ctx = karate().await;
    for (text, says) in [
        (
            "karate GROUP BY club SIZE BY pagerank",
            "SIZE BY pagerank is of the nodes",
        ),
        (
            "karate GROUP BY club COLOR BY club",
            "COLOR BY club is of the nodes",
        ),
        (
            "karate GROUP BY club LAYOUT force",
            "LAYOUT force is of the nodes",
        ),
        (
            "karate GROUP BY club POSITIONS (SELECT 1)",
            "POSITIONS (…) is of the nodes",
        ),
        (
            "karate GROUP BY nothing",
            "GROUP BY nothing: graph 'karate' has no vertex column",
        ),
        ("SELECT src, dst FROM ties GROUP BY louvain", ""),
    ] {
        let said = refusal(&ctx, text).await;
        assert!(said.contains(says), "{text}: {said}");
    }
}
