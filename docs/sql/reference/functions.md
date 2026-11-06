<!-- Generated from SELECT * FROM ursa_functions(). Do not edit by hand: run
     URSA_BLESS=1 cargo test -p ursa-cli --test docs function_reference
-->

# Function reference

Every graph function Ursa's SQL engine offers. The page is generated from `SELECT * FROM ursa_functions()`, so it matches the engine you run.

An algorithm or a statistic takes the graph as its first argument, then named parameters with `=>`. It returns one row per node: the node's `id` and its output columns. `neighbors_agg` does too, from a value per node it reads. A traversal returns edges or walks. `algorithms` runs several per-node functions in one call.

A parameter's default is the SQL literal used when the parameter is left out. `required` means the call must give it. A `string` parameter with choices takes one of them, quoted: `direction => 'both'`. An `expression` parameter takes an expression over the edge table's columns: `weight => log10(1 + weight)`, or for `neighbors_agg`'s `attribute` over the node table's. A `query` parameter takes a subquery in parentheses.

A `node ids` or `node id` parameter takes integers or strings: a list, a query of one column, or one value. On a graph with integer ids, a string must be an integer, such as `'7'`; floats (even `2.0`), booleans and dates are refused. On a graph with string ids, an integer is taken as its string. A NULL, or an id not in the graph, matches no node.

## Algorithms

