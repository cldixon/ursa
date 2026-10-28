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

The steps run in the order you write them, as in Polars. Each one sees the rows and column names the steps before it leave, so a step after `rename` uses the new names and a step after `select` sees only the selected columns.

Without a `sort`, rows come back in input order, the same on every run and every machine. On a plain frame that is the row order. After a graph algorithm it is the attribute table's row order, or node order for an `edges.nodes()` frame; after a traversal it is the traversal's own output order. A `join` returns the left rows in order, each followed by its matches in the right frame's order. `group_by().agg()` and `distinct()` return each group or distinct row at the position of its first input row, and `sample` keeps the picked rows in input order. `head(n)` keeps the first `n` rows of that order.

### Columns

| Method | Description |
|---|---|
| `.with_columns(**exprs)` | Adds one column per keyword. Each value is an [expression](expressions.md) or an algorithm, computed over the rows the steps before it leave. A keyword that names an existing column replaces it in place; other columns stay. An algorithm's column attaches to the rows by the node id column. |
| `.select(*columns)` | Keeps only the given columns, in the given order. Each item is a name, `ur.col(name)`, or an [expression](expressions.md), named by its `.alias()` or else by its first column. |
| `.rename({old: new, ...})` | Renames columns. |

### Rows

| Method | Description |
|---|---|
| `.filter(predicate)` | Keeps the rows where the [expression](expressions.md) is true. On an `EdgeFrame` before `nodes()`, the filter selects a subgraph. |
| `.sort(by, *more_by, descending=False)` | Sorts by one or more columns or [expressions](expressions.md). `by` is a column name, an expression, or a list of them, and `more_by` adds more. Later columns break ties in earlier ones. `descending` is one bool for every column, or a list with one bool per column. Nulls sort last for every column. A float column sorts NaN above every number, whatever its sign bit, and `-0.0` equal to `0.0` (see [NaN in comparisons](../semantics.md#nan-in-comparisons-and-sorts)). The sort is stable: rows that tie on every column keep their input order, on every run. After a graph algorithm that is node order, or the attribute table's row order when the frame has one; after a traversal it is the traversal's own output order. |
| `.head(n=10)` | Keeps the first `n` rows. `.limit(n)` is the same method. |
| `.distinct()` | Removes duplicate rows. |
| `.sample(n, *, seed=None)` | Keeps `n` random rows. `seed` makes the choice reproducible. |

### Groups and joins

| Method | Description |
|---|---|
| `.group_by(*keys).agg(*exprs, **named)` | Groups by one or more columns and computes [aggregations](expressions.md#aggregations). The output columns are the keys, then the aggregations. |
| `.join(other, *, on, how="inner")` | An equi-join with another frame on the shared key column or columns `on`. `how` is `"inner"` or `"left"`. Composes with `group_by().agg()` in either order, which runs as written. |

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
