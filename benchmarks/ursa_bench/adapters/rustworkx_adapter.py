"""rustworkx adapter — the honest "real bar" (a mature Rust/PyO3 graph core).

rustworkx assigns its own dense integer node indices as nodes are added, so the
adapter keeps the original ids as node payloads and maps results back through the
``ids`` list (index ``i`` == ``ids[i]``, since nodes are added in one shot).

Convention notes (surfaced by the correctness check if wrong, never silently):
* **closeness** — rustworkx measures distance *along* edge direction (outgoing),
  which already matches Ursa's out-edge form, so no reversal is applied (unlike
  the NetworkX reference, which measures incoming and must be reversed).
* **triangle_count** — rustworkx has only global transitivity, not per-node
  triangles, so this adapter reports it unsupported (a real rustworkx gap).
"""

from __future__ import annotations

from pathlib import Path

from ..algorithms import Algorithm
from .base import Adapter, load_edges


class RustworkxAdapter(Adapter):
    NAME = "rustworkx"

    def version(self) -> str:
        import rustworkx as rx

        return rx.__version__

    def supports(self, algo: str) -> bool:
        # Per-node triangle_count has no rustworkx equivalent (only transitivity).
        return algo in {
            "pagerank",
            "betweenness",
            "closeness",
            "connected_components",
            "degree",
        }

    def ingest(self, dataset_path: str | Path, algo: Algorithm):
        import rustworkx as rx

        src, dst = load_edges(dataset_path)
        node_ids = sorted(set(src) | set(dst))
        id_to_idx = {nid: i for i, nid in enumerate(node_ids)}

        graph = rx.PyDiGraph() if algo.view == "directed" else rx.PyGraph()
        # Payload == original id; indices come back 0..n-1 in this order.
        graph.add_nodes_from(node_ids)
        edges = [(id_to_idx[s], id_to_idx[d]) for s, d in zip(src, dst, strict=True)]
        # rustworkx closeness measures *incoming* distance (like un-reversed
        # NetworkX), so to match Ursa's out-edge form we feed it the reversed
        # graph — the same reversal the NetworkX oracle applies.
        if algo.name == "closeness":
            edges = [(d, s) for s, d in edges]
        graph.add_edges_from_no_data(edges)
        return {"graph": graph, "ids": node_ids}

    def run(self, handle, algo: Algorithm) -> dict[int, float]:
        import rustworkx as rx

        graph = handle["graph"]
        ids = handle["ids"]
        name = algo.name

        def remap(mapping) -> dict[int, float]:
            return {ids[idx]: val for idx, val in dict(mapping).items()}

        if name == "pagerank":
            p = algo.params
            m = rx.pagerank(
                graph,
                alpha=p.get("damping", 0.85),
                tol=p.get("tol", 1e-6),
                max_iter=max(p.get("max_iter", 100), 100),
            )
            return remap(m)
        if name == "betweenness":
            return remap(rx.betweenness_centrality(graph, normalized=False, endpoints=False))
        if name == "closeness":
            return remap(rx.closeness_centrality(graph, wf_improved=False))
        if name == "connected_components":
            # `weakly_connected_components` is digraph-only; the undirected view
            # (PyGraph) uses `connected_components`.
            components = (
                rx.weakly_connected_components(graph)
                if isinstance(graph, rx.PyDiGraph)
                else rx.connected_components(graph)
            )
            labels: dict[int, float] = {}
            for label, comp in enumerate(components):
                for idx in comp:
                    labels[ids[idx]] = label
            return labels
        if name == "degree":
            # Total degree on a digraph is in + out; PyGraph.degree already totals.
            if isinstance(graph, rx.PyDiGraph):
                return {
                    ids[i]: graph.in_degree(i) + graph.out_degree(i) for i in graph.node_indices()
                }
            return {ids[i]: graph.degree(i) for i in graph.node_indices()}
        raise NotImplementedError(name)  # pragma: no cover - guarded by supports()
