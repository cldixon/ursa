//! The page's files (`web/dist`, and the console's), and the directory's
//! data files at `/data/`.
//!
//! The page's files hold no data and need no token (§5). `/` with the
//! run's token sets it as a cookie and drops it from the address; the
//! index is served with a `<meta name="ursa-transport" content="socket">`,
//! which tells `engine-client.js` to talk to this server and not start the
//! wasm engine. `/data/NAME` serves a file of the root directory (no
//! subdirectories, no dot files) to a page with the token, with `HEAD` and
//! byte ranges, as the deploy serves the datasets.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE,
    SET_COOKIE,
};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::{cookie_name, token_ok, Shared};

/// A file's type, by its extension.
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "wasm" => "application/wasm",
        "csv" => "text/csv",
        "parquet" => "application/vnd.apache.parquet",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

/// `path` (a URL's, decoded) under `root`, if it names a file there: no
/// `..`, no hidden names.
fn within(root: &Path, path: &str) -> Option<PathBuf> {
    let mut full = root.to_path_buf();
    for piece in path.split('/').filter(|p| !p.is_empty()) {
        if piece.starts_with('.') || piece.contains('\\') || piece.contains('\0') {
            return None;
        }
        full.push(piece);
    }
    full.is_file().then_some(full)
}

/// Percent-decode a URL's path; `None` if it does not decode to UTF-8.
fn decode(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Any other path: the page's files.
pub(crate) async fn asset(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
    }
    let uri = request.uri();
    let Some(path) = decode(uri.path()) else {
        return not_found();
    };
    if path == "/" {
        return index(&shared, uri.query(), request.headers()).await;
    }
    let Some(file) = shared
        .config
        .assets
        .iter()
        .find_map(|root| within(root, &path))
    else {
        return not_found();
    };
    match tokio::fs::read(&file).await {
        Ok(bytes) => ([(CONTENT_TYPE, content_type(&file))], bytes).into_response(),
        Err(_) => not_found(),
    }
}

/// `/`: the shell's page, for this server. With `?token=`, a cookie for the
/// token and a redirect to the same page without it, so the token is never
/// in the address the page shares (`.share` links).
async fn index(shared: &Shared, query: Option<&str>, headers: &HeaderMap) -> Response {
    let pairs: Vec<(&str, &str)> = query
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .collect();
    if let Some((_, token)) = pairs.iter().find(|(k, _)| *k == "token") {
        if !token_ok(shared, Some(token), &HeaderMap::new()) {
            return (
                StatusCode::FORBIDDEN,
                "this is not the token ursa serve printed\n",
            )
                .into_response();
        }
        let rest: Vec<String> = pairs
            .iter()
            .filter(|(k, _)| *k != "token")
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let location = if rest.is_empty() {
            "/".to_string()
        } else {
            format!("/?{}", rest.join("&"))
        };
        let cookie = format!(
            "{}={}; HttpOnly; SameSite=Strict; Path=/",
            cookie_name(shared.port),
            shared.token
        );
        return (
            StatusCode::SEE_OTHER,
            [(LOCATION, location), (SET_COOKIE, cookie)],
        )
            .into_response();
    }
    let _ = headers;
    let Some(file) = shared
        .config
        .assets
        .iter()
        .find_map(|root| within(root, "index.html"))
    else {
        return not_found();
    };
    let Ok(html) = tokio::fs::read_to_string(&file).await else {
        return not_found();
    };
    let html = html.replacen(
        "<head>",
        "<head>\n  <meta name=\"ursa-transport\" content=\"socket\">",
        1,
    );
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response()
}

/// `/data/NAME`: a file of the root directory, for a page with the token.
pub(crate) async fn data(
    State(shared): State<Arc<Shared>>,
    UrlPath(name): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let named = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !named {
        return not_found();
    }
    if !token_ok(&shared, None, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            "the data needs the token ursa serve printed\n",
        )
            .into_response();
    }
    let Some(file) = within(&shared.config.root, &name) else {
        return not_found();
    };
    let Ok(bytes) = tokio::fs::read(&file).await else {
        return not_found();
    };
    ranged(&method, &headers, bytes, content_type(&file))
}

/// The bytes, or the range of them `Range: bytes=a-b` asks for (206), or 416
/// for a range past the end.
fn ranged(method: &Method, headers: &HeaderMap, bytes: Vec<u8>, kind: &'static str) -> Response {
    let size = bytes.len() as u64;
    let range = headers
        .get(RANGE)
        .and_then(|r| r.to_str().ok())
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.split_once('-'));
    let head = *method == Method::HEAD;
    let Some((from, to)) = range else {
        let body = if head {
            Body::empty()
        } else {
            Body::from(bytes)
        };
        return (
            [(CONTENT_TYPE, kind), (ACCEPT_RANGES, "bytes")],
            [(CONTENT_LENGTH, size.to_string())],
            body,
        )
            .into_response();
    };
    let (start, end) = match (from.parse::<u64>().ok(), to.parse::<u64>().ok()) {
        (None, Some(last)) => (size.saturating_sub(last), size.saturating_sub(1)),
        (Some(start), None) => (start, size.saturating_sub(1)),
        (Some(start), Some(end)) => (start, end.min(size.saturating_sub(1))),
        (None, None) => return not_found(),
    };
    if start > end || start >= size {
        return (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(CONTENT_RANGE, format!("bytes */{size}"))],
        )
            .into_response();
    }
    let slice = bytes[start as usize..=end as usize].to_vec();
    let body = if head {
        Body::empty()
    } else {
        Body::from(slice)
    };
    (
        StatusCode::PARTIAL_CONTENT,
        [(CONTENT_TYPE, kind), (ACCEPT_RANGES, "bytes")],
        [
            (CONTENT_RANGE, format!("bytes {start}-{end}/{size}")),
            (CONTENT_LENGTH, (end - start + 1).to_string()),
        ],
        body,
    )
        .into_response()
}
