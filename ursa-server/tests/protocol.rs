//! The session protocol on `ursa serve`'s WebSocket (`design/console.md`
//! §4): the shell over a socket, sessions new and shared, `cancel`, bad
//! requests, and stopping.

mod common;

use std::time::{Duration, Instant};

use common::{call, feed, receive, send, Server};
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
