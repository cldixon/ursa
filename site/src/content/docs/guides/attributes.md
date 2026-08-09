---
title: Attributes & neighbours
description: Joining node attributes to computed metrics, and pulling attributes across the topology with neighbors().agg().
subtitle: Attributes resolve against the ambient frame; topology comes from the threaded EdgeFrame.
---

## Enrichment

A `NodeFrame` is an attribute table. Computed metrics attach to it with `with_columns`, joined by
id:

```python
edges = ur.scan_edges("links.parquet", src="tower_a", dst="tower_b")
nodes = ur.scan_nodes("towers.parquet", id="tower_id")

enriched = (
    nodes
    .with_columns(
        pagerank  = ur.pagerank(edges, damping=0.85),
        component = ur.connected_components(edges),
        deg       = ur.degree(edges, direction="both"),
    )
    .filter(ur.col("region") == 2)          # an attribute column
    .sort("pagerank", descending=True)      # a computed column
    .collect()
)
```

The join is a **LEFT join from the node table**, so every attribute row survives even if it has no
edges. Attribute columns and computed columns are interchangeable in the tail: by the time the
filter and sort run, they are all just columns.

`with_columns` stays additive; `select(...)` narrows the output Polars-faithfully — and because
the projection propagates back into the scan, narrowing the output narrows what is read from the
node file.

```python
enriched.select("tower_id", ur.col("pagerank"))   # reads fewer columns from Parquet
```

## Neighbour aggregation

`ur.neighbors(edges).agg(expr)` pulls attributes across the topology: for each node, aggregate an
attribute over its neighbours.

```python
nodes.with_columns(
    nbr_avg_capacity = ur.neighbors(edges).agg(ur.col("capacity_gbps").mean()),
    nbr_regions      = ur.neighbors(edges).agg(ur.col("region").n_unique()),
    nbr_in_seniority = ur.neighbors(edges, direction="in").agg(ur.col("seniority").mean()),
)
```

**The attribute resolution rule:** topology comes from the threaded EdgeFrame; attribute columns
resolve against the ambient frame the expression runs in — here `nodes`, since the neighbours are
rows of that same frame.

Supported aggregations today:

| Function | Accepts |
|---|---|
| `.mean()` | numeric attribute columns |
| `.sum()` | numeric |
| `.min()` | numeric |
| `.max()` | numeric |
| `.count()` | numeric or string |
| `.n_unique()` | numeric or string |

`direction=` takes `"out"` (default), `"in"` or `"both"`, exactly as elsewhere.

The `from_=` override — resolving the aggregation against a *different* node frame — is part of
the designed surface but is not wired yet, and raises rather than being silently ignored.

## Why the EdgeFrame is an argument

Because topology is threaded explicitly rather than bound to an object, "there is no graph, only
frames" composes in principle: degree in the full graph beside degree in a subgraph, each
threading its own edge frame.

Two current limits shape how you write that today, and both raise clearly rather than
mis-executing:

- Within a single `with_columns`, every graph algorithm must run over the **same** edge frame.
  Compute over different graphs in separate steps or separate collects.
- A graph op over a *filtered* edge frame raises — subgraph views over a parent index are
  deferred. Materialize the filtered edges and construct a new frame:

```python
active = ur.EdgeFrame(
    edges.filter(ur.col("last_seen") > cutoff).collect().to_arrow(),
    src=edges.src_col, dst=edges.dst_col,
)

nodes.with_columns(deg_all=ur.degree(edges, direction="both")).collect()
ur.degree(active, direction="both").collect()   # the subgraph's answer, separately
```

## Multiplicity

Duplicate `(src, dst)` rows are parallel edges, and every kernel sees them. If your source data
has repeated edges and you want simple-graph semantics, deduplicate at the source — a
materialize-and-reconstruct, since `distinct()` is row-changing and the derived frame refuses
graph ops:

```python
simple = ur.EdgeFrame(edges.distinct().collect().to_arrow(),
                      src=edges.src_col, dst=edges.dst_col)
```
