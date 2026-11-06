---
name: ursa-sql
description: Write and run SQL for Ursa, a SQL engine for graph analytics over Parquet and CSV files (the `ursa` command-line tool, crate ursa-cli). Use when a task needs graph algorithms (PageRank, degree, components, triangles, Louvain communities, centrality, shortest paths, k-hop neighbourhoods, random walks) over edge lists in files, when the user mentions Ursa SQL, `ursa`, `.ursa` database files or `CREATE PROPERTY GRAPH`, or when writing SQL that calls Ursa's graph table functions.
---

# Writing Ursa SQL

Ursa is a SQL engine for graphs, built on DataFusion. Files are tables, a graph is an edge table, and algorithms are table functions. You run it with the `ursa` binary.

## Start by looking

Never guess function names, parameters or defaults. Ask the engine:

```sql
SELECT name, family, summary FROM ursa_functions() ORDER BY family, name;
```

`ursa_functions()` has one row per graph function: `name`, `family` (`algorithm`, `statistic`, `traversal`, `combinator`), `summary`, `networkx` (the NetworkX call it matches), `parameters` (a list of `{name, type, default, choices, doc}`), `directions` and `outputs`. Read it as JSON to see the parameters:

```bash
ursa --format json -c "SELECT * FROM ursa_functions() WHERE name = 'pagerank'"
```

In a database that already has graphs, list them with `SHOW GRAPHS;` and inspect each with `DESCRIBE GRAPH name;` before writing queries. Never redefine a graph that exists.

## Running ursa

```bash
ursa -c "SELECT * FROM 'edges.csv' LIMIT 3"            # in memory
ursa club.ursa --format json -c "SELECT * FROM degree(g)"  # a database file, JSON out
ursa club.ursa -f query.sql                             # SQL from a file (--file)
```

- Use `--format json` (an array of objects) or `--format ndjson` whenever you read results yourself. `--format csv` for files. The default `table` is for people.
- Separate statements in one `-c` with `;`. They run in order.
- On error, `ursa` prints `Error: <message>` to stderr and exits with status 1. The message usually names the fix.
- With no `-c`, `--file` or stdin, `ursa` starts an interactive shell and waits for input. Don't start it from an agent.

## Define a graph from files

Name each file as a view, then define the graph over the views:

```sql
CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv';
CREATE VIEW ties AS SELECT * FROM 'karate_edges.csv';

CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (
    ties SOURCE KEY (src) REFERENCES members (id)
         DESTINATION KEY (dst) REFERENCES members (id)
  );
```

