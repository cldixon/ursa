---
title: "Statements"
description: "The SQL statements Ursa runs, and the ones it adds to DataFusion's SQL for files, graphs, databases and settings."
---

Ursa runs the SQL that [DataFusion](https://datafusion.apache.org/user-guide/sql/index.html) runs: `SELECT`, joins, aggregates, window functions, `CREATE VIEW`, `CREATE EXTERNAL TABLE`, `COPY … TO`, `EXPLAIN`, `SET` and `SHOW`. This page covers what Ursa adds: reading files, and statements for graphs, databases and settings.

| Statement | What it does | Page |
| --- | --- | --- |
| `SELECT … FROM 'file.parquet'`, `read_parquet`, `read_csv`, `read_json` | Read a file as a table | [Files](/docs/import-export/files/) |
| `COPY … TO` | Write a query's rows to a file | [Files](/docs/import-export/files/#reading-and-writing-files) |
| `CREATE PROPERTY GRAPH` | Define a graph over vertex and edge tables | [Property graphs](/docs/graphs/property-graphs/) |
| `DROP PROPERTY GRAPH` | Drop a graph | [Property graphs](/docs/graphs/property-graphs/#drop-property-graph) |
| `SHOW GRAPHS` | List the graphs | [Property graphs](/docs/graphs/property-graphs/#show-graphs) |
| `DESCRIBE GRAPH` | List a graph's columns | [Property graphs](/docs/graphs/property-graphs/#describe-graph) |
| `GRAPH_TABLE` | Match patterns of nodes and edges | [Pattern matching](/docs/graphs/pattern-matching/) |
| `ATTACH` | Open a database file | [ATTACH](#attach), below |
| `SET`, `SHOW`, `RESET` | Change and read a setting | [Settings](/docs/database/settings/) |

The examples below share one session, over the karate club graph:

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

## ATTACH

`ATTACH 'file.ursa'` opens a database file in an in-memory session, as if `ursa file.ursa` had opened it. The shell's `.open` runs it.

<!-- skip -->
```sql
ATTACH 'club.ursa';
```

The session must not have a database already, and must not have defined anything yet. `ATTACH DATABASE 'file.ursa'` is the same. An alias (`AS other`) is parsed but not supported yet. [The database file](/docs/database/database-file/) explains what the file keeps.

## Graph functions

The algorithms, statistics and traversals are table functions. They go in `FROM`, take the graph as the first argument and named parameters with `=>`:

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

`SELECT * FROM ursa_functions()` lists them all, and the [function reference](/docs/reference/functions/) describes each one.
