// The engine, in a Web Worker so a long query never freezes the page.
//
// It runs the `ursa` shell itself (WebShell, from the ursa-shell crate): the
// page's terminal sends it each typed line and writes back what it printed,
// as a terminal shows the CLI's output.
//
// Messages are {id, op, ...}; replies are {id, ok: true, ...} or {id, ok: false, error}.
//   start     {commands, shortcuts}  -> {version, prompt, promptColor}  (a fresh shell)
//   feed      {line}           -> {out, err, flow, command, prompt, pending}
//   cancel    {}               -> {prompt}       (drop a half-typed statement)
//   width     {columns}        -> {}             (tables fit the terminal)
//   memory    {}               -> {bytes}        (the engine's WebAssembly memory,
//                                               for graphMemory)
//   highlight {text, alone?}   -> {text}         (typed text in ANSI colour, after
//                                               what is pending unless alone)
//   addFile   {name, bytes}    -> {url}          (memory://uploads/<name>)
//   files     {}               -> {files}        (the session's files, added or
//                                               written by COPY … TO 'name':
//                                               [{name, url, bytes}])
//   export    {format}         -> {bytes}        (the last result, every row)
//   graph     {text}           -> {ugr, skipped, positions?}  (a graph to draw: the last
//                                               result, a query's or a property
//                                               graph's; see WebShell.graph. `ugr`
//                                               is the graph result as one UGR
//                                               buffer, transferred, which
//                                               graph.js reads; `skipped` the rows
//                                               left out for a NULL endpoint;
//                                               `positions` where POSITIONS (query)
//                                               puts the nodes, x then y in a
//                                               Float32Array, transferred, #502)
//   explore   {command}        -> {out, err, delta, nodes, edges}  (an explore
//                                               command on the graph the last
//                                               `graph` opened: .expand, .hide,
//                                               .show, .undo, .redo, .graph
//                                               history; see WebShell.explore.
//                                               `delta` is the change as one UGR
//                                               delta buffer, transferred, or
//                                               null when nothing changed;
//                                               `nodes` and `edges` are how many
//                                               are shown after it)
//   find      {command}        -> {out, err, selected, matches, notShown,
//                                   notShownTotal}  (.find TEXT or .find WHERE
//                                               condition on the open graph; see
//                                               WebShell.find. `selected` is the
//                                               view's indexes of the matches it
//                                               shows, a Uint32Array of at most
//                                               1,000, transferred; `matches` how
//                                               many there are; `notShown` the
//                                               first matches it does not show,
//                                               [{key, hidden}], of
//                                               `notShownTotal`)
//   notShown  {}               -> {counts}       (for each node slot of the open
//                                               graph, how many of its edges in
//                                               the property graph lead to
//                                               something not shown, a
//                                               Uint32Array, transferred: the
//                                               view's +N; empty with no graph)
//   inspect   {node, key, neighbours?} | {edge, src, dst}
//                              -> {table, sql, columns, rows, row, note, ms}
//                                               (the inspector's row: what the
//                                               open graph's tables say of its
//                                               node or edge, by index, with the
//                                               keys the page shows, which are
//                                               checked; see WebShell.inspectNode.
//                                               With `neighbours`, {neighbours, ms}:
//                                               how many it has in the whole graph.
//                                               `ms` is the engine's time)
//   closeGraph {}              -> {}             (the page closed its view)
import init, { WebShell, version } from "./engine/ursa_wasm.js";

let shell = null;
// The engine's wasm exports, once loaded.
let wasm = null;
let columns = 80;
let hostCommands = [];
let shortcuts = "";

const ready = (async () => {
  const t0 = performance.now();
  const engine = await (await fetch(new URL("./engine/engine.json", import.meta.url))).json();
  wasm = await init({ module_or_path: new URL(engine.wasm, self.location.origin) });
  // Relative paths are the datasets' files.
  WebShell.setBaseUrl(new URL("/data/", self.location.origin).href);
  return { version: version(), ms: performance.now() - t0 };
})();

ready.then(
  ({ version, ms }) => postMessage({ ready: true, version, ms }),
  (e) => postMessage({ ready: false, error: String(e?.message ?? e) }),
);

