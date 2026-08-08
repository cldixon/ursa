---
title: Semantics vs NetworkX
description: The definitional choices behind each kernel, stated explicitly — and the exact NetworkX call each one is pinned against in the test suite.
subtitle: Difference imaging. Subtract the reference frame from ours; what remains is documented here.
---

Every kernel in Ursa is cross-checked against NetworkX in the test suite, which forces a decision
about *which* definition each one implements. Those decisions are not obvious, and you will hit
them the first time a number does not match something you computed elsewhere. They are collected
here rather than left in test comments.

The right-hand column is the exact NetworkX call Ursa is pinned against, so you can reproduce the
comparison yourself.

## Centralities

**Closeness** — out-edge closeness with the Wasserman–Faust correction **off**.

```python
nx.closeness_centrality(G.reverse(), wf_improved=False)
```

The `reverse()` is not a mistake: NetworkX's closeness measures distance *to* a node along
in-edges, and Ursa's follows out-edges like every other directed operation in the library.
Reversing the graph makes the two agree.

**Betweenness** — raw, un-normalized, directed.

```python
nx.betweenness_centrality(G, normalized=False)
```

Sampled betweenness (`sample=`) is seeded and deterministic; the sample is a `seed`-shuffled
subset of sources, so a given `(sample, seed)` pair always selects the same sources.

One genuine divergence: **parallel edges count as distinct shortest paths** in Ursa, because
multiplicity is just rows. A multigraph therefore diverges from a simple-graph reference like
NetworkX. Call `.distinct()` first to compare like with like.

## Triangles and clustering

**Triangle count** and **clustering coefficient** are computed on the **undirected view**.

```python
nx.triangles(nx.Graph(edges))
```

Direction is discarded for these two, which is the standard definition — a triangle is a triangle
regardless of which way the arrows point. Note that this is the one place in the library where
direction is *not* a parameter of the operation.

## Communities

**Louvain** — the partition is chosen by modularity score, and the labels are **arbitrary ids**.
Comparing two runs means comparing the partition (which nodes share a label), not the label values
themselves. The same is true of `label_propagation` and of `connected_components`.

**Connected components** — weak by default. `mode="strong"` raises; strongly connected components
are a later release rather than a silently-weak fallback.

## Multiplicity, self-loops and direction

Three rules, applied everywhere:

- **Multiplicity is just rows.** Duplicate `(src, dst)` rows are parallel edges and every kernel
  sees them. `.distinct()` if you want simple-graph semantics.
- **Self-loops are just rows** where `src == dst`. They are excluded from the density denominator;
  otherwise each kernel's docstring is the source of truth for its treatment.
- **Direction is a per-operation parameter**, `direction="out" | "in" | "both"`, defaulting to
  `"out"` — except for the two undirected-by-definition kernels above.

## Determinism

Every stochastic algorithm — `random_walk`, `label_propagation`, `louvain`, sampled `betweenness` —
takes `seed=`, and the guarantee is stronger than the usual one:

> Same seed ⇒ same result, **independent of thread count**.

That is not free. Floating-point addition is not associative, so a parallel reduction that sums in
whatever order the threads finish gives different answers at different thread counts. Ursa uses
fixed-order f64 reductions, order-independent tie-breaks in Louvain, and a single serial RNG
stream for walks, specifically to hold the guarantee.

The practical consequence: a result you commit to a test, or a walk you feed to an embedding
pipeline, is reproducible on a different machine with a different core count.

## Weighted variants

Weighted kernels are pinned the same way. With continuous weights, exact-cost ties effectively
vanish, so weighted shortest-path results agree with the reference to floating-point precision.
Where a tie *is* exact, it is broken deterministically.

`weight=` on a kernel that does not consume it raises. See [Weights](/docs/guides/weights).

## Reading the differences honestly

Ursa's benchmark harness treats a definitional mismatch as a **correctness failure**, not a
footnote: every measured cell is checked against the NetworkX oracle using these exact alignments,
and a fast wrong answer is recorded as `incorrect` rather than scored. A convention slip shows up
as a red cell, never as a silent pass — which is the only way a table of comparative numbers means
anything.
