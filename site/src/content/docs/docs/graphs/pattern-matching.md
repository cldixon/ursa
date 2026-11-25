---
title: "Pattern matching"
description: "Find patterns of nodes and edges, and paths of any length, with SQL/PGQ's GRAPH_TABLE."
---

:::note[New in 0.7]
`GRAPH_TABLE` is new in `ursa-cli` 0.7; 0.6.0 does not have it. Check with `ursa --version`, and upgrade by running the installer again.
:::

`GRAPH_TABLE` finds patterns of nodes and edges in a graph and returns them as a table. It is SQL/PGQ's pattern matching, the part of the SQL standard for property graphs, so a `GRAPH_TABLE` query written for another SQL/PGQ engine, such as DuckPGQ, runs here too.

The examples on this page share one session. They use the karate club graph from the [Quickstart](/docs/quickstart/):

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

## A first match

A pattern is nodes in parentheses and edges in brackets. `COLUMNS` says what each match returns:

```sql
SELECT *
FROM GRAPH_TABLE (karate
  MATCH (a)-[e]-(b)
  WHERE a.club = 'Officer' AND b.club = 'Mr. Hi'
  COLUMNS (a.id AS officer, b.id AS member)
)
ORDER BY officer, member;
```

```text
┌─────────┬────────┐
│ officer │ member │
│  Int64  │ Int64  │
├─────────┼────────┤
│       9 │      2 │
│      27 │      2 │
│      28 │      2 │
│      30 │      1 │
│      30 │      8 │
│      31 │      0 │
│      32 │      2 │
│      32 │      8 │
│      33 │      8 │
│      33 │     13 │
│      33 │     19 │
└─────────┴────────┘
```

Each match binds `a` to a node, `e` to an edge between it and `b`, and `b` to the node at the edge's other end: these are the friendships between the two clubs. `-[e]-` follows an edge either way; `-[e]->` would only follow it from `a` to `b`, the way the edge table stores it. `a.club` is a property of `a`: a column of its vertex table. The result is a table like any other: filter it, join it, aggregate it, or define a view over it.

## The pattern

```text
GRAPH_TABLE ( graph
  MATCH path [, path …]
  [KEEP selector]
  [WHERE condition]
  [ONE ROW PER MATCH]
  [COLUMNS ( expression [AS name], … ) | COLUMNS (*)] )

path := node (edge node)*
node := ( [variable] [:labels | IS labels] [WHERE condition] )
labels := label [| label …]  |  %
edge := -[ … ]->               from left to right
      | <-[ … ]-               from right to left
      | -[ … ]-  |  <-[ … ]->  either way
      | ->  |  <-  |  -  |  <->     the same, with no variable
```

- **Variables** name what a node or edge binds to, so conditions and `COLUMNS` can refer to it. A variable used twice is the same node: `MATCH (a)-[]->(b)-[]->(c), (a)-[]->(c)` finds triangles.
- **Labels** say which tables a variable reads: `(p:person)` or `(p IS person)`. A table's labels are its name and the labels its definition gives, and one label may be on several tables (LDBC's `Message` on `Post` and `Comment`). `(p:person|school)` reads the tables with either label, and `%` is any label. A variable without a label reads every table of its kind that fits its edges: an edge matches only between the vertex tables its edge table `REFERENCES`, so in `(a:person)-[e]->(b)`, `e` is any edge table that starts at `person`, and `b` the table that edge table ends at.
- **Several tables.** A variable over several tables matches a row of any of them. The pattern runs once for each combination of tables its edges allow, and the rows are combined with `UNION ALL`; combinations the edges' `REFERENCES` rule out are left out. When every variable has one table, there is one combination, planned as a single query. A pattern may expand into at most 64 combinations; more is refused, with a request for labels. An undirected edge between two different tables follows the one way its edge table goes.
- **Contradictions.** A label that contradicts the edge is refused: `(a:person)-[:knows]->(s:school)` is an error when `knows` goes from `person` to `person`. The standard gives such a pattern no rows; Ursa reports it, as DuckPGQ does, because it is almost always a mistake.
- **Conditions** go inside a node or edge (`(a WHERE a.age > 30)`), or after the pattern, where they can use every variable.
- **Properties.** A variable's properties are its table's columns, or what its labels' `PROPERTIES` clauses give, under their property names. A variable over several tables reads any property one of them has: in the rows of a table without it, the property is NULL, as the standard says. Where several tables have it, they must have one type (two kinds of number, or of text, are read as the type both fit); otherwise the query is refused, naming the types. A property none of its tables has is refused as a mistake, with the tables named. `a.*`, and `COLUMNS` left out, return only the properties all its tables share: name a property only some of them have to get it. `element_id(a)` works on any table, and `a IS LABELED school` tells which table a row came from.
- **Either way.** `-[e]-` matches each edge in both directions, so each friendship of two members is two matches; an edge from a node to itself matches once. `<-[e]->` is the standard's "left or right", and means the same, since every edge of an Ursa graph is directed. The standard's undirected edges (`~[e]~`) are refused for that reason.
- **COLUMNS.** Left out, or written `COLUMNS (*)`, it returns every property of each named node and edge, in the order they first appear, as `COLUMNS (a.*, e.*, b.*)` would; a repeated name gets a suffix (`id`, `id:1`). This is DuckPGQ's form.

