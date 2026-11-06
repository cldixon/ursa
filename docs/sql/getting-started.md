# Getting started with Ursa SQL

Ursa is a SQL engine for graph analytics. It reads Parquet and CSV files where they are, on your disk or in object storage, and runs graph algorithms over them with SQL. There is no server and nothing to load first. An edge table is a graph.

## Install

On Linux or macOS, run the install script:

```bash
curl -fsSL https://cldixon.github.io/ursa/install.sh | sh
```

It downloads the newest `ursa` for your machine from the [releases page](https://github.com/cldixon/ursa/releases), checks it against its SHA-256 checksum, and puts it in `~/.local/bin`. It needs no `sudo`. If `~/.local/bin` is not on your `PATH`, it prints the line to add. To choose the version or the directory, set `URSA_VERSION` or `URSA_INSTALL_DIR` after the pipe:

```bash
curl -fsSL https://cldixon.github.io/ursa/install.sh | URSA_VERSION=0.6.0 URSA_INSTALL_DIR=/opt/bin sh
```

On Windows, in PowerShell. This installs to `%LOCALAPPDATA%\ursa\bin` and adds it to your user `PATH`:

```powershell
irm https://cldixon.github.io/ursa/install.ps1 | iex
```

The prebuilt binaries cover Linux (x86_64 and aarch64, glibc 2.35 or later), macOS (Intel and Apple silicon) and Windows (x64). On anything else, install with Cargo. You need Rust 1.94 or later:

```bash
cargo install ursa-cli
```

Or download a prebuilt binary yourself from the [releases page](https://github.com/cldixon/ursa/releases). Each release has one archive per platform:

| Platform              | Archive                                              |
| --------------------- | ---------------------------------------------------- |
| Linux x86_64          | `ursa-cli-v0.6.0-x86_64-unknown-linux-gnu.tar.gz`    |
| Linux aarch64         | `ursa-cli-v0.6.0-aarch64-unknown-linux-gnu.tar.gz`   |
| macOS (Intel)         | `ursa-cli-v0.6.0-x86_64-apple-darwin.tar.gz`         |
| macOS (Apple silicon) | `ursa-cli-v0.6.0-aarch64-apple-darwin.tar.gz`        |
| Windows x64           | `ursa-cli-v0.6.0-x86_64-pc-windows-msvc.zip`         |

Each archive has a `.sha256` file next to it. For example, on an Apple silicon Mac:

```bash
curl -LO https://github.com/cldixon/ursa/releases/download/ursa-cli-v0.6.0/ursa-cli-v0.6.0-aarch64-apple-darwin.tar.gz
tar xzf ursa-cli-v0.6.0-aarch64-apple-darwin.tar.gz
sudo mv ursa-cli-v0.6.0-aarch64-apple-darwin/ursa /usr/local/bin/
```

The binary is called `ursa`. Check it runs:

```bash
ursa --version
```

```text
ursa 0.6.0
```

## Query a file

This guide uses Zachary's karate club: 34 members of a club, and 78 friendships between them. Download the two files:

```bash
curl -LO https://raw.githubusercontent.com/cldixon/ursa/main/python/ursa/datasets/data/karate_edges.csv
curl -LO https://raw.githubusercontent.com/cldixon/ursa/main/python/ursa/datasets/data/karate_nodes.csv
```

Any file is a table. Name it in `FROM`:

```bash
ursa -c "SELECT * FROM 'karate_edges.csv' LIMIT 3"
```

```text
+-----+-----+
| src | dst |
+-----+-----+
| 0   | 1   |
| 0   | 2   |
| 0   | 3   |
+-----+-----+
```

The format comes from the extension: `.parquet`, `.csv`, and compressed CSV (`.csv.gz`, `.csv.zst`). A directory or a glob reads every file in it: `'edges/*.parquet'`.

## Open a database

Start the shell with a database file:

```bash
ursa club.ursa
```

The file is created if it does not exist. It keeps the tables, views and graphs you define, and the graph indexes Ursa builds, so the next session (or the next `ursa -c` against the same file) starts where you left off. Without a file name, `ursa` runs in memory and forgets everything when it exits. [The database file](database.md) has the details.

The rest of this guide runs in that shell.

## Define a graph

First, name the files as tables:

```sql
CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv';
CREATE VIEW ties    AS SELECT * FROM 'karate_edges.csv';
```

A view reads the file each time it is queried, so it always sees the file's current contents. Nothing is copied into the database.

Then define the graph. Ursa uses the `CREATE PROPERTY GRAPH` statement from the SQL standard (SQL/PGQ):

```sql
CREATE PROPERTY GRAPH karate
  VERTEX TABLES (members KEY (id))
  EDGE TABLES (
    ties SOURCE KEY (src) REFERENCES members (id)
         DESTINATION KEY (dst) REFERENCES members (id)
  );
```

Every row of `ties` is an edge from `src` to `dst`. Every row of `members` is a node, keyed by `id`.

The node table is optional. Without it, the nodes are the endpoints of the edges:

```sql
CREATE PROPERTY GRAPH karate_edges_only
  EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));
```

A graph is a schema with two tables. `karate.edges` holds the edges, and `karate.nodes` holds one row per node with its attributes:

```sql
SELECT * FROM karate.nodes LIMIT 3;
```

```text
+----+--------+
| id | club   |
+----+--------+
| 0  | Mr. Hi |
| 1  | Mr. Hi |
| 2  | Mr. Hi |
+----+--------+
```

## Run an algorithm

An algorithm is a table function. Its first argument is the graph. It returns one row per node: the node's `id`, and a column named after the algorithm.

```sql
SELECT id, round(pagerank, 4) AS pagerank
FROM pagerank(karate, direction => 'both')
ORDER BY pagerank DESC
LIMIT 5;
```

```text
+----+----------+
| id | pagerank |
+----+----------+
| 33 | 0.1009   |
| 0  | 0.097    |
| 32 | 0.0717   |
| 2  | 0.0571   |
| 1  | 0.0529   |
+----+----------+
```

Parameters are named, with `=>`:

```sql
SELECT * FROM louvain(karate, seed => 1);
SELECT * FROM pagerank(karate, damping => 0.9, direction => 'both');
```

## Combine algorithms and attributes

`algorithms` runs several algorithms over one graph and returns one row per node with a column for each. Name a column with `name =>`; otherwise it is named after the algorithm. Join it to the node table on `id`. This query finds the five best-connected members, with their club, how many triangles they are part of, and their community:

```sql
SELECT id, club, degree, triangles, community
FROM karate.nodes
JOIN algorithms(karate,
                degree(direction => 'both'),
                triangles => triangle_count(),
                community => louvain(seed => 1)) USING (id)
ORDER BY degree DESC
LIMIT 5;
```

```text
+----+---------+--------+-----------+-----------+
| id | club    | degree | triangles | community |
+----+---------+--------+-----------+-----------+
| 33 | Officer | 17     | 15        | 2         |
| 0  | Mr. Hi  | 16     | 18        | 0         |
| 32 | Officer | 12     | 13        | 2         |
| 2  | Mr. Hi  | 10     | 11        | 0         |
| 1  | Mr. Hi  | 9      | 12        | 0         |
+----+---------+--------+-----------+-----------+
```

Each algorithm also works on its own, as in the previous section, and those results join on `id` too.

All three algorithms run over one index of the graph. Ursa builds it the first time a query needs it and stores it in `club.ursa`. The next query, in this session or a later one, uses the stored index. If `karate_edges.csv` changes, Ursa notices and builds a new one.

## Aggregate over neighbours

`neighbors_agg` aggregates a value of each node over its neighbours. It returns one row per node. Take the karate members again. Some of them have friends in both clubs. This query counts them, by club:

```sql
SELECT club, count(*) AS members
FROM karate.nodes
JOIN neighbors_agg(karate, attribute => club, agg => 'n_unique', direction => 'both') USING (id)
WHERE neighbors_agg = 2
GROUP BY club
ORDER BY club;
```

```text
+---------+---------+
| club    | members |
+---------+---------+
| Mr. Hi  | 6       |
| Officer | 7       |
+---------+---------+
```

There are two ways to give the value of each node:

- `attribute =>` takes an expression over the graph's node table.
- `values =>` takes any query of two columns, node ids then values. The values can even come from another algorithm, like each member's friends' mean degree:

```sql
SELECT id, round(neighbors_agg, 2) AS friends_degree
FROM neighbors_agg(karate,
                   values => (SELECT id, degree FROM degree(karate, direction => 'both')),
                   agg => 'mean', direction => 'both')
ORDER BY friends_degree DESC, id
LIMIT 3;
```

```text
+----+----------------+
| id | friends_degree |
+----+----------------+
| 11 | 16.0           |
| 14 | 14.5           |
| 15 | 14.5           |
+----+----------------+
```

The aggregations are `mean` (the default), `sum`, `min`, `max`, `count` and `n_unique`. Strings take only `count` and `n_unique`. A node whose neighbours have no values gets NULL for `mean`, `min` and `max`, and 0 for the others.

## Direction

Every edge is stored from `src` to `dst`. Algorithms that walk edges take a `direction`:

| Value                 | Follows                   |
| --------------------- | ------------------------- |
| `'out'` (the default) | edges from a node         |
| `'in'`                | edges into a node         |
| `'both'`              | edges in either direction |

The karate edges are stored once per friendship, so `direction => 'both'` gives the undirected answer. Some algorithms only make sense one way and take no `direction`: `triangle_count`, `clustering_coefficient`, `louvain` and `label_propagation` read the graph as undirected.

## Weights

A weight is a column of the edge table, or an expression over its columns. Les Misérables has a `weight` column counting how often two characters appear together:

```sql
CREATE VIEW lesmis_edges AS SELECT * FROM 'lesmis_edges.csv';
CREATE PROPERTY GRAPH lesmis
  EDGE TABLES (lesmis_edges SOURCE KEY (src) DESTINATION KEY (dst));

SELECT * FROM pagerank(lesmis, weight => weight, direction => 'both')
ORDER BY pagerank DESC
LIMIT 5;
```

```text
+----------+----------------------+
| id       | pagerank             |
+----------+----------------------+
| Valjean  | 0.09956068175644767  |
| Marius   | 0.051667656904091475 |
| Myriel   | 0.03923416880292588  |
| Cosette  | 0.03691018363299517  |
| Enjolras | 0.036614491459848165 |
+----------+----------------------+
```

The weight can be any expression over the edge columns:

```sql
SELECT * FROM louvain(lesmis, weight => log10(1 + weight), seed => 7)
ORDER BY id
LIMIT 5;
```

```text
+------------+---------+
| id         | louvain |
+------------+---------+
| Anzelma    | 3       |
| Babet      | 3       |
| Bahorel    | 5       |
| Bamatabois | 1       |
| BaronessT  | 4       |
+------------+---------+
```

Weights must be non-negative and not null. The graph index does not depend on the weight, so trying several weightings reuses one index.

## Subgraphs

A subgraph is a graph over a view that filters the edges:

```sql
CREATE VIEW strong_ties AS SELECT * FROM lesmis_edges WHERE weight >= 3;

CREATE PROPERTY GRAPH lesmis_strong
  EDGE TABLES (strong_ties SOURCE KEY (src) DESTINATION KEY (dst));

SELECT count(DISTINCT connected_components) AS groups
FROM connected_components(lesmis_strong);
```

```text
+--------+
| groups |
+--------+
| 3      |
+--------+
```

The nodes of a subgraph are the endpoints of the edges it keeps. A character whose every tie is weaker than 3 is not in `lesmis_strong`.

## Traversals

Traversals are table functions too, and they return edges. `hop` returns the edges reached within `n` steps of a set of seed nodes; the seeds can be a list or a query. Member 0 has 16 friends, so this returns 16 rows:

```sql
SELECT * FROM hop(karate, seeds => [0], n => 1, direction => 'both');

SELECT * FROM hop(karate,
                  seeds => (SELECT id FROM karate.nodes WHERE club = 'Officer'),
                  n => 2);
```

The second query returns the 39 edges reachable within two steps of an Officer member, following edges in their stored direction. The seeds query runs as part of the same plan, so a filter in it is pushed into the scan of `karate_nodes.csv`. A seed that is not in the graph reaches nothing.

`random_walk` returns walks as rows: one per step, with the walk's number and position. Give it a `seed` for walks you can reproduce:

```sql
SELECT * FROM random_walk(karate, start => [0, 1], steps => 4, walks_per_node => 2, seed => 7);
```

`shortest_path` returns the path between two nodes, one edge per row, with its step number and the cost so far:

```sql
SELECT * FROM shortest_path(karate, source => 0, target => 33, direction => 'both');
```

```text
+-----+-----+-----+------+
| src | dst | hop | cost |
+-----+-----+-----+------+
| 0   | 8   | 0   | 1.0  |
| 8   | 33  | 1   | 2.0  |
+-----+-----+-----+------+
```

## Files in object storage

Anything that works with a local path works with a URL. Credentials come from the standard environment variables (`AWS_*`, `GOOGLE_*`, `AZURE_*`) or the usual config files (see [Credentials](#credentials) below):

<!-- skip -->
```sql
CREATE VIEW follows AS
  SELECT * FROM 's3://my-bucket/social/follows/*.parquet';

CREATE PROPERTY GRAPH social
  EDGE TABLES (follows SOURCE KEY (follower) DESTINATION KEY (followee));

SELECT * FROM pagerank(social) ORDER BY pagerank DESC LIMIT 10;
```

`gs://`, `az://` and `https://` work the same way. Ursa reads only the columns a query needs, and stores the graph index in the database file, so the second query does not read the edges again.

### Credentials

Ursa finds credentials where each cloud's own tools do. It looks in the places below in order, and uses the first that has credentials. Nothing is stored in the database file.

**S3** (`s3://`):

1. Environment variables: `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, plus `AWS_SESSION_TOKEN` for temporary credentials.
2. An AWS profile in `~/.aws/credentials` and `~/.aws/config`. Ursa reads the profile that `AWS_PROFILE` names, or `default`. The profile can hold:
    - access keys;
    - an SSO login (run `aws sso login` first);
    - a role to assume (`role_arn` with `source_profile`);
    - a `credential_process`.

    `AWS_SHARED_CREDENTIALS_FILE` and `AWS_CONFIG_FILE` point to other files. If `AWS_PROFILE` names a profile that has no credentials, the query fails. Ursa doesn't go on to the sources below.
3. Web identity: `AWS_WEB_IDENTITY_TOKEN_FILE` and `AWS_ROLE_ARN`, as on EKS.
4. Container credentials: `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` or `AWS_CONTAINER_CREDENTIALS_FULL_URI`, as on ECS.
5. The instance's role, from the EC2 instance metadata service.

```bash
AWS_PROFILE=analytics ursa -c "SELECT count(*) FROM 's3://my-bucket/follows/*.parquet'"
```

The region comes from `AWS_REGION` or `AWS_DEFAULT_REGION`. If neither is set, Ursa asks S3 where the bucket is. If S3 doesn't answer, Ursa uses the profile's `region`.

Some settings for other cases:

- **An S3-compatible store** (MinIO, R2): set `AWS_ENDPOINT`. For a plain `http://` endpoint, also set `AWS_ALLOW_HTTP=true`.
- **A public bucket**: set `AWS_SKIP_SIGNATURE=true` and no credentials are needed.

Ursa reads profiles through `aws-config`, the AWS SDK's own configuration loader, in the `aws-profile` feature of `ursa-cli`. The feature is on by default. A build without it (`--no-default-features`) skips step 2.

**Google Cloud Storage** (`gs://`):

1. A service account key: a file named by `GOOGLE_SERVICE_ACCOUNT` (or `GOOGLE_SERVICE_ACCOUNT_PATH`), or the key's JSON in `GOOGLE_SERVICE_ACCOUNT_KEY`.
2. Application default credentials: the file `GOOGLE_APPLICATION_CREDENTIALS` names, or the one `gcloud auth application-default login` writes to `~/.config/gcloud/`.
3. The instance's service account, from the GCE metadata server.

**Azure Blob Storage** (`az://`, `abfs://`, `abfss://`, `adl://`). Set the account with `AZURE_STORAGE_ACCOUNT_NAME`, then the credentials:

1. An account key: `AZURE_STORAGE_ACCOUNT_KEY`.
2. Workload identity: `AZURE_CLIENT_ID`, `AZURE_TENANT_ID` and `AZURE_FEDERATED_TOKEN_FILE`, as on AKS.
3. A service principal: `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` and `AZURE_TENANT_ID`.
4. A SAS token: `AZURE_STORAGE_SAS_KEY`.
5. The Azure CLI's login, when `AZURE_USE_AZURE_CLI=true`.
6. A managed identity, from the instance metadata service.

**HTTP** (`https://`, `http://`) URLs are read without credentials.

## Output

The shell prints tables. From the command line, choose a format with `--format`:

```bash
ursa club.ursa --format json -c "SELECT * FROM pagerank(karate) ORDER BY pagerank DESC LIMIT 3"
ursa club.ursa --format csv -c "SELECT * FROM louvain(karate, seed => 1)" > communities.csv
```

The formats are `table` (the default), `csv`, `tsv`, `json`, `ndjson` and `arrow`. To write a file from SQL, use `COPY`:

```sql
COPY (SELECT * FROM pagerank(karate)) TO 'pagerank.parquet';
```

## Find out what is available

```sql
SHOW GRAPHS;
DESCRIBE GRAPH karate;
SELECT * FROM ursa_functions();
```

`ursa_functions()` lists every graph function with its parameters, their defaults, the NetworkX function it matches, and a one-line description.

## Next steps

- The [function reference](reference/functions.md) lists every function and exactly what it computes.
- [Statements](statements.md) covers defining, describing and dropping graphs, `ATTACH` and the settings.
- [The shell](shell.md) covers dot-commands, init files and history.
- The [Python library](../index.md) runs the same algorithms with a DataFrame API, and gives the same values.
