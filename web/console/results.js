// The Results panel (URSA-14, design/console.md §6): each result in a tab of
// its own, as a table, as a graph when its columns say it can be one, and as
// its plan.
//
//   import { createResults } from "/console/results.js";
//   const results = createResults({ mount: panels.results, session });
//   await results.run("SELECT * FROM pagerank(karate)");   // the editor's Run
//   results.setCommandsSink(fn);                            // History (URSA-17)
//
// - run(sql) sends `query {sql, rows}`. A result with columns opens a tab,
//   labelled with its statement (shortened) and its row count; a statement
//   with none (CREATE, SET) says its notices over the tabs instead. A tab
//   is closed with its ×, or Delete on it, which sends `close {result}`:
//   the session frees it.
// - The tabs are the ARIA tabs pattern: one focus stop, the arrow keys, Home
//   and End move between them (and show the one they reach), Delete closes.
//   Each tab's own tabs show the result as a Table (table.js: paged with
//   `fetch`, typed, sortable, copyable), a Graph and a Plan (`explain`).
// - The Graph view is offered when `query` says by which rule the result is
//   a graph (its `graph`: the engine decides, from the columns). The page has
//   one graph pane (graph-pane.js: graph.js draws one view at a time), which
//   moves to the tab whose Graph view is shown, and draws its result there
//   with `graph.open {result}`. Its commands (a double-click's `.expand 33`)
//   go to the commands sink: History (history.js) takes it (setCommandsSink)
//   and lists them, grouped under their drawing; without History, a small
//   list under the graph keeps them. Either way they run in the pane.
// - Export writes the whole result with `export {format, result}`, as CSV,
//   Parquet, JSON or Arrow, downloaded as a file.
// - The session keeps 16 results, within 256 MB: a tab whose result has gone
//   (`not_found`, from fetch, export or graph.open) says so and offers to run
//   its statement again, in its place.
import { createTable, PAGE } from "./table.js";
import { createPane } from "/graph-pane.js";

const FORMATS = [
  { format: "csv", name: "CSV", ext: "csv", type: "text/csv" },
  { format: "parquet", name: "Parquet", ext: "parquet", type: "application/vnd.apache.parquet" },
  { format: "json", name: "JSON", ext: "json", type: "application/json" },
  { format: "arrow", name: "Arrow", ext: "arrow", type: "application/vnd.apache.arrow.stream" },
];

/** What each rule draws, for the Graph view's tooltip. */
const RULES = {
  nodes: "its rows as nodes, with the graph's edges among them",
  edges: "each row as an edge",
  path: "its rows as paths",
  rings: "rings around the seeds, by distance",
  pattern: "the matches of its pattern",
  graph: "a property graph",
};

const count = (n, one, many = `${one}s`) => `${n.toLocaleString("en-US")} ${n === 1 ? one : many}`;

/** A row count, short, for a tab: 34, 1,000, 100k, 1.2M. */
function compact(n) {
  if (n < 10_000) return n.toLocaleString("en-US");
  if (n < 1_000_000) return `${Math.round(n / 1000)}k`;
  return `${(n / 1_000_000).toFixed(n < 10_000_000 ? 1 : 0)}M`;
}

