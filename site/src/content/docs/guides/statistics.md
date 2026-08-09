---
title: Whole-graph statistics
description: describe returns a lazy one-row summary frame; density, avg_path_length and diameter are the one deliberate exception to laziness.
subtitle: A summary is still a frame. Three scalars are eager, on purpose.
---

## describe

`ur.describe` returns a lazy **one-row summary frame**, so it flows anywhere a frame flows.

```python
ur.describe(edges).collect().to_polars()
```

| Column | Meaning |
|---|---|
| `n_nodes` | distinct nodes appearing as `src` or `dst` |
| `n_edges` | edge rows, multiplicity included |
| `density` | `m / (n · (n − 1))` — directed, self-loops excluded from the denominator |
| `avg_degree` | mean degree over the node set |
| `n_components` | weakly connected components — **only with `full=True`** |

`n_components` is the expensive member, so it is gated:

```python
ur.describe(edges, full=True).collect().to_polars()
```

That gate is a deliberate resolution of an open question, not an accident: the default summary
should be cheap enough to call without thinking about it.

`describe` is a whole-graph summary rather than a node-keyed frame — it has no `id` column — so
collect it rather than deriving node-keyed frames from it. It is currently the final step of a
pipeline: a filter/sort/head tail after `describe()` raises.

## The eager scalars

Three metrics additionally exist as *eager* convenience functions returning plain Python numbers.
This is the one deliberate exception to Ursa being lazy-only, chosen for ergonomics — you almost
never want a one-value frame.

```python
ur.density(edges)                          # -> float
ur.avg_path_length(edges)                  # -> float
ur.avg_path_length(edges, sample=0.1)      # estimate from 10% of sources
ur.diameter(edges)                         # -> int, approximate by default
ur.diameter(edges, approximate=False)      # exact, on request
```

**`density`** — directed edge density, `m / (n · (n − 1))`: edges present over edges possible.
Multiplicity counts as given, so call `.distinct()` first if you want the simple-graph value.

**`avg_path_length`** — the mean shortest-path length over reachable ordered pairs, directed,
following out-edges. `sample` is a fraction in `(0, 1]` that estimates from a subset of sources;
omit it for the exact mean. On a large graph the exact value is an all-pairs traversal, and the
signature says so rather than hiding it.

**`diameter`** — the longest shortest path over reachable pairs, directed. The default is a
*lower-bound estimate*; pass `approximate=False` for the exact value, and expect it to cost what
an exact eccentricity computation costs.

The pattern across all three: the expensive thing is available, opt-in, and named honestly.

## Cost

All of these consult the same cached topology index as everything else, so calling `describe`
after a pipeline over the same frame does not rebuild anything. What they cost is the traversal
itself, and the sampled variants exist precisely because the exact ones are `O(n · m)`.
