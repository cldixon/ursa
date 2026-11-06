import { EXAMPLES, GRAPHS } from "./examples.js";

const $ = (id) => document.getElementById(id);
// Props are assigned as properties, except aria* ones, set as attributes
// (ariaSelected becomes aria-selected), which every browser supports.
const el = (tag, props = {}, ...children) => {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key.startsWith("aria")) node.setAttribute(`aria-${key.slice(4).toLowerCase()}`, value);
    else node[key] = value;
  }
  node.append(...children.filter((c) => c !== null && c !== undefined));
  return node;
};

const editor = $("sql");
const runButton = $("run");
const engineChip = $("engine");
const sessionChip = $("session");
const status = $("status");
const output = $("output");
const MAX_ROWS = 1000;

// ---------------------------------------------------------------- engine

const worker = new Worker(new URL("./engine-worker.js", import.meta.url), { type: "module" });
let nextId = 0;
const pending = new Map();
let engineReady;
const engineStarted = new Promise((resolve, reject) => (engineReady = { resolve, reject }));

worker.onmessage = ({ data }) => {
  if ("ready" in data) {
    if (data.ready) engineReady.resolve(data);
    else engineReady.reject(new Error(data.error));
    return;
  }
  const call = pending.get(data.id);
  pending.delete(data.id);
  if (data.ok) call.resolve(data);
  else call.reject(new Error(data.error));
};

function engine(op, args) {
  return new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    worker.postMessage({ id, op, ...args });
  });
}

// ---------------------------------------------------------------- state

// The editor runs in the workspace, or in the Tour's session at one step.
let mode = { kind: "workspace" };
let tour = [];
let tourRanThrough = -1; // the last Tour step run in the Tour's session
let shown = { session: "workspace", results: [], index: 0 };
let view = "table";
let busy = false;

function setMode(next) {
  mode = next;
  if (mode.kind === "tour") {
    sessionChip.textContent = `Tour · step ${mode.step + 1} of ${tour.length}`;
    sessionChip.classList.add("tour");
    sessionChip.title = "Runs in the Tour's own session; earlier steps run first";
  } else {
    sessionChip.textContent = "Workspace";
    sessionChip.classList.remove("tour");
    sessionChip.title = "The workspace session: the sample graphs are defined here";
  }
  renderTour();
}

// ---------------------------------------------------------------- running

function selectedSql() {
  const { selectionStart: a, selectionEnd: b, value } = editor;
  return a !== b ? value.slice(a, b) : value;
}

async function run(sql = selectedSql()) {
  if (busy || !sql.trim()) return;
  busy = true;
  runButton.disabled = true;
  status.classList.remove("error");
  const session = mode.kind === "tour" ? "tour" : "workspace";
  try {
    if (mode.kind === "tour") await runTourPrerequisites(mode.step);
    status.textContent = "Running…";
    const { results, ms } = await engine("run", { session, sql, maxRows: MAX_ROWS });
    if (mode.kind === "tour") tourRanThrough = Math.max(tourRanThrough, mode.step);
    remember(sql);
    if (mode.kind === "workspace") history.replaceState(null, "", `#${new URLSearchParams({ sql })}`);
    show(session, results, ms);
  } catch (e) {
    showError(e.message);
  } finally {
    busy = false;
    runButton.disabled = false;
    renderTour();
  }
}

/** Run the Tour's steps before `step` in its session, from a fresh session when needed. */
async function runTourPrerequisites(step) {
  if (step <= tourRanThrough) {
    // Running an earlier step again: start over, so its CREATEs do not collide.
    await engine("reset", { session: "tour" });
    tourRanThrough = -1;
  }
  if (tourRanThrough < step - 1) {
    const before = tour.slice(tourRanThrough + 1, step);
    status.textContent = `Running steps ${tourRanThrough + 2}–${step} first…`;
    try {
      await engine("run", { session: "tour", sql: before.map((s) => s.sql).join("\n"), maxRows: 0 });
    } catch (e) {
      throw new Error(`An earlier Tour step failed: ${e.message}`);
    }
    tourRanThrough = step - 1;
  }
}

// ---------------------------------------------------------------- results

const NUMERIC = /^(U?Int|Float|Decimal)/;

function statementLabel(text, i) {
  const words = text.replace(/\s+/g, " ").trim();
  return `${i + 1} · ${words.length > 28 ? words.slice(0, 27) + "…" : words}`;
}

