---
icon: lucide/house
hide:
  - navigation
---

# Ursa

**Polars-shaped dataframes for graph data.**

Ursa is a graph analytics library for Python. You write a query the way you write a Polars query. A Rust engine runs it: Apache Arrow for the data, DataFusion for the plan, and parallel, deterministic kernels for the algorithms.

There is no `Graph` object. An edge table *is* the graph. Every operation returns a table.

## Install

```bash
pip install ursa-graph
```

The package is `ursa-graph`. The import name is `ursa`. Wheels include the compiled core, so you need no Rust toolchain. Python 3.10 or later. See [Install](install.md) for the optional extras.

## First example

Paste this into a Python session. It needs no files and no network.

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

-   :material-table-arrow-right:{ .lg .middle } **A dataframe API**

    ---

    `with_columns`, `filter`, `sort`, `group_by`, `join`. If you know Polars, you know the shape. Lazy until `collect()`.

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

- [Quickstart](quickstart.md): a ten-minute session that uses most of the API.
- [Guides](guides/index.md): one task per page, with code that runs.
- [Reference](reference/index.md): every public name.

Ursa is version 0.3 and in active development. The [Limits](limits.md) page lists what does not run yet.