Two steps out from member 0, with each friend of a friend once:

```sql
SELECT DISTINCT friend_of_friend
FROM GRAPH_TABLE (karate
  MATCH (a WHERE a.id = 0)-[]-(b)-[]-(c)
  WHERE c.id <> 0
  COLUMNS (c.id AS friend_of_friend)
)
ORDER BY friend_of_friend
LIMIT 5;
```

```text
┌──────────────────┐
│ friend_of_friend │
│      Int64       │
├──────────────────┤
│                1 │
│                2 │
│                3 │
│                4 │
│                5 │
└──────────────────┘
```

## Graph predicates

SQL/PGQ's predicates on elements work in conditions and in `COLUMNS`:

| predicate | is true when |
|---|---|
| `SAME(a, b, …)` | the nodes are one node |
| `ALL_DIFFERENT(a, b, …)` | no two of the nodes are one node |
| `x IS [NOT] LABELED l` | `x`'s table has the label `l` (`l\|m`: either) |
| `a IS [NOT] SOURCE OF e` | edge `e` starts at node `a` |
| `a IS [NOT] DESTINATION OF e` | edge `e` ends at node `a` |

`element_id(a)` is a node's key. Triangles, each of its three members different:

```sql
SELECT count(*) AS triangles
FROM GRAPH_TABLE (karate
  MATCH (a)-[]->(b)-[]->(c), (a)-[]->(c)
  WHERE all_different(a, b, c)
  COLUMNS (a.id)
);
```

```text
┌───────────┐
│ triangles │
│   Int64   │
├───────────┤
│        45 │
└───────────┘
```

Member 16's friends, and which way each tie is stored:

```sql
SELECT *
FROM GRAPH_TABLE (karate
  MATCH (a WHERE a.id = 16)-[e]-(b)
  COLUMNS (element_id(b) AS friend, a IS SOURCE OF e AS from_16)
)
ORDER BY friend;
```

```text
┌────────┬─────────┐
│ friend │ from_16 │
│ Int64  │ Boolean │
├────────┼─────────┤
│      5 │ false   │
│      6 │ false   │
└────────┴─────────┘
```

`SAME` and `ALL_DIFFERENT` compare nodes for now. `element_id(e)` of an edge is its row in the edge table, the number `edges(p)` lists, so the edge must be the one edge of a path with a variable: `MATCH p = (a)-[e]->(b) COLUMNS (element_id(e))`.

## With graph algorithms

A `GRAPH_TABLE` joins to an algorithm like any table. The most central members tied to member 33:

```sql
SELECT id, round(pagerank, 4) AS pagerank
FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 33)-[]-(b) COLUMNS (b.id AS id))
JOIN pagerank(karate, direction => 'both') USING (id)
ORDER BY pagerank DESC
LIMIT 3;
```

```text
┌───────┬──────────┐
│  id   │ pagerank │
│ Int64 │ Float64  │
├───────┼──────────┤
│    32 │   0.0717 │
│    31 │   0.0372 │
│    23 │   0.0315 │
└───────┴──────────┘
```

## Paths of any length

A quantifier after an edge repeats it: `-[e]->{1,3}` is one to three edges, `*` is any number (none included), `+` one or more, `?` none or one, and `{2}`, `{2,}` and `{,3}` are what they look like. `MATCH p = …` names the path, so `COLUMNS` can ask about it. How far member 16 is from member 26, and the way:

```sql
SELECT *
FROM GRAPH_TABLE (karate
  MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26)
  COLUMNS (path_length(p) AS steps, vertices(p) AS members, edges(p) AS ties)
);
```

```text
┌───────┬───────────────────────┬──────────────────────┐
│ steps │        members        │         ties         │
│ Int64 │ List(non-null Int64)  │ List(non-null Int64) │
├───────┼───────────────────────┼──────────────────────┤
│     5 │ [16, 5, 0, 8, 33, 26] │ [39, 4, 7, 43, 67]   │
└───────┴───────────────────────┴──────────────────────┘
```

