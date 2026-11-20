"""Scale benchmark of the ``ursa`` binary on the Bluesky follows graph.

The same script runs on a laptop, on an EC2 instance and in a Fargate task (see
README.md). Every step is its own ``ursa`` process, so each step's wall time,
CPU time and peak RSS are its own. Steps never share a process, but they share
one ``.ursa`` database, so a step after ``build_*`` loads the stored index the
way a user's second query would.

Data: the ICPSR 300499 files ``deidentified_follows_edgelist.parquet`` and
``deidentified_nodes.csv.gz``, in a local directory or under an ``s3://``
prefix (downloaded to ``--work`` first, and timed as ``download``).

Graphs (``--before DATE`` keeps the follows before DATE; without it, the whole
graph):

- ``g``: the follows with the users as a vertex table (every user is a node).
- ``ge``: the same follows with no vertex table (nodes are edge endpoints).

With ``--before 2024-01-01`` the graphs are the ones in the round-1 to round-3
reports (``bsky23`` and ``bsky23e``: 95,744,886 edges), and the ``check``
steps compare the results with the values those reports cross-checked against
plain SQL. For other cutoffs the values are recorded but not compared.

Output: ``results.json`` (one object per step, with the raw numbers) and
``summary.md`` (a table for people and GitHub's job summary) in ``--out``, and
under ``--results s3://...`` too when given. The exit status is 1 when a check
fails or a step errors, so a CI run goes red.

Usage::

    python3 run.py --data ~/Downloads/ICPSR_300499-V1.1 --work /tmp/bsky \\
        --before 2024-01-01 --out /tmp/bsky/out
    python3 run.py --help

Only the standard library is needed, plus ``boto3`` for ``s3://`` paths.
"""

from __future__ import annotations

import argparse
import contextlib
import csv
import io
import json
import os
import platform
import shutil
import signal
import subprocess
import sys
import threading
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path

FOLLOWS = "deidentified_follows_edgelist.parquet"
NODES = "deidentified_nodes.csv.gz"

# Node ids used by the traversal and pattern steps (all present before 2024).
HUB = 6987779  # the most-followed account before 2024
MID = 33127650  # 3,249 out-edges before 2024
SRC, DST = 7078889, 25003023  # two steps apart before 2024


@dataclass
class Step:
    name: str
    sql: str
    group: str  # setup | build | algorithm | traversal | pattern | check
    graph: str = ""
    # For checks: the expected first row, as strings, under --before 2024-01-01.
    expect: list[str] | None = None
    # Compare floats in `expect` to this relative tolerance.
    rel_tol: float = 0.0


@dataclass
class Result:
    name: str
    group: str
    graph: str
    status: str  # ok | error | timeout | skipped
    wall_s: float = 0.0
    user_s: float = 0.0
    sys_s: float = 0.0
    peak_rss_bytes: int = 0
    rows: list[list[str]] = field(default_factory=list)
    columns: list[str] = field(default_factory=list)
    notices: list[str] = field(default_factory=list)
    error: str = ""
    check: str = ""  # pass | fail | "" (not a check, or nothing to compare)
    expected: list[str] | None = None


