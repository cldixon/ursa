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
from .base import Adapter, load_edges, load_weighted_edges


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
            "clustering_coefficient",
            "louvain",
            "label_propagation",
            "pipeline_influencers",
            "degree_in",
            "degree_out",
            "pagerank_weighted",
            "closeness_weighted",
            "betweenness_weighted",
        }

    def ingest(self, dataset_path: str | Path, algo: Algorithm):
        import networkx as nx

        # Build the view the algorithm reads: directed for centralities, the
        # undirected projection for triangles / weak components.
        graph = nx.DiGraph() if algo.view == "directed" else nx.Graph()
        if algo.weighted:
            src, dst, w = load_weighted_edges(dataset_path)
            graph.add_weighted_edges_from(zip(src, dst, w, strict=True))
        else:
            src, dst = load_edges(dataset_path)
            graph.add_edges_from(zip(src, dst, strict=True))
        return graph

    def run(self, handle, algo: Algorithm) -> dict[int, float]:
        import networkx as nx

        g = handle
        name = algo.name

        if name == "pipeline_influencers":
            # The idiomatic hand-written equivalent: compute, then filter/sort/top
            # in Python — the tax Ursa avoids by fusing it into one plan.
            p = algo.params
            pr = nx.pagerank(
                g,
                alpha=p.get("damping", 0.85),
                max_iter=max(p.get("max_iter", 100), 100),
                tol=p.get("tol", 1e-6),
            )
            indeg = dict(g.in_degree())
            min_in = p.get("min_in_degree", 3)
            eligible = [n for n in pr if indeg.get(n, 0) >= min_in]
            top = sorted(eligible, key=lambda n: pr[n], reverse=True)[: p.get("top_k", 20)]
            return {n: rank for rank, n in enumerate(top)}

        if name == "degree_in":
            return {n: int(d) for n, d in g.in_degree()}
        if name == "degree_out":
            return {n: int(d) for n, d in g.out_degree()}
        if name == "pagerank_weighted":
            p = algo.params
            return dict(
                nx.pagerank(
                    g,
                    alpha=p.get("damping", 0.85),
                    max_iter=max(p.get("max_iter", 100), 100),
                    tol=p.get("tol", 1e-6),
                    weight="weight",
                )
            )
        if name == "closeness_weighted":
            # weight is edge distance; reverse for the out-edge form (as unweighted).
            return dict(nx.closeness_centrality(g.reverse(), distance="weight", wf_improved=False))
        if name == "betweenness_weighted":
            return dict(
                nx.betweenness_centrality(g, weight="weight", normalized=False, endpoints=False)
            )

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
        if name == "clustering_coefficient":
            return dict(nx.clustering(g))
        if name == "louvain":
            p = algo.params
            comms = nx.community.louvain_communities(
                g, resolution=p.get("resolution", 1.0), seed=p.get("seed", 1)
            )
            return _labels_from_communities(comms)
        if name == "label_propagation":
            return _labels_from_communities(nx.community.label_propagation_communities(g))
        raise NotImplementedError(name)  # pragma: no cover - guarded by supports()


def _labels_from_communities(communities) -> dict[int, float]:
    """Turn an iterable of node-sets into a {node: community_label} mapping."""
    labels: dict[int, float] = {}
    for label, comm in enumerate(communities):
        for node in comm:
            labels[node] = label
    return labels
