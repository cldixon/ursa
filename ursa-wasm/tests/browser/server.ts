// A static server for the browser tests and local development: the wasm package
// at /pkg/, data files at /data/ (with HEAD and byte ranges, as the Worker
// serves R2), and a blank page at /. Run with bun.
import { file } from "bun";
import { extname, join, normalize } from "node:path";

const TYPES: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".csv": "text/csv",
  ".parquet": "application/vnd.apache.parquet",
  ".json": "application/json",
};

/** Serve `path` under `root`, honouring HEAD and a single `Range: bytes=a-b`. */
async function serveFile(root: string, path: string, req: Request): Promise<Response> {
  const full = normalize(join(root, path));
  if (!full.startsWith(normalize(root))) return new Response("forbidden", { status: 403 });
  const f = file(full);
  if (!(await f.exists())) return new Response("not found", { status: 404 });
  const size = f.size;
  const headers: Record<string, string> = {
    "content-type": TYPES[extname(full)] ?? "application/octet-stream",
    "accept-ranges": "bytes",
    "last-modified": new Date(f.lastModified).toUTCString(),
    etag: `"${size}-${f.lastModified}"`,
  };
  const range = req.headers.get("range");
  const m = range?.match(/^bytes=(\d*)-(\d*)$/);
  if (m) {
    let start = m[1] === "" ? size - Number(m[2]) : Number(m[1]);
    let end = m[1] === "" || m[2] === "" ? size - 1 : Math.min(Number(m[2]), size - 1);
    if (start < 0) start = 0;
    if (start > end || start >= size) {
      return new Response(null, { status: 416, headers: { "content-range": `bytes */${size}` } });
    }
    headers["content-range"] = `bytes ${start}-${end}/${size}`;
    headers["content-length"] = String(end - start + 1);
    const body = req.method === "HEAD" ? null : f.slice(start, end + 1);
    return new Response(body, { status: 206, headers });
  }
  headers["content-length"] = String(size);
  return new Response(req.method === "HEAD" ? null : f, { headers });
}

export function startServer(opts: { pkg: string; data: string; port?: number }) {
  return Bun.serve({
    port: opts.port ?? 0,
    async fetch(req) {
      const { pathname } = new URL(req.url);
      if (pathname === "/") {
        return new Response("<!doctype html><meta charset=utf-8><title>ursa-wasm test</title>", {
          headers: { "content-type": "text/html; charset=utf-8" },
        });
      }
      if (pathname.startsWith("/pkg/")) return serveFile(opts.pkg, pathname.slice(5), req);
      if (pathname.startsWith("/data/")) return serveFile(opts.data, pathname.slice(6), req);
      return new Response("not found", { status: 404 });
    },
  });
}

if (import.meta.main) {
  const [pkg, data, port] = Bun.argv.slice(2);
  const server = startServer({ pkg, data, port: Number(port ?? 8787) });
  console.log(`serving ${pkg} at /pkg/ and ${data} at /data/ on ${server.url}`);
}
