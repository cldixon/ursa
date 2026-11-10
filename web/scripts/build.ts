// Assemble dist/, the static assets: public/, and the engine's JS glue and
// engine.json from .engine/ (run `bun run engine` first), and the terminal's
// font, IBM Plex Mono (SIL OFL 1.1), served from the Worker with the page.
import { cpSync, existsSync, mkdirSync, rmSync } from "node:fs";
import { join } from "node:path";
import { ENGINE_DIR, WEB } from "./common";

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

// The weights and styles style.css uses, in the Latin and Latin Extended subsets.
const FONT = join(WEB, "node_modules", "@fontsource", "ibm-plex-mono");
mkdirSync(join(dist, "fonts"), { recursive: true });
for (const subset of ["latin", "latin-ext"])
  for (const face of ["400-normal", "500-normal", "600-normal", "400-italic"]) {
    const file = `ibm-plex-mono-${subset}-${face}.woff2`;
    cpSync(join(FONT, "files", file), join(dist, "fonts", file));
  }
cpSync(join(FONT, "LICENSE"), join(dist, "fonts", "LICENSE.txt"));
console.log("built dist/");
