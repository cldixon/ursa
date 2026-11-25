//! `ursa serve`: the shell's page (and the console's) in a browser tab, over
//! a WebSocket to a native engine session (`design/console.md` §3–§5).
//!
//! The server holds the session protocol's native end. Each request the page
//! sends on `/session` goes to a session actor: a thread of its own that owns
//! an [`ursa_shell::session::Session`], the same code the browser demo runs
//! in its Web Worker, and runs one request at a time.
//!
//! What keeps it local (§5):
//!
//! - it listens on `127.0.0.1` and `::1` only;
//! - a token, 32 random bytes made at startup, opens the page (`/?token=…`,
//!   which sets it as an HTTP-only, same-site cookie and drops it from the
//!   address) and is required on the WebSocket (the cookie, or `?token=`)
//!   and on `/data/`;
//! - every request's `Host` must be this server's (no DNS rebinding), and
//!   every WebSocket upgrade's `Origin` too (no other site's page);
//! - every response carries the deploy's security headers ([`headers`]).
//!
//! This is the spike of #536: one session per WebSocket, no cancel, no
//! events.

mod files;
pub mod headers;
mod session;
mod wire;

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, Request, State};
use axum::http::header::{HeaderMap, HeaderValue, COOKIE, HOST, ORIGIN};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::ListenerExt;
use axum::Router;
use base64::Engine;
use tokio::net::TcpListener;

/// The port `ursa serve` tries first.
pub const DEFAULT_PORT: u16 = 4280;

/// How `ursa serve` runs.
#[derive(Debug, Clone)]
pub struct Config {
    /// The directory the session reads relative paths in, and `/data/`
    /// serves.
    pub root: PathBuf,
    /// The port to listen on; the next free one if it is taken (up to 20
    /// further).
    pub port: u16,
    /// The page's files, looked for in order: `web/dist` (the shell) and the
    /// console's build.
    pub assets: Vec<PathBuf>,
    /// The version the session reports.
    pub version: String,
}

/// What every request handler shares.
pub(crate) struct Shared {
    pub(crate) config: Config,
    pub(crate) token: String,
    pub(crate) port: u16,
}

/// A server bound to its port, not yet serving.
pub struct Bound {
    listeners: Vec<TcpListener>,
    shared: Arc<Shared>,
}

impl Bound {
    /// The URL to open, with its token.
    pub fn url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/?token={}",
            self.shared.port, self.shared.token
        )
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.shared.port
    }

    /// The run's token.
    pub fn token(&self) -> &str {
        &self.shared.token
    }

    /// The addresses it listens on.
    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.listeners
            .iter()
            .filter_map(|l| l.local_addr().ok())
            .collect()
    }

    /// Serve until the process ends.
    pub async fn serve(self) -> io::Result<()> {
        let app = router(Arc::clone(&self.shared));
        let mut servers = Vec::new();
        for listener in self.listeners {
            // A reply is a text frame and then its parts: without
            // TCP_NODELAY, Nagle's algorithm holds the second write until
            // the client's delayed ACK, 40 ms on Linux, on every reply.
            let listener = listener.tap_io(|tcp| {
                let _ = tcp.set_nodelay(true);
            });
            servers.push(tokio::spawn(std::future::IntoFuture::into_future(
                axum::serve(listener, app.clone()),
            )));
        }
        for server in servers {
            server.await.map_err(io::Error::other)??;
        }
        Ok(())
    }
}

/// Bind to `127.0.0.1` and `::1` on the configured port, or the next free
/// one, and make the run's token.
pub async fn bind(config: Config) -> io::Result<Bound> {
    let mut last = None;
    for port in config.port..=config.port.saturating_add(20) {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(v4) => {
                let port = v4.local_addr()?.port();
                let mut listeners = vec![v4];
                // A machine without IPv6 still serves on 127.0.0.1.
                if let Ok(v6) = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await {
                    listeners.push(v6);
                }
                let shared = Arc::new(Shared {
                    config,
                    token: new_token()?,
                    port,
                });
                return Ok(Bound { listeners, shared });
            }
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && config.port != 0 => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no free port")))
}

/// 32 random bytes, base64url.
fn new_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/session", get(session_upgrade))
        .route("/data/{name}", get(files::data))
        .fallback(files::asset)
        .layer(middleware::from_fn_with_state(Arc::clone(&shared), guard))
        .with_state(shared)
}

/// Every request: from this server's own `Host`, and answered with the
/// security headers.
async fn guard(State(shared): State<Arc<Shared>>, request: Request, next: Next) -> Response {
    let mut response = if host_ok(request.headers(), shared.port) {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "wrong Host\n").into_response()
    };
    for (name, value) in headers::SECURITY_HEADERS {
        response
            .headers_mut()
            .insert(*name, HeaderValue::from_static(value));
    }
    response
}

/// The names this server answers to, with its port.
fn own_hosts(port: u16) -> [String; 3] {
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
}

/// `Host` is this server's: a page that reached it by another name (DNS
/// rebinding) is refused.
pub(crate) fn host_ok(headers: &HeaderMap, port: u16) -> bool {
    let host = headers.get(HOST).and_then(|h| h.to_str().ok());
    host.is_some_and(|host| {
        own_hosts(port)
            .iter()
            .any(|own| own.eq_ignore_ascii_case(host))
    })
}

/// `Origin` is this server's own page.
fn origin_ok(headers: &HeaderMap, port: u16) -> bool {
    let origin = headers.get(ORIGIN).and_then(|h| h.to_str().ok());
    origin.is_some_and(|origin| {
        own_hosts(port)
            .iter()
            .any(|own| origin.eq_ignore_ascii_case(&format!("http://{own}")))
    })
}

/// The cookie the token is kept in: one per port, as cookies are shared
/// across a host's ports.
pub(crate) fn cookie_name(port: u16) -> String {
    format!("ursa_token_{port}")
}

/// The request carries the run's token: in `?token=`, or in the cookie the
/// page was given.
pub(crate) fn token_ok(shared: &Shared, query: Option<&str>, headers: &HeaderMap) -> bool {
    let name = cookie_name(shared.port);
    let from_cookie = headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .any(|(k, v)| k == name && same(v, &shared.token));
    from_cookie || query.is_some_and(|q| same(q, &shared.token))
}

/// Compare in time that does not depend on where the strings differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// `/session`: the WebSocket, for this server's own page with the token.
async fn session_upgrade(
    State(shared): State<Arc<Shared>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !origin_ok(&headers, shared.port) {
        return (StatusCode::FORBIDDEN, "this page may not open a session\n").into_response();
    }
    if !token_ok(&shared, query.get("token").map(String::as_str), &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            "a session needs the token ursa serve printed\n",
        )
            .into_response();
    }
    let options = session::options(&shared.config);
    upgrade.on_upgrade(move |socket| wire::connection(socket, options))
}
