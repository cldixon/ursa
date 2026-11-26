// End-to-end test of the console's History panel (URSA-17,
// web/console/history.js) over `ursa serve`, in headless Chromium.
//
//   bun tests/console-history.ts [port] [ursa binary] [screenshot dir]
//   bun run test:console-history -- 9121
//
// It starts `ursa serve` on a root of its own (data/ with the karate club's
// Parquet files from web/.data) with this checkout's pages (`bun run build`
// first; the binary as tests/console.ts finds it: URSA_BIN, or
// $CARGO_TARGET_DIR/release/ursa), and checks:
//
// - two queries and a double-click's `.expand` show in History, all three
//   attributed to the console, the step under its drawing, under its query,
//   each with its time, its duration and its rows or nodes; an error shows
//   as one;
// - a second page on the shared session sees them too, live, and its own
//   query shows on the first page;
// - a page opened later sees them, through the `history` op;
// - an agent on the same session (a third client, `hello {client: {kind:
//   "agent"}}`) shows as an agent;
// - choosing a query's entry opens its result's tab again; one whose tab
//   was closed offers to run it again, which does;
// - the keyboard: one Tab stop, Up, Down, Home, End, Enter, Escape; new
//   entries are announced politely;
// - Copy as SQL gives a script that replays: run line by line in a session
//   of its own, as the shell's share link runs it, it draws the same graph;
// - no page errors.
//
// With a screenshot dir, it saves the console with History populated, in
// light and dark at 1440×900 (history-light-1440x900.png,
// history-dark-1440x900.png), and History's column close up.
import { chromium, type Page } from "playwright";
import { copyFileSync, mkdirSync, mkdtempSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { cargoTargetDir, DATA_DIR, WEB } from "../scripts/common";

const port = Number(Bun.argv[2] ?? 9121);
const bin = Bun.argv[3] || process.env.URSA_BIN || join(cargoTargetDir(), "release", "ursa");
const shots = Bun.argv[4];
const failures: string[] = [];
const check = (ok: boolean, label: string, detail = "") => {
  console.log(`${ok ? "ok  " : "FAIL"} ${label}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures.push(`${label}${detail ? `: ${detail}` : ""}`);
};

// ---------------------------------------------------------------- the server

const root = realpathSync(mkdtempSync(join(tmpdir(), "ursa-history-")));
mkdirSync(join(root, "data"), { recursive: true });
for (const f of ["karate_nodes.parquet", "karate_edges.parquet"]) copyFileSync(join(DATA_DIR, f), join(root, "data", f));
const server = Bun.spawn([bin, "serve", root, "--port", String(port), "--no-open", "--assets", join(WEB, "dist-console"), "--assets", join(WEB, "dist")], { stdout: "pipe", stderr: "inherit" });
process.on("exit", () => {
  server.kill();
  rmSync(root, { recursive: true, force: true });
});
let printed = "";
let link: string | undefined;
{
  const reader = server.stdout.getReader();
  while (!link || !/once the link has opened\.\n/.test(printed)) {
    const { value, done } = await reader.read();
    if (done) throw new Error(`ursa serve stopped:\n${printed}`);
    printed += new TextDecoder().decode(value);
    link = printed.match(/http:\/\/127\.0\.0\.1:\d+\/\S*\?token=[A-Za-z0-9_-]+/)?.[0];
  }
  reader.releaseLock();
}
console.log(printed.trim());
const base = `http://127.0.0.1:${new URL(link!).port}`;

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin: base });
const errors: string[] = [];
function watch(p: Page, name: string) {
  p.on("pageerror", (e) => errors.push(`${name}: page error: ${e.message}`));
  p.on("console", (m) => {
    if (m.type() === "error" && !/WebSocket|Failed to load resource/.test(m.text())) errors.push(`${name}: console error: ${m.text()}`);
  });
}
const connected = (p: Page) => p.waitForFunction(() => (window as any).ursaConsole?.session.state === "connected", null, { timeout: 30_000 });
/** History's entries on `p`, oldest first. */
const entries = (p: Page) => p.evaluate(() => (window as any).ursaConsole.history.entries());
/** Wait until `p`'s History has an entry for which `fn(entry, arg)` holds; whether it came. */
async function waitEntry<A>(p: Page, fn: (e: any, arg: A) => boolean, arg: A, timeout = 20_000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if ((await entries(p)).some((e: any) => fn(e, arg))) return true;
    await Bun.sleep(100);
  }
  return false;
}
const rowFor = (p: Page, text: string) => p.locator("#history .hx-row", { hasText: text });

