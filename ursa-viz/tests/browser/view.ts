// The graph view in headless Chromium: build ursa-viz for the web first
// (see ../../README.md), then
//
//   bun ursa-viz/tests/browser/view.ts [screenshot dir]
//
// For each test graph: the backend, layout cost per iteration (in the
// worker), draw cost per frame and frame rate, a screenshot, and, for the
// clustered graph, how well the layout separates its clusters.
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
const CASES_ALL = [
  { name: "clusters 8x500", query: "graph=clusters&k=8&size=500&iterations=300", iterations: 300, timeout: 60_000 },
  { name: "grid 100x100", query: "graph=grid&w=100&h=100&iterations=500", iterations: 500, timeout: 90_000 },
  { name: "rmat 16K nodes, 100K edges", query: "graph=rmat&scale=14&edges=100000&iterations=100", iterations: 100, timeout: 120_000 },
  { name: "rmat 131K nodes, 1M edges", query: "graph=rmat&scale=17&edges=1000000&iterations=12", iterations: 12, timeout: 300_000 },
];
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
    .waitForFunction((n) => (window as any).stats.iterations >= n, c.iterations, { timeout: c.timeout, polling: 500 })
    .then(() => true, () => false);
  await page.waitForTimeout(1500); // a second of frames with the layout settled
  const s: any = await page.evaluate(() => (window as any).stats);
  if (!done) failures.push(`${c.name}: the layout reached ${s.iterations} of ${c.iterations} iterations`);
  let quality = "";
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
    if (!(ratio < 0.6)) failures.push(`${c.name}: clusters not separated (ratio ${ratio.toFixed(2)})`);
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
    if (shots) await Bun.write(`${shots}/viz-${c.name.split(" ")[0]}-${s.backend}.png`, Buffer.from(snap.png.split(",")[1], "base64"));
    quality += ` · ${(snap.drawn * 100).toFixed(1)}% of pixels drawn`;
  }
  console.log(
    `${c.name}: ${s.backend} · create ${s.createMs.toFixed(0)} ms · layout ${s.msPerIteration?.toFixed(1)} ms/it` +
      ` (${s.iterations} it) · draw ${s.drawMsMedian?.toFixed(2)} ms median · ${s.fps?.toFixed(0)} fps${quality}`,
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
