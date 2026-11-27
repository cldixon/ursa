//! The session's history (URSA-17): the `activity` event and the `history`
//! op, through [`Session::handle`].

use super::*;

const EDGES: &str = "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2), (2, 3), (3, 1), (3, 4), (4, 5)) AS t(src, dst); \
                     CREATE PROPERTY GRAPH club EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))";

/// A session past a `hello` from the console.
async fn console() -> Session {
    let mut shell = Session::new(Options::default());
    shell
        .handle(
            "hello",
            json!({"protocol": 1, "client": {"name": "console", "kind": "human"}}),
            Vec::new(),
        )
        .await
        .unwrap();
    shell
}

/// `op`, and the `activity` events it caused.
async fn activity(shell: &mut Session, op: &str, args: Value) -> Vec<Value> {
    let _ = shell.handle(op, args, Vec::new()).await;
    shell
        .take_events()
        .await
        .iter()
        .map(Event::to_json)
        .filter(|e| e["event"] == "activity")
        .collect()
}

/// `op`, which makes one entry: it.
async fn one(shell: &mut Session, op: &str, args: Value) -> Value {
    let mut events = activity(shell, op, args.clone()).await;
    assert_eq!(events.len(), 1, "{op} {args}: {events:?}");
    events.remove(0)
}

async fn history(shell: &mut Session, args: Value) -> Value {
    shell
        .handle("history", args, Vec::new())
        .await
        .unwrap()
        .result
}

#[tokio::test]
async fn a_query_is_sent_as_activity_with_its_client() {
    let mut shell = console().await;
    let sql = "SELECT * FROM (VALUES (1), (2), (3)) AS t(x)";
    let e = one(&mut shell, "query", json!({"sql": sql})).await;
    assert_eq!(e["event"], "activity");
    assert_eq!(e["id"], 1);
    assert_eq!(e["client"], json!({"name": "console", "kind": "human"}));
    assert_eq!(e["op"], "query");
    assert_eq!(e["summary"], sql);
    assert_eq!(e["result"], "r1");
    assert_eq!(e["rows"], 3);
    assert!(e["ms"].as_f64().unwrap() >= 0.0, "{e}");
    // When it ran: a time this year or later, in ms since the epoch.
    assert!(e["at"].as_u64().unwrap() > 1_700_000_000_000, "{e}");
    assert!(e.get("error").is_none(), "{e}");

    // A statement with no columns keeps no result.
    let created = one(
        &mut shell,
        "query",
        json!({"sql": "CREATE VIEW v AS SELECT 1 AS x"}),
    )
    .await;
    assert_eq!(created["id"], 2);
    assert!(
        created.get("result").is_none() && created.get("rows").is_none(),
        "{created}"
    );

    // One that fails says why.
    let failed = one(&mut shell, "query", json!({"sql": "SELECT nope"})).await;
    assert!(
        failed["error"].as_str().unwrap().contains("nope"),
        "{failed}"
    );
    assert!(failed.get("result").is_none(), "{failed}");

    // Operations that run nothing make no entry.
    for (op, args) in [
        ("catalog", json!({})),
        ("complete", json!({"sql": "SEL", "cursor": 3})),
        ("highlight", json!({"sql": "SELECT 1"})),
        ("fetch", json!({"result": "r1"})),
        ("explain", json!({"sql": "SELECT 1"})),
        ("history", json!({})),
        ("memory", json!({})),
    ] {
        assert_eq!(
            activity(&mut shell, op, args).await,
            Vec::<Value>::new(),
            "{op}"
        );
    }

    // The host says who sends the next request: on a shared session, each
    // connection's client.
    shell.set_client(Some(json!({"name": "claude", "kind": "agent", "extra": 1})));
    let agent = one(&mut shell, "query", json!({"sql": "SELECT 1"})).await;
    assert_eq!(agent["client"], json!({"name": "claude", "kind": "agent"}));
    shell.set_client(None);
    let nobody = one(&mut shell, "query", json!({"sql": "SELECT 2"})).await;
    assert_eq!(nobody["client"], Value::Null);
}

