"""NetworkX adapter — the correctness oracle / floor.

NetworkX is pure Python (its pagerank is scipy-backed); it is not a serious speed
competitor, but it is the *reference*: its result defines "correct", using the
exact definitional alignments pinned in ``tests/test_networkx_reference.py``:

* closeness on the **reversed** digraph, ``wf_improved=False`` → Ursa's out-edge form;
* betweenness ``normalized=False, endpoints=False`` → Ursa's raw form;
* triangles on the **undirected** projection.

Node labels are the original integer ids, so results need no remapping.
"""

from __future__ import annotations

from pathlib import Path

from ..algorithms import Algorithm
from .base import Adapter, load_edges


class NetworkxAdapter(Adapter):
    NAME = "networkx"

    def version(self) -> str:
        import networkx as nx

        return nx.__version__

    def supports(self, algo: str) -> bool:
        return algo in {
            "pagerank",
            "betweenness",
            "closeness",
            "triangle_count",
            "connected_components",
            "degree",
        }

    def ingest(self, dataset_path: str | Path, algo: Algorithm):
        import networkx as nx

        src, dst = load_edges(dataset_path)
        # Build the view the algorithm reads: directed for centralities, the
        # undirected projection for triangles / weak components.
        graph = nx.DiGraph() if algo.view == "directed" else nx.Graph()
        graph.add_edges_from(zip(src, dst, strict=True))
        return graph

    def run(self, handle, algo: Algorithm) -> dict[int, float]:
        import networkx as nx

        g = handle
        name = algo.name
        if name == "pagerank":
            p = algo.params
            return dict(
                nx.pagerank(
                    g,
                    alpha=p.get("damping", 0.85),
                    max_iter=max(p.get("max_iter", 100), 100),
                    tol=p.get("tol", 1e-6),
                )
            )
        if name == "betweenness":
            return dict(nx.betweenness_centrality(g, normalized=False, endpoints=False))
        if name == "closeness":
            # Ursa measures outgoing distance; NetworkX measures incoming, so
            # reverse. wf_improved=False drops the (n-1) scaling.
            return dict(nx.closeness_centrality(g.reverse(), wf_improved=False))
        if name == "triangle_count":
            return {n: int(t) for n, t in nx.triangles(g).items()}
        if name == "connected_components":
            labels: dict[int, int] = {}
            for label, comp in enumerate(nx.connected_components(g)):
                for node in comp:
                    labels[node] = label
            return labels
        if name == "degree":
            return {n: int(d) for n, d in g.degree()}
        raise NotImplementedError(name)  # pragma: no cover - guarded by supports()