function show(session, results, ms) {
  // Show the last result with columns (the one a reader is after), or the last one.
  let index = results.length - 1;
  for (let i = results.length - 1; i >= 0; i--) {
    if (results[i].columns.length) {
      index = i;
      break;
    }
  }
  shown = { session, results, index, ms };
  renderResults();
}

function showError(message) {
  shown = { session: shown.session, results: [], index: 0 };
  $("result-tabs").replaceChildren();
  $("download").disabled = true;
  status.textContent = "Error";
  status.classList.add("error");
  output.replaceChildren(el("pre", { className: "error", textContent: message }));
}

function renderResults() {
  const { results, index, ms } = shown;
  const tabs = $("result-tabs");
  tabs.replaceChildren(
    ...(results.length > 1
      ? results.map((r, i) =>
          el("button", {
            role: "tab",
            textContent: statementLabel(r.statement, i),
            title: r.statement,
            ariaSelected: String(i === index),
            onclick: () => {
              shown.index = i;
              renderResults();
            },
          }),
        )
      : []),
  );
  const r = results[index];
  if (!r) {
    output.replaceChildren(el("p", { className: "empty", textContent: "Nothing to show." }));
    return;
  }
  const time = ms < 1000 ? `${ms.toFixed(0)} ms` : `${(ms / 1000).toFixed(2)} s`;
  const statements = results.length > 1 ? `${results.length} statements in ${time}` : time;
  if (!r.columns.length) {
    status.textContent = `Done · ${statements}`;
    output.replaceChildren(el("p", { className: "empty", textContent: "The statement ran. It returns no rows." }));
    $("download").disabled = true;
    return;
  }
  const rows = `${r.totalRows.toLocaleString()} row${r.totalRows === 1 ? "" : "s"}`;
  const capped = r.totalRows > r.rows.length ? ` · showing the first ${r.rows.length.toLocaleString()}; download for all` : "";
  status.textContent = `${rows} · ${statements}${capped}`;
  $("download").disabled = false;
  if (view === "text") {
    output.replaceChildren(el("pre", { textContent: r.table }));
    return;
  }
  const numeric = r.columns.map((c) => NUMERIC.test(c.type));
  const head = el(
    "tr",
    {},
    ...r.columns.map((c) => el("th", { scope: "col" }, c.name, el("span", { className: "type", textContent: c.type }))),
  );
  const body = el("tbody");
  for (const row of r.rows) {
    body.append(
      el(
        "tr",
        {},
        ...row.map((v, i) =>
          v === null
            ? el("td", { className: "null", textContent: "NULL" })
            : el("td", { className: numeric[i] ? "num" : "", textContent: v, title: v.length > 40 ? v : "" }),
        ),
      ),
    );
  }
  output.replaceChildren(el("table", { className: "grid" }, el("thead", {}, head), body));
}

function setView(next) {
  view = next;
  $("view-table").setAttribute("aria-pressed", String(view === "table"));
  $("view-text").setAttribute("aria-pressed", String(view === "text"));
  if (shown.results.length) renderResults();
}
$("view-table").onclick = () => setView("table");
$("view-text").onclick = () => setView("text");

const EXTENSIONS = { csv: "csv", parquet: "parquet", json: "json", arrow: "arrows" };
const TYPES = { csv: "text/csv", parquet: "application/vnd.apache.parquet", json: "application/json", arrow: "application/vnd.apache.arrow.stream" };
$("download").onchange = async (e) => {
  const format = e.target.value;
  e.target.value = "";
  if (!format) return;
  try {
    const { bytes } = await engine("export", { session: shown.session, index: shown.index, format });
    const url = URL.createObjectURL(new Blob([bytes], { type: TYPES[format] }));
    el("a", { href: url, download: `ursa-result.${EXTENSIONS[format]}` }).click();
    setTimeout(() => URL.revokeObjectURL(url), 10_000);
  } catch (err) {
    showError(err.message);
  }
};

// ---------------------------------------------------------------- editor

runButton.onclick = () => run();
let tabMovesFocus = false;
editor.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    run();
  } else if (e.key === "Escape") {
    // Escape, then Tab, leaves the editor (Tab otherwise indents).
    tabMovesFocus = true;
  } else if (e.key === "Tab" && !tabMovesFocus && !e.shiftKey) {
    e.preventDefault();
    editor.setRangeText("  ", editor.selectionStart, editor.selectionEnd, "end");
  } else {
    tabMovesFocus = false;
  }
});

