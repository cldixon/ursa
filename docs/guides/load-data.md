# Load data

An `EdgeFrame` needs two columns: the source and the destination of each edge. A `NodeFrame` needs one: the id. Everything else is up to you. This page shows each way to build a frame.

## From Python values

The constructors take row dicts or a dict of columns. `src=` and `dst=` name the endpoint columns. They do not rename them.

```python
import ursa as ur

edges = ur.EdgeFrame(
    {"from": [0, 0, 1, 2], "to": [1, 2, 2, 0], "amount": [5.0, 1.0, 2.0, 4.0]},
    src="from",
    dst="to",
)
nodes = ur.NodeFrame(
    [{"id": 0, "team": "red"}, {"id": 1, "team": "red"}, {"id": 2, "team": "blue"}],
    id="id",
)
print(edges.src_col, edges.dst_col, nodes.id_col)
print(edges.collect())
```

```text
from to id
shape: (4, 3)
┌───────┬───────┬────────┐
│ from  ┆ to    ┆ amount │
│ ---   ┆ ---   ┆ ---    │
│ int64 ┆ int64 ┆ double │
╞═══════╪═══════╪════════╡
│ 0     ┆ 1     ┆ 5      │
│ 0     ┆ 2     ┆ 1      │
│ 1     ┆ 2     ┆ 2      │
│ 2     ┆ 0     ┆ 4      │
└───────┴───────┴────────┘
```

For a graph written by hand, `from_edgelist` takes tuples. A third element becomes the `weight` column.

```python
tiny = ur.from_edgelist([("a", "b"), ("b", "c"), ("c", "a")])
print(tiny.collect())
```

```text
shape: (3, 2)
┌────────┬────────┐
│ src    ┆ dst    │
│ ---    ┆ ---    │
│ string ┆ string │
╞════════╪════════╡
│ a      ┆ b      │
│ b      ┆ c      │
│ c      ┆ a      │
└────────┴────────┘
```

Node ids can be integers or strings. One frame uses one kind.

## From a dataframe library

A frame built from a dataframe uses the same constructors. The data crosses as Arrow, with no copy.

=== "polars"

    ```python
    import polars as pl

    df = pl.DataFrame({"s": [1, 2, 3], "d": [2, 3, 1]})
    edges = ur.EdgeFrame(df, src="s", dst="d")      # or ur.from_polars(df, src="s", dst="d")
    print(edges.collect())
    ```

    ```text
    shape: (3, 2)
    ┌───────┬───────┐
    │ s     ┆ d     │
    │ ---   ┆ ---   │
    │ int64 ┆ int64 │
    ╞═══════╪═══════╡
    │ 1     ┆ 2     │
    │ 2     ┆ 3     │
    │ 3     ┆ 1     │
    └───────┴───────┘
    ```

=== "pandas"

    ```python
    import pandas as pd

    df = pd.DataFrame({"s": [1, 2, 3], "d": [2, 3, 1]})
    edges = ur.EdgeFrame(df, src="s", dst="d")      # or ur.from_pandas(df, src="s", dst="d")
    print(edges.collect())
    ```

    ```text
    shape: (3, 2)
    ┌───────┬───────┐
    │ s     ┆ d     │
    │ ---   ┆ ---   │
    │ int64 ┆ int64 │
    ╞═══════╪═══════╡
    │ 1     ┆ 2     │
    │ 2     ┆ 3     │
    │ 3     ┆ 1     │
    └───────┴───────┘
    ```

=== "pyarrow"

    ```python
    import pyarrow as pa

    tbl = pa.table({"s": [1, 2, 3], "d": [2, 3, 1]})
    edges = ur.EdgeFrame(tbl, src="s", dst="d")     # or ur.from_arrow(tbl, src="s", dst="d")
    print(edges.collect())
    ```

    ```text
    shape: (3, 2)
    ┌───────┬───────┐
    │ s     ┆ d     │
    │ ---   ┆ ---   │
    │ int64 ┆ int64 │
    ╞═══════╪═══════╡
    │ 1     ┆ 2     │
    │ 2     ┆ 3     │
    │ 3     ┆ 1     │
    └───────┴───────┘
    ```

Any object with an `__arrow_c_stream__` method works the same way.

## From a file

`scan_edges` and `scan_nodes` read Parquet and CSV. The read happens inside `collect()`, and a Parquet read gets only the columns the query needs.

