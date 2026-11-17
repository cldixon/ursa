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

On an M-series Mac (12 cores, 32 GB) with `ursa 0.9.0`, the 2024-01-01 run passes all 22 checks in about 3 minutes.

**Measurement notes:**
- Peak memory is each `ursa` process's own `ru_maxrss`. On Linux that is close to the real peak. On macOS RSS overstates it; the round reports use `phys_footprint` there.
- The first step on each graph builds its index and stores it in the run's database, so later steps load it the way a user's second query does.
- Every run starts with a fresh database.

## AWS

### Sizes and costs

From the round 3 report (`sql-cli-0.9.0-retest.md`) and a measured 983M-edge run:

| Full graph (2.42B edges, vertex table) | Memory | Fits Fargate 16 vCPU / 120 GB? |
|---|---|---|
| Build the index | ~44 GB | yes |
| PageRank | ~47–49 GB | yes |
| Components, traversals, bound MATCH | ~25–27 GB | yes |
| Triangles | ~75 GB | yes |
| Louvain (ursa 0.9.0) | ~155 GB | **no**. Skip it on Fargate (`--skip louvain_g,louvain_ge,check_louvain_ge`), or run on an r7g.8xlarge (256 GiB) |

**Disk:** ~13 GB of data plus ~41 GB of stored index. The task definition asks for 150 GiB of ephemeral storage; the EC2 instance gets a 250 GB gp3 volume.

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
./bench-cloud.sh push 0.9.0           # needs Docker; builds linux/arm64
```

### Live work on EC2

```bash
./bench-cloud.sh ec2-up                      # r7g.4xlarge; or: ec2-up r7g.8xlarge spot
./bench-cloud.sh ec2-connect                 # a shell over SSM; no SSH key, no open port
#   on the instance: data in /work/data, `ursa` on the PATH, the image pulled, see /etc/motd
./bench-cloud.sh ec2-down                    # terminate it: it costs money every hour
```

Connecting needs the Session Manager plugin for the AWS CLI. The instance has no inbound ports, and its role can only read the data, write results and pull the image.

### A Fargate run

```bash
URSA_VERSION=0.9.0 ./bench-cloud.sh run --before 2024-01-01      # the checked 95.7M-edge graph
URSA_VERSION=0.9.0 ./bench-cloud.sh run --skip louvain_g,louvain_ge,check_louvain_ge   # whole graph
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
