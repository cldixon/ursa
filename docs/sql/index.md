# Ursa SQL

!!! info "Versions"

    The newest release is `ursa-cli` 0.9.0; see [Getting Started](getting-started.md#install) to install or upgrade it. Features marked **New in 0.7** or **New in 0.8** need that version or later (`ursa --version` says which you have). The SQL surface may still change between minor versions, and the [changelog](../changelog.md) records every change.

Ursa SQL is a SQL engine for graph analytics, used through a command-line tool called `ursa`. It works like DuckDB. You point it at Parquet, CSV, JSON or Arrow files, on your disk or in object storage, and query them with SQL. Graph algorithms are part of that SQL. Nothing needs converting or loading first.

```sql
CREATE VIEW ties AS SELECT * FROM 'karate_edges.csv';
CREATE PROPERTY GRAPH club
  EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));

SELECT * FROM pagerank(club, direction => 'both')
ORDER BY pagerank DESC
LIMIT 3;
```

```text
┌───────┬─────────────────────┐
│  id   │      pagerank       │
│ Int64 │       Float64       │
├───────┼─────────────────────┤
│    33 │ 0.10091893648817557 │
│     0 │ 0.09699751091990577 │
│    32 │ 0.07169303486619161 │
└───────┴─────────────────────┘
```

## Install

On Linux or macOS:

```bash
curl -fsSL https://cldixon.github.io/ursa/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://cldixon.github.io/ursa/install.ps1 | iex
```

Or `cargo install ursa-cli`, or a prebuilt binary from the [releases page](https://github.com/cldixon/ursa/releases). [Getting started](getting-started.md#install) has the details.

## What it is

- **Files are tables.** `SELECT * FROM 'edges.parquet'` reads the file: Parquet, CSV, JSON/NDJSON (`.json`, `.jsonl`, `.ndjson`) and Arrow. Globs, directories and compressed CSV and JSON work too. So do `s3://`, `gs://`, `az://` and `https://` URLs, and DuckDB's `read_parquet`, `read_csv` and `read_json`.
- **A graph is an edge table.** `CREATE PROPERTY GRAPH`, from the SQL standard (SQL/PGQ), names the edge table and its two endpoint columns. A node table with attributes is optional.
- **Algorithms are table functions.** `pagerank(club)` returns one row per node. Join it to anything on `id`. Parameters are named: `damping => 0.9`.
- **A database file keeps your work.** `ursa club.ursa` keeps your views, graphs and the graph indexes Ursa builds, so the next query does not rebuild them.

The engine is built on [Apache DataFusion](https://datafusion.apache.org/), so the rest of SQL works as DataFusion runs it: joins, aggregates, window functions, `COPY … TO`.

## How it relates to the Python library

Ursa has two front ends over one Rust core.

| | Python library | SQL engine |
| --- | --- | --- |
| Package | [`ursa-graph`](../install.md) on PyPI | [`ursa-cli`](getting-started.md#install): an install script, prebuilt binaries, and crates.io |
| You write | Python, with a Polars-shaped DataFrame API | SQL, in the `ursa` shell or with `ursa -c` |
| Graph | `ur.read_edges(...)` | `CREATE PROPERTY GRAPH` |
| Algorithm | `ur.pagerank(edges)` | `SELECT * FROM pagerank(g)` |

Both run the same kernels, from one catalog of algorithms. The same graph and the same parameters give the same values, and every algorithm matches the NetworkX function it names. The parameter names and defaults are the same in both.

A few things differ:

- **Row order.** A SQL result has no order unless you write `ORDER BY`. The Python library returns rows in a fixed order.
- **Subgraph nodes** (new in 0.7). A filtered edge frame in Python keeps the parent's nodes. A SQL graph's nodes are its vertex table's keys, as SQL/PGQ defines them, so a subgraph over a filtered view with the parent's vertex table keeps them too, and gives the same results as the Python frame when the vertex table lists the nodes in the parent's order (a graph numbers its nodes in its vertex table's order; see [edge order](statements.md)). Without a vertex table, a SQL graph's nodes are the endpoints of the edges it keeps. In both, `describe(g, nodes => 'touched')` describes only the nodes at an end of a kept edge.
- **Edges to no node.** A SQL edge whose source or destination is not a key of the vertex table is left out of the graph, as in DuckPGQ; `describe` counts them in `n_dropped_edges`, a column the Python library's `describe` does not have.
- **Layouts.** These are in the Python library only, for now.
- **Neighbour aggregation.** Python aggregates a column of a node frame: `ur.neighbors(edges).agg(ur.col("age").mean())`. SQL takes the values as a query, or as a column of the graph's node table: `neighbors_agg(g, values => (SELECT id, age FROM people), agg => 'mean')`.

## Where to go next

- [Getting started](getting-started.md): install `ursa`, define a graph and run algorithms on it.
- [The database file](database.md): what a `.ursa` file keeps, and how stored indexes work.
- [The shell](shell.md): dot-commands, init files and history.
- [Statements](statements.md): reading and writing files, `CREATE PROPERTY GRAPH`, `SHOW GRAPHS`, `DESCRIBE GRAPH`, `ATTACH` and the settings.
- [Differences from DuckDB](duckdb.md): what carries over from DuckDB, and what does not.
- [Function reference](reference/functions.md): every graph function, its parameters and its output.
- [The agent skill](https://github.com/cldixon/ursa/tree/main/skills): teach a coding agent to write Ursa SQL.
