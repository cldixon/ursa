# Pattern matching

!!! info "New in 0.7"

    `GRAPH_TABLE` arrives in `ursa-cli` 0.7. It is not in 0.6.0; until 0.7 is released, build `ursa` from the repository to use it: `cargo install --git https://github.com/cldixon/ursa ursa-cli`.

`GRAPH_TABLE` finds patterns of nodes and edges in a graph and returns them as a table. It is SQL/PGQ's pattern matching, the part of the SQL standard for property graphs, so a `GRAPH_TABLE` query written for another SQL/PGQ engine, such as DuckPGQ, runs here too.

The examples on this page share one session. They use the karate club graph from [Getting started](getting-started.md):

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
+---------+--------+
| officer | member |
+---------+--------+
| 9       | 2      |
| 27      | 2      |
| 28      | 2      |
| 30      | 1      |
| 30      | 8      |
| 31      | 0      |
| 32      | 2      |
| 32      | 8      |
| 33      | 8      |
| 33      | 13     |
| 33      | 19     |
+---------+--------+
```

Each match binds `a` to a node, `e` to an edge between it and `b`, and `b` to the node at the edge's other end: these are the friendships between the two clubs. `-[e]-` follows an edge either way; `-[e]->` would only follow it from `a` to `b`, the way the edge table stores it. `a.club` is a property of `a`: a column of its vertex table. The result is a table like any other: filter it, join it, aggregate it, or define a view over it.

## The pattern

```text
GRAPH_TABLE ( graph
  MATCH path [, path …]
  [WHERE condition]
  COLUMNS ( expression [AS name], … ) )

path := node (edge node)*
node := ( [variable] [:label | IS label] [WHERE condition] )
edge := -[ … ]->   from left to right
      | <-[ … ]-   from right to left
      | -[ … ]-    either way
      | ->  |  <-  |  -     the same, with no variable
```

- **Variables** name what a node or edge binds to, so conditions and `COLUMNS` can refer to it. A variable used twice is the same node: `MATCH (a)-[]->(b)-[]->(c), (a)-[]->(c)` finds triangles.
- **Labels** say which table a variable reads: `(p:person)` or `(p IS person)`. A table's labels are its name and the labels its definition gives. Without a label, a variable reads the only table it can: the one its edges `REFERENCES`, or the graph's only vertex or edge table. When that cannot be told, the query is refused and asks for a label.
- **Conditions** go inside a node or edge (`(a WHERE a.age > 30)`), or after the pattern, where they can use every variable.
- **Properties.** A variable's properties are its table's columns, or what its labels' `PROPERTIES` clauses give, under their property names.
- **Either way.** `-[e]-` matches each edge in both directions, so each friendship of two members is two matches; an edge from a node to itself matches once.

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
+------------------+
| friend_of_friend |
+------------------+
| 1                |
| 2                |
| 3                |
| 4                |
| 5                |
+------------------+
```

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
+----+----------+
| id | pagerank |
+----+----------+
| 32 | 0.0717   |
| 31 | 0.0372   |
| 23 | 0.0315   |
+----+----------+
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
+-------+-----------------------+--------------------+
| steps | members               | ties               |
+-------+-----------------------+--------------------+
| 5     | [16, 5, 0, 8, 33, 26] | [39, 4, 7, 43, 67] |
+-------+-----------------------+--------------------+
```

A path has four functions: `path_length(p)` is its number of edges, `vertices(p)` its nodes' keys from first to last, `edges(p)` its edges' rows in the edge table (counted from 0), and `element_id(p)` the two alternating, as DuckPGQ lists them (it needs integer keys).

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
+-------+---------+
| steps | members |
+-------+---------+
| 0     | 1       |
| 1     | 16      |
| 2     | 9       |
| 3     | 8       |
+-------+---------+
```

### Which paths

A selector, after `p =`, says which of a pair of nodes' paths to keep:

| selector | keeps, for each first and last node |
|---|---|
| none, or `ALL` | every path |
| `ANY SHORTEST`, or `ANY` | one shortest path |
| `SHORTEST k`, or `ANY k` | the `k` shortest paths |
| `ALL SHORTEST` | every path as short as the shortest |

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
+------------------------+
| members                |
+------------------------+
| [16, 5, 0, 8, 33, 26]  |
| [16, 5, 0, 13, 33, 26] |
| [16, 5, 0, 19, 33, 26] |
| [16, 5, 0, 31, 33, 26] |
| [16, 6, 0, 8, 33, 26]  |
| [16, 6, 0, 13, 33, 26] |
| [16, 6, 0, 19, 33, 26] |
| [16, 6, 0, 31, 33, 26] |
+------------------------+
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
+-------+
| paths |
+-------+
| 113   |
+-------+
```

The bounds count edges, and a shortest path of at least one edge from a node back to itself is a cycle, as the SQL standard defines it. DuckPGQ leaves those out; `WHERE a.id <> b.id` does the same here.

### How it runs

A quantified edge is not a chain of joins. It is a breadth-first search on the graph's index, the one `hop` and `shortest_path` use, starting from the nodes that the first node's conditions keep. Its nodes are the vertex table's keys, as the algorithms' are: a node on no edge has its path of no edges (`{0,…}`), and an edge to or from a value that is no key is not followed, as a fixed-length edge's join leaves it out. A condition on the first node makes the search small. A quantified edge can sit between fixed ones, as in `(s:school)<-[:studyat]-(a)-[:knows]->{1,3}(b)`.

## What is not supported yet

These are refused with a message:

- a path variable, selector or path mode on a path of more than one edge;
- a condition inside a quantified edge, or an edge variable of one in `COLUMNS` (use `edges(p)`);
- a quantified edge between two vertex tables;
- label expressions (`:a|b`) and parenthesized path patterns (`((a)-[e]->(b))*`).

`COLUMNS` is required. `GRAPH TABLE`, in two words, works too.
