"""Seeded synthetic graph generators → Parquet edge lists.

Synthetic graphs give a *size knob*: sweep node/edge count and watch the
asymptotic curve, which is where an accidental O(d²) shows up before a user hits
it. Three structures, each exercising a different kernel regime:

* **Erdős–Rényi** (``er``) — uniform random, flat degree distribution;
* **Barabási–Albert** (``ba``) — preferential attachment, heavy-tailed degree
  (the realistic hard case: a few very high-degree hubs);
* **Grid** (``grid``) — a 2-D lattice, bounded degree, long diameter.

Generation goes through NetworkX (already a bench dependency and the reference
tooling), then the edge list is written straight to Parquet as contiguous int64
``src``/``dst`` columns. Every generator is seeded, so a named dataset is bit-for-bit
reproducible across machines.
"""

from __future__ import annotations

import random
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq


def _weights(m: int, seed: int) -> list[float]:
    """A seeded positive weight per edge (decorrelated from the graph's own seed).

    Continuous weights in [0.1, 10) make exact-cost path ties vanishingly unlikely,
    so weighted shortest-path algorithms have a unique answer to agree on across
    libraries — the one place weighted correctness would otherwise get murky.
    """
    rng = random.Random((seed * 2654435761) & 0xFFFFFFFF)
    return [rng.uniform(0.1, 10.0) for _ in range(m)]


def _write_edges(
    src: list[int], dst: list[int], path: str | Path, weights: list[float] | None = None
) -> Path:
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    cols = {"src": pa.array(src, pa.int64()), "dst": pa.array(dst, pa.int64())}
    if weights is not None:
        cols["weight"] = pa.array(weights, pa.float64())
    pq.write_table(pa.table(cols), path)
    return path


def er(n: int, avg_degree: float, seed: int, path: str | Path) -> Path:
    """Directed Erdős–Rényi G(n, p) with ``p`` set to hit ``avg_degree``."""
    import networkx as nx

    p = min(1.0, avg_degree / max(n - 1, 1))
    g = nx.gnp_random_graph(n, p, seed=seed, directed=True)
    src = [int(u) for u, _ in g.edges()]
    dst = [int(v) for _, v in g.edges()]
    return _write_edges(src, dst, path, _weights(len(src), seed))


def ba(n: int, m: int, seed: int, path: str | Path) -> Path:
    """Barabási–Albert preferential-attachment graph, oriented as a directed edge list.

    BA is intrinsically undirected; each undirected edge becomes one directed
    ``u→v`` edge. The heavy-tailed degree distribution is the point — it stresses
    the high-degree adjacency paths.
    """
    import networkx as nx

    g = nx.barabasi_albert_graph(n, m, seed=seed)
    src = [int(u) for u, v in g.edges()]
    dst = [int(v) for u, v in g.edges()]
    return _write_edges(src, dst, path, _weights(len(src), seed))


def grid(side: int, path: str | Path) -> Path:
    """A ``side × side`` 2-D lattice, relabelled to contiguous ints, directed edges."""
    import networkx as nx

    g = nx.grid_2d_graph(side, side)
    g = nx.convert_node_labels_to_integers(g, ordering="sorted")
    src = [int(u) for u, v in g.edges()]
    dst = [int(v) for u, v in g.edges()]
    return _write_edges(src, dst, path, _weights(len(src), side))


def sbm(n: int, blocks: int, p_in: float, p_out: float, seed: int, path: str | Path) -> Path:
    """Stochastic block model — ``blocks`` equal-size communities, dense within
    (``p_in``) and sparse between (``p_out``), oriented as a directed edge list.

    This is the *honest* test for community detection (Louvain, label propagation):
    ER/BA graphs have no planted community structure, so a high modularity there is
    partly luck; an SBM has a ground-truth partition to recover.
    """
    import networkx as nx

    sizes = [n // blocks] * blocks
    probs = [[p_in if i == j else p_out for j in range(blocks)] for i in range(blocks)]
    g = nx.stochastic_block_model(sizes, probs, seed=seed)
    src = [int(u) for u, v in g.edges()]
    dst = [int(v) for u, v in g.edges()]
    return _write_edges(src, dst, path, _weights(len(src), seed))
