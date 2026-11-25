// End-to-end test of the Ursa Web Shell in headless Chromium, against a
// running site: `bun run preview` (http://localhost:8788), `cf dev` or a deploy.
//
//   bun tests/e2e.ts [url] [screenshot dir]
//
// The page is the `ursa` shell in a terminal (xterm.js), so the test reads the
// screen as text, as a person would. It loads every dataset from the Datasets
// menu, types SQL at the prompt (one line and several), runs dot-commands (the
// shell's and the page's), downloads the last result in each format, imports a
// file, replays a shared link in a fresh page, and checks New, the theme toggle
// and history, Shift+Enter, Ctrl+C, overlapping datasets and an engine crash. It
// also checks the keyboard (Tab out of the terminal, the Datasets menu's keys,
// the focus after a click), a phone's bar and SQL, and the grey text's
// contrast. The line editor's checks cover wide characters (CJK and emoji),
// lines that wrap or fill the width exactly, resizing, colour, and its keys.
// And it checks what keeps the page safe: a shared link asks before it runs,
// shows control characters as escapes and downloads nothing; awkward file
// names import; and every response carries the security headers, which the
// page runs under with no Content Security Policy violation. An error on
// screen fails the run, except the ones a test asks for. It also runs the
// tour on a phone, and opens every "Try it live" link in the README, the SQL
// docs and the website (a Share link is answered y).
import { chromium, type Page } from "playwright";

const base = (Bun.argv[2] ?? "http://localhost:8788").replace(/\/$/, "");
const shots = Bun.argv[3];
const failures: string[] = [];
const check = (ok: boolean, label: string, detail = "") => {
  console.log(`${ok ? "ok  " : "FAIL"} ${label}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures.push(`${label}${detail ? `: ${detail}` : ""}`);
};

const launched = await chromium.launch();
// Against `ursa serve` (scripts/e2e-serve.ts sets URSA_SERVE_TOKEN): every
// context carries the run's token as the cookie the page gets from the link
// ursa serve prints, and the checks of browser-only features are skipped,
// each with a "skip" line: the engine's crash (a WebAssembly trap), its
// WebAssembly memory, and the wasm engine's files and worker.
const serveToken = process.env.URSA_SERVE_TOKEN ?? "";
const overSocket = serveToken !== "";
const serveCookie = overSocket ? `ursa_token_${new URL(base).port}=${serveToken}` : "";
const skip = (label: string) => console.log(`skip ${label}: browser-only (the wasm engine)`);
const withToken = async <C extends { addCookies(c: { name: string; value: string; url: string }[]): Promise<void> }>(c: C) => {
  if (overSocket) await c.addCookies([{ name: `ursa_token_${new URL(base).port}`, value: serveToken, url: base }]);
  return c;
};
const browser = new Proxy(launched, {
  get(target, key) {
    if (key === "newContext") return async (options?: any) => withToken(await target.newContext(options));
    if (key === "newPage")
      return async (options?: any) => {
        const ctx = await withToken(await target.newContext(options));
        return ctx.newPage();
      };
    const value = (target as any)[key];
    return typeof value === "function" ? value.bind(target) : value;
  },
});
// Bun's own requests (the headers' checks) carry the cookie too.
const fetch = (url: string, init: RequestInit = {}) =>
  globalThis.fetch(url, overSocket ? { ...init, headers: { ...(init.headers as Record<string, string>), cookie: serveCookie } } : init);
const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, acceptDownloads: true });
// Content Security Policy violations, from every page: the console's, and
// the documents' securitypolicyviolation events.
const violations: string[] = [];
await context.addInitScript(() => {
  document.addEventListener("securitypolicyviolation", (e) =>
    console.error(`CSP violation: ${e.violatedDirective} blocked ${e.blockedURI || "inline"} @${e.sourceFile}:${e.lineNumber}:${e.columnNumber}`),
  );
});
context.on("console", (m) => {
  if (m.type() === "error" && /Content Security Policy|CSP violation/i.test(m.text())) violations.push(m.text());
});
const page = await context.newPage();
page.on("pageerror", (e) => failures.push(`page error: ${e.message}`));

async function ready(p: Page) {
  // Not busy, and started: the banner and a prompt are on screen. A fresh page
  // is not busy for a moment before its session starts (and clears the screen).
  await p.waitForFunction(() => {
    const shell = (window as any).ursaShell;
    return shell && !shell.busy() && shell.screen().includes("memory U");
  }, null, { timeout: 120_000 });
}
async function idle(p: Page = page) {
  await p.evaluate(() => (window as any).ursaShell.idle());
  await ready(p);
}
const screen = (p: Page = page): Promise<string> => p.evaluate(() => (window as any).ursaShell.screen());
const count = (text: string, needle: string) => text.split(needle).length - 1;
/** Tables on screen: each starts with a ┌ at the start of a line. */
const tables = async (p: Page = page) => (await screen(p)).split("\n").filter((l) => l.startsWith("┌")).length;
const errors = async (p: Page = page) => count(await screen(p), "Error: ");
/** The queries in a dataset's commands: not its CREATEs, dot-commands or comments. */
const queries = (commands: string[]) =>
  commands.map((c) => c.split("\n").filter((l) => !/^\s*--/.test(l)).join("\n")).filter((c) => !/^(CREATE|\.)/i.test(c)).length;
/** What has the focus: an id, or the terminal. */
const focused = (p: Page = page): Promise<string> =>
  p.evaluate(() => (document.activeElement?.classList.contains("xterm-helper-textarea") ? "terminal" : document.activeElement?.id || document.activeElement?.tagName || ""));
/** WCAG contrast of two computed CSS colours, rgb(...). */
function contrast(a: string, b: string) {
  const lum = (c: string) => {
    const [r, g, bl] = c.match(/\d+(\.\d+)?/g)!.slice(0, 3).map((v) => {
      const x = Number(v) / 255;
      return x <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4;
    });
    return 0.2126 * r + 0.7152 * g + 0.0722 * bl;
  };
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}
/** The colour of the types under a table's header (the shell's grey), and the terminal's background. */
// xterm draws its rows on the next animation frame: wait for the span.
const greyText = (p: Page = page) =>
  p
    .waitForFunction(
      () => {
        const span = [...document.querySelectorAll(".xterm-rows span")].findLast((s) => s.textContent!.trim() === "Int64");
        return span ? [getComputedStyle(span).color, getComputedStyle(document.querySelector(".terminal")!).backgroundColor] : null;
      },
      null,
      { timeout: 5_000 },
    )
    .then((h) => h.jsonValue() as Promise<string[]>, () => null);

/** Type `text` at the prompt, a line at a time, each with Enter; wait for it. */
async function type(text: string, p: Page = page) {
  for (const line of text.split("\n")) {
    await p.keyboard.type(line);
    await p.keyboard.press("Enter");
  }
  await idle(p);
}
/** What the last command printed: the screen after the last line that starts with `prompt + first`. */
async function after(first: string, p: Page = page): Promise<string> {
  const lines = (await screen(p)).split("\n");
  for (let i = lines.length - 1; i >= 0; i--) if (lines[i].startsWith(`memory U ${first}`)) return lines.slice(i + 1).join("\n");
  return "";
}
/** The cells of the last table in `text`, row by row (header, types, then rows). */
function lastTable(text: string) {
  const lines = text.split("\n");
  const end = lines.findLastIndex((l) => l.startsWith("└"));
  const start = lines.slice(0, end).findLastIndex((l) => l.startsWith("┌"));
  const rows = lines.slice(start + 1, end).filter((l) => l.startsWith("│"));
  const cells = rows.map((l) => l.slice(1, -1).split("│").map((c) => c.trim()));
  return { head: cells[0] ?? [], types: cells[1] ?? [], rows: cells.slice(2), footer: lines[end + 1] ?? "" };
}

const t0 = Date.now();
await page.goto(base);
await ready(page);
const banner = await screen();
check(/Ursa Web Shell running\.\.\.\nUrsa v\d\S*\nEnter "\.help" for usage hints\.\nPick a sample graph from the Datasets menu, or type \.dataset karate\nmemory U/.test(banner), "banner", `${banner.match(/Ursa v\S+/)?.[0]} (${Date.now() - t0} ms)`);
await page.click("#terminal");

// The keyboard can leave the terminal: Tab and Shift+Tab move the focus on
// (there is no completion for them to do), and Tab comes back to it.
{
  const before = await screen();
  await page.keyboard.press("Shift+Tab");
  check((await focused()) === "theme", "Shift+Tab leaves the terminal", await focused());
  await page.keyboard.press("Tab");
  check((await focused()) === "terminal", "Tab goes back into the terminal", await focused());
  await page.keyboard.press("Tab");
  const left = await focused();
  check(left !== "terminal" && (await screen()) === before, "Tab leaves the terminal, typing nothing", left);
  // The theme toggle: a click hands the keys back to the terminal; from the
  // keyboard, the focus stays on it.
  await page.click("#theme");
  check((await focused()) === "terminal", "focus is back in the terminal after a theme click", await focused());
  await page.focus("#theme");
  await page.keyboard.press("Enter");
  check((await focused()) === "theme", "focus stays on the theme toggle pressed from the keyboard", await focused());
  await page.click("#terminal");
}

// The Datasets menu from the keyboard: a menu button.
{
  const menuState = () =>
    page.evaluate(() => ({
      open: !document.getElementById("dataset-menu")!.hidden,
      expanded: document.getElementById("datasets")!.getAttribute("aria-expanded"),
      item: [...document.querySelectorAll("#dataset-menu [role=menuitem]")].indexOf(document.activeElement!),
    }));
  const n = await page.locator("#dataset-menu [role=menuitem]").count();
  const steps: [string, number][] = [["ArrowDown", 1], ["ArrowUp", 0], ["ArrowUp", n - 1], ["ArrowDown", 0], ["End", n - 1], ["Home", 0]];
  await page.focus("#datasets");
  await page.keyboard.press("ArrowDown");
  let s = await menuState();
  let ok = s.open && s.expanded === "true" && s.item === 0;
  const seen = [s.item];
  for (const [key, want] of steps) {
    await page.keyboard.press(key);
    s = await menuState();
    seen.push(s.item);
    ok &&= s.item === want;
  }
  check(ok, "the Datasets menu's arrow keys, Home and End", JSON.stringify(seen));
  await page.keyboard.press("Escape");
  s = await menuState();
  check(!s.open && s.expanded === "false" && (await focused()) === "datasets", "Escape closes the menu, back on its button", JSON.stringify(s));
  const attrs = await page.evaluate(() => {
    const b = document.getElementById("datasets")!, m = document.getElementById("dataset-menu")!;
    return [b.getAttribute("aria-haspopup"), b.getAttribute("aria-controls"), m.getAttribute("role"), m.getAttribute("aria-labelledby"), m.querySelector("button")!.tabIndex].join(",");
  });
  check(attrs === "menu,dataset-menu,menu,datasets,-1", "the menu's aria attributes", attrs);
  await page.click("#terminal");
}

// Every dataset, from the menu: each starts a fresh session, and its queries
// print a table each, with no error.
const datasets: { id: string; title: string; commands: string[] }[] = await page.evaluate(() => (window as any).ursaShell.datasets);
for (const [i, d] of datasets.entries()) {
  const t = Date.now();
  if (i === 0) {
    // The first from the keyboard: Enter opens the menu on its first item, and
    // Enter picks it.
    await page.focus("#datasets");
    await page.keyboard.press("Enter");
    await page.keyboard.press("Enter");
  } else {
    await page.click("#datasets");
    await page.click(`#dataset-menu button:has-text("${d.title}")`);
  }
  await idle();
  const text = await screen();
  check(!text.includes("Error: ") && (await tables()) === queries(d.commands) && text.startsWith("Ursa Web Shell running"), `dataset ${d.id}`, `${await tables()} tables in ${Date.now() - t} ms`);
  if (i === 0) {
    check((await focused()) === "datasets", "a dataset picked from the keyboard leaves the focus on the menu button", await focused());
    // Its explanations are shown, as SQL comments.
    check(/^memory U -- \S/m.test(text), "a dataset's explanations show as comments", text.match(/^memory U --.*$/m)?.[0]);
    await page.click("#terminal");
  }
  if (d.id === "rmat") check(text.includes("Run Time (s): real "), "the R-MAT dataset turns the timer on");
  if (shots && d.id === "karate") await page.screenshot({ path: `${shots}/shell-karate.png` });
}
await type(".tables");
check(JSON.stringify(lastTable(await after(".tables")).rows.map((r) => r[0])) === '["rmat_edges"]', "a dataset replaces the last one", JSON.stringify(lastTable(await after(".tables")).rows));
await type(".dataset karate");
check((await tables()) === 3 && !(await errors()), ".dataset karate");

// SQL at the prompt: one line, several lines, and an error.
await type("SELECT count(*) AS members FROM members;");
let t = lastTable(await after("SELECT count(*) AS members"));
check(t.rows[0]?.[0] === "34" && t.types[0] === "Int64", "one line", JSON.stringify(t.rows));
await type("SELECT club,\n       count(*) AS n\nFROM members\nGROUP BY club\nORDER BY club;");
t = lastTable(await after("SELECT club,"));
check(JSON.stringify(t.rows) === '[["Mr. Hi","17"],["Officer","17"]]', "several lines", JSON.stringify(t.rows));
const lines = (await screen()).split("\n");
const at = lines.findLastIndex((l) => l.startsWith("memory U SELECT club,"));
check(lines[at + 1] === "                count(*) AS n", "continuation lines line up under the prompt", JSON.stringify(lines[at + 1]));
await type("SELECT * FROM no_such_table;");
check((await after("SELECT * FROM no_such_table")).startsWith("Error: ") && (await after("SELECT * FROM no_such_table")).includes("no_such_table"), "an error is shown");
const errorsSoFar = await errors();

// Long results: the first and last rows, a gap and a footer, as the CLI shows.
await type(".maxrows 6");
await type("SELECT * FROM ties ORDER BY src, dst;");
t = lastTable(await after("SELECT * FROM ties"));
check(t.rows.length === 9 && /78 rows \(6 shown\)/.test(t.footer), ".maxrows and a long table", `${t.rows.length} rows, footer ${t.footer}`);
await type(".maxrows 40");

// Dot-commands: the shell's own.
await type(".help");
const help = await after(".help");
check(help.includes(".describe NAME") && help.includes(".share") && help.includes("Run .help graph"), ".help lists the shell's and the page's commands");
await type(".help graph");
check((await after(".help graph")).includes("ANY SHORTEST | ALL SHORTEST | SHORTEST k"), ".help graph");
{
  // .help starts with where to begin: the tour, and how to draw a graph.
  const first = help.split("\n.clear")[0];
  check(first.includes(".tour") && first.includes(".graph karate"), ".help names .tour and .graph before the list", JSON.stringify(first.slice(0, 120)));
}
await type(".help shortcuts");
check((await after(".help shortcuts")).includes("Shift+Enter"), ".help shortcuts (the page's keys)");
await type(".help timer");
const timer = (await after(".help timer")).split("\n").filter((l) => l.trim() && !l.startsWith("memory U"));
check(timer.length === 1 && timer[0].startsWith(".timer on|off"), ".help PATTERN", JSON.stringify(timer));
await type(".tables");
t = lastTable(await after(".tables"));
check(t.rows.some((r) => r[0] === "members") && !t.rows.some((r) => r[0] === "rmat_edges"), ".tables", JSON.stringify(t.rows.map((r) => r[0])));
await type(".graphs");
check((await after(".graphs")).includes("karate"), ".graphs");
await type(".describe karate");
check(!(await after(".describe karate")).includes("Error"), ".describe karate");
await type(".functions");
t = lastTable(await after(".functions"));
check(t.rows.length >= 15 && t.rows.some((r) => r[0] === "pagerank"), ".functions", `${t.rows.length} functions`);
await type(".functions pagerank");
check((await after(".functions pagerank")).includes("damping"), ".functions pagerank");
await type(".timer on");
await type("SELECT 1 AS one;");
check((await after("SELECT 1 AS one")).includes("Run Time (s): real "), ".timer on");
await type(".timer off");
await type(".version");
check(/^Ursa v\d/.test(await after(".version")), ".version");
// The page's own.
await type(".files");
check(lastTable(await after(".files")).rows.some((r) => r[0] === "karate_edges.parquet"), ".files");
await type(".history 2");
check((await after(".history 2")).includes(".files"), ".history");
await type(".dataset");
check(lastTable(await after(".dataset")).rows.length === datasets.length, ".dataset lists");
// COPY … TO writes a file in the session, which a read finds by its name
// (#517). A relative path was "relative URL without a base", and
// memory://uploads/… ended the engine.
await type("COPY (SELECT * FROM layout_fa2(karate)) TO 'pos.parquet';");
check(lastTable(await after("COPY (SELECT * FROM layout_fa2(karate))")).rows[0]?.[0] === "34" && (await errors()) === errorsSoFar, "COPY … TO a relative path writes a session file", await after("COPY (SELECT * FROM layout_fa2(karate))"));
await type("SELECT count(*) AS n, count(x) AS placed FROM 'pos.parquet';");
check(JSON.stringify(lastTable(await after("SELECT count(*) AS n, count(x)")).rows[0]) === '["34","34"]', "a file COPY wrote reads back by its name", await after("SELECT count(*) AS n, count(x)"));
await type("COPY (SELECT * FROM ties LIMIT 5) TO 'memory://uploads/ties.csv';");
const tiesWritten = lastTable(await after("COPY (SELECT * FROM ties LIMIT 5)")).rows[0]?.[0];
await type("SELECT count(*) AS n FROM 'memory://uploads/ties.csv';");
const tiesByUrl = lastTable(await after("SELECT count(*) AS n FROM 'memory:")).rows[0]?.[0];
await type("SELECT count(*) AS n FROM 'ties.csv';");
const tiesByName = lastTable(await after("SELECT count(*) AS n FROM 'ties.csv'")).rows[0]?.[0];
check(Number(tiesWritten) > 0 && tiesByUrl === tiesWritten && tiesByName === tiesWritten && !(await screen()).includes("the engine stopped"), "COPY … TO 'memory://uploads/…' writes the file, and the engine runs on", `${tiesWritten} written, ${tiesByUrl} by URL, ${tiesByName} by name`);
for (const ext of ["json", "arrow"]) {
  await type(`COPY (SELECT 7 AS seven) TO 'seven.${ext}';`);
  await type(`SELECT seven + 1 AS eight FROM 'seven.${ext}';`);
  check(lastTable(await after("SELECT seven + 1 AS eight")).rows[0]?.[0] === "8", `COPY … TO a .${ext} file round-trips`, await after("COPY (SELECT 7 AS seven)"));
}
await type(".files");
t = lastTable(await after(".files"));
check(
  ["pos.parquet", "ties.csv", "seven.json", "seven.arrow"].every((f) => t.rows.some((r) => r[0] === f && Number(r[1]) > 0 && r[2] === "written here")) && t.rows.some((r) => r[0] === "karate_edges.parquet"),
  ".files lists the files COPY wrote",
  JSON.stringify(t.rows.filter((r) => r[2] === "written here")),
);
// The forms .help graph shows, on the karate graph: they must parse and run.
for (const sql of [
  "FROM GRAPH_TABLE (karate MATCH (a)<-[e]-(b) WHERE a.id = 1 COLUMNS (b.id));",
  "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 1)-[e]->{1,3}(b) COLUMNS (b.id));",
  "FROM GRAPH_TABLE (karate MATCH p = TRAIL (a WHERE a.id = 1)-[e]->+(b WHERE b.id = 33) COLUMNS (path_length(p))) LIMIT 5;",
  "FROM GRAPH_TABLE (karate MATCH p = SHORTEST 2 (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26) COLUMNS (path_length(p), vertices(p), edges(p)));",
  "SELECT * FROM pagerank(karate, damping => 0.85) LIMIT 3;",
  "SELECT * FROM algorithms(karate, pagerank(), community => louvain(seed => 1)) LIMIT 3;",
])
  await type(sql);
check((await errors()) === errorsSoFar, "no errors from the dot-commands and graph forms", `${(await errors()) - errorsSoFar} new`);
// What cannot be written is refused, with what works; nothing stops.
for (const [target, says] of [
  ["https://example.com/pos.parquet", "writes only this session's files"],
  ["out/pos.parquet", "with no folder"],
  ["pos.txt", "names no format"],
]) {
  await type(`COPY (SELECT 1 AS a) TO '${target}';`);
  const said = (await after("COPY (SELECT 1 AS a)")).replace(/\n/g, " ");
  check(said.includes("Error: ") && said.replace(/\s+/g, " ").includes(says) && said.includes(`'${target}'`) && !said.includes("memory:") && said.includes("name.parquet") === (target !== "pos.txt"), `COPY … TO '${target}' is refused, saying what works`, said);
}
await type("SELECT 41 + 1 AS still;");
check(lastTable(await after("SELECT 41 + 1 AS still;")).rows[0]?.[0] === "42" && !(await screen()).includes("the engine stopped"), "the engine runs on after the COPY refusals");

// Type-ahead: keys pressed while a line runs are kept.
await page.keyboard.type("SELECT 2 AS");
await page.keyboard.press("Enter");
await page.keyboard.type(" two;");
await page.keyboard.press("Enter");
await idle();
check(lastTable(await after("SELECT 2 AS")).rows[0]?.[0] === "2", "keys typed while a line runs are kept");

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
await page.waitForFunction(() => (window as any).ursaShell.screen().includes(`FROM "my_ties" LIMIT 10;`), null, { timeout: 30_000 });
await idle();
t = lastTable(await after(`FROM "my_ties" LIMIT 10;`));
check(t.rows.length === 3 && t.head.join(",") === "src,dst", "import", JSON.stringify(t.rows));
// Names a URL or SQL would read otherwise: non-ASCII, #, and a keyword.
for (const [name, view] of [["données.csv", "données"], ["a#b.csv", "a_b"], ["order.csv", "order"]]) {
  const before = await errors();
  await page.setInputFiles("#file-input", { name, mimeType: "text/csv", buffer: Buffer.from("src,dst\n1,2\n2,3\n3,1\n") });
  await page.waitForFunction((v) => (window as any).ursaShell.screen().includes(`FROM "${v}" LIMIT 10;`), view, { timeout: 30_000 });
  await idle();
  t = lastTable(await after(`FROM "${view}" LIMIT 10;`));
  check(t.rows.length === 3 && (await errors()) === before, `import ${name}`, JSON.stringify(t.rows));
}
// An import that fails prints its error on a line of its own, and the prompt
// and what was being typed come back below it.
await page.keyboard.type("SELECT 'draft'");
await page.setInputFiles("#file-input", { name: ".hidden.csv", mimeType: "text/csv", buffer: Buffer.from("a\n1\n") });
await page.waitForFunction(() => /\nError: [^\n]*\.hidden\.csv/.test((window as any).ursaShell.screen()), null, { timeout: 30_000 });
await idle();
{
  const lines = (await screen()).split("\n");
  const at = lines.findLastIndex((l) => l.includes(".hidden.csv"));
  check(lines[at].startsWith("Error: ") && lines[at - 1] !== "memory U SELECT 'draft'" && lines.at(-1) === "memory U SELECT 'draft'", "an import error prints on its own line, then the prompt", JSON.stringify(lines.slice(at - 1)));
}
await page.keyboard.press("Control+C");
await idle();

// History: the up arrow brings back the last command.
await page.keyboard.press("ArrowUp");
await page.waitForTimeout(100);
const recalled = (await screen()).split("\n").at(-1) ?? "";
check(recalled.startsWith("memory U ") && recalled.length > "memory U ".length, "history (up arrow)", recalled.slice(0, 60));
await page.keyboard.press("Control+C");
await idle();

// Copy: a selection made with the mouse copies as text, whole lines, the
// tables' boxes and the statements' indentation as on screen.
await context.grantPermissions(["clipboard-read", "clipboard-write"]);
await type("SELECT club,\n       count(*) AS n\nFROM members GROUP BY club ORDER BY club;");
{
  const box = (await page.locator(".xterm-screen").boundingBox())!;
  const rows: number = await page.evaluate(() => (window as any).ursaShell.rows());
  const rowHeight = box.height / rows;
  const shown = (await screen()).split("\n").slice(-rows);
  const top = shown.findLastIndex((l) => l.startsWith("memory U SELECT club,"));
  const bottom = shown.findLastIndex((l) => l.startsWith("└"));
  await page.mouse.move(box.x + 1, box.y + rowHeight * (top + 0.5));
  await page.mouse.down();
  await page.mouse.move(box.x + box.width - 2, box.y + rowHeight * (bottom + 0.5), { steps: 8 });
  await page.mouse.up();
  await page.keyboard.press("Control+C");
  await page.waitForTimeout(100);
  const copied = await page.evaluate(() => navigator.clipboard.readText());
  check(
    copied.startsWith("memory U SELECT club,\n                count(*) AS n\n") && copied.includes("│ Mr. Hi   │    17 │") && copied.trimEnd().endsWith("┘"),
    "copy a statement and its table",
    JSON.stringify(copied.slice(0, 80)),
  );
}

// Share: a link that replays the session's commands in a fresh page.
await page.click("#share");
await idle();
const url = page.url().includes("#run=") ? page.url() : await page.evaluate(() => navigator.clipboard.readText());
check(url.includes("#run="), "share link", `${url.length} characters`);
/** Open a shared link in a new page and wait for its question. */
async function openShared(link: string) {
  const p = await context.newPage();
  p.on("pageerror", (e) => failures.push(`replay page error: ${e.message}`));
  await p.goto(link);
  await p.waitForFunction(() => (window as any).ursaShell?.screen().includes("? [y/N] "), null, { timeout: 120_000 });
  return p;
}
const linkTo = (commands: string[]) =>
  `${base}/#run=${Buffer.from(JSON.stringify(commands)).toString("base64").replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "")}`;

// A shared link shows its commands and asks; only y runs them.
let replay = await openShared(url);
{
  const asked = await screen(replay);
  const n = asked.match(/Run (\d+) shared commands\? \[y\/N\]/)?.[1];
  check(!!n && asked.includes(`This link runs ${n} shared commands:`) && (await tables(replay)) === 0, "a shared link asks before it runs", `${n} commands`);
}
let replayDownloads = 0;
replay.on("download", () => replayDownloads++);
await replay.keyboard.press("y");
await idle(replay);
const replayed = await screen(replay);
// The imported file is not in the link: its view is the one error expected.
const replayErrors = replayed.split("\n").filter((l) => l.startsWith("Error: "));
check(replayed.includes("Replaying") && replayErrors.every((e) => /my_ties/.test(e)) && (await tables(replay)) > 5, "shared link replays after y", replayErrors[0] ?? `${await tables(replay)} tables`);
check(replayDownloads === 0 && !replayed.includes("Downloaded "), "a shared link downloads nothing", `${replayDownloads} downloads`);
await replay.close();

replay = await openShared(url);
await replay.keyboard.press("n");
await idle(replay);
{
  const text = await screen(replay);
  check(/\[y\/N\] N\nNot run\.\nmemory U/.test(text) && (await tables(replay)) === 0 && !text.includes("Replaying"), "a shared link answered N runs nothing");
  await type("SELECT 3 AS three;", replay);
  check(lastTable(await after("SELECT 3 AS three;", replay)).rows[0]?.[0] === "3", "the shell works after N");
}
await replay.close();