def steps(before: str | None) -> list[Step]:
    """Every step, in run order."""
    known = before == "2024-01-01"

    def exp(*values: object) -> list[str] | None:
        return [str(v) for v in values] if known else None

    s: list[Step] = []
    for g in ("g", "ge"):  # with the vertex table, and edges only
        s.append(
            Step(
                f"build_{g}",
                f"SELECT n_nodes, n_edges, n_components, n_dropped_edges "
                f"FROM describe({g}, full => true)",
                "build",
                g,
                exp(39650447, 95744886, 37177151, 0)
                if g == "g"
                else exp(2476178, 95744886, 2882, 0),
            )
        )
    s.append(Step("load_g", "SELECT count(*) FROM degree(g)", "build", "g", exp(39650447)))
    for g in ("g", "ge"):
        s += [
            Step(
                f"pagerank_{g}",
                f"SELECT count(*), max(pagerank) FROM pagerank({g})",
                "algorithm",
                g,
                exp(39650447, 0.003783540063674796)
                if g == "g"
                else exp(2476178, 0.015268822724612531),
                rel_tol=1e-9,
            ),
            Step(f"wcc_{g}", f"SELECT count(*) FROM connected_components({g})", "algorithm", g),
            Step(
                f"scc_{g}",
                f"SELECT count(*) FROM connected_components({g}, mode => 'strong')",
                "algorithm",
                g,
            ),
            Step(
                f"triangles_{g}",
                f"SELECT sum(triangle_count) FROM triangle_count({g})",
                "algorithm",
                g,
                exp(3 * 1229699618),
            ),
            Step(f"louvain_{g}", f"SELECT count(*) FROM louvain({g}, seed => 1)", "algorithm", g),
        ]
    s += [
        Step(
            "hop_1",
            f"SELECT count(*) FROM hop(g, seeds => [{MID}], n => 1)",
            "traversal",
            "g",
            exp(3216),
        ),
        Step(
            "hop_2",
            f"SELECT count(*) FROM hop(g, seeds => [{MID}], n => 2)",
            "traversal",
            "g",
            exp(166664),
        ),
        Step(
            "shortest_path",
            f"SELECT count(*), max(cost) FROM shortest_path(g, source => {SRC}, target => {DST})",
            "traversal",
            "g",
            exp(2, 2.0),
        ),
        Step(
            "match_1edge_ge",
            f"SELECT count(*) FROM GRAPH_TABLE (ge MATCH (a WHERE a.id = {MID})"
            f"-[]->(b) COLUMNS (b.id AS b))",
            "pattern",
            "ge",
            exp(3249),
        ),
        Step(
            "match_1edge_g",
            f"SELECT count(*) FROM GRAPH_TABLE (g MATCH (a WHERE a.id = {MID})"
            f"-[]->(b) COLUMNS (b.id AS b))",
            "pattern",
            "g",
            exp(3249),
        ),
        Step(
            "match_2edge_g",
            f"SELECT count(*) FROM GRAPH_TABLE (g MATCH (a WHERE a.id = {MID})"
            f"-[]->(b)-[]->(c) COLUMNS (c.id AS c))",
            "pattern",
            "g",
            exp(708845),
        ),
        Step(
            "mutuals",
            f"SELECT count(*), count(DISTINCT b) FROM GRAPH_TABLE (g MATCH "
            f"(a WHERE a.id = {MID})-[]->(b)-[]->(a) COLUMNS (b.id AS b))",
            "pattern",
            "g",
            exp(2263, 2216),
        ),
        # The path as text: CSV output cannot hold a list column (0.9.0 report, N34).
        Step(
            "any_shortest",
            "SELECT steps, array_to_string(path, ' ') FROM GRAPH_TABLE "
            f"(g MATCH p = ANY SHORTEST (a WHERE a.id = {SRC})-[e]->*(b WHERE b.id = {DST}) "
            "COLUMNS (path_length(p) AS steps, vertices(p) AS path))",
            "pattern",
            "g",
            exp(2, f"{SRC} 36081159 {DST}"),
        ),
        Step(
            "hist_1_3",
            "SELECT string_agg(CAST(n AS VARCHAR), ' ' ORDER BY steps) FROM "
            "(SELECT steps, count(*) AS n FROM GRAPH_TABLE (g MATCH p = ANY SHORTEST "
            f"(a WHERE a.id = {MID})-[e]->{{1,3}}(b) COLUMNS (path_length(p) AS steps)) "
            "GROUP BY steps)",
            "pattern",
            "g",
            exp("3216 163449 1130480"),
        ),
        # Checks: values the timed steps above don't give, from the round-3 report.
        Step(
            "check_components_g",
            "SELECT count(DISTINCT connected_components) FROM connected_components(g)",
            "check",
            "g",
            exp(37177151),
        ),
        Step(
            "check_components_ge",
            "SELECT count(DISTINCT connected_components) FROM connected_components(ge)",
            "check",
            "ge",
            exp(2882),
        ),
        Step(
            "check_scc_g",
            "SELECT count(DISTINCT connected_components) FROM "
            "connected_components(g, mode => 'strong')",
            "check",
            "g",
            exp(37842653),
        ),
        Step(
            "check_scc_ge",
            "SELECT count(DISTINCT connected_components) FROM "
            "connected_components(ge, mode => 'strong')",
            "check",
            "ge",
            exp(668384),
        ),
        Step(
            "check_louvain_ge",
            "SELECT count(DISTINCT louvain) FROM louvain(ge, seed => 1)",
            "check",
            "ge",
            exp(3624),
        ),
        Step(
            "check_in_degree",
            f"SELECT degree FROM degree(g, direction => 'in') WHERE id = {HUB}",
            "check",
            "g",
            exp(565265),
        ),
        Step(
            "check_hop_in",
            f"SELECT count(*) FROM hop(g, seeds => [{HUB}], n => 1, direction => 'in')",
            "check",
            "g",
            exp(562584),
        ),
    ]
    return s