- `VERTEX TABLES` is optional. Without it, the nodes are the edges' endpoints: `CREATE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));`
- Paths: `.parquet`, `.csv`, `.csv.gz`, `.csv.zst`, a directory, or a glob (`'edges/*.parquet'`). URLs work: `s3://`, `gs://`, `az://`, `https://`. Credentials come from the environment (`AWS_*`, `GOOGLE_*`, `AZURE_*`), AWS profiles (`AWS_PROFILE`, `~/.aws/`), gcloud's application default credentials, or the machine's role. There is no SQL statement for credentials. For a public S3 bucket, set `AWS_SKIP_SIGNATURE=true`.
- The source and destination columns must have the same type, integer or string. Cast in the view if they don't.
- A graph is a schema: `karate.edges` and `karate.nodes` are tables.
- With `VERTEX TABLES`, the vertex table's keys are the nodes: a node on no edge is still a node (degree 0, a row in every algorithm's result). An edge whose source or destination is not a key is dropped, as in DuckPGQ; `describe(g)` counts them in `n_dropped_edges`. `g.nodes` lists exactly the nodes the algorithms see; `g.edges` is every row of the edge table. (From `ursa` 0.7; in 0.6.0 the algorithms see only the edges' endpoints.)
- A subgraph is a graph over a view that filters the edges. With the parent's `VERTEX TABLES`, it keeps every node (as a Python filtered frame does); without, its nodes are only the endpoints of the kept edges. `describe(g, nodes => 'touched')` counts only the nodes on a kept edge.
- The syntax is SQL/PGQ's, so DuckPGQ graph definitions run as written (from `ursa` 0.7; 0.6.0 takes one vertex table with a `KEY` and one edge table). A graph can have several vertex and edge tables, `AS` names, `LABEL` and `PROPERTIES`. A vertex `KEY` can be left out: it is then the table's primary key, or the column the edges `REFERENCES`. Keys are one column.
- An algorithm runs on one vertex table and one edge table. With one edge table the pair is implied (`pagerank(karate)`); otherwise name it after the graph, as DuckPGQ does: `pagerank(social, person, knows)`. A graph of several tables has no `nodes`/`edges` tables; query its tables by their own names.

`DESCRIBE GRAPH` shows the two tables, their columns and where each comes from:

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

## Calling a graph function

A graph function goes in `FROM`. The first argument is the graph's bare name. Every other argument is named, with `=>`:

```sql
SELECT id, pagerank
FROM pagerank(karate, damping => 0.9, direction => 'both')
ORDER BY pagerank DESC
LIMIT 3;
```

```text
+----+---------------------+
| id | pagerank            |
+----+---------------------+
| 33 | 0.10346168993478702 |
| 0  | 0.0992272932102213  |
| 32 | 0.07331159631922396 |
+----+---------------------+
```

- An algorithm returns one row per node: `id` plus a column named after the function (`pagerank`, `degree`, `louvain`, ...). Rows have no order: always `ORDER BY`.
- A statistic (`density`, `diameter`, `avg_path_length`, `describe`) returns one row.
- `direction` is `'out'` (the default), `'in'` or `'both'`. For an undirected edge list stored once per edge, use `'both'`. `triangle_count`, `clustering_coefficient`, `louvain` and `label_propagation` take no direction: they read the graph as undirected.
- Stochastic functions take `seed => n`. Set it to get the same result twice.
- Join results to node attributes on `id`: `FROM karate.nodes JOIN degree(karate) USING (id)`.

## Several algorithms: algorithms()

To get several per-node results, use one `algorithms()` call, not a chain of joins. Each argument is a call without the graph. Name a column with `name =>`:

```sql
SELECT id, club, degree, triangles, community
FROM karate.nodes
JOIN algorithms(karate,
                degree(direction => 'both'),
                triangles => triangle_count(),
                community => louvain(seed => 1)) USING (id)
ORDER BY degree DESC
LIMIT 3;
```

```text
+----+---------+--------+-----------+-----------+
| id | club    | degree | triangles | community |
+----+---------+--------+-----------+-----------+
| 33 | Officer | 17     | 15        | 2         |
| 0  | Mr. Hi  | 16     | 18        | 0         |
| 32 | Officer | 12     | 13        | 2         |
+----+---------+--------+-----------+-----------+
```

## Weights

`weight =>` takes a column of the edge table or any expression over its columns. Weights must be non-negative and not null.

```sql
CREATE VIEW lesmis_edges AS SELECT * FROM 'lesmis_edges.csv';
CREATE PROPERTY GRAPH lesmis
  EDGE TABLES (lesmis_edges SOURCE KEY (src) DESTINATION KEY (dst));

SELECT id, louvain AS community
FROM louvain(lesmis, weight => log10(1 + weight), seed => 7)
WHERE id IN ('Valjean', 'Javert', 'Cosette')
ORDER BY id;
```

```text
+---------+-----------+
| id      | community |
+---------+-----------+
| Cosette | 4         |
| Javert  | 1         |
| Valjean | 1         |
+---------+-----------+
```

## Neighbour aggregation: neighbors_agg()

`neighbors_agg` aggregates a value of each node over its neighbours, and returns one row per node: `id` and `neighbors_agg` (a `DOUBLE`). There are two ways to give the values:

- `values =>` takes a query of two columns, node ids then values.
- `attribute =>` takes an expression over the graph's node table.

```sql
SELECT id, round(neighbors_agg, 2) AS friends_degree
FROM neighbors_agg(karate,
                   values => (SELECT id, degree FROM degree(karate, direction => 'both')),
                   agg => 'mean', direction => 'both')
ORDER BY friends_degree DESC, id
LIMIT 3;
```

```text
+----+----------------+
| id | friends_degree |
+----+----------------+
| 11 | 16.0           |
| 14 | 14.5           |
| 15 | 14.5           |
+----+----------------+
```

```sql
SELECT club, count(*) AS members
FROM karate.nodes
JOIN neighbors_agg(karate, attribute => club, agg => 'n_unique', direction => 'both') USING (id)
WHERE neighbors_agg = 2
GROUP BY club
ORDER BY club;
```

```text
+---------+---------+
| club    | members |
+---------+---------+
| Mr. Hi  | 6       |
| Officer | 7       |
+---------+---------+
```

- `agg`: `'mean'` (the default), `'sum'`, `'min'`, `'max'`, `'count'` or `'n_unique'`. String values take only `'count'` and `'n_unique'`.
- `direction`: `'out'` (the default), `'in'` or `'both'`. With `'both'`, a neighbour joined by edges both ways counts twice.
- What is skipped: a NULL value, a row whose id is not in the graph, and a node with no row.
- A node with no neighbour values gets NULL for mean, min and max, and 0 for sum, count and n_unique.
- Each node can have only one row in the values; a second row is an error.
- `neighbors_agg` can't go inside `algorithms()`. Call it on its own and join on `id`.

## Traversals

Traversals return edges or walk steps, not one row per node. A node-id argument takes a list literal (`[0, 1]`, `['Valjean']`), one id, or a subquery of one column:

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

```sql
SELECT count(*) AS reached
FROM hop(karate,
         seeds => (SELECT id FROM karate.nodes WHERE club = 'Officer'),
         n => 2);
```

- `hop(g, seeds =>, n =>, direction =>)`: one `(src, dst)` row per seed and node reached within `n` steps.
- `shortest_path(g, source =>, target =>, weight =>, direction =>)`: one edge per row with `hop` and `cost`. Exactly one source and one target.
- `random_walk(g, start =>, steps =>, walks_per_node =>, seed =>)`: one row per step.

## Pattern matching: GRAPH_TABLE

Needs `ursa` 0.7 or later (check `ursa --version`); 0.6.0 has no `GRAPH_TABLE`.

SQL/PGQ's `GRAPH_TABLE` finds node-and-edge patterns and returns a table. Write it as you would for any SQL/PGQ engine (DuckPGQ queries run as written). Member 0's friends, by club:

```sql
SELECT b.club, count(*) AS ties
FROM GRAPH_TABLE (karate
  MATCH (a WHERE a.id = 0)-[e]-(b)
  COLUMNS (b.club AS club)
) b
GROUP BY b.club
ORDER BY b.club;
```

```text
+---------+------+
| club    | ties |
+---------+------+
| Mr. Hi  | 15   |
| Officer | 1    |
+---------+------+
```

- Nodes are `(var:label WHERE …)`, edges `-[var:label WHERE …]->`, `<-[…]-`, or `-[…]-` (either way; each mutual tie is two matches). `->`, `<-`, `-` with no variable.
- `COLUMNS (…)` is required. Properties are the element table's columns (or its `PROPERTIES`).
- Several paths share variables: `MATCH (a)-[]->(b)-[]->(c), (a)-[]->(c)`.
- Labels pick the table in a graph of several tables; without one, a variable takes the table its edges `REFERENCES`. Ambiguous patterns are refused with a request for a label.
- Quantifiers go after the edge: `-[e]->{1,3}`, `->*`, `-[e]-+`, `?`, `{2,}`. Name the path and pick paths per pair of end nodes: `MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b)` (also `ALL SHORTEST`, `SHORTEST k`; modes `TRAIL`, `SIMPLE`, `ACYCLIC`). In `COLUMNS`: `path_length(p)`, `vertices(p)` (node keys), `edges(p)` (edge rows from 0), `element_id(p)`.
- Ties between equally short paths go to the path whose edges come first in the edge table. Without a selector, an unbounded quantifier needs `TRAIL`, `SIMPLE` or `ACYCLIC`.
- Put conditions on the first node of a quantified edge: the search starts from the nodes they keep.
- Not yet: a path variable, selector or mode on a path of several edges; conditions inside a quantified edge; `:a|b`.

## The .ursa database file

`ursa club.ursa` creates or opens a database. It keeps views, external tables and graphs, and the graph indexes Ursa builds. Rerunning a query in a new process loads the stored index instead of rebuilding it, and a changed source file is noticed and rebuilt.

- Define each view and graph once per database, not before every query. Check `SHOW GRAPHS` first; use `CREATE OR REPLACE` to redefine.
- The file does not store rows. `CREATE TABLE ... AS`, `INSERT`, `UPDATE` and `DELETE` are refused. Write results with `COPY (SELECT ...) TO 'out.parquet'` and define a view over the file.
- Relative paths are stored relative to the database's directory.
- One writer at a time. Use `--readonly` to read a database another process has open.

## Common mistakes

Each of these fails with the message shown. Fix the query as the message says.

**Positional parameters.** Every argument after the graph is named:

```sql
SELECT * FROM pagerank(karate, 0.9);
```

```text
Error: pagerank() takes its parameters by name, as name => value; got 0.9. Write pagerank(<graph>, damping => 0.85, max_iter => 30, tol => 1e-6, weight => NULL, direction => 'out')
```

**A quoted graph name.** A graph is an identifier, not a string:

```sql
SELECT * FROM degree('karate');
```

```text
Error: the first argument of degree() must be the name of a graph; got 'karate' (write the name without quotes: karate)
```

**A chain of `JOIN … USING (id)`.** DataFusion cannot resolve `id` after the second `USING`. Use one `algorithms()` call instead:

```sql
SELECT id, club, degree, pagerank
FROM karate.nodes
JOIN degree(karate) USING (id)
JOIN pagerank(karate) USING (id);
```

```text
Error: Ambiguous reference to unqualified field id
```

Write `JOIN algorithms(karate, degree(), pagerank()) USING (id)` instead.

**Layouts.** `layout_fr`, `layout_circle` and `layout_random` are not in SQL. The error lists what is:

```sql
SELECT * FROM layout_fr(karate);
```

```text
Error: there is no graph function 'layout_fr'. The graph functions are: degree, pagerank, connected_components, triangle_count, clustering_coefficient, betweenness, closeness, label_propagation, louvain, neighbors_agg, density, avg_path_length, diameter, describe, hop, shortest_path, random_walk, algorithms
```

**`log` is base 10.** In DataFusion, as in PostgreSQL and DuckDB, `log(x)` is the base-10 logarithm, not the natural one as in Python. Write `ln(x)` for the natural log, and `log10(x)` when you mean base 10, so the reader does not have to know:

```sql
SELECT log(100) AS log, log10(100) AS log10, ln(100) AS ln;
```

```text
+-----+-------+-------------------+
| log | log10 | ln                |
+-----+-------+-------------------+
| 2.0 | 2.0   | 4.605170185988092 |
+-----+-------+-------------------+
```

**A float, boolean or date as a node id.** `seeds`, `start`, `source` and `target` take integers or strings. On a graph with integer ids a string must be an integer (`'7'`), and nothing is rounded: `1.5`, `2.0` and `true` are refused. On a graph with string ids, an integer is taken as its string.

```sql
SELECT * FROM hop(karate, seeds => [1.5]);
```

```text
Error: hop(seeds =>) must be node ids of graph 'karate', which are integers; got 1.5
```

**Other mistakes:**

- A misspelled parameter or graph gets a "did you mean" suggestion. A wrong `direction` lists the accepted values.
- A list for `shortest_path`'s `source` or `target` is refused: it takes exactly one of each.

## Worked examples

The most central members, by betweenness:

```sql
SELECT club, id, round(betweenness, 3) AS betweenness
FROM karate.nodes
JOIN betweenness(karate, direction => 'both') USING (id)
ORDER BY betweenness DESC
LIMIT 4;
```

```text
+---------+----+-------------+
| club    | id | betweenness |
+---------+----+-------------+
| Mr. Hi  | 0  | 231.071     |
| Officer | 33 | 160.552     |
| Officer | 32 | 76.69       |
| Mr. Hi  | 2  | 75.851      |
+---------+----+-------------+
```

The characters within two steps of Valjean, over strong ties only:

```sql
CREATE VIEW strong_ties AS SELECT * FROM lesmis_edges WHERE weight >= 3;
CREATE PROPERTY GRAPH lesmis_strong
  EDGE TABLES (strong_ties SOURCE KEY (src) DESTINATION KEY (dst));

SELECT count(DISTINCT dst) AS reached
FROM hop(lesmis_strong, seeds => ['Valjean'], n => 2, direction => 'both');
```

```text
+---------+
| reached |
+---------+
| 39      |
+---------+
```

A summary of the graph, read as undirected:

```sql
SELECT * FROM describe(karate, direction => 'both', full => true);
```

```text
+---------+---------+---------------------+-------------------+--------------+-----------------+
| n_nodes | n_edges | density             | avg_degree        | n_components | n_dropped_edges |
+---------+---------+---------------------+-------------------+--------------+-----------------+
| 34      | 78      | 0.13903743315508021 | 4.588235294117647 | 1            | 0               |
+---------+---------+---------------------+-------------------+--------------+-----------------+
```
