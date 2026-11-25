// Build the console spike's page (#536) into dist-console/, which `ursa serve`
// serves before dist/: console.html, its script and style, and apache-arrow
// (the Arrow IPC reader) bundled as one ES module, vendor/arrow.mjs. The
// deployed shell does not include it: `bun run build` makes dist/ alone.
//
//   bun run console        (after `bun run build`, whose dist/ it sits beside)
import { cpSync, mkdirSync, rmSync } from "node:fs";
import { join } from "node:path";
import { WEB } from "./common";

const out = join(WEB, "dist-console");
rmSync(out, { recursive: true, force: true });
mkdirSync(join(out, "vendor"), { recursive: true });
for (const file of ["console.html", "console.js", "console.css"]) cpSync(join(WEB, "console", file), join(out, file));
const built = await Bun.build({
  entrypoints: [join(WEB, "node_modules", "apache-arrow", "Arrow.dom.mjs")],
  target: "browser",
  format: "esm",
  minify: true,
});
if (!built.success) {
  console.error(built.logs.join("\n"));
  process.exit(1);
}
await Bun.write(join(out, "vendor", "arrow.mjs"), built.outputs[0]);
console.log(`built dist-console/ (arrow ${(built.outputs[0].size / 1024).toFixed(0)} KB)`);
