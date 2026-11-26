// The Sources panel (URSA-16, design/console.md §6): the files and folders
// under the session's root, and buckets.
//
// - It lists a folder (`sources.list {url}`) with each entry's format and
//   size, folders first; a folder opens, and the path above the list goes
//   back up.
// - "Add a bucket" takes an `s3://`, `gs://` or `az://` prefix and browses
//   it. Buckets added are kept in this browser for this server (its port).
// - Choosing a file hands `SELECT * FROM '<url>' LIMIT 100` to the editor,
//   which runs it (actions.js): under the root, by its path relative to it, which the
//   session's queries resolve against. A database file (.ursa) is attached
//   (`sources.attach`), and its tables show in the Catalog.
// - Without the `sources` capability (the browser demo, or a session with
//   no directory root) it says so and shows nothing else.
// - What fails (a folder that has gone, a bucket with no credentials) shows
//   in the panel, with a way to try again.
//
// The keyboard: Tab to the list, Up and Down between its entries, Enter to
// open or use one, Backspace (or Alt+Up) to go up a folder.
import { session } from "./session.js";
import { useQuery, describe, quoteString, makeNote, formatBytes } from "./actions.js";

const BUCKET = /^(s3|gs|az|abfss?):\/\/[^/\s]+(\/\S*)?$/i;
const storageKey = () => `ursa-console-buckets:${location.port || "default"}`;

const ICON = {
  root: '<svg viewBox="0 0 16 16"><path d="M2 4.5a1 1 0 0 1 1-1h3.25l1.5 1.5H13a1 1 0 0 1 1 1V12a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1Z"/></svg>',
  bucket: '<svg viewBox="0 0 16 16"><path d="M2.5 4.5h11l-1.25 8.25a1 1 0 0 1-1 .75h-6.5a1 1 0 0 1-1-.75Z"/><ellipse cx="8" cy="4.5" rx="5.5" ry="1.5"/></svg>',
  dir: '<svg viewBox="0 0 16 16"><path d="M2 4.5a1 1 0 0 1 1-1h3.25l1.5 1.5H13a1 1 0 0 1 1 1V12a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1Z"/></svg>',
  file: '<svg viewBox="0 0 16 16"><path d="M4 2.5h5.5L12.5 5.5v8H4Z"/><path d="M9.5 2.5v3h3"/></svg>',
  ursa: '<svg viewBox="0 0 16 16"><ellipse cx="8" cy="4" rx="5.25" ry="2"/><path d="M2.75 4v8c0 1.1 2.35 2 5.25 2s5.25-.9 5.25-2V4M2.75 8c0 1.1 2.35 2 5.25 2s5.25-.9 5.25-2"/></svg>',
  up: '<svg viewBox="0 0 16 16"><path d="M8 13V3.5M4 7.5l4-4 4 4"/></svg>',
  plus: '<svg viewBox="0 0 16 16"><path d="M8 3v10M3 8h10"/></svg>',
  close: '<svg viewBox="0 0 16 16"><path d="m4.5 4.5 7 7M11.5 4.5l-7 7"/></svg>',
};
const FORMAT_LABEL = { parquet: "parquet", csv: "csv", json: "json", arrow: "arrow", ursa: "ursa" };

