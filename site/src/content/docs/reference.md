---
title: API reference
description: The catalog — every public symbol in ursa, its signature, and what it returns.
subtitle: Numbered records, one per object. Where a parameter is accepted but not yet wired, it says so.
---

Import convention throughout: `import ursa as ur`.

Everything in `ursa.__all__` appears below. Where something is modelled in the plan but does not
execute yet, it raises a clear error when collected rather than being silently dropped — those
cases are marked **not wired**.

## Constructors

The frame types are public, data-first constructors — no `pyarrow` import required:

```python
ur.EdgeFrame(data, *, src, dst, on_null="error") -> EdgeFrame
ur.NodeFrame(data, *, id)                        -> NodeFrame
```

`data` may be a list of row dicts, a dict of equal-length columns, a `polars.DataFrame`, a
`pandas.DataFrame`, a `pyarrow.Table` or `RecordBatch`, or any object implementing
`__arrow_c_stream__`. polars and pandas are detected, never imported by Ursa.

```python
edges = ur.EdgeFrame({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}, src="s", dst="d")
nodes = ur.NodeFrame([{"id": 0, "team": "red"}, {"id": 1, "team": "blue"}], id="id")
```

However a frame is built — constructor, typed alias, or scan — it behaves identically from
there on.

## IO

### Scans

```python
ur.scan_edges(path, *, src, dst, storage_options=None, store=None,
              on_null="error", **format_opts) -> EdgeFrame
ur.scan_nodes(path, *, id, storage_options=None, store=None, **format_opts) -> NodeFrame
ur.read_edges(path, **kwargs) -> MaterializedFrame
ur.read_nodes(path, **kwargs) -> MaterializedFrame
```

