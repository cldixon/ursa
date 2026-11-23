---
title: Traversals
description: Hops, random walks, shortest paths and the edges among a set of nodes, as SQL table functions that return rows of their own.
---

Traversals are table functions too, and they return rows of their own: the nodes reached, a path's edges, walks, or the edges among a set of nodes. For patterns and paths of any length in SQL/PGQ's syntax, see [Pattern matching](/docs/graphs/pattern-matching/).

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

## hop

`hop` returns the nodes within `n` steps of a set of seed nodes, one `(seed, id, distance)` row per seed and node reached: `id` is the node and `distance` the fewest steps from the seed to it. The seeds can be a list or a query. Member 0 has 16 friends, so this returns 16 rows:

```sql
SELECT * FROM hop(karate, seeds => [0], steps => 1, direction => 'both');

SELECT * FROM hop(karate,
                  seeds => (SELECT id FROM karate.nodes WHERE club = 'Officer'),
                  steps => 2);
```

The second query returns 39 rows: each node within two steps of an Officer member, following edges in their stored direction, once for each member that reaches it. The seeds query runs as part of the same plan, so a filter in it is pushed into the scan of `karate_nodes.csv`. A seed that is not in the graph reaches nothing. The rows come back in no particular order, which can differ from one run to another; add `ORDER BY` (for example `ORDER BY seed, distance, id`) when the order matters.

## random_walk

`random_walk` returns walks as rows: one per step, with the walk's number and position. Give it a `seed` for walks you can reproduce:

```sql
SELECT * FROM random_walk(karate, seeds => [0, 1], steps => 4, walks_per_node => 2, random_seed => 7);
```

## shortest_path

`shortest_path` returns the path between two nodes, one edge per row, with its step number and the cost so far:

```sql
SELECT * FROM shortest_path(karate, source => 0, target => 33, direction => 'both');
```

```text
┌───────┬───────┬───────┬─────────┐
│  src  │  dst  │ step  │  cost   │
│ Int64 │ Int64 │ Int64 │ Float64 │
├───────┼───────┼───────┼─────────┤
│     0 │     8 │     0 │     1.0 │
│     8 │    33 │     1 │     2.0 │
└───────┴───────┴───────┴─────────┘
```

## induced_edges

`induced_edges` returns the graph's edges among a set of nodes: the edges whose two ends are both in the set. Each row is one edge, with its two ends as the graph stores its direction and `edge`, its row in the edge table. The nodes can be a list or a query, as `hop`'s seeds:

```sql
SELECT * FROM induced_edges(karate, nodes => [0, 1, 2, 3]);
```

```text
┌───────┬───────┬───────┐
│  src  │  dst  │ edge  │
│ Int64 │ Int64 │ Int64 │
├───────┼───────┼───────┤
│     0 │     1 │     0 │
│     0 │     2 │     1 │
│     0 │     3 │     2 │
│     1 │     2 │    16 │
│     1 │     3 │    17 │
│     2 │     3 │    24 │
└───────┴───────┴───────┘
```

It reads the graph's index, so its time depends on how many edges the nodes have, not on the size of the graph. A filter on the edge table, `WHERE src IN (…) AND dst IN (…)`, gives the same rows by reading every edge. With a query for the nodes, it returns the part of a graph another function picked. The ten highest-ranked members and the ties among them:

```sql
SELECT src, dst
FROM induced_edges(karate, nodes => (
  SELECT id FROM pagerank(karate, direction => 'both') ORDER BY pagerank DESC LIMIT 10
))
LIMIT 5;
```

```text
┌───────┬───────┐
│  src  │  dst  │
│ Int64 │ Int64 │
├───────┼───────┤
│     0 │     1 │
│     0 │     2 │
│     0 │     3 │
│     0 │     8 │
│     0 │    13 │
└───────┴───────┘
```

A node that is not in the graph is on no edge. Parallel edges are rows of their own, and rows are ordered by `edge`.

The [function reference](/docs/reference/functions/#traversals) lists every traversal's parameters.
