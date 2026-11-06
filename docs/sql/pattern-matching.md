# Pattern matching

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
  MATCH (a)-[e]->(b)
  WHERE a.club = 'Officer' AND b.club = 'Mr. Hi'
  COLUMNS (a.id AS officer, b.id AS member)
)
ORDER BY officer, member;
```

```text
OUTPUT_1
```

Each match binds `a` to a node, `e` to an edge from it and `b` to the node the edge goes to. `a.club` is a property of `a`: a column of its vertex table. The result is a table like any other: filter it, join it, aggregate it, or define a view over it.

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
OUTPUT_2
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
OUTPUT_3
```

## What is not supported yet

Paths of variable length (`-[e]->*`, `-[e]->{1,3}`), path variables (`p = …`), `ANY SHORTEST`, and label expressions (`:a|b`) are refused with a message: they come next ([#297](https://github.com/cldixon/ursa/issues/297)). `COLUMNS` is required. `GRAPH TABLE`, in two words, works too.
