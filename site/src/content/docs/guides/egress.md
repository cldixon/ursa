---
title: Getting results out
description: collect, to_polars, to_arrow, to_dicts, sink_parquet and sink_csv — Arrow is the boundary, so leaving Ursa costs nothing.
subtitle: Arrow at the boundaries. Egress is zero-copy in both directions.
---

`.collect()` runs the plan and returns a `MaterializedFrame` — a chunked Arrow table. The engine
hands back a *list* of record batches and they are never concatenated into one contiguous batch
just to make the result look tidy, so materializing does not double your peak memory.

Everything below is a method on that materialized frame.

## To the ecosystem

```python
result = enriched.filter(ur.col("component") == 0).collect()

df   = result.to_polars()    # polars.DataFrame, zero-copy via Arrow
tbl  = result.to_arrow()     # pyarrow.Table
rows = result.to_dicts()     # list[dict], for small results and API responses
```

A materialized frame previews itself, so a bare `collect()` in a REPL shows the head rather than a
summary line. `len(result)` is the row count and `result.columns` the column names — neither needs
polars:

```python
>>> edges.nodes().with_columns(pr=ur.pagerank(edges)).head(2).collect()
shape: (2, 2)
┌─────┬──────────┐
│ id  ┆ pr       │
│ --- ┆ ---      │
│ i64 ┆ f64      │
╞═════╪══════════╡
│ 0   ┆ 0.477977 │
│ 1   ┆ 0.447023 │
└─────┴──────────┘
```

`to_arrow()` is the general exit. Anything that speaks Arrow takes it directly — for example
appending to an Iceberg table:

```python
table.append(result.to_arrow())
```

`polars` is an optional dependency, touched only by the interop shims. Ursa does not depend on the
Polars Rust crates; the contract between them is Arrow, which is why the handoff is free.

## To files

```python
result.sink_parquet("metrics.parquet")
result.sink_csv("metrics.csv")
```

`sink_parquet` passes keyword options through to pyarrow, so compression and row-group settings
are available:

```python
result.sink_parquet("metrics.parquet", compression="zstd")
```

## In a service

Because results are frames and frames are cheap to slice, a request handler is just the tail of a
pipeline:

```python
@app.get("/towers/critical")
def critical():
    return (
        result_frame
        .sort("betweenness", descending=True)
        .head(100)
        .collect()
        .to_dicts()
    )
```

`collect()` releases the GIL for the duration of execution, so Ursa behaves inside a threaded
Python server rather than blocking the interpreter while a kernel runs.

## Round-tripping

Ingress mirrors egress exactly, so a result can go back in as a graph:

```python
paths = ur.shortest_path(edges, 0, 5).collect().to_arrow()
sub   = ur.from_arrow(paths, src="src", dst="dst")   # the path, as a graph
```

Role mappings are preserved through a scan, so the original column names survive a round trip
through Parquet and back.

## Inspecting instead of running

```python
frame.explain()
```

`explain()` prints the plan and, usefully, whether the topology index is preserved or will be
rebuilt — which is the part of the performance model you most often want to check before running
something expensive.
