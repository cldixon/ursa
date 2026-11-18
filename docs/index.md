# Ursa

**Graph analytics in SQL.**

Ursa is a SQL engine for graphs. Point the `ursa` command-line tool at an edge list in Parquet, CSV or JSON, on your disk or in object storage, name it as a graph, and run PageRank, Louvain, shortest paths and the rest as table functions. It works like DuckDB: one binary, no server, and a database file that keeps your work. A Rust engine runs it: Apache Arrow for the data, DataFusion for the plan, and parallel, deterministic kernels for the algorithms.

The same engine is also a Python library, with a Polars-shaped API: see [From Python](#from-python) below.

## Try it in your browser

[Ursa Shell](https://ursa-shell.cl-dixon.workers.dev) is the `ursa` shell in a web page, with nothing to install. It runs the same engine, compiled to WebAssembly, in your browser. Pick a dataset from its menu, or import a file of your own: it stays on your machine.

## Install

On Linux or macOS:

```bash
curl -fsSL https://cldixon.github.io/ursa/install.sh | sh
```

On Windows, in PowerShell: `irm https://cldixon.github.io/ursa/install.ps1 | iex`. Or `cargo install ursa-cli`. [Getting started](sql/getting-started.md#install) has the details.

## First query

Download Zachary's karate club, 78 friendships between 34 members, and start the shell:

```bash
curl -LO https://raw.githubusercontent.com/cldixon/ursa/main/python/ursa/datasets/data/karate_edges.csv
ursa
```

A file is a table. Name it as a graph, then run an algorithm over it:

```sql
CREATE PROPERTY GRAPH club
  EDGE TABLES ('karate_edges.csv' SOURCE KEY (src) DESTINATION KEY (dst));

SELECT id, round(pagerank, 4) AS pagerank
FROM pagerank(club, direction => 'both')
ORDER BY pagerank DESC
LIMIT 5;
```

```text
┌───────┬──────────┐
│  id   │ pagerank │
│ Int64 │ Float64  │
├───────┼──────────┤
│    33 │   0.1009 │
│     0 │    0.097 │
│    32 │   0.0717 │
│     2 │   0.0571 │
│     1 │   0.0529 │
└───────┴──────────┘
```

[Getting started](sql/getting-started.md) goes on from here: node attributes, several algorithms at once, weights, subgraphs, traversals and `GRAPH_TABLE` patterns. [Ursa SQL](sql/index.md) is the overview.

## From Python

```bash
pip install ursa-graph
```

The package is `ursa-graph`. The import name is `ursa`. Wheels include the compiled core, so you need no Rust toolchain. Python 3.10 or later. See [Install](install.md) for the optional extras.

There is no `Graph` object. An edge table *is* the graph. Every operation returns a table. Paste this into a Python session. It needs no files and no network.

```python
import ursa as ur

edges = ur.datasets.load_karate()   # 34 nodes, 78 edges, bundled with the package

top = (
    edges.nodes()
    .with_columns(
        degree=ur.degree(edges, direction="both"),
        triangles=ur.triangle_count(edges),
        community=ur.louvain(edges, seed=1),
    )
    .sort("degree", descending=True)
    .head(5)
    .collect()
)
print(top)
```

```text
shape: (5, 4)
┌───────┬────────┬───────────┬───────────┐
│ id    ┆ degree ┆ triangles ┆ community │
│ ---   ┆ ---    ┆ ---       ┆ ---       │
│ int64 ┆ uint32 ┆ uint32    ┆ uint32    │
╞═══════╪════════╪═══════════╪═══════════╡
│ 33    ┆ 17     ┆ 15        ┆ 2         │
│ 0     ┆ 16     ┆ 18        ┆ 0         │
│ 32    ┆ 12     ┆ 13        ┆ 2         │
│ 2     ┆ 10     ┆ 11        ┆ 0         │
│ 1     ┆ 9      ┆ 12        ┆ 0         │
└───────┴────────┴───────────┴───────────┘
```

What happened:

1. `edges.nodes()` starts a lazy table of the node ids.
2. `with_columns(...)` adds three columns. Each is a graph algorithm over `edges`.
3. `sort(...)` and `head(5)` shape the rows.
4. `collect()` runs all of it as one query and returns Arrow data.

To run the same query over your own data, change the first line:

```py
edges = ur.scan_edges("edges.parquet", src="from_id", dst="to_id")
```

Nothing after that line changes.

## What you get

<div class="grid cards" markdown>

-   :material-console:{ .lg .middle } **SQL, like DuckDB**

    ---

    Files are tables, a graph is an edge table, and algorithms are table functions you join on `id`. An interactive shell, `ursa -c` for scripts, and output as tables, CSV, JSON or Arrow.

    [:octicons-arrow-right-24: Ursa SQL](sql/index.md)

-   :material-table-arrow-right:{ .lg .middle } **A dataframe API**

    ---

    In Python: `with_columns`, `filter`, `sort`, `group_by`, `join`. If you know Polars, you know the shape. Lazy until `collect()`.

    [:octicons-arrow-right-24: Concepts](concepts/index.md)

-   :material-graph-outline:{ .lg .middle } **The standard algorithms**

    ---

    Degree, PageRank, components, triangles, clustering, closeness, betweenness, Louvain, label propagation. Each with a weighted form. Hops, shortest paths, random walks.

    [:octicons-arrow-right-24: Compute graph metrics](guides/metrics.md)

-   :material-database-arrow-down-outline:{ .lg .middle } **Your data, where it is**

    ---

    Parquet and CSV, local or on S3, GCS, and Azure. Polars, pandas, pyarrow, NetworkX, NumPy, and SciPy in memory. Arrow in, Arrow out, no copies.

    [:octicons-arrow-right-24: Load data](guides/load-data.md)

-   :material-check-decagram-outline:{ .lg .middle } **Results you can check**

    ---

    Every kernel is deterministic on any thread count. Each one is pinned to the NetworkX call it matches, and the test suite checks it.

    [:octicons-arrow-right-24: Semantics vs NetworkX](semantics.md)

</div>

## Next steps

- [Ursa Shell](https://ursa-shell.cl-dixon.workers.dev): try Ursa SQL in your browser.
- [Getting started with Ursa SQL](sql/getting-started.md): install `ursa`, define a graph and run algorithms on it.
- [Quickstart](quickstart.md): a ten-minute Python session that uses most of the API.
- [Guides](guides/index.md): the Python library, one task per page, with code that runs.
- [Reference](reference/index.md): every public name in Python; the [function reference](sql/reference/functions.md) for SQL.

The `ursa` CLI is version 0.9 and the Python library 0.5; both are in active development. The [Limits](limits.md) page lists what does not run yet.