A path has four functions: `path_length(p)` is its number of edges, `vertices(p)` its nodes' keys from first to last, `edges(p)` its edges' rows in the edge table (counted from 0), and `element_id(p)` the two alternating, as DuckPGQ lists them (it needs integer keys). A path is not a value itself: `COLUMNS (p)` is an error that names these four. Nor is a node or an edge: name one of its properties (`a.id`), or use `element_id(a)`.

How many members are each number of steps from member 0:

```sql
SELECT steps, count(*) AS members
FROM GRAPH_TABLE (karate
  MATCH p = ANY SHORTEST (a WHERE a.id = 0)-[e]-*(b)
  COLUMNS (path_length(p) AS steps)
)
GROUP BY steps
ORDER BY steps;
```

```text
┌───────┬─────────┐
│ steps │ members │
│ Int64 │  Int64  │
├───────┼─────────┤
│     0 │       1 │
│     1 │      16 │
│     2 │       9 │
│     3 │       8 │
└───────┴─────────┘
```

### Which paths

A selector, after `p =`, says which of a pair of nodes' paths to keep:

| selector | keeps, for each first and last node |
|---|---|
| none, or `ALL` | every path |
| `ANY SHORTEST`, or `ANY` | one shortest path |
| `SHORTEST k`, or `ANY k` | the `k` shortest paths |
| `ALL SHORTEST` | every path as short as the shortest |

`KEEP` after a pattern of one path says the same: `MATCH p = (a)-[e]-*(b) KEEP ANY SHORTEST` is `MATCH p = ANY SHORTEST (a)-[e]-*(b)`, and `KEEP TRAIL` is a path mode, as below. `ONE ROW PER MATCH`, what a `GRAPH_TABLE` returns anyway, is accepted.

When several paths are as short, they are ordered by their edges' rows in the edge table: the path whose first edge comes first, then its second, and so on. `ANY SHORTEST` keeps the first, so the same query always picks the same path.

Every shortest way from member 16 to member 26:

```sql
SELECT *
FROM GRAPH_TABLE (karate
  MATCH p = ALL SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26)
  COLUMNS (vertices(p) AS members)
)
ORDER BY members;
```

```text
┌────────────────────────┐
│        members         │
│  List(non-null Int64)  │
├────────────────────────┤
│ [16, 5, 0, 8, 33, 26]  │
│ [16, 5, 0, 13, 33, 26] │
│ [16, 5, 0, 19, 33, 26] │
│ [16, 5, 0, 31, 33, 26] │
│ [16, 6, 0, 8, 33, 26]  │
│ [16, 6, 0, 13, 33, 26] │
│ [16, 6, 0, 19, 33, 26] │
│ [16, 6, 0, 31, 33, 26] │
└────────────────────────┘
```

A path mode, after the selector, says what a path may repeat: `WALK` (the default) anything, `TRAIL` no edge, `ACYCLIC` no node, and `SIMPLE` no node except that it may end where it started. Without a selector, a path of unbounded length needs one of the last three, or its walks would never end. The paths from member 0 along the ties as stored, each member at most once:

```sql
SELECT count(*) AS paths
FROM GRAPH_TABLE (karate
  MATCH ACYCLIC (a WHERE a.id = 0)-[e]->+(b)
  COLUMNS (b.id)
);
```

```text
┌───────┐
│ paths │
│ Int64 │
├───────┤
│   113 │
└───────┘
```

The bounds count edges, and a shortest path of at least one edge from a node back to itself is a cycle, as the SQL standard defines it. DuckPGQ leaves those out; `WHERE a.id <> b.id` does the same here.

### How many paths

A path search holds its paths in memory, and stops with an error once it holds more than `ursa.max_paths` (10,000,000 unless `SET`; `0` for no limit). The error names the setting and what to do:

- Without a selector, a walk's paths grow fast with its length: give the quantified edge a lower upper bound, or keep fewer paths with `ANY SHORTEST`.
- `ANY SHORTEST` keeps one path per first and last node, so its search holds at most one partial path per node. It counts the paths it keeps, and never refuses one start node's: from one node it may reach every node of the graph. From many start nodes it can still reach the limit; to count the nodes at each distance, [`hop`](/docs/graphs/traversals/#hop) answers without listing paths, or `SET ursa.max_paths` higher. Because of that floor, `ursa.max_paths` set below the graph's node count does not limit an `ANY SHORTEST` search (nor below `k` times it a `SHORTEST k` one): the search says so in a notice on stderr, `ursa.max_paths 1000 is below the node count; ANY SHORTEST keeps up to 39650447 paths`, and its results are the same.

### How it runs

