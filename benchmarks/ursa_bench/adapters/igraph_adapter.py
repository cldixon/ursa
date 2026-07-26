"""igraph adapter — a mature C graph core (often the perf king on centralities).

igraph assigns contiguous integer vertex ids 0..n-1 as the graph is built, so the
adapter maps the original ids to indices (via `ids[i]`) and back on the way out,
exactly like the rustworkx adapter.

Convention notes (each verified empirically against the NetworkX oracle — a
mismatch shows up as an `incorrect` row, never a silent pass):
* **closeness** — igraph `mode="out", normalized=True` gives (reachable-1)/Σdist,
  which matches Ursa's out-edge form (`nx.closeness_centrality(reverse,
  wf_improved=False)`).
* **betweenness** — igraph is raw (unnormalised) by default, matching Ursa.
* **triangle_count** — igraph has no per-vertex triangle count (only local
  transitivity), so it is reported unsupported.
"""

from __future__ import annotations

from pathlib import Path

from ..algorithms import Algorithm
from .base import Adapter, load_edges, load_weighted_edges


class IgraphAdapter(Adapter):
    NAME = "igraph"

    def version(self) -> str:
        import igraph

        return igraph.__version__

    def supports(self, algo: str) -> bool:
        # No per-vertex triangle count in igraph (only global/local transitivity).
        return algo in {
            "pagerank",
            "betweenness",
            "closeness",
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
        import igraph

        if algo.weighted:
            src, dst, weights = load_weighted_edges(dataset_path)
        else:
            src, dst = load_edges(dataset_path)
            weights = None
        node_ids = sorted(set(src) | set(dst))
        id_to_idx = {nid: i for i, nid in enumerate(node_ids)}
        edges = [(id_to_idx[s], id_to_idx[d]) for s, d in zip(src, dst, strict=True)]
        graph = igraph.Graph(n=len(node_ids), edges=edges, directed=(algo.view == "directed"))
        if weights is not None:
            graph.es["weight"] = weights
        return {"graph": graph, "ids": node_ids}

    def run(self, handle, algo: Algorithm) -> dict[int, float]:
        graph = handle["graph"]
        ids = handle["ids"]
        name = algo.name

        def remap(values) -> dict[int, float]:
            return {ids[i]: v for i, v in enumerate(values)}

        if name == "pipeline_influencers":
            p = algo.params
            pr = graph.pagerank(damping=p.get("damping", 0.85))
            indeg = graph.degree(mode="in")
            min_in = p.get("min_in_degree", 3)
            eligible = [i for i in range(len(pr)) if indeg[i] >= min_in]
            eligible.sort(key=lambda i: pr[i], reverse=True)
            top = eligible[: p.get("top_k", 20)]
            return {ids[i]: rank for rank, i in enumerate(top)}

        if name == "pagerank":
            p = algo.params
            return remap(graph.pagerank(damping=p.get("damping", 0.85)))
        if name == "pagerank_weighted":
            p = algo.params
            return remap(graph.pagerank(damping=p.get("damping", 0.85), weights="weight"))
        if name == "betweenness":
            return remap(graph.betweenness(directed=True))
        if name == "betweenness_weighted":
            return remap(graph.betweenness(directed=True, weights="weight"))
        if name == "closeness":
            return remap(graph.closeness(mode="out", normalized=True))
        if name == "closeness_weighted":
            return remap(graph.closeness(mode="out", normalized=True, weights="weight"))
        if name == "degree":
            return remap(graph.degree(mode="all"))
        if name == "degree_in":
            return remap(graph.degree(mode="in"))
        if name == "degree_out":
            return remap(graph.degree(mode="out"))
        if name == "clustering_coefficient":
            return remap(graph.transitivity_local_undirected(mode="zero"))
        if name == "connected_components":
            membership = graph.connected_components(mode="weak").membership
            return remap(membership)
        if name == "louvain":
            return remap(graph.community_multilevel().membership)
        if name == "label_propagation":
            return remap(graph.community_label_propagation().membership)
        raise NotImplementedError(name)  # pragma: no cover - guarded by supports()
