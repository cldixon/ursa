// Serve the shell locally without Cloudflare, as the Worker would: dist/ for
// the page, .data/ at /data/ (with HEAD and byte ranges, which the engine
// reads Parquet with) and the engine from .engine/ at /engine/<hash>.wasm.
// Build first: `bun run engine`, `bun run upload:data -- --dry` (stages
// .data/) and `bun run build`.
//
//   bun run preview [port]
import { file } from "bun";
import { extname, join, normalize } from "node:path";
import { DATA_DIR, ENGINE_DIR, WEB } from "./common";

const dist = join(WEB, "dist");
const TYPES: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
  ".wasm": "application/wasm",
  ".csv": "text/csv",
  ".parquet": "application/vnd.apache.parquet",
  ".svg": "image/svg+xml",
};

/** `path` under `root`, or null if it escapes it or does not exist. */
async function within(root: string, path: string) {
  const full = normalize(join(root, path));
  if (!full.startsWith(normalize(root))) return null;
  const f = file(full);
  return (await f.exists()) ? { f, full } : null;
}

async function serveRange(req: Request, f: ReturnType<typeof file>, type: string): Promise<Response> {
  const size = f.size;
  const headers: Record<string, string> = { "content-type": type, "accept-ranges": "bytes", etag: `"${size}-${f.lastModified}"` };
  const m = req.headers.get("range")?.match(/^bytes=(\d*)-(\d*)$/);
  if (m) {
    let start = m[1] === "" ? size - Number(m[2]) : Number(m[1]);
    const end = m[1] === "" || m[2] === "" ? size - 1 : Math.min(Number(m[2]), size - 1);
    if (start < 0) start = 0;
    if (start > end) return new Response(null, { status: 416, headers: { "content-range": `bytes */${size}` } });
    headers["content-range"] = `bytes ${start}-${end}/${size}`;
    headers["content-length"] = String(end - start + 1);
    return new Response(req.method === "HEAD" ? null : f.slice(start, end + 1), { status: 206, headers });
  }
  headers["content-length"] = String(size);
  return new Response(req.method === "HEAD" ? null : f, { headers });
}

const port = Number(Bun.argv[2] ?? 8788);
const server = Bun.serve({
  port,
  async fetch(req) {
    const path = decodeURIComponent(new URL(req.url).pathname);
    if (path.startsWith("/data/")) {
      const hit = await within(DATA_DIR, path.slice("/data/".length));
      return hit ? serveRange(req, hit.f, TYPES[extname(hit.full)] ?? "application/octet-stream") : new Response("not found", { status: 404 });
    }
    const wasm = path.match(/^\/engine\/([0-9a-f]+)\.wasm$/);
    if (wasm) {
      const hit = await within(ENGINE_DIR, `${wasm[1]}.wasm.br`);
      if (!hit) return new Response("not found", { status: 404 });
      return new Response(hit.f, { headers: { "content-type": "application/wasm", "content-encoding": "br" } });
    }
    const hit = await within(dist, path === "/" ? "index.html" : path);
    if (!hit) return new Response("not found", { status: 404 });
    return new Response(hit.f, { headers: { "content-type": TYPES[extname(hit.full)] ?? "application/octet-stream" } });
  },
});
console.log(`the shell at http://localhost:${server.port}/`);
