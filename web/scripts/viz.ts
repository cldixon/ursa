// Build the graph view for the web: ursa-viz for wasm32 (the `web` feature,
// the wasm-release profile) and its JS glue (wasm-bindgen --target web), into
// .viz/<hash>/, named by a hash of both so a deploy never pairs one build's
// glue with another's .wasm. .viz/viz.json names the build the page loads.
// The page loads it only on the first `.graph` (public/graph.js).
//
//   bun scripts/viz.ts        (build.ts runs it; cargo does nothing when up to date)
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";
import { cargoTargetDir, REPO, run, VIZ_DIR } from "./common";

export function buildViz(): { module: string; bytes: number; gzipBytes: number } {
  run(
    "cargo",
    ["build", "-p", "ursa-viz", "--features", "web", "--target", "wasm32-unknown-unknown", "--profile", "wasm-release", "--locked"],
    { cwd: REPO },
  );
  const built = join(cargoTargetDir(), "wasm32-unknown-unknown", "wasm-release", "ursa_viz.wasm");

  const staging = join(VIZ_DIR, "staging");
  rmSync(VIZ_DIR, { recursive: true, force: true });
  mkdirSync(staging, { recursive: true });
  run("wasm-bindgen", ["--target", "web", "--no-typescript", "--remove-name-section", "--remove-producers-section", "--out-dir", staging, built]);

  const wasm = readFileSync(join(staging, "ursa_viz_bg.wasm"));
  const glue = readFileSync(join(staging, "ursa_viz.js"));
  const hash = createHash("sha256").update(glue).update(wasm).digest("hex").slice(0, 20);
  renameSync(staging, join(VIZ_DIR, hash));
  const manifest = {
    module: `/viz/${hash}/ursa_viz.js`,
    bytes: wasm.length + glue.length,
    gzipBytes: gzipSync(wasm, { level: 9 }).length + gzipSync(glue, { level: 9 }).length,
  };
  writeFileSync(join(VIZ_DIR, "viz.json"), JSON.stringify(manifest, null, 2) + "\n");
  const mb = (n: number) => `${(n / 1e6).toFixed(2)} MB`;
  console.log(`graph view: ${hash}, ${mb(manifest.bytes)} raw, ${mb(manifest.gzipBytes)} gzipped`);
  return manifest;
}

if (import.meta.main) buildViz();
