// The Ursa Web Shell: a terminal over Ursa's engine, running in a Web Worker.
//
// Type SQL at the `ursa>` prompt: Enter runs a statement that ends with `;`
// (Shift+Enter adds a line), and a dot-command at once. The Datasets menu
// runs a dataset's commands at the prompt as if typed. Share copies a link
// that replays the session's commands.
import { DATASETS } from "./datasets.js";

const $ = (id) => document.getElementById(id);
const el = (tag, props = {}, ...children) => {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key.startsWith("aria")) node.setAttribute(`aria-${key.slice(4).toLowerCase()}`, value);
    else node[key] = value;
  }
  node.append(...children.filter((c) => c !== null && c !== undefined && c !== false));
  return node;
};

const terminal = $("terminal");
const log = $("log");
const input = $("input");
const highlight = $("highlight");
const prompts = $("prompts");
const inputLine = $("input-line");

const PROMPT = "ursa> ";
const CONTINUATION = " ...> ";
const DOCS = "https://cldixon.github.io/ursa/sql/";

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

function engine(op, args, transfer = []) {
  return new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    worker.postMessage({ id, op, ...args }, transfer);
  });
}

// ---------------------------------------------------------------- settings

// The engine keeps at most this many rows of a result for the page: in effect
// every row of a listing.
const ALL_ROWS = 1_000_000;
const settings = { maxRows: 40, timer: false };
// The commands this session ran, for Share; and where `.export` reads from.
let ran = [];
let lastResult = null; // {index} of the last result with columns
let uploads = []; // names added with Import

// ---------------------------------------------------------------- theme

function currentTheme() {
  const set = document.documentElement.dataset.theme;
  if (set) return set;
  return matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}
$("theme").onclick = () => {
  const next = currentTheme() === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = next;
  try {
    localStorage.setItem("ursa-theme", next);
  } catch {}
};

// ---------------------------------------------------------------- highlighting

const KEYWORDS = new Set(
  (
    "select from where group by order having limit offset join left right inner outer full cross on using as and or not in is null " +
    "case when then else end distinct union all intersect except with recursive create replace view table drop if exists insert into values " +
    "delete update set describe show explain analyze copy to property graph graphs vertex edge tables key source destination references label " +
    "properties no are columns default graph_table match shortest any trail simple acyclic walk asc desc nulls first last true false cast " +
    "between like ilike over partition rows range filter attach database"
  ).split(" "),
);

function escapeHtml(s) {
  return s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
}

/** SQL (or a dot-command) as HTML with its keywords, strings, numbers and comments marked. */
function highlightSql(text) {
  if (/^\s*\./.test(text)) {
    const m = text.match(/^(\s*\.\S*)(.*)$/s);
    return `<span class="d">${escapeHtml(m[1])}</span><span class="a">${escapeHtml(m[2])}</span>`;
  }
  const token = /(--[^\n]*|\/\*[\s\S]*?(?:\*\/|$))|('(?:[^']|'')*'?)|("(?:[^"]|"")*"?)|(\b\d+(?:\.\d+)?\b)|([A-Za-z_][A-Za-z0-9_]*)|([\s\S])/g;
  let out = "";
  for (const m of text.matchAll(token)) {
    const [all, comment, string, quoted, number, word] = m;
    if (comment) out += `<span class="c">${escapeHtml(comment)}</span>`;
    else if (string) out += `<span class="s">${escapeHtml(string)}</span>`;
    else if (quoted) out += escapeHtml(quoted);
    else if (number) out += `<span class="n">${number}</span>`;
    else if (word) out += KEYWORDS.has(word.toLowerCase()) ? `<span class="k">${escapeHtml(word)}</span>` : escapeHtml(word);
    else out += escapeHtml(all);
  }
  return out;
}

/** The prompt column for `lines` lines: `ursa> ` then ` ...> `. */
function promptsFor(lines) {
  return [PROMPT, ...Array(Math.max(0, lines - 1)).fill(CONTINUATION)].join("\n");
}

// ---------------------------------------------------------------- the input

function syncInput() {
  // The trailing newline keeps the highlight as tall as the textarea.
  highlight.innerHTML = highlightSql(input.value) + "\n";
  prompts.textContent = promptsFor(input.value.split("\n").length);
}
input.addEventListener("input", syncInput);

