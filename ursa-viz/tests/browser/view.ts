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
// Then the highlight layer (#435), on the karate club in the light theme and
// the dark, always on WebGL2 (the backend with the least to draw with, and
// the one whose frames read back here): a path of five edges reads back as
// a 3-pixel line in the highlight colour with its nodes at full strength and
// everything else at 35%; a route's ends and a selected node carry their
// rings; a hidden node and its edges are gone; and a selection of one node
// uploads 24 bytes. ONLY=highlight runs these alone.
//
// Then the fixed layouts (#436), which MODE=fixed runs alone: karate's two
// hops from node 0 as rings and its shortest paths from 16 to 26 as layers,
// each done in the worker's one message, each node where its level puts it
// and no node drawn over another, with a screenshot; and how long rings and
// layers take on neighbourhoods of the 1M-edge graph.
//
// Then scenes that change in place (#441), on WebGL2 again. On the karate
// club, a view made from a result takes deltas: new nodes and edges change
// only the pixels around them and upload only their bytes, also when the
// buffers double to hold them; removed nodes and edges are gone; a delta
// that does not fit changes nothing; and after each delta's inverse the
// frame read back is the first frame, byte for byte. On R-MAT with 1M
// edges, a delta of 100 nodes and 300 edges uploads at most 64 KB and takes
// at most 2 ms. ONLY=delta runs these alone (ONLY="delta karate" the first).
//
// Then making a view (#426), on WebGL2: at 46K nodes and 1M edges no task
// on the page's thread is over 60 ms, the page's wasm memory is at most
// 20 MiB afterwards, and the view has no adjacency until a layout on its
// own thread steps. ONLY=create runs these alone.
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

