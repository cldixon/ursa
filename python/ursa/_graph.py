"""Graph verbs and algorithms — members of the expression family.

Two positioning rules from the spec:

- **Traversals** (``hop``, ``shortest_path``, ``random_walk``) are frame-valued:
  a hop is EdgeFrame -> EdgeFrame, so traversal composes.
- **Node-valued algorithms** (``pagerank``, ``connected_components``,
  centralities, ...) are *dual-positioned*: the same kernel is either an
  expression inside ``with_columns`` or, standalone, a NodeFrame of
  ``(id, value)`` with every frame method. Both spellings lower to the same
  ``GraphAlgorithmNode``.

Topology is always threaded explicitly as the ``edges`` argument — there is no
ambient graph. Direction is a per-op parameter; weight is any expression over
edge columns.

These build ``Expr("graph", ...)`` nodes and the frames' Rust plan objects;
``collect()`` executes them through the ``ursa-plan`` physical operators
(``ursa-core`` kernels behind them).
"""

from __future__ import annotations

import functools
from collections.abc import Sequence
from typing import Any

from . import _ir
from ._expr import Expr
from ._frames import EdgeFrame, NodeFrame, _Frame, _GroupBy, _native


class GraphExpr(Expr, NodeFrame):
    """A node-valued graph algorithm — the spec's *dual-positioned* kernel.

    Used inside ``with_columns(pr=ur.pagerank(edges))`` it reads as an expression
    (the column definition). Used standalone it *is* a lazy ``NodeFrame`` of
    ``(id, value)``: ``edges.nodes().with_columns(<verb>=self)``, with every frame
    method (``filter``, ``join``, ``group_by``, ``schema()``, ``explain()``, ...),
    so ``ur.pagerank(edges).join(ur.degree(edges), on="id").sort(...)`` composes
    like any frame. Its plan is built the first time a frame method needs it, so a
    verb used as an expression never builds one.
    """

    # Expr comes first in the MRO: its constructor, operators (``==`` builds an
    # expression), ``repr`` and unhashability are the verb's. The frame methods
    # that return a new frame are spelled out so they return a plain NodeFrame.

    @functools.cached_property
    def _node(self) -> Any:
        return self._frame()._node

    def _frame(self) -> NodeFrame:
        return _verb_edges(self).nodes().with_columns(**{self.payload["verb"]: self})

    def _as_frame(self) -> NodeFrame:
        return NodeFrame._wrap(self._node)

    def filter(self, predicate: Any) -> NodeFrame:
        return self._as_frame().filter(predicate)

    def select(self, *columns: Any) -> NodeFrame:
        return self._as_frame().select(*columns)

    def with_columns(self, **exprs: Any) -> NodeFrame:
        return self._as_frame().with_columns(**exprs)

    def rename(self, mapping: dict[str, str]) -> NodeFrame:
        return self._as_frame().rename(mapping)

    def sort(self, by: Any, *more_by: Any, descending: bool | Sequence[bool] = False) -> NodeFrame:
        return self._as_frame().sort(by, *more_by, descending=descending)

    def head(self, n: int = 10) -> NodeFrame:
        return self._as_frame().head(n)

    limit = head

    def sample(self, n: int, *, seed: int | None = None) -> NodeFrame:
        return self._as_frame().sample(n, seed=seed)

    def distinct(self) -> NodeFrame:
        return self._as_frame().distinct()

    def group_by(self, *keys: Any) -> _GroupBy[NodeFrame]:  # ty: ignore[invalid-method-override]
        return self._as_frame().group_by(*keys)

    def join(self, other: _Frame, *, on: Any = None, how: str = "inner") -> NodeFrame:
        return self._as_frame().join(other, on=on, how=how)


def _verb_edges(e: GraphExpr) -> Any:
    """The edge frame a standalone graph verb runs over; for a value with no
    ``nodes()``, the ``AttributeError`` calling it gave, naming the verb."""
    edges = e.payload["edges"]
    if not hasattr(edges, "nodes"):
        raise AttributeError(
            f"{e.payload['verb']}() takes an EdgeFrame (the graph) as its edges; "
            f"got {type(edges).__name__}"
        )
    return edges


def _graph_expr(verb: str, **params: Any) -> GraphExpr:
    return GraphExpr("graph", {"verb": verb, **params})


def _check_direction(verb: str, direction: str) -> str:
    """Validate a ``direction=`` argument eagerly, so a typo fails at the call site
    rather than at ``collect()`` (the engine knows which directions a verb takes)."""
    _native().check_direction(verb, direction)
    return direction