/** Whether `sql` ends a statement: a `;` outside strings and comments. */
function isComplete(sql) {
  const stripped = sql.replace(/--[^\n]*|\/\*[\s\S]*?\*\/|'(?:[^']|'')*'|"(?:[^"]|"")*"/g, " ");
  return /;\s*$/.test(stripped);
}

const HISTORY_KEY = "ursa-shell-history";
function readHistory() {
  try {
    return JSON.parse(localStorage.getItem(HISTORY_KEY) ?? "[]");
  } catch {
    return [];
  }
}
let history = readHistory();
let historyAt = history.length;
let draft = "";
function remember(command) {
  history = history.filter((h) => h !== command);
  history.push(command);
  history = history.slice(-200);
  historyAt = history.length;
  try {
    localStorage.setItem(HISTORY_KEY, JSON.stringify(history));
  } catch {}
}

input.addEventListener("keydown", (e) => {
  const value = input.value;
  if (e.key === "Enter" && !e.shiftKey) {
    const command = value.trim();
    if (!command) {
      e.preventDefault();
      echo("");
      return;
    }
    if (command.startsWith(".") || isComplete(value) || e.ctrlKey || e.metaKey) {
      e.preventDefault();
      input.value = "";
      syncInput();
      submit(command);
    }
    // Otherwise the newline goes in: the statement continues.
  } else if (e.key === "ArrowUp" && !value.slice(0, input.selectionStart).includes("\n")) {
    if (historyAt === 0) return;
    if (historyAt === history.length) draft = value;
    historyAt--;
    setInput(history[historyAt]);
    e.preventDefault();
  } else if (e.key === "ArrowDown" && !value.slice(input.selectionEnd).includes("\n")) {
    if (historyAt >= history.length) return;
    historyAt++;
    setInput(historyAt === history.length ? draft : history[historyAt]);
    e.preventDefault();
  } else if (e.key === "c" && e.ctrlKey && input.selectionStart === input.selectionEnd) {
    // Ctrl-C drops what was typed, as in a terminal.
    echo(value, "^C");
    setInput("");
    e.preventDefault();
  } else if (e.key === "l" && e.ctrlKey) {
    log.replaceChildren();
    e.preventDefault();
  } else if (e.key === "Tab") {
    e.preventDefault();
    input.setRangeText("  ", input.selectionStart, input.selectionEnd, "end");
    syncInput();
  }
});

function setInput(text) {
  input.value = text;
  input.setSelectionRange(text.length, text.length);
  syncInput();
}

// A click on the terminal (not a selection) puts the cursor in the input.
terminal.addEventListener("mouseup", () => {
  if (!getSelection().toString() && !input.disabled) input.focus({ preventScroll: true });
});

// ---------------------------------------------------------------- output

function scrollDown() {
  terminal.scrollTop = terminal.scrollHeight;
}

/** A command at the prompt, as it was typed. */
function echo(command, suffix = "") {
  const lines = command.split("\n").length;
  const code = el("pre");
  code.innerHTML = highlightSql(command) + (suffix ? `<span class="muted">${escapeHtml(suffix)}</span>` : "");
  log.append(el("div", { className: "echo" }, el("pre", { className: "prompts", textContent: promptsFor(lines) }), code));
  scrollDown();
}

function say(text, className = "") {
  log.append(el("pre", { className: `line ${className}`.trim(), textContent: text }));
  scrollDown();
}

function error(message) {
  say(message.startsWith("Error") ? message : `Error: ${message}`, "err");
}

const NUMERIC = /^(U?Int|Float|Decimal)/;
const LONG = 60;

