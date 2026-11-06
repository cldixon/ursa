// Assemble dist/, the static assets: public/, the engine's JS glue and
// engine.json from .engine/ (run `bun run engine` first), and tour.json, the
// Tour, parsed from the tested Getting Started fixture.
import { cpSync, existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { ENGINE_DIR, WEB } from "./common";
import { readTour } from "./tour";

const dist = join(WEB, "dist");
if (!existsSync(join(ENGINE_DIR, "engine.json"))) {
  console.error("no engine build: run `bun run engine` first");
  process.exit(1);
}
rmSync(dist, { recursive: true, force: true });
cpSync(join(WEB, "public"), dist, { recursive: true });
mkdirSync(join(dist, "engine"), { recursive: true });
cpSync(join(ENGINE_DIR, "pkg", "ursa_wasm.js"), join(dist, "engine", "ursa_wasm.js"));
cpSync(join(ENGINE_DIR, "engine.json"), join(dist, "engine", "engine.json"));
writeFileSync(join(dist, "tour.json"), JSON.stringify(readTour(), null, 2) + "\n");
console.log("built dist/");