// The highlight layer.
type RGB = [number, number, number];
const THEMES: Record<string, { background: RGB; node: RGB; highlight: RGB; selection: RGB; pin: RGB }> = {
  // scene.rs's Theme::light() and Theme::dark().
  light: { background: [0xff, 0xff, 0xff], node: [0x2a, 0x2a, 0x2a], highlight: [0xd9, 0x48, 0x0f], selection: [0x0b, 0x63, 0xce], pin: [0x7a, 0x3f, 0xc0] },
  dark: { background: [0x0b, 0x0b, 0x0a], node: [0xe8, 0xe4, 0xdc], highlight: [0xff, 0xa9, 0x4d], selection: [0x6c, 0xb2, 0xff], pin: [0xc9, 0xa3, 0xff] },
};
// Five edges of the karate club, end to end.
const PATH = [16, 5, 0, 2, 32, 14];
const HIGHLIGHT_CASES = ["light", "dark"].filter((t) => !ONLY || `highlight ${t}`.startsWith(ONLY));
if (HIGHLIGHT_CASES.length) {
  const gl = await chromium.launch({ args: ["--disable-features=WebGPU"] });
  for (const themeName of HIGHLIGHT_CASES) {
    const name = `highlight ${themeName}`;
    const fail = (what: string) => failures.push(`${name}: ${what}`);
    const page = await gl.newPage({ viewport: { width: 1000, height: 700 }, deviceScaleFactor: 1 });
    page.on("pageerror", (e) => fail(e.message));
    page.on("console", (m) => m.type() === "error" && fail(m.text()));
    await page.goto(`${base}?graph=karate&theme=${themeName}`);
    await page.waitForFunction(() => (window as any).stats?.layoutDone, null, { timeout: 60_000, polling: 100 });
    await page.waitForTimeout(500); // the last positions drawn
    const r: any = await page.evaluate(
      async ({ theme, path }) => {
        const w = window as any, view = w.view, g = w.graph;
        const canvas = document.getElementById("view") as HTMLCanvasElement;
        const W = canvas.width, H = canvas.height;
        w.pauseFrames(true);
        await new Promise((r) => setTimeout(r, 100));
        const out: any = { backend: view.backend, problems: [] as string[], pngs: {} as Record<string, string> };
        const problem = (what: string) => out.problems.push(what);
        const frame = async (tag: string) => {
          const px: Uint8Array = await view.snapshot();
          const c = document.createElement("canvas");
          c.width = W; c.height = H;
          c.getContext("2d")!.putImageData(new ImageData(new Uint8ClampedArray(px), W), 0, 0);
          out.pngs[tag] = c.toDataURL("image/png");
          return px;
        };
        const at = (px: Uint8Array, x: number, y: number): number[] => {
          const i = 4 * (Math.min(H - 1, Math.max(0, Math.floor(y))) * W + Math.min(W - 1, Math.max(0, Math.floor(x))));
          return [px[i], px[i + 1], px[i + 2]];
        };
        const far = (a: number[], b: number[]) => Math.max(...a.map((v, k) => Math.abs(v - b[k])));
        const mix = (a: number[], b: number[], t: number) => a.map((v, k) => v + (b[k] - v) * t);
        // How much of `color` over the background the pixel `p` is: 0 to 1.
        const share = (p: number[], color: number[]) => {
          const d = color.map((v, k) => v - theme.background[k]);
          return p.reduce((s, v, k) => s + (v - theme.background[k]) * d[k], 0) / d.reduce((s, v) => s + v * v, 0);
        };
        const n = g.n, m = g.src.length;
        const at_node = Array.from({ length: n }, (_, i) => Array.from(view.nodePixel(i) as Float32Array));
        // Radii as the view draws them by default: 1.5 + sqrt(degree), at most 12.
        const neighbours = Array.from({ length: n }, () => new Set<number>());
        for (let e = 0; e < m; e++) { neighbours[g.src[e]].add(g.dst[e]); neighbours[g.dst[e]].add(g.src[e]); }
        const radius = neighbours.map((s) => Math.min(1.5 + Math.sqrt(s.size), 12));
        const onPath = new Set<number>(path);
        const steps = path.slice(1).map((v: number, k: number) => [path[k], v]);
        const toSegment = (p: number[], a: number[], b: number[]) => {
          const dx = b[0] - a[0], dy = b[1] - a[1];
          const t = Math.max(0, Math.min(1, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / (dx * dx + dy * dy || 1)));
          return Math.hypot(p[0] - a[0] - t * dx, p[1] - a[1] - t * dy);
        };
        const nearPath = (p: number[]) => Math.min(...steps.map(([a, b]: number[]) => toSegment(p, at_node[a], at_node[b])));
        const nearNode = (p: number[], but: number[] = []) =>
          Math.min(...at_node.map((q, i) => (but.includes(i) ? Infinity : Math.hypot(p[0] - q[0], p[1] - q[1]) - radius[i])));

        const before = await frame("plain");
        const flagBytes0 = view.flagBytesUploaded, edgeBytes0 = view.highlightBytesUploaded, positions0 = w.stats.uploadBytes;
        view.setHighlights(Uint32Array.of(0, path.length), Uint32Array.from(path));
        out.flagBytes = view.flagBytesUploaded - flagBytes0;
        out.edgeBytes = view.highlightBytesUploaded - edgeBytes0;
        if (w.stats.uploadBytes !== positions0) problem("highlighting uploaded positions");
        const lit = await frame("path");

        // 1. Each step of the path is a 3-pixel line in the highlight colour.
        out.widths = [];
        for (const [a, b] of steps) {
          const [pa, pb] = [at_node[a], at_node[b]];
          const len = Math.hypot(pb[0] - pa[0], pb[1] - pa[1]);
          if (len < 40) { problem(`step ${a}-${b} is only ${len.toFixed(0)} px long: too short to measure`); continue; }
          const mid = [(pa[0] + pb[0]) / 2, (pa[1] + pb[1]) / 2];
          const normal = [-(pb[1] - pa[1]) / len, (pb[0] - pa[0]) / len];
          const centre = at(lit, mid[0], mid[1]);
          if (far(centre, theme.highlight) > 6) problem(`step ${a}-${b}: its middle is rgb(${centre}), not the highlight colour`);
          // The highlight colour's share of each pixel across the line, summed: its width.
          let width = 0;
          for (let t = -8; t <= 8; t += 0.125) width += 0.125 * Math.max(0, Math.min(1, share(at(lit, mid[0] + t * normal[0], mid[1] + t * normal[1]), theme.highlight)));
          out.widths.push(Math.round(width * 100) / 100);
          if (!(width > 2.4 && width < 3.6)) problem(`step ${a}-${b}: ${width.toFixed(2)} px wide, not 3`);
          if (far(at(before, mid[0], mid[1]), theme.highlight) < 40) problem(`step ${a}-${b}: highlighted before it was set`);
        }

        // 2. The path's nodes are at full strength, the others at 35%.
        let dimmed = 0;
        for (let i = 0; i < n; i++) {
          const p = at_node[i];
          // Only nodes drawn clear of the others (their centre was the node colour).
          if (far(at(before, p[0], p[1]), theme.node) > 6) continue;
          const now = at(lit, p[0], p[1]);
          if (onPath.has(i)) {
            if (far(now, theme.node) > 6) problem(`path node ${i} is rgb(${now}), not the node colour at full strength`);
          } else if (nearPath(p) > radius[i] + 4) {
            dimmed++;
            const want = mix(theme.background, theme.node, 0.35);
            if (far(now, want) > 8) problem(`node ${i} is rgb(${now}), not dimmed to 35% (rgb(${want.map(Math.round)}))`);
          }
        }
        out.dimmedNodes = dimmed;
        if (dimmed < 15) problem(`only ${dimmed} nodes could be checked for dimming`);

        // 3. The other edges are at 35% too: their ink at their middles, before and after.
        let inkBefore = 0, inkAfter = 0, edgesChecked = 0;
        for (let e = 0; e < m; e++) {
          const a = at_node[g.src[e]], b = at_node[g.dst[e]];
          const mid = [(a[0] + b[0]) / 2, (a[1] + b[1]) / 2];
          if (nearPath(mid) < 8 || nearNode(mid) < 12) continue;
          const ink = (px: Uint8Array) => { const p = at(px, mid[0], mid[1]); return Math.abs(p[0] - theme.background[0]) + Math.abs(p[1] - theme.background[1]) + Math.abs(p[2] - theme.background[2]); };
          inkBefore += ink(before); inkAfter += ink(lit); edgesChecked++;
        }
        out.edgeDim = Math.round((inkAfter / inkBefore) * 1000) / 1000;
        out.edgesChecked = edgesChecked;
        if (edgesChecked < 10) problem(`only ${edgesChecked} edges could be checked for dimming`);
        if (!(out.edgeDim > 0.27 && out.edgeDim < 0.45)) problem(`the other edges are at ${out.edgeDim} of their ink, not 0.35`);

        // 4. The route's ends carry a ring in the highlight colour (3 to 5 px
        // past the disc), on the side away from the path.
        const ringAt = (i: number, awayFrom: number, past: number) => {
          const p = at_node[i], q = at_node[awayFrom];
          const d = Math.hypot(p[0] - q[0], p[1] - q[1]) || 1, r = Math.max(radius[i], 3) + past;
          return [p[0] + ((p[0] - q[0]) / d) * r, p[1] + ((p[1] - q[1]) / d) * r];
        };
        for (const [end, next] of [[path[0], path[1]], [path[path.length - 1], path[path.length - 2]]]) {
          const p = ringAt(end, next, 4);
          const c = at(lit, p[0], p[1]);
          if (far(c, theme.highlight) > 6) problem(`the ring of the route's end ${end} is rgb(${c}), not the highlight colour`);
        }
        // A node inside the path has a halo, not a ring: the highlight
        // colour, faint, 2 px past its disc (looked at where the path is farthest).
        {
          const i = path[2], p = at_node[i], r = Math.max(radius[i], 3) + 2;
          const around = Array.from({ length: 16 }, (_, k) => [p[0] + r * Math.cos((k * Math.PI) / 8), p[1] + r * Math.sin((k * Math.PI) / 8)]);
          const clear = around.sort((a, b) => nearPath(b) - nearPath(a))[0];
          out.halo = Math.round(share(at(lit, clear[0], clear[1]), theme.highlight) * 100) / 100;
          if (!(out.halo > 0.15 && out.halo < 0.6)) problem(`the halo of path node ${i} is ${out.halo} of the highlight colour 2 px out, not about a third`);
        }

        // 5. A selection of one node: a few bytes, a ring in the selection
        // colour, and nothing dimmed by it.
        view.clearHighlights();
        const lone = [...Array(n).keys()].filter((i) => !onPath.has(i)).sort((a, b) => nearNode(at_node[b], [b]) - nearNode(at_node[a], [a]))[0];
        // Everything it uploads: its flags, and the node again to draw in front.
        const uploaded = () => view.flagBytesUploaded + view.highlightBytesUploaded;
        const bytes0 = uploaded();
        view.setSelection(Uint32Array.of(lone));
        out.selectionBytes = uploaded() - bytes0;
        out.nodes = n;
        const selected = await frame("selected");
        {
          const p = at_node[lone];
          // Straight up from the node: 4 px past the disc is inside the ring (3 to 5).
          const ring = at(selected, p[0], p[1] - Math.max(radius[lone], 3) - 4);
          if (far(ring, theme.selection) > 6 && far(at(selected, p[0] - Math.max(radius[lone], 3) - 4, p[1]), theme.selection) > 6)
            problem(`the selection ring of node ${lone} is rgb(${ring}), not the selection colour`);
          let same = 0, differ = 0;
          for (let i = 0; i < before.length; i += 4) {
            const x = (i / 4) % W, y = Math.floor(i / 4 / W);
            if (Math.hypot(x - p[0], y - p[1]) < radius[lone] + 12) continue;
            if (before[i] === selected[i] && before[i + 1] === selected[i + 1] && before[i + 2] === selected[i + 2]) same++; else differ++;
          }
          out.selectionChangedElsewhere = differ;
          if (differ > 0) problem(`selecting node ${lone} changed ${differ} pixels away from it`);
        }

        // 6. A pinned node carries a ring in the pin colour, hugging the disc.
        view.setSelection(new Uint32Array());
        view.setPinned(Uint32Array.of(lone));
        const pinnedFrame = await frame("pinned");
        {
          const p = at_node[lone], r = radius[lone] + 1.25;
          const up = at(pinnedFrame, p[0], p[1] - r), left = at(pinnedFrame, p[0] - r, p[1]);
          if (share(up, theme.pin) < 0.5 && share(left, theme.pin) < 0.5) problem(`the pin ring of node ${lone} is rgb(${up}), not the pin colour`);
        }
        view.setPinned(new Uint32Array());

        // 7. A hidden node is gone, with its edges, and cannot be picked.
        const hub = 33;
        const inkOf = (px: Uint8Array) => { let s = 0; for (let i = 0; i < px.length; i += 4) if (far([px[i], px[i + 1], px[i + 2]], theme.background) > 12) s++; return s; };
        view.setHidden(Uint32Array.of(hub));
        const hiddenFrame = await frame("hidden");
        const c = at(hiddenFrame, at_node[hub][0], at_node[hub][1]);
        if (far(c, theme.node) < 60) problem(`hidden node ${hub} is still drawn: rgb(${c})`);
        if (view.pick(at_node[hub][0], at_node[hub][1]) === hub) problem(`hidden node ${hub} can be picked`);
        out.inkPlain = inkOf(before); out.inkHidden = inkOf(hiddenFrame);
        // Node 33 has 17 of the 78 edges.
        if (!(out.inkHidden < 0.9 * out.inkPlain)) problem(`hiding node ${hub} left ${out.inkHidden} of ${out.inkPlain} inked pixels`);
        // An edge of the hidden node, at its middle, is gone.
        let gone = 0, tried = 0;
        for (let e = 0; e < m; e++) {
          if (g.src[e] !== hub && g.dst[e] !== hub) continue;
          const a = at_node[g.src[e]], b = at_node[g.dst[e]];
          const mid = [(a[0] + b[0]) / 2, (a[1] + b[1]) / 2];
          if (far(at(before, mid[0], mid[1]), theme.background) < 12) continue; // not on a pixel centre before
          tried++;
          if (far(at(hiddenFrame, mid[0], mid[1]), theme.background) < far(at(before, mid[0], mid[1]), theme.background)) gone++;
        }
        out.hiddenEdges = `${gone}/${tried}`;
        if (tried < 5 || gone < tried - 2) problem(`${gone} of ${tried} edges of hidden node ${hub} are gone`);
        view.setHidden(new Uint32Array());
        const again = await frame("again");
        let differ = 0;
        for (let i = 0; i < before.length; i++) if (before[i] !== again[i]) differ++;
        if (differ) problem(`with every flag cleared, ${differ} bytes of the frame differ from the first`);
        w.pauseFrames(false);
        return out;
      },
      { theme: THEMES[themeName], path: PATH },
    ).catch((e) => ({ problems: [String(e)], pngs: {} }));
    for (const p of r.problems) fail(p);
    if (r.backend && r.backend !== "Gl") fail(`ran on ${r.backend}, not WebGL2`);
    // The runs of nodes the path changed, 4 bytes a node; then five edges
    // of 16 bytes and six nodes in front of 20.
    if (!(r.flagBytes > 0 && r.flagBytes <= 4 * r.nodes)) fail(`highlighting the path uploaded ${r.flagBytes} bytes of flags`);
    const instanceBytes = 16 * (PATH.length - 1) + 20 * PATH.length;
    if (r.edgeBytes !== instanceBytes) fail(`highlighting the path uploaded ${r.edgeBytes} bytes of edges and nodes in front, not ${instanceBytes}`);
    if (!(r.selectionBytes > 0 && r.selectionBytes <= r.nodes)) fail(`selecting one node uploaded ${r.selectionBytes} bytes; at most ${r.nodes} (one a node) are allowed`);
    if (shots) for (const [tag, png] of Object.entries(r.pngs as Record<string, string>)) await Bun.write(`${shots}/viz-highlight-${themeName}-${tag}.png`, Buffer.from(png.split(",")[1], "base64"));
    console.log(
      `${name}: ${r.backend} · path widths ${r.widths?.join(", ")} px · ${r.dimmedNodes} other nodes at 35% · other edges at ${r.edgeDim} (${r.edgesChecked} checked) · ` +
        `flags ${r.flagBytes} B and instances ${r.edgeBytes} B for the path · ${r.selectionBytes} B for a selection of one of ${r.nodes} nodes · hidden node's edges gone ${r.hiddenEdges}`,
    );
    await page.close();
  }
  await gl.close();
  // The same layer under WebGPU, whose validation is the stricter: every
  // pipeline of the layer draws (a path, a selection, a pin, a hidden node)
  // with no error from the device. Its frames cannot be read back here.
  if (webgpu) {
    const gpu = await chromium.launch({ args: ["--enable-unsafe-webgpu", "--enable-features=Vulkan"] });
    const page = await gpu.newPage({ viewport: { width: 1000, height: 700 } });
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
    await page.goto(`${base}?graph=karate&highlight=path`);
    await page.waitForFunction(() => (window as any).stats?.layoutDone, null, { timeout: 60_000, polling: 100 });
    const backend = await page.evaluate(async () => {
      const view = (window as any).view;
      const frames = () => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
      await frames();
      view.setSelection(Uint32Array.of(3));
      view.setPinned(Uint32Array.of(7));
      view.setHovered(9);
      await frames();
      view.setHidden(Uint32Array.of(33));
      await frames();
      view.clearHighlights();
      await frames();
      return view.backend;
    });
    await page.waitForTimeout(300);
    for (const e of errors) failures.push(`highlight on ${backend}: ${e}`);
    console.log(`highlight on ${backend}: a path, a selection, a pin and a hidden node drawn with ${errors.length} errors`);
    await gpu.close();
  }
}
// Making a view (#426), on WebGL2: R-MAT's 1M edges over the 46K nodes
// they name (the shell's `.graph rmat`). The view builds no adjacency, so
// making it holds the page's thread for no task over 60 ms and leaves the
// page's wasm memory at 20 MiB or less; the adjacency is built by the first
// step() of a layout on the view's own thread, and not before. A view made
// from a result is held to the same task length.
const CREATE_CASES = ["create rmat 1M", "create result rmat 1M"].filter((c) => !ONLY || c.startsWith(ONLY));
if (CREATE_CASES.length) {
  const gl = await chromium.launch({ args: ["--disable-features=WebGPU"] });
  for (const name of CREATE_CASES) {
    const fail = (what: string) => failures.push(`${name}: ${what}`);
    const fromResult = name.includes("result");
    const runs: any[] = [];
    // The browser's first WebGL2 context starts its GPU process, a second
    // of the page's thread that is not the view's: a small view first.
    const warm = await gl.newPage();
    await warm.goto(`${base}?graph=karate`);
    await warm.waitForFunction(() => (window as any).stats?.phase === "running", null, { timeout: 120_000 });
    await warm.close();
    // Three runs, each a new page. The middle one of their longest tasks is
    // what is held to 60 ms: this machine's other work can only lengthen a
    // task, and one run in three may meet it.
    for (let run = 0; run < 3; run++) {
      const page = await gl.newPage({ viewport: { width: 1000, height: 700 }, deviceScaleFactor: 1 });
      page.on("pageerror", (e) => fail(e.message));
      page.on("console", (m) => m.type() === "error" && fail(m.text()));
      // Flat and no iterations: the worker's layout is not what is measured.
      await page.goto(`${base}?graph=rmat&scale=16&edges=1000000&compact=1&multilevel=0&iterations=0${fromResult ? "&result=1" : ""}`);
      await page.waitForFunction(() => (window as any).stats?.layoutDone, null, { timeout: 300_000, polling: 100 });
      const r: any = await page.evaluate(async (last) => {
        const w = window as any, view = w.view, s = w.stats;
        w.pauseFrames(true);
        // The observer's entries arrive after the tasks they report.
        await new Promise((r) => setTimeout(r, 300));
        const [from, to] = [s.graphReadyAt, s.graphReadyAt + s.createMs];
        const tasks = s.longTasks.filter(([start, ms]: number[]) => start + ms > from && start < to).map(([, ms]: number[]) => ms);
        const out: any = {
          backend: view.backend, nodes: view.nodeCount, edges: view.edgeCount, createMs: s.createMs,
          longest: Math.max(0, ...tasks), tasks: tasks.length, wasmBytes: s.createWasmBytes, topology: view.hasTopology,
        };
        if (last) {
          // What a view does without a layout of its own builds nothing.
          view.setPositions(view.positions());
          view.render();
          view.pick(500, 350);
          out.topologyInUse = view.hasTopology;
          // A layout on the view's own thread builds it, on its first step.
          const t = performance.now();
          view.step(1);
          out.stepMs = performance.now() - t;
          out.topologyStepped = view.hasTopology;
          out.steppedWasmBytes = w.wasm.memory.buffer.byteLength;
          out.iterations = view.iterations;
        }
        return out;
      }, run === 2).catch((e) => ({ error: String(e) }));
      if (r.error) fail(r.error);
      else runs.push(r);
      await page.close();
    }
    if (runs.length < 3) continue;
    const r = runs[2];
    const longest = runs.map((x) => x.longest).sort((a, b) => a - b)[1];
    const wasm = Math.max(...runs.map((x) => x.wasmBytes));
    const mib = (bytes: number) => `${(bytes / 2 ** 20).toFixed(1)} MiB`;
    if (r.backend !== "Gl") fail(`ran on ${r.backend}, not WebGL2`);
    if (r.edges !== 1_000_000) fail(`${r.edges} edges, not 1,000,000`);
    if (!(longest <= 60)) fail(`making the view held the page's thread for ${longest} ms in one task; at most 60 ms are allowed`);
    // A view made from a result (#501) holds the result and no second copy
    // of its edges; here the demo also builds the result's bytes in the
    // same memory (resultFromEdges), 8 MB more than a page given them holds.
    const limit = (fromResult ? 34 : 20) * 2 ** 20;
    if (!(wasm <= limit)) fail(`the page's wasm memory is ${mib(wasm)} after making the view; at most ${mib(limit)} are allowed`);
    if (runs.some((x) => x.topology) || r.topologyInUse) fail("the view built the graph's adjacency without a layout of its own");
    if (!r.topologyStepped || r.iterations !== 1) fail(`step(1) should build the adjacency and run one iteration (built: ${r.topologyStepped}, iterations: ${r.iterations})`);
    console.log(
      `${name}: ${r.backend} · ${r.nodes.toLocaleString()} nodes, ${r.edges.toLocaleString()} edges · the longest task while making the view ${longest} ms, the median of three ` +
        `(${runs.map((x) => `${x.longest} ms of ${x.tasks} over 50, ${x.createMs.toFixed(0)} ms in all`).join("; ")}) · wasm memory after ${mib(wasm)} · ` +
        `no adjacency until step(1): ${r.stepMs.toFixed(0)} ms, then ${mib(r.steppedWasmBytes)}`,
    );
  }
  await gl.close();
}
// Scenes that change in place.
const DELTA_CASES = ["delta karate", "delta rmat 1M"].filter((c) => !ONLY || c.startsWith(ONLY));
if (DELTA_CASES.length) {
  const gl = await chromium.launch({ args: ["--disable-features=WebGPU"] });
  const open = async (name: string, query: string) => {
    const page = await gl.newPage({ viewport: { width: 1000, height: 700 }, deviceScaleFactor: 1 });
    page.on("pageerror", (e) => failures.push(`${name}: ${e.message}`));
    page.on("console", (m) => m.type() === "error" && failures.push(`${name}: ${m.text()}`));
    await page.goto(`${base}?${query}`);
    await page.waitForFunction(() => (window as any).stats?.layoutDone, null, { timeout: 300_000, polling: 100 });
    await page.waitForTimeout(500); // the last positions drawn
    return page;
  };
  if (DELTA_CASES.includes("delta karate")) {
    const name = "delta karate";
    const fail = (what: string) => failures.push(`${name}: ${what}`);
    // 1. The result has positions, so new nodes bring theirs: inside the
    // drawing, where they do not move the camera.
    let page = await open(name, "graph=karate&result=xy");
    const r: any = await page.evaluate(async (theme) => {
      const w = window as any, view = w.view, viz = w.viz, g = w.graph;
      const canvas = document.getElementById("view") as HTMLCanvasElement;
      const W = canvas.width, H = canvas.height, n = g.n, m = g.src.length;
      w.pauseFrames(true);
      await new Promise((r) => setTimeout(r, 100));
      const out: any = { backend: view.backend, problems: [] as string[], pngs: {} as Record<string, string> };
      const problem = (what: string) => out.problems.push(what);
      const frame = async (tag: string) => {
        const px: Uint8Array = await view.snapshot();
        const c = document.createElement("canvas");
        c.width = W; c.height = H;
        c.getContext("2d")!.putImageData(new ImageData(new Uint8ClampedArray(px), W), 0, 0);
        out.pngs[tag] = c.toDataURL("image/png");
        return px;
      };
      const differ = (a: Uint8Array, b: Uint8Array) => { let d = 0; for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) d++; return d; };
      const far = (a: number[], b: number[]) => Math.max(...a.map((v, k) => Math.abs(v - b[k])));
      const at = (px: Uint8Array, p: number[]) => { const i = 4 * (Math.floor(p[1]) * W + Math.floor(p[0])); return [px[i], px[i + 1], px[i + 2]]; };
      const pixel = (i: number) => Array.from(view.nodePixel(i) as Float32Array);
      const toSegment = (p: number[], a: number[], b: number[]) => {
        const dx = b[0] - a[0], dy = b[1] - a[1];
        const t = Math.max(0, Math.min(1, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / (dx * dx + dy * dy || 1)));
        return Math.hypot(p[0] - a[0] - t * dx, p[1] - a[1] - t * dy);
      };
      // The changed pixels farther than `reach` from every one of `near`
      // (points, and segments as pairs of points).
      const strays = (a: Uint8Array, b: Uint8Array, points: number[][], segments: number[][][], reach: number) => {
        let changed = 0, stray = 0;
        for (let i = 0; i < a.length; i += 4) {
          if (a[i] === b[i] && a[i + 1] === b[i + 1] && a[i + 2] === b[i + 2] && a[i + 3] === b[i + 3]) continue;
          changed++;
          const p = [((i / 4) % W) + 0.5, Math.floor(i / 4 / W) + 0.5];
          if (points.some((q) => Math.hypot(p[0] - q[0], p[1] - q[1]) <= reach)) continue;
          if (segments.some(([q, r]) => toSegment(p, q, r) <= 2.5)) continue;
          stray++;
        }
        return { changed, stray };
      };
      const first = await frame("first");
      const info0 = JSON.stringify(view.info());
      out.info0 = view.info();
      const undo = (delta: Uint8Array, positions?: Float32Array) => view.applyDelta(viz.invertDelta(delta), positions);
      const same = async (what: string) => {
        const d = differ(first, await frame(`after ${what}`));
        if (d) problem(`after ${what} and its inverse, ${d} bytes of the frame differ from the first`);
        if (JSON.stringify(view.info()) !== info0) problem(`after ${what} and its inverse, info() is ${JSON.stringify(view.info())}`);
      };

      // Three nodes between others, a ring through them, and two edges to old nodes.
      const xy = view.positions();
      const between = (a: number, b: number, t: number) => [xy[a] + (xy[b] - xy[a]) * t, xy[n + a] + (xy[n + b] - xy[n + a]) * t];
      const spots = [between(0, 33, 0.5), between(5, 26, 0.5), between(16, 14, 0.4)];
      const grow = () => new viz.DeltaBuilder()
        .addNodes(n, 3, Float32Array.from([...spots.map((p) => p[0]), ...spots.map((p) => p[1])]))
        .addEdges(Uint32Array.of(n, n + 1, n + 2, n, 11), Uint32Array.of(0, n, n + 1, n + 2, n + 2))
        .toBytes();
      const old = [0, 11].map(pixel);
      const growth = grow();
      let applied = view.applyDelta(growth);
      out.growth = { nodes: applied.nodes, edges: applied.edges, bytes: applied.bytesUploaded, grew: applied.grew, nodeCapacity: applied.nodeCapacity, edgeCapacity: applied.edgeCapacity };
      if (view.nodeCount !== n + 3 || view.edgeCount !== m + 5) problem(`the view has ${view.nodeCount} nodes and ${view.edgeCount} edges after the delta`);
      const grown = await frame("grown");
      out.infoGrown = view.info();
      const fresh = [n, n + 1, n + 2].map(pixel);
      if (far(pixel(0), old[0]) > 0) problem("the delta moved the camera, though its nodes are inside the drawing");
      for (const [k, p] of fresh.entries()) {
        const c = at(grown, p);
        if (far(c, theme.node) > 6) problem(`new node ${n + k} is rgb(${c}) at its centre, not the node colour`);
        if (view.pick(p[0], p[1]) !== n + k) problem(`new node ${n + k} cannot be picked`);
      }
      // What changed is around the new nodes and edges, and the two old
      // nodes that grew with their degree: what the buffers held is intact.
      const ends = [[fresh[0], old[0]], [fresh[1], fresh[0]], [fresh[2], fresh[1]], [fresh[0], fresh[2]], [old[1], fresh[2]]];
      out.grownPixels = strays(first, grown, [...fresh, ...old], ends, 14);
      if (!out.grownPixels.changed) problem("the delta drew nothing");
      if (out.grownPixels.stray) problem(`the delta changed ${out.grownPixels.stray} pixels away from what it added`);
      applied = undo(growth);
      out.shrinkBytes = applied.bytesUploaded;
      await same("a growth");

      // The same again draws the same.
      applied = view.applyDelta(grow());
      out.regrowBytes = applied.bytesUploaded;
      if (differ(grown, await frame("grown again"))) problem("the same delta drew another frame the second time");
      undo(growth);
      await same("a second growth");

      // More than the buffers have room for: they double, what they held
      // is copied on the GPU, and only the delta's own bytes are uploaded.
      // A hundred nodes along the line from node 16 to node 14, each with
      // an edge to the next and one to an old node.
      const many = 100, line = Array.from({ length: many }, (_, k) => between(16, 14, (k + 1) / (many + 1)));
      const bigSrc = new Uint32Array(2 * many), bigDst = new Uint32Array(2 * many);
      for (let k = 0; k < many; k++) { bigSrc[2 * k] = n + k; bigDst[2 * k] = k + 1 < many ? n + k + 1 : 14; bigSrc[2 * k + 1] = n + k; bigDst[2 * k + 1] = k % n; }
      const big = new viz.DeltaBuilder().addNodes(n, many, Float32Array.from([...line.map((p) => p[0]), ...line.map((p) => p[1])])).addEdges(bigSrc, bigDst).toBytes();
      applied = view.applyDelta(big);
      out.big = { bytes: applied.bytesUploaded, grew: applied.grew, nodeCapacity: applied.nodeCapacity, edgeCapacity: applied.edgeCapacity };
      const bigger = await frame("grown past the buffers");
      const added = Array.from({ length: many }, (_, k) => pixel(n + k)), all = Array.from({ length: n }, (_, i) => pixel(i));
      out.bigPixels = strays(first, bigger, [...added, ...all], Array.from({ length: 2 * many }, (_, e) => [pixel(bigSrc[e]), pixel(bigDst[e])]), 14);
      if (out.bigPixels.changed < 1000) problem(`100 new nodes and 200 edges changed only ${out.bigPixels.changed} pixels`);
      if (out.bigPixels.stray) problem(`growing the buffers changed ${out.bigPixels.stray} pixels away from what was added`);
      undo(big);
      await same("a growth past the buffers");
      // Back in the grown buffers, the small growth draws what it drew.
      view.applyDelta(grow());
      if (differ(grown, await frame("grown in the grown buffers"))) problem("after the buffers grew, the same delta drew another frame");
      undo(growth);

      // A node far outside the drawing: the camera, which follows, fits
      // it in, and fits back when it goes.
      const reach = Math.max(...Array.from(xy.subarray(0, n))) - Math.min(...Array.from(xy.subarray(0, n)));
      const outlier = new viz.DeltaBuilder().addNodes(n, 1, Float32Array.of(xy[0] + 3 * reach, xy[n])).addEdges(Uint32Array.of(n), Uint32Array.of(0)).toBytes();
      view.applyDelta(outlier);
      out.cameraMoved = Math.hypot(pixel(0)[0] - old[0][0], pixel(0)[1] - old[0][1]);
      if (!(out.cameraMoved > 50)) problem(`a node far outside the drawing moved the camera by ${out.cameraMoved.toFixed(1)} px`);
      const [fx, fy] = pixel(n);
      if (!(fx > 0 && fx < W && fy > 0 && fy < H)) problem(`the far node is drawn at ${fx.toFixed(0)}, ${fy.toFixed(0)}: outside the canvas`);
      if (!differ(first, await frame("with a far node"))) problem("a far node changed no pixel");
      undo(outlier);
      await same("a node outside the drawing");

      // A removed node is hidden as `setHidden` hides it: four bytes.
      const hub = 33;
      const hide = new viz.DeltaBuilder().removeNodes(Uint32Array.of(hub)).toBytes();
      out.hideBytes = view.applyDelta(hide).bytesUploaded;
      const hidden = await frame("hidden");
      out.infoHidden = view.info();
      if (far(at(hidden, pixel(hub)), theme.node) < 60) problem(`removed node ${hub} is still drawn`);
      if (view.pick(...pixel(hub)) === hub) problem(`removed node ${hub} can be picked`);
      undo(hide);
      await same("removing a node");
      view.setHidden(Uint32Array.of(hub));
      if (differ(hidden, await frame("set hidden"))) problem("a removed node is not drawn as a hidden one is");
      view.setHidden(new Uint32Array());

      // Removed edges are gone, and nothing else: eight bytes each.
      const cut = [0, 20, 40, 77];
      const remove = new viz.DeltaBuilder().removeEdges(Uint32Array.from(cut)).toBytes();
      out.cutBytes = view.applyDelta(remove).bytesUploaded;
      const without = await frame("cut");
      out.cutPixels = strays(first, without, [], cut.map((e) => [pixel(g.src[e]), pixel(g.dst[e])]), 0);
      if (out.cutPixels.changed < 40) problem(`removing four edges changed only ${out.cutPixels.changed} pixels`);
      if (out.cutPixels.stray) problem(`removing four edges changed ${out.cutPixels.stray} pixels away from them`);
      undo(remove);
      await same("removing edges");

      // A delta that does not fit throws and changes nothing.
      let threw = "";
      try { view.applyDelta(new viz.DeltaBuilder().addNodes(n, 1, Float32Array.of(0, 0)).addEdges(Uint32Array.of(n), Uint32Array.of(999)).toBytes()); } catch (e) { threw = String(e); }
      out.threw = threw;
      if (!threw.includes("999")) problem(`a delta naming node 999 did not say so: "${threw}"`);
      if (view.nodeCount !== n) problem(`a refused delta left ${view.nodeCount} nodes`);
      await same("a refused delta");
      w.pauseFrames(false);
      return out;
    }, THEMES.light).catch((e) => ({ problems: [String(e)], pngs: {} }));
    for (const p of r.problems) fail(p);
    if (r.backend && r.backend !== "Gl") fail(`ran on ${r.backend}, not WebGL2`);
    // Three nodes of 16 bytes and 4 of flags, five edges of 8, and the
    // radii of the two old nodes whose degree changed.
    const growthBytes = 3 * 20 + 5 * 8 + 2 * 4;
    if (r.growth?.bytes !== growthBytes) fail(`the growth uploaded ${r.growth?.bytes} bytes, not ${growthBytes}`);
    // A view of a result starts with room: 128 nodes and 256 edges here.
    if (!(r.growth && !r.growth.grew && r.growth.nodeCapacity === 128 && r.growth.edgeCapacity === 256)) fail(`the growth should fit the buffers as made: ${JSON.stringify(r.growth)}`);
    if (!(r.big?.grew && r.big.nodeCapacity === 256 && r.big.edgeCapacity === 512)) fail(`the buffers did not double for 100 nodes and 200 edges: ${JSON.stringify(r.big)}`);
    if (r.big?.bytes > 100 * 20 + 200 * 8 + 34 * 4) fail(`the growth that doubled the buffers uploaded ${r.big?.bytes} bytes: more than its own`);
    if (r.regrowBytes !== growthBytes) fail(`the second growth uploaded ${r.regrowBytes} bytes, not ${growthBytes}`);
    if (r.shrinkBytes !== 8) fail(`undoing the growth uploaded ${r.shrinkBytes} bytes, not the 8 of two radii`);
    if (r.hideBytes !== 4) fail(`removing a node uploaded ${r.hideBytes} bytes, not 4`);
    if (r.cutBytes !== 32) fail(`removing four edges uploaded ${r.cutBytes} bytes, not 32`);
    if (r.info0 && !(r.info0.nodes === 34 && r.info0.edges === 78 && r.info0.size.column === null && r.info0.color === null)) fail(`info() of the karate club: ${JSON.stringify(r.info0)}`);
    if (r.infoGrown && !(r.infoGrown.nodes === 37 && r.infoGrown.edges === 83 && r.infoGrown.nodeSlots === 37)) fail(`info() after the growth: ${JSON.stringify(r.infoGrown)}`);
    // Node 33 has 17 of the 78 edges.
    if (r.infoHidden && !(r.infoHidden.nodes === 33 && r.infoHidden.hiddenNodes === 1 && r.infoHidden.edges === 61 && r.infoHidden.nodeSlots === 34)) fail(`info() with node 33 removed: ${JSON.stringify(r.infoHidden)}`);
    if (shots) for (const [tag, png] of Object.entries(r.pngs as Record<string, string>)) await Bun.write(`${shots}/viz-delta-${tag.replace(/ /g, "-")}.png`, Buffer.from(png.split(",")[1], "base64"));
    console.log(
      `${name}: ${r.backend} · growth of 3 nodes and 5 edges ${r.growth?.bytes} B, ${r.grownPixels?.changed} pixels changed, ${r.grownPixels?.stray} astray · of 100 nodes and 200 edges ${r.big?.bytes} B (buffers doubled to ${r.big?.nodeCapacity} nodes, ${r.big?.edgeCapacity} edges), ${r.bigPixels?.changed} pixels changed, ${r.bigPixels?.stray} astray · ` +
        `undo ${r.shrinkBytes} B · removed node ${r.hideBytes} B · four removed edges ${r.cutBytes} B, ${r.cutPixels?.changed} pixels changed, ${r.cutPixels?.stray} astray · every inverse restored the first frame: ${r.problems.length === 0}`,
    );
    await page.close();

    // 2. The result has no positions: the view places the new nodes, the
    // camera follows, a layout moves everything, and the undo puts every
    // node back with the positions saved before the step.
    page = await open(name, "graph=karate&result=1");
    const u: any = await page.evaluate(async (theme) => {
      const w = window as any, view = w.view, viz = w.viz, g = w.graph, n = g.n;
      const canvas = document.getElementById("view") as HTMLCanvasElement;
      const W = canvas.width;
      w.pauseFrames(true);
      await new Promise((r) => setTimeout(r, 100));
      const problems: string[] = [];
      const differ = (a: Uint8Array, b: Uint8Array) => { let d = 0; for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) d++; return d; };
      const first: Uint8Array = await view.snapshot();
      const saved: Float32Array = view.positions();
      const delta = new viz.DeltaBuilder().addNodes(n, 4).addEdges(Uint32Array.of(n, n + 1, n + 2, n + 3), Uint32Array.of(0, 0, n, 33)).toBytes();
      const applied = view.applyDelta(delta);
      const grown: Uint8Array = await view.snapshot();
      const placed: Float32Array = view.positions();
      if (!placed.every(Number.isFinite)) problems.push("a new node has no place");
      let drawn = 0;
      for (let k = 0; k < 4; k++) {
        const [x, y] = view.nodePixel(n + k), i = 4 * (Math.floor(y) * W + Math.floor(x));
        if (Math.max(Math.abs(grown[i] - theme.node[0]), Math.abs(grown[i + 1] - theme.node[1]), Math.abs(grown[i + 2] - theme.node[2])) <= 40) drawn++;
      }
      if (drawn < 3) problems.push(`only ${drawn} of 4 placed nodes are drawn where the view says they are`);
      // A layout's next positions: everything moves.
      view.setPositions(placed.map((v, i) => (i < placed.length / 2 ? v * 1.3 + 0.2 : v * 0.8 - 0.1)));
      const moved = differ(first, await view.snapshot());
      if (!moved) problems.push("moving every node changed no pixel");
      const undone = view.applyDelta(viz.invertDelta(delta), saved);
      const d = differ(first, await view.snapshot());
      if (d) problems.push(`after a growth, a layout and the undo with its saved positions, ${d} bytes of the frame differ from the first`);
      if (view.nodeCount !== n) problems.push(`the undo left ${view.nodeCount} nodes`);
      w.pauseFrames(false);
      return { problems, bytes: applied.bytesUploaded, undoBytes: undone.bytesUploaded, moved };
    }, THEMES.light).catch((e) => ({ problems: [String(e)] }));
    for (const p of u.problems) fail(p);
    console.log(`${name}, placed by the view: growth ${u.bytes} B · undo with the step's positions ${u.undoBytes} B (8 a node) · the first frame restored: ${u.problems.length === 0}`);
    await page.close();
  }
  if (DELTA_CASES.includes("delta rmat 1M")) {
    const name = "delta rmat 1M";
    const fail = (what: string) => failures.push(`${name}: ${what}`);
    // Flat, one iteration: the layout is not what is measured.
    const page = await open(name, "graph=rmat&scale=17&edges=1000000&result=1&multilevel=0&iterations=1");
    const r: any = await page.evaluate(async () => {
      const w = window as any, view = w.view, viz = w.viz, g = w.graph, n = g.n;
      w.pauseFrames(true);
      await new Promise((r) => setTimeout(r, 200));
      const first: Uint8Array = await view.snapshot();
      // An expansion: 100 nodes off one hub, each with two more edges.
      let state = 441;
      const next = () => (state = (Math.imul(state, 1103515245) + 12345) >>> 0) % n;
      const src = new Uint32Array(300), dst = new Uint32Array(300);
      for (let i = 0; i < 100; i++) for (let k = 0; k < 3; k++) { src[3 * i + k] = n + i; dst[3 * i + k] = k ? next() : g.src[0]; }
      const delta = new viz.DeltaBuilder().addNodes(n, 100).addEdges(src, dst).toBytes();
      const inverse = viz.invertDelta(delta);
      const timed = (f: () => any) => { const t = performance.now(); const v = f(); return [v, performance.now() - t]; };
      const median = (v: number[]) => [...v].sort((a, b) => a - b)[Math.floor(v.length / 2)];
      // The first one, cold.
      const [grew, growMs] = timed(() => view.applyDelta(delta));
      const [, drawGrownMs] = timed(() => view.render());
      view.applyDelta(inverse);
      const applyMs: number[] = [], undoMs: number[] = [], drawMs: number[] = [], plainMs: number[] = [];
      let applied: any;
      for (let i = 0; i < 30; i++) {
        // A frame is seconds here, with no GPU: only a few are drawn.
        if (i < 2) plainMs.push(timed(() => view.render())[1]);
        let ms;
        [applied, ms] = timed(() => view.applyDelta(delta));
        applyMs.push(ms);
        if (i < 2) drawMs.push(timed(() => view.render())[1]);
        undoMs.push(timed(() => view.applyDelta(inverse))[1]);
      }
      const [, infoMs] = timed(() => view.info());
      const last: Uint8Array = await view.snapshot();
      let differ = 0;
      for (let i = 0; i < first.length; i++) if (first[i] !== last[i]) differ++;
      w.pauseFrames(false);
      return {
        backend: view.backend, nodes: n, edges: g.src.length, deltaBytes: delta.length,
        grew: grew.grew, growBytes: grew.bytesUploaded, growMs, drawGrownMs, nodeCapacity: grew.nodeCapacity, edgeCapacity: grew.edgeCapacity,
        bytes: applied.bytesUploaded, regrew: applied.grew, applyMs: median(applyMs), applyMax: Math.max(...applyMs), undoMs: median(undoMs),
        drawMs: median(drawMs), plainMs: median(plainMs), infoMs, differ,
      };
    }).catch((e) => ({ error: String(e) }));
    if (r.error) fail(r.error);
    else {
      if (r.backend !== "Gl") fail(`ran on ${r.backend}, not WebGL2`);
      for (const [what, bytes] of [["the first delta", r.growBytes], ["a delta", r.bytes]]) if (!(bytes > 0 && bytes <= 64 * 1024)) fail(`${what} uploaded ${bytes} bytes; at most 64 KB are allowed`);
      // A view of a result has room for it: 1,048,576 edges.
      if (r.grew || r.regrew) fail(`the buffers should have room for the delta: first ${r.grew}, later ${r.regrew}`);
      // The medians: the first call also pays for the wasm's first run.
      for (const [what, ms] of [["a delta", r.applyMs], ["its inverse", r.undoMs]]) if (!(ms <= 2)) fail(`${what} took ${ms.toFixed(2)} ms outside the frame; at most 2 ms are allowed`);
      if (r.differ) fail(`after 31 deltas and their inverses, ${r.differ} bytes of the frame differ from the first`);
      console.log(
        `${name}: ${r.backend} · ${r.nodes.toLocaleString()} nodes, ${r.edges.toLocaleString()} edges · a delta of 100 nodes and 300 edges (${r.deltaBytes} B of UGR): ${r.bytes} B uploaded, ` +
          `applyDelta ${r.applyMs.toFixed(2)} ms median (${r.applyMax.toFixed(2)} max), its inverse ${r.undoMs.toFixed(2)} ms · the frame after it ${r.drawMs.toFixed(1)} ms against ${r.plainMs.toFixed(1)} plain · ` +
          `the first delta: ${r.growBytes} B, ${r.growMs.toFixed(2)} ms, its frame ${r.drawGrownMs.toFixed(1)} ms (buffers of ${r.nodeCapacity.toLocaleString()} nodes and ${r.edgeCapacity.toLocaleString()} edges) · info() ${r.infoMs.toFixed(2)} ms · first frame restored: ${r.differ === 0}`,
      );
    }
    await page.close();
  }
  await gl.close();
  // The same under WebGPU, whose validation is the stricter: buffers that
  // grow (copies on the GPU), slots written, removed nodes and edges, and
  // their inverses, with no error from the device.
  if (webgpu && DELTA_CASES.includes("delta karate")) {
    const gpu = await chromium.launch({ args: ["--enable-unsafe-webgpu", "--enable-features=Vulkan"] });
    const page = await gpu.newPage({ viewport: { width: 1000, height: 700 } });
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
    await page.goto(`${base}?graph=karate&result=1`);
    await page.waitForFunction(() => (window as any).stats?.layoutDone, null, { timeout: 60_000, polling: 100 });
    const backend = await page.evaluate(async () => {
      const w = window as any, view = w.view, viz = w.viz, n = w.graph.n;
      const frames = () => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
      await frames();
      const saved = view.positions();
      const deltas = [
        new viz.DeltaBuilder().addNodes(n, 200).addEdges(Uint32Array.from({ length: 400 }, (_, i) => n + (i % 200)), Uint32Array.from({ length: 400 }, (_, i) => (i * 7) % n)).toBytes(),
        new viz.DeltaBuilder().removeNodes(Uint32Array.of(33, n + 1)).removeEdges(Uint32Array.of(0, 5, 100)).toBytes(),
      ];
      if (!view.applyDelta(deltas[0]).grew) throw new Error("200 nodes did not grow the buffers");
      await frames();
      view.applyDelta(deltas[1]);
      await frames();
      view.applyDelta(viz.invertDelta(deltas[1]));
      await frames();
      view.applyDelta(viz.invertDelta(deltas[0]), saved);
      await frames();
      return view.backend;
    });
    await page.waitForTimeout(300);
    for (const e of errors) failures.push(`delta on ${backend}: ${e}`);
    console.log(`delta on ${backend}: buffers grown, slots written, nodes and edges removed and all undone with ${errors.length} errors`);
    await gpu.close();
  }
}
server.stop();
if (failures.length) {
  console.error("\nFAIL:\n" + failures.join("\n"));
  process.exit(1);
}
console.log("\nall passed");
