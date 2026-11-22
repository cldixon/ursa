// The graph view's benchmark, in Chromium: build ursa-viz for the web first
// (see ../../README.md), then, from ursa-viz/,
//
//   bun run bench                     # or: bun tests/browser/bench.ts [out.json]
//
// For each graph (R-MAT and a grid, about 1K, 10K, 100K and 1M edges) and
// each backend (WebGPU, WebGL2), it runs the demo page and measures:
//
// - time to first frame (the first frame drawn with layout positions) and
//   time to layout done, coarse to fine and flat (flat runs a fixed number of
//   iterations, so it is a cost per iteration, not a time to converge);
// - frame time while panning and while zooming: `view.pan` or `view.zoom`,
//   then `view.render`, then a sync point that waits for the GPU (WebGL2: a
//   1-pixel `readPixels`; WebGPU: `queue.onSubmittedWorkDone`), one frame per
//   animation frame, median and p95. Wall time, not GPU timestamps: the view
//   requests no `timestamp-query` feature;
// - the frame budget (#495): the same pan and zoom drawn as a page draws
//   them, `view.renderFrame` with each frame's cost (the time to the next
//   animation frame) told to `view.frameCost`, the edges in the layout's
//   importance order: the time between frames, median and p95, and how many
//   edges and nodes the last frame drew. This is the interaction trace to
//   read on a laptop: its p95 is what the budget holds;
// - pick latency (`view.pick` at random pixels);
// - memory: the page's and the layout worker's wasm memory, and
//   `performance.measureUserAgentSpecificMemory` (the server sends
//   COOP/COEP, so the page is cross-origin isolated) or `performance.memory`;
// - bytes uploaded per layout step (positions, 8 bytes a node) and in all;
// - the highlight layer: the burst again with a path of 100 nodes
//   highlighted, and with a node hidden too; the time to set a highlight
//   and a selection, and the bytes each uploads.
//
// Then, unless NATIVE=0, the layout alone natively (ursa-core's
// `fa2_multilevel` example in `bench` mode), on one thread and on all.
//
// Environment: BACKENDS=webgpu,webgl2 · GRAPHS=rmat,grid · SIZES=1k,10k,100k,1m
// · MODES=multilevel,flat · FRAMES=120 (and FRAME_BUDGET_MS=15000: frames stop
// after ~15 s, past the first 20) · PICKS=200 · HEADED=1 (a headed
// window, for a real GPU) · CHROME=/path/to/chrome (else Playwright's
// Chromium) · NATIVE=0.
import { chromium, type Browser } from "playwright";
import { file, spawnSync } from "bun";
import { cpus, totalmem, platform, release } from "node:os";
import { extname, join, normalize, resolve } from "node:path";

const root = resolve(import.meta.dir, "../..");
const repo = resolve(root, "..");
const out = Bun.argv[2] ?? join(root, "bench/results.json");
const list = (name: string, all: string[]) => (process.env[name] ? process.env[name]!.split(",") : all);
const BACKENDS = list("BACKENDS", ["webgpu", "webgl2"]);
const GRAPHS = list("GRAPHS", ["rmat", "grid"]);
const SIZES = list("SIZES", ["1k", "10k", "100k", "1m"]);
const MODES = list("MODES", ["multilevel", "flat"]);
const FRAMES = Number(process.env.FRAMES ?? 120);
const PICKS = Number(process.env.PICKS ?? 200);
const FRAME_BUDGET_MS = Number(process.env.FRAME_BUDGET_MS ?? 15_000);
const HEADED = process.env.HEADED === "1";

// The graphs: R-MAT at about 8 edges a node (as the shell's `rmat` dataset),
// and a square grid, which has a known right answer (a flat sheet).
const GRAPH_QUERY: Record<string, Record<string, string>> = {
  rmat: {
    "1k": "graph=rmat&scale=7&edges=1000",
    "10k": "graph=rmat&scale=11&edges=10000",
    "100k": "graph=rmat&scale=14&edges=100000",
    "1m": "graph=rmat&scale=17&edges=1000000",
  },
  grid: {
    "1k": "graph=grid&w=23&h=23",
    "10k": "graph=grid&w=71&h=71",
    "100k": "graph=grid&w=224&h=224",
    "1m": "graph=grid&w=707&h=707",
  },
};
// Flat ForceAtlas2 runs this many iterations: a cost per iteration, for
// reference (it would need hundreds to converge at any size; see README).
const FLAT_ITERATIONS: Record<string, number> = { "1k": 300, "10k": 300, "100k": 30, "1m": 5 };

