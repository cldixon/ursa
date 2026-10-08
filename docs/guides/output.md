# Get the result out

`collect()` returns a `MaterializedFrame`. It holds an Arrow table. From there the data goes to polars, to pyarrow, to Python values, or to a file. No step copies the data unless it must.

```python
import ursa as ur

edges, nodes = ur.datasets.load_karate(with_nodes=True)
result = nodes.with_columns(degree=ur.degree(edges, direction="both")).collect()
```

## In the REPL

`repr` shows the shape, the columns with their types, and the first 10 rows.

```python
print(result)
```

```text
shape: (34, 3)
┌───────┬────────┬────────┐
│ id    ┆ club   ┆ degree │
│ ---   ┆ ---    ┆ ---    │
│ int64 ┆ string ┆ uint32 │
╞═══════╪════════╪════════╡
│ 0     ┆ Mr. Hi ┆ 16     │
│ 1     ┆ Mr. Hi ┆ 9      │
│ 2     ┆ Mr. Hi ┆ 10     │
│ 3     ┆ Mr. Hi ┆ 6      │
│ 4     ┆ Mr. Hi ┆ 3      │
│ 5     ┆ Mr. Hi ┆ 4      │
│ 6     ┆ Mr. Hi ┆ 4      │
│ 7     ┆ Mr. Hi ┆ 4      │
│ 8     ┆ Mr. Hi ┆ 5      │
│ 10    ┆ Mr. Hi ┆ 3      │
│ …     ┆ …      ┆ …      │
└───────┴────────┴────────┘
```

## To Arrow

```python
table = result.to_arrow()
print(type(table).__name__, table.schema.names, table.num_rows)
```

```text
Table ['id', 'club', 'degree'] 34
```

## To polars

With the `polars` extra installed:

```python
df = result.to_polars()
print(df.filter(df["degree"] > 10))
```

```text
shape: (3, 3)
┌─────┬─────────┬────────┐
│ id  ┆ club    ┆ degree │
│ --- ┆ ---     ┆ ---    │
│ i64 ┆ str     ┆ u32    │
╞═════╪═════════╪════════╡
│ 0   ┆ Mr. Hi  ┆ 16     │
│ 32  ┆ Officer ┆ 12     │
│ 33  ┆ Officer ┆ 17     │
└─────┴─────────┴────────┘
```

## To Python values

```python
rows = result.to_dicts()
print(rows[0])
print(result.columns)
```

```text
{'id': 0, 'club': 'Mr. Hi', 'degree': 16}
['id', 'club', 'degree']
```

## To a file

`sink_parquet` and `sink_csv` write the result. Keyword arguments to `sink_parquet` go to `pyarrow.parquet.write_table`.

```python
result.sink_parquet("karate_degree.parquet", compression="zstd")
result.sink_csv("karate_degree.csv")

back = ur.read_nodes("karate_degree.parquet", id="id")
print(back.columns, len(back.to_dicts()))
```

```text
['id', 'club', 'degree'] 34
```

## Shortcuts on a lazy frame

A lazy frame has the same methods. Each one calls `collect()` for you.

```python
rows = nodes.with_columns(degree=ur.degree(edges)).head(2).to_dicts()
print(rows)
```

```text
[{'id': 0, 'club': 'Mr. Hi', 'degree': 16}, {'id': 1, 'club': 'Mr. Hi', 'degree': 8}]
```