| Parameter | Notes |
|---|---|
| `path` | one path or glob. Parquet and CSV. Local, `s3://`, `gs://`, `az://`, `file://`, or `http(s)://` (single hosted file, no glob; query strings are dropped, so presigned URLs won't authenticate). A *list* of paths is accepted by the signature but raises at collect |
| `src` / `dst` / `id` | role mappings, not renames — original column names are preserved |
| `storage_options` | dict, layered over the backend's default credential chain |
| `on_null` | `"error"` (default) raises on a null `src`/`dst`; `"drop"` filters those rows and reports the dropped count as a warning. Edge inputs only |
| `store` | reserved for a pre-configured obstore store — **not wired**, raises at collect |
| `**format_opts` | reserved (e.g. CSV `delimiter=`) — **not wired**, raises at construction |

`read_*` is `scan_*` + `collect()`. See [Reading data](/docs/guides/scans).

### Typed aliases and interop

All zero-copy where the source is Arrow-backed; `on_null` is accepted wherever edges come in.

```python
ur.from_polars(df, *, src, dst) / ur.from_polars(df, *, id)
ur.from_pandas(df, *, src, dst) / ur.from_pandas(df, *, id)
ur.from_arrow(tbl, *, src, dst) / ur.from_arrow(tbl, *, id)

ur.from_edgelist(edges, *, weighted=None, on_null="error")   # 2- or 3-tuples
ur.from_networkx(graph, *, weight="weight")                   # -> EdgeFrame
ur.nodes_from_networkx(graph, *, id="id")                     # -> NodeFrame of attributes
ur.from_numpy(array, *, kind="auto", weighted=False)          # adjacency matrix or edge array
ur.from_scipy_sparse(matrix, *, weighted=False)               # sparse adjacency
```

networkx, numpy and scipy are imported lazily inside each function — none is a dependency.

### Datasets

`ur.datasets` ships small canonical graphs for examples and tests. Bundled sets load offline
from the wheel; `load_facebook` downloads once and caches under `$URSA_DATA_HOME`
(default `~/.cache/ursa/datasets`).

```python
ur.datasets.list_datasets()          # -> list[DatasetInfo]
ur.datasets.load(name, **kwargs)     # dispatch by name

ur.datasets.load_karate(with_nodes=False)  # 34 n / 78 e; with_nodes adds a `club` label table
ur.datasets.load_lesmis()                  # 77 / 254, weighted
ur.datasets.load_florentine()              # 15 / 20
ur.datasets.load_kite()                    # Krackhardt kite, 10 / 18
ur.datasets.load_facebook()                # SNAP ego-Facebook, 4 039 / 88 234 (downloaded)
```

Every loader returns an `EdgeFrame`.

## Frame methods

Available on both `EdgeFrame` and `NodeFrame`. The relational surface executes; `schema()` is
the one remaining plan-only verb.

| Method | Notes |
|---|---|
| `filter(predicate)` | full predicate algebra — see [Expressions](#expressions) |
| `select(*columns)` | bare names or `ur.col(name)`; one `select` per pipeline, after any `with_columns` |
| `with_columns(**exprs)` | one `with_columns` per pipeline |
| `sort(by, *, descending=False)` | a single column name |
| `head(n=10)` / `limit(n)` | |
| `distinct()` | row order of the result is unspecified |
| `sample(n, *, seed=None)` | seeded and reproducible; `n ≥ rows` returns everything |
| `rename(mapping)` | unknown source column raises `ColumnNotFoundError` |
| `group_by(*keys).agg(...)` | see below |
| `join(other, *, on, how="inner")` | see below |
| `collect()` | returns a `MaterializedFrame` |
| `explain()` | the plan as text, including index state |
| `schema()` | **not wired** |
| `to_polars()` / `to_arrow()` / `to_dicts()` | egress, via `collect()` |
| `sink_parquet(path, **opts)` | `opts` pass through to pyarrow |
| `sink_csv(path)` | takes no options, deliberately — none would be honoured |

**`join`** — `how="inner"` or `"left"` (`right`/`outer` raise). `on=` is required: a column name,
`ur.col(name)`, or a list of either; key inference is not attempted. A non-key column present on
both sides raises rather than being silently suffixed.

**`group_by().agg`** — keys are names or `ur.col(name)`. Aggregations are
`ur.col(c).<fn>()` with `fn` in `mean`, `sum`, `min`, `max`, `count`, `n_unique`, optionally
`.alias(x)` or passed as `name=...`; the output name falls back to `{column}_{fn}`. Aggregating a
computed expression raises.

**Composition limits, stated:** one `join`, one `group_by().agg`, one `with_columns` and one
`select` per pipeline; `filter` after `group_by().agg` (SQL `HAVING`) is not supported yet —
filter before grouping; `group_by` composes with `sort`/`head`/`rename` on the grouped result but
not `distinct`/`sample`/`select`; `join` and `group_by` do not compose with each other or with
traversal results yet. Each limit raises a clear error naming itself.

### EdgeFrame-specific

```python
edges.nodes()      -> NodeFrame    # the distinct union of src and dst
edges.reverse()    -> EdgeFrame    # swap the src/dst roles; metadata-only
edges.src_col      -> str          # role introspection
edges.dst_col      -> str
```

## Expressions

```python
ur.col(name)     # a column reference
ur.lit(value)    # a literal
ur.src()         # the frame's source role column
ur.dst()         # the frame's destination role column
ur.id()          # the frame's id role column
```

Predicates lower as a full expression algebra: comparisons (`> >= < <= == !=`), boolean
combinations (`&`, `|`, `~`), arithmetic (nested inside comparisons), `col <op> col`, and
string/bool equality. The top of a filter must be a boolean predicate — a bare column or bare
arithmetic expression raises.

```python
edges.nodes().with_columns(pr=ur.pagerank(edges), indeg=ur.degree(edges, direction="in")) \
     .filter((ur.col("pr") * 100 > ur.col("indeg")) & ~(ur.col("indeg") == 0))
```

`ur.src()` / `ur.dst()` / `ur.id()` resolve against whatever frame the expression runs in — the
role columns of an edge frame, `id` in a node query, the literal `src`/`dst` of a traversal
result — and raise naming the available roles when the context has none.

There are no `.str` / `.dt` namespaces yet. The six aggregation methods (`mean`, `sum`, `min`,
`max`, `count`, `n_unique`) and `.alias()` complete the expression surface.

## Graph verbs

```python
ur.degree(edges, direction="out") -> GraphExpr
```
Degree per node. `direction` is `"out"`, `"in"` or `"both"`.

```python
ur.neighbors(edges, direction="out", from_=None).agg(expr) -> GraphExpr
```
Neighbour aggregation. `expr` is `ur.col(<name>).<fn>()` with `fn` in `mean`, `sum`, `min`, `max`,
`count`, `n_unique`; `mean`/`sum`/`min`/`max` need a numeric column, `count`/`n_unique` also accept
strings. `from_=` is **not wired** and raises.

```python
ur.hop(edges, n=1, direction="out").from_(seeds) -> EdgeFrame
```
k-hop reachability. `src` is the seed, `dst` the reached node. `seeds` is a list of ids or a
`NodeFrame`. Keeps duplicates — call `.distinct()` for the reachable set.

```python
ur.shortest_path(edges, source, target, weight=None, direction="out") -> EdgeFrame
```
Single-pair shortest path, returned as `(src, dst, hop, cost)`: one row per edge on the path, in
order, `hop` the zero-based position. `cost` is always present — the cumulative edge cost when
`weight=` is given (the last row carries the total), and the hop count for the unweighted BFS
case, so the schema is uniform. An unreachable target returns an empty frame.

```python
ur.random_walk(edges, start, steps, walks_per_node=1, seed=None) -> NodeFrame
```
Random walks, returned as `(walk_id, step, node)`. Walks draw from a single seeded stream, so a
given `seed` reproduces the exact walks regardless of machine or thread count; `seed=None` uses a
fixed default and is also reproducible. A walk stops early at a node with no out-neighbour.

## Algorithms

Each is **dual-positioned**: used inside `with_columns(...)` it reads as an expression; called
bare it behaves as a lazy `NodeFrame` of `(id, value)`, where the value column takes the
algorithm's name. Both spellings are the same kernel.

The **float-valued** kernels — `pagerank`, `closeness`, `betweenness`, `clustering_coefficient`,
and `neighbors().agg(...)` — take `dtype="f32"` to emit the value column as 32-bit float instead
of 64-bit. The kernel still accumulates in `f64`; only the emitted column narrows, halving its
wire and on-disk size (`sink_parquet` a precomputed metric at half the bytes). The integer-valued
kernels (`degree`, `connected_components`, `triangle_count`, `label_propagation`, `louvain`) have
no `dtype` — their `u32` output has no lossy narrowing.

```python
ur.pagerank(edges, damping=0.85, max_iter=30, tol=1e-6, weight=None, dtype="f64")
```
Pull-based fixpoint PageRank. Dangling mass is redistributed uniformly, matching `nx.pagerank`.

```python
ur.connected_components(edges, mode="weak")
```
`"weak"` is union-find over the undirected view; `"strong"` is Tarjan SCC over the directed
graph. Labels are arbitrary-but-stable ids — compare partitions, not label values.

```python
ur.triangle_count(edges)
```
Per-node triangle count, computed on the **undirected view**.

```python
ur.clustering_coefficient(edges)
```
Local clustering coefficient, derived from triangles.

```python
ur.betweenness(edges, sample=None, weight=None, seed=None)
```
Brandes betweenness, **directed and unnormalized**. `sample=` approximates from a `seed`-shuffled
subset of sources (Brandes–Pich, scaled by `n/k`); exact is `O(n·m)`, and the signature says so.
Parallel edges count as distinct shortest paths.

```python
ur.closeness(edges, weight=None)
```
Out-edge closeness, Wasserman–Faust off.

```python
ur.label_propagation(edges, max_iter=20, seed=None)
```
Community detection by label propagation.

```python
ur.louvain(edges, weight=None, resolution=1.0, seed=None)
```
Community detection by modularity optimization. Labels are arbitrary ids; the partition is chosen
by modularity score.

Every stochastic algorithm takes `seed=`, and the guarantee is that the same seed gives the same
result **independent of thread count** — see [Semantics](/docs/semantics).

## Graph-level statistics

```python
ur.describe(edges, full=False)             -> NodeFrame   # lazy one-row summary
ur.density(edges)                          -> float       # eager
ur.avg_path_length(edges, sample=None)     -> float       # eager
ur.diameter(edges, approximate=True)       -> int         # eager
```

`describe` gates the expensive `n_components` behind `full=True`, and must be the final step of a
pipeline — a filter/sort/head tail after it raises. The three scalars are the one deliberate
exception to laziness. See [Whole-graph statistics](/docs/guides/statistics).

## Types

| Symbol | What it is |
|---|---|
| `ur.EdgeFrame` | a lazy frame with `src`/`dst` roles and a cached topology index |
| `ur.NodeFrame` | a lazy frame with an `id` role |
| `ur.MaterializedFrame` | what `collect()` returns; a chunked Arrow table |
| `ur.Expr` | an expression-tree node |
| `ur.datasets.DatasetInfo` | the record `list_datasets()` returns |

### MaterializedFrame

What `collect()` returns. Egress methods are listed under [Frame methods](#frame-methods); these
are the ones that only exist on a materialized result:

| Member | What it does |
|---|---|
| `len(result)` | row count |
| `result.columns` | column names, in order |
| `repr(result)` | a shape line, the column names and dtypes, and up to 10 rows; the rest elide behind `…` |

`repr` reads the head only, so previewing a large result costs the same as previewing a small one,
and it needs no optional dependencies.

## Errors

```python
ur.UrsaError             # base class for every error raised from native execution
ur.ColumnNotFoundError   # a query referenced a column that does not exist
ur.ComputeError          # a graph computation failed
```

These exist whether or not the compiled extension is present, so `except ursa.UrsaError` is always
valid. A `NotImplementedError` — as opposed to one of these — means the surface is designed but
not yet wired, or a composition limit was hit; the message names which.

## Module attributes

```python
ur.__version__        # the installed distribution version
ur.__core_version__   # the native core version, or None without the extension
```