#[tokio::test]
async fn activity_follows_the_catalog_event() {
    let mut shell = console().await;
    let _ = shell
        .handle(
            "query",
            json!({"sql": "CREATE TABLE t AS SELECT 1 AS x"}),
            Vec::new(),
        )
        .await;
    let events: Vec<_> = shell.take_events().await.iter().map(|e| e.name).collect();
    assert_eq!(events, ["catalog", "activity"]);
}

#[tokio::test]
async fn a_fed_statement_is_one_entry_once_it_ends() {
    let mut shell = console().await;
    let fed = |line: &str| json!({"line": line});
    // A line that does not end the statement runs nothing yet.
    assert!(activity(&mut shell, "feed", fed("SELECT 1"))
        .await
        .is_empty());
    assert_eq!(
        activity(&mut shell, "feed", fed("  + 1 AS two;"))
            .await
            .len(),
        1
    );
    let entries = history(&mut shell, json!({})).await;
    assert_eq!(entries["entries"][0]["op"], "feed");
    assert_eq!(entries["entries"][0]["summary"], "SELECT 1\n  + 1 AS two;");

    // A dot-command is a statement of its own.
    let tables = one(&mut shell, "feed", fed(".tables")).await;
    assert_eq!(tables["summary"], ".tables");
    // A failed one says why, without the terminal's colours.
    let failed = one(&mut shell, "feed", fed("SELECT nope;")).await;
    let error = failed["error"].as_str().unwrap();
    assert!(
        error.contains("nope") && !error.contains('\x1b'),
        "{failed}"
    );
    // Blank lines are nothing.
    assert!(activity(&mut shell, "feed", fed("   ")).await.is_empty());
    // What cancelInput drops is not part of the next statement.
    assert!(activity(&mut shell, "feed", fed("SELECT 'dropped'"))
        .await
        .is_empty());
    assert!(activity(&mut shell, "cancelInput", json!({}))
        .await
        .is_empty());
    let next = one(&mut shell, "feed", fed("SELECT 3;")).await;
    assert_eq!(next["summary"], "SELECT 3;");
    // A host's command (.expand, .graph) is the host's: the requests it
    // sends make its entries.
    assert!(activity(&mut shell, "feed", fed(".expand 1"))
        .await
        .is_empty());
}

#[tokio::test]
async fn graph_steps_belong_to_their_drawing() {
    let mut shell = console().await;
    one(&mut shell, "query", json!({"sql": EDGES})).await;
    let hop = "SELECT * FROM hop(club, seeds => [1], direction => 'both')";
    let query = one(&mut shell, "query", json!({"sql": hop})).await;
    let result = query["result"].as_str().unwrap().to_string();

    let drawn = one(&mut shell, "graph.open", json!({"result": result})).await;
    assert_eq!(drawn["op"], "graph.open");
    assert_eq!(drawn["summary"], ".graph");
    assert_eq!(drawn["result"], result);
    assert_eq!(drawn["drawing"], 1);
    assert_eq!(
        (drawn["nodes"].as_u64(), drawn["edges"].as_u64()),
        (Some(3), Some(3))
    );

    let expanded = one(&mut shell, "graph.explore", json!({"command": ".expand 3"})).await;
    assert_eq!(expanded["summary"], ".expand 3");
    assert_eq!(expanded["drawing"], 1);
    assert_eq!(
        (expanded["nodes"].as_u64(), expanded["edges"].as_u64()),
        (Some(4), Some(4))
    );
    assert!(expanded.get("error").is_none(), "{expanded}");

    let found = one(&mut shell, "graph.find", json!({"command": ".find 4"})).await;
    assert_eq!(
        (found["op"].as_str(), found["drawing"].as_u64()),
        (Some("graph.find"), Some(1))
    );

    // A step that fails says why.
    let bad = one(
        &mut shell,
        "graph.explore",
        json!({"command": ".expand 99999"}),
    )
    .await;
    assert!(bad["error"].is_string(), "{bad}");

    // A new drawing, by its text: the steps after it are its own.
    let again = one(&mut shell, "graph.open", json!({"text": "club"})).await;
    assert_eq!(
        (again["summary"].as_str(), again["drawing"].as_u64()),
        (Some(".graph club"), Some(2))
    );
    assert!(again.get("result").is_none(), "{again}");
    let undo = one(&mut shell, "graph.explore", json!({"command": ".undo"})).await;
    assert_eq!(undo["drawing"], 2);

    // A drawing that fails to open starts none, and leaves the one open.
    let failed = one(&mut shell, "graph.open", json!({"text": "SELECT nope"})).await;
    assert!(
        failed["error"].is_string() && failed.get("drawing").is_none(),
        "{failed}"
    );

    // Once closed, a step is of no drawing.
    assert!(activity(&mut shell, "graph.close", json!({}))
        .await
        .is_empty());
    let closed = one(&mut shell, "graph.explore", json!({"command": ".undo"})).await;
    assert!(
        closed.get("drawing").is_none() && closed["error"].is_string(),
        "{closed}"
    );
}

