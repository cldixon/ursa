// The Tour: the Getting Started steps, parsed from the browser parity test's
// fixture (ursa-wasm/tests/getting_started.sql), so every step the shell shows
// is one CI runs in the browser and checks against the native engine.
//
// In the fixture, `-- @step <title>` starts a step; the comment lines right
// after it are the step's note; the SQL up to the next step is the step's SQL.
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { REPO } from "./common";

export type Step = { title: string; note: string; sql: string };

export function parseTour(text: string): Step[] {
  const steps: Step[] = [];
  let step: { title: string; note: string[]; sql: string[] } | null = null;
  const flush = () => {
    if (!step) return;
    const sql = step.sql.join("\n").trim();
    if (!sql) throw new Error(`tour step "${step.title}" has no SQL`);
    steps.push({ title: step.title, note: step.note.join(" ").trim(), sql });
  };
  for (const line of text.split("\n")) {
    const start = line.match(/^-- @step (.+)$/);
    if (start) {
      flush();
      step = { title: start[1].trim(), note: [], sql: [] };
    } else if (step && step.sql.length === 0 && line.startsWith("--")) {
      step.note.push(line.replace(/^--\s?/, ""));
    } else if (step) {
      step.sql.push(line);
    }
  }
  flush();
  if (steps.length === 0) throw new Error("the fixture has no `-- @step` markers");
  return steps;
}

export function readTour(): Step[] {
  return parseTour(readFileSync(join(REPO, "ursa-wasm", "tests", "getting_started.sql"), "utf8"));
}

if (import.meta.main) {
  const steps = readTour();
  for (const s of steps) console.log(`- ${s.title}: ${s.sql.split("\n").length} lines`);
}
