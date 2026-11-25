// The console spike's page (#536): SQL in a textarea, run with the session
// protocol's `query` op, whose reply carries the first rows as an Arrow IPC
// stream, decoded here with apache-arrow and shown as a table with each
// column's type and `ursa.*` metadata; and a Graph button that opens the
// result in the shell's graph pane (graph.js), by the result's id.
//
// It talks to `ursa serve` (SocketTransport), or with ?transport=worker to
// the wasm engine in a Web Worker: the same operations either way.
import { tableFromIPC } from "./vendor/arrow.mjs";
import { DATASETS } from "./datasets.js";
import { connect, SocketTransport, WorkerTransport } from "./engine-client.js";

const $ = (id) => document.getElementById(id);
const worker = new URLSearchParams(location.search).get("transport") === "worker";
const client = connect(worker ? new WorkerTransport() : new SocketTransport(), { name: "console", kind: "human" });

// The result shown, by its id, for the Graph button.
let shown = null;
let graphModule = null;

function log(line) {
  $("history").append(Object.assign(document.createElement("li"), { textContent: line }));
}

/** Run `sql`: the table, or the error. */
async function query(sql, rows = 1000) {
  const start = performance.now();
  try {
    const r = await client.call("query", { sql, rows });
    const table = tableFromIPC(r.arrow);
    const ms = performance.now() - start;
    shown = r;
    render(r, table);
    $("draw").disabled = !r.columns.length;
    $("info").textContent = `${r.rows.toLocaleString("en-US")} rows${r.more ? `, first ${table.numRows.toLocaleString("en-US")} shown` : ""} · engine ${r.ms.toFixed(1)} ms · round trip ${ms.toFixed(1)} ms · ${r.arrow.byteLength.toLocaleString("en-US")} bytes of Arrow`;
    log(sql);
    return { ...r, table, roundTrip: ms };
  } catch (e) {
    $("result").replaceChildren(Object.assign(document.createElement("pre"), { textContent: `Error (${e.kind}): ${e.message}` }));
    $("info").textContent = "";
    throw e;
  }
}

/** A value as text: BigInts, nested values and NULL. */
function text(value) {
  if (value === null || value === undefined) return null;
  if (typeof value === "bigint") return value.toString();
  if (typeof value === "object") return JSON.stringify(value.toJSON?.() ?? value, (_, v) => (typeof v === "bigint" ? v.toString() : v));
  return String(value);
}

/** The result as a table: a header with each column's type and metadata. */
function render(r, table) {
  const node = (tag, props = {}, ...children) => {
    const n = Object.assign(document.createElement(tag), props);
    n.append(...children);
    return n;
  };
  const fields = table.schema.fields;
  const head = node(
    "tr",
    {},
    ...fields.map((f) => {
      const meta = [...f.metadata].filter(([k]) => k.startsWith("ursa.")).map(([k, v]) => `${k}=${v}`).join(" ");
      return node("th", {}, f.name, node("br"), node("span", { className: "type", textContent: String(f.type) }), ...(meta ? [node("br"), node("span", { className: "meta", textContent: meta })] : []));
    }),
  );
  const numeric = fields.map((f) => /Int|Float|Decimal/.test(String(f.type)));
  const body = [];
  const columns = fields.map((_, i) => table.getChildAt(i));
  for (let row = 0; row < Math.min(table.numRows, 1000); row++) {
    body.push(
      node(
        "tr",
        {},
        ...columns.map((c, i) => {
          const t = text(c.get(row));
          return t === null ? node("td", { className: "null", textContent: "NULL" }) : node("td", { className: numeric[i] ? "number" : "", textContent: t });
        }),
      ),
    );
  }
  $("result").replaceChildren(r.columns.length ? node("table", {}, node("thead", {}, head), node("tbody", {}, ...body)) : node("p", { textContent: r.notices.join(" ") }));
}

/** Draw the shown result in the graph pane. */
async function draw() {
  if (!shown) return;
  const graph = await client.call("graph.open", { result: shown.result });
  $("graph-pane").hidden = false;
  graphModule ??= await import("./graph.js");
  const drawn = await graphModule.show($("graph-stage"), graph, {
    status: $("graph-status"),
    legend: $("graph-legend"),
    onError: (message) => log(`graph view: ${message}`),
    // The view's commands (a double-click's .expand 33) go to History, not a
    // terminal, and their deltas to the view.
    onCommand: async (command) => {
      log(command);
      const r = await client.call("graph.explore", { command });
      if (r.out || r.err) log((r.out + r.err).trim());
      if (r.delta) graphModule.applyDelta(r.delta, { command });
    },
    run: async (command) => log(`(not run: ${command})`),
    inspect: (args) => client.call("graph.inspect", args),
  });
  log(`Graph: ${drawn.nodes} nodes, ${drawn.edges} edges${drawn.drawnAs ? ` as ${drawn.drawnAs}` : ""}`);
  return drawn;
}

$("run").onclick = () => query($("sql").value).catch(() => {});
$("draw").onclick = () => draw().catch((e) => log(`Error: ${e.message}`));
$("sql").onkeydown = (e) => {
  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    $("run").click();
  }
};

/** The karate club's tables and graph, from the Datasets menu's commands. */
async function setup(id = "karate") {
  const dataset = DATASETS.find((d) => d.id === id);
  for (const command of dataset.commands) {
    const sql = command
      .split("\n")
      .filter((l) => !/^\s*--/.test(l))
      .join("\n")
      .trim();
    if (/^CREATE/i.test(sql)) await client.call("query", { sql, rows: 0 });
  }
  log(`Loaded ${id}`);
}

// For tests and measurements.
window.ursaConsole = { client, query, draw, setup, graph: () => graphModule?.info() ?? null };

client.ready.then(
  async (hello) => {
    $("info").textContent = `Connected: ${hello.transport} engine ${hello.version}`;
    await setup();
    $("info").textContent += " · karate loaded";
  },
  (e) => ($("info").textContent = `Not connected: ${e.message}`),
);
