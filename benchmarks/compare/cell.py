"""One measured cell: an engine runs one algorithm on one prepared graph.

Runs in its own process (``compare.py`` starts it), so its peak RSS, CPU time
and imports are its own. Reads the prepared graph (``prep.py``):

- ``edges.parquet``: ``src``, ``dst`` (int64 node ids), deduplicated, no self-loops;
- ``dense.npz``: ``src``, ``dst`` as dense int32 indices and ``ids``, the int64 id
  of each index.

The Python libraries ingest the dense arrays (the fair, fast path for each);
``ursa-df`` scans the Parquet file itself, as its users do. ``ursa-sql`` is not
run here: ``compare.py`` drives the ``ursa`` binary directly.

Writes the result (``ids``, ``values``) to ``--out`` as ``.npz`` and prints one
JSON object of measurements to stdout. Usage (normally only from compare.py)::

    python cell.py --engine igraph --algo pagerank --graph DIR --out res.npz
"""

from __future__ import annotations

import argparse
import json
import resource
import sys
import time
from pathlib import Path

import numpy as np

ENGINES = ("ursa-df", "igraph", "rustworkx", "networkx")
ALGOS = ("degree", "pagerank", "wcc", "scc", "triangles", "louvain")

# Canonical parameters, the same for every engine.
DAMPING, MAX_ITER, TOL, SEED = 0.85, 100, 1e-6, 1

# Algorithms an engine has no implementation of (recorded as `unsupported`).
UNSUPPORTED = {
    # Neither exposes a per-node triangle count (as in the July flywheel).
    "igraph": {"triangles"},
    "rustworkx": {"triangles", "louvain"},
}


def supports(engine: str, algo: str) -> bool:
    return algo not in UNSUPPORTED.get(engine, set())


# --- ingest -----------------------------------------------------------------


def ingest(engine: str, graph: Path, algo: str):
    """Build the engine's graph handle; returns (handle, n, ids)."""
    if engine == "ursa-df":
        import ursa as ur

        return ur.scan_edges(str(graph / "edges.parquet"), src="src", dst="dst"), None, None
    z = np.load(graph / "dense.npz")
    src, dst, ids = z["src"], z["dst"], z["ids"]
    n = len(ids)
    undirected = algo in ("triangles", "louvain")
    if engine == "igraph":
        import igraph as ig

        g = ig.Graph(n=n, edges=np.column_stack([src, dst]), directed=True)
        if undirected:
            # Mutual follows become one undirected edge, as a user would build it.
            g = g.as_undirected(mode="collapse")
        return g, n, ids
    if engine == "rustworkx":
        import rustworkx as rx

        g = rx.PyDiGraph()
        g.add_nodes_from(range(n))
        g.extend_from_edge_list(list(zip(src.tolist(), dst.tolist(), strict=True)))
        return g, n, ids
    if engine == "networkx":
        import networkx as nx

        g = nx.DiGraph()
        g.add_nodes_from(range(n))
        g.add_edges_from(zip(src.tolist(), dst.tolist(), strict=True))
        if undirected:
            g = g.to_undirected(as_view=False)
        return g, n, ids
    raise ValueError(engine)


# --- algorithms ---------------------------------------------------------------


def _labels_from_sets(sets, n: int) -> np.ndarray:
    lab = np.empty(n, dtype=np.int64)
    for i, members in enumerate(sets):
        lab[list(members)] = i
    return lab


