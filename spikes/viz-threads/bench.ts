// The threaded-layout spike's measurement and its determinism check (#503).
// Build first (./build.sh), then:
//
//   bun bench.ts check                # the determinism check (CI runs this)
//   bun bench.ts bench [out.json]     # the measurement
//
// Both serve web/ with the shell's own security headers (web/src/headers.ts:
// its CSP, COOP and COEP), open it in headless Chromium, and run the layout
// in a fresh worker a job: the unthreaded build, and the threaded one on
// pools of each size. `check` also serves the page without COOP/COEP, to see
// that the pool is not used there.
//
// Environment: SIZES=100k,1m · THREADS=1,2,4 · REPEATS=3 · NATIVE=0 ·
// CHROME=/path/to/chrome (else Playwright's Chromium).
import { chromium, type Page } from "playwright";
import { file, spawnSync } from "bun";
import { cpus, loadavg, totalmem } from "node:os";
import { extname, join, normalize, sep } from "node:path";
import { mkdirSync, writeFileSync } from "node:fs";
import { withSecurityHeaders } from "../../web/src/headers";

const here = import.meta.dir;
const web = join(here, "web");
const mode = Bun.argv[2] ?? "check";
const list = (name: string, all: string[]) => (process.env[name] ? process.env[name]!.split(",") : all);
const cores = cpus().length;
const TYPES: Record<string, string> = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".wasm": "application/wasm" };

/** web/ on a free port: with the shell's headers, or with none of them. */
function serve(isolated: boolean) {
  return Bun.serve({
    port: 0,
    async fetch(req) {
      let path = decodeURIComponent(new URL(req.url).pathname);
      if (path === "/") path = "/index.html";
      if (path === "/favicon.ico") return new Response(null, { status: 204 });
      const full = normalize(join(web, path));
      const f = file(full);
      if (!full.startsWith(web + sep) || !(await f.exists())) return new Response("not found", { status: 404 });
      const res = new Response(f, { headers: { "content-type": TYPES[extname(full)] ?? "application/octet-stream" } });
      return isolated ? withSecurityHeaders(res) : res;
    },
  });
}

type Run = { buildMs: number; layoutMs: number; iterations: number; hash: string };
type Result = {
  ok: boolean; error?: string; graph: string; build: string; threads: number; poolThreads: number; isolated: boolean;
  fellBack: string | null; initMs: number; poolMs: number; startMs: number; workerMs: number; memoryBytes: number; shared: boolean; runs: Run[];
};

const browser = await chromium.launch({ executablePath: process.env.CHROME || undefined });
const problems: string[] = [];

async function open(isolated: boolean): Promise<{ page: Page; close: () => Promise<void> }> {
  const server = serve(isolated);
  const page = await browser.newPage();
  page.on("console", (m) => (m.type() === "error" || m.type() === "warning") && problems.push(`console ${m.type()}: ${m.text()}`));
  if (process.env.DEBUG) page.on("console", (m) => console.log(`[${m.type()}] ${m.text()}`));
  page.on("pageerror", (e) => problems.push(`page error: ${e.message}`));
  await page.goto(`http://localhost:${server.port}/`);
  await page.waitForFunction("window.ready === true");
  return { page, close: async () => { await page.close(); server.stop(true); } };
}

const run = async (page: Page, job: object): Promise<Result> => {
  const r = (await page.evaluate((j) => (window as any).run(j), job)) as Result;
  if (!r.ok) throw new Error(`${JSON.stringify(job)}: ${r.error}`);
  return r;
};

function native(bin: string, sizes: string[], threads: number[], repeats: number) {
  const p = spawnSync([join(here, "bin", bin), sizes.join(","), threads.join(","), String(repeats)], { stdout: "pipe", stderr: "inherit" });
  if (p.exitCode !== 0) throw new Error(`${bin} failed`);
  return p.stdout.toString().trim().split("\n").map((l) => JSON.parse(l));
}

const median = (xs: number[]) => { const s = [...xs].sort((a, b) => a - b); return s[Math.floor(s.length / 2)]; };

