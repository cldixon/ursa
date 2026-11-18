# Ursa SQL: performance program and baseline

**Status:** updated 2026-11-18. Baseline through `ursa-cli` 0.10.0, including the first full-graph run and the first cross-library comparison (#378).
**How to run each harness:** `benchmarks/README.md` (the map), `bluesky_cloud/README.md`, `compare/README.md`.

This is the plan for measuring the SQL engine and the `ursa` CLI the way
the flywheel measured the Python DataFrame library, plus the baseline so far.
The baseline comes from four field-test rounds (0.6.0, 0.7.0, 0.9.0, 0.10.0),
the runs in AWS, and the first cross-library comparison.

---

## 1. What the repository has today

| Harness | Path | What it measures | Direction | Carry over? |
|---|---|---|---|---|
| **Flywheel** (cross-library) | `benchmarks/ursa_bench/` | Ursa (Python) vs NetworkX, rustworkx and igraph, one kernel at a time | DataFrame | **The method, yes; the code, partly.** Subprocess isolation, failures as data, a correctness oracle, a thread sweep, interleaved rounds, Parquet results, then a report. Its Ursa adapter is the Python API; add an `ursa-sql` adapter |
| **Criterion kernels** | `ursa-core/benches/`, `.github/workflows/bench.yml` (weekly) | The Rust kernels and CSR build on ER and R-MAT graphs, 10k–1M edges | Both | **As is.** Both front ends share these kernels |
| **Plan matrix** | `benchmarks/plan_matrix/` | Two builds of Ursa (A/B) over 21 plan-layer scenarios | DataFrame | **The A/B pattern, yes.** It needs SQL scenarios |
| **SQL scale** | `benchmarks/sql_scale/{gen,run,patterns,isolated}.py` | The `ursa` binary on seeded R-MAT, 1M–20M edges: first and second query, stored index, GRAPH_TABLE vs traversals, isolated vertices | SQL | **Yes.** It's the synthetic tier of the new program |
| **Bluesky, laptop** | `~/projects/ursa-bluesky` (outside the repo) | The Python wheel on Bluesky snapshots (95.7M, 367M, 983M) with `phys_footprint` | DataFrame | Superseded by the next row |
| **Bluesky, cloud** | `benchmarks/bluesky_cloud/` | The released `ursa` binary on the real Bluesky graph: 29 steps, 23 correctness checks, on a laptop, EC2 or Fargate | SQL | **New:** the real-data tier (T3) |
| **Cross-library comparison** | `benchmarks/compare/` (#378) | `ursa-sql`, `ursa-df` (DataFrame), igraph, rustworkx and NetworkX on Bluesky cuts of 5.1M–2.4B edges, checked against `ursa-sql`. NumPy throughout, so it scales where the flywheel stops | Both | **New:** the cross-engine tier (T4), without DuckPGQ so far |

Rendered reports are committed: `reports/ursa-sql-benchmark.html`, `bluesky_cloud/reports/*.md` and `compare/results/*.md`.

## 2. What to measure

Every number is per step, in its own process, with provenance: ursa version, git SHA, host, instance type or task size, and the date.

| Metric | Definition | Why it matters for SQL |
|---|---|---|
| **First query (cold)** | Wall time of the first query on a graph: scan the source, build the index, store it, run | What a user waits for. Today it's dominated by the serial source scan (N30) |
| **Next query (warm)** | Wall time in a new process with the index stored | The DuckDB-style experience: every `ursa -c` is a new process |
| **Kernel time** | Warm time minus the time of a `degree()` load-only step on the same graph | Separates the algorithm from index loading |
| **Peak memory** | Each process's peak: `ru_maxrss` on Linux, `phys_footprint` on macOS | The scale limit. It decides which machine a graph needs |
| **Bytes per edge** | Peak memory or index size ÷ edges | Extrapolates to bigger graphs (flat from 95.7M to 983M; §4.3) |
| **Index size** | Stored `.idx` bytes, out-adjacency only and after the reverse is added | Disk and the storage limit (N32) |
| **Build throughput** | Source rows read per second, and edges kept per second | Shows a scan-bound build (2.4B rows read to keep 95.7M edges) |
| **CPU use** | user + sys ÷ wall | 1.0 on 12 cores means serial: the clearest signal for N30 and Louvain's local moving |
| **Correctness** | The result against a cross-checked expected value (plain SQL, or the previous release) | A fast wrong answer is a failure, as in the flywheel |
| **Status** | ok, error, timeout, memory, unsupported | Failures are data |
| **Cost per run** (cloud) | Task or instance hours × price | Keeps automated runs honest |

**Workload families.** These are the step groups in `bluesky_cloud/run.py`, and the same names should go into `sql_scale`:
- index build and load;
- algorithms (PageRank, weak and strong components, triangles, Louvain), with and without a vertex table;
- traversals (`hop`, `shortest_path`);
- GRAPH_TABLE (a bound 1-edge and 2-edge pattern, a cycle, `ANY SHORTEST` to a target, `{1,3}` with a selector);
- plain SQL scans (DataFusion's own floor);
- agent-facing (`--format csv/json` on lists and nulls; a capped table).

## 3. Tiers and cadence

| Tier | What | Where | When | Gate? |
|---|---|---|---|---|
| T0 smoke | `run.py` on `smoke_data.py` (1M edges), all 31 steps; the flywheel `smoke` | CI (`ubuntu-24.04-arm`), seconds | every PR | **Yes:** every step runs |
| T1 kernels | criterion (`ursa-core`) | CI, weekly | weekly + on demand | no (noisy runners) |
| T2 synthetic scale | `sql_scale` R-MAT, 1M–100M edges, int and string ids, weighted, isolated vertices | EC2 r7g, or Fargate | each release + on demand | A/B against the previous release, beyond the noise floor |
| T3 real scale | `bluesky_cloud`: 95.7M with checks; 983M; 2.42B | Fargate (16 vCPU / 120 GB) or EC2 | each release (95.7M), monthly or on demand (full graph) | **Yes, for correctness:** 23 checks |
| T4 cross-engine | `compare/`: `ursa-sql`, `ursa-df`, igraph, rustworkx, NetworkX (DuckDB + DuckPGQ to add), on the same machine in the same run | laptop (5.1M), EC2 (95.7M, 983M) | each minor release | no; a leaderboard and a gaps list |

**Noise.**
- The plan matrix measured 20–25% swings on a shared 4-core VM. Repeat each T2/T3 step (`--repeat 3`, take the median) and run a second baseline to estimate the noise floor.
- Fargate's hardware generation isn't selectable, and its disk is slower than gp3 (§4.2). Use EC2 for numbers you'll publish, and Fargate for correctness and regression trends.

## 4. Baseline

### 4.1 Release history on the 95.7M-edge graph (laptop)

Graph `bsky23`: follows before 2024, 95,744,886 edges, with the 39.65M-user vertex table. The machine is an Apple M-series with 12 cores and 32 GB. Memory is macOS "peak memory footprint". Each query runs in its own process with a stored index unless noted.

| Query | 0.7.0 | 0.9.0 | 0.10.0 |
|---|---|---|---|
| Index build (first query) | 30.7 s / 3.1 GB | 30.3 s / 3.0 GB | **10.3 s** / 3.76 GB (parallel decode, N30) |
| Index build, edges only | — | 28.1 s / 2.04 GB | **6.0 s** / 2.87 GB |
| PageRank | 6.9 s / 4.83 GB | 3.6 s / 3.47 GB | 2.9 s / 3.67 GB |
| Weak components | 2.3 s / 2.21 GB | 2.6 s / 2.11 GB | 2.3 s / 2.25 GB |
| Triangles | 11.9 s / 4.70 GB | 12.5 s / 4.44 GB | 11.3 s / 4.44 GB |
| Louvain | 42.7 s / 13.5 GB | 18.6 s / 7.06 GB | **14.6 s / 4.48 GB** (N31) |
| Louvain, edges only | — | 12.7 s / 6.17 GB | 12.0 s / **3.23 GB** |
| 1-edge MATCH from a bound node | 14.0 s | 0.57–0.92 s | 0.53–0.91 s |
| Mutual follows (2-edge cycle) | 12.8 s | 1.09 s | 0.98 s |
| `ANY SHORTEST` to a fixed target | 12.6 s | 1.01 s | 0.91 s |
| `{1,3}` distance histogram | 9.6 s | 1.69 s | 1.63 s |

0.9.0 and 0.10.0 times include process start and index load (about 0.9 s). The 0.6.0 numbers aren't comparable, because 0.6.0 left vertex-table nodes with no edges out of the node set. They're in `sql-cli-0.6.0-test-findings.md`.

### 4.2 Platforms, ursa 0.9.0, the same graph (2026-11-17)

`bluesky_cloud/run.py --before 2024-01-01`, one run each. All 23 checked steps pass on every platform. `g` has the vertex table; `ge` is edges only.

| Step | Mac (M-series, 12c, 32 GB) | EC2 r7g.4xlarge (Docker, gp3) | Fargate 16 vCPU / 120 GB arm64 |
|---|---|---|---|
| Index build, `g` (inside the process) | 27.4 s | 40.8 s | 35.8 s (54.9 s wall) |
| Index build, `ge` | 25.6 s | 39.1 s | 34.9 s (45.9 s wall) |
| PageRank, `g` | 4.11 s | 6.58 s | 15.92 s |
| PageRank, `ge` | 3.05 s | 4.47 s | 7.97 s |
| Weak / strong components, `g` | 2.24 / 3.79 s | 4.31 / 6.36 s | 3.46 / 6.06 s |
| Triangles, `g` | 11.94 s | 10.53 s | 10.29 s |
| Louvain, `g` / `ge` | 15.33 / 12.01 s | 26.80 / 20.39 s | 21.48 / 15.47 s |
| 1-edge MATCH, `g` | 0.87 s | 1.50 s | 1.25 s |
| Mutuals | 0.98 s | 1.75 s | 1.45 s |
| `ANY SHORTEST` | 0.88 s | 1.55 s | 1.25 s |
| `{1,3}` histogram | 1.57 s | 3.20 s | 2.40 s |
| Whole run (29 steps) | ~3 min | ~4 min | 4.7 min (incl. 33 s S3 download) |
| Peak memory, largest step | Louvain `ge`, 6.2 GB (RSS, overstated) | Louvain `g`, 6.73 GB | Louvain `g`, 6.73 GB |
| Cost of the run | — | ~$0.06 | ~$0.08 |

**What it says:**
- **The serial build makes the cloud slower than the laptop.** It runs on one core (N30), and a Graviton3 core is slower than an M-series core. Triangles, the one well-parallelised kernel, is the only step the 16-core machines win.
- **Fargate's disk is slower.** Steps that read the stored index heavily, such as PageRank adding the reverse index, take 2–4× longer than on EC2. Its other times vary run to run; one run each isn't enough to rank Fargate against EC2.
- **The run is cheap enough to automate:** a checked 95.7M-edge run costs under ten cents.

### 4.3 Scaling to the full graph (laptop 0.9.0; EC2 0.10.0)

| | 95.7M edges | 983M edges | Per edge (95.7M → 983M) |
|---|---|---|---|
| Build peak | 2.04 GB | 17.4 GB | 21.2 → 17.7 B |
| Stored index, out-adjacency | 768 MiB | 7.6 GiB | 8.4 → 8.3 B |
| Stored index, with reverse | 1.5 GiB | 16.2 GB | 16.8 → 16.5 B |
| Load + weak components | 1.01 GB | 9.79 GB | 10.5 → 10.0 B |
| PageRank | 1.92 GB | 18.8 GB | 20.0 → 19.1 B |
| Build time (unfiltered file) | 1.6 s | 28.6 s | 60M → 34M edges/s |

Memory per edge is flat, so extrapolating linearly to the full 2.42B-edge graph (with its vertex table) is reasonable:

| Full graph | Estimate | 32 GB | 64 GB | Fargate 120 GB |
|---|---|---|---|---|
| Build | ~44 GB | no | yes | yes |
| Stored index (disk) | ~41 GB | — | — | 150 GiB disk: yes |
| Components, traversals, MATCH | ~25–27 GB | borderline | yes | yes |
| PageRank | ~47–49 GB | no | yes | yes |
| Triangles | ~75 GB | no | no | yes |
| Louvain (0.9.0) | ~155 GB | no | no | **no** (r7g.8xlarge, 256 GiB) |
| Louvain after N31 (estimate) | ~80 GB | no | no | yes |

**Measured (2026-11-18, ursa 0.10.0, EC2 r7g.4xlarge, 70 min, about $1.10).** Every peak came in at or under the estimate:

| Full graph, 2.42B edges | Estimate | Measured | Wall | Cores busy |
|---|---|---|---|---|
| Build | ~44 GB | 44.6 GB | 90 s (build 40.8 s) | 2.6 |
| Stored index (disk) | ~41 GB | 18.6 GiB; 36.9 GiB with the reverse | — | — |
| Components, traversals, MATCH | ~25–27 GB | 22.5 GB | 63–82 s (components); ~16 s (point queries) | 1.0–1.3 |
| PageRank | ~47–49 GB | 42.4 GB | 115 s | 8.0 |
| Triangles | ~75 GB | 65.1 GB | 30.5 min | 15.5 |
| Louvain | ~80 GB after N31 | 73.9 GB | 27 min | 1.5 |

The report is `bluesky_cloud/reports/2026-11-18-ec2-r7g.4xlarge-full-0.10.0.md`. At this size, time is the limit, not memory. Triangles grow with the hubs. Louvain and the components run near one core (N52). Every query pays a 15 s single-core index load (N51). The `{1,3}` histogram hits a 10M-path cap that `ANY SHORTEST` shouldn't need (N50).

### 4.4 Where the time goes (profiles, 0.9.0 and 0.10.0)

- **Index build** (macOS `sample`, 20 s): 0.8 cores busy on 12. Parquet zstd + decode 74%, I/O 7%, id interning 9%, filter 2%, CSR under 1%. A parallel SQL scan of the same rows takes 5.3 s against the build's 28 s, so a parallel source scan should bring the first query from ~30 s to ~5–6 s (N30).
- **Louvain** (footprint timeline + `sample`): a 2 s spike to 5.9 GB in `Graph::from_topology`, where `Vec<(u32, f64)>` is padded to 16 B per entry (3.06 GB) and is alive alongside `targets` + `wts` (2.3 GB). The fix should give 5.9 → ~3.2 GB with bit-identical results (N31). The local-moving phase (10 s) runs on one thread.
- **0.10.0, after the fixes.**
  - The vertex-table build runs at 3.7 cores. Parquet decode is ~56% of its CPU and I/O ~25%. The remaining serial phase is reading the vertex keys before the edges: +4.3 s from a `.csv.gz`, +1.1 s from Parquet (N49).
  - Louvain's spike is gone: one step up to 3.08 GB, then flat (edges only).

### 4.5 Cross-library, first result (laptop, `bsky-5m`, 2026-11-18)

`compare/` on the Bluesky follows before 2023-06-01: 5,119,823 deduplicated edges, 107,689 nodes. Every engine agrees with `ursa-sql` on every algorithm it has. Cold is file to result; peak is the cell's own process. Full table: `compare/results/2026-11-18-laptop-bsky-5m.md`.

| Cold / peak | ursa-df | ursa-sql | igraph 1.0.0 | rustworkx 0.18.1 | NetworkX 3.7 |
|---|---|---|---|---|---|
| PageRank | **0.17 s** / 0.20 GB | 0.34 s / 0.24 GB | 1.21 s / 1.07 GB | 1.70 s / 0.99 GB | 6.79 s / 2.30 GB |
| Strong components | **0.12 s** | 0.29 s | 0.91 s | 1.21 s | 5.37 s |
| Triangles | **1.26 s** | 1.40 s | unsupported | unsupported | 35.3 s |
| Louvain | **0.50 s** | 0.66 s | 15.4 s | unsupported | 50.0 s |

- At this size the DataFrame API is a little faster than SQL. SQL starts a process and loads its stored index for each query (~60 ms here).
- Both Ursa engines use about a fifth of the others' memory.
- Two fairness fixes the first run found:
  - NetworkX and rustworkx stop PageRank at an L1 change below `n * tol`, so they get `tol / n`.
  - Library imports happen before the clock starts.

## 5. The gaps list (the flywheel's output)

As in the flywheel, the program's product is a to-do list. Today's, from the reports and the AWS runs:

Fixed in 0.10.0: N29 (build notice), N30 (serial build), N31 (Louvain memory), N32 (storage limit), N34 / N35 (CSV lists, JSON nulls). Open, from the 0.10.0 report:

| Gap | Kind | Evidence | Expected gain |
|---|---|---|---|
| N44: one view over a missing file makes the database unopenable | reliability | even `DROP VIEW` and `--readonly` fail; only editing the catalog JSON fixes it | moved files no longer lock users out |
| N51: every query loads the whole index on one core | perf at scale | full graph: 15.3 s at 1.0 cores, so a 1-edge MATCH takes 16 s | memory-map the index: point queries well under a second |
| N52: components and Louvain stay near one core | perf at scale | full graph: components 63–82 s at 1.2–1.3 cores; Louvain 27 min at 1.5 | parallel components (identical results); deterministic parallel Louvain |
| N50: a 10M-path cap refuses a valid `ANY SHORTEST` | correctness | 3 steps from one account reach more than 10M nodes; the error suggests the selector the query already uses | count destinations, not paths; make the cap a setting |
| N49: vertex keys are read serially, before the edges | perf | +4.3 s (`.csv.gz`) / +1.1 s (Parquet) of a 10.3 s build | overlap with the edge scan |
| N43: CSV text of nested values is ambiguous for strings | agent usability | `['a, b','c']` and `['a','b','c']` both print `"[a, b, c]"` | quote string elements |
| Cloud disk sensitivity | ops | Fargate PageRank 15.9 s vs EC2 6.6 s | use gp3/EC2 for published numbers |
| DuckDB + DuckPGQ in the comparison | program | not in `compare/` yet | the SQL/PGQ peer to compare against |

## 6. Next steps, in order

Done: the full-graph run (§4.3), the cross-library harness and its first result (§4.5), committed reports, and T0 in CI (`bench-smoke.yml`).

1. **T4 at scale (#378):** `compare/` on `bsky-96m` (EC2 r7g.4xlarge, ~$2), then `bsky-983m` (r7g.8xlarge Spot, ~$1–3), each approved with a budget.
2. **DuckDB + DuckPGQ** as a `compare/` engine, the SQL/PGQ peer.
3. **One results store.** Every `results.json` in `s3://…/results/`, plus a small `report.py` that renders §4.1–4.5 from them: a release trend, a platform comparison, the cross-library leaderboard, and the gaps list.
4. **T2 in the cloud:** run `sql_scale` (R-MAT 10M–100M, int/string ids, weighted, isolated) on the same machines, with `--repeat 3` and a second baseline for the noise floor.
5. **The GitHub Actions workflow (`bluesky-bench.yml`):** create the `bluesky-bench` environment with a required reviewer, then trigger it on each release.

## Sources

- The field-test reports (outside the repo): `sql-cli-0.6.0-test-findings.md`, `sql-cli-0.7.0-retest.md`, `sql-cli-0.9.0-retest.md`, `sql-cli-0.10.0-retest.md`
- `benchmarks/bluesky_cloud/reports/` (every run since 2026-11-17) and `benchmarks/compare/results/`
- `benchmarks/README.md`, `benchmarks/sql_scale/*.py`, `benchmarks/plan_matrix/`