def _seeds(seeds: Any) -> Any:
    """A seed set for the plan: a NodeFrame's plan, or the value as given."""
    return seeds._node if isinstance(seeds, NodeFrame) else seeds


class LayoutExpr(GraphExpr):
    """A layout — the first kernel that yields *two* columns from one run.

    Positions are two plain ``f32`` columns rather than a struct, because struct
    values and ``unnest`` do not exist in the dialect and a layout is a poor reason
    to invent them. So a layout is selected one axis at a time::

        nodes.with_columns(x=ur.layout_fr(edges).x, y=ur.layout_fr(edges).y)

    Both spellings carry identical parameters, so the engine keys them to the same
    computation and **runs the simulation once** (#115) — the two accessors pick
    different outputs of one result, not two results.

    Used bare it is a NodeFrame of ``(id, x, y)``, like the other
    dual-positioned kernels.
    """

    def _axis(self, field: int) -> GraphExpr:
        return GraphExpr("graph", {**self.payload, "field": field})

    @property
    def x(self) -> GraphExpr:
        """The x column."""
        return self._axis(0)

    @property
    def y(self) -> GraphExpr:
        """The y column."""
        return self._axis(1)

    def _frame(self) -> NodeFrame:
        return _verb_edges(self).nodes().with_columns(x=self.x, y=self.y)


def _layout_expr(verb: str, **params: Any) -> LayoutExpr:
    return LayoutExpr("graph", {"verb": verb, **params})


# --- traversal verbs (frame-valued) ----------------------------------------
class _NeighborAgg:
    """Returned by ``ur.neighbors(edges)``; ``.agg(expr)`` closes it into an Expr.

    Attribute-resolution rule: topology comes from the threaded ``edges``;
    attribute columns in ``expr`` resolve against the ambient frame the
    expression runs in (override with ``from_=``).
    """

    def __init__(self, edges: EdgeFrame, direction: str, from_: Any) -> None:
        self._edges, self._direction, self._from = edges, direction, from_

    def agg(self, expr: Expr, dtype: str = "f64") -> GraphExpr:
        """Close the neighbour aggregation into a graph expression. ``dtype="f32"``
        emits the aggregated column as 32-bit float."""
        return _graph_expr(
            "neighbors_agg",
            edges=self._edges,
            direction=self._direction,
            from_=self._from,
            agg=expr,
            dtype=dtype,
        )


def neighbors(edges: EdgeFrame, direction: str = "out", from_: Any = None) -> _NeighborAgg:
    """Neighbour aggregation. Fused by the optimizer into a segmented CSR reduction."""
    return _NeighborAgg(edges, direction, from_)


def degree(edges: EdgeFrame, direction: str = "out") -> GraphExpr:
    """Per-node degree (UInt32). ``direction`` is 'out' | 'in' | 'both'."""
    return _graph_expr("degree", edges=edges, direction=direction)


class _Hop:
    """Returned by ``ur.hop(...)``; frame-positioned. ``.from_(seeds)`` restricts
    the seed set; the result is a lazy EdgeFrame (src = seed, dst = reached)."""

    def __init__(self, edges: EdgeFrame, n: int, direction: str, seeds: Any = None) -> None:
        self._edges, self._n, self._direction, self._seeds = edges, n, direction, seeds

    def from_(self, seeds: Any) -> EdgeFrame:
        # Binding the seed set completes the traversal, so return the materialized
        # lazy EdgeFrame directly (not another `_Hop`). This is the frame every verb
        # composes on — a relational tail (`.filter`/`.collect`/...) and, since #116,
        # a node-valued graph op (`ur.pagerank(ur.hop(e, n).from_(seeds))`), which
        # needs a real `EdgeFrame` rather than the `__getattr__`-delegating builder.
        return _Hop(self._edges, self._n, self._direction, seeds)._materialize()

    def distinct(self) -> EdgeFrame:
        return self._materialize().distinct()

    def _materialize(self) -> EdgeFrame:
        # The frame's rows are the hop's reached edges, not the parent graph's: its
        # columns are literal ``src``/``dst`` (seed -> reached), so a tail filter
        # references ``src``/``dst``. (The parent's role names aren't carried
        # through a traversal.)
        node = _native().PlanNode.hop(
            _ir.edges_node(self._edges, "hop"),
            _ir.int_arg(self._n),
            self._direction,
            _seeds(self._seeds),
        )
        return EdgeFrame._wrap(node)

    def collect(self):
        return self._materialize().collect()

    def __getattr__(self, name: str) -> Any:  # delegate EdgeFrame verbs onto the hop
        return getattr(self._materialize(), name)


