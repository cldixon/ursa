//! `.find` through the shell (#445), with no browser: a test host feeds each
//! line, opens a graph for `.graph`, and runs `.find` as the page does,
//! keeping what it found (what the page's view would select).

use std::path::{Path, PathBuf};

use ursa_engine::graph::view::find::{Found, NotShown, MAX_LISTED, MAX_SELECTED};

use super::*;

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("python/ursa/datasets/data")
        .join(file)
}

struct Host {
    shell: Shell<Vec<u8>, Vec<u8>>,
    found: Option<Found>,
}

impl Host {
    async fn new(setup: &str) -> Host {
        let mut shell = Shell::new(
            ursa_engine::new_session(),
            OutputFormat::Table,
            Vec::new(),
            Vec::new(),
        );
        shell.keep_last_result();
        shell.add_host_command(".graph", "?QUERY|NAME?", "Draw a graph");
        shell.add_explore_commands();
        let mut host = Host { shell, found: None };
        let (_, err) = host.run(setup).await;
        assert_eq!(err, "");
        host
    }

    /// A host whose session has karate: `members`, `ties` and the graph.
    async fn karate() -> Host {
        Host::new(&format!(
            "CREATE VIEW members AS SELECT * FROM '{}';
             CREATE VIEW ties AS SELECT * FROM '{}';
             CREATE PROPERTY GRAPH karate
               VERTEX TABLES (members KEY (id))
               EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                                 DESTINATION KEY (dst) REFERENCES members (id));",
            data("karate_nodes.csv").display(),
            data("karate_edges.csv").display()
        ))
        .await
    }

    /// Type `line`: what it printed to stdout and stderr.
    async fn run(&mut self, line: &str) -> (String, String) {
        if let Flow::Host(command) = self.shell.feed(line).await {
            if command.starts_with(".find") {
                self.found = self.shell.find(&command).await;
            } else if command.starts_with(".graph") && !command.contains("history") {
                let text = command.trim_start_matches(".graph").trim();
                if let Err(e) = self.shell.open_graph(text).await {
                    panic!("{line}: {}", ursa_engine::user_message(&e));
                }
            } else {
                self.shell.explore(&command).await;
            }
        }
        let out = String::from_utf8(std::mem::take(&mut self.shell.out)).unwrap();
        let err = String::from_utf8(std::mem::take(&mut self.shell.err)).unwrap();
        (out, err)
    }

    /// A line that prints no error: what it printed.
    async fn says(&mut self, line: &str) -> String {
        let (out, err) = self.run(line).await;
        assert_eq!(err, "", "{line}");
        out.trim_end().to_string()
    }

    /// The keys `.find` selected, in its order.
    fn selected(&self) -> Vec<String> {
        let found = self.found.as_ref().expect(".find found something");
        let keys = self.shell.explorer().unwrap().result().node_keys();
        found
            .selected
            .iter()
            .map(|&i| keys.get(i as usize).to_string())
            .collect()
    }

    fn found(&self) -> &Found {
        self.found.as_ref().expect(".find found something")
    }
}