def setup_sql(work: Path, before: str | None, vertex: str) -> str:
    follows = (work / "data" / FOLLOWS).as_posix()
    users = (
        (work / "data" / NODES).as_posix()
        if vertex == "csv"
        else (work / "users.parquet").as_posix()
    )
    users_from = f"read_csv('{users}')" if vertex == "csv" else f"'{users}'"
    where = f" WHERE date_followed < DATE '{before}'" if before else ""
    return f"""
CREATE VIEW follows AS FROM '{follows}'{where};
CREATE VIEW users AS FROM {users_from};
CREATE PROPERTY GRAPH g
  VERTEX TABLES (users)
  EDGE TABLES (follows SOURCE KEY (from) REFERENCES users (id)
                       DESTINATION KEY (to) REFERENCES users (id));
CREATE PROPERTY GRAPH ge
  EDGE TABLES (follows SOURCE KEY (from) DESTINATION KEY (to));
"""


def run_ursa(ursa: str, db: Path, sql: str, timeout: float) -> tuple[Result, str]:
    """Run one ``ursa`` process; return its measurements and its stdout.

    The child is reaped here with ``os.wait4``, so its rusage (CPU time and
    peak RSS) is its own, not the running maximum ``RUSAGE_CHILDREN`` keeps.
    """
    cmd = [ursa, str(db), "--format", "csv", "--no-init", "-c", sql]
    t0 = time.monotonic()
    proc = subprocess.Popen(
        cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True
    )
    chunks: dict[str, list[bytes]] = {"out": [], "err": []}

    def drain(stream, key: str) -> None:
        for block in iter(lambda: stream.read(1 << 16), b""):
            chunks[key].append(block)

    readers = [
        threading.Thread(target=drain, args=(proc.stdout, "out"), daemon=True),
        threading.Thread(target=drain, args=(proc.stderr, "err"), daemon=True),
    ]
    for t in readers:
        t.start()
    status = "ok"
    while True:
        pid, wstatus, ru = os.wait4(proc.pid, os.WNOHANG)
        if pid:
            break
        if time.monotonic() - t0 > timeout:
            os.killpg(proc.pid, signal.SIGKILL)
            pid, wstatus, ru = os.wait4(proc.pid, 0)
            status = "timeout"
            break
        time.sleep(0.05)
    wall = time.monotonic() - t0
    proc.returncode = os.waitstatus_to_exitcode(wstatus)  # keep Popen from waiting again
    for t in readers:
        t.join()
    if status == "ok" and proc.returncode != 0:
        status = "error"
    r = Result(
        name="",
        group="",
        graph="",
        status=status,
        wall_s=round(wall, 3),
        user_s=round(ru.ru_utime, 3),
        sys_s=round(ru.ru_stime, 3),
        peak_rss_bytes=_maxrss_bytes(ru.ru_maxrss),
    )
    errtext = b"".join(chunks["err"]).decode("utf-8", "replace")
    r.notices = [ln for ln in errtext.splitlines() if ln.startswith("ursa:")]
    if status != "ok":
        r.error = "\n".join(errtext.splitlines()[-15:]) or f"exit status {proc.returncode}"
    return r, b"".join(chunks["out"]).decode("utf-8", "replace")


