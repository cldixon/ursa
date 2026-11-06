// The example queries in the shell's picker. The first is the one the editor opens with.
export const EXAMPLES = [
  {
    title: "Who holds the karate club together?",
    sql: `-- PageRank and Louvain communities, joined to the members' attributes.
-- Everything runs in your browser: the engine reads Parquet from R2 over HTTP.
SELECT id, club, round(pagerank, 4) AS pagerank, community
FROM karate.nodes
JOIN algorithms(karate,
                pagerank(direction => 'both'),
                community => louvain(seed => 1)) USING (id)
ORDER BY pagerank DESC
LIMIT 10;`,
  },
  {
    title: "Query a file",
    sql: `SELECT * FROM 'karate_edges.parquet' LIMIT 5;`,
  },
  {
    title: "Weighted PageRank (Les Misérables)",
    sql: `SELECT * FROM pagerank(lesmis, weight => weight, direction => 'both')
ORDER BY pagerank DESC
LIMIT 10;`,
  },
  {
    title: "Brokers among Florentine families",
    sql: `SELECT * FROM betweenness(florentine, direction => 'both')
ORDER BY betweenness DESC
LIMIT 5;`,
  },
  {
    title: "Shortest path",
    sql: `SELECT * FROM shortest_path(karate, source => 0, target => 33, direction => 'both');`,
  },
  {
    title: "Two hops from the officers",
    sql: `SELECT count(*) AS edges, count(DISTINCT dst) AS reached
FROM hop(karate,
         seeds => (SELECT id FROM karate.nodes WHERE club = 'Officer'),
         n => 2);`,
  },
  {
    title: "Define your own graph",
    sql: `CREATE OR REPLACE VIEW strong_ties AS SELECT * FROM lesmis_edges WHERE weight >= 3;
CREATE OR REPLACE PROPERTY GRAPH lesmis_strong
  EDGE TABLES (strong_ties SOURCE KEY (src) DESTINATION KEY (dst));
SELECT connected_components AS component, count(*) AS members
FROM connected_components(lesmis_strong)
GROUP BY 1 ORDER BY members DESC;`,
  },
  {
    title: "Describe a graph",
    sql: `SHOW GRAPHS;
DESCRIBE GRAPH karate;
SELECT * FROM describe(karate);`,
  },
  {
    title: "Every function",
    sql: `SELECT name, family, summary FROM ursa_functions();`,
  },
];