const page = await context.newPage();
watch(page, "page");
// Light, and a layout with room for History: the inspector folded.
await page.addInitScript(() => {
  try {
    if (!sessionStorage.getItem("history-test")) {
      sessionStorage.setItem("history-test", "1");
      localStorage.setItem("ursa-console-layout", JSON.stringify({ rows: { main: 0.3, right: 0.12 } }));
      localStorage.setItem("ursa-theme", "light");
    }
  } catch {}
});
await page.goto(link!);
await connected(page);
const second = await context.newPage();
watch(second, "second page");
await second.goto(`${base}/console/`);
await connected(second);

check(
  (await page.locator("#history .placeholder").count()) === 0 && (await page.locator("#history .hx-empty").isVisible()),
  "History is mounted, and says nothing has run yet",
  await page.locator("#history").innerText(),
);
const list = await page.locator("#history ol.hx-list").getAttribute("aria-label");
check(/History/.test(list ?? ""), "History is a list, labelled", String(list));

// ---------------------------------------------------------------- two queries and a step

/** Run SQL as the editor runs it (ursaConsole.run: a tab for its result). */
const run = (p: Page, sql: string) => p.evaluate((sql) => (window as any).ursaConsole.run(sql).then((r: any) => ({ id: r.tab?.id ?? null, error: r.error?.message ?? null })), sql);

const setup = [
  "CREATE VIEW members AS SELECT * FROM 'data/karate_nodes.parquet'",
  "CREATE VIEW ties AS SELECT * FROM 'data/karate_edges.parquet'",
  "CREATE PROPERTY GRAPH karate VERTEX TABLES (members KEY (id)) EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id) DESTINATION KEY (dst) REFERENCES members (id))",
];
for (const sql of setup) await run(page, sql);
const Q1 = "SELECT * FROM pagerank(karate) ORDER BY pagerank DESC";
const Q2 = "SELECT * FROM hop(karate, seeds => [0], steps => 1, direction => 'both')";
const r1 = await run(page, Q1);
const r2 = await run(page, Q2);
check(Boolean(r1.id && r2.id), "two queries open their tabs", JSON.stringify([r1, r2]));
await page.waitForFunction(() => (window as any).ursaConsole.history.entries().length >= 5, null, { timeout: 10_000 }).catch(() => {});

// Draw the second and double-click a node: the pane's command goes to History's sink.
await page.evaluate((id) => (window as any).ursaConsole.results.tabs().find((t: any) => t.id === id).show("graph"), r2.id);
const drawn = await page
  .waitForFunction(() => (window as any).ursaConsole.results.pane()?.view()?.info()?.nodes > 0, null, { timeout: 60_000 })
  .then(() => true, () => false);
check(drawn, "the hop's result draws in its Graph view");
await page.waitForFunction(() => (window as any).ursaConsole.results.pane()?.view()?.info()?.notShownKnown === true, null, { timeout: 30_000 }).catch(() => {});
await page.waitForTimeout(500);
const before = await page.evaluate(() => (window as any).ursaConsole.results.pane().view().info().nodes);
// A node with neighbours not shown (graph.notShown's counts, by node slot).
const target = await page.evaluate(async () => {
  const { counts } = await (window as any).ursaConsole.session.client.call("graph.notShown", {});
  const slot = Array.from(counts as Uint32Array).findIndex((n, i) => i > 0 && n > 0);
  return slot > 0 ? slot : 1;
});
const canvas = page.locator("#results-graph-stage canvas");
const box = (await canvas.boundingBox())!;
const at = await page.evaluate((id) => (window as any).ursaConsole.results.pane().view().nodePlace(id), target);
await page.mouse.dblclick(box.x + at.x, box.y + at.y);
const stepped = await waitEntry(page, (e) => e.op === "graph.explore" && e.summary.startsWith(".expand "), null, 30_000);
const all = await entries(page);
const step = all.find((e: any) => e.op === "graph.explore");
const opened = all.find((e: any) => e.op === "graph.open");
check(stepped && step?.via === "view" && !step.error && step.drawing === opened?.drawing && step.nodes > before, "a double-click's .expand reaches History, with its drawing and the nodes it shows", JSON.stringify(step));

