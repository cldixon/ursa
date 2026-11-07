-- The Getting Started page's SQL (docs/sql/getting-started.md), from "Query a
-- file" through "Traversals" and "Find out what is available", as one session.
--
-- Two things read this file:
--
-- - The browser parity test (browser/parity.ts) runs it in the wasm engine and
--   in the native `ursa` binary and requires byte-identical output. The native
--   outputs the page shows are checked against the page by
--   ursa-cli/tests/getting_started.rs.
-- - The Ursa Shell's Tour (web/scripts/tour.ts) is built from it: each
--   `-- @step <title>` starts a step, and the comment lines right after it are
--   the step's note. So every Tour step is tested.
--
-- Not here: the `.ursa` database (the browser build has none), "Output" (COPY
-- writes files), and "Files in object storage".

-- @step Query a file
-- A file is a table: name it in FROM. Ursa reads Parquet and CSV, here over
-- HTTP from the shell's bucket, fetching only the byte ranges it needs.
SELECT * FROM 'karate_edges.csv' LIMIT 3;

-- @step Define a graph
-- Name the files as tables, then define a graph over them with SQL/PGQ's
-- CREATE PROPERTY GRAPH. The vertex table is optional: without one, the nodes
-- are the edges' endpoints.
CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv';
CREATE VIEW ties    AS SELECT * FROM 'karate_edges.csv';
CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (
    ties SOURCE KEY (src) REFERENCES members (id)
         DESTINATION KEY (dst) REFERENCES members (id)
  );
CREATE PROPERTY GRAPH karate_edges_only
  EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));

-- @step A graph's tables
-- A graph is a schema with two tables: its nodes and its edges.
SELECT * FROM karate.nodes LIMIT 3;

-- @step Run an algorithm
-- An algorithm is a table function whose first argument is the graph. It
-- returns one row per node: its id and a column named after the algorithm.
-- The first query over a graph builds its index; later ones reuse it.
SELECT id, round(pagerank, 4) AS pagerank
FROM pagerank(karate, direction => 'both')
ORDER BY pagerank DESC
LIMIT 5;

-- @step Parameters
-- Parameters are named, with the same names and defaults as the Python
-- library. Seeded algorithms give the same answer every time.
SELECT * FROM louvain(karate, seed => 1) ORDER BY id;
SELECT * FROM pagerank(karate, damping => 0.9, direction => 'both') ORDER BY id;

-- @step Combine algorithms and attributes
-- algorithms() runs several algorithms over one graph and returns a column for
-- each. Join it to the node table on id.
SELECT id, club, degree, triangles, community
FROM karate.nodes
JOIN algorithms(karate,
                degree(direction => 'both'),
                triangles => triangle_count(),
                community => louvain(seed => 1)) USING (id)
ORDER BY degree DESC
LIMIT 5;

-- @step Aggregate over neighbours
-- neighbors_agg aggregates a value of each node over its neighbours. Here:
-- members with friends in both clubs, by club.
SELECT club, count(*) AS members
FROM karate.nodes
JOIN neighbors_agg(karate, attribute => club, agg => 'n_unique', direction => 'both') USING (id)
WHERE neighbors_agg = 2
GROUP BY club
ORDER BY club;

-- @step Values from another query
-- values => takes any query of node ids and values, even another algorithm:
-- each member's friends' mean degree.
SELECT id, round(neighbors_agg, 2) AS friends_degree
FROM neighbors_agg(karate,
                   values => (SELECT id, degree FROM degree(karate, direction => 'both')),
                   agg => 'mean', direction => 'both')
ORDER BY friends_degree DESC, id
LIMIT 3;

-- @step Weights
-- A weight is a column of the edge table, or an expression over its columns.
-- Les Misérables counts how often two characters appear together.
CREATE VIEW lesmis_edges AS SELECT * FROM 'lesmis_edges.csv';
CREATE PROPERTY GRAPH lesmis
  EDGE TABLES (lesmis_edges SOURCE KEY (src) DESTINATION KEY (dst));
SELECT * FROM pagerank(lesmis, weight => weight, direction => 'both')
ORDER BY pagerank DESC
LIMIT 5;

-- @step Weight expressions
-- Any expression over the edge columns works as a weight.
SELECT * FROM louvain(lesmis, weight => log10(1 + weight), seed => 7)
ORDER BY id
LIMIT 5;

-- @step Subgraphs
-- A subgraph is a graph over a view that filters the edges.
CREATE VIEW strong_ties AS SELECT * FROM lesmis_edges WHERE weight >= 3;
CREATE PROPERTY GRAPH lesmis_strong
  EDGE TABLES (strong_ties SOURCE KEY (src) DESTINATION KEY (dst));
SELECT count(DISTINCT connected_components) AS groups
FROM connected_components(lesmis_strong);

-- @step Traversals
-- Traversals return rows of their own. hop returns (seed, id, distance): each
-- node within n steps of a seed; the seeds can be a list or a query.
SELECT * FROM hop(karate, seeds => [0], n => 1, direction => 'both') ORDER BY seed, id;
SELECT * FROM hop(karate,
                  seeds => (SELECT id FROM karate.nodes WHERE club = 'Officer'),
                  n => 2) ORDER BY seed, id;

-- @step Shortest paths and random walks
-- shortest_path returns the path's edges with their running cost. A seeded
-- random walk repeats exactly.
SELECT * FROM shortest_path(karate, source => 0, target => 33, direction => 'both');
SELECT * FROM random_walk(karate, start => [0, 1], steps => 4, walks_per_node => 2, seed => 7);

-- @step Match patterns
-- GRAPH_TABLE is SQL/PGQ's pattern matching: nodes in parentheses, edges in
-- brackets, COLUMNS for what each match returns. A quantified edge searches
-- paths: the shortest way from member 16 to member 26.
SELECT * FROM GRAPH_TABLE (karate
  MATCH (a WHERE a.id = 0)-[e]-(b WHERE b.club = 'Officer')
  COLUMNS (b.id AS member)
) ORDER BY member;
SELECT * FROM GRAPH_TABLE (karate
  MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26)
  COLUMNS (path_length(p) AS steps, vertices(p) AS members)
);

-- @step Find out what is available
-- SHOW GRAPHS and DESCRIBE GRAPH describe what is defined; ursa_functions()
-- lists every graph function with its parameters.
SHOW GRAPHS;
DESCRIBE GRAPH karate;
