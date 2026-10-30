# Errors

## When an error is raised

A frame checks each step as you add it. A step that cannot run on the frame it is added to raises on that line, before anything runs: a column the frame does not have, two columns with one name, a comparison between types that do not compare, a role such as `ur.id()` on a frame without one, a join key missing from either side, or a combination of steps that does not run yet. The exception and its message are the ones `collect()` raised for them in earlier versions.

```py
nodes = ur.from_arrow({"id": [0, 1], "age": [31, 47]}, id="id")
try:
    nodes.filter(ur.col("agee") > 30)  # raises here, not at collect()
except ur.ColumnNotFoundError as exc:
    print(exc)  # filter() references unknown column 'agee'; the frame has ['id', 'age'].
```

Some errors depend on the rows, so `collect()` still raises them: a null endpoint, a value out of range for its type, an integer divide by zero, a seed that is not in the graph, a file that is missing or unreadable. A frame over `scan_edges` or `scan_nodes` is checked for its steps' shape when you build it, and for its columns when it runs, since its columns are the file's and the file can change in between.

`edges.nodes()` with no columns yet does not collect, but you can still build on it: the `with_columns` after it gives it rows. Collecting it as it is raises `NotImplementedError`.

## Engine errors

The engine raises one of three exception types. They are ordinary exceptions, so `except Exception` catches them.

| Type | Raised when |
|---|---|
| `ur.UrsaError` | The base class. Catch it to catch every engine error. |
| `ur.ColumnNotFoundError` | A step names a column that the frame does not have: a `filter`, `select`, `with_columns`, `rename`, `sort`, `group_by` or `agg`. The message has one shape, `sort() references unknown column 'x'; the frame has [...]`. A subclass of both `UrsaError` and `ValueError`. |
| `ur.ComputeError` | A computation fails: an invalid parameter, an unsupported operation, or a kernel error. A subclass of `UrsaError`. |

```py
try:
    result = nodes.with_columns(pr=ur.pagerank(edges)).collect()
except ur.ColumnNotFoundError as exc:
    print(exc)
```

The message names the column or the parameter, and says what is valid.

## Other exceptions

| Type | Raised when |
|---|---|
| `NotImplementedError` | The plan has a step, or a combination of steps, that does not run yet. Raised by the step that adds it. See [Limits](../limits.md). |
| `ValueError` | An argument is invalid: an unknown `on_null` value, an edge tuple of the wrong length, two aggregations with one name, or a `select` or `rename` that would give two columns one name. Raised by the step that takes it. A `ColumnNotFoundError` is a `ValueError` too. |
| `ModuleNotFoundError` | `to_polars()` is called without `polars` installed. The message names the `ursa-graph[polars]` extra. |
| `RuntimeError` | A dataset download fails. See [Datasets](datasets.md#the-download-cache). |

## Warnings

`on_null="drop"` emits a `UserWarning` with the number of rows it removed. Use the `warnings` module to silence it or to turn it into an error.