const q1 = all.find((e: any) => e.summary === Q1);
const q2 = all.find((e: any) => e.summary === Q2);
const human = (e: any) => e?.client?.name === "console" && e?.client?.kind === "human";
check(
  human(q1) && human(q2) && human(step) && human(opened),
  "the two queries, the drawing and the step are the console's",
  JSON.stringify([q1?.client, q2?.client, step?.client]),
);
check(
  q1?.rows === 34 && q1?.result === r1.id && q2?.result === r2.id && q2?.rows > 0 && q1?.ms >= 0 && q1?.at > Date.now() - 600_000 && q1?.at <= Date.now() + 1000,
  "each entry has its time, its duration, its result and its rows",
  JSON.stringify(q1),
);
// As shown: the query's row, the drawing under it, the step under that.
const shown = await page.evaluate(() => {
  const hist = document.getElementById("history")!;
  const rows = [...hist.querySelectorAll(".hx-row")].map((r) => ({ depth: r.closest(".hx-item")!.className.match(/depth-(\d)/)![1], text: (r as HTMLElement).innerText.replace(/\s+/g, " ") }));
  return rows;
});
const q2Row = shown.find((r) => r.text.includes("hop(karate"));
const drawRow = shown.find((r) => r.text.startsWith(".graph"));
const stepRow = shown.find((r) => r.text.startsWith(".expand"));
check(
  q2Row?.depth === "0" && drawRow?.depth === "1" && stepRow?.depth === "2",
  "the step is grouped under its drawing, under the query it drew",
  JSON.stringify(shown.map((r) => `${r.depth} ${r.text.slice(0, 40)}`)),
);
check(
  /console/.test(q1Row(shown)) && /34 rows/.test(q1Row(shown)) && /\d\d:\d\d:\d\d/.test(q1Row(shown)) && /ms|s\b/.test(q1Row(shown)) && /nodes/.test(stepRow?.text ?? ""),
  "a row says who ran it, when, how long it took and what it gave",
  q1Row(shown),
);
function q1Row(rows: { text: string }[]) {
  return rows.find((r) => r.text.includes("pagerank(karate)"))?.text ?? "";
}
const label = await rowFor(page, "pagerank(karate)").getAttribute("aria-label");
check(/console ran query .*pagerank.*34 rows/.test(label ?? ""), "a row's label says it all to a screen reader", String(label));

// An error shows as one.
await run(page, "SELECT nope FROM members");
const failed = await waitEntry(page, (e) => Boolean(e.error) && e.summary.includes("nope"), null);
await page.waitForTimeout(100);
const failedRow = page.locator("#history .hx-item.failed .hx-error");
check(failed && (await failedRow.count()) === 1 && /nope/.test(await failedRow.innerText()), "a failed query shows its error", failed ? await failedRow.innerText() : "no entry");

// ---------------------------------------------------------------- shared

const ids = (es: any[]) => es.map((e) => e.id).join(",");
const onSecond = await second
  .waitForFunction((n) => (window as any).ursaConsole.history.entries().length >= n, (await entries(page)).length, { timeout: 10_000 })
  .then(() => true, () => false);
check(onSecond && ids(await entries(second)) === ids(await entries(page)), "a second page on the shared session has the same History, live", `${ids(await entries(second))} | ${ids(await entries(page))}`);
check((await second.locator("#history .hx-row", { hasText: ".expand" }).count()) === 1, "the second page shows the step, under its drawing");
// The other way: the second page's query shows here as it runs.
const Q3 = "SELECT count(*) AS members FROM members";
await run(second, Q3);
check(await waitEntry(page, (e, q) => e.summary === q, Q3), "a query on the second page shows on the first, live");
const announced = await page
  .waitForFunction((q) => (document.querySelector('#history [aria-live="polite"]')?.textContent ?? "").includes("count(*)"), Q3, { timeout: 5_000 })
  .then(() => true, () => false);
check(announced, "a new entry is announced politely", (await page.locator('#history [aria-live="polite"]').textContent()) ?? "");

// An agent on the session: a client of its own.
const agentRan = await second.evaluate(async () => {
  const { connect } = await import("/engine-client.js");
  const { SocketTransport } = await import("/socket-transport.js");
  const client = connect(new SocketTransport(), { name: "claude", kind: "agent" }, { session: "shared" });
  await client.ready;
  await client.call("query", { sql: "SELECT id FROM members WHERE id < 3", rows: 10 });
  client.close();
  return true;
}).catch((e) => String(e));
const agent = await waitEntry(page, (e) => e.client?.kind === "agent" && e.client?.name === "claude", null);
const marked = await page
  .locator("#history .hx-client.agent", { hasText: "claude" })
  .waitFor({ timeout: 5_000 })
  .then(() => true, () => false);
