// Assemble dist/, the static assets: public/, and the engine's JS glue and
// engine.json from .engine/ (run `bun run engine` first), and the terminal's
// font, IBM Plex Mono (SIL OFL 1.1), and xterm.js (MIT), the terminal, served
// from the Worker with the page.
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

// The terminal: xterm.js and its fit addon, as ES modules.
const XTERM = join(WEB, "node_modules", "@xterm");
mkdirSync(join(dist, "vendor"), { recursive: true });
cpSync(join(XTERM, "xterm", "lib", "xterm.mjs"), join(dist, "vendor", "xterm.mjs"));
cpSync(join(XTERM, "xterm", "css", "xterm.css"), join(dist, "vendor", "xterm.css"));
cpSync(join(XTERM, "addon-fit", "lib", "addon-fit.mjs"), join(dist, "vendor", "addon-fit.mjs"));
cpSync(join(XTERM, "xterm", "LICENSE"), join(dist, "vendor", "LICENSE-xterm.txt"));
console.log("built dist/");
