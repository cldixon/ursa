// The Ursa Console's page (design/console.md §6), `ursa serve`'s at
// /console/: the layout (layout.js), the session (session.js) and the status
// bar, which says whether the page is connected, to what engine and with what
// root, and shows the request running, with a Cancel button.
//
// The panels are mount points for the issues that fill them (each panel's
// placeholder names its issue):
//
//   import { session } from "/console/session.js";
//   const { panels } = window.ursaConsole;   // panels.editor, panels.results, …
//
// window.ursaConsole is also what the e2e test (tests/console.ts) reads.
import { session } from "./session.js";
import { setupLayout } from "./layout.js";
import { createResults } from "./results.js";

const $ = (id) => document.getElementById(id);

// ---------------------------------------------------------------- theme

// As the shell's: the button flips between light and dark and remembers it
// (theme.js applies it before the first paint); the graph pane follows
// data-theme by itself.
function currentTheme() {
  const set = document.documentElement.dataset.theme;
  if (set === "light" || set === "dark") return set;
  return matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}
$("theme").addEventListener("click", () => {
  const next = currentTheme() === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = next;
  try {
    localStorage.setItem("ursa-theme", next);
  } catch {}
});

// ---------------------------------------------------------------- layout

const layout = setupLayout();

// ---------------------------------------------------------------- status bar

let countdown = null;

function showState() {
  const { state, hello, reason } = session;
  $("status").dataset.state = state;
  clearInterval(countdown);
  countdown = null;
  // The state is said once (role=status reads its changes); the countdown is
  // beside it, not live.
  $("connection").textContent =
    state === "connected"
      ? `Connected${hello.session?.kind ? ` · ${hello.session.kind} session` : ""}`
      : state === "connecting"
        ? "Connecting…"
        : `Disconnected${reason ? `: ${reason}` : ""}.`;
  const retry = $("retry-in");
  retry.textContent = "";
  if (state === "reconnecting") {
    const tick = () => {
      const left = session.retryAt ? Math.ceil((session.retryAt - performance.now()) / 1000) : 0;
      retry.textContent = left > 0 ? `Trying again in ${left} s` : "Trying again…";
    };
    tick();
    countdown = setInterval(tick, 250);
  }
  $("reconnect").hidden = state !== "reconnecting";
  // What the page is connected to, kept (dimmed) while it reconnects.
  if (hello) {
    $("engine").hidden = false;
    $("engine").textContent = `ursa ${hello.version} · ${hello.transport}`;
    $("engine").title = `The engine: Ursa ${hello.version}, ${hello.transport === "native" ? "native, in the ursa serve process" : "WebAssembly, in this tab"}; protocol ${hello.protocol}`;
    const cwd = hello.session?.cwd;
    $("root").hidden = !cwd;
    if (cwd) {
      $("root-path").textContent = cwd;
      $("root").title = `The session's root: relative paths in queries are read from ${cwd}`;
    }
  }
}
session.on("state", showState);
$("reconnect").addEventListener("click", () => session.reconnect());

// The request running: the oldest the console is waiting for (the session
// runs them one at a time, in order), with its time and a Cancel button.
let ticking = null;
let announced = null;
function showRequests() {
  const requests = session.requests;
  const running = requests[0];
  // Cancel had the keyboard: it stays in the status bar.
  if (!running && $("running").contains(document.activeElement)) $("status").focus();
  $("running").hidden = !running;
  clearInterval(ticking);
  ticking = null;
  if (!running) {
    if (announced) say(announced.cancelling ? `Cancelled ${announced.label}` : `Finished ${announced.label}`);
    announced = null;
    return;
  }
  const queued = requests.length - 1;
  $("running-text").textContent = `${running.cancelling ? "Cancelling" : "Running"} ${running.label}${queued ? ` (and ${queued} queued)` : ""}`;
  const cancel = $("cancel");
  cancel.hidden = !session.canCancel;
  cancel.disabled = running.cancelling;
  cancel.setAttribute("aria-label", `Cancel ${running.label}`);
  const tick = () => ($("running-time").textContent = seconds(performance.now() - running.started));
  tick();
  ticking = setInterval(tick, 100);
  if (announced !== running) say(`Running ${running.label}`);
  announced = running;
}
session.on("requests", showRequests);
$("cancel").addEventListener("click", () => session.cancel());

/** Say `text` to a screen reader, once. */
function say(text) {
  $("announce").textContent = text;
}

function seconds(ms) {
  return ms < 60_000 ? `${(ms / 1000).toFixed(1)} s` : `${Math.floor(ms / 60_000)} min ${Math.floor((ms % 60_000) / 1000)} s`;
}

// ---------------------------------------------------------------- start

showState();
session.start();

// The Results panel (URSA-14). `ursaConsole.run(sql)` is what the editor
// (URSA-13, editor.js) calls to run SQL into it.
const results = createResults({ mount: layout.panels.results, session });
const run = (sql) => results.run(sql);

window.ursaConsole = { session, panels: layout.panels, layout, results, run };