#[tokio::test]
async fn history_lists_what_ran_oldest_first() {
    let mut shell = console().await;
    assert_eq!(
        history(&mut shell, json!({})).await,
        json!({"entries": [], "limit": HISTORY_LIMIT})
    );
    for sql in ["SELECT 1", "SELECT 2", "SELECT 3"] {
        one(&mut shell, "query", json!({"sql": sql})).await;
    }
    let all = history(&mut shell, json!({})).await;
    let summaries: Vec<_> = all["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["id"].as_u64().unwrap(),
                e["summary"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        summaries,
        [
            (1, "SELECT 1".into()),
            (2, "SELECT 2".into()),
            (3, "SELECT 3".into())
        ]
    );
    // The entries are the events, without `event`.
    assert!(all["entries"][0].get("event").is_none());
    assert_eq!(all["entries"][0]["client"]["name"], "console");
    // The last `limit`.
    let last = history(&mut shell, json!({"limit": 2})).await;
    assert_eq!(last["entries"].as_array().unwrap().len(), 2);
    assert_eq!(last["entries"][0]["id"], 2);

    // A fresh shell (`start`) keeps the session's history.
    let _ = shell.handle("start", json!({}), Vec::new()).await.unwrap();
    let _ = shell.take_events().await;
    assert_eq!(
        history(&mut shell, json!({})).await["entries"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn history_keeps_the_last_500() {
    let mut shell = console().await;
    // A step with no drawing open is quick, and still an entry.
    for _ in 0..HISTORY_LIMIT + 5 {
        let _ = shell
            .handle("graph.explore", json!({"command": ".undo"}), Vec::new())
            .await;
    }
    let events = shell.take_events().await;
    assert_eq!(events.len(), HISTORY_LIMIT + 5);
    let all = history(&mut shell, json!({})).await;
    let entries = all["entries"].as_array().unwrap();
    assert_eq!(entries.len(), HISTORY_LIMIT);
    assert_eq!(entries[0]["id"], 6);
    assert_eq!(entries.last().unwrap()["id"], HISTORY_LIMIT as u64 + 5);
}

#[tokio::test]
async fn a_cancelled_request_is_sent_as_cancelled() {
    let mut shell = console().await;
    // The host cancels a request by dropping its future, between `handle`'s
    // start and its end: what began is sent when the host takes the events.
    shell.history.begin(
        "query",
        &json!({"sql": "SELECT count(*) FROM range(0, 100000000000) a"}),
    );
    let events: Vec<_> = shell
        .take_events()
        .await
        .iter()
        .map(Event::to_json)
        .collect();
    let cancelled: Vec<_> = events.iter().filter(|e| e["event"] == "activity").collect();
    assert_eq!(cancelled.len(), 1, "{events:?}");
    assert_eq!(cancelled[0]["error"], "cancelled");
    assert!(cancelled[0]["summary"]
        .as_str()
        .unwrap()
        .starts_with("SELECT count(*)"));
    // Sent once.
    assert!(shell.take_events().await.is_empty());
}

#[tokio::test]
async fn hello_names_the_client() {
    let mut shell = Session::new(Options::default());
    // A client that is not {name, kind} is none.
    shell
        .handle("hello", json!({"protocol": 1, "client": "me"}), Vec::new())
        .await
        .unwrap();
    let e = one(&mut shell, "query", json!({"sql": "SELECT 1"})).await;
    assert_eq!(e["client"], Value::Null);
    // A request before hello is refused, and makes no entry.
    let mut early = Session::new(Options::default());
    assert!(early
        .handle("query", json!({"sql": "SELECT 1"}), Vec::new())
        .await
        .is_err());
    assert!(early.take_events().await.is_empty());
}
