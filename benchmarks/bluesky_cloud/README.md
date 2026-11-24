# Bluesky benchmark in the cloud

The `ursa` binary on the Bluesky follows graph (ICPSR 300499: 2.42B edges, 39.65M
users). This graph is too big to build on a 32 GB laptop, so the run happens on
a large AWS machine:

1. **EC2, for live work.** Start an instance, open a shell, and debug and experiment
   with the same image and data.
2. **Fargate, for repeatable runs.** One `ursa` release, one task, results in S3.
   Started from a laptop (`bench-cloud.sh run`) or from GitHub Actions
   (`.github/workflows/bluesky-bench.yml`).

Both use the same container image, `run.py` inside it, and the same data in S3.
So what works on EC2 runs unchanged on Fargate.

## Files

| File | What it is |
|---|---|
| `run.py` | The benchmark. Every step is its own `ursa` process, with wall/user/sys time and peak RSS (`wait4`), notices, and correctness checks. Standard library only; `boto3` for `s3://` |
| `smoke_data.py` | Writes a small synthetic stand-in for the two data files, for smoke tests |
| `Dockerfile` | `debian:bookworm-slim`, a released `ursa` installed with the public install script, and `run.py` |
| `stack.yml` | CloudFormation: S3 bucket, ECR, VPC, ECS cluster, Fargate task definition, IAM roles, GitHub OIDC role |
| `bench-cloud.sh` | Deploy, push the image, upload the data, run on Fargate, fetch results, start and stop the EC2 instance |
| `ec2-user-data.sh` | The EC2 instance's first boot: Docker, the image, the data, and `ursa` |

## The benchmark (`run.py`)

**Graphs.** With `--before DATE`, only follows before DATE are kept; without it, the whole graph:
- `g`: the follows with the users as a vertex table (every user is a node);
- `ge`: the same follows with no vertex table.

**Steps.** `python3 run.py --list` shows them all.
- build both indexes;
- PageRank, components (weak and strong), triangles and Louvain on each graph;
- `hop` and `shortest_path`;
- GRAPH_TABLE patterns: bound 1- and 2-edge, mutual follows, `ANY SHORTEST`, and the `{1,3}` histogram;
- checks.

`--steps` and `--skip` take step names or groups (`build`, `algorithm`, `traversal`, `pattern`, `check`).

**Checks.** With `--before 2024-01-01`, the graphs are the 95,744,886-edge ones from the round 1–3 reports (`bsky23`, `bsky23e`). Each step's result is compared with the value those reports cross-checked against plain SQL, and a mismatch fails the run (exit status 1). For other cutoffs the values are recorded but not compared.

**Output.** `results.json` (every number, plus notices and errors) and `summary.md` (a table, also used as the GitHub job summary), in `--out`, and in `--results s3://…` when given.

**Run it anywhere:**

```bash
python3 run.py --data ~/Downloads/ICPSR_300499-V1.1 --work /tmp/bsky --before 2024-01-01

# no dataset: a synthetic stand-in, in seconds
python3 smoke_data.py /tmp/smoke/data
python3 run.py --data /tmp/smoke/data --work /tmp/smoke/work --before 2024-03-01
```

The 2024-01-01 run passes all 23 checked steps on every machine and release so far:

| Where | Release | Total | Index build (`g` / `ge`) | PageRank `g` | Louvain `g` | Peak (largest step) |
|---|---|---|---|---|---|---|
| M-series Mac, 12 cores, 32 GB | 0.9.0 | ~3 min | 27.4 / 25.6 s | 4.1 s | 15.3 s | 6.2 GB (RSS overstates on macOS) |
| M-series Mac | **0.10.0** | 2.4 min | **9.4 / 7.2 s** | 4.4 s | 16.7 s | 4.5 GB (RSS) |
| EC2 r7g.4xlarge, Docker, 250 GB gp3 | 0.9.0 | ~4 min | 40.8 / 39.1 s | 6.6 s | 26.8 s | 6.7 GB |
| Fargate 16 vCPU / 120 GB, arm64, 150 GiB | 0.9.0 | 4.7 min (33 s of it the S3 download) | 35.8 / 34.9 s (55 / 46 s wall) | 15.9 s | 21.5 s | 6.7 GB |

**How to read these numbers:**
- In 0.9.0 the index build ran on one core, so a Graviton3 machine built slower than the laptop. 0.10.0 decodes in parallel.
- Fargate's disk is slower than gp3, so steps that read a lot from disk (the build, PageRank adding the reverse index) take longer there than on EC2.
- Compare runs on the same kind of machine.