// A crafted link: escape sequences are shown as escapes, in the question, the
// echo and .print, and its .export downloads nothing.
replay = await openShared(linkTo([".print \x1b[2J\x1b[8mhidden\x07\u009b0m", "SELECT 1 AS \"\x1b[31mone\";", ".export csv", "SELECT 2 AS two;"]));
{
  const asked = await screen(replay);
  check(asked.includes("  .print \\u{1b}[2J\\u{1b}[8mhidden\\u{7}\\u{9b}0m") && !asked.includes("\x1b"), "a shared link's commands are shown with escapes", asked.split("\n").find((l) => l.includes(".print")));
  let downloads = 0;
  replay.on("download", () => downloads++);
  await replay.keyboard.press("y");
  await idle(replay);
  const text = await screen(replay);
  const printed = (await after(".print", replay)).split("\n")[0];
  check(printed === "\\u{1b}[2J\\u{1b}[8mhidden\\u{7}\\u{9b}0m", ".print writes control characters as escapes", JSON.stringify(printed));
  check(text.includes(`memory U SELECT 1 AS "\\u{1b}[31mone";`) && text.includes("This link runs 4 shared commands:"), "replayed commands are echoed with escapes");
  check(downloads === 0 && text.includes("Skipped .export") && lastTable(await after("SELECT 2 AS two;", replay)).rows[0]?.[0] === "2", "a crafted link's .export is skipped and the rest runs", `${downloads} downloads`);
}
await replay.close();

// New: a fresh session and a clear screen.
await page.click("#new");
await idle();
check((await screen()).startsWith("Ursa Web Shell running"), "New clears the screen");
await type("SELECT count(*) FROM members;");
check((await after("SELECT count(*) FROM members")).includes("Error: "), "New starts an empty session");

// Shift+Enter adds a line to what is typed; it doesn't run it.
await page.keyboard.type("SELECT 7 AS seven,");
await page.keyboard.press("Shift+Enter");
await page.keyboard.type("8 AS eight;");
await page.waitForTimeout(200);
check(!(await screen()).includes("│     7 │"), "Shift+Enter doesn't run the line");
check(
  (await screen()).split("\n").slice(-2).join("\n") === "memory U SELECT 7 AS seven,\n         8 AS eight;",
  "Shift+Enter adds one line, and sends nothing to the shell",
  JSON.stringify((await screen()).split("\n").slice(-3)),
);
await page.keyboard.press("Enter");
await idle();
check(lastTable(await after("SELECT 7 AS seven,")).rows[0]?.join(",") === "7,8", "Shift+Enter then Enter runs both lines", JSON.stringify(lastTable(await after("SELECT 7 AS seven,")).rows));

// Ctrl+C drops a half-typed statement; what is typed right after it runs, in order.
await type("SELECT 1 AS");
await page.keyboard.press("Control+C");
await page.keyboard.type("SELECT 5 AS five;");
await page.keyboard.press("Enter");
await page.keyboard.type("SELECT 6 AS six;");
await page.keyboard.press("Enter");
await idle();
{
  const text = await screen();
  const five = text.lastIndexOf("│     5 │"), six = text.lastIndexOf("│     6 │");
  check(five > 0 && six > five, "keys typed after Ctrl+C run, in order", `five at ${five}, six at ${six}`);
}

// Share with a statement half typed: the statement is dropped, Share works,
// and what was being typed comes back.
await type("SELECT 2 AS");
await page.keyboard.type("SELECT 'draft'");
await page.click("#share");
await idle();
{
  const text = await screen();
  check(/Copied a link|address bar|No commands/.test(text.split("\n").slice(-4).join("\n")), "Share with a half-typed statement", text.split("\n").slice(-3).join(" | "));
  check((text.split("\n").at(-1) ?? "").startsWith("memory U SELECT 'draft'"), "what was being typed comes back after Share", text.split("\n").at(-1));
}
await page.keyboard.press("Control+C");
await idle();

// Two datasets picked back to back: the second wins, with no errors.
for (const title of [datasets[0].title, datasets[2].title]) {
  await page.click("#datasets");
  await page.click(`#dataset-menu button:has-text("${title}")`);
}
await idle();
{
  const text = await screen();
  check(!text.includes("Error: ") && (await tables()) === queries(datasets[2].commands), "a dataset picked during another replaces it cleanly", `${await tables()} tables`);
}
// New during a dataset: the dataset stops; the session is empty.
await page.click("#datasets");
await page.click(`#dataset-menu button:has-text("${datasets[1].title}")`);
await page.click("#new");
await idle();
check(!(await screen()).includes("Error: ") && (await tables()) === 0, "New during a dataset stops it", `${await tables()} tables`);

// The line editor: wide characters, wrapping, resizing, colour and its keys.
{
  const shell = (fn: string, ...args: any[]) => page.evaluate(([fn, args]) => (window as any).ursaShell[fn as string](...(args as any[])), [fn, args] as const);
  const cursorAt = async () => (await shell("cursor")) as { x: number; y: number };
  const lastLines = async (n: number) => (await screen()).split("\n").slice(-n);
  const settle = async () => {
    await page.waitForTimeout(150);
    await page.evaluate(() => new Promise((r) => requestAnimationFrame(r)));
  };
  /** Drop what is typed. */
  const drop = async () => {
    await page.keyboard.press("Control+C");
    await idle();
  };
  await page.click("#new");
  await idle();

  // Wide characters: CJK takes two cells, an emoji sequence two; the cursor
  // goes where the characters are, and moves over (and deletes) them whole.
  await page.keyboard.type("SELECT '日本語' AS t");
  for (let i = 0; i < 6; i++) await page.keyboard.press("ArrowLeft");
  await settle();
  let c = await cursorAt();
  let line = (await screen()).split("\n")[c.y] ?? "";
  // memory U SELECT '日本語' AS t: the cursor is on the closing quote, after three wide characters.
  check(c.x === "memory U SELECT '".length + 6, "the cursor after CJK text", `column ${c.x}, line ${JSON.stringify(line)}`);
  await page.keyboard.press("Backspace");
  await page.keyboard.type("x");
  await settle();
  c = await cursorAt();
  check((await lastLines(1))[0] === "memory U SELECT '日本x' AS t" && c.x === "memory U SELECT '".length + 5, "Backspace deletes a CJK character", `${JSON.stringify((await lastLines(1))[0])} column ${c.x}`);
  await drop();
  const cp = (...cps: number[]) => String.fromCodePoint(...cps);
  // A face, a ZWJ family, a flag, a skin tone, a heart with VS16, and e with a combining accent.
  const emoji = [cp(0x1f600), cp(0x1f468, 0x200d, 0x1f469, 0x200d, 0x1f467), cp(0x1f1fa, 0x1f1f8), cp(0x1f44d, 0x1f3fd), cp(0x2764, 0xfe0f), cp(0x65, 0x301)];
  await page.keyboard.insertText(`SELECT '${emoji.join("")}'`);
  await page.keyboard.press("ArrowLeft");
  await settle();
  const typed = `memory U SELECT '${emoji.join("")}`;
  c = await cursorAt();
  check(c.x === "memory U SELECT '".length + 11, "the cursor after emoji", `column ${c.x}`);
  const cells = ((await shell("cells", c.y)) as { ch: string }[]).map((x) => x.ch);
  check(JSON.stringify(cells.slice(17, 23)) === JSON.stringify(emoji), "an emoji sequence is one character on screen", JSON.stringify(cells.slice(17, 23)));
  let shown = "";
  for (const want of [...emoji].reverse()) {
    await page.keyboard.press("ArrowLeft");
    await page.keyboard.press("Delete");
    await settle();
    shown = (await lastLines(1))[0];
    if (!shown.startsWith(typed.slice(0, typed.lastIndexOf(want))) || shown.includes(cp(0xfffd))) break;
  }
  check(shown === "memory U SELECT ''", "Left and Delete take an emoji sequence whole", JSON.stringify(shown));
  await page.keyboard.press("End");
  await page.keyboard.insertText("🇺🇸🇫🇷");
  await page.keyboard.press("Backspace");
  await settle();
  check((await lastLines(1))[0] === "memory U SELECT ''🇺🇸", "Backspace deletes a flag whole", JSON.stringify((await lastLines(1))[0]));
  await drop();

  // A long CJK line wraps (a wide character that doesn't fit the row starts
  // the next one) and is drawn once however it is edited.
  const cols: number = await shell("cols");
  const long = "漢字".repeat(cols);
  await page.keyboard.insertText(`SELECT '${long}`);
  await page.keyboard.type("' AS kanji;");
  for (let i = 0; i < 11; i++) await page.keyboard.press("ArrowLeft");
  await page.keyboard.type("x");
  await page.keyboard.press("End");
  await page.keyboard.press("Backspace");
  await settle();
  {
    // The ; is gone: the line ends "AS kanji".
    const text = await screen();
    const rows = text.split("\n");
    const first = rows.findLastIndex((l) => l.startsWith("memory U SELECT '漢字"));
    const joined = rows.slice(first).join("");
    check(count(text, "memory U SELECT '漢字") === 1 && joined === `memory U SELECT '${long}x' AS kanji`, "a wrapped CJK line is drawn once", `${count(text, "memory U SELECT '漢字")} prompts, ${rows.length - first} rows`);
    c = await cursorAt();
    check(c.y === rows.length - 1 && c.x === [...rows.at(-1)!].reduce((n, ch) => n + (ch.charCodeAt(0) > 0x2e80 ? 2 : 1), 0), "the cursor at the end of a wrapped CJK line", JSON.stringify(c));
  }
  await page.keyboard.type(";");
  await page.keyboard.press("Enter");
  await idle();
  check(lastTable(await after("SELECT '漢字")).head[0] === "kanji", "a wrapped CJK line runs", JSON.stringify(lastTable(await after("SELECT '漢字")).head));

  // A line that fills the terminal's width exactly, with lines after it: no
  // empty row after it.
  const exact = "SELECT " + "1".repeat(cols - "memory U SELECT ".length - " AS w,".length) + " AS w,";
  await page.keyboard.type(exact);
  await page.keyboard.press("Shift+Enter");
  await page.keyboard.type("2 AS v;");
  await page.keyboard.press("ArrowUp");
  await settle();
  {
    const rows = await lastLines(2);
    check(rows[0] === `memory U ${exact}` && rows[1] === "         2 AS v;", "a line as wide as the terminal leaves no empty row", JSON.stringify(rows));
  }
  await page.keyboard.press("End");
  await settle();
  c = await cursorAt();
  check(c.y === (await screen()).split("\n").length - 1 && c.x === 0, "the cursor after a line as wide as the terminal", JSON.stringify(c));
  await page.keyboard.press("Enter");
  await idle();
  {
    const rows = (await after(exact.slice(0, 20))).split("\n");
    check(rows[0] === "         2 AS v;" && rows[1].startsWith("┌"), "a line as wide as the terminal, run", JSON.stringify(rows.slice(0, 2)));
  }

  // Resizing redraws what is typed for the new width, at once.
  const statement = "SELECT 'resize' AS r, " + Array.from({ length: 12 }, (_, i) => `${i} AS c${i}`).join(", ");
  await page.keyboard.type(statement);
  await page.keyboard.press("Shift+Enter");
  await page.keyboard.type("FROM members");
  await page.keyboard.press("ArrowUp");
  await page.keyboard.press("End");
  await settle();
  for (const width of [640, 1100]) {
    // The columns before the resize: read after it, they may be the new
    // ones already, and the wait below would never end.
    const before: number = await shell("cols");
    await page.setViewportSize({ width, height: 900 });
    await page.waitForFunction((n) => (window as any).ursaShell.cols() !== n, before);
    await settle();
    await settle();
    const n: number = await shell("cols");
    const first = `memory U ${statement}`;
    const want: string[] = [];
    for (let i = 0; i < first.length; i += n) want.push(first.slice(i, i + n));
    want.push("         FROM members");
    const text = await screen();
    const rows = text.split("\n");
    c = await cursorAt();
    check(
      count(text, "memory U SELECT 'resize'") === 1 && JSON.stringify(rows.slice(-want.length)) === JSON.stringify(want),
      `resizing to ${n} columns redraws what is typed`,
      `${count(text, "memory U SELECT 'resize'")} prompts; ${JSON.stringify(rows.slice(-want.length - 1))}`,
    );
    const end = (statement.length + 9) % n;
    check(c.y === rows.length - 2 && c.x === end, `the cursor where it was, at ${n} columns`, `${JSON.stringify(c)}, want x ${end}`);
  }
  await page.setViewportSize({ width: 1280, height: 900 });
  await settle();
  await drop();

  // What is typed and run before the highlighter answers is coloured all the same.
  await shell("input", [..."SELECT 'quick' AS fast;", "\r"]);
  await idle();
  {
    const rows = (await screen()).split("\n");
    const y = rows.findLastIndex((l) => l.startsWith("memory U SELECT 'quick'"));
    const cells = (await shell("cells", y)) as { ch: string; fg: number; bold: boolean }[];
    const word = (from: number, n: number) => cells.slice(from, from + n);
    const keyword = word(9, 6), string = word(16, 7);
    check(keyword.every((x) => x.bold || x.fg >= 0) && string.every((x) => x.fg >= 0) && string[0].fg !== keyword[0].fg, "a line run at once is coloured", JSON.stringify([keyword[0], string[0]]));
  }

  // .help shortcuts lists the keys the line editor knows, and they do what it says.
  await type(".help shortcuts");
  {
    const listed = (await after(".help shortcuts"))
      .split("\n")
      .filter((l) => /^\S/.test(l) && !l.startsWith("memory U"))
      .flatMap((l) => {
        const [keys, what] = l.split(/\s{2,}/);
        return [...keys.split(", "), ...(what.match(/\(also ([^)]*)\)/)?.[1].split(", ") ?? [])];
      });
    const known = ["Enter", "Shift+Enter", "Up", "Down", "Left", "Right", "Ctrl+B", "Ctrl+F", "Home", "End", "Ctrl+A", "Ctrl+E", "Backspace", "Ctrl+H", "Delete", "Ctrl+U", "Ctrl+K", "Ctrl+W", "Tab", "Shift+Tab", "Ctrl+C", "Cmd+C", "Ctrl+L"];
    check(JSON.stringify([...listed].sort()) === JSON.stringify([...known].sort()), ".help shortcuts lists the editor's keys", JSON.stringify(listed));
    // Each editing key on "one two three", the cursor after "two": what is left, and where the cursor is.
    const edits: [string[], string, number][] = [
      [["ArrowLeft"], "one two three", 6],
      [["Control+B"], "one two three", 6],
      [["ArrowRight"], "one two three", 8],
      [["Control+F"], "one two three", 8],
      [["Home"], "one two three", 0],
      [["Control+A"], "one two three", 0],
      [["End"], "one two three", 13],
      [["Control+E"], "one two three", 13],
      [["Backspace"], "one tw three", 6],
      [["Control+H"], "one tw three", 6],
      [["Delete"], "one twothree", 7],
      [["Control+U"], " three", 0],
      [["Control+K"], "one two", 7],
      [["Control+W"], "one  three", 4],
    ];
    const wrong: string[] = [];
    for (const [keys, want, at] of edits) {
      await page.keyboard.type("one two three");
      for (let i = 0; i < 6; i++) await page.keyboard.press("ArrowLeft");
      for (const k of keys) await page.keyboard.press(k);
      await settle();
      const got = (await lastLines(1))[0];
      c = await cursorAt();
      if (got !== `memory U ${want}` || c.x !== 9 + at) wrong.push(`${keys}: ${JSON.stringify(got)} at ${c.x - 9}`);
      await drop();
    }
    check(!wrong.length, "the editing keys do what .help shortcuts says", wrong.join("; "));
  }
}

// The engine crashing (out of memory) leaves a working shell, in a fresh session.
if (overSocket) {
  skip("an engine crash is reported");
  skip("the shell works after an engine crash");
} else {
  await type("SELECT length(repeat('x', 2000000000)) AS n;");
  await idle();
  check((await screen()).includes("the engine stopped"), "an engine crash is reported");
  await type("SELECT 41 + 1 AS answer;");
  check(lastTable(await after("SELECT 41 + 1 AS answer;")).rows[0]?.[0] === "42", "the shell works after an engine crash");
}

// The theme toggle, remembered.
const theme0 = await page.evaluate(() => document.documentElement.dataset.theme ?? "");
await page.click("#theme");
const theme1 = await page.evaluate(() => document.documentElement.dataset.theme ?? "");
await page.reload();
await ready(page);
const theme2 = await page.evaluate(() => document.documentElement.dataset.theme ?? "");
check(theme1 !== theme0 && theme2 === theme1, "theme toggle", `${theme0 || "system"} → ${theme1} → ${theme2} after reload`);
if (shots) await page.screenshot({ path: `${shots}/shell-start-${theme1}.png` });

// A narrow screen: tables fit it.
const phone = await browser.newPage({ viewport: { width: 390, height: 844 } });
await phone.goto(`${base}/?dataset=karate`);
await ready(phone);
await idle(phone);
const columns: number = await phone.evaluate(() => (window as any).ursaShell.cols());
const widest = Math.max(...(await screen(phone)).split("\n").filter((l) => /^[┌│└├]/.test(l)).map((l) => [...l].length));
check(widest <= columns, "tables fit a phone's width", `widest ${widest}, ${columns} columns`);
{
  const hint = (await screen(phone)).split("\n").slice(3, 6);
  check(hint.join(" ").startsWith("Pick a sample graph from the Datasets menu, or type .dataset karate") && hint.every((l) => [...l].length < columns), "the banner's hint breaks between words on a phone", JSON.stringify(hint));
}
if (shots) await phone.screenshot({ path: `${shots}/shell-phone.png` });
// The dataset's SQL is broken at spaces to fit: no line fills the width, as a
// line the terminal wraps (mid-word) does.
{
  const full = (await screen(phone)).split("\n").filter((l) => [...l].length >= columns);
  check(full.length === 0, "a dataset's SQL fits a phone without wrapping mid-word", full[0]);
}
// iOS zooms in on a text field with a font under 16px.
{
  const size: string = await phone.evaluate(() => getComputedStyle(document.querySelector(".xterm-helper-textarea")!).fontSize);
  check(parseFloat(size) >= 16, "the terminal's textarea is 16px or more", size);
}
await phone.close();

// The narrowest phone: the bar is one row, and the SQL still fits.
const small = await browser.newPage({ viewport: { width: 320, height: 640 } });
await small.goto(`${base}/?dataset=lesmis`);
await ready(small);
await idle(small);
{
  const bar = (await small.locator("header.bar").boundingBox())!;
  const fits: boolean = await small.evaluate(() =>
    [...document.querySelectorAll("header button, header a")].every((e) => {
      const r = e.getBoundingClientRect();
      return r.height <= 44 && r.right <= innerWidth && r.left >= 0;
    }),
  );
  check(bar.height <= 56 && fits, "the bar is one row at 320px", `${bar.height}px`);
  const cols: number = await small.evaluate(() => (window as any).ursaShell.cols());
  const full = (await screen(small)).split("\n").filter((l) => [...l].length >= cols);
  check(full.length === 0 && !(await errors(small)) && (await tables(small)) === queries(datasets[1].commands), "lesmis at 320px: no mid-word wraps, no errors", full[0] ?? `${await tables(small)} tables`);
}
await small.close();

// The dim text (types, footers, nulls) is readable: 4.5:1 or more on the
// terminal's background, in both themes.
for (const scheme of ["light", "dark"] as const) {
  const ctx = await browser.newContext({ viewport: { width: 1000, height: 700 }, colorScheme: scheme });
  const p = await ctx.newPage();
  await p.goto(base);
  await ready(p);
  await p.click("#terminal");
  await type("SELECT 1 AS one;", p);
  const colors = await greyText(p);
  const ratio = colors ? contrast(colors[0], colors[1]) : 0;
  check(ratio >= 4.5, `dim text contrast, ${scheme}`, `${ratio.toFixed(2)}:1 (${colors?.join(" on ")})`);
  await ctx.close();
}