A quantified edge is not a chain of joins. It is a breadth-first search on the graph's index, the one `hop` and `shortest_path` use, starting from the nodes that the first node's conditions keep. Its nodes are the vertex table's keys, as the algorithms' are: a node on no edge has its path of no edges (`{0,…}`), and an edge to or from a value that is no key is not followed, as a fixed-length edge's join leaves it out. A condition on the first node makes the search small. A quantified edge can sit between fixed ones, as in `(s:school)<-[:studyat]-(a)-[:knows]->{1,3}(b)`.

## What is fast

A match is fast when it starts from a few nodes. Name them by their key, in the node or in the pattern's `WHERE`:

- `(a WHERE a.id = 33)`, one node;
- `(a WHERE a.id IN (0, 33))`, a list;
- `WHERE a.id IN (SELECT id FROM members WHERE club = 'Officer')`, a subquery. For now, a table that a subquery inside the pattern reads must also be named in the statement outside the `GRAPH_TABLE`, as in `… FROM GRAPH_TABLE (…) g JOIN members m ON m.id = g.id`; otherwise it is reported as not found.

Such a node is *bound*. From a bound node, every edge of its path is followed on the graph index, outward from it, one edge at a time from the nodes the edge before reached: `MATCH (a WHERE a.id = 33)-[]-(b)-[]-(c)` reads only the edges at member 33 and at its neighbours, never the whole edge table. With the index stored in a database, that takes about as long as `hop(karate, seeds => [33], steps => 2)`. The bound node can be anywhere in the path; the edges on its left are followed backward.

A node whose key is all the query reads (in `COLUMNS` and the conditions) is not joined to its vertex table: its keys come from the index. The vertex table is read only for the properties the query asks for, such as `c.club`.

A path search whose last node is bound stops as soon as it has that node's paths. The shortest way from member 16 to member 26 looks no further than member 26, as `shortest_path(karate, source => 16, target => 26)` does:

```sql
SELECT steps
FROM GRAPH_TABLE (karate
  MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26)
  COLUMNS (path_length(p) AS steps)
);
```

```text
┌───────┐
│ steps │
│ Int64 │
├───────┤
│     5 │
└───────┘
```

`EXPLAIN` shows the plan: each edge followed on the index is a `PathSearchExec` of one edge (`-[]->{1,1}`), a search that stops at its last node says `, targets`, and `EXPLAIN ANALYZE` counts how many paths each one extended (`expanded=`). Edges are joined instead, as in a plain SQL join of the edge and vertex tables, when:

- no node of the path is bound (a condition such as `a.club = 'Officer'` keeps nodes, but does not name them);
- the query reads an edge's columns (`e.since`): that edge, and the edges beyond it, are joined;
- the edge table or a vertex table it references is not a file or a view over files: an in-memory table's index is built again for every query, so a join is cheaper.

Either way, the rows are the same.

**Keep large vertex tables in Parquet.** A query that reads a vertex table's properties scans it, and a gzipped CSV file is read whole on every scan; a Parquet file reads only the columns asked for, and can skip the rest. `COPY (SELECT * FROM 'nodes.csv.gz') TO 'nodes.parquet'` converts one.

## What is not supported yet

These are refused with a message:

- a path variable, selector or path mode on a path of more than one edge, and `KEEP` on a pattern of several paths;
- a condition inside a quantified edge, an edge variable of one in `COLUMNS` (use `edges(p)`), and aggregates in `COLUMNS` (aggregate outside: `SELECT count(*) FROM GRAPH_TABLE (…)`);
- a quantified edge between two vertex tables, and a quantified edge (or the edge of a path with a variable, selector or mode) that can be a row of several edge tables: give it a label;
- label expressions with `&`, `!` or parentheses (`|` and `%` work), and parenthesized path patterns (`((a)-[e]->(b))*`);
- `ONE ROW PER VERTEX` and `ONE ROW PER STEP`: return `vertices(p)` or `edges(p)` and `unnest` them;
- a pattern that refers to a table outside the `GRAPH_TABLE`, with `LATERAL` or by a column of another table in the `FROM`: join on the `GRAPH_TABLE`'s columns instead;
- `SAME` and `ALL_DIFFERENT` on edges.

Cypher's forms inside a pattern are refused with the SQL/PGQ to write: a property map, `(a:person {name: 'Ann'})`, is `(a:person WHERE a.name = 'Ann')`, and a range, `-[e*1..3]->`, is `-[e]->{1,3}`.

Text left over after a `WHERE` condition or a `COLUMNS` item is a syntax error that names it and its position, so a missing `AND` or comma is reported: `WHERE a.id = 0 b.id = 3` does not run as `WHERE a.id = 0`.

`GRAPH TABLE`, in two words, works too.
