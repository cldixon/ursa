# Ursa

**Polars-shaped dataframes for graph data.**

Ursa is a graph analytics library for Python. You write a query the way you write a Polars query. A Rust engine runs it: Apache Arrow for the data, [DataFusion](https://datafusion.apache.org/) for the plan, and parallel, deterministic kernels for the algorithms.

There is no `Graph` object. An edge table *is* the graph. Every operation returns a table.

**Documentation: <https://ursa.cldixon.dev>**

## Install

```bash
pip install ursa-graph
# or
uv add ursa-graph
```

The package on PyPI is **`ursa-graph`**. The import name is **`ursa`**. Wheels include the compiled core, so you need no Rust toolchain. Python 3.10 or later.

`polars` is optional. Install `pip install 'ursa-graph[polars]'` for `.to_polars()` and `ur.from_polars()`. Data crosses the boundary as Arrow either way.

## Try it

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
    .collect()          # one query in the engine; the result is Arrow data
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

To run the same query over your own data, change the first line:

```python
edges = ur.scan_edges("edges.parquet", src="from_id", dst="to_id")
```

Nothing after that line changes. See the [quickstart](https://ursa.cldixon.dev/quickstart/).

## What it does

- **A dataframe API.** `with_columns`, `filter`, `sort`, `select`, `group_by`, `join`. Lazy until `collect()`. If you know Polars, you know the shape.
- **The standard algorithms.** Degree, PageRank, connected components, triangle count, clustering coefficient, closeness, betweenness, Louvain, label propagation. Each with a weighted form, where a weight is an expression over edge columns. Neighbour aggregation. Hops, shortest paths, and random walks.
- **Your data, where it is.** Parquet and CSV, local or on S3, GCS, and Azure. Polars, pandas, pyarrow, NetworkX, NumPy, and SciPy in memory. Arrow in, Arrow out, with no copies.
- **Results you can check.** Every kernel is deterministic on any thread count. Each one is [pinned to the NetworkX call it matches](https://ursa.cldixon.dev/semantics/), and the test suite checks it.

Ursa is version 0.3 and in active development. The [Limits](https://ursa.cldixon.dev/limits/) page lists what does not run yet.

## How it is built

| Layer | Language | Role |
|---|---|---|
| `python/ursa` | Python | The API. Builds a plan. |
| `ursa-plan` | Rust | Turns the plan into one DataFusion query, with graph operations as plan nodes. |
| `ursa-core` | Rust | The CSR topology index and the kernels. Parallel with `rayon`; compiles for `wasm32` without it. |
| `ursa-py` | Rust | PyO3 bindings. Arrow crosses as zero-copy capsules; the GIL is released during compute. |

The design document is [`design/SPEC.md`](design/SPEC.md).

## Develop

```bash
uv sync                  # builds the extension and installs the dev tools
uv run pytest            # the Python suite, including every example in the docs
cargo test --workspace   # the Rust suites
uv run ruff check . && uv run ty check

uv sync --group docs && uv run zensical serve   # the documentation site, served locally
```

The docs are in [`docs/`](docs/), built with [Zensical](https://zensical.org/) and served by GitHub Pages. Every ```` ```python ```` block in them runs in the test suite, so an example that stops working fails CI. The benchmark harness is in [`benchmarks/`](benchmarks/).

## License

MIT OR Apache-2.0.
