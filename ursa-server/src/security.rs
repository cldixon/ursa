//! Who may reach the server (`design/console.md` §5): this server's own
//! names, its own page, and the run's token.

use std::io;

use axum::http::header::{HeaderMap, COOKIE, HOST, ORIGIN};
use base64::Engine;

use crate::Shared;

/// 32 random bytes from the operating system, base64url.
pub(crate) fn new_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
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

/// `Origin` is this server's own page. A request with no `Origin` (no
/// browser sends a WebSocket upgrade without one) is refused too.
pub(crate) fn origin_ok(headers: &HeaderMap, port: u16) -> bool {
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

/// The `Set-Cookie` that keeps the token: out of the page's scripts' reach
/// (`HttpOnly`) and never sent by another site's page (`SameSite=Strict`).
pub(crate) fn token_cookie(shared: &Shared) -> String {
    format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/",
        cookie_name(shared.port),
        shared.token
    )
}

/// The request carries the run's token: `token` (from `?token=`), or the
/// cookie the page was given.
pub(crate) fn token_ok(shared: &Shared, token: Option<&str>, headers: &HeaderMap) -> bool {
    let name = cookie_name(shared.port);
    let from_cookie = headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .any(|(k, v)| k == name && same(v, &shared.token));
    from_cookie || token.is_some_and(|t| same(t, &shared.token))
}

/// The value of `name` in a URL's query, as written (the token is base64url,
/// which needs no decoding).
pub(crate) fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

/// Compare in time that does not depend on where the strings differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn with(name: axum::http::HeaderName, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn hosts_and_origins() {
        assert!(host_ok(&with(HOST, "127.0.0.1:4280"), 4280));
        assert!(host_ok(&with(HOST, "LOCALHOST:4280"), 4280));
        assert!(host_ok(&with(HOST, "[::1]:4280"), 4280));
        for host in ["127.0.0.1", "127.0.0.1:4281", "evil.example:4280", ""] {
            assert!(!host_ok(&with(HOST, host), 4280), "{host}");
        }
        assert!(!host_ok(&HeaderMap::new(), 4280));
        assert!(origin_ok(&with(ORIGIN, "http://localhost:4280"), 4280));
        for origin in [
            "https://localhost:4280",
            "http://localhost:4281",
            "http://localhost",
            "null",
            "http://127.0.0.1:4280.evil.example",
        ] {
            assert!(!origin_ok(&with(ORIGIN, origin), 4280), "{origin}");
        }
        assert!(!origin_ok(&HeaderMap::new(), 4280));
    }

    #[test]
    fn tokens_are_long_and_differ() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert!(same(&a, &a.clone()));
        assert!(!same(&a, &b));
        assert!(!same(&a, &a[1..]));
    }

    #[test]
    fn query_params() {
        assert_eq!(
            query_param(Some("a=1&token=xy"), "token").as_deref(),
            Some("xy")
        );
        assert_eq!(query_param(Some("tokens=xy"), "token"), None);
        assert_eq!(query_param(None, "token"), None);
    }
}
