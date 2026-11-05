import { EXAMPLES } from "./examples.js";

const $ = (id) => document.getElementById(id);
const editor = $("sql");
const output = $("output");
const status = $("status");
const run = $("run");
const picker = $("examples");

for (const [i, e] of EXAMPLES.entries()) picker.add(new Option(e.title, String(i)));

// A shared link carries the query in the URL: #sql=<encoded>.
function fromHash() {
  const sql = new URLSearchParams(location.hash.slice(1)).get("sql");
  return sql ?? EXAMPLES[0].sql;
}
editor.value = fromHash();

picker.addEventListener("change", () => {
  editor.value = EXAMPLES[Number(picker.value)].sql;
  picker.selectedIndex = 0;
  editor.focus();
});

const engine = new Worker(new URL("./engine-worker.js", import.meta.url), { type: "module" });
let ready = false;
let next = 0;
const pending = new Map();

engine.onmessage = ({ data }) => {
  if ("ready" in data) {
    if (data.ready) {
      ready = true;
      run.disabled = false;
      status.textContent = `Ursa ${data.version} running in your browser (loaded in ${(data.ms / 1000).toFixed(1)} s).`;
      if (location.hash.includes("sql=")) execute();
    } else {
      status.textContent = "The engine failed to start.";
      showError(data.error);
    }
    return;
  }
  pending.get(data.id)?.(data);
  pending.delete(data.id);
};

function showError(message) {
  output.textContent = message;
  output.classList.add("error");
}

function execute() {
  if (!ready) return;
  const sql = editor.value;
  history.replaceState(null, "", `#${new URLSearchParams({ sql })}`);
  const id = next++;
  run.disabled = true;
  status.textContent = "Running…";
  pending.set(id, (r) => {
    run.disabled = false;
    if (r.error !== undefined) {
      status.textContent = "Error.";
      showError(r.error);
    } else {
      output.classList.remove("error");
      output.textContent = r.out.trim() === "" ? "(no rows to show)" : r.out;
      status.textContent = `Done in ${r.ms < 1000 ? `${r.ms.toFixed(0)} ms` : `${(r.ms / 1000).toFixed(2)} s`}.`;
    }
  });
  engine.postMessage({ id, sql });
}

run.addEventListener("click", execute);
editor.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    execute();
  }
});

// The datasets, from the manifest the upload script writes.
fetch("/data/manifest.json")
  .then((r) => r.json())
  .then((m) => {
    $("datasets").replaceChildren(
      ...m.datasets.map((d) => {
        const li = document.createElement("li");
        li.innerHTML = `<b>${d.name}</b>: ${d.title}. <span class="muted">${d.source}</span><br><code>${d.files.join("</code> <code>")}</code>`;
        return li;
      }),
    );
    $("licence").textContent = m.licence;
  })
  .catch(() => {});