check(agentRan === true && agent && marked, "an agent's query shows as the agent's", `${agentRan} ${agent} ${marked}`);

// ---------------------------------------------------------------- late

const late = await context.newPage();
watch(late, "late page");
await late.goto(`${base}/console/`);
await connected(late);
const lateGot = await late
  .waitForFunction((n) => (window as any).ursaConsole.history.entries().length >= n, (await entries(page)).length, { timeout: 10_000 })
  .then(() => true, () => false);
check(lateGot && ids(await entries(late)) === ids(await entries(page)), "a page opened later sees what came before (history)", `${ids(await entries(late))} | ${ids(await entries(page))}`);
await late.close();

// ---------------------------------------------------------------- choosing

// Show the first query's tab by its entry.
await rowFor(page, "pagerank(karate)").click();
const reopened = await page.evaluate(() => (window as any).ursaConsole.results.selected()?.id);
check(reopened === r1.id, "choosing a query's entry opens its result's tab again", `${reopened} (want ${r1.id})`);
// The drawing's entry: its tab's Graph view.
await rowFor(page, ".expand").click();
const graphShown = await page.evaluate(() => {
  const s = (window as any).ursaConsole.results.selected();
  return `${s?.id} ${s?.view}`;
});
check(graphShown === `${r2.id} graph`, "choosing a graph step shows its drawing's tab, as a graph", graphShown);
// A closed tab: the entry offers to run it again, which does.
await page.evaluate((id) => (window as any).ursaConsole.results.tabs().find((t: any) => t.id === id).close(), r1.id);
await page.waitForTimeout(100);
await rowFor(page, "pagerank(karate)").first().click();
const offer = page.locator("#history .hx-offer");
check((await offer.count()) === 1 && /not open/.test(await offer.innerText()), "a closed result's entry offers to run it again", (await offer.count()) ? await offer.innerText() : "no offer");
const focusedAgain = await page.evaluate(() => document.activeElement?.classList.contains("hx-again"));
check(Boolean(focusedAgain), "the offer takes the keyboard, on Run again");
const nTabs = (await page.evaluate(() => (window as any).ursaConsole.results.tabs().length)) as number;
await page.keyboard.press("Enter");
const rerun = await page
  .waitForFunction((n) => (window as any).ursaConsole.results.tabs().length === n + 1, nTabs, { timeout: 20_000 })
  .then(() => true, () => false);
const newest = await page.evaluate(() => (window as any).ursaConsole.results.selected());
check(rerun && newest?.sql === Q1 && (await offer.count()) === 0, "Run again runs it into a new tab", JSON.stringify(newest?.sql));

// ---------------------------------------------------------------- keyboard

await page.locator("#history .hx-row").first().focus();
await page.keyboard.press("End");
const atEnd = await page.evaluate(() => {
  const rows = [...document.querySelectorAll("#history .hx-row")];
  return rows.indexOf(document.activeElement!) === rows.length - 1;
});
await page.keyboard.press("Home");
const atHome = await page.evaluate(() => [...document.querySelectorAll("#history .hx-row")].indexOf(document.activeElement!) === 0);
await page.keyboard.press("ArrowDown");
await page.keyboard.press("ArrowDown");
const two = await page.evaluate(() => [...document.querySelectorAll("#history .hx-row")].indexOf(document.activeElement!));
await page.keyboard.press("ArrowUp");
const one = await page.evaluate(() => [...document.querySelectorAll("#history .hx-row")].indexOf(document.activeElement!));
const stops = await page.evaluate(() => [...document.querySelectorAll("#history .hx-row")].filter((r) => (r as HTMLElement).tabIndex === 0).length);
check(atEnd && atHome && two === 2 && one === 1 && stops === 1, "the keyboard: one Tab stop, Home, End, Up and Down", `${atEnd} ${atHome} ${two} ${one} ${stops}`);
// Enter on the failed query's row: its offer; Escape closes it and gives the keyboard back.
await page.locator("#history .hx-item.failed .hx-row").focus();
await page.keyboard.press("Enter");
const offered = (await offer.count()) === 1 && /It failed/.test(await offer.innerText());
await page.keyboard.press("Escape");
const closed = (await offer.count()) === 0 && (await page.evaluate(() => document.activeElement?.closest(".hx-item")?.classList.contains("failed")));
check(offered && Boolean(closed), "Enter offers a failed query again; Escape closes the offer", `${offered} ${closed}`);