def hop(edges: EdgeFrame, n: int = 1, direction: str = "out") -> _Hop:
    """``n``-hop expansion. Returns a lazy EdgeFrame so traversal composes."""
    return _Hop(edges, n, direction)


def shortest_path(
    edges: EdgeFrame,
    source: Any,
    target: Any,
    weight: Expr | None = None,
    direction: str = "out",
) -> EdgeFrame:
    """Single-pair shortest path. Returns an EdgeFrame ``(src, dst, hop, cost)``: one
    row per edge on the path, in order, with ``hop`` the 0-based position and ``cost``
    the cumulative path cost from ``source`` to that edge's destination. ``weight``
    selects minimum-cost (Dijkstra) over the edge-weight expression; omit it for
    unweighted BFS. For a weighted path ``cost`` is the summed edge weight (so the
    final row's ``cost`` is the total path cost); for an unweighted path it is the
    hop count (``hop + 1``), kept for schema uniformity."""
    # Result columns are literal ``src``/``dst`` (one row per edge on the path);
    # source/target resolve like hop seeds, as 1-element id sets.
    node = _native().PlanNode.shortest_path(
        _ir.edges_node(edges, "shortest_path"),
        [source],
        [target],
        _ir.optional_expr(weight, "shortest_path(weight=)"),
        direction,
    )
    return EdgeFrame._wrap(node)


def random_walk(
    edges: EdgeFrame,
    start: Any,
    steps: int,
    walks_per_node: int = 1,
    seed: int | None = None,
) -> NodeFrame:
    """Random walks; a frame of ``(walk_id, step, node)`` — feeds node2vec-style
    embedding pipelines directly."""
    node = _native().PlanNode.random_walk(
        _ir.edges_node(edges, "random_walk"),
        _seeds(start),
        _ir.int_arg(steps),
        _ir.int_arg(walks_per_node),
        _ir.int_arg(seed),
    )
    return NodeFrame._wrap(node)


# --- node-valued algorithms (dual-positioned) ------------------------------
# In `with_columns(...)` they read as expressions; called bare they are a
# NodeFrame of (id, value): `edges.nodes().with_columns(<verb>=self)` (see
# GraphExpr._frame).


def pagerank(
    edges: EdgeFrame,
    damping: float = 0.85,
    max_iter: int = 30,
    tol: float = 1e-6,
    weight: Expr | None = None,
    dtype: str = "f64",
    direction: str = "out",
) -> GraphExpr:
    """PageRank (pull-based fixpoint). ``dtype="f32"`` emits the score column as
    32-bit float (half the size; the fixpoint still iterates in f64).

    ``direction="out"`` (default) follows the rows as stored: ``nx.pagerank(G)``
    on the ``DiGraph``. ``direction="both"`` reads the graph as undirected — one
    edge per distinct unordered pair, self-loops dropped — which is
    ``nx.pagerank(G.to_undirected())``. With ``weight=``, a pair joined by several
    rows takes the smallest weight. ``"in"`` is not supported."""
    return _graph_expr(
        "pagerank",
        edges=edges,
        damping=damping,
        max_iter=max_iter,
        tol=tol,
        weight=weight,
        dtype=dtype,
        direction=_check_direction("pagerank", direction),
    )


def connected_components(edges: EdgeFrame, mode: str = "weak") -> GraphExpr:
    """Connected components, one integer component label per node.

    ``mode='weak'`` (default) treats edges as undirected: two nodes share a label if
    a path connects them ignoring direction. ``mode='strong'`` returns strongly
    connected components: two nodes share a label only if each is reachable from the
    other following edge direction. Labels are arbitrary but stable ids (group nodes
    by label to get the components)."""
    return _graph_expr("connected_components", edges=edges, mode=mode)


def triangle_count(edges: EdgeFrame) -> GraphExpr:
    """Per-node triangle count (treats edges as undirected)."""
    return _graph_expr("triangle_count", edges=edges)


def clustering_coefficient(edges: EdgeFrame, dtype: str = "f64") -> GraphExpr:
    """Local clustering coefficient (derived from triangles). ``dtype="f32"`` emits
    the coefficient column as 32-bit float."""
    return _graph_expr("clustering_coefficient", edges=edges, dtype=dtype)


