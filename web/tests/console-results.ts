// The console's result tabs (URSA-14), over `ursa serve`: tests/console.ts
// runs these checks on its page while the server is up.
//
// - DDL run from the editor says its notices, and opens no tab;
// - `SELECT * FROM pagerank(karate)` opens a tab of typed columns, each
//   header with its type and its `ursa.*` role, numbers right-aligned;
// - the tabs are the ARIA tabs pattern: arrows move, Delete closes;
// - a 100,000-row result pages with `fetch` and scrolls smoothly, Ctrl+End
//   reaches its last row, a sort says it sorts the loaded rows only, and a
//   range copies as tab-separated text;
// - the Graph view draws karate in the pane, and a double-click's `.expand`
//   reaches the commands sink (History's, which lists it; then a sink set
//   by setCommandsSink);
// - the Plan view shows `explain`'s text;
// - Export downloads a CSV of every row;
// - closing a tab frees its result (`fetch` of it is `not_found`), and a tab
//   whose result has gone says so and runs it again;
// - an error says itself, with no tab.
//
// With a screenshot dir: results-{table,graph,100k}-{light,dark}-1440x900.png.
import type { BrowserContext, Page } from "playwright";
import { readFileSync } from "node:fs";
import { join } from "node:path";

type Check = (ok: boolean, label: string, detail?: string) => void;