// The Graph view (.graph): a result, a query or a property graph drawn in a
// pane beside the terminal (under it on a phone), loaded only when first
// asked for. Headless Chromium draws it with WebGL2 (SwiftShader).
/** A context watched for Content Security Policy violations, as the main one is. */
async function watchedContext(options: Parameters<typeof browser.newContext>[0]) {
  const ctx = await browser.newContext(options);
  await ctx.addInitScript(() => {
    document.addEventListener("securitypolicyviolation", (e) =>
      console.error(`CSP violation: ${e.violatedDirective} blocked ${e.blockedURI || "inline"} @${e.sourceFile}:${e.lineNumber}:${e.columnNumber}`),
    );
  });
  ctx.on("console", (m) => {
    if (m.type() === "error" && /Content Security Policy|CSP violation/i.test(m.text())) violations.push(m.text());
  });
  ctx.on("page", (p) =>
    p.on("pageerror", (e) => {
      // Said at once too: a later step may stop the run before the end.
      console.log(`FAIL page error: ${e.message}`);
      failures.push(`page error: ${e.message}`);
    }),
  );
  ctx.on("console", (m) => {
    if (m.type() === "error" && /panicked|unreachable|recursive use|borrowed/i.test(m.text())) console.log(`CONSOLE ${m.text().slice(0, 600)}`);
  });
  return ctx;
}
const graphInfo = (p: Page): Promise<any> => p.evaluate(() => (window as any).ursaShell.graph());
const graphOpen = (p: Page): Promise<boolean> => p.evaluate(() => (window as any).ursaShell.graphOpen());
/** Wait until the view has drawn a frame and its layout has run to the end. */
async function graphSettled(p: Page) {
  try {
    await p.waitForFunction(
      () => {
        const g = (window as any).ursaShell.graph();
        return g && g.frames > 0 && g.layoutDone;
      },
      null,
      { timeout: 60_000, polling: 200 },
    );
  } catch (e) {
    // What the page shows, so that a view that closed itself (a lost WebGL
    // context, a layout that stopped) says so here.
    const seen = await p
      .evaluate(() => {
        const shell = (window as any).ursaShell;
        return { open: shell.graphOpen(), graph: shell.graph(), status: document.getElementById("graph-status")?.textContent, screen: shell.screen().split("\n").slice(-6) };
      })
      .catch(() => null);
    throw new Error(`the graph view did not settle: ${JSON.stringify(seen)}`, { cause: e });
  }
}
/** Wait until the camera has made room for the inspector's panel, or given it back (#493). */
const roomSettled = (p: Page) => p.waitForFunction(() => (window as any).ursaShell.graph()?.room.waits === false, null, { timeout: 10_000, polling: 50 }).catch(() => {});
/** The terminal's columns, once a resize has settled. */
async function settledCols(p: Page) {
  let last = -1;
  for (let i = 0; i < 20; i++) {
    await p.waitForTimeout(100);
    const now: number = await p.evaluate(() => (window as any).ursaShell.cols());
    if (now === last) return now;
    last = now;
  }
  return last;
}
for (const scheme of ["light", "dark"] as const) {
  const gctx = await watchedContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 2, colorScheme: scheme });
  const g = await gctx.newPage();
  const vizRequests: string[] = [];
  g.on("request", (r) => {
    if (/\/viz\/|\/graph(-worker)?\.js/.test(r.url())) vizRequests.push(new URL(r.url()).pathname);
  });
  await g.goto(base);
  await ready(g);
  await g.click("#terminal");
  const fullCols = await settledCols(g);
  if (scheme === "light") {
    // Errors, each said plainly: nothing to draw yet, too few columns, no rows,
    // an unknown name, a query that fails.
    await type(".graph", g);
    check((await after(".graph", g)).startsWith("Error: no result to draw yet"), ".graph with no result says so", (await after(".graph", g)).split("\n")[0]);
    for (const [sql, says] of [
      ["SELECT 1 AS x", "it has 1 column (x: Int64)"],
      ["SELECT 1 AS src, 2 AS dst WHERE false", "the result has no rows"],
      ["nothing_here", "no graph or table named 'nothing_here'"],
      ["SELECT * FROM no_such_table", "no_such_table"],
    ]) {
      await type(`.graph ${sql}`, g);
      const out = await after(`.graph ${sql}`, g);
      check(out.startsWith("Error: ") && out.includes(says) && !(await graphOpen(g)), `.graph ${sql}: an error`, out.split("\n")[0]);
    }
  }
  await type(".dataset karate", g);
  check(!(await errors(g)) && vizRequests.length === 0 && !(await graphOpen(g)), `${scheme}: the graph view is not downloaded before .graph`, vizRequests.join(", "));

  // .graph draws the last result: karate's 78 ties between its 34 members.
  await type("SELECT * FROM ties;", g);
  await type(".graph", g);
  const drawing = (await after(".graph", g)).split("\n")[0];
  check(drawing === "Drawing 78 edges, 34 nodes as edges (src, dst) (Esc closes)", `${scheme}: .graph draws the last result, and says what as`, drawing);
  check(vizRequests.some((u) => u.startsWith("/viz/") && u.endsWith(".wasm")) && vizRequests.includes("/graph.js"), `${scheme}: .graph loads the graph view`, vizRequests.join(", "));
  await graphSettled(g);
  let info = await graphInfo(g);
  check(info.nodes === 34 && info.edges === 78 && info.iterations > 0 && info.layoutDone, `${scheme}: the view holds the graph, its layout run to the end`, JSON.stringify(info));
  if (scheme === "light") {
    // The layout worker's fixed layouts (#436): rings and layers are done in
    // one message. A seed, three neighbours, and two children of the second.
    const fixed: any = await g.evaluate(async () => {
      const { module } = await (await fetch("/viz/viz.json")).json();
      const run = (kind: string) =>
        new Promise((resolve) => {
          const worker = new Worker("/graph-worker.js", { type: "module" });
          const seen: any[] = [];
          worker.onmessage = ({ data }) => {
            seen.push(data.type === "positions" ? { type: data.type, xy: Array.from(data.xy as Float32Array), total: data.total, levels: data.levels } : data);
            if (data.type === "done" || data.type === "error") {
              worker.terminate();
              resolve(seen);
            }
          };
          const level = Uint32Array.of(0, 1, 1, 1, 2, 2);
          worker.postMessage({ type: "start", module: new URL(module, location.origin).href, n: 6, src: Uint32Array.of(0, 0, 0, 2, 2), dst: Uint32Array.of(1, 2, 3, 4, 5), layout: { kind, level } });
        });
      return { rings: await run("rings"), layers: await run("layers"), other: await run("spiral") };
    });
    const types = (m: any[]) => m.map((x) => x.type).join(",");
    const ringXy: number[] = fixed.rings[0]?.xy ?? [];
    const radii = [0, 1, 2, 3, 4, 5].map((v) => Math.hypot(ringXy[v], ringXy[6 + v]));
    // (After the positions, once, the order to draw the edges in: #495.)
    check(
      types(fixed.rings) === "positions,order,done" && fixed.rings[0].total === 0 && radii.every((r, v) => Math.abs(r - [0, 1, 1, 1, 2, 2][v]) < 1e-5),
      "the layout worker lays out rings in one message: each node at its distance",
      `${types(fixed.rings)}; radii ${radii.map((r) => r.toFixed(3))}`,
    );
    check(
      types(fixed.layers) === "positions,order,done" && JSON.stringify(fixed.layers[0].xy) === JSON.stringify([-1, 0, 0, 0, 1, 1, 0, 0.5, 0, -0.5, 0.25, -0.25]),
      "the layout worker lays out layers in one message: each layer a line, left to right",
      `${types(fixed.layers)}; ${JSON.stringify(fixed.layers[0]?.xy)}`,
    );
    // With no hierarchy, the edges at the nodes of highest degree are
    // first: node 0's and node 2's (degree 3 each), all five here, each once.
    const ringOrder: number[] = Array.from(fixed.rings[1]?.order ?? []);
    check(
      ringOrder.length === 5 && [...ringOrder].sort().join() === "0,1,2,3,4" && fixed.rings[1].ms >= 0,
      "the layout worker posts the order to draw the edges in, once: a permutation of them",
      JSON.stringify(ringOrder),
    );
    check(types(fixed.other) === "error" && fixed.other[0].message === "no layout called spiral", "the layout worker refuses a layout it does not know", JSON.stringify(fixed.other));
  }
  const inked = await g.evaluate(() => (window as any).ursaShell.graphInked());
  check(inked && inked.inked > 500, `${scheme}: the view draws the graph (non-blank pixels)`, `${inked?.inked} of ${inked?.width}x${inked?.height} px, ${info.backend}`);
  const pane = (await g.locator("#graph-pane").boundingBox())!;
  const termBox = (await g.locator("#terminal").boundingBox())!;
  const split = await settledCols(g);
  check(pane.x >= termBox.x + termBox.width - 1 && Math.abs(pane.width - termBox.width) < 4 && split < fullCols * 0.6, `${scheme}: the pane sits beside the terminal, which refits`, `terminal ${termBox.width}px, ${split} of ${fullCols} columns; pane ${pane.width}px`);
  const label = await g.evaluate(() => [document.getElementById("graph-pane")!.getAttribute("aria-label"), document.getElementById("graph-close")!.getAttribute("aria-label"), document.querySelector("#graph-stage canvas")?.getAttribute("aria-label")]);
  check(label.every(Boolean), `${scheme}: the pane, its close button and canvas are labelled`, JSON.stringify(label));
  check((await focused(g)) === "terminal", `${scheme}: the terminal keeps the focus while the graph shows`, await focused(g));
  if (shots) await g.screenshot({ path: `${shots}/graph-split-${scheme}.png` });
  if (scheme === "dark") {
    await gctx.close();
    continue;
  }

  // Typing works while the pane is open.
  await type("SELECT 6 * 7 AS answer;", g);
  check(lastTable(await after("SELECT 6 * 7", g)).rows[0]?.[0] === "42" && (await graphOpen(g)), "queries run while the graph shows");

  // #438: hover and selection. Node 33 is karate's officer, of degree 17.
  const box = (await g.locator("#graph-stage canvas").boundingBox())!;
  const place = (id: number): Promise<{ index: number; x: number; y: number } | null> => g.evaluate((id) => (window as any).ursaShell.graphNodePlace(id), id);
  const statusText = async () => (await g.locator("#graph-status").textContent()) ?? "";
  const frames2 = () => g.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(() => r(null)))));
  const n33 = (await place(33))!;
  const n0 = (await place(0))!;
  {
    // Hovering it shows its tooltip, beside it, and rings it.
    await g.mouse.move(box.x + n33.x, box.y + n33.y);
    await g.waitForFunction(() => (window as any).ursaShell.graph()?.hovered?.id == 33, null, { timeout: 5_000 }).catch(() => {});
    const tip = await g.evaluate(() => {
      const el = document.querySelector<HTMLElement>("#graph-stage .graph-tip");
      const stage = document.getElementById("graph-stage")!.getBoundingClientRect();
      const b = el?.getBoundingClientRect();
      return el && b ? { hidden: el.hidden, text: Array.from(el.children, (c) => c.textContent).join(" | "), inside: b.left >= stage.left && b.right <= stage.right && b.top >= stage.top && b.bottom <= stage.bottom, ariaHidden: el.getAttribute("aria-hidden") } : null;
    });
    info = await graphInfo(g);
    check(!!tip && !tip.hidden && /^node 33 \| (label \S+ \| )?degree 17$/.test(tip.text) && tip.inside && info.hovered?.index === n33.index, "hovering node 33 shows its tooltip: its key and degree", JSON.stringify(tip));
    check(info.selection.length === 0 && !(await statusText()).includes("Selected"), "a hover selects nothing");

    // Many moves in one frame are one pick.
    const picks = await g.evaluate(async ([x, y]) => {
      const shell = (window as any).ursaShell;
      const canvas = document.querySelector("#graph-stage canvas")!;
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
      const before = shell.graph().hoverPicks;
      for (let i = 0; i < 25; i++) canvas.dispatchEvent(new PointerEvent("pointermove", { pointerId: 1, pointerType: "mouse", clientX: x + (i % 5), clientY: y + (i % 3), bubbles: true }));
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
      return shell.graph().hoverPicks - before;
    }, [box.x + n33.x, box.y + n33.y]);
    check(picks === 1, "25 pointer moves in a frame are one pick", `${picks} picks`);

    // Off the node, the tooltip goes.
    await g.mouse.move(box.x + 3, box.y + 3);
    await frames2();
    check((await g.evaluate(() => document.querySelector<HTMLElement>("#graph-stage .graph-tip")?.hidden)) === true && (await graphInfo(g)).hovered === null, "off the node, the tooltip goes");

    // A click selects it: the status, a live region, names it, and a frame is drawn.
    const drawnBefore = (await graphInfo(g)).frames;
    await g.mouse.click(box.x + n33.x, box.y + n33.y);
    await frames2();
    info = await graphInfo(g);
    const live = await g.locator("#graph-status").getAttribute("aria-live");
    check(/^Selected node 33: (label \S+, )?degree 17 · 34 nodes · 78 edges$/.test(await statusText()) && live === "polite" && info.selection.length === 1 && info.picked?.index === n33.index && info.frames > drawnBefore, "a click on node 33 selects it, and the status names it", await statusText());
    const selectedInk = await g.evaluate(() => (window as any).ursaShell.graphInked());

    // Shift+click adds node 0; Shift+click on it again takes it out.
    await g.keyboard.down("Shift");
    await g.mouse.click(box.x + n0.x, box.y + n0.y);
    await g.keyboard.up("Shift");
    info = await graphInfo(g);
    check(/^Selected 2 nodes: 33, 0 · 34 nodes · 78 edges$/.test(await statusText()) && info.selection.map((n: any) => n.index).join() === `${n33.index},${n0.index}`, "Shift+click selects two", await statusText());
    await g.keyboard.down("Shift");
    await g.mouse.click(box.x + n0.x, box.y + n0.y);
    await g.keyboard.up("Shift");
    check(/^Selected node 33: /.test(await statusText()), "Shift+click on a selected node takes it out", await statusText());

    // A drag on empty space pans, and keeps the selection.
    await g.mouse.move(box.x + 4, box.y + 4);
    await g.mouse.down();
    await g.mouse.move(box.x + 24, box.y + 14, { steps: 4 });
    await g.mouse.up();
    const moved = (await place(33))!;
    check(Math.abs(moved.x - n33.x - 20) <= 1.5 && Math.abs(moved.y - n33.y - 10) <= 1.5 && /^Selected node 33: /.test(await statusText()), "a drag on empty space pans, and keeps the selection", JSON.stringify(moved));

    // A click on empty space clears it, and the frame loses the selection's ink.
    await g.mouse.click(box.x + 4, box.y + 4);
    await frames2();
    info = await graphInfo(g);
    const clearedInk = await g.evaluate(() => (window as any).ursaShell.graphInked());
    check(info.selection.length === 0 && /^34 nodes · 78 edges( · drawn as [^·]+)? · Done$/.test(await statusText()), "a click on empty space clears the selection", await statusText());
    check(selectedInk.inked > clearedInk.inked, "the selected node's ring and wide edges are drawn", `${selectedInk.inked} px inked selected, ${clearedInk.inked} cleared`);

    // Esc, with the focus in the pane, clears a selection and leaves the pane open.
    const again = (await place(33))!;
    await g.mouse.click(box.x + again.x, box.y + again.y);
    await g.focus("#graph-stage canvas");
    await g.keyboard.press("Escape");
    check((await graphOpen(g)) && (await graphInfo(g)).selection.length === 0 && !(await statusText()).includes("Selected"), "Esc clears the selection before it closes the pane", await statusText());
    await g.click("#terminal");
  }
  // Double-click fits, and the wheel zooms, without errors.
  await g.mouse.dblclick(box.x + box.width / 2, box.y + box.height / 2);
  await g.mouse.wheel(0, -200);

  // #430: graphTimings() says what the view cost to draw and to use. A wheel
  // zoom in and back out, an event a frame for 30 frames, so the view is
  // moving; then two keys with the focus on the canvas.
  await g.evaluate(
    ([x, y]) =>
      new Promise<void>((done) => {
        const canvas = document.querySelector("#graph-stage canvas")!;
        let n = 0;
        const step = () => {
          if (n === 30) return done();
          canvas.dispatchEvent(new WheelEvent("wheel", { deltaY: n++ < 15 ? -30 : 30, clientX: x, clientY: y, bubbles: true, cancelable: true }));
          requestAnimationFrame(step);
        };
        requestAnimationFrame(step);
      }),
    [box.x + box.width / 2, box.y + box.height / 2],
  );
  await g.focus("#graph-stage canvas");
  await g.keyboard.press("+");
  await g.keyboard.press("0");
  await g.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
  const t: any = await g.evaluate(() => (window as any).ursaShell.graphTimings());
  const spread = (q: any) => q && q.count > 0 && 0 <= q.p50 && q.p50 <= q.p95 && q.p95 <= q.max && q.max < 10_000;
  check(
    t.enter === 0 && t.engine >= 0 && t.firstDraw >= t.engine && t.done >= t.firstPositions,
    "graphTimings still gives when each step of .graph ended",
    JSON.stringify(Object.fromEntries(Object.entries(t).filter(([, v]) => typeof v === "number"))),
  );
  info = await graphInfo(g);
  check(
    t.frames.count >= info.frames && info.frames > 30 && spread(t.frames.render) && t.frames.render.count === Math.min(120, t.frames.count) && spread(t.frames.interval) && t.frames.interval.count <= 120,
    "graphTimings.frames: the time in render() and between frames while the view moved",
    JSON.stringify(t.frames),
  );
  check(t.pick.count >= 1 && t.pick.last >= 0 && t.pick.last < 100 && t.pick.p95 >= 0 && t.pick.p95 < 100, "graphTimings.pick: the last pick and the p95", JSON.stringify(t.pick));
  const acted = (command: string) => t.actions.findLast((a: any) => a.command === command);
  const drew = t.actions.findLast((a: any) => a.command.startsWith(".graph"));
  check(
    drew && drew.at === 0 && drew.engine >= 0 && Math.abs(drew.engine - t.engine) <= 1 && drew.delta >= 0 && drew.frame >= drew.engine && drew.frame >= t.firstDraw - 1 && drew.frame < 10_000,
    "graphTimings.actions: .graph's time in the engine, making the view, and from Enter to its frame",
    JSON.stringify(drew),
  );
  const inputs = ["click", "fit", "key +", "key 0"].map(acted);
  check(
    inputs.every((a: any) => a && a.at > drew.frame && a.frame >= 0 && a.frame < 5_000 && a.engine === null && a.delta === null),
    "graphTimings.actions: a click, a fit and each key, from input to the frame that showed it",
    JSON.stringify(inputs),
  );
  check(
    Array.isArray(t.longTasks) && t.longTasks.length <= 64 && t.longTasks.every((l: any) => Number.isFinite(l.start) && l.duration >= 50),
    "graphTimings.longTasks: the page's tasks over 50 ms",
    JSON.stringify(t.longTasks),
  );
  const mem: any = await g.evaluate(() => (window as any).ursaShell.graphMemory({ timeoutMs: 2_000 }));
  const pages = (bytes: any) => Number.isInteger(bytes) && bytes > 0 && bytes % 65536 === 0;
  if (overSocket) skip("graphMemory: each wasm instance's memory, and the browser's measure of the page");
  else check(
    mem.crossOriginIsolated === true && pages(mem.wasm.engine) && pages(mem.wasm.view) && pages(mem.wasm.layout) && (mem.userAgent ? mem.userAgent.bytes > mem.wasm.view : typeof mem.userAgentError === "string"),
    "graphMemory: each wasm instance's memory, and the browser's measure of the page",
    JSON.stringify({ ...mem, userAgent: mem.userAgent && { bytes: mem.userAgent.bytes, parts: mem.userAgent.breakdown.length } }),
  );

  {
    const held: any = await g.evaluate(() => (window as any).ursaShell.graphTimings().memory);
    const part = (p: any) => pages(p?.bytes) && p.perEdge === Math.round(p.bytes / held.edges);
    check(
      held && held.edges > 0 && part(held.view) && part(held.layout) && held.view.bytes === mem.wasm.view && held.layout.bytes === mem.wasm.layout,
      "graphTimings.memory: what the view and the layout worker hold, in bytes and bytes an edge",
      JSON.stringify(held),
    );
  }

  // Esc, in the terminal, clears a selection first, then closes the pane: the
  // terminal takes the whole width again.
  {
    const at33 = (await g.evaluate(() => (window as any).ursaShell.graphNodePlace(33)))!;
    await g.mouse.click(box.x + at33.x, box.y + at33.y);
    const selected = (await graphInfo(g)).picked?.id;
    await g.click("#terminal");
    await g.keyboard.press("Escape");
    check(selected == 33 && (await graphOpen(g)) && (await graphInfo(g)).selection.length === 0, "Esc in the terminal clears the selection first", `selected ${selected}`);
  }
  await g.keyboard.press("Escape");
  check(!(await graphOpen(g)) && (await settledCols(g)) === fullCols && (await focused(g)) === "terminal", "Esc closes the graph view and the terminal refits", `${await settledCols(g)} of ${fullCols} columns`);

  // .graph NAME: a property graph.
  await type(".graph karate", g);
  check((await after(".graph karate", g)).startsWith("Drawing 78 edges, 34 nodes as the graph karate (Esc closes)"), ".graph karate draws the property graph", (await after(".graph karate", g)).split("\n")[0]);
  // #444: the inspector. Selecting something shows everything known of it.
  {
    await graphSettled(g);
    const inspector = (): Promise<any> => g.evaluate(() => (window as any).ursaShell.graphInspector());
    const rowsRead = () => g.waitForFunction(() => (window as any).ursaShell.graphInspector()?.rows, null, { timeout: 10_000 }).catch(() => {});
    const row = (shown: any, i = 0): Record<string, string | null> => Object.fromEntries(shown?.rows?.[i] ?? []);
    const press = (text: string) => g.locator("#graph-stage .graph-inspector button", { hasText: text }).first().click();
    const onCanvas = (x: number, y: number) => g.evaluate(([x, y]) => document.elementFromPoint(x, y)?.tagName === "CANVAS", [box.x + x, box.y + y]);
    check((await inspector()) === null && (await g.locator("#graph-stage .graph-inspector").isHidden()), "the inspector is closed while nothing is selected");

    // A node: its key, its degree drawn and in the graph, and its row.
    const at33 = (await place(33))!;
    await g.mouse.click(box.x + at33.x, box.y + at33.y);
    await rowsRead();
    await g.waitForFunction(() => (window as any).ursaShell.graphInspector()?.neighbours !== undefined, null, { timeout: 5_000 }).catch(() => {});
    let shown = await inspector();
    check(shown?.kind === "node" && shown.title === "node 33" && shown.table === "members" && row(shown).id === "33" && row(shown).club === "Officer", "clicking member 33 shows its row: club Officer", JSON.stringify(shown?.rows));
    check(shown?.neighbours === 17 && /degree\s*17 drawn · 17 neighbours in karate/.test(shown.text) && shown.sql === "SELECT * FROM members WHERE id = 33", "the inspector says the node's degree as drawn, its neighbours in the graph, and the query", shown?.text);
    check(["expand", "hide", "pin", "path from here", "colour by", "size by"].every((b) => shown?.buttons.includes(b)), "a node has expand, hide, pin, path from here, and colour by and size by on its properties", JSON.stringify(shown?.buttons));
    console.log(`     the row came ${Math.round(shown?.rowMs)} ms after the selection (${Math.round(shown?.engineMs)} ms in the engine)`);
    if (shots) await g.screenshot({ path: `${shots}/graph-inspector-node.png` });

    // Keys: i moves the focus into the panel, Esc back, and the selection stays.
    await g.focus("#graph-stage canvas");
    await g.keyboard.press("i");
    const inPanel = (await inspector())?.focused;
    await g.keyboard.press("Escape");
    check(inPanel === true && (await g.evaluate(() => document.activeElement?.tagName)) === "CANVAS" && (await graphInfo(g)).selection.length === 1 && (await inspector())?.kind === "node", "i focuses the inspector, and Esc returns to the graph with the selection kept");

    // Pin marks the node; the button says so.
    await press("pin");
    check((await graphInfo(g)).pinned.join() === "33" && (await inspector()).buttons.includes("unpin"), "pin pins the node", JSON.stringify((await graphInfo(g)).pinned));
    await press("unpin");
    check((await graphInfo(g)).pinned.length === 0, "unpin takes the pin off");

    // An edge: a click on it, with no node there, shows its ends and its row.
    const [a0, a1] = [(await place(0))!, (await place(1))!];
    const mid = { x: (a0.x + a1.x) / 2, y: (a0.y + a1.y) / 2 };
    const under = await g.evaluate(([x, y]) => (window as any).ursaShell.graphEdgeAt(x, y), [mid.x, mid.y]);
    // With nothing selected: the panel is over part of the view while it is open.
    await g.focus("#graph-stage canvas");
    await g.keyboard.press("Escape");
    await g.mouse.click(box.x + mid.x, box.y + mid.y);
    await rowsRead();
    shown = await inspector();
    info = await graphInfo(g);
    check(under?.src == 0 && under?.dst == 1 && info.edge?.src.id == 0 && info.edge?.dst.id == 1 && info.selection.length === 0 && /^Selected edge 0 → 1 · 34 nodes · 78 edges$/.test(await statusText()), "a click on an edge selects it, and the status names it", `${JSON.stringify(under)} ${await statusText()}`);
    check(shown?.kind === "edge" && shown.title === "edge 0 → 1" && shown.table === "ties" && shown.rows.length === 1 && row(shown).src === "0" && row(shown).dst === "1" && /from\s*0/.test(shown.text) && /to\s*1/.test(shown.text), "clicking a tie shows its ends and its row", JSON.stringify(shown?.rows));
    // 3 px off the edge is still the edge; 12 px off is empty space.
    const [ex, ey] = [a1.x - a0.x, a1.y - a0.y];
    const len = Math.hypot(ex, ey);
    const off = (d: number) => g.evaluate(([x, y]) => (window as any).ursaShell.graphEdgeAt(x, y), [mid.x - (ey / len) * d, mid.y + (ex / len) * d]);
    const [near, far] = [await off(3), await off(12)];
    check(near?.index === under?.index && far?.index !== under?.index, "an edge is picked within 4 px of it, and no further", `${JSON.stringify(near)} ${JSON.stringify(far)}`);
    // An end's button selects that node.
    await press("select");
    await rowsRead();
    shown = await inspector();
    check(shown?.kind === "node" && shown.title === "node 0" && (await graphInfo(g)).edge === null && row(shown).club === "Mr. Hi", "an edge's end is selected from the inspector", shown?.title);

    // Several nodes: the counts, and a WHERE clause to copy. Node 33 and the
    // node furthest left, which the panel (on the right) is never over.
    const now33 = (await place(33))!;
    const all = await Promise.all(Array.from({ length: 34 }, (_, id) => place(id).then((at) => ({ id, ...at! }))));
    const left = all.filter((n) => n.id !== 33).reduce((a, b) => (b.x < a.x ? b : a));
    await g.focus("#graph-stage canvas");
    await g.keyboard.press("Escape");
    await g.mouse.click(box.x + now33.x, box.y + now33.y);
    const clear = await onCanvas(left.x, left.y);
    await g.keyboard.down("Shift");
    await g.mouse.click(box.x + left.x, box.y + left.y);
    await g.keyboard.up("Shift");
    shown = await inspector();
    const where = `WHERE id IN (33, ${left.id})`;
    check(clear && shown?.kind === "nodes" && shown.title === "2 nodes" && shown.count === 2 && (shown.edges === 0 || shown.edges === 1) && shown.where === where && shown.text.includes(where) && shown.buttons.includes("copy") && shown.buttons.includes("path between"), "several nodes show their count, the edges among them and a WHERE clause to copy", JSON.stringify(shown));
    await gctx.grantPermissions(["clipboard-read", "clipboard-write"]);
    await press("copy");
    const copied = await g.evaluate(() => navigator.clipboard.readText()).catch((e) => String(e));
    check(copied === where, "copy puts the WHERE clause on the clipboard", copied);
    if (shots) await g.screenshot({ path: `${shots}/graph-inspector-nodes.png` });

    // The panel never covers what was just selected: a node on the right
    // of the view puts it on the left.
    {
      const ids = Array.from({ length: 34 }, (_, i) => i);
      const places = await Promise.all(ids.map((id) => place(id)));
      const rightmost = places.reduce((a, b) => (b!.x > a!.x ? b : a))!;
      await g.focus("#graph-stage canvas");
      await g.keyboard.press("Escape");
      await g.mouse.click(box.x + rightmost.x, box.y + rightmost.y);
      const side = (await inspector())?.side;
      const covered = !(await onCanvas(rightmost.x, rightmost.y));
      check(side === "left" && !covered, "the inspector moves to the left when the selected node is on the right", `${side}, node at ${Math.round(rightmost.x)} of ${Math.round(box.width)}`);
    }

    // Esc clears the selection, and the inspector goes with it.
    await g.focus("#graph-stage canvas");
    await g.keyboard.press("Escape");
    check((await graphOpen(g)) && (await inspector()) === null && (await g.locator("#graph-stage .graph-inspector").isHidden()), "Esc clears the selection and closes the inspector, not the pane");

    // A button runs its command in the terminal: colour by redraws the graph.
    await g.mouse.click(box.x + now33.x, box.y + now33.y);
    await rowsRead();
    await g.locator("#graph-stage .graph-inspector dd", { hasText: "Officer" }).locator("button", { hasText: "colour by" }).click();
    await idle(g);
    await graphSettled(g);
    const legendText = (await g.locator("#graph-legend").textContent()) ?? "";
    check((await screen(g)).includes("memory U .graph karate COLOR BY club") && /colour: club/.test(legendText) && legendText.includes("Officer"), "colour by on a property runs .graph with the clause", legendText);
    // The legend moved the canvas down.
    const below = (await g.locator("#graph-stage canvas").boundingBox())!;
    const re33 = (await place(33))!;
    await g.mouse.click(below.x + re33.x, below.y + re33.y);
    await rowsRead();
    await press("expand");
    await idle(g);
    check((await screen(g)).includes("memory U .expand 33"), "expand runs .expand in the terminal", (await after(".expand 33", g)).split("\n")[0]);

    // A value with markup is text: no element comes of it.
    await g.click("#terminal");
    await type("CREATE TABLE notes AS SELECT * FROM (VALUES (1, '<img src=x onerror=alert(1)><b>bold</b>'), (2, 'plain')) AS t(id, body);", g);
    await type("CREATE TABLE refs AS SELECT 1 AS src, 2 AS dst;", g);
    await type("CREATE PROPERTY GRAPH markup VERTEX TABLES (notes KEY (id)) EDGE TABLES (refs SOURCE KEY (src) REFERENCES notes (id) DESTINATION KEY (dst) REFERENCES notes (id));", g);
    await type(".graph markup", g);
    await graphSettled(g);
    const plain = (await g.locator("#graph-stage canvas").boundingBox())!;
    const m1 = (await place(1))!;
    await g.mouse.click(plain.x + m1.x, plain.y + m1.y);
    await rowsRead();
    shown = await inspector();
    const made = await g.evaluate(() => document.querySelectorAll("#graph-stage .graph-inspector img, #graph-stage .graph-inspector b").length);
    const wraps = await g.evaluate(() => {
      const value = [...document.querySelectorAll<HTMLElement>("#graph-stage .graph-inspector .value")].find((v) => v.textContent!.includes("<b>"));
      const panel = document.querySelector<HTMLElement>("#graph-stage .graph-inspector")!;
      return !!value && getComputedStyle(value).userSelect === "text" && panel.scrollWidth <= panel.clientWidth;
    });
    check(row(shown).body === "<img src=x onerror=alert(1)><b>bold</b>" && made === 0 && wraps, "a value with markup is shown as text, wrapped and selectable", JSON.stringify(shown?.rows));
    // Back to karate, with nothing selected, for what follows.
    await g.click("#terminal");
    await type(".graph karate", g);
    await graphSettled(g);
  }
  // .graph QUERY replaces the drawing.
  await type(".graph SELECT src, dst FROM ties WHERE src = 1;", g);
  // Its line wraps in the narrower terminal.
  const replaced = (await after(".graph SELECT src, dst FROM ties WHERE src = 1", g)).split("\n").slice(0, 2).join("").replace(/\s+/g, " ").split(" memory U")[0].trim();
  await graphSettled(g);
  info = await graphInfo(g);
  check(/^Drawing 8 edges, 9 nodes as edges \(src, dst\) \(replacing the last drawing; E ?s ?c ?c ?l ?o ?s ?e ?s ?\)$/.test(replaced) && info.edges === 8 && info.nodes === 9, ".graph QUERY replaces the drawing", replaced);
  // Esc with the focus in the pane closes it, and the focus goes back to the terminal.
  await g.focus("#graph-stage canvas");
  await g.keyboard.press("Escape");
  check(!(await graphOpen(g)) && (await focused(g)) === "terminal", "Esc in the pane, with nothing selected, closes it, focus back in the terminal", await focused(g));

  // The Graph button draws the last result; a click hands the keys back to the terminal.
  await type("SELECT * FROM ties;", g);
  await g.click("#graph");
  await idle(g);
  check((await after(".graph", g)).startsWith("Drawing 78 edges, 34 nodes") && (await graphOpen(g)) && (await focused(g)) === "terminal", "the Graph button draws the last result", (await after(".graph", g)).split("\n")[0]);
  const buttonLabel = await g.evaluate(() => document.getElementById("graph")!.textContent!.trim());
  check(buttonLabel === "Graph", "the Graph button is named", buttonLabel);
  // The close button closes it.
  await g.click("#graph-close");
  check(!(await graphOpen(g)) && (await focused(g)) === "terminal", "the close button closes the pane", await focused(g));
  check((await errors(g)) === 0, "no errors from the graph view (since the dataset cleared the screen)", `${await errors(g)}`);
  // New, or a dataset, closes the pane.
  await type(".graph karate", g);
  await g.click("#new");
  await idle(g);
  check(!(await graphOpen(g)), "New closes the graph view");
  await type(".dataset karate", g);
  await type(".graph karate", g);
  check(await graphOpen(g), ".graph karate after a fresh session");
  await type(".dataset florentine", g);
  check(!(await graphOpen(g)), "a dataset closes the graph view");
  await gctx.close();
}
// #427 and #433: the ends of an edge are ids, and .graph draws what a result
// means. A score or a path is not a node: where it drew nonsense (each id tied
// to its PageRank; one edge from a path's length to its list of vertices) it
// now draws the nodes sized by the score, and the path; and what is no graph
// says so, and what to type instead.
{
  const actx = await watchedContext({ viewport: { width: 1280, height: 800 } });
  const s = await actx.newPage();
  await s.goto(`${base}/?dataset=karate`);
  await ready(s);
  await idle(s);
  await s.click("#terminal");
  // Compared without spaces: the terminal wraps long lines.
  const squeezed = (t: string) => t.replace(/\s+/g, "");
  const shortest = "FROM GRAPH_TABLE (karate MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26) COLUMNS (path_length(p), vertices(p), edges(p)));";
  // What is not a graph says what it is and what would draw it (rule 6).
  for (const [query, says] of [
    ["SELECT pagerank FROM pagerank(karate) LIMIT 5;", ["cannot draw this result as a graph: it has 1 column (pagerank: Float64), which is not edges", "None of its columns can be a node's id", ".graph SELECT … AS src, … AS dst FROM …"]],
    ["SELECT club, count(*) AS n FROM members GROUP BY club;", ["cannot draw this result as a graph: it has 2 columns (club: Utf8", "n: Int64), which are not edges", "if club and n are the two ends of an edge: .graph AS edges(club, n)", ".graph AS nodes(club)"]],
    ["SELECT src, dst / 2.0 AS dst FROM ties;", ["cannot draw columns src and dst as edges: 'dst' is a number that is not whole (Float64)"]],
  ] as [string, string[]][]) {
    await type(query, s);
    const before = await errors(s);
    await type(".graph", s);
    const out = await after(".graph", s);
    check(
      out.startsWith("Error: cannot draw ") && says.every((part) => squeezed(out).includes(squeezed(part))) && (await errors(s)) === before + 1 && !(await graphOpen(s)),
      `.graph after ${query.slice(0, 44)}…: not a graph, and what would be`,
      out.split("\n").slice(0, 4).join(" "),
    );
  }

  // The result → graph rules (#433): .graph draws what the last result means.
  // Each: the query, what the terminal and the pane say it was drawn as, its
  // nodes and edges, and what the legend shows.
  const hop = "SELECT * FROM hop(karate, seeds => [0], steps => 2, direction => 'both');";
  const three = "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0)-[]-(b WHERE b.id = 1)-[]-(c) COLUMNS (a.id AS a, b.id AS b, c.id AS c));";
  for (const [what, query, as, nodes, edges, legend] of [
    // A path: the route, six nodes in order, not one edge from 5 to a list.
    ["a path", shortest.replace("path_length(p), vertices(p), edges(p)", "path_length(p) AS steps, vertices(p) AS members, edges(p) AS ties"), "a path on karate (members, ties)", 6, 5, ""],
    // A hop: the seed, distance 1 and 2, with the real edges; not a star.
    ["a hop", hop, "rings on karate (seed, id, distance)", 26, 59, "colour: distance0 (1)1 (16)2 (9)"],
    // A node table: five nodes sized by PageRank with the edges among them, not ids tied to scores.
    ["a node table", "SELECT * FROM pagerank(karate) LIMIT 5;", "nodes of karate (id, pagerank)", 5, 7, "size: pagerank"],
    // Every node, with a metric and a category: the graph, sized and coloured.
    ["algorithms()", "SELECT * FROM algorithms(karate, pagerank(direction => 'both'), community => louvain(seed => 1));", "nodes of karate (id, pagerank, community)", 34, 78, "colour: community"],
    // Three variables: both edges of each match (0–1, and 1's nine ties), not only a–b.
    ["a three-variable match", three, "pattern matches on karate (a, b, c)", 10, 9, ""],
    // shortest_path(): the same edges, as a route.
    ["shortest_path()", "SELECT * FROM shortest_path(karate, source => 16, target => 26, direction => 'both');", "a path on karate (src, dst, step)", 6, 5, ""],
  ] as [string, string, string, number, number, string][]) {
    await type(query, s);
    const before = await errors(s);
    await type(".graph", s);
    const out = squeezed(await after(".graph", s));
    await graphSettled(s);
    const drawn = await graphInfo(s);
    const status = (await s.locator("#graph-status").textContent()) ?? "";
    const shown = ((await s.locator("#graph-legend").textContent()) ?? "").trim();
    const count = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;
    check(
      out.startsWith(squeezed(`Drawing ${count(edges, "edge")}, ${count(nodes, "node")} as ${as} (`)) &&
        drawn.nodes === nodes &&
        drawn.edges === edges &&
        drawn.drawnAs === as &&
        status === `${count(nodes, "node")} · ${count(edges, "edge")} · drawn as ${as} · Done` &&
        (legend === "" ? shown === "" : shown.includes(legend)) &&
        (await errors(s)) === before,
      `.graph after ${what}: drawn as ${as}`,
      `${out.slice(0, 90)} | ${status} | ${shown} | ${JSON.stringify(drawn)}`,
    );
    await s.keyboard.press("Escape");
  }
  // A node table that is no graph's: the nodes, without edges, and it says so.
  await type(".graph SELECT 'x' AS id, 0.5 AS pagerank UNION ALL SELECT 'y', 0.25", s);
  await graphSettled(s);
  {
    const drawn = await graphInfo(s);
    const status = (await s.locator("#graph-status").textContent()) ?? "";
    check(drawn.nodes === 2 && drawn.edges === 0 && status.includes("drawn as nodes (id, pagerank) · the edges of 'karate' could not be read") && status.includes("the nodes are drawn without edges"), "a node table of no graph draws its nodes, and says there are no edges", `${status} | ${JSON.stringify(drawn)}`);
  }
  await s.keyboard.press("Escape");
  check(!(await graphOpen(s)), "Esc closes the drawing of a node table");
  // AS, ON and LAYOUT (#434): the user says what a result is. Each form on
  // columns of other names, which the rules read otherwise or not at all.
  {
    // Computed columns carry no metadata: by the rules, s and i are edges.
    const hops = "SELECT seed + 0 AS s, id + 0 AS i, CAST(distance AS INT) AS far FROM hop(karate, seeds => [0], steps => 2, direction => 'both')";
    const pairs = "SELECT 'tie' AS tag, a.id AS x, b.id AS y FROM members a JOIN ties t ON t.src = a.id JOIN members b ON b.id = t.dst";
    for (const [what, command, as, nodes, edges, legend, fixed] of [
      // One of each form.
      ["AS edges", `${pairs} AS edges(x, y)`, "edges (x, y)", 34, 78, "", false],
      ["AS nodes", "SELECT id + 100 AS id, id AS who FROM members WHERE id < 5 AS nodes(who)", "nodes of karate (who)", 5, 7, "", false],
      ["AS path", "SELECT 'route' AS id, [16, 5, 0, 8, 33, 26] AS stops AS path(stops)", "a path (stops)", 6, 5, "", true],
      ["AS hops", `${hops} AS hops(s, i, far)`, "rings on karate (s, i, far)", 26, 59, "colour: distance0 (1)1 (16)2 (9)", true],
      // ON names the graph, so an algorithm sizes rows drawn as edges.
      ["ON graph", `${pairs} AS edges(x, y) ON karate SIZE BY pagerank`, "edges of karate (x, y)", 34, 78, "size: pagerank", false],
      // LAYOUT overrides the rule: a hop by its edges' pull, in any order.
      ["LAYOUT force", `${hops} LAYOUT force AS hops(s, i, far)`, "rings on karate (s, i, far)", 26, 59, "colour: distance", false],
      ["LAYOUT layers", "SELECT * FROM hop(karate, seeds => [0], steps => 2, direction => 'both') LAYOUT layers", "rings on karate (seed, id, distance)", 26, 59, "colour: distance", true],
    ] as [string, string, string, number, number, string, boolean][]) {
      const before = await errors(s);
      await type(`.graph ${command}`, s);
      const out = squeezed(await after(".graph", s));
      await graphSettled(s);
      const drawn = await graphInfo(s);
      const status = (await s.locator("#graph-status").textContent()) ?? "";
      const shown = ((await s.locator("#graph-legend").textContent()) ?? "").trim();
      check(
        out.includes(squeezed(`Drawing ${edges} edges, ${nodes} nodes as ${as} (`)) &&
          drawn.nodes === nodes &&
          drawn.edges === edges &&
          drawn.drawnAs === as &&
          status === `${nodes} nodes · ${edges} edges · drawn as ${as} · Done` &&
          (legend === "" ? shown === "" : shown.includes(legend)) &&
          // Rings and layers are placed at once; a force layout iterates.
          (fixed ? drawn.iterations === 0 : drawn.iterations > 0) &&
          (await errors(s)) === before,
        `.graph … ${what}: drawn as ${as}`,
        `${out.slice(-120)} | ${status} | ${shown} | ${JSON.stringify(drawn)}`,
      );
      await s.keyboard.press("Escape");
    }
    // On the last result, as the refusal of rule 6 says to.
    await type("SELECT club, id AS member, id + 1 AS next FROM members WHERE id < 4;", s);
    await type(".graph AS edges(member, next)", s);
    await graphSettled(s);
    {
      const drawn = await graphInfo(s);
      check(drawn.nodes === 5 && drawn.edges === 4 && drawn.drawnAs === "edges (member, next)", ".graph AS edges(…) draws the last result", JSON.stringify(drawn));
    }
    await s.keyboard.press("Escape");
    // Every misuse says what to write, and draws nothing.
    for (const [command, says] of [
      [`${pairs} AS edges(x, z)`, ["AS edges(x, z): the result has no column z. The result's columns are tag (Utf8), x (Int64), y (Int64); name one of them"]],
      [`${pairs} AS edges(tag)`, ["AS edges(…) takes the columns of an edge's two ends, and then its weight's, and was given 1: AS edges(src, dst [, weight])"]],
      ["SELECT * FROM pagerank(karate) AS edges(id, pagerank)", ["AS edges(id, pagerank): column pagerank is a number that is not whole (Float64), and an edge's end is an id: an integer, a string or a date. The result's columns are id (Int64), pagerank (Float64)"]],
      ["SELECT * FROM pagerank(karate) AS path(id)", ["AS path(id): column id is an integer (Int64), and a path's nodes are a list of ids"]],
      [`${hops} AS hops(s, i)`, ["AS hops(…) takes three columns: the seed, the node reached and its distance, and was given 2: AS hops(seed, id, distance)"]],
      [`${hops} AS hop(s, i, far)`, ["AS hop is not a form; write AS hops(seed, id, distance)"]],
      ["karate AS tree(id)", ["AS tree is not a form; write one of AS edges(src, dst [, weight]), AS nodes(id), AS path(nodes [, edges]) or AS hops(seed, id, distance)"]],
      [`${pairs} AS edges(x, y) ON nope`, ["ON nope: there is no property graph named 'nope'. The graphs are karate; name one of them"]],
      [`${pairs} AS edges(x, y) LAYOUT rings`, ["LAYOUT rings places each node by its distance from a seed or its step along a path, and edges have neither", "AS hops(seed, id, distance)", "LAYOUT force"]],
      ["karate LAYOUT spiral", ["LAYOUT needs force, rings or layers, and 'spiral' is none of them"]],
      ["karate AS nodes(id)", ["AS nodes(id) says how to read a result's rows, and 'karate' is a property graph, drawn whole. Leave the clause out (.graph karate)"]],
    ] as [string, string[]][]) {
      const before = await errors(s);
      await type(`.graph ${command}`, s);
      const out = await after(".graph", s);
      check(
        says.every((part) => squeezed(out).includes(squeezed(part))) && (await errors(s)) === before + 1 && !(await graphOpen(s)),
        `.graph ${command.slice(-28)}: says what to write`,
        out.split("\n").slice(-6).join(" "),
      );
    }
    // .help graph lists the clauses.
    await type(".help graph", s);
    const help = await after(".help graph", s);
    check(
      ["AS edges(src, dst [, weight])", "AS nodes(id)", "AS path(nodes [, edges])", "AS hops(seed, id, distance)", "ON graph", "LAYOUT force|rings|layers"].every((part) => help.includes(part)),
      ".help graph lists .graph's clauses",
      help.slice(-600),
    );
  }
  // The explore session (#440): commands typed in the terminal change the
  // engine's graph, which answers with what changed and a delta. (Drawing the
  // delta in the pane is #443, so the counts here are the engine's.)
  {
    const explored = (): Promise<any> => s.evaluate(() => (window as any).ursaShell.explored());
    const said = async (line: string) => {
      await type(line, s);
      return squeezed(await after(line, s));
    };
    await type(hop.replace("steps => 2", "steps => 1"), s);
    await type(".graph", s);
    await graphSettled(s);
    const base = await graphInfo(s);
    const before = await errors(s);
    const expanded = await said(".expand 2 LIMIT 3");
    const first = await explored();
    check(
      base.nodes === 17 && base.edges === 34 && expanded.startsWith(squeezed("+3 nodes, +6 edges (1 more neighbour of 2 is not shown")) && first.nodes === 20 && first.edges === 40 && first.deltaBytes > 0,
      ".expand typed in the terminal adds the node's neighbours: the node count changes",
      `${expanded.slice(0, 80)} | ${JSON.stringify(base)} | ${JSON.stringify(first)}`,
    );
    const hidden = await said(".hide 32 0");
    const second = await explored();
    check(hidden.startsWith(squeezed("-2 nodes, -")) && second.nodes === 18 && second.deltaBytes > 0, ".hide hides nodes and their edges", `${hidden.slice(0, 60)} | ${JSON.stringify(second)}`);
    const history = await said(".graph history");
    check(history.includes(squeezed("1 .expand 2 LIMIT 3 +3 nodes, +6 edges")) && history.includes(squeezed("Shown: 18 nodes")) && (await graphOpen(s)), ".graph history lists the steps, and draws nothing", history.slice(0, 200));
    await type(".undo\n.undo", s);
    const undone = await explored();
    const again = await said(".undo");
    check(undone.nodes === 17 && undone.edges === 34 && undone.deltaBytes > 0 && again.startsWith(squeezed("Nothing to undo")) && (await explored()).deltaBytes === 0, ".undo takes the steps back, to the graph as it was drawn", `${JSON.stringify(undone)} | ${again.slice(0, 40)}`);
    const redone = await said(".redo");
    check(redone.startsWith(squeezed("Redid .expand 2 LIMIT 3: +3 nodes, +6 edges")) && (await explored()).nodes === 20, ".redo applies the step again", redone.slice(0, 60));
    const sql = await said(".expand 32 --sql");
    check(sql.startsWith(squeezed('SELECT h.id, count(*) OVER () AS neighbours FROM hop("karate", seeds => [32]')) && sql.includes(squeezed('FROM induced_edges("karate", nodes => [')) && (await explored()).nodes === 20, ".expand --sql prints the SQL and changes nothing", sql.slice(0, 120));
    check((await errors(s)) === before, "the explore commands print no error");
    const missing = await said(".expand 99");
    check(missing.includes(squeezed("Error: no node '99' in the graph that is drawn")), "an id that is not in the graph says so", missing.slice(0, 80));
    await type(".help", s);
    const help = squeezed(await after(".help", s));
    check([".expand ID", ".hide ID", ".show ID", ".undo", ".redo", ".graph history"].every((c) => help.includes(squeezed(c))), ".help lists the explore commands", help.slice(0, 200));
    await s.keyboard.press("Escape");
    const closed = await said(".expand 2");
    check(closed.includes(squeezed("Error: no graph is open")), "with the pane closed, .expand says no graph is open", closed.slice(0, 80));
  }
  // Search and focus (#445): `/` in the pane and .find in the terminal select
  // the nodes that match and fit them; f fits the selection; the matches the
  // view does not show are listed.
  {
    const found = (): Promise<any> => s.evaluate(() => (window as any).ursaShell.found());
    const said = async (line: string) => {
      await type(line, s);
      return squeezed(await after(line, s));
    };
    const status = async () => (await s.locator("#graph-status").textContent()) ?? "";
    // The selection's box, in CSS pixels, once the camera rests.
    const fitted = async () => {
      await s.waitForFunction(() => !(window as any).ursaShell.graph()?.fitting, null, { timeout: 10_000 });
      await s.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(() => r(null)))));
      return s.evaluate(() => {
        const shell = (window as any).ursaShell;
        const canvas = document.querySelector("#graph-stage canvas")!;
        const at = shell.graph().selection.map((n: any) => shell.graphNodePlace(n.id));
        const xs = at.map((p: any) => p.x);
        const ys = at.map((p: any) => p.y);
        const [x0, x1, y0, y1] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
        // In the part of the canvas the inspector's panel leaves clear (#493).
        const ratio = shell.graph().width / canvas.clientWidth;
        const [left, top, right, bottom] = shell.graph().room.inset.map((v: number) => v / ratio);
        return { cx: (x0 + x1) / 2 - left, cy: (y0 + y1) / 2 - top, w: x1 - x0, h: y1 - y0, width: canvas.clientWidth - left - right, height: canvas.clientHeight - top - bottom, ids: shell.graph().selection.map((n: any) => String(n.id)) };
      });
    };
    const centred = (b: any) => Math.abs(b.cx - b.width / 2) <= 2 && Math.abs(b.cy - b.height / 2) <= 2;
    const before = await errors(s);
    await type(".graph karate", s);
    await graphSettled(s);

    // `/` opens the box in the pane's header; Enter runs .find in the shell.
    await s.focus("#graph-stage canvas");
    await s.keyboard.press("/");
    const box = await s.evaluate(() => ({ open: !document.getElementById("graph-search")!.hidden, focused: document.activeElement?.id, typed: (document.activeElement as HTMLInputElement).value }));
    check(box.open && box.focused === "graph-search-input" && box.typed === "", "/ in the pane opens the search box, focused", JSON.stringify(box));
    await s.keyboard.type("33");
    await s.keyboard.press("Enter");
    await idle(s);
    let b = await fitted();
    let f = await found();
    const typed = squeezed(await after(".find 33", s));
    check(
      f.command === ".find 33" && f.matched === 1 && typed.startsWith(squeezed("Selected 1 node")) && /^Selected node 33: /.test(await status()) && b.ids.join() === "33" && centred(b),
      "/33 selects node 33 and centres it, as .find 33 typed in the terminal",
      `${JSON.stringify(f)} | ${JSON.stringify(b)} | ${await status()}`,
    );
    const closed = await s.evaluate(() => ({ hidden: document.getElementById("graph-search")!.hidden, focus: document.activeElement?.tagName }));
    check(closed.hidden && closed.focus === "CANVAS", "Enter closes the box, and the focus is the graph's again", JSON.stringify(closed));

    // Esc in the box closes the box, not the pane, and keeps the selection.
    await s.keyboard.press("/");
    await s.keyboard.type("zz");
    await s.keyboard.press("Escape");
    check((await graphOpen(s)) && (await s.evaluate(() => document.getElementById("graph-search")!.hidden)) && /^Selected node 33: /.test(await status()), "Esc in the search box closes the box alone", await status());

    // A condition on the vertex table's columns, from the terminal.
    await s.click("#terminal");
    const officers = "9,14,15,18,20,22,23,24,25,26,27,28,29,30,31,32,33";
    const where = await said(".find WHERE club = 'Officer'");
    b = await fitted();
    f = await found();
    check(
      where.startsWith(squeezed("Selected 17 nodes")) && f.matched === 17 && f.notShownTotal === 0 && b.ids.join() === officers && /^Selected 17 nodes: 9, 14, 15, 18, 20 and 12 more · 34 nodes/.test(await status()),
      ".find WHERE club = 'Officer' selects the officers' members",
      `${where.slice(0, 60)} | ${b.ids.join()} | ${await status()}`,
    );
    check(centred(b) && Math.max(b.w / b.width, b.h / b.height) > 0.7 && b.w <= b.width && b.h <= b.height, "the matches are fitted in view", JSON.stringify(b));

    // f fits the selection again after the view has moved; the move takes 200 ms.
    await s.focus("#graph-stage canvas");
    for (let i = 0; i < 3; i++) await s.keyboard.press("ArrowLeft");
    await s.keyboard.press("-");
    const away = await fitted();
    await s.keyboard.press("f");
    const moving = (await graphInfo(s)).fitting;
    b = await fitted();
    check(!centred(away) && moving && centred(b) && Math.max(b.w / b.width, b.h / b.height) > 0.7, "f fits the selection, with a camera move", `${JSON.stringify(away)} -> ${JSON.stringify(b)}`);
    // Under prefers-reduced-motion the camera is there at once.
    await s.emulateMedia({ reducedMotion: "reduce" });
    await s.keyboard.press("0");
    await s.keyboard.press("f");
    const still = (await graphInfo(s)).fitting;
    b = await fitted();
    check(!still && centred(b), "with reduced motion, f moves the camera at once", JSON.stringify(b));
    await s.emulateMedia({ reducedMotion: "no-preference" });

    // A search with no match says so and keeps the selection.
    await s.click("#terminal");
    const none = await said(".find nobody");
    check(none.startsWith(squeezed("No node matches 'nobody'")) && (await graphInfo(s)).selection.length === 17, "a search that matches nothing keeps the selection", none.slice(0, 60));

    // Part of the graph: the matches that are not drawn are named under the
    // header, and a hidden one has a button that shows it again.
    await type(hop.replace("steps => 2", "steps => 1"), s);
    await type(".graph", s);
    await graphSettled(s);
    await type(".hide 31", s);
    const part = await said(".find 3");
    f = await found();
    const listed = await s.evaluate(() => {
      const el = document.getElementById("graph-found")!;
      return { hidden: el.hidden, text: el.textContent, buttons: Array.from(el.querySelectorAll("button.add"), (x) => x.getAttribute("aria-label")) };
    });
    check(
      part.startsWith(squeezed("Selected 1 node Not shown: 30, 31 (hidden), 32, 33")) && f.matched === 1 && f.notShownTotal === 4 && (await fitted()).ids.join() === "3" && !listed.hidden && listed.text!.startsWith("Not shown (4):30") && listed.buttons.join() === "Show node 31 again",
      "matches that are not drawn are listed as not shown, a hidden one with a button",
      `${part.slice(0, 80)} | ${JSON.stringify(f)} | ${JSON.stringify(listed)}`,
    );
    await s.click("#graph-found button.add");
    await idle(s);
    const shown = await s.evaluate(() => (window as any).ursaShell.explored());
    check(shown.command === ".show 31" && shown.deltaBytes > 0 && squeezed(await after(".show 31", s)).startsWith(squeezed("+1 node")), "its button runs .show in the shell", JSON.stringify(shown));
    // The button is gone, and the focus is the graph's.
    check((await s.evaluate(() => document.activeElement?.tagName)) === "CANVAS" && !(await s.evaluate(() => document.querySelector("#graph-found button.add"))), "the focus goes back to the graph");
    await s.click("#terminal");
    const sql = await said(".find 3 --sql");
    check(sql.startsWith(squeezed('SELECT CAST("id" AS VARCHAR) AS node')) && (await graphInfo(s)).selection.length === 1, ".find --sql prints the SQL and selects nothing", sql.slice(0, 80));
    check((await errors(s)) === before, "search prints no error");
    await s.keyboard.press("Escape");
    await s.keyboard.press("Escape");
    check(!(await graphOpen(s)) && (await s.evaluate(() => document.getElementById("graph-found")!.hidden)), "closing the pane clears the matches not shown");
    const noGraph = await said(".find 3");
    check(noGraph.includes(squeezed("Error: no graph is open")), "with the pane closed, .find says no graph is open", noGraph.slice(0, 80));
  }

  // Click to expand (#443): the pane draws each delta, a double-click on a
  // node runs `.expand ID` through the terminal, and an undo puts the frame
  // back as it was.
  {
    const shell = <T,>(fn: string, ...args: any[]): Promise<T> => s.evaluate(([fn, args]) => (window as any).ursaShell[fn as string](...(args as any[])), [fn, args] as const);
    const place = (id: number) => shell<{ index: number; x: number; y: number; hidden: boolean } | null>("graphNodePlace", id);
    const lines = async (needle: string) => (await screen(s)).split("\n").filter((l) => l.includes(needle)).length;
    const waitFor = (test: string) => s.waitForFunction(test, null, { timeout: 30_000, polling: 50 }).then(() => true, () => false);
    // The canvas takes the focus when it is clicked: back to the terminal to type.
    const typed = async (text: string) => {
      await s.click("#terminal");
      await type(text, s);
    };
    const settled = async () => {
      await idle(s);
      await graphSettled(s);
    };
    await typed(hop.replace("steps => 2", "steps => 1"));
    await typed(".graph");
    await settled();
    const box = (await s.locator("#graph-stage canvas").boundingBox())!;
    const away = () => s.mouse.move(box.x - 40, box.y + 5);
    const dblclick = async (id: number, options: { shift?: boolean } = {}) => {
      const at = (await place(id))!;
      if (options.shift) await s.keyboard.down("Shift");
      await s.mouse.dblclick(box.x + at.x, box.y + at.y);
      if (options.shift) await s.keyboard.up("Shift");
    };
    const base = await graphInfo(s);
    // The +N: 2 has four neighbours beyond 0's (32, 27, 28 and 9).
    check(await waitFor("window.ursaShell.graph()?.notShownKnown === true"), "the engine's counts of what is not shown reach the view");
    {
      const at = (await place(2))!;
      await s.mouse.click(box.x + at.x, box.y + at.y);
      await s.waitForFunction(() => !(document.querySelector(".graph-tip") as HTMLElement).hidden, null, { timeout: 5_000 }).catch(() => {});
      const tip = (await s.locator(".graph-tip").textContent()) ?? "";
      const status = (await s.locator("#graph-status").textContent()) ?? "";
      check(status.startsWith("Selected node 2: ") && status.includes("+4 not shown") && /not shown\s*\+4/.test(tip), "a node with neighbours not shown says +N, in the header and the tooltip", `${status} | ${tip}`);
    }
    // The frame before: node 2 selected, the pointer off the canvas, and the
    // camera at rest, having made room for the inspector's panel (#493).
    await roomSettled(s);
    await away();
    await s.waitForFunction(() => (window as any).ursaShell.graph().hovered === null);
    const places = async (ids: number[]) => Promise.all(ids.map(place));
    const before = { inked: await shell<any>("graphInked"), at: await places([0, 2, 7, 31]) };
    const echoes = await lines("from the graph");
    await shell("graphTimingsReset");

    await dblclick(2);
    await settled();
    await away();
    const grown = await graphInfo(s);
    const text = await screen(s);
    const explored: any = await shell("explored");
    check(
      base.nodes === 17 && base.slots === 17 && grown.nodes === 21 && grown.slots === 21 && grown.edges > base.edges && grown.deltas === 1 && explored.nodes === grown.nodes && explored.edges === grown.edges,
      "a double-click on a node adds its neighbours to the drawing: the node count grows, as the engine's",
      `${base.nodes} nodes, ${base.edges} edges -> ${grown.nodes}, ${grown.edges} | engine ${JSON.stringify(explored)}`,
    );
    check(
      (await lines("from the graph")) === echoes + 1 && /from the graph memory U \.expand 2\n\+4 nodes, \+\d+ edges/.test(text),
      "the double-click's command is echoed as typed, after a dimmed 'from the graph', with the shell's one-line answer",
      text.split("\n").slice(-4).join(" | "),
    );
    {
      // The new nodes are drawn: each is on the canvas, is what a click there
      // picks, and the frame has more ink than before.
      const added = await places([32, 27, 28, 9]);
      const picked = await Promise.all(added.map((a) => (a ? shell<number | null>("graphNodeAt", a.x * (grown.width / box.width), a.y * (grown.height / box.height)) : null)));
      const inked: any = await shell("graphInked");
      check(
        added.every((a, i) => a && !a.hidden && a.x > 0 && a.x < box.width && a.y > 0 && a.y < box.height && picked[i] === a.index) && inked.inked > 0,
        "the new nodes are drawn where a click picks them",
        `${JSON.stringify(added)} | picked ${picked} | ${inked.inked} px inked (${before.inked.inked} before)`,
      );
      await waitFor("window.ursaShell.graph()?.notShownKnown === true");
      const status = (await s.locator("#graph-status").textContent()) ?? "";
      check(status.includes("21 nodes") && status.startsWith("Selected node 2: ") && !status.includes("not shown"), "the header counts the nodes shown, and 2 has nothing more to show", status);
    }
    {
      const t: any = await shell("graphTimings");
      const action = t.actions.findLast((a: any) => a.command === ".expand 2");
      check(action && action.engine > 0 && action.delta >= 0 && action.frame >= action.engine && action.frame < 5_000, "graphTimings.actions: the expansion, with its ms in the engine and applying the delta, to the frame that showed it", JSON.stringify(action));
    }
    await typed(".history 3");
    check((await after(".history 3", s)).includes(".expand 2"), ".history lists the double-click's command", (await after(".history 3", s)).slice(0, 120));

    // .undo: the frame as it was before the expansion, node for node and pixel for pixel.
    const expandedAt = await places([0, 2, 7, 31, 32]);
    await typed(".undo");
    await settled();
    await away();
    await s.waitForFunction(() => (window as any).ursaShell.graph().hovered === null);
    {
      const undone = await graphInfo(s);
      const after_ = { inked: await shell<any>("graphInked"), at: await places([0, 2, 7, 31]) };
      const moved = Math.max(...after_.at.map((a, i) => Math.hypot(a!.x - before.at[i]!.x, a!.y - before.at[i]!.y)));
      check(
        undone.nodes === 17 && undone.slots === 17 && undone.edges === base.edges && (await place(32)) === null && moved < 0.01 && after_.inked.inked === before.inked.inked && JSON.stringify(after_.inked.palette) === JSON.stringify(before.inked.palette),
        ".undo restores the earlier frame: the same nodes, in the same places, and the same pixels inked",
        `${undone.nodes} nodes, moved ${moved.toFixed(3)} px, ${after_.inked.inked} px inked (${before.inked.inked} before)`,
      );
    }
    // .redo: the expansion again, its nodes where they had settled, with no layout.
    await typed(".redo");
    await settled();
    {
      const redone = await graphInfo(s);
      const at = await places([0, 2, 7, 31, 32]);
      const moved = Math.max(...at.map((a, i) => (a && expandedAt[i] ? Math.hypot(a.x - expandedAt[i]!.x, a.y - expandedAt[i]!.y) : Infinity)));
      check(redone.nodes === 21 && moved < 0.01, ".redo draws the expansion again, where it had settled", `${redone.nodes} nodes, moved ${moved.toFixed(3)} px`);
    }

    // Shift expands up to the cap; Enter expands the selection.
    await dblclick(32, { shift: true });
    await settled();
    const shifted = await graphInfo(s);
    check((await screen(s)).includes("from the graph memory U .expand 32 LIMIT 5000") && shifted.nodes > 21, "Shift+double-click expands with LIMIT 5000", `${shifted.nodes} nodes`);
    {
      const at = (await place(33))!;
      await s.mouse.click(box.x + at.x, box.y + at.y);
      await s.focus("#graph-stage canvas");
      await s.keyboard.press("Enter");
      await settled();
      const entered = await graphInfo(s);
      check((await screen(s)).includes("from the graph memory U .expand 33") && entered.nodes > shifted.nodes && entered.selection.length === 1 && entered.selection[0].id == 33, "Enter expands the selected node", `${entered.nodes} nodes`);
      // Nothing more to add: the command says so, and nothing is drawn again.
      await s.keyboard.press("Enter");
      await settled();
      const again = await graphInfo(s);
      check(again.deltas === entered.deltas && again.nodes === entered.nodes && again.busy.length === 0, "expanding a node with nothing more to show changes nothing", (await screen(s)).split("\n").slice(-3).join(" | "));
      await s.click("#terminal");
    }
    // A double-click on empty space still fits.
    {
      const info = await graphInfo(s);
      const commands = await lines("from the graph");
      await shell("graphTimingsReset");
      // A corner of the canvas, where no node is.
      const empty = (await shell<number | null>("graphNodeAt", 3, 3)) === null;
      await s.mouse.dblclick(box.x + 3, box.y + 3);
      await idle(s);
      await s.waitForFunction(() => (window as any).ursaShell.graphTimings().actions.some((a: any) => a.command === "fit"), null, { timeout: 5_000 }).catch(() => {});
      const t: any = await shell("graphTimings");
      check(empty && t.actions.some((a: any) => a.command === "fit") && (await lines("from the graph")) === commands && (await graphInfo(s)).deltas === info.deltas, "a double-click on empty space still fits, and runs no command");
    }

    // A command that waits its turn: the node carries a busy ring until it has run.
    await typed(".undo\n.undo\n.undo");
    await settled();
    {
      const start = await graphInfo(s);
      // A slow statement, not waited for: the engine is busy with it.
      await s.click("#terminal");
      await s.keyboard.type("SELECT count(*) FROM range(4000) a, range(4000) b WHERE a.value + b.value = 7;");
      await s.keyboard.press("Enter");
      await dblclick(2);
      await dblclick(31);
      const busy = (await graphInfo(s)).busy.map((b: any) => Number(b.id)).sort((a: number, b: number) => a - b);
      const rings = await s.locator(".graph-busy:not([hidden])").count();
      const waiting = (await graphInfo(s)).deltas === start.deltas;
      await settled();
      const done = await graphInfo(s);
      check(
        waiting && JSON.stringify(busy) === "[2,31]" && rings === 2 && done.busy.length === 0 && (await s.locator(".graph-busy").count()) === 0 && done.deltas === start.deltas + 2 && done.nodes > start.nodes,
        "double-clicks while a command runs queue, each node with a busy ring until its command has run",
        `busy ${JSON.stringify(busy)}, ${rings} rings, ${start.nodes} -> ${done.nodes} nodes, ${done.deltas - start.deltas} deltas`,
      );
    }

    // .graph echo off: the command runs and is drawn, and the terminal stays quiet.
    await typed(".undo\n.undo");
    await settled();
    {
      const start = await graphInfo(s);
      await typed(".graph echo off");
      const off = await after(".graph echo off", s);
      const commands = await lines("from the graph");
      const answers = await lines("+4 nodes");
      await dblclick(2);
      await settled();
      const quiet = await graphInfo(s);
      await typed(".history 2");
      const listed = await after(".history 2", s);
      check(
        off.includes("without being shown") && quiet.nodes === start.nodes + 4 && (await lines("from the graph")) === commands && (await lines("+4 nodes")) === answers && listed.includes(".expand 2"),
        ".graph echo off: a double-click still expands, with nothing written but its place in .history",
        `${start.nodes} -> ${quiet.nodes} nodes | ${listed.slice(0, 80)}`,
      );
      await typed(".graph echo on");
      await dblclick(31);
      await settled();
      check((await after(".graph echo on", s)).includes("are shown here") && (await lines("from the graph")) === commands + 1, ".graph echo on shows them again");
      await typed(".graph echo loud");
      check((await after(".graph echo loud", s)).includes("Error: usage: .graph echo on|off"), ".graph echo takes on or off");
    }

    // .hide and .show are drawn too: a hidden node is not picked, and comes back where it was.
    {
      const start = await graphInfo(s);
      const was = (await place(31))!;
      await typed(".hide 31");
      await idle(s);
      const hidden = await graphInfo(s);
      const gone = (await place(31))!;
      const picked = await shell<number | null>("graphNodeAt", gone.x * (hidden.width / box.width), gone.y * (hidden.height / box.height));
      await typed(".show 31");
      await idle(s);
      const back = (await place(31))!;
      const shown = await graphInfo(s);
      check(
        hidden.nodes === start.nodes - 1 && hidden.slots === start.slots && gone.hidden && picked !== gone.index && shown.nodes === start.nodes && !back.hidden && Math.hypot(back.x - was.x, back.y - was.y) < 0.01,
        ".hide takes a node out of the drawing and .show brings it back where it was",
        `${start.nodes} -> ${hidden.nodes} -> ${shown.nodes} nodes`,
      );
    }

    // A shared link of the session replays to the same drawing.
    {
      const here = await graphInfo(s);
      await typed(".share");
      const link = s.url().includes("#run=") ? s.url() : await s.evaluate(() => navigator.clipboard.readText());
      const r = await actx.newPage();
      r.on("pageerror", (e) => failures.push(`replay page error: ${e.message}`));
      await r.goto(link);
      await r.waitForFunction(() => (window as any).ursaShell?.screen().includes("? [y/N] "), null, { timeout: 120_000 });
      const asked = await screen(r);
      await r.keyboard.press("y");
      await idle(r);
      await graphSettled(r);
      const there = await graphInfo(r);
      check(
        asked.includes(".expand 2") && asked.includes(".hide 31") && there.nodes === here.nodes && there.edges === here.edges && there.slots === here.slots && (await errors(r)) === 0,
        "a shared link of the session replays the exploration, to the same node and edge counts",
        `${here.nodes} nodes, ${here.edges} edges | replayed ${there.nodes}, ${there.edges}`,
      );
      await r.close();
    }
    await s.keyboard.press("Escape");
    await s.keyboard.press("Escape");
    check(!(await graphOpen(s)), "Esc closes the explored drawing");
  }
  // Two columns of ids, whatever their names, draw as before: integers, and strings.
  for (const sql of ["SELECT src AS a, dst AS b FROM ties", "SELECT src::VARCHAR, dst::VARCHAR, 0.5 FROM ties"]) {
    await type(`.graph ${sql}`, s);
    const out = (await after(`.graph ${sql}`, s)).split("\n")[0];
    check(out.startsWith("Drawing 78 edges, 34 nodes") && (await graphOpen(s)), `.graph ${sql}: two id columns draw as before`, out);
  }
  await s.keyboard.press("Escape");
  check(!(await graphOpen(s)), "Esc closes the drawing of two id columns");
  await actx.close();
}
// SIZE BY and COLOR BY (#389): Karate's two factions in two colours, in each
// theme, read back from the view's pixels (the view draws in the theme's own
// colours: no CSS inversion), a theme switch with the pane open, the legend,
// NODES (…), and each error said plainly. And the status line (#390), from
// Enter to Done.
/** Statuses the pane's header showed, in order, since the last call. */
const statuses = (p: Page): Promise<string[]> =>
  p.evaluate(() => {
    const w = window as any;
    const seen: string[] = w.__statuses ?? [];
    w.__statuses = [];
    if (!w.__statusObserver) {
      const el = document.getElementById("graph-status")!;
      w.__statusObserver = new MutationObserver(() => w.__statuses.push(el.textContent));
      w.__statusObserver.observe(el, { childList: true, characterData: true, subtree: true });
    }
    return seen;
  });