def measure(ursa: str, db: Path, step: Step, timeout: float) -> Result:
    r, out = run_ursa(ursa, db, step.sql, timeout)
    r.name, r.group, r.graph = step.name, step.group, step.graph
    if r.status == "ok":
        rows = list(csv.reader(io.StringIO(out)))
        if rows:
            r.columns, r.rows = rows[0], rows[1:6]
    if step.expect is not None and r.status == "ok":
        r.expected = step.expect
        got = r.rows[0] if r.rows else []
        r.check = "pass" if _matches(got, step.expect, step.rel_tol) else "fail"
    return r


def _maxrss_bytes(v: int) -> int:
    return v if sys.platform == "darwin" else v * 1024  # Linux reports KiB


def _matches(got: list[str], want: list[str], rel_tol: float) -> bool:
    if len(got) != len(want):
        return False
    for g, w in zip(got, want, strict=True):
        if g == w:
            continue
        try:
            gf, wf = float(g), float(w)
        except ValueError:
            return False
        if abs(gf - wf) > rel_tol * max(abs(wf), 1e-300):
            return False
    return True


# --- data in and results out ------------------------------------------------


def _s3(url: str) -> tuple[str, str]:
    bucket, _, key = url[len("s3://") :].partition("/")
    return bucket, key.rstrip("/")


def fetch_data(src: str, work: Path) -> float:
    """Put the two data files in ``work/data``; return the seconds it took."""
    dst = work / "data"
    dst.mkdir(parents=True, exist_ok=True)
    t0 = time.monotonic()
    for name in (FOLLOWS, NODES):
        target = dst / name
        if target.exists():
            continue
        if src.startswith("s3://"):
            import boto3  # only needed for s3:// paths

            bucket, prefix = _s3(src)
            key = f"{prefix}/{name}" if prefix else name
            print(f"downloading s3://{bucket}/{key}", flush=True)
            boto3.client("s3").download_file(bucket, key, str(target))
        else:
            path = Path(src).expanduser() / name
            if not path.exists():
                sys.exit(f"run.py: {path} not found")
            target.symlink_to(path.resolve())
    return time.monotonic() - t0


def upload_results(out: Path, dest: str) -> None:
    import boto3

    bucket, prefix = _s3(dest)
    s3 = boto3.client("s3")
    for f in sorted(out.iterdir()):
        if f.is_file():
            key = f"{prefix}/{f.name}" if prefix else f.name
            s3.upload_file(str(f), bucket, key)
            print(f"uploaded s3://{bucket}/{key}", flush=True)


def host_info() -> dict:
    info = {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "cpus": os.cpu_count(),
        "label": os.environ.get("BENCH_HOST_LABEL", ""),
    }
    try:
        if sys.platform == "darwin":
            info["mem_bytes"] = int(subprocess.check_output(["sysctl", "-n", "hw.memsize"]))
        else:
            with open("/proc/meminfo") as f:
                info["mem_bytes"] = int(f.readline().split()[1]) * 1024
    except (OSError, ValueError, subprocess.CalledProcessError):
        pass
    # The container's memory limit, which /proc/meminfo (the host's) does not
    # show: cgroup v2, then v1 (Fargate's Linux platform 1.4 uses v1).
    for k, p in (
        ("cgroup_mem_max", "/sys/fs/cgroup/memory.max"),
        ("cgroup_mem_max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"),
        ("cgroup_cpu_max", "/sys/fs/cgroup/cpu.max"),
    ):
        if k not in info:
            with contextlib.suppress(OSError):
                info[k] = Path(p).read_text().strip()
    # On ECS, the task's own size and placement, from the task metadata endpoint.
    meta_uri = os.environ.get("ECS_CONTAINER_METADATA_URI_V4")
    if meta_uri:
        import urllib.request

        with contextlib.suppress(OSError, ValueError):
            with urllib.request.urlopen(f"{meta_uri}/task", timeout=2) as resp:
                task = json.load(resp)
            info["ecs"] = {
                "limits": task.get("Limits"),
                "availability_zone": task.get("AvailabilityZone"),
                "launch_type": task.get("LaunchType"),
                "task_arn": task.get("TaskARN"),
            }
            limits = task.get("Limits") or {}
            if limits.get("Memory"):
                info["mem_bytes"] = int(limits["Memory"]) * 1024 * 1024
    # On EC2, the instance type (IMDSv2), so results say what they ran on.
    elif sys.platform == "linux":
        info.update(_ec2_identity())
    return info


