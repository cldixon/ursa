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
// the focus after a click), a phone's bar and SQL, and the grey text's contrast. An error on screen fails the run, except the ones a test asks
// for.
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
  await p.waitForFunction(() => (window as any).ursaShell && !(window as any).ursaShell.busy(), null, { timeout: 120_000 });
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
const greyText = (p: Page = page) =>
  p.evaluate(() => {
    const span = [...document.querySelectorAll(".xterm-rows span")].findLast((s) => s.textContent!.trim() === "Int64");
    return span ? [getComputedStyle(span).color, getComputedStyle(document.querySelector(".terminal")!).backgroundColor] : null;
  });

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
check(/Ursa Web Shell running\.\.\.\nUrsa v\d\S*\nEnter "\.help" for usage hints\.\nmemory U/.test(banner), "banner", `${banner.match(/Ursa v\S+/)?.[0]} (${Date.now() - t0} ms)`);
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
const replay = await context.newPage();
replay.on("pageerror", (e) => failures.push(`replay page error: ${e.message}`));
await replay.goto(url);
await ready(replay);
await idle(replay);
const replayed = await screen(replay);
// The imported file is not in the link: its view is the one error expected.
const replayErrors = replayed.split("\n").filter((l) => l.startsWith("Error: "));
check(replayed.includes("Replaying") && replayErrors.every((e) => /my_ties/.test(e)) && (await tables(replay)) > 5, "shared link replays", replayErrors[0] ?? `${await tables(replay)} tables`);
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

// The engine crashing (out of memory) leaves a working shell, in a fresh session.
await type("SELECT length(repeat('x', 2000000000)) AS n;");
await idle();
check((await screen()).includes("the engine stopped"), "an engine crash is reported");
await type("SELECT 41 + 1 AS answer;");
check(lastTable(await after("SELECT 41 + 1 AS answer;")).rows[0]?.[0] === "42", "the shell works after an engine crash");

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

await browser.close();
console.log(failures.length ? `\n${failures.length} failures:\n- ${failures.join("\n- ")}` : "\nall passed");
process.exit(failures.length ? 1 : 0);