Each run's `summary.md` is kept in [`reports/`](reports/), named `<date>-<machine>-<graph>-<release>.md`.

### The full graph

All 2,416,311,437 follows with all 39,650,447 users as nodes (`run.py` without `--before`):

| Item | Value |
|---|---|
| Release | `ursa` 0.10.0, installed natively |
| Machine | EC2 r7g.4xlarge |
| Date | 2026-11-18 |
| Instance time | 70 minutes, about $1.10 |
| Report | [`reports/2026-11-18-ec2-r7g.4xlarge-full-0.10.0.md`](reports/2026-11-18-ec2-r7g.4xlarge-full-0.10.0.md) |

| Step | Wall | Cores busy | Peak RSS |
|---|---|---|---|
| Index build (`describe(g, full => true)`) | 90 s (build 40.8 s) | 2.6 | 44.6 GB |
| Stored index | — | — | 18.6 GiB; 36.9 GiB with the reverse adjacency |
| Index load | 15.3 s | 1.0 | 22.5 GB |
| PageRank (adds the reverse) | 115 s | 8.0 | 42.4 GB |
| Weak / strong components | 63 / 82 s | 1.3 / 1.2 | 22.5 GB |
| Triangles (559.8 billion) | 30.5 min | 15.5 | 65.1 GB |
| Louvain | 27 min | 1.5 | 73.9 GB |
| `hop`, `shortest_path`, MATCH, mutuals, `ANY SHORTEST` | ~16 s each | 1.0 | 22.5 GB |
| `{1,3}` histogram | error: the 10M-path cap (finding N50) | — | — |

**What fits where:**
- **32 GB laptop:** nothing; even loading the index takes 22.5 GB.
- **64 GB:** everything but triangles and Louvain.
- **Fargate's 120 GB task or an r7g.4xlarge:** everything.

At this size the limits are time rather than memory: triangles grow with the hubs, Louvain and the components run near one core, and every query pays the 15 s index load.

**Measurement notes:**
- Peak memory is each `ursa` process's own `ru_maxrss`. On Linux that is close to the real peak. On macOS RSS overstates it; the round reports use `phys_footprint` there.
- The first step on each graph builds its index and stores it in the run's database, so later steps load it the way a user's second query does.
- Every run starts with a fresh database.

## AWS

### Sizes and costs

Memory for the full graph is measured (ursa 0.10.0, above). Every step fits Fargate's 120 GB task and an r7g.4xlarge's 128 GiB; the largest is Louvain at 73.9 GB. With ursa 0.9.0 Louvain needed ~155 GB.

**Disk:** ~13 GB of data plus 36.9 GiB of stored index. The task definition asks for 150 GiB of ephemeral storage; the EC2 instance gets a 250 GB gp3 volume.

**Time and cost guards.** The full graph takes about 70 minutes on an r7g.4xlarge, most of it triangles (30 min) and Louvain (27 min).
- **Per-step time limit:** `run.py --timeout SECONDS` (default 3600).
- **Choose the steps:** on the full graph, pass `--steps` with graph `g`'s steps only (the edge-only `ge` builds a second 37 GiB index), and leave out `triangles_g` and `louvain_g` when you don't need them.
- **Start detached:** run long jobs with `setsid nohup … &`, so they don't depend on the SSM session (an SSM command is killed at its timeout).
- **Watch, then shut down:** check progress with a short SSM `tail`, and run `ec2-down` as soon as the results are in S3.

**Cost.** Check these against AWS's pricing pages:

| Item | Cost |
|---|---|
| Fargate, 16 vCPU / 120 GB, ARM, us-east-1 | ~$0.95 per hour |
| EC2 r7g.4xlarge (16 vCPU, 128 GiB) | ~$0.86 per hour on demand; Spot is less |
| S3 (~13 GB) | ~$0.30 per month |

The stack itself costs nothing while idle: no NAT gateway, and S3 traffic goes through a free gateway endpoint.

**Quotas.** New accounts often have 16 On-Demand standard vCPUs (enough for one r7g.4xlarge) and 30 Fargate vCPUs. For an r7g.8xlarge, use Spot (`ec2-up r7g.8xlarge spot`, if the Spot quota allows 32 vCPUs) or ask for an On-Demand quota increase.

### The data's terms

The data comes from ICPSR under its terms of use. Before uploading it to S3, check that keeping it in your own private cloud storage is allowed. The bucket blocks all public access, refuses non-TLS requests, and only the benchmark's roles can read `data/`.

### Setup, once

