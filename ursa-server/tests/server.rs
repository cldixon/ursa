//! `ursa serve`'s server, driven as a browser and as an attacker would:
//! what it binds to, who may open a session (`design/console.md` §5), and
//! the session protocol on its WebSocket (§4.2).

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::PathBuf;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

/// A server on a free port, serving a directory with one CSV and a page.
async fn start() -> (u16, String, tempfile::TempDir) {
    // The session reads relative paths against the process's directory,
    // which `ursa serve` sets to the root: a test names the file in full.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ties.csv"), "src,dst\n1,2\n2,3\n3,1\n").unwrap();
    let assets = dir.path().join("dist");
    std::fs::create_dir(&assets).unwrap();
    std::fs::write(
        assets.join("index.html"),
        "<!doctype html><html><head><title>t</title></head></html>",
    )
    .unwrap();
    std::fs::write(assets.join("app.js"), "export {};").unwrap();
    let config = ursa_server::Config {
        root: dir.path().to_path_buf(),
        port: 0,
        assets: vec![assets],
        version: "test".into(),
    };
    let bound = ursa_server::bind(config).await.unwrap();
    let (port, token) = (bound.port(), bound.token().to_string());
    tokio::spawn(bound.serve());
    (port, token, dir)
}

/// A raw HTTP/1.1 GET with these headers: the status, and the whole response.
async fn get(port: u16, path: &str, headers: &[(&str, &str)]) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut request = format!("GET {path} HTTP/1.1\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response).into_owned();
    let status = response.split(' ').nth(1).unwrap().parse().unwrap();
    (status, response)
}

