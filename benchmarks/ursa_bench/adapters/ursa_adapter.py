"""Ursa adapter — the library under test.

Ingest is deliberately the in-memory Arrow path (``pq.read_table`` →
``ur.from_arrow``): "the data is already in Arrow" is exactly Ursa's design
premise, and an in-memory ``EdgeFrame`` memoises its topology index across ops,
so the two-clock split lands correctly:

* **ingest** — read Parquet + wrap in an EdgeFrame (cheap; no CSR yet);
* **cold compute** — first algorithm call, which builds *and* caches the CSR;
* **warm compute** — later calls reuse the cached topology (kernel-only).

Every verb here is a standalone node-valued algorithm, so ``collect()`` yields a
frame with an ``id`` column and a column named after the algorithm.
"""

from __future__ import annotations

from pathlib import Path

import pyarrow.parquet as pq

from ..algorithms import Algorithm
from .base import Adapter


class UrsaAdapter(Adapter):
    NAME = "ursa"

    def version(self) -> str:
        import ursa as ur

        return getattr(ur, "__version__", "unknown")

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
        }

    def ingest(self, dataset_path: str | Path, algo: Algorithm):
        import ursa as ur

        table = pq.read_table(dataset_path, columns=["src", "dst"])
        return ur.from_arrow(table, src="src", dst="dst")

    def run(self, handle, algo: Algorithm) -> dict[int, float]:
        import ursa as ur

        edges = handle
        name = algo.name

        if name == "pipeline_influencers":
            # The whole point: one composed query, executed as a single plan.
            p = algo.params
            frame = (
                edges.nodes()
                .with_columns(
                    pr=ur.pagerank(
                        edges,
                        damping=p.get("damping", 0.85),
                        max_iter=p.get("max_iter", 100),
                        tol=p.get("tol", 1e-6),
                    ),
                    indeg=ur.degree(edges, direction="in"),
                )
                .filter(ur.col("indeg") >= p.get("min_in_degree", 3))
                .sort("pr", descending=True)
                .head(p.get("top_k", 20))
                .collect()
            )
            return {r["id"]: rank for rank, r in enumerate(frame.to_dicts())}

        if name == "pagerank":
            p = algo.params
            expr = ur.pagerank(
                edges,
                damping=p.get("damping", 0.85),
                max_iter=p.get("max_iter", 100),
                tol=p.get("tol", 1e-6),
            )
        elif name == "betweenness":
            expr = ur.betweenness(edges)
        elif name == "closeness":
            expr = ur.closeness(edges)
        elif name == "triangle_count":
            expr = ur.triangle_count(edges)
        elif name == "connected_components":
            expr = ur.connected_components(edges, mode="weak")
        elif name == "degree":
            expr = ur.degree(edges, direction=algo.params.get("direction", "both"))
        elif name == "clustering_coefficient":
            expr = ur.clustering_coefficient(edges)
        elif name == "louvain":
            p = algo.params
            expr = ur.louvain(edges, resolution=p.get("resolution", 1.0), seed=p.get("seed"))
        elif name == "label_propagation":
            expr = ur.label_propagation(edges, seed=algo.params.get("seed"))
        else:  # pragma: no cover - guarded by supports()
            raise NotImplementedError(name)

        rows = expr.collect().to_dicts()
        return {r["id"]: r[name] for r in rows}
