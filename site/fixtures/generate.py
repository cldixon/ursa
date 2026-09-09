#!/usr/bin/env python
"""Compute the documentation site's figures with Ursa.

The site used to compute its own graph statistics in TypeScript — a second
PageRank, living in the documentation of a library whose whole job is PageRank.
Every number the site showed was therefore *plausible* rather than true, and
nothing tied it to the engine it was advertising.

This closes that. Ursa runs here, at documentation build time, and writes flat
JSON fixtures the site loads. Nothing computes graph statistics in the browser or
in the Astro build any more.

Run it with the native extension available:

    uv run python site/fixtures/generate.py

CI runs the same command and fails if the committed output changes, so a fixture
cannot silently drift away from the engine that produced it.

**Determinism is the contract.** Same input, same bytes out, or the staleness
check becomes noise that everyone learns to ignore. Everything below is seeded,
and floats are rounded on the way out — full f64 text would make the diff churn
on the last bit for no visible difference.

**One thing here is not Ursa yet: the layout.** Node positions come from
NetworkX, because Ursa has no layout kernel — that is `layout_fa2`, blocked on
the multi-output half of #115. It is deliberately the only such call in the file,
so replacing it later is a single edit. Everything else — degree, PageRank,
communities — is Ursa.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import ursa as ur

OUT = Path(__file__).resolve().parents[1] / "src" / "fixtures"

# Seeds, fixed forever. Changing one re-rolls a figure, which is a visible change
# to the site and should be a deliberate commit rather than a side effect.
LOUVAIN_SEED = 20260908
LAYOUT_SEED = 20260908


def layout(
    edge_rows: list[tuple[str, str, float]], n_iter: int = 400
) -> dict[str, tuple[float, float]]:
    """Force-directed positions.

    The one computation on this page Ursa cannot do yet. `layout_fa2` is a
    Barnes-Hut fixpoint over the CSR — the same computational shape as PageRank —
    and lands once a kernel can emit two columns (x and y) from one invocation.
    Until then NetworkX draws the picture and Ursa supplies everything in it.

    Seeded, and `spring_layout` is deterministic under a fixed seed, so the
    fixture is stable across runs.
    """
    import networkx as nx

    g = nx.Graph()
    for src, dst, weight in edge_rows:
        g.add_edge(src, dst, weight=weight)
    pos = nx.spring_layout(g, seed=LAYOUT_SEED, iterations=n_iter, weight="weight")
    return {str(k): (float(v[0]), float(v[1])) for k, v in pos.items()}


def lesmis() -> dict[str, Any]:
    """Les Misérables co-occurrence: 77 characters, weighted by shared scenes.

    Chosen over a synthetic graph because it has what a synthetic one cannot:
    real community structure, real names to put in a tooltip, and a weight that
    means something. It is also small enough to read.
    """
    edges = ur.datasets.load_lesmis()
    weight = ur.col("weight")

    # One query, three kernels. Ursa shares the work across the columns of a
    # single `with_columns` (#115), so this is one pass over the topology.
    frame = (
        edges.nodes()
        .with_columns(
            degree=ur.degree(edges, direction="both"),
            pagerank=ur.pagerank(edges, weight=weight),
            community=ur.louvain(edges, weight=weight, seed=LOUVAIN_SEED),
        )
        .sort("id")
        .collect()
    )
    table = frame.to_arrow()
    ids: list[str] = table.column("id").to_pylist()
    degree: list[int] = table.column("degree").to_pylist()
    pagerank: list[float] = table.column("pagerank").to_pylist()
    community: list[int] = table.column("community").to_pylist()

    # Dense render indices: the renderer addresses nodes by position, and so will
    # the wire protocol when the explorer lands. Resolving user ids to indices
    # here keeps that translation out of the browser.
    index = {name: i for i, name in enumerate(ids)}

    edge_rows = [
        (str(r["src"]), str(r["dst"]), float(r["weight"])) for r in edges.collect().to_dicts()
    ]
    pos = layout(edge_rows)

    return {
        "name": "lesmis",
        "title": "Les Misérables",
        "description": "Character co-occurrence, weighted by shared scenes.",
        "ids": ids,
        # Positions are centred on the origin and scaled to a comfortable extent;
        # the renderer frames whatever it is given, so the absolute scale only
        # decides how the initial `fit` looks.
        "x": [round(pos[name][0] * 400, 2) for name in ids],
        "y": [round(pos[name][1] * 400, 2) for name in ids],
        "degree": degree,
        # Six significant figures: enough that the tooltip reads exactly, few
        # enough that the file does not churn on the last bit of an f64.
        "pagerank": [float(f"{v:.6g}") for v in pagerank],
        "community": community,
        "edges": [v for src, dst, _ in edge_rows for v in (index[src], index[dst])],
        "weights": [round(w, 3) for _, _, w in edge_rows],
    }


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    data = lesmis()
    path = OUT / f"{data['name']}.json"
    # Trailing newline and sorted keys so the file is diff-stable and a text
    # editor does not "fix" it into a spurious change.
    path.write_text(json.dumps(data, indent=1, sort_keys=True) + "\n")
    n_comm = len(set(data["community"]))
    print(
        f"{path.relative_to(Path.cwd()) if path.is_relative_to(Path.cwd()) else path}: "
        f"{len(data['ids'])} nodes, {len(data['edges']) // 2} edges, {n_comm} communities"
    )


if __name__ == "__main__":
    main()
