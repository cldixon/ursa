//! The explore commands through the shell (#440), with no browser: a test
//! host does what the page does. It feeds each line; a line the shell hands
//! back is `.graph` (open a graph) or an explore command, whose delta goes
//! over the wire as UGR bytes to a view, here a `GraphResult` of its own.

use std::path::{Path, PathBuf};

use ursa_viz::model::{GraphResult, Key};

use super::*;

type TestShell = Shell<Vec<u8>, Vec<u8>>;

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("python/ursa/datasets/data")
        .join(file)
}

/// A host with a graph view: the shell, and what its view draws.
struct Host {
    shell: TestShell,
    view: Option<GraphResult>,
    /// Every delta the view was sent, as its bytes.
    sent: Vec<Vec<u8>>,
}

impl Host {
    /// A host whose session has karate: `members`, `ties` and the graph.
    async fn karate() -> Host {
        let mut shell = Shell::new(
            ursa_engine::new_session(),
            OutputFormat::Table,
            Vec::new(),
            Vec::new(),
        );
        shell.keep_last_result();
        shell.add_host_command(".graph", "?QUERY|NAME?", "Draw a graph");
        shell.add_explore_commands();
        let mut host = Host {
            shell,
            view: None,
            sent: Vec::new(),
        };
        let (_, err) = host
            .run(&format!(
                "CREATE VIEW members AS SELECT * FROM '{}';
                 CREATE VIEW ties AS SELECT * FROM '{}';
                 CREATE PROPERTY GRAPH karate
                   VERTEX TABLES (members KEY (id))
                   EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                                     DESTINATION KEY (dst) REFERENCES members (id));",
                data("karate_nodes.csv").display(),
                data("karate_edges.csv").display()
            ))
            .await;
        assert_eq!(err, "");
        host
    }

    /// Type `line`: what it printed to stdout and stderr.
    async fn run(&mut self, line: &str) -> (String, String) {
        if let Flow::Host(command) = self.shell.feed(line).await {
            match ExploreCommand::parse(&command) {
                Ok(None) => {
                    let text = command.trim_start_matches(".graph").trim();
                    match self.shell.open_graph(text).await {
                        Ok(_) => {
                            let bytes = self.shell.explorer().unwrap().result().to_bytes();
                            self.view = Some(GraphResult::from_bytes(&bytes).unwrap());
                        }
                        Err(e) => panic!("{line}: {}", ursa_engine::user_message(&e)),
                    }
                }
                _ => {
                    if let Some(delta) = self.shell.explore(&command).await {
                        let bytes = delta.to_bytes();
                        let read = GraphDelta::from_bytes(&bytes).unwrap();
                        assert_eq!(read, delta, "{line}: the delta's bytes read back");
                        let view = self.view.as_mut().expect("a view");
                        view.apply(&read)
                            .unwrap_or_else(|e| panic!("{line}: the view refused the delta: {e}"));
                        self.sent.push(bytes);
                    }
                }
            }
        }
        let out = String::from_utf8(std::mem::take(&mut self.shell.out)).unwrap();
        let err = String::from_utf8(std::mem::take(&mut self.shell.err)).unwrap();
        (out, err)
    }

    /// A line that prints one line and no error.
    async fn says(&mut self, line: &str) -> String {
        let (out, err) = self.run(line).await;
        assert_eq!(err, "", "{line}");
        out.trim_end().to_string()
    }

    fn session(&self) -> &ExploreSession {
        self.shell.explorer().expect("an open graph")
    }

    /// The keys of the nodes shown, sorted.
    fn nodes(&self) -> Vec<i64> {
        shown_nodes(self.session().result())
    }

    /// The edges shown as (src, dst) keys, sorted.
    fn edges(&self) -> Vec<(i64, i64)> {
        shown_edges(self.session().result())
    }

    /// Karate's ties with both ends in `nodes`, read from the edge table
    /// with a filter: what should be drawn among them.
    async fn ties_among(&mut self, nodes: &[i64]) -> Vec<(i64, i64)> {
        let list: Vec<String> = nodes.iter().map(i64::to_string).collect();
        let list = list.join(", ");
        let rows = self
            .shell
            .rows(&format!(
                "SELECT src, dst FROM ties WHERE src IN ({list}) AND dst IN ({list}) \
                 ORDER BY src, dst"
            ))
            .await
            .unwrap();
        rows.iter()
            .map(|r| (r[0].parse().unwrap(), r[1].parse().unwrap()))
            .collect()
    }

    /// The session and the view agree, and what is shown is `nodes` and
    /// karate's ties among them.
    async fn shows(&mut self, nodes: &[i64]) {
        assert_eq!(self.nodes(), nodes);
        let expected = self.ties_among(nodes).await;
        assert_eq!(self.edges(), expected);
        let view = self.view.as_ref().expect("a view");
        assert_eq!(view, self.session().result(), "the view is the session's");
        assert_eq!(view.to_bytes(), self.session().result().to_bytes());
    }
}

