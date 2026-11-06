// Upload the engine .wasm that `bun run engine` built (brotli, content-hashed)
// to the bucket, as engine/<hash>.wasm.br. Upload before deploying the page that
// names it. `--local` seeds cf dev's simulated bucket instead.
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { ENGINE_DIR, put } from "./common";

const local = Bun.argv.includes("--local");
const { wasm } = JSON.parse(readFileSync(join(ENGINE_DIR, "engine.json"), "utf8")) as { wasm: string };
const file = wasm.slice("/engine/".length); // <hash>.wasm
put(`engine/${file}.br`, join(ENGINE_DIR, `${file}.br`), "application/wasm", local);