/// The issue's acceptance, on the engine's side: `/33` is `.find 33`.
#[tokio::test]
async fn find_selects_by_key_then_label_and_by_a_condition() {
    let mut host = Host::karate().await;
    host.says(".graph karate").await;

    assert_eq!(host.says(".find 33").await, "Selected 1 node");
    assert_eq!(host.selected(), ["33"]);
    assert_eq!(host.found().matches, 1);

    // The key itself first, then the keys it starts.
    assert_eq!(host.says(".find 3;").await, "Selected 5 nodes");
    assert_eq!(host.selected(), ["3", "30", "31", "32", "33"]);

    // The label column, club, whatever the letters' case.
    let officers: Vec<String> = host
        .shell
        .rows("SELECT id FROM members WHERE club = 'Officer' ORDER BY id")
        .await
        .unwrap()
        .into_iter()
        .map(|row| row[0].clone())
        .collect();
    assert_eq!(officers.len(), 17);
    assert_eq!(host.says(".find officer").await, "Selected 17 nodes");
    assert_eq!(host.selected(), officers);

    assert_eq!(
        host.says(".find WHERE club = 'Officer'").await,
        "Selected 17 nodes"
    );
    assert_eq!(host.selected(), officers);
    assert!(host.found().not_shown.is_empty());

    // Any condition on the vertex table's columns.
    host.says(".find WHERE club = 'Mr. Hi' AND id > 10").await;
    assert_eq!(host.selected(), ["11", "12", "13", "16", "17", "19", "21"]);

    assert_eq!(host.says(".find zzz").await, "No node matches 'zzz'");
    assert!(host.found().selected.is_empty());

    // Nothing changed: .find is no step.
    let history = host.says(".graph history").await;
    assert!(history.contains("No steps yet"), "{history}");
}

#[tokio::test]
async fn matches_outside_the_view_are_named_not_selected() {
    let mut host = Host::karate().await;
    host.says("SELECT * FROM hop(karate, seeds => [16], steps => 1, direction => 'both');")
        .await;
    host.says(".graph").await;

    // 16 is drawn; 1 and 10 to 19 match too, and all but 16 are not drawn.
    assert_eq!(
        host.says(".find 1").await,
        "Selected 1 node\nNot shown: 1, 10, 11, 12, 13, 14, 15, 17, 18, 19"
    );
    assert_eq!(host.selected(), ["16"]);
    assert_eq!(host.found().not_shown_total, 10);
    assert_eq!(
        host.found().not_shown[0],
        NotShown {
            key: "1".into(),
            hidden: false
        }
    );

    assert_eq!(
        host.says(".find 3").await,
        "No node in the view matches '3'\nNot shown: 3, 30, 31, 32, 33"
    );
    assert!(host.found().selected.is_empty());

    // A hidden node is not shown either, and says how to bring it back.
    host.says(".hide 5").await;
    assert_eq!(
        host.says(".find WHERE id IN (5, 6, 7)").await,
        "Selected 1 node\nNot shown: 5 (hidden), 7\n.show ID shows a hidden node again"
    );
    assert_eq!(host.selected(), ["6"]);
    assert_eq!(
        host.found().not_shown,
        [
            NotShown {
                key: "5".into(),
                hidden: true
            },
            NotShown {
                key: "7".into(),
                hidden: false
            }
        ]
    );

    // More than are named: counted.
    let all = host.says(".find WHERE true").await;
    assert!(
        all.starts_with("Selected 2 nodes\nNot shown: 0, 1, 2, 3, 4, 5 (hidden), 7, ")
            && all.contains(" and 12 more"),
        "{all}"
    );
    assert_eq!(host.found().not_shown.len(), MAX_LISTED);
    assert_eq!(host.found().not_shown_total, 32);
}

#[tokio::test]
async fn find_prints_its_sql_and_its_errors() {
    let mut host = Host::karate().await;
    let (out, err) = host.run(".find 33").await;
    assert!(out.is_empty() && err.contains("no graph is open"), "{err}");
    assert!(host.found.is_none());

    host.says(".graph karate").await;
    let sql = host.says(".find WHERE club = 'Officer' --sql").await;
    assert_eq!(
        sql,
        "SELECT CAST(\"id\" AS VARCHAR) AS node FROM members WHERE (club = 'Officer') ORDER BY \"id\";"
    );
    assert!(host.found().selected.is_empty());
    let sql = host.says(".find O'Neil --sql").await;
    assert!(
        sql.contains("starts_with(lower(CAST(\"id\" AS VARCHAR)), 'o''neil')")
            && sql.contains("strpos(lower(\"club\"), 'o''neil') > 0"),
        "{sql}"
    );
    // The statement is what ran.
    host.says(".find WHERE club = 'Officer'").await;
    assert_eq!(
        host.found().sql,
        ["SELECT CAST(\"id\" AS VARCHAR) AS node FROM members WHERE (club = 'Officer') ORDER BY \"id\""]
    );

    let (_, err) = host.run(".find WHERE team = 1").await;
    assert!(err.contains("team"), "{err}");
    let (_, err) = host.run(".find").await;
    assert!(
        err.contains("usage: .find TEXT or .find WHERE condition"),
        "{err}"
    );

    let help = host.says(".help").await;
    assert!(help.contains(".find TEXT|WHERE condition"), "{help}");
    assert_eq!(
        host.shell.feed(".find WHERE club = 'Officer';").await,
        Flow::Host(".find WHERE club = 'Officer'".into())
    );
}

