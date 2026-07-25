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

from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq


def _write_edges(src: list[int], dst: list[int], path: str | Path) -> Path:
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    table = pa.table({"src": pa.array(src, pa.int64()), "dst": pa.array(dst, pa.int64())})
    pq.write_table(table, path)
    return path


def er(n: int, avg_degree: float, seed: int, path: str | Path) -> Path:
    """Directed Erdős–Rényi G(n, p) with ``p`` set to hit ``avg_degree``."""
    import networkx as nx

    p = min(1.0, avg_degree / max(n - 1, 1))
    g = nx.gnp_random_graph(n, p, seed=seed, directed=True)
    src = [int(u) for u, _ in g.edges()]
    dst = [int(v) for _, v in g.edges()]
    return _write_edges(src, dst, path)


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
    return _write_edges(src, dst, path)


def grid(side: int, path: str | Path) -> Path:
    """A ``side × side`` 2-D lattice, relabelled to contiguous ints, directed edges."""
    import networkx as nx

    g = nx.grid_2d_graph(side, side)
    g = nx.convert_node_labels_to_integers(g, ordering="sorted")
    src = [int(u) for u, v in g.edges()]
    dst = [int(v) for u, v in g.edges()]
    return _write_edges(src, dst, path)
