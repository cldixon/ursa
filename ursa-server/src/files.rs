//! The page's files, and the served directory's data files at `/data/`.
//!
//! The page's files hold no data and need no token (§5). There are two
//! pages: the console at `/console/` (`web/console/`, built into
//! `web/dist-console/console/`), which `ursa serve`'s link opens, and the
//! Web Shell at `/` (`web/dist/index.html`), where the browser demo's
//! addresses and share links have it. Either, with the run's token, sets it
//! as a cookie and drops it from the address; each is served with a
//! `<meta name="ursa-transport" content="socket">`, which tells the page to
//! talk to this server (`socket-transport.js`) and not start the wasm
//! engine. `/data/PATH` serves a file under the root directory (no `..`,
//! no hidden names) to a page with the token, with `HEAD` and byte ranges, as
//! the deploy serves the datasets; a range is read from the file, not the
//! whole file.

use std::borrow::Cow;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE,
    SET_COOKIE,
};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::security::{query_param, token_cookie, token_ok};
use crate::{Assets, Shared};

/// A file's type, by its extension.
fn content_type(path: &str) -> &'static str {
    let extension = path.rsplit_once('.').map_or("", |(_, e)| e);
    match extension.to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "wasm" => "application/wasm",
        "csv" => "text/csv; charset=utf-8",
        "tsv" => "text/tab-separated-values; charset=utf-8",
        "parquet" => "application/vnd.apache.parquet",
        "arrow" => "application/vnd.apache.arrow.file",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

/// The pieces of a URL's path (decoded), if every one is a plain name: no
/// `..`, no hidden names, no backslashes or NULs.
fn pieces(path: &str) -> Option<Vec<&str>> {
    let pieces: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let plain = |p: &&str| !p.starts_with('.') && !p.contains(['\\', '\0', ':']);
    pieces.iter().all(plain).then_some(pieces)
}

/// `path` under `root`, if it names a file there.
fn within(root: &Path, path: &str) -> Option<PathBuf> {
    let mut full = root.to_path_buf();
    full.extend(pieces(path)?);
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

/// The console's address, beside the shell's `/`.
pub const CONSOLE: &str = "/console/";

/// The console's page, among the page's files.
const CONSOLE_INDEX: &str = "console/index.html";

/// The page served at `path`, if `path` is one: the shell's index, or the
/// console's.
fn page(path: &str) -> Option<&'static str> {
    match path {
        "/" | "/index.html" => Some("index.html"),
        "/console/" | "/console/index.html" => Some(CONSOLE_INDEX),
        _ => None,
    }
}

/// Whether `assets` have the console's page (`bun run build` makes it).
pub(crate) fn has_console(assets: &Assets) -> bool {
    match assets {
        Assets::Dirs(dirs) => dirs.iter().any(|dir| within(dir, CONSOLE_INDEX).is_some()),
        Assets::Embedded => embedded(CONSOLE_INDEX).is_some(),
    }
}

/// The page's file at `path` (`index.html`, `app.js`, …).
async fn page_file(assets: &Assets, path: &str) -> Option<Cow<'static, [u8]>> {
    match assets {
        Assets::Dirs(dirs) => {
            let file = dirs.iter().find_map(|dir| within(dir, path))?;
            tokio::fs::read(file).await.ok().map(Cow::Owned)
        }
        Assets::Embedded => embedded(path),
    }
}

#[cfg(feature = "embed")]
fn embedded(path: &str) -> Option<Cow<'static, [u8]>> {
    /// `web/dist`, without the wasm engine's files.
    #[derive(rust_embed::RustEmbed)]
    #[folder = "../web/dist"]
    #[exclude = "engine/*"]
    struct Ui;

    /// `web/dist-console`: the console's own files, under `console/`. A build
    /// without them serves the shell alone.
    #[derive(rust_embed::RustEmbed)]
    #[folder = "../web/dist-console"]
    #[allow_missing = true]
    struct Console;

    let path = pieces(path)?.join("/");
    Console::get(&path)
        .or_else(|| Ui::get(&path))
        .map(|file| file.data)
}

