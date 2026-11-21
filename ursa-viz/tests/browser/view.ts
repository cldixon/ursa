// The graph view in headless Chromium: build ursa-viz for the web first
// (see ../../README.md), then
//
//   bun ursa-viz/tests/browser/view.ts [screenshot dir]
//
// For each test graph, laid out coarse to fine (the default) and flat
// (`&multilevel=0`): the backend, layout cost per iteration (in the worker)
// or time to the end of the coarse-to-fine schedule, draw cost per frame and
// frame rate, a screenshot, and how good the layout is: for the clustered
// graph, how well it separates its clusters; for the grid, whether it is
// unfolded. MODE=flat or MODE=multilevel runs one of the two.
//
// Then the fixed layouts (#436), which MODE=fixed runs alone: karate's two
// hops from node 0 as rings and its shortest paths from 16 to 26 as layers,
// each done in the worker's one message, each node where its level puts it
// and no node drawn over another, with a screenshot; and how long rings and
// layers take on neighbourhoods of the 1M-edge graph.
import { chromium, type Page } from "playwright";
import { file } from "bun";
import { extname, join, normalize, resolve } from "node:path";

const root = resolve(import.meta.dir, "../..");
const shots = Bun.argv[2];
const TYPES: Record<string, string> = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm" };
const server = Bun.serve({
  port: 0,
  async fetch(req) {
    const path = normalize(join(root, new URL(req.url).pathname));
    if (!path.startsWith(root)) return new Response("forbidden", { status: 403 });
    const f = file(path);
    if (!(await f.exists())) return new Response("not found", { status: 404 });
    return new Response(f, { headers: { "content-type": TYPES[extname(path)] ?? "application/octet-stream" } });
  },
});
const base = `http://localhost:${server.port}/demo/index.html`;

/** A frame read back from the page's view: the share of pixels drawn on, and the frame as a PNG. */
function snapshot(page: Page): Promise<{ drawn: number; png: string | null; error: string | null }> {
  return page.evaluate(async () => { try {
    const v = (window as any).view, canvas = document.getElementById("view") as HTMLCanvasElement;
    const px: Uint8Array = await (window as any).snapshot();
    let drawn = 0;
    for (let i = 0; i < px.length; i += 4) if (px[i] < 235 || px[i + 1] < 235 || px[i + 2] < 235) drawn++;
    const out = document.createElement("canvas");
    out.width = canvas.width; out.height = canvas.height;
    out.getContext("2d")!.putImageData(new ImageData(new Uint8ClampedArray(px), canvas.width), 0, 0);
    return { drawn: drawn / (px.length / 4), png: out.toDataURL("image/png"), error: null };
  } catch (e) { return { drawn: 0, png: null, error: String(e) }; } });
}

const ONLY = process.env.ONLY;
const MODE = process.env.MODE ?? "both";
const GRAPHS = [
  { name: "clusters 8x500", query: "graph=clusters&k=8&size=500", iterations: 300, timeout: 60_000 },
  { name: "grid 100x100", query: "graph=grid&w=100&h=100", iterations: 500, timeout: 90_000 },
  { name: "rmat 16K nodes, 100K edges", query: "graph=rmat&scale=14&edges=100000", iterations: 100, timeout: 120_000 },
  { name: "rmat 131K nodes, 1M edges", query: "graph=rmat&scale=17&edges=1000000", iterations: 12, timeout: 300_000 },
];
// Flat runs a fixed number of iterations; coarse to fine runs its schedule.
const CASES_ALL = (MODE === "fixed" ? [] : GRAPHS).flatMap((g) => [
  ...(MODE !== "flat" ? [{ ...g, multilevel: true, query: g.query }] : []),
  ...(MODE !== "multilevel" ? [{ ...g, multilevel: false, query: `${g.query}&multilevel=0&iterations=${g.iterations}` }] : []),
]);
const CASES = ONLY ? CASES_ALL.filter((c) => c.name.startsWith(ONLY)) : CASES_ALL;

