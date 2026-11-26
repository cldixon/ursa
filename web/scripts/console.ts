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
//   dist-console/console/vendor/codemirror.mjs     CodeMirror 6 (MIT), the SQL
//                                                  editor's, from
//                                                  scripts/codemirror.ts
//
// The console's own modules are plain JavaScript served as they are, like the
// shell's; they import the shell's shared modules (engine-client.js,
// socket-transport.js, graph-pane.js) and styles from dist/ by absolute path.
import { cpSync, mkdirSync, readdirSync, readFileSync, rmSync } from "node:fs";
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
  const cm = await Bun.build({
    entrypoints: [join(WEB, "scripts", "codemirror.ts")],
    target: "browser",
    format: "esm",
    minify: true,
    metafile: true,
  });
  if (!cm.success) {
    console.error(cm.logs.join("\n"));
    process.exit(1);
  }
  await Bun.write(join(out, "vendor", "codemirror.mjs"), cm.outputs[0]);
  // Every package in the bundle (from the build's metafile) must be MIT; its
  // LICENSE goes beside the bundle.
  const bundled = new Set(Object.keys(cm.metafile!.inputs).map((input) => input.match(/node_modules\/((?:@[^/]+\/)?[^/]+)/)?.[1]));
  bundled.delete(undefined);
  for (const name of [...bundled].sort() as string[]) {
    const dir = join(WEB, "node_modules", name);
    const { license } = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
    if (license !== "MIT") {
      console.error(`${name} is ${license}, not MIT: the console's bundle takes MIT packages only`);
      process.exit(1);
    }
    cpSync(join(dir, "LICENSE"), join(out, "vendor", `LICENSE-${name.replace(/^@/, "").replace("/", "-")}.txt`));
  }
  console.log(
    `built dist-console/ (arrow ${(built.outputs[0].size / 1024).toFixed(0)} KB, codemirror ${(cm.outputs[0].size / 1024).toFixed(0)} KB)`,
  );
}

if (import.meta.main) await buildConsole();