// ---------------------------------------------------------------- Copy as SQL

await page.locator("#history .hx-copy").click();
const clip = await page.evaluate(() => navigator.clipboard.readText()).catch((e) => `(clipboard: ${e})`);
const { script, commands } = await page.evaluate(() => (window as any).ursaConsole.history.script());
check(clip === script && script.length > 0, "Copy as SQL copies the script", clip.slice(0, 200));
check(
  commands.length > 0 &&
    commands.includes(`${Q1};`) &&
    commands.includes(`${Q2};`) &&
    commands.some((c: string) => /^\.expand /.test(c)) &&
    !commands.some((c: string) => /nope/.test(c)) &&
    commands.indexOf(`${Q2};`) < commands.findIndex((c: string) => c.startsWith(".graph")) &&
    commands.findIndex((c: string) => c.startsWith(".graph")) < commands.findIndex((c: string) => c.startsWith(".expand")),
  "the script has the statements and graph steps that succeeded, in order",
  JSON.stringify(commands),
);
console.log(script);

// It replays: each command run as the shell runs a share link's, in a
// session of its own; its last drawing has the nodes the console's has.
const replay = await page.evaluate(async (commands: string[]) => {
  const { connect } = await import("/engine-client.js");
  const { SocketTransport } = await import("/socket-transport.js");
  const client = connect(new SocketTransport(), { name: "replay", kind: "agent" }, { session: "new" });
  await client.ready;
  await client.call("start", {});
  let nodes = 0;
  const failed: string[] = [];
  for (const command of commands) {
    const lines = command.split("\n");
    let r: any = null;
    for (const line of lines) r = await client.call("feed", { line });
    if (r.flow === "host") {
      // The shell's page runs its graph commands with graph.open and graph.explore.
      if (/^\.graph\b/.test(command)) {
        await client.call("graph.open", { text: command.replace(/^\.graph\s*/, "") });
        const shown = await client.call("graph.explore", { command: ".graph history" });
        nodes = Number(/Shown: (\d+) node/.exec(shown.out)?.[1] ?? 0);
      } else {
        const e = await client.call("graph.explore", { command });
        if (e.err) failed.push(`${command}: ${e.err}`);
        nodes = e.nodes;
      }
    } else if (r.err) failed.push(`${command}: ${r.err}`);
  }
  client.close();
  return { nodes, failed };
}, commands);
const consoleNodes = await page.evaluate(() => (window as any).ursaConsole.history.entries().filter((e: any) => e.op === "graph.explore" && !e.error).at(-1)?.nodes);
check(replay.failed.length === 0 && replay.nodes === consoleNodes && replay.nodes > 0, "the script replays: every command runs, and the drawing ends as the console's did", `${JSON.stringify(replay)} vs ${consoleNodes}`);

// ---------------------------------------------------------------- screenshots

if (shots) {
  mkdirSync(shots, { recursive: true });
  // Show the drawing, with History scrolled to its end.
  await page.evaluate((id) => (window as any).ursaConsole.results.tabs().find((t: any) => t.id === id)?.show("graph"), r2.id);
  await page.waitForTimeout(1200);
  // History from the drawn query on: its drawing, its step, the error, the agent.
  await page.evaluate(() => {
    const row = [...document.querySelectorAll("#history .hx-item.depth-0")].find((li) => li.textContent!.includes("hop(karate"));
    const scroller = document.querySelector("#history .hx-scroll")!;
    if (row) scroller.scrollTop = (row as HTMLElement).offsetTop - 4;
  });
  for (const theme of ["light", "dark"] as const) {
    await page.evaluate((t) => (document.documentElement.dataset.theme = t), theme);
    await page.evaluate(() => (document.activeElement as HTMLElement)?.blur?.());
    await page.waitForTimeout(500);
    await page.screenshot({ path: join(shots, `history-${theme}-1440x900.png`) });
    await page.locator("#right").screenshot({ path: join(shots, `history-panel-${theme}.png`) });
  }
  await page.evaluate(() => (document.documentElement.dataset.theme = "light"));
}

check(errors.length === 0, "no page errors", errors.join(" | "));
await browser.close();
if (failures.length) {
  console.log(`\n${failures.length} failed:\n  ${failures.join("\n  ")}`);
  process.exit(1);
}
console.log("\nall History checks passed");
process.exit(0);