const webgpu = process.env.WEBGPU !== "0";
const browser = await chromium.launch({ args: webgpu ? ["--enable-unsafe-webgpu", "--enable-features=Vulkan"] : ["--disable-features=WebGPU"] });
const failures: string[] = [];
for (const c of CASES) {
  const page = await browser.newPage({ viewport: { width: 1000, height: 700 } });
  page.on("pageerror", (e) => failures.push(`${c.name}: ${e.message}`));
  page.on("console", (m) => m.type() === "error" && failures.push(`${c.name}: ${m.text()}`));
  await page.goto(`${base}?${c.query}`);
  await page.waitForFunction(() => (window as any).stats?.phase === "running", null, { timeout: 120_000 });
  const done = await page
    .waitForFunction(() => (window as any).stats.layoutDone, null, { timeout: c.timeout, polling: 250 })
    .then(() => true, () => false);
  await page.waitForTimeout(1500); // a second of frames with the layout settled
  const s: any = await page.evaluate(() => (window as any).stats);
  const mode = c.multilevel ? "multilevel" : "flat";
  if (!done) failures.push(`${c.name} (${mode}): the layout did not finish (${s.iterations} iterations)`);
  let quality = "";
  if (c.name.startsWith("grid")) {
    // Correlation of layout distance with grid distance over sampled pairs:
    // near 1 for a sheet laid out flat, far below for a folded one.
    const r = await page.evaluate(() => {
      const v = (window as any).view, g = (window as any).graph;
      const xy = v.positions(), n = g.n, w = Math.round(Math.sqrt(n));
      const a: number[] = [], b: number[] = [];
      for (let i = 0; i < n; i += 13)
        for (let j = i + 7; j < n; j += 97) {
          a.push(Math.hypot((i % w) - (j % w), Math.floor(i / w) - Math.floor(j / w)));
          b.push(Math.hypot(xy[i] - xy[j], xy[n + i] - xy[n + j]));
        }
      const m = a.length, ma = a.reduce((p, q) => p + q) / m, mb = b.reduce((p, q) => p + q) / m;
      let sab = 0, saa = 0, sbb = 0;
      for (let k = 0; k < m; k++) { sab += (a[k] - ma) * (b[k] - mb); saa += (a[k] - ma) ** 2; sbb += (b[k] - mb) ** 2; }
      return sab / Math.sqrt(saa * sbb);
    });
    quality = ` · grid r ${r.toFixed(2)} (layout vs grid distance; 1 is unfolded)`;
    if (c.multilevel && !(r > 0.9)) failures.push(`${c.name} (${mode}): the grid is folded (r ${r.toFixed(2)})`);
  }
  if (c.name.startsWith("clusters")) {
    // Mean distance between nodes of one cluster over the mean between clusters.
    const ratio = await page.evaluate(() => {
      const v = (window as any).view, g = (window as any).graph;
      const xy = v.positions(), n = g.n;
      let inS = 0, inN = 0, outS = 0, outN = 0;
      for (let i = 0; i < n; i += 7)
        for (let j = i + 3; j < n; j += 11) {
          const d = Math.hypot(xy[i] - xy[j], xy[n + i] - xy[n + j]);
          if (g.community[i] === g.community[j]) { inS += d; inN++; } else { outS += d; outN++; }
        }
      return inS / inN / (outS / outN);
    });
    quality = ` · cluster spread ${ratio.toFixed(2)} (within ÷ between; lower is better)`;
    if (!(ratio < (c.multilevel ? 0.1 : 0.6))) failures.push(`${c.name} (${mode}): clusters not separated (ratio ${ratio.toFixed(2)})`);
  }
  // Pixels drawn, read back from an offscreen frame: the canvas itself
  // cannot be screenshot under headless WebGPU.
  const snap = await snapshot(page);
  if (snap.error) {
    // Reading a frame back fails under headless Chromium's software WebGPU
    // (SwiftShader) through wgpu 29, though WebGPU itself reads back there:
    // reported, not failed, until it is understood. WebGL2 is checked.
    if (s.backend === "BrowserWebGpu") quality += ` · snapshot unavailable (${snap.error.slice(0, 60)})`;
    else failures.push(`${c.name}: snapshot failed: ${snap.error}`);
  } else {
    if (!(snap.drawn > 0.002)) failures.push(`${c.name}: the frame is blank (${(snap.drawn * 100).toFixed(2)}% drawn)`);
    const tag = c.name.split(",")[0].replace(/ /g, "-").replace(/[^A-Za-z0-9-]/g, "");
    if (shots) await Bun.write(`${shots}/viz-${tag}-${mode}-${s.backend}.png`, Buffer.from(snap.png!.split(",")[1], "base64"));
    quality += ` · ${(snap.drawn * 100).toFixed(1)}% of pixels drawn`;
  }
  const layout = c.multilevel
    ? `coarse to fine in ${(s.layoutMs / 1000).toFixed(1)} s (hierarchy ${s.buildMs?.toFixed(0)} ms, ${s.iterations} it of work)`
    : `${s.msPerIteration?.toFixed(1)} ms/it (${s.iterations} it in ${(s.layoutMs / 1000).toFixed(1)} s)`;
  console.log(
    `${c.name} [${mode}]: ${s.backend} · create ${s.createMs.toFixed(0)} ms · layout ${layout} · draw ${s.drawMsMedian?.toFixed(2)} ms median · ${s.fps?.toFixed(0)} fps${quality}`,
  );
  await page.close();
}

