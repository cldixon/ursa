# ursa-cli

`ursa` is a SQL engine for graph analytics, used from the command line. It reads Parquet, CSV, JSON/NDJSON (`.json`, `.jsonl`, `.ndjson`) and Arrow files where they are, on your disk or in object storage, and runs graph algorithms over them with SQL. There is no server and nothing to load first. An edge table is a graph.

On Linux or macOS, install the prebuilt binary with the install script:

```bash
curl -fsSL https://cldixon.github.io/ursa/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://cldixon.github.io/ursa/install.ps1 | iex
```

Or build it with Cargo:

```bash
cargo install ursa-cli
```

The binary is called `ursa`. The prebuilt binaries for Linux, macOS and Windows are also on the [releases page](https://github.com/cldixon/ursa/releases), with a SHA-256 checksum for each.

```bash
ursa club.ursa
```

```sql
CREATE VIEW ties AS SELECT * FROM 'karate_edges.csv';
CREATE PROPERTY GRAPH karate
  EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));

SELECT * FROM pagerank(karate, direction => 'both')
ORDER BY pagerank DESC
LIMIT 5;
```

Graphs are defined with SQL/PGQ `CREATE PROPERTY GRAPH`. Algorithms and traversals are table functions with named parameters. `SELECT * FROM ursa_functions()` lists them all. A `.ursa` database file keeps your views, graphs and the graph indexes Ursa builds, so the next session starts where you left off.

For scripts and programs, `ursa -c "<sql>"`, `ursa -f script.sql` and SQL on stdin run and exit; a script may use the shell's dot-commands (`.mode`, `.timer on`, `.read`), as in DuckDB. `--format json` writes results for a program to read. `ursa --skill` prints a guide to writing Ursa SQL for coding agents, and `ursa --help` ends with where to start.

`ursa serve [club.ursa | DIR]` opens the shell in a browser tab on your machine, over a local WebSocket to the native engine. It prints a link with a one-time token and opens it in your default browser (`--no-open` only prints it); Ctrl+C stops it. A database file is opened as `ursa club.ursa` opens one, and relative paths in queries resolve against its directory; a directory is the sessions' root; with neither, the session is in memory, in the current directory. `--port N` picks the port (4280 by default, or the next free one), and `--read-only` writes no files: `COPY … TO`, `INSERT INTO` a table of files, `.output`, `.once`, and anything that changes the database (opened read-only) are refused, for pairing with an agent.

The release binaries (the installers' and GitHub's) have `ursa serve` with its page built in (the `serve-embed` feature). `cargo install ursa-cli --features serve` builds it without the page: pass `--assets DIR`, a `web/dist` from Ursa's repository built with `bun run build`.

The engine is built on [Apache DataFusion](https://datafusion.apache.org/). It runs the same algorithms as Ursa's Python library ([`ursa-graph`](https://pypi.org/project/ursa-graph/)) and gives the same values.

- Documentation: <https://cldixon.github.io/ursa/sql/>
- Source: <https://github.com/cldixon/ursa>

Licensed under either of MIT or Apache-2.0, at your option.