#[cfg(not(feature = "embed"))]
fn embedded(_path: &str) -> Option<Cow<'static, [u8]>> {
    None
}

/// Any other path: the page's files.
pub(crate) async fn asset(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let method = request.method().clone();
    if !matches!(method, Method::GET | Method::HEAD) {
        return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
    }
    let uri = request.uri();
    let Some(path) = decode(uri.path()) else {
        return not_found();
    };
    if let Some(file) = page(&path) {
        return index(&shared, &path, file, uri.query()).await;
    }
    // The console's own files are beside its page: its address ends in `/`.
    if path == "/console" {
        let location = match uri.query() {
            Some(query) => format!("{CONSOLE}?{query}"),
            None => CONSOLE.to_string(),
        };
        return (StatusCode::PERMANENT_REDIRECT, [(LOCATION, location)]).into_response();
    }
    let Some(bytes) = page_file(&shared.config.assets, &path).await else {
        return not_found();
    };
    let length = bytes.len().to_string();
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(bytes)
    };
    (
        [
            (CONTENT_TYPE, content_type(&path).to_string()),
            (CONTENT_LENGTH, length),
            // The files change when the page is rebuilt: ask each time.
            (CACHE_CONTROL, "no-cache".to_string()),
        ],
        body,
    )
        .into_response()
}

/// A page (`/`, the shell, or `/console/`), from its `file`, for this
/// server. With `?token=`, a cookie for the token and a redirect to the same
/// page without it, so the token is never in the address the page shares
/// (`.share` links).
async fn index(shared: &Shared, path: &str, file: &str, query: Option<&str>) -> Response {
    if let Some(token) = query_param(query, "token") {
        if !token_ok(shared, Some(&token), &HeaderMap::new()) {
            return (
                StatusCode::FORBIDDEN,
                "this is not the token ursa serve printed\n",
            )
                .into_response();
        }
        let rest: Vec<&str> = query
            .unwrap_or("")
            .split('&')
            .filter(|p| !p.is_empty() && p.split_once('=').map_or(*p, |(k, _)| k) != "token")
            .collect();
        let location = if rest.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{}", rest.join("&"))
        };
        return (
            StatusCode::SEE_OTHER,
            [(LOCATION, location), (SET_COOKIE, token_cookie(shared))],
        )
            .into_response();
    }
    let Some(html) = page_file(&shared.config.assets, file).await else {
        return (
            StatusCode::NOT_FOUND,
            "this ursa has no page to serve: build web/ (bun run build) and pass --assets web/dist\n",
        )
            .into_response();
    };
    let html = String::from_utf8_lossy(&html).replacen(
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

/// `/data/PATH`: a file under the root directory, for a page with the token.
pub(crate) async fn data(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let method = request.method().clone();
    let headers = request.headers();
    if !token_ok(&shared, None, headers) {
        return (
            StatusCode::UNAUTHORIZED,
            "the data needs the token ursa serve printed\n",
        )
            .into_response();
    }
    let Some(path) = decode(request.uri().path()) else {
        return not_found();
    };
    let Some(name) = path.strip_prefix("/data/") else {
        return not_found();
    };
    let Some(file) = within(&shared.config.root, name) else {
        return not_found();
    };
    ranged(&method, headers, &file, content_type(name))
        .await
        .unwrap_or_else(|_| not_found())
}

/// The range `Range: bytes=…` asks of a file of `size` bytes, inclusive:
/// `None` for no range, or one this server ignores (several ranges, another
/// unit, bad syntax: the whole file is sent, as RFC 9110 allows), and
/// `Some(None)` for one past the end.
fn range(headers: &HeaderMap, size: u64) -> Option<Option<(u64, u64)>> {
    let spec = headers.get(RANGE)?.to_str().ok()?.strip_prefix("bytes=")?;
    let (from, to) = spec.trim().split_once('-')?;
    let number = |s: &str| -> Option<Option<u64>> {
        if s.is_empty() {
            Some(None)
        } else if s.bytes().all(|b| b.is_ascii_digit()) {
            s.parse().ok().map(Some)
        } else {
            None
        }
    };
    let (start, end) = match (number(from)?, number(to)?) {
        // The last `n` bytes.
        (None, Some(n)) if n > 0 && size > 0 => (size.saturating_sub(n), size - 1),
        (None, Some(_)) => return Some(None),
        (Some(start), None) => (start, size.saturating_sub(1)),
        (Some(start), Some(end)) if start <= end => (start, end.min(size.saturating_sub(1))),
        (Some(_), Some(_)) => return None,
        (None, None) => return None,
    };
    Some((start < size && start <= end).then_some((start, end)))
}

/// The file, or the range of it the request asks for (206), or 416 for a
/// range past the end. The body is read from the file as it is sent.
async fn ranged(
    method: &Method,
    headers: &HeaderMap,
    file: &Path,
    kind: &'static str,
) -> std::io::Result<Response> {
    let mut opened = tokio::fs::File::open(file).await?;
    let size = opened.metadata().await?.len();
    let (status, start, length, content_range) = match range(headers, size) {
        None => (StatusCode::OK, 0, size, None),
        Some(Some((start, end))) => (
            StatusCode::PARTIAL_CONTENT,
            start,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{size}")),
        ),
        Some(None) => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(CONTENT_RANGE, format!("bytes */{size}"))],
            )
                .into_response())
        }
    };
    let body = if *method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        opened.seek(SeekFrom::Start(start)).await?;
        Body::from_stream(chunks(opened.take(length)))
    };
    let mut response = (
        status,
        [
            (CONTENT_TYPE, kind.to_string()),
            (ACCEPT_RANGES, "bytes".to_string()),
            (CONTENT_LENGTH, length.to_string()),
        ],
        body,
    )
        .into_response();
    if let Some(content_range) = content_range {
        let value = content_range.parse().expect("a header value");
        response.headers_mut().insert(CONTENT_RANGE, value);
    }
    Ok(response)
}

