---
title: API reference
description: The catalog — every public symbol in ursa, its signature, and what it returns.
subtitle: Numbered records, one per object. Where a parameter is accepted but not yet wired, it says so.
---

Import convention throughout: `import ursa as ur`.

Everything in `ursa.__all__` appears below. Where something is modelled in the plan but does not
execute yet, it raises a clear error when collected rather than being silently dropped — those
cases are marked **not wired**.

## IO

```python
ur.scan_edges(path, *, src, dst, storage_options=None, store=None, **format_opts) -> EdgeFrame
ur.scan_nodes(path, *, id,        storage_options=None, store=None, **format_opts) -> NodeFrame
ur.read_edges(path, **kwargs) -> MaterializedFrame
ur.read_nodes(path, **kwargs) -> MaterializedFrame
ur.from_polars(df, *, src, dst) -> EdgeFrame
ur.from_polars(df, *, id)       -> NodeFrame
ur.from_arrow(tbl, *, src, dst) -> EdgeFrame
ur.from_arrow(tbl, *, id)       -> NodeFrame
```

| Parameter | Notes |
|---|---|
| `path` | a path, a glob, or a list of paths. Parquet and CSV. Local, `s3://`, `gs://`, `az://`, `file://` |
| `src` / `dst` / `id` | role mappings, not renames — original column names are preserved |
| `storage_options` | dict, layered over the backend's default credential chain |
| `store` | reserved for a pre-configured obstore store — **not wired**, raises |
| `**format_opts` | reserved (e.g. CSV `delimiter=`) — **not wired**, raises |

`read_*` is `scan_*` + `collect()`. `from_*` is zero-copy. See [Reading data](/docs/guides/scans).

## Frame methods

Available on both `EdgeFrame` and `NodeFrame`.

| Method | Status |
|---|---|
| `filter(predicate)` | executes; predicates lower as `col <op> literal` |
| `select(*columns)` | executes; one `select` per pipeline, and it must follow any `with_columns` |
| `with_columns(**exprs)` | executes; one `with_columns` per pipeline |
| `sort(by, *, descending=False)` | executes |
| `head(n=10)` / `limit(n)` | executes |
| `distinct()` | executes on node pipelines and hops |
| `collect()` | executes — returns a `MaterializedFrame` |
| `explain()` | executes — returns the plan as text, including index state |
| `rename(mapping)` | **not wired** |
| `sample(n, *, seed=None)` | **not wired** |
| `group_by(*keys).agg(...)` | **not wired** |
| `join(other, *, on=None, how="inner")` | **not wired** |
| `schema()` | **not wired** |
| `to_polars()` / `to_arrow()` / `to_dicts()` | egress, via `collect()` |
| `sink_parquet(path, **opts)` / `sink_csv(path)` | egress, via `collect()` |

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

Arithmetic, comparison and boolean operators build an expression tree: `(ur.col("a") * ur.lit(2) >
ur.lit(5)) & (ur.col("b") >= ur.lit(1))`. The tree is pure Python and always available, with or
without the compiled extension.

The *executable* filter surface is narrower than the tree you can build: predicates currently
lower as `col <op> literal`. `col <op> col`, boolean combinations and arithmetic inside a
predicate raise rather than mis-executing. `ur.src()` / `ur.dst()` / `ur.id()` are likewise part
of the designed dialect ahead of the lowering.

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
Single-pair shortest path, returned as `(src, dst, hop)`: one row per edge on the path, in order.
Unweighted is BFS; `weight=` selects minimum cost (Dijkstra). The accumulated path cost is not yet
returned as a column.

```python
ur.random_walk(edges, start, steps, walks_per_node=1, seed=None) -> NodeFrame
```
Random walks, returned as `(walk_id, step, node)`. `seed=` gives reproducibility that holds across
thread counts.

## Algorithms

Each is **dual-positioned**: used inside `with_columns(...)` it reads as an expression; called
bare it behaves as a lazy `NodeFrame` of `(id, value)`, where the value column takes the
algorithm's name. Both spellings are the same kernel.

```python
ur.pagerank(edges, damping=0.85, max_iter=30, tol=1e-6, weight=None)
```
Pull-based fixpoint PageRank.

```python
ur.degree(edges, direction="out")
```
See above.

```python
ur.connected_components(edges, mode="weak")
```
Union-find weak components. `mode="strong"` raises — SCC is a later release.

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
subset of sources; exact is `O(n·m)`, and the signature says so. Parallel edges count as distinct
shortest paths.

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

`describe` gates the expensive `n_components` behind `full=True`. The three scalars are the one
deliberate exception to laziness. See [Whole-graph statistics](/docs/guides/statistics).

## Types

| Symbol | What it is |
|---|---|
| `ur.EdgeFrame` | a lazy frame with `src`/`dst` roles and a cached topology index |
| `ur.NodeFrame` | a lazy frame with an `id` role |
| `ur.MaterializedFrame` | what `collect()` returns; a chunked Arrow table |
| `ur.Expr` | an expression-tree node |

## Errors

```python
ur.UrsaError             # base class for every error raised from native execution
ur.ColumnNotFoundError   # a query referenced a column that does not exist
ur.ComputeError          # a graph computation failed
```

These exist whether or not the compiled extension is present, so `except ursa.UrsaError` is always
valid. A `NotImplementedError` — as opposed to one of these — means the surface is designed but
not yet wired.

## Module attributes

```python
ur.__version__        # the installed distribution version
ur.__core_version__   # the native core version, or None without the extension
```
