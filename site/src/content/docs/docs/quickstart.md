---
title: Quickstart
description: Query a file, define a graph and run graph algorithms on it with Ursa SQL.
---

Ursa is a SQL engine for graph analytics. It reads Parquet, CSV, JSON and Arrow files where they are, on your disk or in object storage, and runs graph algorithms over them with SQL. There is no server and nothing to load first. An edge table is a graph.

This page needs `ursa` installed (see [Installation](/docs/installation/)), or open [Ursa Shell with the karate dataset loaded](https://ursa-shell.cl-dixon.workers.dev/?dataset=karate).

## Query a file

This guide uses Zachary's karate club: 34 members of a club, and 78 friendships between them. Download the two files:

```bash
curl -LO https://raw.githubusercontent.com/cldixon/ursa/main/python/ursa/datasets/data/karate_edges.csv
curl -LO https://raw.githubusercontent.com/cldixon/ursa/main/python/ursa/datasets/data/karate_nodes.csv
```

Any file is a table. Name it in `FROM`:

```bash
ursa -c "SELECT * FROM 'karate_edges.csv' LIMIT 3"
```

```text
┌───────┬───────┐
│  src  │  dst  │
│ Int64 │ Int64 │
├───────┼───────┤
│     0 │     1 │
│     0 │     2 │
│     0 │     3 │
└───────┴───────┘
```

The format comes from the extension: `.parquet`, `.csv`, `.json`, `.jsonl` and `.ndjson` (JSON, one object per line; `.jsonl` and `.ndjson` are new in 0.7), and `.arrow`. CSV and JSON may be compressed (`.csv.gz`, `.jsonl.zst`). A directory or a glob reads every file in it: `'edges/*.parquet'`.

**New in 0.7:** `read_parquet`, `read_csv` and `read_json` read a file in the format they name, whatever its extension, with options, as in DuckDB:

```bash
ursa -c "SELECT * FROM read_csv('karate_edges.csv', header => true, delim => ',') LIMIT 3"
```

[Reading and writing files](/docs/import-export/files/) lists the options.

## Open a database

Start the shell with a database file:

```bash
ursa club.ursa
```

The file is created if it does not exist. It keeps the tables, views and graphs you define, and the graph indexes Ursa builds, so the next session (or the next `ursa -c` against the same file) starts where you left off. Without a file name, `ursa` runs in memory and forgets everything when it exits. [The database file](/docs/database/database-file/) has the details.

The rest of this guide runs in that shell.

## Define a graph

First, name the files as tables:

```sql
CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv';
CREATE VIEW ties    AS SELECT * FROM 'karate_edges.csv';
```

A view reads the file each time it is queried, so it always sees the file's current contents. Nothing is copied into the database.

Then define the graph. Ursa uses the `CREATE PROPERTY GRAPH` statement from the SQL standard (SQL/PGQ):

```sql
CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (
    ties SOURCE KEY (src) REFERENCES members (id)
         DESTINATION KEY (dst) REFERENCES members (id)
  );
```

Every row of `ties` is an edge from `src` to `dst`. Every row of `members` is a node, keyed by `id`. The node table defines the graph's nodes, as in SQL/PGQ and DuckPGQ (**new in 0.7**; in 0.6.0 the algorithms see only the edges' endpoints): a member with no ties is a node all the same, of degree 0, and every algorithm returns a row for it. A tie whose `src` or `dst` is not an `id` of `members` is left out of the graph, and `describe` counts such ties in its `n_dropped_edges` column.

The node table is optional. Without it, the nodes are the endpoints of the edges:

```sql
CREATE PROPERTY GRAPH karate_edges_only
  EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));
```

A graph is a schema with two tables. `karate.edges` holds the edges, and `karate.nodes` holds one row per node with its attributes:

```sql
SELECT * FROM karate.nodes LIMIT 3;
```

```text
┌───────┬────────┐
│  id   │  club  │
│ Int64 │  Utf8  │
├───────┼────────┤
│     0 │ Mr. Hi │
│     1 │ Mr. Hi │
│     2 │ Mr. Hi │
└───────┴────────┘
```

[Property graphs](/docs/graphs/property-graphs/) covers the whole statement: several tables, labels and properties.

## Run an algorithm

An algorithm is a table function. Its first argument is the graph. It returns one row per node: the node's `id`, and a column named after the algorithm.

```sql
SELECT id, round(pagerank, 4) AS pagerank
FROM pagerank(karate, direction => 'both')
ORDER BY pagerank DESC
LIMIT 5;
```