const TYPES: Record<string, string> = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm" };
const server = Bun.serve({
  port: 0,
  async fetch(req) {
    const path = normalize(join(root, new URL(req.url).pathname));
    if (!path.startsWith(root)) return new Response("forbidden", { status: 403 });
    const f = file(path);
    if (!(await f.exists())) return new Response("not found", { status: 404 });
    return new Response(f, {
      headers: {
        "content-type": TYPES[extname(path)] ?? "application/octet-stream",
        // Cross-origin isolation, for performance.measureUserAgentSpecificMemory.
        "cross-origin-opener-policy": "same-origin",
        "cross-origin-embedder-policy": "require-corp",
      },
    });
  },
});
const base = `http://localhost:${server.port}/demo/index.html`;

const quantile = (xs: number[], q: number) => {
  if (!xs.length) return null;
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(q * s.length))];
};
const summary = (xs: number[]) => ({ median: round(quantile(xs, 0.5)), p95: round(quantile(xs, 0.95)), n: xs.length });
const round = (x: number | null | undefined, d = 3) => (x == null ? null : Math.round(x * 10 ** d) / 10 ** d);

async function launch(backend: string): Promise<Browser> {
  const args = backend === "webgpu" ? ["--enable-unsafe-webgpu", "--enable-features=Vulkan"] : ["--disable-features=WebGPU"];
  args.push("--enable-blink-features=ForceEagerMeasureMemory", "--enable-precise-memory-info");
  return chromium.launch({ headless: !HEADED, args, ...(process.env.CHROME ? { executablePath: process.env.CHROME } : {}) });
}