/** A fresh shell, with the page's own commands and keys. */
function fresh() {
  shell?.free();
  shell = new WebShell();
  shell.setWidth(columns);
  for (const [name, args, help] of hostCommands) shell.addHostCommand(name, args, help);
  if (shortcuts) shell.setShortcuts(shortcuts);
}

// One request at a time, in order: the shell runs one line at once.
let queue = Promise.resolve();
onmessage = ({ data }) => {
  queue = queue.then(() => handle(data));
};

async function handle({ id, op, ...args }) {
  try {
    await ready;
    if (op === "start") {
      hostCommands = args.commands ?? hostCommands;
      shortcuts = args.shortcuts ?? shortcuts;
      fresh();
      // The page's default for the edges a view draws, where it is not the
      // engine's (a phone, #502): a SET like any other, which RESET undoes.
      if (args.graphMaxEdges) await shell.feed(`SET ursa.graph_max_edges = ${Number(args.graphMaxEdges)};`);
      postMessage({ id, ok: true, version: version(), prompt: shell.prompt(), promptColor: WebShell.promptColor() });
    } else if (op === "feed") {
      const result = JSON.parse(await shell.feed(args.line));
      postMessage({ id, ok: true, ...result, prompt: shell.prompt(), continuation: shell.continuation(), pending: shell.isPending() });
    } else if (op === "cancel") {
      shell.cancelInput();
      postMessage({ id, ok: true, prompt: shell.prompt() });
    } else if (op === "width") {
      columns = args.columns;
      shell?.setWidth(columns);
      postMessage({ id, ok: true });
    } else if (op === "memory") {
      postMessage({ id, ok: true, bytes: wasm.memory.buffer.byteLength });
    } else if (op === "highlight") {
      // As the next line of what is typed, unless asked for the text alone.
      const text = shell && !args.alone ? shell.highlight(args.text) : WebShell.highlightText(args.text);
      postMessage({ id, ok: true, text });
    } else if (op === "addFile") {
      const url = await shell.addFile(args.name, new Uint8Array(args.bytes));
      postMessage({ id, ok: true, url });
    } else if (op === "files") {
      postMessage({ id, ok: true, files: JSON.parse(await shell.files()) });
    } else if (op === "export") {
      const bytes = shell.exportLast(args.format);
      postMessage({ id, ok: true, bytes }, [bytes.buffer]);
    } else if (op === "graph") {
      const g = await shell.graph(args.text ?? "");
      // One buffer, the engine's whole answer: nothing is copied per array.
      const ugr = g.takeUgr();
      const skipped = g.skipped;
      const positions = g.takePositions();
      g.free();
      postMessage({ id, ok: true, ugr, skipped, positions }, positions ? [ugr.buffer, positions.buffer] : [ugr.buffer]);
    } else if (op === "explore") {
      const e = await shell.explore(args.command);
      const bytes = e.takeDelta();
      const reply = { id, ok: true, out: e.out, err: e.err, delta: bytes.length ? bytes : null, nodes: e.nodes, edges: e.edges };
      e.free();
      postMessage(reply, reply.delta ? [reply.delta.buffer] : []);
    } else if (op === "find") {
      const f = await shell.find(args.command);
      const selected = f.takeSelected();
      const reply = { id, ok: true, out: f.out, err: f.err, selected, matches: f.matches, notShown: JSON.parse(f.notShown), notShownTotal: f.notShownTotal };
      f.free();
      postMessage(reply, [selected.buffer]);
    } else if (op === "notShown") {
      const counts = await shell.notShown();
      postMessage({ id, ok: true, counts }, [counts.buffer]);
    } else if (op === "inspect") {
      const start = performance.now();
      const text = args.edge === undefined ? await shell.inspectNode(args.node, args.key, !!args.neighbours) : await shell.inspectEdge(args.edge, args.src, args.dst);
      postMessage({ id, ok: true, ...JSON.parse(text), ms: performance.now() - start });
    } else if (op === "closeGraph") {
      shell?.closeGraph();
      postMessage({ id, ok: true });
    } else {
      throw new Error(`unknown op ${op}`);
    }
  } catch (e) {
    postMessage({ id, ok: false, error: String(e?.message ?? e) });
  }
}
