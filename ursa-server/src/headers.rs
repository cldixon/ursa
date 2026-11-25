//! The security headers on every response: the deploy's, from
//! `web/src/headers.ts`, which says why each is there. That file is the
//! source of truth; a test here fails when the two differ.

/// The Content Security Policy, as `CONTENT_SECURITY_POLICY` in
/// `web/src/headers.ts`. `connect-src 'self'` covers the page's WebSocket to
/// this server (`ws:` to the same host and port).
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; \
    script-src 'self' 'wasm-unsafe-eval'; \
    worker-src 'self'; \
    connect-src 'self'; \
    style-src 'self' 'unsafe-inline'; \
    font-src 'self'; \
    img-src 'self'; \
    base-uri 'none'; \
    form-action 'none'; \
    object-src 'none'; \
    frame-ancestors 'none'";

/// Every header and its value, as `SECURITY_HEADERS` in `web/src/headers.ts`.
pub const SECURITY_HEADERS: &[(&str, &str)] = &[
    ("content-security-policy", CONTENT_SECURITY_POLICY),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-embedder-policy", "require-corp"),
    ("cross-origin-resource-policy", "same-origin"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The quoted strings in `text` between `start` and the next `end`.
    fn quoted<'a>(text: &'a str, start: &str, end: &str) -> Vec<&'a str> {
        let from = text.find(start).expect(start) + start.len();
        let body = &text[from..from + text[from..].find(end).expect(end)];
        body.split('"').skip(1).step_by(2).collect()
    }

    #[test]
    fn the_headers_are_the_deploys() {
        let ts = include_str!("../../web/src/headers.ts");
        let csp = quoted(ts, "export const CONTENT_SECURITY_POLICY = [", "]");
        assert_eq!(csp.join("; "), CONTENT_SECURITY_POLICY);
        let pairs = quoted(
            ts,
            "SECURITY_HEADERS: Readonly<Record<string, string>> = {",
            "};",
        );
        let mut expected = Vec::new();
        for pair in pairs.chunks(2) {
            expected.push((pair[0], pair.get(1).copied().unwrap_or("")));
        }
        // The CSP's value is the constant, not a quoted string.
        assert_eq!(expected[0].0, "content-security-policy");
        let ours: Vec<_> = SECURITY_HEADERS.iter().map(|(n, _)| *n).collect();
        let theirs: Vec<_> = std::iter::once("content-security-policy")
            .chain(
                quoted(
                    ts,
                    "\"content-security-policy\": CONTENT_SECURITY_POLICY,",
                    "};",
                )
                .chunks(2)
                .map(|p| p[0]),
            )
            .collect();
        assert_eq!(ours, theirs);
        let values: Vec<_> = quoted(
            ts,
            "\"content-security-policy\": CONTENT_SECURITY_POLICY,",
            "};",
        )
        .chunks(2)
        .map(|p| p[1])
        .collect();
        assert_eq!(
            SECURITY_HEADERS[1..]
                .iter()
                .map(|(_, v)| *v)
                .collect::<Vec<_>>(),
            values
        );
    }
}
