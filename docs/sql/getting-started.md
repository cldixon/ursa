# Getting started with Ursa SQL

Ursa is a SQL engine for graph analytics. It reads Parquet and CSV files where they are, on your disk or in object storage, and runs graph algorithms over them with SQL. There is no server and nothing to load first. An edge table is a graph.

## Install

Install with Cargo. You need Rust 1.94 or later:

```bash
cargo install ursa-cli
```

Or download a prebuilt binary for Linux, macOS or Windows from the [releases page](https://github.com/cldixon/ursa/releases). Each release has one archive per platform:

| Platform              | Archive                                              |
| --------------------- | ---------------------------------------------------- |
| Linux x86_64          | `ursa-cli-v0.6.0-x86_64-unknown-linux-gnu.tar.gz`    |
| Linux aarch64         | `ursa-cli-v0.6.0-aarch64-unknown-linux-gnu.tar.gz`   |
| macOS (Intel)         | `ursa-cli-v0.6.0-x86_64-apple-darwin.tar.gz`         |
| macOS (Apple silicon) | `ursa-cli-v0.6.0-aarch64-apple-darwin.tar.gz`        |
| Windows x64           | `ursa-cli-v0.6.0-x86_64-pc-windows-msvc.zip`         |

Each archive has a `.sha256` file next to it. For example, on an Apple silicon Mac:

```bash
curl -LO https://github.com/cldixon/ursa/releases/download/ursa-cli-v0.6.0/ursa-cli-v0.6.0-aarch64-apple-darwin.tar.gz
tar xzf ursa-cli-v0.6.0-aarch64-apple-darwin.tar.gz
sudo mv ursa-cli-v0.6.0-aarch64-apple-darwin/ursa /usr/local/bin/
```

The binary is called `ursa`. Check it runs:

```bash
ursa --version
```

```text
ursa 0.6.0
```

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
+-----+-----+
| src | dst |
+-----+-----+
| 0   | 1   |
| 0   | 2   |
| 0   | 3   |
+-----+-----+
```

The format comes from the extension: `.parquet`, `.csv`, and compressed CSV (`.csv.gz`, `.csv.zst`). A directory or a glob reads every file in it: `'edges/*.parquet'`.

## Open a database

Start the shell with a database file:

```bash
ursa club.ursa
```

The file is created if it does not exist. It keeps the tables, views and graphs you define, and the graph indexes Ursa builds, so the next session (or the next `ursa -c` against the same file) starts where you left off. Without a file name, `ursa` runs in memory and forgets everything when it exits. [The database file](database.md) has the details.

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

Every row of `ties` is an edge from `src` to `dst`. Every row of `members` is a node, keyed by `id`.

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
+----+--------+
| id | club   |
+----+--------+
| 0  | Mr. Hi |
| 1  | Mr. Hi |
| 2  | Mr. Hi |
+----+--------+
```

## Run an algorithm

An algorithm is a table function. Its first argument is the graph. It returns one row per node: the node's `id`, and a column named after the algorithm.

```sql
SELECT id, round(pagerank, 4) AS pagerank
FROM pagerank(karate, direction => 'both')
ORDER BY pagerank DESC
LIMIT 5;
```

```text
+----+----------+
| id | pagerank |
+----+----------+
| 33 | 0.1009   |
| 0  | 0.097    |
| 32 | 0.0717   |
| 2  | 0.0571   |
| 1  | 0.0529   |
+----+----------+
```

Parameters are named, with `=>`:

```sql
SELECT * FROM louvain(karate, seed => 1);
SELECT * FROM pagerank(karate, damping => 0.9, direction => 'both');
```

## Combine algorithms and attributes

`algorithms` runs several algorithms over one graph and returns one row per node with a column for each. Name a column with `name =>`; otherwise it is named after the algorithm. Join it to the node table on `id`. This query finds the five best-connected members, with their club, how many triangles they are part of, and their community:

```sql
SELECT id, club, degree, triangles, community
FROM karate.nodes
JOIN algorithms(karate,
                degree(direction => 'both'),
                triangles => triangle_count(),
                community => louvain(seed => 1)) USING (id)
ORDER BY degree DESC
LIMIT 5;
```

```text
+----+---------+--------+-----------+-----------+
| id | club    | degree | triangles | community |
+----+---------+--------+-----------+-----------+
| 33 | Officer | 17     | 15        | 2         |
| 0  | Mr. Hi  | 16     | 18        | 0         |
| 32 | Officer | 12     | 13        | 2         |
| 2  | Mr. Hi  | 10     | 11        | 0         |
| 1  | Mr. Hi  | 9      | 12        | 0         |
+----+---------+--------+-----------+-----------+
```

Each algorithm also works on its own, as in the previous section, and those results join on `id` too.

All three algorithms run over one index of the graph. Ursa builds it the first time a query needs it and stores it in `club.ursa`. The next query, in this session or a later one, uses the stored index. If `karate_edges.csv` changes, Ursa notices and builds a new one.

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
+----------+----------------------+
| id       | pagerank             |
+----------+----------------------+
| Valjean  | 0.09956068175644767  |
| Marius   | 0.051667656904091475 |
| Myriel   | 0.03923416880292588  |
| Cosette  | 0.03691018363299517  |
| Enjolras | 0.036614491459848165 |
+----------+----------------------+
```

The weight can be any expression over the edge columns:

```sql
SELECT * FROM louvain(lesmis, weight => log10(1 + weight), seed => 7)
ORDER BY id
LIMIT 5;
```

```text
+------------+---------+
| id         | louvain |
+------------+---------+
| Anzelma    | 3       |
| Babet      | 3       |
| Bahorel    | 5       |
| Bamatabois | 1       |
| BaronessT  | 4       |
+------------+---------+
```

Weights must be non-negative and not null. The graph index does not depend on the weight, so trying several weightings reuses one index.

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
+--------+
| groups |
+--------+
| 3      |
+--------+
```

The nodes of a subgraph are the endpoints of the edges it keeps. A character whose every tie is weaker than 3 is not in `lesmis_strong`.

## Traversals

Traversals are table functions too, and they return edges. `hop` returns the edges reached within `n` steps of a set of seed nodes; the seeds can be a list or a query. Member 0 has 16 friends, so this returns 16 rows:

```sql
SELECT * FROM hop(karate, seeds => [0], n => 1, direction => 'both');

SELECT * FROM hop(karate,
                  seeds => (SELECT id FROM karate.nodes WHERE club = 'Officer'),
                  n => 2);
```

The second query returns the 39 edges reachable within two steps of an Officer member, following edges in their stored direction. The seeds query runs as part of the same plan, so a filter in it is pushed into the scan of `karate_nodes.csv`. A seed that is not in the graph reaches nothing.

`random_walk` returns walks as rows: one per step, with the walk's number and position. Give it a `seed` for walks you can reproduce:

```sql
SELECT * FROM random_walk(karate, start => [0, 1], steps => 4, walks_per_node => 2, seed => 7);
```

`shortest_path` returns the path between two nodes, one edge per row, with its step number and the cost so far:

```sql
SELECT * FROM shortest_path(karate, source => 0, target => 33, direction => 'both');
```

```text
+-----+-----+-----+------+
| src | dst | hop | cost |
+-----+-----+-----+------+
| 0   | 8   | 0   | 1.0  |
| 8   | 33  | 1   | 2.0  |
+-----+-----+-----+------+
```

## Files in object storage

Anything that works with a local path works with a URL. Credentials come from the standard environment variables (`AWS_*`, `GOOGLE_*`, `AZURE_*`) or the usual config files:

<!-- skip -->
```sql
CREATE VIEW follows AS
  SELECT * FROM 's3://my-bucket/social/follows/*.parquet';

CREATE PROPERTY GRAPH social
  EDGE TABLES (follows SOURCE KEY (follower) DESTINATION KEY (followee));

SELECT * FROM pagerank(social) ORDER BY pagerank DESC LIMIT 10;
```

`gs://`, `az://` and `https://` work the same way. Ursa reads only the columns a query needs, and stores the graph index in the database file, so the second query does not read the edges again.

## Output

The shell prints tables. From the command line, choose a format with `-f`:

```bash
ursa club.ursa -f json -c "SELECT * FROM pagerank(karate) ORDER BY pagerank DESC LIMIT 3"
ursa club.ursa -f csv  -c "SELECT * FROM louvain(karate, seed => 1)" > communities.csv
```

The formats are `table` (the default), `csv`, `tsv`, `json`, `ndjson` and `arrow`. To write a file from SQL, use `COPY`:

```sql
COPY (SELECT * FROM pagerank(karate)) TO 'pagerank.parquet';
```

## Find out what is available

```sql
SHOW GRAPHS;
DESCRIBE GRAPH karate;
SELECT * FROM ursa_functions();
```

`ursa_functions()` lists every graph function with its parameters, their defaults, the NetworkX function it matches, and a one-line description.

## Next steps

- The [function reference](reference/functions.md) lists every function and exactly what it computes.
- [Statements](statements.md) covers defining, describing and dropping graphs, `ATTACH` and the settings.
- [The shell](shell.md) covers dot-commands, init files and history.
- The [Python library](../index.md) runs the same algorithms with a DataFrame API, and gives the same values.
