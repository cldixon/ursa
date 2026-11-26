//! `ursa serve`: Ursa's shell in a browser tab, over a WebSocket to a native
//! engine session (`design/console.md` §3–§5).
//!
//! The server is the session protocol's native end. A page opens `/session`,
//! says `hello`, and gets a session: a new one of its own (`hello {session:
//! "new"}`, the shell's choice), or the server's one shared
//! session (`"shared"`, the default, for the console and agents). A session is an actor: a
//! thread of its own that owns an [`ursa_shell::session::Session`], the same
//! code the browser demo runs in its Web Worker, and runs its requests one at
//! a time, in the order they came.
//!
//! What keeps it local (§5):
//!
//! - it listens on `127.0.0.1` and `::1` only;
//! - a token, 32 random bytes made at startup, opens the page (the
//!   console's `/console/?token=…`, or the shell's `/?token=…`,
//!   which sets it as an `HttpOnly`, `SameSite=Strict` cookie named for the
//!   port and drops it from the address) and is required on the WebSocket
//!   (the cookie, or `?token=`) and on `/data/`;
//! - every request's `Host` must be this server's (no DNS rebinding), and
//!   every WebSocket upgrade's `Origin` too (no other site's page);
//! - every response carries the deploy's security headers ([`headers`]).
//!
//! ```no_run
//! # async fn run() -> std::io::Result<()> {
//! let config = ursa_server::Config {
//!     root: ".".into(),
//!     database: None,
//!     read_only: false,
//!     port: ursa_server::DEFAULT_PORT,
//!     assets: ursa_server::Assets::Dirs(vec!["web/dist-console".into(), "web/dist".into()]),
//!     version: "0.13.0".into(),
//! };
//! let bound = ursa_server::bind(config).await?;
//! println!("{}", bound.url());
//! // Ctrl+C, in `ursa serve`: tokio::signal::ctrl_c().
//! let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
//! # drop(stop);
//! bound
//!     .serve_with_shutdown(async { stopped.await.ok(); })
//!     .await
//! # }
//! ```

mod files;
pub mod headers;
mod security;
mod session;
mod wire;

