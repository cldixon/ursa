"""Scale test of the ``ursa`` SQL binary, with the Python library alongside.

Every step runs as its own process, so each one's wall time and peak RSS are
its own: the release ``ursa`` binary for SQL, and a fresh interpreter for the
Python library. Peak RSS is the child's ``ru_maxrss`` from ``wait4``.

Per dataset (an ``rmat-<size>-<int|str>.parquet`` from ``gen.py``), in a fresh
``.ursa`` database:

==================  ==========================================================
``define``          ``CREATE VIEW`` + ``CREATE PROPERTY GRAPH``
``first_query``     ``pagerank(g)``: builds the index and stores it
``second_query``    the same query in a new process: loads the stored index
``pagerank_both``   ``pagerank(g, direction => 'both')``
``components``      ``connected_components(g)``
``louvain``         ``louvain(g, seed => 1)``
``degree``          ``degree(g)``
``hop``             ``hop(g, seeds => [three ids], n => 2)``
``weighted``        ``pagerank(gw, weight => weight)`` over the ``-w`` variant
                    (its first query builds that graph's index)
``weighted_again``  the same, in a new process
``algorithms``      ``algorithms(g, degree(), pagerank(), louvain(seed => 1))``
==================  ==========================================================

Each SQL step reports the graph indexes it built and loaded
(``URSA_INDEX_STATS=1``); after ``first_query`` the stored index's size on
disk is recorded. The Python steps scan the same Parquet with
``ur.scan_edges`` and ``.collect()`` the same algorithm with the same
parameters; with no stored index, each builds its index afresh. Both report
the number of result rows, so the two can be checked against each other.

A step that runs past ``--timeout`` or whose RSS passes ``--mem-limit-gb`` is
killed and recorded as ``timeout`` or ``memory``; a failing step is recorded
as ``error`` with the end of its stderr. Nothing is dropped.

Raw rows go to ``--out`` (CSV, appended with a run id), and a summary table is
printed. Usage::

    cargo build --release -p ursa-cli
    uv run --group bench python benchmarks/sql_scale/run.py \\
        --data DIR --work DIR --out results.csv
    python benchmarks/sql_scale/run.py --help
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import platform
import re
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SQL_STEPS = [
    "define",
    "first_query",
    "second_query",
    "pagerank_both",
    "components",
    "louvain",
    "degree",
    "hop",
    "weighted",
    "weighted_again",
    "algorithms",
]
PY_STEPS = [
    "first_query",
    "pagerank_both",
    "components",
    "louvain",
    "degree",
    "hop",
    "weighted",
    "algorithms",
]


@dataclass
class Row:
    run: str
    dataset: str
    edges: int
    engine: str
    step: str
    status: str = "ok"
    wall_s: float = 0.0
    peak_rss_mb: float = 0.0
    built: int | None = None
    loaded: int | None = None
    rows: int | None = None
    inner_s: float | None = None
    index_bytes: int | None = None
    note: str = ""
    extra: dict = field(default_factory=dict, repr=False)


def rss_mb(pid: int) -> float:
    """Current resident set of ``pid``, in MB, or 0 if it is gone."""
    try:
        with open(f"/proc/{pid}/status") as fh:
            for line in fh:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) / 1024
    except OSError:
        pass
    return 0.0


def measure(
    argv: list[str], *, cwd: Path, env: dict, timeout: float, mem_limit_mb: float
) -> tuple[str, float, float, str, str]:
    """Run ``argv``; return (status, wall seconds, peak RSS MB, stdout, stderr)."""
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        start = time.perf_counter()
        proc = subprocess.Popen(argv, cwd=cwd, env=env, stdout=out, stderr=err)
        status, killed = "ok", None
        while True:
            pid, code, usage = os.wait4(proc.pid, os.WNOHANG)
            if pid:
                break
            if time.perf_counter() - start > timeout:
                killed = "timeout"
            elif rss_mb(proc.pid) > mem_limit_mb:
                killed = "memory"
            if killed:
                os.kill(proc.pid, signal.SIGKILL)
                pid, code, usage = os.wait4(proc.pid, 0)
                break
            time.sleep(0.02)
        wall = time.perf_counter() - start
        proc.returncode = os.waitstatus_to_exitcode(code)
        if killed:
            status = killed
        elif proc.returncode == -signal.SIGKILL:
            status = "memory"  # the kernel's OOM killer
        elif proc.returncode != 0:
            status = "error"
        out.seek(0)
        err.seek(0)
        stdout = out.read().decode(errors="replace")
        stderr = err.read().decode(errors="replace")
    return status, wall, usage.ru_maxrss / 1024, stdout, stderr


def first_ids(path: Path, n: int = 3) -> list:
    """The first ``n`` distinct sources of the edge file: the hop seeds."""
    import pyarrow.parquet as pq

    batch = next(pq.ParquetFile(path).iter_batches(batch_size=64, columns=["src"]))
    seeds: list = []
    for value in batch.column(0).to_pylist():
        if value not in seeds:
            seeds.append(value)
        if len(seeds) == n:
            break
    return seeds


def sql_literal(values: list) -> str:
    return "[" + ", ".join(repr(v) if isinstance(v, str) else str(v) for v in values) + "]"


def sql_steps(edges: Path, weighted: Path | None, seeds: list) -> dict[str, str]:
    define = (
        f"CREATE VIEW e AS SELECT * FROM '{edges}';\n"
        "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (src) DESTINATION KEY (dst));"
    )
    if weighted is not None:
        define += (
            f"\nCREATE VIEW ew AS SELECT * FROM '{weighted}';\n"
            "CREATE PROPERTY GRAPH gw EDGE TABLES (ew SOURCE KEY (src) DESTINATION KEY (dst));"
        )
    first = "SELECT count(*) AS n, sum(pagerank) AS s FROM pagerank(g)"
    steps = {
        "define": define,
        "first_query": first,
        "second_query": first,
        "pagerank_both": (
            "SELECT count(*) AS n, sum(pagerank) AS s FROM pagerank(g, direction => 'both')"
        ),
        "components": (
            "SELECT count(*) AS n, count(DISTINCT connected_components) AS c "
            "FROM connected_components(g)"
        ),
        "louvain": (
            "SELECT count(*) AS n, count(DISTINCT louvain) AS c FROM louvain(g, seed => 1)"
        ),
        "degree": "SELECT count(*) AS n, sum(degree) AS s FROM degree(g)",
        "hop": f"SELECT count(*) AS n FROM hop(g, seeds => {sql_literal(seeds)}, n => 2)",
        "algorithms": (
            "SELECT count(*) AS n, sum(degree) AS d, sum(pagerank) AS p, "
            "count(DISTINCT louvain) AS c "
            "FROM algorithms(g, degree(), pagerank(), louvain => louvain(seed => 1))"
        ),
    }
    if weighted is not None:
        steps["weighted"] = (
            "SELECT count(*) AS n, sum(pagerank) AS s FROM pagerank(gw, weight => weight)"
        )
        steps["weighted_again"] = steps["weighted"]
    return steps


# The Python child: import, scan, build and compute, timing the part after the
# import. One step per process, as the SQL steps are.
PY_CHILD = r"""
import json, sys, time
t0 = time.perf_counter()
import ursa as ur
t1 = time.perf_counter()
step, path, wpath, seeds = sys.argv[1], sys.argv[2], sys.argv[3], json.loads(sys.argv[4])
e = ur.scan_edges(path, src="src", dst="dst")
if step == "first_query":
    f = ur.pagerank(e).collect()
