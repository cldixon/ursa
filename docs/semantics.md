# Semantics vs NetworkX

Ursa's kernels make a small number of definitional choices. Most results match NetworkX exactly once you account for direction and normalization. A few differ by design. This page pins each kernel to the NetworkX call it matches.

The test suite checks every row of the table below against NetworkX on a seeded random graph (`tests/test_networkx_reference.py`).

## Three things to know first

1. **An `EdgeFrame` is directed.** `pagerank`, `betweenness`, `closeness`, `degree`, `hop`, `shortest_path`, and the whole-graph statistics follow out-edges unless you pass `direction=`. The undirected kernels (`triangle_count`, `clustering_coefficient`, weak components, `louvain`, `label_propagation`) read the rows as undirected. If your NetworkX graph is an undirected `nx.Graph`, see [Undirected graphs](#undirected-graphs).
2. **Parallel edges are rows, and rows are kept.** Ursa does not merge two rows with the same endpoints. A NetworkX `Graph` or `DiGraph` merges them. On a graph with parallel edges, `pagerank` and `betweenness` differ from NetworkX. On a simple graph they match.
3. **Outputs are deterministic.** Every kernel gives the same result on every run and on every thread count. The random kernels (`random_walk`, sampled `betweenness`, `louvain`, `label_propagation`) take `seed=`. Without one they use a fixed default seed.

## Undirected graphs

Most NetworkX users arrive with an undirected `nx.Graph`. Stored once per edge, its rows are a directed graph, and the default `direction="out"` results differ from the NetworkX ones: on the karate club, `density` is half the undirected value and `diameter` is 3 instead of 5.

Pass `direction="both"`. It reads the rows as the **simple undirected graph**: one edge for each distinct pair of nodes joined by a row in either direction. A reciprocal pair (`u → v` and `v → u`) and parallel rows are one edge. Self-loops are dropped. This is NetworkX's `U = G.to_undirected()` on a graph without self-loops.

| Ursa | NetworkX |
|---|---|
| `ur.degree(edges, direction="both")` | `U.degree()` on a simple graph |
| `ur.pagerank(edges, direction="both")` | `nx.pagerank(U)` |
| `ur.betweenness(edges, direction="both")` | `nx.betweenness_centrality(U, normalized=False)` |
| `ur.closeness(edges, direction="both")` | `nx.closeness_centrality(U, wf_improved=False)` |
| `ur.density(edges, direction="both")` | `nx.density(U)` |
| `ur.avg_path_length(edges, direction="both")` | `nx.average_shortest_path_length(U)` on a connected graph |
| `ur.diameter(edges, approximate=False, direction="both")` | `nx.diameter(U)` on a connected graph |
| `ur.describe(edges, direction="both")` | `U.number_of_edges()`, `nx.density(U)`, and the mean degree |

On the karate club (`ur.datasets.load_karate()`, 78 rows, each edge once):

| Call | `direction="out"` (default) | `direction="both"` | NetworkX on `U` |
|---|---|---|---|
| `density` | 0.0695 | 0.1390 | 0.1390 |
| `avg_path_length` | 1.2736 | 2.4082 | 2.4082 |
| `diameter(approximate=False)` | 3 | 5 | 5 |

With `weight=`, a pair joined by several rows takes the smallest of their weights. Two differences from NetworkX remain. NetworkX keeps a self-loop in `U`, and `nx.density` counts it; Ursa drops it. For a reciprocal pair with two different weights, `to_undirected()` keeps one of them, and Ursa keeps the smaller.

`ur.from_networkx` symmetrizes an undirected graph for you: each edge of an `nx.Graph` becomes two rows, so the defaults already give the undirected result. `betweenness` is the exception: on two rows per edge, `direction="out"` counts each pair from both ends, twice the NetworkX value. Pass `direction="both"` to it. `ur.from_networkx(G, symmetrize=False)` keeps one row per edge.

`direction="in"` follows edges backward. `closeness(direction="in")` measures the distance to the node, which is `nx.closeness_centrality(G, wf_improved=False)` on the `DiGraph`. For `density`, `avg_path_length`, `diameter`, and `describe`, `"in"` gives the same value as `"out"`. `pagerank` and `betweenness` reject it.

## Per-kernel alignment

In the table, `G` is a NetworkX `DiGraph` with the same rows, and `U = nx.Graph(G)` is its undirected view.

| Ursa | NetworkX | Notes |
|---|---|---|
| `ur.degree(edges)` | `G.out_degree()` | `direction="in"` is `G.in_degree()`. `direction="both"` is `U.degree()` on a simple graph. |
| `ur.pagerank(edges, damping=0.85)` | `nx.pagerank(G, alpha=0.85)` | A dangling node spreads its score to all nodes. `direction="both"` is `nx.pagerank(U)`. |
| `ur.betweenness(edges)` | `nx.betweenness_centrality(G, normalized=False, endpoints=False)` | Each ordered pair is counted once, so there is no final halving. `direction="both"` is `nx.betweenness_centrality(U, normalized=False)`. |
| `ur.closeness(edges)` | `nx.closeness_centrality(G.reverse(), wf_improved=False)` | See [Closeness](#closeness). |
| `ur.density(edges)` | `nx.density(G)` on a simple graph | `direction="both"` is `nx.density(U)`. |
| `ur.triangle_count(edges)` | `nx.triangles(U)` | |
| `ur.clustering_coefficient(edges)` | `nx.clustering(U)` | |
| `ur.connected_components(edges)` | `nx.connected_components(U)` | Weak components. |
| `ur.connected_components(edges, mode="strong")` | `nx.strongly_connected_components(G)` | |
| `ur.louvain(edges)` | `nx.community.louvain_communities(U)` | Compare the modularity, not the labels. |
| `ur.label_propagation(edges)` | `nx.community.label_propagation_communities(U)` | Compare the modularity, not the labels. |
| `ur.shortest_path(edges, s, t)` | `nx.shortest_path(G, s, t)` | With `weight=`: `nx.dijkstra_path(G, s, t, weight=...)`. |

## Closeness

Ursa measures outgoing distance by default: from the node, along out-edges. NetworkX measures incoming distance. The equivalent call reverses the graph: `nx.closeness_centrality(G.reverse(), wf_improved=False)`. `direction="in"` measures incoming distance, matching `nx.closeness_centrality(G, wf_improved=False)`, and `direction="both"` ignores direction, matching `nx.closeness_centrality(U, wf_improved=False)`.

`wf_improved=False` removes the `(n − 1)` scaling that NetworkX applies by default. Ursa's value is `reachable / Σ distance`, the standard form for a disconnected graph. A node that reaches nothing scores `0.0`.

## Betweenness

Ursa counts each ordered pair `(s, t)` once and does not normalize. Compare with `normalized=False, endpoints=False`. With `direction="both"`, Ursa reads the undirected view and counts each unordered pair once, which is `nx.betweenness_centrality(U, normalized=False)`.

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

## Floats: one order for comparisons, sorts and keys

Ursa compares floats one way everywhere: in **total order**, with NaN and zero canonical.

- `-0.0` equals `0.0`.
- Every NaN equals every NaN, whatever its sign bit or payload (`0/0` gives a negative one on most machines), and NaN is greater than every number.

This holds for top-level `float16`, `float32`, `float64` and dictionary-of-float columns, wherever a float is compared. A float nested in a struct or list column compares by its bits: `distinct` keeps `{"x": -0.0}` and `{"x": 0.0}` as two rows.

Where floats compare:

- **Filters.** `ur.col("x") > 1` keeps a NaN row, `ur.col("x") == float("nan")` keeps exactly the NaN rows, and `ur.col("x") != ur.col("x")` keeps none. A comparison with null is null, and `filter` drops that row. NumPy and pyarrow compare the IEEE way instead, where every comparison with NaN is false. To drop NaN rows, filter on a range, for example `ur.col("x") <= float("inf")`.
- **`sort()`.** Ascending, every NaN comes after every number; descending, before. Nulls come last in either direction. Rows that tie, such as two NaNs of different sign or `-0.0` and `0.0`, keep their input order.
- **`group_by` keys and `distinct`.** `-0.0` and `0.0` are one group, and so are all the NaNs. `distinct` compares every float column of the row this way. Nulls form one group of their own.
- **`n_unique`.** Counts `-0.0` and `0.0` once, and every NaN once.
- **Join keys**, for every `how`. `-0.0` joins `0.0`, and NaN joins NaN. A null key never joins anything. The key of a `right` or `outer` join keeps the left key's type, and a right key's value fits it in total order: `-0.0` fits an integer key as `0`, and a NaN fits a narrower float whatever its payload.

A group or distinct row shows its **first** row's value, as it comes first in the output: grouping `[-0.0, 0.0]` gives one row with key `-0.0`, and a NaN group shows the bits of its first NaN. A joined row keeps each side's own key value. An aggregate over a key column (`group_by("x").agg(s=col("x").sum())`) folds the original values.