/** An Arrow type as a short SQL-ish name, as DuckDB's shell labels columns. */
function typeName(type) {
  if (/^(Large)?Utf8(View)?$/.test(type)) return "varchar";
  if (type === "Float64") return "double";
  if (type === "Float32") return "float";
  if (type === "Boolean") return "boolean";
  if (type === "Date32" || type === "Date64") return "date";
  if (type.startsWith("Timestamp")) return "timestamp";
  if (/^(Large)?List/.test(type)) {
    // Arrow prints `List(non-null Int64)` or `List(Field { …, data_type: Int64, … })`.
    const inner = type.match(/List\((?:non-null |nullable )?(?:Field \{.*?data_type: )?([A-Za-z0-9]+)/);
    return inner ? `${typeName(inner[1])}[]` : "list";
  }
  if (type.startsWith("Struct")) return "struct";
  if (type.startsWith("Decimal")) return type.toLowerCase().replace(/^decimal\d*/, "decimal");
  return type.toLowerCase();
}

function plural(n, word) {
  return `${n.toLocaleString("en-US")} ${word}${n === 1 ? "" : "s"}`;
}

/** One result: a table, or nothing for a statement with no columns. */
function renderResult(r) {
  if (!r.columns.length) return;
  const numeric = r.columns.map((c) => NUMERIC.test(c.type));
  const cell = (v, i) =>
    v === null
      ? el("td", { className: "null", textContent: "NULL" })
      : el("td", {
          className: [numeric[i] ? "num" : "", v.length > LONG ? "long" : ""].join(" ").trim(),
          textContent: v,
          title: v.length > LONG ? v : "",
        });
  const body = el("tbody");
  for (const row of r.rows) body.append(el("tr", {}, ...row.map(cell)));
  const tail = r.tail ?? [];
  if (tail.length) {
    for (let i = 0; i < 3; i++) body.append(el("tr", { className: "gap" }, ...r.columns.map(() => el("td", { textContent: "·" }))));
    for (const row of tail) body.append(el("tr", {}, ...row.map(cell)));
  }
  const head = el(
    "tr",
    {},
    ...r.columns.map((c) => el("th", { scope: "col" }, c.name, el("span", { className: "type", textContent: typeName(c.type) }))),
  );
  const shown = r.rows.length + tail.length;
  const footer =
    r.totalRows > shown || r.totalRows === 0
      ? el(
          "div",
          { className: "footer" },
          el("span", { textContent: r.totalRows === 0 ? "0 rows" : `${plural(r.totalRows, "row")} (${shown} shown)` }),
          el("span", { textContent: plural(r.columns.length, "column") }),
        )
      : null;
  log.append(el("div", { className: "result" }, el("table", { className: "box" }, el("thead", {}, head), body), footer));
}

function formatMs(ms) {
  return ms < 1000 ? `${ms.toFixed(0)} ms` : `${(ms / 1000).toFixed(2)} s`;
}

// ---------------------------------------------------------------- running

// Commands run one at a time, in order, whether typed or from a dataset. A
// batch (a dataset's commands, a shared link's) is queued at once and stops at
// its first error: the commands after it are skipped.
let queue = Promise.resolve();
function submit(command, { record = true, batch = null } = {}) {
  const run = queue.then(() => (batch?.failed ? undefined : execute(command, record)));
  queue = run.catch(() => {
    if (batch) batch.failed = true;
  });
  return run;
}

/** Queue `commands` as one batch; resolves when they have all run or one failed. */
function submitAll(commands) {
  const batch = { failed: false };
  const runs = commands.map((command) => submit(command, { batch }));
  return Promise.allSettled(runs);
}

async function execute(command, record) {
  echo(command);
  if (record) remember(command);
  setBusy(true);
  try {
    await engineStarted;
    if (command.startsWith(".")) await dotCommand(command, record);
    else {
      await runSql(command);
      if (record) ran.push(command);
    }
  } catch (e) {
    error(e.message);
    throw e;
  } finally {
    setBusy(false);
    scrollDown();
  }
}

async function runSql(sql, { session = "shell", maxRows = settings.maxRows } = {}) {
  const { results, ms } = await engine("run", { session, sql, maxRows });
  results.forEach((r, i) => {
    renderResult(r);
    if (r.columns.length) lastResult = { index: i };
  });
  if (settings.timer) say(`Run Time: ${formatMs(ms)}`, "muted");
  return results;
}

function setBusy(busy) {
  input.disabled = busy;
  inputLine.classList.toggle("busy", busy);
  if (!busy) input.focus({ preventScroll: true });
}

// ---------------------------------------------------------------- dot-commands

const COMMANDS = [
  [".clear", "", "Clear the screen (also Ctrl-L)"],
  [".describe", "NAME", "Describe a graph, table or view"],
  [".export", "FORMAT", "Download the last result: csv, parquet, json or arrow"],
  [".files", "[add]", "List the dataset files and imported files; add opens a file"],
  [".functions", "[NAME]", "List the graph functions, or show one's parameters"],
  [".graphs", "", "List the graphs (SHOW GRAPHS)"],
  [".help", "", "Show this message"],
  [".maxrows", "N", "Show at most N rows of a table (default 40; 0 shows all)"],
  [".share", "", "Copy a link that replays this session"],
  [".tables", "", "List the tables and views"],
  [".timer", "on|off", "Show how long each statement takes"],
];

const FUNCTIONS = `SELECT name, family,
       CASE WHEN strpos(summary, '. ') > 0 THEN split_part(summary, '. ', 1) || '.' ELSE summary END AS summary
FROM ursa_functions();`;

async function dotCommand(command, record) {
  const [name, ...rest] = command.replace(/;\s*$/, "").trim().split(/\s+/);
  const arg = rest.join(" ");
  switch (name.toLowerCase()) {
    case ".help":
      log.append(
        el(
          "div",
          { className: "help" },
          ...COMMANDS.flatMap(([cmd, args, doc]) => [
            el("span", { className: "cmd", textContent: cmd }),
            el("span", { className: "muted", textContent: args }),
            el("span", { textContent: doc }),
          ]),
        ),
      );
      say(`End SQL with ; to run it (Shift+Enter adds a line). The Datasets menu loads a sample graph and queries it.\nThe SQL reference: ${DOCS}`, "muted");
      return;
    case ".clear":
      log.replaceChildren();
      return;
    case ".tables":
      return tables();
    case ".graphs":
      await runSql("SHOW GRAPHS;");
      return;
    case ".describe":
      if (!arg) throw new Error("usage: .describe NAME (a graph, table or view)");
      return describe(arg);
    case ".functions":
      if (arg) return describeFunction(arg);
      await runSql(FUNCTIONS, { maxRows: ALL_ROWS });
      return;
    case ".maxrows": {
      const n = Number(arg);
      if (!Number.isInteger(n) || n < 0) throw new Error("usage: .maxrows N (0 shows every row)");
      settings.maxRows = n === 0 ? ALL_ROWS : n;
      say(n === 0 ? "Tables show every row." : `Tables show at most ${n} rows.`, "muted");
      if (record) ran.push(command);
      return;
    }
    case ".timer":
      if (arg !== "on" && arg !== "off") throw new Error("usage: .timer on|off");
      settings.timer = arg === "on";
      if (record) ran.push(command);
      return;
    case ".export":
      return exportLast(arg.toLowerCase());
    case ".files":
      if (arg === "add") return pickFiles();
      if (arg) throw new Error("usage: .files [add]");
      return listFiles();
    case ".share":
      return share();
    default:
      throw new Error(`unknown command ${name}; .help lists them`);
  }
}

async function graphNames() {
  const { results } = await engine("run", { session: "shell", sql: "SHOW GRAPHS;", maxRows: 10_000 });
  const r = results[0];
  const i = r.columns.findIndex((c) => c.name === "name");
  return new Set(r.rows.map((row) => row[i]));
}

/** `.tables`: the tables and views, leaving out the graphs' own schemas (which `.graphs` lists). */
async function tables() {
  const graphs = await graphNames();
  const excluded = ["information_schema", ...graphs].map((g) => `'${g.replace(/'/g, "''")}'`).join(", ");
  await runSql(
    `SELECT CASE WHEN table_schema = 'public' THEN table_name ELSE table_schema || '.' || table_name END AS name,
       CASE WHEN table_type = 'VIEW' THEN 'view' ELSE 'table' END AS type
FROM information_schema.tables
WHERE table_schema NOT IN (${excluded})
ORDER BY table_schema <> 'public', table_schema, table_name;`,
    { maxRows: ALL_ROWS },
  );
}

async function describe(name) {
  const graphs = await graphNames();
  const isGraph = [...graphs].some((g) => g.toLowerCase() === name.replace(/"/g, "").toLowerCase());
  await runSql(isGraph ? `DESCRIBE GRAPH ${name};` : `DESCRIBE ${name};`, { maxRows: ALL_ROWS });
}

async function describeFunction(name) {
  const literal = `'${name.replace(/'/g, "''")}'`;
  await engine("run", {
    session: "meta",
    sql: `SELECT name, family, summary, parameters, outputs FROM ursa_functions() WHERE lower(name) = lower(${literal})`,
    maxRows: 0, // read back through export, below
  });
  const { bytes } = await engine("export", { session: "meta", index: 0, format: "json" });
  const [f] = JSON.parse(new TextDecoder().decode(bytes));
  if (!f) throw new Error(`no graph function named '${name}'; .functions lists them`);
  const lines = [`${f.name} (${f.family})`, f.summary, ""];
  if (f.parameters?.length) {
    lines.push("Parameters:");
    for (const p of f.parameters) {
      const choices = p.choices?.length ? ` (${p.choices.join(", ")})` : "";
      lines.push(`  ${p.name} => ${p.default}   ${p.type}${choices}. ${p.doc}`);
    }
  }
  if (f.outputs?.length) lines.push("", `Returns: ${f.outputs.map((o) => `${o.name} ${typeName(o.type ?? "")}`.trim()).join(", ")}`);
  say(lines.join("\n"));
}

const EXTENSIONS = { csv: "csv", parquet: "parquet", json: "json", arrow: "arrows" };
const MIME = { csv: "text/csv", parquet: "application/vnd.apache.parquet", json: "application/json", arrow: "application/vnd.apache.arrow.stream" };

async function exportLast(format) {
  if (!EXTENSIONS[format]) throw new Error("usage: .export csv|parquet|json|arrow");
  if (!lastResult) throw new Error("no result to export yet; run a query first");
  const { bytes } = await engine("export", { session: "shell", index: lastResult.index, format });
  const name = `ursa-result.${EXTENSIONS[format]}`;
  const url = URL.createObjectURL(new Blob([bytes], { type: MIME[format] }));
  el("a", { href: url, download: name }).click();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
  say(`Downloaded ${name} (${formatBytes(bytes.byteLength)}).`, "muted");
}

function formatBytes(n) {
  return n < 1024 ? `${n} B` : n < 1024 ** 2 ? `${(n / 1024).toFixed(1)} KB` : `${(n / 1024 ** 2).toFixed(1)} MB`;
}

let manifest = null;
async function listFiles() {
  manifest ??= await (await fetch("/data/manifest.json")).json();
  const rows = manifest.datasets.flatMap((d) =>
    d.files.map((f) => `('${f.name}', ${f.bytes}, '${d.title.replace(/'/g, "''")}')`),
  );
  rows.push(...uploads.map((u) => `('memory://uploads/${u.name.replace(/'/g, "''")}', ${u.bytes}, 'imported')`));
  await runSql(
    `SELECT name AS file, bytes, dataset FROM (VALUES ${rows.join(", ")}) AS t(name, bytes, dataset) ORDER BY dataset = 'imported', name;`,
    { maxRows: ALL_ROWS },
  );
  say("Read one by name: SELECT * FROM 'karate_edges.parquet'; Import adds your own.", "muted");
}

// ---------------------------------------------------------------- the bar

// New: a fresh session and a clear screen.
$("new").onclick = () => {
  queue = queue.then(async () => {
    await engine("reset", { session: "shell" });
    ran = [];
    lastResult = null;
    uploads = [];
    settings.maxRows = 40;
    settings.timer = false;
    history_replace("");
    log.replaceChildren();
    banner();
  });
};

// Share: a link whose fragment holds the session's commands, replayed on open.
function shareUrl() {
  const encoded = btoa(unescape(encodeURIComponent(JSON.stringify(ran))))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
  return `${location.origin}${location.pathname}#run=${encoded}`;
}
async function share() {
  if (!ran.length) {
    say("No commands to share.", "muted");
    return;
  }
  const url = shareUrl();
  history_replace(url);
  try {
    await navigator.clipboard.writeText(url);
    say(`Copied a link that replays ${plural(ran.length, "command")}.`, "muted");
  } catch {
    say(`A link that replays ${plural(ran.length, "command")} is in the address bar.`, "muted");
  }
  if (uploads.length) say("Imported files stay in this browser: whoever opens the link needs to import them again.", "muted");
}
$("share").onclick = () => submit(".share", { record: false });

function history_replace(url) {
  window.history.replaceState(null, "", url || location.pathname);
}

function sharedCommands() {
  const m = location.hash.match(/^#run=([A-Za-z0-9_-]+)$/);
  if (!m) return [];
  try {
    const b64 = m[1].replace(/-/g, "+").replace(/_/g, "/");
    const list = JSON.parse(decodeURIComponent(escape(atob(b64))));
    return Array.isArray(list) ? list.filter((c) => typeof c === "string") : [];
  } catch {
    return [];
  }
}

// Import: files from this computer, added to the session and given a view.
const fileInput = $("file-input");
function pickFiles() {
  fileInput.click();
}
$("import").onclick = () => submit(".files add", { record: false });
fileInput.onchange = async () => {
  const files = [...fileInput.files];
  fileInput.value = "";
  for (const file of files) {
    const view = file.name.replace(/\.(gz|zst|bz2|xz)$/i, "").replace(/\.[^.]+$/, "").toLowerCase().replace(/[^a-z0-9_]+/g, "_").replace(/^(\d)/, "_$1") || "imported";
    try {
      const buffer = await file.arrayBuffer();
      const { url } = await engine("addFile", { session: "shell", name: file.name, bytes: buffer }, [buffer]);
      uploads.push({ name: file.name, bytes: file.size });
      say(`Imported ${file.name} (${formatBytes(file.size)}) as ${url}`, "muted");
      await submit(`CREATE OR REPLACE VIEW ${view} AS SELECT * FROM '${url.replace(/'/g, "''")}';`);
      await submit(`FROM ${view} LIMIT 10;`);
    } catch (e) {
      error(e.message);
    }
  }
};

// Datasets: run a dataset's commands at the prompt, one after another.
const menu = $("dataset-menu");
const menuButton = $("datasets");
menu.replaceChildren(
  ...DATASETS.map((d) =>
    el("button", {
      type: "button",
      role: "menuitem",
      textContent: d.title,
      onclick: () => {
        closeMenu();
        loadDataset(d);
      },
    }),
  ),
);
function closeMenu() {
  menu.hidden = true;
  menuButton.setAttribute("aria-expanded", "false");
}
menuButton.onclick = (e) => {
  e.stopPropagation();
  const open = menu.hidden;
  menu.hidden = !open;
  menuButton.setAttribute("aria-expanded", String(open));
  if (open) menu.querySelector("button")?.focus();
};
document.addEventListener("click", (e) => {
  if (!menu.hidden && !menu.contains(e.target)) closeMenu();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !menu.hidden) {
    closeMenu();
    menuButton.focus();
  }
});

function loadDataset(d) {
  return submitAll(d.commands);
}

// ---------------------------------------------------------------- start

function banner() {
  say("Ursa Web Shell running...", "muted");
  if (version) say(`Ursa v${version} (in your browser, WebAssembly)`);
  say('Enter ".help" for usage hints, or pick a graph from Datasets.', "muted");
}

let version = null;
say("Loading the engine (about 5 MB)...", "muted");
syncInput();

engineStarted.then(
  (info) => {
    version = info.version;
    log.replaceChildren();
    banner();
    setBusy(false);
    // A shared link replays its commands; ?dataset=karate loads one.
    const shared = sharedCommands();
    const wanted = new URLSearchParams(location.search).get("dataset");
    if (shared.length) {
      say(`Replaying ${plural(shared.length, "command")} from a shared link.`, "muted");
      submitAll(shared);
    } else if (wanted) {
      const d = DATASETS.find((x) => x.id === wanted);
      if (d) loadDataset(d);
    }
  },
  (e) => {
    log.replaceChildren();
    error(`The engine failed to start: ${e.message}`);
  },
);

// For tests: resolves once every command queued so far has finished.
window.ursaShell = {
  idle: async () => {
    let seen;
    do {
      seen = queue;
      await seen;
    } while (seen !== queue);
    return true;
  },
  datasets: DATASETS,
};
