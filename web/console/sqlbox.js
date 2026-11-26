// TEMPORARY (URSA-14): a plain textarea in the Editor panel, to run SQL
// until the editor comes. URSA-13 replaces this module with CodeMirror; what
// stays is the run function it calls, `ursaConsole.run(sql)` (results.js's
// run), which the editor calls the same way.
//
// Ctrl+Enter (Cmd+Enter on a Mac) runs the selected text, or else the whole
// box. What was typed is kept in this browser across a reload.

const KEY = "ursa-console-sql";
const MAC = /Mac|iPhone|iPad/.test(navigator.platform);

export function createSqlBox({ mount, run }) {
  mount.replaceChildren();
  const box = document.createElement("div");
  box.className = "sqlbox";
  box.innerHTML = `
    <div class="sqlbox-note">
      <span class="issue">Temporary</span>
      <span>A plain box to run SQL until the editor (URSA-13) replaces it.</span>
    </div>
    <textarea id="sql-input" class="sqlbox-input" spellcheck="false" autocapitalize="off" autocomplete="off"
              aria-label="SQL to run" aria-describedby="sql-keys"
              placeholder="SELECT * FROM pagerank(karate)"></textarea>
    <div class="sqlbox-bar">
      <button id="sql-run" type="button" class="sqlbox-run">
        <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M5 3.5v9l7-4.5z" fill="currentColor"/></svg>Run
      </button>
      <span id="sql-keys" class="sqlbox-keys"><kbd>${MAC ? "⌘" : "Ctrl"}</kbd>+<kbd>Enter</kbd> runs the selection, or all of it</span>
    </div>`;
  mount.append(box);
  const input = box.querySelector("textarea");
  const button = box.querySelector("button");
  try {
    input.value = localStorage.getItem(KEY) ?? "";
  } catch {}
  input.addEventListener("input", () => {
    try {
      localStorage.setItem(KEY, input.value);
    } catch {}
  });

  async function go() {
    const { selectionStart: a, selectionEnd: b, value } = input;
    const sql = a !== b ? value.slice(a, b) : value;
    if (!sql.trim()) return null;
    button.disabled = true;
    try {
      return await run(sql);
    } finally {
      button.disabled = false;
    }
  }
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      go();
    }
  });
  button.addEventListener("click", go);
  return { input, run: go };
}
