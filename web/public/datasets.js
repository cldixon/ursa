// The Datasets menu. Picking one runs its commands at the prompt, one after
// another, as if typed: its tables and graph first, then a few queries that
// show what Ursa does with it. The files are Parquet in the bucket, read by
// name (see /data/manifest.json).
//
// The `--` lines say what each step is about. They are part of the commands,
// so they are shown at the prompt (and the shell takes a line that is only a
// comment as a no-op): keep the explanations there, not in comments here.
//
// Keep every query here in step with what the engine returns: the e2e test
// runs each dataset and fails on any error. Each ends with a comment that says
// how to draw it with .graph: a dataset does not open the Graph view itself,
// which would download it (a few MB) for every visitor who only wanted SQL.
//
// The first entry is the tour (`.tour`, or ?dataset=tour): one story on the
// karate club for a first visit, in five steps that each say what they show.
// It sits next to the karate dataset rather than replacing it: the dataset is
// the compact one (?dataset=karate, and the docs' links, load it to try SQL
// on), and the tour is the narrative, which draws the graph.

// On a phone every table it prints fits the screen (46 columns), and its SQL
// and comments are broken to fit as they are typed.
const TOUR = {
  id: "tour",
  title: "Tour: the karate club",
  // A comment line is a sentence or two: the page breaks it to fit the screen.
  commands: [
    `-- Welcome to Ursa, graph analytics in SQL. This tour takes five steps, and each says what it shows.
-- Step 1, the data: Zachary's karate club, 34 members and the friendships between them.
CREATE VIEW members AS SELECT * FROM 'karate_nodes.parquet';`,
    `-- The club split in two, behind Mr. Hi (member 0) and the Officer (member 33): club says whose side each member took.
CREATE VIEW ties AS SELECT * FROM 'karate_edges.parquet';`,
    `-- A property graph over the two tables: members are its nodes, and ties, the friendships, its edges.
CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (ties
    SOURCE KEY (src) REFERENCES members (id)
    DESTINATION KEY (dst) REFERENCES members (id));`,
    `-- Step 2, who matters: graph algorithms are table functions, so you join them to tables on id.
-- PageRank scores each member by how well connected they are, and the two leaders come out on top.
SELECT id, club, round(pagerank, 3) AS pagerank
FROM members JOIN pagerank(karate, direction => 'both') USING (id)
ORDER BY pagerank DESC
LIMIT 5;`,
    `-- Step 3, the two factions: Louvain finds close-knit groups from the friendships alone.
-- Set against the side each member really took, it gets 33 of the 34 right.
SELECT louvain AS faction, club, count(*) AS members
FROM members JOIN louvain(karate, resolution => 0.5, seed => 1) USING (id)
GROUP BY faction, club
ORDER BY faction, club;`,
    `-- Step 4, draw it: members sized by PageRank and coloured by the side they took.
-- Drag to pan, scroll to zoom, click a member to see who it is; Esc closes the view.
.graph karate SIZE BY pagerank COLOR BY club`,
    `-- Step 5, paths: GRAPH_TABLE matches patterns of nodes and edges, in SQL/PGQ from the SQL standard.
-- ALL SHORTEST finds every shortest chain of friendships from Mr. Hi to the Officer.
FROM GRAPH_TABLE (karate
  MATCH p = ALL SHORTEST (a WHERE a.id = 0)-[e]-*(b WHERE b.id = 33)
  COLUMNS (path_length(p) AS steps, vertices(p) AS path))
ORDER BY path;
-- Two steps, through four friends of both: member 8 is the one Louvain placed with the Officer.
-- That's the tour. Type your own SQL (.help lists the commands), or pick a graph from Datasets.`,
  ],
};

