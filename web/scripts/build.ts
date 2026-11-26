// Assemble dist/, the static assets: public/, and the engine's JS glue and
// engine.json from .engine/ (run `bun run engine` first), the graph view
// (ursa-viz, built here by scripts/viz.ts into dist/viz/) and its reader of
// the engine's graphs (ursa-viz/ui/ugr.js, as dist/ugr.js), and the terminal's
// font, IBM Plex Mono (SIL OFL 1.1), and xterm.js (MIT), the terminal, served
// from the Worker with the page. Then the console, `ursa serve`'s page, into
// dist-console/ (scripts/console.ts).
//
//   bun scripts/build.ts [--no-engine]
//
// --no-engine leaves the wasm engine out (no `bun run engine` needed): the
// page `ursa serve` embeds (`ursa-cli`'s `serve-embed`), which talks to the
// native engine and never loads the wasm one. The release builds use it.
import { cpSync, existsSync, mkdirSync, rmSync } from "node:fs";
import { join } from "node:path";
import { ENGINE_DIR, REPO, VIZ_DIR, WEB } from "./common";
import { buildConsole } from "./console";
import { buildViz } from "./viz";

const dist = join(WEB, "dist");
const withEngine = !process.argv.includes("--no-engine");
if (withEngine && !existsSync(join(ENGINE_DIR, "engine.json"))) {
  console.error("no engine build: run `bun run engine` first (or build for `ursa serve` only, with --no-engine)");
  process.exit(1);
}
// The graph view: a few MB the page loads only on the first `.graph`.
buildViz();
rmSync(dist, { recursive: true, force: true });
cpSync(join(WEB, "public"), dist, { recursive: true });
if (withEngine) {
  mkdirSync(join(dist, "engine"), { recursive: true });
  cpSync(join(ENGINE_DIR, "pkg", "ursa_wasm.js"), join(dist, "engine", "ursa_wasm.js"));
  cpSync(join(ENGINE_DIR, "engine.json"), join(dist, "engine", "engine.json"));
}
cpSync(VIZ_DIR, join(dist, "viz"), { recursive: true });
// The reader of UGR, the format the engine sends a graph in: ursa-viz's own,
// so the page and every other front end read it with the same code.
cpSync(join(REPO, "ursa-viz", "ui", "ugr.js"), join(dist, "ugr.js"));

// The weights and styles style.css uses, in the Latin and Latin Extended subsets.
const FONT = join(WEB, "node_modules", "@fontsource", "ibm-plex-mono");
mkdirSync(join(dist, "fonts"), { recursive: true });
for (const subset of ["latin", "latin-ext"])
  for (const face of ["400-normal", "500-normal", "600-normal", "400-italic"]) {
    const file = `ibm-plex-mono-${subset}-${face}.woff2`;
    cpSync(join(FONT, "files", file), join(dist, "fonts", file));
  }
cpSync(join(FONT, "LICENSE"), join(dist, "fonts", "LICENSE.txt"));

// The terminal: xterm.js and its fit addon, as ES modules.
const XTERM = join(WEB, "node_modules", "@xterm");
mkdirSync(join(dist, "vendor"), { recursive: true });
cpSync(join(XTERM, "xterm", "lib", "xterm.mjs"), join(dist, "vendor", "xterm.mjs"));
cpSync(join(XTERM, "xterm", "css", "xterm.css"), join(dist, "vendor", "xterm.css"));
cpSync(join(XTERM, "addon-fit", "lib", "addon-fit.mjs"), join(dist, "vendor", "addon-fit.mjs"));
cpSync(join(XTERM, "xterm", "LICENSE"), join(dist, "vendor", "LICENSE-xterm.txt"));
console.log(withEngine ? "built dist/" : "built dist/, without the wasm engine (for ursa serve)");

// The console (`ursa serve`'s page), in dist-console/: never in dist/, which
// is deployed.
await buildConsole();
