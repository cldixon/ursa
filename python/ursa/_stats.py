"""Graph-level descriptive statistics.

``describe`` returns a lazy one-row summary frame that flows anywhere a frame
flows. ``density``, ``avg_path_length`` and ``diameter`` are lazy one-row frames
too: called with ``lazy=True`` they return the frame, and called as before they
collect it and return the plain Python number, for ergonomics.

Every stat reads the subgraph view ``degree`` reads: over a filtered edge frame
or a ``hop`` / ``shortest_path`` result, the parent graph's nodes and only the
kept edges (#207, #210).

Every stat takes ``direction=`` (``"out"`` by default, as in ``ur.degree``).
``"both"`` reads the graph as undirected: one edge per distinct unordered pair of
nodes joined by a row in either direction, self-loops dropped — the figures
NetworkX reports for ``G.to_undirected()``. ``"in"`` follows in-edges; over the
whole graph it gives the same value as ``"out"``.
"""

from __future__ import annotations

from typing import Any, Literal, overload

from . import _ir
from ._frames import EdgeFrame, NodeFrame, _native


def _check_direction(fn: str, direction: str) -> str:
    _native().check_direction(fn, direction)
    return direction


def _value(frame: NodeFrame) -> Any:
    """The one value of a stat's one-row frame."""
    return frame.to_arrow().column(0)[0].as_py()


def describe(
    edges: EdgeFrame, full: bool = False, direction: str = "out", nodes: str = "all"
) -> NodeFrame:
    """A lazy one-row summary frame with columns ``n_nodes``, ``n_edges``,
    ``density``, ``avg_degree`` (and ``n_components`` when ``full=True``, which is
    the expensive member — gated behind the flag by default).

    ``direction="out"`` (default) describes the rows as stored: ``n_edges`` is the
    row count, ``density`` the directed density and ``avg_degree`` the mean
    out-degree. ``direction="both"`` reports the undirected figures: ``n_edges``
    counts distinct unordered pairs (self-loops dropped), ``density`` is the
    undirected density and ``avg_degree`` the mean undirected degree
    (``2 * n_edges / n_nodes``). ``n_components`` is always the weak count.

    Over a filtered edge frame, or a ``hop`` / ``shortest_path`` result, this
    is the subgraph view ``degree`` reads: the parent graph's nodes, and only
    the kept edges. ``n_nodes`` stays the parent's count, a node left without
    an edge is its own component, and ``density`` divides by the parent's
    ``n``, so a small neighbourhood of a large graph has a density near 0.

    ``nodes="touched"`` describes only the nodes the view touches — those at an
    end of a kept edge: ``n_nodes`` counts them, ``density`` and ``avg_degree``
    divide by that count, and ``n_components`` leaves the other nodes out. Over a
    whole graph every node is at an end of an edge, so it changes nothing there.
    ``nodes="all"`` (default) keeps the parent's node set.

    This is a whole-graph summary, not a node-keyed frame: it has no ``id`` column,
    so the carried ``id_col`` is an unused placeholder (you collect the summary
    rather than derive node-keyed frames from it).
    """
    edges_node = _ir.edges_node(edges, "describe")
    _check_direction("describe", direction)
    return NodeFrame._wrap(_native().PlanNode.describe(edges_node, bool(full), direction, nodes))


@overload
def density(edges: EdgeFrame, direction: str = ..., *, lazy: Literal[False] = ...) -> float: ...
@overload
def density(edges: EdgeFrame, direction: str = ..., *, lazy: Literal[True]) -> NodeFrame: ...
def density(edges: EdgeFrame, direction: str = "out", *, lazy: bool = False) -> float | NodeFrame:
    """Edge density: a float, or with ``lazy=True`` a lazy one-row frame with
    one ``density`` column (``Float64``).

    ``direction="out"`` (default; ``"in"`` is identical) is the directed density
    ``m / (n * (n - 1))`` — rows present over ordered pairs possible (self-loops
    excluded from the possible count). Multiplicity counts as given; call
    ``.distinct()`` first for the simple-graph value.

    ``direction="both"`` is the undirected density ``m_u / (n * (n - 1) / 2)``,
    where ``m_u`` counts the distinct unordered pairs joined by a row in either
    direction (reciprocal and parallel rows collapse, self-loops are dropped):
    ``nx.density(G.to_undirected())`` for a graph without self-loops.

    Over a filtered edge frame or a traversal result it is the subgraph view's:
    the kept edges over the parent's ``n``, the ``density`` ``describe`` reports.
    """
    edges_node = _ir.edges_node(edges, "density")
    _check_direction("density", direction)
    frame = NodeFrame._wrap(_native().PlanNode.density(edges_node, direction))
    return frame if lazy else float(_value(frame))


@overload
def avg_path_length(
    edges: EdgeFrame,
    sample: float | None = ...,
    direction: str = ...,
    *,
    lazy: Literal[False] = ...,
) -> float: ...
@overload
def avg_path_length(
    edges: EdgeFrame, sample: float | None = ..., direction: str = ..., *, lazy: Literal[True]
) -> NodeFrame: ...
def avg_path_length(
    edges: EdgeFrame, sample: float | None = None, direction: str = "out", *, lazy: bool = False
) -> float | NodeFrame:
    """Average shortest-path length over reachable pairs: a float, or with
    ``lazy=True`` a lazy one-row frame with one ``avg_path_length`` column
    (``Float64``). ``direction`` (default ``"out"``) is the way edges are
    followed; ``"both"`` ignores direction, giving
    ``nx.average_shortest_path_length(G.to_undirected())`` on a connected
    graph. ``sample`` (a fraction in (0, 1]) estimates from a subset of sources;
    omit for the exact mean.

    Over a filtered edge frame or a traversal result only the kept edges are
    followed, and a sample picks its sources among the nodes at an end of a
    kept edge."""
    edges_node = _ir.edges_node(edges, "avg_path_length")
    _check_direction("avg_path_length", direction)
    frame = NodeFrame._wrap(_native().PlanNode.avg_path_length(edges_node, sample, direction))
    return frame if lazy else float(_value(frame))


@overload
def diameter(
    edges: EdgeFrame, approximate: bool = ..., direction: str = ..., *, lazy: Literal[False] = ...
) -> int: ...
@overload
def diameter(
    edges: EdgeFrame, approximate: bool = ..., direction: str = ..., *, lazy: Literal[True]
) -> NodeFrame: ...
def diameter(
    edges: EdgeFrame, approximate: bool = True, direction: str = "out", *, lazy: bool = False
) -> int | NodeFrame:
    """Graph diameter — the longest shortest path over reachable pairs: an int,
    or with ``lazy=True`` a lazy one-row frame with one ``diameter`` column
    (``Int64``). ``direction`` (default ``"out"``) is the way edges are followed;
    ``"both"`` ignores direction, giving ``nx.diameter(G.to_undirected())`` on a
    connected graph. ``approximate`` (default) is a lower-bound estimate; pass
    ``approximate=False`` for the exact value.

    Over a filtered edge frame or a traversal result only the kept edges are
    followed, and the estimate picks its sources among the nodes at an end of a
    kept edge."""
    edges_node = _ir.edges_node(edges, "diameter")
    _check_direction("diameter", direction)
    frame = NodeFrame._wrap(_native().PlanNode.diameter(edges_node, approximate, direction))
    return frame if lazy else int(_value(frame))
