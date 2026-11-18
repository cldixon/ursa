---
title: Traversals
description: Hops, random walks and shortest paths, as SQL table functions that return rows of their own.
---

Traversals are table functions too, and they return rows of their own: the nodes reached, a path's edges, or walks. For patterns and paths of any length in SQL/PGQ's syntax, see [Pattern matching](/docs/graphs/pattern-matching/).

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
SELECT * FROM hop(karate, seeds => [0], n => 1, direction => 'both');

SELECT * FROM hop(karate,
                  seeds => (SELECT id FROM karate.nodes WHERE club = 'Officer'),
                  n => 2);
```

The second query returns 39 rows: each node within two steps of an Officer member, following edges in their stored direction, once for each member that reaches it. The seeds query runs as part of the same plan, so a filter in it is pushed into the scan of `karate_nodes.csv`. A seed that is not in the graph reaches nothing.

## random_walk

`random_walk` returns walks as rows: one per step, with the walk's number and position. Give it a `seed` for walks you can reproduce:

```sql
SELECT * FROM random_walk(karate, start => [0, 1], steps => 4, walks_per_node => 2, seed => 7);
```

## shortest_path

`shortest_path` returns the path between two nodes, one edge per row, with its step number and the cost so far:

```sql
SELECT * FROM shortest_path(karate, source => 0, target => 33, direction => 'both');
```

```text
┌───────┬───────┬───────┬─────────┐
│  src  │  dst  │  hop  │  cost   │
│ Int64 │ Int64 │ Int64 │ Float64 │
├───────┼───────┼───────┼─────────┤
│     0 │     8 │     0 │     1.0 │
│     8 │    33 │     1 │     2.0 │
└───────┴───────┴───────┴─────────┘
```

The [function reference](/docs/reference/functions/#traversals) lists every traversal's parameters.
