// End-to-end test of the Ursa Web Shell in headless Chromium, against a
// running site: `bun run preview` (http://localhost:8788), `cf dev` or a deploy.
//
//   bun tests/e2e.ts [url] [screenshot dir]
//
// It loads every dataset from the Datasets menu, types SQL at the prompt (one
// line and several), runs every dot-command, downloads the last result in each
// format, imports a file, replays a shared link in a fresh page, and checks
// New, the theme toggle and history. Any error the page shows fails the run,
// except the one a test asks for.
import { chromium, type Page } from "playwright";

const base = (Bun.argv[2] ?? "http://localhost:8788").replace(/\/$/, "");
const shots = Bun.argv[3];
const failures: string[] = [];
const check = (ok: boolean, label: string, detail = "") => {
  console.log(`${ok ? "ok  " : "FAIL"} ${label}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures.push(`${label}${detail ? `: ${detail}` : ""}`);
};

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, acceptDownloads: true });
const page = await context.newPage();
page.on("pageerror", (e) => failures.push(`page error: ${e.message}`));

async function ready(p: Page) {
  await p.waitForFunction(() => !(document.getElementById("input") as HTMLTextAreaElement).disabled, null, { timeout: 120_000 });
}
async function idle(p: Page = page) {
  await p.evaluate(() => (window as any).ursaShell.idle());
}
/** The errors on screen. */
async function errors(p: Page = page): Promise<string[]> {
  return p.$$eval("#log .err", (e) => e.map((x) => x.textContent ?? ""));
}
/** Type `text` at the prompt and press Enter, then wait for it to finish. */
async function type(text: string, p: Page = page) {
  await p.focus("#input");
  const lines = text.split("\n");
  for (let i = 0; i < lines.length; i++) {
    await p.keyboard.insertText(lines[i]);
    if (i < lines.length - 1) await p.keyboard.press("Shift+Enter");
  }
  await p.keyboard.press("Enter");
  await idle(p);
}
const tables = (p: Page = page) => p.$$eval("#log table.box", (t) => t.length);
const lastTable = (p: Page = page) =>
  p.$$eval("#log table.box", (t) => {
    const last = t[t.length - 1];
    return {
      head: [...last.querySelectorAll("th")].map((th) => th.childNodes[0]?.textContent ?? ""),
      types: [...last.querySelectorAll("th .type")].map((x) => x.textContent ?? ""),
      rows: [...last.querySelectorAll("tbody tr:not(.gap)")].map((tr) => [...tr.querySelectorAll("td")].map((td) => td.textContent ?? "")),
      gap: last.querySelectorAll("tbody tr.gap").length,
      footer: last.parentElement?.querySelector(".footer")?.textContent ?? "",
    };
  });

const t0 = Date.now();
await page.goto(base);
await ready(page);
const banner = await page.textContent("#log");
check(/Ursa v\d/.test(banner ?? ""), "banner", `${(banner ?? "").match(/Ursa v\S+/)?.[0]} (${Date.now() - t0} ms)`);

// Every dataset, from the menu: its commands run with no error.
const datasets: { id: string; title: string; commands: string[] }[] = await page.evaluate(() => (window as any).ursaShell.datasets);
for (const d of datasets) {
  const before = await tables();
  const t = Date.now();
  await page.click("#datasets");
  await page.click(`#dataset-menu button:has-text("${d.title}")`);
  await idle();
  const errs = await errors();
  const queries = d.commands.filter((c) => !/^CREATE/i.test(c)).length;
  const made = (await tables()) - before;
  check(!errs.length && made === queries, `dataset ${d.id}`, errs[0] ?? `${made} tables in ${Date.now() - t} ms`);
  if (shots && d.id === "karate") await page.screenshot({ path: `${shots}/shell-karate.png` });
}

// SQL at the prompt: one line, several lines, and an error.
await type("SELECT count(*) AS members FROM members;");
let t = await lastTable();
check(t.rows[0]?.[0] === "34" && t.types[0] === "int64", "one line", JSON.stringify(t.rows));
await type("SELECT club,\n       count(*) AS n\nFROM members\nGROUP BY club\nORDER BY club;");
t = await lastTable();
check(JSON.stringify(t.rows) === '[["Mr. Hi","17"],["Officer","17"]]', "several lines", JSON.stringify(t.rows));
const before = (await errors()).length;
await type("SELECT * FROM no_such_table;");
const errs = await errors();
check(errs.length === before + 1 && /no_such_table/.test(errs.at(-1) ?? ""), "an error is shown", errs.at(-1));

// Long results: the first and last rows, a gap and a footer, as the CLI shows.
await type(".maxrows 6");
await type("SELECT * FROM ties ORDER BY src, dst;");
t = await lastTable();
check(t.rows.length === 6 && t.gap === 3 && /78 rows \(6 shown\)/.test(t.footer), ".maxrows and a long table", `${t.rows.length} rows, footer ${t.footer}`);
await type(".maxrows 40");

// Dot-commands.
await type(".help");
check(((await page.textContent("#log .help:last-of-type")) ?? "").includes(".describe"), ".help");
await type(".tables");
t = await lastTable();
check(t.rows.some((r) => r[0] === "members") && t.rows.some((r) => r[0] === "rmat_edges"), ".tables", JSON.stringify(t.rows.map((r) => r[0])));
await type(".graphs");
t = await lastTable();
check(["karate", "lesmis", "florentine", "rmat"].every((g) => t.rows.some((r) => r.includes(g))), ".graphs");
await type(".describe karate");
check(!(await errors()).slice(errs.length).length, ".describe karate");
await type(".functions");
t = await lastTable();
check(t.rows.length >= 15 && t.rows.some((r) => r[0] === "pagerank"), ".functions", `${t.rows.length} functions`);
await type(".functions pagerank");
check(((await page.textContent("#log")) ?? "").includes("damping =>"), ".functions pagerank");
await type(".timer on");
await type("SELECT 1 AS one;");
check(((await page.$$eval("#log pre.line", (l) => l.at(-1)?.textContent ?? ""))).startsWith("Run Time:"), ".timer on");
await type(".timer off");
await type(".files");
t = await lastTable();
check(t.rows.some((r) => r[0] === "karate_edges.parquet"), ".files");
await type(".version");
check(/^Ursa v\d/.test(await page.$$eval("#log pre.line", (l) => l.at(-1)?.textContent ?? "")), ".version");
await type(".help graph");
check(((await page.textContent("#log")) ?? "").includes("ANY SHORTEST | ALL SHORTEST"), ".help graph");
await type(".help shortcuts");
await type(".help timer");
check((await page.$$eval("#log .help", (h) => h.at(-1)?.children.length ?? 0)) === 2, ".help PATTERN", "one command");
await type(".history 2");
check(((await page.$$eval("#log pre.line", (l) => l.at(-1)?.textContent ?? ""))).includes(".help timer"), ".history");
await type(".dataset");
check((await page.$$eval("#log .help", (h) => h.at(-1)?.children.length ?? 0)) === datasets.length * 2, ".dataset lists");
// The forms .help graph shows, on the karate graph: they must parse and run.
for (const sql of [
  "FROM GRAPH_TABLE (karate MATCH (a)<-[e]-(b) WHERE a.id = 1 COLUMNS (b.id));",
  "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 1)-[e]->{1,3}(b) COLUMNS (b.id));",
  "FROM GRAPH_TABLE (karate MATCH p = TRAIL (a WHERE a.id = 1)-[e]->+(b WHERE b.id = 33) COLUMNS (path_length(p))) LIMIT 5;",
  "FROM GRAPH_TABLE (karate MATCH p = SHORTEST 2 (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26) COLUMNS (path_length(p), vertices(p), edges(p)));",
  "SELECT * FROM pagerank(karate, damping => 0.85) LIMIT 3;",
  "SELECT * FROM algorithms(karate, pagerank(), community => louvain(seed => 1)) LIMIT 3;",
]) await type(sql);
check((await errors()).length === errs.length, "no errors from the dot-commands", (await errors()).slice(errs.length).join(" | "));

// Downloads of the last result, in each format.
await type("SELECT id, club FROM members ORDER BY id LIMIT 3;");
for (const format of ["csv", "parquet", "json", "arrow"]) {
  const [download] = await Promise.all([page.waitForEvent("download"), type(`.export ${format}`)]);
  const bytes = (await Bun.file((await download.path())!).bytes()).length;
  check(bytes > 0, `.export ${format}`, `${download.suggestedFilename()} ${bytes} bytes`);
}

// Import: a file from this computer becomes a view.
await page.setInputFiles("#file-input", {
  name: "my_ties.csv",
  mimeType: "text/csv",
  buffer: Buffer.from("src,dst\n1,2\n2,3\n3,1\n"),
});
await page.waitForFunction(() => document.getElementById("log")?.textContent?.includes("FROM my_ties LIMIT 10"), null, { timeout: 30_000 });
await idle();
t = await lastTable();
check(t.rows.length === 3 && t.head.join(",") === "src,dst", "import", JSON.stringify(t.rows));

// History: the up arrow brings back the last command.
await page.focus("#input");
await page.keyboard.press("ArrowUp");
const recalled = await page.inputValue("#input");
check(recalled.length > 0, "history (up arrow)", recalled.slice(0, 40));
await page.fill("#input", "");

// Share: a link that replays the session's commands in a fresh page.
await context.grantPermissions(["clipboard-read", "clipboard-write"]);
await page.click("#share");
await idle();
const url = page.url().includes("#run=") ? page.url() : await page.evaluate(() => navigator.clipboard.readText());
check(url.includes("#run="), "share link", `${url.length} characters`);
const replay = await context.newPage();
replay.on("pageerror", (e) => failures.push(`replay page error: ${e.message}`));
await replay.goto(url);
await ready(replay);
await replay.waitForFunction(() => document.getElementById("log")?.textContent?.includes("Replaying"), null, { timeout: 10_000 });
await idle(replay);
const replayErrors = await errors(replay);
// The imported file is not in the link: its view is the one error expected.
check(replayErrors.every((e) => /my_ties/.test(e)) && (await tables(replay)) > 5, "shared link replays", replayErrors[0] ?? `${await tables(replay)} tables`);
await replay.close();

// New: a fresh session and a clear screen.
await page.click("#new");
await idle();
await type("SELECT count(*) FROM members;");
check(((await errors()).at(-1) ?? "").includes("members"), "New starts an empty session");

// The theme toggle, remembered.
const theme0 = await page.evaluate(() => document.documentElement.dataset.theme ?? "");
await page.click("#theme");
const theme1 = await page.evaluate(() => document.documentElement.dataset.theme ?? "");
await page.reload();
await ready(page);
const theme2 = await page.evaluate(() => document.documentElement.dataset.theme ?? "");
check(theme1 !== theme0 && theme2 === theme1, "theme toggle", `${theme0 || "system"} → ${theme1} → ${theme2} after reload`);
if (shots) await page.screenshot({ path: `${shots}/shell-start-${theme1}.png` });

// A narrow screen.
if (shots) {
  const phone = await browser.newPage({ viewport: { width: 390, height: 844 } });
  await phone.goto(`${base}/?dataset=florentine`);
  await ready(phone);
  await phone.waitForFunction(() => document.querySelectorAll("#log table.box").length >= 2, null, { timeout: 60_000 });
  await idle(phone);
  await phone.screenshot({ path: `${shots}/shell-phone.png` });
}

await browser.close();
console.log(failures.length ? `\n${failures.length} failures:\n- ${failures.join("\n- ")}` : "\nall passed");
process.exit(failures.length ? 1 : 0);