fn int(result: &GraphResult, node: u32) -> i64 {
    match result.node_key(node as usize) {
        Key::Int(v) => v,
        other => panic!("{other:?}"),
    }
}

fn shown_nodes(result: &GraphResult) -> Vec<i64> {
    let mut out: Vec<i64> = (0..result.n_nodes() as u32)
        .filter(|&i| !result.node_is_removed(i as usize))
        .map(|i| int(result, i))
        .collect();
    out.sort_unstable();
    out
}

fn shown_edges(result: &GraphResult) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = (0..result.n_edges())
        .filter(|&i| result.edge_is_shown(i))
        .map(|i| (int(result, result.src()[i]), int(result, result.dst()[i])))
        .collect();
    out.sort_unstable();
    out
}

/// Node 0 and its 16 neighbours: what `hop` from 0 draws.
const AROUND_0: [i64; 17] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12, 13, 17, 19, 21, 31];

fn with(base: &[i64], more: &[i64]) -> Vec<i64> {
    let mut out: Vec<i64> = base.iter().chain(more).copied().collect();
    out.sort_unstable();
    out
}

fn without(base: &[i64], less: &[i64]) -> Vec<i64> {
    base.iter().copied().filter(|n| !less.contains(n)).collect()
}

/// The acceptance script of #440: `.graph` on a hop result, three
/// expansions, a hide and two undos. Each step gives what it printed, the
/// nodes shown and the delta's bytes.
const SCRIPT: [&str; 6] = [
    ".expand 2 LIMIT 3",
    ".expand 32",
    ".expand 5",
    ".hide 33 27",
    ".undo",
    ".undo",
];

async fn play(host: &mut Host) -> Vec<(String, Vec<i64>, Vec<(i64, i64)>)> {
    host.says("SELECT * FROM hop(karate, seeds => [0], n => 1, direction => 'both');")
        .await;
    host.says(".graph").await;
    let mut steps = Vec::new();
    for line in SCRIPT {
        let said = host.says(line).await;
        steps.push((said, host.nodes(), host.edges()));
    }
    steps
}

