"""The plan-layer scenarios: one per process, printing `name<TAB>seconds<TAB>peak_rss_mb`.

    python scenarios.py DATA_DIR SCENARIO
    python scenarios.py --list

Each scenario builds what it needs from the Parquet files `gen.py` wrote, warms the
index where the point is a repeat collect, and times the step under test. Times are
the best of a few runs (one run for the slow kernels); peak RSS is the process max.
`run.py` calls this once per scenario so no scenario inherits another's memory.
"""

from __future__ import annotations

import resource
import sys
import time
from collections.abc import Callable

import pyarrow.parquet as pq

import ursa as ur

DATA = sys.argv[1] if len(sys.argv) > 1 else "data"


def path(name: str) -> str:
    return f"{DATA}/{name}.parquet"


def mem(name: str) -> ur.EdgeFrame:
    return ur.from_arrow(pq.read_table(path(name)), src="s", dst="d")


def scan(name: str) -> ur.EdgeFrame:
    return ur.scan_edges(path(name), src="s", dst="d")


def warm(e: ur.EdgeFrame) -> ur.EdgeFrame:
    """Build the index (an unweighted degree collect), so the timed step is a repeat."""
    ur.degree(e).collect()
    return e


def best(f: Callable[[], object], k: int = 3) -> float:
    b = float("inf")
    for _ in range(k):
        t = time.perf_counter()
        f()
        b = min(b, time.perf_counter() - t)
    return b


def once(f: Callable[[], object]) -> float:
    t = time.perf_counter()
    f()
    return time.perf_counter() - t


def nodes_1m() -> ur.NodeFrame:
    return ur.from_arrow(pq.read_table(path("nodes1m")), id="id")


W = ur.col("w")


def build_mem_compact():
    e = mem("e10m")
    return once(lambda: ur.degree(e).collect())


def build_mem_sparse():
    e = mem("e10m_sparse")
    return once(lambda: ur.degree(e).collect())


def build_scan_compact():
    e = scan("e10m")
    return once(lambda: ur.degree(e).collect())


def pagerank_repeat():
    e = warm(mem("e10m"))
    return best(lambda: ur.pagerank(e).collect())


def pagerank_w_mem():
    e = warm(mem("e10m"))
    ur.pagerank(e, weight=W).collect()
    return best(lambda: ur.pagerank(e, weight=W).collect())


def pagerank_w_scan():
    e = warm(scan("e10m"))
    ur.pagerank(e, weight=W).collect()
    return best(lambda: ur.pagerank(e, weight=W).collect())


def pagerank_both():
    e = warm(mem("e10m"))
    ur.pagerank(e, direction="both").collect()
    return best(lambda: ur.pagerank(e, direction="both").collect())


def pagerank_both_w():
    e = warm(mem("e10m"))
    ur.pagerank(e, weight=W, direction="both").collect()
    return best(lambda: ur.pagerank(e, weight=W, direction="both").collect())


def masked_degree_scan():
    e = warm(scan("e10m"))
    f = e.filter(ur.col("w") > 0.5)
    ur.degree(f).collect()
    return best(lambda: ur.degree(f).collect())


def attach_degree():
    e, nodes = warm(mem("e10m")), nodes_1m()
    return best(lambda: nodes.with_columns(dg=ur.degree(e)).collect())


def attach_pagerank():
    e, nodes = warm(mem("e10m")), nodes_1m()
    return best(lambda: nodes.with_columns(pr=ur.pagerank(e)).collect())


def triangles():
    e = warm(mem("e10m"))
    return best(lambda: ur.triangle_count(e).collect(), 2)


def components():
    e = warm(mem("e10m"))
    return best(lambda: ur.connected_components(e).collect())


def label_prop():
    e = warm(mem("e10m"))
    return best(lambda: ur.label_propagation(e, seed=1).collect(), 2)


def closeness_w_80k():
    e = warm(mem("e80k"))
    return best(lambda: ur.closeness(e, weight=W).collect(), 2)


def avg_path_50k():
    e = warm(mem("e50k"))
    return best(lambda: ur.avg_path_length(e), 2)


def betweenness_1m():
    e = warm(mem("e1m"))
    return once(lambda: ur.betweenness(e, sample=0.005, seed=1).collect())


def betweenness_w_1m():
    e = warm(mem("e1m"))
    return once(lambda: ur.betweenness(e, sample=0.002, weight=W, seed=1).collect())


def louvain_500k():
    e = warm(mem("e500k"))
    return once(lambda: ur.louvain(e, seed=1).collect())


def small_collects():
    """Per-collect overhead on a tiny graph (seconds per collect)."""
    k = ur.datasets.load_karate()
    ur.degree(k).collect()
    return once(lambda: [ur.degree(k).collect() for _ in range(300)]) / 300


def pipeline_scan():
    """The cross-feature path: a scan, an attribute frame, weighted, both-direction,
    component and community columns in one query with a relational tail, plus a
    subgraph column (its own collect: one query's algorithms share one edge frame)."""
    e = scan("e1m")
    table = pq.read_table(path("nodes1m"))
    nodes = ur.from_arrow(table.filter(table["id"].to_numpy() < 200_000), id="id")
    f = e.filter(ur.col("w") > 0.2)

    def q():
        nodes.with_columns(
            pr=ur.pagerank(e, weight=W),
            prb=ur.pagerank(e, direction="both"),
            cc=ur.connected_components(e),
            lv=ur.louvain(e, seed=1),
        ).filter(ur.col("cc") >= 0).sort("pr", descending=True).head(100).collect()
        nodes.with_columns(dg=ur.degree(f)).filter(ur.col("dg") > 1).collect()

    q()
    return best(q)


SCENARIOS: dict[str, Callable[[], float]] = {
    f.__name__: f
    for f in (
        build_mem_compact,
        build_mem_sparse,
        build_scan_compact,
        pagerank_repeat,
        pagerank_w_mem,
        pagerank_w_scan,
        pagerank_both,
        pagerank_both_w,
        masked_degree_scan,
        attach_degree,
        attach_pagerank,
        triangles,
        components,
        label_prop,
        closeness_w_80k,
        avg_path_50k,
        betweenness_1m,
        betweenness_w_1m,
        louvain_500k,
        small_collects,
        pipeline_scan,
    )
}

if __name__ == "__main__":
    if sys.argv[1] == "--list":
        print("\n".join(SCENARIOS))
        sys.exit()
    name = sys.argv[2]
    dt = SCENARIOS[name]()
    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024
    print(f"{name}\t{dt:.6g}\t{peak:.0f}", flush=True)
