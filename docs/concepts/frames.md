# Frames

Ursa has two frame types. Both are lazy: a frame is a plan, and `collect()` runs the plan.

## EdgeFrame: the graph

An `EdgeFrame` is a table with two designated columns: the source (`src`) and the destination (`dst`). Each row is one directed edge. The frame is the graph. There is no other graph object.

```py
edges = ur.EdgeFrame(
    {"from": [0, 0, 1], "to": [1, 2, 2]},
    src="from",
    dst="to",
)
```

`src=` and `dst=` give the *roles* of two columns. They do not rename the columns. The original names stay in the data, and you can read them from `edges.src_col` and `edges.dst_col`.

An `EdgeFrame` can carry more columns. A weight, a timestamp, or a type column are ordinary columns. An algorithm reads one of them only when you ask for it, for example with `weight=ur.col("amount")`.

### The index

The first graph operation on an `EdgeFrame` builds a topology index (a CSR structure) and caches it on the frame. Later operations on the same frame reuse it. The index is built once per frame, not once per algorithm.

A `filter` on the edges makes a subgraph with its own index. See [Work on a subgraph](../guides/subgraphs.md).

### Rows are kept

Ursa does not merge parallel edges, and it does not remove self-loops. Two rows with the same `(src, dst)` are two edges. If you want a simple graph, remove the duplicate rows before you build the frame. See [Reshaped edge sets](../guides/subgraphs.md#reshaped-edge-sets).

## NodeFrame: the attributes

A `NodeFrame` is a table with one designated column: the id. It holds attributes of nodes. It has no topology.

```py
nodes = ur.NodeFrame(
    {"id": [0, 1, 2], "team": ["red", "red", "blue"]},
    id="id",
)
```

You get a `NodeFrame` in three ways:

- `edges.nodes()` gives the distinct ids in `src` and `dst`, with no attributes. Add at least one column before you collect it.
- The `NodeFrame(...)` constructor, `ur.scan_nodes(...)`, or an interop function gives a table you supply.
- A standalone algorithm call such as `ur.pagerank(edges)` acts as a `NodeFrame` of `(id, pagerank)`.

When you call `nodes.with_columns(pagerank=ur.pagerank(edges))`, Ursa joins the computed values to your table by id. The join is a left join: every row of your table stays.

## Lazy plans and `collect()`

A frame method returns a new frame with one more step in its plan. The plan is a value. You can hold it, extend it, or print it with `explain()`.

```py
plan = edges.nodes().with_columns(pr=ur.pagerank(edges)).sort("pr", descending=True)
print(plan.explain())
```

`collect()` runs the plan once and returns a `MaterializedFrame`. The result holds an Arrow table. From there, call `to_polars()`, `to_arrow()`, `to_dicts()`, `sink_parquet()`, or `sink_csv()`. See [Get the result out](../guides/output.md).

Shortcuts: `frame.to_polars()`, `frame.to_arrow()`, and `frame.to_dicts()` call `collect()` for you.

## Sources

A frame starts from one of two kinds of source:

- **In memory.** Row dicts, a column dict, a `polars.DataFrame`, a `pandas.DataFrame`, a `pyarrow.Table`, or any object with `__arrow_c_stream__`.
- **A file.** `ur.scan_edges(...)` and `ur.scan_nodes(...)` read Parquet and CSV, from a local path, from object storage, or from an HTTP URL. The read happens inside `collect()`.

A frame behaves the same way after construction, whatever its source. See [Load data](../guides/load-data.md).
