"""Graph-level descriptive statistics.

``describe`` returns a lazy one-row summary frame that flows anywhere a frame
flows. A small, documented set of scalar metrics (``density``, ``avg_path_length``,
``diameter``) are the *one deliberate exception to laziness*: they are eager and
return plain Python numbers, chosen for ergonomics.
"""

from __future__ import annotations

from ._frames import EdgeFrame, NodeFrame, _PlanStep

_ENGINE_TODO = "requires the DataFusion execution engine (ursa-plan), not yet wired."


def describe(edges: EdgeFrame, full: bool = False) -> NodeFrame:
    """A lazy one-row summary frame (n_nodes, n_edges, density, avg_degree, ...).

    ``full=True`` computes the expensive members (e.g. ``n_components``); whether
    those are default or opt-in is an open question (spec §Open questions #4).
    """
    return NodeFrame(id_col="stat", plan=(_PlanStep("describe", {"full": full}),))


def density(edges: EdgeFrame) -> float:
    """Directed edge density (eager; plain float).

    ``m / (n * (n - 1))`` — edges present over edges possible (self-loops
    excluded). Multiplicity counts as given; call ``.distinct()`` first for the
    simple-graph value.
    """
    from ._execute import _native, _require_edges

    src, dst = _require_edges(edges)
    return float(_native().graph_density(src, dst))


def avg_path_length(edges: EdgeFrame, sample: float | None = None) -> float:
    """Average shortest-path length (eager; sampled estimator by default)."""
    raise NotImplementedError(f"avg_path_length() {_ENGINE_TODO}")


def diameter(edges: EdgeFrame, approximate: bool = True) -> int:
    """Graph diameter (eager; approximate by default, exact only on request)."""
    raise NotImplementedError(f"diameter() {_ENGINE_TODO}")