#[tokio::test]
async fn a_script_of_expansions_a_hide_and_undos() {
    let mut host = Host::karate().await;
    host.says("SELECT * FROM hop(karate, seeds => [0], n => 1, direction => 'both');")
        .await;
    assert_eq!(host.says(".graph").await, "");
    host.shows(&AROUND_0).await;
    assert_eq!(host.session().shown(), (17, 34));

    // 2's neighbours that are not shown are 32 (degree 12), 27 (4), 28 (3)
    // and 9 (2): the three of highest degree, in that order.
    assert_eq!(
        host.says(".expand 2 LIMIT 3").await,
        "+3 nodes, +6 edges (1 more neighbour of 2 is not shown: .expand 2 again adds the next)"
    );
    let first = with(&AROUND_0, &[27, 28, 32]);
    host.shows(&first).await;
    let keys = host.session().result().node_keys();
    assert_eq!(
        keys.iter().skip(17).collect::<Vec<_>>(),
        ["32", "27", "28"],
        "by degree, then key"
    );

    // 32's: 33 (17), 23 (5), 29 and 30 (4), then the five of degree 2 by key.
    assert_eq!(host.says(".expand 32").await, "+9 nodes, +27 edges");
    let second = with(&first, &[14, 15, 18, 20, 22, 23, 29, 30, 33]);
    host.shows(&second).await;
    let keys = host.session().result().node_keys();
    assert_eq!(
        keys.iter().skip(20).collect::<Vec<_>>(),
        ["33", "23", "29", "30", "14", "15", "18", "20", "22"]
    );

    assert_eq!(host.says(".expand 5").await, "+1 node, +2 edges");
    let third = with(&second, &[16]);
    host.shows(&third).await;

    assert_eq!(host.says(".hide 33 27").await, "-2 nodes, -17 edges");
    host.shows(&without(&third, &[33, 27])).await;
    // Hidden nodes keep their slots: nothing is renumbered.
    assert_eq!(host.session().result().n_nodes(), 30);

    assert_eq!(
        host.says(".undo").await,
        "Undid .hide 33 27: +2 nodes, +17 edges"
    );
    host.shows(&third).await;
    assert_eq!(
        host.says(".undo").await,
        "Undid .expand 5: -1 node, -2 edges"
    );
    host.shows(&second).await;
    // The undone expansion's slots are gone.
    assert_eq!(host.session().result().n_nodes(), 29);
    assert_eq!(host.session().applied(), 2);
    assert_eq!(host.session().steps().len(), 4);

    // Redo is the step's delta again; a new step drops what was undone.
    assert_eq!(
        host.says(".redo").await,
        "Redid .expand 5: +1 node, +2 edges"
    );
    host.shows(&third).await;
    assert_eq!(
        host.says(".undo").await,
        "Undid .expand 5: -1 node, -2 edges"
    );
    assert_eq!(host.says(".hide 0").await, "-1 node, -16 edges");
    assert_eq!(host.says(".redo").await, "Nothing to redo");
    assert_eq!(host.session().steps().len(), 3);
    host.shows(&without(&second, &[0])).await;
}

#[tokio::test]
async fn a_replayed_script_gives_the_same_sets_and_the_same_bytes() {
    let mut first = Host::karate().await;
    let mut second = Host::karate().await;
    let a = play(&mut first).await;
    let b = play(&mut second).await;
    assert_eq!(a, b);
    assert_eq!(a.len(), SCRIPT.len());
    // Bit for bit: every delta sent, and the graph they leave.
    assert_eq!(first.sent, second.sent);
    assert_eq!(first.sent.len(), SCRIPT.len());
    assert_eq!(
        first.session().result().to_bytes(),
        second.session().result().to_bytes()
    );
    // The same session, played again from a new `.graph`.
    let again = play(&mut first).await;
    assert_eq!(again, a);
}

#[tokio::test]
async fn undoing_everything_restores_the_graph_bit_for_bit() {
    let mut host = Host::karate().await;
    host.says("SELECT * FROM hop(karate, seeds => [0], n => 1, direction => 'both');")
        .await;
    host.says(".graph").await;
    let base = host.session().result().to_bytes();
    for line in [".expand 2", ".hide 9 0", ".expand 32 LIMIT 2", ".show all"] {
        host.says(line).await;
    }
    for _ in 0..4 {
        host.says(".undo").await;
    }
    assert_eq!(host.says(".undo").await, "Nothing to undo");
    assert_eq!(host.session().result().to_bytes(), base);
    assert_eq!(host.view.as_ref().unwrap().to_bytes(), base);
}

