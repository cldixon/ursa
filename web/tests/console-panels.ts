// End-to-end test of the console's Catalog (URSA-15) and Sources (URSA-16)
// panels (web/console/catalog.js, sources.js) over `ursa serve`, in headless
// Chromium.
//
//   bun tests/console-panels.ts [port] [ursa binary] [screenshot dir]
//   bun run test:console-panels -- 9061
//
// It makes a root of its own in a temp directory (data/karate/ with the
// karate club's Parquet and CSV files from web/.data, a database file
// club.ursa, a text file), starts `ursa serve ROOT` on it with this
// checkout's pages (`bun run build` first; the binary as tests/console.ts
// finds it), and checks:
//
// - Catalog: a CREATE TABLE shows without a reload, on this page and on a
//   second page on the shared session; what was open stays open across a
//   refetch; tables, views and graphs are marked by kind, columns have
//   their types; the filter; the ARIA tree's keys; choosing a table hands
//   `SELECT * FROM … LIMIT 100` to the editor (ursaConsole.editor.insert),
//   or, with no editor, runs it; a graph hands `.graph karate`.
// - Sources: the root's folders and files with their formats and sizes;
//   browsing into a folder and back up (the path, Backspace); a Parquet
//   file's query runs; attaching club.ursa shows its views and graph in the
//   Catalog; a bucket that cannot be reached shows its error in the panel;
//   buckets are kept across a reload; without the `sources` capability the
//   panel says so and shows no controls.
// - No page errors.
//
// With a screenshot dir, it saves both panels, populated, in light and dark
// at 1440×900 (panels-light.png, panels-dark.png).
import { chromium, type Page } from "playwright";
import { copyFileSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { cargoTargetDir, DATA_DIR, WEB } from "../scripts/common";

const port = Number(Bun.argv[2] ?? 9061);
const bin = Bun.argv[3] || process.env.URSA_BIN || join(cargoTargetDir(), "release", "ursa");
const shots = Bun.argv[4];
const failures: string[] = [];
const check = (ok: boolean, label: string, detail = "") => {
  console.log(`${ok ? "ok  " : "FAIL"} ${label}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures.push(`${label}${detail ? `: ${detail}` : ""}`);
};

// ---------------------------------------------------------------- the root

const root = realpathSync(mkdtempSync(join(tmpdir(), "ursa-panels-")));
mkdirSync(join(root, "data", "karate"), { recursive: true });
for (const f of ["karate_nodes.parquet", "karate_edges.parquet", "karate_edges.csv"]) copyFileSync(join(DATA_DIR, f), join(root, "data", "karate", f));
writeFileSync(join(root, "notes.txt"), "not a table\n");
// A database file: two views over the files and a graph over them.
writeFileSync(
  join(root, "club.ursa"),
  JSON.stringify({
    format: "ursa",
    version: 1,
    statements: [
      "CREATE VIEW members AS SELECT * FROM 'data/karate/karate_nodes.parquet'",
      "CREATE VIEW ties AS SELECT * FROM 'data/karate/karate_edges.parquet'",
      "CREATE PROPERTY GRAPH club VERTEX TABLES (members KEY (id)) EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id) DESTINATION KEY (dst) REFERENCES members (id))",
    ],
  }),
);

const server = Bun.spawn([bin, "serve", root, "--port", String(port), "--no-open", "--assets", join(WEB, "dist-console"), "--assets", join(WEB, "dist")], { stdout: "pipe", stderr: "inherit" });
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

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
const errors: string[] = [];
function watch(p: Page, name: string) {
  p.on("pageerror", (e) => errors.push(`${name}: page error: ${e.message}`));
  p.on("console", (m) => {
    if (m.type() === "error" && !/WebSocket|Failed to load resource/.test(m.text())) errors.push(`${name}: console error: ${m.text()}`);
  });
}
const connected = (p: Page) => p.waitForFunction(() => (window as any).ursaConsole?.session.state === "connected", null, { timeout: 30_000 });
/** Run SQL on the shared session from `p`. */
const sql = (p: Page, text: string) => p.evaluate((sql) => (window as any).ursaConsole.session.run("query", { sql, rows: 0 }).then(() => "ok", (e: Error) => e.message), text);
/** The catalog's rows as "level name", in order. */
const treeRows = (p: Page, tree: "tables" | "graphs") =>
  p.$$eval(`#catalog [data-tree="${tree}"] [role="treeitem"]`, (els) => els.map((el) => `${el.getAttribute("aria-level")} ${el.querySelector(".name")!.textContent}`));
const row = (p: Page, key: string) => p.locator(`#catalog [role="treeitem"][data-key="${key}"]`);
const noteOf = (p: Page, panel: string) => p.locator(`#${panel} .panel-note`);
/** Wait for a panel's note to say `pattern`; what it says. */
async function noteSays(p: Page, panel: string, pattern: RegExp, timeout = 20_000) {
  try {
    await p.waitForFunction(([panel, src]) => new RegExp(src).test(document.querySelector(`#${panel} .panel-note`)?.textContent ?? ""), [panel, pattern.source] as const, { timeout });
  } catch {}
  return (await noteOf(p, panel).textContent()) ?? "";
}

const page = await context.newPage();
watch(page, "page");
await page.goto(link!);
await connected(page);
const second = await context.newPage();
watch(second, "second page");
await second.goto(`http://127.0.0.1:${new URL(link!).port}/console/`);
await connected(second);

// ---------------------------------------------------------------- catalog

check(
  (await page.locator("#catalog .placeholder").count()) === 0 && (await page.locator('#catalog [role="tree"]').count()) === 2,
  "the Catalog panel is mounted: a tree for tables and one for graphs",
);
await page.waitForSelector('#catalog [data-key="d/datafusion"]');
check((await treeRows(page, "tables")).join("|") === "1 datafusion|2 public", "an empty session: its database and schema", (await treeRows(page, "tables")).join("|"));

check((await sql(page, "CREATE TABLE karate_nodes AS SELECT * FROM 'data/karate/karate_nodes.parquet'")) === "ok", "CREATE TABLE runs");
for (const [p, name] of [[page, "this page"], [second, "a second page on the shared session"]] as const) {
  const shown = await p
    .waitForSelector('#catalog [data-key="t/datafusion/public/karate_nodes"]', { timeout: 10_000 })
    .then(() => true)
    .catch(() => false);
  check(shown, `CREATE TABLE shows in the Catalog without a reload, on ${name}`);
}

// What is open stays open, and the scroll stays, across a refetch.
await row(page, "t/datafusion/public/karate_nodes").locator(".twisty").click();
check((await treeRows(page, "tables")).join("|") === "1 datafusion|2 public|3 karate_nodes|4 id|4 club", "a table opens to its columns", (await treeRows(page, "tables")).join("|"));
const types = await page.$$eval('#catalog .type-column .detail', (els) => els.map((e) => e.textContent));
check(types.join(" ").includes("bigint") && types.join(" ").includes("varchar"), "columns show their types", types.join(" "));
await sql(page, "CREATE TABLE karate_edges AS SELECT * FROM 'data/karate/karate_edges.parquet'");
await sql(page, "CREATE VIEW hubs AS SELECT * FROM karate_nodes WHERE id < 5");
await sql(page, "CREATE PROPERTY GRAPH karate VERTEX TABLES (karate_nodes AS person KEY (id)) EDGE TABLES (karate_edges AS knows SOURCE KEY (src) REFERENCES person (id) DESTINATION KEY (dst) REFERENCES person (id))");
await page.waitForSelector('#catalog [data-key="g/karate"]', { timeout: 10_000 }).catch(() => {});
const afterRefetch = (await treeRows(page, "tables")).join("|");
check(afterRefetch === "1 datafusion|2 public|3 hubs|3 karate_edges|3 karate_nodes|4 id|4 club", "a refetch keeps what was open", afterRefetch);
check((await row(page, "t/datafusion/public/hubs").locator(".kind-chip").textContent()) === "view" && (await row(page, "t/datafusion/public/karate_edges").locator(".kind-chip").count()) === 0, "a view is marked as one");
check((await treeRows(page, "graphs")).join("|") === "1 karate|2 person|2 knows", "the graph has a section of its own, with its vertex and edge tables", (await treeRows(page, "graphs")).join("|"));
check((await row(page, "g/karate/e/knows").locator(".detail").textContent()) === "src → dst", "an edge table says what it joins", (await row(page, "g/karate/e/knows").locator(".detail").textContent()) ?? "");

// The scroll: in a short panel, scrolled down, a refetch leaves it there.
const scroll = await page.evaluate(async () => {
  const body = document.getElementById("catalog-body")!;
  body.style.maxHeight = "120px";
  body.scrollTop = 60;
  const before = body.scrollTop;
  await (window as any).ursaConsole.catalog.refresh();
  const after = body.scrollTop;
  body.style.maxHeight = "";
  return { before, after };
});
check(scroll.before > 0 && scroll.after === scroll.before, "a refetch keeps the scroll", JSON.stringify(scroll));

// ARIA: the tree's attributes and one Tab stop.
const aria = await page.evaluate(() => {
  const el = document.querySelector('#catalog [data-key="t/datafusion/public/karate_nodes"]')!;
  return {
    expanded: el.getAttribute("aria-expanded"),
    level: el.getAttribute("aria-level"),
    set: `${el.getAttribute("aria-posinset")}/${el.getAttribute("aria-setsize")}`,
    stops: document.querySelectorAll('#catalog [data-tree="tables"] [role="treeitem"][tabindex="0"]').length,
    label: document.querySelector('#catalog [data-tree="tables"]')!.getAttribute("aria-labelledby"),
  };
});
check(aria.expanded === "true" && aria.level === "3" && aria.set === "3/3" && aria.stops === 1 && aria.label === "catalog-tables-head", "treeitems carry aria-expanded, -level, -posinset and -setsize, with one Tab stop in the tree", JSON.stringify(aria));

// Keys: Home, Down, Right (go in), Left (out, close), End, a letter.
await page.locator('#catalog [data-tree="tables"] [role="treeitem"][tabindex="0"]').focus();
const focusedKey = () => page.evaluate(() => (document.activeElement as HTMLElement)?.dataset.key);
await page.keyboard.press("Home");
const k1 = await focusedKey();
await page.keyboard.press("ArrowDown");
await page.keyboard.press("ArrowDown");
const k2 = await focusedKey();
await page.keyboard.press("k");
const k3 = await focusedKey();
await page.keyboard.press("ArrowRight");
const opened = await row(page, "t/datafusion/public/karate_edges").getAttribute("aria-expanded");
const k4 = await focusedKey();
await page.keyboard.press("ArrowRight");
const k5 = await focusedKey();
await page.keyboard.press("ArrowLeft");
const k6 = await focusedKey();
await page.keyboard.press("ArrowLeft");
const closed = await row(page, "t/datafusion/public/karate_edges").getAttribute("aria-expanded");
await page.keyboard.press("ArrowLeft");
const k7 = await focusedKey();
await page.keyboard.press("End");
const k8 = await focusedKey();
check(
  k1 === "d/datafusion" && k2 === "t/datafusion/public/hubs" && k3 === "t/datafusion/public/karate_edges" && opened === "true" && k4 === k3 && k5 === "c/datafusion/public/karate_edges/src" && k6 === k3 && closed === "false" && k7 === "s/datafusion/public" && k8 === "c/datafusion/public/karate_nodes/club",
  "the tree's keys: Home, Down, a letter, Right opens then goes in, Left goes out, closes, then up; End",
  [k1, k2, k3, opened, k4, k5, k6, closed, k7, k8].join(" "),
);

// The filter.
await page.fill("#catalog .filter-input", "club");
const filtered = (await treeRows(page, "tables")).join("|");
check(filtered === "1 datafusion|2 public|3 hubs|4 club|3 karate_nodes|4 club" && (await page.locator('#catalog [data-tree="graphs"]').isHidden()), "the filter keeps what matches, with what is above it", filtered);
check((await page.locator("#catalog mark").first().textContent()) === "club", "and marks the match");
await page.fill("#catalog .filter-input", "zzz");
check((await page.locator("#catalog .catalog-state").textContent())?.includes("Nothing matches") ?? false, "a filter with no match says so");
await page.locator("#catalog .filter-input").press("Escape");
check((await page.inputValue("#catalog .filter-input")) === "" && (await treeRows(page, "graphs")).length === 3, "Esc clears the filter");

// Choosing: with no editor, the query runs on the session.
await row(page, "t/datafusion/public/karate_nodes").locator(".name").click();
const ran = await noteSays(page, "catalog", /rows/);
check(/SELECT \* FROM karate_nodes LIMIT 100/.test(ran) && /34 rows/.test(ran), "clicking a table runs SELECT * FROM it LIMIT 100 (no editor yet)", ran);
// With an editor (URSA-13's insert), it is handed to the editor.
await page.evaluate(() => {
  (window as any).inserted = [];
  (window as any).ursaConsole.editor = { insert: (text: string) => (window as any).inserted.push(text) };
});
await row(page, "t/datafusion/public/hubs").locator(".name").click();
await row(page, "g/karate").focus();
await page.keyboard.press("Enter");
await page.waitForFunction(() => (window as any).inserted.length >= 2, null, { timeout: 5000 }).catch(() => {});
const inserted = await page.evaluate(() => (window as any).inserted);
check(JSON.stringify(inserted) === JSON.stringify(["SELECT * FROM hubs LIMIT 100", ".graph karate"]), "with the editor, a table's query and a graph's .graph go to it (click, Enter)", JSON.stringify(inserted));
await page.evaluate(() => delete (window as any).ursaConsole.editor);

// ---------------------------------------------------------------- sources

const entries = (p: Page) => p.$$eval("#sources .entry", (els) => els.map((el) => `${el.classList.contains("kind-dir") ? "dir" : el.classList.contains("kind-up") ? "up" : "file"} ${(el as HTMLElement).dataset.name}${el.querySelector(".format-chip") ? ` ${el.querySelector(".format-chip")!.textContent}` : ""}${el.querySelector(".size")?.textContent ? ` ${el.querySelector(".size")!.textContent}` : ""}`));
await page.waitForSelector("#sources .entry");
const top = await entries(page);
check(top.join("|") === "dir data|file club.ursa ursa " + (await page.locator('#sources .entry[data-name="club.ursa"] .size').textContent()) + "|file notes.txt 12 B", "Sources lists the root: folders first, files with their format and size", top.join("|"));
check(await page.locator('#sources .entry[data-name="club.ursa"] .attach').isVisible(), "a .ursa file offers Attach");
check((await page.locator('#sources .entry[data-name="notes.txt"] .entry-main').getAttribute("aria-disabled")) === "true", "a file Ursa cannot read is not offered");

await page.locator('#sources .entry[data-name="data"] .entry-main').click();
await page.waitForSelector('#sources .entry[data-name="karate"]');
await page.locator('#sources .entry[data-name="karate"] .entry-main').click();
await page.waitForSelector('#sources .entry[data-name="karate_nodes.parquet"]');
const inKarate = await entries(page);
check(inKarate[0] === "up .." && inKarate.some((e) => /^file karate_nodes\.parquet parquet \d+(\.\d)? K?B$/.test(e)) && inKarate.some((e) => e.startsWith("file karate_edges.csv csv")), "browsing into folders: Parquet and CSV, sized", inKarate.join("|"));
const crumbs = await page.$$eval("#sources .crumbs li", (els) => els.map((e) => e.textContent));
check(crumbs.join("/") === `${root.split("/").pop()}/data/karate`, "the path above the list", crumbs.join("/"));
check((await page.evaluate(() => document.activeElement?.closest(".entry")?.getAttribute("data-name"))) === "..", "the keyboard is in the new folder's list");

await page.locator('#sources .entry[data-name="karate_nodes.parquet"] .entry-main').click();
const parquet = await noteSays(page, "sources", /rows|fail|cannot/i);
check(/SELECT \* FROM 'data\/karate\/karate_nodes\.parquet' LIMIT 100/.test(parquet) && /34 rows/.test(parquet), "clicking a Parquet file runs its query, by its path under the root", parquet);

// Keys: Down, then Backspace goes up.
await page.locator('#sources .entry[data-name="karate_nodes.parquet"] .entry-main').focus();
await page.keyboard.press("ArrowUp");
const upKey = await page.evaluate(() => document.activeElement?.closest(".entry")?.getAttribute("data-name"));
await page.keyboard.press("Backspace");
await page.waitForSelector('#sources .entry[data-name="karate"]');
check(upKey === "karate_edges.parquet", "Up moves between entries", upKey ?? "");
await page.keyboard.press("Backspace");
await page.waitForSelector('#sources .entry[data-name="club.ursa"]');
check(await page.locator("#sources .crumbs").isHidden(), "Backspace goes up a folder, to the root");

// Attach a database file: ATTACH comes before anything is defined, so
// first in a fresh session (`start`), which empties the Catalog; then its
// views and graph show in it.
await page.locator('#sources .entry[data-name="club.ursa"] .attach').click();
check(/Cannot attach club\.ursa: ATTACH must come before/.test(await noteSays(page, "sources", /Cannot|Attached/)), "an ATTACH the engine refuses says why in the panel");
await page.evaluate(() => (window as any).ursaConsole.session.run("start", {}));
await page.waitForFunction(() => !document.querySelector('#catalog [data-key="g/karate"]'), null, { timeout: 10_000 }).catch(() => {});
check((await treeRows(page, "graphs")).length === 0, "a fresh session (start) empties the Catalog", (await treeRows(page, "graphs")).join("|"));
await page.locator('#sources .entry[data-name="club.ursa"] .attach').click();
const attached = await page
  .waitForSelector('#catalog [data-key="g/club"]', { timeout: 15_000 })
  .then(() => true)
  .catch(() => false);
const clubRows = (await treeRows(page, "tables")).filter((r) => /members|ties|club/.test(r));
check(attached && clubRows.includes("3 members") && clubRows.includes("3 ties"), "attaching club.ursa shows its views and graph in the Catalog", `${attached} ${clubRows.join("|")} ${await noteOf(page, "sources").textContent()}`);
check(/Attached club\.ursa/.test(await noteSays(page, "sources", /Attached|Cannot/)), "and the panel says so");

// A bucket that cannot be reached: the error is in the panel.
await page.click("#sources .add-bucket");
await page.fill("#sources-bucket-url", "http://not-a-bucket");
await page.press("#sources-bucket-url", "Enter");
check((await page.getAttribute("#sources-bucket-url", "aria-invalid")) === "true", "a URL that is not a bucket's is refused in the form");
await page.fill("#sources-bucket-url", "s3://ursa-console-e2e-no-such-bucket/some/prefix");
await page.press("#sources-bucket-url", "Enter");
const bucketError = await page
  .waitForSelector("#sources .error-box", { timeout: 60_000 })
  .then((el) => el.textContent())
  .catch(() => null);
check(Boolean(bucketError && /Cannot list s3:\/\/ursa-console-e2e-no-such-bucket\/some\/prefix\//.test(bucketError)), "an unreachable bucket shows a clear error in the panel", bucketError ?? "(no error shown)");
check(errors.length === 0, "and no page error", errors.join("; "));

// Kept for this server across a reload.
await page.reload();
await connected(page);
await page.waitForSelector("#sources .place-button");
const places = await page.$$eval("#sources .place-button", (els) => els.map((e) => (e as HTMLElement).dataset.place));
check(places.includes("s3://ursa-console-e2e-no-such-bucket/some/prefix/"), "an added bucket is kept across a reload", places.join(" "));
await page.locator('#sources .place-remove').first().click();
check((await page.$$eval("#sources .place-button", (els) => els.length)) === 1, "and can be removed");

// Without the capability (the browser demo): it says so, no controls.
const off = await page.evaluate(() => {
  const s = (window as any).ursaConsole.session;
  const real = s.hello;
  s.emit("connect", { ...real, capabilities: real.capabilities.filter((c: string) => c !== "sources") });
  const panel = document.getElementById("sources-body")!;
  const out = { text: panel.querySelector<HTMLElement>(".sources-off")!.innerText, controls: [...panel.querySelectorAll("button, input")].filter((b) => (b as HTMLElement).offsetParent !== null).length };
  s.emit("connect", real);
  return out;
});
check(/No files to browse/.test(off.text) && off.controls === 0, "with no sources capability, the panel says so and shows no controls", JSON.stringify(off));

// ---------------------------------------------------------------- screenshots

if (shots) {
  mkdirSync(shots, { recursive: true });
  // The karate club's tables and graph again (the attach began a fresh
  // session), the data folder, a table open, nothing filtered.
  for (const statement of [
    "CREATE TABLE karate_nodes AS SELECT * FROM 'data/karate/karate_nodes.parquet'",
    "CREATE TABLE karate_edges AS SELECT * FROM 'data/karate/karate_edges.parquet'",
    "CREATE VIEW hubs AS SELECT * FROM karate_nodes WHERE id < 5",
    "CREATE PROPERTY GRAPH karate VERTEX TABLES (karate_nodes AS person KEY (id)) EDGE TABLES (karate_edges AS knows SOURCE KEY (src) REFERENCES person (id) DESTINATION KEY (dst) REFERENCES person (id))",
  ]) {
    const done = await sql(page, statement);
    if (done !== "ok") console.log(`(screenshots) ${statement}: ${done}`);
  }
  await page.waitForSelector('#catalog [data-key="g/karate"]', { timeout: 10_000 }).catch(() => {});
  await page.waitForSelector('#sources .entry[data-name="data"]');
  await page.locator('#sources .entry[data-name="data"] .entry-main').click();
  await page.waitForSelector('#sources .entry[data-name="karate"]');
  await page.locator('#sources .entry[data-name="karate"] .entry-main').click();
  await page.waitForSelector('#sources .entry[data-name="karate_nodes.parquet"]');
  await page.evaluate(() => (window as any).ursaConsole.catalog.expand(["t/datafusion/public/karate_nodes"]));
  for (const theme of ["light", "dark"]) {
    await page.evaluate((t) => (document.documentElement.dataset.theme = t), theme);
    await page.mouse.move(700, 600);
    await page.locator('#catalog [data-key="t/datafusion/public/karate_nodes"]').hover();
    await page.waitForTimeout(150);
    await page.screenshot({ path: join(shots, `panels-${theme}.png`) });
  }
  console.log(`screenshots in ${shots}`);
}

await browser.close();
server.kill("SIGINT");
await Promise.race([server.exited, Bun.sleep(5000)]);
rmSync(root, { recursive: true, force: true });
check(errors.length === 0, "no page errors in the run", errors.join("; "));
console.log(failures.length ? `\n${failures.length} failed:\n- ${failures.join("\n- ")}` : "\nall passed");
process.exit(failures.length ? 1 : 0);
