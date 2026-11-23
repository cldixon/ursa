---
title: "Property graphs"
description: "Define, list, describe and drop graphs with CREATE PROPERTY GRAPH and the other SQL/PGQ statements."
---

A graph in Ursa is a name for an edge table, and optionally vertex tables, defined with `CREATE PROPERTY GRAPH` from SQL/PGQ, the property graph part of the SQL standard. Algorithms, traversals and `GRAPH_TABLE` patterns then take the graph's name.

The examples on this page share one session. They start with two views over the karate club files:

```sql
CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv';
CREATE VIEW ties    AS SELECT * FROM 'karate_edges.csv';
```

## CREATE PROPERTY GRAPH

:::note[New in 0.7]
0.6.0 accepts one vertex table with a `KEY` and one edge table. Several tables, optional `KEY`, `AS` names, `LABEL` and `PROPERTIES` (DuckPGQ's syntax included) are new in `ursa-cli` 0.7.
:::

Defines a graph over vertex tables and edge tables. The syntax is SQL/PGQ's, the property graph part of the SQL standard, so a graph definition written for another SQL/PGQ engine, such as DuckPGQ, runs here too:

```text
CREATE [OR REPLACE] PROPERTY GRAPH [IF NOT EXISTS] name
  [VERTEX TABLES ( vertex_table [, …] )]
  [EDGE TABLES ( edge_table [, …] )]

vertex_table := table [AS name] [KEY (column)] [labels]
edge_table   := table [AS name] [KEY (column)]
                SOURCE KEY (column) [REFERENCES vertex_table (column)]
                DESTINATION KEY (column) [REFERENCES vertex_table (column)]
                [labels]
labels       := DEFAULT LABEL [properties] | LABEL name [properties] [LABEL name [properties] …] | properties
              | properties LABEL name [LABEL name …]
properties   := PROPERTIES (column [AS name], …)
              | PROPERTIES [ARE] ALL COLUMNS [EXCEPT (column, …)]
              | NO PROPERTIES
```

The graph most of these pages use has one vertex table and one edge table:

```sql
CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (
    ties SOURCE KEY (src) REFERENCES members (id)
         DESTINATION KEY (dst) REFERENCES members (id)
  );
```

- **An edge table** is any table or view. Each row is an edge from its `SOURCE KEY` column to its `DESTINATION KEY` column. The two columns must both be integers or both be strings. Integers may differ in width (`INT` and `BIGINT`): node ids are `BIGINT` either way.
- **A vertex table** defines the nodes and gives them attributes, keyed by its `KEY` column. `KEY` can be left out: the key is then the table's primary key, or else the column the edge tables' `REFERENCES` name. Ursa adds one relaxation to the standard: vertex tables are optional, and without one the nodes are the endpoints of the edges.
- **The node set** (new in 0.7; in 0.6.0 the algorithms see only the edges' endpoints). With a vertex table, every key is a node, one with no edges included, and every algorithm, traversal and pattern runs on those nodes, in the table's order. An edge whose source or destination is not a key is left out, as DuckPGQ leaves it out; `describe` counts such edges in `n_dropped_edges`.
- **A key identifies one row** (behaviour change in 0.12). A vertex table's key must be unique and not NULL, as SQL/PGQ requires. A vertex table with a duplicate or a NULL key is refused with an error that counts the rows and names a duplicate, by whatever reads it as the graph's nodes: the algorithms and traversals, `g.nodes`, and `GRAPH_TABLE`. The table is read when a query runs, so `CREATE PROPERTY GRAPH` still accepts it. Deduplicate it in a view and define the graph over the view: `CREATE VIEW people_nodes AS SELECT DISTINCT ON (id) * FROM people WHERE id IS NOT NULL ORDER BY id`. Before 0.12 the algorithms counted a duplicated key once and skipped a NULL key, while `g.nodes` and `GRAPH_TABLE` returned every row, so they did not agree on the nodes.
- **Keys with no edges** (new in 0.8) cost the algorithms next to nothing: they run on the nodes with at least one edge and fill in each isolated node's result, which is the same every time. It is degree 0, its own component and its own community (a label of its own), 0 triangles and a clustering coefficient of 0. Its PageRank is its share of the teleport and of the dangling nodes' score, since a node with no edges is dangling too. The results are those of running on every node, bit for bit, but two of them depend on how many keys there are. PageRank's scores sum to 1 over every node, so a vertex table with more keys gives every node a lower score. And every isolated node is a community: `count(DISTINCT louvain)` counts each one, so count the communities with more than one member with `GROUP BY louvain HAVING count(*) > 1`.
- **`REFERENCES`** names a vertex table, by its `AS` name or its table name, and its key. With several vertex tables, every endpoint needs one, to say which table it refers to.
- **Labels and properties.** Each table has a label, its name unless `LABEL` gives others, and exposes its columns as properties, all of them unless a `PROPERTIES` clause says otherwise. DuckPGQ's order, `Student PROPERTIES (id, name) LABEL Person`, gives `Person` those properties.
- `NODE TABLES` and `RELATIONSHIP TABLES`, the standard's synonyms, work too.

A graph with one edge table and at most one vertex table is a schema with two tables. `karate.edges` holds the edge table's rows. `karate.nodes` holds one row per node, with the vertex table's columns:

```sql
SELECT count(*) AS edges FROM karate.edges;
```

```text
┌───────┐
│ edges │
│ Int64 │
├───────┤
│    78 │
└───────┘
```

A graph over a view that filters edges is a subgraph. With a vertex table, its nodes are the vertex table's keys, those on no kept edge included; without one, they are the endpoints of the edges it keeps:

```sql
CREATE VIEW first_ties AS SELECT * FROM ties WHERE src < 10;
CREATE PROPERTY GRAPH first EDGE TABLES (first_ties SOURCE KEY (src) DESTINATION KEY (dst));
```

**Edge order.** The order of the edges decides the graph's internal node numbering, which in turn decides community label values and the last bits of floating-point results. With a vertex table, the nodes are numbered in the vertex table's order instead, as DuckPGQ numbers them, so the same edges with and without a vertex table can differ in those label values and last bits; a vertex table that lists the nodes in the order the edges first name them gives the Python library's results exactly. A vertex table that is not read in a fixed order (a join, an aggregate) is sorted by its key. Over a file, or a view that filters or projects one, Ursa reads the edges in file order, as the Python library does. A join, an aggregate or a `UNION` gives its rows in no fixed order, so over an edge table built with one of those, Ursa sorts the edges by `(source, destination)` before building the graph. Results are then the same on every run and any thread count. The same holds for a traversal's `seeds`: from a subquery with a join or an aggregate, they are sorted by value. `EXPLAIN` shows the sort as a `SortExec` under the graph function.

`OR REPLACE` replaces a graph of the same name. `IF NOT EXISTS` does nothing if there is one. Defining a graph that exists without either is an error.

### Several tables

:::note[New in 0.7]
Graphs of several tables are new in `ursa-cli` 0.7.
:::

A graph can have any number of vertex and edge tables. Here members follow each other and write posts:

```sql
CREATE VIEW posts AS SELECT * FROM (VALUES (100, 'hello'), (101, 'graphs')) t(pid, title);
CREATE VIEW wrote AS SELECT * FROM (VALUES (0, 100), (33, 101)) t(author, post);
CREATE PROPERTY GRAPH club
  VERTEX TABLES (
    members AS person LABEL person PROPERTIES (id, club),
    posts
  )
  EDGE TABLES (
    ties SOURCE KEY (src) REFERENCES person (id)
         DESTINATION KEY (dst) REFERENCES person (id) LABEL knows,
    wrote SOURCE KEY (author) REFERENCES person (id)
          DESTINATION KEY (post) REFERENCES posts (pid)
  );
```

Neither vertex table names its `KEY`: each is the column the edges reference. A graph of several tables has no `nodes` and `edges` tables; its tables are queried by their own names.

**An algorithm runs on one vertex table and one edge table**, as in DuckPGQ. With one edge table, as in `karate`, the pair is implied. Otherwise name it after the graph, by table name or label:

```sql
SELECT id, round(pagerank, 4) AS pagerank
FROM pagerank(club, person, knows, direction => 'both')
ORDER BY pagerank DESC
LIMIT 3;
```

```text
┌───────┬──────────┐
│  id   │ pagerank │
│ Int64 │ Float64  │
├───────┼──────────┤
│    33 │   0.1009 │
│     0 │    0.097 │
│    32 │   0.0717 │
└───────┴──────────┘
```

The edge table must connect one vertex table to itself: `wrote`, from members to posts, is not a graph an algorithm can run on.

Keys of more than one column, and properties that are expressions rather than columns, are not supported yet. Each is refused with a message, never read as something else:

```sql
CREATE PROPERTY GRAPH pairs
  EDGE TABLES (ties SOURCE KEY (src, dst) DESTINATION KEY (dst));
```

```text
Error: multi-column keys are not supported yet at Line: 2, Column: 36; KEY, SOURCE KEY, DESTINATION KEY and REFERENCES take one column
```

## DROP PROPERTY GRAPH

```sql
DROP PROPERTY GRAPH first;
DROP PROPERTY GRAPH IF EXISTS first;
```

Dropping a graph leaves its tables and views alone. `PROPERTY` may be left out: `DROP GRAPH first`. There is no `ALTER PROPERTY GRAPH`; to change a graph, write its whole new definition with `CREATE OR REPLACE PROPERTY GRAPH`.

## SHOW GRAPHS

Lists the graphs, one row per edge table, with its endpoint columns, the vertex table and key its endpoints refer to, and whether the graph can be used:

```sql
SHOW GRAPHS;
```

```text
┌────────┬────────────┬────────┬─────────────┬──────────────┬────────────┬────────┬────────┐
│  name  │ edge_table │ source │ destination │ vertex_table │ vertex_key │ status │ reason │
│  Utf8  │    Utf8    │  Utf8  │    Utf8     │     Utf8     │    Utf8    │  Utf8  │  Utf8  │
├────────┼────────────┼────────┼─────────────┼──────────────┼────────────┼────────┼────────┤
│ club   │ ties       │ src    │ dst         │ members      │ id         │ ok     │ NULL   │
│ club   │ wrote      │ author │ post        │ members      │ id         │ ok     │ NULL   │
│ karate │ ties       │ src    │ dst         │ members      │ id         │ ok     │ NULL   │
└────────┴────────────┴────────┴─────────────┴──────────────┴────────────┴────────┴────────┘
```

`status` is `ok` for a graph you can use. A graph of the [database](/docs/database/database-file/) that could not be defined when the database was opened, because a file it reads was moved or deleted, is listed too, from its stored definition, with `status` `unavailable` and `reason` saying why and how to repair it: what the notice said when the database opened, and what using the graph says. A vertex key the definition leaves out is NULL for such a graph, since finding it needs the table. See [when a file is missing](/docs/database/database-file/#when-a-file-is-missing).

## DESCRIBE GRAPH

Lists the columns of a graph's two tables, `nodes` and `edges`: each column's type, its role in the graph, and the column it comes from. For a graph of several tables, it lists each table's columns instead. `DESCRIBE PROPERTY GRAPH`, DuckPGQ's spelling, works too:

```sql
DESCRIBE GRAPH karate;
```

```text
┌────────────┬─────────────┬───────────┬─────────────┬──────────────┐
│ table_name │ column_name │ data_type │    role     │    origin    │
│    Utf8    │    Utf8     │   Utf8    │    Utf8     │     Utf8     │
├────────────┼─────────────┼───────────┼─────────────┼──────────────┤
│ nodes      │ id          │ Int64     │ node id     │ members.id   │
│ nodes      │ club        │ Utf8      │ NULL        │ members.club │
│ edges      │ src         │ Int64     │ source      │ ties.src     │
│ edges      │ dst         │ Int64     │ destination │ ties.dst     │
└────────────┴─────────────┴───────────┴─────────────┴──────────────┘
```
