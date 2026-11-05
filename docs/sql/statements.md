# Statements

Ursa runs the SQL that [DataFusion](https://datafusion.apache.org/user-guide/sql/index.html) runs: `SELECT`, joins, aggregates, window functions, `CREATE VIEW`, `CREATE EXTERNAL TABLE`, `COPY … TO`, `EXPLAIN`, `SET` and `SHOW`. This page covers the statements Ursa adds, for graphs, databases and settings.

The examples on this page share one session. They start with two views over the karate club files:

```sql
CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv';
CREATE VIEW ties    AS SELECT * FROM 'karate_edges.csv';
```

## CREATE PROPERTY GRAPH

Defines a graph over an edge table, and optionally a vertex table. The syntax is a subset of SQL/PGQ, the property graph part of the SQL standard:

```text
CREATE [OR REPLACE] PROPERTY GRAPH [IF NOT EXISTS] name
  [VERTEX TABLES ( table KEY (column) )]
  EDGE TABLES (
    table SOURCE KEY (column) [REFERENCES vertex_table (column)]
          DESTINATION KEY (column) [REFERENCES vertex_table (column)]
  )
```

```sql
CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (
    ties SOURCE KEY (src) REFERENCES members (id)
         DESTINATION KEY (dst) REFERENCES members (id)
  );
```

- **The edge table** is any table or view. Each row is an edge from its `SOURCE KEY` column to its `DESTINATION KEY` column. The two columns must have the same type: integers or strings.
- **The vertex table** is optional. It gives the nodes attributes, keyed by its `KEY` column. The graph's nodes are the vertex table's keys plus the endpoints of the edges. Without a vertex table, the nodes are the endpoints of the edges.
- **`REFERENCES`** names the vertex table and its key. It is optional, and when given it must match `VERTEX TABLES`.
- `NODE TABLES` and `RELATIONSHIP TABLES`, the standard's synonyms, work too.

A graph is a schema with two tables. `karate.edges` holds the edge table's rows. `karate.nodes` holds one row per node, with the vertex table's columns:

```sql
SELECT count(*) AS edges FROM karate.edges;
```

```text
+-------+
| edges |
+-------+
| 78    |
+-------+
```

A graph over a view that filters edges is a subgraph. Its nodes are the endpoints of the edges it keeps:

```sql
CREATE VIEW first_ties AS SELECT * FROM ties WHERE src < 10;
CREATE PROPERTY GRAPH first EDGE TABLES (first_ties SOURCE KEY (src) DESTINATION KEY (dst));
```

`OR REPLACE` replaces a graph of the same name. `IF NOT EXISTS` does nothing if there is one. Defining a graph that exists without either is an error.

SQL/PGQ allows more than Ursa supports so far: several vertex or edge tables, `LABEL`, `PROPERTIES`, `AS` aliases, an edge table `KEY`, and keys of more than one column. Each is recognized and refused with a message, never read as something else:

```sql
CREATE PROPERTY GRAPH labelled
  EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst) LABEL knows);
```

```text
Error: labels (LABEL, DEFAULT LABEL) are not supported yet: Ursa graphs have one edge table and at most one vertex table at Line: 2, Column: 60
```

## DROP PROPERTY GRAPH

```sql
DROP PROPERTY GRAPH first;
DROP PROPERTY GRAPH IF EXISTS first;
```

Dropping a graph leaves its tables and views alone.

## SHOW GRAPHS

Lists the graphs, one row each, with the edge table, its endpoint columns, and the vertex table and key if there is one:

```sql
SHOW GRAPHS;
```

```text
+--------+------------+--------+-------------+--------------+------------+
| name   | edge_table | source | destination | vertex_table | vertex_key |
+--------+------------+--------+-------------+--------------+------------+
| karate | ties       | src    | dst         | members      | id         |
+--------+------------+--------+-------------+--------------+------------+
```

## DESCRIBE GRAPH

Lists the columns of a graph's two tables, `nodes` and `edges`: each column's type, its role in the graph, and the column it comes from:

```sql
DESCRIBE GRAPH karate;
```

```text
+------------+-------------+-----------+-------------+--------------+
| table_name | column_name | data_type | role        | origin       |
+------------+-------------+-----------+-------------+--------------+
| nodes      | id          | Int64     | node id     | members.id   |
| nodes      | club        | Utf8      |             | members.club |
| edges      | src         | Int64     | source      | ties.src     |
| edges      | dst         | Int64     | destination | ties.dst     |
+------------+-------------+-----------+-------------+--------------+
```

## ATTACH

`ATTACH 'file.ursa'` opens a database file in an in-memory session, as if `ursa file.ursa` had opened it. The shell's `.open` runs it.

<!-- skip -->
```sql
ATTACH 'club.ursa';
```

The session must not have a database already, and must not have defined anything yet. `ATTACH DATABASE 'file.ursa'` is the same. An alias (`AS other`) is parsed but not supported yet. [The database file](database.md) explains what the file keeps.

## Settings

Ursa's settings start with `ursa.`. Set one with `SET` and read it with `SHOW`:

```sql
SET ursa.index_storage_limit = '8GiB';
SHOW ursa.index_storage_limit;
```

```text
+--------------------------+-------+
| name                     | value |
+--------------------------+-------+
| ursa.index_storage_limit | 8GiB  |
+--------------------------+-------+
```

| Setting                    | Default | Meaning                                                                                                                                         |
| -------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| `ursa.index_storage_limit` | `4GiB`  | The most space stored graph indexes may take in a database. Bytes, or a number with `KiB`, `MiB`, `GiB` or `TiB`. See [the database file](database.md#the-storage-limit). |

`SHOW ursa.index_storage_limit VERBOSE` adds a description. `SHOW ALL` lists every setting, DataFusion's and Ursa's. DataFusion's settings, such as `datafusion.execution.target_partitions`, work as in DataFusion. A setting lasts for the session; the database file does not keep it.

## Graph functions

The algorithms, statistics and traversals are table functions. They go in `FROM`, take the graph as the first argument and named parameters with `=>`:

```sql
SELECT * FROM degree(karate, direction => 'both') ORDER BY degree DESC LIMIT 1;
```

```text
+----+--------+
| id | degree |
+----+--------+
| 33 | 17     |
+----+--------+
```

`SELECT * FROM ursa_functions()` lists them all, and the [function reference](reference/functions.md) describes each one.
