"""Compare Ursa (SQL and DataFrame) with igraph, rustworkx and NetworkX.

For each prepared graph (``prep.py``), every engine runs every algorithm in its
own process, with a time limit and a memory ceiling:

- ``ursa-sql``: the ``ursa`` binary. One process builds and stores the graph
  index in a fresh database (ingest), then one process per algorithm writes its
  result with the index stored (warm). Cold = ingest + warm. It is the reference
  the other engines' results are checked against.
- ``ursa-df``, ``igraph``, ``rustworkx``, ``networkx``: ``cell.py`` in the given
  Python interpreter. Cold = ingest + the first run; warm = a second run on the
  same handle (skipped when the first took over ``--warm-limit`` seconds).

Each row records ingest/cold/warm seconds, peak RSS and CPU time of the cell's
process(es), status (``ok``, ``unsupported``, ``error``, ``timeout``,
``memory``) and a correctness check against ``ursa-sql``:

- degree, triangles: exact;
- PageRank: the largest absolute difference within 1e-3 of the largest score;
- weak/strong components: the same partition;
- Louvain: modularity, on the undirected simple graph, at least the reference's
  minus 0.05 (communities differ between heuristics; quality is what counts).

Writes ``results.json`` and ``summary.md`` to ``--out``. Usage::

    python compare.py --graph /tmp/cmp/bsky-5m --out /tmp/cmp/out \\
        --engines ursa-sql,ursa-df,igraph,rustworkx,networkx --timeout 1800 --mem-limit-gb 100
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

import numpy as np
import pyarrow.parquet as pq

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import cell  # noqa: E402

ENGINES = ("ursa-sql", "ursa-df", "igraph", "rustworkx", "networkx")
THREAD_VARS = (
    "RAYON_NUM_THREADS",
    "OMP_NUM_THREADS",
    "OPENBLAS_NUM_THREADS",
    "MKL_NUM_THREADS",
    "NUMEXPR_NUM_THREADS",
    "VECLIB_MAXIMUM_THREADS",
)
SQL = {
    "degree": ("degree(g)", "degree"),
    "pagerank": (
        f"pagerank(g, damping => {cell.DAMPING}, max_iter => {cell.MAX_ITER}, tol => {cell.TOL})",
        "pagerank",
    ),
    "wcc": ("connected_components(g)", "connected_components"),
    "scc": ("connected_components(g, mode => 'strong')", "connected_components"),
    "triangles": ("triangle_count(g)", "triangle_count"),
    "louvain": (f"louvain(g, seed => {cell.SEED})", "louvain"),
}


# --- running one process with limits ------------------------------------------


def _rss_kb(pid: int) -> int:
    try:
        if sys.platform == "linux":
            with open(f"/proc/{pid}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        return int(line.split()[1])
            return 0
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True)
        return int(out.stdout.strip() or 0)
    except (OSError, ValueError):
        return 0


def run_limited(cmd: list[str], env: dict, timeout: float, mem_limit: int) -> dict:
    """Run a process; kill it at the time or memory limit. Returns measurements."""
    t0 = time.monotonic()
    proc = subprocess.Popen(
        cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, start_new_session=True
    )
    buf: dict[str, list[bytes]] = {"out": [], "err": []}

    def drain(stream, key):
        for block in iter(lambda: stream.read(1 << 16), b""):
            buf[key].append(block)

    readers = [
        threading.Thread(target=drain, args=(proc.stdout, "out"), daemon=True),
        threading.Thread(target=drain, args=(proc.stderr, "err"), daemon=True),
    ]
    for t in readers:
        t.start()
    status, polled_peak, last_poll = "ok", 0, 0.0
    while True:
        pid, wstatus, ru = os.wait4(proc.pid, os.WNOHANG)
        if pid:
            break
        now = time.monotonic()
        if now - last_poll >= 0.5:
            last_poll = now
            rss = _rss_kb(proc.pid) * 1024
            polled_peak = max(polled_peak, rss)
            if mem_limit and rss > mem_limit:
                status = "memory"
        if now - t0 > timeout:
            status = "timeout"
        if status != "ok":
            os.killpg(proc.pid, signal.SIGKILL)
            pid, wstatus, ru = os.wait4(proc.pid, 0)
            break
        time.sleep(0.05)
    wall = time.monotonic() - t0
    proc.returncode = os.waitstatus_to_exitcode(wstatus)
    for t in readers:
        t.join()
    if status == "ok" and proc.returncode != 0:
        status = "error"
    maxrss = ru.ru_maxrss if sys.platform == "darwin" else ru.ru_maxrss * 1024
    err = b"".join(buf["err"]).decode("utf-8", "replace")
    return {
        "status": status,
        "wall_s": wall,
        "user_s": ru.ru_utime,
        "sys_s": ru.ru_stime,
        "peak_rss_bytes": max(maxrss, polled_peak),
        "stdout": b"".join(buf["out"]).decode("utf-8", "replace"),
        "error": "" if status == "ok" else ("\n".join(err.strip().splitlines()[-6:]) or status),
    }


# --- engines -------------------------------------------------------------------


def ursa_version(ursa: str) -> str:
    return subprocess.run([ursa, "--version"], capture_output=True, text=True).stdout.strip()


def run_sql(graph: Path, work: Path, algos, a, env) -> list[dict]:
    """ursa-sql: build and store the index once, then one process per algorithm."""
    db = work / "sql" / "g.ursa"
    if db.parent.exists():
        shutil.rmtree(db.parent)
    db.parent.mkdir(parents=True)
    edges = (graph / "edges.parquet").as_posix()
    define = (
        f"SET ursa.index_storage_limit = '512GiB'; CREATE VIEW e AS FROM '{edges}'; "
        "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (src) DESTINATION KEY (dst))"
    )
    base = [a.ursa, str(db), "--no-init", "--quiet", "--format", "csv"]
    # The catalog persists; the storage limit is per session, so set it again below.
    subprocess.run([*base, "-c", define], check=True, env=env, stdout=subprocess.DEVNULL)
    lim = "SET ursa.index_storage_limit = '512GiB'; "
    build = run_limited(
        [*base, "-c", lim + "SELECT count(*) FROM degree(g)"], env, a.timeout, a.mem_limit
    )
    version = ursa_version(a.ursa)
    rows = []
    for algo in algos:
        row = {"engine": "ursa-sql", "algo": algo, "version": version}
        if build["status"] != "ok":
            rows.append(
                {
                    **row,
                    "status": build["status"],
                    "error": "index build: " + build["error"],
                    "peak_rss_bytes": build["peak_rss_bytes"],
                }
            )
            continue
        fn, col = SQL[algo]
        res = work / "sql" / f"{algo}.parquet"
        q = lim + f"COPY (SELECT id, {col} AS value FROM {fn}) TO '{res.as_posix()}'"
        r = run_limited([*base, "-c", q], env, a.timeout, a.mem_limit)
        row.update(
            status=r["status"],
            error=r["error"],
            ingest_s=build["wall_s"],
            warm_s=r["wall_s"],
            cold_s=build["wall_s"] + r["wall_s"],
            user_s=build["user_s"] + r["user_s"],
            sys_s=build["sys_s"] + r["sys_s"],
            peak_rss_bytes=max(build["peak_rss_bytes"], r["peak_rss_bytes"]),
            result=str(res) if r["status"] == "ok" else "",
        )
        rows.append(row)
    return rows


def run_py(engine: str, graph: Path, work: Path, algos, a, env) -> list[dict]:
    rows = []
    for algo in algos:
        row = {"engine": engine, "algo": algo}
        if not cell.supports(engine, algo):
            rows.append({**row, "status": "unsupported"})
            continue
        res = work / engine / f"{algo}.npz"
        res.parent.mkdir(parents=True, exist_ok=True)
        cmd = [
            a.python,
            "-I",
            str(HERE / "cell.py"),
            "--engine",
            engine,
            "--algo",
            algo,
            "--graph",
            str(graph),
            "--out",
            str(res),
            "--warm",
            str(a.warm_limit),
        ]
        r = run_limited(cmd, env, a.timeout, a.mem_limit)
        row.update(
            status=r["status"],
            error=r["error"],
            user_s=r["user_s"],
            sys_s=r["sys_s"],
            peak_rss_bytes=r["peak_rss_bytes"],
            wall_s=r["wall_s"],
        )
        if r["status"] == "ok":
            rec = json.loads(r["stdout"].strip().splitlines()[-1])
            row.update(
                version=rec["version"],
                ingest_s=rec["ingest_s"],
                cold_s=rec["cold_s"],
                warm_s=rec["warm_s"] if rec["warm_s"] is not None else rec["first_s"],
                warm_measured=rec["warm_s"] is not None,
                result=str(res),
            )
        rows.append(row)
    return rows


# --- correctness -------------------------------------------------------------------


def load_result(path: str) -> tuple[np.ndarray, np.ndarray]:
    if path.endswith(".parquet"):
        t = pq.read_table(path)
        ids, vals = t.column("id").to_numpy(), t.column("value").to_numpy()
    else:
        z = np.load(path)
        ids, vals = z["ids"], z["values"]
    order = np.argsort(ids, kind="stable")
    return ids[order], vals[order]


def canonical_partition(labels: np.ndarray) -> np.ndarray:
    """Relabel each group by the position of its first member.

    The ids are sorted, so that is the group's smallest id: two labelings of the
    same partition give the same array whatever their label values.
    """
    _, first, inv = np.unique(labels, return_index=True, return_inverse=True)
    return first[inv]


class Modularity:
    """Modularity of a partition on the undirected simple graph of the edge list."""

    def __init__(self, graph: Path):
        t = pq.read_table(graph / "edges.parquet")
        s, d = t.column("src").to_numpy(), t.column("dst").to_numpy()
        lo, hi = np.minimum(s, d), np.maximum(s, d)
        pairs = np.unique(np.stack([lo, hi], axis=1), axis=0)
        self.u, self.v = pairs[:, 0], pairs[:, 1]
        self.m = len(pairs)

    def __call__(self, ids: np.ndarray, labels: np.ndarray) -> float:
        lu = labels[np.searchsorted(ids, self.u)]
        lv = labels[np.searchsorted(ids, self.v)]
        _, comm = np.unique(labels, return_inverse=True)
        k = len(np.unique(comm))
        inside = np.bincount(comm[np.searchsorted(ids, self.u)][lu == lv], minlength=k).astype(
            float
        )
        deg = np.bincount(
            np.concatenate([np.searchsorted(ids, self.u), np.searchsorted(ids, self.v)]),
            minlength=len(ids),
        ).astype(float)
        dsum = np.bincount(comm, weights=deg, minlength=k)
        return float((inside / self.m - (dsum / (2 * self.m)) ** 2).sum())


def check(algo: str, ref: tuple, cand: tuple, mod: Modularity | None) -> tuple[bool | None, str]:
    rid, rv = ref
    cid, cv = cand
    if not np.array_equal(rid, cid):
        return False, f"node sets differ ({len(cid)} vs {len(rid)} nodes)"
    if algo in ("degree", "triangles"):
        bad = int((rv.astype(np.int64) != cv.astype(np.int64)).sum())
        return bad == 0, "exact" if bad == 0 else f"{bad} nodes differ"
    if algo == "pagerank":
        diff = float(np.abs(rv - cv).max())
        ok = diff <= 1e-3 * float(rv.max())
        top_r, top_c = set(rid[np.argsort(-rv)[:10]]), set(cid[np.argsort(-cv)[:10]])
        return (
            ok,
            f"max abs diff {diff:.2e} ({diff / rv.max():.1e} of the top score); "
            f"top-10 overlap {len(top_r & top_c)}/10",
        )
    if algo in ("wcc", "scc"):
        same = np.array_equal(canonical_partition(rv), canonical_partition(cv))
        k = len(np.unique(cv))
        return (
            same,
            f"{k} components, same partition"
            if same
            else f"partition differs ({k} vs {len(np.unique(rv))})",
        )
    if algo == "louvain" and mod is not None:
        qr, qc = mod(rid, rv), mod(cid, cv)
        return (
            qc >= qr - 0.05,
            f"modularity {qc:.4f} vs ursa-sql {qr:.4f}; {len(np.unique(cv))} communities",
        )
    return None, ""


# --- report -------------------------------------------------------------------------


def fmt_s(v) -> str:
    if v is None:
        return ""
    return f"{v * 1000:.0f} ms" if v < 1 else f"{v:.2f} s" if v < 100 else f"{v / 60:.1f} min"


def summary_md(meta: dict, rows: list[dict]) -> str:
    out = [
        f"## Comparison: {meta['graph']} ({meta['m']:,} edges, {meta['n']:,} nodes)",
        "",
        f"- Host: {meta['host']}, {meta['cpus']} CPUs; threads per engine: {meta['threads']}",
        f"- Limits per cell: {meta['timeout']:.0f} s, {meta['mem_limit_gb']} GB",
        "",
        "| Algorithm | Engine | Status | Ingest | Cold | Warm | Peak RSS | Cores | Correct |",
        "|---|---|---|---:|---:|---:|---:|---:|---|",
    ]
    for algo in meta["algos"]:
        for r in [r for r in rows if r["algo"] == algo]:
            cores = ""
            if r.get("status") == "ok" and r.get("cold_s"):
                cores = (
                    f"{(r['user_s'] + r['sys_s']) / max(r.get('wall_s', r['cold_s']), 1e-9):.1f}"
                )
            corr = {True: "yes", False: "**NO**", None: ""}[r.get("correct")]
            if r.get("check"):
                corr += f" ({r['check']})"
            warm = fmt_s(r.get("warm_s")) + ("" if r.get("warm_measured", True) else "*")
            peak = f"{r['peak_rss_bytes'] / 1e9:.2f} GB" if r.get("peak_rss_bytes", 0) > 0 else ""
            status = r["status"] if r["status"] == "ok" else f"**{r['status']}**"
            out.append(
                f"| {algo} | {r['engine']} | {status} | {fmt_s(r.get('ingest_s'))} | "
                f"{fmt_s(r.get('cold_s'))} | {warm} | {peak} | {cores} | {corr} |"
            )
    out += [
        "",
        "\\* first run only: warm skipped because the first took longer than the warm limit.",
        "Cold = file to result (ingest + first run). For ursa-sql, warm is a new `ursa` process "
        "with the index stored, so it includes loading the index.",
    ]
    errs = [r for r in rows if r.get("error")]
    for r in errs:
        out += [
            "",
            f"**{r['engine']} / {r['algo']}: {r['status']}**",
            "```",
            r["error"][-800:],
            "```",
        ]
    return "\n".join(out) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--graph", required=True, type=Path, help="a directory from prep.py")
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--engines", default=",".join(ENGINES))
    ap.add_argument("--algos", default=",".join(cell.ALGOS))
    ap.add_argument("--timeout", type=float, default=1800, help="seconds per cell")
    ap.add_argument(
        "--mem-limit-gb", type=float, default=0, help="kill a cell above this RSS (0: none)"
    )
    ap.add_argument(
        "--warm-limit", type=float, default=60, help="skip the warm run when the first took longer"
    )
    ap.add_argument(
        "--threads", type=int, default=os.cpu_count(), help="thread budget for every engine"
    )
    ap.add_argument("--ursa", default=shutil.which("ursa") or "ursa")
    ap.add_argument(
        "--python", default=sys.executable, help="interpreter with the Python engines installed"
    )
    a = ap.parse_args()
    a.mem_limit = int(a.mem_limit_gb * 1e9)
    engines = [e for e in a.engines.split(",") if e]
    algos = [x for x in a.algos.split(",") if x]
    gmeta = json.loads((a.graph / "meta.json").read_text())
    work = a.out / "work"
    work.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    for v in THREAD_VARS:
        env[v] = str(a.threads)
    meta = {
        "graph": a.graph.name,
        "m": gmeta["m"],
        "n": gmeta["n"],
        "algos": algos,
        "engines": engines,
        "host": os.environ.get("BENCH_HOST_LABEL") or platform.platform(),
        "cpus": os.cpu_count(),
        "threads": a.threads,
        "timeout": a.timeout,
        "mem_limit_gb": a.mem_limit_gb,
        "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    rows: list[dict] = []
    for engine in engines:
        print(f"== {engine}", flush=True)
        new = (
            run_sql(a.graph, work, algos, a, env)
            if engine == "ursa-sql"
            else run_py(engine, a.graph, work, algos, a, env)
        )
        for r in new:
            print(
                f"   {r['algo']:10} {r['status']:11} cold {fmt_s(r.get('cold_s')):>9}"
                f"  warm {fmt_s(r.get('warm_s')):>9}"
                f"  peak {r.get('peak_rss_bytes', 0) / 1e9:6.2f} GB",
                flush=True,
            )
        rows += new

    # Correctness against ursa-sql.
    refs = {
        r["algo"]: load_result(r["result"])
        for r in rows
        if r["engine"] == "ursa-sql" and r.get("result")
    }
    mod = Modularity(a.graph) if "louvain" in refs else None
    for r in rows:
        if r["engine"] == "ursa-sql" or not r.get("result") or r["algo"] not in refs:
            continue
        r["correct"], r["check"] = check(r["algo"], refs[r["algo"]], load_result(r["result"]), mod)
    if mod is not None:
        for r in rows:
            if r["engine"] == "ursa-sql" and r["algo"] == "louvain" and r.get("result"):
                ids, lab = refs["louvain"]
                r["check"] = (
                    f"modularity {mod(ids, lab):.4f}; {len(np.unique(lab))} communities (reference)"
                )
    meta["finished"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    (a.out / "results.json").write_text(json.dumps({"meta": meta, "rows": rows}, indent=2) + "\n")
    (a.out / "summary.md").write_text(summary_md(meta, rows))
    print(f"wrote {a.out / 'results.json'} and {a.out / 'summary.md'}", flush=True)
    bad = [f"{r['engine']}/{r['algo']}" for r in rows if r.get("correct") is False]
    if bad:
        print("compare.py: incorrect results: " + ", ".join(bad), file=sys.stderr)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
