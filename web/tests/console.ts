// The console spike's page (#536) against `ursa serve`: it runs
// `SELECT * FROM pagerank(karate)`, shows typed columns with their `ursa.*`
// metadata, and draws the result as a graph in the shell's graph pane.
//
//   URSA_SERVE_TOKEN=… bun tests/console.ts http://localhost:PORT [screenshot.png]
import { chromium } from "playwright";

const base = (Bun.argv[2] ?? "http://localhost:4280").replace(/\/$/, "");
const failures: string[] = [];
const check = (ok: boolean, label: string, detail = "") => {
  console.log(`${ok ? "ok  " : "FAIL"} ${label}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures.push(label);
};
const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
await ctx.addCookies([{ name: `ursa_token_${new URL(base).port}`, value: process.env.URSA_SERVE_TOKEN ?? "", url: base }]);
const p = await ctx.newPage();
p.on("pageerror", (e) => failures.push(`page error: ${e.message}`));
await p.goto(`${base}/console.html`);
await p.waitForFunction(() => document.getElementById("info")!.textContent!.includes("karate loaded"), null, { timeout: 30_000 });
check((await p.textContent("#info"))!.includes("native"), "the console connects to the native engine", (await p.textContent("#info")) ?? "");
await p.fill("#sql", "SELECT * FROM pagerank(karate)");
await p.click("#run");
await p.waitForSelector(".console-result table");
const head = await p.$$eval(".console-result th", (ths) => ths.map((th) => th.textContent));
check(head.length === 2 && head[0]!.includes("id") && head[1]!.includes("pagerank"), "the result's columns", JSON.stringify(head));
check(/Int64/.test(head[0]!) && /Float64/.test(head[1]!), "each column's type, from the Arrow schema", JSON.stringify(head));
check(head[0]!.includes("ursa.role=node") && head[1]!.includes("ursa.role=metric") && head[0]!.includes("ursa.graph=karate"), "each column's ursa.* metadata", JSON.stringify(head));
const rows = await p.$$eval(".console-result tbody tr", (trs) => trs.length);
check(rows === 34, "every member's row", `${rows}`);
await p.click("#draw");
await p.waitForFunction(() => (window as any).ursaConsole.graph()?.nodes === 34, null, { timeout: 30_000 }).catch(() => {});
const info = await p.evaluate(() => (window as any).ursaConsole.graph());
check(info?.nodes === 34 && info?.edges === 78, "Graph draws the result in the graph pane", JSON.stringify(info && { nodes: info.nodes, edges: info.edges }));
if (Bun.argv[3]) {
  await p.waitForTimeout(1500);
  await p.screenshot({ path: Bun.argv[3] });
}
check(failures.length === 0, "no page errors", failures.join(" | "));
await browser.close();
process.exit(failures.length ? 1 : 0);