export const DATASETS = [
  TOUR,
  {
    id: "karate",
    title: "Karate club (graph + attributes)",
    commands: [
      `-- Zachary's karate club: 34 members and the friendships between them,
-- recorded just before the club split between Mr. Hi and the Officer.
CREATE VIEW members AS SELECT * FROM 'karate_nodes.parquet';`,
      `CREATE VIEW ties AS SELECT * FROM 'karate_edges.parquet';`,
      `-- A property graph over the two tables: members are its vertices, ties its edges.
CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                    DESTINATION KEY (dst) REFERENCES members (id));`,
      `-- Graph algorithms are table functions: join them to the members' attributes.
-- PageRank scores each member's influence; Louvain finds communities.
SELECT id, club, round(pagerank, 4) AS pagerank, community
FROM members
JOIN algorithms(karate, pagerank(direction => 'both'), community => louvain(seed => 1)) USING (id)
ORDER BY pagerank DESC
LIMIT 10;`,
      `-- SQL/PGQ pattern matching: the friendships across the club's split.
FROM GRAPH_TABLE (karate
  MATCH (a)-[e]-(b)
  WHERE a.club = 'Officer' AND b.club = 'Mr. Hi'
  COLUMNS (a.id AS officer, b.id AS member))
ORDER BY officer, member;`,
      `-- A shortest path, as a pattern with a quantified edge: -[e]-* is any number of hops.
FROM GRAPH_TABLE (karate
  MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26)
  COLUMNS (path_length(p) AS steps, vertices(p) AS members));
-- To draw the club, type .graph karate (or take the tour: .tour).`,
    ],
  },
  {
    id: "lesmis",
    title: "Les Misérables (weighted)",
    commands: [
      `-- Les Misérables: characters linked by the scenes they share, weighted by how many.
CREATE VIEW scenes AS SELECT * FROM 'lesmis_edges.parquet';`,
      `-- An edge table alone makes a graph: its vertices are the ids in src and dst.
CREATE PROPERTY GRAPH lesmis EDGE TABLES (scenes SOURCE KEY (src) DESTINATION KEY (dst));`,
      `-- Weighted PageRank: the characters at the centre of the story.
SELECT id AS character, round(pagerank, 4) AS pagerank
FROM pagerank(lesmis, weight => weight, direction => 'both')
ORDER BY pagerank DESC
LIMIT 10;`,
      `-- Louvain communities, each with its size and its most central character.
SELECT community, count(*) AS characters,
       first_value(id ORDER BY pagerank DESC) AS most_central
FROM algorithms(lesmis,
                pagerank(weight => weight, direction => 'both'),
                community => louvain(weight => weight, seed => 1))
GROUP BY community
ORDER BY characters DESC;`,
      `-- A pattern with a condition on its edge: who shares five or more scenes with Valjean.
FROM GRAPH_TABLE (lesmis
  MATCH (a WHERE a.id = 'Valjean')-[e WHERE e.weight >= 5]-(b)
  COLUMNS (b.id AS character, e.weight AS scenes))
ORDER BY scenes DESC;
-- To draw the characters and their scenes, type .graph lesmis`,
    ],
  },
  {
    id: "florentine",
    title: "Florentine families",
    commands: [
      `-- Marriages between the leading families of Renaissance Florence.
CREATE VIEW marriages AS SELECT * FROM 'florentine_edges.parquet';`,
      `CREATE PROPERTY GRAPH florentine EDGE TABLES (marriages SOURCE KEY (src) DESTINATION KEY (dst));`,
      `-- Betweenness: how often a family lies on the shortest paths between the others.
-- The Medici, who came to rule Florence, top it.
SELECT id AS family, round(betweenness, 3) AS betweenness
FROM betweenness(florentine, direction => 'both')
ORDER BY betweenness DESC
LIMIT 5;`,
      `-- Every shortest chain of marriages from the Strozzi to the Medici.
FROM GRAPH_TABLE (florentine
  MATCH p = ALL SHORTEST (a WHERE a.id = 'Strozzi')-[e]-*(b WHERE b.id = 'Medici')
  COLUMNS (vertices(p) AS route));
-- To draw the marriages, type .graph florentine`,
    ],
  },
  {
    id: "rmat",
    title: "R-MAT graph, 1M edges (Parquet)",
    commands: [
      `-- A synthetic R-MAT graph of a million edges, read from Parquet in your browser.
-- .timer on shows how long each statement takes.
.timer on`,
      `CREATE VIEW rmat_edges AS SELECT * FROM 'rmat_1m_edges.parquet';`,
      `CREATE PROPERTY GRAPH rmat EDGE TABLES (rmat_edges SOURCE KEY (src) DESTINATION KEY (dst));`,
      `-- The graph's size: its nodes, edges, density and average degree.
FROM describe(rmat);`,
      `-- PageRank over all million edges.
SELECT id, round(pagerank, 6) AS pagerank FROM pagerank(rmat) ORDER BY pagerank DESC LIMIT 10;`,
      `-- A breadth-first search from vertex 0: how many vertices lie how many steps away.
SELECT steps, count(*) AS nodes
FROM GRAPH_TABLE (rmat
  MATCH p = ANY SHORTEST (a WHERE a.id = 0)-[e]->*(b)
  COLUMNS (path_length(p) AS steps))
GROUP BY steps
ORDER BY steps;
-- To draw all million edges, type .graph rmat: the layout takes a few seconds.`,
    ],
  },
];
