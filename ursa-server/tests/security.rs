//! Who may reach `ursa serve` (`design/console.md` §5), tried as a browser
//! and as an attacker would: what it binds to, the token, `Origin` and
//! `Host`, and what `/data/` gives.

mod common;

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::PathBuf;

use common::{get, open, request, Server};
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread")]
async fn it_listens_on_loopback_only() {
    let server = Server::start().await;
    let config = ursa_server::Config {
        root: PathBuf::from("."),
        port: 0,
        assets: ursa_server::Assets::Dirs(vec![]),
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
                TcpStream::connect((ip, server.port)).await.is_err(),
                "reached on {ip}"
            );
        }
    }
    // A taken port: the next free one.
    let config = ursa_server::Config {
        root: PathBuf::from("."),
        port: server.port,
        assets: ursa_server::Assets::Dirs(vec![]),
        version: "test".into(),
    };
    let next = ursa_server::bind(config).await.unwrap();
    assert_ne!(next.port(), server.port);
    assert!(next.addresses().iter().all(|a| a.ip().is_loopback()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_needs_the_token_its_own_origin_and_host() {
    let server = Server::start().await;
    let (port, token) = (server.port, server.token.as_str());
    let host = server.host();
    let origin = server.origin();
    let own = Some(origin.as_str());
    let cookie = server.cookie();
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
        let refused = open(port, &host, Some(foreign), Some(token), None)
            .await
            .err();
        assert_eq!(refused, Some(403), "{foreign}");
    }
    assert_eq!(
        open(port, &host, None, Some(token), None).await.err(),
        Some(403)
    );
    // A name that rebinds to 127.0.0.1, with an origin to match.
    let rebound = format!("evil.example:{port}");
    let rebound_origin = format!("http://evil.example:{port}");
    let refused = open(port, &rebound, Some(&rebound_origin), Some(token), None)
        .await
        .err();
    assert_eq!(refused, Some(403));
    assert_eq!(
        open(port, "127.0.0.1:1", own, Some(token), None)
            .await
            .err(),
        Some(403)
    );
    // The right ones, by token or by cookie, and by each of the server's names.
    assert!(open(port, &host, own, Some(token), None).await.is_ok());
    assert!(open(port, &host, own, None, Some(&cookie)).await.is_ok());
    let localhost = format!("localhost:{port}");
    let localhost_origin = format!("http://localhost:{port}");
    assert!(
        open(port, &localhost, Some(&localhost_origin), Some(token), None)
            .await
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_console_is_at_console_and_its_link_is_printed() {
    let server = Server::start().await;
    let (port, token) = (server.port, server.token.as_str());
    let host = server.host();
    let h = [("Host", host.as_str())];
    // The link opens the console.
    assert_eq!(
        server.url,
        format!("http://127.0.0.1:{port}/console/?token={token}")
    );
    // It takes the token as the shell's page does, and keeps its address.
    let (status, response) = get(port, &format!("/console/?token={token}"), &h).await;
    assert_eq!(status, 303, "{response}");
    assert!(response.contains("location: /console/\r\n"), "{response}");
    assert!(
        response.contains(&format!("set-cookie: ursa_token_{port}={token};")),
        "{response}"
    );
    let (status, response) = get(port, &format!("/console/?x=1&token={token}"), &h).await;
    assert_eq!(status, 303, "{response}");
    assert!(response.contains("location: /console/?x=1\r\n"), "{response}");
    assert_eq!(get(port, "/console/?token=nope", &h).await.0, 403);
    // The page, marked for the socket; its address ends in a slash.
    for path in ["/console/", "/console/index.html"] {
        let (status, page) = get(port, path, &h).await;
        assert_eq!(status, 200, "{page}");
        assert!(page.contains("<title>console</title>"), "{page}");
        assert!(
            page.contains("<meta name=\"ursa-transport\" content=\"socket\">"),
            "{page}"
        );
        assert!(page.contains("cache-control: no-store\r\n"), "{page}");
    }
    let (status, response) = get(port, "/console?token=x", &h).await;
    assert_eq!(status, 308, "{response}");
    assert!(response.contains("location: /console/?token=x\r\n"), "{response}");
    // Its own files, and the shell's beside it, from the other directory.
    let (status, js) = get(port, "/console/console.js", &h).await;
    assert_eq!(status, 200, "{js}");
    assert!(js.contains("content-type: text/javascript"), "{js}");
    let (status, page) = get(port, "/", &h).await;
    assert_eq!(status, 200);
    assert!(page.contains("<title>t</title>"), "{page}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_takes_the_token_once() {
    let server = Server::start().await;
    let (port, token) = (server.port, server.token.as_str());
    let host = server.host();
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
    let (status, response) = get(port, &format!("/?token={token}"), &h).await;
    assert_eq!(status, 303);
    assert!(response.contains("location: /\r\n"), "{response}");
    let (status, _) = get(port, "/?token=nope", &h).await;
    assert_eq!(status, 403);
    // The page says it talks to this server, and has the deploy's headers.
    for path in ["/", "/index.html"] {
        let (status, page) = get(port, path, &h).await;
        assert_eq!(status, 200);
        assert!(
            page.contains("<meta name=\"ursa-transport\" content=\"socket\">"),
            "{page}"
        );
        assert!(page.contains("cache-control: no-store\r\n"), "{page}");
        for (name, value) in ursa_server::headers::SECURITY_HEADERS {
            assert!(
                page.contains(&format!("{name}: {value}\r\n")),
                "{name}: {page}"
            );
        }
    }
    // The page's files need no token; they are only read.
    let (status, js) = get(port, "/app.js", &h).await;
    assert_eq!(status, 200);
    assert!(js.contains("content-type: text/javascript"), "{js}");
    assert_eq!(request(port, "POST", "/app.js", &h).await.0, 405);
    assert_eq!(get(port, "/no/such/file.js", &h).await.0, 404);
    // A request by another name is refused, whatever it asks for, and still
    // has the security headers.
    for host in ["evil.example", "127.0.0.1", "localhost:1"] {
        let (status, response) = get(port, "/app.js", &[("Host", host)]).await;
        assert_eq!(status, 403, "{host}");
        assert!(response.contains("content-security-policy: "), "{response}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn data_needs_the_token_and_serves_ranges() {
    let server = Server::start().await;
    let port = server.port;
    let host = server.host();
    let with = server.cookie();
    let h = [("Host", host.as_str())];
    let c = [("Host", host.as_str()), ("Cookie", with.as_str())];
    // No token, a wrong cookie, another port's.
    assert_eq!(get(port, "/data/ties.csv", &h).await.0, 401);
    let wrong = format!("ursa_token_{port}={}", "A".repeat(server.token.len()));
    let w = [("Host", host.as_str()), ("Cookie", wrong.as_str())];
    assert_eq!(get(port, "/data/ties.csv", &w).await.0, 401);
    let other = format!("ursa_token_{}={}", port.wrapping_add(1), server.token);
    let o = [("Host", host.as_str()), ("Cookie", other.as_str())];
    assert_eq!(get(port, "/data/ties.csv", &o).await.0, 401);
    // With the cookie: the file, a subdirectory's, HEAD and ranges.
    let (status, csv) = get(port, "/data/ties.csv", &c).await;
    assert_eq!(status, 200);
    assert!(csv.contains("content-length: 20\r\n"), "{csv}");
    assert!(csv.contains("accept-ranges: bytes\r\n"), "{csv}");
    assert!(csv.ends_with("\r\n\r\nsrc,dst\n1,2\n2,3\n3,1\n"), "{csv}");
    let (status, nested) = get(port, "/data/sub/n.csv", &c).await;
    assert_eq!(status, 200);
    assert!(nested.ends_with("\r\n\r\nid\n1\n"), "{nested}");
    let (status, head) = request(port, "HEAD", "/data/ties.csv", &c).await;
    assert_eq!(status, 200);
    assert!(head.contains("content-length: 20\r\n"), "{head}");
    assert!(head.ends_with("\r\n\r\n"), "{head}");
    let ranged = |range: &'static str| {
        let (host, with) = (host.clone(), with.clone());
        async move {
            let r = [
                ("Host", host.as_str()),
                ("Cookie", with.as_str()),
                ("Range", range),
            ];
            get(port, "/data/ties.csv", &r).await
        }
    };
    let (status, range) = ranged("bytes=0-2").await;
    assert_eq!(status, 206);
    assert!(range.contains("content-range: bytes 0-2/20\r\n"), "{range}");
    assert!(range.ends_with("\r\n\r\nsrc"), "{range}");
    let (status, range) = ranged("bytes=-4").await;
    assert_eq!(status, 206);
    assert!(range.ends_with("\r\n\r\n3,1\n"), "{range}");
    let (status, range) = ranged("bytes=16-").await;
    assert_eq!(status, 206);
    assert!(range.ends_with("\r\n\r\n3,1\n"), "{range}");
    let (status, range) = ranged("bytes=20-").await;
    assert_eq!(status, 416);
    assert!(range.contains("content-range: bytes */20\r\n"), "{range}");
    // Several ranges: the whole file.
    assert_eq!(ranged("bytes=0-1,4-5").await.0, 200);
    // Never outside the directory, or a hidden file.
    for path in [
        "/data/..%2fdist%2findex.html",
        "/data/../ties.csv",
        "/data/%2e%2e/ties.csv",
        "/data/.secret",
        "/data/sub/../.secret",
        "/data/sub",
        "/data/",
        "/../ties.csv",
        "/%2e%2e/ties.csv",
    ] {
        let (status, response) = get(port, path, &c).await;
        assert!(status >= 400, "{path}: {status}");
        assert!(!response.contains("1,2\n"), "{path}: {response}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn data_is_read_as_it_is_sent() {
    // A file larger than a chunk, read back whole and in a range from its
    // middle.
    let server = Server::start().await;
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(server.dir.path().join("big.bin"), &big).unwrap();
    let (host, cookie) = (server.host(), server.cookie());
    let read = |range: Option<&'static str>| {
        let (host, cookie, port) = (host.clone(), cookie.clone(), server.port);
        async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let mut request = format!(
                "GET /data/big.bin HTTP/1.1\r\nHost: {host}\r\nCookie: {cookie}\r\nConnection: close\r\n"
            );
            if let Some(range) = range {
                request.push_str(&format!("Range: {range}\r\n"));
            }
            request.push_str("\r\n");
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let head = String::from_utf8_lossy(&response[..split]).into_owned();
            (head, response[split + 4..].to_vec())
        }
    };
    let (head, body) = read(None).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    // Sent with its length, not chunked.
    assert!(head.contains("content-length: 3000000"), "{head}");
    assert!(body == big);
    let (head, body) = read(Some("bytes=1000000-2500000")).await;
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert!(body == big[1_000_000..=2_500_000]);
}