/** The palette colours drawn: how many of each (8 hues, then other) cover more than `min` pixels. */
const huesDrawn = async (p: Page, min = 40) => {
  const inked: any = await p.evaluate(() => (window as any).ursaShell.graphInked());
  return { inked, hues: (inked?.palette ?? []).filter((n: number) => n > min).length };
};
/** The legend as text: what sizes the nodes, what colours them, then each colour's name and count. */
const legendText = (p: Page) =>
  p.evaluate(() => {
    const legend = document.getElementById("graph-legend")!;
    if (legend.hidden) return "";
    return [...legend.querySelectorAll(".size, .color-head, li")].map((e) => e.textContent!.replace(/\s+/g, " ").trim()).join(" ");
  });
for (const scheme of ["light", "dark"] as const) {
  const sctx = await watchedContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 2, colorScheme: scheme });
  const s = await sctx.newPage();
  await s.goto(`${base}/?dataset=karate`);
  await ready(s);
  await idle(s);
  await s.click("#terminal");
  await statuses(s);
  if (scheme === "light") {
    for (const [args, says] of [
      ["karate SIZE BY nope", "SIZE BY nope: graph 'karate' has no vertex column or algorithm named 'nope'. Its vertex columns are"],
      ["karate COLOR BY wcc", "the algorithms are degree, pagerank"],
      ["karate SIZE BY club", "SIZE BY club needs numbers, and club is text; colour nodes by it instead: COLOR BY club"],
      ["karate COLOR BY pagerank", "distinct values over 34 nodes are too many categories to tell apart by colour. A measure is better shown by size: SIZE BY pagerank"],
      ["karate COLOR BY id", "COLOR BY id: 34 distinct values over 34 nodes are too many categories to tell apart by colour. Colour by a column with fewer values, or size nodes by this one: SIZE BY id"],
      ["karate COLOR BY name", "COLOR BY name: graph 'karate' has no vertex column or algorithm named 'name'. Its vertex columns are club, id;"],
      ["SELECT src, dst FROM ties SIZE BY pagerank", "SIZE BY pagerank runs pagerank() on a property graph, and this draws rows"],
      ["SELECT src, dst FROM ties COLOR BY club", "no column named 'club'. Rows drawn as edges are sized or coloured by degree, or by a column of NODES"],
      ["karate SIZE BY", "SIZE BY needs a numeric column or an algorithm"],
      ["karate SIZE BY pagerank SIZE BY degree", "SIZE BY is given twice"],
      ["karate NODES (SELECT id, 1 AS one FROM members)", "NODES (…) gives values for SIZE BY or COLOR BY to use"],
      ["karate NODES (SELECT id FROM members) SIZE BY id", "NODES (…) needs two columns or more"],
    ]) {
      await type(`.graph ${args}`, s);
      const out = await after(`.graph ${args}`, s);
      // Compared without spaces: the terminal wraps long lines.
      const squeezed = (t: string) => t.replace(/\s+/g, "");
      check(out.startsWith("Error: ") && squeezed(out).includes(squeezed(says)) && !(await graphOpen(s)), `.graph ${args}: a clear error`, out.split("\n").slice(0, 3).join(" "));
    }
  }

  // Sized by PageRank, coloured by Louvain: run through algorithms(karate, …).
  await statuses(s);
  await type(".graph karate SIZE BY pagerank COLOR BY louvain", s);
  check((await after(".graph karate SIZE BY", s)).startsWith("Drawing 78 edges, 34 nodes as the graph karate (Esc closes)"), `${scheme}: .graph karate SIZE BY pagerank COLOR BY louvain draws`, (await after(".graph karate SIZE BY", s)).split("\n")[0]);
  await graphSettled(s);
  const seen = await statuses(s);
  const loading = seen.findIndex((t) => t.startsWith("Loading graph view…"));
  const laying = seen.findIndex((t) => / · Laying out · level \d+ of \d+$/.test(t));
  const done = seen.findIndex((t) => t === "34 nodes · 78 edges · drawn as the graph karate · Done");
  check(laying >= 0 && done > laying && (loading < 0 || loading < laying), `${scheme}: the status shows the layout's progress, then Done`, JSON.stringify(seen));
  const legend = await legendText(s);
  check(/^size: pagerank 0\.\d+–0\.\d+ colour: louvain (\S+ \(\d+\) ?){2,8}$/.test(legend), `${scheme}: the legend names what sizes and colours the nodes`, legend);
  const counts = [...legend.matchAll(/\((\d+)\)/g)].reduce((a, m) => a + Number(m[1]), 0);
  check(counts === 34, `${scheme}: the legend counts every node`, `${counts}`);
  const filter = await s.evaluate(() => getComputedStyle(document.querySelector("#graph-stage canvas")!).filter);
  let drawn = await huesDrawn(s);
  const bg = scheme === "light" ? [255, 255, 255] : [11, 11, 10];
  check(drawn.hues >= 2 && filter === "none" && drawn.inked.corner.every((v: number, i: number) => Math.abs(v - bg[i]) <= 2), `${scheme}: two colours or more, in the theme's own palette, on its own ground (no inversion)`, `${JSON.stringify(drawn.inked.palette)}, corner ${drawn.inked.corner}, filter ${filter}`);
  if (shots) {
    await s.waitForTimeout(300);
    await s.screenshot({ path: `${shots}/graph-styled-${scheme}.png` });
    await s.locator("#graph-pane").screenshot({ path: `${shots}/graph-legend-${scheme}.png`, clip: undefined });
  }

  // The theme switched with the pane open: the view is drawn in the other's colours.
  await s.click("#theme");
  const other = scheme === "light" ? [11, 11, 10] : [255, 255, 255];
  drawn = await huesDrawn(s);
  check(drawn.hues >= 2 && drawn.inked.corner.every((v: number, i: number) => Math.abs(v - other[i]) <= 2), `${scheme}: switching the theme recolours the open view`, `${JSON.stringify(drawn.inked.palette)}, corner ${drawn.inked.corner}`);
  if (shots) await s.screenshot({ path: `${shots}/graph-styled-${scheme}-switched.png` });
  await s.click("#theme");

  if (scheme === "light") {
    // A vertex column, and degree; a click names the node's values.
    await type(".graph karate SIZE BY degree COLOR BY club", s);
    await graphSettled(s);
    const clubs = await legendText(s);
    check(/^size: degree 1–17 colour: club (Mr\. Hi|Officer) \(\d+\) (Mr\. Hi|Officer) \(\d+\)$/.test(clubs), "COLOR BY a vertex column, SIZE BY degree", clubs);
    const box = (await s.locator("#graph-stage canvas").boundingBox())!;
    const hit: { x: number; y: number } | null = await s.evaluate(([w, h]) => {
      for (let y = 2; y < h; y += 2)
        for (let x = 2; x < w; x += 2) if ((window as any).ursaShell.graphNodeAt(x * 2, y * 2) !== null) return { x, y };
      return null;
    }, [box.width, box.height]);
    if (hit) await s.mouse.click(box.x + hit.x, box.y + hit.y);
    const status = await s.locator("#graph-status").textContent();
    check(!!hit && /^Selected node \d+: (label \S+, )?degree \d+, club (Mr\. Hi|Officer) · 34 nodes · 78 edges$/.test(status ?? ""), "a click names the node's size and colour values", status ?? "no node found");
    {
      // The tooltip has the same: the key, the label (when the result names one), SIZE BY, COLOR BY and the degree.
      await s.mouse.move(box.x + 3, box.y + 3);
      if (hit) await s.mouse.move(box.x + hit.x, box.y + hit.y);
      await s.waitForFunction(() => document.querySelector<HTMLElement>("#graph-stage .graph-tip")?.hidden === false, null, { timeout: 5_000 }).catch(() => {});
      const tip = await s.evaluate(() => Array.from(document.querySelector("#graph-stage .graph-tip")?.children ?? [], (c) => c.textContent).join(" | "));
      check(/^node \d+ \| (label \S+ \| )?degree \d+ \| club (Mr\. Hi|Officer) \| degree \d+$/.test(tip), "the tooltip has the node's key, SIZE BY and COLOR BY values and degree", tip);
    }
    await s.click("#terminal");

    // NODES (…): values for a query's rows, joined on id.
    // Karate's members are 0 to 33: the four past 29 have no values.
    const nodes = ".graph SELECT src, dst FROM ties NODES (SELECT id, CASE WHEN club = 'Officer' THEN 'officer' END AS side, id * 2 AS twice FROM members WHERE id < 30) SIZE BY twice COLOR BY side";
    await type(nodes, s);
    const drew = (await after(".graph SELECT src, dst FROM ties NODES", s)).split("\n").filter((l, i, all) => l.startsWith("Drawing") || all[i - 1]?.startsWith("Drawing")).join("");
    check(drew.replace(/\s+/g, "") === "Drawing 78 edges, 34 nodes as edges (src, dst) (replacing the last drawing; Esc closes)".replace(/\s+/g, ""), ".graph QUERY NODES (…) draws", drew);
    await graphSettled(s);
    const sides = await legendText(s);
    check(/^size: twice 0–58 colour: side officer \(\d+\) no value \(\d+\)$/.test(sides), "NODES (…) columns size and colour a query's nodes", sides);
    drawn = await huesDrawn(s, 10);
    check(drawn.inked.palette[0] > 10 && drawn.inked.palette[8] > 10, "NODES (…): a category's colour, and other's grey for no value", JSON.stringify(drawn.inked.palette));
    await s.keyboard.press("Escape");
    check(!(await graphOpen(s)) && (await legendText(s)) === "", "Esc closes the pane and its legend");
    check((await errors(s)) === 12, "no errors from styled drawings but the ones asked for", `${await errors(s)}`);
  }
  await sctx.close();
}
// A big graph says so at once (#390): rmat's million edges show a status
// within a second of Enter, and the layout's levels before it is done.
{
  const rctx = await watchedContext({ viewport: { width: 1280, height: 800 } });
  const r = await rctx.newPage();
  await r.goto(`${base}/?dataset=rmat`);
  await ready(r);
  await idle(r);
  await r.click("#terminal");
  // Each status as it is set, and when: the page is too busy drawing a
  // million edges (in software, headless) to be polled on time.
  await r.evaluate(() => {
    const w = window as any;
    const el = document.getElementById("graph-status")!;
    w.__timed = [];
    new MutationObserver(() => w.__timed.push([performance.now(), el.textContent])).observe(el, { childList: true, characterData: true, subtree: true });
  });
  await r.keyboard.type(".graph rmat");
  const enter: number = await r.evaluate(() => performance.now());
  await r.keyboard.press("Enter");
  await graphSettled(r);
  const timed: [number, string][] = await r.evaluate(() => (window as any).__timed);
  const first = timed[0];
  const laying = timed.filter(([, t]) => / · Laying out · level \d+ of \d+( · [\d,]+ groups)?$/.test(t)).map(([, t]) => t.match(/level (\d+) of (\d+)/)!.slice(1).map(Number));
  const doneAt = timed.findIndex(([, t]) => t.endsWith(" · Done"));
  const times = await r.evaluate(() => (window as any).ursaShell.graphTimings());
  check(
    first && first[0] - enter < 1_000 && first[1] === "Loading graph view…" && laying.length > 1 && laying[0][1] > 1 && doneAt > 0,
    "a big graph: its status within a second, then its layout's levels, then Done",
    `"${first?.[1]}" at ${Math.round((first?.[0] ?? 0) - enter)} ms; levels ${JSON.stringify(laying.slice(0, 3))}…; ms after Enter ${JSON.stringify(times)}`,
  );
  // #497: a graph this large is first drawn as the coarsest level of its
  // layout's hierarchy, then each level the layout comes to, the status
  // saying how many groups each has (more at every level), until the graph
  // itself; nothing is picked meanwhile, and the frames of the levels are
  // 50 ms apart at most (the 95th percentile; twice that is allowed here).
  {
    const groups = timed.map(([, t]) => t.match(/ · Laying out · level (\d+) of (\d+) · ([\d,]+) groups$/)).filter(Boolean).map((m) => [Number(m![1]), Number(m![2]), Number(m![3].replace(/,/g, ""))]);
    const graph = await graphInfo(r);
    const levels: [number, number][] = graph.hierarchy;
    const rising = groups.every((g, i) => i === 0 || (g[0] > groups[i - 1][0] ? g[2] > groups[i - 1][2] : g[0] === groups[i - 1][0] && g[2] === groups[i - 1][2]));
    const last = timed.findLast(([, t]) => / · Laying out · /.test(t))?.[1] ?? "";
    check(
      groups.length > 3 && groups[0][0] === 1 && groups[0][2] < 100 && rising && / · Laying out · level (\d+) of \1$/.test(last) && graph.coarse === null,
      "a big graph: the status counts each level's groups, coarsest first, until the graph itself",
      `${JSON.stringify(groups.slice(0, 4))}…; the last "${last}"; coarse ${JSON.stringify(graph.coarse)}`,
    );
    check(
      levels.length === groups[0]?.[1] && levels[0][1] === groups[0]?.[2] && levels.at(-1)![0] === 0 && levels.every((l, i) => i === 0 || (l[0] === levels[i - 1][0] - 1 && l[1] > levels[i - 1][1])),
      "a big graph: every level of the hierarchy reached the page, each larger than the one above",
      JSON.stringify(levels),
    );
    const lf = graph.levelFrames;
    check(lf.count > 5 && lf.p95 <= 100 && times.graphDrawn > times.firstDraw, "a big graph: the coarse levels' frames come 50 ms apart at most, before the graph itself is drawn", `${JSON.stringify(lf)}; first frame at ${Math.round(times.firstDraw)} ms, the graph itself at ${Math.round(times.graphDrawn)} ms`);
  }
  // #498: the aggregated view. Laid out, the million edges are drawn as a
  // cut through the layout's hierarchy: at the fitted zoom at most 20,000
  // discs and 100,000 merged edges, which the status line says first, the
  // groups together holding every node, the largest labelled with their
  // counts; a cut takes the layout worker 30 ms at most (twice that is
  // allowed here); a click on a group selects its nodes; `a` and
  // `.graph detail on|off` force full detail and let it go; and zooming in
  // reaches the graph's own nodes, drawn as the graph is.
  {
    const cutDrawn = (want = true) =>
      r
        .waitForFunction((want) => { const g = (window as any).ursaShell.graph(); return g && !g.cuts.waiting && (g.cut !== null) === want; }, want, { timeout: 30_000, polling: 50 })
        .then(() => true, () => false);
    const statusNow = async () => (await r.locator("#graph-status").textContent()) ?? "";
    const drawn = await cutDrawn();
    const g = await graphInfo(r);
    const status = await statusNow();
    const said = status.match(/^aggregated: ([\d,]+) groups of 46,318 nodes · 1,000,000 edges · /);
    check(
      drawn && g.cut && g.cut.groups <= 20_000 && g.cut.groups > 20 && g.cut.edges <= 100_000 && g.cut.edges > 0 && g.cut.totalEdges >= g.cut.edges && g.coarse === null,
      "a big graph, laid out, is drawn aggregated: at most 20,000 discs and 100,000 merged edges at the fitted zoom",
      JSON.stringify(g.cut),
    );
    check(g.cut?.nodes === 46_318 && said && Number(said[1].replace(/,/g, "")) === g.cut.groups, "a big graph: the groups hold every node, and the status says aggregated: G groups of N nodes", status);
    check(g.cuts.ms.count >= 1 && g.cuts.ms.max <= 60 && g.cuts.prepared?.bytes > 0, "a big graph: a cut takes the layout worker 30 ms at most (60 allowed)", JSON.stringify(g.cuts));
    check(g.groupLabels.length >= 1 && g.groupLabels.every((l: string) => /^[\d,.]+[KM]?$/.test(l)), "a big graph: its largest groups are labelled with their node counts", JSON.stringify(g.groupLabels));
    // A click on the heaviest group's label: its nodes are selected.
    const box = (await r.locator("#graph-stage canvas").boundingBox())!;
    const label = await r.evaluate(() => {
      const el = [...document.querySelectorAll<HTMLElement>("#graph-stage .graph-group")].find((l) => !l.hidden)!;
      const b = el.getBoundingClientRect();
      return { x: b.x + b.width / 2, y: b.y + b.height / 2, text: el.textContent };
    });
    await r.mouse.click(label.x, label.y);
    await r.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
    const picked = await statusNow();
    const members = Number(label.text!.replace(/,/g, ""));
    check(
      new RegExp(`^Selected ${label.text!.replace(".", "\\.")} nodes: .* · aggregated: [\\d,]+ groups of 46,318 nodes · `).test(picked) && (await graphInfo(r)).cut !== null && members > 1,
      "a big graph: a click on a group selects its nodes",
      picked.slice(0, 160),
    );
    // A click on empty space clears.
    await r.mouse.click(box.x + 8, box.y + box.height - 8);
    await r.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
    check(/^aggregated: /.test(await statusNow()), "a big graph: a click on empty space clears the group's selection", (await statusNow()).slice(0, 80));
    // `a` forces full detail, and `a` again lets it go.
    await r.locator("#graph-stage canvas").focus();
    await r.keyboard.press("a");
    const forced = await cutDrawn(false);
    const full = await graphInfo(r);
    check(forced && full.cut === null && full.cuts.detail && !full.cuts.on && !(await statusNow()).includes("aggregated") && full.groupLabels.length === 0, "a big graph: a forces full detail", `${JSON.stringify(full.cuts)}; ${(await statusNow()).slice(0, 80)}`);
    await r.keyboard.press("a");
    check((await cutDrawn()) && /^aggregated: /.test(await statusNow()), "a big graph: a again aggregates it", (await statusNow()).slice(0, 80));
    // Zooming in on the middle opens the groups in view, down to the
    // graph's own nodes: then the graph itself is drawn.
    const sizes: number[] = [];
    let reached = false;
    for (let step = 0; step < 16 && !reached; step++) {
      await r.evaluate(
        ([x, y]) => {
          const canvas = document.querySelector("#graph-stage canvas")!;
          for (let i = 0; i < 4; i++) canvas.dispatchEvent(new WheelEvent("wheel", { deltaY: -100, clientX: x, clientY: y, bubbles: true, cancelable: true }));
        },
        [box.x + box.width / 2, box.y + box.height / 2],
      );
      // The camera at rest, and the cut for it.
      await r.waitForTimeout(250);
      await r.waitForFunction(() => !(window as any).ursaShell.graph().cuts.waiting, null, { timeout: 30_000, polling: 50 }).catch(() => {});
      const now = await graphInfo(r);
      sizes.push(now.cut?.groups ?? 0);
      reached = now.cut === null;
    }
    const near = await graphInfo(r);
    check(
      reached && sizes.length > 1 && sizes[0] > g.cut.groups && sizes.slice(0, -1).every((n) => n <= 20_000) && near.cuts.on && !(await statusNow()).includes("aggregated"),
      "a big graph: zooming into a region opens its groups and reaches the graph's own nodes",
      `${g.cut.groups} groups fitted, then ${sizes.join(", ")} (0: the graph itself); ${(await statusNow()).slice(0, 80)}`,
    );
    check(near.cuts.ms.max <= 60, "a big graph: every cut on the way took the layout worker 30 ms at most (60 allowed)", JSON.stringify(near.cuts.ms));
    // Fitted again, it is aggregated again.
    await r.keyboard.press("0");
    check((await cutDrawn()) && (await graphInfo(r)).cut.groups === g.cut.groups, "a big graph: fitted again, the same cut as before", JSON.stringify((await graphInfo(r)).cut));
    // `.graph detail on` in the terminal: full detail from here, which is
    // what the checks below (the frame budget and the refinement of the
    // million edges themselves) are of.
    await r.click("#terminal");
    await type(".graph detail", r);
    const asked = await after(".graph detail", r);
    await type(".graph detail on", r);
    const on = await after(".graph detail on", r);
    await cutDrawn(false);
    check(
      asked.includes("drawn aggregated once it is laid out") && on.includes("drawn in full detail") && (await graphInfo(r)).cut === null && !(await statusNow()).includes("aggregated"),
      ".graph detail says which, and .graph detail on draws every node and edge",
      `${asked.trim().slice(0, 60)} | ${on.trim().slice(0, 60)} | ${(await statusNow()).slice(0, 60)}`,
    );
    await type(".graph detail sideways", r);
    check((await after(".graph detail sideways", r)).includes("usage: .graph detail on|off"), ".graph detail refuses another word");
  }
  // #495: the frame budget, on the million edges. The layout worker's order
  // of the edges is in the view; a wheel zoom, an event an animation frame,
  // is drawn in frames 50 ms apart at most (the 95th percentile; twice that
  // is allowed here, for a machine busy with other things), each with a
  // prefix of the edges and the largest nodes; and once the view rests, a
  // frame with everything.
  {
    await r.waitForFunction(() => (window as any).ursaShell.graph()?.frame.ordered === 1_000_000, null, { timeout: 30_000, polling: 100 }).catch(() => {});
    // From rest: the layout is done, and its frame with every edge is
    // drawn (seconds of the page's thread, with no GPU), so the zoom's
    // frames are the view moving and nothing else.
    const settled = await r
      .waitForFunction(() => (window as any).ursaShell.graph()?.frame.partial === false, null, { timeout: 60_000, polling: 100 })
      .then(() => true, () => false);
    check(settled, "a big graph: after the layout, a frame with every edge", JSON.stringify((await graphInfo(r)).frame));
    await r.evaluate(() => new Promise((done) => requestAnimationFrame(() => setTimeout(done, 150))));
    const before = await graphInfo(r);
    const box = (await r.locator("#graph-stage canvas").boundingBox())!;
    await r.evaluate(() => (window as any).ursaShell.graphTimingsReset());
    const moving: any = await r.evaluate(
      ([x, y]) =>
        new Promise((done) => {
          const canvas = document.querySelector("#graph-stage canvas")!;
          const began = performance.now();
          const step = () => {
            const t = performance.now() - began;
            if (t >= 2_000) return done((window as any).ursaShell.graph().frame);
            canvas.dispatchEvent(new WheelEvent("wheel", { deltaY: t < 1_000 ? -30 : 30, clientX: x, clientY: y, bubbles: true, cancelable: true }));
            requestAnimationFrame(step);
          };
          requestAnimationFrame(step);
        }),
      [box.x + box.width / 2, box.y + box.height / 2],
    );
    const zoomed = (await r.evaluate(() => (window as any).ursaShell.graphTimings())).frames;
    check(
      before.frame.ordered === 1_000_000 && before.frame.edges === 1_000_000 && before.frame.orderMs >= 0,
      "a big graph: the layout worker's order of the edges is in the view",
      JSON.stringify(before.frame),
    );
    check(
      zoomed.interval.count >= 20 && zoomed.interval.p95 <= 100 && moving.partial && moving.k >= 256 && moving.k < 1_000_000 && moving.nodes >= 64 && moving.frames >= 20 && moving.p95 > 0,
      "a big graph: a wheel zoom is drawn in frames at most 50 ms apart (p95; 100 allowed), each a prefix of the edges",
      `intervals ${JSON.stringify(zoomed.interval)}; ${JSON.stringify(moving)}`,
    );
    const rested = await r
      .waitForFunction(() => (window as any).ursaShell.graph()?.frame.partial === false, null, { timeout: 60_000, polling: 100 })
      .then(() => true, () => false);
    check(rested, "a big graph: at rest again, a frame with every edge", JSON.stringify((await graphInfo(r)).frame));
  }
  // #496: refinement at rest. The frame with every edge is drawn offscreen
  // in slices, the status saying so, with no long task on the page (none
  // over 50 ms is the target; twice that is allowed here); a hover at rest
  // draws no slices; and a zoom begun under the slices stops them.
  {
    const box = (await r.locator("#graph-stage canvas").boundingBox())!;
    const [x, y] = [box.x + box.width / 2, box.y + box.height / 2];
    const refined: any = await r.evaluate(
      ([x, y]) =>
        new Promise((done) => {
          const canvas = document.querySelector("#graph-stage canvas")!;
          const frame = () => (window as any).ursaShell.graph().frame;
          const status = () => document.getElementById("graph-status")?.textContent ?? "";
          const tasks: number[][] = [];
          const seen = new PerformanceObserver((list) => {
            for (const e of list.getEntries()) tasks.push([e.startTime, e.duration]);
          });
          seen.observe({ type: "longtask" });
          canvas.dispatchEvent(new WheelEvent("wheel", { deltaY: -30, clientX: x, clientY: y, bubbles: true, cancelable: true }));
          const t0 = performance.now();
          let said = "";
          let began = 0;
          const look = () => {
            if (frame().refining) {
              began ||= performance.now();
              if (status().includes("refining…")) said = status();
            }
            if (!began || frame().partial || frame().refining) return void (performance.now() - t0 < 60_000 ? setTimeout(look, 20) : done({ timedOut: true, frame: frame() }));
            const ended = performance.now();
            setTimeout(() => {
              seen.disconnect();
              done({ said, status: status(), frame: frame(), ms: ended - t0, tasks: tasks.filter(([start]) => start >= began && start <= ended).map(([, d]) => Math.round(d)) });
            }, 300);
          };
          setTimeout(look, 20);
        }),
      [x, y],
    );
    check(
      !refined.timedOut && refined.frame.refined && refined.frame.refinement.slices > 4 && refined.said.endsWith(" · refining…") && !refined.status.includes("refining"),
      "a big graph: at rest, the frame with every edge is drawn in slices, and the status says refining… meanwhile",
      JSON.stringify(refined),
    );
    check(!refined.timedOut && Math.max(0, ...refined.tasks) <= 100, "a big graph: no task of the page over 50 ms while the slices are drawn (100 allowed)", JSON.stringify(refined.tasks));
    // A hover at rest: the refined texture and the hovered node over it.
    await r.mouse.move(x + 7, y + 5);
    await r.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(done, 300)))));
    const hovered = (await graphInfo(r)).frame;
    check(
      hovered.refined && !hovered.refining && !hovered.partial && hovered.refinement.ms === refined.frame?.refinement.ms,
      "a big graph: a hover at rest draws no slices",
      JSON.stringify(hovered),
    );
    // A zoom under the slices: they stop, and its frame is a moving one.
    const under: any = await r.evaluate(
      ([x, y]) =>
        new Promise((done) => {
          const canvas = document.querySelector("#graph-stage canvas")!;
          const frame = () => (window as any).ursaShell.graph().frame;
          const wheel = () => canvas.dispatchEvent(new WheelEvent("wheel", { deltaY: 30, clientX: x, clientY: y, bubbles: true, cancelable: true }));
          wheel();
          const t0 = performance.now();
          const look = () => {
            if (!frame().refining) return void (performance.now() - t0 < 30_000 ? setTimeout(look, 10) : done({ timedOut: true, frame: frame() }));
            // Slices are being drawn: a few more, then the zoom.
            setTimeout(() => {
              const before = frame();
              wheel();
              const asked = performance.now();
              requestAnimationFrame(() => requestAnimationFrame(() => done({ before, after: frame(), ms: performance.now() - asked })));
            }, 100);
          };
          setTimeout(look, 10);
        }),
      [x, y],
    );
    check(
      !under.timedOut && under.before.refining && !under.after.refining && !under.after.refined && under.after.partial && under.ms <= 250,
      "a big graph: a zoom begun under the slices stops them, and is drawn within two frames",
      JSON.stringify(under),
    );
    const again = await r
      .waitForFunction(() => (window as any).ursaShell.graph()?.frame.partial === false, null, { timeout: 60_000, polling: 100 })
      .then(() => true, () => false);
    check(again, "a big graph: at rest after that zoom, a frame with every edge", JSON.stringify((await graphInfo(r)).frame));
  }
  // #445: a key search on the million edges answers within 50 ms (the
  // engine's round trip; drawing the selection is the view's, after it).
  {
    const ms: number[] = [];
    let last: any = null;
    let capped = "";
    for (const key of ["1", "4242", "9999"]) {
      await type(`.find ${key}`, r);
      last = await r.evaluate(() => (window as any).ursaShell.found());
      ms.push(Math.round(last.ms));
      // More matches than are selected: the status says how many there were.
      if (key === "1") capped = `${last.selected} of ${last.matched}: ${((await r.locator("#graph-status").textContent()) ?? "").slice(0, 60)}`;
    }
    check(/^1000 of (\d+): Selected 1,000 of [\d,]+ matching nodes: /.test(capped) && Number(capped.match(/of (\d+)/)![1]) > 1000, "at most 1,000 matches are selected, and the status says how many there were", capped);
    const sorted = [...ms].sort((a, b) => a - b);
    check(sorted[1] <= 50 && last.command === ".find 9999" && last.matched >= 1, "a key search on rmat answers in 50 ms", `${ms.join(", ")} ms; ${JSON.stringify({ ...last, notShown: undefined })}`);
    await r.waitForFunction(() => !(window as any).ursaShell.graph()?.fitting, null, { timeout: 60_000 });
    await r.keyboard.press("Escape");
  }
  await r.keyboard.press("Escape");
  check(!(await graphOpen(r)) && (await errors(r)) === 1, "a big graph's view closes, with no error but the one asked for", `${await errors(r)}`);

  // #443's acceptance, on the million-edge graph: from a hop result, ten
  // double-clicks each add nodes, each echoes one command, .history lists
  // them, and a click's frame comes within 250 ms.
  {
    // (From node 0: this R-MAT graph has no node 1.)
    await type(".graph SELECT * FROM hop(rmat, seeds => [0], steps => 1, direction => 'both')", r);
    await graphSettled(r);
    const box = (await r.locator("#graph-stage canvas").boundingBox())!;
    const known = () => r.waitForFunction(() => (window as any).ursaShell.graph()?.notShownKnown === true, null, { timeout: 30_000, polling: 50 }).then(() => true, () => false);
    const commands: string[] = [];
    const grew: number[] = [];
    let last = (await graphInfo(r)).nodes;
    const first = last;
    for (let i = 0; i < 10; i++) {
      await known();
      // A node not yet expanded, with neighbours to add, the fewest first:
      // the hubs of this graph have thousands.
      // With the inspector open, once the camera has made room for its
      // panel (#493): any node in sight can be double-clicked.
      await roomSettled(r);
      const all: any[] = await r.evaluate(() => (window as any).ursaShell.graphExpandable(100_000));
      const next = all.filter((c) => !commands.includes(`.expand ${c.id}`)).sort((a, b) => a.notShown - b.notShown)[0];
      if (!next) break;
      await r.mouse.dblclick(box.x + next.x, box.y + next.y);
      await idle(r);
      await graphSettled(r);
      const now = (await graphInfo(r)).nodes;
      commands.push(`.expand ${next.id}`);
      grew.push(now - last);
      last = now;
    }
    const text = await screen(r);
    const echoed = commands.filter((c) => count(text, `from the graph memory U ${c}\n`) === 1).length;
    check(commands.length === 10 && grew.every((n) => n > 0) && echoed === 10, "rmat: ten double-clicks each add nodes, and each echoes one command", `${first} nodes, then +${grew.join(", +")}; ${echoed} echoed`);
    await r.click("#terminal");
    await type(".history 11", r);
    const listed = await after(".history 11", r);
    check(commands.every((c) => listed.split("\n").some((l) => l.trim().endsWith(c))), "rmat: .history lists the ten commands", listed.split("\n").slice(0, 3).join(" | "));
    const t: any = await r.evaluate(() => (window as any).ursaShell.graphTimings());
    const frames: number[] = commands.map((c) => t.actions.findLast((a: any) => a.command === c)?.frame).filter((f) => f !== undefined);
    const sorted = [...frames].sort((a, b) => a - b);
    const p95 = sorted[Math.min(sorted.length - 1, Math.ceil(0.95 * sorted.length) - 1)];
    const median = sorted[Math.floor(sorted.length / 2)];
    const engineMs = commands.map((c) => t.actions.findLast((a: any) => a.command === c)?.engine);
    // The target is 250 ms at the p95. The median is what is asserted: a
    // wall-clock p95 of ten flakes on a shared runner, so it is printed.
    check(frames.length === 10 && median <= 250, "rmat: a double-click's frame comes within 250 ms (the median of ten)", `click to frame ${sorted.map((f) => Math.round(f)).join(", ")} ms (p95 ${Math.round(p95)}); in the engine ${engineMs.map((e: number) => Math.round(e)).join(", ")} ms`);
    // A shared link of the session replays to the same drawing.
    const here = await graphInfo(r);
    await type(".share", r);
    const link = r.url();
    const again = await rctx.newPage();
    again.on("pageerror", (e) => failures.push(`replay page error: ${e.message}`));
    await again.goto(link);
    await again.waitForFunction(() => (window as any).ursaShell?.screen().includes("? [y/N] "), null, { timeout: 120_000 });
    await again.keyboard.press("y");
    await idle(again);
    await graphSettled(again);
    const there = await graphInfo(again);
    check(link.includes("#run=") && there.nodes === here.nodes && there.edges === here.edges, "rmat: a shared link of the session replays to the same node and edge counts", `${here.nodes} nodes, ${here.edges} edges | replayed ${there.nodes}, ${there.edges}`);
    await again.close();
    await r.keyboard.press("Escape");
  }
  await rctx.close();
}
// #502: saved positions, and the edge cap as a setting. rmat's million edges
// are laid out once (layout_fa2) and drawn from that with no layout: the
// picture is the live layout's end state, read back; its first frame comes
// well within 2 s of the engine's answer; nodes with no saved position are
// placed; a drawing at saved positions still grows; the cap's refusal names
// ursa.graph_max_edges; and a drawing of more than two million edges says how
// to save its positions.
{
  const sctx = await watchedContext({ viewport: { width: 1280, height: 800 } });
  const s = await sctx.newPage();
  await s.goto(`${base}/?dataset=rmat`);
  await ready(s);
  await idle(s);
  await s.click("#terminal");
  const status = () => s.evaluate(() => document.getElementById("graph-status")!.textContent ?? "");
  await type("SHOW ursa.graph_max_edges;", s);
  check(lastTable(await after("SHOW ursa.graph_max_edges;", s)).rows[0]?.[1] === "5000000", "the edge cap is ursa.graph_max_edges, 5,000,000 on a desktop", await after("SHOW ursa.graph_max_edges;", s));

  // The live layout, to its end: each node's place by its key, centred on
  // the origin as layout_fa2 centres what it returns (in f32, in node
  // order: ursa-core's `center`), and the frame's inked pixels.
  await type(".graph rmat", s);
  await graphSettled(s);
  await s.evaluate(() => {
    const w = window as any;
    const g = w.ursaShell.graphPlacement();
    const n = g.ids.length;
    const f = Math.fround;
    let [sx, sy] = [0, 0];
    for (let i = 0; i < n; i++) {
      sx = f(sx + g.xy[i]);
      sy = f(sy + g.xy[n + i]);
    }
    const [mx, my] = [f(sx / f(n)), f(sy / f(n))];
    w.__live = new Map();
    for (let i = 0; i < n; i++) w.__live.set(g.ids[i], [f(g.xy[i] - mx), f(g.xy[n + i] - my)]);
  });
  // Laid out, rmat is drawn aggregated (#498), and saved positions have no
  // hierarchy to aggregate: compare the frames in full detail.
  await type(".graph detail on", s);
  await s.waitForFunction(() => { const g = (window as any).ursaShell.graph(); return g && g.cut === null && !g.cuts.waiting; }, null, { timeout: 30_000, polling: 50 });
  const inkLive = await s.evaluate(() => (window as any).ursaShell.graphInked());

  await type("CREATE TABLE pos AS SELECT * FROM layout_fa2(rmat);", s);
  await s.evaluate(() => {
    const w = window as any;
    const el = document.getElementById("graph-status")!;
    w.__timed = [];
    new MutationObserver(() => w.__timed.push(el.textContent)).observe(el, { childList: true, characterData: true, subtree: true });
  });
  await type(".graph rmat POSITIONS (SELECT * FROM pos)", s);
  await graphSettled(s);
  const saved = await graphInfo(s);
  const times = await s.evaluate(() => (window as any).ursaShell.graphTimings());
  const seen: string[] = await s.evaluate(() => (window as any).__timed);
  check(
    saved.saved === true && saved.layoutDone && saved.iterations === 0 && (await status()).includes(" · saved positions · Done") && !seen.some((t) => t.includes("Laying out")),
    "saved positions: rmat is drawn where POSITIONS puts it, and no layout runs",
    `${await status()}; ${JSON.stringify({ saved: saved.saved, iterations: saved.iterations })}`,
  );
  check((await after(".graph rmat POSITIONS", s)).replace(/\n/g, "").includes("at its saved positions"), "saved positions: the terminal says so", await after(".graph rmat POSITIONS", s));
  const firstFrame = times.firstDraw - times.engine;
  check(firstFrame > 0 && firstFrame <= 2_000, "saved positions: the first frame comes within 2 s of the engine's answer", `${Math.round(firstFrame)} ms after the engine (${Math.round(times.engine)} ms), ${Math.round(times.firstDraw)} ms after Enter`);
  const same = await s.evaluate(() => {
    const w = window as any;
    const g = w.ursaShell.graphPlacement();
    const n = g.ids.length;
    let [same, differ, missing, worst] = [0, 0, 0, 0];
    for (let i = 0; i < n; i++) {
      const a = w.__live.get(g.ids[i]);
      if (!a) missing++;
      else if (Object.is(a[0], g.xy[i]) && Object.is(a[1], g.xy[n + i])) same++;
      else {
        differ++;
        worst = Math.max(worst, Math.abs(a[0] - g.xy[i]), Math.abs(a[1] - g.xy[n + i]));
      }
    }
    return { nodes: n, live: w.__live.size, same, differ, missing, worst };
  });
  check(same.nodes === 46_318 && same.same === same.nodes && same.live === same.nodes, "saved positions: layout_fa2's positions are the live layout's end state, node for node, to the bit", JSON.stringify(same));
  const inkSaved = await s.evaluate(() => (window as any).ursaShell.graphInked());
  check(inkLive.inked > 10_000 && JSON.stringify(inkSaved) === JSON.stringify(inkLive), "saved positions: the frame read back is the live layout's, pixel count for pixel count", `${JSON.stringify(inkLive)} | ${JSON.stringify(inkSaved)}`);
  await type(".graph detail off", s);
  // The edges' order still comes (by degree: there is no hierarchy), and a
  // frame with every edge.
  const ordered = await s.waitForFunction(() => { const f = (window as any).ursaShell.graph()?.frame; return f?.ordered === 1_000_000 && f.partial === false; }, null, { timeout: 60_000, polling: 100 }).then(() => true, () => false);
  check(ordered, "saved positions: the edges are ordered for the frame budget, and a frame has them all", JSON.stringify((await graphInfo(s)).frame));

  // A tenth of the nodes have no saved position: placed, and said.
  await type(".graph rmat POSITIONS (SELECT * FROM pos WHERE id % 10 <> 0)", s);
  await graphSettled(s);
  const partial = await s.evaluate(() => {
    const w = window as any;
    const g = w.ursaShell.graphPlacement();
    const n = g.ids.length;
    let [kept, moved, bad] = [0, 0, 0];
    for (let i = 0; i < n; i++) {
      const a = w.__live.get(g.ids[i]);
      if (!Number.isFinite(g.xy[i]) || !Number.isFinite(g.xy[n + i])) bad++;
      else if (Object.is(a[0], g.xy[i]) && Object.is(a[1], g.xy[n + i])) kept++;
      else moved++;
    }
    return { kept, moved, bad };
  });
  const missing = Number(((await status()).match(/ · ([\d,]+) of 46,318 nodes had no saved position and were placed beside their neighbours · /)?.[1] ?? "0").replace(/,/g, ""));
  check(missing > 4_000 && partial.bad === 0 && partial.kept === 46_318 - missing && partial.moved === missing, "saved positions: the nodes without one are placed beside their neighbours, and the others do not move", `${await status()}; ${JSON.stringify(partial)}`);

  // The positions as a file (#517): COPY writes it in the session, and the
  // drawing from it is the drawing from the table.
  await type("COPY (SELECT * FROM layout_fa2(rmat)) TO 'pos.parquet';", s);
  const wrote = lastTable(await after("COPY (SELECT * FROM layout_fa2(rmat))", s)).rows[0]?.[0];
  await type(".graph rmat POSITIONS (SELECT * FROM 'pos.parquet')", s);
  await graphSettled(s);
  const fromFile = await s.evaluate(() => {
    const w = window as any;
    const g = w.ursaShell.graphPlacement();
    const n = g.ids.length;
    let kept = 0;
    for (let i = 0; i < n; i++) {
      const a = w.__live.get(g.ids[i]);
      if (Object.is(a[0], g.xy[i]) && Object.is(a[1], g.xy[n + i])) kept++;
    }
    return { n, kept };
  });
  const filed = await graphInfo(s);
  check(
    wrote === "46318" && filed.saved === true && filed.iterations === 0 && (await status()).includes(" · saved positions · Done") && fromFile.n === 46_318 && fromFile.kept === 46_318,
    "saved positions: COPY … TO 'pos.parquet' saves a layout, and .graph … POSITIONS (SELECT * FROM 'pos.parquet') draws from it",
    `${wrote} rows; ${await status()}; ${JSON.stringify(fromFile)}`,
  );

  // A drawing at saved positions grows as any other: the layout worker is
  // at them, and places the new nodes.
  await type(".graph SELECT src, dst FROM rmat_edges LIMIT 300 ON rmat POSITIONS (SELECT * FROM pos)", s);
  await graphSettled(s);
  const small = await graphInfo(s);
  await type(".expand 49956", s);
  await s.waitForFunction((n) => { const g = (window as any).ursaShell.graph(); return g && g.nodes > n && g.layoutDone && g.epoch > 0; }, small.nodes, { timeout: 60_000, polling: 100 }).catch(() => {});
  const grown = await graphInfo(s);
  check(small.saved && grown.nodes === small.nodes + 100 && grown.layoutDone && grown.epoch === 1, "saved positions: .expand adds nodes to the drawing, placed by the layout", `${small.nodes} nodes, then ${grown.nodes}; ${JSON.stringify({ epoch: grown.epoch, done: grown.layoutDone })}`);

  // The cap: a SET, and a refusal that names it and leaves the drawing.
  await type("SET ursa.graph_max_edges = 500000;", s);
  await type(".graph rmat", s);
  const refused = (await after(".graph rmat", s)).replace(/\n/g, "");
  check(
    refused.includes("1,000,000 edges; the graph view draws at most 500,000 (ursa.graph_max_edges)") && refused.includes("SET ursa.graph_max_edges = 1000000") && (await graphInfo(s)).nodes === grown.nodes,
    "the edge cap: a graph over ursa.graph_max_edges is refused, by name, and the drawing stays",
    refused,
  );
  await type("RESET ursa.graph_max_edges;", s);

  // Over two million edges, laid out: the status line and the terminal
  // offer saved positions. (Few nodes, so the layout itself is quick.)
  await type("CREATE TABLE big AS SELECT i % 3000 AS src, (i % 3000 + 1 + (i / 3000) % 5) % 3000 AS dst FROM (SELECT unnest(range(0, 2100000)) AS i);", s);
  await s.evaluate(() => ((window as any).__timed.length = 0));
  await type(".graph big", s);
  await graphSettled(s);
  const offered: string[] = await s.evaluate(() => (window as any).__timed);
  const told = (await after(".graph big", s)).replace(/\n/g, "");
  check(
    offered.some((t) => / · Laying out · .* · the fine level takes about a minute; saved positions draw at once: COPY \(SELECT \* FROM layout_fa2\(g\)\) TO 'pos\.parquet'$/.test(t)) && (await status()).endsWith(" · Done"),
    "over two million edges: the status line offers saved positions while the layout runs",
    offered.find((t) => t.includes("Laying out")) ?? (await status()),
  );
  const [copyAt, tableAt] = [told.indexOf("COPY (SELECT * FROM layout_fa2(g)) TO 'pos.parquet';"), told.indexOf("CREATE TABLE pos AS SELECT * FROM layout_fa2(g);")];
  check(copyAt >= 0 && told.includes(".graph g POSITIONS (SELECT * FROM 'pos.parquet')") && tableAt > copyAt && told.includes(".graph g POSITIONS (SELECT * FROM pos)"), "over two million edges: the terminal says how to save the positions, COPY first", told);
  await sctx.close();
}
// #499: GROUP BY. `.graph karate GROUP BY club` draws a disc for each club
// and the ties between the two, counted as SQL counts them; a double-click
// opens a group, its members inside its disc; .undo is the closed drawing
// again, to the pixel (a read-back of the frame); and .collapse closes one.
{
  const gctx = await watchedContext({ viewport: { width: 1280, height: 800 } });
  const k = await gctx.newPage();
  await k.goto(`${base}/?dataset=karate`);
  await ready(k);
  await idle(k);
  await k.click("#terminal");
  const status = () => k.evaluate(() => document.getElementById("graph-status")!.textContent ?? "");
  const ink = () => k.evaluate(() => (window as any).ursaShell.graphInked());
  const openIs = (groups: number[]) =>
    k.waitForFunction((want) => { const g = (window as any).ursaShell.graph(); return g?.grouped && g.grouped.open.join() === want && !(window as any).ursaShell.busy(); }, groups.join(), { timeout: 30_000, polling: 50 }).catch(() => {});
  // What SQL says of the clubs: their sizes, and the ties between them.
  await type("SELECT club, count(*) AS n FROM members GROUP BY club ORDER BY n DESC, club;", k);
  const clubs = lastTable(await screen(k)).rows;
  await type("SELECT count(*) AS n FROM ties t JOIN members a ON a.id = t.src JOIN members b ON b.id = t.dst WHERE a.club <> b.club;", k);
  const between = lastTable(await screen(k)).rows[0]?.[0];
  await type("SELECT a.club, count(*) AS n FROM ties t JOIN members a ON a.id = t.src JOIN members b ON b.id = t.dst WHERE a.club = b.club GROUP BY a.club ORDER BY a.club;", k);
  const inside = Object.fromEntries(lastTable(await screen(k)).rows);

  await type(".graph karate GROUP BY club", k);
  await graphSettled(k);
  let g = await graphInfo(k);
  const said = (await after(".graph karate GROUP BY club", k)).replace(/\n/g, "");
  check(
    g.grouped?.groups === 2 && g.grouped.total === 34 && g.nodes === 2 && g.edges === 1 && g.cut?.groups === 2 && g.cut.groupsDrawn === 2 && g.cut.edgesDrawn === 1,
    "GROUP BY: karate by club is two discs and the one edge between them",
    JSON.stringify({ grouped: g.grouped && { groups: g.grouped.groups, total: g.grouped.total }, cut: g.cut }),
  );
  check(
    clubs.length === 2 && g.groupLabels.includes(`${clubs[0][0]} · ${clubs[0][1]}`) && g.groupLabels.includes(`${clubs[1][0]} · ${clubs[1][1]}`) && Number(between) > 0 && g.groupLabels.includes(String(between)),
    "GROUP BY: each disc says its club and its members, and the edge its ties, as SQL's GROUP BY counts them",
    `${JSON.stringify(g.groupLabels)}; SQL: ${JSON.stringify(clubs)}, ${between} between`,
  );
  const legend = await k.evaluate(() => document.getElementById("graph-legend")!.textContent ?? "");
  check(
    /^grouped by club: 2 groups of 34 nodes · drawn as groups of karate · Done$/.test(await status()) && legend.startsWith("grouped by club: 2 groups") && said.includes("Drawing groups of karate: 2 groups of 34 nodes by club, with 1 edge between groups"),
    "GROUP BY: the status line, the legend and the terminal say grouped by club: 2 groups",
    `${await status()} | ${legend} | ${said}`,
  );
  // The frame, read back: two discs of the first two colours, each the
  // size 17 of 34 nodes gives (3% of the canvas), and neither over the other.
  const closed = await ink();
  const disc = Math.min(0.12 * Math.min(closed.width, closed.height), Math.sqrt((0.06 * closed.width * closed.height * 0.5) / Math.PI));
  const area = Math.PI * disc * disc;
  const apart = Math.hypot(g.grouped.at[0][0] - g.grouped.at[1][0], g.grouped.at[0][1] - g.grouped.at[1][1]);
  check(
    Math.abs(closed.palette[0] - area) < 0.1 * area && Math.abs(closed.palette[1] - area) < 0.1 * area && apart > 2 * disc,
    "GROUP BY: the frame read back has the two clubs' discs, each its share of the canvas, apart",
    `${closed.palette[0]} and ${closed.palette[1]} px for ${Math.round(area)}; centres ${Math.round(apart)} px apart, radius ${Math.round(disc)}`,
  );

  // A double-click on the first club's disc opens it.
  const box = (await k.locator("#graph-stage canvas").boundingBox())!;
  const ratio = g.width / box.width;
  const at = (i: number) => [box.x + g.grouped.at[i][0] / ratio, box.y + g.grouped.at[i][1] / ratio] as const;
  await k.mouse.dblclick(...at(0));
  await openIs([0]);
  await idle(k);
  g = await graphInfo(k);
  const within = (info: any, group: number) => {
    const reach = info.grouped.radii[group] * info.camera[2];
    const [cx, cy] = info.grouped.at[group];
    const members = info.grouped.at.slice(info.grouped.groups).filter((m: number[]) => m[2] === group);
    return { members: members.length, outside: members.filter((m: number[]) => Math.hypot(m[0] - cx, m[1] - cy) > reach + 0.5).length, reach };
  };
  const first = within(g, 0);
  const opened = (await screen(k)).replace(/\n/g, "");
  check(
    g.grouped.open.join() === "0" && g.slots === 2 + Number(clubs[0][1]) && first.members === Number(clubs[0][1]) && first.outside === 0 && Math.abs(first.reach - disc) < 1,
    "GROUP BY: a double-click opens a group: its members are in its disc, which keeps its size",
    `${first.members} members, ${first.outside} outside a disc of ${first.reach.toFixed(1)} px (closed: ${disc.toFixed(1)}); open ${g.grouped.open}`,
  );
  check(
    opened.includes(".expand group:0") && opened.includes(`+${clubs[0][1]} nodes, +${inside[clubs[0][0]]} edges`) && g.edges === 1 + Number(inside[clubs[0][0]]) && (await status()).includes(" · 1 open"),
    "GROUP BY: opening ran .expand group:0, which added the club's members and the ties among them, as SQL counts them",
    `${opened.slice(-160)}; ${g.edges} edges; ${await status()}`,
  );
  const open = await ink();
  check(open.inked > 0 && open.palette[0] > 17 * 4 && open.palette[0] < 0.5 * closed.palette[0] && Math.abs(open.palette[1] - closed.palette[1]) < 0.02 * area, "GROUP BY: read back, the open disc is a rim with its members inside, and the other is as it was", `${JSON.stringify(open.palette.slice(0, 2))} after ${JSON.stringify(closed.palette.slice(0, 2))}`);
  if (shots) await k.screenshot({ path: `${shots}/graph-group-open.png` });

  // .undo is the closed drawing again: the same frame, to the pixel count.
  await k.click("#terminal");
  await type(".undo", k);
  await openIs([]);
  // (Nothing selected, as when the closed frame was read: no ring.) No
  // frame is drawn between the click and the read-back: a read-back is of
  // the view as it is, not of its last frame, whose discs had the ring.
  // (Under load the frame came after the read-back, and the ring with it.)
  await k.evaluate(() => {
    (window as any).__raf = window.requestAnimationFrame;
    window.requestAnimationFrame = () => 0;
  });
  await k.mouse.click(box.x + 6, box.y + 6);
  g = await graphInfo(k);
  const undone = await ink();
  // Frames again, and one drawn (a read-back asks for one when it ends).
  await k.evaluate(async () => {
    window.requestAnimationFrame = (window as any).__raf;
    await (window as any).ursaShell.graphInked();
  });
  check(
    g.grouped.open.length === 0 && g.slots === 2 && g.nodes === 2 && g.edges === 1 && undone.inked === closed.inked && undone.palette.join() === closed.palette.join(),
    "GROUP BY: .undo restores the closed drawing, read back",
    `${undone.inked} px inked for ${closed.inked}; ${JSON.stringify(undone.palette.slice(0, 2))}`,
  );

  // Both open: every tie is drawn. .collapse closes one, and opening it
  // again needs no query.
  await k.click("#terminal");
  await type(".expand group:0", k);
  await type(".expand group:1", k);
  await openIs([0, 1]);
  g = await graphInfo(k);
  const both = [within(g, 0), within(g, 1)];
  await type("SELECT count(*) AS n FROM ties;", k);
  const ties = Number(lastTable(await screen(k)).rows[0]?.[0]);
  check(g.nodes === 36 && g.edges === 1 + ties && both.every((b) => b.members === 17 && b.outside === 0), "GROUP BY: with both clubs open every member is in its own club's disc, and every tie is drawn", `${g.nodes} nodes, ${g.edges} edges for ${ties} ties; ${JSON.stringify(both)}`);
  await type(".collapse group:0", k);
  await openIs([1]);
  g = await graphInfo(k);
  const collapsed = (await after(".collapse group:0", k)).replace(/\n/g, "");
  check(g.grouped.open.join() === "1" && g.nodes === 19 && collapsed.includes("-17 nodes"), ".collapse group:0 closes the group", `${collapsed}; open ${g.grouped.open}; ${g.nodes} nodes`);

  // A click selects a group, and the status says what it is.
  await k.mouse.click(...at(0));
  g = await graphInfo(k);
  const selected = await status();
  check(
    g.selection.length === 1 && g.selection[0].index === 0 && selected.startsWith(`Selected group:0: club ${clubs[0][0]}, members ${clubs[0][1]}, edges inside ${inside[clubs[0][0]]} · grouped by club: 2 groups of 34 nodes`),
    "GROUP BY: a click selects a group, and the status says its club, its members and the ties inside it",
    selected,
  );
  await k.keyboard.press("Enter");
  await openIs([0, 1]);
  check((await graphInfo(k)).grouped.open.join() === "0,1", "GROUP BY: Enter opens the selected group", JSON.stringify((await graphInfo(k)).grouped.open));

  // What does not go with GROUP BY is refused, and the drawing stays; an
  // algorithm groups too, and a query's rows, in parentheses.
  await k.click("#terminal");
  await type(".graph karate GROUP BY club SIZE BY pagerank", k);
  const refused = (await after(".graph karate GROUP BY club SIZE", k)).replace(/\n/g, "");
  check(refused.includes("SIZE BY pagerank is of the nodes") && (await graphInfo(k)).grouped?.by === "club", "GROUP BY with SIZE BY is refused, and says why", refused);
  await type("SELECT count(DISTINCT louvain) AS n FROM louvain(karate, seed => 1);", k);
  const communities = Number(lastTable(await screen(k)).rows[0]?.[0]);
  await type(".graph karate GROUP BY louvain", k);
  await graphSettled(k);
  g = await graphInfo(k);
  check(communities > 2 && g.grouped?.by === "louvain" && g.grouped.groups === communities && g.cut.nodes === 34 && (await status()).startsWith(`grouped by louvain: ${communities} groups of 34 nodes`), "GROUP BY louvain: a disc for each community louvain finds", `${g.grouped?.groups} groups for ${communities} communities; ${await status()}`);
  await type(".graph (SELECT * FROM louvain(karate, seed => 1)) GROUP BY louvain", k);
  await graphSettled(k);
  g = await graphInfo(k);
  check(g.grouped?.groups === communities && g.cut.nodes === 34, "GROUP BY after a query in parentheses groups the query's rows", `${g.grouped?.groups} groups; ${await status()}`);
  // A read-back borrows the view until its pixels come. What reads the
  // view meanwhile still works (a call that found the view borrowed would
  // throw, and leave it unable to be freed: the next .graph then failed
  // with "attempted to take ownership of Rust value while it was borrowed").
  const during = await k.evaluate(async () => {
    const w = window as any;
    const ink = w.ursaShell.graphInked();
    const threw: string[] = [];
    for (let t = 0; t < 4; t++) {
      await new Promise((r) => setTimeout(r, 0));
      try {
        w.ursaShell.graph();
      } catch (e) {
        threw.push(String(e));
      }
    }
    return { threw, inked: (await ink)?.inked ?? 0 };
  });
  check(during.threw.length === 0 && during.inked > 0, "a read-back of the view does not keep what reads the view from it", `${during.threw[0] ?? "nothing threw"}; ${during.inked} px inked`);
  await type(".graph karate GROUP BY club", k);
  await graphSettled(k);
  check((await graphInfo(k)).grouped?.by === "club" && (await errors(k)) === 1, "after a read-back with the view read meanwhile, the next drawing replaces it", (await after(".graph karate GROUP BY club", k)).replace(/\n/g, "").slice(0, 160));
  // And the pane closed while a read-back is on its way: the view is
  // freed once the pixels have come, with no error.
  const before = failures.length;
  const shut = await k.evaluate(async () => {
    const w = window as any;
    const ink = w.ursaShell.graphInked();
    await new Promise((r) => setTimeout(r, 0));
    document.getElementById("graph-close")!.click();
    const open = w.ursaShell.graphOpen();
    return { open, inked: (await ink)?.inked ?? 0 };
  });
  await k.waitForTimeout(200);
  check(!shut.open && shut.inked > 0 && failures.length === before && !(await graphOpen(k)), "the pane closes while its view is read back: the view is freed afterwards, with no error", JSON.stringify(shut));
  await type(".graph karate GROUP BY club", k);
  await graphSettled(k);
  check((await graphInfo(k)).grouped?.groups === 2, "and it is drawn again after that", await status());
  check((await errors(k)) === 1, "GROUP BY: the one error on the screen is the refusal", `${await errors(k)} errors`);
  await gctx.close();
}
// #499 on rmat: the communities of a million edges are drawn within 2 s of
// the algorithm's result, a disc for each community of more than one node,
// and the largest opens with its members in its disc.
{
  const gctx = await watchedContext({ viewport: { width: 1280, height: 800 } });
  const r = await gctx.newPage();
  await r.goto(`${base}/?dataset=rmat`);
  await ready(r);
  await idle(r);
  await r.click("#terminal");
  await type("SELECT count(*) AS n, count(*) FILTER (WHERE members > 1) AS over_one, max(members) AS largest FROM (SELECT louvain, count(*) AS members FROM louvain(rmat, seed => 1) GROUP BY louvain);", r);
  const [found, overOne, largest] = lastTable(await screen(r)).rows[0].map(Number);
  await type(".graph rmat GROUP BY louvain", r);
  await graphSettled(r);
  let g = await graphInfo(r);
  const times = await r.evaluate(() => (window as any).ursaShell.graphTimings());
  const drawn = times.firstDraw - times.engine;
  check(drawn > 0 && drawn <= 2_000, "GROUP BY louvain on rmat is drawn within 2 s of the algorithm's result", `${Math.round(drawn)} ms after the engine's answer (${Math.round(times.engine)} ms after Enter)`);
  check(
    found > 1 && g.grouped?.groups === Math.min(found, 2000) + (found > 2000 ? 1 : 0) && g.cut.merged === overOne && g.cut.nodes === 46_318 && g.cut.groupsDrawn === g.grouped.groups,
    "GROUP BY louvain on rmat: a disc for each community, every one of more than one node among them, and the groups hold every node",
    `${g.grouped?.groups} discs, ${g.cut?.merged} of more than one node, for ${found} communities (${overOne} over one); ${g.cut?.nodes} nodes`,
  );
  await type(".expand group:0", r);
  await r.waitForFunction(() => (window as any).ursaShell.graph()?.grouped?.open.length === 1, null, { timeout: 60_000, polling: 100 }).catch(() => {});
  g = await graphInfo(r);
  const reach = g.grouped.radii[0] * g.camera[2];
  const [cx, cy] = g.grouped.at[0];
  const members = g.grouped.at.slice(g.grouped.groups).filter((m: number[]) => m[2] === 0);
  const outside = members.filter((m: number[]) => Math.hypot(m[0] - cx, m[1] - cy) > reach + 0.5).length;
  check(
    g.nodes === g.grouped.groups + largest && members.length === Math.min(largest, 5000 - g.grouped.groups) && outside === 0 && reach > 5,
    "GROUP BY on rmat: the largest group opens, its members within its disc",
    `${g.nodes - g.grouped.groups} members shown for ${largest}; ${outside} of ${members.length} outside a disc of ${reach.toFixed(1)} px`,
  );
  if (shots) await r.screenshot({ path: `${shots}/graph-group-rmat.png` });
  await type(".undo", r);
  g = await graphInfo(r);
  check(g.grouped.open.length === 0 && g.nodes === g.grouped.groups && (await errors(r)) === 0, "GROUP BY on rmat: .undo closes the group again", `${g.nodes} nodes; open ${g.grouped.open}`);
  await gctx.close();
}
// #493: the inspector's panel never keeps a node from the pointer. With the
// panel open the camera makes room for it, so a node that was under it can
// be double-clicked and expands; closing it gives the view back; and the
// panel opens at once on the side away from what was selected. On a desktop
// (a column at the right, a mouse) and a phone (a sheet across the bottom,
// fingers).
for (const phone of [false, true]) {
  const name = phone ? "a phone" : "a desktop";
  const rctx = await watchedContext(phone ? { viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true } : { viewport: { width: 1280, height: 800 } });
  const p = await rctx.newPage();
  await p.goto(`${base}/?dataset=karate`);
  await ready(p);
  await idle(p);
  await p.click("#terminal");
  await type(".graph SELECT * FROM hop(karate, seeds => [0], steps => 1, direction => 'both')", p);
  await graphSettled(p);
  await p.waitForFunction(() => (window as any).ursaShell.graph()?.notShownKnown === true, null, { timeout: 30_000, polling: 50 }).catch(() => {});
  const box = (await p.locator("#graph-stage canvas").boundingBox())!;
  type At = { id: number; x: number; y: number };
  /** Where every node drawn is, in CSS pixels of the canvas. */
  const places = (): Promise<At[]> =>
    p.evaluate(() => {
      const shell = (window as any).ursaShell;
      const out: { id: number; x: number; y: number }[] = [];
      for (let id = 0; id < 34; id++) {
        const at = shell.graphNodePlace(id);
        if (at && !at.hidden) out.push({ id, x: at.x, y: at.y });
      }
      return out;
    });
  const inspector = (): Promise<any> => p.evaluate(() => (window as any).ursaShell.graphInspector());
  const panel = () => p.locator("#graph-stage .graph-inspector").boundingBox();
  const onCanvas = (at: { x: number; y: number }) => p.evaluate(([x, y]) => document.elementFromPoint(x, y)?.tagName === "CANVAS", [box.x + at.x, box.y + at.y]);
  /** The camera has made room for the panel (`open`), or has given it back. */
  const room = (open: boolean) =>
    p
      .waitForFunction(
        (open) => {
          const r = (window as any).ursaShell.graph()?.room;
          return r && !r.waits && r.inset.some((v: number) => v > 0) === open;
        },
        open,
        { timeout: 10_000, polling: 50 },
      )
      .then(() => true, () => false);
  const cdp = phone ? await rctx.newCDPSession(p) : null;
  const touch = (type: "touchStart" | "touchMove" | "touchEnd", at: { x: number; y: number }[]) => cdp!.send("Input.dispatchTouchEvent", { type, touchPoints: at.map((f, id) => ({ x: box.x + f.x, y: box.y + f.y, id })) });
  const click = async (at: { x: number; y: number }) => {
    if (!phone) return p.mouse.click(box.x + at.x, box.y + at.y);
    await touch("touchStart", [at]);
    await touch("touchEnd", []);
  };
  const dblclick = async (at: { x: number; y: number }) => {
    if (!phone) return p.mouse.dblclick(box.x + at.x, box.y + at.y);
    await click(at);
    await p.waitForTimeout(60);
    await click(at);
  };
  const drag = async (from: { x: number; y: number }, to: { x: number; y: number }) => {
    const steps = 8;
    const at = (i: number) => ({ x: from.x + ((to.x - from.x) * i) / steps, y: from.y + ((to.y - from.y) * i) / steps });
    if (phone) {
      await touch("touchStart", [from]);
      for (let i = 1; i <= steps; i++) await touch("touchMove", [at(i)]);
      await touch("touchEnd", []);
    } else {
      await p.mouse.move(box.x + from.x, box.y + from.y);
      await p.mouse.down();
      for (let i = 1; i <= steps; i++) await p.mouse.move(box.x + at(i).x, box.y + at(i).y);
      await p.mouse.up();
    }
  };
  const esc = async () => {
    await p.focus("#graph-stage canvas");
    await p.keyboard.press("Escape");
  };
  // Along the side the panel takes: across a desktop's view, down a phone's.
  const along = (at: { x: number; y: number }) => (phone ? at.y : at.x);
  const extent = phone ? box.height : box.width;
  const within = (at: { x: number; y: number }) => at.x >= 0 && at.y >= 0 && at.x <= box.width && at.y <= box.height;
  const find = (all: At[], id: number) => all.find((n) => n.id === id)!;

  // A node to expand, drawn where the panel opens (a drag puts it there:
  // the camera no longer follows the layout), and a node to select, far
  // from that side.
  const expandable: At[] = (await p.evaluate(() => (window as any).ursaShell.graphExpandable(100))).map((n: any) => ({ id: Number(n.id), x: n.x, y: n.y }));
  const b0 = expandable.filter((n) => n.id !== 0).reduce((a, b) => (along(b) > along(a) ? b : a), expandable[0]);
  const spot = phone ? { x: b0.x, y: box.height - 40 } : { x: box.width - 40, y: 60 };
  await drag({ x: 5, y: 5 }, { x: 5 + spot.x - b0.x, y: 5 + spot.y - b0.y });
  const before = await places();
  const b = find(before, b0.id);
  const a = before.filter((n) => n.id !== b.id && n.x > 20 && n.y > 20 && n.x < box.width - 20 && n.y < box.height - 20).reduce((a, b) => (along(b) < along(a) ? b : a));
  check(Math.hypot(b.x - spot.x, b.y - spot.y) < 3 && along(a) < extent / 2 && !(await graphInfo(p)).room.following, `#493, ${name}: a node to expand is drawn where the panel opens, and one to select away from it`, `node ${b?.id} at ${Math.round(b.x)},${Math.round(b.y)}, node ${a?.id} at ${Math.round(a.x)},${Math.round(a.y)}, of ${Math.round(box.width)} x ${Math.round(box.height)}`);

  // Selecting opens the panel at once, over the first node; then the
  // camera makes room, and no node in sight is under the panel.
  await click(a);
  const opened = await inspector();
  check(await room(true), `#493, ${name}: the camera makes room for the panel`, JSON.stringify((await graphInfo(p)).room));
  const over = (await panel())!;
  const wasUnder = box.x + b.x > over.x && box.x + b.x < over.x + over.width && box.y + b.y > over.y && box.y + b.y < over.y + over.height;
  const made = await places();
  const clearNow = await Promise.all(made.filter(within).map(onCanvas));
  check(
    opened?.kind === "node" && opened.side === "right" && wasUnder && before.filter(within).every((n) => within(find(made, n.id))) && clearNow.length >= before.filter(within).length && clearNow.every(Boolean),
    `#493, ${name}: every node that was in sight is in sight still, and none is under the panel`,
    `side ${opened?.side}; node ${b.id} was at ${Math.round(b.x)},${Math.round(b.y)}, the panel at ${JSON.stringify(over)}; ${clearNow.filter(Boolean).length} of the ${clearNow.length} in sight clear`,
  );
  if (shots) await p.screenshot({ path: `${shots}/graph-inspector-room-${phone ? "phone" : "desktop"}.png` });

  // Closing the panel gives the view back as it was.
  await esc();
  const back = await room(false);
  const restored = await places();
  const moved = Math.max(...before.map((n) => Math.hypot(find(restored, n.id).x - n.x, find(restored, n.id).y - n.y)));
  check(back && (await inspector()) === null && moved < 1, `#493, ${name}: closing the panel puts the view back where it was`, `${moved.toFixed(2)} px`);

  // Under prefers-reduced-motion the camera moves in one step; otherwise over several frames.
  const steps = async (reduced: boolean) => {
    await p.emulateMedia({ reducedMotion: reduced ? "reduce" : "no-preference" });
    await p.evaluate(() => {
      const w = window as any;
      w.__room = [];
      const look = () => {
        const now = JSON.stringify(w.ursaShell.graph()?.room.inset);
        if (w.__room.at(-1) !== now) w.__room.push(now);
      };
      look();
      w.__look = setInterval(look, 5);
    });
    await click(find(await places(), a.id));
    await room(true);
    const seen: string[] = await p.evaluate(() => {
      clearInterval((window as any).__look);
      return (window as any).__room;
    });
    return seen.length - 1;
  };
  const eased = await steps(false);
  await esc();
  await room(false);
  const atOnce = await steps(true);
  await p.emulateMedia({ reducedMotion: "no-preference" });
  check(eased > 2 && atOnce === 1, `#493, ${name}: the camera eases to make room, and moves in one step under prefers-reduced-motion`, `${eased} steps, then ${atOnce}`);

  // The acceptance: with the panel open, a double-click on the node that
  // was under it expands it.
  const nodes0 = (await graphInfo(p)).nodes;
  const target = find(await places(), b.id);
  const reached = await onCanvas(target);
  await dblclick(target);
  await idle(p);
  await graphSettled(p);
  const grown = await graphInfo(p);
  const said = (await screen(p)).replace(/\s+/g, "");
  check(
    reached && grown.nodes > nodes0 && said.includes(`fromthegraphmemoryU.expand${b.id}`) && (await inspector())?.title === `node ${b.id}`,
    `#493, ${name}: with the panel open, a double-click on a node that was under it expands the node`,
    `${nodes0} -> ${grown.nodes} nodes; inspecting ${(await inspector())?.title}`,
  );

  // A fit is of the part the panel leaves clear, and follows the layout there.
  await room(true);
  await p.focus("#graph-stage canvas");
  await p.keyboard.press("0");
  await p.waitForTimeout(100);
  const fit = await places();
  const fitClear = await Promise.all(fit.map(onCanvas));
  const info = await graphInfo(p);
  check(info.room.following && fit.every(within) && fitClear.every(Boolean), `#493, ${name}: 0 fits the graph in the part of the view the panel leaves clear`, `${fitClear.filter(Boolean).length} of ${fit.length} clear; ${JSON.stringify(info.room)}`);
  // The panel is still reached from the keys: i goes in, Esc comes back.
  await p.keyboard.press("i");
  const inPanel = (await inspector())?.focused;
  await p.keyboard.press("Escape");
  check(inPanel === true && (await p.evaluate(() => document.activeElement?.tagName)) === "CANVAS" && (await inspector()) !== null, `#493, ${name}: i still moves into the panel, and Esc back to the view`);

  // The panel takes the other side when what is selected is drawn under
  // its own: a second click or tap reaches the node at once.
  await esc();
  await room(false);
  const last = (await places()).filter(within).reduce((a, b) => (along(b) > along(a) ? b : a));
  await drag({ x: 5, y: 5 }, phone ? { x: 5, y: 5 + extent - 40 - along(last) } : { x: 5 + extent - 40 - along(last), y: 5 });
  const low = find(await places(), last.id);
  await click(low);
  const flipped = await inspector();
  check(flipped?.side === "left" && (await onCanvas(low)), `#493, ${name}: the panel opens on the other side when the node selected is drawn under its own`, `side ${flipped?.side}, node ${low.id} at ${Math.round(along(low))} of ${Math.round(extent)}`);
  check((await errors(p)) === 0, `#493, ${name}: no error`, `${await errors(p)}`);
  await rctx.close();
}
// #502: a phone (a coarse pointer and at most 4 GB of memory) starts at
// 500,000 edges, and its refusal names the setting.
{
  const pctx = await watchedContext({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true });
  await pctx.addInitScript(() => Object.defineProperty(Navigator.prototype, "deviceMemory", { get: () => 4, configurable: true }));
  const ph = await pctx.newPage();
  await ph.goto(`${base}/?dataset=rmat`);
  await ready(ph);
  await idle(ph);
  await ph.click("#terminal");
  const coarse = await ph.evaluate(() => matchMedia("(pointer: coarse)").matches);
  await type("SHOW ursa.graph_max_edges;", ph);
  await type(".graph rmat", ph);
  const text = (await screen(ph)).replace(/\n/g, "");
  check(coarse && text.includes("500000") && text.includes("draws at most 500,000 (ursa.graph_max_edges)"), "a phone: the edge cap starts at 500,000, and rmat's million edges are refused by its name", text.slice(-400));
  await pctx.close();
}

