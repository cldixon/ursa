---
title: Weights
description: Weight is a per-operation expression over edge columns, evaluated to one f64 per edge — never a blessed column on the frame.
subtitle: weight=ur.col("amount") * ur.col("fx_rate") is a legitimate weight.
---

Most graph libraries bless a column — an edge attribute called `weight`, set once when the graph
is built. Ursa does not. Weight is a **parameter of each operation**, and it is an expression over
edge columns:

```python
ur.pagerank(edges, weight=ur.col("amount"))
ur.pagerank(edges, weight=ur.col("amount") * ur.col("fx_rate"))
```

That falls out of the same decision as `direction=`: properties of a *question* belong to the
operation asking it, not to the data. It also means two differently weighted answers can sit
side by side in one pipeline over one topology index.

```python
nodes.with_columns(
    pr_flat   = ur.pagerank(edges),
    pr_amount = ur.pagerank(edges, weight=ur.col("amount")),
)
```

## Where weights are supported

| Function | Weighted behaviour |
|---|---|
| `ur.pagerank` | rank is split by edge weight rather than uniformly across out-edges |
| `ur.shortest_path` | Dijkstra over the cost, instead of BFS over hops |
| `ur.closeness` | distances are summed costs |
| `ur.betweenness` | Dijkstra–Brandes |
| `ur.louvain` | weighted modularity |

Passing `weight=` to a verb that does not consume it — `label_propagation`, `triangle_count`,
`degree` — **raises**. It is not silently ignored, because a silently ignored weight is an
unweighted answer wearing a weighted label.

## How it evaluates

The weight expression is evaluated once, through the engine, to one `f64` per edge row. The
topology index keeps an `edge_ids` permutation mapping each CSR slot back to its original row, so
kernels gather `weight[edge_ids[k]]` on the fly.

The consequence worth knowing: topology lives in the index, properties stay in the original Arrow
columns, and nothing is copied twice. Adding a weight to an algorithm does not duplicate the edge
table.

A null weight value raises, on the same honour-or-raise principle as null endpoints.

## Requirements

A weighted algorithm needs an in-memory or `scan_edges`-backed source, so the weight columns can
actually be read. A frame whose provenance does not carry the edge attributes raises a clear error
rather than silently falling back to unweighted.

## Precision and ties

Continuous weights make exact-cost path ties vanish, which is usually what you want — with
float-valued costs, weighted shortest-path results agree with other libraries to floating-point
precision.

Where ties *are* possible, they are resolved deterministically: only *exactly* float-equal path
costs tie in weighted betweenness, and the tie-break is order-independent so the result does not
depend on how many threads ran.