[`degree`](#degree) · [`pagerank`](#pagerank) · [`connected_components`](#connected_components) · [`triangle_count`](#triangle_count) · [`clustering_coefficient`](#clustering_coefficient) · [`betweenness`](#betweenness) · [`closeness`](#closeness) · [`label_propagation`](#label_propagation) · [`louvain`](#louvain)

### degree

Per-node degree.

```text
degree(graph, direction => 'out')
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `direction` | string | `'out'` | `'out'`, `'in'`, `'both'` | Follow out-edges, in-edges, or both (the undirected view). |

**Directions:** `'out'`, `'in'`, `'both'`

**Returns:** One row per node: `id` (the node), `degree` (UInt32)

**NetworkX:** `G.out_degree() (in: G.in_degree(); both: U.degree() on a simple graph)`

### pagerank

PageRank (pull-based fixpoint); a dangling node spreads its score to all nodes.

```text
pagerank(graph, damping => 0.85, max_iter => 30, tol => 1e-6, weight => NULL, direction => 'out')
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `damping` | number | `0.85` | – | Probability of following an edge. |
| `max_iter` | integer | `30` | – | Iteration cap. |
| `tol` | number | `1e-6` | – | Convergence tolerance on the L1 change. |
| `weight` | expression | `NULL` | – | A non-negative weight per edge; null is unweighted. |
| `direction` | string | `'out'` | `'out'`, `'both'` | Follow the rows as stored, or read the graph as undirected. |

**Directions:** `'out'`, `'both'`

**Returns:** One row per node: `id` (the node), `pagerank` (Float64)

**NetworkX:** `nx.pagerank(G, alpha=damping) (both: nx.pagerank(U))`

### connected_components

A component label per node: weak (ignoring direction) or strong.

```text
connected_components(graph, mode => 'weak')
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `mode` | string | `'weak'` | `'weak'`, `'strong'` | 'weak' ignores edge direction; 'strong' needs a path each way. |

**Returns:** One row per node: `id` (the node), `connected_components` (UInt32)

**NetworkX:** `nx.connected_components(U) (strong: nx.strongly_connected_components(G))`

### triangle_count

Per-node triangle count, reading edges as undirected.

```text
triangle_count(graph)
```

**Returns:** One row per node: `id` (the node), `triangle_count` (UInt32)

**NetworkX:** `nx.triangles(U)`

### clustering_coefficient

Local clustering coefficient, derived from the triangle counts.

```text
clustering_coefficient(graph)
```

**Returns:** One row per node: `id` (the node), `clustering_coefficient` (Float64)

**NetworkX:** `nx.clustering(U)`

### betweenness

Betweenness centrality (Brandes), unnormalized.

```text
betweenness(graph, sample => NULL, weight => NULL, seed => NULL, direction => 'out')
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `sample` | number or NULL | `NULL` | – | Estimate from this fraction of the sources; null is exact. |
| `weight` | expression | `NULL` | – | A non-negative weight per edge; null is unweighted. |
| `seed` | integer or NULL | `NULL` | – | Seed for the deterministic random stream; null is a fixed default. |
| `direction` | string | `'out'` | `'out'`, `'both'` | Follow the rows as stored, or read the graph as undirected. |

**Directions:** `'out'`, `'both'`

**Returns:** One row per node: `id` (the node), `betweenness` (Float64)

**NetworkX:** `nx.betweenness_centrality(G, normalized=False, endpoints=False) (both: nx.betweenness_centrality(U, normalized=False))`

### closeness

Closeness centrality, reachable / sum of distances.

```text
closeness(graph, weight => NULL, direction => 'out')
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `weight` | expression | `NULL` | – | A non-negative weight per edge; null is unweighted. |
| `direction` | string | `'out'` | `'out'`, `'in'`, `'both'` | Follow out-edges, in-edges, or both (the undirected view). |

**Directions:** `'out'`, `'in'`, `'both'`

**Returns:** One row per node: `id` (the node), `closeness` (Float64)

**NetworkX:** `nx.closeness_centrality(G.reverse(), wf_improved=False) (in: on G; both: on U)`

### label_propagation

Community labels by label propagation.

```text
label_propagation(graph, max_iter => 20, seed => NULL)
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `max_iter` | integer | `20` | – | Iteration cap. |
| `seed` | integer or NULL | `NULL` | – | Seed for the deterministic random stream; null is a fixed default. |

**Returns:** One row per node: `id` (the node), `label_propagation` (UInt32)

**NetworkX:** `nx.community.label_propagation_communities(U)`

### louvain

Community labels by Louvain modularity optimization.

```text
louvain(graph, weight => NULL, resolution => 1.0, seed => NULL)
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `weight` | expression | `NULL` | – | A non-negative weight per edge; null is unweighted. |
| `resolution` | number | `1.0` | – | Modularity resolution; larger gives smaller communities. |
| `seed` | integer or NULL | `NULL` | – | Seed for the deterministic random stream; null is a fixed default. |

**Returns:** One row per node: `id` (the node), `louvain` (UInt32)

**NetworkX:** `nx.community.louvain_communities(U)`

## Statistics

[`density`](#density) · [`avg_path_length`](#avg_path_length) · [`diameter`](#diameter) · [`describe`](#describe)

### density

Edges present over edges possible.

```text
density(graph, direction => 'out')
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `direction` | string | `'out'` | `'out'`, `'in'`, `'both'` | Follow out-edges, in-edges, or both (the undirected view). |

**Directions:** `'out'`, `'in'`, `'both'`

**Returns:** One row: `density` (Float64)

**NetworkX:** `nx.density(G) on a simple graph (both: nx.density(U))`

### avg_path_length

Mean shortest-path length over reachable ordered pairs.

```text
avg_path_length(graph, sample => NULL, direction => 'out')
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `sample` | number or NULL | `NULL` | – | Estimate from this fraction of the sources; null is exact. |
| `direction` | string | `'out'` | `'out'`, `'in'`, `'both'` | Follow out-edges, in-edges, or both (the undirected view). |

**Directions:** `'out'`, `'in'`, `'both'`

**Returns:** One row: `avg_path_length` (Float64)

**NetworkX:** `nx.average_shortest_path_length(U) on a connected graph (both)`

### diameter

The longest shortest path over reachable ordered pairs.

```text
diameter(graph, approximate => true, direction => 'out')
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `approximate` | boolean | `true` | – | A lower bound from a bounded sample of sources. |
| `direction` | string | `'out'` | `'out'`, `'in'`, `'both'` | Follow out-edges, in-edges, or both (the undirected view). |

**Directions:** `'out'`, `'in'`, `'both'`

**Returns:** One row: `diameter` (Int64)

**NetworkX:** `nx.diameter(U) on a connected graph (both, approximate=False)`

### describe

A one-row summary: nodes, edges, density, mean degree, components.

```text
describe(graph, full => false, direction => 'out', nodes => 'all')
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `full` | boolean | `false` | – | Also count the weak components (n_components is null otherwise). |
| `direction` | string | `'out'` | `'out'`, `'in'`, `'both'` | Follow out-edges, in-edges, or both (the undirected view). |
| `nodes` | string | `'all'` | `'all'`, `'touched'` | Describe every node, or only those at an end of a kept edge. |

**Directions:** `'out'`, `'in'`, `'both'`

**Returns:** One row: `n_nodes` (Int64), `n_edges` (Int64), `density` (Float64), `avg_degree` (Float64), `n_components` (Int64, may be NULL), `n_dropped_edges` (Int64)

**NetworkX:** `U.number_of_edges(), nx.density(U) and the mean degree (both)`

## Neighbour aggregation

[`neighbors_agg`](#neighbors_agg)

### neighbors_agg

A node attribute aggregated over each node's neighbours.

```text
neighbors_agg(graph, values => NULL, direction => 'out', attribute => NULL, agg => 'mean')
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `values` | query | `NULL` | – | A query of two columns, node ids then values: the value of each node. Give this or attribute. |
| `direction` | string | `'out'` | `'out'`, `'in'`, `'both'` | Follow out-edges, in-edges, or both (the undirected view). |
| `attribute` | expression | `NULL` | – | An expression over the graph's node table, the value of each node. Give this or values. |
| `agg` | string | `'mean'` | `'mean'`, `'sum'`, `'min'`, `'max'`, `'count'`, `'n_unique'` | The aggregation; mean and sum need numbers, and min and max take numbers, strings, dates or times. count and n_unique return integers. |

**Directions:** `'out'`, `'in'`, `'both'`

**Returns:** `neighbors_agg` (Float64; UInt32 for count and n_unique; the values' type for min and max of strings, dates or times, may be NULL)

## Traversals

[`hop`](#hop) · [`shortest_path`](#shortest_path) · [`random_walk`](#random_walk)

### hop

The nodes within n steps of each seed: one (src, dst) row per seed and node reached, src the seed. A seed is not its own row; a repeated seed repeats its rows.

```text
hop(graph, seeds => …, n => 1, direction => 'out')
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `seeds` | node ids | `required` | – | The nodes to start from: a list, a query of one column, or one id. |
| `n` | integer | `1` | – | How many steps to take. |
| `direction` | string | `'out'` | `'out'`, `'in'`, `'both'` | Follow out-edges, in-edges, or both (the undirected view). |

**Directions:** `'out'`, `'in'`, `'both'`

**Returns:** `src` (Int64 or Utf8), `dst` (Int64 or Utf8)

**NetworkX:** `nx.single_source_shortest_path_length(G, seed, cutoff=n), the seed left out (in: on G.reverse(); both: on U)`

### shortest_path

One shortest path from source to target: one row per edge, in order, with its 0-based position (hop) and the cost from source to its end. No path, no rows.

```text
shortest_path(graph, source => …, target => …, weight => NULL, direction => 'out')
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `source` | node id | `required` | – | The node the path starts at: an id, or a query of one value. |
| `target` | node id | `required` | – | The node the path ends at: an id, or a query of one value. |
| `weight` | expression | `NULL` | – | A non-negative weight per edge for the least-cost path; null is the fewest edges. |
| `direction` | string | `'out'` | `'out'`, `'in'`, `'both'` | Follow out-edges, in-edges, or both (the undirected view). |

**Directions:** `'out'`, `'in'`, `'both'`

**Returns:** `src` (Int64 or Utf8), `dst` (Int64 or Utf8), `hop` (Int64), `cost` (Float64)

**NetworkX:** `nx.shortest_path(G, source, target) (weighted: nx.dijkstra_path(G, source, target, weight=...))`

### random_walk

Random walks along out-edges from each start node: one (walk_id, step, node) row per step. A walk stops early at a node with no out-edges.

```text
random_walk(graph, start => …, steps => …, walks_per_node => 1, seed => NULL)
```

| Parameter | Type | Default | Choices | Description |
|---|---|---|---|---|
| `start` | node ids | `required` | – | The nodes to start walks at: a list, a query of one column, or one id. |
| `steps` | integer | `required` | – | How many steps each walk takes at most. |
| `walks_per_node` | integer | `1` | – | How many walks start at each start node. |
| `seed` | integer or NULL | `NULL` | – | Seed for the deterministic random stream; null is a fixed default. |

**Returns:** `walk_id` (Int64), `step` (Int64), `node` (Int64 or Utf8)

## Combining functions

[`algorithms`](#algorithms)

### algorithms

Several per-node functions over one index, one row per node: algorithms(g, degree(direction => 'both'), community => louvain(seed => 1)). Each argument is a call with named parameters; its column is named by its key or after the function.