#[tokio::test]
async fn hidden_nodes_come_back_with_their_edges() {
    let mut host = Host::karate().await;
    host.says(".graph SELECT * FROM hop(karate, seeds => [0], n => 1, direction => 'both')")
        .await;
    assert_eq!(host.says(".hide 2 8").await, "-2 nodes, -7 edges");
    // 32 is a neighbour of 2 and of 8, which are hidden when it arrives.
    assert_eq!(
        host.says(".expand 31 LIMIT 1").await.split(" (").next(),
        Some("+1 node, +3 edges")
    );
    assert_eq!(host.nodes(), without(&with(&AROUND_0, &[33]), &[2, 8]));
    assert_eq!(
        host.says(".expand 33 LIMIT 1").await.split(" (").next(),
        Some("+1 node, +2 edges")
    );
    // .show runs no query, and 2 and 8 have their edges to 32.
    assert_eq!(host.says(".show 2 8").await, "+2 nodes, +10 edges");
    host.shows(&with(&AROUND_0, &[32, 33])).await;
    // An expansion brings a hidden neighbour back.
    host.says(".hide 32").await;
    assert_eq!(
        host.says(".expand 2 LIMIT 1").await.split(" (").next(),
        Some("+1 node, +4 edges")
    );
    host.shows(&with(&AROUND_0, &[32, 33])).await;
    assert_eq!(
        host.says(".show all").await,
        "Nothing changed: no node is hidden"
    );
    assert_eq!(host.says(".hide 32 32").await, "-1 node, -4 edges");
    assert_eq!(
        host.says(".hide 32").await,
        "Nothing changed: 32 is already hidden"
    );
    assert_eq!(host.says(".show all").await, "+1 node, +4 edges");
}

#[tokio::test]
async fn directions_and_what_is_left() {
    let mut host = Host::karate().await;
    host.says(".graph SELECT * FROM hop(karate, seeds => [0], n => 1, direction => 'both')")
        .await;
    // Karate's ties are stored from the lower id: 31's out-edges go to 32
    // and 33, its in-edges come from 0, 24, 25 and 28.
    assert_eq!(host.says(".expand 31 out").await, "+2 nodes, +8 edges");
    assert_eq!(host.says(".expand 31 in").await, "+3 nodes, +6 edges");
    assert_eq!(
        host.says(".expand 31").await,
        "Nothing changed: every neighbour of 31 is shown already, with the edges among them"
    );
    assert_eq!(
        host.says(".expand 33 out").await,
        "Nothing changed: every out-neighbour of 33 is shown already, with the edges among them"
    );
    let all = with(&AROUND_0, &[24, 25, 28, 32, 33]);
    host.shows(&all).await;
    // Nothing that changed nothing is a step.
    assert_eq!(host.session().steps().len(), 2);
}

#[tokio::test]
async fn sql_is_printed_instead() {
    let mut host = Host::karate().await;
    host.says(".graph SELECT * FROM hop(karate, seeds => [5], n => 1, direction => 'both')")
        .await;
    let before = host.session().result().to_bytes();
    let sql = host.says(".expand 6 --sql LIMIT 2").await;
    assert_eq!(
        sql,
        "SELECT h.id, count(*) OVER () AS neighbours FROM hop(\"karate\", seeds => [6], n => 1, \
         direction => 'both') AS h JOIN degree(\"karate\", direction => 'both') AS d ON d.id = \
         h.id WHERE h.id NOT IN (5, 6, 10, 16, 0) ORDER BY d.degree DESC, h.id LIMIT 2;\n\
         SELECT src, dst, edge FROM induced_edges(\"karate\", nodes => [5, 6, 10, 16, 0, 4]);"
    );
    assert_eq!(host.session().result().to_bytes(), before);
    assert!(host.session().steps().is_empty());
    // The statements are what the step runs, and they run as typed.
    for statement in sql.split(";\n") {
        let (_, err) = host.run(&format!("{statement};")).await;
        assert_eq!(err, "", "{statement}");
    }
    host.says(".graph SELECT * FROM hop(karate, seeds => [5], n => 1, direction => 'both')")
        .await;
    assert_eq!(host.says(".expand 6 LIMIT 2").await, "+1 node, +3 edges");
    let step = &host.session().steps()[0];
    assert_eq!(step.sql.join(";\n") + ";", sql);
    assert_eq!(
        step.delta.provenance.sql.as_deref(),
        Some(sql.trim_end_matches(';'))
    );
    assert_eq!(
        host.says(".hide 4 --sql").await,
        "-- .hide runs no SQL: it changes what is shown of the graph"
    );
    assert_eq!(host.nodes(), [0, 4, 5, 6, 10, 16]);
}