async function runCase(browser: Browser, backend: string, graph: string, size: string, mode: string) {
  const multilevel = mode === "multilevel";
  const query = GRAPH_QUERY[graph][size] + (multilevel ? "" : `&multilevel=0&iterations=${FLAT_ITERATIONS[size]}`);
  const page = await browser.newPage({ viewport: { width: 1000, height: 700 }, deviceScaleFactor: 1 });
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  await page.goto(`${base}?${query}`);
  await page.waitForFunction(() => (window as any).stats?.phase === "running", null, { timeout: 180_000 });
  const done = await page
    .waitForFunction(() => (window as any).stats.layoutDone && (window as any).stats.firstFrameAt !== undefined, null, {
      timeout: 600_000,
      polling: 100,
    })
    .then(() => true, () => false);
  await page.waitForTimeout(500); // the last positions drawn
  const s: any = await page.evaluate(() => (window as any).stats);
  const result: any = {
    backend: s.backend,
    graph,
    size,
    mode,
    nodes: s.nodes,
    edges: s.edges,
    done,
    errors,
    initMs: round(s.initMs, 1),
    generateMs: round(s.generateMs, 1),
    createViewMs: round(s.createMs, 1),
    // From the graph's being ready (wasm loaded, graph generated) on the page.
    firstFrameMs: round(s.firstFrameAt - s.graphReadyAt, 1),
    layoutDoneMs: round(s.layoutDoneAt - s.graphReadyAt, 1),
    // The worker's own clock, from building the layout to its end.
    workerLayoutMs: round(s.layoutMs, 1),
    hierarchyMs: round(s.buildMs, 1),
    // The worker's loading and compiling its own copy of the wasm.
    workerInitMs: round(s.workerInitMs, 1),
    iterations: s.iterations,
    msPerIteration: multilevel ? null : round(s.msPerIteration, 2),
    uploads: s.uploads,
    bytesPerUpload: s.nodes * 8,
    uploadBytes: s.uploadBytes,
    edgeIndexBytes: s.edges * 8,
  };

  if (multilevel) {
    // Frames: pan, then zoom, each frame synced; then picks.
    Object.assign(
      result,
      await page.evaluate(
        async ({ frames, picks, budgetMs }) => {
          const w = window as any, view = w.view;
          const canvas = document.getElementById("view") as HTMLCanvasElement;
          w.pauseFrames(true);
          await new Promise((r) => setTimeout(r, 50));
          let sync: () => unknown, syncKind: string | undefined;
          if (view.backend === "Gl") {
            const gl = canvas.getContext("webgl2")!;
            const px = new Uint8Array(4);
            sync = () => gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px);
            syncKind = "webgl2 readPixels(1x1)";
          } else {
            const ctx: any = canvas.getContext("webgpu");
            const device = ctx?.getConfiguration?.()?.device;
            let works = false;
            if (device) {
              try {
                await device.queue.onSubmittedWorkDone();
                works = true;
              } catch (e) {
                // Headless Chromium's software WebGPU rejects it ("A valid
                // external Instance reference no longer exists"), as it does
                // the view's snapshot.
                syncKind = `none: onSubmittedWorkDone rejected (${String(e).slice(0, 80)})`;
              }
            }
            if (works) {
              sync = () => device.queue.onSubmittedWorkDone();
              syncKind = "webgpu queue.onSubmittedWorkDone";
            } else {
              syncKind ??= "none: no device from the canvas";
              sync = () => undefined;
            }
          }
          const raf = () => new Promise((r) => requestAnimationFrame(r));
          const run = async (move: (i: number) => void) => {
            const cpu: number[] = [], total: number[] = [], interval: number[] = [];
            let last = 0;
            const start = performance.now();
            // At most `frames`, and at most ~15 s of them past the first 15
            // (software WebGL2 takes seconds a frame at 1M edges).
            for (let i = 0; i < frames + 5 && (i < 20 || performance.now() - start < budgetMs); i++) {
              const ts = (await raf()) as number;
              const t0 = performance.now();
              move(i);
              view.render();
              const t1 = performance.now();
              await sync();
              const t2 = performance.now();
              if (i >= 5) { cpu.push(t1 - t0); total.push(t2 - t0); interval.push(ts - last); } // 5 to warm up
              last = ts;
            }
            return { cpu, total, interval };
          };
          // Small moves back and forth, so the drawing stays in view.
          const pan = await run((i) => view.pan(i % 20 < 10 ? 6 : -6, i % 14 < 7 ? 4 : -4));
          view.fit();
          const zoom = await run((i) => view.zoom(i % 30 < 15 ? 1.05 : 1 / 1.05, canvas.width / 2, canvas.height / 2));
          view.fit();
          // Draw cost without the compositor's pacing: a burst of frames in
          // one task (pan and render each), one sync at the end, per frame.
          const burstRun = async () => {
            const burst: number[] = [];
            for (let k = 0; k < 5; k++) {
              await raf();
              const t0 = performance.now();
              for (let i = 0; i < 10; i++) { view.pan(i % 2 ? 3 : -3, 0); view.render(); }
              await sync();
              burst.push((performance.now() - t0) / 10);
              if (performance.now() - t0 > budgetMs / 3) break;
            }
            view.fit();
            view.render();
            await sync();
            return burst;
          };
          const burst = await burstRun();
          // The highlight layer (#435): the same burst with a highlight set
          // (a path of 100 nodes: wide edges, halos, the rest dimmed), then
          // with a node hidden as well (the edges read the flags). And what
          // setting one costs: its time, and the bytes it uploads.
          const n = view.nodeCount;
          const path = Uint32Array.from({ length: Math.min(100, n) }, (_, i) => Math.floor((i * (n - 1)) / 99));
          const bytes = () => ({ flags: view.flagBytesUploaded, edges: view.highlightBytesUploaded });
          const b0 = bytes();
          const setMs: number[] = [];
          for (let k = 0; k < 20; k++) {
            view.clearHighlights();
            const t0 = performance.now();
            view.setHighlights(Uint32Array.of(0, path.length), path);
            setMs.push(performance.now() - t0);
          }
          const b1 = bytes();
          const highlightBurst = await burstRun();
          const selectMs: number[] = [];
          for (let k = 0; k < 50; k++) {
            const t0 = performance.now();
            view.setSelection(Uint32Array.of((k * 7919) % n));
            selectMs.push(performance.now() - t0);
          }
          const b2 = bytes();
          view.setSelection(new Uint32Array());
          view.setHidden(Uint32Array.of(0));
          const hiddenBurst = await burstRun();
          view.setHidden(new Uint32Array());
          view.clearHighlights();
          const highlight = {
            pathNodes: path.length,
            burstMs: highlightBurst,
            hiddenBurstMs: hiddenBurst,
            setMs,
            // Each of the 20 sets after the first clears and sets again.
            flagBytesPerSet: (b1.flags - b0.flags) / 20,
            edgeBytesPerSet: (b1.edges - b0.edges) / 20,
            selectMs,
            // The old node's flags and the new one's; and the new node again,
            // to draw in front.
            flagBytesPerSelection: (b2.flags - b1.flags) / 50,
            frontBytesPerSelection: (b2.edges - b1.edges) / 50,
          };
          // Picks at pseudo-random pixels (a fixed sequence).
          let seed = 12345;
          const rnd = () => ((seed = (seed * 1103515245 + 12345) >>> 0) / 2 ** 32);
          const pickMs: number[] = [];
          let hits = 0;
          for (let i = 0; i < picks; i++) {
            const x = rnd() * canvas.width, y = rnd() * canvas.height;
            const t = performance.now();
            const hit = view.pick(x, y);
            pickMs.push(performance.now() - t);
            if (hit !== undefined) hits++;
          }
          // Memory.
          const memory: any = { pageWasmBytes: w.wasm.memory.buffer.byteLength, workerWasmBytes: w.stats.workerMemoryBytes };
          const pm = (performance as any).memory;
          if (pm) memory.jsHeapUsedBytes = pm.usedJSHeapSize;
          memory.crossOriginIsolated = crossOriginIsolated;
          if (crossOriginIsolated && (performance as any).measureUserAgentSpecificMemory) {
            try {
              const m = await Promise.race([
                (performance as any).measureUserAgentSpecificMemory(),
                new Promise((_, no) => setTimeout(() => no(new Error("timed out")), 30_000)),
              ]);
              memory.userAgentSpecificBytes = (m as any).bytes;
            } catch (e) {
              memory.userAgentSpecificError = String(e);
            }
          }
          // The frame budget (#495), after the memory is read: ordering the
          // edges here builds a layout's hierarchy on the page, which a
          // page that has a layout worker never does.
          const budgetRun = async (move: (i: number) => void) => {
            const interval: number[] = [];
            let last = 0, started = 0, moving = false;
            const start = performance.now();
            for (let i = 0; i < frames + 20 && (i < 40 || performance.now() - start < budgetMs); i++) {
              const ts = (await raf()) as number;
              if (moving) view.frameCost(ts - started);
              move(i);
              started = ts;
              moving = view.renderFrame(ts);
              if (i >= 20) interval.push(ts - last); // 20 for the budget to find its level
              last = ts;
            }
            const f = view.frameInfo();
            return { interval, edges: f.k, nodes: f.nodes, p95: f.p95, limit: f.limit };
          };
          const tOrder = performance.now();
          const layout = new w.viz.Layout(n, w.graph.src, w.graph.dst);
          const tHierarchy = performance.now();
          const order = layout.edgeOrder(w.graph.src, w.graph.dst);
          const orderMs = performance.now() - tHierarchy;
          layout.free();
          view.setEdgeOrder(order);
          const tWrite = performance.now();
          let writes = 1;
          while (view.orderEdges()) writes++;
          const budget = {
            hierarchyMs: tHierarchy - tOrder,
            orderMs,
            writeMs: performance.now() - tWrite,
            writes,
            pan: await budgetRun((i) => view.pan(i % 20 < 10 ? 6 : -6, i % 14 < 7 ? 4 : -4)),
            zoom: (view.fit(), await budgetRun((i) => view.zoom(i % 30 < 15 ? 1.05 : 1 / 1.05, canvas.width / 2, canvas.height / 2))),
          };
          view.fit();
          w.pauseFrames(false);
          return { syncKind, pan, zoom, burstMs: burst, highlight, pickMs, pickHits: hits, memory, budget };
        },
        { frames: FRAMES, picks: PICKS, budgetMs: FRAME_BUDGET_MS },
      ),
    );
    for (const k of ["pan", "zoom"]) {
      const r = result[k];
      result[k] = { cpuMs: summary(r.cpu), frameMs: summary(r.total), rafIntervalMs: summary(r.interval) };
    }
    for (const k of ["pan", "zoom"]) result.budget[k] = { ...result.budget[k], interval: undefined, rafIntervalMs: summary(result.budget[k].interval) };
    result.pickMs = summary(result.pickMs);
    result.burstMsPerFrame = summary(result.burstMs);
    delete result.burstMs;
    const h = result.highlight;
    result.highlight = {
      ...h,
      burstMs: undefined,
      hiddenBurstMs: undefined,
      burstMsPerFrame: summary(h.burstMs),
      hiddenBurstMsPerFrame: summary(h.hiddenBurstMs),
      setMs: summary(h.setMs),
      selectMs: summary(h.selectMs),
    };
  }
  await page.close();
  return result;
}

