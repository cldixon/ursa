# The database file

A `.ursa` file is a database. It keeps two things between sessions: the **catalog**, the views, external tables and graphs you define, and the **graph indexes** Ursa builds from them. A later process opens the file and starts where the last one stopped. It does not rebuild an index it already has.

## Open a database

Name the file when you start `ursa`:

```bash
ursa club.ursa                          # the shell
ursa club.ursa -c "SHOW GRAPHS"         # one command
```

The file is created if it does not exist. Its name must end in `.ursa`. Without a file, `ursa` runs in memory and keeps nothing when it exits.

In an in-memory session that has not defined anything yet, `ATTACH` opens a database for the rest of the session. The shell's `.open` does the same:

<!-- skip -->
```sql
ATTACH 'club.ursa';
```

Only one database can be open in a session.

## What it keeps

Every view, external table and graph you define is kept, once its statement succeeds:

```sql
CREATE VIEW ties AS SELECT * FROM 'karate_edges.csv';
CREATE PROPERTY GRAPH karate
  EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));
```

The file stores the statements, not the data. Opening the database runs them again, so a view over a file always reads the file's current contents. `DROP` removes an object's statement. `CREATE OR REPLACE` replaces it.

The catalog does not store rows. So statements that create tables with data, or change them, are refused in a database session:

```sql
CREATE TABLE scores AS SELECT * FROM pagerank(karate);
```

```text
Error: CREATE TABLE … AS is not supported in a database file: version 1 of the .ursa file stores views, external tables and graphs, not tables with data. Define a view instead (CREATE VIEW scores AS SELECT …), or write the rows to a file with COPY (SELECT …) TO 'scores.parquet' and define a view over it (CREATE VIEW scores AS SELECT * FROM 'scores.parquet')
```

Write the rows to a file and define a view over it instead:

```sql
COPY (SELECT * FROM pagerank(karate)) TO 'scores.parquet';
CREATE VIEW scores AS SELECT * FROM 'scores.parquet';
```

The same goes for `INSERT`, `UPDATE`, `DELETE`, `CREATE SCHEMA`, `CREATE FUNCTION` and `ALTER`. `TEMPORARY` views and tables run, but are not kept.

## Paths

A relative path resolves against the current directory when a statement runs, as in DuckDB. When Ursa stores a definition, it rewrites each relative path to be relative to the database file's directory. So the catalog finds the same files from any directory, and you can move a database and its data together. A path outside the database's directory is stored as an absolute path. URLs are stored as written.

## Stored graph indexes

The first query over a graph builds its index: the graph's edges, laid out for the algorithms. Ursa then writes the index next to the database, in the background. Later queries, in this process or another, load it instead of building it again.

An index is stored under a key made from two things: the plan that produces the edges, and the identity of every file it reads (its location, size and modification time, and the ETag where the object store has one). So:

- if a source file changes, the key changes, and Ursa builds a new index. The old one is never used again;
- a weight is not part of the key, so trying several weightings reuses one index;
- every graph function in a query shares one index, and so does `algorithms()`.

Edges that are not read from files, such as an in-memory table, have no stable key. Their index is built once per session and not stored.

To see whether a process built or loaded its indexes, set `URSA_INDEX_STATS=1`. `ursa` prints the counts when it exits:

```bash
URSA_INDEX_STATS=1 ursa club.ursa -c "SELECT * FROM pagerank(karate) LIMIT 1"
```

```text
ursa: graph indexes built 0, loaded 1
```

### The storage limit

Stored indexes are evicted least recently used first once their total size passes a limit. The default is 4 GiB. Change it for the session with `SET`:

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

The setting lasts for the session. It is not kept in the database. Eviction never removes the index just written, or one a running query uses. An index bigger than the whole limit is not stored.

## Read-only and sharing

One process at a time may have a database open for writing. A second process waits two seconds, then fails with `club.ursa is in use by another process`.

`--readonly` opens a database without taking that lock. It reads the catalog and the stored indexes, and refuses any statement that defines or drops something:

```bash
ursa club.ursa --readonly -c "SELECT * FROM degree(karate) LIMIT 3"
```

## On disk

```text
club.ursa          the catalog: a small JSON file of statements
club.ursa.d/       created on first use
  lock             held while a process has the database open
  indexes/
    manifest.json  one entry per stored index: its key, file, size and last use
    <hash>.idx     one stored graph index per file
```

The catalog is plain JSON, so you can read it and diff it:

```json
{
  "format": "ursa",
  "version": 1,
  "statements": [
    "CREATE VIEW ties AS SELECT * FROM 'karate_edges.csv'",
    "CREATE PROPERTY GRAPH karate EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst))"
  ]
}
```

Copy or move `club.ursa` together with `club.ursa.d/`. Without the directory, the catalog still opens, and the indexes are built again. A damaged or out-of-date index file is treated as missing, never as an error.
