# Result metadata

A result says what its columns are. Every graph function and `GRAPH_TABLE` marks the columns it returns with Arrow field metadata under an `ursa.` prefix, so a program that reads the result (the Graph view, a notebook, another tool) does not have to guess from a name that `id` is a node and `pagerank` a score.

The metadata changes no value, no column name and no type. A plain table, and a query over files only, has none.

The Web Shell's `.graph` reads it to draw a result as what it is: a `hop` result as its seeds and the nodes reached with the graph's edges among them, `vertices(p)` as paths, three pattern variables as every edge of the pattern, and an algorithm's `id` and score as nodes sized by the score. Without it, `.graph` goes by the columns' names and types.

## The keys

| Key | On | Value |
|---|---|---|
| `ursa.role` | a column | What the column is: one of the roles below. |
| `ursa.graph` | a column | The property graph it came from. |
| `ursa.label` | a column | The vertex table a node belongs to, or the edge table an edge belongs to. Left out for a node of a graph without vertex tables. |
| `ursa.var` | a column of `GRAPH_TABLE` | The pattern variable the column reads. |
| `ursa.pattern` | the schema of `GRAPH_TABLE` | The pattern's edges, as JSON. |

## The roles

| Role | The column is | Where |
|---|---|---|
| `node` | a node's key | an algorithm's `id`, `hop`'s `id`, `random_walk`'s `node`, a node variable's key in `GRAPH_TABLE` |
| `metric` | a number per node | `pagerank`, `degree`, `betweenness`, `closeness`, `triangle_count`, `clustering_coefficient`, and `neighbors_agg` when it returns a number |
| `category` | a group per node | `louvain`, `label_propagation`, `connected_components` |
| `seed` | the node a row started from | `hop`'s `seed` |
| `distance` | the fewest steps from the seed | `hop`'s `distance` |
| `src`, `dst` | the two ends of an edge | `shortest_path`'s and `induced_edges`' `src` and `dst` |
| `edge` | an edge's row in its edge table | `induced_edges`' `edge` |
| `step` | a row's position in its path or walk | `shortest_path`'s `hop`, `random_walk`'s `step` |
| `walk` | which walk a row is in | `random_walk`'s `walk_id` |
| `property` | a property of a pattern variable | `a.club`, `e.weight` in `GRAPH_TABLE` |
| `path_nodes` | a path's nodes, in order | `vertices(p)` |
| `path_edges` | a path's edges, in order | `edges(p)` |
| `path_elements` | a path's nodes and edges, alternating | `element_id(p)` |

A column is what its call is, whatever it is named: in `algorithms(g, community => louvain())`, `community` is a `category`. The columns of a statistic (`describe`, `density`) and of a layout (`x`, `y`), `shortest_path`'s `cost` and `path_length(p)` have no role.

## Reading it

`arrow_metadata(column, key)` returns one value of a column's metadata, or `NULL` if the column has none; `arrow_metadata(column)` returns all of it, as a map. The examples use the karate club graph:

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

```sql
SELECT arrow_metadata(id, 'ursa.role')       AS id_role,
       arrow_metadata(id, 'ursa.graph')      AS id_graph,
       arrow_metadata(id, 'ursa.label')      AS id_label,
       arrow_metadata(pagerank, 'ursa.role') AS pagerank_role
FROM pagerank(karate)
LIMIT 1;
```

```text
┌─────────┬──────────┬──────────┬───────────────┐
│ id_role │ id_graph │ id_label │ pagerank_role │
│  Utf8   │   Utf8   │   Utf8   │     Utf8      │
├─────────┼──────────┼──────────┼───────────────┤
│ node    │ karate   │ members  │ metric        │
└─────────┴──────────┴──────────┴───────────────┘
```

In `GRAPH_TABLE`, each column also names its variable:

```sql
SELECT arrow_metadata(officer, 'ursa.role') AS officer_role,
       arrow_metadata(officer, 'ursa.var')  AS officer_var,
       arrow_metadata(club, 'ursa.role')    AS club_role,
       arrow_metadata(club, 'ursa.var')     AS club_var
FROM GRAPH_TABLE (karate
  MATCH (a)-[e]->(b)
  COLUMNS (a.id AS officer, b.club)
)
LIMIT 1;
```

