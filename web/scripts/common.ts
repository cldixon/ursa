// Shared helpers for the build and upload scripts. Run from web/ with bun.
import { spawnSync } from "node:child_process";
import { resolve } from "node:path";

export const WEB = resolve(import.meta.dir, "..");
export const REPO = resolve(WEB, "..");
export const ENGINE_DIR = resolve(WEB, ".engine");
export const DATA_DIR = resolve(WEB, ".data");
export const BUCKET = "ursa-graph";

/** Run a command, inheriting stdio; exit if it fails. */
export function run(cmd: string, args: string[], opts: { cwd?: string } = {}): void {
  console.log(`$ ${cmd} ${args.join(" ")}`);
  const r = spawnSync(cmd, args, { stdio: "inherit", cwd: opts.cwd ?? WEB });
  if (r.status !== 0) {
    console.error(`${cmd} failed (${r.status ?? r.signal})`);
    process.exit(1);
  }
}

/** Cargo's target directory (honours CARGO_TARGET_DIR). */
export function cargoTargetDir(): string {
  const r = spawnSync("cargo", ["metadata", "--format-version", "1", "--no-deps"], {
    cwd: REPO,
    encoding: "utf8",
  });
  if (r.status !== 0) throw new Error(r.stderr);
  return JSON.parse(r.stdout).target_directory;
}

/**
 * Upload one file to the bucket with the cf CLI. `--local` writes cf dev's
 * simulated bucket instead (under .wrangler/state, which `cf dev` reads).
 */
export function put(key: string, file: string, contentType: string, local: boolean): void {
  const args = ["cf", "r2", "objects", "put", key, "--bucket-name", BUCKET, "--content-type", contentType, "--file", file];
  if (local) args.push("--local", "--persist-to", ".wrangler/state");
  run("bunx", args);
}