if (mode === "check") {
  // Determinism: for each graph, every build at every pool size gives the
  // same bits as the unthreaded wasm build; and natively, rayon on every
  // pool size gives the same bits as the serial kernels.
  const sizes = list("SIZES", ["10k", "100k"]);
  const threads = list("THREADS", ["1", "2", "3", "4", "8"]).map(Number);
  let failed = false;
  const fail = (msg: string) => { failed = true; console.error(`FAIL ${msg}`); };

  const { page, close } = await open(true);
  const pick = await page.evaluate(() => (window as any).pick());
  if (!pick.isolated) fail("the page is not cross-origin isolated under the shell's headers");
  console.log(`isolated page picks ${JSON.stringify(pick)} (${cores} cores)`);
  for (const graph of sizes) {
    const base = await run(page, { graph, want: { build: "serial" }, repeats: 2 });
    if (base.shared) fail(`${graph}: the unthreaded build's memory is shared`);
    const want = base.runs[0].hash;
    if (base.runs[1].hash !== want) fail(`${graph}: the unthreaded build is not repeatable`);
    console.log(`${graph} wasm serial      ${want}`);
    for (const t of threads) {
      const r = await run(page, { graph, want: { build: "threads", threads: t }, repeats: 2 });
      if (r.build !== "threads" || !r.shared || r.poolThreads !== t) fail(`${graph}: asked for ${t} threads, got ${r.build} with ${r.poolThreads} (${r.fellBack})`);
      for (const one of r.runs) if (one.hash !== want) fail(`${graph}: ${t} threads gave ${one.hash}, unthreaded ${want}`);
      console.log(`${graph} wasm threads=${t}   ${r.runs.map((x) => x.hash).join(" ")}`);
    }
    // The page's own choice.
    const auto = await run(page, { graph });
    if (auto.build !== pick.build || auto.runs[0].hash !== want) fail(`${graph}: the page's choice ran ${auto.build} and gave ${auto.runs[0].hash}`);
  }
  await close();

  // Without COOP/COEP there is no SharedArrayBuffer: the unthreaded build
  // runs, even when the threaded one is asked for.
  const plain = await open(false);
  const pick2 = await plain.page.evaluate(() => (window as any).pick());
  if (pick2.isolated || pick2.build !== "serial") fail(`a page that is not isolated picks ${JSON.stringify(pick2)}`);
  const fb = await run(plain.page, { graph: sizes[0], want: { build: "threads", threads: 2 } });
  if (fb.build !== "serial" || fb.shared) fail(`a page that is not isolated ran ${fb.build}`);
  console.log(`not isolated: picks ${JSON.stringify(pick2)}, ran ${fb.build} ${fb.runs[0].hash}`);
  await plain.close();

  if (process.env.NATIVE !== "0") {
    for (const graph of sizes) {
      const [serial] = native("bench-serial", [graph], [1], 1);
      console.log(`${graph} native serial    ${serial.hash}`);
      for (const r of native("bench-threads", [graph], threads, 1)) {
        if (r.hash !== serial.hash) fail(`${graph}: native rayon on ${r.threads} threads gave ${r.hash}, serial ${serial.hash}`);
        console.log(`${graph} native threads=${r.threads} ${r.hash}`);
      }
    }
  }
  for (const p of problems) fail(p);
  await browser.close();
  if (failed) process.exit(1);
  console.log("ok: the same bits at every thread count");
} else {
  const out = Bun.argv[3] ?? join(here, "results", "results.json");
  const sizes = list("SIZES", ["100k", "1m"]);
  const threads = list("THREADS", ["1", "2", "4"]).map(Number);
  const repeats = Number(process.env.REPEATS ?? 3);
  const rows: object[] = [];
  const { page, close } = await open(true);
  for (const graph of sizes) {
    // Each repeat is a fresh worker (start-up is measured each time), and
    // the builds are interleaved so that a burst of load from elsewhere on
    // the machine lands on all of them.
    const jobs = [{ build: "serial", threads: 1 }, ...threads.map((t) => ({ build: "threads", threads: t }))];
    const got = new Map<string, Result[]>();
    for (let i = 0; i < repeats; i++) {
      for (const want of jobs) {
        const load = loadavg()[0];
        const r = await run(page, { graph, want });
        const key = `${want.build}/${want.threads}`;
        got.set(key, [...(got.get(key) ?? []), r]);
        rows.push({ where: "chromium", graph, build: r.build, threads: r.threads, initMs: r.initMs, poolMs: r.poolMs, startMs: r.startMs,
          buildMs: r.runs[0].buildMs, layoutMs: r.runs[0].layoutMs, iterations: r.runs[0].iterations, hash: r.runs[0].hash,
          memoryBytes: r.memoryBytes, load1: load });
        console.log(`${graph} ${key} start ${r.startMs.toFixed(0)} ms (pool ${r.poolMs.toFixed(0)}), build ${r.runs[0].buildMs.toFixed(0)} ms, layout ${r.runs[0].layoutMs.toFixed(0)} ms, ${(r.memoryBytes / 2 ** 20).toFixed(0)} MB, ${r.runs[0].hash}, load ${load.toFixed(1)}`);
      }
    }
    const base = median(got.get("serial/1")!.map((r) => r.runs[0].layoutMs));
    for (const [key, rs] of got) {
      const ms = rs.map((r) => r.runs[0].layoutMs);
      console.log(`  ${graph} ${key}: layout median ${median(ms).toFixed(0)} ms (min ${Math.min(...ms).toFixed(0)}), x${(base / median(ms)).toFixed(2)}`);
    }
  }
  await close();
  if (process.env.NATIVE !== "0") {
    for (const graph of sizes) {
      for (let i = 0; i < repeats; i++) {
        const load = loadavg()[0];
        for (const r of [...native("bench-serial", [graph], [1], 1), ...native("bench-threads", [graph], threads, 1)]) {
          rows.push({ ...r, load1: load });
          console.log(`${graph} native ${r.build}/${r.threads} build ${r.buildMs} ms, layout ${r.layoutMs} ms, ${r.hash}, load ${load.toFixed(1)}`);
        }
      }
    }
  }
  await browser.close();
  mkdirSync(join(out, ".."), { recursive: true });
  writeFileSync(out, JSON.stringify({ cores, cpu: cpus()[0]?.model, memoryGB: Math.round(totalmem() / 2 ** 30), chromium: browser.version(), rows, problems }, null, 2) + "\n");
  console.log(`wrote ${out}`);
  if (problems.length) console.log(problems.join("\n"));
}
