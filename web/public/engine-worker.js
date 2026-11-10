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
//   highlight {text}           -> {text}         (typed text in ANSI colour)
//   addFile   {name, bytes}    -> {url}          (memory://uploads/<name>)
//   export    {format}         -> {bytes}        (the last result, every row)
import init, { WebShell, version } from "./engine/ursa_wasm.js";

let shell = null;
let columns = 80;
let hostCommands = [];
let shortcuts = "";

const ready = (async () => {
  const t0 = performance.now();
  const engine = await (await fetch(new URL("./engine/engine.json", import.meta.url))).json();
  await init({ module_or_path: new URL(engine.wasm, self.location.origin) });
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
    } else if (op === "highlight") {
      postMessage({ id, ok: true, text: WebShell.highlight(args.text) });
    } else if (op === "addFile") {
      const url = await shell.addFile(args.name, new Uint8Array(args.bytes));
      postMessage({ id, ok: true, url });
    } else if (op === "export") {
      const bytes = shell.exportLast(args.format);
      postMessage({ id, ok: true, bytes }, [bytes.buffer]);
    } else {
      throw new Error(`unknown op ${op}`);
    }
  } catch (e) {
    postMessage({ id, ok: false, error: String(e?.message ?? e) });
  }
}
