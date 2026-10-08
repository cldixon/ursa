# Semantics vs NetworkX

Ursa's kernels make a small number of definitional choices. Most results match NetworkX exactly once you account for direction and normalization. A few differ by design. This page pins each kernel to the NetworkX call it matches.

The test suite checks every row of the table below against NetworkX on a seeded random graph (`tests/test_networkx_reference.py`).

## Three things to know first

1. **An `EdgeFrame` is directed.** `pagerank`, `betweenness`, and `closeness` follow the rows as stored. `degree`, `hop`, and `shortest_path` follow out-edges unless you pass `direction=`. The undirected kernels (`triangle_count`, `clustering_coefficient`, weak components, `louvain`, `label_propagation`) read the rows as undirected.
2. **Parallel edges are rows, and rows are kept.** Ursa does not merge two rows with the same endpoints. A NetworkX `Graph` or `DiGraph` merges them. On a graph with parallel edges, `pagerank` and `betweenness` differ from NetworkX. On a simple graph they match.
3. **Outputs are deterministic.** Every kernel gives the same result on every run and on every thread count. The random kernels (`random_walk`, sampled `betweenness`, `louvain`, `label_propagation`) are reproducible from `seed=`.

## Per-kernel alignment

In the table, `G` is a NetworkX `DiGraph` with the same rows, and `U = nx.Graph(G)` is its undirected view.

| Ursa | NetworkX | Notes |
|---|---|---|
| `ur.degree(edges)` | `G.out_degree()` | `direction="in"` is `G.in_degree()`. `direction="both"` is `U.degree()` on a simple graph. |
| `ur.pagerank(edges, damping=0.85)` | `nx.pagerank(G, alpha=0.85)` | A dangling node spreads its score to all nodes. |
| `ur.betweenness(edges)` | `nx.betweenness_centrality(G, normalized=False, endpoints=False)` | Each ordered pair is counted once, so there is no final halving. |
| `ur.closeness(edges)` | `nx.closeness_centrality(G.reverse(), wf_improved=False)` | See [Closeness](#closeness). |
| `ur.triangle_count(edges)` | `nx.triangles(U)` | |
| `ur.clustering_coefficient(edges)` | `nx.clustering(U)` | |
| `ur.connected_components(edges)` | `nx.connected_components(U)` | Weak components. |
| `ur.connected_components(edges, mode="strong")` | `nx.strongly_connected_components(G)` | |
| `ur.louvain(edges)` | `nx.community.louvain_communities(U)` | Compare the modularity, not the labels. |
| `ur.label_propagation(edges)` | `nx.community.label_propagation_communities(U)` | Compare the modularity, not the labels. |
| `ur.shortest_path(edges, s, t)` | `nx.shortest_path(G, s, t)` | With `weight=`: `nx.dijkstra_path(G, s, t, weight=...)`. |

## Closeness

Ursa measures outgoing distance: from the node, along out-edges. NetworkX measures incoming distance. The equivalent call reverses the graph: `nx.closeness_centrality(G.reverse(), wf_improved=False)`.

`wf_improved=False` removes the `(n − 1)` scaling that NetworkX applies by default. Ursa's value is `reachable / Σ distance`, the standard form for a disconnected graph. A node that reaches nothing scores `0.0`.

## Betweenness

Ursa counts each ordered pair `(s, t)` once and does not normalize. Compare with `normalized=False, endpoints=False`. On an undirected baseline, divide the NetworkX value by two, or compare on the `DiGraph`.

`sample=` runs Brandes from a random subset of sources and scales the result by `n / k`. The subset is a seeded shuffle, so the estimate is unbiased and reproducible from `seed=`.

With `weight=`, two paths tie only when their total costs are exactly equal as floats.

## Community detection

Louvain and label propagation are heuristics. Two correct runs can give a different partition, and the same partition with different label values. Compare the objective instead:

```py
import networkx as nx

labels = ur.louvain(edges, seed=1).collect().to_dicts()
groups = {}
for row in labels:
    groups.setdefault(row["louvain"], set()).add(row["id"])
print(nx.community.modularity(U, groups.values()))
```

Ursa's Louvain reaches a modularity within a small margin of the NetworkX value.

## Weighted kernels

`weight=` is an expression over the edge columns, evaluated to one `float64` per edge. Nothing is weighted unless you ask. `pagerank`, `shortest_path`, `closeness`, `betweenness`, and `louvain` accept it. `shortest_path` rejects a negative weight.

## Self-loops and parallel edges

Ursa keeps self-loops and parallel edges as rows. If your NetworkX baseline used a `Graph` or `DiGraph`, it merged the parallel edges. To compare, remove the duplicates from the rows before you build the frame. See [Reshaped edge sets](guides/subgraphs.md#reshaped-edge-sets).