/** `sql`'s last statement on one line, without its comments: a tab's label. */
export function shorten(sql, max = 34) {
  const statements = sql
    .replace(/--[^\n]*/g, " ")
    .replace(/\/\*[\s\S]*?\*\//g, " ")
    .split(";")
    .map((s) => s.replace(/\s+/g, " ").trim())
    .filter(Boolean);
  const last = statements.at(-1) ?? sql.trim();
  return last.length > max ? `${last.slice(0, max - 1).trimEnd()}…` : last;
}

const ICON = {
  close: '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4.5 4.5l7 7M11.5 4.5l-7 7" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>',
  table: '<svg viewBox="0 0 16 16" aria-hidden="true"><g fill="none" stroke="currentColor" stroke-width="1.3"><rect x="2" y="3" width="12" height="10" rx="1.5"/><path d="M2 6.5h12M6 6.5V13"/></g></svg>',
  graph: '<svg viewBox="0 0 16 16" aria-hidden="true"><g fill="none" stroke="currentColor" stroke-width="1.3"><path d="M4.5 11 8 4.5l3.5 6.5M4.5 11h7"/></g><g fill="currentColor"><circle cx="8" cy="4.5" r="1.9"/><circle cx="4.5" cy="11" r="1.9"/><circle cx="11.5" cy="11" r="1.9"/></g></svg>',
  plan: '<svg viewBox="0 0 16 16" aria-hidden="true"><g fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"><path d="M3 3.5h7M5 7h7M7 10.5h6M3 3.5v3.5h2M5 7v3.5h2"/></g></svg>',
  download: '<svg viewBox="0 0 16 16" aria-hidden="true"><g fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M8 2.5v8M4.8 7.5 8 10.7l3.2-3.2M3 13.5h10"/></g></svg>',
  chevron: '<svg viewBox="0 0 12 12" aria-hidden="true"><path d="M3 4.5 6 7.5 9 4.5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>',
  again: '<svg viewBox="0 0 16 16" aria-hidden="true"><g fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M12.5 8a4.5 4.5 0 1 1-1.3-3.2M12.5 2.8v2.4h-2.4"/></g></svg>',
};

const el = (tag, className, attrs = {}, html) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
  if (html !== undefined) node.innerHTML = html;
  return node;
};

/**
 * The ARIA tabs pattern on `list`'s `[role=tab]`s: one focus stop, the arrow
 * keys and Home and End move to a tab and show it (`show(tab)`).
 */
function rovingTabs(list, show, { remove } = {}) {
  list.addEventListener("keydown", (e) => {
    const tabs = [...list.querySelectorAll('[role="tab"]:not([hidden])')];
    const at = tabs.indexOf(document.activeElement);
    if (at < 0) return;
    let next = null;
    if (e.key === "ArrowRight") next = tabs[(at + 1) % tabs.length];
    else if (e.key === "ArrowLeft") next = tabs[(at - 1 + tabs.length) % tabs.length];
    else if (e.key === "Home") next = tabs[0];
    else if (e.key === "End") next = tabs.at(-1);
    else if ((e.key === "Delete" || e.key === "Backspace") && remove) {
      e.preventDefault();
      remove(tabs[at]);
      return;
    } else return;
    e.preventDefault();
    show(next);
    next.focus();
  });
}

