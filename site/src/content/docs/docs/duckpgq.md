---
title: "Coming from SQL/PGQ or DuckPGQ"
description: "What runs unchanged from SQL/PGQ and DuckPGQ, what differs, and Ursa's extensions."
---

If you know SQL/PGQ, the property graph part of the SQL standard (ISO/IEC 9075-16), or DuckPGQ, which adds it to DuckDB, most of what you write runs in Ursa as it is. Ursa follows three rules:

1. **Where the standard says how, Ursa does it that way:** `CREATE PROPERTY GRAPH` and `GRAPH_TABLE … MATCH`.
2. **Where the standard says nothing, Ursa does what DuckPGQ does.** No standard defines graph algorithms, so Ursa takes DuckPGQ's calls: `pagerank(pg, Student, know)`.
3. **Ursa's own additions are extensions,** and this page marks them. The standard form works beside each one.

Every claim on this page is checked by Ursa's SQL/PGQ and DuckPGQ conformance suite, `tests/conformance/` in the repository, or by the tests of the engine. The suite keeps a reviewed list of the cases where Ursa differs, and fails when one of them stops differing, so the list below stays true.

## The example graph

The examples on this page use DuckPGQ's own test graph: five students who know each other, and the schools they study at. A database file stores views, not tables of rows, so the tables are views here; in a session without a file, `CREATE TABLE` works too.

```sql
CREATE VIEW Student AS SELECT * FROM (VALUES (0,'Daniel'),(1,'Tavneet'),(2,'Gabor'),(3,'Peter'),(4,'David')) t(id, name);
CREATE VIEW know AS SELECT * FROM (VALUES (0,1,10),(0,2,11),(0,3,12),(3,0,13),(1,2,14),(1,3,15),(2,3,16),(4,3,17)) t(src, dst, createDate);
CREATE VIEW School AS SELECT * FROM (VALUES ('VU',0,0),('UVA',1,0)) t(school_name, school_id, school_kind);
CREATE VIEW StudyAt AS SELECT * FROM (VALUES (0,0),(1,0),(2,1),(3,1),(4,1)) t(personId, schoolId);
```

## What runs unchanged

### Graph definitions

DuckPGQ's definition of the graph runs as DuckPGQ's documentation writes it: several vertex and edge tables, `LABEL`, `PROPERTIES` (before or after the label, as DuckPGQ writes them), and no `KEY` on a vertex table, which is taken from the edges' `REFERENCES`.

```sql
CREATE PROPERTY GRAPH pg
VERTEX TABLES (Student PROPERTIES (id, name) LABEL Person, School LABEL School)
EDGE TABLES (
  know    SOURCE KEY (src) REFERENCES Student (id) DESTINATION KEY (dst) REFERENCES Student (id)
          PROPERTIES (createDate) LABEL Knows,
  studyAt SOURCE KEY (personId) REFERENCES Student (id) DESTINATION KEY (schoolId) REFERENCES School (school_id));
```

`REFERENCES Student` without a column list names the vertex table's key. `CREATE OR REPLACE`, `IF NOT EXISTS`, `PROPERTIES ARE ALL COLUMNS`, `PROPERTIES ALL COLUMNS EXCEPT (…)`, `NO PROPERTIES` and `DEFAULT LABEL` are read as the standard defines them. DuckPGQ's `DESCRIBE PROPERTY GRAPH`, `SHOW PROPERTY GRAPHS`, `DROP PROPERTY GRAPH` and `SUMMARIZE PROPERTY GRAPH` run too.

### Pattern matching

`GRAPH_TABLE` takes the standard's patterns: labels with `:` or `IS`, label disjunctions (`:Person|School`) and `%`, a variable without a label, every edge direction, conditions on a node, an edge or the whole pattern, and several paths that share variables.

```sql
SELECT * FROM GRAPH_TABLE (pg
  MATCH (a:Person)-[k:knows]->(b:Person)
  WHERE a.name = 'Daniel'
  COLUMNS (a.name AS a, b.name AS b))
ORDER BY b;
```

```text
┌────────┬─────────┐
│   a    │    b    │
│  Utf8  │  Utf8   │
├────────┼─────────┤
│ Daniel │ Gabor   │
│ Daniel │ Peter   │
│ Daniel │ Tavneet │
└────────┴─────────┘
```

