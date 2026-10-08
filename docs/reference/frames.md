# Frames

## `ur.EdgeFrame`

```py
ur.EdgeFrame(data, *, src: str, dst: str, on_null: str = "error") -> EdgeFrame
```

The graph: a table in which each row is one directed edge.

| Parameter | Description |
|---|---|
| `data` | Row dicts, a column dict, a `polars.DataFrame`, a `pandas.DataFrame`, a `pyarrow.Table`, a `pyarrow.RecordBatch`, or any object with `__arrow_c_stream__`. |
| `src` | The name of the source column. |
| `dst` | The name of the destination column. |
| `on_null` | `"error"` raises if `src` or `dst` is null in any row. `"drop"` removes those rows and logs a warning with the count. |

The `src` and `dst` columns must have the same type: `int64` or `string`.

### Properties

| Property | Description |
|---|---|
| `edges.src_col` | The name of the source column. |
| `edges.dst_col` | The name of the destination column. |

### Methods

| Method | Returns | Description |
|---|---|---|
| `edges.nodes()` | `NodeFrame` | The distinct ids in `src` and `dst`. Lazy. |
| `edges.reverse()` | `EdgeFrame` | The same rows with the `src` and `dst` roles swapped. No data moves. |
| `edges.collect()` | `MaterializedFrame` | The edge rows, after any `filter`, `sort`, `head`, `distinct`, `sample`, or `rename`. |

`EdgeFrame` also has every [shared method](#shared-methods).

## `ur.NodeFrame`

```py
ur.NodeFrame(data, *, id: str) -> NodeFrame
```

A table of node attributes, with one designated id column.

| Parameter | Description |
|---|---|
| `data` | The same input kinds as `EdgeFrame`. |
| `id` | The name of the id column. |

### Properties

| Property | Description |
|---|---|
| `nodes.id_col` | The name of the id column. |

`NodeFrame` has every [shared method](#shared-methods).

## Shared methods

Both frame types have these methods. Each lazy method returns a new frame with one more step in its plan.

### Columns

| Method | Description |
|---|---|
| `.with_columns(**exprs)` | Adds one column per keyword. Each value is an [expression](expressions.md) or an algorithm. Existing columns stay. |
| `.select(*columns)` | Keeps only the named columns, in the given order. Each item is a name or `ur.col(name)`. Call it after `with_columns`. |
| `.rename({old: new, ...})` | Renames columns. |

### Rows

| Method | Description |
|---|---|
| `.filter(predicate)` | Keeps the rows where the [expression](expressions.md) is true. On an `EdgeFrame` before `nodes()`, the filter selects a subgraph. |
| `.sort(by, *, descending=False)` | Sorts by one column name. |
| `.head(n=10)` | Keeps the first `n` rows. `.limit(n)` is the same method. |
| `.distinct()` | Removes duplicate rows. |
| `.sample(n, *, seed=None)` | Keeps `n` random rows. `seed` makes the choice reproducible. |

### Groups and joins

| Method | Description |
|---|---|
| `.group_by(*keys).agg(*exprs, **named)` | Groups by one or more columns and computes [aggregations](expressions.md#aggregations). The output columns are the keys, then the aggregations. |
| `.join(other, *, on, how="inner")` | An equi-join with another frame on the shared key column or columns `on`. `how` is `"inner"` or `"left"`. |

### Inspection

| Method | Description |
|---|---|
| `.explain()` | The plan as text. Runs nothing. |
| `.schema()` | Not available yet. Raises `NotImplementedError`. |

### Execution

| Method | Description |
|---|---|
| `.collect()` | Runs the plan. Returns a `MaterializedFrame`. |
| `.to_polars()` | `collect()`, then a `polars.DataFrame`. Needs the `polars` extra. |
| `.to_arrow()` | `collect()`, then a `pyarrow.Table`. |
| `.to_dicts()` | `collect()`, then a list of row dicts. |
| `.sink_parquet(path, **opts)` | `collect()`, then write a Parquet file. `opts` go to `pyarrow.parquet.write_table`. |
| `.sink_csv(path)` | `collect()`, then write a CSV file. |

Some combinations of steps do not run yet. See [Limits](../limits.md).

## `ur.MaterializedFrame`

The result of `collect()`. It holds a chunked `pyarrow.Table`. You do not construct it yourself.

| Member | Description |
|---|---|
| `.columns` | The column names, as a list. |
| `.to_arrow()` | The `pyarrow.Table`. No copy. |
| `.to_polars()` | A `polars.DataFrame`. No copy. Needs the `polars` extra. |
| `.to_dicts()` | A list of row dicts. |
| `.sink_parquet(path, **opts)` | Writes a Parquet file. |
| `.sink_csv(path)` | Writes a CSV file. |
| `repr(result)` | A table preview: the shape, the column names, the types, and the first 10 rows. |