/// Text keys, a graph with no vertex table, and a drawing of no graph.
#[tokio::test]
async fn find_matches_keys_where_there_is_no_vertex_table() {
    let mut host = Host::new(
        "CREATE TABLE knows AS SELECT * FROM (VALUES ('Ann', 'bob'), ('bob', 'o''neil'), \
         ('Ann', 'anna'), ('anna', 'o''neil')) AS t(a, b);
         CREATE PROPERTY GRAPH social EDGE TABLES (knows SOURCE KEY (a) DESTINATION KEY (b));",
    )
    .await;
    // The whole graph: its keys are in the session, and no SQL runs.
    host.says(".graph social").await;
    assert_eq!(host.says(".find an").await, "Selected 2 nodes");
    assert_eq!(host.selected(), ["Ann", "anna"]);
    assert!(host.found().sql.is_empty());
    assert_eq!(host.says(".find ANNA").await, "Selected 1 node");
    assert_eq!(host.says(".find \"o'n\"").await, "Selected 1 node");
    assert_eq!(host.selected(), ["o'neil"]);
    // The graph's nodes table stands in for a vertex table.
    assert_eq!(host.says(".find WHERE id < 'b'").await, "Selected 2 nodes");
    assert_eq!(host.selected(), ["Ann", "anna"]);

    // Part of it: the rest of the graph is asked for.
    host.says(".graph SELECT * FROM hop(social, seeds => ['bob'], steps => 1)")
        .await;
    assert_eq!(
        host.says(".find an").await,
        "No node in the view matches 'an'\nNot shown: Ann, anna"
    );
    assert_eq!(host.found().sql.len(), 1);

    // Rows that are no graph's: the keys drawn, and no condition.
    host.says(".graph SELECT * FROM (VALUES (1, 12), (12, 2)) AS t(src, dst)")
        .await;
    assert_eq!(host.says(".find 1").await, "Selected 2 nodes");
    assert_eq!(host.selected(), ["1", "12"]);
    let (_, err) = host.run(".find WHERE id = 1").await;
    assert!(err.contains("not of a property graph"), "{err}");
}

#[tokio::test]
async fn find_selects_a_thousand_and_says_how_many_matched() {
    let mut host = Host::new(
        "CREATE TABLE ring AS SELECT i AS src, (i + 1) % 2500 AS dst FROM range(0, 2500) AS t(i);
         CREATE PROPERTY GRAPH big EDGE TABLES (ring SOURCE KEY (src) DESTINATION KEY (dst));",
    )
    .await;
    host.says(".graph big").await;
    assert_eq!(
        host.says(".find WHERE id >= 500").await,
        "Selected the first 1,000 of 2,000 nodes that match id >= 500"
    );
    assert_eq!(host.found().selected.len(), MAX_SELECTED);
    assert_eq!(host.found().matches, 2000);
    // Keys 1, 10 to 19, 100 to 199 and 1000 to 1999.
    assert_eq!(
        host.says(".find 1").await,
        "Selected the first 1,000 of 1,111 nodes that match '1'"
    );
    assert_eq!(host.selected()[..3], ["1", "10", "11"]);
}