const results: any[] = [];
const backendsRan: Record<string, string> = {};
for (const backend of BACKENDS) {
  const browser = await launch(backend);
  for (const graph of GRAPHS)
    for (const size of SIZES)
      for (const mode of MODES) {
        const t = performance.now();
        let r: any;
        try {
          r = await runCase(browser, backend, graph, size, mode);
        } catch (e) {
          r = { graph, size, mode, error: String(e).split("\n")[0] };
        }
        r.requested = backend;
        if (r.backend) backendsRan[backend] = r.backend;
        results.push(r);
        const f = (x: any) => (x == null ? "–" : typeof x === "number" ? x.toFixed(1) : x);
        console.log(
          r.error
            ? `${backend} ${graph} ${size} ${mode}: ${r.error}`
            : `${backend}→${r.backend} ${graph} ${size} ${mode}: ${r.nodes} nodes, ${r.edges} edges · first frame ${f(r.firstFrameMs)} ms · layout ${f(r.layoutDoneMs)} ms` +
                (r.msPerIteration != null ? ` (${f(r.msPerIteration)} ms/it)` : "") +
                (r.pan ? ` · pan ${f(r.pan.frameMs.median)}/${f(r.pan.frameMs.p95)} ms · zoom ${f(r.zoom.frameMs.median)}/${f(r.zoom.frameMs.p95)} ms · pick ${f(r.pickMs.median)} ms` : "") +
                (r.highlight
                  ? ` · burst ${f(r.burstMsPerFrame.median)} ms, highlighted ${f(r.highlight.burstMsPerFrame.median)} ms, with a hidden node ${f(r.highlight.hiddenBurstMsPerFrame.median)} ms · set highlight ${r.highlight.setMs.median} ms (${r.highlight.flagBytesPerSet} + ${r.highlight.edgeBytesPerSet} B) · select ${r.highlight.selectMs.median} ms (${r.highlight.flagBytesPerSelection} + ${r.highlight.frontBytesPerSelection} B)`
                  : "") +
                (r.errors.length ? ` · errors: ${r.errors.join("; ").slice(0, 200)}` : "") +
                ` [${((performance.now() - t) / 1000).toFixed(0)} s]`,
        );
      }
  await browser.close();
}
server.stop();

