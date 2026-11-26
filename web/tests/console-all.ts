// Every end-to-end suite of the Ursa Console, in turn, over `ursa serve`
// (URSA-18): what CI runs (.github/workflows/shell-e2e.yml, the console leg).
//
//   bun tests/console-all.ts [port] [ursa binary] [out dir] [--only a,b] [--skip a,b]
//   bun run test:console-all -- 9100 target/debug/ursa e2e-out
//
// The suites, by name:
//
// - `console`: tests/console.ts (the layout, the keyboard, the connection,
//   Cancel, the shell at /) and tests/console-results.ts (the result tabs);
// - `editor`: tests/console-editor.ts (the SQL editor);
// - `panels`: tests/console-panels.ts (Catalog and Sources).
//
// Each starts `ursa serve` itself (two of them on a root of their own), so
// each runs as a process of its own, on a port of its own: the first on PORT,
// the next on PORT + 1, and so on. The binary is a build with `--features
// serve` (by default $CARGO_TARGET_DIR/release/ursa; URSA_BIN= also names
// it); build the pages first (`bun run build`).
//
// With an out dir, each suite saves its screenshots (the console in light
// and dark) in OUT/<suite>/ and its output in OUT/<suite>.log. `--only` runs
// just the suites named, `--skip` all but those. It runs every suite even
// when one fails, then prints a summary: each suite's checks passed and
// failed, and every failed check. It exits 1 if any check failed or any suite
// did not finish.
import { mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { WEB } from "../scripts/common";

const SUITES: Record<string, string> = {
  console: "console.ts",
  editor: "console-editor.ts",
  panels: "console-panels.ts",
};
/** A suite that has not finished in this long has hung. */
const TIMEOUT_MS = 15 * 60_000;

const positional: string[] = [];
let only: string[] | undefined;
let skip: string[] = [];
const argv = Bun.argv.slice(2);
for (let i = 0; i < argv.length; i++) {
  const a = argv[i];
  const list = (v: string | undefined) => (v ?? "").split(",").map((s) => s.trim()).filter(Boolean);
  if (a === "--only") only = list(argv[++i]);
  else if (a.startsWith("--only=")) only = list(a.slice(7));
  else if (a === "--skip") skip = list(argv[++i]);
  else if (a.startsWith("--skip=")) skip = list(a.slice(7));
  else positional.push(a);
}
for (const name of [...(only ?? []), ...skip])
  if (!(name in SUITES)) {
    console.error(`no suite ${name}: the suites are ${Object.keys(SUITES).join(", ")}`);
    process.exit(2);
  }
const port = Number(positional[0] ?? 9100);
const bin = positional[1] || process.env.URSA_BIN || "";
const out = positional[2] ? resolve(positional[2]) : undefined;
const names = Object.keys(SUITES).filter((n) => (only ? only.includes(n) : true) && !skip.includes(n));
if (!names.length) {
  console.error("no suite to run");
  process.exit(2);
}

type Outcome = { name: string; ok: number; failed: string[]; code: number | string; seconds: number };
const outcomes: Outcome[] = [];

for (const [i, name] of names.entries()) {
  const shots = out && join(out, name);
  if (shots) mkdirSync(shots, { recursive: true });
  const args = ["bun", join(WEB, "tests", SUITES[name]), String(port + i), bin, ...(shots ? [shots] : [])];
  console.log(`\n=== ${name}: ${args.slice(1).filter(Boolean).join(" ")}\n`);
  const t0 = Date.now();
  const suite = Bun.spawn(args, { cwd: WEB, stdout: "pipe", stderr: "inherit" });
  let hung = false;
  const timer = setTimeout(() => {
    hung = true;
    suite.kill();
  }, TIMEOUT_MS);
  // Echo the suite's output as it comes, and keep it to count its checks.
  let printed = "";
  const decoder = new TextDecoder();
  for await (const chunk of suite.stdout) {
    const text = decoder.decode(chunk, { stream: true });
    process.stdout.write(text);
    printed += text;
  }
  const exit = await suite.exited;
  clearTimeout(timer);
  const seconds = (Date.now() - t0) / 1000;
  const lines = printed.split("\n");
  const ok = lines.filter((l) => l.startsWith("ok  ")).length;
  const failed = lines.filter((l) => l.startsWith("FAIL ")).map((l) => l.slice(5));
  const code = hung ? `timed out after ${TIMEOUT_MS / 60_000} min` : exit;
  if (out) writeFileSync(join(out, `${name}.log`), printed);
  outcomes.push({ name, ok, failed, code, seconds });
}

// A suite passed if it exited 0 with no failed check; one that exited
// otherwise without a failed check stopped before its end (a thrown error,
// a server that would not start, a timeout).
const passed = (o: Outcome) => o.code === 0 && o.failed.length === 0;
console.log("\n=== summary\n");
for (const o of outcomes) {
  const how = passed(o) ? "passed" : o.failed.length ? `${o.failed.length} failed` : `did not finish (${o.code})`;
  console.log(`${passed(o) ? "ok  " : "FAIL"} ${o.name.padEnd(8)} ${String(o.ok).padStart(4)} ok, ${how} (${o.seconds.toFixed(0)} s)`);
}
const bad = outcomes.filter((o) => !passed(o));
for (const o of bad) for (const f of o.failed) console.log(`  ${o.name}: ${f}`);
console.log(bad.length ? `\n${bad.length} of ${outcomes.length} suites failed` : `\nall ${outcomes.length} suites passed`);
if (out) console.log(`screenshots and logs in ${out}`);
process.exit(bad.length ? 1 : 0);
