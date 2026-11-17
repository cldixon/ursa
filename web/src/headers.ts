/**
 * The security headers every response carries, from the Worker (index.ts) and
 * the local preview server (scripts/preview.ts) alike, so the e2e test runs
 * the page under the same policy as the deploy.
 *
 * The Content Security Policy allows what the page does and nothing else:
 *
 * - `default-src 'none'`: anything not named below is refused.
 * - `script-src 'self' 'wasm-unsafe-eval'`: the page's own scripts (app.js,
 *   theme.js, xterm.js, the engine's glue), and compiling the engine's
 *   WebAssembly. No inline script and no `eval`.
 * - `worker-src 'self'`: the engine's module worker, engine-worker.js.
 * - `connect-src 'self'`: fetches to this origin only: /data/* (the datasets,
 *   with range requests), /engine/*.wasm and engine.json.
 * - `style-src 'self' 'unsafe-inline'`: style.css and xterm.css, and the
 *   `<style>` elements xterm.js adds at run time for its DOM renderer (the
 *   cell size and the theme's colours); their contents change with the font
 *   and theme, so no hash can name them.
 * - `font-src 'self'`: IBM Plex Mono and Ursa Box, under /fonts/.
 * - `img-src 'self'`: the favicon.
 * - `base-uri 'none'`, `form-action 'none'`, `object-src 'none'`: the page
 *   has no <base>, no forms and no plugins.
 * - `frame-ancestors 'none'`: no other site may frame the shell.
 *
 * Downloads (.export) are blob: URLs opened by a link with `download`, a
 * navigation, which the policy does not govern; nothing loads a blob: URL.
 */
export const CONTENT_SECURITY_POLICY = [
	"default-src 'none'",
	"script-src 'self' 'wasm-unsafe-eval'",
	"worker-src 'self'",
	"connect-src 'self'",
	"style-src 'self' 'unsafe-inline'",
	"font-src 'self'",
	"img-src 'self'",
	"base-uri 'none'",
	"form-action 'none'",
	"object-src 'none'",
	"frame-ancestors 'none'",
].join("; ");

export const SECURITY_HEADERS: Readonly<Record<string, string>> = {
	"content-security-policy": CONTENT_SECURITY_POLICY,
	"x-content-type-options": "nosniff",
	"referrer-policy": "no-referrer",
};

/**
 * `response` with the security headers set. A response the Worker made is
 * changed in place (so the engine's keeps `encodeBody: "manual"`); one it
 * fetched, such as a static asset's, has immutable headers and is copied.
 */
export function withSecurityHeaders(response: Response): Response {
	const set = (r: Response) => {
		for (const [name, value] of Object.entries(SECURITY_HEADERS)) r.headers.set(name, value);
		return r;
	};
	try {
		return set(response);
	} catch {
		return set(new Response(response.body, response));
	}
}
