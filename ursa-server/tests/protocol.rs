//! The session protocol on `ursa serve`'s WebSocket (`design/console.md`
//! §4): the shell over a socket, sessions new and shared, `cancel`, bad
//! requests, and stopping.

mod common;

use std::time::{Duration, Instant};

use common::{call, feed, next_message, receive, send, Server};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

/// A statement that runs until it is cancelled: a sum over 10^11 rows.
const FOREVER: &str = "SELECT sum(value) AS s FROM range(0, 100000000000);";

#[tokio::test(flavor = "multi_thread")]
async fn the_shell_runs_over_the_socket() {
    let server = Server::start().await;
    let (mut socket, hello) = server.session(Some("new")).await;
    assert_eq!(hello["result"]["transport"], "native", "{hello}");
    assert_eq!(hello["result"]["protocol"], 1);
    assert_eq!(hello["result"]["version"], "test");
    assert_eq!(hello["result"]["session"]["kind"], "new");
    assert!(hello["result"]["session"]["cwd"].is_string(), "{hello}");
    let capabilities = hello["result"]["capabilities"].as_array().unwrap();
    assert!(capabilities.contains(&json!("cancel")), "{hello}");
    call(&mut socket, 1, "start", json!({}), &[]).await;
    let ties = server.dir.path().join("ties.csv");
    let line = format!("CREATE VIEW ties AS SELECT * FROM '{}';", ties.display());
    let (_, err) = feed(&mut socket, 2, &line).await;
    assert_eq!(err, "");
    let (graph, parts) = call(&mut socket, 3, "graph.open", json!({"text": "ties"}), &[]).await;
    assert_eq!(graph["result"]["skipped"], 0, "{graph}");
    assert_eq!(
        graph["parts"],
        json!([{"name": "ugr", "kind": "ugr", "bytes": parts[0].len()}])
    );
    assert_eq!(&parts[0][..4], b"UGR1");
    // A request with a part: a file, uploaded.
    let (added, _) = call(
        &mut socket,
        4,
        "addFile",
        json!({"name": "e.csv"}),
        &[b"a,b\n1,2\n"],
    )
    .await;
    assert_eq!(added["result"]["url"], "memory://uploads/e.csv", "{added}");
    // An error has the protocol's kind; memory is the process's.
    let (failed, _) = call(&mut socket, 5, "export", json!({"format": "xml"}), &[]).await;
    assert_eq!(failed["ok"], false);
    assert_eq!(failed["error"]["kind"], "sql", "{failed}");
    let (memory, _) = call(&mut socket, 6, "memory", json!({}), &[]).await;
    assert_eq!(memory["result"]["kind"], "rss");
    assert!(memory["result"]["bytes"].as_u64().unwrap() > 0 || !cfg!(target_os = "linux"));
    let (unknown, _) = call(&mut socket, 7, "no.such.op", json!({}), &[]).await;
    assert_eq!(unknown["error"]["kind"], "protocol", "{unknown}");
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_comes_first_and_bad_requests_are_protocol_errors() {
    let server = Server::start().await;
    let mut socket = server.connect().await;
    let (early, _) = call(&mut socket, 1, "feed", json!({"line": "SELECT 1;"}), &[]).await;
    assert_eq!(early["error"]["kind"], "protocol", "{early}");
    let (early, _) = call(&mut socket, 1, "memory", json!({}), &[]).await;
    assert_eq!(early["error"]["kind"], "protocol", "{early}");
    let (wrong, _) = call(
        &mut socket,
        2,
        "hello",
        json!({"protocol": 1, "session": "mine"}),
        &[],
    )
    .await;
    assert_eq!(wrong["error"]["kind"], "protocol", "{wrong}");
    let (newer, _) = call(&mut socket, 3, "hello", json!({"protocol": 2}), &[]).await;
    assert_eq!(newer["error"]["kind"], "protocol", "{newer}");
    let (hello, _) = call(&mut socket, 3, "hello", json!({"protocol": 1}), &[]).await;
    assert_eq!(hello["ok"], true, "{hello}");
    // Another version, no op, args that are not an object, a part's length
    // that is not what was listed: each refused, and the connection goes on.
    let bad = [
        json!({"v": 2, "id": 4, "op": "feed"}),
        json!({"v": 1, "id": 5}),
        json!({"v": 1, "id": 6, "op": "feed", "args": [1]}),
    ];
    for (request, id) in bad.iter().zip(4..) {
        socket
            .send(Message::text(request.to_string()))
            .await
            .unwrap();
        let (refused, _) = receive(&mut socket).await;
        assert_eq!(refused["id"], id);
        assert_eq!(refused["error"]["kind"], "protocol", "{refused}");
    }
    let request = json!({"v": 1, "id": 7, "op": "addFile", "args": {"name": "x"},
                         "parts": [{"kind": "bytes", "name": "file", "bytes": 99}]});
    socket
        .send(Message::text(request.to_string()))
        .await
        .unwrap();
    let mut frame = 7u32.to_le_bytes().to_vec();
    frame.extend_from_slice(&[0, 0, 0, 0, b'x']);
    socket.send(Message::binary(frame)).await.unwrap();
    let (refused, _) = receive(&mut socket).await;
    assert_eq!(refused["error"]["kind"], "protocol", "{refused}");
    // A second hello for the other kind of session is refused.
    let (other, _) = call(
        &mut socket,
        8,
        "hello",
        json!({"protocol": 1, "session": "new"}),
        &[],
    )
    .await;
    assert_eq!(other["error"]["kind"], "protocol", "{other}");
    let (_, err) = feed(&mut socket, 9, "SELECT 41 + 1 AS answer;").await;
    assert_eq!(err, "");
    // A part of another request: the framing is lost, and the connection
    // closes after saying so.
    let request = json!({"v": 1, "id": 10, "op": "addFile", "args": {"name": "x"},
                         "parts": [{"kind": "bytes", "name": "file", "bytes": 1}]});
    socket
        .send(Message::text(request.to_string()))
        .await
        .unwrap();
    let mut frame = 11u32.to_le_bytes().to_vec();
    frame.extend_from_slice(&[0, 0, 0, 0, b'x']);
    socket.send(Message::binary(frame)).await.unwrap();
    let (lost, _) = receive(&mut socket).await;
    assert_eq!(lost["error"]["kind"], "protocol", "{lost}");
    match socket.next().await {
        Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 1002),
        other => panic!("not closed: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_tab_has_a_session_or_shares_one() {
    let server = Server::start().await;
    let (mut a, hello_a) = server.session(Some("new")).await;
    let (mut b, hello_b) = server.session(Some("new")).await;
    let (mut c, hello_c) = server.session(None).await;
    let (mut d, hello_d) = server.session(Some("shared")).await;
    let id = |hello: &Value| {
        hello["result"]["session"]["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_ne!(id(&hello_a), id(&hello_b));
    assert_eq!(id(&hello_c), id(&hello_d));
    assert_ne!(id(&hello_a), id(&hello_c));
    assert_eq!(hello_c["result"]["session"]["kind"], "shared");
    // A view made in one new session is not in another; in the shared one,
    // every connection sees it.
    feed(&mut a, 1, "CREATE VIEW mine AS SELECT 1 AS x;").await;
    let (_, err) = feed(&mut b, 1, "SELECT * FROM mine;").await;
    assert!(err.contains("mine"), "{err}");
    feed(&mut c, 1, "CREATE VIEW ours AS SELECT 2 AS x;").await;
    let (out, err) = feed(&mut d, 1, "SELECT x FROM ours;").await;
    assert_eq!(err, "");
    assert!(out.contains('2'), "{out}");
    // The shared session outlives a connection.
    drop(c);
    drop(d);
    let (mut e, hello_e) = server.session(Some("shared")).await;
    assert_eq!(id(&hello_e), id(&hello_c));
    let (_, err) = feed(&mut e, 1, "SELECT x FROM ours;").await;
    assert_eq!(err, "");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_a_running_request() {
    let server = Server::start().await;
    let (mut socket, _) = server.session(Some("new")).await;
    send(&mut socket, 1, "feed", json!({"line": FOREVER}), &[]).await;
    // Queued behind it: cancelled before it runs.
    send(&mut socket, 2, "feed", json!({"line": "SELECT 1;"}), &[]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    // cancel bypasses the queue: its reply comes first.
    let (cancelled, _) = call(&mut socket, 3, "cancel", json!({"target": 2}), &[]).await;
    assert_eq!(cancelled["ok"], true, "{cancelled}");
    let (cancelled, _) = call(&mut socket, 4, "cancel", json!({"target": 1}), &[]).await;
    assert_eq!(cancelled["ok"], true, "{cancelled}");
    let (first, _) = receive(&mut socket).await;
    assert_eq!(first["id"], 1);
    assert_eq!(first["error"]["kind"], "cancelled", "{first}");
    let (second, _) = receive(&mut socket).await;
    assert_eq!(second["id"], 2);
    assert_eq!(second["error"]["kind"], "cancelled", "{second}");
    assert!(started.elapsed() < Duration::from_secs(10));
    // A request that has replied, or never was: not an error.
    let (late, _) = call(&mut socket, 5, "cancel", json!({"target": 1}), &[]).await;
    assert_eq!(late["ok"], true, "{late}");
    let (none, _) = call(&mut socket, 6, "cancel", json!({}), &[]).await;
    assert_eq!(none["error"]["kind"], "protocol", "{none}");
    // The session goes on, with nothing left of the cancelled statement.
    let (out, err) = feed(&mut socket, 7, "SELECT 41 + 1 AS answer;").await;
    assert_eq!(err, "");
    assert!(out.contains("42"), "{out}");
    let (memory, _) = call(&mut socket, 8, "memory", json!({}), &[]).await;
    assert_eq!(memory["ok"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_tab_stops_its_request() {
    // A shared session: another connection's request runs once the closed
    // one's is cancelled.
    let server = Server::start().await;
    let (mut a, _) = server.session(Some("shared")).await;
    send(&mut a, 1, "feed", json!({"line": FOREVER}), &[]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (mut b, _) = server.session(Some("shared")).await;
    send(
        &mut b,
        1,
        "feed",
        json!({"line": "SELECT 41 + 1 AS answer;"}),
        &[],
    )
    .await;
    drop(a);
    let reply = tokio::time::timeout(Duration::from_secs(10), receive(&mut b))
        .await
        .expect("the closed tab's request still runs");
    assert!(reply.0["result"]["out"].as_str().unwrap().contains("42"));
}

#[tokio::test(flavor = "multi_thread")]
async fn it_stops_gracefully() {
    let mut server = Server::start().await;
    let (mut socket, _) = server.session(Some("new")).await;
    send(&mut socket, 1, "feed", json!({"line": FOREVER}), &[]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    server.stop();
    // The page hears why, and the server returns.
    loop {
        match socket.next().await {
            Some(Ok(Message::Close(Some(frame)))) => {
                assert_eq!(u16::from(frame.code), 1001);
                assert_eq!(frame.reason.as_str(), "the server stopped");
                break;
            }
            Some(Ok(Message::Text(_))) => continue,
            other => panic!("not closed: {other:?}"),
        }
    }
    let served = server.served.take().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), served).await;
    assert!(result.expect("still serving").unwrap().is_ok());
    assert!(started.elapsed() < ursa_server::SHUTDOWN_GRACE);
    // Nothing listens.
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", server.port))
        .await
        .is_err());
}

/// The next message that is not an `activity` event (which
/// [`every_connection_on_a_session_hears_its_activity`] checks).
async fn next_but_activity(socket: &mut common::Socket) -> Value {
    loop {
        let message = next_message(socket).await;
        if message["event"] != "activity" {
            return message;
        }
    }
}

/// The `catalog` event goes to every connection on the session, before the
/// reply to the request that caused it (§4.4, §4.5).
#[tokio::test(flavor = "multi_thread")]
async fn every_connection_on_a_session_hears_its_events() {
    let server = Server::start().await;
    let (mut a, _) = server.session(Some("shared")).await;
    let (mut b, _) = server.session(Some("shared")).await;
    let (mut other, _) = server.session(Some("new")).await;
    send(
        &mut a,
        1,
        "query",
        json!({"sql": "CREATE VIEW v AS SELECT 1 AS x"}),
        &[],
    )
    .await;
    assert_eq!(next_but_activity(&mut a).await, json!({"event": "catalog"}));
    let reply = next_but_activity(&mut a).await;
    assert_eq!(
        (reply["id"].clone(), reply["ok"].clone()),
        (json!(1), json!(true)),
        "{reply}"
    );
    assert_eq!(next_but_activity(&mut b).await, json!({"event": "catalog"}));
    // A query that changes nothing sends none: b's next message is its reply.
    send(&mut b, 1, "feed", json!({"line": "SELECT * FROM v;"}), &[]).await;
    assert_eq!(next_but_activity(&mut b).await["id"], 1);
    // Nor does another session hear this one's.
    send(&mut other, 1, "feed", json!({"line": "SELECT 1;"}), &[]).await;
    assert_eq!(next_but_activity(&mut other).await["id"], 1);
    // A connection that closed is not written to; the rest still are.
    drop(a);
    send(&mut b, 2, "feed", json!({"line": "DROP VIEW v;"}), &[]).await;
    assert_eq!(next_but_activity(&mut b).await, json!({"event": "catalog"}));
    assert_eq!(next_but_activity(&mut b).await["id"], 2);
}

/// The `activity` event (URSA-17): every connection on the shared session
/// hears what each ran, named by the `client` its own `hello` gave, before
/// the reply; a connection that joins later reads it with `history`.
#[tokio::test(flavor = "multi_thread")]
async fn every_connection_on_a_session_hears_its_activity() {
    let server = Server::start().await;
    let hello = |name: &str, kind: &str| json!({"protocol": 1, "session": "shared", "client": {"name": name, "kind": kind}});
    let mut console = server.connect().await;
    call(&mut console, 0, "hello", hello("console", "human"), &[]).await;
    let mut agent = server.connect().await;
    call(&mut agent, 0, "hello", hello("claude", "agent"), &[]).await;
    let mut other = server.session(Some("new")).await.0;

    send(
        &mut console,
        1,
        "query",
        json!({"sql": "SELECT 42 AS answer"}),
        &[],
    )
    .await;
    let event = next_message(&mut console).await;
    assert_eq!(event["event"], "activity", "{event}");
    assert_eq!(event["client"], json!({"name": "console", "kind": "human"}));
    assert_eq!(
        (
            event["op"].as_str(),
            event["summary"].as_str(),
            event["rows"].as_u64()
        ),
        (Some("query"), Some("SELECT 42 AS answer"), Some(1)),
        "{event}"
    );
    // Then the reply, with its rows.
    assert_eq!(receive(&mut console).await.0["id"], 1);
    // The second connection hears it too.
    assert_eq!(next_message(&mut agent).await, event);

    // The agent's request is the agent's, though the session is shared.
    send(&mut agent, 1, "query", json!({"sql": "SELECT nope"}), &[]).await;
    let failed = next_message(&mut console).await;
    assert_eq!(failed["client"], json!({"name": "claude", "kind": "agent"}));
    assert!(
        failed["error"].as_str().unwrap().contains("nope"),
        "{failed}"
    );
    assert_eq!(next_message(&mut agent).await, failed);
    assert_eq!(receive(&mut agent).await.0["id"], 1);

    // A session of its own hears none of it.
    let (listed, _) = call(&mut other, 1, "history", json!({}), &[]).await;
    assert_eq!(listed["result"]["entries"], json!([]), "{listed}");

    // A page that joins later reads what came before.
    let (mut late, _) = server.session(Some("shared")).await;
    let (listed, _) = call(&mut late, 1, "history", json!({}), &[]).await;
    let entries = listed["result"]["entries"].as_array().unwrap();
    let who: Vec<_> = entries
        .iter()
        .map(|e| {
            (
                e["id"].as_u64().unwrap(),
                e["client"]["name"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(who, [(1, "console"), (2, "claude")], "{listed}");
    let mut first = event.clone();
    first.as_object_mut().unwrap().remove("event");
    assert_eq!(entries[0], first);
}