#[tokio::test]
async fn history_lists_the_steps() {
    let mut host = Host::karate().await;
    host.says(".graph SELECT * FROM hop(karate, seeds => [5], n => 1, direction => 'both')")
        .await;
    assert_eq!(
        host.says(".graph history").await,
        "Explored from: SELECT * FROM hop(karate, seeds => [5], n => 1, direction => 'both')\n\
         No steps yet: .expand ID adds a node's neighbours\n\
         Shown: 5 nodes, 7 edges"
    );
    for line in [".expand 6;", ".hide 0", ".hide 4", ".undo"] {
        host.says(line).await;
    }
    assert_eq!(
        host.says(".graph history").await,
        "Explored from: SELECT * FROM hop(karate, seeds => [5], n => 1, direction => 'both')\n\
         1  .expand 6  +1 node, +3 edges\n\
         2  .hide 0    -1 node, -4 edges\n\
         3  .hide 4    -1 node, -2 edges  (undone)\n\
         Shown: 5 nodes, 6 edges (1 hidden)"
    );
}

#[tokio::test]
async fn no_open_graph_and_ids_that_are_not_nodes_say_so() {
    let mut host = Host::karate().await;
    for line in [
        ".expand 0",
        ".hide 0",
        ".show all",
        ".undo",
        ".redo",
        ".graph history",
    ] {
        let (out, err) = host.run(line).await;
        assert_eq!(out, "", "{line}");
        assert_eq!(
            err,
            "Error: no graph is open: draw one first, with .graph NAME for a property graph or \
             .graph after a query\n",
            "{line}"
        );
    }
    host.says(".graph SELECT * FROM hop(karate, seeds => [5], n => 1, direction => 'both')")
        .await;
    let base = host.session().result().to_bytes();
    for (line, want) in [
        (".expand 33", "no node '33' in the graph that is drawn"),
        (".hide 5 99", "no node '99' in the graph that is drawn"),
        (
            ".show nobody",
            "no node 'nobody' in the graph that is drawn",
        ),
        (".expand", "usage: .expand ID [in|out|both] [LIMIT n]"),
        (
            ".expand 5 6",
            "'6' is not part of .expand (one node at a time)",
        ),
        (
            ".expand 5 LIMIT 0",
            "LIMIT takes a number of neighbours from 1 to 5,000",
        ),
        (
            ".expand 5 LIMIT 5001",
            "LIMIT takes a number of neighbours from 1 to 5,000",
        ),
        (".expand 5 sideways", "'sideways' is not part of .expand"),
        (".hide", "usage: .hide ID…"),
        (".show", "usage: .show ID… or .show all"),
        (".undo 3", "usage: .undo"),
        (".graph history now", "usage: .graph history"),
    ] {
        let (out, err) = host.run(line).await;
        assert_eq!(out, "", "{line}");
        assert!(
            err.starts_with("Error: ") && err.contains(want),
            "{line}: {err}"
        );
    }
    assert_eq!(host.says(".hide 5").await, "-1 node, -4 edges");
    let (_, err) = host.run(".expand 5").await;
    assert_eq!(err, "Error: node 5 is hidden; .show 5 first\n");
    host.says(".undo").await;
    assert_eq!(host.session().result().to_bytes(), base);

    // A drawing that is of no property graph has no neighbours to add.
    host.says(".graph SELECT 1 AS src, 2 AS dst").await;
    let (_, err) = host.run(".expand 1").await;
    assert!(
        err.contains("this drawing is not of a property graph"),
        "{err}"
    );
    assert_eq!(host.says(".hide 1").await, "-1 node, -1 edge");

    // A failed .graph leaves the open graph and its steps.
    let failed = host.shell.open_graph("SELECT 'x' AS a").await;
    assert!(failed.is_err());
    assert_eq!(host.session().steps().len(), 1);
    host.shell.close_graph();
    let (_, err) = host.run(".undo").await;
    assert!(err.contains("no graph is open"), "{err}");
}

