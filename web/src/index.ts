/**
 * The Ursa Shell's Worker (#279). It does no query work: the engine runs in the
 * browser, in WebAssembly. Every request reaches it first (`runWorkerFirst` in
 * cloudflare.config.ts):
 *
 * - /data/<file> serves a sample dataset from the R2 bucket (key
 *   `datasets/<file>`). The engine reads Parquet and CSV with HTTP range
 *   requests, so this supports HEAD, single byte ranges (206 + Content-Range)
 *   and ETag / If-Range validators, as Plain Text's Worker does for DuckDB-WASM.
 *   The bucket holds only public sample data, so any file under `datasets/` is
 *   served; nothing else in the bucket is reachable.
 * - /engine/<hash>.wasm serves the engine from R2 (key `engine/<hash>.wasm.br`).
 *   It is too big for a static asset (the limit is 25 MiB a file; the engine is
 *   about 31 MB), so it is stored brotli-compressed (about 5 MB) and sent as is,
 *   with `Content-Encoding: br`. The name is a content hash: cached for good.
 * - everything else is a static asset (dist/: the page, the engine's JS glue
 *   and engine.json, which names the current .wasm).
 *
 * `Env` (the `ASSETS` and R2 `DATA` bindings) is generated from
 * cloudflare.config.ts by `cf workers types` into .cloudflare/types/.
 */

const DATA_PREFIX = "/data/";
const ENGINE_PREFIX = "/engine/";

/** A dataset file name: no directories, no dot files. */
const DATA_FILE = /^[A-Za-z0-9_-][A-Za-z0-9_.-]*$/;
/** A content-hashed engine build. */
const ENGINE_FILE = /^[0-9a-f]{16,64}\.wasm$/;

const TYPES: Record<string, string> = {
	parquet: "application/vnd.apache.parquet",
	csv: "text/csv; charset=utf-8",
	json: "application/json",
	md: "text/markdown; charset=utf-8",
};

type ByteRange = { offset: number; length: number };

/**
 * Parses a single-range `Range: bytes=…` header against the object size.
 * Returns null when there is no usable header (serve the whole object),
 * or "unsatisfiable" for a well-formed range outside the object.
 */
function parseRange(header: string | null, size: number): ByteRange | null | "unsatisfiable" {
	if (!header) return null;
	const match = /^bytes=(\d*)-(\d*)$/.exec(header.trim());
	if (!match) return null; // Multi-range or malformed: ignore, as RFC 9110 allows.
	const [, startText = "", endText = ""] = match;
	if (startText === "" && endText === "") return null;

	if (startText === "") {
		// Suffix range: the last N bytes.
		const suffix = Number(endText);
		if (suffix === 0) return "unsatisfiable";
		const length = Math.min(suffix, size);
		return { offset: size - length, length };
	}

	const start = Number(startText);
	if (start >= size) return "unsatisfiable";
	const end = endText === "" ? size - 1 : Math.min(Number(endText), size - 1);
	if (end < start) return null;
	return { offset: start, length: end - start + 1 };
}

function methodNotAllowed(): Response {
	return new Response("Method not allowed\n", { status: 405, headers: { allow: "GET, HEAD" } });
}

/** Serves one dataset with HEAD, a single byte range and ETag / If-Range validators. */
async function serveData(request: Request, env: Env, file: string): Promise<Response> {
	if (request.method !== "GET" && request.method !== "HEAD") return methodNotAllowed();
	if (!DATA_FILE.test(file)) return new Response("Not found\n", { status: 404 });
	const key = `datasets/${file}`;

	const head = await env.DATA.head(key);
	if (!head) return new Response("Not found\n", { status: 404 });

	const headers = new Headers();
	head.writeHttpMetadata(headers);
	const extension = file.slice(file.lastIndexOf(".") + 1);
	if (!headers.has("content-type")) headers.set("content-type", TYPES[extension] ?? "application/octet-stream");
	headers.set("etag", head.httpEtag);
	headers.set("last-modified", head.uploaded.toUTCString());
	headers.set("accept-ranges", "bytes");
	headers.set("cache-control", "public, max-age=3600");

	const ifNoneMatch = request.headers.get("if-none-match");
	if (ifNoneMatch && ifNoneMatch === head.httpEtag) {
		return new Response(null, { status: 304, headers });
	}

	// Ignore Range when If-Range names a different version of the object.
	const ifRange = request.headers.get("if-range");
	const rangeHeader = ifRange && ifRange !== head.httpEtag ? null : request.headers.get("range");
	const range = parseRange(rangeHeader, head.size);

	if (range === "unsatisfiable") {
		headers.set("content-range", `bytes */${head.size}`);
		return new Response(null, { status: 416, headers });
	}

	const length = range ? range.length : head.size;
	headers.set("content-length", String(length));
	if (range) {
		headers.set("content-range", `bytes ${range.offset}-${range.offset + range.length - 1}/${head.size}`);
	}
	const status = range ? 206 : 200;

	if (request.method === "HEAD") return new Response(null, { status, headers });

	const object = await env.DATA.get(key, range ? { range } : undefined);
	if (!object) return new Response("Not found\n", { status: 404 });
	return new Response(object.body, { status, headers });
}

/** Serves a content-hashed engine build, stored brotli-compressed, as is. */
async function serveEngine(request: Request, env: Env, file: string): Promise<Response> {
	if (request.method !== "GET" && request.method !== "HEAD") return methodNotAllowed();
	if (!ENGINE_FILE.test(file)) return new Response("Not found\n", { status: 404 });

	const object = await env.DATA.get(`engine/${file}.br`);
	if (!object) return new Response("Not found\n", { status: 404 });
	const headers = new Headers({
		"content-type": "application/wasm",
		"content-encoding": "br",
		"content-length": String(object.size),
		etag: object.httpEtag,
		"cache-control": "public, max-age=31536000, immutable",
	});
	if (request.method === "HEAD") return new Response(null, { headers, encodeBody: "manual" });
	// `manual`: the body is already brotli; the runtime must not encode it again.
	return new Response(object.body, { headers, encodeBody: "manual" });
}

export default {
	async fetch(request, env): Promise<Response> {
		const url = new URL(request.url);
		const path = decodeURIComponent(url.pathname);
		if (path.startsWith(DATA_PREFIX)) return serveData(request, env, path.slice(DATA_PREFIX.length));
		if (path.startsWith(ENGINE_PREFIX) && path.endsWith(".wasm")) {
			return serveEngine(request, env, path.slice(ENGINE_PREFIX.length));
		}
		return env.ASSETS.fetch(request);
	},
} satisfies ExportedHandler<Env>;
