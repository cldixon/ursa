// A Share link (`#run=…`) for the Ursa Web Shell, encoded as app.js's
// shareUrl() encodes one: the commands as a JSON array of strings, UTF-8,
// base64url with no padding. Opening it lists the commands and asks before it
// runs them. The docs' "Try it live" links are made with this, so they can be
// made again when an example changes:
//
//   bun scripts/share-link.ts example.sql        # or the SQL on stdin
//   bun scripts/share-link.ts --base http://localhost:8799 example.sql
//
// The input is split into commands as the shell records them: a statement
// runs up to the line that ends with `;`, a dot-command is a line of its own,
// and `--` comment lines go with the command after them. Blank lines between
// commands are dropped.

export const SHELL_URL = "https://ursa-shell.cl-dixon.workers.dev";

/** The commands in `text`, one per statement or dot-command. */
export function splitCommands(text: string): string[] {
  const commands: string[] = [];
  let lines: string[] = [];
  const flush = () => {
    if (lines.some((l) => l.trim())) commands.push(lines.join("\n").trim());
    lines = [];
  };
  for (const line of text.split("\n")) {
    const t = line.trim();
    const pending = lines.some((l) => l.trim() && !l.trim().startsWith("--"));
    if (!t && !pending) continue;
    lines.push(line.trimEnd());
    if ((t.startsWith(".") && !pending) || t.endsWith(";")) flush();
  }
  flush();
  return commands;
}

/** The link that replays `commands` in the shell at `base`. */
export function shareLink(commands: string[], base = SHELL_URL): string {
  const encoded = Buffer.from(JSON.stringify(commands), "utf8").toString("base64").replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  return `${base.replace(/\/$/, "")}/#run=${encoded}`;
}

/** The commands a link replays, as app.js's sharedCommands() reads them. */
export function linkCommands(link: string): string[] {
  const m = link.match(/#run=([A-Za-z0-9_-]+)$/);
  if (!m) return [];
  return JSON.parse(Buffer.from(m[1].replace(/-/g, "+").replace(/_/g, "/"), "base64").toString("utf8"));
}

if (import.meta.main) {
  const args = Bun.argv.slice(2);
  let base = SHELL_URL;
  const at = args.indexOf("--base");
  if (at >= 0) [, base] = args.splice(at, 2);
  const text = args[0] ? await Bun.file(args[0]).text() : await new Response(Bun.stdin.stream()).text();
  const commands = splitCommands(text);
  if (!commands.length) {
    console.error("no commands to encode");
    process.exit(1);
  }
  console.log(shareLink(commands, base));
}
