// Build the Ursa Console's page (design/console.md §6) into dist-console/,
// beside dist/: `bun run build` calls it. `ursa serve` serves dist-console/
// before dist/ (and builds both into its binary with the `embed` feature), so
// the console is at /console/ and the shell at /. The console's files sit in
// a directory of their own so that the deployed shell, dist/, has none of
// them.
//
//   dist-console/console/index.html                web/console/console.html
//   dist-console/console/*.js, *.css               web/console/, as they are
//   dist-console/console/vendor/arrow.mjs          apache-arrow (Apache-2.0),
//                                                  the Arrow IPC reader, as
//                                                  one ES module
//
// The console's own modules are plain JavaScript served as they are, like the
// shell's; they import the shell's shared modules (engine-client.js,
// socket-transport.js, graph-pane.js) and styles from dist/ by absolute path.
import { cpSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { join } from "node:path";
import { WEB } from "./common";

export const CONSOLE_DIST = join(WEB, "dist-console");

export async function buildConsole(): Promise<void> {
  const out = join(CONSOLE_DIST, "console");
  rmSync(CONSOLE_DIST, { recursive: true, force: true });
  mkdirSync(join(out, "vendor"), { recursive: true });
  for (const file of readdirSync(join(WEB, "console"))) {
    // console.html is the page at /console/: its index.
    if (/\.(html|js|css)$/.test(file)) cpSync(join(WEB, "console", file), join(out, file === "console.html" ? "index.html" : file));
  }
  const ARROW = join(WEB, "node_modules", "apache-arrow");
  const built = await Bun.build({
    entrypoints: [join(ARROW, "Arrow.dom.mjs")],
    target: "browser",
    format: "esm",
    minify: true,
  });
  if (!built.success) {
    console.error(built.logs.join("\n"));
    process.exit(1);
  }
  await Bun.write(join(out, "vendor", "arrow.mjs"), built.outputs[0]);
  cpSync(join(ARROW, "LICENSE.txt"), join(out, "vendor", "LICENSE-arrow.txt"));
  cpSync(join(ARROW, "NOTICE.txt"), join(out, "vendor", "NOTICE-arrow.txt"));
  console.log(`built dist-console/ (arrow ${(built.outputs[0].size / 1024).toFixed(0)} KB)`);
}

if (import.meta.main) await buildConsole();