// The layout alone, natively: one thread (as the browser runs it) and all.
const native: any = {};
if (process.env.NATIVE !== "0") {
  const sizes = SIZES.join(",");
  for (const [label, features] of [["oneThread", ["--no-default-features"]], ["allThreads", []]] as const) {
    const p = spawnSync(
      ["cargo", "run", "-q", "--release", "--locked", "-p", "ursa-core", ...features, "--example", "fa2_multilevel", "--", "bench", sizes, GRAPHS.join(",")],
      { cwd: repo, stderr: "inherit" },
    );
    const text = p.stdout.toString();
    try {
      native[label] = JSON.parse(text.slice(text.indexOf("{")));
    } catch {
      native[label] = { error: `exit ${p.exitCode}: ${text.slice(0, 200)}` };
    }
    console.log(`native ${label}: ${JSON.stringify(native[label]).slice(0, 400)}…`);
  }
}

const chromiumVersion = await (async () => {
  try {
    const b = await chromium.launch(process.env.CHROME ? { executablePath: process.env.CHROME } : {});
    const v = b.version();
    await b.close();
    return v;
  } catch {
    return null;
  }
})();
const report = {
  date: new Date().toISOString(),
  machine: {
    platform: `${platform()} ${release()}`,
    cpu: cpus()[0]?.model,
    cores: cpus().length,
    memoryBytes: totalmem(),
    chromium: chromiumVersion,
    headless: !HEADED,
    backends: backendsRan,
  },
  method: {
    frames: FRAMES,
    picks: PICKS,
    viewport: "1000x700 at devicePixelRatio 1",
    frameTiming: "wall time per frame: view.pan or view.zoom, view.render, then a GPU sync point (see syncKind); no GPU timestamp queries",
    flatIterations: FLAT_ITERATIONS,
  },
  browser: results,
  native,
};
await Bun.write(out, JSON.stringify(report, null, 2) + "\n");
console.log(`\nwrote ${out}`);
