# Cross-library comparison at Bluesky scale (#378)

Ursa's two entry points against the graph libraries the July flywheel raced, on the Bluesky follows graph at four sizes:

| Engine | Package | How it runs |
|---|---|---|
| `ursa-sql` | `ursa-cli` 0.10.0, the released binary | `CREATE PROPERTY GRAPH` + table functions; one `ursa` process per query; index stored in a `.ursa` database |
| `ursa-df` | `ursa-graph` 0.5.0, the PyPI wheel | `ur.scan_edges(...)` + `ur.pagerank(...)` etc., `.collect()` |
| `igraph` | `igraph` 1.0.0 | `Graph(n, edges)` from a NumPy array |
| `rustworkx` | `rustworkx` 0.18.1 | `PyDiGraph` + `extend_from_edge_list` |
| `networkx` | `networkx` 3.7 | `DiGraph` + `add_edges_from` |

`benchmarks/ursa_bench` (the flywheel) has the same method: a process per cell, failures recorded as data, a correctness check. But it passes edges and results as Python lists and dicts, so it stops near 1M edges. This runner keeps NumPy arrays throughout.

## Files

| File | What it does |
|---|---|
| `prep.py` | One graph size: keep follows before a date, drop self-loops and duplicates, and write `edges.parquet` (for both Ursa engines) and `dense.npz` (dense ids, for the Python libraries). Charged to no engine |
| `cell.py` | One measured cell: an engine runs one algorithm, in its own process. Prints JSON measurements and writes the result |
| `compare.py` | Runs every engine × algorithm on a prepared graph with a time limit and memory ceiling per cell, checks results against `ursa-sql`, and writes `results.json` and `summary.md` |
| `ec2-run.sh` | On an instance from `../bluesky_cloud/bench-cloud.sh ec2-up`: install the pinned libraries, prepare sizes from `/work/data`, run, upload to S3 |
| `Dockerfile` | The same as an image, for Fargate and CI |

## What is measured

**Graph sizes:**

| Size | Follows before | Edges, deduplicated |
|---|---|---|
| `bsky-5m` | 2023-06-01 | 5.1M (107,689 nodes) |
| `bsky-96m` | 2024-01-01 | ~95M |
| `bsky-983m` | 2024-11-25 | ~980M |
| `bsky-full` | no cutoff | ~2.4B (Ursa engines only) |

**Algorithms**, with the same parameters for every engine:
- degree (out);
- PageRank (damping 0.85, `max_iter` 100, convergence at an L1 change of 1e-6);
- weak and strong components;
- per-node triangles (undirected);
- Louvain (undirected, seed 1).

igraph and rustworkx have no per-node triangle count, and rustworkx has no Louvain. Those cells are `unsupported`.

**Per cell:**
- **ingest:** file to graph handle;
- **cold:** ingest plus the first run;
- **warm:** a second run on the same handle, skipped when the first takes more than `--warm-limit`. For `ursa-sql`, warm is a new `ursa` process with the index stored, so it includes loading the index (ingest is the process that builds and stores it);
- **peak RSS, and cores busy** ((user + sys) ÷ wall);
- **status:** `ok`, `unsupported`, `error`, `timeout`, `memory`.

**Correctness**, against `ursa-sql`:
- degree and triangles exactly;
- PageRank within 1e-3 of the top score;
- components as the same partition;
- Louvain by modularity on the undirected simple graph, at least the reference's minus 0.05.

**Parameters that mean different things.** NetworkX and rustworkx stop PageRank when the L1 change is below `n * tol`, while Ursa stops below `tol`. So they get `tol / n`; with `tol = 1e-6` as written, they stopped after a few iterations on 50k nodes. Library imports happen before the clock starts: NetworkX loads SciPy lazily inside `nx.pagerank`.

## Results and CI

- **Results:** each run's `summary.md` is committed to [`results/`](results/), named `<date>-<machine>-<size>.md`; the raw `results.json` stays with the run. The first is `results/2026-11-18-laptop-bsky-5m.md`.
- **CI:** `.github/workflows/bench-smoke.yml` runs every engine on a synthetic graph when these files change, and checks they all agree with `ursa-sql`.

## Run it

**Laptop:**

```bash
uv venv /tmp/cmpvenv && uv pip install -p /tmp/cmpvenv/bin/python \
    "ursa-graph==0.5.0" "igraph==1.0.0" "networkx==3.7" "rustworkx==0.18.1" pyarrow numpy scipy
/tmp/cmpvenv/bin/python prep.py --source ~/Downloads/ICPSR_300499-V1.1/deidentified_follows_edgelist.parquet \
    --before 2023-06-01 --out /tmp/cmp/bsky-5m
/tmp/cmpvenv/bin/python compare.py --graph /tmp/cmp/bsky-5m --out /tmp/cmp/bsky-5m-out \
    --timeout 900 --mem-limit-gb 16
```

**AWS.** Each run is approved first, with a budget. From `../bluesky_cloud`, with `AWS_PROFILE` and `BENCH_ACCOUNT` set:

```bash
aws s3 cp --recursive --exclude "*" --include "*.py" --include "*.sh" ../compare s3://$BUCKET/data/tools/compare/
./bench-cloud.sh ec2-up r7g.4xlarge          # the first boot copies data/ (code included) to /work/data
# on the instance, detached so it doesn't depend on the SSM session:
#   setsid nohup bash /work/data/tools/compare/ec2-run.sh $BUCKET cmp-96m "bsky-96m" \
#       --timeout 2700 --mem-limit-gb 110 > /work/cmp.log 2>&1 < /dev/null &
./bench-cloud.sh results cmp-96m
./bench-cloud.sh ec2-down
```
