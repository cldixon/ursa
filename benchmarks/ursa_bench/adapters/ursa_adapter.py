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
            "degree_in",
            "degree_out",
            "pagerank_weighted",
            "closeness_weighted",
            "betweenness_weighted",
        }

    def ingest(self, dataset_path: str | Path, algo: Algorithm):
        import ursa as ur

        # Read the full table (not just src/dst) so a `weight` column is available
        # to `ur.col("weight")` for weighted algorithms — Ursa keeps edge columns.
        columns = None if algo.weighted else ["src", "dst"]
        table = pq.read_table(dataset_path, columns=columns)
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

        # weight expression for the weighted variants; None otherwise
        w = ur.col("weight") if algo.weighted else None
        p = algo.params
        # `col` is the output column of the verb, which differs from the algo name
        # for the weighted / directed-degree variants.
        col = name
        if name in ("pagerank", "pagerank_weighted"):
            expr = ur.pagerank(
                edges,
                damping=p.get("damping", 0.85),
                max_iter=p.get("max_iter", 100),
                tol=p.get("tol", 1e-6),
                weight=w,
            )
            col = "pagerank"
        elif name in ("betweenness", "betweenness_weighted"):
            expr = ur.betweenness(edges, weight=w)
            col = "betweenness"
        elif name in ("closeness", "closeness_weighted"):
            expr = ur.closeness(edges, weight=w)
            col = "closeness"
        elif name == "triangle_count":
            expr = ur.triangle_count(edges)
        elif name == "connected_components":
            expr = ur.connected_components(edges, mode="weak")
        elif name in ("degree", "degree_in", "degree_out"):
            expr = ur.degree(edges, direction=p.get("direction", "both"))
            col = "degree"
        elif name == "clustering_coefficient":
            expr = ur.clustering_coefficient(edges)
        elif name == "louvain":
            expr = ur.louvain(edges, resolution=p.get("resolution", 1.0), seed=p.get("seed"))
        elif name == "label_propagation":
            expr = ur.label_propagation(edges, seed=p.get("seed"))
        else:  # pragma: no cover - guarded by supports()
            raise NotImplementedError(name)

        rows = expr.collect().to_dicts()
        return {r["id"]: r[col] for r in rows}
