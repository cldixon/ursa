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

The engine is built on [Apache DataFusion](https://datafusion.apache.org/). It runs the same algorithms as Ursa's Python library ([`ursa-graph`](https://pypi.org/project/ursa-graph/)) and gives the same values.

- Documentation: <https://cldixon.github.io/ursa/sql/>
- Source: <https://github.com/cldixon/ursa>

Licensed under either of MIT or Apache-2.0, at your option.