export async function resultsChecks(page: Page, context: BrowserContext, check: Check, shots?: string) {
  const ev = <T>(fn: () => T | Promise<T>) => page.evaluate(fn);
  const shot = async (name: string) => {
    if (!shots) return;
    await page.evaluate(() => (document.activeElement as HTMLElement)?.blur?.());
    await page.waitForTimeout(250);
    await page.screenshot({ path: join(shots, name) });
  };
  const theme = (t: "light" | "dark") => page.evaluate((t) => (document.documentElement.dataset.theme = t), t);

  // A fresh layout, in light.
  await page.evaluate(() => {
    // The editor's third of the column; the results the rest.
    localStorage.setItem("ursa-console-layout", JSON.stringify({ rows: { main: 0.3 } }));
    localStorage.removeItem("ursa-console-sql");
    document.documentElement.dataset.theme = "light";
    localStorage.setItem("ursa-theme", "light");
  });
  await page.reload();
  await page.waitForFunction(() => (window as any).ursaConsole?.session.state === "connected", null, { timeout: 30_000 });
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);

  check(
    (await page.locator("#results .rs-empty").isVisible()) && (await page.locator("#editor .cm-editor").count()) === 1,
    "results: an empty Results panel says what comes, beside the editor",
  );

  // ---------------------------------------------------------------- DDL from the editor

  await page.waitForFunction(() => (window as any).ursaConsole?.editor, null, { timeout: 30_000 });
  await page.evaluate(() => {
    const editor = (window as any).ursaConsole.editor;
    editor.setText("CREATE VIEW members AS SELECT * FROM 'karate_nodes.parquet';");
    editor.runCurrent();
  });
  await page.waitForSelector(".rs-message:not([hidden])", { timeout: 30_000 });
  const ddl = {
    message: await page.locator(".rs-message").innerText(),
    tabs: await page.locator(".rs-tab").count(),
  };
  check(/CREATE VIEW members .*OK/.test(ddl.message) && ddl.tabs === 0, "results: the editor runs a CREATE, which says OK and opens no tab", JSON.stringify(ddl));
  const created = await ev(async () => {
    const run = (window as any).ursaConsole.run;
    const a = await run("CREATE VIEW ties AS SELECT * FROM 'karate_edges.parquet'");
    const b = await run(`CREATE PROPERTY GRAPH karate
      VERTEX TABLES (members KEY (id))
      EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id) DESTINATION KEY (dst) REFERENCES members (id))`);
    return [a, b].map((r) => JSON.stringify(r.notices ?? r.error?.message));
  });
  check(created.every((n) => n === '["OK"]'), "results: ursaConsole.run(sql) is the run function, and DDL gives notices", created.join(" "));

  // An error: said, no tab.
  const wrong = await ev(() => (window as any).ursaConsole.run("SELECT nope").then((r: any) => r.error?.kind));
  check(wrong === "sql" && (await page.locator(".rs-message[data-kind=error]").isVisible()) && (await page.locator(".rs-tab").count()) === 0, "results: an error is said over the tabs, with no tab", `${wrong} ${await page.locator(".rs-message").innerText()}`);

  // ---------------------------------------------------------------- a typed table

  await page.evaluate(() => {
    const editor = (window as any).ursaConsole.editor;
    editor.setText("-- The karate club's members, by PageRank\nSELECT * FROM pagerank(karate) ORDER BY pagerank DESC;");
    editor.runCurrent();
  });
  await page.waitForSelector(".rs-tab[aria-selected=true]", { timeout: 30_000 });
  const pr = await ev(() => {
    const tab = document.querySelector<HTMLElement>(".rs-tab[aria-selected=true]")!;
    const panel = document.getElementById(tab.getAttribute("aria-controls")!)!;
    const heads = [...panel.querySelectorAll(".rt-th")].map((h) => ({
      name: h.querySelector(".rt-name")?.textContent,
      type: h.querySelector(".rt-type")?.textContent,
      role: h.querySelector(".rt-role")?.textContent ?? null,
    }));
    const grid = panel.querySelector("[role=grid]")!;
    const numCell = panel.querySelector(".rt-body .rt-row:not([hidden]) .rt-cell.num:not(.rt-gutter)") as HTMLElement;
    return {
      label: tab.querySelector(".rs-tab-label")?.textContent,
      count: tab.querySelector(".rs-tab-count")?.textContent,
      name: tab.getAttribute("aria-label"),
      heads,
      rowcount: grid.getAttribute("aria-rowcount"),
      right: numCell ? getComputedStyle(numCell).textAlign : null,
      views: [...panel.querySelectorAll(".rs-view-tab")].map((b) => b.textContent),
      tablist: document.querySelector(".rs-tabs")?.getAttribute("role"),
    };
  });
  check(
    pr.label!.startsWith("SELECT * FROM pagerank(karate)") && pr.count === "34" && pr.name!.endsWith("34 rows") && pr.tablist === "tablist",
    "results: a result opens a tab labelled with its statement and its row count",
    JSON.stringify({ label: pr.label, count: pr.count, name: pr.name }),
  );
  check(
    JSON.stringify(pr.heads) === JSON.stringify([
      { name: "id", type: "Int64", role: "node" },
      { name: "pagerank", type: "Float64", role: "metric" },
    ]),
    "results: SELECT * FROM pagerank(karate) shows typed columns with their roles",
    JSON.stringify(pr.heads),
  );
  check(pr.rowcount === "35" && pr.right === "right", "results: the grid says its rows, and numbers are right-aligned", `${pr.rowcount} ${pr.right}`);
  check(JSON.stringify(pr.views) === JSON.stringify(["Table", "Graph", "Plan"]), "results: a result whose columns make a graph offers a Graph view", JSON.stringify(pr.views));
  await theme("light");
  await shot("results-table-light-1440x900.png");
  await theme("dark");
  await shot("results-table-dark-1440x900.png");
  await theme("light");

  // A NULL is shown as the shell shows it.
  await ev(() => (window as any).ursaConsole.run("SELECT 1 AS n, CAST(NULL AS VARCHAR) AS s, repeat('long text ', 40) AS t"));
  // The tab exists once run resolves; its rows are drawn on the next frame.
  await page.waitForFunction(() => document.querySelectorAll(".rs-panel:not([hidden]) .rt-body .rt-row:not([hidden]) .rt-cell").length >= 4, null, { timeout: 30_000 });
  const nulls = await ev(() => {
    const panel = document.querySelector<HTMLElement>(".rs-panel:not([hidden])")!;
    const cells = [...panel.querySelectorAll(".rt-body .rt-row:not([hidden]) .rt-cell")] as HTMLElement[];
    const long = cells[3];
    return { text: cells[2].textContent, null: cells[2].classList.contains("null"), graph: !!panel.querySelector('[data-view="graph"]'), cut: long.scrollWidth > long.clientWidth, whole: long.textContent!.length };
  });
  check(nulls.text === "NULL" && nulls.null && !nulls.graph, "results: NULL is shown distinctly, and a result that is no graph has no Graph view", JSON.stringify(nulls));
  // The long text: cut short, whole on focus (the strip under the table) and on hover (its title).
  await page.locator(".rs-panel:not([hidden]) [role=grid]").focus();
  await page.keyboard.press("End");
  const peek = await ev(() => {
    const panel = document.querySelector<HTMLElement>(".rs-panel:not([hidden])")!;
    const p = panel.querySelector<HTMLElement>(".rt-peek")!;
    return { shown: !p.hidden, length: p.textContent!.length };
  });
  await page.locator(".rs-panel:not([hidden]) .rt-body .rt-row:not([hidden]) .rt-cell").nth(3).hover();
  const title = await page.locator(".rs-panel:not([hidden]) .rt-body .rt-row:not([hidden]) .rt-cell").nth(3).getAttribute("title");
  check(nulls.cut && peek.shown && peek.length === 400 && title?.length === 400, "results: long text is cut short, with its whole value on focus and on hover", `${JSON.stringify(peek)} title ${title?.length}`);

  // ---------------------------------------------------------------- the tabs' keys

  const tabsBefore = await page.locator(".rs-tab").count();
  await page.locator(".rs-tab[aria-selected=true]").focus();
  await page.keyboard.press("ArrowLeft");
  const moved = await ev(() => ({ focus: document.activeElement?.querySelector(".rs-tab-label")?.textContent, selected: document.querySelector(".rs-tab[aria-selected=true] .rs-tab-label")?.textContent, stops: document.querySelectorAll(".rs-tab[tabindex='0']").length }));
  check(moved.focus?.startsWith("SELECT * FROM pagerank") && moved.selected === moved.focus && moved.stops === 1, "results: the arrow keys move between tabs and show them, one focus stop", JSON.stringify(moved));
  await page.keyboard.press("ArrowRight");
  const closedId = await ev(() => (window as any).ursaConsole.results.selected().id);
  await page.keyboard.press("Delete");
  await page.waitForTimeout(300);
  const afterClose = await page.evaluate(
    (id) =>
      (window as any).ursaConsole.session.run("fetch", { result: id }).then(
        () => "kept",
        (e: any) => e.kind,
      ),
    closedId,
  );
  check(
    (await page.locator(".rs-tab").count()) === tabsBefore - 1 && afterClose === "not_found" && (await ev(() => document.activeElement?.classList.contains("rs-tab"))),
    "results: Delete closes a tab, and the session frees its result (fetch of it: not_found)",
    `${closedId} → ${afterClose}`,
  );

  // ---------------------------------------------------------------- the graph view

  await page.locator(".rs-tab", { hasText: "pagerank" }).click();
  await page.locator(".rs-panel:not([hidden]) .rs-view-tab[data-view=graph]").click();
  const drawn = await page
    .waitForFunction(
      () => {
        const g = (window as any).ursaConsole.results.pane()?.view()?.info();
        return g && g.frames > 0 && g.layoutDone && !g.pending;
      },
      null,
      { timeout: 60_000, polling: 200 },
    )
    .then(() => true, () => false);
  const info = () => ev(() => (window as any).ursaConsole.results.pane()?.view()?.info() ?? null);
  const g0 = await info();
  const inTab = await ev(() => {
    const pane = document.getElementById("results-graph-pane")!;
    return !pane.hidden && !!pane.closest(".rs-panel:not([hidden]) .rs-graph-view") && !!pane.querySelector("canvas");
  });
  check(drawn && g0?.nodes === 34 && g0?.edges === 78 && inTab, "results: the Graph view draws karate in the pane, in its tab (graph.open {result})", `${g0?.nodes} nodes, ${g0?.edges} edges, in tab ${inTab}`);
  await page.waitForFunction(() => (window as any).ursaConsole.results.pane()?.view()?.info()?.notShownKnown === true, null, { timeout: 30_000 }).catch(() => {});
  const canvas = page.locator("#results-graph-stage canvas");
  const dblclick = async (id: number) => {
    const box = (await canvas.boundingBox())!;
    const at = await page.evaluate((id) => (window as any).ursaConsole.results.pane().view().nodePlace(id), id);
    await page.mouse.dblclick(box.x + at.x, box.y + at.y);
  };
  await dblclick(2);
  // The console's History takes the sink (URSA-17): the step is run, and
  // listed there under its drawing; the Steps list under the graph is not shown.
  const listed = await page
    .waitForFunction(() => (window as any).ursaConsole.history?.entries().some((e: any) => e.summary === ".expand 2" && e.via === "view" && e.drawing && !e.error), null, { timeout: 30_000 })
    .then(() => true, () => false);
  const steps = await ev(() => (window as any).ursaConsole.history?.entries().filter((e: any) => e.op.startsWith("graph.")));
  check(
    listed && (await page.locator("#history .hx-children .hx-row", { hasText: ".expand 2" }).isVisible()) && !(await page.locator(".rs-steps-box").isVisible()),
    "results: a double-click's .expand reaches the commands sink (History), which runs it and lists it under its drawing",
    JSON.stringify(steps),
  );
  // History's hook: a sink of its own gets the pane's commands.
  await ev(() => {
    const w = window as any;
    w.sunk = [];
    w.ursaConsole.results.setCommandsSink(async (command: string, ctx: any) => {
      w.sunk.push({ command, via: ctx.via, result: ctx.result });
      const r = await ctx.exec(command);
      return r.ok;
    });
  });
  await dblclick(33);
  const sunk = await page
    .waitForFunction(() => (window as any).sunk.find((s: any) => s.command === ".expand 33"), null, { timeout: 30_000 })
    .then((h) => h.jsonValue(), () => null);
  // History's sink back.
  await ev(() => (window as any).ursaConsole.results.setCommandsSink((window as any).ursaConsole.history?.sink ?? null));
  check((sunk as any)?.via === "view" && /^r\d+$/.test((sunk as any)?.result), "results: setCommandsSink gives the pane's commands to another sink (History's hook)", JSON.stringify(sunk));
  await page.waitForTimeout(400);
  await theme("light");
  await shot("results-graph-light-1440x900.png");
  await theme("dark");
  await page.waitForTimeout(300);
  await shot("results-graph-dark-1440x900.png");
  await theme("light");

  // The plan.
  await page.locator(".rs-panel:not([hidden]) .rs-view-tab[data-view=plan]").click();
  await page.waitForSelector(".rs-panel:not([hidden]) .rs-plan", { timeout: 30_000 }).catch(() => {});
  const plan = (await page.locator(".rs-panel:not([hidden]) .rs-plan").textContent().catch(() => "")) ?? "";
  const mono = await ev(() => getComputedStyle(document.querySelector(".rs-panel:not([hidden]) .rs-plan")!).fontFamily);
  check(plan.startsWith("logical_plan") && plan.includes("physical_plan") && /Mono|monospace/.test(mono), "results: the Plan view shows explain's text, in monospace", plan.slice(0, 120));
  await page.locator(".rs-panel:not([hidden]) .rs-view-tab[data-view=table]").click();

  // ---------------------------------------------------------------- export

  const exportCsv = async () => {
    const download = page.waitForEvent("download", { timeout: 30_000 });
    await page.locator(".rs-panel:not([hidden]) .rs-export > button").click();
    await page.locator(".rs-panel:not([hidden]) .rs-format[data-format=csv]").click();
    return download;
  };
  const csvDownload = await exportCsv().catch(() => null);
  const csvPath = await csvDownload?.path();
  const csv = csvPath ? readFileSync(csvPath, "utf8").trim().split("\n") : [];
  const ids = csv.slice(1).map((l) => Number(l.split(",")[0])).sort((a, b) => a - b);
  check(
    /^ursa-r\d+\.csv$/.test(csvDownload?.suggestedFilename() ?? "") && csv[0] === "id,pagerank" && csv.length === 35 && ids.join() === Array.from({ length: 34 }, (_, i) => i).join(),
    "results: Export downloads a CSV of every row",
    `${csvDownload?.suggestedFilename()} ${csv.length} lines, ${csv[0]}`,
  );

  // ---------------------------------------------------------------- 100,000 rows

  await ev(() => (window as any).ursaConsole.run("SELECT * FROM range(100000)"));
  await page.waitForFunction(() => (window as any).ursaConsole.results.selected()?.rows === 100000, null, { timeout: 30_000 });
  const big = await ev(() => {
    const tab = (window as any).ursaConsole.results.selected();
    return { count: tab.button.querySelector(".rs-tab-count").textContent, graph: tab.graph, state: tab.table.state() };
  });
  check(big.count === "100k" && big.state.pages === 1 && big.state.shown[0][0] === "1" && big.state.shown[0][1] === "0", "results: a 100,000-row result opens with its first page", JSON.stringify({ count: big.count, pages: big.state.pages, first: big.state.shown[0] }));

  // Smooth: scroll through it a frame at a time; each frame draws its rows.
  const smooth = await ev(async () => {
    const grid = document.querySelector<HTMLElement>(".rs-panel:not([hidden]) [role=grid]")!;
    const frames: number[] = [];
    let last = performance.now();
    for (let i = 0; i < 90; i++) {
      grid.scrollTop += 900;
      await new Promise(requestAnimationFrame);
      const now = performance.now();
      frames.push(now - last);
      last = now;
    }
    frames.sort((a, b) => a - b);
    const t = (window as any).ursaConsole.results.selected().table.state();
    return { median: frames[45], p95: frames[85], worst: frames[89], first: t.first, shown: t.shown.length };
  });
  check(smooth.median < 34 && smooth.p95 < 80 && smooth.shown > 10, "results: 100,000 rows scroll smoothly (a frame at a time, each drawn)", JSON.stringify(smooth));

  // Ctrl+End: the last row, fetched and shown.
  await page.locator(".rs-panel:not([hidden]) [role=grid]").focus();
  await page.keyboard.press("Control+End");
  const reached = await page
    .waitForFunction(() => {
      const t = (window as any).ursaConsole.results.selected().table.state();
      return t.shown.some((r: string[]) => r[0] === "100000" && r[1] === "99999");
    }, null, { timeout: 30_000 })
    .then(() => true, () => false);
  const end = await ev(() => {
    const t = (window as any).ursaConsole.results.selected().table.state();
    const grid = document.querySelector<HTMLElement>(".rs-panel:not([hidden]) [role=grid]")!;
    const active = document.getElementById(grid.getAttribute("aria-activedescendant")!);
    const box = active?.getBoundingClientRect();
    const g = grid.getBoundingClientRect();
    return { active: t.active, text: active?.textContent, onScreen: !!box && box.top >= g.top && box.bottom <= g.bottom + 1, pages: t.pages, index: active?.parentElement?.getAttribute("aria-rowindex") };
  });
  check(reached && end.text === "99999" && end.onScreen && end.index === "100001", "results: Ctrl+End reaches the last row: paged in with fetch, on screen, the active cell", JSON.stringify(end));
  // Halfway, for the screenshot.
  await ev(() => (window as any).ursaConsole.results.selected().table.goTo(49_990));
  await page.waitForFunction(() => (window as any).ursaConsole.results.selected().table.state().shown.some((r: string[]) => r[1] === "49990"), null, { timeout: 30_000 }).catch(() => {});
  await shot("results-100k-light-1440x900.png");
  await theme("dark");
  await shot("results-100k-dark-1440x900.png");
  await theme("light");

  // A sort: the loaded rows only, and it says so.
  await page.locator(".rs-panel:not([hidden]) .rt-th .rt-sort").click();
  await page.locator(".rs-panel:not([hidden]) .rt-th .rt-sort").click();
  const sorted = await ev(() => {
    const panel = document.querySelector<HTMLElement>(".rs-panel:not([hidden])")!;
    const t = (window as any).ursaConsole.results.selected().table.state();
    return { note: panel.querySelector(".rt-note")!.textContent, sort: t.sort, first: t.shown[0], rows: t.viewRows, aria: panel.querySelector(".rt-th")!.getAttribute("aria-sort") };
  });
  check(
    /only the [\d,]+ rows loaded of 100,000/.test(sorted.note!) && sorted.first?.[1] === "99999" && sorted.rows < 100000 && sorted.aria === "descending",
    "results: a column's header sorts the loaded rows only, and says so",
    JSON.stringify(sorted),
  );
  await page.locator(".rs-panel:not([hidden]) .rt-clear").click();

  // Copy a range: Ctrl+Home, Shift+Down twice, Ctrl+C.
  await page.locator(".rs-panel:not([hidden]) [role=grid]").focus();
  await page.keyboard.press("Control+Home");
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("Control+c");
  await page.waitForTimeout(300);
  const copied = await ev(async () => ({ mine: (window as any).ursaConsole.results.selected().table.copied(), board: await navigator.clipboard.readText().catch((e: any) => `error: ${e.message}`) }));
  check(copied.mine === "0\n1\n2" && copied.board === "0\n1\n2", "results: Ctrl+C copies the selected range", JSON.stringify(copied));
  await page.keyboard.press("Control+End");
  await page.keyboard.press("Control+c");
  await page.waitForTimeout(200);
  const one = await ev(() => navigator.clipboard.readText());
  check(one === "99999", "results: and a cell on its own", JSON.stringify(one));

  // ---------------------------------------------------------------- a result that has gone

  const goneId = await ev(() => (window as any).ursaConsole.results.selected().id);
  // Dropped behind the tab's back, as the 17th result or the 256 MB cap does.
  await page.evaluate((id) => (window as any).ursaConsole.session.run("close", { result: id }), goneId);
  await page.locator(".rs-panel:not([hidden]) .rs-export > button").click();
  await page.locator(".rs-panel:not([hidden]) .rs-format[data-format=csv]").click();
  const gone = await page
    .waitForSelector(".rs-panel:not([hidden]) .rs-gone:not([hidden])", { timeout: 15_000 })
    .then((h) => h.innerText(), () => "");
  check(gone.includes(`${goneId} has gone from the session`) && /Run again/.test(gone) && (await ev(() => document.querySelector(".rs-tab[aria-selected=true]")!.classList.contains("dropped"))), "results: a result that has gone (not_found) says so, and offers Run again", gone);
  // A page not loaded yet: not_found too, said the same way, nothing thrown.
  await ev(() => (window as any).ursaConsole.results.selected().table.goTo(70_000));
  await page.waitForTimeout(300);
  await page.locator(".rs-panel:not([hidden]) .rs-gone button").click();
  const again = await page
    .waitForFunction((id) => {
      const t = (window as any).ursaConsole.results.selected();
      return t && t.id !== id && t.rows === 100000 && !t.dropped ? t.id : null;
    }, goneId, { timeout: 30_000 })
    .then((h) => h.jsonValue(), () => null);
  const tabsNow = await ev(() => (window as any).ursaConsole.results.tabs().map((t: any) => t.id));
  check(!!again && !tabsNow.includes(goneId) && tabsNow.includes(again), "results: Run again runs it in the tab's place, under a new id", `${goneId} → ${again}: ${tabsNow.join(" ")}`);

  // Closing every tab: the panel is empty again, and the graph closes with its tab.
  await ev(async () => {
    for (const t of (window as any).ursaConsole.results.tabs()) await t.close();
  });
  const graphOpen = await ev(() => (window as any).ursaConsole.session.run("graph.explore", { command: ".undo" }).then((r: any) => r.err));
  check((await page.locator(".rs-empty").isVisible()) && /no graph is open/.test(graphOpen), "results: closing the tabs empties the panel, and the graph closes with its tab", graphOpen);
}