```text
┌──────────────┬─────────────┬───────────┬──────────┐
│ officer_role │ officer_var │ club_role │ club_var │
│     Utf8     │    Utf8     │   Utf8    │   Utf8   │
├──────────────┼─────────────┼───────────┼──────────┤
│ node         │ a           │ property  │ b        │
└──────────────┴─────────────┴───────────┴──────────┘
```

## What keeps it

The metadata follows a column wherever the column itself goes: `SELECT *`, a column picked or renamed with `AS`, `WHERE`, `ORDER BY`, `LIMIT`, `DISTINCT`, a subquery, a CTE, a view, a join, `UNION`, and a `GROUP BY` key. `CAST` and a unary minus keep it too.

A value computed from a column is a new column, and has none:

```sql
SELECT arrow_metadata(id, 'ursa.role') AS id,
       arrow_metadata(round(pagerank, 4), 'ursa.role') AS rounded,
       arrow_metadata(id + 1, 'ursa.role') AS next
FROM pagerank(karate)
LIMIT 1;
```

```text
┌──────┬─────────┬──────┐
│  id  │ rounded │ next │
│ Utf8 │  Utf8   │ Utf8 │
├──────┼─────────┼──────┤
│ node │ NULL    │ NULL │
└──────┴─────────┴──────┘
```

An inner join on a node's key makes the column it is joined to a node too. `FROM members JOIN pagerank(karate) USING (id)` returns `members`' `id`; the join keeps only the rows where it equals the node's key, so the result says it is one:

```sql
SELECT arrow_metadata(id, 'ursa.role')   AS id_role,
       arrow_metadata(id, 'ursa.graph')  AS id_graph,
       arrow_metadata(club, 'ursa.role') AS club_role
FROM members JOIN pagerank(karate) USING (id)
LIMIT 1;
```

```text
┌─────────┬──────────┬───────────┐
│ id_role │ id_graph │ club_role │
│  Utf8   │   Utf8   │   Utf8    │
├─────────┼──────────┼───────────┤
│ node    │ karate   │ NULL      │
└─────────┴──────────┴───────────┘
```

This holds for `USING`, `NATURAL JOIN` and `ON a.x = b.y`, in an inner join, when the two columns have one type. An outer join's rows need not match, so its plain column stays plain.

## The pattern

`ursa.pattern` is on the schema of a `GRAPH_TABLE` result, not on a column: a JSON array with one object for each edge of the pattern, in the order written. For `MATCH (a)-[e]->(b)` over the graph above:

```json
[{"directed":true,"edge":"e","from":"a","label":"ties","to":"b"}]
```

| Field | Value |
|---|---|
| `from`, `to` | The node variables at the edge's ends, in the edge's direction: `(a)<-[e]-(b)` has `from` `b`. `null` for a node the pattern does not name. |
| `directed` | `false` for `-[e]-`, whose ends are in the order written. |
| `edge` | The edge variable, or `null`. |
| `label` | The edge table. |
| `min`, `max` | Only on a quantified edge (`->{1,3}`, `->*`): how many edges it stands for. `max` is `null` for no upper bound. |
| `path` | Only on the edge of a path with a variable (`p = …`): the path variable. |

With the columns' `ursa.var`, it says which columns are the two ends of which edge.

## In exported files

`COPY … TO` a Parquet or an Arrow file writes the metadata into the file, so the file remembers its graph, and PyArrow, Polars or DuckDB find it on the file's schema.

```sql
COPY (SELECT * FROM pagerank(karate)) TO 'ranks.arrow';
SELECT arrow_metadata(id, 'ursa.graph') AS graph FROM 'ranks.arrow' LIMIT 1;
```

```text
┌────────┐
│ count  │
│ UInt64 │
├────────┤
│     34 │
└────────┘
┌────────┐
│ graph  │
│  Utf8  │
├────────┤
│ karate │
└────────┘
```

Read back with Ursa, an Arrow file's metadata is there. A Parquet file's is in the file but is left out when the file is read, as the metadata of every Parquet file is by default; `SET datafusion.execution.parquet.skip_metadata = false` keeps it. CSV and JSON have no place for it.