def run(engine: str, handle, n: int | None, ids, algo: str) -> tuple[np.ndarray, np.ndarray]:
    """Run ``algo``; return (original node ids, values), in any order."""
    if engine == "ursa-df":
        import ursa as ur

        e = handle
        if algo == "degree":
            f, col = ur.degree(e, direction="out"), "degree"
        elif algo == "pagerank":
            f, col = ur.pagerank(e, damping=DAMPING, max_iter=MAX_ITER, tol=TOL), "pagerank"
        elif algo == "wcc":
            f, col = ur.connected_components(e, mode="weak"), "connected_components"
        elif algo == "scc":
            f, col = ur.connected_components(e, mode="strong"), "connected_components"
        elif algo == "triangles":
            f, col = ur.triangle_count(e), "triangle_count"
        elif algo == "louvain":
            f, col = ur.louvain(e, seed=SEED), "louvain"
        else:
            raise ValueError(algo)
        t = f.collect().to_arrow()
        return t.column("id").to_numpy(), t.column(col).to_numpy()

    if engine == "igraph":
        g = handle
        if algo == "degree":
            v = np.asarray(g.outdegree(), dtype=np.int64)
        elif algo == "pagerank":
            v = np.asarray(g.pagerank(damping=DAMPING, directed=True), dtype=np.float64)
        elif algo in ("wcc", "scc"):
            mode = "weak" if algo == "wcc" else "strong"
            v = np.asarray(g.connected_components(mode=mode).membership, dtype=np.int64)
        elif algo == "louvain":
            import random

            random.seed(SEED)  # igraph's default RNG is Python's
            v = np.asarray(g.community_multilevel().membership, dtype=np.int64)
        else:
            raise ValueError(algo)
        return ids, v

    if engine == "rustworkx":
        import rustworkx as rx

        g = handle
        if algo == "degree":
            v = np.fromiter((g.out_degree(i) for i in range(n)), dtype=np.int64, count=n)
        elif algo == "pagerank":
            # rustworkx, like NetworkX, stops when the L1 change is below n * tol;
            # Ursa stops below tol. tol / n asks for the same convergence.
            pr = rx.pagerank(g, alpha=DAMPING, max_iter=MAX_ITER, tol=TOL / n)
            v = np.zeros(n)
            v[np.fromiter(pr.keys(), dtype=np.int64)] = np.fromiter(pr.values(), dtype=np.float64)
        elif algo == "wcc":
            v = _labels_from_sets(rx.weakly_connected_components(g), n)
        elif algo == "scc":
            v = _labels_from_sets(rx.strongly_connected_components(g), n)
        else:
            raise ValueError(algo)
        return ids, v

    if engine == "networkx":
        import networkx as nx

        g = handle
        if algo == "degree":
            v = np.fromiter((d for _, d in sorted(g.out_degree())), dtype=np.int64, count=n)
        elif algo == "pagerank":
            # NetworkX stops when the L1 change is below n * tol (Ursa: below tol);
            # tol / n asks for the same convergence. With tol=1e-6 on 50k nodes it
            # otherwise stops after a few iterations, near the uniform start.
            pr = nx.pagerank(g, alpha=DAMPING, max_iter=MAX_ITER, tol=TOL / n)
            v = np.fromiter((pr[i] for i in range(n)), dtype=np.float64, count=n)
        elif algo == "wcc":
            v = _labels_from_sets(nx.weakly_connected_components(g), n)
        elif algo == "scc":
            v = _labels_from_sets(nx.strongly_connected_components(g), n)
        elif algo == "triangles":
            t = nx.triangles(g)
            v = np.fromiter((t[i] for i in range(n)), dtype=np.int64, count=n)
        elif algo == "louvain":
            comms = nx.community.louvain_communities(g, seed=SEED)
            v = _labels_from_sets(comms, n)
        else:
            raise ValueError(algo)
        return ids, v
    raise ValueError(engine)


def preload(engine: str) -> None:
    """Import the engine's modules before the clock starts.

    Imports are not part of a library's speed. NetworkX loads SciPy lazily inside
    ``nx.pagerank``, which made a first call 13 s slower than a warm one.
    """
    if engine == "ursa-df":
        import ursa  # noqa: F401
    elif engine == "igraph":
        import igraph  # noqa: F401
    elif engine == "rustworkx":
        import rustworkx  # noqa: F401
    elif engine == "networkx":
        import networkx
        import networkx.algorithms.community  # noqa: F401
        import scipy.sparse
        import scipy.sparse.linalg  # noqa: F401


def version(engine: str) -> str:
    mod = {"ursa-df": "ursa", "igraph": "igraph", "rustworkx": "rustworkx", "networkx": "networkx"}[
        engine
    ]
    return getattr(__import__(mod), "__version__", "unknown")


def _rss_bytes() -> int:
    v = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return v if sys.platform == "darwin" else v * 1024


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True, choices=ENGINES)
    ap.add_argument("--algo", required=True, choices=ALGOS)
    ap.add_argument("--graph", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument(
        "--warm",
        type=float,
        default=60.0,
        help="run the algorithm a second time only if the first took less than this (s)",
    )
    a = ap.parse_args()
    preload(a.engine)
    rec = {"engine": a.engine, "algo": a.algo, "version": version(a.engine)}
    t0 = time.perf_counter()
    handle, n, ids = ingest(a.engine, a.graph, a.algo)
    t1 = time.perf_counter()
    out_ids, values = run(a.engine, handle, n, ids, a.algo)
    t2 = time.perf_counter()
    rec.update(ingest_s=t1 - t0, first_s=t2 - t1, cold_s=t2 - t0)
    if rec["first_s"] < a.warm:
        t3 = time.perf_counter()
        run(a.engine, handle, n, ids, a.algo)
        rec["warm_s"] = time.perf_counter() - t3
    else:
        rec["warm_s"] = None  # skipped to bound cost; the first run stands in
    np.savez(a.out, ids=np.asarray(out_ids, dtype=np.int64), values=np.asarray(values))
    rec["peak_rss_bytes"] = _rss_bytes()
    print(json.dumps(rec))
    return 0


if __name__ == "__main__":
    sys.exit(main())