use std::future::{Future, IntoFuture};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Request, State};
use axum::http::header::{HeaderMap, HeaderValue};
use axum::http::{StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::ListenerExt;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::security::{host_ok, origin_ok, query_param, token_ok};
use crate::session::{Live, Sessions};

/// The port `ursa serve` tries first.
pub const DEFAULT_PORT: u16 = 4280;

/// How long [`Bound::serve_with_shutdown`] waits, once asked to stop, for
/// its connections and sessions to end: a running request is cancelled, but
/// a graph kernel that does not yield runs to its end.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The largest WebSocket message (and frame) a page may send: a file it
/// uploads (`addFile`) travels in one.
const MAX_MESSAGE: usize = 1 << 30;

/// How `ursa serve` runs.
#[derive(Debug, Clone)]
pub struct Config {
    /// The sessions' root: relative paths in their queries resolve against
    /// it, `COPY … TO` in the shared session writes under it, and `/data/`
    /// serves its files. A relative one is made absolute when the server
    /// binds.
    pub root: PathBuf,
    /// A database file every session opens, as the CLI opens one (`ursa
    /// serve club.ursa`). A relative path is the working directory's.
    pub database: Option<PathBuf>,
    /// Sessions write no files (`ursa serve --read-only`): `COPY … TO`,
    /// `INSERT INTO` a table of files, `.output`, `.once`, and anything
    /// that changes the database, which they open read-only, are refused.
    pub read_only: bool,
    /// The port to listen on; the next free one if it is taken (up to 20
    /// further). 0 picks any free port.
    pub port: u16,
    /// Where the page's files come from.
    pub assets: Assets,
    /// The version the sessions report (`hello`, `.version`).
    pub version: String,
}

/// Where the page's files come from: `web/dist-console` (the console, under
/// `console/`) and `web/dist` (the shell, and the modules both use), built by
/// `bun run build`.
#[derive(Debug, Clone)]
pub enum Assets {
    /// These directories, looked in in order: the first that has a file
    /// serves it (`[web/dist-console, web/dist]`).
    Dirs(Vec<PathBuf>),
    /// The files built into the binary (the `embed` feature). Without the
    /// feature, there are none.
    Embedded,
}

impl Assets {
    /// Whether this build has the page's files in it (the `embed` feature).
    pub const EMBEDDED: bool = cfg!(feature = "embed");
}

/// What every request handler shares.
pub(crate) struct Shared {
    pub(crate) config: Config,
    pub(crate) token: String,
    pub(crate) port: u16,
    pub(crate) sessions: Sessions,
    /// Becomes `true` when the server is asked to stop.
    pub(crate) stopping: watch::Receiver<bool>,
    /// The connections and sessions still running.
    pub(crate) live: Arc<Live>,
}

/// A server bound to its port, not yet serving.
pub struct Bound {
    listeners: Vec<TcpListener>,
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
}

impl Bound {
    /// The URL to open, with its token: the console's (`/console/`) if the
    /// assets have it, else the shell's (`/`).
    pub fn url(&self) -> String {
        let page = if self.has_console() {
            files::CONSOLE
        } else {
            "/"
        };
        format!(
            "http://127.0.0.1:{}{page}?token={}",
            self.shared.port, self.shared.token
        )
    }

    /// Whether the assets have the console (`web/dist-console`, which `bun
    /// run build` makes beside `web/dist`): it is served at `/console/`, and
    /// the shell at `/`.
    pub fn has_console(&self) -> bool {
        files::has_console(&self.shared.config.assets)
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
        self.serve_with_shutdown(std::future::pending()).await
    }

    /// Serve until `signal` completes, then stop gracefully: no new
    /// connections, every session's running request cancelled, every
    /// WebSocket closed (code 1001, "the server stopped"), and the sessions
    /// ended. It waits up to [`SHUTDOWN_GRACE`] for them.
    pub async fn serve_with_shutdown(
        self,
        signal: impl Future<Output = ()> + Send + 'static,
    ) -> io::Result<()> {
        let Bound {
            listeners,
            shared,
            stop,
        } = self;
        let app = router(Arc::clone(&shared));
        let mut servers = Vec::new();
        for listener in listeners {
            // A reply is a text frame and then its parts: without
            // TCP_NODELAY, Nagle's algorithm holds the second write until
            // the client's delayed ACK, 40 ms on Linux, on every reply.
            let listener = listener.tap_io(|tcp| {
                let _ = tcp.set_nodelay(true);
            });
            let mut stopping = shared.stopping.clone();
            let served = axum::serve(listener, app.clone()).with_graceful_shutdown(async move {
                let _ = stopping.wait_for(|s| *s).await;
            });
            servers.push(tokio::spawn(served.into_future()));
        }
        tokio::spawn(async move {
            signal.await;
            let _ = stop.send(true);
        });
        // Until asked to stop, or a listener fails.
        let mut stopping = shared.stopping.clone();
        let failed = tokio::select! {
            biased;
            _ = stopping.wait_for(|s| *s) => None,
            (result, _, _) = futures::future::select_all(&mut servers) => Some(result),
        };
        // Stopping, the listeners end too: that is no failure.
        let failed = failed.filter(|_| !*shared.stopping.borrow());
        if let Some(result) = failed {
            // A listener stopped by itself: stop the rest, and say why.
            for server in &servers {
                server.abort();
            }
            shared.sessions.close();
            result.map_err(io::Error::other)??;
            return Err(io::Error::other("the server stopped listening"));
        }
        // The shared session ends once its connections have.
        shared.sessions.close();
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, async {
            for server in servers {
                let _ = server.await;
            }
            shared.live.idle().await;
        })
        .await;
        Ok(())
    }
}

/// Bind to `127.0.0.1` and `::1` on the configured port, or the next free
/// one, and make the run's token.
pub async fn bind(mut config: Config) -> io::Result<Bound> {
    config.root = std::path::absolute(&config.root)?;
    if let Some(database) = &mut config.database {
        *database = std::path::absolute(&*database)?;
    }
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
                let (stop, stopping) = watch::channel(false);
                let live = Arc::new(Live::default());
                let shared = Arc::new(Shared {
                    sessions: Sessions::new(session::options(&config), Arc::clone(&live))?,
                    config,
                    token: security::new_token()?,
                    port,
                    stopping,
                    live,
                });
                return Ok(Bound {
                    listeners,
                    shared,
                    stop,
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && config.port != 0 => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no free port")))
}

fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/session", get(session_upgrade))
        .route("/data/{*name}", get(files::data))
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

/// `/session`: the WebSocket, for this server's own page with the token.
async fn session_upgrade(
    State(shared): State<Arc<Shared>>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if !origin_ok(&headers, shared.port) {
        return (StatusCode::FORBIDDEN, "this page may not open a session\n").into_response();
    }
    let token = query_param(uri.query(), "token");
    if !token_ok(&shared, token.as_deref(), &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            "a session needs the token ursa serve printed\n",
        )
            .into_response();
    }
    if *shared.stopping.borrow() {
        return (StatusCode::SERVICE_UNAVAILABLE, "the server is stopping\n").into_response();
    }
    match upgrade {
        Ok(upgrade) => upgrade
            .max_message_size(MAX_MESSAGE)
            .max_frame_size(MAX_MESSAGE)
            .on_upgrade(move |socket| wire::connection(socket, shared)),
        Err(rejection) => rejection.into_response(),
    }
}
