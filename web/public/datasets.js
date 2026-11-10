// The Datasets menu. Picking one runs its commands at the prompt, one after
// another, as if typed: its tables and graph first, then a few queries that
// show what Ursa does with it. The files are Parquet in the bucket, read by
// name (see /data/manifest.json).
//
// Keep every query here in step with what the engine returns: the e2e test
// runs each dataset and fails on any error.

export const DATASETS = [
  {
    id: "karate",
    title: "Karate club (graph + attributes)",
    commands: [
      `CREATE VIEW members AS SELECT * FROM 'karate_nodes.parquet';`,
      `CREATE VIEW ties AS SELECT * FROM 'karate_edges.parquet';`,
      `CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                    DESTINATION KEY (dst) REFERENCES members (id));`,
      // Algorithms are table functions: join them to the members' attributes.
      `SELECT id, club, round(pagerank, 4) AS pagerank, community
FROM members
JOIN algorithms(karate, pagerank(direction => 'both'), community => louvain(seed => 1)) USING (id)
ORDER BY pagerank DESC
LIMIT 10;`,
      // SQL/PGQ pattern matching: the friendships across the club's split.
      `FROM GRAPH_TABLE (karate
  MATCH (a)-[e]-(b)
  WHERE a.club = 'Officer' AND b.club = 'Mr. Hi'
  COLUMNS (a.id AS officer, b.id AS member))
ORDER BY officer, member;`,
      // A shortest path, as a pattern with a quantified edge.
      `FROM GRAPH_TABLE (karate
  MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26)
  COLUMNS (path_length(p) AS steps, vertices(p) AS members));`,
    ],
  },
  {
    id: "lesmis",
    title: "Les Misérables (weighted)",
    commands: [
      `CREATE VIEW scenes AS SELECT * FROM 'lesmis_edges.parquet';`,
      `CREATE PROPERTY GRAPH lesmis EDGE TABLES (scenes SOURCE KEY (src) DESTINATION KEY (dst));`,
      `SELECT id AS character, round(pagerank, 4) AS pagerank
FROM pagerank(lesmis, weight => weight, direction => 'both')
ORDER BY pagerank DESC
LIMIT 10;`,
      `SELECT community, count(*) AS characters,
       first_value(id ORDER BY pagerank DESC) AS most_central
FROM algorithms(lesmis,
                pagerank(weight => weight, direction => 'both'),
                community => louvain(weight => weight, seed => 1))
GROUP BY community
ORDER BY characters DESC;`,
      `FROM GRAPH_TABLE (lesmis
  MATCH (a WHERE a.id = 'Valjean')-[e WHERE e.weight >= 5]-(b)
  COLUMNS (b.id AS character, e.weight AS scenes))
ORDER BY scenes DESC;`,
    ],
  },
  {
    id: "florentine",
    title: "Florentine families",
    commands: [
      `CREATE VIEW marriages AS SELECT * FROM 'florentine_edges.parquet';`,
      `CREATE PROPERTY GRAPH florentine EDGE TABLES (marriages SOURCE KEY (src) DESTINATION KEY (dst));`,
      `SELECT id AS family, round(betweenness, 3) AS betweenness
FROM betweenness(florentine, direction => 'both')
ORDER BY betweenness DESC
LIMIT 5;`,
      `FROM GRAPH_TABLE (florentine
  MATCH p = ALL SHORTEST (a WHERE a.id = 'Strozzi')-[e]-*(b WHERE b.id = 'Medici')
  COLUMNS (vertices(p) AS route));`,
    ],
  },
  {
    id: "rmat",
    title: "R-MAT graph, 1M edges (Parquet)",
    commands: [
      `CREATE VIEW rmat_edges AS SELECT * FROM 'rmat_1m_edges.parquet';`,
      `CREATE PROPERTY GRAPH rmat EDGE TABLES (rmat_edges SOURCE KEY (src) DESTINATION KEY (dst));`,
      `FROM describe(rmat);`,
      `SELECT id, round(pagerank, 6) AS pagerank FROM pagerank(rmat) ORDER BY pagerank DESC LIMIT 10;`,
      `SELECT steps, count(*) AS nodes
FROM GRAPH_TABLE (rmat
  MATCH p = ANY SHORTEST (a WHERE a.id = 0)-[e]->*(b)
  COLUMNS (path_length(p) AS steps))
GROUP BY steps
ORDER BY steps;`,
    ],
  },
];
