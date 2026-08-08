# How Ursa differs from NetworkX

Ursa's algorithm kernels make a handful of deliberate definitional choices. Most
match NetworkX exactly once you account for **direction** and **normalization**;
a few diverge by design. This page is the user-facing reference for those choices
— the source of truth is the kernel docstrings and the cross-check suite in
[`tests/test_networkx_reference.py`](../tests/test_networkx_reference.py), which
pins each kernel against NetworkX on a seeded ~100-node graph.

If a result surprises you coming from NetworkX, it is almost always one of the
three things at the top of this list.

## The three things to know first

1. **An EdgeFrame is directed.** `src → dst` is a directed edge. Degree, PageRank,
   betweenness, closeness, and traversals follow **out-edges** by default
   (`direction="out"`). Pass `direction="in"` or `direction="both"` where the verb
   accepts it. The kernels that are inherently undirected (triangles, clustering,
   weak components, Louvain) take the **undirected view** of the same edges — see
   the table.

2. **Parallel edges are rows, and rows are kept.** Ursa never collapses parallel
   `(u, v)` edges: each edge row is a distinct entry in the topology
   ("multiplicity-is-rows"). On a **multigraph**, this makes PageRank and
   betweenness diverge from a NetworkX `Graph`/`DiGraph`, which silently dedupes
   parallel edges. On a **simple** graph (no parallel edges) there is no
   difference — which is why the cross-check suite uses a simple random graph.

3. **Outputs are deterministic.** Every kernel is reproducible across runs *and*
   across thread counts (bit-for-bit for the float reductions). Randomized verbs
   (`random_walk`, sampled `betweenness`) take a `seed=` and are reproducible from
   it, independent of parallelism.

## Per-algorithm alignment

| Ursa | Direction | NetworkX equivalent | Notes |
|---|---|---|---|
| `pagerank(edges, damping=0.85)` | directed | `nx.pagerank(G, alpha=0.85)` | Dangling nodes redistribute uniformly. Parallel edges pull rank once **each**. |
| `betweenness(edges)` | directed | `nx.betweenness_centrality(G, normalized=False, endpoints=False)` | Raw (unnormalized) dependency; ordered pairs counted once, so **no final halving**. |
| `closeness(edges)` | out-edge | `nx.closeness_centrality(G.reverse(), wf_improved=False)` | `reachable / Σ dist` over reachable nodes — no `(n−1)` Wasserman–Faust scaling; a node reaching nothing scores `0.0`. |
| `triangle_count(edges)` | undirected | `nx.triangles(nx.Graph(edges))` | Computed on the undirected view. |
| `clustering_coefficient(edges)` | undirected | `nx.clustering(nx.Graph(edges))` | Derived from the undirected triangles. |
| `connected_components(edges)` | undirected | `nx.connected_components` (weak) | `mode="weak"` (default) ignores direction. |
| `connected_components(edges, mode="strong")` | directed | `nx.strongly_connected_components` | Mutual reachability following direction. |
| `louvain(edges)` | undirected | `nx.community.louvain_communities` | Heuristic — compare **modularity**, not labels (see below). |
| `label_propagation(edges)` | undirected | `nx.community.label_propagation` | Heuristic; labels are arbitrary stable ids. |
| `shortest_path(edges, s, t)` | out-edge | `nx.shortest_path` (BFS) / `nx.dijkstra_path` (weighted) | Unweighted = fewest hops; with `weight=` = minimum cost. |

### Closeness

Ursa measures **outgoing** distance (`u → v` following out-edges). NetworkX's
`closeness_centrality` measures **incoming** distance, so the exact equivalent
reverses the graph: `nx.closeness_centrality(G.reverse(), wf_improved=False)`.
`wf_improved=False` drops the `(n−1)` scaling NetworkX applies by default, matching
Ursa's `reachable / Σ dist` form (the standard form for disconnected graphs). A
node reachable at total cost exactly `0.0` (only possible with zero-weight edges)
is counted like any other reachable node; if *every* reachable node is at cost 0,
the score falls back to `0.0` rather than dividing by zero — the same guard
NetworkX applies.

### Betweenness

Directed and **unnormalized** (`normalized=False`). Because Ursa counts each
ordered pair `(s, t)` once, there is no final halving that an undirected
formulation would apply — so compare against NetworkX with `normalized=False,
endpoints=False`. `sample=` runs Brandes from a `seed`-shuffled subset of sources
and scales by `n/k` (the Brandes–Pich estimator); the shuffle (not a strided
sample) keeps the estimate unbiased and, with `seed=`, reproducible. For **weighted**
betweenness, two paths tie as equal-cost shortest paths only when their total costs
are *exactly* float-equal.

### Community detection (Louvain / label propagation)

Both are heuristics, so the exact partition will not match NetworkX's — and two
correct runs can label the same community with different ids. Compare the
**objective** instead: a partition's modularity
(`nx.community.modularity(G, partition)`), which Ursa's Louvain is verified to
reach within a small margin of NetworkX's. Group nodes by their label column to
recover the communities.

## Weighted algorithms

`weight=` is a per-operation **expression** over edge columns, evaluated to one
`f64` per edge — e.g. `ur.pagerank(edges, weight=ur.col("amount") * ur.col("fx"))`.
It is never a blessed column, so nothing is weighted unless you ask. Weighted
PageRank, `shortest_path` (Dijkstra), closeness, betweenness (Dijkstra–Brandes),
and Louvain all ship. A null or negative weight is rejected where the algorithm
requires otherwise (Dijkstra needs non-negative weights).

## Self-loops and multigraphs, in one line

Ursa keeps self-loops and parallel edges as ordinary rows in the topology; it does
not dedupe or drop them. If your NetworkX baseline used a simple `Graph`/`DiGraph`
(which drops parallels) or pre-removed self-loops, normalize the edge **source**
the same way before ingesting — dedupe the `(src, dst)` rows in the table/file you
build the EdgeFrame from — so the two libraries see the same graph.