Quantifiers (`{1,3}`, `*`, `+`, `?`), the selectors `ANY SHORTEST`, `ALL SHORTEST` and `SHORTEST k`, the path modes `WALK`, `TRAIL`, `SIMPLE` and `ACYCLIC`, `KEEP` on a pattern of one path, and the path functions `path_length(p)`, `vertices(p)`, `edges(p)` and `element_id(p)` run as well:

```sql
SELECT * FROM GRAPH_TABLE (pg
  MATCH p = ANY SHORTEST (a:Person WHERE a.id = 0)-[k:knows]->*(b:Person)
  COLUMNS (b.id AS b, path_length(p) AS hops))
ORDER BY b;
```

```text
┌───────┬───────┐
│   b   │ hops  │
│ Int64 │ Int64 │
├───────┼───────┤
│     0 │     0 │
│     1 │     1 │
│     2 │     1 │
│     3 │     1 │
└───────┴───────┘
```

`COLUMNS` may be left out, or written `COLUMNS (*)`, for every property of each variable, as DuckPGQ returns them. The predicates `SAME`, `ALL_DIFFERENT`, `IS LABELED`, `IS SOURCE OF` and `IS DESTINATION OF` are read in `WHERE` and `COLUMNS`. [Pattern matching](/docs/graphs/pattern-matching/) describes all of it.

### DuckPGQ's algorithm calls

DuckPGQ calls an algorithm as a table function with three arguments, the graph, a vertex table and an edge table, and Ursa takes the same call. The vertex and edge table may be named by their labels too (`pagerank(pg, Person, Knows)`).

```sql
SELECT id, pagerank FROM pagerank(pg, Student, know) ORDER BY id;
```

```text
┌───────┬──────────────────────┐
│  id   │       pagerank       │
│ Int64 │       Float64        │
├───────┼──────────────────────┤
│     0 │   0.3256595333889792 │
│     1 │  0.12227026550952791 │
│     2 │  0.17423517516245024 │
│     3 │  0.34783502593904303 │
│     4 │ 0.030000000000000006 │
└───────┴──────────────────────┘
```

Where DuckPGQ names a function differently from Ursa, Ursa accepts DuckPGQ's name and returns DuckPGQ's column:

| DuckPGQ's call | Runs | Column |
|---|---|---|
| `pagerank(pg, Student, know)` | `pagerank` | `pagerank` |
| `weakly_connected_component(pg, Student, know)` | `connected_components(mode => 'weak')` | `componentid` |
| `local_clustering_coefficient(pg, Student, know)` | `clustering_coefficient` | `local_clustering_coefficient` |

```sql
SELECT id, componentId FROM weakly_connected_component(pg, Student, know) ORDER BY id;
```

```text
┌───────┬─────────────┐
│  id   │ componentid │
│ Int64 │   UInt32    │
├───────┼─────────────┤
│     0 │           0 │
│     1 │           0 │
│     2 │           0 │
│     3 │           0 │
│     4 │           0 │
└───────┴─────────────┘
```

```sql
SELECT id, local_clustering_coefficient FROM local_clustering_coefficient(pg, Student, know) ORDER BY id;
```

```text
┌───────┬──────────────────────────────┐
│  id   │ local_clustering_coefficient │
│ Int64 │           Float64            │
├───────┼──────────────────────────────┤
│     0 │                          1.0 │
│     1 │                          1.0 │
│     2 │                          1.0 │
│     3 │                          0.5 │
│     4 │                          0.0 │
└───────┴──────────────────────────────┘
```

The column names follow DuckPGQ, not Ursa (`connected_components`, `clustering_coefficient`): someone who calls a function by DuckPGQ's name is running a DuckPGQ query, and the query selects DuckPGQ's columns. Ursa's own names keep Ursa's columns. A DuckPGQ name takes the named parameters of the function it runs, except the ones its name fixes: `weakly_connected_component(…, mode => 'strong')` is an error that says to call `connected_components`.

`SELECT * FROM ursa_functions()` lists these names as rows of their own, after Ursa's functions. The column `same_as` names the Ursa function each one runs, and `duckpgq` is true for every function DuckPGQ has by that name, `pagerank` included:

```sql
SELECT name, same_as, duckpgq FROM ursa_functions() WHERE duckpgq;
```

