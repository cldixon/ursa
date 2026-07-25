"""The canonical algorithm registry.

Each entry fixes *one* definition of an algorithm — the same one every adapter
must produce — so a cross-library disagreement is a real bug, never a config
mismatch. The definitions are the ones already pinned against NetworkX in
``tests/test_networkx_reference.py``:

* **closeness** — out-edge ``reachable / Σ dist``, no Wasserman–Faust scaling
  (``nx.closeness_centrality(G.reverse(), wf_improved=False)``);
* **betweenness** — raw, unnormalised, no endpoints (``normalized=False``);
* **triangle_count / clustering** — the *undirected* view of the edge list;
* **pagerank** — damping 0.85, summing to 1;
* **connected_components** — weak components (labels arbitrary; compared as a
  partition, not by value).

``compare`` names the correctness check in :mod:`ursa_bench.correctness`. ``view``
tells reference builders whether the algorithm reads the graph as directed or as
its undirected projection. ``params`` are the canonical parameters every adapter
passes through (translated to each library's spelling inside its adapter).
"""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass(frozen=True)
class Algorithm:
    name: str
    description: str
    view: str  # "directed" | "undirected" — how the reference reads the edge list
    compare: str  # correctness check tag: "value_map" | "int_map" | "partition" | "modularity"
    params: dict = field(default_factory=dict)
    tol: float = 1e-6  # absolute tolerance for value_map comparisons


REGISTRY: dict[str, Algorithm] = {
    "pagerank": Algorithm(
        name="pagerank",
        description="PageRank (damping 0.85), stationary distribution summing to 1.",
        view="directed",
        compare="value_map",
        params={"damping": 0.85, "max_iter": 100, "tol": 1e-6},
        # Timing uses a realistic fixed iteration budget (max_iter=100), so three
        # implementations converge to *slightly* different points — cross-library
        # drift is ~5e-5 here. 1e-4 accommodates that while still catching a real
        # bug (wrong damping/normalisation shifts values by 1e-3 or more).
        tol=1e-4,
    ),
    "betweenness": Algorithm(
        name="betweenness",
        description="Raw (unnormalised) shortest-path betweenness centrality, no endpoints.",
        view="directed",
        compare="value_map",
        params={},
        tol=1e-6,
    ),
    "closeness": Algorithm(
        name="closeness",
        description="Out-edge closeness, reachable/Σdist, no Wasserman-Faust scaling.",
        view="directed",
        compare="value_map",
        params={},
        tol=1e-9,
    ),
    "triangle_count": Algorithm(
        name="triangle_count",
        description="Per-node triangle count on the undirected view.",
        view="undirected",
        compare="int_map",
        params={},
    ),
    "connected_components": Algorithm(
        name="connected_components",
        description="Weak connected components (component id per node; labels arbitrary).",
        view="undirected",
        compare="partition",
        params={},
    ),
    "degree": Algorithm(
        name="degree",
        description="Total degree per node.",
        view="directed",
        compare="int_map",
        params={"direction": "both"},
    ),
}


def get(name: str) -> Algorithm:
    try:
        return REGISTRY[name]
    except KeyError:
        raise KeyError(
            f"unknown algorithm {name!r}; known: {', '.join(sorted(REGISTRY))}"
        ) from None


def names() -> list[str]:
    return list(REGISTRY)