/// A reader's bytes as a stream of chunks of up to 256 KB.
fn chunks(
    reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
) -> impl futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static {
    futures::stream::try_unfold(reader, |mut reader| async move {
        let mut chunk = vec![0u8; 256 * 1024];
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        chunk.truncate(read);
        Ok(Some((chunk, reader)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn asking(spec: &str, size: u64) -> Option<Option<(u64, u64)>> {
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_str(spec).unwrap());
        range(&headers, size)
    }

    #[test]
    fn ranges() {
        assert_eq!(asking("bytes=0-2", 10), Some(Some((0, 2))));
        assert_eq!(asking("bytes=4-", 10), Some(Some((4, 9))));
        assert_eq!(asking("bytes=-3", 10), Some(Some((7, 9))));
        assert_eq!(asking("bytes=-30", 10), Some(Some((0, 9))));
        assert_eq!(asking("bytes=5-100", 10), Some(Some((5, 9))));
        // Past the end.
        assert_eq!(asking("bytes=10-", 10), Some(None));
        assert_eq!(asking("bytes=-0", 10), Some(None));
        assert_eq!(asking("bytes=0-0", 0), Some(None));
        // Ignored: the whole file.
        for spec in [
            "bytes=0-1,4-5",
            "bytes=3-1",
            "items=0-1",
            "bytes=a-b",
            "bytes=-",
            "bytes=+1-2",
        ] {
            assert_eq!(asking(spec, 10), None, "{spec}");
        }
        assert_eq!(range(&HeaderMap::new(), 10), None);
    }

    #[test]
    fn plain_names_only() {
        assert_eq!(pieces("/a/b.csv"), Some(vec!["a", "b.csv"]));
        for path in ["/../x", "/a/../../x", "/.hidden", "/a\\b", "/c:x"] {
            assert_eq!(pieces(path), None, "{path}");
        }
    }
}
