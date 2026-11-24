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

const browser = await chromium.launch();
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
    // The frame before: node 2 selected, the pointer off the canvas.
    await away();
    await s.waitForFunction(() => (window as any).ursaShell.graph().hovered === null);
    const places = async (ids: number[]) => Promise.all(ids.map(place));
    const before = { inked: await shell<any>("graphInked"), at: await places([0, 2, 7, 31]) };
    console.log("ROOM", JSON.stringify((await graphInfo(s)).room), JSON.stringify(await shell("graphInspector"))?.slice(0,60));
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
    console.log("ROOM", JSON.stringify((await graphInfo(s)).room), JSON.stringify(await shell("graphInspector"))?.slice(0,60));
    await typed(".undo");
    await settled();
    await away();
    await s.waitForFunction(() => (window as any).ursaShell.graph().hovered === null);
    {
      const undone = await graphInfo(s);
      const after_ = { inked: await shell<any>("graphInked"), at: await places([0, 2, 7, 31]) };
    console.log("ROOM", JSON.stringify((await graphInfo(s)).room), JSON.stringify(await shell("graphInspector"))?.slice(0,60));
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
    console.log("ROOM", JSON.stringify((await graphInfo(s)).room), JSON.stringify(await shell("graphInspector"))?.slice(0,60));
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
await browser.close();
console.log(failures.length ? `\n${failures.length} failures:\n- ${failures.join("\n- ")}` : "\nall passed");
process.exit(failures.length ? 1 : 0);