#[tokio::test]
async fn the_whole_graph_gains_nothing_and_a_path_gains_its_edges() {
    let mut host = Host::karate().await;
    // Every node and edge is drawn (with no rows known): nothing is added twice.
    host.says(".graph karate").await;
    assert_eq!(host.session().shown(), (34, 78));
    assert_eq!(
        host.says(".expand 0").await,
        "Nothing changed: every neighbour of 0 is shown already, with the edges among them"
    );
    // A path draws its own edges; an expansion adds the others among its nodes.
    host.says(
        ".graph SELECT * FROM shortest_path(karate, source => 16, target => 26, direction => 'both')",
    )
    .await;
    let path = host.nodes();
    assert_eq!(host.edges().len(), path.len() - 1);
    host.says(".expand 16").await;
    let nodes = host.nodes();
    let ties = host.ties_among(&nodes).await;
    let mut drawn = host.edges();
    // The path's edges are drawn in the path's direction.
    for e in &mut drawn {
        *e = (e.0.min(e.1), e.0.max(e.1));
    }
    drawn.sort_unstable();
    assert_eq!(drawn, ties);
}

#[tokio::test]
async fn help_lists_the_commands_and_feed_hands_them_back() {
    let mut host = Host::karate().await;
    let help = host.says(".help").await;
    for (name, args, _) in EXPLORE_COMMANDS {
        assert!(
            help.contains(&format!("{name} {args}").trim().to_string()),
            "{name}: {help}"
        );
    }
    assert_eq!(
        host.shell.feed(".expand 33 in LIMIT 5;").await,
        Flow::Host(".expand 33 in LIMIT 5".into())
    );
}

#[tokio::test]
async fn text_keys_are_quoted() {
    let mut shell = Shell::new(
        ursa_engine::new_session(),
        OutputFormat::Table,
        Vec::new(),
        Vec::new(),
    );
    shell.keep_last_result();
    shell
        .run_text(
            "CREATE TABLE knows AS SELECT * FROM (VALUES ('ann', 'bob'), ('bob', 'o''neil'), \
             ('ann', 'cy d'), ('cy d', 'o''neil')) AS t(a, b);
             CREATE PROPERTY GRAPH social EDGE TABLES (knows SOURCE KEY (a) DESTINATION KEY (b));",
        )
        .await;
    let mut host = Host {
        shell,
        view: None,
        sent: Vec::new(),
    };
    host.shell.add_host_command(".graph", "", "");
    host.shell.add_explore_commands();
    host.says(".graph SELECT * FROM hop(social, seeds => ['ann'])")
        .await;
    assert_eq!(host.says(".expand 'cy d'").await, "+1 node, +2 edges");
    let keys = host.session().result().node_keys();
    assert_eq!(
        keys.iter().collect::<Vec<_>>(),
        ["ann", "bob", "cy d", "o'neil"]
    );
    assert_eq!(
        host.says(".hide \"o'neil\" bob").await,
        "-2 nodes, -3 edges"
    );
    let sql = host.says(".expand ann --sql").await;
    assert!(
        sql.contains("seeds => ['ann']") && sql.contains("NOT IN ('ann', 'cy d')"),
        "{sql}"
    );
    assert!(
        sql.contains("nodes => ['ann', 'bob', 'cy d', 'o''neil']"),
        "{sql}"
    );
}