```py
edges = ur.scan_edges("edges.parquet", src="from_id", dst="to_id")
nodes = ur.scan_nodes("nodes.csv", id="node_id")

result = nodes.with_columns(pagerank=ur.pagerank(edges)).collect()
```

The format comes from the file extension. A CSV file needs a header row. A glob reads several files as one table: `"edges/*.parquet"`.

`read_edges` and `read_nodes` are the eager forms. They return the rows at once.

### Object storage

The same scans read from S3, Google Cloud Storage, and Azure. `storage_options` holds the backend settings. They layer over the backend's default credential chain, so a machine with credentials in its environment needs few of them.

```py
edges = ur.scan_edges(
    "s3://my-bucket/graph/edges/*.parquet",
    src="from_id",
    dst="to_id",
    storage_options={"region": "us-east-1"},
)
```

A configured [`obstore`](https://developmentseed.org/obstore/) store works too: pass it as `store=`.

### HTTP

A plain `https://` URL reads one hosted file. The URL must point at the file and must not have a query string.

```py
edges = ur.scan_edges("https://example.com/data/edges.parquet", src="s", dst="d")
```

## From another graph library

Each function imports its library only when called.

=== "NetworkX"

    ```python
    import networkx as nx

    G = nx.les_miserables_graph()
    edges = ur.from_networkx(G)                     # a `weight` column, since every edge has one
    nodes = ur.nodes_from_networkx(G)               # one column per node attribute
    print(edges.collect().columns, nodes.collect().columns)
    ```

    ```text
    ['src', 'dst', 'weight'] ['id']
    ```

=== "NumPy"

    ```python
    import numpy as np

    adjacency = np.array([[0, 1, 1], [0, 0, 1], [1, 0, 0]])
    edges = ur.from_numpy(adjacency)                # each nonzero [i, j] is an edge i -> j
    print(edges.collect())
    ```

    ```text
    shape: (4, 2)
    ┌───────┬───────┐
    │ src   ┆ dst   │
    │ ---   ┆ ---   │
    │ int64 ┆ int64 │
    ╞═══════╪═══════╡
    │ 0     ┆ 1     │
    │ 0     ┆ 2     │
    │ 1     ┆ 2     │
    │ 2     ┆ 0     │
    └───────┴───────┘
    ```

=== "SciPy"

    ```python
    import numpy as np
    import scipy.sparse as sp

    matrix = sp.csr_matrix(np.array([[0, 2.0], [3.0, 0]]))
    edges = ur.from_scipy_sparse(matrix, weighted=True)
    print(edges.collect())
    ```

    ```text
    shape: (2, 3)
    ┌───────┬───────┬────────┐
    │ src   ┆ dst   ┆ weight │
    │ ---   ┆ ---   ┆ ---    │
    │ int64 ┆ int64 ┆ double │
    ╞═══════╪═══════╪════════╡
    │ 0     ┆ 1     ┆ 2      │
    │ 1     ┆ 0     ┆ 3      │
    └───────┴───────┴────────┘
    ```

## Null endpoints

A row with a null `src` or `dst` is an error by default. Pass `on_null="drop"` to remove those rows. Ursa then emits a warning with the count.

```python
edges = ur.EdgeFrame({"s": [1, 2, None], "d": [2, 3, 1]}, src="s", dst="d", on_null="drop")
print(edges.collect())
```

```text
shape: (2, 2)
┌───────┬───────┐
│ s     ┆ d     │
│ ---   ┆ ---   │
│ int64 ┆ int64 │
╞═══════╪═══════╡
│ 1     ┆ 2     │
│ 2     ┆ 3     │
└───────┴───────┘
UserWarning: on_null='drop': dropped 1 edge row(s) with a null src or dst endpoint.
```

## Bundled datasets

`ur.datasets` has four small graphs in the package and one larger graph that downloads on first use. See [Datasets](../reference/datasets.md).

```python
for info in ur.datasets.list_datasets():
    print(f"{info.name:12} {info.nodes:>5} nodes {info.edges:>6} edges  bundled={info.bundled}")
```

```text
karate          34 nodes     78 edges  bundled=True
lesmis          77 nodes    254 edges  bundled=True
florentine      15 nodes     20 edges  bundled=True
kite            10 nodes     18 edges  bundled=True
facebook      4039 nodes  88234 edges  bundled=False
```