```text
┌───────┬──────────┐
│  id   │ pagerank │
│ Int64 │ Float64  │
├───────┼──────────┤
│    33 │   0.1009 │
│     0 │    0.097 │
│    32 │   0.0717 │
│     2 │   0.0571 │
│     1 │   0.0529 │
└───────┴──────────┘
```

Parameters are named, with `=>`:

```sql
SELECT * FROM louvain(karate, seed => 1);
SELECT * FROM pagerank(karate, damping => 0.9, direction => 'both');
```

## Combine algorithms and attributes

`algorithms` runs several algorithms over one graph and returns one row per node with a column for each. Name a column with `name =>`; otherwise it is named after the algorithm. Join it to the node table on `id`. This query finds the five best-connected members, with their club, how many triangles they are part of, and their community:

```sql
WITH top AS (
    SELECT * FROM algorithms(karate,
                             degree(direction => 'both'),
                             triangles => triangle_count(),
                             community => louvain(seed => 1))
    ORDER BY degree DESC
    LIMIT 5
)
SELECT id, club, degree, triangles, community
FROM top JOIN karate.nodes USING (id)
ORDER BY degree DESC;
```

```text
┌───────┬─────────┬────────┬───────────┬───────────┐
│  id   │  club   │ degree │ triangles │ community │
│ Int64 │  Utf8   │ UInt32 │  UInt32   │  UInt32   │
├───────┼─────────┼────────┼───────────┼───────────┤
│    33 │ Officer │     17 │        15 │         2 │
│     0 │ Mr. Hi  │     16 │        18 │         0 │
│    32 │ Officer │     12 │        13 │         2 │
│     2 │ Mr. Hi  │     10 │        11 │         0 │
│     1 │ Mr. Hi  │      9 │        12 │         0 │
└───────┴─────────┴────────┴───────────┴───────────┘
```

Each algorithm also works on its own, as in the previous section, and those results join on `id` too.

:::tip[Take the top k, then join the attributes]
The query takes the five rows it wants from `algorithms(...)` first, and joins the club to those five. Written the other way round, `FROM karate.nodes JOIN algorithms(...) USING (id) ORDER BY degree DESC LIMIT 5` gives the same rows, but the `ORDER BY ... LIMIT` is not moved below the join, so every node's attributes are carried through the sort. On a graph of millions of nodes that is about twice the memory, and slower.

This is safe when the join keeps every row, as a join to the graph's own node table does: one row for each node. A join that drops rows, such as a `WHERE` on an attribute or a [`GRAPH_TABLE`](/docs/graphs/pattern-matching/), must come before the `LIMIT`.
:::

All three algorithms run over one index of the graph. Ursa builds it the first time a query needs it and stores it in `club.ursa`. The next query, in this session or a later one, uses the stored index. If `karate_edges.csv` changes, Ursa notices and builds a new one.

## Output

The shell prints tables. From the command line, choose a format with `--format`:

```bash
ursa club.ursa --format json -c "SELECT * FROM pagerank(karate) ORDER BY pagerank DESC LIMIT 3"
ursa club.ursa --format csv -c "SELECT * FROM louvain(karate, seed => 1)" > communities.csv
```

The formats are `table` (the default), `csv`, `tsv`, `json`, `ndjson` and `arrow`. To write a file from SQL, use `COPY`. It prints how many rows it wrote:

```sql
COPY (SELECT * FROM pagerank(karate)) TO 'pagerank.parquet';
```

```text
┌────────┐
│ count  │
│ UInt64 │
├────────┤
│     34 │
└────────┘
```

## Find out what is available

```sql
SHOW GRAPHS;
DESCRIBE GRAPH karate;
SELECT * FROM ursa_functions();
```

`ursa_functions()` lists every graph function with its parameters, their defaults, the NetworkX function it matches, and a one-line description.

## Next steps

- [Algorithms](/docs/graphs/algorithms/): direction, weights, neighbour aggregation and subgraphs.
- [Traversals](/docs/graphs/traversals/): hops, random walks and shortest paths.
- [Pattern matching](/docs/graphs/pattern-matching/): `GRAPH_TABLE` patterns and paths of any length.
- [Object storage](/docs/import-export/object-storage/): the same queries over files in S3, GCS or Azure.
- The [function reference](/docs/reference/functions/) lists every function and exactly what it computes.
- [The shell](/docs/cli/shell/) covers dot-commands, init files and history.
- The [Python library](https://cldixon.github.io/ursa/) runs the same algorithms with a DataFrame API, and gives the same values.