```text
┌──────────────────────────────┬────────────────────────┬─────────┐
│             name             │        same_as         │ duckpgq │
│             Utf8             │          Utf8          │ Boolean │
├──────────────────────────────┼────────────────────────┼─────────┤
│ pagerank                     │ NULL                   │ true    │
│ weakly_connected_component   │ connected_components   │ true    │
│ local_clustering_coefficient │ clustering_coefficient │ true    │
└──────────────────────────────┴────────────────────────┴─────────┘
```

They are DuckPGQ's spellings, accepted so DuckPGQ's queries run. Ursa keeps no old names of its own: a renamed parameter or function is refused with a "did you mean" that names the new one.

## What differs

| You write | DuckPGQ or the standard | Ursa |
|---|---|---|
| `SELECT "componentId" FROM weakly_connected_component(…)` | DuckDB keeps a column's case: `componentId` | Unquoted names are folded to lower case, as in DataFusion and PostgreSQL, so the column is `componentid`. `componentId` written without quotes finds it; `"componentId"` in quotes does not. |
| `pagerank(pg, Student, know)` | Scores that sum to 0.94 on this graph | Scores that sum to 1, as NetworkX's do (see [numeric differences](#numeric-differences)) |
| `(a:Person)-[k:knows]->(b:School)`, a label the edge's `REFERENCES` contradicts | The standard matches nothing | An error that names the edge table's ends, as DuckPGQ gives |
| `MATCH p = ANY SHORTEST (a)-[k]->+(b)` from a node back to itself | The standard returns the cycle; DuckPGQ leaves it out | The cycle, as the standard says |
| `-[e]->` and `~[e]~` | `~` follows an undirected edge | Every Ursa edge is directed: `~[e]~` is refused with a pointer to `-[e]-`, which follows an edge either way |
| `direction => 'undirected'` | DuckPGQ's word | Refused with a pointer to `'both'` |
| `iterativelength`, `shortestpath`, `reachability`, `cheapest_path_length` | DuckPGQ's scalar functions over its CSR, the plumbing of its `MATCH` | Not in Ursa; the error says to use `GRAPH_TABLE … ANY SHORTEST` or `shortest_path()` |
| `KEY (a, b)` on a vertex or edge table | Keys of several columns | Not supported yet |
| `ONE ROW PER VERTEX` or `ONE ROW PER STEP` | Supported by the standard | Not supported yet |
| `KEEP` over a pattern of several paths, a selector on a path of several edges | Supported by the standard | Not supported yet; write the selector before a path of one edge |
| A quantified edge that can be a row of several edge tables | Supported | Refused: give the edge a label |
| `element_id(e)` of an edge read by a join | Supported | Only the edges of a searched path have one yet |
| `LATERAL GRAPH_TABLE`, or a pattern that refers to a table outside it | Supported by the standard | Not supported yet; join on the output columns instead |

Each "not supported yet" is an error that says so; none is a wrong answer.

## Numeric differences

On DuckPGQ's graph, DuckPGQ's PageRank scores are `0.307, 0.115, 0.164, 0.328, 0.028` for students 0 to 4, which sum to 0.94. Ursa's are NetworkX's, which sum to 1:

```sql
SELECT id, round(pagerank, 4) AS pagerank FROM pagerank(pg, Student, know) ORDER BY id;
```

```text
┌───────┬──────────┐
│  id   │ pagerank │
│ Int64 │ Float64  │
├───────┼──────────┤
│     0 │   0.3257 │
│     1 │   0.1223 │
│     2 │   0.1742 │
│     3 │   0.3478 │
│     4 │     0.03 │
└───────┴──────────┘
```

Ursa's results are pinned to NetworkX's, for the DuckPGQ names too: compatibility covers how a call is written and the shape of its result, not its numbers. The two agree on the order of the nodes. Each of DuckPGQ's scores is Ursa's times about 0.94, student 4's too, whose score is only the share every node gets from the damping factor, as nobody knows student 4. No student here lacks an out-edge, so the difference is not how the two treat nodes without one; it is a constant factor, which the conformance suite records against this case.

`weakly_connected_component` and `local_clustering_coefficient` follow the same definitions as DuckPGQ's: on this graph, one component, and coefficients of 1, 1, 1, 0.5 and 0, reading the edges as undirected.

## Ursa's extensions