// A phone: the pane goes under the terminal, which refits to its new height.
{
  const pctx = await watchedContext({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true });
  const p = await pctx.newPage();
  await p.goto(`${base}/?dataset=karate`);
  await ready(p);
  await idle(p);
  const rows0: number = await p.evaluate(() => (window as any).ursaShell.rows());
  await p.click("#terminal");
  await type(".graph karate", p);
  await graphSettled(p);
  const pane = (await p.locator("#graph-pane").boundingBox())!;
  const termBox = (await p.locator("#terminal").boundingBox())!;
  const rows1: number = await p.evaluate(() => (window as any).ursaShell.rows());
  const bar = (await p.locator("header.bar").boundingBox())!;
  check(pane.y >= termBox.y + termBox.height - 1 && pane.width > 380 && rows1 < rows0 && bar.height <= 56, "a phone: the graph view is stacked under the terminal", `terminal ${rows0} → ${rows1} rows, pane at ${pane.y}px`);
  if (shots) await p.screenshot({ path: `${shots}/graph-stacked-phone.png` });
  await pctx.close();
}

// #428: a phone zooms the graph. Two fingers pinch (zoom by the change in
// their distance, around the point between them, and pan as it moves), one
// drags, a tap names a node and a double tap fits. Touches are Chromium's own
// (CDP Input.dispatchTouchEvent); where each node is drawn is read back from
// the view's picking, before and after.
{
  const zctx = await watchedContext({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 3, isMobile: true, hasTouch: true });
  const p = await zctx.newPage();
  await p.goto(`${base}/?dataset=karate`);
  await ready(p);
  await idle(p);
  await p.click("#terminal");
  await type(".graph karate", p);
  await graphSettled(p);
  const box = (await p.locator("#graph-stage canvas").boundingBox())!;
  const canvasWidth: number = (await graphInfo(p)).width;
  // Canvas pixels for each CSS pixel: 2, though the screen has 3.
  const ratio = canvasWidth / box.width;
  check(Math.abs(ratio - 2) < 0.01, "a phone with 3 pixels a CSS pixel draws 2: the canvas's pixel ratio is capped", `${canvasWidth} px for ${box.width} CSS px`);
  type Spot = { x: number; y: number; round: boolean };
  /** Where each node is drawn, in CSS pixels of the canvas: the middle of the pixels that pick it; `round` if its whole disc is seen (no node over it, not at an edge). */
  const spots = (): Promise<Record<number, Spot>> =>
    p.evaluate((ratio) => {
      const shell = (window as any).ursaShell;
      const { width, height } = shell.graph();
      const seen: Record<number, { n: number; sx: number; sy: number; x0: number; x1: number; y0: number; y1: number }> = {};
      for (let y = 0; y < height; y++)
        for (let x = 0; x < width; x++) {
          const node = shell.graphNodeAt(x + 0.5, y + 0.5);
          if (node === null) continue;
          const s = (seen[node] ??= { n: 0, sx: 0, sy: 0, x0: x, x1: x, y0: y, y1: y });
          s.n++;
          s.sx += x + 0.5;
          s.sy += y + 0.5;
          s.x0 = Math.min(s.x0, x);
          s.x1 = Math.max(s.x1, x + 1);
          s.y0 = Math.min(s.y0, y);
          s.y1 = Math.max(s.y1, y + 1);
        }
      const out: Record<number, { x: number; y: number; round: boolean }> = {};
      for (const [node, s] of Object.entries(seen)) {
        const [w, h] = [s.x1 - s.x0, s.y1 - s.y0];
        const round = s.x0 > 0 && s.y0 > 0 && s.x1 < width && s.y1 < height && Math.abs(w - h) <= 1 && s.n > 0.74 * w * h && Math.abs(s.sx / s.n - (s.x0 + s.x1) / 2) < 0.75 && Math.abs(s.sy / s.n - (s.y0 + s.y1) / 2) < 0.75;
        out[Number(node)] = { x: s.sx / s.n / ratio, y: s.sy / s.n / ratio, round };
      }
      return out;
    }, ratio);
  const cdp = await zctx.newCDPSession(p);
  type Finger = { x: number; y: number };
  const touch = (type: "touchStart" | "touchMove" | "touchEnd", fingers: Finger[]) =>
    cdp.send("Input.dispatchTouchEvent", { type, touchPoints: fingers.map((f, id) => ({ x: box.x + f.x, y: box.y + f.y, id })) });
  /** Fingers down at `from`, moved in steps to `to`, and lifted. */
  async function gesture(from: Finger[], to: Finger[], steps = 12) {
    await touch("touchStart", from);
    for (let i = 1; i <= steps; i++) await touch("touchMove", from.map((f, k) => ({ x: f.x + ((to[k].x - f.x) * i) / steps, y: f.y + ((to[k].y - f.y) * i) / steps })));
    await touch("touchEnd", []);
  }
  const tap = async (at: Finger) => {
    await touch("touchStart", [at]);
    await touch("touchEnd", []);
  };
  const mid = ([a, b]: Finger[]) => ({ x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 });
  const gap = ([a, b]: Finger[]) => Math.hypot(a.x - b.x, a.y - b.y);
  /** How far, in CSS pixels, the nodes `of` are from where a zoom by `factor` that takes `m0` to `m1` puts them. */
  const off = (before: Record<number, Spot>, now: Record<number, Spot>, of: number[], m0: Finger, m1: Finger, factor: number) =>
    Math.max(...of.map((n) => (now[n]?.round ? Math.hypot(now[n].x - (m1.x + factor * (before[n].x - m0.x)), now[n].y - (m1.y + factor * (before[n].y - m0.y))) : Infinity)));

  const fitted = await spots();
  const middle = { x: box.width / 2, y: box.height / 2 };
  // The node nearest the middle, and the one furthest from it that a zoom by 1.8 around it keeps in view.
  const round = Object.entries(fitted).filter(([, s]) => s.round).map(([n, s]) => ({ n: Number(n), ...s }));
  const a = round.toSorted((u, v) => Math.hypot(u.x - middle.x, u.y - middle.y) - Math.hypot(v.x - middle.x, v.y - middle.y))[0];
  const stays = (s: Spot) => {
    const [x, y] = [a.x + 30 + 1.8 * (s.x - a.x), a.y + 20 + 1.8 * (s.y - a.y)];
    return x > 24 && y > 24 && x < box.width - 24 && y < box.height - 24;
  };
  const b = a && round.filter((s) => s.n !== a.n && stays(s)).toSorted((u, v) => Math.hypot(v.x - a.x, v.y - a.y) - Math.hypot(u.x - a.x, u.y - a.y))[0];
  check(!!a && !!b && Math.hypot(b.x - a.x, b.y - a.y) > 40, "two nodes to watch through a pinch", `${round.length} whole discs of ${Object.keys(fitted).length} nodes; ${a?.n} and ${b?.n}`);
  if (a && b) {
    // Fingers either side of node a spread from 100 to 180 CSS pixels apart
    // (a zoom by 1.8) as the point between them moves 30 right and 20 down.
    const from = [{ x: a.x - 50, y: a.y }, { x: a.x + 50, y: a.y }];
    const to = [{ x: a.x + 30 - 90, y: a.y + 20 }, { x: a.x + 30 + 90, y: a.y + 20 }];
    await gesture(from, to);
    const pinched = await spots();
    const scale = Math.hypot(pinched[b.n].x - pinched[a.n].x, pinched[b.n].y - pinched[a.n].y) / Math.hypot(b.x - a.x, b.y - a.y);
    check(Math.abs(scale - 1.8) < 0.03, "a pinch zooms: two fingers spread to 1.8 times apart scale the drawing by 1.8", `${scale.toFixed(3)}`);
    const slip = Math.hypot(pinched[a.n].x - mid(to).x, pinched[a.n].y - mid(to).y);
    check(slip <= 2, "the point between the fingers stays between them (within 2 px)", `${slip.toFixed(2)} px`);
    check(off(fitted, pinched, [a.n, b.n], mid(from), mid(to), gap(to) / gap(from)) <= 2, "every node is where the pinch puts it", `${off(fitted, pinched, [a.n, b.n], mid(from), mid(to), 1.8).toFixed(2)} px`);
    check((await graphInfo(p)).picked === null, "a pinch names no node: the second finger cancels the tap");

    // One finger held on node a while the other moves in: a zoom out around
    // the moving point between them, and still no tap, on a node or not.
    const from2 = [{ x: pinched[a.n].x, y: pinched[a.n].y }, { x: pinched[a.n].x + 150, y: pinched[a.n].y + 30 }];
    const to2 = [from2[0], { x: pinched[a.n].x + 100, y: pinched[a.n].y - 40 }];
    await gesture(from2, to2);
    const pinched2 = await spots();
    const factor2 = gap(to2) / gap(from2);
    check(factor2 < 0.75 && off(pinched, pinched2, [a.n, b.n], mid(from2), mid(to2), factor2) <= 2, "a pinch in, one finger still: zoomed out around the point between them", `factor ${factor2.toFixed(3)}, ${off(pinched, pinched2, [a.n, b.n], mid(from2), mid(to2), factor2).toFixed(2)} px off`);
    check((await graphInfo(p)).picked === null, "a finger held on a node through a pinch does not name it");

    // One finger drags: a pan.
    const start = { x: pinched2[a.n].x + 40, y: pinched2[a.n].y + 40 };
    await gesture([start], [{ x: start.x - 25, y: start.y + 35 }]);
    const panned = await spots();
    check(off(pinched2, panned, [a.n, b.n], start, { x: start.x - 25, y: start.y + 35 }, 1) <= 2 && (await graphInfo(p)).picked === null, "one finger drags the drawing with it, and names no node");

    // A tap names the node under it; a double tap fits the graph again.
    await tap(panned[a.n]);
    const tapped = (await graphInfo(p)).picked;
    check(tapped?.index === a.n && /^Selected node \S+: /.test((await p.locator("#graph-status").textContent()) ?? ""), "a tap selects the node under the finger", JSON.stringify(tapped));
    // The camera makes room for the inspector's panel, and gives it back
    // when the selection is cleared (#493).
    await roomSettled(p);
    const empty = { x: 6, y: 6 };
    await tap(empty);
    await roomSettled(p);
    check(off(pinched2, await spots(), [a.n], start, { x: start.x - 25, y: start.y + 35 }, 1) <= 2, "one tap beside the graph leaves the view where it is");
    await p.waitForTimeout(400);
    await tap(empty);
    await p.waitForTimeout(60);
    await tap(empty);
    const refitted = await spots();
    check(off(fitted, refitted, [a.n, b.n], middle, middle, 1) <= 1, "a double tap fits the graph in view again", `${off(fitted, refitted, [a.n, b.n], middle, middle, 1).toFixed(2)} px off`);
    // A double tap on a node expands it (#443): its command runs in the terminal.
    await p.waitForTimeout(400);
    await tap(refitted[a.n]);
    await p.waitForTimeout(60);
    await tap(refitted[a.n]);
    await idle(p);
    {
      const id = (await graphInfo(p)).picked?.id;
      const text = await screen(p);
      check(id !== undefined && text.replace(/\s+/g, "").includes(`fromthegraphmemoryU.expand${id}`), "a double tap on a node runs .expand for it", text.split("\n").slice(-3).join(" | "));
    }
    // One tap beside the graph: nothing is selected again, and the camera
    // gives back the room it made for the inspector's panel.
    await p.waitForTimeout(400);
    await tap(empty);
    await p.waitForTimeout(400);
    await roomSettled(p);

    // A mouse still drags and zooms with its wheel on the same canvas, and the keys work.
    await p.mouse.move(box.x + 20, box.y + 20);
    await p.mouse.wheel(0, -150);
    const wheeled = await spots();
    const wheel = Math.hypot(wheeled[b.n].x - wheeled[a.n].x, wheeled[b.n].y - wheeled[a.n].y) / Math.hypot(b.x - a.x, b.y - a.y);
    const wheelOff = off(fitted, wheeled, [a.n, b.n], { x: 20, y: 20 }, { x: 20, y: 20 }, wheel);
    check(wheel > 1.02 && wheelOff <= 2, "the wheel still zooms in around the pointer", `by ${wheel.toFixed(3)}, ${wheelOff.toFixed(2)} px off`);
    await p.focus("#graph-stage canvas");
    await p.keyboard.press("0");
    check(off(fitted, await spots(), [a.n, b.n], middle, middle, 1) <= 1, "0 still fits");
    await p.mouse.move(box.x + 30, box.y + 30);
    await p.mouse.down();
    await p.mouse.move(box.x + 50, box.y + 42, { steps: 4 });
    await p.mouse.up();
    check(off(fitted, await spots(), [a.n, b.n], { x: 30, y: 30 }, { x: 50, y: 42 }, 1) <= 1 && (await graphInfo(p)).picked === null, "a mouse still drags the drawing");
  }
  if (shots) await p.screenshot({ path: `${shots}/graph-pinched-phone.png` });
  await zctx.close();
}

