# Errors

## Engine errors

The engine raises one of three exception types. They are ordinary exceptions, so `except Exception` catches them.

| Type | Raised when |
|---|---|
| `ur.UrsaError` | The base class. Catch it to catch every engine error. |
| `ur.ColumnNotFoundError` | A query names a column that the frame does not have. A subclass of `UrsaError`. |
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
| `NotImplementedError` | The plan has a step, or a combination of steps, that does not run yet. See [Limits](../limits.md). |
| `ValueError` | An argument is invalid before the plan runs: an unknown `on_null` value, an edge tuple of the wrong length, a filter on a column the frame does not have, or two aggregations with one name. |
| `ModuleNotFoundError` | `to_polars()` is called without `polars` installed. The message names the `ursa-graph[polars]` extra. |
| `RuntimeError` | A dataset download fails. See [Datasets](datasets.md#the-download-cache). |

## Warnings

`on_null="drop"` emits a `UserWarning` with the number of rows it removed. Use the `warnings` module to silence it or to turn it into an error.