These are Ursa's own. Neither the standard nor DuckPGQ has them, and a query that uses one runs on Ursa only.

- **Named parameters** on every graph function: `pagerank(pg, Student, know, damping => 0.9)`. `name = value` and `name := value` work too.
- **An implied pair.** On a graph of one edge table, `pagerank(g)` needs no vertex or edge table.
- **The `nodes` and `edges` tables** of a graph of one edge table: `SELECT * FROM g.nodes`.
- **More functions:** `degree`, `triangle_count`, `betweenness`, `closeness`, `label_propagation`, `louvain`, the statistics (`density`, `diameter`, `avg_path_length`, `describe`), `neighbors_agg`, the layouts, and the traversals `hop`, `shortest_path`, `random_walk` and `induced_edges`.
- **`algorithms(g, …)`,** several per-node functions over one index in one call.
- **`direction =>`** on the functions that read edges one way: `'out'`, `'in'` or `'both'`.
- **`ursa_functions()`,** the list of graph functions as a table.
- **Graphs without vertex tables:** `CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (src) DESTINATION KEY (dst))`.

## Translating

**From DuckPGQ,** most queries need no change. Write a column DuckPGQ names in mixed case, such as `componentId`, without quotes. For DuckPGQ's path functions, match a path in `GRAPH_TABLE`:

```sql
SELECT * FROM GRAPH_TABLE (pg
  MATCH p = ANY SHORTEST (a:Person WHERE a.id = 4)-[k:knows]->*(b:Person WHERE b.id = 1)
  COLUMNS (path_length(p) AS hops));
```

```text
┌───────┐
│ hops  │
│ Int64 │
├───────┤
│     3 │
└───────┘
```

**From Cypher or GQL,** a pattern goes in `GRAPH_TABLE`, in `FROM`, and `COLUMNS` takes the place of `RETURN`:

| Cypher | SQL/PGQ |
|---|---|
| `MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name, b.name` | `SELECT * FROM GRAPH_TABLE (g MATCH (a:Person)-[:knows]->(b) COLUMNS (a.name, b.name))` |
| `(a:Person {name: 'Daniel'})` | `(a:Person WHERE a.name = 'Daniel')` |
| `-[:KNOWS*1..3]->` | `-[:knows]->{1,3}` |
| `shortestPath((a)-[*]->(b))` | `MATCH p = ANY SHORTEST (a)-[e]->*(b)` |

## Errors that point to the standard form

Ursa answers the common forms that are not SQL/PGQ with the form to write:

```sql
MATCH (a)-[k]->(b) RETURN a.id, b.id;
```

```text
Error: a statement cannot start with MATCH: that is Cypher's or GQL's form. In SQL/PGQ a pattern goes in GRAPH_TABLE, in FROM, and COLUMNS takes the place of RETURN: SELECT * FROM GRAPH_TABLE (g MATCH (a)-[e]->(b) COLUMNS (a.id AS a, b.id AS b))
```

```sql
SELECT * FROM pg;
```

```text
Error: table 'datafusion.public.pg' not found; 'pg' is a graph, not a table: its vertex tables are student, school and its edge tables know, studyat. Read one of them by its own name, match a pattern with SELECT * FROM GRAPH_TABLE (pg MATCH (a)-[e]->(b) COLUMNS (a.id, b.id)), or call a graph function on a vertex table and an edge table, as in SELECT * FROM pagerank(pg, <vertex table>, <edge table>)
```

```sql
SELECT id, pagerank FROM Student;
```

```text
Error: No field named pagerank.
Valid fields are student.id, student.name.
pagerank is a graph function, not a column: join its rows on id, as in SELECT * FROM student JOIN pagerank(pg, student, know) USING (id)
```

```sql
SELECT iterativelength(0, 5, 0, 3);
```

```text
Error: iterativelength() is DuckPGQ's internal path function over its CSR, which Ursa does not have. Find paths with GRAPH_TABLE (g MATCH p = ANY SHORTEST (a)-[e]->*(b) COLUMNS (path_length(p) AS len)), or the table function shortest_path(g, source => …, target => …)
```

A Cypher property map (`{name: 'x'}`) or range (`*1..3`) inside `GRAPH_TABLE` is answered the same way, and so is a graph function called as a scalar (`SELECT pagerank(pg)`).
