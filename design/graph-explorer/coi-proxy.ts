// A proxy in front of the preview that adds the cross-origin isolation
// headers, to see whether the shell runs under them.  bun coi-proxy.ts 8801 http://localhost:8800
const port = Number(Bun.argv[2] ?? 8801);
const upstream = Bun.argv[3] ?? "http://localhost:8800";
Bun.serve({
  port,
  async fetch(req) {
    const url = new URL(req.url);
    const r = await fetch(upstream + url.pathname + url.search, { method: req.method, headers: req.headers, decompress: false } as any);
    const h = new Headers(r.headers);
    h.set("cross-origin-opener-policy", "same-origin");
    h.set("cross-origin-embedder-policy", "require-corp");
    h.set("cross-origin-resource-policy", "same-origin");
    
    return new Response(r.body, { status: r.status, headers: h });
  },
});