function load(sql, nextMode) {
  setMode(nextMode);
  editor.value = sql;
  editor.scrollTop = 0;
}

$("share").onclick = async () => {
  const url = `${location.origin}${location.pathname}#${new URLSearchParams({ sql: editor.value })}`;
  try {
    await navigator.clipboard.writeText(url);
    $("share").textContent = "Copied";
  } catch {
    history.replaceState(null, "", url);
    $("share").textContent = "Link in the address bar";
  }
  setTimeout(() => ($("share").textContent = "Copy link"), 1500);
};

// ---------------------------------------------------------------- panels

for (const tab of document.querySelectorAll("[role=tab][data-panel]")) {
  tab.onclick = () => {
    for (const t of document.querySelectorAll("[role=tab][data-panel]")) {
      t.setAttribute("aria-selected", String(t === tab));
      $(`panel-${t.dataset.panel}`).hidden = t !== tab;
    }
    if (tab.dataset.panel === "functions") loadFunctions();
    if (tab.dataset.panel === "history") renderHistory();
  };
}

function item(title, sub, onclick, { code = false, className = "" } = {}) {
  return el(
    "button",
    { className: `item ${className}`.trim(), onclick },
    el("span", { className: "title", textContent: title }),
    sub ? el(code ? "code" : "span", { className: "sub", textContent: sub }) : null,
  );
}

// Tour
function renderTour() {
  $("tour").replaceChildren(
    ...tour.map((step, i) => {
      const classes = [i <= tourRanThrough ? "done" : "", mode.kind === "tour" && mode.step === i ? "current" : ""];
      return el(
        "li",
        {},
        item(step.title, step.note, () => {
          load(step.sql, { kind: "tour", step: i });
          run(step.sql);
        }, { className: classes.join(" ") }),
      );
    }),
  );
}

// Examples
$("examples").replaceChildren(
  ...EXAMPLES.map((e) =>
    el("li", {}, item(e.title, e.sql.split("\n").find((l) => !l.startsWith("--")), () => {
      load(e.sql, { kind: "workspace" });
      run(e.sql);
    }, { code: true })),
  ),
);

// Data
function query(sql) {
  load(sql, { kind: "workspace" });
  run(sql);
}
$("graphs").replaceChildren(
  ...GRAPHS.map((g) =>
    el(
      "li",
      {},
      el(
        "div",
        { className: "item" },
        el("span", { className: "title", textContent: g.name }),
        el("span", { className: "sub", textContent: g.description }),
        el(
          "div",
          { className: "actions" },
          el("button", { textContent: "Edges", onclick: () => query(`SELECT * FROM ${g.name}.edges LIMIT 100;`) }),
          el("button", { textContent: "Nodes", onclick: () => query(`SELECT * FROM ${g.name}.nodes LIMIT 100;`) }),
          el("button", { textContent: "Describe", onclick: () => query(`SELECT * FROM describe(${g.name});`) }),
          el("button", { textContent: "Definition", onclick: () => query(`DESCRIBE GRAPH ${g.name};`) }),
        ),
      ),
    ),
  ),
);
fetch("/data/manifest.json")
  .then((r) => r.json())
  .then((m) => {
    $("files").replaceChildren(
      ...m.datasets.flatMap((d) =>
        d.files
          .filter((f) => f.name.endsWith(".parquet"))
          .map((f) =>
            el("li", {}, item(f.name, `${d.title} · ${formatBytes(f.bytes)} · ${d.source}`, () => query(`SELECT * FROM '${f.name}' LIMIT 100;`))),
          ),
      ),
    );
    $("licence").textContent = m.licence;
  })
  .catch(() => ($("files").textContent = "The dataset list did not load."));

function formatBytes(n) {
  return n < 1024 ? `${n} B` : n < 1024 ** 2 ? `${(n / 1024).toFixed(1)} KB` : `${(n / 1024 ** 2).toFixed(1)} MB`;
}