// The tour (`.tour`, ?dataset=tour, the first item of the Datasets menu): on a
// phone, it runs end to end with no error, its tables fit the screen, its
// Louvain step matches the club's real split but for one member, and it draws
// the club.
{
  const tctx = await watchedContext({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true });
  const p = await tctx.newPage();
  await p.goto(`${base}/?dataset=tour`);
  await ready(p);
  await idle(p);
  const text = await screen(p);
  const cols: number = await p.evaluate(() => (window as any).ursaShell.cols());
  const tour = datasets.find((d) => d.id === "tour")!;
  const wide = text.split("\n").filter((l) => /^[┌│└├]/.test(l) && [...l].length > cols);
  check(!text.includes("Error: ") && (await tables(p)) === queries(tour.commands) && text.includes("That's the tour"), "the tour runs end to end", `${await tables(p)} tables`);
  check(wide.length === 0, "the tour's tables fit a phone", `${cols} columns; ${wide[0] ?? ""}`);
  const lines = text.split("\n");
  const factions = lastTable(lines.slice(0, lines.findIndex((l) => l.includes("Step 4"))).join("\n"));
  check(JSON.stringify(factions.rows) === '[["0","Mr. Hi","16"],["1","Mr. Hi","1"],["1","Officer","17"]]', "the tour's factions match the club's split", JSON.stringify(factions.rows));
  check(JSON.stringify(lastTable(text).rows.map((r) => r[1])) === '["[0, 8, 33]","[0, 13, 33]","[0, 19, 33]","[0, 31, 33]"]', "the tour's shortest paths", JSON.stringify(lastTable(text).rows));
  await graphSettled(p);
  const info = await graphInfo(p);
  check((await graphOpen(p)) && info.nodes === 34 && info.edges === 78, "the tour draws the club", JSON.stringify(info));
  const tourLegend = await legendText(p);
  check(/colou?r: club/.test(tourLegend) && tourLegend.includes("Mr. Hi (17)") && tourLegend.includes("Officer (17)") && /size: pagerank/.test(tourLegend), "the tour colours the club by side and sizes it by PageRank", tourLegend);
  if (shots) await p.screenshot({ path: `${shots}/tour-phone.png` });
  // .tour runs it again, in a fresh session.
  await p.click("#terminal");
  await type(".tour", p);
  const again = await screen(p);
  check(again.startsWith("Ursa Web Shell running") && !again.includes("Error: ") && (await tables(p)) === queries(tour.commands), ".tour", `${await tables(p)} tables`);
  await tctx.close();
}

