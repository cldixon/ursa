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

Because a view stores the statement, a view over a CSV file parses the whole file again each time a query reads it. For a large vertex table, whose attributes many queries read, convert the file to Parquet once and define the view over the Parquet file. Parquet is much faster to read, and a query reads only the columns it uses:

```sql
COPY (FROM 'karate_nodes.csv') TO 'karate_nodes.parquet';
CREATE VIEW members AS SELECT * FROM 'karate_nodes.parquet';
```

A compressed CSV converts the same way: `COPY (FROM 'users.csv.gz') TO 'users.parquet'`. A graph's edges are read into its [stored index](#stored-graph-indexes), so they are read again only when their files change.

## Paths

A relative path resolves against the current directory when a statement runs, as in DuckDB. When Ursa stores a definition, it rewrites each relative path to be relative to the database file's directory. So the catalog finds the same files from any directory, and you can move a database and its data together. A path outside the database's directory is stored as an absolute path. URLs are stored as written.

## Stored graph indexes

The first query over a graph builds its index: the graph's edges, laid out for the algorithms. Ursa then writes the index next to the database, in the background. Later queries, in this process or another, load it instead of building it again.

An index is stored under a key made from two things: the plan that produces the edges, and the identity of every file it reads (its location, size and modification time, and the ETag where the object store has one). So:

- if a source file changes, the key changes, and Ursa builds a new index. The old one is never used again;
- a weight is not part of the key, so trying several weightings reuses one index;
- every graph function in a query shares one index, and so does `algorithms()`;
- the key is portable. A path inside the database's directory is part of the key relative to that directory, as the catalog stores it, so a database moved together with its data still finds its indexes. Only the file options that change what is read are part of it (the format, the CSV parsing options, partition columns), so a change to an unrelated setting, or a DataFusion upgrade, does not throw the indexes away.

Some algorithms, such as PageRank, in-degree and anything with `direction => 'both'`, also need the reverse of the index: each node's incoming edges. The first query that builds it adds it to the stored index when the database closes. A later process reads it from disk only when an algorithm asks for it, so a query that does not need it does not pay to load it.

Evaluated weights are stored with the index too, one set per weight expression, when the database closes. A later weighted query with the same expression over the same files reads them instead of reading the edges again. A changed expression or source file never uses stored weights: it reads the edges and evaluates them afresh. Stored weights count toward the storage limit, and they are evicted with their index.

Edges that are not read from files, such as an in-memory table, have no stable key. Their index is built once per session and not stored.

To see whether a process built or loaded its indexes, set `URSA_INDEX_STATS=1`. `ursa` prints the counts when it exits:

```bash
URSA_INDEX_STATS=1 ursa club.ursa -c "SELECT * FROM pagerank(karate) LIMIT 1"
```

```text
ursa: graph indexes built 0, loaded 1
```

### The storage limit

A database keeps at most `ursa.index_storage_limit` of stored indexes. The default is 4 GiB. Set it with `SET`:

```sql
SET ursa.index_storage_limit = '8GiB';
SHOW ursa.index_storage_limit;
```

```text
┌──────────────────────────┬───────┐
│           name           │ value │
│           Utf8           │ Utf8  │
├──────────────────────────┼───────┤
│ ursa.index_storage_limit │ 8GiB  │
└──────────────────────────┴───────┘
```

The limit belongs to the database. In a writable database, `SET` stores it in the catalog, every later process obeys it, and `SHOW` shows it. `RESET ursa.index_storage_limit` goes back to the default. In a read-only session, or one without a database, `SET` lasts for the session, and it never removes anything.

The limit is applied only when an index is stored, never when you change it:

- an index bigger than the whole limit is not stored. Ursa says so, and how large a limit would keep it;
- otherwise Ursa evicts stored indexes, least recently used first, until the new one fits, and says which. It evicts only as many as it has to, and never one a running query uses. If the indexes in use leave no room, nothing is evicted and the new index is not stored.

The reverse adjacency and stored weights, added when the database closes, make room in the same way.

For a large graph, set the limit before the first query, so its index is kept: the index of a 100M-edge graph takes a few GiB.

### Notices

Some of this work takes a while, or loses work a later process does again. Ursa says so on stderr, one line each, so stdout holds only results:

```text
ursa: building the index for bsky (95.7M edges)…
ursa: built the index for bsky (95.7M edges) in 30.7 s; stored in club.ursa (1.4 GiB)
ursa: built the index for big (812.0M edges) in 4 min 5 s; not stored: it takes 12.1 GiB, more than ursa.index_storage_limit (4GiB); SET ursa.index_storage_limit = '32GiB' to keep it
ursa: evicted the stored index for old (1.4 GiB, the least recently used) from club.ursa to stay within ursa.index_storage_limit (4GiB)
ursa: built the index for bsky (95.7M edges) in 30.7 s; not stored: club.ursa is open read-only
```

Which notices are printed is `ursa.notices`:

| Value | Prints |
|---|---|
| `on` (the default) | an index build that takes more than a second (when it starts, and when it is done); every index that is not stored or is evicted; a failed write |
| `all` | also every build, load and store, however quick |
| `off` | nothing |

`ursa --quiet` (`-q`) prints no notices; errors are still printed. `ursa.notices` lasts for the session and is not stored in the database.

## Read-only and sharing

One process at a time may have a database open for writing. A second process waits two seconds, then fails, naming the process that has it:

```text
Error: club.ursa is in use by another process (process 4195), which has it open for writing; open it read-only to query it meanwhile (ursa --readonly club.ursa), or wait for that process to finish
```

`--readonly` opens a database without taking that lock. It reads the catalog and the stored indexes, and refuses any statement that defines or drops something:

```bash
ursa club.ursa --readonly -c "SELECT * FROM degree(karate) LIMIT 3"
```

A read-only process uses the stored indexes, but stores none: an index it builds is built again by the next process. A build that takes more than a second says so (`not stored: club.ursa is open read-only`). Run the query once without `--readonly` to store it.

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
  ],
  "settings": {
    "ursa.index_storage_limit": "32GiB"
  }
}
```

`settings` appears once you `SET ursa.index_storage_limit`. Versions of ursa before 0.8 ignore it, and drop it if they rewrite the catalog.

Copy or move `club.ursa` together with `club.ursa.d/`, and, to keep the stored indexes, the data files inside its directory with them. Without `club.ursa.d/`, the catalog still opens, and the indexes are built again. A damaged or out-of-date index file is treated as missing, never as an error. Indexes stored by ursa 0.7 or earlier use the old key: the first writer to open the database with 0.8 removes them, says so, and builds each again when it is first used.