// Functions
const TRY = {
  algorithms: "SELECT * FROM algorithms(karate, pagerank(direction => 'both'), community => louvain(seed => 1)) LIMIT 10;",
  neighbors_agg: "SELECT * FROM neighbors_agg(karate, attribute => club, agg => 'n_unique', direction => 'both') LIMIT 10;",
  hop: "SELECT * FROM hop(karate, seeds => [0], n => 2, direction => 'both') LIMIT 20;",
  shortest_path: "SELECT * FROM shortest_path(karate, source => 0, target => 33, direction => 'both');",
  random_walk: "SELECT * FROM random_walk(karate, start => [0], steps => 5, walks_per_node => 3, seed => 1);",
};
let functionsLoaded = false;
async function loadFunctions() {
  if (functionsLoaded) return;
  functionsLoaded = true;
  try {
    await engineStarted;
    await engine("run", { session: "meta", sql: "SELECT name, family, summary, parameters, outputs FROM ursa_functions()", maxRows: 0 });
    const { bytes } = await engine("export", { session: "meta", index: 0, format: "json" });
    const functions = JSON.parse(new TextDecoder().decode(bytes));
    const list = functions.map((f) => {
      const output = f.outputs?.[0]?.name ?? f.name;
      const sql =
        TRY[f.name] ??
        (f.family === "algorithm"
          ? `SELECT * FROM ${f.name}(karate) ORDER BY ${output} DESC LIMIT 10;`
          : `SELECT * FROM ${f.name}(karate);`);
      const node = el(
        "details",
        { className: "fn" },
        el(
          "summary",
          {},
          el("span", { className: "name", textContent: f.name }),
          el("span", { className: "family", textContent: f.family }),
        ),
        el("p", { textContent: f.summary }),
        el(
          "dl",
          {},
          ...(f.parameters ?? []).flatMap((p) => [
            el("dt", { textContent: `${p.name} => ${p.default}` }),
            el("dd", { textContent: `${p.type}${p.choices?.length ? ` (${p.choices.join(", ")})` : ""}. ${p.doc}` }),
          ]),
        ),
        el("div", { className: "actions" }, el("button", { textContent: "Try it", onclick: () => query(sql) })),
      );
      node.dataset.search = `${f.name} ${f.family} ${f.summary}`.toLowerCase();
      return node;
    });
    $("functions").replaceChildren(...list);
  } catch (e) {
    functionsLoaded = false;
    $("functions").replaceChildren(el("p", { className: "muted", textContent: `The functions did not load: ${e.message}` }));
  }
}
$("function-filter").oninput = (e) => {
  const q = e.target.value.toLowerCase();
  for (const node of $("functions").querySelectorAll(".fn")) node.hidden = !node.dataset.search.includes(q);
};

// History (this browser only; storage may be unavailable, e.g. in a private window)
const HISTORY_KEY = "ursa-shell-history";
function readHistory() {
  try {
    return JSON.parse(localStorage.getItem(HISTORY_KEY) ?? "[]");
  } catch {
    return [];
  }
}
function remember(sql) {
  const entries = readHistory().filter((h) => h.sql !== sql);
  entries.unshift({ sql, at: Date.now() });
  try {
    localStorage.setItem(HISTORY_KEY, JSON.stringify(entries.slice(0, 100)));
  } catch {}
  if (!$("panel-history").hidden) renderHistory();
}
function renderHistory() {
  const entries = readHistory();
  $("history").replaceChildren(
    ...(entries.length
      ? entries.map((h) =>
          el("li", {}, item(new Date(h.at).toLocaleString(), h.sql.replace(/\s+/g, " ").slice(0, 120), () => load(h.sql, { kind: "workspace" }), { code: true })),
        )
      : [el("li", { className: "muted", textContent: "Nothing yet." })]),
  );
}
$("clear-history").onclick = () => {
  try {
    localStorage.removeItem(HISTORY_KEY);
  } catch {}
  renderHistory();
};

// ---------------------------------------------------------------- start

const linked = new URLSearchParams(location.hash.slice(1)).get("sql");
editor.value = linked ?? EXAMPLES[0].sql;

fetch("/tour.json")
  .then((r) => r.json())
  .then((steps) => {
    tour = steps;
    renderTour();
  })
  .catch(() => ($("tour").textContent = "The Tour did not load."));

engineStarted.then(
  ({ version, ms }) => {
    engineChip.textContent = `Ursa ${version} · in your browser · ready in ${(ms / 1000).toFixed(1)} s`;
    runButton.disabled = false;
    status.textContent = "Ready. Run the query, or pick a Tour step or an example.";
    if (linked) run(linked);
  },
  (e) => {
    engineChip.textContent = "The engine failed to start";
    showError(e.message);
  },
);