/// Open `/session` with these `Host`, `Origin` and token; the socket, or the
/// status it was refused with.
async fn open(
    port: u16,
    host: &str,
    origin: Option<&str>,
    token: Option<&str>,
    cookie: Option<&str>,
) -> Result<tokio_tungstenite::WebSocketStream<TcpStream>, u16> {
    let query = token.map(|t| format!("?token={t}")).unwrap_or_default();
    let mut request = format!("ws://127.0.0.1:{port}/session{query}")
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    headers.insert("host", HeaderValue::from_str(host).unwrap());
    if let Some(origin) = origin {
        headers.insert("origin", HeaderValue::from_str(origin).unwrap());
    }
    if let Some(cookie) = cookie {
        headers.insert("cookie", HeaderValue::from_str(cookie).unwrap());
    }
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    match tokio_tungstenite::client_async(request, stream).await {
        Ok((socket, _)) => Ok(socket),
        Err(WsError::Http(response)) => Err(response.status().as_u16()),
        Err(e) => panic!("{e}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn it_listens_on_loopback_only() {
    let (port, _, _dir) = start().await;
    let config = ursa_server::Config {
        root: PathBuf::from("."),
        port: 0,
        assets: vec![],
        version: "test".into(),
    };
    let bound = ursa_server::bind(config).await.unwrap();
    let addresses = bound.addresses();
    assert!(!addresses.is_empty());
    assert!(
        addresses.iter().all(|a| a.ip().is_loopback()),
        "{addresses:?}"
    );
    // This machine's address on its network, if it has one, does not reach
    // the server.
    let outward = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|_| s))
        .and_then(|s| s.local_addr());
    if let Ok(SocketAddr::V4(local)) = outward {
        let ip = IpAddr::V4(*local.ip());
        if !ip.is_loopback() && !ip.is_unspecified() {
            assert!(
                TcpStream::connect((ip, port)).await.is_err(),
                "reached on {ip}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_needs_the_token_its_own_origin_and_host() {
    let (port, token, _dir) = start().await;
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let own = Some(origin.as_str());
    let cookie = format!("ursa_token_{port}={token}");
    // No token, a wrong one (in the query or the cookie), and one a
    // character short.
    assert_eq!(open(port, &host, own, None, None).await.err(), Some(401));
    let wrong = "A".repeat(token.len());
    assert_eq!(
        open(port, &host, own, Some(&wrong), None).await.err(),
        Some(401)
    );
    let wrong_cookie = format!("ursa_token_{port}={wrong}");
    assert_eq!(
        open(port, &host, own, None, Some(&wrong_cookie))
            .await
            .err(),
        Some(401)
    );
    assert_eq!(
        open(port, &host, own, Some(&token[1..]), None).await.err(),
        Some(401)
    );
    // Another port's cookie is not this server's.
    let other = format!("ursa_token_{}={token}", port.wrapping_add(1));
    assert_eq!(
        open(port, &host, own, None, Some(&other)).await.err(),
        Some(401)
    );
    // Another site's page, even with the token; and no Origin at all.
    for foreign in [
        "http://evil.example",
        "http://127.0.0.1:1",
        "https://127.0.0.1",
        "null",
    ] {
        let refused = open(port, &host, Some(foreign), Some(&token), None)
            .await
            .err();
        assert_eq!(refused, Some(403), "{foreign}");
    }
    assert_eq!(
        open(port, &host, None, Some(&token), None).await.err(),
        Some(403)
    );
    // A name that rebinds to 127.0.0.1, with an origin to match.
    let rebound = format!("evil.example:{port}");
    let rebound_origin = format!("http://evil.example:{port}");
    let refused = open(port, &rebound, Some(&rebound_origin), Some(&token), None)
        .await
        .err();
    assert_eq!(refused, Some(403));
    assert_eq!(
        open(port, "127.0.0.1:1", own, Some(&token), None)
            .await
            .err(),
        Some(403)
    );
    // The right ones, by token or by cookie, and by each of the server's names.
    assert!(open(port, &host, own, Some(&token), None).await.is_ok());
    assert!(open(port, &host, own, None, Some(&cookie)).await.is_ok());
    let localhost = format!("localhost:{port}");
    let localhost_origin = format!("http://localhost:{port}");
    assert!(open(
        port,
        &localhost,
        Some(&localhost_origin),
        Some(&token),
        None
    )
    .await
    .is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_takes_the_token_once_and_data_needs_it() {
    let (port, token, _dir) = start().await;
    let host = format!("127.0.0.1:{port}");
    let h = [("Host", host.as_str())];
    // The token becomes a cookie, and leaves the address.
    let (status, response) = get(port, &format!("/?dataset=karate&token={token}"), &h).await;
    assert_eq!(status, 303, "{response}");
    assert!(
        response.contains("location: /?dataset=karate\r\n"),
        "{response}"
    );
    let cookie = format!("ursa_token_{port}={token}; HttpOnly; SameSite=Strict; Path=/");
    assert!(
        response.contains(&format!("set-cookie: {cookie}\r\n")),
        "{response}"
    );
    let (status, _) = get(port, "/?token=nope", &h).await;
    assert_eq!(status, 403);
    // The page says it talks to this server, and has the deploy's headers.
    let (status, page) = get(port, "/", &h).await;
    assert_eq!(status, 200);
    assert!(
        page.contains("<meta name=\"ursa-transport\" content=\"socket\">"),
        "{page}"
    );
    for (name, value) in ursa_server::headers::SECURITY_HEADERS {
        assert!(
            page.contains(&format!("{name}: {value}\r\n")),
            "{name}: {page}"
        );
    }
    let (status, _) = get(port, "/app.js", &h).await;
    assert_eq!(status, 200);
    // Data needs the token; files outside the directory are never served.
    assert_eq!(get(port, "/data/ties.csv", &h).await.0, 401);
    let with = format!("ursa_token_{port}={token}");
    let c = [("Host", host.as_str()), ("Cookie", with.as_str())];
    let (status, csv) = get(port, "/data/ties.csv", &c).await;
    assert_eq!(status, 200);
    assert!(csv.ends_with("src,dst\n1,2\n2,3\n3,1\n"), "{csv}");
    let r = [
        ("Host", host.as_str()),
        ("Cookie", with.as_str()),
        ("Range", "bytes=0-2"),
    ];
    let (status, range) = get(port, "/data/ties.csv", &r).await;
    assert_eq!(status, 206);
    assert!(range.ends_with("\r\n\r\nsrc"), "{range}");
    for path in [
        "/data/..%2fdist%2findex.html",
        "/data/.hidden",
        "/../ties.csv",
        "/%2e%2e/ties.csv",
    ] {
        let status = get(port, path, &c).await.0;
        assert!(status >= 400, "{path}: {status}");
    }
    // A request by another name is refused, whatever it asks for.
    let (status, response) = get(port, "/", &[("Host", "evil.example")]).await;
    assert_eq!(status, 403);
    assert!(response.contains("content-security-policy: "), "{response}");
}

/// Send a request; its reply's JSON and parts.
async fn call(
    socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    id: u32,
    op: &str,
    args: Value,
    parts: &[&[u8]],
) -> (Value, Vec<Vec<u8>>) {
    let listed: Vec<Value> = parts
        .iter()
        .map(|p| json!({"kind": "bytes", "name": "file", "bytes": p.len()}))
        .collect();
    let request = json!({"v": 1, "id": id, "op": op, "args": args, "parts": listed});
    socket
        .send(Message::text(request.to_string()))
        .await
        .unwrap();
    for (index, part) in parts.iter().enumerate() {
        let mut frame = id.to_le_bytes().to_vec();
        frame.extend_from_slice(&(index as u16).to_le_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(part);
        socket.send(Message::binary(frame)).await.unwrap();
    }
    let Some(Ok(Message::Text(text))) = socket.next().await else {
        panic!("no reply");
    };
    let reply: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(reply["id"], id);
    let mut bytes = Vec::new();
    for (index, _) in reply["parts"].as_array().into_iter().flatten().enumerate() {
        let Some(Ok(Message::Binary(frame))) = socket.next().await else {
            panic!("no part");
        };
        assert_eq!(&frame[..4], &id.to_le_bytes());
        assert_eq!(&frame[4..8], &[index as u8, 0, 0, 0]);
        bytes.push(frame[8..].to_vec());
    }
    (reply, bytes)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shell_runs_over_the_socket() {
    let (port, token, _dir) = start().await;
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://127.0.0.1:{port}");
    let mut socket = open(port, &host, Some(&origin), Some(&token), None)
        .await
        .unwrap();
    let (hello, _) = call(
        &mut socket,
        0,
        "hello",
        json!({"client": {"name": "test", "kind": "agent"}}),
        &[],
    )
    .await;
    assert_eq!(hello["result"]["transport"], "native");
    assert_eq!(hello["result"]["protocol"], 1);
    call(&mut socket, 1, "start", json!({}), &[]).await;
    let ties = _dir.path().join("ties.csv");
    let line = format!("CREATE VIEW ties AS SELECT * FROM '{}';", ties.display());
    let (fed, _) = call(&mut socket, 2, "feed", json!({"line": line}), &[]).await;
    assert_eq!(fed["result"]["err"], "", "{fed}");
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
    // An error has a kind; and a natively measured memory.
    let (failed, _) = call(&mut socket, 5, "export", json!({"format": "xml"}), &[]).await;
    assert_eq!(failed["ok"], false);
    assert_eq!(failed["error"]["kind"], "sql");
    let (memory, _) = call(&mut socket, 6, "memory", json!({}), &[]).await;
    assert!(memory["result"]["bytes"].as_u64().unwrap() > 0 || !cfg!(target_os = "linux"));
    // A request on another protocol version is refused.
    socket
        .send(Message::text(
            json!({"v": 2, "id": 7, "op": "hello"}).to_string(),
        ))
        .await
        .unwrap();
    let Some(Ok(Message::Text(text))) = socket.next().await else {
        panic!("no reply");
    };
    let refused: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(refused["error"]["kind"], "protocol");
}
