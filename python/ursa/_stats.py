"""Graph-level descriptive statistics.

``describe`` returns a lazy one-row summary frame that flows anywhere a frame
flows. A small, documented set of scalar metrics (``density``, ``avg_path_length``,
``diameter``) are the *one deliberate exception to laziness*: they are eager and
return plain Python numbers, chosen for ergonomics.

Every stat takes ``direction=`` (``"out"`` by default, as in ``ur.degree``).
``"both"`` reads the graph as undirected: one edge per distinct unordered pair of
nodes joined by a row in either direction, self-loops dropped — the figures
NetworkX reports for ``G.to_undirected()``. ``"in"`` follows in-edges; over the
whole graph it gives the same value as ``"out"``.
"""

from __future__ import annotations

from ._frames import EdgeFrame, NodeFrame, _PlanStep

_DIRECTIONS = ("out", "in", "both")


def _check_direction(fn: str, direction: str) -> str:
    if direction not in _DIRECTIONS:
        raise ValueError(
            f"{fn}(direction={direction!r}) is not supported; use 'out', 'in', or 'both'."
        )
    return direction


def describe(edges: EdgeFrame, full: bool = False, direction: str = "out") -> NodeFrame:
    """A lazy one-row summary frame with columns ``n_nodes``, ``n_edges``,
    ``density``, ``avg_degree`` (and ``n_components`` when ``full=True``, which is
    the expensive member — gated behind the flag by default).

    ``direction="out"`` (default) describes the rows as stored: ``n_edges`` is the
    row count, ``density`` the directed density and ``avg_degree`` the mean
    out-degree. ``direction="both"`` reports the undirected figures: ``n_edges``
    counts distinct unordered pairs (self-loops dropped), ``density`` is the
    undirected density and ``avg_degree`` the mean undirected degree
    (``2 * n_edges / n_nodes``). ``n_components`` is always the weak count.

    This is a whole-graph summary, not a node-keyed frame: it has no ``id`` column,
    so the carried ``id_col`` is an unused placeholder (you collect the summary
    rather than derive node-keyed frames from it).
    """
    _check_direction("describe", direction)
    return NodeFrame._construct(
        id_col="__summary__",
        plan=(_PlanStep("describe", {"full": full, "edges": edges, "direction": direction}),),
    )


def density(edges: EdgeFrame, direction: str = "out") -> float:
    """Edge density (eager; plain float).

    ``direction="out"`` (default; ``"in"`` is identical) is the directed density
    ``m / (n * (n - 1))`` — rows present over ordered pairs possible (self-loops
    excluded from the possible count). Multiplicity counts as given; call
    ``.distinct()`` first for the simple-graph value.

    ``direction="both"`` is the undirected density ``m_u / (n * (n - 1) / 2)``,
    where ``m_u`` counts the distinct unordered pairs joined by a row in either
    direction (reciprocal and parallel rows collapse, self-loops are dropped):
    ``nx.density(G.to_undirected())`` for a graph without self-loops.
    """
    from ._execute import _graph_index, _native

    _check_direction("density", direction)
    return float(_native().graph_density(_graph_index(edges), direction))


def avg_path_length(edges: EdgeFrame, sample: float | None = None, direction: str = "out") -> float:
    """Average shortest-path length over reachable pairs (eager). ``direction``
    (default ``"out"``) is the way edges are followed; ``"both"`` ignores direction,
    giving ``nx.average_shortest_path_length(G.to_undirected())`` on a connected
    graph. ``sample`` (a fraction in (0, 1]) estimates from a subset of sources;
    omit for the exact mean."""
    from ._execute import _graph_index, _native

    _check_direction("avg_path_length", direction)
    return float(_native().graph_avg_path_length(_graph_index(edges), sample, direction))


def diameter(edges: EdgeFrame, approximate: bool = True, direction: str = "out") -> int:
    """Graph diameter — the longest shortest path over reachable pairs (eager).
    ``direction`` (default ``"out"``) is the way edges are followed; ``"both"``
    ignores direction, giving ``nx.diameter(G.to_undirected())`` on a connected
    graph. ``approximate`` (default) is a lower-bound estimate; pass
    ``approximate=False`` for the exact value."""
    from ._execute import _graph_index, _native

    _check_direction("diameter", direction)
    return int(_native().graph_diameter(_graph_index(edges), approximate, direction))