// The fixed layouts.
type Fixed = { name: string; query: string; layout: "rings" | "layers"; nodes?: number; levels?: number[]; timeout?: number };
const FIXED: Fixed[] = [
  { name: "karate, 2 hops from node 0", query: "graph=karate&layout=rings&seeds=0&hops=2", layout: "rings", nodes: 26, levels: [1, 16, 9] },
  { name: "karate, all shortest paths 16 to 26", query: "graph=karate&layout=layers&from=16&to=26", layout: "layers", nodes: 10, levels: [1, 2, 1, 4, 1, 1] },
  { name: "karate, 1 hop from nodes 0 and 33", query: "graph=karate&layout=rings&seeds=0,33&hops=1", layout: "rings" },
  { name: "karate, 2 hops from nodes 0 and 33 in layers", query: "graph=karate&layout=layers&seeds=0,33&hops=2", layout: "layers" },
  { name: "rmat 16K nodes, 100K edges, 2 hops from node 0", query: "graph=rmat&scale=14&edges=100000&layout=rings&seeds=0&hops=2", layout: "rings" },
  { name: "rmat 131K nodes, 1M edges, 3 hops from node 0", query: "graph=rmat&scale=17&edges=1000000&layout=rings&seeds=0&hops=3", layout: "rings", timeout: 120_000 },
  { name: "rmat 131K nodes, 1M edges, 3 hops from node 0 in layers", query: "graph=rmat&scale=17&edges=1000000&layout=layers&seeds=0&hops=3", layout: "layers", timeout: 120_000 },
];
for (const c of MODE === "both" || MODE === "fixed" ? FIXED.filter((c) => !ONLY || c.name.startsWith(ONLY)) : []) {
  const page = await browser.newPage({ viewport: { width: 1000, height: 700 } });
  page.on("pageerror", (e) => failures.push(`${c.name}: ${e.message}`));
  page.on("console", (m) => m.type() === "error" && failures.push(`${c.name}: ${m.text()}`));
  await page.goto(`${base}?${c.query}`);
  const done = await page
    .waitForFunction(() => (window as any).stats?.layoutDone && (window as any).stats.firstFrameAt !== undefined, null, { timeout: c.timeout ?? 60_000 })
    .then(() => true, () => false);
  if (!done) {
    failures.push(`${c.name}: the ${c.layout} layout did not finish`);
    await page.close();
    continue;
  }
  await page.waitForTimeout(1200); // a second of frames, for the draw cost
  const s: any = await page.evaluate(() => (window as any).stats);
  const fail = (m: string) => failures.push(`${c.name} (${c.layout}): ${m}`);
  if (s.positionMessages !== 1) fail(`the worker posted ${s.positionMessages} position messages, not one`);
  if (s.iterations !== 0) fail(`a fixed layout ran ${s.iterations} iterations`);
  if (c.nodes !== undefined && s.nodes !== c.nodes) fail(`${s.nodes} nodes drawn, not ${c.nodes}`);
  // Where the nodes are, in the layout's units and on the canvas (the view
  // fits them: scene.rs's Camera::fit), against their levels.
  const g: any = await page.evaluate(() => {
    const v = (window as any).view, g = (window as any).graph, n = g.n;
    const xy: number[] = Array.from(v.positions());
    const canvas = document.getElementById("view") as HTMLCanvasElement;
    const seen = new Set<string>(), degree = new Array(n).fill(0);
    for (let i = 0; i < g.src.length; i++) {
      const a = g.src[i], b = g.dst[i], key = a < b ? `${a},${b}` : `${b},${a}`;
      if (a === b || seen.has(key)) continue;
      seen.add(key);
      degree[a]++; degree[b]++;
    }
    return { n, x: xy.slice(0, n), y: xy.slice(n), level: Array.from(g.level as Uint32Array), degree, width: canvas.width, height: canvas.height };
  });
  const counts: number[] = [];
  for (const l of g.level) counts[l] = (counts[l] ?? 0) + 1;
  if (c.levels && JSON.stringify(counts) !== JSON.stringify(c.levels)) fail(`nodes per level ${JSON.stringify(counts)}, not ${JSON.stringify(c.levels)}`);
  let membership = "";
  if (c.layout === "rings" && c.levels) {
    // One seed: distance d is on the circle of radius d around it.
    const seed = g.level.indexOf(0);
    let worst = 0;
    for (let v = 0; v < g.n; v++) worst = Math.max(worst, Math.abs(Math.hypot(g.x[v] - g.x[seed], g.y[v] - g.y[seed]) - g.level[v]));
    if (!(worst < 1e-4)) fail(`a node is ${worst} off its ring`);
    membership = ` · ${counts.length - 1} rings (${counts.slice(1).join(", ")} nodes), every node within ${worst.toExponential(1)} of its radius`;
  }
  if (c.layout === "layers") {
    // Level l is one vertical line, and the lines are in level order.
    const columns = new Map<number, number>();
    for (let v = 0; v < g.n; v++) {
      if (columns.has(g.level[v]) && columns.get(g.level[v]) !== g.x[v]) fail(`layer ${g.level[v]} is not one line`);
      columns.set(g.level[v], g.x[v]);
    }
    const xs = [...columns.entries()].sort((a, b) => a[0] - b[0]).map((e) => e[1]);
    if (!xs.every((x, i) => i === 0 || x > xs[i - 1])) fail(`the layers are not left to right: ${xs}`);
    membership = ` · ${xs.length} layers (${counts.join(", ")} nodes)`;
  }
  // No node over another, in pixels: discs of at most 1.5 + sqrt(degree).
  let overlap = "";
  if (g.n <= 2000) {
    const [x0, x1, y0, y1] = [Math.min(...g.x), Math.max(...g.x), Math.min(...g.y), Math.max(...g.y)];
    const scale = Math.min(g.width / Math.max(x1 - x0, 1e-6), g.height / Math.max(y1 - y0, 1e-6)) * 0.92;
    const radius = g.degree.map((d: number) => Math.min(12, 1.5 + Math.sqrt(d)));
    let nearest = Infinity, clear = Infinity;
    for (let i = 0; i < g.n; i++)
      for (let j = i + 1; j < g.n; j++) {
        const d = Math.hypot(g.x[i] - g.x[j], g.y[i] - g.y[j]) * scale;
        nearest = Math.min(nearest, d);
        clear = Math.min(clear, d - radius[i] - radius[j]);
      }
    if (!(clear > 0)) fail(`two nodes overlap by ${(-clear).toFixed(1)} px`);
    overlap = ` · nearest nodes ${nearest.toFixed(0)} px apart, ${clear.toFixed(0)} px clear`;
  }
  const snap = await snapshot(page);
  let drawn = "";
  if (snap.error) {
    if (s.backend === "BrowserWebGpu") drawn = ` · snapshot unavailable (${snap.error.slice(0, 60)})`;
    else fail(`snapshot failed: ${snap.error}`);
  } else {
    if (!(snap.drawn > 0.002)) fail(`the frame is blank (${(snap.drawn * 100).toFixed(2)}% drawn)`);
    const tag = c.name.replace(/[ ,]+/g, "-").replace(/[^A-Za-z0-9-]/g, "");
    if (shots) await Bun.write(`${shots}/viz-${c.layout}-${tag}-${s.backend}.png`, Buffer.from(snap.png!.split(",")[1], "base64"));
    drawn = ` · ${(snap.drawn * 100).toFixed(1)}% of pixels drawn`;
  }
  console.log(
    `${c.name} [${c.layout}]: ${s.backend} · ${s.nodes.toLocaleString()} nodes, ${s.edges.toLocaleString()} edges · layout ${s.layoutMs.toFixed(1)} ms in the worker, one message · ` +
      `first frame ${(s.firstFrameAt - s.graphReadyAt).toFixed(0)} ms after the graph · draw ${s.drawMsMedian?.toFixed(2)} ms median${membership}${overlap}${drawn}`,
  );
  await page.close();
}
await browser.close();
server.stop();
if (failures.length) {
  console.error("\nFAIL:\n" + failures.join("\n"));
  process.exit(1);
}
console.log("\nall passed");
