// Browser parity test (#278): run ../getting_started.sql in the wasm engine in
// headless Chromium and in the native `ursa` binary, and require byte-identical
// output. Also reports load and query timings.
//
//   bun ursa-wasm/tests/browser/parity.ts <wasm-bindgen --target web dir> <ursa binary>
import { chromium } from "playwright";
import { spawnSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { startServer } from "./server";

const [pkgArg, ursaArg] = Bun.argv.slice(2);
if (!pkgArg || !ursaArg) {
  console.error("usage: bun parity.ts <pkg dir> <ursa binary>");
  process.exit(2);
}
const here = import.meta.dir;
const data = resolve(here, "../../../python/ursa/datasets/data");
const script = await Bun.file(join(here, "../getting_started.sql")).text();

// Native.
const native = spawnSync(resolve(ursaArg), ["--file", join(here, "../getting_started.sql")], {
  cwd: data,
  encoding: "utf8",
});
if (native.status !== 0) {
  console.error("native ursa failed:\n" + native.stderr);
  process.exit(1);
}

// A query for the structured API: `run` then `export` as CSV, against `--format csv`.
const EXPORT_SQL =
  "CREATE VIEW t AS SELECT * FROM 'karate_edges.csv'; " +
  "CREATE PROPERTY GRAPH k EDGE TABLES (t SOURCE KEY (src) DESTINATION KEY (dst)); " +
  "SELECT * FROM pagerank(k, direction => 'both') ORDER BY id";
const nativeCsv = spawnSync(resolve(ursaArg), ["--format", "csv", "-c", EXPORT_SQL], { cwd: data, encoding: "utf8" });
if (nativeCsv.status !== 0) {
  console.error("native ursa --format csv failed:\n" + nativeCsv.stderr);
  process.exit(1);
}

// Browser.
const server = startServer({ pkg: resolve(pkgArg), data });
const browser = await chromium.launch();
const page = await browser.newPage();
page.on("console", (m) => {
  if (m.type() === "error") console.error("[browser]", m.text());
});
// A Rust panic surfaces as an uncaught "unreachable" and the query never settles:
// fail at once rather than wait for the timeout.
page.on("pageerror", (e) => {
  console.error("FAIL: the page threw: " + e.message);
  process.exit(1);
});
setTimeout(() => {
  console.error("FAIL: no result within 5 minutes");
  process.exit(1);
}, 5 * 60_000).unref();
await page.goto(server.url.href);
const result = await page.evaluate(async ([sql, exportSql]) => {
  const t0 = performance.now();
  const mod = await import("/pkg/ursa_wasm.js");
  await mod.default();
  const t1 = performance.now();
  const session = new mod.Session();
  session.setBaseUrl(new URL("/data/", location.href).href);
  try {
    const out = await session.sql(sql);
    const t2 = performance.now();
    // A second run of a graph query, with the index already built.
    await session.sql("SELECT count(*) FROM pagerank(karate)");
    const t3 = performance.now();
    await session.sql("SELECT count(*) FROM pagerank(karate)");
    const t4 = performance.now();
    // The structured API: run keeps the results; export writes one.
    const other = new mod.Session();
    const described = JSON.parse(await other.run(exportSql, 5));
    const last = described.length - 1;
    const csv = new TextDecoder().decode(other.export(last, "csv"));
    const shape = { statements: described.length, columns: described[last].columns.map((c) => c.name), shown: described[last].rows.length + described[last].tail.length, total: described[last].totalRows };
    return { out, csv, shape, load: t1 - t0, script: t2 - t1, pagerankFirst: t3 - t2, pagerankWarm: t4 - t3, version: mod.version() };
  } catch (e) {
    return { error: String(e) };
  }
}, [script, EXPORT_SQL]);
await browser.close();
server.stop(true);

if ("error" in result) {
  console.error("browser failed: " + result.error);
  process.exit(1);
}
const ms = (x: number) => `${x.toFixed(0)} ms`;
console.log(
  `ursa-wasm ${result.version}: load+compile ${ms(result.load)}, whole script ${ms(result.script)}, ` +
    `pagerank(karate) ${ms(result.pagerankFirst)} then ${ms(result.pagerankWarm)}`,
);
const shapeOk =
  JSON.stringify(result.shape) === JSON.stringify({ statements: 3, columns: ["id", "pagerank"], shown: 5, total: 34 });
if (result.csv !== nativeCsv.stdout || !shapeOk) {
  console.error(`FAIL: run/export: shape ${JSON.stringify(result.shape)}; CSV ${result.csv === nativeCsv.stdout ? "matches" : "differs from"} native --format csv`);
  process.exit(1);
}
console.log("PASS: run() describes the results, and export() CSV is byte-identical to native --format csv");
if (result.out === native.stdout) {
  console.log(`PASS: browser output is byte-identical to native (${native.stdout.length} bytes)`);
  process.exit(0);
}
const dir = mkdtempSync(join(tmpdir(), "ursa-parity-"));
writeFileSync(join(dir, "native.txt"), native.stdout);
writeFileSync(join(dir, "browser.txt"), result.out);
console.error(`FAIL: outputs differ; see diff ${dir}/native.txt ${dir}/browser.txt`);
process.exit(1);