/** The Results panel in `mount`: see the top of this file. */
export function createResults({ mount, session }) {
  // ---------------------------------------------------------------- markup

  mount.replaceChildren();
  const root = el("div", "rs");
  const bar = el("div", "rs-bar");
  const tablist = el("div", "rs-tabs", { role: "tablist", "aria-label": "Results" });
  bar.append(tablist);
  const message = el("div", "rs-message", { hidden: "" });
  const messageText = el("span", "rs-message-text");
  const messageClose = el("button", "rs-icon", { type: "button", "aria-label": "Dismiss", title: "Dismiss" }, ICON.close);
  messageClose.addEventListener("click", () => showMessage(null));
  message.append(el("span", "rs-message-mark", { "aria-hidden": "true" }), messageText, messageClose);
  const panels = el("div", "rs-panels");
  const empty = el("div", "rs-empty");
  empty.innerHTML = `
    <div class="rs-empty-art" aria-hidden="true">${ICON.table}${ICON.graph}${ICON.plan}</div>
    <p class="rs-empty-what">No results yet</p>
    <p>Run a query, and its result opens here in a tab of its own: a table of every row, its graph when its columns make one, and its plan.</p>`;
  const live = el("span", "visually-hidden", { role: "status" });
  root.append(bar, message, panels, empty, live);
  mount.append(root);

  const say = (text) => {
    live.textContent = "";
    // A change a screen reader hears, though the words repeat.
    requestAnimationFrame(() => (live.textContent = text));
  };

  /** The line over the tabs: a statement's notices, or what went wrong. */
  function showMessage(m) {
    message.hidden = !m;
    if (!m) return;
    message.dataset.kind = m.kind;
    messageText.textContent = m.text;
    message.setAttribute("role", m.kind === "error" ? "alert" : "status");
  }

  // ---------------------------------------------------------------- tabs

  /** The open tabs, in order. */
  const tabs = [];
  let selected = null;
  let made = 0;

  function update() {
    empty.hidden = tabs.length > 0;
    bar.hidden = tabs.length === 0;
  }

  /** A tab for `sql`'s result (`reply`, query's result), before `before`. */
  function addTab(sql, reply, before = null) {
    const n = ++made;
    const tab = { n, sql, reply, id: reply.result, view: "table", dropped: false, plan: null };
    const label = shorten(sql);
    tab.label = label;

    // The tab in the tablist.
    const button = el("div", "rs-tab", { role: "tab", id: `rs-tab-${n}`, "aria-controls": `rs-panel-${n}`, "aria-selected": "false", tabindex: "-1", "aria-keyshortcuts": "Delete" });
    button.setAttribute("aria-label", `${label}, ${count(reply.rows, "row")}`);
    button.title = `${sql.trim()}\n${count(reply.rows, "row")} · ${tab.id}`;
    const text = el("span", "rs-tab-label");
    text.textContent = label;
    const rows = el("span", "rs-tab-count");
    rows.textContent = compact(reply.rows);
    const close = el("span", "rs-tab-close", { "aria-hidden": "true", title: "Close (Delete)" }, ICON.close);
    close.addEventListener("pointerdown", (e) => e.stopPropagation());
    close.addEventListener("click", (e) => {
      e.stopPropagation();
      closeTab(tab);
    });
    button.append(text, rows, close);
    button.addEventListener("click", () => select(tab));
    // A middle click closes, as in a browser.
    button.addEventListener("auxclick", (e) => e.button === 1 && closeTab(tab));
    tab.button = button;

    // Its panel: the views' tabs, the result's numbers, Export; then the views.
    const panel = el("div", "rs-panel", { role: "tabpanel", id: `rs-panel-${n}`, "aria-labelledby": `rs-tab-${n}`, hidden: "" });
    const tools = el("div", "rs-tools");
    const views = el("div", "rs-views", { role: "tablist", "aria-label": `Show ${label} as` });
    const viewTab = (name, title, icon) => {
      const b = el("button", "rs-view-tab", { type: "button", role: "tab", id: `rs-${n}-${name}`, "aria-controls": `rs-${n}-${name}-view`, "aria-selected": "false", tabindex: "-1" }, `${icon}<span>${title}</span>`);
      b.dataset.view = name;
      b.addEventListener("click", () => showView(tab, name));
      views.append(b);
      return b;
    };
    tab.viewTabs = {
      table: viewTab("table", "Table", ICON.table),
      graph: reply.graph ? viewTab("graph", "Graph", ICON.graph) : null,
      plan: viewTab("plan", "Plan", ICON.plan),
    };
    if (reply.graph) tab.viewTabs.graph.title = `Draw it as a graph: ${RULES[reply.graph] ?? reply.graph} (rule: ${reply.graph})`;
    rovingTabs(views, (b) => showView(tab, b.dataset.view));

    const meta = el("span", "rs-meta");
    const ms = reply.ms < 10 ? reply.ms.toFixed(1) : Math.round(reply.ms).toLocaleString("en-US");
    meta.innerHTML = `<b></b> <span class="sep">·</span> <span></span> <span class="sep">·</span> <span></span> <span class="sep">·</span> <span class="rs-id"></span>`;
    const [b, s1, s2, idText] = meta.querySelectorAll("b, span:not(.sep)");
    b.textContent = count(reply.rows, "row");
    s1.textContent = count(reply.columns.length, "column");
    s2.textContent = `${ms} ms`;
    idText.textContent = tab.id;
    idText.title = "The session keeps this result by this id, for fetch, export and graph.open";

    // Export: a button that opens its four formats.
    const exporter = el("div", "rs-export");
    const toggle = el("button", "rs-button", { type: "button", "aria-expanded": "false", "aria-controls": `rs-${n}-formats`, "aria-haspopup": "true" }, `${ICON.download}<span>Export</span>${ICON.chevron}`);
    const menu = el("div", "rs-formats", { id: `rs-${n}-formats`, role: "group", "aria-label": "Export as", hidden: "" });
    for (const f of FORMATS) {
      const item = el("button", "rs-format", { type: "button" });
      item.dataset.format = f.format;
      item.innerHTML = `<span>${f.name}</span><span class="ext">.${f.ext}</span>`;
      item.addEventListener("click", () => {
        openMenu(false);
        toggle.focus();
        exportAs(tab, f);
      });
      menu.append(item);
    }
    const openMenu = (open) => {
      menu.hidden = !open;
      toggle.setAttribute("aria-expanded", String(open));
      if (open) menu.querySelector("button").focus();
    };
    toggle.addEventListener("click", () => openMenu(menu.hidden));
    exporter.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && !menu.hidden) {
        e.preventDefault();
        e.stopPropagation();
        openMenu(false);
        toggle.focus();
      }
      if ((e.key === "ArrowDown" || e.key === "ArrowUp") && !menu.hidden) {
        const items = [...menu.querySelectorAll("button")];
        const at = items.indexOf(document.activeElement);
        items[(at + (e.key === "ArrowDown" ? 1 : -1) + items.length) % items.length].focus();
        e.preventDefault();
      }
    });
    exporter.addEventListener("focusout", (e) => {
      if (!exporter.contains(e.relatedTarget)) openMenu(false);
    });
    exporter.append(toggle, menu);
    tools.append(views, meta, exporter);

    // When the session no longer has the result.
    const gone = el("div", "rs-gone", { hidden: "" });
    const goneText = el("p");
    const again = el("button", "rs-button primary", { type: "button" }, `${ICON.again}<span>Run again</span>`);
    again.addEventListener("click", () => runAgain(tab));
    gone.append(el("span", "rs-gone-mark", { "aria-hidden": "true" }, "!"), goneText, again);
    tab.gone = { box: gone, text: goneText, again };

    const tableView = el("div", "rs-view rs-table-view", { role: "tabpanel", id: `rs-${n}-table-view`, "aria-labelledby": `rs-${n}-table` });
    const graphView = el("div", "rs-view rs-graph-view", { role: "tabpanel", id: `rs-${n}-graph-view`, "aria-labelledby": `rs-${n}-graph`, hidden: "" });
    const planView = el("div", "rs-view rs-plan-view", { role: "tabpanel", id: `rs-${n}-plan-view`, "aria-labelledby": `rs-${n}-plan`, hidden: "" });
    tab.views = { table: tableView, graph: graphView, plan: planView };

    // The graph view: the pane, while it is here, or what it is waiting for.
    const idle = el("div", "rs-idle");
    const idleText = el("p");
    const draw = el("button", "rs-button", { type: "button" }, `${ICON.graph}<span>Draw the graph</span>`);
    draw.addEventListener("click", () => drawGraph(tab));
    idle.append(idleText, draw);
    graphView.append(idle);
    tab.idle = { box: idle, text: idleText, draw };

    panel.append(tools, gone, tableView, graphView, planView);
    tab.panel = panel;

    tab.table = createTable({
      container: tableView,
      columns: reply.columns,
      rows: reply.rows,
      first: reply.arrow,
      label: `${label}: ${count(reply.rows, "row")}`,
      fetch: (offset, rows) => direct("fetch", { result: tab.id, offset, rows }).then((r) => r.arrow),
    });
    tab.table.on("error", (e) => failed(tab, e, "read its rows"));

    tablist.insertBefore(button, before?.button ?? null);
    panels.insertBefore(panel, before?.panel ?? null);
    tabs.splice(before ? tabs.indexOf(before) : tabs.length, 0, tab);
    update();
    return tab;
  }

  /** Show `tab`'s result. */
  function select(tab) {
    if (!tab) return;
    selected = tab;
    for (const t of tabs) {
      const on = t === tab;
      t.button.setAttribute("aria-selected", String(on));
      t.button.tabIndex = on ? 0 : -1;
      t.panel.hidden = !on;
    }
    tab.button.scrollIntoView({ block: "nearest", inline: "nearest" });
    showView(tab, tab.view);
  }

  /** Show `tab` as its table, its graph or its plan. */
  function showView(tab, name) {
    if (!tab.viewTabs[name]) name = "table";
    tab.view = name;
    for (const [view, b] of Object.entries(tab.viewTabs)) {
      if (!b) continue;
      const on = view === name;
      b.setAttribute("aria-selected", String(on));
      b.tabIndex = on ? 0 : -1;
      tab.views[view].hidden = !on;
    }
    if (tab !== selected) return;
    if (name === "graph") openGraph(tab);
    if (name === "plan") showPlan(tab);
  }

  /** Close `tab`: the session frees its result. */
  function closeTab(tab) {
    const at = tabs.indexOf(tab);
    if (at < 0) return Promise.resolve();
    const hadFocus = root.contains(document.activeElement);
    if (drawnFor === tab) {
      pane?.close();
      drawnFor = null;
      parking.append(paneWrap);
    }
    tabs.splice(at, 1);
    tab.button.remove();
    tab.panel.remove();
    tab.closed = true;
    const freed = tab.dropped ? Promise.resolve() : direct("close", { result: tab.id }).catch(() => {});
    if (selected === tab) {
      selected = null;
      const next = tabs[Math.min(at, tabs.length - 1)];
      if (next) select(next);
    }
    if (hadFocus) (selected?.button ?? document.getElementById("results"))?.focus();
    update();
    say(`Closed ${tab.label}`);
    return freed;
  }

  /** The session has no result for `tab`: say so, and offer to run it again. */
  function dropped(tab) {
    if (tab.dropped || tab.closed) return;
    tab.dropped = true;
    tab.button.classList.add("dropped");
    tab.gone.text.innerHTML = "";
    const what = el("b");
    what.textContent = `${tab.id} has gone from the session.`;
    tab.gone.text.append(what, " It was closed, or dropped to keep the session's last 16 results within 256 MB. What is loaded still shows; run the statement again for the rest.");
    tab.gone.box.hidden = false;
    tab.gone.box.setAttribute("role", "alert");
    say(`${tab.id} has gone from the session: run it again`);
  }

  /** An op on `tab`'s result failed. */
  function failed(tab, e, doing) {
    if (e?.kind === "not_found") dropped(tab);
    else if (e?.kind !== "cancelled") showMessage({ kind: "error", text: `Could not ${doing} of ${tab.id}: ${e?.message ?? e}` });
  }

  /** Run `tab`'s statement again, in its place. */
  async function runAgain(tab) {
    const r = await run(tab.sql, { replace: tab });
    return r;
  }

  // ---------------------------------------------------------------- the engine

  const crashed = () => Object.assign(new Error("not connected to the engine"), { kind: "crash", crash: true });
  /** An op straight to the session, not shown in the status bar (a page of rows). */
  const direct = (op, args) => (session.client ? session.client.call(op, args) : Promise.reject(crashed()));

  /**
   * Run `sql`: a tab for its result, or its notices for a statement with no
   * columns. With `replace`, the new tab takes that tab's place. Resolves to
   * {tab} or {notices} or {error}.
   */
  async function run(sql, { replace = null } = {}) {
    sql = String(sql ?? "");
    if (!sql.trim()) return { error: new Error("nothing to run") };
    const label = shorten(sql, 48);
    let reply;
    try {
      reply = await session.run("query", { sql, rows: PAGE }, { label: `query ${label}` });
    } catch (e) {
      const cancelled = e.kind === "cancelled";
      showMessage({ kind: cancelled ? "info" : "error", text: cancelled ? `Cancelled: ${label}` : e.message });
      if (!cancelled) say(`Error: ${e.message}`);
      return { error: e };
    }
    const ms = reply.ms < 10 ? reply.ms.toFixed(1) : Math.round(reply.ms).toLocaleString("en-US");
    if (reply.result === null || !reply.columns.length) {
      // CREATE, SET: no columns, no tab; what it said.
      const notices = reply.notices?.length ? reply.notices.join(" · ") : "OK";
      showMessage({ kind: "ok", text: `${label} · ${notices} · ${ms} ms` });
      say(`${notices}: ${label}`);
      return { notices: reply.notices ?? [] };
    }
    showMessage(reply.notices?.length ? { kind: "info", text: reply.notices.join(" · ") } : null);
    const tab = addTab(sql, reply, replace);
    if (replace) {
      if (replace === selected) selected = null;
      const was = replace.view;
      closeTab(replace);
      tab.view = was;
    }
    select(tab);
    say(`${tab.id}: ${count(reply.rows, "row")}, ${count(reply.columns.length, "column")}${reply.graph ? ", and a graph" : ""}`);
    return { tab: api(tab) };
  }

  // ---------------------------------------------------------------- export

  /** Write `tab`'s whole result as `f`, and download it. */
  async function exportAs(tab, f) {
    let r;
    try {
      r = await session.run("export", { format: f.format, result: tab.id }, { label: `export ${tab.id} as ${f.name}` });
    } catch (e) {
      failed(tab, e, `export as ${f.name}`);
      return null;
    }
    const name = `ursa-${tab.id}.${f.ext}`;
    const url = URL.createObjectURL(new Blob([r.bytes], { type: f.type }));
    const a = el("a", "", { href: url, download: name });
    a.hidden = true;
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
    say(`Exported ${tab.id} as ${name}, ${count(r.bytes.byteLength, "byte")}`);
    return name;
  }

  // ---------------------------------------------------------------- plan

  async function showPlan(tab) {
    if (tab.plan) return tab.plan;
    const view = tab.views.plan;
    view.replaceChildren(el("p", "rs-plan-wait", {}, "Asking for the plan…"));
    tab.plan = session.run("explain", { sql: tab.sql }, { label: `explain ${tab.label}` }).then(
      (r) => {
        const pre = el("pre", "rs-plan", { tabindex: "0", "aria-label": `The plan of ${tab.label}` });
        pre.textContent = r.plan;
        view.replaceChildren(pre);
        return r.plan;
      },
      (e) => {
        const p = el("p", "rs-plan-error", { role: "alert" });
        p.textContent = `No plan: ${e.message}`;
        view.replaceChildren(p);
        tab.plan = null;
        return null;
      },
    );
    return tab.plan;
  }

  // ---------------------------------------------------------------- graph

  // The page's one graph pane, made on the first Graph view, and the tab
  // whose result it draws. It lives in a wrapper (with the list of its
  // commands) that moves into that tab's Graph view; parked when unused.
  let pane = null;
  let drawnFor = null;
  /** The tab whose result the next graph.open draws, and how the last failed. */
  let target = null;
  let openError = null;
  const parking = el("div", "rs-parking", { hidden: "" });
  root.append(parking);
  const paneWrap = el("div", "rs-pane");
  const stepsList = el("ol", "rs-steps", { "aria-label": "Graph steps" });
  const stepsBox = el("div", "rs-steps-box", { hidden: "" });
  stepsBox.append(el("span", "rs-steps-title", {}, "Steps"), stepsList);
  paneWrap.append(stepsBox);
  parking.append(paneWrap);

  /** The pane's engine: graph.open draws the target's kept result. */
  const paneEngine = {
    call(op, args = {}) {
      if (op === "graph.open" && target) {
        const tab = target;
        return session.run(op, { ...args, result: tab.id }, { label: `graph of ${tab.id}` }).catch((e) => {
          openError = e;
          throw e;
        });
      }
      if (op === "graph.explore") return session.run(op, args, { label: args.command ?? op });
      return direct(op, args);
    },
  };

  /** The graph commands the pane asked for: {command, via, result, answer, dropped}, oldest first. */
  const steps = [];
  function logStep(step) {
    steps.push(step);
    renderSteps();
    return step;
  }
  function renderSteps() {
    stepsBox.hidden = steps.length === 0;
    stepsList.replaceChildren(
      ...steps.slice(-6).map((s) => {
        const li = el("li", s.error ? "error" : s.dropped ? "dropped" : "");
        const code = el("code");
        code.textContent = s.command;
        li.append(code);
        if (s.answer) {
          const a = el("span", "answer");
          a.textContent = s.answer;
          li.append(a);
        }
        li.title = `${s.command}${s.answer ? `\n${s.answer}` : ""}\n(from the ${s.via}, on ${s.result})`;
        return li;
      }),
    );
  }

  /** Run a graph command in the pane: {ok, out, err, error, replay} (graph-pane.js's exec). */
  async function exec(command) {
    try {
      return await pane.exec(command);
    } catch (e) {
      return { ok: false, out: "", err: "", error: e.message, replay: null };
    }
  }

  /**
   * Until History takes them (URSA-17): list each command, and run it in the
   * pane. A view's command for a drawing that has gone is listed, not run.
   */
  async function listCommands(command, { via, current, result, exec }) {
    const step = logStep({ command, via, result, answer: "", dropped: false, error: false });
    if (!current()) {
      step.dropped = true;
      step.answer = "not run: its drawing has gone";
      renderSteps();
      return false;
    }
    const r = await exec(command);
    step.answer = r.error ?? (r.out + r.err).trim().split(/\r?\n/)[0];
    step.error = !r.ok;
    renderSteps();
    return r.ok;
  }
  let sink = listCommands;

  /** Make the pane, the first time a Graph view is shown. */
  function ensurePane() {
    if (pane) return pane;
    pane = createPane({
      container: paneWrap,
      engine: paneEngine,
      prefix: "results-graph",
      commands: (command, { via, current }) => sink(command, { via, current, result: drawnFor?.id ?? null, sql: drawnFor?.sql ?? null, exec }),
      status: (text) => {
        if (drawnFor) idleSay(drawnFor, `The graph view stopped: ${text}`, true);
      },
      focus: () => (drawnFor?.viewTabs.graph ?? selected?.button)?.focus(),
    });
    // The steps under the pane, the pane over them.
    paneWrap.prepend(pane.element);
    // Closed by its own button or Esc: the view says so, and how to draw again.
    new MutationObserver(() => {
      if (drawnFor && pane.element.hidden && !drawing) idleSay(drawnFor, "The graph is closed.", false);
    }).observe(pane.element, { attributes: true, attributeFilter: ["hidden"] });
    return pane;
  }

  function idleSay(tab, text, error) {
    tab.idle.box.hidden = false;
    tab.idle.text.textContent = text;
    tab.idle.box.classList.toggle("error", Boolean(error));
    tab.idle.draw.hidden = false;
  }

  /** The Graph view of `tab` is shown: draw its result, unless the pane already does. */
  function openGraph(tab) {
    if (drawnFor === tab && pane?.isOpen()) {
      tab.views.graph.append(paneWrap);
      return Promise.resolve(true);
    }
    if (tab.idle.box.classList.contains("error") && drawnFor !== tab && tab.drawnOnce) return Promise.resolve(false);
    return drawGraph(tab);
  }

  let drawing = null;
  /** Draw `tab`'s result in the pane, moving the pane to its Graph view. */
  async function drawGraph(tab) {
    ensurePane();
    if (drawnFor && drawnFor !== tab && drawnFor.idle) idleSay(drawnFor, "The graph is drawn in another tab: one graph is drawn at a time.", false);
    tab.views.graph.append(paneWrap);
    tab.idle.box.hidden = false;
    tab.idle.box.classList.remove("error");
    tab.idle.text.textContent = "Drawing the graph…";
    tab.idle.draw.hidden = true;
    drawnFor = tab;
    target = tab;
    openError = null;
    tab.drawnOnce = true;
    const run = (drawing = exec(".graph"));
    const r = await run;
    if (drawing === run) drawing = null;
    target = null;
    if (drawnFor !== tab) return false;
    if (!r.ok) {
      if (openError?.kind === "not_found") dropped(tab);
      idleSay(tab, `Not drawn: ${r.error ?? r.err}`, true);
      drawnFor = null;
      return false;
    }
    tab.idle.box.hidden = pane.isOpen();
    // History, once it has the sink, lists the drawing itself (the session's `activity`).
    if (sink === listCommands) logStep({ command: `.graph`, via: "results", result: tab.id, answer: r.out.trim().split(/\r?\n/)[0], dropped: false, error: false });
    say(r.out.trim().split(/\r?\n/)[0]);
    return true;
  }

  // ---------------------------------------------------------------- the connection

  // After a reconnect, rows that failed to load are asked for again (a
  // result that has gone then says so).
  session.on("connect", () => {
    for (const tab of tabs) tab.table.retry();
  });

  // Delete closes the tab with the keyboard.
  rovingTabs(tablist, (button) => select(tabs.find((t) => t.button === button)), {
    remove: (button) => closeTab(tabs.find((t) => t.button === button)),
  });

  update();

  /** What a test or another panel sees of a tab. */
  function api(tab) {
    return {
      get id() {
        return tab.id;
      },
      get sql() {
        return tab.sql;
      },
      get rows() {
        return tab.reply.rows;
      },
      get columns() {
        return tab.reply.columns;
      },
      get graph() {
        return tab.reply.graph ?? null;
      },
      get view() {
        return tab.view;
      },
      get dropped() {
        return tab.dropped;
      },
      get closed() {
        return Boolean(tab.closed);
      },
      table: tab.table,
      element: tab.panel,
      button: tab.button,
      select: () => select(tab),
      show: (view) => (select(tab), showView(tab, view)),
      draw: () => drawGraph(tab),
      plan: () => showPlan(tab),
      export: (format) => exportAs(tab, FORMATS.find((f) => f.format === format)),
      close: () => closeTab(tab),
      runAgain: () => runAgain(tab),
    };
  }

  return {
    run,
    /** The open tabs, in order. */
    tabs: () => tabs.map(api),
    /** The tab shown, or null. */
    selected: () => (selected ? api(selected) : null),
    /**
     * Where the graph pane's commands go: fn(command, {via, current, result,
     * sql, exec}), resolving once it has run, as graph-pane.js's `commands`.
     * `exec(command)` runs it in the pane. History (URSA-17) takes this;
     * without it the Results panel lists them under the graph (the Steps
     * list), which null puts back.
     */
    setCommandsSink(fn) {
      sink = fn ?? listCommands;
      stepsBox.hidden = sink !== listCommands || steps.length === 0;
    },
    /** The commands the default sink listed. */
    steps,
    /** The graph pane, once made. */
    pane: () => pane,
    /** The tab the pane draws, or null. */
    drawn: () => (drawnFor ? api(drawnFor) : null),
  };
}