export function mountSources(mount) {
  mount.replaceChildren();
  mount.classList.add("sources-panel");
  mount.innerHTML = `
    <div class="sources-off" hidden>
      <p class="what">No files to browse here</p>
      <p>This session has no folder on disk: browsing files and buckets needs <code>ursa serve DIR</code>. Queries can still read files by URL.</p>
    </div>
    <div class="sources-on" hidden>
      <div class="panel-tools places" role="toolbar" aria-label="Places">
        <div class="place-list"></div>
        <button type="button" class="tool-button add-bucket" aria-expanded="false" aria-controls="sources-bucket-form" title="Browse an s3://, gs:// or az:// prefix">${ICON.plus}<span>Bucket</span></button>
      </div>
      <form id="sources-bucket-form" class="bucket-form" hidden novalidate>
        <label for="sources-bucket-url">Bucket prefix</label>
        <div class="bucket-row">
          <input id="sources-bucket-url" name="url" type="text" inputmode="url" placeholder="s3://bucket/prefix/" autocomplete="off" spellcheck="false" aria-describedby="sources-bucket-hint">
          <button type="submit" class="tool-button primary">Browse</button>
        </div>
        <p id="sources-bucket-hint" class="hint">An <code>s3://</code>, <code>gs://</code> or <code>az://</code> URL. Credentials come from ursa serve's environment.</p>
      </form>
      <nav class="crumbs" aria-label="Folder"></nav>
      <div class="sources-state" role="status"></div>
      <ul class="entries" aria-label="Files and folders"></ul>
    </div>`;
  const off = mount.querySelector(".sources-off");
  const on = mount.querySelector(".sources-on");
  const placeList = mount.querySelector(".place-list");
  const addButton = mount.querySelector(".add-bucket");
  const form = mount.querySelector(".bucket-form");
  const urlInput = form.querySelector("input");
  const hint = form.querySelector(".hint");
  const crumbs = mount.querySelector(".crumbs");
  const stateLine = mount.querySelector(".sources-state");
  const list = mount.querySelector(".entries");
  const note = makeNote(mount);

  /** The session's root directory (its absolute path), and its file:// URL. */
  let root = "";
  let rootUrl = "";
  /** Where the panel is: {place: "root" | bucket prefix, url: what is listed}. */
  let here = { place: "root", url: "" };
  let buckets = loadBuckets();
  let listing = 0;
  let enabled = false;

  // ---------------------------------------------------------------- places

  function loadBuckets() {
    try {
      const saved = JSON.parse(localStorage.getItem(storageKey()) ?? "[]");
      return Array.isArray(saved) ? saved.filter((b) => typeof b === "string" && BUCKET.test(b)) : [];
    } catch {
      return [];
    }
  }
  function saveBuckets() {
    try {
      localStorage.setItem(storageKey(), JSON.stringify(buckets));
    } catch {}
  }

  function renderPlaces() {
    placeList.replaceChildren();
    const places = [["root", baseName(root) || "Root", ICON.root, `The session's root: ${root}`], ...buckets.map((b) => [b, b.replace(/\/$/, ""), ICON.bucket, b])];
    for (const [place, label, icon, title] of places) {
      const wrap = document.createElement("span");
      wrap.className = "place";
      const b = document.createElement("button");
      b.type = "button";
      b.className = "place-button";
      b.title = title;
      b.dataset.place = place;
      b.setAttribute("aria-pressed", String(here.place === place));
      b.innerHTML = `<span class="icon" aria-hidden="true">${icon}</span>`;
      const text = document.createElement("span");
      text.className = "label";
      text.textContent = label;
      b.append(text);
      b.addEventListener("click", () => go(place, place === "root" ? "" : place));
      wrap.append(b);
      if (place !== "root") {
        const x = document.createElement("button");
        x.type = "button";
        x.className = "place-remove";
        x.innerHTML = ICON.close;
        x.setAttribute("aria-label", `Remove ${place}`);
        x.title = `Remove ${place}`;
        x.addEventListener("click", () => removeBucket(place));
        wrap.append(x);
      }
      placeList.append(wrap);
    }
  }

  function addBucket(url) {
    if (!url.endsWith("/")) url += "/";
    if (!buckets.includes(url)) {
      buckets.push(url);
      saveBuckets();
    }
    go(url, url);
  }

  function removeBucket(url) {
    buckets = buckets.filter((b) => b !== url);
    saveBuckets();
    if (here.place === url) go("root", "");
    else renderPlaces();
    addButton.focus();
  }

  addButton.addEventListener("click", () => {
    const show = form.hidden;
    form.hidden = !show;
    addButton.setAttribute("aria-expanded", String(show));
    if (show) urlInput.focus();
  });
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    const url = urlInput.value.trim();
    if (!BUCKET.test(url)) {
      hint.dataset.kind = "error";
      hint.textContent = "That is not a bucket URL: it starts with s3://, gs:// or az://, then the bucket's name.";
      urlInput.setAttribute("aria-invalid", "true");
      urlInput.focus();
      return;
    }
    urlInput.removeAttribute("aria-invalid");
    hint.dataset.kind = "";
    hint.innerHTML = "An <code>s3://</code>, <code>gs://</code> or <code>az://</code> URL. Credentials come from ursa serve's environment.";
    urlInput.value = "";
    form.hidden = true;
    addButton.setAttribute("aria-expanded", "false");
    addBucket(url);
  });
  form.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      form.hidden = true;
      addButton.setAttribute("aria-expanded", "false");
      addButton.focus();
    }
  });

  // ---------------------------------------------------------------- listing

  /** Show `url` in `place`. */
  async function go(place, url, { focus = false } = {}) {
    here = { place, url };
    renderPlaces();
    renderCrumbs();
    const id = ++listing;
    const client = session.client;
    if (!client) return;
    const slow = setTimeout(() => id === listing && showState("Listing…", "pending"), 150);
    try {
      const reply = await client.call("sources.list", { url });
      if (id !== listing) return;
      // The root as its entries' URLs have it (the real path: hello's may
      // go through a symbolic link).
      if (place === "root" && url === "" && reply.entries?.length) {
        const first = reply.entries[0];
        const path = first.url.replace(/\/$/, "");
        rootUrl = path.slice(0, path.lastIndexOf("/") + 1);
      }
      renderEntries(reply.entries ?? []);
      if (focus) list.querySelector('[tabindex="0"]')?.focus();
    } catch (e) {
      if (id !== listing) return;
      list.replaceChildren();
      showError(e, place, url);
    } finally {
      clearTimeout(slow);
    }
  }

  function showState(text, kind = "") {
    stateLine.replaceChildren(text);
    stateLine.dataset.kind = kind;
    stateLine.hidden = !text;
  }

  function showError(e, place, url) {
    const box = document.createElement("div");
    box.className = "error-box";
    const title = document.createElement("p");
    title.className = "error-title";
    title.textContent = place === "root" ? "Cannot list this folder" : `Cannot list ${url}`;
    const detail = document.createElement("p");
    detail.className = "error-detail";
    detail.textContent = readable(e.message);
    box.append(title, detail);
    if (place !== "root" && /credential|permission|denied|forbidden|403|401|auth|signature|token|access ?key/i.test(e.message))
      box.append(Object.assign(document.createElement("p"), { className: "error-hint", textContent: "Check the credentials in ursa serve's environment (for S3, AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, or a profile)." }));
    const actions = document.createElement("div");
    actions.className = "error-actions";
    const retry = Object.assign(document.createElement("button"), { type: "button", className: "tool-button", textContent: "Try again" });
    retry.addEventListener("click", () => go(place, url, { focus: true }));
    actions.append(retry);
    if (place !== "root") {
      const remove = Object.assign(document.createElement("button"), { type: "button", className: "tool-button", textContent: "Remove bucket" });
      remove.addEventListener("click", () => removeBucket(place));
      actions.append(remove);
    } else if (url) {
      const top = Object.assign(document.createElement("button"), { type: "button", className: "tool-button", textContent: "Back to the root" });
      top.addEventListener("click", () => go("root", "", { focus: true }));
      actions.append(top);
    }
    box.append(actions);
    showState(box, "error");
  }

  /** The path above the list: the place, then each folder down to here. */
  function renderCrumbs() {
    crumbs.replaceChildren();
    const steps = [];
    if (here.place === "root") {
      steps.push([baseName(root) || "Root", ""]);
      const rel = relative(here.url).replace(/\/$/, "");
      let at = "";
      for (const part of rel ? rel.split("/") : []) {
        at += `${part}/`;
        steps.push([part, at]);
      }
    } else {
      const base = here.place;
      steps.push([base.replace(/\/$/, ""), base]);
      const rest = here.url.slice(base.length).replace(/\/$/, "");
      let at = base;
      for (const part of rest ? rest.split("/") : []) {
        at += `${part}/`;
        steps.push([part, at]);
      }
    }
    // At the top of a place, its button says where the list is.
    crumbs.hidden = steps.length < 2;
    const ol = document.createElement("ol");
    steps.forEach(([label, url], i) => {
      const li = document.createElement("li");
      if (i === steps.length - 1) {
        const span = document.createElement("span");
        span.setAttribute("aria-current", "location");
        span.textContent = label;
        li.append(span);
      } else {
        const b = document.createElement("button");
        b.type = "button";
        b.textContent = label;
        b.addEventListener("click", () => go(here.place, url, { focus: true }));
        li.append(b);
      }
      ol.append(li);
    });
    crumbs.append(ol);
  }

  function renderEntries(entries) {
    // The entry with the keyboard keeps it when the same folder is listed again.
    const focused = list.contains(document.activeElement) ? document.activeElement.closest(".entry")?.dataset.url : null;
    const frag = document.createDocumentFragment();
    if (canGoUp()) frag.append(entryRow({ kind: "up", name: "..", url: parentUrl() }));
    for (const entry of entries) frag.append(entryRow(entry));
    list.replaceChildren(frag);
    const again = focused && [...list.querySelectorAll(".entry")].find((li) => li.dataset.url === focused)?.querySelector(".entry-main");
    const first = again ?? list.querySelector(".entry-main");
    if (first) first.tabIndex = 0;
    if (again) again.focus({ preventScroll: true });
    if (!entries.length) showState("This folder is empty.", "empty");
    else showState("");
  }

  function entryRow(entry) {
    const li = document.createElement("li");
    li.className = `entry kind-${entry.kind}${entry.format ? ` format-${entry.format}` : ""}`;
    li.dataset.url = entry.url;
    li.dataset.name = entry.name;
    const main = document.createElement("button");
    main.type = "button";
    main.className = "entry-main";
    main.tabIndex = -1;
    const icon = document.createElement("span");
    icon.className = "icon";
    icon.setAttribute("aria-hidden", "true");
    icon.innerHTML = entry.kind === "up" ? ICON.up : entry.kind === "dir" ? ICON.dir : entry.format === "ursa" ? ICON.ursa : ICON.file;
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = entry.kind === "up" ? "Up a folder" : entry.name;
    main.append(icon, name);
    if (entry.kind === "dir") main.title = `Open ${entry.name}`;
    if (entry.kind === "file") {
      const meta = document.createElement("span");
      meta.className = "meta";
      if (entry.format) {
        const chip = document.createElement("span");
        chip.className = `format-chip fmt-${entry.format}`;
        chip.textContent = FORMAT_LABEL[entry.format] ?? entry.format;
        meta.append(chip);
      }
      const size = document.createElement("span");
      size.className = "size";
      size.textContent = formatBytes(entry.bytes);
      meta.append(size);
      main.append(meta);
      if (!entry.format) {
        li.classList.add("unknown");
        main.title = "Ursa does not read this kind of file";
        main.setAttribute("aria-disabled", "true");
      } else if (entry.format === "ursa") main.title = `Attach ${entry.name}`;
      else main.title = `Query ${entry.name}`;
    }
    li.append(main);
    if (entry.format === "ursa") {
      const attach = document.createElement("button");
      attach.type = "button";
      attach.className = "tool-button attach";
      attach.tabIndex = -1;
      attach.textContent = "Attach";
      attach.setAttribute("aria-label", `Attach ${entry.name}`);
      attach.addEventListener("click", (e) => {
        e.stopPropagation();
        use(entry);
      });
      li.append(attach);
    }
    main.addEventListener("click", () => {
      roveTo(main);
      use(entry);
    });
    return li;
  }

  async function use(entry) {
    if (entry.kind === "up" || entry.kind === "dir") return go(here.place, entry.url, { focus: true });
    if (!entry.format) return;
    if (entry.format === "ursa") return attach(entry);
    const sql = `SELECT * FROM ${quoteString(queryPath(entry.url))} LIMIT 100`;
    note(sql, "pending");
    const outcome = await useQuery(sql, { label: `SELECT from ${entry.name}`, run: true });
    note(outcome.how === "ran" ? `${sql} — ${describe(outcome, sql)}` : describe(outcome, sql), outcome.how === "failed" ? "error" : "info");
    mount.dispatchEvent(new CustomEvent("ursa-sources-use", { bubbles: true, detail: { text: sql, outcome } }));
  }

  async function attach(entry) {
    note(`Attaching ${entry.name}…`, "pending");
    try {
      await session.run("sources.attach", { url: queryPath(entry.url) }, { label: `ATTACH ${entry.name}` });
      note(`Attached ${entry.name}: its tables are in the Catalog.`);
    } catch (e) {
      note(`Cannot attach ${entry.name}: ${e.message}`, "error");
    }
  }

  // Up and Down between entries (one Tab stop); Backspace goes up.
  function roveTo(el) {
    for (const b of list.querySelectorAll('[tabindex="0"]')) b.tabIndex = -1;
    el.tabIndex = 0;
    el.focus();
  }
  list.addEventListener("keydown", (e) => {
    const mains = [...list.querySelectorAll(".entry-main")];
    const at = e.target.closest(".entry");
    const i = mains.findIndex((m) => at?.contains(m));
    let handled = true;
    if (e.key === "ArrowDown") mains[i + 1] && roveTo(mains[i + 1]);
    else if (e.key === "ArrowUp") mains[i - 1] && roveTo(mains[i - 1]);
    else if (e.key === "Home") mains[0] && roveTo(mains[0]);
    else if (e.key === "End") mains.at(-1) && roveTo(mains.at(-1));
    else if (e.key === "ArrowRight" && e.target.classList.contains("entry-main")) {
      const attachButton = at.querySelector(".attach");
      if (attachButton) attachButton.focus();
      else handled = false;
    } else if (e.key === "ArrowLeft" && e.target.classList.contains("attach")) roveTo(at.querySelector(".entry-main"));
    else if ((e.key === "Backspace" || (e.key === "ArrowUp" && e.altKey)) && canGoUp()) go(here.place, parentUrl(), { focus: true });
    else handled = false;
    if (handled) e.preventDefault();
  });

  // ---------------------------------------------------------------- paths

  /** A file:// URL under the root as a path relative to it; others as they are. */
  function relative(url) {
    if (rootUrl && url.startsWith(rootUrl)) return decodeURIComponent(url.slice(rootUrl.length));
    return url;
  }
  /** What a query names a file by: its path under the root, or its URL. */
  function queryPath(url) {
    const rel = relative(url);
    return rel === url && url.startsWith("file://") ? decodeURIComponent(new URL(url).pathname) : rel;
  }
  function canGoUp() {
    if (here.place === "root") return relative(here.url).replace(/\/$/, "") !== "";
    return here.url.length > here.place.length;
  }
  function parentUrl() {
    const trimmed = here.url.replace(/\/$/, "");
    const up = trimmed.slice(0, trimmed.lastIndexOf("/") + 1);
    if (here.place === "root") return relative(up);
    return up.length < here.place.length ? here.place : up;
  }

  // ---------------------------------------------------------------- wiring

  function connected(hello) {
    enabled = Boolean(hello?.capabilities?.includes("sources"));
    off.hidden = enabled;
    on.hidden = !enabled;
    mount.classList.toggle("disabled", !enabled);
    if (!enabled) return;
    root = hello.session?.cwd ?? "";
    rootUrl = root ? `file://${encodeURI(root.replace(/\/?$/, "/"))}` : "";
    buckets = loadBuckets();
    go(here.place === "root" || buckets.includes(here.place) ? here.place : "root", here.place === "root" || buckets.includes(here.place) ? here.url : "");
  }
  session.on("connect", connected);
  session.on("state", (s) => mount.classList.toggle("stale", s.state !== "connected"));
  // A new file in the folder (COPY … TO) shows the next time it is listed;
  // a catalog change is a good moment to look again.
  session.onEvent("catalog", () => enabled && here.place === "root" && go(here.place, here.url));
  if (session.state === "connected") connected(session.hello);

  return {
    go,
    get here() {
      return here;
    },
  };
}

function baseName(path) {
  return path.replace(/\/$/, "").split("/").pop() ?? "";
}

/** An error as people read it: a store's XML error body as its code and message. */
function readable(message = "") {
  return message.replace(/<\?xml[^>]*>\s*/g, "").replace(/<Error>[\s\S]*?<\/Error>/g, (xml) => {
    const tag = (t) => xml.match(new RegExp(`<${t}>([\\s\\S]*?)</${t}>`))?.[1];
    const code = tag("Code");
    const text = tag("Message");
    return code || text ? [code, text].filter(Boolean).join(": ") : xml;
  });
}