```bash
cd benchmarks/bluesky_cloud
export AWS_PROFILE=<your profile>     # required; nothing uses the default profile
export BENCH_ACCOUNT=<account id>     # required; every command checks the profile signs in to it

./bench-cloud.sh deploy               # the stack (add CreateGitHubOidcProvider=false if the
                                      # account already has GitHub's OIDC provider)
./bench-cloud.sh upload-data ~/Downloads/ICPSR_300499-V1.1
./bench-cloud.sh push 0.12.0          # needs Docker; builds linux/arm64 (Fargate and the workflow)
```

`push` needs Docker, and Docker needs to reach `deb.debian.org` to build. EC2 runs don't need the image at all (see "Native runs on EC2").

### Live work on EC2

```bash
./bench-cloud.sh ec2-up                      # r7g.4xlarge; or: ec2-up r7g.8xlarge spot
./bench-cloud.sh ec2-connect                 # a shell over SSM; no SSH key, no open port
#   on the instance: data in /work/data, `ursa` on the PATH, the image pulled, see /etc/motd
./bench-cloud.sh ec2-down                    # terminate it: it costs money every hour
```

Connecting needs the Session Manager plugin for the AWS CLI. The instance has no inbound ports, and its role can only read the data, write results and pull the image.

### Native runs on EC2

This is how the full graph ran. The instance's first boot installs the released `ursa` (`URSA_VERSION`) and copies everything under the bucket's `data/` to `/work/data`, so put the scripts there too:

```bash
BUCKET=$(./bench-cloud.sh outputs | awk '/ Bucket /{print $4}')
aws s3 cp run.py s3://$BUCKET/data/tools/run.py       # first: the instance copies data/ when it boots
URSA_VERSION=0.12.0 ./bench-cloud.sh ec2-up
# on the instance (ec2-connect, or `aws ssm send-command`), detached. The steps are the
# full-graph run's: graph g only, since the edge-only ge would build a second 37 GiB index.
sudo dnf install -y python3.11
setsid nohup python3.11 /work/data/tools/run.py --data /work/data --work /work/full \
    --steps build_g,load_g,pagerank_g,wcc_g,scc_g,triangles_g,louvain_g,hop_1,hop_2,shortest_path,match_1edge_g,match_2edge_g,mutuals,any_shortest \
    > /work/full.log 2>&1 < /dev/null &
# when it's done:
aws s3 cp --recursive /work/full/out s3://$BUCKET/results/<run id>/
./bench-cloud.sh ec2-down
```

`run.py` needs Python 3.10 or later, and Amazon Linux 2023's default `python3` is 3.9. That's why the commands install `python3.11`. For the cross-library comparison, `../compare/ec2-run.sh` does all of this in one script.

### A Fargate run

```bash
URSA_VERSION=0.12.0 ./bench-cloud.sh run --before 2024-01-01     # the checked 95.7M-edge graph
URSA_VERSION=0.12.0 ./bench-cloud.sh run --steps build_g,load_g,pagerank_g,wcc_g,scc_g,louvain_g   # the whole graph
./bench-cloud.sh results                                          # the newest run's results
```

`run` registers a task definition revision for that version's image, starts the task, waits for it, and downloads the results to `results/<run id>/`. Ctrl-C stops the task.

### GitHub Actions

`.github/workflows/bluesky-bench.yml` runs the same thing by hand (Actions → Bluesky benchmark → Run workflow). Its inputs are the version, the cutoff, and the steps. To set it up:

1. Deploy the stack (above). Its `GitHubRoleArn` output is a role only jobs in the `bluesky-bench` environment of `cldixon/ursa` can assume, through OIDC. No AWS keys are stored in GitHub.
2. In the repository settings, create the environment `bluesky-bench`. Add yourself as a required reviewer, so every run (and its cost) waits for approval.
3. On that environment, set the variables:
   - `BENCH_ACCOUNT`: the account id;
   - `BENCH_ROLE_ARN`: the `GitHubRoleArn` output;
   - optionally `BENCH_AWS_REGION` and `BENCH_STACK`.

The runner (`ubuntu-24.04-arm`, free for public repositories) builds and pushes the image, runs `bench-cloud.sh run`, and waits. The task does the work. The summary goes to the job page and the results to an artifact. Cancelling the workflow stops the task.

### Tear down

```bash
aws cloudformation delete-stack --stack-name ursa-bluesky-bench
```

The bucket is kept (it has `DeletionPolicy: Retain`) so results and data survive. Empty and delete it by hand if you want it gone. Delete the ECR images first, or the repository's deletion fails.
