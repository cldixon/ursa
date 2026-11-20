---
title: Algorithms
description: Run graph algorithms as SQL table functions, with direction, weights, neighbour aggregation and subgraphs.
---

The algorithms, statistics and traversals are table functions. They go in `FROM`, take the graph as the first argument and named parameters with `=>`. An algorithm returns one row per node: the node's `id`, and a column named after the algorithm. `SELECT * FROM ursa_functions()` lists them all, and the [function reference](/docs/reference/functions/) describes each one.

The examples on this page share one session. They use the karate club graph from the [Quickstart](/docs/quickstart/):

```sql
CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv';
CREATE VIEW ties    AS SELECT * FROM 'karate_edges.csv';
CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (
    ties SOURCE KEY (src) REFERENCES members (id)
         DESTINATION KEY (dst) REFERENCES members (id)
  );
```

## Run an algorithm

```sql
SELECT * FROM degree(karate, direction => 'both') ORDER BY degree DESC LIMIT 1;
```

```text
┌───────┬────────┐
│  id   │ degree │
│ Int64 │ UInt32 │
├───────┼────────┤
│    33 │     17 │
└───────┴────────┘
```

To run several algorithms over one graph in one call, use `algorithms`; see [Combine algorithms and attributes](/docs/quickstart/#combine-algorithms-and-attributes).

## Direction

Every edge is stored from `src` to `dst`. Algorithms that walk edges take a `direction`:

| Value                 | Follows                   |
| --------------------- | ------------------------- |
| `'out'` (the default) | edges from a node         |
| `'in'`                | edges into a node         |
| `'both'`              | edges in either direction |

The karate edges are stored once per friendship, so `direction => 'both'` gives the undirected answer. Some algorithms only make sense one way and take no `direction`: `triangle_count`, `clustering_coefficient`, `louvain` and `label_propagation` read the graph as undirected.

## Weights

A weight is a column of the edge table, or an expression over its columns. Les Misérables has a `weight` column counting how often two characters appear together:

```sql
CREATE VIEW lesmis_edges AS SELECT * FROM 'lesmis_edges.csv';
CREATE PROPERTY GRAPH lesmis
  EDGE TABLES (lesmis_edges SOURCE KEY (src) DESTINATION KEY (dst));

SELECT * FROM pagerank(lesmis, weight => weight, direction => 'both')
ORDER BY pagerank DESC
LIMIT 5;
```

```text
┌──────────┬──────────────────────┐
│    id    │       pagerank       │
│   Utf8   │       Float64        │
├──────────┼──────────────────────┤
│ Valjean  │  0.09956068175644767 │
│ Marius   │ 0.051667656904091475 │
│ Myriel   │  0.03923416880292588 │
│ Cosette  │  0.03691018363299517 │
│ Enjolras │ 0.036614491459848165 │
└──────────┴──────────────────────┘
```

The weight can be any expression over the edge columns:

```sql
SELECT * FROM louvain(lesmis, weight => log10(1 + weight), seed => 7)
ORDER BY id
LIMIT 5;
```

```text
┌────────────┬─────────┐
│     id     │ louvain │
│    Utf8    │ UInt32  │
├────────────┼─────────┤
│ Anzelma    │       3 │
│ Babet      │       3 │
│ Bahorel    │       5 │
│ Bamatabois │       1 │
│ BaronessT  │       4 │
└────────────┴─────────┘
```

Weights must be non-negative and not null. The graph index does not depend on the weight, so trying several weightings reuses one index.

## Aggregate over neighbours

`neighbors_agg` aggregates a value of each node over its neighbours. It returns one row per node. Take the karate members again. Some of them have friends in both clubs. This query counts them, by club:

```sql
SELECT club, count(*) AS members
FROM karate.nodes
JOIN neighbors_agg(karate, attribute => club, agg => 'n_unique', direction => 'both') USING (id)
WHERE neighbors_agg = 2
GROUP BY club
ORDER BY club;
```

```text
┌─────────┬─────────┐
│  club   │ members │
│  Utf8   │  Int64  │
├─────────┼─────────┤
│ Mr. Hi  │       6 │
│ Officer │       7 │
└─────────┴─────────┘
```

There are two ways to give the value of each node:

- `attribute =>` takes an expression over the graph's node table.
- `values =>` takes any query of two columns, node ids then values. The values can even come from another algorithm, like each member's friends' mean degree:

```sql
SELECT id, round(neighbors_agg, 2) AS friends_degree
FROM neighbors_agg(karate,
                   values => (SELECT id, degree FROM degree(karate, direction => 'both')),
                   agg => 'mean', direction => 'both')
ORDER BY friends_degree DESC, id
LIMIT 3;
```

```text
┌───────┬────────────────┐
│  id   │ friends_degree │
│ Int64 │    Float64     │
├───────┼────────────────┤
│    11 │           16.0 │
│    14 │           14.5 │
│    15 │           14.5 │
└───────┴────────────────┘
```

The aggregations are `mean` (the default), `sum`, `min`, `max`, `count` and `n_unique`. `count` and `n_unique` return integers. Strings, dates and times take `count`, `n_unique`, `min` and `max`, and `min` and `max` return a value of their type. (In 0.6.0, `count` and `n_unique` return floats, and strings take only those two.) A node whose neighbours have no values gets NULL for `mean`, `min` and `max`, and 0 for the others.

## Lay out a graph

**New in 0.12.** `layout_fa2` and `layout_fr` are force-directed layouts: each returns one row per node, its `id` and its position, `x` and `y`. `layout_fa2` is ForceAtlas2, laid out coarse to fine: a coarsened graph first, then each finer level from its shape, which takes far fewer iterations of the graph itself than flat ForceAtlas2. `multilevel => false` runs flat ForceAtlas2 instead:

```sql
SELECT id, round(x, 2) AS x, round(y, 2) AS y
FROM layout_fa2(karate)
ORDER BY id
LIMIT 3;
```

```text
┌───────┬─────────┬─────────┐
│  id   │    x    │    y    │
│ Int64 │ Float32 │ Float32 │
├───────┼─────────┼─────────┤
│     0 │    19.8 │    4.06 │
│     1 │    9.05 │    7.98 │
│     2 │   -0.08 │   -2.24 │
└───────┴─────────┴─────────┘
```

A layout is deterministic: the same call gives the same positions, bit for bit, on any number of threads, so it can be computed once and saved:

```sql
COPY (SELECT * FROM layout_fa2(karate, seed => 7)) TO 'karate_layout.parquet';

SELECT count(*) AS nodes FROM 'karate_layout.parquet';
```

```text
┌────────┐
│ count  │
│ UInt64 │
├────────┤
│     34 │
└────────┘
┌───────┐
│ nodes │
│ Int64 │
├───────┤
│    34 │
└───────┘
```

Inside `algorithms`, a layout's columns are `x` and `y`, or prefixed by its name:

```sql
SELECT id, degree, round(pos_x, 2) AS x, round(pos_y, 2) AS y
FROM algorithms(karate, degree(direction => 'both'), pos => layout_fa2(iterations => 50))
ORDER BY degree DESC
LIMIT 2;
```

```text
┌───────┬────────┬─────────┬─────────┐
│  id   │ degree │    x    │    y    │
│ Int64 │ UInt32 │ Float32 │ Float32 │
├───────┼────────┼─────────┼─────────┤
│    33 │     17 │  -15.81 │   -5.63 │
│     0 │     16 │    17.9 │    7.93 │
└───────┴────────┴─────────┴─────────┘
```

The [function reference](/docs/reference/functions/#layouts) lists every parameter: `iterations`, `seed`, and ForceAtlas2's `k`, `gravity`, `strong_gravity`, `lin_log`, `theta` and `jitter_tolerance`. The layouts take no weight.

## Subgraphs

A subgraph is a graph over a view that filters the edges:

```sql
CREATE VIEW strong_ties AS SELECT * FROM lesmis_edges WHERE weight >= 3;

CREATE PROPERTY GRAPH lesmis_strong
  EDGE TABLES (strong_ties SOURCE KEY (src) DESTINATION KEY (dst));

SELECT count(DISTINCT connected_components) AS groups
FROM connected_components(lesmis_strong);
```

```text
┌────────┐
│ groups │
│ Int64  │
├────────┤
│      3 │
└────────┘
```

Without a node table, the nodes of a subgraph are the endpoints of the edges it keeps. A character whose every tie is weaker than 3 is not in `lesmis_strong`.

With a node table (**new in 0.7**), the node table defines the nodes, so a subgraph keeps every one of them, as a filtered edge frame in Python keeps its parent's nodes. This subgraph of the karate club keeps the ties from members 5 and up. Ten members, 0 to 4 among them, have no ties left, and are still nodes:

```sql
CREATE VIEW late_ties AS SELECT * FROM ties WHERE src >= 5;

CREATE PROPERTY GRAPH karate_late
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (
    late_ties SOURCE KEY (src) REFERENCES members (id)
              DESTINATION KEY (dst) REFERENCES members (id)
  );

SELECT n_nodes, n_edges FROM describe(karate_late);
```

```text
┌─────────┬─────────┐
│ n_nodes │ n_edges │
│  Int64  │  Int64  │
├─────────┼─────────┤
│      34 │      41 │
└─────────┴─────────┘
```

`nodes => 'touched'` describes only the nodes at an end of a kept edge:

```sql
SELECT n_nodes, n_edges FROM describe(karate_late, nodes => 'touched');
```

```text
┌─────────┬─────────┐
│ n_nodes │ n_edges │
│  Int64  │  Int64  │
├─────────┼─────────┤
│      24 │      41 │
└─────────┴─────────┘
```
