"""Graph-level descriptive statistics.

``describe`` returns a lazy one-row summary frame that flows anywhere a frame
flows. A small, documented set of scalar metrics (``density``, ``avg_path_length``,
``diameter``) are the *one deliberate exception to laziness*: they are eager and
return plain Python numbers, chosen for ergonomics.
"""

from __future__ import annotations

from ._frames import EdgeFrame, NodeFrame, _PlanStep


def describe(edges: EdgeFrame, full: bool = False) -> NodeFrame:
    """A lazy one-row summary frame (n_nodes, n_edges, density, avg_degree, ...).

    ``full=True`` computes the expensive members (e.g. ``n_components``); whether
    those are default or opt-in is an open question (spec §Open questions #4).
    """
    return NodeFrame(id_col="stat", plan=(_PlanStep("describe", {"full": full, "edges": edges}),))


def density(edges: EdgeFrame) -> float:
    """Directed edge density (eager; plain float).

    ``m / (n * (n - 1))`` — edges present over edges possible (self-loops
    excluded). Multiplicity counts as given; call ``.distinct()`` first for the
    simple-graph value.
    """
    from ._execute import _native, _require_index

    return float(_native().graph_density(_require_index(edges)))


def avg_path_length(edges: EdgeFrame, sample: float | None = None) -> float:
    """Average shortest-path length over reachable ordered pairs (eager; directed,
    following out-edges). ``sample`` (a fraction in (0, 1]) estimates from a subset
    of sources; omit for the exact mean."""
    from ._execute import _native, _require_index

    return float(_native().graph_avg_path_length(_require_index(edges), sample))


def diameter(edges: EdgeFrame, approximate: bool = True) -> int:
    """Graph diameter — the longest shortest path over reachable pairs (eager;
    directed). ``approximate`` (default) is a lower-bound estimate; pass
    ``approximate=False`` for the exact value."""
    from ._execute import _native, _require_index

    return int(_native().graph_diameter(_require_index(edges), approximate))
