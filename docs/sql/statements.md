# Statements

Ursa runs the SQL that [DataFusion](https://datafusion.apache.org/user-guide/sql/index.html) runs: `SELECT`, joins, aggregates, window functions, `CREATE VIEW`, `CREATE EXTERNAL TABLE`, `COPY … TO`, `EXPLAIN`, `SET` and `SHOW`. This page covers what Ursa adds: reading files, and statements for graphs, databases and settings.

The examples on this page share one session. They start with two views over the karate club files:

```sql
CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv';
CREATE VIEW ties    AS SELECT * FROM 'karate_edges.csv';
```

## Reading and writing files

!!! info "New in 0.7"

    `.jsonl` and `.ndjson` files, `read_parquet`, `read_csv` and `read_json`, and `SHOW TABLES` without `information_schema` are new in `ursa-cli` 0.7. 0.6.0 reads `.parquet`, `.csv`, `.json` and `.arrow` files by path.

A file is a table: name it in `FROM`, with its path or URL in quotes. A relative path is relative to the current directory. The format comes from the extension:

| Extension                        | Format                                    |
| -------------------------------- | ----------------------------------------- |
| `.parquet`                       | Parquet                                   |
| `.csv`                           | CSV, with a header line                   |
| `.json`, `.jsonl`, `.ndjson`     | JSON, one object per line (NDJSON)        |
| `.arrow`                         | Arrow IPC                                 |

CSV and JSON files may be compressed: `.csv.gz`, `.jsonl.zst`, and `.bz2` and `.xz` too. A directory or a glob (`'edges/*.parquet'`) reads every file in it with the extension of the first, and a `year=2024/` directory becomes a `year` column. `s3://`, `gs://`, `az://` and `https://` URLs work the same way.

`COPY … TO` writes a file, in the format its extension names, and prints how many rows it wrote:

```sql
COPY (SELECT * FROM ties) TO 'ties.jsonl';
```

```text
┌────────┐
│ count  │
│ uint64 │
├────────┤
│     78 │
└────────┘
```

```sql
SELECT count(*) AS ties FROM 'ties.jsonl';
```

```text
┌───────┐
│ ties  │
│ int64 │
├───────┤
│    78 │
└───────┘
```

`STORED AS CSV` (or `PARQUET`, `JSON`, `ARROW`) names the format when the extension does not.

!!! tip "Convert large CSV files to Parquet once"

    A view over a file reads the file each time a query uses it. For a CSV or JSON file, that means parsing all of it again, so a query that reads a large vertex table's attributes (a `JOIN` on its rows, `neighbors_agg`'s `attribute`, a `GRAPH_TABLE` property) pays for that every time. Parquet is much faster to read, and a query reads only the columns it needs. Convert the file once and define the view over the Parquet file:

    ```sql
    COPY (FROM 'karate_nodes.csv') TO 'karate_nodes.parquet';
    CREATE VIEW member_attributes AS SELECT * FROM 'karate_nodes.parquet';
    ```

    This works for a compressed file too: `COPY (FROM 'users.csv.gz') TO 'users.parquet'`. A graph's edges are read once, into its [stored index](database.md#stored-graph-indexes); the attributes of its vertex table are read from the file by each query that uses them.

### read_parquet, read_csv and read_json

`read_parquet`, `read_csv` and `read_json` read a file as `FROM 'path'` does, but in the format they name, whatever the extension, and with options. They take a path, a glob, a directory, a URL, or a list of them:

```sql
COPY (SELECT * FROM ties) TO 'ties.txt' STORED AS CSV OPTIONS ('format.delimiter' '|');
SELECT count(*) AS ties FROM read_csv('ties.txt', delim => '|');
COPY (SELECT * FROM ties) TO 'ties.json';
SELECT count(*) AS both_files FROM read_json(['ties.jsonl', 'ties.json']);
```

```text
┌────────┐
│ count  │
│ uint64 │
├────────┤
│     78 │
└────────┘
┌───────┐
│ ties  │
│ int64 │
├───────┤
│    78 │
└───────┘
┌────────┐
│ count  │
│ uint64 │
├────────┤
│     78 │
└────────┘
┌────────────┐
│ both_files │
│   int64    │
├────────────┤
│        156 │
└────────────┘
```

| Function       | Options                                                                                                   |
| -------------- | --------------------------------------------------------------------------------------------------------- |
| `read_parquet` | none                                                                                                      |
| `read_csv`     | `header` (`true` or `false`), `delim` (one character; `sep` and `delimiter` are other names), `quote`, `compression` |
| `read_json`    | `compression`; `format`: `'newline_delimited'` (the default) or `'array'`, for a file that holds one JSON array of objects |