def betweenness(
    edges: EdgeFrame,
    sample: float | None = None,
    weight: Expr | None = None,
    seed: int | None = None,
    dtype: str = "f64",
    direction: str = "out",
) -> GraphExpr:
    """Betweenness centrality (Brandes), unnormalized. ``sample=``
    approximates from a ``seed``-shuffled subset of sources (exact is O(nm));
    ``seed`` makes the sampled estimate reproducible. Parallel edges count as
    distinct shortest paths (so a multigraph diverges from a simple-graph
    reference like NetworkX); weighted, only *exactly* float-equal path costs tie.
    ``dtype="f32"`` emits the centrality column as 32-bit float.

    ``direction="out"`` (default) is directed: each ordered pair counted once,
    ``nx.betweenness_centrality(G, normalized=False)``. ``direction="both"`` reads
    the graph as undirected — one edge per distinct unordered pair, self-loops
    dropped — and counts each unordered pair once:
    ``nx.betweenness_centrality(G.to_undirected(), normalized=False)``. With
    ``weight=``, a pair joined by several rows takes the smallest weight. ``"in"``
    is not supported (over all pairs it equals ``"out"``)."""
    return _graph_expr(
        "betweenness",
        edges=edges,
        sample=sample,
        weight=weight,
        seed=seed,
        dtype=dtype,
        direction=_check_direction("betweenness", direction),
    )


def closeness(
    edges: EdgeFrame, weight: Expr | None = None, dtype: str = "f64", direction: str = "out"
) -> GraphExpr:
    """Closeness centrality, ``reachable / Σ distance``. ``dtype="f32"`` emits the
    score column as 32-bit float.

    ``direction`` picks which way distance is measured: ``"out"`` (default) from
    the node along out-edges, ``"in"`` to the node (NetworkX's directed
    convention), ``"both"`` ignoring direction —
    ``nx.closeness_centrality(G.to_undirected(), wf_improved=False)``."""
    return _graph_expr(
        "closeness",
        edges=edges,
        weight=weight,
        dtype=dtype,
        direction=_check_direction("closeness", direction),
    )


def label_propagation(edges: EdgeFrame, max_iter: int = 20, seed: int | None = None) -> GraphExpr:
    """Community detection via label propagation."""
    return _graph_expr("label_propagation", edges=edges, max_iter=max_iter, seed=seed)


def louvain(
    edges: EdgeFrame, weight: Expr | None = None, resolution: float = 1.0, seed: int | None = None
) -> GraphExpr:
    """Community detection via Louvain modularity optimization."""
    return _graph_expr("louvain", edges=edges, weight=weight, resolution=resolution, seed=seed)


# --- layout (multi-output) --------------------------------------------------
# A force-directed layout is an iterative fixpoint over the CSR — the same
# computational shape as PageRank — so it lives with the kernels rather than in a
# rendering library, and positions come back as ordinary columns: filter them,
# join them, `sink_parquet` them. Precomputing a layout once and caching it in the
# lake is a workflow, not a workaround.


def layout_fr(
    edges: EdgeFrame,
    iterations: int = 300,
    k: float = 1.0,
    gravity: float = 0.02,
    seed: int | None = None,
) -> LayoutExpr:
    """Fruchterman-Reingold layout; ``(x, y)`` as two ``f32`` columns.

    ``k`` is the ideal edge length and sets the units the result comes out in;
    ``gravity`` pulls the drawing toward the origin, which is what keeps
    disconnected components in one picture instead of repelling forever.

    Deterministic: a given ``seed`` reproduces a layout exactly, and the result is
    bit-identical across thread counts. Select an axis with ``.x`` / ``.y``; both
    share one simulation.

    Repulsion is grid-approximated — near cells exactly, far cells through their
    centre of mass — which is right for evenly spread graphs and approximate for
    violently clustered ones. ForceAtlas2 with a Barnes-Hut quadtree is the
    successor.
    """
    return _layout_expr(
        "layout_fr", edges=edges, iterations=iterations, k=k, gravity=gravity, seed=seed
    )


def layout_random(edges: EdgeFrame, k: float = 1.0, seed: int | None = None) -> LayoutExpr:
    """A deterministic spread over a phyllotactic spiral; ``(x, y)`` as two columns.

    No two nodes coincide, which is what makes it a safe starting point for a force
    layout — two nodes at one point feel unbounded repulsion.
    """
    return _layout_expr("layout_random", edges=edges, k=k, seed=seed)


def layout_circle(edges: EdgeFrame, k: float = 1.0) -> LayoutExpr:
    """Nodes evenly spaced on a circle in id order; ``(x, y)`` as two columns.

    A baseline that is obviously not force-directed: if a figure looks the same
    under this and under ``layout_fr``, the force layout did not run.
    """
    return _layout_expr("layout_circle", edges=edges, k=k)
