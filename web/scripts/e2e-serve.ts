// Run the e2e suite (tests/e2e.ts) against `ursa serve`: the shell's page over
// a WebSocket to a native session, in place of the wasm engine in a worker.
//
//   bun scripts/e2e-serve.ts [port] [ursa binary]      (URSA_BIN= also names the binary)
//
// It starts `ursa serve .data --port PORT` (a build with `--features serve`;
// by default $CARGO_TARGET_DIR/release/ursa), reads the token from the URL it
// prints, and runs the suite with URSA_SERVE_TOKEN set, which gives every
// browser context the token's cookie and skips the checks of browser-only
// features (the suite prints each as "skip"). Build dist/ first
// (`bun run build`). At the end it stops the server as Ctrl+C would, and
// fails if it does not stop gracefully.
import { join } from "node:path";
import { cargoTargetDir, DATA_DIR, WEB } from "./common";

const port = Number(Bun.argv[2] ?? 8901);
const bin = Bun.argv[3] ?? process.env.URSA_BIN ?? join(cargoTargetDir(), "release", "ursa");
const server = Bun.spawn([bin, "serve", DATA_DIR, "--port", String(port), "--assets", join(WEB, "dist")], {
  stdout: "pipe",
  stderr: "inherit",
});
let printed = "";
const reader = server.stdout.getReader();
let token: string | undefined;
while (!token) {
  const { value, done } = await reader.read();
  if (done) {
    console.error(`ursa serve stopped:\n${printed}`);
    process.exit(1);
  }
  printed += new TextDecoder().decode(value);
  token = printed.match(/\?token=([A-Za-z0-9_-]+)/)?.[1];
}
const served = Number(printed.match(/127\.0\.0\.1:(\d+)/)?.[1]);
console.log(printed.trim());
const suite = Bun.spawn(["bun", join(WEB, "tests", "e2e.ts"), `http://localhost:${served}`, ...Bun.argv.slice(4)], {
  env: { ...process.env, URSA_SERVE_TOKEN: token },
  stdout: "inherit",
  stderr: "inherit",
});
const code = await suite.exited;
// Ctrl+C: the server cancels what runs, closes its sessions and exits 0.
server.kill("SIGINT");
const stopped = await Promise.race([server.exited, Bun.sleep(10_000).then(() => "timeout" as const)]);
if (stopped !== 0) {
  console.error(`ursa serve did not stop gracefully on Ctrl+C (${stopped})`);
  server.kill();
  process.exit(code || 1);
}
process.exit(code);
