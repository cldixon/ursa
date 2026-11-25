// A second host for the graph pane (graph-pane.js), for the e2e test: the
// engine client, a commands sink, a status line and a container, and nothing
// of the Web Shell (no terminal, no app.js). The pane's commands (a
// double-click's `.expand 2`) arrive in the sink, which lists them in
// History, as the console will, and runs them through the pane.
import { connect, defaultTransport } from "/engine-client.js";
import { createPane } from "/graph-pane.js";
import { DATASETS } from "/datasets.js";

const client = connect(defaultTransport(), { name: "pane-host", kind: "human" });
const history = document.getElementById("history");

// What reached the sink, in order: {command, via}.
const received = [];
// Everything History lists: {kind, text}.
const logged = [];
function log(kind, text) {
  logged.push({ kind, text });
  const li = document.createElement("li");
  li.className = kind;
  li.textContent = text;
  history.append(li);
}

const pane = createPane({
  container: document.getElementById("host"),
  engine: client,
  commands: async (command, { via, current }) => {
    received.push({ command, via });
    log("command", command);
    // A view's command for a drawing that has gone is dropped.
    if (!current()) return false;
    return exec(command);
  },
  status: (message) => log("status", message),
});

/** Run a graph command through the pane, and list what it said. */
async function exec(command) {
  const r = await pane.exec(command);
  const said = (r.out + r.err).trim();
  if (said) log("answer", said);
  if (r.error) log("error", r.error);
  return r.ok;
}

/** The karate club's tables and graph: the dataset's CREATEs, fed line by line. */
async function setup() {
  const karate = DATASETS.find((d) => d.id === "karate");
  for (const command of karate.commands) {
    if (!/^(--[^\n]*\n)*CREATE/i.test(command)) continue;
    for (const line of command.split("\n")) {
      const r = await client.call("feed", { line });
      if (r.err) throw new Error(r.err);
    }
  }
}

const ready = client.ready.then(setup);
window.paneHost = { client, pane, received, logged, exec, ready: () => ready };