elif step == "pagerank_both":
    f = ur.pagerank(e, direction="both").collect()
elif step == "components":
    f = ur.connected_components(e).collect()
elif step == "louvain":
    f = ur.louvain(e, seed=1).collect()
elif step == "degree":
    f = ur.degree(e).collect()
elif step == "hop":
    f = ur.hop(e, n=2).from_(seeds).collect()
elif step == "weighted":
    w = ur.scan_edges(wpath, src="src", dst="dst")
    f = ur.pagerank(w, weight=ur.col("weight")).collect()
elif step == "algorithms":
    f = e.nodes().with_columns(
        degree=ur.degree(e), pagerank=ur.pagerank(e), louvain=ur.louvain(e, seed=1)
    ).collect()
else:
    raise SystemExit(f"unknown step {step}")
t2 = time.perf_counter()
print(json.dumps({"rows": f.height, "import_s": t1 - t0, "inner_s": t2 - t1}))
"""

STATS = re.compile(r"graph indexes built (\d+), loaded (\d+)")


def first_number(stdout: str) -> int | None:
    """The first cell of a one-row result table (``n``)."""
    lines = [ln for ln in stdout.splitlines() if ln.startswith("|")]
    if len(lines) >= 2:
        cell = lines[1].strip("|").split("|")[0].strip()
        if cell.isdigit():
            return int(cell)
    return None


def index_bytes(db: Path) -> int:
    d = db.with_name(db.name + ".d") / "indexes"
    return sum(p.stat().st_size for p in d.glob("*.idx")) if d.exists() else 0


def run_dataset(name: str, args: argparse.Namespace, run_id: str, edges_count: int) -> list[Row]:
    data, work = args.data.resolve(), args.work.resolve()
    edges = data / f"{name}.parquet"
    w = data / f"{name.rsplit('-', 1)[0]}-w.parquet"
    weighted = w if name.endswith("-int") and w.exists() else None
    seeds = first_ids(edges)
    rows: list[Row] = []
    env = {**os.environ, "URSA_INDEX_STATS": "1"}
    limit = args.mem_limit_gb * 1024

    if "sql" in args.engines:
        dbdir = work / name
        if dbdir.exists():
            for p in sorted(dbdir.rglob("*"), reverse=True):
                p.unlink() if p.is_file() else p.rmdir()
        dbdir.mkdir(parents=True, exist_ok=True)
        db = dbdir / f"{name}.ursa"
        for step, query in sql_steps(edges, weighted, seeds).items():
            if args.steps and step not in args.steps:
                continue
            status, wall, rss, out, err = measure(
                [str(args.ursa), str(db), "-c", query],
                cwd=dbdir,
                env=env,
                timeout=args.timeout,
                mem_limit_mb=limit,
            )
            row = Row(run_id, name, edges_count, "sql", step, status, wall, rss)
            if m := STATS.search(err):
                row.built, row.loaded = int(m[1]), int(m[2])
            row.rows = first_number(out)
            if status != "ok":
                row.note = err.strip().splitlines()[-1][:200] if err.strip() else ""
            if step == "first_query":
                row.index_bytes = index_bytes(db)
            rows.append(row)
            print(fmt_row(row), file=sys.stderr, flush=True)

    if "python" in args.engines:
        for step in PY_STEPS:
            if args.steps and step not in args.steps:
                continue
            if step == "weighted" and weighted is None:
                continue
            status, wall, rss, out, err = measure(
                [
                    str(args.python),
                    "-c",
                    PY_CHILD,
                    step,
                    str(edges),
                    str(weighted or ""),
                    json.dumps(seeds),
                ],
                cwd=work,
                env=os.environ.copy(),
                timeout=args.timeout,
                mem_limit_mb=limit,
            )
            row = Row(run_id, name, edges_count, "python", step, status, wall, rss)
            if status == "ok":
                result = json.loads(out.strip().splitlines()[-1])
                row.rows, row.inner_s = result["rows"], result["inner_s"]
            else:
                row.note = err.strip().splitlines()[-1][:200] if err.strip() else ""
            rows.append(row)
            print(fmt_row(row), file=sys.stderr, flush=True)
    return rows


def fmt_row(r: Row) -> str:
    stats = f" built={r.built} loaded={r.loaded}" if r.built is not None else ""
    size = f" index={r.index_bytes / 2**20:.0f}MiB" if r.index_bytes else ""
    return (
        f"  {r.dataset:<14} {r.engine:<6} {r.step:<15} {r.status:<7} "
        f"{r.wall_s:8.2f}s {r.peak_rss_mb:8.0f}MB rows={r.rows}{stats}{size} {r.note}"
    )


def cell(r: Row | None) -> str:
    if r is None:
        return "-"
    if r.status != "ok":
        return f"{r.status.upper()} ({r.wall_s:.0f}s)"
    return f"{r.wall_s:.2f}s / {r.peak_rss_mb:,.0f} MB"


def summary(rows: list[Row]) -> str:
    """One Markdown table per dataset: steps × engine, wall / peak RSS."""
    out = []
    for dataset in dict.fromkeys(r.dataset for r in rows):
        rs = [r for r in rows if r.dataset == dataset]
        by = {(r.engine, r.step): r for r in rs}
        idx = next((r.index_bytes for r in rs if r.index_bytes), None)
        out.append(f"\n### {dataset} ({rs[0].edges:,} edges)\n")
        if idx:
            out.append(f"Stored index: {idx / 2**20:.0f} MiB\n")
        out.append("| step | SQL | Python | SQL built/loaded |")
        out.append("|---|---|---|---|")
        for step in SQL_STEPS:
            s, p = by.get(("sql", step)), by.get(("python", step))
            if s is None and p is None:
                continue
            bl = f"{s.built}/{s.loaded}" if s and s.built is not None else ""
            out.append(f"| {step} | {cell(s)} | {cell(p)} | {bl} |")
    return "\n".join(out)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Scale test of the ursa SQL binary (and the Python library).",
    )
    parser.add_argument("--data", type=Path, required=True, help="directory of gen.py output")
    parser.add_argument("--work", type=Path, required=True, help="directory for .ursa files")
    parser.add_argument("--out", type=Path, required=True, help="CSV of raw rows (appended)")
    parser.add_argument(
        "--datasets",
        default="",
        help="comma-separated names (e.g. rmat-1m-int); default: every -int/-str file",
    )
    parser.add_argument(
        "--ursa", type=Path, default=REPO / "target" / "release" / "ursa", help="ursa binary"
    )
    parser.add_argument(
        "--python",
        type=Path,
        default=Path(sys.executable),
        help="interpreter with ursa installed (default: this one)",
    )
    parser.add_argument("--engines", default="sql,python", help="default: %(default)s")
    parser.add_argument("--steps", default="", help="only these steps (comma-separated)")
    parser.add_argument("--timeout", type=float, default=900, help="seconds per step")
    parser.add_argument(
        "--mem-limit-gb", type=float, default=11, help="kill a step whose RSS passes this"
    )
    parser.add_argument("--label", default="", help="free text stored with every row")
    args = parser.parse_args(argv)
    args.engines = set(args.engines.split(","))
    args.steps = set(filter(None, args.steps.split(",")))
    args.work.mkdir(parents=True, exist_ok=True)

    import pyarrow.parquet as pq

    names = [n for n in args.datasets.split(",") if n] or sorted(
        (p.stem for p in args.data.glob("rmat-*.parquet") if not p.stem.endswith("-w")),
        key=lambda s: (pq.ParquetFile(args.data / f"{s}.parquet").metadata.num_rows, s),
    )
    rev = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"], cwd=REPO, capture_output=True, text=True
    ).stdout.strip()
    run_id = f"{time.strftime('%Y%m%dT%H%M%S')}-{rev}"
    print(
        f"run {run_id} on {platform.node()}: {os.cpu_count()} cpus, {platform.platform()}",
        file=sys.stderr,
    )
    rows: list[Row] = []
    for name in names:
        count = pq.ParquetFile(args.data / f"{name}.parquet").metadata.num_rows
        rows.extend(run_dataset(name, args, run_id, count))

    fields = [f for f in Row.__dataclass_fields__ if f != "extra"]
    new = not args.out.exists()
    with open(args.out, "a", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=[*fields, "label"])
        if new:
            writer.writeheader()
        for r in rows:
            d = asdict(r)
            d.pop("extra")
            writer.writerow({**d, "label": args.label})
    print(summary(rows))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
