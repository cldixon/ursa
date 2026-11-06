// Build the engine for the web: ursa-wasm for wasm32 (the wasm-release profile),
// its JS glue (wasm-bindgen --target web), and a brotli copy of the .wasm named
// by its content hash. Writes .engine/: pkg/ (glue + .wasm), <hash>.wasm.br and
// engine.json, which names the build the page loads.
//
//   bun run engine            (needs the wasm32 target and wasm-bindgen-cli; see ursa-wasm/README.md)
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { brotliCompressSync, constants } from "node:zlib";
import { cargoTargetDir, ENGINE_DIR, REPO, run } from "./common";

run("cargo", ["build", "-p", "ursa-wasm", "--target", "wasm32-unknown-unknown", "--profile", "wasm-release", "--locked"], {
  cwd: REPO,
});
const built = join(cargoTargetDir(), "wasm32-unknown-unknown", "wasm-release", "ursa_wasm.wasm");

rmSync(ENGINE_DIR, { recursive: true, force: true });
const pkg = join(ENGINE_DIR, "pkg");
mkdirSync(pkg, { recursive: true });
run("wasm-bindgen", ["--target", "web", "--remove-name-section", "--remove-producers-section", "--out-dir", pkg, built]);

const wasm = readFileSync(join(pkg, "ursa_wasm_bg.wasm"));
const hash = createHash("sha256").update(wasm).digest("hex").slice(0, 20);
const br = brotliCompressSync(wasm, {
  params: { [constants.BROTLI_PARAM_QUALITY]: 11, [constants.BROTLI_PARAM_SIZE_HINT]: wasm.length },
});
writeFileSync(join(ENGINE_DIR, `${hash}.wasm.br`), br);

const version = JSON.parse(
  readFileSync(join(REPO, "ursa-wasm", "Cargo.toml"), "utf8").match(/^version = (".*")$/m)?.[1] ?? '"unknown"',
);
const manifest = { version, wasm: `/engine/${hash}.wasm`, bytes: wasm.length, brotliBytes: br.length };
writeFileSync(join(ENGINE_DIR, "engine.json"), JSON.stringify(manifest, null, 2) + "\n");
const mb = (n: number) => `${(n / 1e6).toFixed(1)} MB`;
console.log(`engine ${version}: ${hash}.wasm, ${mb(wasm.length)} raw, ${mb(br.length)} brotli`);