// The docs' "Try it live" links (README, the SQL docs, the website): each
// opens here, and a Share link replays its example after y, with no error.
{
  const { linkCommands } = await import("../scripts/share-link.ts");
  const root = `${import.meta.dir}/../..`;
  const links = new Set<string>();
  for (const file of ["README.md", "docs/sql/index.md", "site/src/content/docs/docs/sql/introduction.md", "site/src/content/docs/docs/index.mdx", "site/src/content/docs/docs/quickstart.md", "site/src/pages/index.astro", "site/src/consts.ts"]) {
    const text = await Bun.file(`${root}/${file}`).text();
    for (const m of text.matchAll(/https:\/\/ursa-shell\.cl-dixon\.workers\.dev\/(#run=[A-Za-z0-9_-]+|\?dataset=\w+)/g)) links.add(m[1]);
  }
  for (const m of (await Bun.file(`${root}/site/src/consts.ts`).text()).matchAll(/\$\{SHELL_URL\}\/(\?dataset=\w+)/g)) links.add(m[1]);
  check([...links].some((l) => l.startsWith("#run=")) && links.has("?dataset=tour"), "the docs link to the shell", [...links].map((l) => l.slice(0, 20)).join(", "));
  const lctx = await watchedContext({ viewport: { width: 1280, height: 900 } });
  for (const link of links) {
    let p: Page;
    if (link.startsWith("#run=")) {
      const commands = linkCommands(link);
      p = await lctx.newPage();
      await p.goto(`${base}/${link}`);
      await p.waitForFunction(() => (window as any).ursaShell?.screen().includes("? [y/N] "), null, { timeout: 120_000 });
      check((await screen(p)).includes(`This link runs ${commands.length} shared commands:`), `a docs link asks first (${commands.length} commands)`);
      await p.keyboard.press("y");
    } else {
      p = await lctx.newPage();
      await p.goto(`${base}/${link}`);
      await ready(p);
    }
    await idle(p);
    const text = await screen(p);
    const id = link.slice(0, 32);
    if (link.startsWith("#run=")) {
      const t = lastTable(text);
      check(!text.includes("Error: ") && text.includes("Replaying") && t.head[0] === "id" && t.rows[0]?.[0] === "33", `the docs link ${id}… replays its example`, JSON.stringify(t.rows.slice(0, 2)));
    } else {
      check(!text.includes("Error: ") && (await tables(p)) > 0, `the docs link ${link} loads`, `${await tables(p)} tables`);
    }
    await p.close();
  }
  await lctx.close();
}

// The security headers, on every kind of response.
for (const path of ["/", "/app.js", "/engine/engine.json", "/data/manifest.json", "/data/no_such_file.csv", "/no/such/page"]) {
  const r = await fetch(`${base}${path}`);
  const csp = r.headers.get("content-security-policy") ?? "";
  check(
    /frame-ancestors 'none'/.test(csp) && /script-src 'self' 'wasm-unsafe-eval'/.test(csp) && r.headers.get("x-content-type-options") === "nosniff" && /^(no-referrer|strict-origin-when-cross-origin)$/.test(r.headers.get("referrer-policy") ?? ""),
    `security headers on ${path}`,
    `${r.status}`,
  );
}
// A path that does not decode is not found, not a server error; and no path
// leaves the files served. The Worker answers 404; on Cloudflare the edge may
// refuse some such paths itself with 400 (`%A.` in /engine/%E0%A4%A.wasm) before
// they reach it, which is as good.
for (const path of ["/data/%E0%A4%A", "/%E0%A4%A", "/engine/%E0%A4%A.wasm", "/data/%"]) {
  const r = await fetch(`${base}${path}`);
  check(r.status === 404 || r.status === 400, `${path} is refused (404, or 400 from the edge)`, `${r.status}`);
}
for (const path of [
  "/data/..%2f..%2fpackage.json",
  "/data/%2e%2e%2f%2e%2e%2fpackage.json",
  "/data/..%5c..%5cpackage.json",
  "/data/.%2e/.%2e/package.json",
  "/..%2f..%2fpackage.json",
  "/%2e%2e%2f%2e%2e%2fpackage.json",
  "/engine/..%2f..%2f..%2fpackage.json.wasm",
  "/data/..%2f..%2f..%2f..%2f..%2fetc%2fpasswd",
]) {
  const r = await fetch(`${base}${path}`);
  const body = await r.text();
  check(r.status >= 400 && !body.includes('"devDependencies"') && !body.includes("root:"), `${path} stays blocked`, `${r.status}`);
}

// #429: the shell runs cross-origin isolated. The three headers are on every
// response (a range of a dataset and the engine's .wasm too), and the page
// and both its workers, the engine's and the layout's, say they are isolated.
{
  const engine: { wasm: string } = overSocket ? { wasm: "" } : await (await fetch(`${base}/engine/engine.json`)).json();
  const viz: { module: string } = await (await fetch(`${base}/viz/viz.json`)).json();
  const isolating = (r: Response) =>
    r.headers.get("cross-origin-opener-policy") === "same-origin" && r.headers.get("cross-origin-embedder-policy") === "require-corp" && r.headers.get("cross-origin-resource-policy") === "same-origin";
  const sent = (r: Response) => ["opener", "embedder", "resource"].map((h) => r.headers.get(`cross-origin-${h}-policy`) ?? "none").join(", ");
  for (const path of [
    "/",
    "/?dataset=karate",
    "/app.js",
    "/engine-worker.js",
    "/graph-worker.js",
    "/graph.js",
    "/style.css",
    "/fonts/ursa-box.woff2",
    "/favicon.svg",
    "/engine/engine.json",
    engine.wasm,
    "/viz/viz.json",
    viz.module,
    viz.module.replace(/\.js$/, "_bg.wasm"),
    "/data/manifest.json",
    "/data/karate_edges.parquet",
    "/data/no_such_file.csv",
    "/no/such/page",
  ]) {
    if (overSocket && /^\/engine\/|^$/.test(path)) {
      skip(`cross-origin isolation headers on ${path || "the engine's .wasm"}`);
      continue;
    }
    const r = await fetch(`${base}${path}`);
    await r.body?.cancel();
    check(isolating(r), `cross-origin isolation headers on ${path} (${r.status})`, sent(r));
  }
  {
    // As the engine reads Parquet: a HEAD, then ranges; and a range past the end.
    const head = await fetch(`${base}/data/karate_edges.parquet`, { method: "HEAD" });
    check(head.status === 200 && isolating(head), "cross-origin isolation headers on a dataset's HEAD", `${head.status}: ${sent(head)}`);
    const range = await fetch(`${base}/data/karate_edges.parquet`, { headers: { range: "bytes=0-99" } });
    const bytes = (await range.arrayBuffer()).byteLength;
    check(range.status === 206 && bytes === 100 && isolating(range), "cross-origin isolation headers on a range of a dataset (206)", `${range.status}, ${bytes} bytes: ${sent(range)}`);
    const tail = await fetch(`${base}/data/karate_edges.parquet`, { headers: { range: "bytes=-8" } });
    check(tail.status === 206 && (await tail.arrayBuffer()).byteLength === 8 && isolating(tail), "cross-origin isolation headers on a dataset's last bytes (206)", `${tail.status}: ${sent(tail)}`);
    const past = await fetch(`${base}/data/karate_edges.parquet`, { headers: { range: "bytes=99999999-" } });
    check(past.status === 416 && isolating(past), "cross-origin isolation headers on a range past the end (416)", `${past.status}: ${sent(past)}`);
  }

  const ictx = await watchedContext({ viewport: { width: 1280, height: 800 }, acceptDownloads: true });
  const p = await ictx.newPage();
  const pageErrors: string[] = [];
  const consoleErrors: string[] = [];
  const failed: string[] = [];
  p.on("pageerror", (e) => pageErrors.push(e.message));
  p.on("console", (m) => m.type() === "error" && consoleErrors.push(m.text()));
  p.on("requestfailed", (r) => failed.push(`${new URL(r.url()).pathname}: ${r.failure()?.errorText}`));
  await p.goto(`${base}/?dataset=karate`);
  await ready(p);
  await idle(p);
  await p.click("#terminal");
  const onPage = await p.evaluate(() => ({
    isolated: self.crossOriginIsolated,
    shared: typeof SharedArrayBuffer === "function" && new WebAssembly.Memory({ initial: 1, maximum: 2, shared: true }).buffer instanceof SharedArrayBuffer,
    memory: typeof (performance as any).measureUserAgentSpecificMemory,
  }));
  check(onPage.isolated === true, "the page is cross-origin isolated", JSON.stringify(onPage));
  check(onPage.shared && onPage.memory === "function", "an isolated page can make a shared WebAssembly.Memory and measure its memory", JSON.stringify(onPage));
  // The dataset loaded by range requests, and a query over it answers.
  await type("SELECT count(*) AS ties FROM ties;", p);
  check(lastTable(await after("SELECT count(*)", p)).rows[0]?.[0] === "78", "isolated: a dataset's Parquet is read and queried");
  // The Graph view: ursa-viz and the layout's worker load and draw.
  await type(".graph karate SIZE BY pagerank COLOR BY club", p);
  await graphSettled(p);
  const drawn = await p.evaluate(() => (window as any).ursaShell.graphInked());
  check(drawn && drawn.inked > 500, "isolated: the Graph view draws", `${drawn?.inked} px inked`);
  const workers = await Promise.all(
    p.workers().map(async (w) => ({ name: new URL(w.url()).pathname, isolated: await w.evaluate(() => self.crossOriginIsolated).catch((e) => String(e)) })),
  );
  if (overSocket) skip("the worker /engine-worker.js is cross-origin isolated");
  for (const name of overSocket ? ["/graph-worker.js"] : ["/engine-worker.js", "/graph-worker.js"]) {
    const found = workers.filter((w) => w.name === name);
    check(found.length > 0 && found.every((w) => w.isolated === true), `the worker ${name} is cross-origin isolated`, JSON.stringify(workers));
  }
  // The fonts loaded: the terminal's, and the box-drawing font.
  const fonts = await p.evaluate(async () => {
    await document.fonts.ready;
    return [...document.fonts].filter((f) => f.status === "loaded").map((f) => f.family.replace(/"/g, ""));
  });
  check(fonts.includes("IBM Plex Mono") && fonts.some((f) => /ursa box/i.test(f)), "isolated: the fonts load", [...new Set(fonts)].join(", "));
  // A download (.export) and a Share link.
  const [download] = await Promise.all([p.waitForEvent("download"), type(".export csv", p)]);
  check(/\.csv$/.test(download.suggestedFilename()), "isolated: .export downloads the last result", download.suggestedFilename());
  await type(".share", p);
  const shared = await after(".share", p);
  check(p.url().includes("#run=") && /link that replays \d+ commands/.test(shared), "isolated: .share makes its link", shared.split("\n")[0]);
  check((await errors(p)) === 0 && pageErrors.length === 0 && consoleErrors.length === 0 && failed.length === 0, "isolated: no error on the screen, in the console or on the network", [...pageErrors, ...consoleErrors, ...failed].slice(0, 3).join(" | "));
  await ictx.close();
}
check(violations.length === 0, "no Content Security Policy violations", violations.slice(0, 3).join(" | "));

await browser.close();
console.log(failures.length ? `\n${failures.length} failures:\n- ${failures.join("\n- ")}` : "\nall passed");
process.exit(failures.length ? 1 : 0);