def _ec2_identity() -> dict:
    import urllib.request

    base = "http://169.254.169.254/latest"
    with contextlib.suppress(OSError, ValueError):
        req = urllib.request.Request(
            f"{base}/api/token",
            method="PUT",
            headers={"X-aws-ec2-metadata-token-ttl-seconds": "60"},
        )
        with urllib.request.urlopen(req, timeout=1) as resp:
            token = resp.read().decode()
        req = urllib.request.Request(
            f"{base}/dynamic/instance-identity/document",
            headers={"X-aws-ec2-metadata-token": token},
        )
        with urllib.request.urlopen(req, timeout=1) as resp:
            doc = json.load(resp)
        return {
            "ec2": {
                "instance_type": doc.get("instanceType"),
                "availability_zone": doc.get("availabilityZone"),
            }
        }
    return {}


# --- report -----------------------------------------------------------------


def gb(b: int) -> str:
    return f"{b / 1e9:.2f}" if b else ""


def _host_name(host: dict) -> str:
    ec2 = (host.get("ec2") or {}).get("instance_type")
    return host.get("label") or (f"EC2 {ec2}" if ec2 else host["platform"])


def summary_md(meta: dict, results: list[Result]) -> str:
    lines = [
        f"## Bluesky benchmark: {meta['ursa_version']}",
        "",
        f"- Graph: follows {'before ' + meta['before'] if meta['before'] else '(whole graph)'}, "
        f"vertex table as {meta['vertex']}",
        f"- Host: {_host_name(meta['host'])}, "
        f"{meta['host']['cpus']} CPUs, {gb(meta['host'].get('mem_bytes', 0))} GB",
        f"- Download: {meta['download_s']:.1f} s; total: {meta['total_s']:.1f} s",
        "",
        "| Step | Status | Wall s | User s | Peak RSS GB* | Result | Check |",
        "|---|---|---:|---:|---:|---|---|",
    ]
    for r in results:
        res = " ".join(r.rows[0]) if r.rows else ""
        if len(res) > 40:
            res = res[:37] + "..."
        check = {"pass": "pass", "fail": f"**FAIL** (want {' '.join(r.expected or [])})"}.get(
            r.check, ""
        )
        lines.append(
            f"| {r.name} | {r.status} | {r.wall_s:.2f} | {r.user_s:.2f} | "
            f"{gb(r.peak_rss_bytes)} | {res} | {check} |"
        )
    notices = [(r.name, n) for r in results for n in r.notices]
    if notices:
        lines += ["", "<details><summary>Notices</summary>", "", "```"]
        lines += [f"{name}: {n}" for name, n in notices]
        lines += ["```", "", "</details>"]
    errors = [r for r in results if r.error]
    for r in errors:
        lines += ["", f"### {r.name}: {r.status}", "", "```", r.error, "```"]
    lines += [
        "",
        "\\* Each step's own `ursa` process (`wait4`). On Linux this is close to the "
        "process's real peak; on macOS RSS overstates it (use `phys_footprint` there).",
    ]
    return "\n".join(lines) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--data", help="directory or s3:// prefix holding the data files")
    ap.add_argument("--work", type=Path, help="local working directory (database, data)")
    ap.add_argument(
        "--out", type=Path, help="where results.json and summary.md go (default: WORK/out)"
    )
    ap.add_argument("--results", help="also upload the results under this s3:// prefix")
    ap.add_argument(
        "--before", help="keep follows before this date, e.g. 2024-01-01 (default: all)"
    )
    ap.add_argument(
        "--vertex",
        choices=("parquet", "csv"),
        default="parquet",
        help="read the users from a Parquet copy (default) or the .csv.gz",
    )
    ap.add_argument("--steps", help="comma-separated step names or groups to run (default: all)")
    ap.add_argument("--skip", help="comma-separated step names or groups to leave out")
    ap.add_argument("--ursa", default=shutil.which("ursa") or "ursa", help="the ursa binary")
    ap.add_argument("--timeout", type=float, default=3600, help="seconds per step (default 3600)")
    ap.add_argument(
        "--storage-limit",
        default="512GiB",
        help="ursa.index_storage_limit for the database (default 512GiB)",
    )
    ap.add_argument("--list", action="store_true", help="list the steps and exit")
    a = ap.parse_args()

    plan = steps(a.before)
    if a.list:
        for s in plan:
            print(f"{s.name:22} {s.group:10} {s.sql}")
        return 0
    if not a.data or not a.work:
        ap.error("--data and --work are required")
    if a.steps:
        keep = set(a.steps.split(","))
        plan = [s for s in plan if s.name in keep or s.group in keep]
    if a.skip:
        drop = set(a.skip.split(","))
        plan = [s for s in plan if s.name not in drop and s.group not in drop]

    a.work.mkdir(parents=True, exist_ok=True)
    out = a.out or a.work / "out"
    out.mkdir(parents=True, exist_ok=True)
    version = subprocess.check_output([a.ursa, "--version"], text=True).strip()
    print(version, flush=True)
    t_start = time.monotonic()
    started = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    download_s = fetch_data(a.data, a.work)

    db = a.work / "bench.ursa"
    results: list[Result] = []
    # Setup steps: the Parquet copy of the users, then the catalog. A fresh
    # database every run, so no index from an earlier run is reused.
    for p in (db, a.work / "bench.ursa.d"):
        if p.is_dir():
            shutil.rmtree(p)
        elif p.exists():
            p.unlink()
    setup: list[Step] = []
    if a.vertex == "parquet" and not (a.work / "users.parquet").exists():
        nodes = (a.work / "data" / NODES).as_posix()
        users = (a.work / "users.parquet").as_posix()
        setup.append(
            Step("vertex_parquet", f"COPY (FROM read_csv('{nodes}')) TO '{users}'", "setup")
        )
    setup.append(
        Step(
            "define",
            f"SET ursa.index_storage_limit = '{a.storage_limit}';"
            + setup_sql(a.work, a.before, a.vertex),
            "setup",
        )
    )
    for s in setup + plan:
        r = measure(a.ursa, db, s, a.timeout)
        results.append(r)
        flag = {"pass": " check pass", "fail": " CHECK FAIL"}.get(r.check, "")
        print(
            f"{r.name:22} {r.status:8} {r.wall_s:9.2f}s  {gb(r.peak_rss_bytes):>6} GB"
            f"  {' '.join(r.rows[0]) if r.rows else ''}{flag}",
            flush=True,
        )
        for n in r.notices:
            print(f"    {n}", flush=True)
        if r.status != "ok" and s.group == "setup":
            print(r.error, file=sys.stderr)
            break

    idx = db.parent / (db.name + ".d") / "indexes"
    meta = {
        "ursa_version": version,
        "before": a.before or "",
        "vertex": a.vertex,
        "host": host_info(),
        "download_s": round(download_s, 3),
        "total_s": round(time.monotonic() - t_start, 3),
        "stored_index_bytes": sum(f.stat().st_size for f in idx.glob("*.idx"))
        if idx.is_dir()
        else 0,
        "started": started,
    }
    (out / "results.json").write_text(
        json.dumps({"meta": meta, "steps": [asdict(r) for r in results]}, indent=2) + "\n"
    )
    (out / "summary.md").write_text(summary_md(meta, results))
    print(f"wrote {out / 'results.json'} and {out / 'summary.md'}", flush=True)
    if a.results:
        upload_results(out, a.results.rstrip("/"))

    bad = [r.name for r in results if r.status != "ok" or r.check == "fail"]
    if bad:
        print(f"run.py: failed or wrong: {', '.join(bad)}", file=sys.stderr)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
