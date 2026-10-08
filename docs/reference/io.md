# Input and output

## Scans

A scan reads a file inside `collect()`. The read pushes the projection into the file: a Parquet scan reads only the columns the plan needs.

### `ur.scan_edges`

```py
ur.scan_edges(
    path: str | list[str], *,
    src: str, dst: str,
    storage_options: dict | None = None,
    store=None,
    on_null: str = "error",
) -> EdgeFrame
```

| Parameter | Description |
|---|---|
| `path` | A local path, an object-storage URL (`s3://`, `gs://`, `az://`), or an `http(s)://` URL. A glob is allowed on local and object-storage paths. A list gives several paths. |
| `src`, `dst` | The endpoint columns. |
| `storage_options` | Backend settings for object storage, for example `{"region": "us-east-1"}`. They layer over the backend's default credential chain. |
| `store` | A configured [`obstore`](https://developmentseed.org/obstore/) store, as an alternative to `storage_options`. |
| `on_null` | `"error"` or `"drop"`, as for `EdgeFrame`. |

The format comes from the file extension: `.parquet` or `.csv`. A CSV file must have a header row.

An HTTP URL must point at one file and must not carry a query string. A presigned URL does not work over HTTP. Use the `s3://` backend with `storage_options` for signed access.

### `ur.scan_nodes`

```py
ur.scan_nodes(
    path: str | list[str], *,
    id: str,
    storage_options: dict | None = None,
    store=None,
) -> NodeFrame
```

The same as `scan_edges`, for a node attribute table.

### `ur.read_edges`, `ur.read_nodes`

```py
ur.read_edges(path, **kwargs) -> MaterializedFrame
ur.read_nodes(path, **kwargs) -> MaterializedFrame
```

Eager forms: `scan_edges(path, **kwargs).collect()` and `scan_nodes(path, **kwargs).collect()`.

## Typed constructors

These are aliases for `ur.EdgeFrame(data, src=, dst=)` and `ur.NodeFrame(data, id=)`. Pass `src` and `dst` for an `EdgeFrame`, or `id` for a `NodeFrame`.

| Function | Input |
|---|---|
| `ur.from_arrow(tbl, *, src, dst, on_null="error")` / `ur.from_arrow(tbl, *, id)` | A `pyarrow.Table` or `RecordBatch`. No copy. |
| `ur.from_polars(df, *, src, dst)` / `ur.from_polars(df, *, id)` | A `polars.DataFrame`. No copy. |
| `ur.from_pandas(df, *, src, dst)` / `ur.from_pandas(df, *, id)` | A `pandas.DataFrame`. The index is dropped. |

### `ur.from_edgelist`

```py
ur.from_edgelist(edges, *, weighted: bool | None = None, on_null: str = "error") -> EdgeFrame
```

Builds an `EdgeFrame` from an iterable of tuples. Each tuple is `(src, dst)` or `(src, dst, weight)`. A third element becomes the `weight` column. By default the first tuple sets the arity, and every tuple must match it. Pass `weighted=True` or `weighted=False` to require one arity.

The result has the columns `src`, `dst`, and (if weighted) `weight`.

## Interop

Each function imports its library only when called. Install the matching [extra](../install.md#optional-extras).

### `ur.from_networkx`

```py
ur.from_networkx(graph, *, weight: str = "weight") -> EdgeFrame
```

One row per edge. If every edge has the `weight` attribute, the frame has a `weight` column. Node ids are the NetworkX node labels. An undirected `Graph` gives each edge once. Node attributes are not included; use `nodes_from_networkx` for them.

### `ur.nodes_from_networkx`

```py
ur.nodes_from_networkx(graph, *, id: str = "id") -> NodeFrame
```

One row per node, with one column per attribute key that appears on any node. A missing value is null.

### `ur.from_numpy`

```py
ur.from_numpy(array, *, kind: str = "auto", weighted: bool = False) -> EdgeFrame
```

| `kind` | Shape | Meaning |
|---|---|---|
| `"adjacency"` | `(N, N)` | Each nonzero entry `[i, j]` is an edge `i → j`. With `weighted=True`, the entry becomes the `weight`. |
| `"edges"` | `(M, 2)` or `(M, 3)` | Each row is `[src, dst]` or `[src, dst, weight]`. |
| `"auto"` | | `"adjacency"` for a square array, else `"edges"`. A `(2, 2)` array is ambiguous and resolves to `"adjacency"`. |

### `ur.from_scipy_sparse`

```py
ur.from_scipy_sparse(matrix, *, weighted: bool = False) -> EdgeFrame
```

Any `scipy.sparse` format. Each stored entry `[i, j]` is an edge `i → j`. An explicitly stored zero is kept as an edge.

## Output

See [`MaterializedFrame`](frames.md#urmaterializedframe) for `to_arrow`, `to_polars`, `to_dicts`, `sink_parquet`, and `sink_csv`.