`compression` is `'auto'` (from the extension, the default), `'none'`, `'gzip'`, `'bzip2'`, `'xz'` or `'zstd'`. Options are named with `=>`, or with `=` as DuckDB writes them: `read_csv('ties.txt', header = false)`. `read_csv_auto` and `read_json_auto` are other names for `read_csv` and `read_json`, and `read_ndjson` is `read_json` for one object per line. A view over a `read_*` call, in a [database](database.md), keeps its paths relative to the database file, as a view over `'path'` does.

## CREATE PROPERTY GRAPH

!!! info "New in 0.7"

    0.6.0 accepts one vertex table with a `KEY` and one edge table. Several tables, optional `KEY`, `AS` names, `LABEL` and `PROPERTIES` (DuckPGQ's syntax included) are new in `ursa-cli` 0.7.

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

- **An edge table** is any table or view. Each row is an edge from its `SOURCE KEY` column to its `DESTINATION KEY` column. The two columns must have the same type: integers or strings.
- **A vertex table** defines the nodes and gives them attributes, keyed by its `KEY` column. `KEY` can be left out: the key is then the table's primary key, or else the column the edge tables' `REFERENCES` name. Ursa adds one relaxation to the standard: vertex tables are optional, and without one the nodes are the endpoints of the edges.
- **The node set** (new in 0.7; in 0.6.0 the algorithms see only the edges' endpoints). With a vertex table, every key is a node, one with no edges included, and every algorithm, traversal and pattern runs on those nodes, in the table's order. An edge whose source or destination is not a key is left out, as DuckPGQ leaves it out; `describe` counts such edges in `n_dropped_edges`. A NULL key is no node, and a key given twice is one node.
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
│ int64 │
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

!!! info "New in 0.7"

    Graphs of several tables are new in `ursa-cli` 0.7.

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
│ int64 │  double  │
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

Dropping a graph leaves its tables and views alone.

## SHOW GRAPHS

Lists the graphs, one row per edge table, with its endpoint columns, and the vertex table and key its endpoints refer to:

```sql
SHOW GRAPHS;
```

```text
┌─────────┬────────────┬─────────┬─────────────┬──────────────┬────────────┐
│  name   │ edge_table │ source  │ destination │ vertex_table │ vertex_key │
│ varchar │  varchar   │ varchar │   varchar   │   varchar    │  varchar   │
├─────────┼────────────┼─────────┼─────────────┼──────────────┼────────────┤
│ club    │ ties       │ src     │ dst         │ members      │ id         │
│ club    │ wrote      │ author  │ post        │ members      │ id         │
│ karate  │ ties       │ src     │ dst         │ members      │ id         │
└─────────┴────────────┴─────────┴─────────────┴──────────────┴────────────┘
```

## DESCRIBE GRAPH

Lists the columns of a graph's two tables, `nodes` and `edges`: each column's type, its role in the graph, and the column it comes from. For a graph of several tables, it lists each table's columns instead. `DESCRIBE PROPERTY GRAPH`, DuckPGQ's spelling, works too:

```sql
DESCRIBE GRAPH karate;
```

```text
┌────────────┬─────────────┬───────────┬─────────────┬──────────────┐
│ table_name │ column_name │ data_type │    role     │    origin    │
│  varchar   │   varchar   │  varchar  │   varchar   │   varchar    │
├────────────┼─────────────┼───────────┼─────────────┼──────────────┤
│ nodes      │ id          │ Int64     │ node id     │ members.id   │
│ nodes      │ club        │ Utf8      │ NULL        │ members.club │
│ edges      │ src         │ Int64     │ source      │ ties.src     │
│ edges      │ dst         │ Int64     │ destination │ ties.dst     │
└────────────┴─────────────┴───────────┴─────────────┴──────────────┘
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
┌──────────────────────────┬─────────┐
│           name           │  value  │
│         varchar          │ varchar │
├──────────────────────────┼─────────┤
│ ursa.index_storage_limit │ 8GiB    │
└──────────────────────────┴─────────┘
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
┌───────┬────────┐
│  id   │ degree │
│ int64 │ uint32 │
├───────┼────────┤
│    33 │     17 │
└───────┴────────┘
```

`SELECT * FROM ursa_functions()` lists them all, and the [function reference](reference/functions.md) describes each one.
