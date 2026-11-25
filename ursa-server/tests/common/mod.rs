//! A server on a free port, and clients of it: raw HTTP, and the session
//! protocol on its WebSocket, as a browser would speak them.

#![allow(dead_code)]

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

pub type Socket = tokio_tungstenite::WebSocketStream<TcpStream>;

/// A running server, serving a directory with one CSV and a page.
pub struct Server {
    pub port: u16,
    pub token: String,
    pub dir: tempfile::TempDir,
    stop: Option<oneshot::Sender<()>>,
    pub served: Option<JoinHandle<std::io::Result<()>>>,
}

impl Server {
    pub async fn start() -> Server {
        // The session reads relative paths against the process's directory,
        // which `ursa serve` sets to the root: a test names a file in full.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ties.csv"), "src,dst\n1,2\n2,3\n3,1\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("n.csv"), "id\n1\n").unwrap();
        std::fs::write(dir.path().join(".secret"), "no").unwrap();
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
            assets: ursa_server::Assets::Dirs(vec![assets]),
            version: "test".into(),
        };
        let bound = ursa_server::bind(config).await.unwrap();
        let (port, token) = (bound.port(), bound.token().to_string());
        let (stop, stopped) = oneshot::channel::<()>();
        let served = tokio::spawn(bound.serve_with_shutdown(async {
            let _ = stopped.await;
        }));
        Server {
            port,
            token,
            dir,
            stop: Some(stop),
            served: Some(served),
        }
    }

    pub fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn cookie(&self) -> String {
        format!("ursa_token_{}={}", self.port, self.token)
    }

    /// Ask the server to stop, as Ctrl+C does.
    pub fn stop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
    }

    /// A WebSocket to `/session`, as this server's own page opens it.
    pub async fn connect(&self) -> Socket {
        open(
            self.port,
            &self.host(),
            Some(&self.origin()),
            None,
            Some(&self.cookie()),
        )
        .await
        .unwrap()
    }

    /// A connection that has said `hello`, asking for `session`.
    pub async fn session(&self, session: Option<&str>) -> (Socket, Value) {
        let mut socket = self.connect().await;
        let mut args = json!({"protocol": 1, "client": {"name": "test", "kind": "agent"}});
        if let Some(session) = session {
            args["session"] = json!(session);
        }
        let (hello, _) = call(&mut socket, 0, "hello", args, &[]).await;
        assert_eq!(hello["ok"], true, "{hello}");
        (socket, hello)
    }
}

/// A raw HTTP/1.1 request with these headers: the status, and the whole
/// response.
pub async fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
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

pub async fn get(port: u16, path: &str, headers: &[(&str, &str)]) -> (u16, String) {
    request(port, "GET", path, headers).await
}

/// Open `/session` with these `Host`, `Origin`, token and cookie; the
/// socket, or the status it was refused with.
pub async fn open(
    port: u16,
    host: &str,
    origin: Option<&str>,
    token: Option<&str>,
    cookie: Option<&str>,
) -> Result<Socket, u16> {
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

/// Send a request, framed as §4.2 says, without waiting for its reply.
pub async fn send(socket: &mut Socket, id: u32, op: &str, args: Value, parts: &[&[u8]]) {
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
}

/// The next reply: its JSON and its parts.
pub async fn receive(socket: &mut Socket) -> (Value, Vec<Vec<u8>>) {
    let text = loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => break text,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            other => panic!("no reply: {other:?}"),
        }
    };
    let reply: Value = serde_json::from_str(&text).unwrap();
    let mut bytes = Vec::new();
    for (index, part) in reply["parts"].as_array().into_iter().flatten().enumerate() {
        let Some(Ok(Message::Binary(frame))) = socket.next().await else {
            panic!("no part");
        };
        let id = reply["id"].as_u64().unwrap() as u32;
        assert_eq!(&frame[..4], &id.to_le_bytes());
        assert_eq!(&frame[4..8], &[index as u8, 0, 0, 0]);
        assert_eq!(part["bytes"].as_u64(), Some(frame.len() as u64 - 8));
        bytes.push(frame[8..].to_vec());
    }
    (reply, bytes)
}

/// Send a request; its reply's JSON and parts.
pub async fn call(
    socket: &mut Socket,
    id: u32,
    op: &str,
    args: Value,
    parts: &[&[u8]],
) -> (Value, Vec<Vec<u8>>) {
    send(socket, id, op, args, parts).await;
    let (reply, parts) = receive(socket).await;
    assert_eq!(reply["id"], id, "{reply}");
    (reply, parts)
}

/// Run a line of SQL or a dot-command in the shell; what it printed.
pub async fn feed(socket: &mut Socket, id: u32, line: &str) -> (String, String) {
    let (fed, _) = call(socket, id, "feed", json!({"line": line}), &[]).await;
    assert_eq!(fed["ok"], true, "{fed}");
    let text = |k: &str| fed["result"][k].as_str().unwrap_or("").to_string();
    (text("out"), text("err"))
}
