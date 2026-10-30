# Input and output

## Scans

A scan reads a file inside `collect()`. The read pushes the projection into the file: a Parquet scan reads only the columns the plan needs.

### `ur.scan_edges`

```py
ur.scan_edges(
    path: str | Path | list[str | Path], *,
    src: str, dst: str,
    storage_options: dict | None = None,
    store=None,
    on_null: str = "error",
    delimiter: str = ",",
    has_header: bool = True,
    quote: str = '"',
    escape: str | None = None,
    null_value: str | None = None,
    comment: str | None = None,
) -> EdgeFrame
```

| Parameter | Description |
|---|---|
| `path` | A local path, an object-storage URL (`s3://`, `gs://`, `az://`), or an `http(s)://` URL, as a string or a `pathlib.Path`. A glob is allowed on local and object-storage paths. A list or tuple gives several paths, read as one table (see below). |
| `src`, `dst` | The endpoint columns. |
| `storage_options` | Backend settings for object storage, for example `{"region": "us-east-1"}`. They layer over the backend's default credential chain. |
| `store` | A configured [`obstore`](https://developmentseed.org/obstore/) store, as an alternative to `storage_options`. |
| `on_null` | `"error"` or `"drop"`, as for `EdgeFrame`. |
| `delimiter` | The character between fields of a CSV file. |
| `has_header` | Whether the first line of a CSV file names the columns. With `False`, the columns are named `column_1`, `column_2`, and so on. |
| `quote` | The character that quotes a CSV field. |
| `escape` | Inside a quoted field, a character that makes the next character literal, such as `\`. In an unquoted field it is an ordinary character. By default there is none, and a quote inside a quoted field is written twice. |
| `null_value` | A text that reads as null in a CSV file, such as `"NA"`. An empty field reads as null too, with or without it. |
| `comment` | A character that marks a CSV line as a comment, such as `#`. A line that starts with it is skipped. By default there is none. |

The format comes from the file extension: `.parquet` or `.csv`. With the defaults, a CSV file is comma-separated, has a header row, quotes with `"`, and reads an empty field as null.

The CSV options apply to `.csv` files only. Each character option is one ASCII character other than a line break, and `delimiter`, `quote`, `escape` and `comment` must all differ. Passing an option to a scan with no `.csv` path raises `ValueError`. An unknown keyword raises `TypeError`, and names the option to use for another library's name for one (`sep` → `delimiter`, `header` → `has_header`, `na_values` → `null_value`).

A CSV file's column types are inferred from its first 1,000 rows. A value past them that does not parse as the inferred type raises an error at `collect()`. With a `null_value`, a value parses exactly as without one; only which fields are null differs.

A column of numbers is `double` when one or more of its values is a float. These spellings are floats:

- `1.5`, `.5`, `5.` and `1e3`, with or without a sign, such as `+5` or `-.5`.
- `1e400` and other values past the `double` range. They read as `inf` or `-inf`.
- `nan`, `inf` and `infinity` in any case, with or without a sign, such as `NaN`, `-Inf` or `+nan`.

Each float parses to the bits that pyarrow gives for it when it reads the value as a float. `-0` in a `double` column reads as `-0.0`, and `-nan` keeps its sign.

A text such as `nan` reads as NaN, not as null. pyarrow's default options read `nan`, `NaN` and `null` as null. To read `nan` as null, pass `null_value="nan"`. It matches that spelling only.

A column of integers only, such as `1` and `-0`, is `int64`. A column with other text in its first 1,000 rows is `string`. An integer past the `int64` range and a number with spaces around it are text.

```py
edges = ur.scan_edges("edges.csv", src="from", dst="to", delimiter="\t", null_value="NA")
```

A list of paths reads every path, each a file or a glob, as one table: the rows of the first path, then the second, in the order given, as if the files were read into memory one after another and concatenated. Every graph operation sees that table, a weighted algorithm included: its index and its weights come from the same rows, in the same order. The paths must have the same column names and types, in the same order (their nullability may differ), and be on one store; otherwise `collect()` names each column that differs and its type in each file. Each CSV path infers its own types: a column of `1`s in one file and `0.5`s in another is `int64` in one and `double` in the other, so write `1.0` for a float in every file.

A scanned edge frame has every column of the file, with the file's names, order and types, the same as an `EdgeFrame` built from the file read into memory. `collect()`, `filter`, `sort`, `group_by`, `join` and the sinks see all of them. A graph algorithm reads only the endpoints, plus the columns in a `weight=` expression. A `string_view` column is read as `string`.

An HTTP URL must point at one file and must not carry a query string. A presigned URL does not work over HTTP. Use the `s3://` backend with `storage_options` for signed access.

### `ur.scan_nodes`

```py
ur.scan_nodes(
    path: str | Path | list[str | Path], *,
    id: str,
    storage_options: dict | None = None,
    store=None,
    delimiter: str = ",",
    has_header: bool = True,
    quote: str = '"',
    escape: str | None = None,
    null_value: str | None = None,
    comment: str | None = None,
) -> NodeFrame
```

The same as `scan_edges`, for a node attribute table, with the same CSV options and lists of paths.

### `ur.read_edges`, `ur.read_nodes`

```py
ur.read_edges(path, *, src, dst, ...) -> MaterializedFrame
ur.read_nodes(path, *, id, ...) -> MaterializedFrame
```

Eager forms: `scan_edges(path, ...).collect()` and `scan_nodes(path, ...).collect()`, with the same arguments.

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
ur.from_networkx(graph, *, weight: str = "weight", symmetrize: bool | None = None) -> EdgeFrame
```

One row per edge. If every edge has the `weight` attribute, the frame has a `weight` column. Node ids are the NetworkX node labels. Node attributes are not included; use `nodes_from_networkx` for them.

An undirected `Graph` or `MultiGraph` gives each edge twice, once in each direction, so the default `direction="out"` of each kernel sees the undirected graph. A self-loop stays one row. For `betweenness` on such a frame, pass `direction="both"`; see [Semantics vs NetworkX](../semantics.md#undirected-graphs). `symmetrize=False` gives each undirected edge once, and `symmetrize=True` adds the reverse of every edge of a `DiGraph`.

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
