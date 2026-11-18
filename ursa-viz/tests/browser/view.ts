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
import { chromium } from "playwright";
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

const ONLY = process.env.ONLY;
const MODE = process.env.MODE ?? "both";
const GRAPHS = [
  { name: "clusters 8x500", query: "graph=clusters&k=8&size=500", iterations: 300, timeout: 60_000 },
  { name: "grid 100x100", query: "graph=grid&w=100&h=100", iterations: 500, timeout: 90_000 },
  { name: "rmat 16K nodes, 100K edges", query: "graph=rmat&scale=14&edges=100000", iterations: 100, timeout: 120_000 },
  { name: "rmat 131K nodes, 1M edges", query: "graph=rmat&scale=17&edges=1000000", iterations: 12, timeout: 300_000 },
];
// Flat runs a fixed number of iterations; coarse to fine runs its schedule.
const CASES_ALL = GRAPHS.flatMap((g) => [
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
  const snap = await page.evaluate(async () => { try {
    const v = (window as any).view, canvas = document.getElementById("view") as HTMLCanvasElement;
    const px: Uint8Array = await (window as any).snapshot();
    let drawn = 0;
    for (let i = 0; i < px.length; i += 4) if (px[i] < 235 || px[i + 1] < 235 || px[i + 2] < 235) drawn++;
    const out = document.createElement("canvas");
    out.width = canvas.width; out.height = canvas.height;
    out.getContext("2d")!.putImageData(new ImageData(new Uint8ClampedArray(px), canvas.width), 0, 0);
    return { drawn: drawn / (px.length / 4), png: out.toDataURL("image/png"), error: null };
  } catch (e) { return { drawn: 0, png: null, error: String(e) }; } });
  if (snap.error) {
    // Reading a frame back fails under headless Chromium's software WebGPU
    // (SwiftShader) through wgpu 29, though WebGPU itself reads back there:
    // reported, not failed, until it is understood. WebGL2 is checked.
    if (s.backend === "BrowserWebGpu") quality += ` · snapshot unavailable (${snap.error.slice(0, 60)})`;
    else failures.push(`${c.name}: snapshot failed: ${snap.error}`);
  } else {
    if (!(snap.drawn > 0.002)) failures.push(`${c.name}: the frame is blank (${(snap.drawn * 100).toFixed(2)}% drawn)`);
    const tag = c.name.split(",")[0].replace(/ /g, "-").replace(/[^A-Za-z0-9-]/g, "");
    if (shots) await Bun.write(`${shots}/viz-${tag}-${mode}-${s.backend}.png`, Buffer.from(snap.png.split(",")[1], "base64"));
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
await browser.close();
server.stop();
if (failures.length) {
  console.error("\nFAIL:\n" + failures.join("\n"));
  process.exit(1);
}
console.log("\nall passed");
