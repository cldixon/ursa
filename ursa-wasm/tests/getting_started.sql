-- The Getting Started page's SQL (Tempra `cl_dixon/ursa`, /sql/getting-started.md),
-- from "Query a file" through "Traversals" and "Find out what is available", as
-- one session. The browser parity test (browser/parity.ts) runs it in the wasm
-- engine and in the native `ursa` binary and requires the two outputs to be
-- byte-identical. The native outputs the page shows are checked against the page
-- by ursa-cli/tests/getting_started.rs.
--
-- Not here: the `.ursa` database (the browser build has none), "Output" (COPY
-- writes files), and "Files in object storage".

SELECT * FROM 'karate_edges.csv' LIMIT 3;

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

SELECT * FROM karate.nodes LIMIT 3;

SELECT id, round(pagerank, 4) AS pagerank
FROM pagerank(karate, direction => 'both')
ORDER BY pagerank DESC
LIMIT 5;

SELECT * FROM louvain(karate, seed => 1) ORDER BY id;
SELECT * FROM pagerank(karate, damping => 0.9, direction => 'both') ORDER BY id;

SELECT id, club, degree, triangles, community
FROM karate.nodes
JOIN algorithms(karate,
                degree(direction => 'both'),
                triangles => triangle_count(),
                community => louvain(seed => 1)) USING (id)
ORDER BY degree DESC
LIMIT 5;

CREATE VIEW lesmis_edges AS SELECT * FROM 'lesmis_edges.csv';
CREATE PROPERTY GRAPH lesmis
  EDGE TABLES (lesmis_edges SOURCE KEY (src) DESTINATION KEY (dst));

SELECT * FROM pagerank(lesmis, weight => weight, direction => 'both')
ORDER BY pagerank DESC
LIMIT 5;

SELECT * FROM louvain(lesmis, weight => log10(1 + weight), seed => 7)
ORDER BY id
LIMIT 5;

CREATE VIEW strong_ties AS SELECT * FROM lesmis_edges WHERE weight >= 3;
CREATE PROPERTY GRAPH lesmis_strong
  EDGE TABLES (strong_ties SOURCE KEY (src) DESTINATION KEY (dst));
SELECT count(DISTINCT connected_components) AS groups
FROM connected_components(lesmis_strong);

SELECT * FROM hop(karate, seeds => [0], n => 1, direction => 'both') ORDER BY src, dst;
SELECT * FROM hop(karate,
                  seeds => (SELECT id FROM karate.nodes WHERE club = 'Officer'),
                  n => 2) ORDER BY src, dst;
SELECT * FROM shortest_path(karate, source => 0, target => 33, direction => 'both');
SELECT * FROM random_walk(karate, start => [0, 1], steps => 4, walks_per_node => 2, seed => 7);

SHOW GRAPHS;
DESCRIBE GRAPH karate;
